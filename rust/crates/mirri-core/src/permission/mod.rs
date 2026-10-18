//! Permission gate and policy chain.
//!
//! The permission system evaluates an ordered chain of policies for every tool
//! call. The first policy that returns a non-`Pass` result wins. This mirrors
//! the TypeScript `PermissionManager` with its 20-policy chain.
//!
//! Three permission modes control the overall behavior:
//! - [`PermissionMode::Manual`] — ask the user for anything not explicitly
//!   allowed or denied.
//! - [`PermissionMode::Yolo`] — approve everything (unless explicitly denied).
//! - [`PermissionMode::Auto`] — approve everything except `AskUserQuestion`
//!   (which is denied).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::cancel::CancelToken;
use crate::hook_engine::{HookEngine, HookEventType, PermissionResultOutcome};
use crate::hooks::AuthorizeToolResult;
use crate::plan::PlanManager;
use crate::tool::{ToolAccess, ToolInputDisplay};

mod policies;
mod rule_match;

pub use policies::OUTSIDE_SANDBOX_DENIAL_CONTRACT;
pub use rule_match::{ParsedPattern, match_permission_rule, parse_pattern};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Permission mode — controls how the policy chain treats undecided calls.
///
/// Serialized as lowercase strings (`"manual"`, `"yolo"`, `"auto"`) so agent
/// profiles and TOML config files can reference modes by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PermissionMode {
    /// Ask the user for anything not explicitly allowed or denied.
    #[default]
    Manual,
    /// Approve everything unless explicitly denied.
    Yolo,
    /// Approve everything except `AskUserQuestion` (which is denied).
    Auto,
}

/// Rule decision — what a configured rule says about a tool call.
///
/// Serialized as lowercase strings (`"allow"`, `"deny"`, `"ask"`) so TOML
/// `[[permission.rules]]` tables can reference decisions by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    Allow,
    Deny,
    Ask,
}

/// Rule scope — provenance of a rule, for display and override semantics.
///
/// Serialized as kebab-case strings (`"turn-override"`, `"session-runtime"`,
/// `"project"`, `"user"`) matching the TypeScript schema, so TOML
/// `[[permission.rules]]` tables (and any JSON config) can reference scopes by
/// name. Defaults to [`RuleScope::User`] when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuleScope {
    /// Set for the current turn only (highest user-level priority).
    TurnOverride,
    /// Recorded at runtime via "approve for session".
    SessionRuntime,
    /// From project-level configuration.
    Project,
    /// From user-level configuration.
    #[default]
    User,
}

/// A permission rule.
///
/// Deserializable from TOML `[[permission.rules]]` tables in the user's
/// `config.toml` (see [`crate::config::PermissionConfig`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PermissionRule {
    pub decision: RuleDecision,
    /// Defaults to [`RuleScope::User`] when omitted, matching TypeScript
    /// behavior where a rule without an explicit scope is user-level.
    #[serde(default)]
    pub scope: RuleScope,
    pub pattern: String,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared state consulted by policies
// ---------------------------------------------------------------------------

/// Session-scoped collaboration-mode flag.
///
/// Collaboration mode is entered/exited by the host (`collaboration.enter`
/// / `collaboration.exit` wire ops). While active, the 7 `Collaboration*`
/// builtin tools are exposed to the LLM (the host-side handlers toggle
/// them through [`crate::tool_registry::ToolManager::set_builtin_tool_enabled`]);
/// this flag mirrors the state for the `session.status` read face. Like
/// the swarm-mode state ([`crate::swarm::SwarmModeService`]) it is
/// session-scoped and not persisted — unlike
/// swarm mode, no permission policy consults it.
///
/// The flag is an `AtomicBool` so it can be shared across agents and async
/// tasks without a lock.
#[derive(Default)]
pub struct CollaborationModeState {
    active: AtomicBool,
}

impl CollaborationModeState {
    /// Create a state with collaboration mode inactive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter collaboration mode.
    pub fn enter(&self) {
        self.active.store(true, Ordering::SeqCst);
        tracing::debug!("collaboration mode entered");
    }

    /// Exit collaboration mode.
    pub fn exit(&self) {
        self.active.store(false, Ordering::SeqCst);
        tracing::debug!("collaboration mode exited");
    }

    /// Whether collaboration mode is currently active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for CollaborationModeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollaborationModeState")
            .field("active", &self.is_active())
            .finish()
    }
}

