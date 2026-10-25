//! Production [`SubagentHost`] backed by [`SessionScope`].
//!
//! Spawns child agents by resolving a profile → `AgentScopeConfig`, creating
//! a child `AgentScope`, building a fresh `TurnFlow`, and running one turn.
//! The result (final assistant text + token usage) is returned as a
//! [`SubagentCompletion`].
//!
//! ## Architecture
//!
//! ```text
//! AgentTool / AgentSwarmTool
//!     │
//!     ▼
//! SessionSubagentHost::spawn()       ← sync: resolve profile, gen ID, build handle
//!     │
//!     ▼
//! CompletionFuture (async)           ← inside the SubagentHandle
//!  ├─ SessionScope::create_agent()   ← async: register child AgentScope
//!  ├─ fire SubagentStart hook        ← on the parent session's hook engine
//!  ├─ SubagentLlmFactory::create_llm()
//!  ├─ run_one_turn()                    ← pure async (B4 T4)
//!  ├─ fire SubagentStop hook         ← every exit path after Start
//!  └─ extract_last_assistant_text()
//! ```
//!
//! ## Reference cycle avoidance
//!
//! The host holds a [`Weak<SessionScope>`] — not `Arc` — to avoid a reference
//! cycle (`SessionScope → AgentScope → config.subagent_host → SessionSubagentHost
//! → SessionScope`). The weak ref is upgraded inside the completion future,
//! which always runs while the session is alive.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use crate::context::ContextMessage;
use crate::error::{EngineError, EngineResult};
use crate::hook_engine::HookEventType;
use crate::llm::{FixedLlmResolver, Llm};
use crate::message::{ContentPart, Role};
use crate::model_catalog::ModelCatalog;
use crate::origin::PromptOrigin;
use crate::profile::{ProfileManager, ProfileSummaryPolicy, ThinkingEffort};
use crate::scope::config::AgentScopeConfig;
use crate::scope::{AgentId, AgentScope, SessionScope};
use crate::session::events::{EventBus, SessionEvent};
use crate::tool_registry::ValidatorCache;
use crate::turn_flow::{
    ActivityTracker, TurnEndResult as TurnFlowTurnEndResult, TurnFlow, TurnFlowConfig,
};
use mirri_proto::WireOp;

use super::{
    CompletionFuture, ResumeRequest, SpawnRequest, SubagentCompletion, SubagentError,
    SubagentHandle, SubagentHost, SuspendCause, TokenUsage,
};

/// Factory for creating [`Llm`] providers for child agents.
///
/// Implemented by the host layer (e.g. `mirri-embedded`) which has access
/// to provider construction (API keys, base URLs, model routing). This trait
/// lives in `mirri-core` so the production host can construct child LLMs
/// without depending on the embedded crate.
pub trait SubagentLlmFactory: Send + Sync {
    /// Build an [`Llm`] provider for the given agent config's `model_id`
    /// and `provider_type`, or fail with a human-readable reason.
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String>;
}

/// The default subagent nesting-depth cap (SDR-B4-03): dispatch chains
/// deeper than five levels are refused at the spawn gate. `0` (configured
/// through the session scope) disables the limit entirely (TS parity).
pub const DEFAULT_MAX_NESTING_DEPTH: u32 = 5;

/// The default per-call item cap for `AgentSwarm` (SDR-012): a single
/// swarm dispatches at most this many members unless the `[subagent]`
/// `maxSwarmItems` config key or the `MIRRI_AGENT_SWARM_MAX_ITEMS` env
/// override raises (or `0`-unlimits) it. Deliberately lower than the TS
/// engine's hardcoded 128 — a deliberate Rust-side product deviation
/// recorded in `docs/specs/subagent-requirements.md`.
pub const DEFAULT_MAX_SWARM_ITEMS: u32 = 3;

/// Tracking entry for a spawned child agent.
struct ChildEntry {
    /// The child's profile name (for the handle's `profile_name` field).
    profile_name: String,
    /// The child's resolved model (for the handle's `model` field).
    model: Option<String>,
    /// The child's TurnFlow, stored for reuse in `resume()`.
    ///
    /// A tokio mutex (B4 T3b): the lock is taken with `.await` inside the
    /// completion future, so the future stays `Send` and the turn runs as a
    /// plain async task (a std lock would require `spawn_blocking` +
    /// `block_on` — the nested-block_on deadlock face this batch removes).
    turn_flow: Arc<tokio::sync::Mutex<TurnFlow>>,
    /// Whether a run of this child is currently in flight (G13). Set at
    /// spawn/resume, cleared on every settle (completion, failure,
    /// cancellation) and — through [`RunningGuard`] — when the completion
    /// future is dropped without settling (G19).
    running: bool,
    /// The swarm item label the child was spawned under (`AgentSwarm`
    /// batches) — later `resume_agent_ids` calls relabel the resumed
    /// member's result row with it (TS `getSwarmItem`, D-60).
    swarm_item: Option<String>,
    /// The child's engine activity source — the TurnFlow's activity tracker
    /// (the same instance the turn's observation points stamp). Rides the
    /// [`SubagentHandle`](super::SubagentHandle) so the tool layer can
    /// forward real activity into the tracked task's liveness window.
    activity: Arc<ActivityTracker>,
}

/// Production [`SubagentHost`] backed by [`SessionScope`].
///
/// Holds a [`Weak<SessionScope>`] to avoid reference cycles (the session
/// owns agents whose configs hold `Arc<dyn SubagentHost>` pointing back).
/// The weak ref is upgraded inside the completion future, which always runs
/// while the session is alive.
///
/// Each parent agent gets its own `SessionSubagentHost` instance. The host
/// tracks spawned children for `resume()` validation and TurnFlow reuse.
pub struct SessionSubagentHost {
    /// Weak reference to the session — upgraded inside completion futures.
    session: Weak<SessionScope>,
    /// Profile manager — resolves profile names to `AgentScopeConfig`.
    profile_manager: Arc<ProfileManager>,
    /// The parent agent's ID — for debugging, model fallback, and the
    /// `subagent.spawned` event's parent field.
    parent_agent_id: AgentId,
    /// The parent agent's config — used to inherit optional services.
    parent_config: AgentScopeConfig,
    /// The parent agent's LIVE config handle (spec 11 R5/A3), when one is
    /// available — the seam that makes Tier 3 (inherit the dispatcher's
    /// current model) read the model at SPAWN time instead of the host's
    /// create-time snapshot. Bound once by [`Self::set_parent_config_shared`]
    /// once the parent agent exists in the session (the create/resume paths
    /// bind it after `create_agent`; nested hosts bind it after the child
    /// agent exists); until then, Tier 3 falls back to the `parent_config`
    /// snapshot (directly-constructed hosts in tests).
    ///
    /// Held as a **weak** handle on purpose (review R1, C1): the parent
    /// config's `subagent_host` field points STRONGLY back at this host (the
    /// config carries the `Arc<dyn SubagentHost>`), so a strong
    /// `parent_config_shared` would close a reference cycle — the session
    /// could never drop the host, and with it the whole subagent tree
    /// (callbacks, kernels, approval stores, LLM factories, the children
    /// map). The weak edge breaks the cycle; each `spawn` upgrades it, and a
    /// missing upgrade falls back to the create-time snapshot (semantics
    /// unchanged — the parent agent's config outlives every live host that
    /// references it).
    parent_config_shared: OnceLock<Weak<RwLock<AgentScopeConfig>>>,
    /// The calling agent's profile name, when it runs under one (R3-4).
    ///
    /// The rust engine has no profile-bound session today: the main agent
    /// is built directly from an [`AgentScopeConfig`] (no profile name),
    /// and spawned children cannot nest in v1. So every production host
    /// carries `None` — the caller runs as the **default** profile (the
    /// essential `agent`) and [`Self::effective_allowlist`] resolves
    /// through the default arm of [`crate::profile::subagent_allowlist_for`].
    /// The field is
    /// the seam for a future session profile-bind channel (the wire's
    /// `ProfileBind` op has no production writer yet); tests use it to
    /// exercise the named-caller arm.
    caller_profile_name: Option<String>,
    /// Factory for creating child LLM providers.
    llm_factory: Arc<dyn SubagentLlmFactory>,
    /// TurnFlow config for child agents.
    turn_flow_config: TurnFlowConfig,
    /// The engine's flag registry, wired via
    /// [`Self::with_flag_registry`] — shared with the main agent's flow so
    /// the config `[experimental]` layer governs both. `None` (the default)
    /// leaves the child flows on the env-only flag chain.
    flag_registry: Option<Arc<crate::flags::FlagRegistry>>,
    /// Model catalog — validates model-alias overrides and feeds
    /// `available_profiles()` capabilities. `None` when the host has no
    /// catalog (alias overrides are then passed through unvalidated).
    model_catalog: Option<Arc<dyn ModelCatalog>>,
    /// Spawned children, keyed by agent ID.
    children: Arc<Mutex<HashMap<String, ChildEntry>>>,
    /// The session's event bus — the subagent lifecycle events
    /// (`subagent.spawned`/`started`/`completed`/`failed`) are published
    /// here. `None` when the host was built without one (tests).
    event_bus: Option<Arc<EventBus>>,
    /// The session's turn-id allocator, when the host was built inside a
    /// session — the child flows draw their `turn.ended` ids from it, so
    /// subagent turns share the session's single numeric turn namespace
    /// (with the main agent's and the cron/task turns'). `None` (tests,
    /// hosts built before a session's artifacts exist) leaves the child
    /// flows on their local counters, exactly like a standalone flow.
    turn_id_source: Option<Arc<AtomicU64>>,
    /// This host's depth in the dispatch tree (B4 T5, SDR-B4-03): the
    /// session main agent's host is 1; every nested host is one deeper.
    nesting_depth: u32,
    /// The maximum dispatch depth (SDR-B4-03): the default 5 covers
    /// realistic delegation chains; `0` disables the limit (TS parity).
    max_nesting_depth: u32,
}

/// The resolved model + thinking-effort binding for a spawned child (B4
/// T2, D-48) — the TS `resolveSubagentBinding`
/// (`configSection.ts:91-112`) counterpart, with a profile tier the TS
/// resolves earlier in its service pipeline.
///
/// Priority: explicit alias (already validated against the catalog by the
/// caller; `catalog.default_effort` replaces the effort) → the profile's
/// `default_model`/`model` + the profile's `thinking_effort` → inherit the
/// caller's model + thinking effort.
pub(crate) struct SubagentBinding {
    pub model: String,
    pub thinking: Option<ThinkingEffort>,
}

/// Resolve a child's model + thinking binding (see [`SubagentBinding`]).
///
/// The explicit-alias tier returns [`EngineError::RequestInvalid`] for an
/// alias the catalog does not know (the curated listing, so the caller can
/// self-correct); an absent catalog or an empty one passes the alias
/// through unvalidated — the established host behavior
/// (`should_allow_model_override_when_catalog_is_empty`).
fn resolve_subagent_binding(
    parent_model: &str,
    parent_effort: Option<ThinkingEffort>,
    profile: &crate::profile::AgentProfile,
    requested: Option<&str>,
    catalog: Option<&dyn ModelCatalog>,
) -> EngineResult<SubagentBinding> {
    // Tier 1: explicit model alias — the catalog's default effort rides the
    // binding when the entry declares one; an entry without one binds no
    // effort (the explicit-alias tier replaces the inherited effort).
    if let Some(alias) = requested.filter(|alias| !alias.is_empty()) {
        if let Some(catalog) = catalog {
            let valid_models = catalog.list_models();
            if !valid_models.is_empty() && catalog.get_by_id(alias).is_none() {
                return Err(unknown_model_alias_error(catalog, alias));
            }
            let effort = catalog
                .get_by_id(alias)
                .and_then(|entry| entry.default_effort)
                .and_then(|effort| parse_thinking_effort(&effort));
            return Ok(SubagentBinding {
                model: alias.to_string(),
                thinking: effort,
            });
        }
        // No catalog: the override passes through unvalidated; the
        // caller's effort stays in force (there is no catalog entry to
        // replace it with).
        return Ok(SubagentBinding {
            model: alias.to_string(),
            thinking: parent_effort,
        });
    }
    // Tier 2: the profile's own model (default_model, else model) with the
    // profile's thinking effort.
    let profile_model = profile
        .default_model
        .clone()
        .filter(|model| !model.is_empty())
        .or_else(|| {
            let model = profile.model.clone();
            (!model.is_empty()).then_some(model)
        });
    if let Some(model) = profile_model {
        return Ok(SubagentBinding {
            model,
            thinking: profile.thinking_effort.clone(),
        });
    }
    // Tier 3: inherit the caller's model + thinking effort; when the caller
    // has none either, fail with the curated listing (spec 11 A6 — the
    // subagent chain's three-nothing face, never a silent empty model).
    if parent_model.is_empty() {
        return Err(no_model_resolved_error(catalog, &profile.name));
    }
    Ok(SubagentBinding {
        model: parent_model.to_string(),
        thinking: parent_effort,
    })
}

/// Render the catalog's model listing for an error message (D-M10.4
/// self-correction surface) — the shared
/// [`render_model_listing`](crate::model_catalog::render_model_listing)
/// renderer, so the subagent errors and the embedded switch-path / factory
/// rejections (spec 11 R10/A11) name the same available models.
fn model_listing(catalog: &dyn ModelCatalog) -> String {
    crate::model_catalog::render_model_listing(catalog)
}

/// Build the unknown-alias error with the curated listing (D-M10.4): the
/// inline listing lets the LLM correct itself from what it saw in the tool
/// description. Curated entries carry descriptions; when nothing is
/// curated the bare id list is used.
fn unknown_model_alias_error(catalog: &dyn ModelCatalog, alias: &str) -> EngineError {
    EngineError::RequestInvalid(format!(
        "unknown model alias '{alias}'. {}",
        model_listing(catalog)
    ))
}

/// Build the three-nothing error (spec 11 A6): the subagent chain has no
/// explicit model, the profile declares no default model, and the
/// dispatcher has no current model. Mirrors [`unknown_model_alias_error`]'s
/// curated listing so the LLM can self-correct from what it saw in the tool
/// description; without a catalog the error stays actionable but carries no
/// listing.
fn no_model_resolved_error(catalog: Option<&dyn ModelCatalog>, profile_name: &str) -> EngineError {
    let mut message = format!(
        "no model resolved for subagent '{profile_name}': no explicit model was given, the \
         profile declares no default model, and the dispatcher has no current model. "
    );
    match catalog {
        Some(catalog) => message.push_str(&model_listing(catalog)),
        None => message.push_str(
            "Configure a default model (config.toml default_model) or pass an explicit model alias.",
        ),
    }
    EngineError::RequestInvalid(message)
}

/// Parse a catalog `default_effort` spelling into a [`ThinkingEffort`].
/// Kosong-canonical spellings (`off`/`on`), the rust spellings
/// (`none`/`low`/`medium`/`high`), and any model-declared effort
/// (`max`, `xhigh`, ...) all bind — only a blank string fails, and a
/// non-string JSON value drops to no effort rather than failing the
/// spawn.
fn parse_thinking_effort(effort: &str) -> Option<ThinkingEffort> {
    serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
}

impl SessionSubagentHost {
    /// Build a production subagent host.
    ///
    /// `parent_config` is used to inherit optional services
    /// (`reverse_rpc_callback`, `interaction_kernel`, `web_search_provider`,
    /// `fetch_url_provider`)
    /// and as a fallback for model resolution. `model_catalog` (when
    /// present) validates `model` overrides in [`SubagentHost::spawn`] and
    /// populates real capabilities in [`SubagentHost::available_profiles`].
    /// `event_bus` (when present) receives the subagent lifecycle events.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Weak<SessionScope>,
        profile_manager: Arc<ProfileManager>,
        parent_agent_id: AgentId,
        parent_config: AgentScopeConfig,
        llm_factory: Arc<dyn SubagentLlmFactory>,
        turn_flow_config: TurnFlowConfig,
        model_catalog: Option<Arc<dyn ModelCatalog>>,
        event_bus: Option<Arc<EventBus>>,
    ) -> Self {
        Self {
            session,
            profile_manager,
            parent_agent_id,
            parent_config,
            caller_profile_name: None,
            parent_config_shared: OnceLock::new(),
            llm_factory,
            turn_flow_config,
            flag_registry: None,
            model_catalog,
            children: Arc::new(Mutex::new(HashMap::new())),
            event_bus,
            turn_id_source: None,
            nesting_depth: 1,
            max_nesting_depth: DEFAULT_MAX_NESTING_DEPTH,
        }
    }

    /// Bind the session's turn-id allocator (see [`Self::turn_id_source`])
    /// — the handle `SessionArtifacts::turn_id_counter` hands out. The
    /// create/resume assembly wires it right after resolving the session's
    /// artifacts; nested hosts inherit the parent host's handle so the
    /// whole dispatch tree numbers turns from one namespace.
    pub fn with_turn_id_source(mut self, source: Option<Arc<AtomicU64>>) -> Self {
        self.turn_id_source = source;
        self
    }

    /// Set the nesting guard's limits (B4 T5, SDR-B4-03): the host's own
    /// `depth` and the configured maximum. Production hosts pass the
    /// session-scope config's `max_nesting_depth` with depth 1; nested
    /// hosts are built one deeper with the same maximum.
    pub fn with_nesting_limits(mut self, depth: u32, max: u32) -> Self {
        self.nesting_depth = depth;
        self.max_nesting_depth = max;
        self
    }

    /// Adopt every already-registered subagent agent the session owns (B4
    /// T8, D-55 → ADR-006/ADR-017) — the restart-resume face: after a resume
    /// the session's registered children are DORMANT (their `AgentScope`s are
    /// only lazily hydrated from their journals on first use), so the fresh
    /// host registers every roster child in its children map — hydrated or
    /// not — so `Agent(resume=...)` keeps the same identity and
    /// TurnFlow-reuse semantics across the restart. Children not carrying a
    /// `profile_name`/registration (the main agent, directly-constructed
    /// scopes) are skipped.
    pub async fn adopt_session_agents(&self) {
        let session = match self.session.upgrade() {
            Some(session) => session,
            None => return,
        };
        let roster = session.list_subagent_registrations().await;
        let mut adopted = 0usize;
        for registration in roster {
            let agent_id = AgentId::new(&registration.agent_id);
            let (profile_name, model) = if let Some(agent) = session.get_agent(&agent_id).await {
                let config = agent.config();
                let guard = config.read().unwrap_or_else(|e| e.into_inner());
                match (guard.profile_name.clone(), guard.model_id.clone()) {
                    (Some(name), model) => (name, Some(model)),
                    (None, _) => continue,
                }
            } else {
                // Dormant child: the roster registration carries the
                // profile + pinned model (a model-less registration inherits
                // the parent's at hydration). The model recorded here is a
                // fallback for the handle's `model` field.
                (
                    registration.profile_name.clone(),
                    registration.model.clone(),
                )
            };
            self.adopt_child_entry(agent_id, profile_name, model);
            adopted += 1;
        }
        tracing::debug!(
            adopted,
            "adopted registered subagents into the host children map"
        );
    }

    /// Register a single child into the host's children map (shared by
    /// [`Self::adopt_session_agents`] and the live-spawn path's counterpart),
    /// creating its `ChildEntry` with a fresh turn flow when absent.
    fn adopt_child_entry(&self, agent_id: AgentId, profile_name: String, model: Option<String>) {
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children
            .entry(agent_id.as_str().to_string())
            .or_insert_with(|| {
                tracing::debug!(
                    agent_id = %agent_id,
                    profile = %profile_name,
                    "adopting a restored subagent into the host children map"
                );
                // Supervision B1: the adopted flow keeps the member's
                // identity so its approval asks stay attributable across
                // the restart.
                let mut flow = TurnFlow::new(self.turn_flow_config.clone());
                flow.set_agent_id(Some(agent_id.as_str().to_string()));
                if let Some(source) = &self.turn_id_source {
                    flow.set_turn_id_source(Arc::clone(source));
                }
                let tracker = flow.activity_tracker();
                ChildEntry {
                    profile_name,
                    model,
                    turn_flow: Arc::new(tokio::sync::Mutex::new(flow)),
                    running: false,
                    swarm_item: None,
                    activity: tracker,
                }
            });
    }

    /// Set the calling agent's profile name — the R3-4 subagent-allowlist
    /// seam (see [`Self::caller_profile_name`]). Production callers leave
    /// the host unbound (the session's main agent runs as the default
    /// profile); a future profile-bind channel and the enforcement tests
    /// use this to exercise the named-caller arm.
    pub fn with_caller_profile(mut self, profile_name: Option<String>) -> Self {
        self.caller_profile_name = profile_name;
        self
    }

    /// Bind the engine's flag registry so child flows resolve experimental
    /// flags through the same env → config → defaults chain as the main
    /// agent's flow.
    pub fn with_flag_registry(mut self, registry: Arc<crate::flags::FlagRegistry>) -> Self {
        self.flag_registry = Some(registry);
        self
    }

    /// Bind the parent agent's LIVE config handle (spec 11 R5/A3).
    ///
    /// Once bound, Tier 3 model resolution (spawn without an explicit model
    /// and without a profile default) reads `model_id` from this shared
    /// `Arc<RwLock<AgentScopeConfig>>` at spawn time instead of the host's
    /// create-time snapshot — so a parent that switched models after the
    /// host was built passes the NEW model to children spawned afterwards
    /// (A3). The create/resume paths bind it right after the parent agent
    /// exists; nested hosts bind it after their child agent exists. Bound at
    /// most once (later calls are ignored): the binding point is a fixed
    /// point in the parent's lifetime, never a moving one.
    ///
    /// The handle is stored WEAKLY (see [`Self::parent_config_shared`]): the
    /// parent config's `subagent_host` points strongly back at this host, so
    /// a strong binding would leak the session's whole subagent tree.
    pub fn set_parent_config_shared(&self, shared: Arc<RwLock<AgentScopeConfig>>) {
        let _ = self.parent_config_shared.set(Arc::downgrade(&shared));
    }

    /// The calling agent's effective subagent-dispatch allowlist —
    /// [`crate::profile::subagent_allowlist_for`] resolved against this
    /// host's profile manager, consulted live (per spawn / per description
    /// build, so profile hot-reloads take effect on the next consult — the
    /// TS launch-time resolution shape).
    ///
    /// Default-deny semantics (consumption-domain SDR-012: unset ≡ empty): an absent profile
    /// name cannot dispatch — only its own explicit list, if any, is
    /// consulted.
    fn effective_allowlist(&self) -> Vec<String> {
        crate::profile::subagent_allowlist_for(
            &self.profile_manager,
            self.caller_profile_name.as_deref(),
        )
    }

    /// Publish a subagent lifecycle event on the host's bus, when one is
    /// wired. Fire-and-forget — the spawn path never fails on event
    /// delivery.
    fn publish_event(&self, event: SessionEvent) {
        if let Some(ref bus) = self.event_bus {
            bus.publish(event);
        }
    }
}

impl SubagentHost for SessionSubagentHost {
    fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
        // 0. Subagent-allowlist enforcement (R3-4, default-deny) — the
        // launch gate. Checked BEFORE profile resolution so an
        // unauthorized name is refused even when it is not a registered
        // profile, and deliberately NOT on `resume` (a resume addresses
        // an owned agent_id, not a dispatchable type). Enforcement lives
        // on the HOST, not the tools, because the host is the single
        // spawn choke point both the `Agent` and `AgentSwarm` tools
        // dispatch through, and the only component that knows the
        // calling agent's profile context.
        let allowlist = self.effective_allowlist();
        if !allowlist.contains(&request.profile_name) {
            return Err(EngineError::RequestInvalid(
                crate::profile::subagent_type_not_allowed_message(
                    &request.profile_name,
                    &allowlist,
                ),
            ));
        }

        // 0b. Nesting-depth guard (B4 T5, SDR-B4-03): plan A removes the
        // deadlock face but not the recursion hazard — an unbounded
        // dispatch tree would still exhaust memory and provider quota on
        // self-delegating profiles. Refuse one level BELOW the configured
        // maximum (a depth-5 host spawning depth-6 with max 5).
        if self.max_nesting_depth != 0 && self.nesting_depth + 1 > self.max_nesting_depth {
            return Err(EngineError::RequestInvalid(format!(
                "subagent dispatch refused: the nesting depth would reach {}, but the \
                 configured maximum is {} (maxNestingDepth). Restructure the delegation \
                 chain, or set the [subagent] maxNestingDepth to 0 to disable the limit.",
                self.nesting_depth + 1,
                self.max_nesting_depth
            )));
        }

        // 1. Generate a unique agent ID.
        let agent_id = format!("agent-{}", uuid::Uuid::new_v4());

        // 2. Resolve profile → AgentScopeConfig.
        let mut config = self
            .profile_manager
            .resolve(&request.profile_name, &mirri_os::LocalSyncFs)?;

        // 3. Resolve the model + thinking binding (B4 T2, D-48): explicit
        // alias (validated against the catalog, whose default effort rides
        // the binding) → the profile's model + effort → inherit the caller.
        // The profile's resolved default model is not validated — profiles
        // may name raw provider model ids that are not exposed as aliases.
        // An invalid override fails fast here so the caller gets the
        // curated listing (D-M10.4) instead of a silently misrouted model.
        let profile = self
            .profile_manager
            .resolve_profile(&request.profile_name)
            .ok()
            .unwrap_or_else(|| crate::profile::AgentProfile {
                name: request.profile_name.clone(),
                ..Default::default()
            });
        // Tier 3 (spec 11 R5/A3): inherit the parent's LIVE model — read
        // from the shared config handle when one is bound (the parent may
        // have switched models after this host was built), else from the
        // create-time snapshot. Same pattern as `ConfigResolvingLlm`: a
        // short read lock inside the spawn path, released before any other
        // lock is taken (no lock-order interaction).
        let (parent_model, parent_effort) = match self
            .parent_config_shared
            .get()
            .and_then(|weak| weak.upgrade())
        {
            Some(shared) => {
                let guard = shared.read().unwrap_or_else(|e| e.into_inner());
                (guard.model_id.clone(), guard.thinking_effort.clone())
            }
            None => (
                self.parent_config.model_id.clone(),
                self.parent_config.thinking_effort.clone(),
            ),
        };
        let binding = resolve_subagent_binding(
            &parent_model,
            parent_effort,
            &profile,
            request.model.as_deref(),
            self.model_catalog.as_deref(),
        )?;
        config.model_id = binding.model.clone();
        config.thinking_effort = binding.thinking.clone();
        // B4 T8 (D-55): the identity the restart restore re-adopts from.
        config.profile_name = Some(request.profile_name.clone());

        // 4. Inherit optional services from parent. The reverse-RPC SLOT
        // (not a callback snapshot) inherits, so the child resolves the
        // session's live callback per call — the late-subscribe fix.
        config.reverse_rpc_slot = self.parent_config.reverse_rpc_slot.clone();
        config.interaction_kernel = self.parent_config.interaction_kernel.clone();
        config.web_search_provider = self.parent_config.web_search_provider.clone();
        config.fetch_url_provider = self.parent_config.fetch_url_provider.clone();
        // The session's event bus rides the child config so the child's
        // own goal/skill tools announce on it (`event.goal.updated` /
        // `event.skill.activated` from a subagent reach the session's
        // subscribers too).
        config.event_bus = self.event_bus.clone();
        // Nested dispatch (B4 T5, D-46): the child gets its own host, one
        // depth level deeper, bound to the child's own profile as the
        // caller (R3-4 — the grandchild dispatch allowlist consults the
        // child profile's `subagents` list). The child-config snapshot is
        // cloned BEFORE the host is attached, so the nested host carries
        // no host of its own in `parent_config` (the only Arc edge into
        // the cycle-prone graph is config → host; the host's session
        // reference stays weak).
        let child_config_for_nested_host = config.clone();
        let nested_host = Arc::new(
            SessionSubagentHost::new(
                Weak::clone(&self.session),
                Arc::clone(&self.profile_manager),
                AgentId::new(&agent_id),
                child_config_for_nested_host,
                Arc::clone(&self.llm_factory),
                self.turn_flow_config.clone(),
                self.model_catalog.clone(),
                self.event_bus.clone(),
            )
            .with_nesting_limits(self.nesting_depth + 1, self.max_nesting_depth)
            .with_turn_id_source(self.turn_id_source.clone())
            .with_caller_profile(Some(request.profile_name.clone())),
        );
        // The nested host's shared parent-config handle is bound inside the
        // completion future, right after the child agent exists (spec 11
        // R5/A3): from then on, the grandchild's Tier 3 inheritance reads
        // the child's LIVE model. The handle is cloned before the move into
        // `config` above so the future can reach it (the config carries the
        // trait-object, which has no binding method).
        let nested_host_shared = Arc::clone(&nested_host);
        config.subagent_host = Some(nested_host);

        let resolved_model = Some(config.model_id.clone());

        // 5. Create a TurnFlow for the child (stored for resume reuse),
        //    carrying the member identity (supervision B1) so its approval
        //    asks attribute to this member. The flow's activity tracker is
        //    captured alongside — the handle carries it so the tool layer
        //    can forward the child's engine activity into the tracked
        //    task's liveness window.
        let (turn_flow, activity_tracker, child_registry) = {
            let mut flow = TurnFlow::new(self.turn_flow_config.clone());
            flow.set_agent_id(Some(agent_id.clone()));
            if let Some(source) = &self.turn_id_source {
                flow.set_turn_id_source(Arc::clone(source));
            }
            let tracker = flow.activity_tracker();
            // ADR-006 wait-chain propagation: the handle also carries the
            // child's wait registry so a PARENT's WaitingSubagent probe
            // can fall back to the child's own waits when the child's
            // activity clock is frozen (the child waiting on a task /
            // gate — the fork/converted incident shapes).
            let child_registry = flow.wait_registry();
            (
                Arc::new(tokio::sync::Mutex::new(flow)),
                tracker,
                child_registry,
            )
        };

        // 6. Register child in the tracking map — running, until the
        // completion future settles (G13/G14).
        {
            let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
            children.insert(
                agent_id.clone(),
                ChildEntry {
                    profile_name: request.profile_name.clone(),
                    model: resolved_model.clone(),
                    turn_flow: Arc::clone(&turn_flow),
                    running: true,
                    swarm_item: request.swarm_item.clone(),
                    activity: Arc::clone(&activity_tracker),
                },
            );
        }

        // 6b. Announce the spawn on the session's event bus (the
        // `subagent.spawned` wire event) — synchronously, so subscribers
        // learn the child's identity before its completion future runs.
        self.publish_event(SessionEvent::SubagentSpawned {
            subagent_id: agent_id.clone(),
            subagent_name: request.profile_name.clone(),
            description: Some(request.description.clone()).filter(|d| !d.is_empty()),
            run_in_background: request.run_in_background,
            model: resolved_model.clone(),
            parent_agent_id: Some(self.parent_agent_id.as_str().to_string()),
            parent_tool_call_id: request.parent_tool_call_id.clone(),
            swarm_index: request.swarm_index,
        });