/// Session-scoped, cross-agent shared approval set ("approve for session"
/// memory, SDR-003 / D-07). The patterns live behind a
/// `Mutex<HashSet<String>>` and the store is shared as an `Arc` so every
/// agent of one session can hold the same instance: the session creates the
/// store and hands it to each agent's [`PermissionManager`].
#[derive(Debug, Default)]
pub struct SessionApprovals {
    patterns: Mutex<HashSet<String>>,
}

impl SessionApprovals {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Memorize one approved pattern.
    pub fn record(&self, pattern: String) {
        self.patterns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pattern);
    }

    /// The memorized patterns, in unspecified order.
    pub fn patterns(&self) -> Vec<String> {
        self.patterns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Services that policies depending on engine state consult.
///
/// Each field is optional: when a service is absent the depending policy
/// passes (no opinion). [`PermissionManager::new`] and
/// [`PermissionManager::with_parent`] build the default policy chain with no
/// services (usable for tests); [`PermissionManager::with_services`] wires
/// the live session services (see `AgentScope`).
#[derive(Debug, Clone, Default)]
pub struct PermissionServices {
    /// External-command hook engine that feeds the observation-only
    /// `PermissionResult` fire from
    /// [`before_tool_call`](PermissionManager::before_tool_call).
    pub hook_engine: Option<Arc<HookEngine>>,
    /// Plan-mode state consulted by `PlanModeGuardDenyPolicy`.
    pub plan: Option<Arc<PlanManager>>,
    /// Swarm-mode state consulted by the two swarm policies
    /// ([`crate::swarm::SwarmModeService`] behind a session-shared mutex).
    pub swarm_mode: Option<Arc<Mutex<crate::swarm::SwarmModeService>>>,
    /// The session's workspace sandbox (root + additional dirs), consulted by
    /// `WorkspaceBoundaryPolicy` to classify tool paths. The session scope
    /// injects the same `Arc` the file tools hold (plan
    /// 2026-10-10-sandbox-path-permission, D2); `None` disables boundary
    /// evaluation (policies pass, legacy behavior preserved).
    pub workspace_paths: Option<Arc<crate::tools::WorkspacePaths>>,
}

// ---------------------------------------------------------------------------
// Policy trait and context
// ---------------------------------------------------------------------------

/// The permission policy trait.
///
/// Each policy examines the context and returns [`PermissionPolicyResult`].
/// [`PermissionPolicyResult::Pass`] means "no opinion — ask the next policy".
pub trait PermissionPolicy: Send + Sync {
    fn name(&self) -> &str;
    fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult;
}

/// Context for policy evaluation.
pub struct PermissionPolicyContext<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub args: &'a serde_json::Value,
    pub approval_rule: &'a str,
    pub display: &'a ToolInputDisplay,
    pub accesses: &'a [ToolAccess],
    pub mode: PermissionMode,
    pub rules: &'a [PermissionRule],
    pub session_approvals: &'a HashSet<String>,
    pub cancel: &'a CancelToken,
}

/// Policy evaluation result.
#[derive(Debug, Clone)]
pub enum PermissionPolicyResult {
    /// Approve the tool call.
    Approve { reason: Option<String> },
    /// Deny the tool call.
    Deny {
        reason: Option<String>,
        message: Option<String>,
    },
    /// Ask the user for approval.
    ///
    /// `grant_dir` is present when the ask is about an outside-sandbox path
    /// and carries the directory a positive answer would authorize (Task 3
    /// consumes it for once/session/persist scope semantics). All other asks
    /// leave it `None`.
    Ask {
        reason: Option<String>,
        grant_dir: Option<PathBuf>,
    },
    /// No opinion — fall through to next policy.
    Pass,
}

// ---------------------------------------------------------------------------
// PermissionManager
// ---------------------------------------------------------------------------