        // 7. Capture everything the completion future needs.
        let summary_policy = profile.summary_policy.clone();
        let session_weak = self.session.clone();
        let profile_manager = Arc::clone(&self.profile_manager);
        let llm_factory = Arc::clone(&self.llm_factory);
        // (d) Role prompt prefix (D-47, TS promptPrefix at its simplest —
        // a static string; the dynamic function form is a recorded
        // deviation, SDR-B4-01).
        let prompt = match profile.prompt_prefix.as_deref() {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}\n\n{}", request.prompt),
            _ => request.prompt.clone(),
        };
        let cancel = request.cancel.clone();
        let suppress_rate_limit_failure_event = request.suppress_rate_limit_failure_event;
        let child_agent_id = agent_id.clone();
        let profile_name = request.profile_name.clone();
        let resolved_model_str = resolved_model.clone().unwrap_or_default();
        let children = Arc::clone(&self.children);
        let turn_flow_for_future = Arc::clone(&turn_flow);
        let event_bus = self.event_bus.clone();
        // ADR-011: the per-attempt suspension boundary feed — the
        // gate-hook notify closure below forwards the member's
        // Suspend/Resume notices into this channel; the receiver rides
        // the returned handle so a swarm scheduler can stop the member's
        // attempt clock (ADR-011 c) and free its concurrency slot
        // (ADR-011 d) while it awaits a user decision.
        let (suspension_tx, suspension_rx) = tokio::sync::mpsc::unbounded_channel();
        let parent_agent_id = self.parent_agent_id.clone();
        let parent_permission_mode = self.parent_config.permission_mode;
        let nested_host_shared = nested_host_shared;
        // B4 T8 (D-55): the identity-registry op's payload.
        let registered_model = resolved_model.clone();
        let registered_thinking = binding.thinking.and_then(|effort| {
            serde_json::to_value(effort)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
        });
        let registered_swarm_item = request.swarm_item.clone();

        // The engine's flag registry — travels as an owned Arc so the'static'

        // completion future never borrows the host.

        let flag_registry = self.flag_registry.clone();

        // Settles the tracked entry's running flag on every path the
        // future does not reach itself — most importantly the DROP path
        // (G19: the parent's abort drops this future mid-flight, possibly
        // before its first poll — a swarm item still waiting in the
        // concurrency buffer is exactly that). The guard is moved into the
        // future's captured state below, so it fires whether or not the
        // future was ever polled.
        let running_guard = RunningGuard {
            children: Arc::clone(&children),
            agent_id: agent_id.clone(),
            event_bus: event_bus.clone(),
            settled: std::sync::atomic::AtomicBool::new(false),
        };

        // 8. Build the completion future — all async work happens here.
        let completion: CompletionFuture = Box::pin(
            // clippy:await_holding_lock is allowed for this whole body — the
            // child's parking_lot tools read guard deliberately spans the
            // run + distill phase (SDR-B4-01 risk table — the same
            // turn-long read-lock shape as the main agent's dispatch, made
            // Send through parking_lot's send_guard).
            #[allow(clippy::await_holding_lock)]
            async move {
                let running_guard = running_guard;
                let session = match session_weak.upgrade() {
                    Some(session) => session,
                    None => {
                        running_guard.mark_settled();
                        return Err(SubagentError::TurnFailed(
                            "session no longer exists".to_string(),
                        ));
                    }
                };

                // (a) Inherit the parent's LIVE permission mode (D-47, TS
                // setMode(parent.mode)): the config snapshot may be stale —
                // the parent's mode may have changed at runtime. Fall back to
                // the parent config's mode when the parent agent is not
                // registered (a directly-constructed host in tests).
                config.permission_mode = match session.get_agent(&parent_agent_id).await {
                    Some(parent) => parent.permission_manager().mode(),
                    None => parent_permission_mode,
                };

                // (a-2) Inherit the parent's LIVE reverse-RPC slot and
                // interaction kernel (ADR-009): the serve transport installs
                // the per-connection callback into the session's slot
                // after the host's create-time snapshot was taken. The SLOT
                // is the live handle (one shared Arc per session), so a
                // parent-registered slot overrides the create-time one;
                // the snapshot in `config` from host construction remains
                // the fallback when the live parent has none (or the parent
                // agent is not registered — test hosts).
                if let Some(parent) = session.get_agent(&parent_agent_id).await {
                    let (live_slot, live_kernel) = {
                        let cfg = parent.config();
                        let guard = cfg.read().unwrap_or_else(|e| e.into_inner());
                        (
                            guard.reverse_rpc_slot.clone(),
                            guard.interaction_kernel.clone(),
                        )
                    };
                    if live_slot.is_some() {
                        config.reverse_rpc_slot = live_slot;
                    }
                    if live_kernel.is_some() {
                        config.interaction_kernel = live_kernel;
                    }
                }

                // Create the child AgentScope.
                let agent = session
                    .create_agent(AgentId::new(&child_agent_id), config)
                    .await
                    .map_err(|e| {
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: e.to_string(),
                            },
                        );
                        // The agent was never created — there is nothing to
                        // resume, so the entry is removed outright (the one
                        // cleanup path left, see `cleanup_child`'s doc).
                        cleanup_child(&children, &child_agent_id);
                        running_guard.mark_settled();
                        SubagentError::TurnFailed(e.to_string())
                    })?;

                // spec 11 R5/A3: bind the nested host to the child's LIVE
                // config now that the child exists — the grandchild's Tier 3
                // inheritance reads the child's CURRENT model (a child that
                // later switches models passes the new model down).
                nested_host_shared.set_parent_config_shared(agent.config());

                // B4 T8 (D-55): the identity registry — `subagent.registered`
                // on the MAIN journal (session-domain), so a restart's replay
                // rebuilds the children metadata before any child journal
                // replays. Written only after the child AgentScope exists
                // (review Q3): a create_agent failure leaves no phantom
                // registry entry whose child journal will never appear
                // (restore would warn-and-skip on every restart).
                session
                    .record_op(WireOp::SubagentRegistered {
                        agent_id: child_agent_id.clone(),
                        profile_name: profile_name.clone(),
                        model: registered_model.clone(),
                        thinking_effort: registered_thinking.clone(),
                        parent_agent_id: Some(parent_agent_id.as_str().to_string()),
                        swarm_item: registered_swarm_item.clone(),
                        parent_tool_call_id: request.parent_tool_call_id.clone(),
                        swarm_index: request.swarm_index,
                    })
                    .await;

                // The child agent is live — announce the start on the session
                // bus (the `subagent.started` wire event).
                publish_subagent_event(
                    &event_bus,
                    SessionEvent::SubagentStarted {
                        subagent_id: child_agent_id.clone(),
                    },
                );

                // The child agent is live — notify the parent session's hook
                // engine that a subagent has started (fire-and-forget).
                fire_subagent_hook(
                    &session,
                    HookEventType::SubagentStart,
                    &child_agent_id,
                    &profile_name,
                    &resolved_model_str,
                )
                .await;

                // Guard ensures SubagentStop fires on every exit path, including
                // future cancellation (drop at an await point). Explicit `fire()`
                // calls below mark it as fired so Drop becomes a no-op on normal
                // return paths.
                let mut stop_guard = SubagentStopGuard {
                    session: session.clone(),
                    agent_id: child_agent_id.clone(),
                    profile_name: profile_name.clone(),
                    model: resolved_model_str.clone(),
                    fired: false,
                };

                // (c) Inherit the parent's user tools (D-47): every user tool
                // registered on the parent's ToolManager is re-registered on
                // the child's — strictly BEFORE the turn starts (the turn
                // holds the tools read guard for its whole duration, the same
                // turn-long read-lock shape as the main agent's dispatch;
                // SDR-B4-01 risk table).
                let parent_user_tools = session
                    .get_agent(&parent_agent_id)
                    .await
                    .map(|parent| {
                        let tools = parent.tool_manager();
                        let guard = tools.read();
                        guard.user_tools_snapshot()
                    })
                    .unwrap_or_default();
                if !parent_user_tools.is_empty() {
                    let child_tools = agent.tool_manager();
                    let mut child_tools = child_tools.write();
                    for tool in parent_user_tools {
                        child_tools.register_user_tool(tool);
                    }
                }

                // (c-2) Deliver the session's MCP tools (ADR-014: delivery
                // vs filtering separated): the main agent's tool table is the
                // engine fan-outs' continuously-maintained convergence point,
                // so its CURRENT MCP set is the spawn-time snapshot a child
                // must receive — the same seeding the restore path's
                // `hydrate_subagent` applies to a dormant child. A child
                // spawned after the last pool transition would otherwise
                // miss every registration (the fan-out iterates LIVE agents
                // only), leaving profile-declared `mcp__*` authorizations
                // vacuous (SDR-014's delivery obligation). Filtering stays
                // with the profile's tool whitelist: an unauthorized child
                // never activates these registrations.
                let main_mcp_tools = session
                    .get_agent(&AgentId::new(crate::session::manager::MAIN_AGENT_ID))
                    .await
                    .map(|main| {
                        let tools = main.tool_manager();
                        let guard = tools.read();
                        guard.mcp_tools_snapshot()
                    })
                    .unwrap_or_default();
                if !main_mcp_tools.is_empty() {
                    let child_tools = agent.tool_manager();
                    let mut child_tools = child_tools.write();
                    for (name, tool) in main_mcp_tools {
                        // The qualified name is the entry key; the server tag
                        // rides the registration (removal-by-server still
                        // works) — the restore-path seeding's identical
                        // parsing, kept verbatim.
                        let server = name
                            .strip_prefix("mcp__")
                            .and_then(|rest| rest.split_once("__"))
                            .map(|(server, _)| server.to_string())
                            .unwrap_or_else(|| name.clone());
                        child_tools.register_mcp_tool(server, tool);
                    }
                }

                // Create LLM for the child's model.
                // Extract the result inside the block so the RwLockReadGuard is
                // dropped before any `.await` — holding a non-Send guard across an
                // await makes the future !Send.
                let llm = {
                    let config_arc = agent.config();
                    let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                    llm_factory.create_llm(&config_guard)
                };
                let llm = match llm {
                    Ok(llm) => llm,
                    Err(message) => {
                        running_guard.mark_settled();
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: message.clone(),
                            },
                        );
                        // The agent exists — the entry stays for a later resume
                        // (G14), only the running flag settles (the drop guard
                        // below also covers this path).
                        stop_guard.fire().await;
                        return Err(SubagentError::TurnFailed(message));
                    }
                };

                // Wire up the TurnFlow and run one turn.
                let prompt_text = prompt.clone();
                let prompt_parts = vec![ContentPart::Text { text: prompt }];
                let profile_mgr = Arc::clone(&profile_manager);
                let turn_flow_handle = Arc::clone(&turn_flow_for_future);
                // The child's turn is announced under its own wire turn id
                // (`turn-<uuid>`, the same convention the main agent's dispatch
                // uses) and its live events stream onto the session bus through
                // the shared live-event relay — so subscribers observe the
                // child's `turn.step.*` and tool events while the subagent runs.
                let session_id_str = session.id().as_str().to_string();
                let wire_turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                let live_callback = event_bus.as_ref().map(|bus| {
                    crate::session::live_relay::live_event_callback(
                        bus,
                        &child_agent_id,
                        &session_id_str,
                        Some(wire_turn_id.clone()),
                    )
                });
                let event_bus_for_flow = event_bus.clone();
                // B4 T6 (D-61): the distillation phase re-runs the turn for a
                // short summary, so the LLM/cancel/live-callback handles stay
                // alive beyond the first run.
                let llm_for_distill = Arc::clone(&llm);
                let live_callback_for_distill = live_callback.clone();
                // B4 T7 (D-55): the child's own journal slot — its turn ops
                // record through the routing recorder (child-domain ops to
                // this slot, session-domain ops to the session's log). The
                // resumed child reuses the same slot the spawn face created
                // (the session registry keeps it across the faces).
                let child_journal = session.child_wire_log(&child_agent_id).await;
                let routed_recorder: Arc<dyn crate::loop_engine::WireOpRecorder> =
                    Arc::new(crate::session::journal_route::RoutedWireRecorder::new(
                        child_journal,
                        session.wire_recorder(),
                    ));
                // The dispatch-layer turn bracket (B4 T7): the same
                // turn.prompt + context.append_message records the embedded
                // main-agent dispatch writes, on the child's own journal.
                record_subagent_turn_prompt(&routed_recorder, &prompt_text).await;

                // B4 T4 (SDR-B4-01, Plan A): the turn runs as a plain async
                // body on the driving runtime — no spawn_blocking, no
                // block_in_place, no Handle::block_on. Nested dispatch is a
                // pure await tree (the session's main turn is the single
                // block_on left); dropping this future cancels the turn at
                // its await point (the M1 semantic change, see
                // `should_resume_cleanly_after_the_completion_future_is_dropped_mid_turn`).
                // The parking_lot read guard on the child's tools is Send, so
                // holding it across the whole turn keeps the future Send — the
                // same turn-long read-lock shape the main agent's dispatch
                // uses (a non-regression, SDR-B4-01 risk table).
                let turn_result = {
                    let turn_body = async {
                        let mut turn_flow = turn_flow_handle.lock().await;

                        // Wire services from the child AgentScope.
                        turn_flow.set_permission_manager(agent.permission_manager());
                        turn_flow.set_external_hooks(agent.hook_engine(), HashMap::new());
                        if let Some(registry) = &flag_registry {
                            turn_flow.set_flag_registry(std::sync::Arc::clone(registry));
                        }
                        turn_flow.set_task_manager(agent.tasks());
                        turn_flow.set_profile_manager(profile_mgr);
                        // F: goal usage accounting ONLY (the main-turn
                        // posture, `set_goal_usage_accounting`) — the full
                        // `set_goal_manager` would let the
                        // `should_continue_after_stop` Layer 2 see the
                        // session's active goal and force the child's turn
                        // to continue past its natural stop (the observed
                        // "standby" loop). The goal is the MAIN agent's
                        // loop; the child records usage against it and
                        // stops when its own task is done.
                        turn_flow.set_goal_usage_accounting(agent.goal());
                        turn_flow.set_plan_manager(agent.plan());
                        turn_flow.set_todo_list(agent.todo_list());
                        // US4.2 (SDR-002 parity): the subagent's stale-todo
                        // reminder must fire on the same condition the main
                        // agent's dispatch applies (embedded_server's
                        // message_handlers.rs) — only while TodoList is active
                        // on the child's own tool face. The gate is evaluated
                        // per inject call (never at construction), so a
                        // mid-session disable silences the NEXT injection; the
                        // closure takes its own read lock per call.
                        let gate_tool_manager = agent.tool_manager();
                        turn_flow.set_todo_tool_gate(Arc::new(move || {
                            gate_tool_manager.read().is_tool_active("TodoList")
                        }));
                        // Feed the child's live usage accumulator from its
                        // turn's steps (SDR-002 full aggregation): the
                        // child's usage lands in its OWN usage service, so
                        // the session snapshot (`runtime_extras`) sums it
                        // alongside the main agent's — the same shape the
                        // embedded main-agent dispatch wires (F5, P7c). The
                        // by_model key is the child's spawn-time model alias.
                        turn_flow.set_usage_sink(agent.usage_service());
                        turn_flow.set_usage_model_key(agent.model_id());
                        // The session bus (live turn.step/tool events) and the
                        // minted wire turn id the child announces under.
                        if let Some(ref bus) = event_bus_for_flow {
                            turn_flow.set_event_bus(Arc::clone(bus));
                        }
                        // CR4: the member's structured agent-status publisher —
                        // its `event.agent.status.updated` frames stream on
                        // the session bus under the child's agent-<uuid>
                        // identity, the same vocabulary the subagent
                        // lifecycle events use.
                        if let Some(ref bus) = event_bus_for_flow {
                            let compaction_handle = turn_flow.compaction_handle();
                            turn_flow.set_status_publisher(Some(Arc::new(
                                crate::session::agent_status::AgentStatusPublisher::new(
                                    Arc::clone(bus),
                                    child_agent_id.clone(),
                                    agent.context_memory(),
                                    compaction_handle,
                                    agent.usage_service(),
                                    agent.config(),
                                    agent.plan(),
                                    agent.swarm_mode(),
                                ),
                            )));
                        }
                        turn_flow.set_wire_turn_id(Some(wire_turn_id));
                        // B4 T7 (D-55): the child's turn ops persist to its
                        // own journal through the routing recorder — today
                        // the spawn face never wired a recorder at all
                        // (the plan's §0.2 gap).
                        turn_flow.set_wire_recorder(Some(Arc::clone(&routed_recorder)));
                        // ADR-009 (audit P0-1): the child's approval channel —
                        // the parent's reverse-RPC callback and interaction
                        // kernel ride the child config (inherited at spawn);
                        // without them a Manual-mode Write/Bash is hard-rejected
                        // locally ("approval required but no approval mechanism
                        // available") and the client never sees the ask. Read
                        // the child config like the permission manager wiring
                        // above — short critical section, no await across it.
                        // The SLOT resolves to the current callback here:
                        // the turn flow's permission-ask face needs the
                        // concrete callback snapshot, and resolving through
                        // the slot honors a late transport subscribe.
                        let (reverse_rpc_callback, interaction_kernel) = {
                            let config_arc = agent.config();
                            let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                            (
                                config_guard.reverse_rpc_slot.as_ref().and_then(|slot| {
                                    slot.read().unwrap_or_else(|e| e.into_inner()).clone()
                                }),
                                config_guard.interaction_kernel.clone(),
                            )
                        };
                        turn_flow.set_reverse_rpc_callback(reverse_rpc_callback);
                        turn_flow.set_interaction_kernel(interaction_kernel);
                        // ADR-011: the member's suspension boundary — the
                        // approval-gate edges of this child's turn report
                        // into a callback that feeds the scheduler's
                        // per-attempt channel (parking / resuming signals)
                        // and announces `subagent.suspended` /
                        // `subagent.started` on the session bus (pairing
                        // closure: every suspension is closed by a resume
                        // or by a terminal event from the host's settle
                        // paths). Wired the same ADR-009 way as the
                        // interaction kernel — the host is the single
                        // publisher, the turn core stays adjudication-only.
                        // Always wired for parented children: the batch
                        // scheduler reads the per-member feed even when no
                        // session bus exists.
                        let suspension_notify: Option<crate::subagent::SuspensionNotify> =
                            Some(Arc::new({
                                let bus = event_bus_for_flow.as_ref().map(Arc::clone);
                                let agent_id = child_agent_id.clone();
                                let suspension_tx = suspension_tx.clone();
                                move |notice: crate::subagent::SuspensionNotice| {
                                    // The scheduler's per-member feed
                                    // (ADR-011 c/d) — the signals that pause
                                    // the member's deadline and free its
                                    // slot while it awaits the user.
                                    let _ = suspension_tx.send(notice.clone());
                                    match notice {
                                        crate::subagent::SuspensionNotice::Suspended(cause) => {
                                            if let Some(bus) = &bus {
                                                bus.publish(SessionEvent::SubagentSuspended {
                                                    subagent_id: agent_id.clone(),
                                                    reason: cause,
                                                });
                                            }
                                        }
                                        crate::subagent::SuspensionNotice::Resumed => {
                                            if let Some(bus) = &bus {
                                                bus.publish(SessionEvent::SubagentStarted {
                                                    subagent_id: agent_id.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }));
                        turn_flow.set_suspension_notify(suspension_notify);
                        // F3: the scheduler-ONLY resume sink — the gate's
                        // decision-side wake delivers `Resumed` into the
                        // batch's per-attempt channel WITHOUT publishing the
                        // public `subagent.started` (that fires once, from
                        // the turn-side Resumed when the member actually
                        // resumes running). A decided member may still sit in
                        // the batch's ready queue waiting for a slot — a
                        // decide-time `started` would be premature.
                        let scheduler_resume: Option<crate::subagent::SuspensionNotify> =
                            Some(Arc::new({
                                let suspension_tx = suspension_tx.clone();
                                move |notice: crate::subagent::SuspensionNotice| {
                                    let _ = suspension_tx.send(notice);
                                }
                            }));
                        turn_flow.set_suspension_resume(scheduler_resume);

                        let tools_arc = agent.tool_manager();
                        let memory = agent.context_memory();
                        // The parking_lot send_guard read lock deliberately spans the whole
                        // turn / distill phase (SDR-B4-01 risk table — the same turn-long
                        // read-lock shape as the main agent's dispatch).
                        let tools = tools_arc.read();
                        let mut validators = ValidatorCache::new();

                        turn_flow
                            .run_one_turn(
                                prompt_parts,
                                PromptOrigin::SystemTrigger,
                                Arc::clone(&memory),
                                &tools,
                                // The child's provider is fixed at spawn
                                // time — the subagent host builds one LLM
                                // per child turn (no dynamic model).
                                Arc::new(FixedLlmResolver::new(llm)),
                                &cancel,
                                &mut validators,
                                live_callback,
                                // Subagent prompts never carry the admission
                                // stamp (ADR-006 — inline skill references
                                // resolve on the main session's send path).
                                None,
                            )
                            .await
                    };
                    // Panic isolation parity: the old spawn_blocking surface
                    // turned a panicking turn into a JoinError → "subagent
                    // task panicked". The async body has no JoinHandle, so the
                    // future is catch_unwind-wrapped to keep the same error
                    // mapping (a panic must not tear down the parent's turn).
                    futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(turn_body)).await
                };
                let result = match turn_result {
                    Ok(Ok(result)) => result,
                    Ok(Err(e)) => {
                        running_guard.mark_settled();
                        let error = map_turn_error(&e);
                        // A suppressed rate limit publishes NO terminal event —
                        // the swarm scheduler requeues the member on this same
                        // identity and owns the `subagent.suspended`
                        // announcement instead (TS `suppressRateLimitFailureEvent`).
                        // The entry still settles (running=false) so the
                        // retry-shaped resume is accepted.
                        let publish_failure = !(suppress_rate_limit_failure_event
                            && matches!(error, SubagentError::RateLimited(_)));
                        if publish_failure {
                            publish_subagent_event(
                                &event_bus,
                                SessionEvent::SubagentFailed {
                                    subagent_id: child_agent_id.clone(),
                                    error: e.to_string(),
                                },
                            );
                        }
                        settle_child(&children, &child_agent_id);
                        stop_guard.fire().await;
                        return Err(error);
                    }
                    Err(panic) => {
                        running_guard.mark_settled();
                        let message = panic_message(&panic);
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: format!("subagent task panicked: {message}"),
                            },
                        );
                        settle_child(&children, &child_agent_id);
                        stop_guard.fire().await;
                        return Err(SubagentError::TurnFailed(format!(
                            "subagent task panicked: {message}"
                        )));
                    }
                };

                tracing::info!(
                    "subagent {}: turn completed, stop_reason={:?}",
                    child_agent_id,
                    result.stop_reason
                );

                // B1: Check stop_reason for cancellation even when run_one_turn
                // returned Ok — an Aborted turn returns Ok with stop_reason=Aborted.
                if result.stop_reason == crate::loop_engine::TurnStopReason::Aborted {
                    running_guard.mark_settled();
                    publish_subagent_event(
                        &event_bus,
                        SessionEvent::SubagentFailed {
                            subagent_id: child_agent_id.clone(),
                            error: "cancelled".to_string(),
                        },
                    );
                    settle_child(&children, &child_agent_id);
                    stop_guard.fire().await;
                    return Err(SubagentError::Cancelled);
                }

                // B4 T6 (D-61): a truncated first turn fails with the
                // TS-verbatim max-tokens error (classifyTurnResult parity).
                if result.stop_reason == crate::loop_engine::TurnStopReason::MaxTokens {
                    running_guard.mark_settled();
                    publish_subagent_event(
                        &event_bus,
                        SessionEvent::SubagentFailed {
                            subagent_id: child_agent_id.clone(),
                            error: SUBAGENT_MAX_TOKENS_ERROR.to_string(),
                        },
                    );
                    settle_child(&children, &child_agent_id);
                    stop_guard.fire().await;
                    return Err(SubagentError::TurnFailed(
                        SUBAGENT_MAX_TOKENS_ERROR.to_string(),
                    ));
                }

                // Close the dispatch's turn bracket (B4 T7) — the TS kernel
                // parity `turn.ended` record.
                record_subagent_turn_ended(&routed_recorder, &result).await;

                // Extract final assistant text — through the profile's summary
                // policy when one is declared (B4 T6, D-61), raw otherwise.
                let result_text = match &summary_policy {
                    Some(policy) => {
                        let memory = agent.context_memory();
                        let tools_arc = agent.tool_manager();
                        // The parking_lot send_guard read lock deliberately spans the whole
                        // turn / distill phase (SDR-B4-01 risk table — the same turn-long
                        // read-lock shape as the main agent's dispatch). Lock-order
                        // note (review N6): this site takes tools-read → TurnFlow
                        // while the run body takes TurnFlow → tools-read — the
                        // reversal is safe only because the child's `running`
                        // flag serializes one run per child; keep it that way.
                        let tools = tools_arc.read();
                        let mut turn_flow = turn_flow_handle.lock().await;
                        // Panic isolation parity (review N1): the distill
                        // phase runs the same turn machinery as the run, so
                        // it is catch_unwind-wrapped too — a panic there
                        // keeps the "subagent task panicked" surface instead
                        // of escaping to the tool scheduler and surfacing as
                        // a misleading drop-guard "cancelled".
                        match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                            distill_summary(
                                &mut turn_flow,
                                &agent,
                                policy,
                                &memory,
                                &tools,
                                &llm_for_distill,
                                &cancel,
                                &mut ValidatorCache::new(),
                                &live_callback_for_distill,
                                &routed_recorder,
                            ),
                        ))
                        .await
                        {
                            Ok(Ok(text)) => text,
                            Ok(Err(error)) => {
                                running_guard.mark_settled();
                                let error = distill_error_to_subagent_error(error);
                                let error_text = match &error {
                                    SubagentError::TurnFailed(message)
                                    | SubagentError::RateLimited(message) => message.clone(),
                                    SubagentError::Cancelled => "cancelled".to_string(),
                                };
                                publish_subagent_event(
                                    &event_bus,
                                    SessionEvent::SubagentFailed {
                                        subagent_id: child_agent_id.clone(),
                                        error: error_text,
                                    },
                                );
                                settle_child(&children, &child_agent_id);
                                stop_guard.fire().await;
                                return Err(error);
                            }
                            Err(panic) => {
                                running_guard.mark_settled();
                                let message = panic_message(&panic);
                                publish_subagent_event(
                                    &event_bus,
                                    SessionEvent::SubagentFailed {
                                        subagent_id: child_agent_id.clone(),
                                        error: format!("subagent task panicked: {message}"),
                                    },
                                );
                                settle_child(&children, &child_agent_id);
                                stop_guard.fire().await;
                                return Err(SubagentError::TurnFailed(format!(
                                    "subagent task panicked: {message}"
                                )));
                            }
                        }
                    }
                    None => extract_last_assistant_text(&agent),
                };

                // ADR-009: the subagent turn settled — bump the session's
                // in-memory recency (the enclosing engine turn's settle
                // performs the durable write-through; see
                // [`bump_session_recency`]). Runs AFTER the distill phase,
                // so continuation turns are covered too.
                bump_session_recency(&session).await;

                // Extract token usage.
                let usage = TokenUsage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    cache_read_tokens: result.usage.cache_read_tokens,
                    cache_creation_tokens: result.usage.cache_creation_tokens,
                };

                // Announce the completion on the session bus (the
                // `subagent.completed` wire event) before the result hands back
                // to the parent tool call.
                publish_subagent_event(
                    &event_bus,
                    SessionEvent::SubagentCompleted {
                        subagent_id: child_agent_id.clone(),
                        result_summary: result_text.clone(),
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                );

                // Settle the tracked entry — the child stays resumable (G14).
                settle_child(&children, &child_agent_id);

                stop_guard.fire().await;

                running_guard.mark_settled();
                Ok(SubagentCompletion {
                    result: result_text,
                    usage: Some(usage),
                    stop_reason: result.stop_reason.clone(),
                })
            },
        );

        Ok(
            SubagentHandle::new(agent_id, request.profile_name, resolved_model, completion)
                .with_activity(activity_tracker)
                .with_wait_registry(child_registry)
                .with_suspension_rx(suspension_rx),
        )
    }

    fn forget_child(&self, agent_id: &str) {
        // A dispatch rejected after the spawn side effects (the residual
        // capacity race in the Agent tool): the completion future will
        // never be polled, and the child never ran — there is nothing to
        // resume, so the entry is removed outright (the one cleanup path
        // left, see `cleanup_child`'s doc).
        cleanup_child(&self.children, agent_id);
    }

    fn notify_suspended(&self, agent_id: &str, cause: &SuspendCause) {
        // The `subagent.suspended` wire event — published by the swarm
        // scheduler when it parks a member for a rate-limit retry (the TS
        // service layer's `launcher.suspended` callback face), and by the
        // approval-gate suspension callback the host wires into a member's
        // turn flow. The reason is the structured ADR-011 cause — the wire
        // carries `{ kind, request_id?, tool_call_id? }`.
        self.publish_event(SessionEvent::SubagentSuspended {
            subagent_id: agent_id.to_string(),
            reason: cause.clone(),
        });
    }

    fn swarm_item_of(&self, agent_id: &str) -> Option<String> {
        let children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children
            .get(agent_id)
            .and_then(|entry| entry.swarm_item.clone())
    }

    fn is_child_running(&self, agent_id: &str) -> bool {
        let children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children.get(agent_id).is_some_and(|entry| entry.running)
    }

    fn claim_child_notification_turn(&self, agent_id: &str) -> Option<NotificationRunGuard> {
        // The claim is the atomic form of `is_child_running`'s probe (review
        // round 2, F1): take-and-set under the same map lock the spawn /
        // resume paths use, so a host-driven run started concurrently
        // either sees the flag already claimed (and its resume rejects) or
        // claims first (and this returns None — the caller parks).
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let entry = children.get_mut(agent_id)?;
        if entry.running {
            return None;
        }
        entry.running = true;
        Some(NotificationRunGuard {
            children: Some(Arc::clone(&self.children)),
            agent_id: agent_id.to_string(),
        })
    }

    fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
        // 1. Validate the child exists, is not running, and claim the run
        //    slot (G13): the check-and-set under the map lock makes a
        //    concurrent second resume impossible — the pre-fix code had no
        //    running-state flag, so a mid-run resume queued a second turn on
        //    the same TurnFlow.
        let (profile_name, model, turn_flow, activity) = {
            let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
            let entry = children.get_mut(&request.agent_id).ok_or_else(|| {
                EngineError::RequestInvalid(format!(
                    "subagent '{}' not found (not created by this host)",
                    request.agent_id
                ))
            })?;
            if entry.running {
                return Err(EngineError::RequestInvalid(format!(
                    "subagent '{}' is already running",
                    request.agent_id
                )));
            }
            entry.running = true;
            (
                entry.profile_name.clone(),
                entry.model.clone(),
                Arc::clone(&entry.turn_flow),
                Arc::clone(&entry.activity),
            )
        };

        // 2. Capture for completion future.
        let session_weak = self.session.clone();
        let profile_manager = Arc::clone(&self.profile_manager);
        let llm_factory = Arc::clone(&self.llm_factory);
        let children = Arc::clone(&self.children);
        let prompt = request.prompt.clone();
        let cancel = request.cancel.clone();
        // B4 T6 (D-61): the resumed child distills its summary under the
        // same profile policy as a fresh spawn (the entry's profile name
        // resolves the policy).
        let summary_policy = self
            .profile_manager
            .resolve_profile(&profile_name)
            .ok()
            .and_then(|profile| profile.summary_policy.clone());
        let retry = request.retry;
        let suppress_rate_limit_failure_event = request.suppress_rate_limit_failure_event;
        let child_agent_id = request.agent_id.clone();
        let event_bus = self.event_bus.clone();
        // ADR-011: the per-attempt suspension boundary feed (see the spawn
        // face's wiring comment) — the resumed member's gate notices ride
        // the same channel to the swarm scheduler.
        let (suspension_tx, suspension_rx) = tokio::sync::mpsc::unbounded_channel();
        // The handle returned to the caller carries these; the future needs
        // its own copies for the hook/event payloads.
        let entry_profile_name = profile_name.clone();
        let entry_model = model.clone();
        // The engine's flag registry — travels as an owned Arc so the
        // `'static` completion future never borrows the host.
        let flag_registry = self.flag_registry.clone();

        // Settles the running flag on every path this future does not reach
        // itself — including the drop-before-first-poll path (G19). Created
        // outside the async block so it lives in the future's captured
        // state from construction.
        let running_guard = RunningGuard {
            children: Arc::clone(&children),
            agent_id: request.agent_id.clone(),
            event_bus: event_bus.clone(),
            settled: std::sync::atomic::AtomicBool::new(false),
        };

        // 3. Build completion future — the same run face as spawn (G17):
        //    SubagentStart hook, SubagentStop guard, `subagent.failed` on
        //    every error path, settle (not removal) of the tracked entry.
        let completion: CompletionFuture = Box::pin(
            // clippy:await_holding_lock is allowed for this whole body — the
            // child's parking_lot tools read guard deliberately spans the
            // run + distill phase (SDR-B4-01 risk table — the same
            // turn-long read-lock shape as the main agent's dispatch, made
            // Send through parking_lot's send_guard).
            #[allow(clippy::await_holding_lock)]
            async move {
                let running_guard = running_guard;
                let session = match session_weak.upgrade() {
                    Some(session) => session,
                    None => {
                        running_guard.mark_settled();
                        return Err(SubagentError::TurnFailed(
                            "session no longer exists".to_string(),
                        ));
                    }
                };

                // Load existing child AgentScope — lazily hydrating a
                // registered-but-dormant child first (ADR-006/ADR-017: a
                // resumed session's children stay dormant until first wake).
                // `hydrate_subagent` is a no-op for a child that is already
                // live or not registered.
                session.hydrate_subagent(&child_agent_id).await;
                let agent = match session.get_agent(&AgentId::new(&child_agent_id)).await {
                    Some(agent) => agent,
                    None => {
                        running_guard.mark_settled();
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: "agent scope not found in session".to_string(),
                            },
                        );
                        return Err(SubagentError::TurnFailed(format!(
                            "subagent '{child_agent_id}' not found in session"
                        )));
                    }
                };

                let profile_name_str = entry_profile_name.clone();
                let resolved_model_str = entry_model.clone().unwrap_or_default();

                // The resumed child is live again — announce the start on the
                // session bus (the `subagent.started` wire event).
                publish_subagent_event(
                    &event_bus,
                    SessionEvent::SubagentStarted {
                        subagent_id: child_agent_id.clone(),
                    },
                );

                // The resumed child runs the same hook face as a fresh spawn
                // (G17): the parent session's SubagentStart fires here, and the
                // stop guard below pairs it on every exit path.
                fire_subagent_hook(
                    &session,
                    HookEventType::SubagentStart,
                    &child_agent_id,
                    &profile_name_str,
                    &resolved_model_str,
                )
                .await;

                let mut stop_guard = SubagentStopGuard {
                    session: session.clone(),
                    agent_id: child_agent_id.clone(),
                    profile_name: profile_name_str.clone(),
                    model: resolved_model_str.clone(),
                    fired: false,
                };

                // Create LLM for the child's model.
                let llm = {
                    let config_arc = agent.config();
                    let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                    llm_factory.create_llm(&config_guard)
                };
                let llm = match llm {
                    Ok(llm) => llm,
                    Err(message) => {
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: message.clone(),
                            },
                        );
                        settle_child(&children, &child_agent_id);
                        stop_guard.fire().await;
                        return Err(SubagentError::TurnFailed(message));
                    }
                };

                // Wire up and run one turn (reusing the stored TurnFlow). The
                // retry shape (rate-limit requeue, B3 T2) re-runs the failed
                // turn with NO new prompt — TS `promptService.retry()`: same run
                // face, the memory's last user message drives the loop.
                let prompt_text = if retry { None } else { Some(prompt.clone()) };
                let prompt_parts = prompt_text
                    .clone()
                    .map(|text| vec![ContentPart::Text { text }]);
                let profile_mgr = Arc::clone(&profile_manager);
                let turn_flow_handle = Arc::clone(&turn_flow);
                // The resumed child's turn is announced under a fresh wire turn
                // id and its live events stream onto the session bus — the same
                // wiring the spawn path applies (see `spawn`).
                let session_id_str = session.id().as_str().to_string();
                let wire_turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                let live_callback = event_bus.as_ref().map(|bus| {
                    crate::session::live_relay::live_event_callback(
                        bus,
                        &child_agent_id,
                        &session_id_str,
                        Some(wire_turn_id.clone()),
                    )
                });
                let event_bus_for_flow = event_bus.clone();
                // B4 T6 (D-61): keep the LLM/cancel/live-callback handles for
                // the distillation phase (see the spawn path).
                let llm_for_distill = Arc::clone(&llm);
                let live_callback_for_distill = live_callback.clone();
                // B4 T7 (D-55): the child's own journal slot — its turn ops
                // record through the routing recorder (child-domain ops to
                // this slot, session-domain ops to the session's log). The
                // resumed child reuses the same slot the spawn face created
                // (the session registry keeps it across the faces).
                let child_journal = session.child_wire_log(&child_agent_id).await;
                let routed_recorder: Arc<dyn crate::loop_engine::WireOpRecorder> =
                    Arc::new(crate::session::journal_route::RoutedWireRecorder::new(
                        child_journal,
                        session.wire_recorder(),
                    ));
                // The dispatch-layer turn bracket (B4 T7, review S2): a
                // non-retry resume IS a new dispatch — without the bracket a
                // later restart folds a child memory missing every resume
                // instruction (the journal would carry orphan `turn.ended`
                // records with no opening). The retry shape keeps its
                // no-prompt semantics: nothing to bracket.
                if let Some(text) = &prompt_text {
                    record_subagent_turn_prompt(&routed_recorder, text).await;
                }

                // B4 T4: the same pure-async run face as the spawn path — no
                // spawn_blocking / block_in_place / Handle::block_on (see the
                // spawn path's comment).
                let turn_result = {
                    let turn_body = async {
                        let mut turn_flow = turn_flow_handle.lock().await;

                        turn_flow.set_permission_manager(agent.permission_manager());
                        turn_flow.set_external_hooks(agent.hook_engine(), HashMap::new());
                        if let Some(registry) = &flag_registry {
                            turn_flow.set_flag_registry(std::sync::Arc::clone(registry));
                        }
                        turn_flow.set_task_manager(agent.tasks());
                        turn_flow.set_profile_manager(profile_mgr);
                        // F: goal usage accounting ONLY (the main-turn
                        // posture, `set_goal_usage_accounting`) — the full
                        // `set_goal_manager` would let the
                        // `should_continue_after_stop` Layer 2 see the
                        // session's active goal and force the child's turn
                        // to continue past its natural stop (the observed
                        // "standby" loop). The goal is the MAIN agent's
                        // loop; the child records usage against it and
                        // stops when its own task is done.
                        turn_flow.set_goal_usage_accounting(agent.goal());
                        turn_flow.set_plan_manager(agent.plan());
                        turn_flow.set_todo_list(agent.todo_list());
                        // US4.2 (SDR-002 parity): the resumed child gets the
                        // same reminder gate as the spawn face — the identical
                        // wiring as the main agent's dispatch (see the spawn
                        // face's comment above for the per-call evaluation
                        // semantics).
                        let gate_tool_manager = agent.tool_manager();
                        turn_flow.set_todo_tool_gate(Arc::new(move || {
                            gate_tool_manager.read().is_tool_active("TodoList")
                        }));
                        // Feed the resumed child's usage accumulator too
                        // (SDR-002 — see the spawn face's comment): the
                        // sink persists on the shared flow handle, so the
                        // distillation continuations record as well.
                        turn_flow.set_usage_sink(agent.usage_service());
                        turn_flow.set_usage_model_key(agent.model_id());
                        // The session bus (live turn.step/tool events) and the
                        // minted wire turn id the resumed child announces under.
                        if let Some(ref bus) = event_bus_for_flow {
                            turn_flow.set_event_bus(Arc::clone(bus));
                        }
                        // CR4: the member's structured agent-status publisher —
                        // the same wiring the spawn face applies, so the
                        // resumed child's phase transitions keep publishing
                        // `event.agent.status.updated` frames on the session
                        // bus (the distillation continuation's do too — the
                        // publisher persists on the shared flow handle).
                        if let Some(ref bus) = event_bus_for_flow {
                            let compaction_handle = turn_flow.compaction_handle();
                            turn_flow.set_status_publisher(Some(Arc::new(
                                crate::session::agent_status::AgentStatusPublisher::new(
                                    Arc::clone(bus),
                                    child_agent_id.clone(),
                                    agent.context_memory(),
                                    compaction_handle,
                                    agent.usage_service(),
                                    agent.config(),
                                    agent.plan(),
                                    agent.swarm_mode(),
                                ),
                            )));
                        }
                        turn_flow.set_wire_turn_id(Some(wire_turn_id));
                        // B4 T7 (D-55): the child's turn ops persist to its
                        // own journal through the routing recorder — today
                        // the spawn face never wired a recorder at all
                        // (the plan's §0.2 gap).
                        turn_flow.set_wire_recorder(Some(Arc::clone(&routed_recorder)));
                        // ADR-009 (audit P0-1): the resumed child keeps the
                        // parent's approval channel — see the spawn face's
                        // wiring comment. The SLOT resolves to the current
                        // callback (same late-subscribe honoring as the
                        // spawn face above).
                        let (reverse_rpc_callback, interaction_kernel) = {
                            let config_arc = agent.config();
                            let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                            (
                                config_guard.reverse_rpc_slot.as_ref().and_then(|slot| {
                                    slot.read().unwrap_or_else(|e| e.into_inner()).clone()
                                }),
                                config_guard.interaction_kernel.clone(),
                            )
                        };
                        turn_flow.set_reverse_rpc_callback(reverse_rpc_callback);
                        turn_flow.set_interaction_kernel(interaction_kernel);
                        // ADR-011: the resumed member's suspension boundary
                        // — same wiring as the spawn face (see its comment):
                        // the gate edges feed the scheduler's per-attempt
                        // channel and announce `subagent.suspended` /
                        // `subagent.started` on the session bus.
                        let suspension_notify: Option<crate::subagent::SuspensionNotify> =
                            Some(Arc::new({
                                let bus = event_bus_for_flow.as_ref().map(Arc::clone);
                                let agent_id = child_agent_id.clone();
                                let suspension_tx = suspension_tx.clone();
                                move |notice: crate::subagent::SuspensionNotice| {
                                    let _ = suspension_tx.send(notice.clone());
                                    match notice {
                                        crate::subagent::SuspensionNotice::Suspended(cause) => {
                                            if let Some(bus) = &bus {
                                                bus.publish(SessionEvent::SubagentSuspended {
                                                    subagent_id: agent_id.clone(),
                                                    reason: cause,
                                                });
                                            }
                                        }
                                        crate::subagent::SuspensionNotice::Resumed => {
                                            if let Some(bus) = &bus {
                                                bus.publish(SessionEvent::SubagentStarted {
                                                    subagent_id: agent_id.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }));
                        turn_flow.set_suspension_notify(suspension_notify);
                        // F3: the scheduler-ONLY resume sink — the gate's
                        // decision-side wake delivers `Resumed` into the
                        // batch's per-attempt channel WITHOUT publishing the
                        // public `subagent.started` (that fires once, from
                        // the turn-side Resumed when the member actually
                        // resumes running). A decided member may still sit in
                        // the batch's ready queue waiting for a slot — a
                        // decide-time `started` would be premature.
                        let scheduler_resume: Option<crate::subagent::SuspensionNotify> =
                            Some(Arc::new({
                                let suspension_tx = suspension_tx.clone();
                                move |notice: crate::subagent::SuspensionNotice| {
                                    let _ = suspension_tx.send(notice);
                                }
                            }));
                        turn_flow.set_suspension_resume(scheduler_resume);

                        let tools_arc = agent.tool_manager();
                        let memory = agent.context_memory();
                        // The parking_lot send_guard read lock deliberately spans the whole
                        // turn / distill phase (SDR-B4-01 risk table — the same turn-long
                        // read-lock shape as the main agent's dispatch).
                        let tools = tools_arc.read();
                        let mut validators = ValidatorCache::new();

                        match prompt_parts {
                            Some(parts) => {
                                turn_flow
                                    .run_one_turn(
                                        parts,
                                        PromptOrigin::SystemTrigger,
                                        Arc::clone(&memory),
                                        &tools,
                                        // Fixed provider — see the spawn
                                        // face's comment.
                                        Arc::new(FixedLlmResolver::new(llm)),
                                        &cancel,
                                        &mut validators,
                                        live_callback,
                                        // No admission stamp on subagent
                                        // prompts (ADR-006).
                                        None,
                                    )
                                    .await
                            }
                            None => {
                                turn_flow
                                    .retry_one_turn(
                                        memory,
                                        &tools,
                                        Arc::new(FixedLlmResolver::new(llm)),
                                        &cancel,
                                        &mut validators,
                                        live_callback,
                                    )
                                    .await
                            }
                        }
                    };
                    // Panic isolation parity — see the spawn path's comment.
                    futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(turn_body)).await
                };
                let result = match turn_result {
                    Ok(Ok(result)) => result,
                    Ok(Err(e)) => {
                        running_guard.mark_settled();
                        let error = map_turn_error(&e);
                        // A suppressed rate limit publishes NO terminal event —
                        // the swarm scheduler requeues the member on this same
                        // identity and owns the `subagent.suspended`
                        // announcement instead (TS `suppressRateLimitFailureEvent`).
                        // The entry still settles (running=false) so the
                        // retry-shaped resume is accepted.
                        let publish_failure = !(suppress_rate_limit_failure_event
                            && matches!(error, SubagentError::RateLimited(_)));
                        if publish_failure {
                            publish_subagent_event(
                                &event_bus,
                                SessionEvent::SubagentFailed {
                                    subagent_id: child_agent_id.clone(),
                                    error: e.to_string(),
                                },
                            );
                        }
                        settle_child(&children, &child_agent_id);
                        stop_guard.fire().await;
                        return Err(error);
                    }
                    Err(panic) => {
                        running_guard.mark_settled();
                        let message = panic_message(&panic);
                        publish_subagent_event(
                            &event_bus,
                            SessionEvent::SubagentFailed {
                                subagent_id: child_agent_id.clone(),
                                error: format!("subagent task panicked: {message}"),
                            },
                        );
                        settle_child(&children, &child_agent_id);
                        stop_guard.fire().await;
                        return Err(SubagentError::TurnFailed(format!(
                            "subagent task panicked: {message}"
                        )));
                    }
                };

                // B1: Check stop_reason for cancellation.
                if result.stop_reason == crate::loop_engine::TurnStopReason::Aborted {
                    running_guard.mark_settled();
                    publish_subagent_event(
                        &event_bus,
                        SessionEvent::SubagentFailed {
                            subagent_id: child_agent_id.clone(),
                            error: "cancelled".to_string(),
                        },
                    );
                    settle_child(&children, &child_agent_id);
                    stop_guard.fire().await;
                    return Err(SubagentError::Cancelled);
                }

                // B4 T6 (D-61): a truncated resumed turn fails with the
                // TS-verbatim max-tokens error (classifyTurnResult parity).
                if result.stop_reason == crate::loop_engine::TurnStopReason::MaxTokens {
                    running_guard.mark_settled();
                    publish_subagent_event(
                        &event_bus,
                        SessionEvent::SubagentFailed {
                            subagent_id: child_agent_id.clone(),
                            error: SUBAGENT_MAX_TOKENS_ERROR.to_string(),
                        },
                    );
                    settle_child(&children, &child_agent_id);
                    stop_guard.fire().await;
                    return Err(SubagentError::TurnFailed(
                        SUBAGENT_MAX_TOKENS_ERROR.to_string(),
                    ));
                }

                // Close the dispatch's turn bracket (B4 T7) — the TS kernel
                // parity `turn.ended` record.
                record_subagent_turn_ended(&routed_recorder, &result).await;

                // Extract final assistant text — through the profile's summary
                // policy when one is declared (B4 T6, D-61), raw otherwise.
                let result_text = match &summary_policy {
                    Some(policy) => {
                        let memory = agent.context_memory();
                        let tools_arc = agent.tool_manager();
                        // The parking_lot send_guard read lock deliberately spans the whole
                        // turn / distill phase (SDR-B4-01 risk table — the same turn-long
                        // read-lock shape as the main agent's dispatch). Lock-order
                        // note (review N6): this site takes tools-read → TurnFlow
                        // while the run body takes TurnFlow → tools-read — the
                        // reversal is safe only because the child's `running`
                        // flag serializes one run per child; keep it that way.
                        let tools = tools_arc.read();
                        let mut turn_flow = turn_flow_handle.lock().await;
                        // Panic isolation parity (review N1): the distill
                        // phase runs the same turn machinery as the run, so
                        // it is catch_unwind-wrapped too — a panic there
                        // keeps the "subagent task panicked" surface instead
                        // of escaping to the tool scheduler and surfacing as
                        // a misleading drop-guard "cancelled".
                        match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                            distill_summary(
                                &mut turn_flow,
                                &agent,
                                policy,
                                &memory,
                                &tools,
                                &llm_for_distill,
                                &cancel,
                                &mut ValidatorCache::new(),
                                &live_callback_for_distill,
                                &routed_recorder,
                            ),
                        ))
                        .await
                        {
                            Ok(Ok(text)) => text,
                            Ok(Err(error)) => {
                                running_guard.mark_settled();
                                let error = distill_error_to_subagent_error(error);
                                let error_text = match &error {
                                    SubagentError::TurnFailed(message)
                                    | SubagentError::RateLimited(message) => message.clone(),
                                    SubagentError::Cancelled => "cancelled".to_string(),
                                };
                                publish_subagent_event(
                                    &event_bus,
                                    SessionEvent::SubagentFailed {
                                        subagent_id: child_agent_id.clone(),
                                        error: error_text,
                                    },
                                );
                                settle_child(&children, &child_agent_id);
                                stop_guard.fire().await;
                                return Err(error);
                            }
                            Err(panic) => {
                                running_guard.mark_settled();
                                let message = panic_message(&panic);
                                publish_subagent_event(
                                    &event_bus,
                                    SessionEvent::SubagentFailed {
                                        subagent_id: child_agent_id.clone(),
                                        error: format!("subagent task panicked: {message}"),
                                    },
                                );
                                settle_child(&children, &child_agent_id);
                                stop_guard.fire().await;
                                return Err(SubagentError::TurnFailed(format!(
                                    "subagent task panicked: {message}"
                                )));
                            }
                        }
                    }
                    None => extract_last_assistant_text(&agent),
                };
                // ADR-009: the subagent turn settled — bump the session's
                // in-memory recency (the enclosing engine turn's settle
                // performs the durable write-through; see
                // [`bump_session_recency`]). Runs AFTER the distill phase,
                // so continuation turns are covered too.
                bump_session_recency(&session).await;
                let usage = TokenUsage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    cache_read_tokens: result.usage.cache_read_tokens,
                    cache_creation_tokens: result.usage.cache_creation_tokens,
                };

                publish_subagent_event(
                    &event_bus,
                    SessionEvent::SubagentCompleted {
                        subagent_id: child_agent_id.clone(),
                        result_summary: result_text.clone(),
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                );

                // The entry stays for the next resume (G14) — only the running
                // flag settles.
                settle_child(&children, &child_agent_id);
                stop_guard.fire().await;

                running_guard.mark_settled();
                Ok(SubagentCompletion {
                    result: result_text,
                    usage: Some(usage),
                    stop_reason: result.stop_reason.clone(),
                })
            },
        );

        Ok(
            SubagentHandle::new(request.agent_id, profile_name, model, completion)
                .with_activity(activity)
                .with_suspension_rx(suspension_rx),
        )
    }

    fn curated_models(&self) -> Vec<crate::model_catalog::ModelCatalogEntry> {
        self.model_catalog
            .as_ref()
            .map(|catalog| catalog.curated_models())
            .unwrap_or_default()
    }

    fn available_profiles(&self) -> Vec<super::SubagentProfileInfo> {
        // The caller's effective subagent allowlist (R3-4, default-deny)
        // intersects the dispatchable set — the tool-description face:
        // the LLM only perceives profiles the caller may dispatch. The
        // filter lives HERE, not in the tools, because the host is the
        // only component that knows the calling agent's profile context;
        // both the `Agent` and `AgentSwarm` descriptions therefore list
        // only allowlist-allowed profiles.
        let allowlist = self.effective_allowlist();
        // Collect the catalog entries once — every profile's capabilities
        // and the shared "available models" list come from the same snapshot.
        let catalog_entries: Vec<crate::model_catalog::ModelCatalogEntry> = self
            .model_catalog
            .as_ref()
            .map(|catalog| catalog.list_models())
            .unwrap_or_default();

        let mut available_models: Vec<String> =
            catalog_entries.iter().map(|e| e.id.clone()).collect();
        available_models.sort_unstable();
        available_models.dedup();

        self.profile_manager
            .list()
            .into_iter()
            .filter(|name| allowlist.contains(name))
            .map(|name| {
                // Self-contained profile (a legacy `extends` was already
                // flattened at load time — R2-2) — feeds the
                // description/when_to_use metadata shown to the LLM.
                // Task 15 hygiene: resolve the name ONCE — the scope-config
                // half (the model id) is computed from the already-resolved
                // profile instead of a second `resolve` call.
                let profile = self.profile_manager.resolve_profile(&name).ok();
                let model = profile
                    .as_ref()
                    .and_then(|p| {
                        crate::profile::ProfileManager::scope_config_from_profile(
                            p,
                            &mirri_os::LocalSyncFs,
                        )
                        .ok()
                    })
                    .map(|config| config.model_id);
                // Resolve the profile's model capabilities: match by alias id
                // first, then by the raw model name, so profile models that
                // are provider model ids still pick up known capabilities.
                let model_capabilities = model
                    .as_deref()
                    .and_then(|model_name| {
                        catalog_entries
                            .iter()
                            .find(|e| e.id == model_name || e.model == model_name)
                    })
                    .map(|entry| entry.capabilities.clone())
                    .unwrap_or_default();
                super::SubagentProfileInfo {
                    name,
                    model,
                    description: profile.as_ref().and_then(|p| p.description.clone()),
                    when_to_use: profile.as_ref().and_then(|p| p.when_to_use.clone()),
                    tools: profile.as_ref().and_then(|p| p.tools.clone()),
                    disallowed_tools: profile.as_ref().and_then(|p| p.disallowed_tools.clone()),
                    model_capabilities,
                    available_models: available_models.clone(),
                }
            })
            .collect()
    }
}

/// Mark a child's tracked entry as settled — not running, kept for resume
/// (G14: a completed, failed, or cancelled child stays in the `children`
/// table for the session's lifetime, exactly like the TS host, so the
/// `<resume_hint>` the tools hand out is honest).
fn settle_child(children: &Mutex<HashMap<String, ChildEntry>>, agent_id: &str) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = guard.get_mut(agent_id) {
        entry.running = false;
    }
}

/// Classify a turn error for the subagent surface — the shared seam of the
/// spawn and resume completion paths (TS `isProviderRateLimitError`
/// excluding billing-anchored quota exhaustion):
/// - `EngineError::ProviderRateLimit` → [`SubagentError::RateLimited`] (the
///   swarm scheduler requeues the member on the same identity);
/// - `ProviderQuotaExhausted` (billing 429) is terminal →
///   [`SubagentError::TurnFailed`];
/// - aborts (user/system cancellation) → [`SubagentError::Cancelled`];
/// - everything else → [`SubagentError::TurnFailed`].
fn map_turn_error(error: &EngineError) -> SubagentError {
    match error {
        EngineError::ProviderRateLimit(msg) => SubagentError::RateLimited(msg.clone()),
        e if e.is_abort() => SubagentError::Cancelled,
        e => SubagentError::TurnFailed(e.to_string()),
    }
}

/// The max-tokens truncation failure text for a subagent turn (B4 T6,
/// D-61) — verbatim TS (`runAgentTurn.ts:36-37`).
pub(crate) const SUBAGENT_MAX_TOKENS_ERROR: &str =
    "Subagent turn failed before completing its final summary: reason=max_tokens";

/// The distillation outcome (B4 T6): a short summary either passes, gets
/// continued, or the turn was truncated.
enum DistillError {
    /// A continuation turn itself failed with an engine error.
    Turn(EngineError),
    /// A turn was truncated at max output tokens — the TS parity failure.
    MaxTokens,
}

/// Distill the child's final summary against the profile's
/// [`ProfileSummaryPolicy`] (B4 T6, D-61 — TS `runAgentTurn.ts:126-159`):
///
/// - a summary already meeting `min_chars` passes unchanged;
/// - a short summary is re-distilled by continuing the turn with
///   `continuation_prompt`, at most `retries` times (a non-empty continued
///   text updates the working summary; adequacy stops the loop);
/// - a truncated turn (`stop_reason = MaxTokens`, first turn included)
///   fails with the TS-verbatim [`SUBAGENT_MAX_TOKENS_ERROR`].
///
/// Shared by the spawn and resume faces (the G17 run-face principle): both
/// hold the turn-flow mutex and the tools read guard for the whole
/// run-plus-distill phase.
#[allow(clippy::too_many_arguments)]
async fn distill_summary(
    turn_flow: &mut tokio::sync::MutexGuard<'_, TurnFlow>,
    agent: &AgentScope,
    policy: &ProfileSummaryPolicy,
    memory: &Arc<Mutex<crate::context_memory::ContextMemory>>,
    tools: &crate::tool_registry::ToolManager,
    llm: &Arc<dyn Llm>,
    cancel: &crate::CancelToken,
    validators: &mut ValidatorCache,
    live_callback: &Option<crate::loop_engine::LiveEventCallback>,
    recorder: &Arc<dyn crate::loop_engine::WireOpRecorder>,
) -> Result<String, DistillError> {
    let mut summary = extract_last_assistant_text(agent);
    if summary.trim().len() >= policy.min_chars {
        return Ok(summary);
    }
    for _ in 0..policy.retries {
        // Each continuation is a real turn on the child's memory, so it
        // gets the same dispatch bracket on the child journal (review SF-2)
        // — without it a later restart folds a memory missing every
        // continuation prompt.
        record_subagent_turn_prompt(recorder, &policy.continuation_prompt).await;
        let result = turn_flow
            .run_one_turn(
                vec![ContentPart::Text {
                    text: policy.continuation_prompt.clone(),
                }],
                PromptOrigin::SystemTrigger,
                Arc::clone(memory),
                tools,
                Arc::new(FixedLlmResolver::new(Arc::clone(llm))),
                cancel,
                validators,
                live_callback.clone(),
                // Distill continuations are engine-internal system turns —
                // no admission stamp (ADR-006).
                None,
            )
            .await
            .map_err(DistillError::Turn)?;
        record_subagent_turn_ended(recorder, &result).await;
        if result.stop_reason == crate::loop_engine::TurnStopReason::MaxTokens {
            return Err(DistillError::MaxTokens);
        }
        let continued = extract_last_assistant_text(agent);
        if !continued.trim().is_empty() {
            summary = continued;
        }
        if summary.trim().len() >= policy.min_chars {
            break;
        }
    }
    Ok(summary)
}

/// Record the subagent dispatch's turn bracket on the child's journal (B4
/// T7, D-55): `turn.prompt` + `context.append_message` before the run (the
/// same dispatch-layer records the embedded main-agent path writes on the
/// session log) — the run's loop events, usage, and drift markers ride
/// through the TurnFlow's recorder; `turn.ended` closes the bracket after.
async fn record_subagent_turn_prompt(
    recorder: &Arc<dyn crate::loop_engine::WireOpRecorder>,
    prompt: &str,
) {
    use mirri_proto::WireOp;
    recorder
        .record_op(WireOp::TurnPrompt {
            input: serde_json::json!([{ "type": "text", "text": prompt }]),
            origin: serde_json::json!({ "kind": "system_trigger", "name": "subagent" }),
        })
        .await;
    recorder
        .record_op(WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": [{ "type": "text", "text": prompt }],
                "origin": { "kind": "system_trigger", "name": "subagent" },
            }),
        })
        .await;
}

/// Record the closing `turn.ended` bracket (see
/// [`record_subagent_turn_prompt`]) — the reason carries the TS wire word.
async fn record_subagent_turn_ended(
    recorder: &Arc<dyn crate::loop_engine::WireOpRecorder>,
    result: &TurnFlowTurnEndResult,
) {
    use mirri_proto::WireOp;
    recorder
        .record_op(WireOp::TurnEnded {
            turn_id: result.turn_id,
            reason: crate::session::event_projection::turn_stop_reason_wire(&result.stop_reason)
                .to_string(),
            error: None,
            duration_ms: Some(result.duration_ms),
        })
        .await;
}

/// Bump the session's in-memory `updated_at` when a subagent turn settles
/// (ADR-009: subagent activity is session activity — the same mtime
/// semantics the TS host's turn completion applies). In-memory only by
/// design: mirri-core holds no persistence or index, and a subagent turn
/// usually runs inside an enclosing engine turn (`message.send` / steer /
/// cron / notification) whose settle performs the durable write-through
/// (state.json + index). The outer settle usually lands AFTER this bump, so
/// the disk ends with the later time — but a subagent that finishes only
/// AFTER the outer turn already settled (a background child outliving its
/// enclosing turn) leaves this bump unpushed: there is no later outer
/// settle to write it through, so the disk stays at the outer turn's time
/// until the NEXT enclosing turn settles (self-healing) — and a restart
/// before that next settle drops the bump entirely.
async fn bump_session_recency(session: &Arc<SessionScope>) {
    session.update_metadata(|_| {}).await;
}

/// Map a [`DistillError`] onto the subagent error surface with the shared
/// terminal-event side effects inlined by the caller (the faces differ in
/// their guard bookkeeping).
fn distill_error_to_subagent_error(error: DistillError) -> SubagentError {
    match error {
        DistillError::MaxTokens => SubagentError::TurnFailed(SUBAGENT_MAX_TOKENS_ERROR.to_string()),
        DistillError::Turn(e) => map_turn_error(&e),
    }
}

/// Render a `catch_unwind` panic payload as a human-readable message —
/// the same text surface the old `JoinError` formatting provided for a
/// panicking subagent turn ("subagent task panicked: <message>").
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    let message = if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    };
    match crate::panic_capture::take_last_panic_location() {
        Some(location) => format!("{message} (panic at {location})"),
        None => message,
    }
}

/// Remove a child entry from the tracking map.
///
/// Reserved for the one path where the child's `AgentScope` was never
/// created (`create_agent` failed): no agent exists to resume, so the entry
/// is dead weight. Every other exit path keeps the entry ([`settle_child`])
/// for later `resume`.
fn cleanup_child(children: &Mutex<HashMap<String, ChildEntry>>, agent_id: &str) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    guard.remove(agent_id);
}

/// Drop-guard for a driver-run notification turn on a child (B4 review
/// round 2, F1): [`SubagentHost::claim_child_notification_turn`] sets the
/// child's `running` flag under the children-map lock and hands this guard
/// back; dropping it settles the flag, so the exclusion holds for exactly
/// the notification turn's duration. A no-op guard (no children map) is the
/// claim's "nothing to exclude against" answer — hosts without child
/// tracking, or an addressee with no live parent host (the restored
/// children shape, SF-5/O12).
pub struct NotificationRunGuard {
    children: Option<Arc<Mutex<HashMap<String, ChildEntry>>>>,
    agent_id: String,
}

impl NotificationRunGuard {
    /// A guard that releases nothing (see the type's doc).
    pub fn noop() -> Self {
        Self {
            children: None,
            agent_id: String::new(),
        }
    }
}

impl Drop for NotificationRunGuard {
    fn drop(&mut self) {
        if let Some(children) = &self.children {
            settle_child(children, &self.agent_id);
        }
    }
}

/// Drop-guard that settles the child's `running` flag when the completion
/// future is dropped without reaching a settle point (G19: the parent turn's
/// abort drops the in-flight completion future — Rust async drops run no
/// remaining code, so without this guard the entry would stay `running`
/// forever and every later resume would be rejected by the G13 guard).
///
/// The settle is idempotent — explicit `settle_child` calls in the future's
/// terminal branches and this Drop both just set `running = false`, and a
/// later `resume` (which requires `running == false`) can only start after
/// the previous future settled or dropped, never racing this Drop.
///
/// B3 T6 (SDR-B3-02): when the future is dropped WITHOUT reaching a terminal
/// branch (`settled` never marked), the guard also publishes the terminal
/// `subagent.failed` event on the session bus — the TS scheduler always
/// drains its queue, but a Rust tool future can be dropped mid-flight
/// (parent-turn abort, concurrency-buffer cancellation), and those members
/// used to hang in the UI forever. Every terminal branch marks the guard via
/// [`RunningGuard::mark_settled`] so the backstop never double-fires.
struct RunningGuard {
    children: Arc<Mutex<HashMap<String, ChildEntry>>>,
    agent_id: String,
    /// The session bus for the drop-path terminal event.
    event_bus: Option<Arc<EventBus>>,
    /// Set by every terminal branch of the completion future (interior
    /// mutability so the async block can mark it across awaits).
    settled: std::sync::atomic::AtomicBool,
}

impl RunningGuard {
    /// Mark that the future reached a terminal branch — the Drop-side
    /// backstop event must not fire.
    fn mark_settled(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        settle_child(&self.children, &self.agent_id);
        if !self.settled.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(bus) = &self.event_bus
        {
            bus.publish(SessionEvent::SubagentFailed {
                subagent_id: self.agent_id.clone(),
                error: "cancelled".to_string(),
            });
        }
    }
}

/// Publish a subagent lifecycle event on the session bus, when one is
/// wired into the completion future. Fire-and-forget — a missing bus
/// (tests) never affects the subagent's own execution.
fn publish_subagent_event(bus: &Option<Arc<EventBus>>, event: SessionEvent) {
    if let Some(bus) = bus {
        bus.publish(event);
    }
}

/// Fire a subagent lifecycle hook (`SubagentStart` / `SubagentStop`) on the
/// **parent** session's hook engine so the parent can observe the child's
/// lifecycle.
///
/// TS input shape: `{ agent_id, subagent_type, model }`. Fire-and-forget: a
/// block decision or hook failure never affects the subagent's own execution
/// (the result is deliberately ignored).
async fn fire_subagent_hook(
    session: &SessionScope,
    event: HookEventType,
    agent_id: &str,
    subagent_type: &str,
    model: &str,
) {
    let input = serde_json::json!({
        "agent_id": agent_id,
        "subagent_type": subagent_type,
        "model": model,
    });
    let _ = session
        .hook_engine()
        .trigger(event, input, &HashMap::new())
        .await;
}

/// Guard that fires `SubagentStop` when dropped, unless [`Self::fire`] was
/// already called. This ensures the stop hook fires even if the completion
/// future is dropped (cancelled) at an await point after `SubagentStart`
/// has fired — Rust async drops don't run remaining code, so without this
/// guard the parent session would see a dangling `SubagentStart` with no
/// matching `SubagentStop`.
struct SubagentStopGuard {
    session: std::sync::Arc<SessionScope>,
    agent_id: String,
    profile_name: String,
    model: String,
    fired: bool,
}

impl SubagentStopGuard {
    /// Fire the stop hook explicitly and mark as fired so `Drop` is a no-op.
    async fn fire(&mut self) {
        if self.fired {
            return;
        }
        self.fired = true;
        fire_subagent_hook(
            &self.session,
            HookEventType::SubagentStop,
            &self.agent_id,
            &self.profile_name,
            &self.model,
        )
        .await;
    }
}