/// The permission manager — owns the policy chain and rule store.
///
/// Session agents (main, subagents, swarm members) are built by
/// [`SessionScope::create_agent`](crate::scope::SessionScope::create_agent)
/// over the session's shared [`SessionApprovals`] store and stamped with the
/// session's user-level rules. The [`with_parent`](Self::with_parent) link is
/// the direct-construction path kept for tests: a child that inherits the
/// parent's rules and (read-side) approvals without a shared store.
///
/// The `headless` flag controls how `Ask` results from the policy chain are
/// translated by [`before_tool_call`](Self::before_tool_call):
/// - `headless == true` (no UI/user to answer approval prompts): `Ask` →
///   `Approved` (programmatic mode — the host is expected to have its own
///   approval flow or has opted into auto-approval).
/// - `headless == false` (default, safe): `Ask` → `Blocked` with an
///   explanatory reason, preventing silent auto-approval of tools that
///   require user confirmation.
pub struct PermissionManager {
    /// Current permission mode.
    ///
    /// Behind an `RwLock` so the mode can change at runtime through a shared
    /// `Arc<PermissionManager>` (e.g. the `agent.setPermission` RPC) while
    /// policy evaluation keeps reading it through `&self`. The lock is held
    /// only for the duration of a copy of the `Copy` mode value, never across
    /// an await point.
    mode: RwLock<PermissionMode>,
    /// Precomputed `effective_rules` (own + parent's), computed at
    /// construction and swapped wholesale by [`set_rules`](Self::set_rules).
    /// Behind an `RwLock<Arc<…>>` so a runtime rule push (user-config
    /// hot-reload / RPC CRUD) can replace the set through `&self` while
    /// policy evaluation keeps cloning the `Arc` — the write lock is held
    /// only for the pointer swap, never across an await.
    effective_rules_cache: RwLock<Arc<[PermissionRule]>>,
    policies: Vec<Box<dyn PermissionPolicy>>,
    /// Session approvals ("approve for session" decisions), shared across
    /// every agent of the session (SDR-003). Managers built by
    /// [`with_shared_approvals`](Self::with_shared_approvals) receive the
    /// session's store; the default constructors self-own one so standalone
    /// managers keep working. A [`with_parent`](Self::with_parent)-built
    /// child (the direct-construction test path) additionally aggregates its
    /// parent's approvals on read.
    session_approvals: Arc<SessionApprovals>,
    parent: Option<Arc<PermissionManager>>,
    headless: bool,
    /// External-command hook engine used to fire post-decision
    /// `PermissionResult` hooks from [`before_tool_call`](Self::before_tool_call).
    /// Wired from [`PermissionServices::hook_engine`]; `None` disables the
    /// fire (e.g. standalone managers built without services).
    hook_engine: Option<Arc<HookEngine>>,
}

impl PermissionManager {
    /// Create a new manager with the given mode and rules.
    ///
    /// The default policy chain is created without engine services (see
    /// [`PermissionServices`]) — session-dependent policies (plan mode,
    /// hooks, swarm mode) pass. Use [`with_services`](Self::with_services) to
    /// wire live services.
    ///
    /// Defaults to `headless: false` (the safe default — `Ask` results are
    /// blocked rather than silently approved).
    pub fn new(mode: PermissionMode, rules: Vec<PermissionRule>) -> Self {
        Self::with_services(mode, rules, PermissionServices::default())
    }

    /// Create a new manager with the given mode, rules, and engine services.
    ///
    /// The services are consulted by the session-dependent policies:
    /// [`policies::PlanModeGuardDenyPolicy`] (plan mode) and the two swarm
    /// policies (swarm mode). The hook engine feeds the observation-only
    /// `PermissionResult` fire from
    /// [`before_tool_call`](Self::before_tool_call) — it is not a policy
    /// input (v2 parity: hooks never gate permission decisions).
    ///
    /// Defaults to `headless: false` (the safe default — `Ask` results are
    /// blocked rather than silently approved).
    pub fn with_services(
        mode: PermissionMode,
        rules: Vec<PermissionRule>,
        services: PermissionServices,
    ) -> Self {
        Self::with_shared_approvals(mode, rules, services, Arc::new(SessionApprovals::new()))
    }

    /// Create a manager whose session approvals live in `store` — shared
    /// across every manager handed the same `Arc` (SDR-003 cross-agent
    /// session approvals). The session scope creates one store per session
    /// and injects it into every agent's manager, so a session-scoped
    /// approval recorded through one agent is visible to all of them.
    pub fn with_shared_approvals(
        mode: PermissionMode,
        rules: Vec<PermissionRule>,
        services: PermissionServices,
        store: Arc<SessionApprovals>,
    ) -> Self {
        let effective = rules.iter().cloned().collect::<Arc<[PermissionRule]>>();
        Self {
            mode: RwLock::new(mode),
            effective_rules_cache: RwLock::new(effective),
            policies: create_default_policies(&services),
            session_approvals: store,
            parent: None,
            headless: false,
            hook_engine: services.hook_engine.clone(),
        }
    }