impl Drop for SubagentStopGuard {
    fn drop(&mut self) {
        if self.fired {
            return;
        }
        // Can't `.await` in Drop — spawn the hook fire on the runtime.
        // Under SDR-B4-01 M1 the drop is the *normal* cancel route, and it
        // can run during runtime shutdown (session close aborting an
        // in-flight child future): `tokio::spawn` panics off-runtime, and a
        // panic inside `Drop` can abort the process — so best-effort spawn
        // via `Handle::try_current`, skipping the hook fire when no runtime
        // is left to run it (review N2).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let session = self.session.clone();
        let agent_id = std::mem::take(&mut self.agent_id);
        let profile_name = std::mem::take(&mut self.profile_name);
        let model = std::mem::take(&mut self.model);
        // `Handle::spawn` itself panics when the runtime is already shutting
        // down — a Drop must never panic, so even this best-effort fire is
        // guarded.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            handle.spawn(async move {
                fire_subagent_hook(
                    &session,
                    HookEventType::SubagentStop,
                    &agent_id,
                    &profile_name,
                    &model,
                )
                .await;
            });
        }));
    }
}

/// Extract the last non-empty assistant text from an agent's context memory.
///
/// Iterates the history in reverse, finds the first `Assistant` message with
/// at least one `Text` part containing non-empty text, and returns the joined
/// text. Returns an empty string if no such message exists.
fn extract_last_assistant_text(agent: &AgentScope) -> String {
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap_or_else(|e| e.into_inner());

    for ctx_msg in guard.history().iter().rev() {
        let ContextMessage { message, .. } = ctx_msg;
        if message.role != Role::Assistant {
            continue;
        }
        let text: String = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if !text.trim().is_empty() {
            return text.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::hook_engine::{HookDef, HookEventType};
    use crate::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, ModelCapability};
    use crate::permission::PermissionMode;
    use crate::profile::AgentProfile;
    use crate::scope::config::AgentScopeConfig;
    use crate::scope::{SessionId, SessionMetadata, SessionScope, SessionScopeConfig};
    use crate::skill::SkillCatalog;
    use crate::tool::Tool;

    /// A minimal LLM factory that always fails — used to test construction.
    struct FailingLlmFactory;

    impl SubagentLlmFactory for FailingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Err("no provider configured".to_string())
        }
    }

    /// A mock LLM that immediately completes a single step with one text block
    /// and an `EndTurn` stop reason — the minimum needed for `run_one_turn` to
    /// succeed.
    struct StaticLlm {
        capability: ModelCapability,
        /// How many `chat` calls this LLM served — a goal-continuation loop
        /// would drive this above one (each continued step is a new call).
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StaticLlm {
        fn new() -> Self {
            Self {
                capability: ModelCapability::default(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Llm for StaticLlm {
        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            "test-model"
        }

        fn capability(&self) -> &ModelCapability {
            &self.capability
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = vec![
                LlmEvent::TextPart {
                    text: "mock reply".to_string(),
                },
                LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// An LLM factory that always succeeds with [`StaticLlm`] — each child
    /// gets its own instance (call counts stay private to one turn).
    struct StaticLlmFactory;

    impl SubagentLlmFactory for StaticLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(StaticLlm::new()))
        }
    }

    /// An LLM factory that always succeeds with [`StaticLlm`], exposing the
    /// served call count through a shared counter (the spawned child owns
    /// its own `Arc<StaticLlm>`; the factory hands the same Arc out so the
    /// test can observe how many steps the child's turn actually ran).
    struct CountingLlmFactory {
        llm: Arc<StaticLlm>,
    }

    impl CountingLlmFactory {
        fn new() -> Self {
            Self {
                llm: Arc::new(StaticLlm::new()),
            }
        }
    }

    impl SubagentLlmFactory for CountingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::clone(&self.llm) as Arc<dyn Llm>)
        }
    }

    /// Build a real session scope (mirrors the session_scope test helper).
    fn make_session(config: SessionScopeConfig) -> Arc<SessionScope> {
        Arc::new(SessionScope::new(
            SessionId::new("session-test"),
            SessionMetadata::default(),
            config,
            Arc::new(SkillCatalog::new()),
            None,
            None,
        ))
    }

    /// Build a profile manager with a resolvable `coder` profile.
    fn make_profile_manager() -> Arc<ProfileManager> {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "test-model".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();
        manager
    }

    /// Register hooks that append the triggered event's JSON input (piped to
    /// the command's stdin) to `log_path`, prefixed with the event name, so
    /// tests can observe which lifecycle events fired on the parent session.
    fn register_hook_recorders(session: &SessionScope, log_path: &std::path::Path) {
        session.hook_engine().register_all(vec![
            HookDef::new(
                HookEventType::SubagentStart,
                format!("sed 's/^/START /' >> '{}'", log_path.display()),
            ),
            HookDef::new(
                HookEventType::SubagentStop,
                format!("sed 's/^/STOP /' >> '{}'", log_path.display()),
            ),
        ]);
    }

    /// Spawn a `coder` subagent through the given host.
    fn spawn_with_host(host: &SessionSubagentHost, cancel: CancelToken) -> SubagentHandle {
        host.spawn(SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: None,
            cancel,
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        })
        .unwrap()
    }

    /// should return empty string when no assistant messages exist
    #[test]
    fn should_return_empty_when_no_assistant_messages() {
        // We can't easily construct a full AgentScope in a unit test,
        // but we can test extract_last_assistant_text's logic indirectly
        // by verifying it compiles and the function signature is correct.
        // Full integration testing happens in the embedded server tests.
    }

    /// should keep the completion future Send across its whole body
    ///
    /// Auxiliary assertion only (B4 T3b): the `CompletionFuture` alias
    /// already carries `+ Send` on its trait object, so the compiler checks
    /// this on every build — this test pins the intent at a call site so a
    /// future weakening of the alias breaks compilation here. The real
    /// gate is the full workspace suite plus `clippy -D warnings`; do not
    /// treat this test as coverage.
    #[test]
    fn should_keep_the_completion_future_send_across_its_whole_body() {
        fn assert_send<T: Send>(_: &T) {}
        fn probe() -> CompletionFuture {
            Box::pin(std::future::pending::<
                Result<SubagentCompletion, SubagentError>,
            >())
        }
        assert_send(&probe());
    }

    /// should return the profile name and model from the child entry
    #[test]
    fn should_track_spawned_child_metadata_when_child_subagent_is_spawned() {
        // Verify that SessionSubagentHost can be constructed.
        let profile_manager = Arc::new(ProfileManager::new());
        let config = AgentScopeConfig::default();
        let host = SessionSubagentHost::new(
            Weak::new(), // no session — won't be used in this test
            profile_manager,
            AgentId::new("main"),
            config,
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );
        // The host should have an empty children map.
        let children = host.children.lock().unwrap();
        assert!(children.is_empty());
    }

    /// should return error when resuming unknown agent id
    #[test]
    fn should_return_error_when_resuming_unknown_agent() {
        let profile_manager = Arc::new(ProfileManager::new());
        let config = AgentScopeConfig::default();
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            config,
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let result = host.resume(ResumeRequest {
            agent_id: "nonexistent".to_string(),
            prompt: "continue".to_string(),
            cancel: CancelToken::new(),
            retry: false,
            suppress_rate_limit_failure_event: false,
        });

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("not found"));
    }

    /// should trigger SubagentStart hook when subagent is spawned
    #[tokio::test(flavor = "multi_thread")]
    async fn should_trigger_subagent_start_hook_when_subagent_is_spawned() {
        // Given a session whose hook engine records SubagentStart/SubagentStop
        // events, and a host whose child LLM construction always fails
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("hooks.log");
        let session = make_session(SessionScopeConfig::default());
        register_hook_recorders(&session, &log_path);
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        // When the subagent is spawned (LLM construction fails, so SubagentStop
        // fires on that exit path too)
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result = (&mut handle.completion).await;

        // Then the turn failed, but the parent saw a balanced start/stop pair
        // carrying the child's metadata
        assert!(matches!(result, Err(SubagentError::TurnFailed(_))));
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("START "),
            "expected SubagentStart in log: {log}"
        );
        assert!(log.contains("STOP "), "expected SubagentStop in log: {log}");
        assert!(
            log.contains("\"agent_id\":\"agent-"),
            "expected agent_id in log: {log}"
        );
        assert!(
            log.contains("\"subagent_type\":\"coder\""),
            "expected subagent_type in log: {log}"
        );
        assert!(
            log.contains("\"model\":\"test-model\""),
            "expected model in log: {log}"
        );
    }

    /// should trigger SubagentStop hook when subagent completes
    #[tokio::test(flavor = "multi_thread")]
    async fn should_trigger_subagent_stop_hook_when_subagent_completes() {
        // Given a session whose hook engine records lifecycle events, and a
        // host whose child LLM completes a full turn
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("hooks.log");
        let session = make_session(SessionScopeConfig::default());
        register_hook_recorders(&session, &log_path);
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        // When the subagent runs a full turn to completion
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result = (&mut handle.completion).await;

        // Then it completed and the parent saw SubagentStart followed by
        // SubagentStop
        assert!(result.is_ok());
        let log = std::fs::read_to_string(&log_path).unwrap();
        let start_idx = log.find("START ").expect("SubagentStart should fire");
        let stop_idx = log.find("STOP ").expect("SubagentStop should fire");
        assert!(
            start_idx < stop_idx,
            "SubagentStart must precede SubagentStop: {log}"
        );
        assert!(
            log.contains("\"subagent_type\":\"coder\""),
            "expected subagent_type in log: {log}"
        );
    }

    /// should trigger SubagentStop hook when subagent is cancelled
    #[tokio::test(flavor = "multi_thread")]
    async fn should_trigger_subagent_stop_hook_when_subagent_is_cancelled() {
        // Given a session whose hook engine records lifecycle events, and a
        // cancel token that is already cancelled
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("hooks.log");
        let session = make_session(SessionScopeConfig::default());
        register_hook_recorders(&session, &log_path);
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );
        let cancel = CancelToken::new();
        cancel.cancel();

        // When the subagent is spawned with an already-cancelled token
        let mut handle = spawn_with_host(&host, cancel);
        let result = (&mut handle.completion).await;

        // Then it was cancelled and the parent saw a balanced start/stop pair
        assert!(matches!(result, Err(SubagentError::Cancelled)));
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("START "),
            "expected SubagentStart in log: {log}"
        );
        assert!(log.contains("STOP "), "expected SubagentStop in log: {log}");
        assert!(
            log.contains("\"model\":\"test-model\""),
            "expected model in log: {log}"
        );
    }

    /// should end the child's turn after one step when a session goal is active
    ///
    /// The regression guard for the goal-continuation leak: the child's
    /// TurnFlow must wire goal USAGE ACCOUNTING only (the main-turn posture),
    /// never the full `set_goal_manager` — a full wiring lets the
    /// `should_continue_after_stop` Layer 2 see the session's active goal
    /// and force the child's turn to continue long after its task is done
    /// (the observed "收到，保持待命" loop, burning tokens until max_steps).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_end_the_child_turn_after_one_step_when_a_session_goal_is_active() {
        // Given a session with an ACTIVE goal (budget far from exhausted)
        let session = make_session(SessionScopeConfig::default());
        session
            .goal()
            .create("finish the release".to_string(), None)
            .unwrap();

        // And a host whose child LLM answers every call with one text block
        // and a natural EndTurn stop. The turn's step cap is 3: on the
        // UNFIXED wiring the goal-continuation layer loops the turn past
        // its natural stop until the cap (the observed "standby" spin —
        // bounded here so the test fails instead of hanging); on the fix
        // the natural stop is honored and the cap never engages.
        let factory = Arc::new(CountingLlmFactory::new());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            factory.clone(),
            TurnFlowConfig {
                max_steps: Some(3),
                ..TurnFlowConfig::default()
            },
            None,
            None,
        );

        // When the subagent runs a full turn to its natural completion
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result = (&mut handle.completion).await;

        // Then the turn completed successfully
        assert!(result.is_ok(), "subagent should complete: {result:?}");

        // And the child's LLM was called exactly once — the natural stop was
        // honored, NOT overridden by a goal-driven continuation step (the
        // pre-fix wiring served a second call: the "standby" loop's first
        // iteration)
        let calls = factory.llm.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            calls, 1,
            "the child turn must end after its natural stop while a session goal is active; \
             got {calls} LLM calls (goal-continuation leaked into the subagent)"
        );

        // And the accounting half survives: the child's step still recorded
        // its turn usage against the session goal (accounting-only wiring,
        // not a dropped goal manager) — pins the fix against a future
        // refactor that removes the slot entirely.
        let used_turns = session.goal().used_turns();
        assert_eq!(
            used_turns, 1,
            "the child's step usage must still be accounted against the session goal; \
             got {used_turns} used turns (goal accounting was dropped, not downgraded)"
        );
    }

    /// A fixed catalog for testing model-alias validation and profile
    /// capability resolution.
    struct TestCatalog {
        entries: Vec<crate::model_catalog::ModelCatalogEntry>,
    }

    impl TestCatalog {
        fn new(entries: Vec<crate::model_catalog::ModelCatalogEntry>) -> Self {
            Self { entries }
        }
    }

    impl crate::model_catalog::ModelCatalog for TestCatalog {
        fn list_models(&self) -> Vec<crate::model_catalog::ModelCatalogEntry> {
            self.entries.clone()
        }

        fn get_by_id(&self, id: &str) -> Option<crate::model_catalog::ModelCatalogEntry> {
            self.entries.iter().find(|e| e.id == id).cloned()
        }
    }

    fn entry(id: &str, model: &str) -> crate::model_catalog::ModelCatalogEntry {
        crate::model_catalog::ModelCatalogEntry {
            id: id.to_string(),
            model: model.to_string(),
            provider: "test".to_string(),
            description: None,
            max_context_size: None,
            capabilities: Vec::new(),
            default_effort: None,
        }
    }

    /// should validate model alias before spawning subagent when model is
    /// specified and catalog is provided
    #[test]
    fn should_validate_model_alias_before_spawning_subagent_when_model_is_specified() {
        // Given a host whose catalog contains the "sonnet" alias
        let profile_manager = make_profile_manager();
        let config = AgentScopeConfig::default();
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            config,
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![entry(
                "sonnet",
                "claude-sonnet-4",
            )]))),
            None,
        );

        // When spawning with a model override that matches the catalog
        let result = host.spawn(SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: Some("sonnet".to_string()),
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        });

        // Then the spawn proceeds and the handle carries the validated alias
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model.as_deref(), Some("sonnet"));
    }

    /// should return error with valid options when model alias is invalid
    #[test]
    fn should_return_error_with_valid_options_when_model_alias_is_invalid() {
        // Given a host whose catalog exposes "sonnet", "opus", and "haiku"
        let profile_manager = make_profile_manager();
        let config = AgentScopeConfig::default();
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            config,
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![
                entry("sonnet", "claude-sonnet-4"),
                entry("opus", "claude-opus-4"),
                entry("haiku", "claude-haiku-4"),
            ]))),
            None,
        );

        // When spawning with an unknown model alias
        let result = host.spawn(SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: Some("does-not-exist".to_string()),
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        });

        // Then the spawn fails with a RequestInvalid error naming the valid
        // aliases so the caller can retry with a real one
        assert!(matches!(result, Err(EngineError::RequestInvalid(_))));
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("unknown model alias 'does-not-exist'"),
            "should name the bad alias: {err}"
        );
        assert!(err.contains("sonnet"), "should list valid options: {err}");
        assert!(err.contains("opus"), "should list valid options: {err}");
        assert!(err.contains("haiku"), "should list valid options: {err}");
    }

    /// should inline the curated list (with descriptions) in the error when
    /// the model param is unknown
    #[test]
    fn should_inline_curated_list_in_error_when_model_param_is_unknown() {
        // Given a catalog whose aliases carry descriptions (curated set
        // non-empty)
        let mut sonnet = entry("sonnet", "claude-sonnet-4");
        sonnet.description = Some("The balanced model".to_string());
        sonnet.max_context_size = Some(200_000);
        sonnet.capabilities = vec!["vision".to_string(), "tool-use".to_string()];
        let mut opus = entry("opus", "claude-opus-4");
        opus.description = Some("Heavy lifting".to_string());
        let host = SessionSubagentHost::new(
            Weak::new(),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![sonnet, opus]))),
            None,
        );

        // When spawning with a model alias that was not configured
        let result = host.spawn(SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: Some("does-not-exist".to_string()),
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        });

        // Then the error inlines the curated listing (alias + description) so
        // the LLM can self-correct from what it saw in the tool description
        assert!(matches!(result, Err(EngineError::RequestInvalid(_))));
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("unknown model alias 'does-not-exist'"),
            "should name the bad alias: {err}"
        );
        assert!(
            err.contains("- sonnet — The balanced model, 195k context (vision, tool-use)"),
            "should inline curated alias with description and context: {err}"
        );
        assert!(
            err.contains("- opus — Heavy lifting"),
            "should inline curated alias with description: {err}"
        );
    }

    /// should truncate the inlined curated list at 20 entries when the
    /// catalog has more (context-window guard)
    #[test]
    fn should_truncate_curated_list_in_error_at_20_when_more_curated_entries_exist() {
        // Given a catalog with 25 curated aliases
        let entries: Vec<crate::model_catalog::ModelCatalogEntry> = (0..25)
            .map(|i| {
                let mut e = entry(&format!("alias-{i}"), &format!("model-{i}"));
                e.description = Some(format!("Model {i}"));
                e
            })
            .collect();
        let host = SessionSubagentHost::new(
            Weak::new(),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(entries))),
            None,
        );

        // When spawning with an unknown alias
        let err = host
            .spawn(SpawnRequest {
                profile_name: "coder".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: Some("nope".to_string()),
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: None,
                swarm_item: None,
                suppress_rate_limit_failure_event: false,
            })
            .err()
            .unwrap()
            .to_string();

        // Then at most 20 curated lines appear and the tail notes the cut
        assert!(
            err.contains("- alias-0 — Model 0"),
            "should inline the first entry: {err}"
        );
        assert!(
            err.contains("- alias-19 — Model 19"),
            "should inline up to the 20th entry: {err}"
        );
        assert!(
            !err.contains("alias-20"),
            "should truncate beyond 20 entries: {err}"
        );
        assert!(
            err.contains("5 more") && err.contains("truncated"),
            "should say how many were cut: {err}"
        );
    }

    /// should allow model overrides when catalog is empty (nothing to
    /// validate against)
    #[test]
    fn should_allow_model_override_when_catalog_is_empty() {
        // Given a host with a catalog that configures no models
        let profile_manager = make_profile_manager();
        let config = AgentScopeConfig::default();
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            config,
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![]))),
            None,
        );

        // When spawning with any model override
        let result = host.spawn(SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: Some("anything".to_string()),
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        });

        // Then the override passes through unvalidated (no aliases configured)
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model.as_deref(), Some("anything"));
    }

    /// should list catalog aliases and profile capabilities in
    /// available_profiles when catalog is provided
    #[test]
    fn should_fill_capabilities_and_available_models_when_catalog_is_provided() {
        // Given a profile whose model is in the catalog
        let profile_manager = Arc::new(ProfileManager::new());
        profile_manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "gpt-4o".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();
        let mut vision_entry = entry("gpt-4o", "gpt-4o");
        vision_entry.capabilities = vec!["vision".to_string(), "tool-use".to_string()];
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![
                vision_entry,
                entry("sonnet", "claude-sonnet-4"),
            ]))),
            None,
        );

        // When listing available profiles
        let infos = host.available_profiles();

        // Then capabilities come from the catalog and available_models lists
        // every alias
        let coder = infos
            .iter()
            .find(|i| i.name == "coder")
            .expect("coder profile");
        assert_eq!(coder.model_capabilities, vec!["vision", "tool-use"]);
        assert_eq!(coder.available_models, vec!["gpt-4o", "sonnet"]);
    }

    // ===== subagent allowlist enforcement (R3-4) =====

    /// A host bound to a caller profile that declares `subagents`.
    ///
    /// The parent config carries a current model (`test-model`) so the
    /// allowlist tests exercise their own gate, not the model-resolution
    /// chain (spec 11 A3 — a dispatcher with a current model resolves Tier 3
    /// instead of failing).
    fn make_host_for_caller(
        profile_manager: Arc<ProfileManager>,
        caller_profile_name: Option<&str>,
    ) -> SessionSubagentHost {
        SessionSubagentHost::new(
            Weak::new(), // the completion futures are never awaited here
            profile_manager,
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "test-model".to_string(),
                ..AgentScopeConfig::default()
            },
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        )
        .with_caller_profile(caller_profile_name.map(str::to_string))
    }

    /// A manager with a `reviewer` caller (allowlist `["coder"]`) and a
    /// `planner` caller (explicitly empty allowlist), next to the
    /// built-ins.
    fn make_allowlist_profile_manager() -> Arc<ProfileManager> {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "reviewer".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                subagents: Some(vec!["coder".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();
        manager
            .register(AgentProfile {
                name: "planner".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                subagents: Some(vec![]),
                ..AgentProfile::default()
            })
            .unwrap();
        manager
    }

    fn spawn_request(profile_name: &str) -> SpawnRequest {
        SpawnRequest {
            profile_name: profile_name.to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: None,
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        }
    }

    /// Given a caller profile declaring `subagents: ["coder"]`, When it
    /// spawns an allowlisted profile, Then the spawn proceeds.
    #[test]
    fn should_spawn_allowed_profile_when_caller_declares_subagent_allowlist() {
        let host = make_host_for_caller(make_allowlist_profile_manager(), Some("reviewer"));
        let handle = host
            .spawn(spawn_request("coder"))
            .expect("coder is allowed");
        assert_eq!(handle.profile_name, "coder");
    }

    /// Given a caller profile declaring `subagents: ["coder"]`, When it
    /// spawns a profile outside the list, Then the spawn is refused with
    /// the TS-aligned `subagentTypeNotAllowedMessage` text.
    #[test]
    fn should_refuse_disallowed_profile_with_ts_aligned_message_when_not_in_allowlist() {
        let host = make_host_for_caller(make_allowlist_profile_manager(), Some("reviewer"));
        let error = host
            .spawn(spawn_request("explore"))
            .err()
            .expect("explore is outside the allowlist");
        match error {
            EngineError::RequestInvalid(message) => assert_eq!(
                message,
                "Subagent type \"explore\" is not allowed for this agent. \
                 Allowed subagent types: coder."
            ),
            other => panic!("expected RequestInvalid, got: {other}"),
        }
    }

    /// Given a caller profile with an unset allowlist, When it spawns any
    /// dispatchable profile, Then the spawn is refused — a named caller
    /// with no own list allows NOTHING (default-deny: unset ≡ empty).
    #[test]
    fn should_refuse_all_profiles_when_caller_declares_no_allowlist() {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "planner".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();
        let host = make_host_for_caller(manager, Some("planner"));
        let error = host
            .spawn(spawn_request("explore"))
            .err()
            .expect("no own allowlist = default-deny");
        match error {
            EngineError::RequestInvalid(message) => assert_eq!(
                message,
                "Subagent type \"explore\" is not allowed for this agent. \
                 Allowed subagent types: none."
            ),
            other => panic!("expected RequestInvalid, got: {other}"),
        }
    }

    /// Given an unbound caller and the default (essential) profile, When a
    /// built-in dispatchable profile is spawned, Then the spawn proceeds —
    /// the essential's built-in allowlist seeds the built-in names.
    #[test]
    fn should_spawn_builtin_profiles_for_unbound_caller_under_default_allowlist() {
        let host = make_host_for_caller(Arc::new(ProfileManager::new()), None);
        assert!(host.spawn(spawn_request("explore")).is_ok());
    }

    /// Given an unbound caller and an essential profile whose allowlist is
    /// emptied (the same-name override face), When any profile is spawned,
    /// Then the spawn is refused — the main agent follows its own list.
    #[test]
    fn should_refuse_all_profiles_when_the_essential_allowlist_is_emptied() {
        let manager = Arc::new(ProfileManager::new());
        let mut essential = crate::profile::AgentProfile {
            name: "agent".to_string(),
            ..Default::default()
        };
        essential.subagents = Some(vec![]);
        manager
            .register(essential)
            .expect("the essential override registers");
        let host = make_host_for_caller(manager, None);
        assert!(host.spawn(spawn_request("coder")).is_err());
    }

    /// Given a caller with `subagents: ["coder"]`, When the dispatchable
    /// profile listing is built, Then it intersects with the allowlist —
    /// the tool-description face (TS `agentTool.ts:152-156`).
    #[test]
    fn should_intersect_available_profiles_with_caller_allowlist_for_descriptions() {
        let host = make_host_for_caller(make_allowlist_profile_manager(), Some("reviewer"));
        let names: Vec<String> = host
            .available_profiles()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["coder".to_string()]);

        // And the Agent tool's description built over that host lists only
        // the allowlisted profile.
        let tool = crate::tools::AgentTool::new(
            Arc::new(host) as Arc<dyn SubagentHost>,
            Arc::new(crate::task::TaskManager::new()),
            "s1".to_string(),
            None,
            Arc::new(|| true),
        );
        let description = tool.description();
        assert!(
            description.contains("- coder"),
            "description: {description}"
        );
        // The base description text names 'explore' as an example, so the
        // intersection is asserted on the profile listing lines.
        assert!(
            !description.contains("- explore"),
            "explore must be hidden from the profile listing: {description}"
        );
    }

    /// Given a caller with an unset allowlist, When the dispatchable
    /// profile listing is built, Then NO profile appears (default-deny:
    /// the LLM perceives nothing it cannot dispatch).
    #[test]
    fn should_list_no_profiles_when_caller_has_no_allowlist() {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "planner".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();
        let host = make_host_for_caller(manager, Some("planner"));
        let names: Vec<String> = host
            .available_profiles()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert!(names.is_empty(), "no own allowlist = no visible profiles");
    }

    /// Given a profile whose own file-level `enabled: false` flag disables
    /// it (R3-1), When the dispatchable listing and the Agent tool
    /// description are built, Then the name is absent from both — the
    /// dispatch face never offers a disabled profile, even though the
    /// management face's `list_all` still shows it.
    #[test]
    fn should_hide_file_disabled_profiles_from_available_profiles_and_tool_descriptions() {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "offline".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..AgentProfile::default()
            })
            .unwrap();
        let host = make_host_for_caller(manager, None);

        // When both faces over the same host are built
        let names: Vec<String> = host
            .available_profiles()
            .into_iter()
            .map(|i| i.name)
            .collect();
        let tool = crate::tools::AgentTool::new(
            Arc::new(host) as Arc<dyn SubagentHost>,
            Arc::new(crate::task::TaskManager::new()),
            "s1".to_string(),
            None,
            Arc::new(|| true),
        );
        let description = tool.description();

        // Then the disabled profile is offered by neither dispatch face
        assert!(
            !names.iter().any(|name| name == "offline"),
            "the disabled profile must not be a candidate: {names:?}"
        );
        assert!(
            !description.contains("- offline"),
            "the disabled profile must not appear in the Agent tool description: {description}"
        );
    }

    // ===== resume contract (G13/G14/G17/G19 — subagent-context confirmed
    // rust defects 1, 4, 5) =====

    /// A host wired to a real session with a working LLM factory and an
    /// event bus the test can observe.
    fn make_running_host(session: &Arc<SessionScope>) -> (SessionSubagentHost, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(64));
        let host = SessionSubagentHost::new(
            Arc::downgrade(session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        (host, bus)
    }

    /// An LLM that parks its first stream until the shared cancel token
    /// fires — keeps the child genuinely mid-turn for the running-resume
    /// test, and unwinds deterministically when the test cancels.
    struct ParkingLlm {
        cancel: CancelToken,
        capability: ModelCapability,
    }

    impl Llm for ParkingLlm {
        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            "test-model"
        }

        fn capability(&self) -> &ModelCapability {
            &self.capability
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            let cancel = self.cancel.clone();
            Ok(Box::pin(futures::stream::once(async move {
                let _ = cancel.cancelled().await;
                LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                }
            })))
        }
    }

    struct ParkingLlmFactory {
        cancel: CancelToken,
    }

    impl SubagentLlmFactory for ParkingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(ParkingLlm {
                cancel: self.cancel.clone(),
                capability: ModelCapability::default(),
            }))
        }
    }

    /// Given a spawned subagent whose turn is observably in flight (the
    /// parked LLM keeps it mid-run), When a resume is requested, Then it is
    /// rejected with the already-running reason — the pre-fix code had no
    /// running-state flag and queued a second turn on the same TurnFlow
    /// (G13).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_resume_while_the_subagent_is_still_running() {
        let session = make_session(SessionScopeConfig::default());
        let bus = Arc::new(EventBus::new(64));
        let mut receiver = bus.subscribe();
        let child_cancel = CancelToken::new();
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ParkingLlmFactory {
                cancel: child_cancel.clone(),
            }),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        let handle = spawn_with_host(&host, child_cancel.clone());
        let agent_id = handle.agent_id.clone();
        let runner = tokio::spawn(async move {
            let _ = handle.completion.await;
        });

        // Wait until the child is observably mid-run (its started event
        // published; the turn then parks in the LLM call).
        let mut started = false;
        while let Ok(event) =
            tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv()).await
        {
            if matches!(event, Ok(SessionEvent::SubagentStarted { .. })) {
                started = true;
                break;
            }
        }
        assert!(started, "the spawned child must announce its start");

        let result = host.resume(ResumeRequest {
            agent_id,
            prompt: "continue".to_string(),
            cancel: CancelToken::new(),
            retry: false,
            suppress_rate_limit_failure_event: false,
        });
        let err = result
            .err()
            .expect("a resume of a running child must be rejected");
        assert!(
            err.to_string().contains("already running"),
            "the rejection must carry the already-running reason, got: {err}"
        );

        // Unwind the parked turn so the runtime teardown does not wait on it.
        child_cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), runner).await;
    }

    /// Given a subagent that completed its run, When it is resumed, Then the
    /// resume is accepted — the pre-fix code deleted the tracked entry on
    /// completion, so every post-completion resume failed with "not found"
    /// and the tools' resume hint was a lie (G14).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_accept_resume_after_the_subagent_completes() {
        let session = make_session(SessionScopeConfig::default());
        let (host, _bus) = make_running_host(&session);
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        let first = (&mut handle.completion).await;
        assert!(first.is_ok(), "the first run must complete: {first:?}");

        let mut resumed = host
            .resume(ResumeRequest {
                agent_id,
                prompt: "continue the work".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("a resume of a completed child must be accepted");
        let second = (&mut resumed.completion).await;
        assert!(second.is_ok(), "the resumed run must complete: {second:?}");
    }

    /// Given a resumed subagent run, When it executes, Then the parent
    /// session observes the same SubagentStart/SubagentStop hook pair a
    /// fresh spawn fires — the pre-fix resume path fired no hooks at all
    /// (G17).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_fire_start_and_stop_hooks_when_a_resume_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("hooks.log");
        let session = make_session(SessionScopeConfig::default());
        register_hook_recorders(&session, &log_path);
        let (host, _bus) = make_running_host(&session);
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        (&mut handle.completion).await.expect("first run completes");

        let mut resumed = host
            .resume(ResumeRequest {
                agent_id,
                prompt: "continue".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("resume accepted");
        (&mut resumed.completion)
            .await
            .expect("resumed run completes");

        let log = std::fs::read_to_string(&log_path).unwrap();
        let starts = log.matches("START ").count();
        let stops = log.matches("STOP ").count();
        assert_eq!(
            starts, 2,
            "spawn + resume must both fire SubagentStart: {log}"
        );
        assert_eq!(
            stops, 2,
            "spawn + resume must both fire SubagentStop: {log}"
        );
    }

    /// Given a resumed run whose turn fails, When the failure lands, Then
    /// subscribers observe `subagent.failed` — the pre-fix resume error paths
    /// published nothing (except Aborted), leaving the observation face
    /// dangling (G17).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_publish_failed_when_a_resume_encounters_an_error() {
        let session = make_session(SessionScopeConfig::default());
        let bus = Arc::new(EventBus::new(64));
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            // Every LLM construction fails — both the spawn and the resume.
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        let mut receiver = bus.subscribe();
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        // The spawn's LLM failure settles the entry without removing it.
        let first = (&mut handle.completion).await;
        assert!(first.is_err(), "the spawn must fail (no LLM)");

        let mut resumed = host
            .resume(ResumeRequest {
                agent_id,
                prompt: "continue".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("resume of a failed-but-settled child is accepted");
        let second = (&mut resumed.completion).await;
        assert!(second.is_err(), "the resumed run must fail (no LLM)");

        // The resumed failure published subagent.failed (the spawn failure
        // published its own — drain both).
        let mut saw_spawned_or_started = false;
        let mut saw_failed_after_resume = false;
        while let Ok(event) =
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv()).await
        {
            match event {
                Ok(SessionEvent::SubagentStarted { .. }) => saw_spawned_or_started = true,
                Ok(SessionEvent::SubagentFailed { .. }) if saw_spawned_or_started => {
                    saw_failed_after_resume = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(
            saw_failed_after_resume,
            "the resumed failure must publish subagent.failed"
        );
    }

    /// Given a completion future dropped mid-flight (the parent turn's
    /// abort drops it before it settles), When a resume is requested, Then
    /// it is accepted — the pre-fix leak left the entry un-settled forever
    /// (G19; the RunningGuard settles the flag on drop).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_accept_resume_after_the_completion_future_is_dropped_mid_flight() {
        let session = make_session(SessionScopeConfig::default());
        let (host, _bus) = make_running_host(&session);
        let handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        // Drop the handle without awaiting — the parent's abort path.
        drop(handle);

        // Give the dropped future's RunningGuard its synchronous settle.
        let resumed = host.resume(ResumeRequest {
            agent_id,
            prompt: "continue after the abort".to_string(),
            cancel: CancelToken::new(),
            retry: false,
            suppress_rate_limit_failure_event: false,
        });
        assert!(
            resumed.is_ok(),
            "a resume after the dropped future must be accepted, got: {:?}",
            resumed.err()
        );
    }

    // ===== B3 T1: rate-limit classification + suppress face + suspended =====

    /// Given a surfaced transient 429, a billing-anchored quota 429, and an
    /// abort, When the turn error is classified, Then only the transient
    /// rate limit maps to the requeue-capable variant.
    #[test]
    fn should_classify_provider_rate_limits_but_not_quota_exhaustion() {
        let rate_limited = EngineError::ProviderRateLimit("HTTP 429: too many requests".into());
        let quota = EngineError::ProviderQuotaExhausted {
            message: "exceeded your current quota".into(),
            retry_after_ms: Some(3_000),
        };
        assert!(matches!(
            map_turn_error(&rate_limited),
            SubagentError::RateLimited(ref m) if m.contains("429")
        ));
        assert!(matches!(
            map_turn_error(&quota),
            SubagentError::TurnFailed(_)
        ));
        assert!(matches!(
            map_turn_error(&EngineError::Aborted("stop".into())),
            SubagentError::Cancelled
        ));
    }

    /// Given a host wired to a real event bus, When the scheduler parks a
    /// member for retry, Then the suspended wire event carries the identity
    /// and the structured rate-limit cause (ADR-011).
    #[tokio::test]
    async fn should_publish_subagent_suspended_when_the_host_is_notified() {
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let host = SessionSubagentHost::new(
            Weak::new(),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        host.notify_suspended("agent-1", &SuspendCause::RateLimit);
        match rx.try_recv() {
            Ok(SessionEvent::SubagentSuspended {
                subagent_id,
                reason,
            }) => {
                assert_eq!(subagent_id, "agent-1");
                assert_eq!(
                    reason,
                    SuspendCause::RateLimit,
                    "the reason is machine-readable, not a free-form string"
                );
            }
            other => panic!("expected SubagentSuspended, got {other:?}"),
        }
    }

    /// An LLM whose first `fail_until` chat calls fail the stream with a 429
    /// and whose later calls succeed. The step-level retry service's default
    /// patterns include "429" with `max_retries_per_step = 2`, so surfacing
    /// the rate limit to the subagent host takes 1 initial + 2 step-retried
    /// calls (3 total) — the member-level requeue then stacks ON TOP of the
    /// exhausted step retries, mirroring the TS kosong-then-batch-scheduler
    /// layering. Every chat call records the user-role message texts so
    /// retry-path tests can assert no new user message was appended.
    struct RateLimitRecoveryLlm {
        calls: std::sync::atomic::AtomicUsize,
        /// Chat calls 1..=fail_until fail with a 429; later calls succeed.
        fail_until: usize,
        /// Per chat call, (user-role message count, their joined texts) so
        /// retry-path tests can assert no new user message was appended
        /// (an empty-text user message still bumps the count even when the
        /// projection filters it out of the wire view).
        user_messages: Mutex<Vec<(usize, String)>>,
        capability: ModelCapability,
    }

    impl RateLimitRecoveryLlm {
        /// Fail the first attempt's full retry ladder (3 calls) with 429s,
        /// then succeed — the requeue-capable failure surfaces to the host.
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail_until: 3,
                user_messages: Mutex::new(Vec::new()),
                capability: ModelCapability::default(),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Llm for RateLimitRecoveryLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
            use std::sync::atomic::Ordering;
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let user_prompt = params
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|part| match part {
                            crate::message::ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n---\n");
            let count = params
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count();
            self.user_messages
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((count, user_prompt));
            if call < self.fail_until {
                Ok(Box::pin(futures::stream::iter(vec![LlmEvent::Error {
                    message: "HTTP 429: too many requests".to_string(),
                }])))
            } else {
                Ok(Box::pin(futures::stream::iter(vec![
                    LlmEvent::TextPart {
                        text: "recovered".to_string(),
                    },
                    LlmEvent::Done {
                        stop_reason: "completed".to_string(),
                    },
                ])))
            }
        }
    }

    /// A factory sharing one [`RateLimitRecoveryLlm`] across every child turn.
    struct SharedRateLimitFactory(Arc<RateLimitRecoveryLlm>);

    impl SubagentLlmFactory for SharedRateLimitFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::clone(&self.0) as Arc<dyn Llm>)
        }
    }

    /// Given a host whose child LLM fails the first turn with a 429, When a
    /// swarm member attempt runs with the suppression flag set, Then the
    /// error is the requeue-capable variant and NO terminal subagent event
    /// fired (the scheduler owns the suspended event).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_publish_a_failure_event_when_a_rate_limit_is_suppressed() {
        let llm = RateLimitRecoveryLlm::new();
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(SharedRateLimitFactory(Arc::clone(&llm))),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "coder".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: Some(0),
                swarm_item: None,
                suppress_rate_limit_failure_event: true,
            })
            .unwrap();
        let outcome = (&mut handle.completion).await;

        assert!(
            matches!(outcome, Err(SubagentError::RateLimited(_))),
            "expected RateLimited, got {outcome:?}"
        );
        // The bus carries the lifecycle events published up to this point —
        // spawn/started, but NO SubagentFailed (the suppression held).
        let mut saw_started = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::SubagentStarted { .. } => saw_started = true,
                SessionEvent::SubagentFailed { error, .. } => {
                    panic!("suppressed rate limit must not publish failed: {error}")
                }
                _ => {}
            }
        }
        assert!(saw_started, "the child must have announced its start");
        assert_eq!(
            llm.calls(),
            3,
            "the 429 must exhaust the step-retry ladder (1 + 2 retries) \
             before surfacing to the host"
        );
    }

    /// Given the same 429 failure WITHOUT the suppression flag, When the
    /// attempt fails, Then the ordinary failure event still fires (the
    /// Agent-tool face keeps its terminal event).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_publish_the_ordinary_failure_event_when_the_flag_is_not_set() {
        let llm = RateLimitRecoveryLlm::new();
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(SharedRateLimitFactory(Arc::clone(&llm))),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "coder".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: Some(0),
                swarm_item: None,
                suppress_rate_limit_failure_event: false,
            })
            .unwrap();
        let outcome = (&mut handle.completion).await;

        assert!(matches!(outcome, Err(SubagentError::RateLimited(_))));
        let mut saw_failed = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, SessionEvent::SubagentFailed { .. }) {
                saw_failed = true;
            }
        }
        assert!(saw_failed, "the unsuppressed failure must publish failed");
    }

    // ===== B3 T2: retry-shaped resume (no new user message) =====

    /// Given a host whose child fails its first attempt with a 429 (after the
    /// step-retry ladder) and then recovers, When the scheduler retries the
    /// same agent identity with the retry-shaped resume, Then the retry
    /// completes and the child's memory still carries exactly ONE user
    /// prompt — the retry turn added no new one (TS `promptService.retry()`).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_retry_the_failed_turn_on_the_same_identity_without_a_new_prompt() {
        let llm = RateLimitRecoveryLlm::new();
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(SharedRateLimitFactory(Arc::clone(&llm))),
            TurnFlowConfig::default(),
            None,
            None,
        );
        let cancel = CancelToken::new();
        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "coder".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: cancel.clone(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: Some(0),
                swarm_item: None,
                suppress_rate_limit_failure_event: true,
            })
            .unwrap();
        let agent_id = handle.agent_id.clone();
        assert!(matches!(
            (&mut handle.completion).await,
            Err(SubagentError::RateLimited(_))
        ));

        // Snapshot the child's raw user-message count BEFORE the retry —
        // the projection handed to the LLM filters empty-text user messages,
        // so the memory history is the only honest probe.
        async fn user_message_count(session: &Arc<SessionScope>, agent_id: &str) -> usize {
            let agent = session
                .get_agent(&AgentId::new(agent_id))
                .await
                .expect("child scope exists");
            let memory = agent.context_memory();
            let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .history()
                .iter()
                .filter(|m| m.message.role == Role::User)
                .count()
        }
        let before = user_message_count(&session, &agent_id).await;

        // When the scheduler retries the same agent identity
        let mut retry = host
            .resume(ResumeRequest {
                agent_id: agent_id.clone(),
                prompt: String::new(), // unused in retry mode
                cancel,
                retry: true,
                suppress_rate_limit_failure_event: true,
            })
            .unwrap();
        let retried = (&mut retry.completion).await;

        // Then the retry completes and the message list the model saw is
        // IDENTICAL across every call — the retry turn appended no new
        // user message (a non-retry resume would append the — empty —
        // second user prompt and bump the last call's user-message count).
        // The shared tools_added system-reminder rides identically on
        // every call.
        assert!(retried.is_ok(), "retry must complete: {retried:?}");
        let recorded = llm
            .user_messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(recorded.len(), 4, "3 failed + 1 recovered chat call");
        for (i, (count, seen)) in recorded.iter().enumerate() {
            let (first_count, first_text) = (&recorded[0].0, recorded[0].1.as_str());
            assert_eq!(
                (count, seen.as_str()),
                (first_count, first_text),
                "chat call {i} must see exactly the same user messages as call 0 \
                 (the retry turn adds no new user prompt)"
            );
        }
        assert!(
            recorded[0].1.starts_with("do the thing"),
            "the original prompt must be the first user message: {}",
            &recorded[0].1[..recorded[0].1.len().min(60)]
        );
        // And the raw memory carries no new user message after the retry.
        let after = user_message_count(&session, &agent_id).await;
        assert_eq!(
            before, after,
            "the retry turn must not append a new user message to the child's memory"
        );
    }

    // ===== B3 T6: RunningGuard terminal-event backstop (G19 tail) =====

    /// Given a spawned member whose completion future is never polled,
    /// When the swarm batch drops it mid-flight, Then the bus still carries
    /// a terminal event for the member — the G19 open tail (queued/mid-run
    /// members used to hang in the UI forever).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_publish_a_terminal_event_when_a_member_is_dropped_without_settling() {
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            Some(Arc::clone(&bus)),
        );
        let handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        // When the future is dropped before its first poll (the buffered
        // queue behind the concurrency cap being cancelled is exactly this)
        drop(handle);
        // Then a terminal event arrives for the dropped member (the
        // synchronously published `subagent.spawned` rides the bus first —
        // drain until the backstop's `subagent.failed` lands).
        let event = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("terminal event must fire")
                .expect("bus open");
            if matches!(event, SessionEvent::SubagentFailed { .. }) {
                break event;
            }
        };
        match event {
            SessionEvent::SubagentFailed { subagent_id, error } => {
                assert_eq!(subagent_id, agent_id);
                assert!(error.contains("cancelled"), "error: {error}");
            }
            other => panic!("expected SubagentFailed, got {other:?}"),
        }
    }
    // ===== de-block_on behavior (B4 T4: A2 / A3 / A16) =====

    /// An LLM whose `chat` never resolves — the turn parks on its stream.
    /// Signals the test that the turn reached the LLM await through
    /// `started` before parking.
    struct HangingLlm {
        started: Arc<std::sync::atomic::AtomicBool>,
        capability: ModelCapability,
    }

    impl Llm for HangingLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    /// A factory whose LLM hangs (test 2) — every child built from it parks
    /// its turn on the LLM stream.
    struct HangingLlmFactory {
        started: Arc<std::sync::atomic::AtomicBool>,
    }

    impl SubagentLlmFactory for HangingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(HangingLlm {
                started: Arc::clone(&self.started),
                capability: ModelCapability::default(),
            }))
        }
    }

    /// A factory whose LLM behavior flips between "hang" and "complete"
    /// (test 3): the drop-mid-turn face runs under `hang`, the resume face
    /// under `complete`.
    struct FlippingLlmFactory {
        mode: Arc<std::sync::atomic::AtomicBool>,
    }

    /// `false` = hang, `true` = complete with the static reply.
    impl SubagentLlmFactory for FlippingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(FlippingLlm {
                mode: Arc::clone(&self.mode),
                capability: ModelCapability::default(),
            }))
        }
    }

    struct FlippingLlm {
        mode: Arc<std::sync::atomic::AtomicBool>,
        capability: ModelCapability,
    }

    impl Llm for FlippingLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            if self.mode.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(Box::pin(futures::stream::iter(vec![
                    LlmEvent::TextPart {
                        text: "resumed reply".to_string(),
                    },
                    LlmEvent::Done {
                        stop_reason: "completed".to_string(),
                    },
                ])))
            } else {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
    }

    /// should complete a turn on a current_thread runtime without panic
    ///
    /// A2 (K3): `block_in_place` panics on the current_thread flavor —
    /// with the pure-async turn it must not. `#[tokio::test]` defaults to
    /// the current_thread runtime.
    #[tokio::test]
    async fn should_complete_a_turn_on_a_current_thread_runtime_without_panic() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result = (&mut handle.completion).await;

        assert!(
            result.is_ok(),
            "the turn must complete on a current_thread runtime: {result:?}"
        );
    }

    /// should not hold a blocking thread for the duration of a turn
    ///
    /// A3 (K1, behavioral assertion per the architecture review): the child
    /// turn parks on the LLM await; a sentinel `spawn_blocking` on a
    /// `max_blocking_threads(1)` runtime must still complete — under the
    /// old `spawn_blocking` + `block_on` shape the child's turn occupied
    /// the pool's only blocking thread and the sentinel timed out.
    #[test]
    fn should_not_hold_a_blocking_thread_for_the_duration_of_a_turn() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let session = make_session(SessionScopeConfig::default());
            let host = SessionSubagentHost::new(
                Arc::downgrade(&session),
                make_profile_manager(),
                AgentId::new("main"),
                AgentScopeConfig::default(),
                Arc::new(HangingLlmFactory {
                    started: Arc::clone(&started),
                }),
                TurnFlowConfig::default(),
                None,
                None,
            );

            // Drive the completion future on the runtime...
            let mut handle = spawn_with_host(&host, CancelToken::new());
            let completion =
                std::mem::replace(&mut handle.completion, Box::pin(std::future::pending()));
            let task = tokio::spawn(async move {
                let mut completion = completion;
                (&mut completion).await
            });

            // ...until the turn parks on the LLM stream.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !started.load(std::sync::atomic::Ordering::SeqCst) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the child turn never reached the LLM await"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            // Then a sentinel blocking-pool task completes while the child
            // turn is still parked — the pool's single thread is free.
            let sentinel = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tokio::task::spawn_blocking(|| 42),
            )
            .await;
            assert_eq!(sentinel.expect("sentinel completes").unwrap(), 42);

            task.abort();
        });
    }

    /// should resume cleanly after the completion future is dropped mid turn
    ///
    /// A16 (M1): dropping the completion future mid-turn now CANCELS the
    /// turn at its await point instead of letting a detached blocking task
    /// run to completion — the reused TurnFlow (G14) must accept a resume
    /// afterwards without panicking or deadlocking.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_resume_cleanly_after_the_completion_future_is_dropped_mid_turn() {
        let mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FlippingLlmFactory {
                mode: Arc::clone(&mode),
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        // Given a spawned child whose turn parks on the LLM stream
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let agent_id = handle.agent_id.clone();
        let completion =
            std::mem::replace(&mut handle.completion, Box::pin(std::future::pending()));
        let task = tokio::spawn(async move {
            let mut completion = completion;
            (&mut completion).await
        });
        // Let the turn start and park (poll the spawned task briefly).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !task.is_finished(),
            "the child turn should still be parked on the LLM"
        );

        // When the completion future is dropped mid-turn (the parent
        // aborted) — the turn cancels at its await point.
        task.abort();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Then a resume on the same agent id runs cleanly to completion
        // (the mode now completes) — same TurnFlow reused (G14).
        mode.store(true, std::sync::atomic::Ordering::SeqCst);
        let resume = host
            .resume(ResumeRequest {
                agent_id,
                prompt: "continue the task".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("resume accepted");
        let mut resume = resume;
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut resume.completion)
                .await
                .expect("resume completes without hanging (A16)");
        let completion = result.expect("the resumed turn completes successfully");
        assert!(
            !completion.result.is_empty(),
            "the resumed turn must produce the scripted reply"
        );
    }
    // ===== binding resolution (B4 T2, D-48) =====

    /// A catalog entry with a default effort, for the binding tests.
    fn entry_with_effort(
        id: &str,
        effort: Option<&str>,
    ) -> crate::model_catalog::ModelCatalogEntry {
        let mut e = entry(id, &format!("model-{id}"));
        e.default_effort = effort.map(str::to_string);
        e
    }

    /// Given an explicit model alias whose catalog entry declares a default
    /// effort, When the binding is resolved, Then the catalog effort rides
    /// the binding (TS configSection.ts:96-108 tier 1).
    #[test]
    fn should_resolve_catalog_default_effort_when_explicit_model_alias_is_given() {
        let catalog = TestCatalog::new(vec![
            entry_with_effort("sonnet", Some("high")),
            entry_with_effort("haiku", None),
        ]);
        let profile = AgentProfile {
            name: "coder".to_string(),
            model: String::new(),
            ..AgentProfile::default()
        };

        let binding = resolve_subagent_binding(
            "caller-model",
            Some(ThinkingEffort::Low),
            &profile,
            Some("sonnet"),
            Some(&catalog),
        )
        .expect("binding resolves");

        assert_eq!(binding.model, "sonnet");
        assert_eq!(binding.thinking, Some(ThinkingEffort::High));

        // And an alias whose entry declares no default effort binds none —
        // not the caller's effort (the explicit-alias tier replaces it).
        let binding = resolve_subagent_binding(
            "caller-model",
            Some(ThinkingEffort::Low),
            &profile,
            Some("haiku"),
            Some(&catalog),
        )
        .expect("binding resolves");
        assert_eq!(binding.thinking, None);
    }

    #[test]
    fn should_bind_custom_catalog_default_effort_spellings_when_spawning_subagents() {
        // Given a catalog default_effort spelling the engine does not enumerate
        // Then it binds verbatim instead of silently dropping to no effort
        assert_eq!(
            parse_thinking_effort("max"),
            Some(ThinkingEffort::Custom("max".to_string()))
        );
    }

    /// Given a profile that declares no model and no effort, When the
    /// binding is resolved with no explicit alias, Then the caller's model
    /// and thinking effort are inherited.
    #[test]
    fn should_inherit_caller_model_and_thinking_when_no_override() {
        let profile = AgentProfile {
            name: "coder".to_string(),
            model: String::new(),
            ..AgentProfile::default()
        };

        let binding = resolve_subagent_binding(
            "caller-model",
            Some(ThinkingEffort::High),
            &profile,
            None,
            None,
        )
        .expect("binding resolves");

        assert_eq!(binding.model, "caller-model");
        assert_eq!(binding.thinking, Some(ThinkingEffort::High));
    }

    /// Given a profile that declares a default model and an effort, When
    /// the binding is resolved with no explicit alias, Then the profile's
    /// model wins over the caller's inheritance (and the profile effort
    /// rides along).
    #[test]
    fn should_take_profile_default_model_over_caller_inheritance() {
        let profile = AgentProfile {
            name: "coder".to_string(),
            model: String::new(),
            default_model: Some("profile-model".to_string()),
            thinking_effort: Some(ThinkingEffort::Medium),
            ..AgentProfile::default()
        };

        let binding = resolve_subagent_binding(
            "caller-model",
            Some(ThinkingEffort::High),
            &profile,
            None,
            None,
        )
        .expect("binding resolves");

        assert_eq!(binding.model, "profile-model");
        assert_eq!(binding.thinking, Some(ThinkingEffort::Medium));
    }

    /// Given a profile with no model and no default model and a caller with
    /// no current model, When the binding is resolved with no explicit
    /// alias, Then the three-nothing face fails with the curated listing
    /// (spec 11 A6) — never a silent empty model.
    #[test]
    fn should_error_with_available_models_when_all_three_tiers_empty() {
        let profile = AgentProfile {
            name: "coder-nomodel".to_string(),
            ..AgentProfile::default()
        };
        let catalog = TestCatalog::new(vec![
            entry("sonnet", "claude-sonnet-4"),
            entry("opus", "claude-opus-4"),
        ]);

        let err = match resolve_subagent_binding("", None, &profile, None, Some(&catalog)) {
            Err(err) => err,
            Ok(_) => panic!("the three-nothing chain must fail"),
        };

        let message = err.to_string();
        assert!(
            message.contains("no model resolved for subagent 'coder-nomodel'"),
            "should name the profile in the error: {message}"
        );
        assert!(
            message.contains("sonnet"),
            "should list valid options: {message}"
        );
        assert!(
            message.contains("opus"),
            "should list valid options: {message}"
        );
    }

    /// Given a caller with no current model and no catalog, When the
    /// binding is resolved with no explicit alias, Then the three-nothing
    /// face fails with the actionable (listing-less) error (spec 11 A6).
    #[test]
    fn should_error_with_actionable_message_when_three_tiers_empty_and_no_catalog() {
        let profile = AgentProfile {
            name: "coder-nomodel".to_string(),
            ..AgentProfile::default()
        };

        let err = match resolve_subagent_binding("", None, &profile, None, None) {
            Err(err) => err,
            Ok(_) => panic!("the three-nothing chain must fail"),
        };

        let message = err.to_string();
        assert!(
            message.contains("no model resolved for subagent 'coder-nomodel'"),
            "should name the profile in the error: {message}"
        );
        assert!(
            message.to_lowercase().contains("default model"),
            "should carry the configuration guidance: {message}"
        );
    }
    // ===== three-tier reachability via spawn (spec 11 R5/A3: Tier 3 live) =====

    /// An LLM factory that records the model_id every `create_llm` sees —
    /// the assertion surface for "the child's resolved binding reached
    /// provider construction" (spec 11 A3/A5). The LLM itself is the static
    /// immediate-completion mock so the child turn settles.
    struct ModelRecordingLlmFactory {
        models: Arc<Mutex<Vec<String>>>,
    }

    impl SubagentLlmFactory for ModelRecordingLlmFactory {
        fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            self.models
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(config.model_id.clone());
            Ok(Arc::new(StaticLlm::new()))
        }
    }

    /// A profile manager whose `coder-nomodel` profile declares neither
    /// `model` nor `default_model` — the four builtin profiles' no-model
    /// shape (spec 11 R6/A5), so a model-less spawn falls through to Tier 3.
    /// The essential profile is overridden to allowlist the test profile,
    /// mirroring a user configuring the main agent's `subagents`.
    fn profile_manager_with(profile: AgentProfile) -> Arc<ProfileManager> {
        let manager = Arc::new(ProfileManager::new());
        let name = profile.name.clone();
        manager.register(profile).unwrap();
        let mut essential = AgentProfile {
            name: crate::profile::ESSENTIAL_PROFILE_NAME.to_string(),
            ..Default::default()
        };
        essential.subagents = Some(vec![name]);
        manager.register(essential).unwrap();
        manager
    }

    /// Given a parent agent that switches models AFTER the host was built,
    /// When a child is spawned without a model and its profile has no
    /// default, Then the child's LLM construction sees the parent's LIVE
    /// (new) model — defect B's direct assertion (spec 11 A3).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_inherit_live_parent_model_when_spawned_without_model() {
        // Given a session whose parent agent "main" starts on "model-a", and
        // a host built over the parent's config snapshot while it was still
        // "model-a" (bound to the parent's LIVE config handle)
        let session = make_session(SessionScopeConfig::default());
        session
            .create_agent(
                AgentId::new("main"),
                AgentScopeConfig {
                    model_id: "model-a".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let main = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("main agent registered");
        let models = Arc::new(Mutex::new(Vec::new()));
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            profile_manager_with(AgentProfile {
                name: "coder-nomodel".to_string(),
                ..AgentProfile::default()
            }),
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-a".to_string(),
                ..AgentScopeConfig::default()
            },
            Arc::new(ModelRecordingLlmFactory {
                models: Arc::clone(&models),
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );
        host.set_parent_config_shared(main.config());

        // When the parent switches models to "model-b" and a child is
        // spawned without a model (defect B: the old snapshot would hand the
        // child "model-a")
        main.set_model("model-b".to_string());
        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "coder-nomodel".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: None,
                swarm_item: None,
                suppress_rate_limit_failure_event: false,
            })
            .expect("spawn with a live parent model resolves");
        let completion = (&mut handle.completion)
            .await
            .expect("the child turn completes");

        // Then the child's LLM construction saw the parent's NEW model — and
        // the child's config carries it too (the lock guard is dropped by the
        // clone before the awaits below)
        let seen: Vec<String> = models.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            seen.iter().any(|model| model == "model-b"),
            "the child LLM must be built on the parent's live model, got: {seen:?}"
        );
        assert_eq!(completion.result, "mock reply");
        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent registered");
        assert_eq!(
            child
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id,
            "model-b"
        );
    }

    /// Given a profile that declares a default model, When a child is
    /// spawned without a model while the parent carries a different live
    /// model, Then the profile's default wins over the parent's model —
    /// defect C's direct assertion (spec 11 A5).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_use_profile_default_model_over_parent_model() {
        // Given a session whose parent agent runs "model-a", a host bound to
        // the parent's LIVE config, and a profile declaring a default model
        let session = make_session(SessionScopeConfig::default());
        session
            .create_agent(
                AgentId::new("main"),
                AgentScopeConfig {
                    model_id: "model-a".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let main = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("main agent registered");
        let models = Arc::new(Mutex::new(Vec::new()));
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            profile_manager_with(AgentProfile {
                name: "coder-default-model".to_string(),
                default_model: Some("profile-default-model".to_string()),
                ..AgentProfile::default()
            }),
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-a".to_string(),
                ..AgentScopeConfig::default()
            },
            Arc::new(ModelRecordingLlmFactory {
                models: Arc::clone(&models),
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );
        host.set_parent_config_shared(main.config());

        // When the child is spawned without a model
        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "coder-default-model".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: None,
                swarm_item: None,
                suppress_rate_limit_failure_event: false,
            })
            .expect("spawn with a profile default resolves");
        let _ = (&mut handle.completion)
            .await
            .expect("the child turn completes");

        // Then the profile's default model reached the child's LLM
        // construction — the parent's live model did not override it (the
        // lock guard is dropped by the clone before the awaits below)
        let seen: Vec<String> = models.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            seen.iter().any(|model| model == "profile-default-model"),
            "the child LLM must be built on the profile's default model, got: {seen:?}"
        );
        assert!(
            !seen.iter().any(|model| model == "model-a"),
            "the parent's live model must not be used over the profile default: {seen:?}"
        );
        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent registered");
        assert_eq!(
            child
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id,
            "profile-default-model"
        );
    }

    /// Given a host whose parent live config has NO current model, a profile
    /// with no default, and no explicit model, When a child is spawned, Then
    /// the spawn is refused with the available-model listing (spec 11 A6).
    #[test]
    fn should_error_with_available_models_when_spawned_with_three_empty_tiers() {
        // Given a host (no session needed — the refusal happens synchronously)
        // whose parent live handle carries an empty model and whose catalog
        // exposes two aliases
        let shared = Arc::new(RwLock::new(AgentScopeConfig::default()));
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager_with(AgentProfile {
                name: "coder-nomodel".to_string(),
                ..AgentProfile::default()
            }),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ModelRecordingLlmFactory {
                models: Arc::new(Mutex::new(Vec::new())),
            }),
            TurnFlowConfig::default(),
            Some(Arc::new(TestCatalog::new(vec![
                entry("sonnet", "claude-sonnet-4"),
                entry("opus", "claude-opus-4"),
            ]))),
            None,
        );
        host.set_parent_config_shared(shared);

        // When the child is spawned without a model
        let err = match host.spawn(SpawnRequest {
            profile_name: "coder-nomodel".to_string(),
            prompt: "do the thing".to_string(),
            description: "test spawn".to_string(),
            model: None,
            cancel: CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
        }) {
            Err(err) => err,
            Ok(_) => panic!("the three-nothing spawn must be refused"),
        };

        // Then the error carries the curated available-model listing
        let message = err.to_string();
        assert!(
            message.contains("no model resolved for subagent 'coder-nomodel'"),
            "should name the profile in the error: {message}"
        );
        assert!(
            message.contains("sonnet"),
            "should list valid options: {message}"
        );
        assert!(
            message.contains("opus"),
            "should list valid options: {message}"
        );
    }
    // ===== nesting + inheritance (B4 T5: A1 / A4 / A5 / A6 / A7 + depth) =====

    /// A simple user tool the inheritance tests register on the parent.
    struct UpperTool;

    impl Tool for UpperTool {
        fn name(&self) -> &str {
            "Upper"
        }
        fn description(&self) -> &str {
            "Uppercases the input"
        }
        fn parameters(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                })
            })
        }
        fn resolve_execution(
            &self,
            input: serde_json::Value,
        ) -> EngineResult<crate::tool::ToolExecution> {
            let text = input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(crate::tool::ToolExecution::Runnable {
                plan: crate::tool::ExecutionPlan {
                    accesses: Vec::new(),
                    display: crate::tool::ToolInputDisplay::Generic {
                        summary: "Uppercase the input".to_string(),
                    },
                    approval_rule: "Upper".to_string(),
                    stop_batch_after_this: false,
                },
                execute: Box::new(move |_ctx| {
                    Box::pin(async move { crate::tool::ToolResult::success(text.to_uppercase()) })
                }),
            })
        }
    }

    /// An LLM that walks a scripted queue of responses (each chat call
    /// pops the next script entry) — the multi-step child face for the
    /// nesting e2e tests.
    struct ScriptedLlm {
        script: std::sync::Mutex<Vec<Vec<LlmEvent>>>,
        /// Every prompt's last user text, recorded for assertions.
        seen_prompts: std::sync::Mutex<Vec<String>>,
        capability: ModelCapability,
    }

    impl Llm for ScriptedLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
            let last_user = params
                .messages
                .iter()
                .rev()
                .find_map(|m| {
                    if m.role == Role::User {
                        Some(m.content.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let text: String = last_user
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            self.seen_prompts.lock().unwrap().push(text);
            let events = self
                .script
                .lock()
                .unwrap()
                .pop()
                .expect("the scripted LLM ran out of responses");
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// A factory that hands the dispatching model a `ScriptedLlm` walking
    /// `script` (consumed back-to-front — the FIRST response is the LAST
    /// element) and every other model the static `mock reply` LLM.
    struct ScriptedLlmFactory {
        script: Vec<Vec<LlmEvent>>,
        dispatching_model: &'static str,
    }

    impl SubagentLlmFactory for ScriptedLlmFactory {
        fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            if config.model_id == self.dispatching_model {
                Ok(Arc::new(ScriptedLlm {
                    script: std::sync::Mutex::new(self.script.clone()),
                    seen_prompts: std::sync::Mutex::new(Vec::new()),
                    capability: ModelCapability::default(),
                }))
            } else {
                Ok(Arc::new(StaticLlm::new()))
            }
        }
    }

    /// A single scripted response: one assistant text part, done.
    fn text_response(text: &str) -> Vec<LlmEvent> {
        vec![
            LlmEvent::TextPart {
                text: text.to_string(),
            },
            LlmEvent::Done {
                stop_reason: "completed".to_string(),
            },
        ]
    }

    /// A single scripted response: one Agent tool call, done (tool-use
    /// stop so the loop executes it).
    fn agent_dispatch_response(subagent_type: &str, prompt: &str) -> Vec<LlmEvent> {
        vec![
            LlmEvent::ToolCall {
                tool: "Agent".to_string(),
                args: serde_json::json!({
                    "subagent_type": subagent_type,
                    "prompt": prompt,
                    "description": "grandchild work"
                }),
                call_id: "call-1".to_string(),
            },
            LlmEvent::Done {
                stop_reason: "tool_calls".to_string(),
            },
        ]
    }

    /// Given a child whose turn dispatches a grandchild through the Agent
    /// tool, When both turns complete, Then the two-level nesting succeeds
    /// without deadlocking (A1 — the 10s timeout makes a deadlock a test
    /// failure) and the grandchild's answer reaches the child.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_spawn_a_grandchild_when_a_subagent_dispatches_another_agent() {
        let session = make_session(SessionScopeConfig::default());
        // The coder profile dispatches; the reviewer profile answers
        // directly (its model gets the static reply LLM).
        let profile_manager = Arc::new(ProfileManager::new());
        profile_manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "scripted-model".to_string(),
                // The coder may dispatch reviewers (default-deny: without
                // this list the child could not spawn the grandchild).
                subagents: Some(vec!["reviewer".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();
        profile_manager
            .register(AgentProfile {
                name: "reviewer".to_string(),
                model: "static-model".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();

        // The child (coder) first dispatches a reviewer grandchild, then
        // finishes with its own summary text (the queue is consumed
        // back-to-front: the dispatch is the LAST element).
        let factory = ScriptedLlmFactory {
            script: vec![
                text_response("the child is done"),
                agent_dispatch_response("reviewer", "review the thing"),
            ],
            dispatching_model: "scripted-model",
        };
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            profile_manager,
            AgentId::new("main"),
            // Yolo so the child's Agent tool call is not permission-gated.
            AgentScopeConfig {
                permission_mode: PermissionMode::Yolo,
                ..AgentScopeConfig::default()
            },
            Arc::new(factory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut handle.completion)
                .await
                .expect("two-level nesting completes without deadlock (A1)");

        // Then the child completed and its summary is the final text
        let completion = result.expect("child completes");
        assert_eq!(completion.result, "the child is done");

        // And the grandchild ran to completion too: the child's turn
        // consumed the tool result (its second LLM call happened because
        // the first returned tool_calls) — evidenced by the child's
        // context memory holding the grandchild's Agent tool result.
        let agent = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent in session");
        let memory = agent.context_memory();
        let memory = memory.lock().unwrap();
        let history_dump = format!("{:?}", memory.history());
        assert!(
            history_dump.contains("mock reply"),
            "the grandchild's reply must reach the child's context, got: {history_dump}"
        );
    }

    /// Given a parent agent whose live permission mode differs from its
    /// config snapshot, When a child is spawned, Then the child inherits
    /// the LIVE mode (A4).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_inherit_the_callers_live_permission_mode_when_spawning_a_child() {
        let session = make_session(SessionScopeConfig::default());
        let parent = session
            .create_agent(
                AgentId::new("main"),
                AgentScopeConfig {
                    permission_mode: PermissionMode::Yolo,
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("parent agent");
        // The parent's mode flipped at runtime after creation.
        parent.permission_manager().set_mode(PermissionMode::Auto);

        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig {
                permission_mode: PermissionMode::Yolo,
                ..AgentScopeConfig::default()
            },
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");

        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        assert_eq!(
            child.permission_manager().mode(),
            PermissionMode::Auto,
            "the child must inherit the parent's LIVE mode, not the snapshot"
        );
    }

    /// Given a parent agent with a registered user tool, When a child is
    /// spawned, Then the child's ToolManager carries the same user tool
    /// (A5).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_inherit_the_callers_user_tools_when_spawning_a_child() {
        let session = make_session(SessionScopeConfig::default());
        let parent = session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("parent agent");
        {
            let tools = parent.tool_manager();
            let mut tools = tools.write();
            tools.register_user_tool(Arc::new(UpperTool));
        }

        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");

        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        let tools = child.tool_manager();
        let tools = tools.read();
        assert!(
            tools.find_tool("Upper").is_some(),
            "the child must inherit the parent's user tools"
        );
    }

    /// Given a session whose main agent has an MCP tool registered, When a
    /// child is spawned with a profile that declares no tool whitelist,
    /// Then the child's tool table holds the same MCP tool (SDR-014
    /// delivery obligation; the restore path's hydration semantics applied
    /// to live spawn — ADR-014).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_inherit_the_sessions_mcp_tools_when_spawning_a_child_with_the_default_surface()
    {
        let session = make_session(SessionScopeConfig::default());
        let parent = session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("parent agent");
        // The convergence point carries one MCP tool, as a connected server's
        // fan-out registration would have left it.
        {
            let tools = parent.tool_manager();
            let mut tools = tools.write();
            tools.register_mcp_tool("mcp-atlassian".to_string(), Arc::new(UpperTool));
        }

        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");

        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        let tools = child.tool_manager();
        let tools = tools.read();
        assert!(
            tools.find_tool("Upper").is_some(),
            "the child must hold the session's MCP tools in its tool table"
        );
    }

    /// Given a session whose main agent has an MCP tool registered, When a
    /// child is spawned with a profile whose tool whitelist omits MCP tools,
    /// Then the child's active surface does not expose the MCP tool —
    /// delivery (registration) must never widen the profile's authorization
    /// (SDR-014: the profile's declaration governs the usable surface).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_expose_mcp_tools_to_a_child_whose_profile_omits_them() {
        let session = make_session(SessionScopeConfig::default());
        let parent = session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("parent agent");
        {
            let tools = parent.tool_manager();
            let mut tools = tools.write();
            tools.register_mcp_tool("mcp-atlassian".to_string(), Arc::new(UpperTool));
        }

        // A restricted profile: only the read tools, no MCP entries.
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "reader".to_string(),
                model: "test-model".to_string(),
                tools: Some(vec!["Read".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();
        // Default-deny: allowlist the test profile on the essential main agent.
        manager
            .register(AgentProfile {
                name: "agent".to_string(),
                subagents: Some(vec!["reader".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();

        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            manager,
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = host
            .spawn(SpawnRequest {
                profile_name: "reader".to_string(),
                prompt: "do the thing".to_string(),
                description: "test spawn".to_string(),
                model: None,
                cancel: CancelToken::new(),
                run_in_background: false,
                parent_tool_call_id: None,
                swarm_index: None,
                swarm_item: None,
                suppress_rate_limit_failure_event: false,
            })
            .unwrap();
        let _ = (&mut handle.completion).await.expect("child completes");

        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        let tools = child.tool_manager();
        let tools = tools.read();
        // Raw registration may hold the tool (delivery ≠ authorization);
        // the ACTIVE surface — what the LLM sees — must not.
        assert!(
            !tools.loop_tools().iter().any(|t| t.name == "Upper"),
            "a profile that omits MCP tools must not have them on its active surface"
        );
    }

    /// Given a profile that declares a promptPrefix, When a child is
    /// spawned, Then the dispatch prompt carries the prefix (A6).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_the_profiles_prompt_prefix_to_the_spawn_prompt() {
        let session = make_session(SessionScopeConfig::default());
        let profile_manager = Arc::new(ProfileManager::new());
        profile_manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "test-model".to_string(),
                prompt_prefix: Some("PREFIX: operate carefully.".to_string()),
                ..AgentProfile::default()
            })
            .unwrap();

        let factory = ScriptedLlmFactory {
            script: vec![text_response("ok")],
            dispatching_model: "test-model",
        };
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            profile_manager,
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(factory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");

        // The prompt the child's LLM saw carries the prefix + the prompt.
        // (The scripted factory's queue was consumed by the child's own
        // LLM; the seen-prompts are not directly reachable, so assert via
        // the child's context memory: the user message carries the prefix.)
        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        let memory = child.context_memory();
        let memory = memory.lock().unwrap();
        let user_texts: Vec<String> = memory
            .history()
            .iter()
            .filter(|m| m.message.role == Role::User)
            .map(|m| {
                m.message
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect();
        assert!(
            user_texts
                .iter()
                .any(|t| t.starts_with("PREFIX: operate carefully.\n\ndo the thing")),
            "the prompt prefix must ride the dispatch prompt: {user_texts:?}"
        );
    }

    /// Given a child profile that declares a subagent allowlist not
    /// containing the grandchild type, When the child dispatches, Then the
    /// grandchild dispatch is refused by the R3-4 named arm (A7).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_enforce_the_child_profiles_subagent_allowlist_for_grandchild_dispatch() {
        let session = make_session(SessionScopeConfig::default());
        let profile_manager = Arc::new(ProfileManager::new());
        profile_manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "scripted-model".to_string(),
                // The coder may only dispatch reviewers.
                subagents: Some(vec!["reviewer".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();
        profile_manager
            .register(AgentProfile {
                name: "reviewer".to_string(),
                model: "static-model".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();

        let factory = ScriptedLlmFactory {
            script: vec![
                text_response("the child finished after the refusal"),
                agent_dispatch_response("planner", "try to dispatch me"),
            ],
            dispatching_model: "scripted-model",
        };
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            profile_manager,
            AgentId::new("main"),
            AgentScopeConfig {
                permission_mode: PermissionMode::Yolo,
                ..AgentScopeConfig::default()
            },
            Arc::new(factory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut handle.completion)
                .await
                .expect("the child turn completes");
        assert!(result.is_ok(), "the child itself completes: {result:?}");

        // Then the grandchild dispatch was refused and the refusal rode
        // the Agent tool result into the child's context.
        let child = session
            .get_agent(&AgentId::new(&handle.agent_id))
            .await
            .expect("child agent");
        let memory = child.context_memory();
        let memory = memory.lock().unwrap();
        let history_dump = format!("{:?}", memory.history());
        assert!(
            history_dump.contains("is not allowed for this agent"),
            "the grandchild dispatch must be refused through the child profile's allowlist: {history_dump}"
        );
    }

    /// Given a host already at the configured maximum depth, When it
    /// spawns, Then the dispatch is refused with the depth message
    /// (SDR-B4-03; the 6th level with the default of 5).
    #[test]
    fn should_refuse_dispatch_beyond_the_configured_nesting_depth() {
        let profile_manager = make_profile_manager();
        let host = SessionSubagentHost::new(
            Weak::new(),
            profile_manager,
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        )
        .with_nesting_limits(5, 5);

        let error = host
            .spawn(spawn_request("coder"))
            .err()
            .expect("the depth-6 dispatch is refused");
        match error {
            EngineError::RequestInvalid(message) => {
                assert!(
                    message.contains("nesting depth would reach 6"),
                    "should name the refused depth: {message}"
                );
                assert!(
                    message.contains("maximum is 5"),
                    "should name the configured maximum: {message}"
                );
            }
            other => panic!("expected RequestInvalid, got: {other}"),
        }

        // And one level inside the limit still dispatches.
        let host = SessionSubagentHost::new(
            Weak::new(),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        )
        .with_nesting_limits(4, 5);
        assert!(
            host.spawn(spawn_request("coder")).is_ok(),
            "the depth-5 dispatch is allowed at the boundary"
        );
    }

    /// Given maxNestingDepth = 0 (the TS-parity unlimited setting), When a
    /// deep host dispatches, Then the depth guard does not apply.
    #[test]
    fn should_allow_dispatch_beyond_the_default_depth_when_max_nesting_depth_is_zero() {
        let host = SessionSubagentHost::new(
            Weak::new(),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(FailingLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        )
        .with_nesting_limits(50, 0);

        assert!(
            host.spawn(spawn_request("coder")).is_ok(),
            "depth 0 disables the nesting limit (TS parity)"
        );
    }
    // ===== summaryPolicy distillation (B4 T6: A9 / A10) =====

    /// A profile manager whose `coder` declares the given summary policy.
    fn make_distill_profile_manager(policy: Option<ProfileSummaryPolicy>) -> Arc<ProfileManager> {
        let manager = Arc::new(ProfileManager::new());
        manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "scripted-model".to_string(),
                summary_policy: policy,
                ..AgentProfile::default()
            })
            .unwrap();
        manager
    }

    /// Given a summary already meeting minChars, When the turn completes,
    /// Then it is returned unchanged with no continuation turn (the
    /// scripted queue has exactly one response — a second LLM call would
    /// panic the queue).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_the_summary_unchanged_when_it_meets_min_chars() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_distill_profile_manager(Some(ProfileSummaryPolicy {
                min_chars: 5,
                continuation_prompt: "expand the summary".to_string(),
                retries: 3,
            })),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ScriptedLlmFactory {
                script: vec![text_response("mock reply")],
                dispatching_model: "scripted-model",
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let completion = (&mut handle.completion).await.expect("completes");

        assert_eq!(completion.result, "mock reply");
    }

    /// Given a summary shorter than minChars, When the turn completes, Then
    /// the turn continues with the continuation prompt until the summary
    /// is long enough.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_continue_the_turn_with_the_continuation_prompt_when_summary_is_short() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_distill_profile_manager(Some(ProfileSummaryPolicy {
                min_chars: 40,
                continuation_prompt: "expand the summary".to_string(),
                retries: 3,
            })),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ScriptedLlmFactory {
                // Back-to-front: first "a short reply", then the adequate one.
                script: vec![
                    text_response("the expanded final summary is definitely long enough now"),
                    text_response("a short reply"),
                ],
                dispatching_model: "scripted-model",
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let completion = (&mut handle.completion).await.expect("completes");

        assert_eq!(
            completion.result, "the expanded final summary is definitely long enough now",
            "the continued (adequate) summary must win"
        );
    }

    /// Given a policy whose retries run out while every summary stays
    /// short, When the turn completes, Then the last non-empty summary is
    /// returned after exactly `retries` continuations.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_stop_after_policy_retries_when_summary_stays_short() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_distill_profile_manager(Some(ProfileSummaryPolicy {
                min_chars: 100,
                continuation_prompt: "expand the summary".to_string(),
                retries: 2,
            })),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ScriptedLlmFactory {
                // Back-to-front: first, two continuations, third is last.
                script: vec![
                    text_response("third short"),
                    text_response("second short"),
                    text_response("first short"),
                ],
                dispatching_model: "scripted-model",
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let completion = (&mut handle.completion).await.expect("completes");

        assert_eq!(
            completion.result, "third short",
            "the last non-empty summary after the retries are exhausted"
        );
    }

    /// Given a turn truncated at max output tokens, When the turn
    /// completes, Then the completion fails with the TS-verbatim
    /// max-tokens message.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_fail_with_max_tokens_message_when_a_turn_is_truncated() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_distill_profile_manager(Some(ProfileSummaryPolicy {
                min_chars: 100,
                continuation_prompt: "expand the summary".to_string(),
                retries: 2,
            })),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(ScriptedLlmFactory {
                script: vec![vec![
                    LlmEvent::TextPart {
                        text: "trunca".to_string(),
                    },
                    LlmEvent::Done {
                        stop_reason: "truncated".to_string(),
                    },
                ]],
                dispatching_model: "scripted-model",
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let error = (&mut handle.completion)
            .await
            .expect_err("the truncated turn fails");

        match error {
            SubagentError::TurnFailed(message) => assert_eq!(
                message,
                "Subagent turn failed before completing its final summary: reason=max_tokens"
            ),
            other => panic!("expected the max-tokens failure, got {other:?}"),
        }
    }
    // ===== per-subagent journals (B4 T7: A11 / A12 / A15) =====

    /// Install a real wire log for the session under `dir` and return the
    /// main journal's path.
    async fn install_session_wire_log(
        session: &Arc<SessionScope>,
        dir: &std::path::Path,
    ) -> std::path::PathBuf {
        let path = dir.join("wire.jsonl");
        let writer = mirri_persistence::JsonlAppendLog::create(&path)
            .await
            .expect("session wire log");
        session.install_wire_log(writer).await;
        path
    }

    fn read_log(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|_| String::new())
    }

    /// Given a child spawned in a session with a live wire log, When its
    /// turn completes, Then the child's turn ops land in its own journal
    /// and the main journal carries NO op type the child's turn produced
    /// (A11 — the turn-time event ops included, not just turn.prompt).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_write_child_turn_ops_to_the_child_journal_not_the_session_wire_log() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(SessionScopeConfig::default());
        let main_path = install_session_wire_log(&session, tmp.path()).await;
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let completion = (&mut handle.completion).await.expect("child completes");
        assert_eq!(completion.result, "mock reply");

        // Then the child's journal exists and carries the turn's ops.
        let child_path = tmp
            .path()
            .join("agents")
            .join(&handle.agent_id)
            .join("wire.jsonl");
        let child_log = read_log(&child_path);
        assert!(
            child_log.contains("\"type\":\"turn.prompt\""),
            "child journal should carry turn.prompt: {child_log}"
        );
        assert!(
            child_log.contains("\"type\":\"context.append_message\""),
            "child journal should carry the conversation: {child_log}"
        );
        assert!(
            child_log.contains("\"type\":\"metadata\""),
            "child journal should open with the metadata envelope: {child_log}"
        );

        // And the main journal carries NO op type the child's turn
        // produced — the full child-turn op surface, turn-time event ops
        // included (request.header.changed, context_size.measured, ...).
        let main_log = read_log(&main_path);
        let child_turn_op_types = [
            "turn.prompt",
            "turn.steer",
            "turn.steer_flushed",
            "turn.cancel",
            "turn.ended",
            "context.append_message",
            "context.append_loop_event",
            "context.clear",
            "context.apply_compaction",
            "context.undo",
            "context_size.measured",
            "usage.record",
            "llm.request",
            "llm.tools_snapshot",
            "request.header.changed",
            "interruptionReminder.recorded",
        ];
        for op_type in child_turn_op_types {
            assert!(
                !main_log.contains(&format!("\"type\":\"{op_type}\"")),
                "the main journal must not carry the child-turn op {op_type}: {main_log}"
            );
        }
    }

    /// Given a child whose turn creates a goal, When the turn completes,
    /// Then the session-domain `goal.create` op lands in the MAIN journal
    /// and never in the child's journal (A12).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_session_domain_ops_to_the_main_journal_when_a_child_records_them() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(SessionScopeConfig::default());
        let main_path = install_session_wire_log(&session, tmp.path()).await;
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig {
                permission_mode: PermissionMode::Yolo,
                ..AgentScopeConfig::default()
            },
            Arc::new(ScriptedLlmFactory {
                script: vec![
                    text_response("goal created"),
                    vec![
                        LlmEvent::ToolCall {
                            tool: "CronCreate".to_string(),
                            args: serde_json::json!({
                                "cron": "0 9 * * *",
                                "prompt": "check the build"
                            }),
                            call_id: "call-1".to_string(),
                        },
                        LlmEvent::Done {
                            stop_reason: "tool_calls".to_string(),
                        },
                    ],
                ],
                dispatching_model: "test-model",
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let completion = (&mut handle.completion).await.expect("child completes");
        assert_eq!(completion.result, "goal created");

        // Then cron.add is in the MAIN journal...
        let main_log = read_log(&main_path);
        assert!(
            main_log.contains("\"type\":\"cron.add\""),
            "the session-domain op must reach the main journal: {main_log}"
        );
        // ...and not in the child's journal.
        let child_path = tmp
            .path()
            .join("agents")
            .join(&handle.agent_id)
            .join("wire.jsonl");
        let child_log = read_log(&child_path);
        assert!(
            !child_log.contains("\"type\":\"cron.add\""),
            "the session-domain op must never reach the child journal: {child_log}"
        );
    }

    /// Given a deferred (SDR-004 lazy) session, When a child runs before
    /// the session materializes, Then its journal buffers and flushes on
    /// the session's materialization with its ops intact.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_defer_child_journal_until_the_session_materializes() {
        let tmp = tempfile::tempdir().unwrap();
        let session = make_session(SessionScopeConfig::default());
        session.defer_wire_log().await;
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        // The child's turn runs to completion while the session is still
        // deferred — no journal files exist yet.
        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");
        let child_path = tmp
            .path()
            .join("agents")
            .join(&handle.agent_id)
            .join("wire.jsonl");
        assert!(
            !child_path.exists(),
            "no child journal before materialization"
        );

        // When the session materializes (the first user message on a lazy
        // session), every registered child journal flushes with it.
        let main_path = tmp.path().join("wire.jsonl");
        let main_path_clone = main_path.clone();
        assert!(
            session
                .materialize_wire_log(async move {
                    mirri_persistence::JsonlAppendLog::create(&main_path_clone).await
                })
                .await
                .expect("materialize"),
            "the first materialization installs the writer"
        );

        let child_log = read_log(&child_path);
        assert!(
            child_log.contains("\"type\":\"turn.prompt\""),
            "the buffered child ops flush at materialization: {child_log}"
        );
        // Review round 2, F3: the envelope is minted at materialization
        // (where the target path is knowable), PREPENDED ahead of the ops
        // buffered during the deferred window, exactly once.
        let first_line_leads_with_metadata = child_log
            .lines()
            .next()
            .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .is_some_and(|t| t == "metadata");
        assert!(
            first_line_leads_with_metadata,
            "the envelope must lead the flushed journal: {child_log}"
        );
        let metadata_lines = child_log
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                    })
                    .is_some_and(|t| t == "metadata")
            })
            .count();
        assert_eq!(
            metadata_lines, 1,
            "the deferred-window journal must carry exactly one envelope: {child_log}"
        );
    }

    /// Given an idle child, When a notification-turn claim is taken and
    /// held, Then the child reads as running (a host resume rejects) until
    /// the claim drops — the claim is the atomic exclusion the notification
    /// driver needs (review round 2, F1).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_exclude_a_notification_turn_claim_from_a_host_resume() {
        let session = make_session(SessionScopeConfig::default());
        let host = SessionSubagentHost::new(
            Arc::downgrade(&session),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(StaticLlmFactory),
            TurnFlowConfig::default(),
            None,
            None,
        );

        let mut handle = spawn_with_host(&host, CancelToken::new());
        let _ = (&mut handle.completion).await.expect("child completes");
        let child_id = handle.agent_id.clone();

        // Idle: the claim succeeds and reads as running.
        let claim = host
            .claim_child_notification_turn(&child_id)
            .expect("an idle child is claimable");
        assert!(
            host.is_child_running(&child_id),
            "a held claim must read as running"
        );
        // A second claim on the same child is busy.
        assert!(
            host.claim_child_notification_turn(&child_id).is_none(),
            "a claimed child must not be claimable again"
        );

        // Dropping the claim releases the child — the host resume that
        // raced the notification turn now goes through.
        drop(claim);
        assert!(
            !host.is_child_running(&child_id),
            "dropping the claim must settle the child"
        );
        let mut resume = host
            .resume(ResumeRequest {
                agent_id: child_id.clone(),
                prompt: "after the notification turn".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("the child resumes after the claim drops");
        let completion =
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut resume.completion)
                .await
                .expect("the resumed turn completes")
                .expect("the resumed turn succeeds");
        assert_eq!(completion.result, "mock reply");
    }
    // ===== restart replay + resume (B4 T8: A13) =====

    /// An LLM factory whose LLMs record every chat's full message content
    /// into a shared log — the assertion surface for "the resumed child
    /// answers from its restored memory".
    struct RecordingLlmFactory {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl SubagentLlmFactory for RecordingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(RecordingLlm {
                seen: Arc::clone(&self.seen),
                capability: ModelCapability::default(),
            }))
        }
    }

    struct RecordingLlm {
        seen: Arc<Mutex<Vec<String>>>,
        capability: ModelCapability,
    }

    impl Llm for RecordingLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
            let dump = params
                .messages
                .iter()
                .map(|m| {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    format!("{:?}: {}", m.role, text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.seen.lock().unwrap().push(dump);
            Ok(Box::pin(futures::stream::iter(vec![
                LlmEvent::TextPart {
                    text: "resumed answer".to_string(),
                },
                LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                },
            ])))
        }
    }

    /// Locate a child's on-disk journal (`agents/<id>/wire.jsonl`) under a
    /// sessions root — the restart tests' durability probe.
    fn find_child_journal(root: &std::path::Path, child_id: &str) -> std::path::PathBuf {
        let expected = std::path::Path::new("agents")
            .join(child_id)
            .join("wire.jsonl");
        fn walk(dir: &std::path::Path, expected: &std::path::Path) -> Option<std::path::PathBuf> {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if let Some(hit) = walk(&path, expected) {
                        return Some(hit);
                    }
                } else if path.ends_with(expected) {
                    return Some(path);
                }
            }
            None
        }
        walk(root, &expected).unwrap_or_else(|| {
            panic!(
                "the child journal must exist under {}: agents/{}/wire.jsonl",
                root.display(),
                child_id
            )
        })
    }

    /// A session manager over a temp workspace — the restart-replay face.
    async fn make_session_manager(tmp: &std::path::Path) -> (crate::SessionManager, SessionId) {
        let app = crate::AppScope::new(crate::AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                crate::WorkspaceId::new("test-ws"),
                tmp.to_path_buf(),
                crate::WorkspaceScopeConfig::default(),
            )
            .await
            .unwrap();
        let manager = crate::SessionManager::with_persistence_dir(
            workspace,
            tmp.join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session_with_id(
                crate::SessionId::new("subagent-replay"),
                crate::SessionMetadata::default(),
                crate::SessionScopeConfig::default(),
            )
            .await
            .unwrap();
        (manager, session.id().clone())
    }

    /// Given a session whose child completed a turn and the process
    /// restarted, When the session resumes and the child is resumed through
    /// a fresh host, Then the child answers from its restored memory (the
    /// prompt it sees carries the first round) — A13.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_replay_a_subagents_journal_after_restart_and_allow_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, session_id) = make_session_manager(tmp.path()).await;

        // Round 1: spawn a child through a host over the persisted session
        // and let it complete a turn.
        {
            let session = manager.get_session(&session_id).await.unwrap();
            let host = SessionSubagentHost::new(
                Arc::downgrade(&session),
                make_profile_manager(),
                AgentId::new("main"),
                AgentScopeConfig::default(),
                Arc::new(StaticLlmFactory),
                TurnFlowConfig::default(),
                None,
                None,
            );
            let mut handle = spawn_with_host(&host, CancelToken::new());
            let completion = (&mut handle.completion).await.expect("child completes");
            assert_eq!(completion.result, "mock reply");
            // The main agent's model record (spec 11 R4): the resume chain
            // resolves the model from the wire log's record, else the base
            // config — this fixture's host never set one, so record what an
            // embedded create would have written.
            session
                .record_op(WireOp::ConfigUpdate {
                    model_alias: Some("test-model".to_string()),
                    profile_name: None,
                    thinking_effort: None,
                    system_prompt: None,
                    disallowed_tools: None,
                })
                .await;
        }

        // The restart: the session and host are gone; the process state is
        // only the sessions dir on disk.
        drop_session_from_workspace(&manager, &session_id).await;

        // Round 2: resume the session from its wire logs.
        let resumed = manager
            .resume_session(&session_id)
            .await
            .expect("session resumes");

        // Then the child's agent stays DORMANT after the lazy resume
        // (ADR-006/ADR-017): its identity is in the roster, but its agent
        // scope is not created until the child is woken.
        let child_id = resumed
            .list_subagent_registrations()
            .await
            .into_iter()
            .map(|r| r.agent_id)
            .find(|id| id.starts_with("agent-"))
            .expect("the child is in the roster");
        assert!(
            resumed.get_agent(&AgentId::new(&child_id)).await.is_none(),
            "the child must stay dormant (no AgentScope) until it is woken"
        );
        assert!(
            resumed
                .list_agents()
                .await
                .into_iter()
                .map(|id| id.as_str().to_string())
                .all(|id| id != child_id),
            "the dormant child must not appear in list_agents"
        );

        // Waking the child through a session hydrator (the hook the resume
        // path installs) hydrates its AgentScope with the journal-rebuilt
        // memory and identity.
        manager.install_subagent_hydrator(&resumed);
        assert!(
            resumed.hydrate_subagent(&child_id).await,
            "the dormant child hydrates on wake"
        );
        let child = resumed
            .get_agent(&AgentId::new(&child_id))
            .await
            .expect("child agent");
        {
            let config = child.config();
            let guard = config.read().unwrap_or_else(|e| e.into_inner());
            assert_eq!(guard.profile_name.as_deref(), Some("coder"));
            assert_eq!(guard.model_id, "test-model");
        }
        {
            let memory = child.context_memory();
            let memory = memory.lock().unwrap();
            let history = format!("{:?}", memory.history());
            assert!(
                history.contains("do the thing"),
                "the child's memory must carry the first-round prompt: {history}"
            );
        }

        // And a fresh host adopts the restored child, so `Agent(resume=...)`
        // answers from the old context (the LLM sees the first round).
        let seen = Arc::new(Mutex::new(Vec::new()));
        let host = SessionSubagentHost::new(
            Arc::downgrade(&resumed),
            make_profile_manager(),
            AgentId::new("main"),
            AgentScopeConfig::default(),
            Arc::new(RecordingLlmFactory {
                seen: Arc::clone(&seen),
            }),
            TurnFlowConfig::default(),
            None,
            None,
        );
        host.adopt_session_agents().await;
        let mut resume = host
            .resume(ResumeRequest {
                agent_id: child_id.clone(),
                prompt: "continue the task".to_string(),
                cancel: CancelToken::new(),
                retry: false,
                suppress_rate_limit_failure_event: false,
            })
            .expect("the adopted child resumes");
        let completion =
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut resume.completion)
                .await
                .expect("the resumed turn completes")
                .expect("the resumed turn succeeds");
        assert_eq!(completion.result, "resumed answer");

        let seen = seen.lock().unwrap().join("\n---\n");
        assert!(
            seen.contains("do the thing"),
            "the resumed child must answer from its restored memory (the              first-round prompt visible to the LLM): {seen}"
        );

        // Review S1 regression: the resumed turn must have APPENDED to the
        // adopted journal — never truncated it — so the pre-restart history
        // is still on disk, and no second metadata envelope was minted.
        let journal = find_child_journal(tmp.path(), &child_id);
        let journal_text = std::fs::read_to_string(&journal).unwrap();
        assert!(
            journal_text.contains("do the thing"),
            "the pre-restart turn must survive the resumed turn (adopt, never truncate): {journal_text}"
        );
        // Review S2 regression: the resume dispatch is bracketed on the
        // child journal (turn.prompt + context.append_message), so a later
        // restart folds the resume instruction too.
        assert!(
            journal_text.contains("continue the task"),
            "the resume prompt must be journaled on the child's own log: {journal_text}"
        );
        let metadata_lines = journal_text
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                    })
                    .is_some_and(|t| t == "metadata")
            })
            .count();
        assert_eq!(
            metadata_lines, 1,
            "adopting the existing journal must not mint a second metadata envelope: {journal_text}"
        );

        // The second restart folds BOTH rounds (S1's acceptance): the
        // journal's durability is the child's memory across restarts.
        drop_session_from_workspace(&manager, &session_id).await;
        let second = manager
            .resume_session(&session_id)
            .await
            .expect("the second restart resumes");
        assert!(
            second.hydrate_subagent(&child_id).await,
            "the dormant child hydrates on wake after the second restart"
        );
        let child_again = second
            .get_agent(&AgentId::new(&child_id))
            .await
            .expect("the child rehydrates after the second restart");
        let memory = child_again.context_memory();
        let memory = memory.lock().unwrap();
        let history = format!("{:?}", memory.history());
        assert!(
            history.contains("do the thing") && history.contains("continue the task"),
            "the second restart must fold both rounds from the durable journal: {history}"
        );
    }

    /// Given a session with a registered child, When the session restarts
    /// and resumes, Then the registry rebuilds the child's metadata (the
    /// profile, model, and identity survive the restart) — A13.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_rebuild_children_metadata_from_the_registry_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (manager, session_id) = make_session_manager(tmp.path()).await;

        // Round 1: one child with a swarm label.
        {
            let session = manager.get_session(&session_id).await.unwrap();
            let host = SessionSubagentHost::new(
                Arc::downgrade(&session),
                make_profile_manager(),
                AgentId::new("main"),
                AgentScopeConfig::default(),
                Arc::new(StaticLlmFactory),
                TurnFlowConfig::default(),
                None,
                None,
            );
            let mut request = spawn_request("coder");
            request.swarm_item = Some("batch-1".to_string());
            let mut handle = host.spawn(request).unwrap();
            let _ = (&mut handle.completion).await.expect("child completes");
            // The main agent's model record (spec 11 R4): the resume chain
            // resolves the model from the wire log's record, else the base
            // config — this fixture's host never set one, so record what an
            // embedded create would have written.
            session
                .record_op(WireOp::ConfigUpdate {
                    model_alias: Some("test-model".to_string()),
                    profile_name: None,
                    thinking_effort: None,
                    system_prompt: None,
                    disallowed_tools: None,
                })
                .await;
        }
        drop_session_from_workspace(&manager, &session_id).await;

        // Round 2: the main journal's fold carries the registry; the child stays
        // DORMANT (no AgentScope) until woken — the lazy-restore contract.
        let resumed = manager
            .resume_session(&session_id)
            .await
            .expect("session resumes");
        let agents: Vec<String> = resumed
            .list_agents()
            .await
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect();
        let child_ids: Vec<String> = resumed
            .list_subagent_registrations()
            .await
            .into_iter()
            .map(|r| r.agent_id)
            .collect();
        assert_eq!(
            child_ids.len(),
            1,
            "exactly the registered child is in the roster"
        );
        assert!(
            agents.iter().all(|id| !id.starts_with("agent-")),
            "the child must stay dormant after resume (no AgentScope): {agents:?}"
        );
        assert!(
            resumed
                .get_agent(&AgentId::new(&child_ids[0]))
                .await
                .is_none(),
            "the dormant child is not an agent scope yet"
        );
        // Waking the child hydrates it with the registry-rebuilt metadata.
        manager.install_subagent_hydrator(&resumed);
        assert!(
            resumed.hydrate_subagent(&child_ids[0]).await,
            "the dormant child hydrates on wake"
        );
        let child = resumed
            .get_agent(&AgentId::new(&child_ids[0]))
            .await
            .expect("child agent");
        let config = child.config();
        let guard = config.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(guard.profile_name.as_deref(), Some("coder"));
        assert_eq!(guard.model_id, "test-model");
    }

    /// Close (and thereby drop) the live session from the workspace so a
    /// resume is possible — the restart stand-in.
    async fn drop_session_from_workspace(
        manager: &crate::SessionManager,
        session_id: &crate::SessionId,
    ) {
        manager
            .close_session(session_id, crate::CloseReason::Exit)
            .await
            .unwrap();
        assert!(manager.get_session(session_id).await.is_none());
    }
}