    /// Create a child manager that inherits from `parent`.
    ///
    /// The child inherits the parent's `headless` setting. The child's policy
    /// chain is built without engine services.
    pub fn with_parent(
        mode: PermissionMode,
        rules: Vec<PermissionRule>,
        parent: Arc<PermissionManager>,
    ) -> Self {
        let headless = parent.headless;
        let parent_rules = parent.effective_rules();
        let mut effective: Vec<PermissionRule> = rules.to_vec();
        effective.extend(parent_rules.iter().cloned());
        Self {
            mode: RwLock::new(mode),
            effective_rules_cache: RwLock::new(effective.into()),
            policies: create_default_policies(&PermissionServices::default()),
            session_approvals: Arc::new(SessionApprovals::new()),
            parent: Some(parent),
            headless,
            hook_engine: None,
        }
    }

    /// Set the headless flag.
    ///
    /// When `true`, `Ask` results from the policy chain are translated to
    /// `Approved` by [`before_tool_call`](Self::before_tool_call) (suitable for
    /// programmatic/headless mode where no user is available to answer
    /// approval prompts). When `false` (default), `Ask` results are blocked.
    pub fn set_headless(&mut self, headless: bool) {
        self.headless = headless;
    }

    /// Whether this manager is in headless mode.
    pub fn is_headless(&self) -> bool {
        self.headless
    }

    /// Current permission mode (own, not inherited).
    pub fn mode(&self) -> PermissionMode {
        *self.mode.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Set the permission mode.
    ///
    /// Takes `&self` (not `&mut self`) so the mode can be changed at runtime
    /// through a shared `Arc<PermissionManager>` — e.g. the
    /// `agent.setPermission` RPC, or
    /// [`SessionManager::resume_session`](crate::session::SessionManager::resume_session)
    /// restoring the mode from a replayed wire log. Changing the mode at
    /// runtime is safe — it only affects subsequent policy evaluations;
    /// in-flight evaluations are not affected because the policy chain reads
    /// the mode at evaluation time.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    /// Effective rules: own rules followed by parent's effective rules.
    ///
    /// Returns a cheaply-cloned `Arc<[PermissionRule]>` — the set is swapped
    /// wholesale by [`set_rules`](Self::set_rules), so this stays O(1).
    pub fn effective_rules(&self) -> Arc<[PermissionRule]> {
        Arc::clone(
            &self
                .effective_rules_cache
                .read()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Replace the effective rule set at runtime (user-config rules
    /// hot-reload / RPC CRUD push, D-34).
    ///
    /// Takes `&self` — the cache lives behind an `RwLock` and the swap only
    /// replaces the `Arc` pointer. In-flight evaluations are unaffected:
    /// they already hold their own `Arc` clone. A manager with a parent
    /// recomputes the merge so the parent's rules stay included.
    pub fn set_rules(&self, rules: Vec<PermissionRule>) {
        let mut effective = rules;
        if let Some(parent) = &self.parent {
            let parent_rules = parent.effective_rules();
            effective.extend(parent_rules.iter().cloned());
        }
        *self
            .effective_rules_cache
            .write()
            .unwrap_or_else(|e| e.into_inner()) = effective.into();
    }

    /// Effective mode: own mode.
    pub fn effective_mode(&self) -> PermissionMode {
        self.mode()
    }

    /// All session approvals: own store's plus parent's (deduplicated).
    pub fn all_session_approvals(&self) -> Vec<String> {
        let mut all = self.session_approvals.patterns();
        if let Some(parent) = &self.parent {
            all.extend(parent.all_session_approvals());
        }
        all
    }

    /// Record a session approval ("approve for session").
    ///
    /// Takes `&self` (interior mutability) so the turn flow's shared
    /// `Arc<PermissionManager>` can record the approval when a user
    /// approves a tool for the session, and a resume can restore replayed
    /// approvals — the write lands in the session-shared
    /// [`SessionApprovals`] store, so every agent of the session sees it.
    pub fn record_approval(&self, pattern: String) {
        self.session_approvals.record(pattern);
    }

    /// Evaluate all policies in order. First non-`Pass` result wins.
    pub fn evaluate(&self, ctx: &PermissionPolicyContext) -> PermissionPolicyResult {
        for policy in &self.policies {
            let result = policy.evaluate(ctx);
            match result {
                PermissionPolicyResult::Pass => continue,
                _ => return result,
            }
        }
        // Fallback — FallbackAskPolicy is always last, so this is unreachable
        // in practice, but we return Ask for safety.
        PermissionPolicyResult::Ask {
            reason: Some("No policy decided".into()),
            grant_dir: None,
        }
    }

    /// Evaluate and translate to [`AuthorizeToolResult`] (for the
    /// [`LoopHooks`](crate::hooks::LoopHooks) interface).
    ///
    /// When the policy chain returns `Ask`:
    /// - If `headless == true`: translates to `Approved` (programmatic mode —
    ///   the host is expected to have its own approval flow or has opted into
    ///   auto-approval).
    /// - If `headless == false` (default): translates to `Blocked` with the
    ///   reason `"approval required but no approval mechanism available"`,
    ///   preventing silent auto-approval of tools that require user
    ///   confirmation.
    pub fn before_tool_call(&self, ctx: &PermissionPolicyContext) -> AuthorizeToolResult {
        let result = match self.evaluate(ctx) {
            PermissionPolicyResult::Approve { .. } => AuthorizeToolResult::Approved,
            PermissionPolicyResult::Deny { message, .. } => AuthorizeToolResult::Blocked {
                reason: message.unwrap_or_else(|| "Permission denied".to_string()),
            },
            PermissionPolicyResult::Ask { .. } => {
                if self.headless {
                    AuthorizeToolResult::Approved
                } else {
                    AuthorizeToolResult::Blocked {
                        reason: "approval required but no approval mechanism available".to_string(),
                    }
                }
            }
            PermissionPolicyResult::Pass => AuthorizeToolResult::Approved,
        };

        // Re-broadcast the decision as a `PermissionResult` external hook
        // (observation-only, fire-and-forget). The synchronous policy path has
        // no turn id, so the hook input carries `turn_id: 0`.
        self.fire_permission_result(ctx, &result);

        result
    }

    /// Fire the `PermissionResult` external hook for a decision made by
    /// [`before_tool_call`](Self::before_tool_call).
    ///
    /// `PermissionPolicy::evaluate` is synchronous but the hook engine is
    /// asynchronous; we bridge via the shared blocking runtime on a worker
    /// thread exactly like `PreToolCallHookPolicy`. The fire is fire-and-forget
    /// (an observation hook cannot block the already-made decision), so we
    /// never wait for the hook command here.
    fn fire_permission_result(&self, ctx: &PermissionPolicyContext, result: &AuthorizeToolResult) {
        let Some(engine) = &self.hook_engine else {
            return;
        };
        // Fast path: without PermissionResult hooks there is nothing to fire
        // (avoids spawning a worker thread per tool call).
        if engine
            .list_by_event(HookEventType::PermissionResult)
            .is_empty()
        {
            return;
        }
        let outcome = match result {
            AuthorizeToolResult::Approved => PermissionResultOutcome::Approved,
            AuthorizeToolResult::Blocked { reason } => PermissionResultOutcome::Rejected {
                feedback: reason.clone(),
            },
            // Synthetic results are not permission decisions.
            AuthorizeToolResult::Synthetic { .. } => return,
        };
        let input = crate::hook_engine::permission_result_input(
            0,
            ctx.tool_call_id,
            ctx.tool_name,
            &format!("execute {}", ctx.tool_name),
            &outcome,
        );
        let engine = Arc::clone(engine);
        std::thread::spawn(move || {
            let _ = crate::hook_engine::shared_blocking_runtime().block_on(engine.trigger(
                HookEventType::PermissionResult,
                input,
                &std::collections::HashMap::new(),
            ));
        });
    }
}

// ---------------------------------------------------------------------------
// Default policy chain
// ---------------------------------------------------------------------------

/// Create the default ordered policy chain (20 policies).
///
/// Order matters — the first non-`Pass` result wins. The `services` provide
/// optional engine state (plan mode, hooks, swarm mode) to the policies that
/// need it.
///
/// No hook policy lives here (v2 parity): the `PreToolUse` external hook
/// fires once at the tool-executor boundary
/// ([`TurnFlowHooks::prepare_tool_execution`](crate::turn_flow::TurnFlowHooks)),
/// and `PermissionRequest` / `PermissionResult` are observation-only events.
fn create_default_policies(services: &PermissionServices) -> Vec<Box<dyn PermissionPolicy>> {
    vec![
        Box::new(policies::AgentSwarmExclusiveDenyPolicy::new(
            services.swarm_mode.clone(),
        )),
        Box::new(policies::AutoModeAskUserQuestionDenyPolicy),
        Box::new(policies::PlanModeGuardDenyPolicy::new(
            services.plan.clone(),
        )),
        Box::new(policies::UserConfiguredDenyPolicy),
        Box::new(policies::AutoModeApprovePolicy),
        Box::new(policies::SessionApprovalHistoryPolicy),
        Box::new(policies::UserConfiguredAskPolicy),
        Box::new(policies::UserConfiguredAllowPolicy),
        Box::new(policies::ExitPlanModeReviewAskPolicy),
        Box::new(policies::GoalStartReviewAskPolicy),
        Box::new(policies::PlanModeToolApprovePolicy),
        Box::new(policies::SensitiveFileAccessAskPolicy),
        Box::new(policies::GitControlPathAccessAskPolicy),
        Box::new(policies::YoloModeApprovePolicy),
        Box::new(policies::SwarmModeAgentSwarmApprovePolicy::new(
            services.swarm_mode.clone(),
        )),
        // The boundary policy sits after the mode policies (Yolo/Auto already
        // decided) and before default-approve: an outside path must not be
        // silently approved by the Read/Grep/... default list (plan D2).
        Box::new(policies::WorkspaceBoundaryPolicy::new(
            services.workspace_paths.clone(),
        )),
        Box::new(policies::DefaultToolApprovePolicy),
        Box::new(policies::GitCwdWriteApprovePolicy),
        Box::new(policies::FallbackAskPolicy),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owned inputs a policy context borrows from (built once per test,
    /// no leaks — the context borrows only within the test's scope).
    struct CtxBundle {
        args: serde_json::Value,
        display: ToolInputDisplay,
        cancel: CancelToken,
    }

    impl CtxBundle {
        fn new() -> Self {
            Self {
                args: serde_json::Value::Null,
                display: ToolInputDisplay::Generic {
                    summary: "t".to_string(),
                },
                cancel: CancelToken::new(),
            }
        }

        /// Build a policy context over the given borrowed inputs (the two
        /// set_rules tests differ only in `rules`/`tool_call_id`).
        fn ctx<'a>(
            &'a self,
            rules: &'a [PermissionRule],
            session_approvals: &'a HashSet<String>,
            tool_call_id: &'a str,
        ) -> PermissionPolicyContext<'a> {
            PermissionPolicyContext {
                tool_name: "Bash",
                tool_call_id,
                args: &self.args,
                approval_rule: "Bash",
                display: &self.display,
                accesses: &[],
                mode: PermissionMode::Manual,
                rules,
                session_approvals,
                cancel: &self.cancel,
            }
        }
    }

    fn user_rule(decision: RuleDecision, pattern: &str) -> PermissionRule {
        PermissionRule {
            decision,
            scope: RuleScope::User,
            pattern: pattern.to_string(),
            reason: None,
        }
    }

    /// Given a manager built with a deny rule, when set_rules swaps the rule
    /// set at runtime, then the same tool call evaluates to the new decision.
    #[test]
    fn should_evaluate_new_decision_when_set_rules_swaps_rules_at_runtime() {
        let manager = PermissionManager::new(
            PermissionMode::Manual,
            vec![user_rule(RuleDecision::Deny, "Bash")],
        );
        let approvals = HashSet::new();
        let bundle = CtxBundle::new();

        let deny_rules = manager.effective_rules();
        assert!(matches!(
            manager.evaluate(&bundle.ctx(&deny_rules, &approvals, "c1")),
            PermissionPolicyResult::Deny { .. }
        ));

        manager.set_rules(vec![user_rule(RuleDecision::Allow, "Bash")]);

        let allow_rules = manager.effective_rules();
        assert!(matches!(
            manager.evaluate(&bundle.ctx(&allow_rules, &approvals, "c2")),
            PermissionPolicyResult::Approve { .. }
        ));
    }

    /// Given a manager built in Manual mode, when set_mode swaps the mode at
    /// runtime, then mode() reports the new value — the live-read contract the
    /// authorize hook relies on (each tool call reads the mode at evaluation
    /// time; no snapshot, no restart needed).
    #[test]
    fn should_return_new_mode_when_set_mode_changes_it_at_runtime() {
        let manager = PermissionManager::new(PermissionMode::Manual, vec![]);
        assert_eq!(manager.mode(), PermissionMode::Manual);

        manager.set_mode(PermissionMode::Yolo);

        assert_eq!(manager.mode(), PermissionMode::Yolo);
    }

    /// Given two managers sharing one SessionApprovals store, when one records
    /// a session approval, then the other's all_session_approvals sees it
    /// (SDR-003: session-scoped approvals are session-wide, not per-agent).
    #[test]
    fn should_share_session_approvals_when_managers_share_one_store() {
        let store = Arc::new(SessionApprovals::new());
        let a = PermissionManager::with_shared_approvals(
            PermissionMode::Manual,
            Vec::new(),
            PermissionServices::default(),
            Arc::clone(&store),
        );
        let b = PermissionManager::with_shared_approvals(
            PermissionMode::Manual,
            Vec::new(),
            PermissionServices::default(),
            Arc::clone(&store),
        );
        a.record_approval("Bash".to_string());
        assert_eq!(b.all_session_approvals(), vec!["Bash".to_string()]);
    }

    // -----------------------------------------------------------------------
    // WorkspaceBoundaryPolicy chain wiring (plan 2026-10-10-sandbox-path-permission, D2):
    // outside-sandbox paths enter the permission chain; the mode policies
    // ahead of the boundary policy keep their first-wins semantics.
    // -----------------------------------------------------------------------

    /// A manager wired with a real workspace sandbox (root + an outside file),
    /// returning the manager, the canonical outside file, and its parent dir.
    /// The root `TempDir` is returned too so the sandbox root stays on disk
    /// for the whole test (dropping it would make classify fail).
    fn boundary_fixture(
        mode: PermissionMode,
        rules: Vec<PermissionRule>,
    ) -> (
        tempfile::TempDir,
        PermissionManager,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "s3cret").unwrap();
        let workspace_paths =
            Arc::new(crate::tools::WorkspacePaths::new(root.path().to_path_buf()));
        let manager = PermissionManager::with_services(
            mode,
            rules,
            PermissionServices {
                workspace_paths: Some(workspace_paths),
                ..Default::default()
            },
        );
        (
            root,
            manager,
            outside_file.canonicalize().unwrap(),
            outside.path().canonicalize().unwrap(),
        )
    }

    fn boundary_ctx<'a>(
        bundle: &'a CtxBundle,
        args: &'a serde_json::Value,
        mode: PermissionMode,
        approvals: &'a HashSet<String>,
    ) -> PermissionPolicyContext<'a> {
        PermissionPolicyContext {
            tool_name: "Read",
            tool_call_id: "call-outside-read",
            args,
            approval_rule: "Read",
            display: &bundle.display,
            accesses: &[],
            mode,
            rules: &[],
            session_approvals: approvals,
            cancel: &bundle.cancel,
        }
    }

    /// Given a Manual-mode manager wired with the session's workspace sandbox,
    /// when the Read tool targets a clean absolute path outside the sandbox
    /// with no `path:` rule, then the chain asks the user with the outside
    /// reason and a grant_dir (the outside target's parent) — the boundary
    /// policy must win before DefaultToolApprovePolicy would approve Read.
    #[test]
    fn should_ask_with_grant_dir_when_manual_mode_reads_an_outside_path_and_no_rule_matches() {
        let (_root, manager, outside_file, outside_dir) =
            boundary_fixture(PermissionMode::Manual, vec![]);
        let args = serde_json::json!({ "path": outside_file });
        let bundle = CtxBundle::new();
        let approvals = HashSet::new();
        let ctx = boundary_ctx(&bundle, &args, PermissionMode::Manual, &approvals);

        match manager.evaluate(&ctx) {
            PermissionPolicyResult::Ask { reason, grant_dir } => {
                assert_eq!(
                    reason.as_deref(),
                    Some("the path is outside the workspace sandbox")
                );
                assert_eq!(
                    grant_dir.as_deref(),
                    Some(outside_dir.as_path()),
                    "the grant_dir must be the outside file's parent directory"
                );
            }
            other => panic!("a Manual-mode outside read must Ask, got {other:?}"),
        }
    }

    /// Given a Yolo-mode manager wired with the workspace sandbox, when the
    /// Read tool targets an outside path, then the chain approves with the
    /// yolo-mode reason — `YoloModeApprovePolicy` (ahead of the boundary
    /// policy) wins first, so the boundary policy never asks in Yolo.
    #[test]
    fn should_approve_with_yolo_reason_when_yolo_mode_reads_an_outside_path() {
        let (_root, manager, outside_file, _) = boundary_fixture(PermissionMode::Yolo, vec![]);
        let args = serde_json::json!({ "path": outside_file });
        let bundle = CtxBundle::new();
        let approvals = HashSet::new();
        let ctx = boundary_ctx(&bundle, &args, PermissionMode::Yolo, &approvals);

        match manager.evaluate(&ctx) {
            PermissionPolicyResult::Approve { reason } => {
                assert_eq!(reason.as_deref(), Some("yolo-mode"));
            }
            other => {
                panic!("a Yolo-mode outside read must approve via the yolo policy, got {other:?}")
            }
        }
    }

    /// Given an Auto-mode manager wired with the workspace sandbox, when the
    /// Read tool targets an outside path, then the chain approves with the
    /// auto-mode reason — `AutoModeApprovePolicy` (ahead of the boundary
    /// policy) wins first, so the boundary policy never asks in Auto.
    #[test]
    fn should_approve_with_auto_reason_when_auto_mode_reads_an_outside_path() {
        let (_root, manager, outside_file, _) = boundary_fixture(PermissionMode::Auto, vec![]);
        let args = serde_json::json!({ "path": outside_file });
        let bundle = CtxBundle::new();
        let approvals = HashSet::new();
        let ctx = boundary_ctx(&bundle, &args, PermissionMode::Auto, &approvals);

        match manager.evaluate(&ctx) {
            PermissionPolicyResult::Approve { reason } => {
                assert_eq!(reason.as_deref(), Some("auto-mode"));
            }
            other => {
                panic!("an Auto-mode outside read must approve via the auto policy, got {other:?}")
            }
        }
    }

    /// Given a manager in any mode (Yolo shown — the most permissive) wired
    /// with the workspace sandbox, when the Read tool targets a sensitive
    /// file that also lies outside the sandbox, then the chain asks with the
    /// sensitive-file reason and NO grant_dir — the sensitive gate (position
    /// 12) precedes both `YoloModeApprovePolicy` and the boundary policy, so
    /// an outside `.env` never enters the directory-grant machinery.
    #[test]
    fn should_ask_without_a_grant_dir_when_a_sensitive_file_lies_outside_the_sandbox_even_in_yolo_mode()
     {
        for mode in [PermissionMode::Yolo, PermissionMode::Manual] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let env_file = outside.path().join(".env");
            std::fs::write(&env_file, "SECRET=1").unwrap();
            let workspace_paths =
                Arc::new(crate::tools::WorkspacePaths::new(root.path().to_path_buf()));
            let manager = PermissionManager::with_services(
                mode,
                Vec::new(),
                PermissionServices {
                    workspace_paths: Some(workspace_paths),
                    ..Default::default()
                },
            );
            let args = serde_json::json!({ "path": env_file });
            let bundle = CtxBundle::new();
            let approvals = HashSet::new();
            let accesses = [crate::tool::ToolAccess {
                kind: crate::tool::AccessKind::File,
                path: Some(env_file.to_string_lossy().into_owned()),
                operation: crate::tool::AccessOperation::Read,
            }];
            let ctx = PermissionPolicyContext {
                tool_name: "Read",
                tool_call_id: "call-outside-env-read",
                args: &args,
                approval_rule: "Read",
                display: &bundle.display,
                accesses: &accesses,
                mode,
                rules: &[],
                session_approvals: &approvals,
                cancel: &bundle.cancel,
            };
            match manager.evaluate(&ctx) {
                PermissionPolicyResult::Ask { reason, grant_dir } => {
                    assert_eq!(
                        reason.as_deref(),
                        Some("sensitive-file"),
                        "the sensitive gate must win over the boundary policy in {mode:?}"
                    );
                    assert!(
                        grant_dir.is_none(),
                        "a sensitive ask must never carry a grant dir (mode {mode:?})"
                    );
                }
                other => panic!("a sensitive outside read must Ask in {mode:?}, got {other:?}"),
            }
        }
    }
}
