//! Session/agent domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer).

/// The resumed-session summary caps each agent's serialized history to the
/// most recent N turns. The TUI replay renderer (the only consumer of the
/// summary's `replay` view) trims the client side to its own
/// `REPLAY_TURN_LIMIT` anyway, so anything older never renders — while an
/// uncapped summary re-serializes the agent's whole transcript on every
/// resume/reload, which dominated the server's peak memory footprint.
const RESUMED_SUMMARY_TURN_LIMIT: usize = 10;

/// The index the resumed summary's history serialization starts from: the
/// position of the oldest real-user turn still inside the most recent
/// `limit` turns (0 when the history fits). Mirrors the client's
/// `limitReplayRecordsByTurn` trimming — same turn-start semantics
/// (`PromptOrigin::is_real_user_input`), so both sides agree on what a
/// "turn window" is.
fn recent_turn_window_start(
    history: &[mirri_core::context::ContextMessage],
    limit: usize,
) -> usize {
    let mut turn_starts: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.message.role == mirri_core::message::Role::User
                && message
                    .origin
                    .as_ref()
                    .is_some_and(|origin| origin.is_real_user_input())
        })
        .map(|(index, _)| index)
        .collect();
    if limit == 0 || turn_starts.len() <= limit {
        return 0;
    }
    turn_starts.truncate(turn_starts.len() - limit);
    // The last dropped turn's opener marks the window start: everything
    // from the next surviving turn's opener onward rides the summary.
    turn_starts.last().expect("non-empty after truncate") + 1
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use mirri_core::config::redact_config_wire;
use mirri_core::goal_status_wire;
use mirri_core::message::ContentPart;
use mirri_core::session_export::{ExportSessionPayload, SessionExporter};
use mirri_core::task_notification::{
    delivered_keys, due_notifications_for, is_notification_candidate, parked_keys,
};
use mirri_core::{
    AgentId, AgentLifecycle, AgentScopeConfig, AgentTask, CloseReason, ESSENTIAL_PROFILE_NAME,
    PermissionMode, PromptOrigin, ProviderType, RpcTool, ServerConfig, SessionEvent, SessionId,
    SessionMetadata, SessionScope, SwarmModeTrigger, TaskStatus, ThinkingEffort, uuid_from_id,
    validate_workspace_path,
};
use mirri_proto::{ReplayLimits, WireOp};
use mirri_transport::adapter::EngineError;

use crate::llm_factory::SubagentLlmAdapter;
use crate::providers::config_factory::resolve_provider_env_chain;

use super::additional_dirs::{
    file_dirs_of, find_project_root, paths_equal, read_workspace_local_toml,
    resolve_additional_dir, resolve_existing_additional_dirs, session_grant_sink,
    write_workspace_local_toml,
};
use super::responses::{
    ActivateSkillResult, AddAdditionalDirResult, BtwStartResult, ContextSummary, CoreInfoResult,
    CronDeleteResult, CronTasksResult, ExportResult, GetConfigResult, GetContextResult,
    GetPermissionResult, GetUsageResult, GoalResult, PlanStatusResult, RegisterToolResult,
    SESSION_SCOPE_KIND, SessionAgentResult, SessionDetailsResult, SessionHandleResult,
    SessionMetaResult, SessionSearchHitResult, SessionSearchResult, SetActiveToolsResult,
    SetModelResult, SetUserToolEnabledResult, ShellRunResult, SkillGetContentResult,
    SkillReloadResult, SkillReloadWorkspaceResult, SkillSetEnabledResult, UndoResult,
    UnregisterToolResult, ack, to_result,
};
use super::session_routing::resume_session_not_found;
use super::task_driver::{notification_recorder, task_addressee_id};
use super::{
    AgentRefParams, DEFAULT_MAIN_AGENT_ID, EmbeddedServer, ListingVisibility, SessionIdParams,
    SessionView, engine_error, goal_snapshot_json, invalid_params, normalize_params, not_found,
    not_found_domain, resolve_session_agent, search, session_error,
    subagent_model_target_rejection, token_usage_to_json, token_usage_ts_json,
    turn_flow_config_from,
};

impl EmbeddedServer {
    // --- dispatch ---------------------------------------------------------

    /// Merge the essential `agent` profile into a main-agent config — the
    /// create and resume paths share this so a restart never diverges from
    /// a fresh create.
    ///
    /// The profile's tool whitelist and role text fill what the config left
    /// unset (the same resolution a dispatched subagent gets through the
    /// profile manager). Request-level fields stay authoritative: an
    /// explicit `systemPrompt` still replaces the whole base, and the
    /// resolved model / provider / permission mode ride untouched. A
    /// missing profile catalog or an unresolvable essential profile is a
    /// no-op — the main agent then runs the unrestricted default.
    fn apply_essential_profile_to_main_agent(&self, config: &mut AgentScopeConfig) {
        let Some(profile_manager) = &self.profile_manager else {
            return;
        };
        let Ok(profile_config) =
            profile_manager.resolve(ESSENTIAL_PROFILE_NAME, &mirri_os::LocalSyncFs)
        else {
            return;
        };
        if config.enabled_tools.is_none() {
            config.enabled_tools = profile_config.enabled_tools;
        }
        if config.role_additional.is_none() {
            config.role_additional = profile_config.role_additional;
        }
    }

    /// Seed a session scope config with the enabled plugins' rendered
    /// system-prompt contributions (item J).
    ///
    /// This is the zero-IO data channel (contract 6): the engine never asks
    /// the plugin manager — the composition root assembles the values here
    /// and the engine renders them into the default-base prompt's
    /// `{{plugin_sections}}` slot at agent creation. `None` plugin manager
    /// (the server runs without plugins) leaves the config untouched.
    fn seed_plugin_system_prompts(
        &self,
        config: mirri_core::SessionScopeConfig,
    ) -> mirri_core::SessionScopeConfig {
        let Some(plugin_manager) = &self.plugin_manager else {
            return config;
        };
        let sections = plugin_manager
            .enabled_system_prompts()
            .into_iter()
            .map(
                |section| mirri_core::plugin_runtime::PluginSystemPromptSection {
                    plugin_id: section.plugin_id,
                    content: section.content,
                },
            )
            .collect();
        mirri_core::SessionScopeConfig {
            plugin_system_prompts: sections,
            ..config
        }
    }

    /// Seed a session scope config with the `<brandHome>/SYSTEM.md` text
    /// (D21) — the same zero-IO data channel as the plugin sections: the
    /// engine's default-base assembly consumes the text at agent creation,
    /// never reading the file itself. A missing, empty, or unreadable file
    /// leaves the config untouched (the builtin base; a read failure
    /// degrades with a warn instead of failing the session, mirroring the
    /// TS `loadSystemMdProfile` degradation).
    fn seed_base_prompt_override(
        &self,
        config: mirri_core::SessionScopeConfig,
    ) -> mirri_core::SessionScopeConfig {
        if let Some(text) = self.read_system_md_override() {
            mirri_core::SessionScopeConfig {
                base_prompt_override: Some(text),
                ..config
            }
        } else {
            config
        }
    }

    /// Publish a session event on the addressed session's event bus, when
    /// the session has artifacts. Fire-and-forget: event delivery never
    /// fails the RPC that produced it.
    pub(super) fn publish_session_event(&self, session_id: &str, event: SessionEvent) {
        if let Ok(artifacts) = self.resolve_session_artifacts(session_id) {
            artifacts.event_bus.publish(event);
        }
    }

    /// `session.create` — create a session, provision its default main agent
    /// (see the module docs), and start relaying its events.
    pub(super) async fn dispatch_create_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CreateSessionParams {
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            tags: Option<Vec<String>>,
            #[serde(default, alias = "modelId")]
            model_id: Option<String>,
            #[serde(default, alias = "providerType")]
            provider_type: Option<ProviderType>,
            #[serde(default, alias = "systemPrompt")]
            system_prompt: Option<String>,
            /// The workspace the session is created in — the registry id of a
            /// registered workspace. The v2 REST create face pins the
            /// snake_case spelling (`sessionCreateSchema`, POST /sessions),
            /// and the web REST client already dual-sends it with
            /// `metadata.cwd`; the camelCase alias matches this struct's
            /// convention. `None` keeps the default workspace (existing
            /// callers, zero break).
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            /// The v2 REST create body's `metadata` — only `cwd` is read
            /// here: the dual-send pattern pairs it with `workspace_id`, and
            /// a mismatch between the two is rejected exactly like the TS
            /// route does (a mismatched pair must never mis-attribute the
            /// session). Routing keys on `workspace_id` alone.
            #[serde(default)]
            metadata: Option<CreateSessionMetadataParam>,
            #[serde(default, alias = "thinkingEffort")]
            thinking_effort: Option<ThinkingEffort>,
            #[serde(default, alias = "permissionMode")]
            permission_mode: Option<PermissionMode>,
            /// Session-scoped MCP overlay servers (the ACP `mcpServers`
            /// request parameter, already converted to `{name, config}`
            /// entries by the host). `None` keeps no overlay; even an empty
            /// list installs an (empty) overlay, replacing any previous
            /// one — every request re-registers (the overlay is never
            /// persisted).
            #[serde(default, rename = "mcpServers")]
            mcp_servers: Option<Vec<Value>>,
        }
        /// The v2 REST create body's `metadata` (only the `cwd` field is
        /// meaningful for routing — see the `metadata` param doc above).
        #[derive(Debug, Deserialize)]
        struct CreateSessionMetadataParam {
            #[serde(default)]
            cwd: Option<String>,
        }
        let params: CreateSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // Validate: if a provider type is specified, a model id must also be
        // provided — otherwise the resolved default model would be sent to
        // the wrong provider, producing a confusing error at message.send time.
        if params.provider_type.is_some() && params.model_id.is_none() {
            return Err(invalid_params(
                "modelId is required when providerType is specified",
            ));
        }

        // Route the create to its workspace (W4): an explicit id must name a
        // REGISTERED workspace — an unknown id is `invalid_params`, never a
        // silent fallback to the default (the TS v2 route's
        // WORKSPACE_NOT_FOUND parity; the message mirrors its text). A
        // registered-but-unbuildable workspace fails the RPC rather than
        // mis-creating on the default. An absent id keeps the default
        // workspace's manager (existing callers, zero break).
        let manager = match params.workspace_id.as_deref() {
            Some(workspace_id) => {
                let registration = self.registry.registration(workspace_id);
                let runtime = match self.registry.ensure_built(workspace_id) {
                    Some(Ok(runtime)) => runtime,
                    Some(Err(error)) => {
                        return Err(EngineError::Internal(format!(
                            "building workspace {workspace_id} failed: {error}"
                        )));
                    }
                    None => {
                        return Err(invalid_params(format!(
                            "workspace {workspace_id} does not exist"
                        )));
                    }
                };
                // The dual-send `metadata.cwd` must agree with the routed
                // workspace's root (the TS v2 route's parity check): a
                // mismatched pair would otherwise create in
                // `workspace_id`'s workspace while the request claims
                // another cwd.
                if let Some(cwd) = params
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.cwd.as_deref())
                {
                    let root = registration
                        .map(|row| row.root.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if cwd != root {
                        return Err(invalid_params(format!(
                            "metadata.cwd ({cwd}) must equal workspace root ({root})"
                        )));
                    }
                }
                Arc::clone(&runtime.manager)
            }
            None => Arc::clone(&self.manager),
        };

        let now = Utc::now();
        let metadata = SessionMetadata {
            title: params
                .title
                .clone()
                .unwrap_or_else(|| "Untitled Session".to_string()),
            // A caller-provided title is user-set: the send path's
            // prompt-derived auto-title must not overwrite it.
            is_custom_title: params
                .title
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty()),
            created_at: now,
            updated_at: now,
            tags: params
                .tags
                .clone()
                .map(|tags| tags.into_iter().collect())
                .unwrap_or_default(),
            // A freshly created session is never archived (only
            // `session.archive` sets the flag).
            archived: false,
            // Unset by the caller: the ROUTED manager's own workspace
            // backfills this with its cwd (the session's own workspace root,
            // never the process default root — see
            // `create_session_inner`'s backfill).
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
            last_used_at: None,
        };
        // B1 T4: the session's task-lifecycle knobs (capacity, kill grace,
        // keep-alive, subagent timeout) come from the typed [task] /
        // [subagent] sections of the live server config — the same
        // derivation every later workspace's sessions get.
        let scope_config = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            mirri_core::session_scope_config_from(&snapshot)
        };
        // Item J — seed the session config with the enabled plugins'
        // rendered system-prompt contributions (zero-IO contract 6: the
        // data is assembled here, at the composition root, and rendered by
        // the engine's default-base prompt assembly at agent creation). The
        // same seed runs on the resume path, so a re-created agent renders
        // the same sections.
        let scope_config = self.seed_plugin_system_prompts(scope_config);
        // D21 — seed the `<home>/SYSTEM.md` base-prompt override (the same
        // zero-IO data channel): a fresh session's default-base agents
        // render the override as their base. Seeded per session (not at
        // boot) so a settings-page write applies to sessions created after
        // it without an engine restart.
        let scope_config = self.seed_base_prompt_override(scope_config);
        // SDR-004 b: the config default seeds the session's initial
        // posture when the create request carries no explicit mode.
        let seed_permission_mode = scope_config.permission_mode;
        // The effective create-time permission mode: an explicit request
        // wins over the config seed, which wins over the Manual baseline.
        let effective_permission_mode = params.permission_mode.unwrap_or(seed_permission_mode);
        // The model id preference order: the request's explicit `modelId`,
        // then the config snapshot's `default_model` alias (config.toml).
        // There is NO built-in fallback (spec 11 R4): when neither source
        // resolves, the create fails loudly with an actionable error
        // instead of silently binding a model the user never configured.
        // An explicit-but-empty `modelId` (`""` — the wire's "no
        // preference" spelling some clients send instead of omitting the
        // field) is filtered before resolution, symmetric with the resume
        // path's empty-record filter (R9): it falls through to the
        // configured default, never a half-empty model id.
        // The id is kept verbatim on the agent (the web round-trips its own
        // pick): a `provider/model` composite (the C6 id the web pickers
        // compose) resolves at every consumption point — the config-aware
        // LLM factory, the model catalog's context-window lookup, and the
        // klient model methods — so no silent wrong-provider default can
        // hide behind it.
        //
        // Resolved BEFORE the session exists so a rejected create leaves no
        // half-created session behind in the workspace.
        let resolved_model_alias = params
            .model_id
            .clone()
            .filter(|model_id| !model_id.trim().is_empty())
            .or_else(|| {
                self.config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .default_model
                    .clone()
            })
            .ok_or_else(|| {
                engine_error(mirri_core::EngineError::RequestInvalid(
                    "no model configured: set default_model in config.toml or pass modelId \
                     when creating the session"
                        .to_string(),
                ))
            })?;
        let session = manager
            .create_session_lazy(metadata, scope_config)
            .await
            .map_err(session_error)?;

        // The routing seam's first level: the created session is active in
        // its workspace — a routed create notes against the ROUTED
        // workspace, so every later seam resolution (message.send, relays,
        // the index write-through) lands on it; an absent id notes the
        // default.
        if let Some(workspace_id) = params.workspace_id.as_deref() {
            self.note_active_session_in(workspace_id, session.id().as_str());
        } else {
            self.note_active_session(session.id().as_str());
        }

        // The D8 touch debounce (W6, design §2.3): creating a session opens
        // its workspace, so `last_opened_at` advances — IN MEMORY only, with
        // a debounced registry flush. A session creation never writes
        // workspaces.json directly (the write-amplification red line).
        let touched_workspace = params
            .workspace_id
            .clone()
            .or_else(|| self.registry.default_workspace_id());
        if let Some(workspace_id) = touched_workspace {
            self.touch_workspace_last_opened(&workspace_id);
        }

        // The workspace-local additional dirs are effective from the
        // moment the session exists — the TS `createSession` reads
        // `readWorkspaceAdditionalDirs` before building the session. The
        // cwd is the ROUTED workspace's root (W4), not the default's.
        seed_workspace_additional_dirs(&session, manager.workspace().cwd());

        // Provision the default main agent: embedded consumers (ACP, NAPI)
        // have no agent-creation RPC, so the session must be promptable the
        // moment it exists.
        //
        // Seed the engine-level reverse-RPC callback (if registered) into
        // the SESSION's slot: `create_agent` stamps that slot onto the main
        // agent's `AskUserQuestion` tool, which resolves the CURRENT value
        // per call — so both this engine-level callback and a later serve
        // transport subscribe (which overwrites the same slot) reach the
        // tool without recreating it.
        if let Some(engine_callback) = self
            .reverse_rpc_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            *session
                .reverse_rpc_slot()
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(engine_callback);
        }

        // Wire the interaction kernel (B4-a) over the session's HITL gates so
        // the ask flows register their pending requests with the gate — the
        // `approval.decide` / `question.answer` / `interaction.*` RPCs of this
        // embedded surface can then see and resolve them. The session was just
        // created, so its artifacts (and their gates) already exist.
        let interaction_kernel = self
            .resolve_session_artifacts(session.id().as_str())
            .ok()
            .map(|artifacts| {
                Arc::new(
                    mirri_core::InteractionKernel::new(
                        Arc::clone(&artifacts.approval_gate),
                        Arc::clone(&artifacts.question_gate),
                    )
                    // Plan D3: the session's directory-grant sink rides the
                    // kernel so the grant surfaces (decide/respond/reply)
                    // apply session/persist grants into the shared sandbox.
                    .with_grant_sink(session_grant_sink(&session)),
                )
            });

        // Build the main agent config. `resolved_model_alias` was resolved
        // before the session was created (explicit `modelId`, else the
        // config snapshot's `default_model` alias — else the create was
        // rejected; see above).
        let mut agent_config = AgentScopeConfig {
            interaction_kernel,
            model_id: resolved_model_alias.clone(),
            provider_type: params.provider_type.unwrap_or_default(),
            system_prompt: params.system_prompt,
            thinking_effort: params.thinking_effort,
            permission_mode: effective_permission_mode,
            ..AgentScopeConfig::default()
        };
        // The essential `agent` profile rides the main agent (the same
        // resolution a dispatched subagent gets): its tool whitelist and
        // role text fill what the request left unset.
        self.apply_essential_profile_to_main_agent(&mut agent_config);

        // The session's event bus rides the agent config so the goal/skill
        // tools announce their model-initiated mutations on the wire
        // (`event.goal.updated` / `event.skill.activated`). The same
        // artifacts carry the session's turn-id allocator for the subagent
        // host below (child flows share the session's numeric turn
        // namespace).
        let session_artifacts = self.resolve_session_artifacts(session.id().as_str()).ok();
        let event_bus = session_artifacts
            .as_ref()
            .map(|artifacts| Arc::clone(&artifacts.event_bus));
        agent_config.event_bus = event_bus.clone();

        // Wire the subagent host when both an LLM factory and profile
        // manager are available — this registers the `Agent` and
        // `AgentSwarm` tools for the main agent.
        let mut created_host: Option<Arc<mirri_core::SessionSubagentHost>> = None;
        if let (Some(llm_factory), Some(profile_manager)) =
            (&self.llm_factory, &self.profile_manager)
        {
            // The session's event bus rides the host so the subagent
            // lifecycle (`subagent.spawned`/`started`/`completed`/`failed`)
            // reaches the session's subscribers.
            let host = Arc::new(
                mirri_core::SessionSubagentHost::new(
                    Arc::downgrade(&session),
                    Arc::clone(profile_manager),
                    AgentId::new(DEFAULT_MAIN_AGENT_ID),
                    agent_config.clone(),
                    Arc::new(SubagentLlmAdapter(Arc::clone(llm_factory))),
                    self.turn_flow_config_for(&resolved_model_alias),
                    Some(Arc::new(crate::model_catalog::ConfigModelCatalog::new(
                        Arc::clone(&self.config),
                    ))),
                    event_bus,
                )
                .with_nesting_limits(1, session.config().max_nesting_depth)
                .with_turn_id_source(session_artifacts.as_ref().map(|a| a.turn_id_counter()))
                .with_tool_result_home(self.home_dir().map(ToOwned::to_owned))
                .with_flag_registry(Arc::clone(self.flag_registry())),
            );
            // Kept as a concrete handle so the live config can be bound
            // after `create_agent` (below) — the config carries the trait
            // object, which has no binding method.
            created_host = Some(Arc::clone(&host));
            agent_config.subagent_host = Some(host as Arc<dyn mirri_core::SubagentHost>);
        }

        // Wire web search and fetch URL providers into the agent config.
        // These register the `WebSearch` and `FetchURL` builtin tools.
        if let Some(ref provider) = self.web_search_provider {
            agent_config.web_search_provider = Some(Arc::clone(provider));
        }
        if let Some(ref provider) = self.fetch_url_provider {
            agent_config.fetch_url_provider = Some(Arc::clone(provider));
        }

        session
            .create_agent(AgentId::new(DEFAULT_MAIN_AGENT_ID), agent_config)
            .await
            .map_err(|e| {
                // Rollback: remove the half-created session so it doesn't
                // linger in the workspace with no agent. The close runs on
                // the ROUTED workspace's manager (W4) — the default's
                // manager cannot see the session.
                let manager = Arc::clone(&manager);
                let session_id = session.id().clone();
                tokio::spawn(async move {
                    let _ = manager
                        .close_session(
                            &session_id,
                            CloseReason::Error(
                                "agent creation failed during session.create".to_string(),
                            ),
                        )
                        .await;
                });
                engine_error(e)
            })?;

        // spec 11 R5/A3 (defect B): bind the host to the main agent's LIVE
        // config handle now that the agent exists — children spawned after
        // a later `setModel` inherit the NEW model; the create-time
        // snapshot alone would go stale.
        if let Some(host) = created_host
            && let Some(main_agent) = session
                .get_agent(&AgentId::new(DEFAULT_MAIN_AGENT_ID))
                .await
        {
            host.set_parent_config_shared(main_agent.config());
        }

        // Persist the effective model alias so it survives a resume: the
        // live agent's config carries it in memory alone, but the resume
        // rebuilds the agent from the replayed wire log — recording the
        // `config.update` op lets the reducer fold the model back into the
        // agent state.
        session
            .record_op(WireOp::ConfigUpdate {
                model_alias: Some(resolved_model_alias.clone()),
                profile_name: None,
                thinking_effort: None,
                system_prompt: None,
                disallowed_tools: None,
            })
            .await;

        // Persist a create-time permission mode so it survives a resume:
        // the live agent's PermissionManager is built from the in-memory
        // config alone, but `session.resume` rebuilds the agent from the
        // replayed wire log — recording the same `permission.set_mode` op
        // `agent.setPermission` writes makes the effective mode round-trip.
        // Only a non-default mode needs the record (the replayed state's
        // baseline is already "manual") — this covers both an explicit
        // request and a non-manual config seed.
        if effective_permission_mode != PermissionMode::Manual {
            // `PermissionMode` is a unit-variant enum: `to_value` cannot fail
            // and always yields the lowercase wire string the reducer parses
            // back ("manual" | "auto" | "yolo") — the branch already
            // guarantees the mode is non-default, so no fallback is needed
            // (final-review finding Minor-5).
            let mode_str = serde_json::to_value(effective_permission_mode)
                .expect("PermissionMode unit enum serialization cannot fail")
                .as_str()
                .expect("PermissionMode always serializes to its wire string")
                .to_string();
            session
                .record_op(WireOp::PermissionSetMode { mode: mode_str })
                .await;
        }

        // Install the session's MCP overlay *before* the tool registration
        // so the overlay's server names shadow same-name workspace/user
        // entries in that registration (overlay > workspace > user). The
        // overlay's connects are spawned in the background — the response
        // never waits for them.
        if let Some(overlay_servers) = params.mcp_servers {
            self.prepare_session_overlay(session.id().as_str(), overlay_servers);
        }

        // Register MCP tools from connected servers into the agent's tool
        // manager so the LLM can call them like builtin tools.
        if let Some(agent) = resolve_session_agent(&session, Some(DEFAULT_MAIN_AGENT_ID)).await {
            self.register_mcp_tools(&agent, session.id().as_str()).await;
        }

        self.spawn_session_relay(session.id().as_str());
        // The session's cron driver ticks its due tasks (event + prompt
        // injection) for as long as the session lives.
        self.spawn_cron_driver(session.id().as_str());
        // The session's task sweeper terminates expired background tasks
        // (real cancellation) for as long as the session lives (B1).
        self.spawn_task_driver(session.id().as_str());
        // The created session's consistency follow driver — the create
        // path's twin of the resume path's spawn (Task 7): a session this
        // engine CREATED must also follow foreign journal appends, or the
        // dual-instance posture is asymmetric (the creator would never
        // see the other instance's writes until a close→resume cycle).
        // Same flag gating as every other spawn site.
        self.spawn_follow_driver(session.id().as_str());

        // SDR-004 lazy physical creation: the created session writes NO
        // index row and NO session directory — it exists in memory only
        // until the first user message materializes it (the send path's
        // `materialize_session` + `last_prompt` write-through). The
        // in-memory listing still sees it (the live union over the built
        // runtimes), so the create is immediately usable.

        serde_json::to_value(self.session_view(&session).await)
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.list` — list sessions (live + persisted) across **every
    /// workspace in scope** (W5), with the v2 filter face pinned to the TS
    /// kap-server list route (`packages/kap-server/src/routes/sessions.ts`).
    ///
    /// Params (snake_case wire, camelCase aliases accepted — the REST
    /// client's exact spellings): `workspace_id` (alias-expanded to its D3
    /// alias set; unknown → `invalid params`, the TS route's
    /// `WORKSPACE_NOT_FOUND` message), `include_archive`, `archived_only`,
    /// `busy`, `exclude_empty` (SDR-004 — a session that never received a
    /// user input is hidden), and the keyset cursors `before_id` /
    /// `after_id` / `page_size`. Response: the page envelope
    /// `{items, has_more}` over the global `(updated_at DESC, id DESC)`
    /// order — an unknown cursor answers the empty terminal page.
    ///
    /// `busy` filters **post-page** for ordinary lists (the TS parity: a
    /// page may be under-filled) and **pre-page** for `archived_only` (the
    /// archived view drains to a full page). No page size = every row,
    /// `has_more: false` (the TS `pageSize ?? visible.length`).
    ///
    /// Rows report their TRUE work directory — never backfilled to the
    /// bound root (W5): a live session's view computes its cwd from live
    /// metadata (W4), a persisted row echoes its recorded `state.json`
    /// workDir.
    ///
    /// The no-index degrade (no injected home) keeps the pre-W5 plain-scan
    /// shape: the live union plus the default bucket's disk scan —
    /// `exclude_empty` is not applicable there (its data source IS the
    /// index).
    pub(super) async fn dispatch_list_sessions(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ListSessionsParams {
            #[serde(default, alias = "includeArchive")]
            include_archive: Option<bool>,
            #[serde(default, alias = "archivedOnly")]
            archived_only: Option<bool>,
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            #[serde(default, alias = "excludeEmpty")]
            exclude_empty: Option<bool>,
            #[serde(default, alias = "beforeId")]
            before_id: Option<String>,
            #[serde(default, alias = "afterId")]
            after_id: Option<String>,
            #[serde(default, alias = "pageSize")]
            page_size: Option<u64>,
            #[serde(default)]
            busy: Option<bool>,
        }
        let params: ListSessionsParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let archived_only = params.archived_only == Some(true);
        let include_archive = archived_only || params.include_archive == Some(true);
        let exclude_empty = params.exclude_empty == Some(true);
        let has_index = self.session_index().is_some();

        // The workspace scope: an explicit id must name a REGISTERED
        // workspace — unknown is `invalid params` (the TS route's
        // WORKSPACE_NOT_FOUND parity; the W4 create spelling), never a
        // silent fallback — and expands to its D3 alias set. The embedded
        // host pin (§2.2) restricts the DEFAULT scope to the pinned root;
        // serve lists every registered workspace.
        let workspace_ids: Option<Vec<String>> = match params.workspace_id.as_deref() {
            Some(workspace_id) => {
                if self.registry.registration(workspace_id).is_none() {
                    return Err(invalid_params(format!(
                        "workspace {workspace_id} does not exist"
                    )));
                }
                Some(self.registry.alias_group_of(workspace_id))
            }
            None => match self.listing_visibility {
                ListingVisibility::PinnedToDefault => self
                    .registry
                    .default_workspace_id()
                    .map(|default| vec![default]),
                ListingVisibility::AllRegistered => None,
            },
        };

        // The merged source: index rows (multi-workspace) ∪ live union, in
        // the global cursor order.
        let mut entries = self
            .merged_session_entries(workspace_ids.as_deref(), include_archive)
            .await;

        // The no-index degrade: the default bucket's plain scan joins the
        // live union (the pre-W5 fallback shape).
        if !has_index {
            let live_ids: std::collections::HashSet<String> =
                entries.iter().map(|entry| entry.info.id.clone()).collect();
            for info in self.manager.list_all_sessions(include_archive).await {
                if live_ids.contains(&info.id) {
                    continue;
                }
                entries.push(super::session_index::MergedSessionEntry {
                    info,
                    cwd: None,
                    last_prompt: None,
                });
            }
            entries.sort_by(|a, b| {
                b.info
                    .updated_at
                    .cmp(&a.info.updated_at)
                    .then_with(|| b.info.id.cmp(&a.info.id))
            });
        }

        // The narrow archived filter: `archived_only` keeps only archived
        // rows (the fetch above already included them).
        if archived_only {
            entries.retain(|entry| entry.info.archived);
        }
        // SDR-004: a never-conversed session is hidden under
        // `exclude_empty` — `last_prompt` NULL or empty (the TS judgment).
        // Not applicable on the no-index degrade (no data source).
        if exclude_empty && has_index {
            entries.retain(|entry| {
                entry
                    .last_prompt
                    .as_deref()
                    .is_some_and(|prompt| !prompt.is_empty())
            });
        }

        // The keyset cursors, applied over the merged global order:
        // `before_id` = strictly older (forward/default paging),
        // `after_id` = strictly newer. An unknown cursor answers the empty
        // terminal page (a client cannot spin on a cursor the server
        // cannot advance — the TS route's exact behaviour).
        if let Some(cursor) = params.before_id.as_deref().or(params.after_id.as_deref()) {
            match entries.iter().position(|entry| entry.info.id == cursor) {
                None => entries.clear(),
                Some(index) => {
                    if params.before_id.is_some() {
                        // Drop the cursor row and everything newer than it.
                        entries.drain(..=index);
                    } else {
                        // Keep only the rows strictly newer than the cursor.
                        entries.truncate(index);
                    }
                }
            }
        }

        // The busy filter: `archived_only` applies it pre-page (the archived
        // view drains to a full page), ordinary lists post-page (TS parity).
        let busy_filter = |entry: &super::session_index::MergedSessionEntry| {
            let busy = self.session_is_busy(&entry.info.id);
            params.busy.is_none_or(|wanted| busy == wanted)
        };
        if archived_only && params.busy.is_some() {
            entries.retain(busy_filter);
        }

        // The page: no size = every row, has_more false (the TS
        // `pageSize ?? visible.length`).
        let limit = params.page_size.map_or(usize::MAX, |size| {
            usize::try_from(size).unwrap_or(usize::MAX)
        });
        let has_more = entries.len() > limit;
        entries.truncate(limit);

        if !archived_only && params.busy.is_some() {
            entries.retain(busy_filter);
        }

        // The wire views: live sessions report the runtime fields from
        // their scope; persisted-only rows the static summary plus the
        // row's own recorded cwd (never a bound-root backfill). Both carry
        // the merged `last_prompt`.
        let mut views: Vec<SessionView> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let mut view = match self.resolve_session(&entry.info.id).await {
                Ok(session) => self.session_view(&session).await,
                Err(_) => {
                    let mut view = SessionView::from(&entry.info);
                    view.cwd = entry.cwd.clone();
                    view
                }
            };
            view.last_prompt = entry.last_prompt.clone();
            // The interrupted-task recovery badge override (spec §2.1
            // interception point ① / §2.2's OR-merge): an unsettled,
            // unexpired entry in the in-process recovery list rewrites
            // the row's outcome as `interrupted` BEFORE any existing
            // string folding could misfile it. The persisted-fact arm
            // (the backfilled closure in the wire log) reaches the same
            // row through the normal projection once the session is
            // resident — this override only covers the in-process arm's
            // race window. F-06: before consulting the list, a resident
            // session that drove a NEWER turn settles its entry (the
            // manual-continuation badge leak).
            self.settle_superseded_interruptions().await;
            if let Some(reason) = self.recovery.last_turn_reason_override(&entry.info.id) {
                view.last_turn_reason = Some(reason);
            }
            views.push(view);
        }
        serde_json::to_value(json!({ "items": views, "has_more": has_more }))
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.resume` — resume a closed session from its persisted wire
    /// log and start relaying its events again.
    /// Build the v1 `ResumedSessionSummary`-compatible wire body for one
    /// session — the `session.resume` (and `session.reload`) return.
    ///
    /// The shape is the TS SDK's resume contract (`ResumeSessionResult`):
    /// the `SessionSummary` scalars plus `sessionMetadata` and the
    /// per-agent `ResumedAgentState` snapshots the TUI's replay renderer
    /// consumes (`agents['main'].replay / background`, the todo panel's
    /// `toolStore`). The engine's honest gaps stay absent rather than
    /// fabricated: `provider` (no resolved `ProviderConfig` read),
    /// `warning` (no migration channel), `lastPrompt`, and the replay
    /// records' `time` (context memory carries no per-message timestamps,
    /// so the records stamp `0`).
    async fn resumed_session_summary(&self, session: &SessionScope) -> Result<Value, EngineError> {
        let session_id = session.id();
        let metadata = session.metadata().await;
        // The owning workspace's root / session dir (the routing seam, W3) —
        // a resumed non-default session reports its own workspace's paths.
        let (owner_manager, _owning_workspace) = self
            .resolve_session_owner(session_id.as_str())
            .map_err(|_| not_found(format!("session not found: {session_id}")))?;
        let work_dir = owner_manager
            .workspace()
            .cwd()
            .to_string_lossy()
            .into_owned();
        let session_dir = owner_manager
            .persistence()
            .session_dir(session_id)
            .to_string_lossy()
            .into_owned();
        let title = metadata.title.clone();
        let created_ms = metadata.created_at.timestamp_millis();
        let updated_ms = metadata.updated_at.timestamp_millis();

        // The session-level background tasks ride every agent's snapshot
        // (v1 scopes them per agent; the engine tracks them per session —
        // with the single main agent the two views coincide).
        let background: Vec<Value> = session
            .tasks()
            .list()
            .iter()
            .map(|task| serde_json::to_value(task.background_task_info()))
            .collect::<Result<_, _>>()
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        let swarm_mode = session.swarm_mode().is_active();
        let collaboration_mode = session.collaboration_mode().is_active();
        let plan_manager = session.plan();
        let plan_json = match plan_manager.get_plan() {
            Some(content) => json!({
                // The engine's plan manager has no plan id of its own — a
                // session-scoped id is the stable stand-in (nothing on the
                // v1 consume side keys off it; the TUI branches on null).
                "id": format!("plan-{}", session_id.as_str()),
                "content": content,
                "path": plan_manager
                    .get_plan_file_path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }),
            None => Value::Null,
        };

        let mut agents = serde_json::Map::new();
        for agent_id in session.list_agents().await {
            let Some(agent) = session.get_agent(&agent_id).await else {
                continue;
            };
            let agent_key = agent_id.as_str().to_string();
            let agent_type = if agent_key == DEFAULT_MAIN_AGENT_ID {
                "main"
            } else {
                "sub"
            };

            let config_arc = agent.config();
            let config = config_arc.read().unwrap_or_else(|e| e.into_inner());
            // The v1 `AgentConfigData.thinkingEffort` is a required string —
            // an unset engine effort maps onto the "none" spelling.
            let thinking_effort = match config.thinking_effort.clone() {
                Some(effort) => serde_json::to_value(effort).unwrap_or_else(|_| json!("none")),
                None => json!("none"),
            };

            // One history read serves both the context snapshot and the
            // replay fold (the serialized `ContextMessage`s are exactly the
            // `message` records the v1 replay vocabulary names). The two
            // snake_case provenance keys are renamed onto the v1 camelCase
            // spellings — the engine's internal serialization keeps its
            // own, the TS replay contract expects `toolCalls`/`toolCallId`.
            //
            // The history is capped to the most recent
            // [`RESUMED_SUMMARY_TURN_LIMIT`] turns BEFORE serialization:
            // the resume response is a summary, not a transcript — the TUI
            // replay renderer only renders the recent turns (the client's
            // `REPLAY_TURN_LIMIT`), and re-serializing every agent's full
            // history on every resume piled gigabytes of short-lived JSON
            // into the server's peak footprint (P2 of the 26G peak
            // investigation). A turn starts at a real user prompt —
            // `is_real_user_input`, the same standalone-user semantics the
            // client's turn-limit trims by.
            let memory = agent.context_memory();
            let memory_guard = memory.lock().unwrap_or_else(|e| e.into_inner());
            let token_count = memory_guard.token_count();
            let window_start =
                recent_turn_window_start(memory_guard.history(), RESUMED_SUMMARY_TURN_LIMIT);
            let history: Vec<Value> = memory_guard.history()[window_start..]
                .iter()
                .map(|message| {
                    serde_json::to_value(message)
                        .map(context_message_to_wire)
                        .map_err(|e| EngineError::Internal(e.to_string()))
                })
                .collect::<Result<_, _>>()?;
            drop(memory_guard);
            let replay: Vec<Value> = history
                .iter()
                .map(|message| {
                    json!({
                        "type": "message",
                        "time": 0,
                        "message": message,
                    })
                })
                .collect();

            let usage = agent
                .usage_service()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status();
            let by_model: Value = usage
                .by_model
                .iter()
                .map(|(model, usage)| (model.clone(), token_usage_to_json(usage)))
                .collect();

            let tools: Vec<Value> = agent
                .tool_manager()
                .read()
                .loop_tools()
                .into_iter()
                .map(|definition| {
                    json!({
                        "name": definition.name,
                        "description": definition.description,
                        "active": true,
                        "source": "builtin",
                    })
                })
                .collect();

            // The todo panel's hydration source (the TUI replay renderer
            // reads `toolStore['todo']`): the live memory list that restore
            // just synced from the folded wire state's `"todo"` entry. Only
            // the main agent's entry carries the key — the rust todo list is
            // session-scoped (one shared `Arc<TodoList>` for every agent), so
            // the store is emitted once on the main agent's entry; a
            // deliberate deviation from TS, where each agent had its own
            // store.
            let mut entry = json!({
                "type": agent_type,
                "config": {
                    "cwd": work_dir,
                    "modelAlias": config.model_id,
                    "modelCapabilities": {
                        // The unknown-model defaults (kosong's
                        // UNKNOWN_CAPABILITY) for everything except the
                        // context window: the server's config model
                        // catalog resolves the agent's model to its
                        // configured window (0 = unknown).
                        "imageIn": false,
                        "videoIn": false,
                        "audioIn": false,
                        "thinking": !matches!(
                            config.thinking_effort,
                            Some(mirri_core::ThinkingEffort::None) | None
                        ),
                        "toolUse": true,
                        "maxContextTokens": self.max_context_window(&config.model_id),
                    },
                    "thinkingEffort": thinking_effort,
                    "systemPrompt": config.system_prompt,
                },
                "context": {
                    "history": history,
                    "tokenCount": token_count,
                },
                "replay": replay,
                "permission": {
                    "mode": serde_json::to_value(agent.permission_manager().mode())
                        .unwrap_or_else(|_| Value::String("manual".to_string())),
                    // D-34 snapshot honesty: the agent's REAL effective
                    // rules (user-config rules wired since B9 —
                    // previously hardcoded empty). The empty → non-empty
                    // widening is backward compatible.
                    "rules": agent
                        .permission_manager()
                        .effective_rules()
                        .iter()
                        .map(mirri_core::config::permission_rule_to_wire)
                        .collect::<Vec<_>>(),
                },
                "plan": plan_json,
                "swarmMode": swarm_mode,
                "collaborationMode": collaboration_mode,
                "usage": {
                    "total": token_usage_to_json(&usage.total),
                    "currentTurn": token_usage_to_json(&usage.current_turn),
                    "byModel": by_model,
                },
                "tools": tools,
                "background": background,
            });
            if agent_type == "main" {
                entry["toolStore"] = json!({
                    // Same item shape the wire `tools.update_store` (todo)
                    // op records — the client deserializes both with one
                    // parser.
                    "todo": agent.todo_list().get().iter()
                        .map(|t| json!({ "title": t.title, "status": t.status.as_str() }))
                        .collect::<Vec<_>>(),
                });
            }
            agents.insert(agent_key, entry);
        }

        Ok(json!({
            "id": session_id.as_str(),
            "title": title,
            "workDir": work_dir,
            "sessionDir": session_dir,
            "createdAt": created_ms,
            "updatedAt": updated_ms,
            "archived": metadata.archived,
            "sessionMetadata": {
                "createdAt": metadata.created_at.to_rfc3339(),
                "updatedAt": metadata.updated_at.to_rfc3339(),
                "title": title,
                "isCustomTitle": false,
                "agents": {
                    "main": {
                        "homedir": work_dir,
                        "type": "main",
                        "parentAgentId": Value::Null,
                    }
                },
                "custom": {},
            },
            "agents": Value::Object(agents),
        }))
    }

    pub(super) async fn dispatch_resume_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ResumeSessionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            /// Session-scoped MCP overlay servers (the ACP `session/load`
            /// `mcpServers` parameter — the client re-sends the list on
            /// every load; the overlay is never persisted). `None` keeps
            /// whatever overlay the session already has in this process.
            #[serde(default, rename = "mcpServers")]
            mcp_servers: Option<Vec<Value>>,
        }
        let params: ResumeSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // The resumed agent is rebuilt on a base config carrying the host's
        // reverse-RPC callback (when one is registered): replayed user-tool
        // registrations and approval asks need it to reach the client —
        // without it the resume would silently downgrade to a bare agent.
        // The callback seeds the SESSION slot once the resume lands its
        // session (create_agent stamps the slot onto the rebuilt agent's
        // tools); the config itself no longer carries a snapshot. The
        // config snapshot's `default_model` rides the same base config
        // as the no-record resume fallback (spec 11 R9): a session whose
        // wire log carries no model record resumes on the configured
        // default, exactly like a fresh create would.
        let engine_reverse_rpc_callback = self
            .reverse_rpc_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut base_config = AgentScopeConfig {
            model_id: self
                .config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .default_model
                .clone()
                .unwrap_or_default(),
            ..AgentScopeConfig::default()
        };
        // The essential `agent` profile rides the resumed main agent too —
        // the same resolution the create path applies, so a restart does
        // not diverge from a fresh create.
        self.apply_essential_profile_to_main_agent(&mut base_config);
        // Resume-time capacity limits for the fold simulation: the same
        // strategy caps the session's live turns ran with (`TurnFlow`
        // applies them via `set_limits` every turn). Built from the same
        // `[loop_control]`-derived turn-flow config the live turn flow
        // uses — the message-count caps do not depend on the model
        // window, and the resumed agent's model id is only known after
        // the replay the limits parameterize.
        let replay_limits = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            let flow_config = turn_flow_config_from(&snapshot, 0);
            Some(ReplayLimits {
                max_history_messages: flow_config.compaction_strategy.max_history_messages,
                max_deferred_messages: flow_config.compaction_strategy.max_deferred_messages,
            })
        };
        // Route first (W3): the owning workspace's manager replays the
        // session — a persisted session of a non-default workspace must
        // resume on its own runtime (its bucket lives there), never on the
        // default's. An id no workspace owns keeps the `session.init_failed`
        // domain code with an honest routing message (see
        // [`resume_session_not_found`]).
        let (owner_manager, owning_workspace) = self
            .resolve_session_owner(&params.session_id)
            .map_err(|_| resume_session_not_found(&params.session_id))?;
        // B1 T4: the resumed session runs with the same task-lifecycle
        // knobs a fresh create derives from the live server config.
        let scope_config = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            mirri_core::session_scope_config_from(&snapshot)
        };
        // Item J — seed the plugin system-prompt contributions like the
        // create path (zero-IO data channel; the resumed main agent's
        // default-base prompt renders them at re-creation).
        let scope_config = self.seed_plugin_system_prompts(scope_config);
        // D21 — seed the SYSTEM.md base-prompt override like the create
        // path, so a resumed session renders the same base a fresh one
        // would (a settings write between the create and the resume
        // applies to the resumed agents).
        let scope_config = self.seed_base_prompt_override(scope_config);
        let session = owner_manager
            .resume_session_with_scope(
                &SessionId::new(&params.session_id),
                base_config,
                replay_limits,
                scope_config,
            )
            .await
            .map_err(session_error)?;

        // Seed the engine-level reverse-RPC callback (when one is
        // registered) into the RESUMED session's slot — the same seeding
        // the create path applies. `install_fresh_artifacts` already
        // aliased the session's slot into the artifacts, and the rebuilt
        // main agent's `AskUserQuestion` tool holds the same slot, so a
        // serve transport subscribe later overwrites this engine-level
        // fallback exactly like on a fresh create.
        if let Some(engine_callback) = engine_reverse_rpc_callback {
            *session
                .reverse_rpc_slot()
                .write()
                .unwrap_or_else(|e| e.into_inner()) = Some(engine_callback);
        }

        // The routing seam's first level (W3): a resumed
        // session is active again in its owning workspace (a
        // persisted-only session may not have held an entry).
        self.note_active_session_in(&owning_workspace, session.id().as_str());

        // The resumed session's sandbox is re-seeded from the workspace-local
        // config — the TS resume `mergeAdditionalDirs` effect: the persisted
        // additional dirs are immediately effective for the resumed agents.
        seed_workspace_additional_dirs(&session, owner_manager.workspace().cwd());

        // Restore the durable steer buffer (T3, F-P7-11/12): steers parked
        // via `message.steer` that never reached context (`turn.steer`
        // records not yet consumed by a `turn.steer_flushed` marker) are
        // re-parked into the resumed main agent's steer buffer, so the
        // next turn's first step flushes them — a steer the user sent
        // before the restart is not silently swallowed. Clearing first
        // makes the restore idempotent: a same-process close→resume may
        // still hold the pre-close buffer with its own copies.
        let pending_steers = owner_manager
            .replayed_pending_steers(&SessionId::new(&params.session_id))
            .await;
        // The resumed main agent's scope config feeds the steer-buffer
        // restore's turn-flow creation (the agent's model id drives the
        // loop-control limits and the compaction window); the same handle
        // is reused below for the MCP tool registration.
        let main_agent = resolve_session_agent(&session, Some(DEFAULT_MAIN_AGENT_ID)).await;
        let main_model_id = main_agent
            .as_ref()
            .map(|agent| {
                agent
                    .config()
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .model_id
                    .clone()
            })
            .unwrap_or_default();
        // Migration guard (the task-8 resume restore): a background-task
        // steer whose owning task's addressee is a live SUBAGENT is a
        // pre-fix record — before the notification routing change, subagent
        // notification steers rode the MAIN journal. Re-parking it into
        // main's buffer would wake a wrong-addressee main notification
        // turn; the addressee is instead reconciled by
        // [`restore_task_notifications`] / the parked-steer restore of the
        // child journals, so the stale record is filtered out here (the
        // awaits live BEFORE the buffer lock is taken). Note: a pre-fix
        // delivered-to-main notification for a subagent task is re-appended
        // once into the subagent's context on this first post-fix resume —
        // an intentional reconciliation of the routing change, not a replay.
        let pending_steers: Vec<_> = {
            let mut kept = Vec::with_capacity(pending_steers.len());
            for (content, origin) in pending_steers {
                if let PromptOrigin::BackgroundTask { task_id, .. } = &origin
                    && let Some(task) = session.tasks().get(task_id)
                    && task_addressee_id(&session, &task).await != DEFAULT_MAIN_AGENT_ID
                {
                    tracing::debug!(
                        session_id = %params.session_id,
                        task_id,
                        "skipping a stale main-journal subagent notification steer on resume"
                    );
                    continue;
                }
                kept.push((content, origin));
            }
            kept
        };
        {
            let steer_buffer =
                self.steer_buffer_for(&params.session_id, DEFAULT_MAIN_AGENT_ID, &main_model_id);
            let mut buffer = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
            buffer.clear();
            for (content, origin) in pending_steers {
                buffer.push(content, origin);
            }
        }

        // Restore the durable steer buffers of the resumed subagent
        // addressees (the task-8 resume restore): pending steers recorded
        // on a CHILD journal — the busy-path notification park routes its
        // `turn.steer` records to the addressee's own `agents/<id>/wire.jsonl`
        // — are re-parked into THAT child's steer buffer. The main-journal
        // replay above can never see them (it reads the session journal
        // only), so without this pass a restart would strand the parked
        // content: park no longer marks (FIX 1), so the restore
        // reconciliation would silently re-append the still-pending
        // notification instead of delivering it as the parked turn it was
        // scheduled as. Clearing first mirrors the main pass (idempotent
        // same-process close→resume). The addressees are the ROSTER —
        // `list_subagent_registrations` — because the restored children are
        // DORMANT after a lazy resume (ADR-006/ADR-017: their `AgentScope`s
        // are reconstructed on first use, so `list_agents` no longer holds
        // them at this point). The model id of each restored agent drives its
        // turn-flow creation, exactly as the main pass reads the resumed
        // main agent's. A child is hydrated ONLY when it actually has pending
        // steers to park — hydrating all of them to check their buffers
        // would defeat the lazy restore's purpose.
        for registration in session.list_subagent_registrations().await {
            let agent_id = registration.agent_id;
            if agent_id == DEFAULT_MAIN_AGENT_ID {
                continue;
            }
            let pending = owner_manager
                .replayed_pending_agent_steers(&SessionId::new(&params.session_id), &agent_id)
                .await;
            if pending.is_empty() {
                continue;
            }
            // A pending-steer-carrying child is woken: hydrate it (a no-op if
            // it is already live) so its steer buffer restore hits a real
            // agent. If hydration fails the child stays dormant and the
            // parked steers are left in the journal for the next hydrate —
            // the same best-effort posture as the eager restore.
            session.hydrate_subagent(&agent_id).await;
            let Some(agent) = session.get_agent(&AgentId::new(&agent_id)).await else {
                continue;
            };
            let agent_model_id = {
                let config = agent.config();
                let guard = config.read().unwrap_or_else(|e| e.into_inner());
                guard.model_id.clone()
            };
            let steer_buffer =
                self.steer_buffer_for(&params.session_id, &agent_id, &agent_model_id);
            let mut buffer = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
            buffer.clear();
            for (content, origin) in pending {
                buffer.push(content, origin);
            }
        }

        // Turn-id allocation needs NO local re-seed here: the resume's replay
        // seeded the session's shared turn-id allocator (see
        // `SessionManager`'s `land_resume_tail`), and every flow this server
        // creates for the session — including the steer-buffer flow created
        // above — draws from it, so the next turn continues the wire log's
        // numbering no matter which path mints it first.

        // Restore-time notification reconciliation (TS
        // `restoreAgentTaskNotifications`, the silent variant): terminal
        // tasks whose notification never delivered (the context ledger
        // misses the key) get their notification message appended directly
        // into the main agent's context — no turn runs for it; the model
        // reads it on the next user message. Runs AFTER the steer-buffer
        // backfill (a parked notification is not undelivered) and BEFORE
        // the task driver's spawn (the first tick must not re-deliver what
        // this reconciliation just appended).
        self.restore_task_notifications(&session, &params.session_id)
            .await;

        // Re-register the session's MCP overlay before the tool
        // registration, exactly like `session.create`: the re-sent servers
        // replace any previous overlay (last request wins), and the
        // registration sees the overlay's names shadowing both pools.
        if let Some(overlay_servers) = params.mcp_servers {
            self.prepare_session_overlay(session.id().as_str(), overlay_servers);
        }

        // Register MCP tools into the resumed session's agent(s).
        if let Some(agent) = main_agent.as_ref() {
            self.register_mcp_tools(agent, session.id().as_str()).await;
        }

        // Rebuild the subagent host for the resumed session's main agent
        // (F-P7-3): the resume path recreates the agent from the replayed
        // wire log, which carries no host — without this, the `Agent` /
        // `AgentSwarm` tools never come back and the model can no longer
        // spawn subagents after a resume. The host needs a `Weak` to the
        // session, so it can only be built here, after the resume has
        // created the session (the create path builds it before agent
        // creation; here the agent already exists and the tools are
        // registered onto its live tool manager).
        if let (Some(llm_factory), Some(profile_manager)) =
            (&self.llm_factory, &self.profile_manager)
            && let Some(agent) = resolve_session_agent(&session, Some(DEFAULT_MAIN_AGENT_ID)).await
        {
            let parent_config = {
                let config_arc = agent.config();
                let guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                (*guard).clone()
            };
            let mut parent_config = parent_config;
            parent_config.subagent_host = None;
            let event_bus = self
                .resolve_session_artifacts(&params.session_id)
                .ok()
                .map(|artifacts| Arc::clone(&artifacts.event_bus));
            // The session's turn-id allocator — the resumed host's child flows
            // continue the replayed log's numbering (the same counter the
            // resume's replay seeded).
            let turn_id_source = self
                .resolve_session_artifacts(&params.session_id)
                .ok()
                .map(|artifacts| artifacts.turn_id_counter());
            parent_config.event_bus = event_bus.clone();
            let flow_config = self.turn_flow_config_for(&parent_config.model_id);
            let host = Arc::new(
                mirri_core::SessionSubagentHost::new(
                    Arc::downgrade(&session),
                    Arc::clone(profile_manager),
                    AgentId::new(DEFAULT_MAIN_AGENT_ID),
                    parent_config,
                    Arc::new(SubagentLlmAdapter(Arc::clone(llm_factory))),
                    flow_config,
                    Some(Arc::new(crate::model_catalog::ConfigModelCatalog::new(
                        Arc::clone(&self.config),
                    ))),
                    event_bus,
                )
                .with_nesting_limits(1, session.config().max_nesting_depth)
                .with_turn_id_source(turn_id_source)
                .with_tool_result_home(self.home_dir().map(ToOwned::to_owned))
                .with_flag_registry(Arc::clone(self.flag_registry())),
            );
            // spec 11 R5/A3 (defect B): bind the host to the resumed main
            // agent's LIVE config handle — children spawned after a later
            // `setModel` inherit the NEW model, matching the fresh-create
            // path.
            host.set_parent_config_shared(agent.config());
            // B4 T8 (D-55): adopt every subagent the restart restore
            // re-created from its journal, so `Agent(resume=...)` keeps the
            // same identity across the restart.
            host.adopt_session_agents().await;
            let host = host as Arc<dyn mirri_core::SubagentHost>;
            // Register the tools onto the existing agent's tool
            // manager (the create path registers them at agent
            // construction; the resumed agent was built without a
            // host) and record the host on the agent's config so later
            // reads see it.
            {
                let tool_manager = agent.tool_manager();
                // Same background gate the agent-scope assembly builds
                // (B1 D-50): a Weak handle onto the live tool manager
                // the resumed tools register into.
                let weak = std::sync::Arc::downgrade(&tool_manager);
                let background_gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                    weak.upgrade()
                        .map(|tools| {
                            ["TaskList", "TaskOutput", "TaskStop"]
                                .iter()
                                .all(|name| tools.read().is_tool_active(name))
                        })
                        .unwrap_or(false)
                });
                let subagent_timeout = session.config().task_lifecycle.subagent_timeout;
                let mut tools = tool_manager.write();
                tools.register_builtin(Arc::new(mirri_core::tools::AgentTool::new(
                    Arc::clone(&host),
                    agent.tasks(),
                    params.session_id.clone(),
                    subagent_timeout,
                    background_gate,
                )));
                tools.register_builtin(Arc::new(mirri_core::tools::AgentSwarmTool::new(
                    Arc::clone(&host),
                    subagent_timeout,
                    session.config().task_lifecycle.max_swarm_items,
                )));
            }
            agent
                .config()
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .subagent_host = Some(host);
        }

        // Draining the previous relay, if any: its exit path must finish
        // (and stamp its final durable seq into the watermark) before the
        // new relay seeds from that watermark, otherwise both could stamp
        // the same seq. Awaiting the JoinHandle also lets the old relay's
        // own map cleanup run against the entry we just removed, so it
        // cannot delete the *new* relay's entry. (The removal runs in its
        // own statement so the mutex guard is dropped before the await —
        // holding it across the await would make this future non-Send.)
        let prev = self
            .relays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&params.session_id);
        if let Some(prev) = prev {
            let _ = prev.await;
        }

        self.spawn_session_relay(session.id().as_str());
        // The resumed session's cron driver picks up its restored tasks.
        self.spawn_cron_driver(session.id().as_str());
        // The resumed session's task sweeper drives its rehydrated tasks
        // (a hung task left by the previous process gets terminated, B1).
        self.spawn_task_driver(session.id().as_str());
        // The resumed session's consistency follow driver absorbs foreign
        // journal appends (batch 1, behind the session-consistency-follow
        // experimental flag — a disabled flag spawns nothing).
        self.spawn_follow_driver(session.id().as_str());

        // Announce the resume on the session bus *after* the relay is
        // installed, so `event.session.updated {change: 'resumed'}`
        // actually reaches subscribers. Publishing inside `resume_session`
        // fires on the fresh bus before any relay subscribes — the event
        // could never be observed there.
        if let Ok(artifacts) = self.resolve_session_artifacts(&params.session_id) {
            artifacts.event_bus.publish(SessionEvent::SessionResumed {
                session_id: uuid_from_id(&params.session_id),
            });
        }

        // The v1 resume contract: the full `ResumedSessionSummary` body
        // (summary scalars + per-agent replay snapshots), not a bare
        // session view — the TUI's replay renderer reads
        // `agents['main'].replay/background` off this result.
        self.resumed_session_summary(&session).await
    }

    /// Restore-time notification reconciliation (TS
    /// `restoreAgentTaskNotifications`, the silent variant), per addressee:
    /// every terminal detached task whose notification key the addressee's
    /// replayed context ledger misses (never delivered before the restart)
    /// gets its notification message appended directly into THAT agent's
    /// (main or subagent) context — recorded as a durable
    /// `context.append_message` with the notification origin on the
    /// addressee's own journal (session journal for main, child journal for
    /// subagents — [`notification_recorder`]'s routing contract) and
    /// appended into memory, with NO turn launched for it. An addressee
    /// that cannot be resolved is never silently re-delivered to: its
    /// pending tasks are marked durably delivered instead.
    async fn restore_task_notifications(&self, session: &Arc<SessionScope>, session_id: &str) {
        // Group every still-eligible task by its notification addressee —
        // the same attribution the driver tick uses (`task_addressee_id`
        // already folds a dispatcher that is no longer live back to main).
        // Grouping first keeps one addressee's resume from re-delivering a
        // task another addressee's pass already reconciled.
        let mut by_addressee: std::collections::BTreeMap<String, Vec<AgentTask>> =
            std::collections::BTreeMap::new();
        for task in session.tasks().list() {
            if !is_notification_candidate(&task) || task.notification_delivered_at.is_some() {
                continue;
            }
            let addressee_id = task_addressee_id(session, &task).await;
            by_addressee.entry(addressee_id).or_default().push(task);
        }
        if by_addressee.is_empty() {
            return;
        }
        let mut reappended = 0usize;
        for (addressee_id, tasks) in by_addressee {
            // The restored children are DORMANT after a lazy resume
            // (ADR-006/ADR-017) — a subagent addressee's rebuilt context must
            // be hydrated before the re-append lands in it (the append rides
            // the child's own journal, so the next hydration reproduces it).
            session.hydrate_subagent(&addressee_id).await;
            let Some(agent) = session.get_agent(&AgentId::new(&addressee_id)).await else {
                // Defensive branch: `task_addressee_id` folds a dispatcher
                // that is no longer live back to main, so an unresolvable
                // addressee should not fire under the current attribution.
                // Never re-deliver to an unrestorable addressee: its rebuilt
                // context cannot carry the re-append, so a driver tick would
                // fire a full notification turn for it. Mark the debt
                // durably delivered instead (same effect as a dedup marker).
                for task in &tasks {
                    session.tasks().mark_notification_delivered(&task.id);
                }
                tracing::warn!(
                    session_id,
                    addressee_id,
                    count = tasks.len(),
                    "restore: addressee not resolvable — marking its undelivered \
                     notifications delivered without re-append"
                );
                continue;
            };
            let memory = agent.context_memory();
            // The delivered/parked ledgers are scans of THIS addressee's
            // rebuilt context and steer buffer (the parked lookup mirrors
            // the main-only form: the buffer key is `<session>:<agent>`).
            let delivered = {
                let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
                delivered_keys(&guard)
            };
            let parked = {
                let buffers = self.steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
                buffers
                    .get(&format!("{session_id}:{addressee_id}"))
                    .map(|buffer| parked_keys(&buffer.lock().unwrap_or_else(|e| e.into_inner())))
                    .unwrap_or_default()
            };
            let task_ids: std::collections::HashSet<String> =
                tasks.iter().map(|task| task.id.clone()).collect();
            let due = due_notifications_for(&session.tasks(), &task_ids, &delivered, &parked);
            if due.is_empty() {
                continue;
            }
            // The routed recorder: a subagent addressee's durable append
            // rides its OWN journal (the record the next restart's
            // rehydrated child context is rebuilt from) — main's rides the
            // session journal exactly as before.
            let recorder = notification_recorder(session, &addressee_id).await;
            for notification in due {
                let content = vec![ContentPart::Text {
                    text: notification.xml.clone(),
                }];
                let content_wire =
                    serde_json::to_value(&content).unwrap_or_else(|_| serde_json::json!([]));
                // Write-before-mutate: the durable append lands before the
                // in-memory message — a crash between the two replays the
                // message from the record. No memory guard is held across
                // the awaits.
                recorder
                    .record_op(WireOp::ContextAppendMessage {
                        message: serde_json::json!({
                            "role": "user",
                            "content": content_wire,
                            "origin": notification.key.wire(),
                        }),
                    })
                    .await;
                memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append_user_message(content, notification.key.origin());
                // The durable half of the ledger: the task record carries
                // the delivery so the driver's next tick (and a later
                // restart) never re-arms the debt for this addressee.
                session
                    .tasks()
                    .mark_notification_delivered(&notification.task.id);
                reappended += 1;
            }
        }
        if reappended > 0 {
            tracing::info!(
                session_id,
                count = reappended,
                "silently re-appending undelivered background task notifications"
            );
        }
    }

    // --- session domain (Phase 4.4) --------------------------------------

    /// `session.reload` — a live session needs no reload in the embedded
    /// engine (there is no external state to re-read), so a live session
    /// answers with its current view and a closed one is resumed.
    pub(super) async fn dispatch_reload_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if let Ok(session) = self.resolve_session(&params.session_id).await {
            // The v1 reload contract: the same `ResumedSessionSummary` body
            // `session.resume` serves — the SDK's `reloadSession` feeds the
            // result straight back into the session's resume state.
            return self.resumed_session_summary(&session).await;
        }
        self.dispatch_resume_session(json!({ "session_id": params.session_id }))
            .await
    }

    /// `session.close` — close a session (user-initiated), removing it from
    /// the workspace. Persisted files are kept for a future resume.
    pub(super) async fn dispatch_close_session(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session_id = SessionId::new(&params.session_id);
        // Phase 3: the final row is upserted from the live metadata — the
        // close itself does not rewrite `state.json`, so the index keeps the
        // freshest view of the now persisted-only session (captured before
        // the close drops the live scope).
        // Route first (W3): the close runs on the owning workspace's
        // manager; an id no workspace owns errors exactly like the old
        // manager-level SessionError::NotFound did.
        let (owner_manager, _owning_workspace) = self.resolve_session_owner(&params.session_id)?;
        let closing_metadata = match owner_manager.get_session(&session_id).await {
            Some(session) => Some(session.metadata().await),
            None => None,
        };
        owner_manager
            .close_session(&session_id, CloseReason::UserInitiated)
            .await
            .map_err(session_error)?;
        // The routing seam's first level (W3 forward-wiring): the closed
        // session leaves the active table.
        self.forget_active_session(&params.session_id);
        if let Some(metadata) = closing_metadata {
            self.index_upsert_live(&session_id, &metadata);
        }
        // The closed session's activity views are disposed (their agents are
        // gone; a later resume folds into fresh views).
        self.dispose_activity_trackers(&params.session_id);
        // The closed session's turn flows and steer buffers die with it — a
        // surviving flow would hold the closed session's dead event bus and
        // a same-process resume would reuse it (F-P7-3).
        self.dispose_turn_flows(&params.session_id);
        // The closed session's print-steer budget dies with it too — a
        // same-id resume must start a fresh budget, and a long-lived server
        // must not accumulate entries for dead sessions.
        self.forget_print_state(&params.session_id);
        // The session's MCP overlay dies with it — its connections are
        // torn down and the child processes go with them.
        self.clear_session_overlay(&params.session_id).await;
        ack()
    }

    /// `session.archive` — close the session with `UserClosed` reason so it
    /// is removed from the active workspace but its wire log and state
    /// snapshot persist on disk for a later resume, **and** set the
    /// persisted `archived` flag (TS parity: the TS engine stores the flag in
    /// `state.json` and filters archived sessions out of default listings —
    /// `session.restore` clears it again).
    pub(super) async fn dispatch_archive_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session_id = SessionId::new(&params.session_id);
        // Route first (W3): the archive's close + flag write run on the
        // owning workspace's manager/persistence (the archived flag lives
        // in that bucket's state.json).
        let (owner_manager, _owning_workspace) = self.resolve_session_owner(&params.session_id)?;
        match owner_manager
            .close_session(&session_id, CloseReason::UserClosed)
            .await
        {
            Ok(()) => {
                // Live close: the archived flag IS the product contract (it
                // is what keeps the session out of default listings), so a
                // failed flag write propagates instead of degrading to an
                // un-archived closed session.
                owner_manager
                    .persistence()
                    .set_archived(&session_id, true)
                    .await
                    .map_err(session_error)?;
            }
            Err(mirri_core::SessionError::NotFound(_)) => {
                // TS parity: a session that exists only on disk
                // (persisted-but-not-live) can still be archived — the TS
                // engine flips the `archived` flag in `state.json` without
                // touching a live session. `close_session` fails with
                // `NotFound` because the session is not in the workspace;
                // the flag write is the whole archive operation. If the
                // session has no state.json either, `set_archived` errors —
                // an unknown session id is correctly a failure.
                owner_manager
                    .persistence()
                    .set_archived(&session_id, true)
                    .await
                    .map_err(session_error)?;
            }
            Err(error) => return Err(session_error(error)),
        }
        // The routing seam's first level (W3 forward-wiring): the archived
        // session leaves the active table like a closed one.
        self.forget_active_session(&params.session_id);
        // Phase 3: the archived flag is part of what a listing filters on,
        // so the row is refreshed from the just-written `state.json`
        // (best-effort; a failure never fails the RPC).
        self.index_refresh_from_disk(&session_id).await;
        // Archived sessions leave the workspace like closed ones — their
        // activity views are disposed (a restore folds into fresh views).
        // These are idempotent no-ops for the persisted-only branch above.
        self.dispose_activity_trackers(&params.session_id);
        // Their turn flows and steer buffers die with them too (the same
        // dead-bus reasoning as `session.close`, F-P7-3).
        self.dispose_turn_flows(&params.session_id);
        // Their print-steer budget dies with them too — same reasoning as
        // `session.close`.
        self.forget_print_state(&params.session_id);
        // The archived session's MCP overlay dies with it, exactly like a
        // closed session's.
        self.clear_session_overlay(&params.session_id).await;
        // The klient contract models the method as `noResult` (void | null —
        // JSON has no `undefined`, so the wire spelling is JSON `null`), not
        // the `{ok: true}` ack the engine previously sent.
        Ok(Value::Null)
    }

    /// `session.restore` — the B1 EMB backfill of the klient
    /// `sessionLifecycle.restore` reference. The engine models archive as
    /// `close(UserClosed)` plus the persisted `archived` flag (see
    /// [`Self::dispatch_archive_session`]), so restore is the resume path
    /// **plus clearing that flag** (TS parity: the TS `restore` = resume +
    /// `setArchived(false)`): the session is replayed from its persisted
    /// files into a live one and re-enters default listings. Serves the same
    /// wire shape mirri-server's transport-owned handler does (the bare
    /// `session_view`).
    pub(super) async fn dispatch_restore_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        // The session id is needed for the flag clear below; the full resume
        // assembly (MCP tool registration + event relay) applies to a
        // restored session identically.
        let parsed: SessionIdParams = serde_json::from_value(normalize_params(params.clone()))
            .map_err(|e| invalid_params(e.to_string()))?;
        // Retention restore-decompression (§6, Task 5/9): a compressed
        // archive must return to the plain shape BEFORE the resume reads
        // the wire log and before ANY marker clears (the crash mid-restore
        // leaves the still-compressed archive state — idempotently
        // re-restorable). The action holds the session directory's
        // advisory lock and fails loudly when another action
        // (compression / delete) holds it (§5): the caller surfaces a
        // retryable error, never a half state.
        {
            let (owner_manager, _owning_workspace) = self
                .resolve_session_owner(&parsed.session_id)
                .map_err(|_| resume_session_not_found(&parsed.session_id))?;
            let session_id = SessionId::new(&parsed.session_id);
            mirri_core::session::retention::restore_decompress(
                owner_manager.persistence(),
                &session_id,
                &owner_manager.persistence().session_dir(&session_id),
            )
            .await
            .map_err(session_error)?;
        }
        let result = self.dispatch_resume_session(params).await?;

        // Clear the persisted archived flag so the session re-enters default
        // listings (only `session.restore` clears it — a plain
        // `session.resume`/`session.reload` keeps the flag, matching the TS
        // split). The session is live again; the `state.json` write is what
        // matters — through the owning workspace's persistence (W3).
        let session_id = SessionId::new(&parsed.session_id);
        let (owner_manager, _owning_workspace) = self.resolve_session_owner(&parsed.session_id)?;
        owner_manager
            .persistence()
            .set_archived(&session_id, false)
            .await
            .map_err(session_error)?;
        // The live session's in-memory metadata was rebuilt from the
        // (archived) state.json by the resume, so clear it there too — the
        // live view and any later `save_state` (e.g. a rename) stay honest.
        if let Some(session) = owner_manager.get_session(&session_id).await {
            session
                .update_metadata(|metadata| metadata.archived = false)
                .await;
            // Phase 3: the restored (un-archived) row re-enters default
            // listings — the live metadata is the freshest view.
            self.index_upsert_live(&session_id, &session.metadata().await);
        } else {
            // A persisted-only restore still needs its row refreshed from
            // the just-cleared flag on disk.
            self.index_refresh_from_disk(&session_id).await;
        }
        Ok(result)
    }

    /// `session.fork` — fork a session's history into a new session (F2
    /// pipeline). The fork rewinds the source's main wire log to a turn
    /// anchor: an explicit `turnId` (a completed turn's id) or, by default,
    /// the most recent completed turn — an active turn's content never
    /// carries.
    ///
    /// Anchor guard (ADR-F4 rev2): the guard is ANCHOR-shaped, not
    /// session-shaped — there is NO session_is_busy gate. A source with a
    /// turn in flight still forks at any COMPLETED anchor (the active
    /// bracket is simply cut). The `turn.ended` records are the only
    /// addressable anchor namespace: an in-progress turn has no
    /// `turn.ended`, hence no id a caller can name, so "turnId points at
    /// the active turn" is not expressible — a turnId that matches no
    /// completed turn surfaces as ForkTargetNotFound (invalid_params)
    /// whose message names the completed range and, when the log ends with
    /// an open bracket, states that a further turn is in progress.
    ///
    /// Landing is atomic (F4, design §3.5): a pipeline failure after the
    /// target was minted rolls the target back to zero residue (workspace
    /// map, artifacts, directory).
    ///
    /// `agentId` is deprecated and ignored (ADR-F5: the fork addresses
    /// the session, not one agent); `metadata` is validated as a JSON
    /// object and accepted on the contract — its storage lands with
    /// F3 (state.json's custom dict), so today it is logged and dropped.
    pub(super) async fn dispatch_fork_session(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ForkSessionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(default)]
            title: Option<String>,
            /// The completed turn to fork at (`turn.ended`'s turn id).
            /// Absent = the most recent completed turn.
            #[serde(default, alias = "turnId")]
            turn_id: Option<u64>,
            /// Custom fork metadata (v2 `ForkSessionPayload.metadata`):
            /// must be a JSON object. Validated here and stored into the
            /// fork's `state.json` custom dict (merged over the source's
            /// custom with the v2 forkCustomMetadata semantics, F3).
            #[serde(default)]
            metadata: Option<serde_json::Value>,
        }
        let params: ForkSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        if let Some(agent_id) = &params.agent_id {
            // ADR-F5: the fork target is the SESSION, not one agent — the
            // parameter is accepted (never an error) and deliberately
            // ignored; the forked history is the session's main log.
            tracing::debug!(
                agent_id = %agent_id,
                "session.fork ignores the deprecated agentId parameter (ADR-F5)"
            );
        }
        if let Some(metadata) = &params.metadata
            && !metadata.is_object()
        {
            return Err(invalid_params("metadata must be a JSON object"));
        }

        let source_session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent_id = source_session
            .list_agents()
            .await
            .into_iter()
            .next()
            .ok_or_else(|| {
                not_found(format!(
                    "session '{}' has no agents to fork",
                    params.session_id
                ))
            })?;

        // TS parity: the TS fork copies the source session's `state.json`
        // wholesale, so its `lastPrompt` carries over — the fork's metadata
        // inherits the source's last prompt (the conversation preview must
        // not reset on a fork). The source session is live here
        // (`resolve_session` above), so its scope metadata is authoritative.
        let source_last_prompt = source_session.metadata().await.last_prompt;
        let now = Utc::now();
        let is_custom_title = params
            .title
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        // SDR-012: the default title names the SOURCE TITLE, not the id
        // (the same fetch createChild uses; empty falls back to the id).
        let source_title = source_session.metadata().await.title.trim().to_string();
        let source_title = if source_title.is_empty() {
            params.session_id.clone()
        } else {
            source_title
        };
        let metadata = SessionMetadata {
            title: params
                .title
                .unwrap_or_else(|| format!("Fork of {source_title}")),
            is_custom_title,
            created_at: now,
            updated_at: now,
            tags: Default::default(),
            archived: false,
            work_dir: None,
            last_prompt: source_last_prompt,
            forked_from: Some(params.session_id.clone()),
            // The fork's cache-key lineage root: the manager's pipeline
            // derives it from the source's persisted state (chain root or
            // the source itself) unless the caller already derived it.
            cache_root_id: None,
            // The fork metadata (F3): the manager's pipeline merges it over
            // the source's persisted custom dict with the v2
            // forkCustomMetadata semantics (goal dropped).
            custom: params
                .metadata
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .cloned(),
            last_used_at: None,
        };
        // The fork lands in the SOURCE session's workspace (W3): the new
        // session's persistence bucket and sandbox root follow the source.
        // The turn anchor (F2) rides the fork options. The anchor guard is
        // ADR-F4 rev2: NO session_is_busy gate — a completed anchor passes
        // even while the source runs another turn; a turnId matching no
        // completed turn surfaces as ForkTargetNotFound, mapped to
        // invalid_params by `session_error`'s dedicated arm (its message
        // distinguishes the completed range from an in-progress tail).
        // A pipeline failure past this point rolls the target back with
        // zero residue (F4 atomic landing, design §3.5).
        let (owner_manager, owning_workspace) = self.resolve_session_owner(&params.session_id)?;
        let new_session = owner_manager
            .fork_session_with_options(
                source_session.id(),
                mirri_core::ForkOptions {
                    turn_id: params.turn_id,
                },
                &agent_id,
                metadata,
            )
            .await
            .map_err(session_error)?;

        // The routing seam's first level (W3): the forked session is active
        // in its owning workspace from the moment it exists.
        self.note_active_session_in(&owning_workspace, new_session.id().as_str());

        // Register MCP tools into the fork's rebuilt main agent, exactly
        // like `session.create`/`session.resume`: the fork pipeline mints
        // a fresh tool manager, so without this the forked session would
        // silently lose every workspace/user-pool MCP tool until some
        // later reconnect event happened to re-register them.
        if let Some(agent) = resolve_session_agent(&new_session, Some(DEFAULT_MAIN_AGENT_ID)).await
        {
            self.register_mcp_tools(&agent, new_session.id().as_str())
                .await;
        }

        // Rebuild the subagent host for the forked session's main agent
        // (the fork-session sibling of resume's F-P7-3 fix — the same
        // rebuild as `dispatch_create_child` and the resume path): the
        // fork pipeline recreates the agent from the source's config
        // snapshot, whose `subagent_host` slot carries the SOURCE
        // session's host — children spawned through it land in the
        // source's TaskManager (starving the fork's stall-watchdog
        // liveness feed) and journal in the source's agents/ directory.
        // The host needs a `Weak` to the FORKED session, so it can only
        // be built here, after the fork created it.
        if let (Some(llm_factory), Some(profile_manager)) =
            (&self.llm_factory, &self.profile_manager)
            && let Some(agent) =
                resolve_session_agent(&new_session, Some(DEFAULT_MAIN_AGENT_ID)).await
        {
            let parent_config = {
                let config_arc = agent.config();
                let guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                (*guard).clone()
            };
            let mut parent_config = parent_config;
            parent_config.subagent_host = None;
            let event_bus = self
                .resolve_session_artifacts(new_session.id().as_str())
                .ok()
                .map(|artifacts| Arc::clone(&artifacts.event_bus));
            // The session's turn-id allocator — the forked host's child
            // flows continue the forked log's numbering (the same counter
            // the fork seeded).
            let turn_id_source = self
                .resolve_session_artifacts(new_session.id().as_str())
                .ok()
                .map(|artifacts| artifacts.turn_id_counter());
            parent_config.event_bus = event_bus.clone();
            let flow_config = self.turn_flow_config_for(&parent_config.model_id);
            let host = Arc::new(
                mirri_core::SessionSubagentHost::new(
                    Arc::downgrade(&new_session),
                    Arc::clone(profile_manager),
                    AgentId::new(DEFAULT_MAIN_AGENT_ID),
                    parent_config,
                    Arc::new(SubagentLlmAdapter(Arc::clone(llm_factory))),
                    flow_config,
                    Some(Arc::new(crate::model_catalog::ConfigModelCatalog::new(
                        Arc::clone(&self.config),
                    ))),
                    event_bus,
                )
                .with_nesting_limits(1, new_session.config().max_nesting_depth)
                .with_turn_id_source(turn_id_source)
                .with_tool_result_home(self.home_dir().map(ToOwned::to_owned))
                .with_flag_registry(Arc::clone(self.flag_registry())),
            );
            // spec 11 R5/A3 (defect B): bind the host to the forked main
            // agent's LIVE config handle — children spawned after a later
            // `setModel` inherit the NEW model, matching the fresh-create
            // path.
            host.set_parent_config_shared(agent.config());
            // B4 T8 (D-55): adopt subagents the fork carried over from
            // the source's replayed `subagent.registered` entries, so
            // `Agent(resume=...)` keeps identity in the fork.
            host.adopt_session_agents().await;
            let host = host as Arc<dyn mirri_core::SubagentHost>;
            // Register the tools onto the existing agent's tool manager —
            // `register_builtin` is a same-name map insert, so this
            // REPLACES the source-host-backed Agent/AgentSwarm tools the
            // config snapshot carried in — and record the host on the
            // agent's config so later reads see it.
            {
                let tool_manager = agent.tool_manager();
                // Same background gate the agent-scope assembly builds
                // (B1 D-50): a Weak handle onto the live tool manager
                // the tools register into.
                let weak = std::sync::Arc::downgrade(&tool_manager);
                let background_gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                    weak.upgrade()
                        .map(|tools| {
                            ["TaskList", "TaskOutput", "TaskStop"]
                                .iter()
                                .all(|name| tools.read().is_tool_active(name))
                        })
                        .unwrap_or(false)
                });
                let subagent_timeout = new_session.config().task_lifecycle.subagent_timeout;
                let mut tools = tool_manager.write();
                tools.register_builtin(Arc::new(mirri_core::tools::AgentTool::new(
                    Arc::clone(&host),
                    agent.tasks(),
                    new_session.id().as_str().to_string(),
                    subagent_timeout,
                    background_gate,
                )));
                tools.register_builtin(Arc::new(mirri_core::tools::AgentSwarmTool::new(
                    Arc::clone(&host),
                    subagent_timeout,
                    new_session.config().task_lifecycle.max_swarm_items,
                )));
            }
            agent
                .config()
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .subagent_host = Some(host);
        }

        // The new session must stream its events like any other session.
        self.spawn_session_relay(new_session.id().as_str());

        // Phase 3: the fork's index row (best-effort; a failure never
        // fails the RPC) — every other write path (create/resume/rename/
        // updateMeta/close) upserts on mutation, and the fork is one. The
        // row carries the inherited last_prompt (the live view's
        // last_prompt reads the index row, N1), so an exclude-empty
        // listing — the web refresh path — surfaces the fork instead of
        // dropping it as "never conversed".
        self.index_upsert_live(new_session.id(), &new_session.metadata().await);

        serde_json::to_value(self.session_view(&new_session).await)
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.rename` — set a session's display title (mirrors
    /// `mirri-server`'s title validation: non-empty, at most 200 characters).
    pub(super) async fn dispatch_rename_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct RenameSessionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            title: String,
        }
        let params: RenameSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let title = params.title.trim().to_string();
        if title.is_empty() {
            return Err(invalid_params("title must not be empty"));
        }
        if title.chars().count() > 200 {
            return Err(invalid_params("title exceeds 200 characters"));
        }

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        session
            .update_metadata(|metadata| {
                metadata.title = title;
                // A rename is user intent: later prompts must not overwrite
                // it with the auto-title.
                metadata.is_custom_title = true;
                metadata.updated_at = Utc::now();
            })
            .await;

        // Phase 3: the renamed session's row keeps the index warm
        // (best-effort; a failure never fails the RPC).
        self.index_upsert_live(
            &SessionId::new(&params.session_id),
            &session.metadata().await,
        );

        // Announce the metadata change on the session's event bus (the
        // `session.meta.updated` wire event).
        self.publish_session_event(
            &params.session_id,
            SessionEvent::SessionMetaUpdated {
                title: Some(session.metadata().await.title),
                patch: None,
            },
        );

        serde_json::to_value(self.session_view(&session).await)
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.touch` — bump ONLY the user-interaction recency key
    /// `last_used_at` (SDR-014 / event-delivery ADR-003): a client whose
    /// user interacted with the session (selected it, expanded it) marks
    /// it "used" so the sidebar re-sorts — WITHOUT refreshing
    /// `updated_at` (a touch is not a content update, so
    /// [`SessionScope::update_metadata`] is deliberately NOT used here; it
    /// unconditionally stamps `updated_at`).
    ///
    /// The write-through mirrors [`note_turn_settled`]'s steps 2-3
    /// (state.json save + index upsert, both best-effort — a persistence
    /// or index failure logs and never fails the RPC), but without its
    /// step 1: no `update_metadata`, the dedicated
    /// [`SessionScope::touch_last_used`] writes the key alone.
    pub(super) async fn dispatch_touch_session(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct TouchSessionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
        }
        let params: TouchSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let session_id = SessionId::new(&params.session_id);

        // 1. Bump ONLY last_used_at — updated_at stays untouched (the
        //    SDR-014 contract: the touch is a user-interaction signal, not
        //    a content change).
        session.touch_last_used().await;
        let metadata = session.metadata().await;

        // 2. `state.json` write-through (best-effort, the note_turn_settled
        //    pattern): a touch must survive a restart, or the sidebar
        //    re-sorts back on reload.
        if let Ok((owner_manager, _workspace_id)) = self.resolve_session_owner(&params.session_id)
            && let Err(error) = owner_manager
                .persistence()
                .save_state(&session_id, &metadata)
                .await
        {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "touch state save failed; the last-used recency stays in memory"
            );
        }

        // 3. Index write-through (best-effort; the row's updated_at keeps
        //    the listing order honest while the live view carries the
        //    fresh last_used_at).
        self.index_upsert_live(&session_id, &metadata);

        serde_json::to_value(self.session_view(&session).await)
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.updateMeta` — update a session's tags and title, and apply
    /// the runtime `model`/`thinking` changes.
    ///
    /// Tolerant of both wire conventions: the fields may sit at the top
    /// level (`{ sessionId, tags, title, model, thinking }`) or nested under
    /// `metadata` (the TS `SessionMetadataPatch` shape). Unsupported keys
    /// are ignored rather than rejected — the embedded engine has no
    /// arbitrary key-value metadata store. `model` must be a configured
    /// alias (else the TS `model.not_found` domain error); it is applied to
    /// the session's first agent and persisted for resume replay. `thinking`
    /// accepts the rust spellings (`none`/`low`/`medium`/`high`) plus any
    /// model-declared effort string carried verbatim (e.g. `max`, `xhigh`),
    /// validated against the model's `support_efforts` when it declares
    /// one; it is parsed like `agent.setThinking` and applied in memory
    /// only.
    pub(super) async fn dispatch_update_session_metadata(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct UpdateSessionMetadataParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            tags: Option<Vec<String>>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default, alias = "thinkingEffort")]
            thinking: Option<String>,
            #[serde(default)]
            metadata: Option<Value>,
        }
        let params: UpdateSessionMetadataParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // Flatten the nested TS patch shape
        // (`metadata: { title?, tags?, model?, thinking? }`) onto the
        // top-level fields.
        let nested_title = params
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("title"))
            .and_then(|title| title.as_str())
            .map(str::to_string);
        let nested_tags = params
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("tags"))
            .and_then(|tags| serde_json::from_value::<Vec<String>>(tags.clone()).ok());
        let nested_model = params
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("model"))
            .and_then(|model| model.as_str())
            .map(str::to_string);
        let nested_thinking = params
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("thinking"))
            .and_then(|thinking| thinking.as_str())
            .map(str::to_string);
        let title = params.title.or(nested_title);
        let tags = params.tags.or(nested_tags);
        let model = params.model.or(nested_model);
        let thinking = params.thinking.or(nested_thinking);

        if let Some(title) = &title {
            let title = title.trim();
            if title.is_empty() {
                return Err(invalid_params("title must not be empty"));
            }
            if title.chars().count() > 200 {
                return Err(invalid_params("title exceeds 200 characters"));
            }
        }
        if let Some(tags) = &tags
            && tags.iter().any(|tag| tag.chars().count() > 50)
        {
            return Err(invalid_params("tags must be at most 50 characters each"));
        }

        // Apply the runtime model change, when present. One param-carrying
        // update is one standard switch: the shared path validates the
        // alias, sets the live config, persists, pushes the context
        // window, records the marker, publishes the event.
        if let Some(model) = &model {
            let ref_params = AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: None,
            };
            let agent = self.resolve_agent(&ref_params).await?;
            switch_session_model(self, &ref_params.session_id, &agent, model).await?;
        }
        // Apply the runtime thinking effort, when present. Parsed with the
        // same serde spelling set as `agent.setThinking` (including the
        // `effort` alias and the `off`/`on` kosong spellings), and applied
        // in memory only — thinking is not persisted for resume replay,
        // mirroring the `agent.setThinking` semantics.
        if let Some(thinking) = &thinking {
            let effort: ThinkingEffort = serde_json::from_value(json!(thinking))
                .map_err(|e| invalid_params(e.to_string()))?;
            let ref_params = AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: None,
            };
            let agent = self.resolve_agent(&ref_params).await?;
            // Validate against the post-switch model: the model branch above
            // has already run, so an updateMeta carrying both { model,
            // thinking } validates the thinking against the NEW model.
            let model_id = agent.model_id();
            self.validate_effort_for_model(&model_id, &effort)?;
            agent.set_thinking(Some(effort));
        }

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        session
            .update_metadata(|metadata| {
                if let Some(title) = &title {
                    metadata.title = title.clone();
                    // A user-set title via updateMeta is user intent, the
                    // same as `session.rename`: later prompts must not
                    // overwrite it with the auto-title (A2 guard).
                    metadata.is_custom_title = true;
                }
                if let Some(tags) = &tags {
                    metadata.tags = tags.iter().cloned().collect();
                }
                metadata.updated_at = Utc::now();
            })
            .await;

        // Phase 3: the updated metadata keeps the index row warm
        // (best-effort; a failure never fails the RPC).
        self.index_upsert_live(
            &SessionId::new(&params.session_id),
            &session.metadata().await,
        );

        // Announce the metadata change on the session's event bus (the
        // `session.meta.updated` wire event): the patch object carries
        // the changed fields (camelCase, TS `SessionMetadataPatch` shape).
        if title.is_some() || tags.is_some() || model.is_some() || thinking.is_some() {
            let mut patch = serde_json::Map::new();
            if let Some(title) = &title {
                patch.insert("title".to_string(), json!(title));
            }
            if let Some(tags) = &tags {
                patch.insert("tags".to_string(), json!(tags));
            }
            if let Some(model) = &model {
                patch.insert("model".to_string(), json!(model));
            }
            if let Some(thinking) = &thinking {
                patch.insert("thinking".to_string(), json!(thinking));
            }
            self.publish_session_event(
                &params.session_id,
                SessionEvent::SessionMetaUpdated {
                    title: None,
                    patch: Some(Value::Object(patch)),
                },
            );
        }

        serde_json::to_value(self.session_view(&session).await)
            .map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `session.getMeta` — the session's metadata: id, title, timestamps,
    /// agent ids, and tags.
    pub(super) async fn dispatch_get_session_metadata(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let metadata = session.metadata().await;
        let agents: Vec<String> = session
            .list_agents()
            .await
            .iter()
            .map(|agent_id| agent_id.as_str().to_string())
            .collect();
        to_result(SessionMetaResult {
            id: session.id().as_str().to_string(),
            title: metadata.title,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            agents,
            tags: metadata.tags.into_iter().collect(),
        })
    }

    /// `session.get` — the session's summary view plus its agent list. The
    /// B1 EMB backfill of the klient `sessionIndex.get` reference; serves
    /// the same wire shape mirri-server's transport-owned handler does, so
    /// both backends answer identically.
    pub(super) async fn dispatch_get_session(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agents: Vec<SessionAgentResult> = session
            .list_agents()
            .await
            .iter()
            .map(|agent_id| SessionAgentResult {
                id: agent_id.as_str().to_string(),
            })
            .collect();
        to_result(SessionDetailsResult {
            view: self.session_view(&session).await,
            agents,
        })
    }

    /// `session.countActive` — the B1 gap implementation of the klient
    /// `sessionIndex.countActive` reference: the number of non-archived
    /// (live) sessions. The engine binds a single workspace, so the
    /// `workspaceIds` filter is advisory: an empty array selects no
    /// workspaces (0); any non-empty selection (or an omitted filter)
    /// counts the engine's live sessions.
    pub(super) async fn dispatch_count_active_sessions(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CountActiveParams {
            #[serde(default, alias = "workspace_ids")]
            workspace_ids: Option<Vec<String>>,
        }
        let params: CountActiveParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let count = match params.workspace_ids {
            Some(ref ids) if ids.is_empty() => 0,
            _ => self.manager.list_sessions().await.len(),
        };
        to_result(count)
    }

    /// `session.createChild` — the B1 gap implementation of the klient
    /// `sessionLifecycle.createChild` reference: fork the source session's
    /// first agent into a new child session and answer the scope-handle
    /// wire shape `{ id, kind }`. The engine mints its own session ids
    /// (`newSessionId` is rejected explicitly — no silent id drift) and
    /// does not model the parent-session linkage the v2 metadata records.
    pub(super) async fn dispatch_create_child_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreateChildParams {
            source_session_id: String,
            #[serde(default)]
            new_session_id: Option<String>,
            #[serde(default)]
            title: Option<String>,
        }
        let params: CreateChildParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if let Some(new_session_id) = params.new_session_id.filter(|id| !id.is_empty()) {
            return Err(invalid_params(format!(
                "newSessionId is not supported by the embedded engine \
                 (the session id is engine-minted): {new_session_id}"
            )));
        }

        let source_session = self
            .resolve_session(&params.source_session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.source_session_id)))?;
        let agent_id = source_session
            .list_agents()
            .await
            .into_iter()
            .next()
            .ok_or_else(|| {
                not_found(format!(
                    "session '{}' has no agents to fork",
                    params.source_session_id
                ))
            })?;

        // The v2 default title: "Child: {source title}".
        let source_title = source_session.metadata().await.title;
        // TS parity (same as `session.fork`): the source session's last
        // prompt carries into the child — its metadata is the fresh live
        // state (ADR-009), duplicated here before the metadata is built.
        let source_last_prompt = source_session.metadata().await.last_prompt;
        let now = Utc::now();
        let is_custom_title = params
            .title
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty());
        let metadata = SessionMetadata {
            title: params
                .title
                .unwrap_or_else(|| format!("Child: {source_title}")),
            is_custom_title,
            created_at: now,
            updated_at: now,
            tags: Default::default(),
            archived: false,
            work_dir: None,
            last_prompt: source_last_prompt,
            // createChild does not mint lineage in F3 (only session.fork
            // does; the v2 parent linkage is out of scope — see the
            // dispatch doc-comment).
            forked_from: None,
            cache_root_id: None,
            custom: None,
            last_used_at: None,
        };
        // The fork lands in the SOURCE session's workspace (W3): the new
        // session's persistence bucket and sandbox root follow the source.
        let (owner_manager, owning_workspace) =
            self.resolve_session_owner(&params.source_session_id)?;
        let new_session = owner_manager
            .fork_session(&source_session, &agent_id, metadata)
            .await
            .map_err(session_error)?;

        // The routing seam's first level (W3): the child session is active
        // in its owning workspace from the moment it exists.
        self.note_active_session_in(&owning_workspace, new_session.id().as_str());

        // Register MCP tools into the child's rebuilt main agent, exactly
        // like `session.create`/`session.resume` (and `session.fork`):
        // createChild shares the fork pipeline, which mints a fresh tool
        // manager — without this the child session would silently lose
        // every workspace/user-pool MCP tool until some later reconnect
        // event happened to re-register them.
        if let Some(agent) = resolve_session_agent(&new_session, Some(DEFAULT_MAIN_AGENT_ID)).await
        {
            self.register_mcp_tools(&agent, new_session.id().as_str())
                .await;
        }

        // Rebuild the subagent host for the child session's main agent
        // (the create_child sibling of resume's F-P7-3 fix — the same
        // rebuild as `dispatch_fork_session` and the resume path):
        // createChild shares the fork pipeline, so the child agent's
        // config snapshot carries the SOURCE session's `subagent_host`
        // — children spawned through it land in the source's
        // TaskManager (starving the child's stall-watchdog liveness
        // feed) and journal in the source's agents/ directory. The host
        // needs a `Weak` to the CHILD session, so it can only be built
        // here, after the fork created it.
        if let (Some(llm_factory), Some(profile_manager)) =
            (&self.llm_factory, &self.profile_manager)
            && let Some(agent) =
                resolve_session_agent(&new_session, Some(DEFAULT_MAIN_AGENT_ID)).await
        {
            let parent_config = {
                let config_arc = agent.config();
                let guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                (*guard).clone()
            };
            let mut parent_config = parent_config;
            parent_config.subagent_host = None;
            let event_bus = self
                .resolve_session_artifacts(new_session.id().as_str())
                .ok()
                .map(|artifacts| Arc::clone(&artifacts.event_bus));
            // The session's turn-id allocator — the child host's flows
            // continue the forked log's numbering (the same counter the
            // fork seeded).
            let turn_id_source = self
                .resolve_session_artifacts(new_session.id().as_str())
                .ok()
                .map(|artifacts| artifacts.turn_id_counter());
            parent_config.event_bus = event_bus.clone();
            let flow_config = self.turn_flow_config_for(&parent_config.model_id);
            let host = Arc::new(
                mirri_core::SessionSubagentHost::new(
                    Arc::downgrade(&new_session),
                    Arc::clone(profile_manager),
                    AgentId::new(DEFAULT_MAIN_AGENT_ID),
                    parent_config,
                    Arc::new(SubagentLlmAdapter(Arc::clone(llm_factory))),
                    flow_config,
                    Some(Arc::new(crate::model_catalog::ConfigModelCatalog::new(
                        Arc::clone(&self.config),
                    ))),
                    event_bus,
                )
                .with_nesting_limits(1, new_session.config().max_nesting_depth)
                .with_turn_id_source(turn_id_source)
                .with_tool_result_home(self.home_dir().map(ToOwned::to_owned))
                .with_flag_registry(Arc::clone(self.flag_registry())),
            );
            // spec 11 R5/A3 (defect B): bind the host to the child main
            // agent's LIVE config handle — children spawned after a later
            // `setModel` inherit the NEW model, matching the fresh-create
            // path.
            host.set_parent_config_shared(agent.config());
            // B4 T8 (D-55): adopt subagents the fork carried over from
            // the source's replayed `subagent.registered` entries, so
            // `Agent(resume=...)` keeps identity in the child session.
            host.adopt_session_agents().await;
            let host = host as Arc<dyn mirri_core::SubagentHost>;
            // Register the tools onto the existing agent's tool manager —
            // `register_builtin` is a same-name map insert, so this
            // REPLACES the source-host-backed Agent/AgentSwarm tools the
            // config snapshot carried in — and record the host on the
            // agent's config so later reads see it.
            {
                let tool_manager = agent.tool_manager();
                // Same background gate the agent-scope assembly builds
                // (B1 D-50): a Weak handle onto the live tool manager
                // the tools register into.
                let weak = std::sync::Arc::downgrade(&tool_manager);
                let background_gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                    weak.upgrade()
                        .map(|tools| {
                            ["TaskList", "TaskOutput", "TaskStop"]
                                .iter()
                                .all(|name| tools.read().is_tool_active(name))
                        })
                        .unwrap_or(false)
                });
                let subagent_timeout = new_session.config().task_lifecycle.subagent_timeout;
                let mut tools = tool_manager.write();
                tools.register_builtin(Arc::new(mirri_core::tools::AgentTool::new(
                    Arc::clone(&host),
                    agent.tasks(),
                    new_session.id().as_str().to_string(),
                    subagent_timeout,
                    background_gate,
                )));
                tools.register_builtin(Arc::new(mirri_core::tools::AgentSwarmTool::new(
                    Arc::clone(&host),
                    subagent_timeout,
                    new_session.config().task_lifecycle.max_swarm_items,
                )));
            }
            agent
                .config()
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .subagent_host = Some(host);
        }

        // The new session must stream its events like any other session.
        self.spawn_session_relay(new_session.id().as_str());

        // Phase 3: the child's index row (best-effort) — createChild
        // shares the fork pipeline, so it shares the fork's missing
        // write-through too: without the row, an exclude-empty listing
        // drops the child until its first message stamps a last_prompt
        // (the same defect session.fork's write-through fixes).
        self.index_upsert_live(new_session.id(), &new_session.metadata().await);

        to_result(SessionHandleResult {
            id: new_session.id().as_str().to_string(),
            kind: SESSION_SCOPE_KIND,
        })
    }

    /// `session.registerAgent` — the B1 gap implementation of the klient
    /// `sessionMetadata.registerAgent` reference: create the named agent
    /// in the addressed session (the registration's engine effect). The
    /// engine has no agent-meta store, so the klient `agentMeta` fields
    /// (type/parent/labels/…) are accepted but not modeled yet; the
    /// method answers JSON null (the klient `noResult` spelling).
    pub(super) async fn dispatch_register_agent(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        // The klient `agentMeta` shape (sessionMetadata.ts) — kept explicit
        // so the wire contract is documented; the fields are accepted but
        // not modeled by the engine yet (see the method docs).
        #[allow(dead_code)]
        #[derive(Debug, Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AgentMetaParams {
            #[serde(default)]
            homedir: Option<String>,
            #[serde(default, rename = "type")]
            agent_type: Option<String>,
            #[serde(default)]
            parent_agent_id: Option<String>,
            #[serde(default)]
            forked_from: Option<String>,
            #[serde(default)]
            labels: Option<std::collections::HashMap<String, String>>,
            #[serde(default)]
            swarm_item: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct RegisterAgentParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "agentId")]
            agent_id: String,
            #[serde(default, flatten)]
            meta: AgentMetaParams,
        }
        let params: RegisterAgentParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // Accepted, not modeled (see the method docs) — recorded at debug
        // level so the dropped klient agentMeta fields stay traceable.
        tracing::debug!(meta = ?params.meta, "session.registerAgent agentMeta accepted (not modeled yet)");

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        // The same interaction-kernel wiring `session.create` applies (B4-a):
        // the registered agent's ask flows must register with the session's
        // HITL gates so the embedded interaction RPCs can see and resolve
        // them. (Like `session.create`, `registerAgent` does not wire the
        // reverse-RPC callback — the engine-level registration path.)
        let interaction_kernel = self
            .resolve_session_artifacts(session.id().as_str())
            .ok()
            .map(|artifacts| {
                Arc::new(
                    mirri_core::InteractionKernel::new(
                        Arc::clone(&artifacts.approval_gate),
                        Arc::clone(&artifacts.question_gate),
                    )
                    // Plan D3: same grant-sink wiring as `session.create`.
                    .with_grant_sink(session_grant_sink(&session)),
                )
            });
        session
            .create_agent(
                AgentId::new(&params.agent_id),
                AgentScopeConfig {
                    interaction_kernel,
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .map_err(engine_error)?;
        Ok(Value::Null)
    }

    /// `session.export` — export a session's persisted files to a zip
    /// archive under `<home>/.mirri-code/exports/`. Mirrors `mirri-server`:
    /// caller-supplied `outputPath` must resolve inside the export root.
    pub(super) async fn dispatch_export_session(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ExportSessionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "outputPath")]
            output_path: Option<String>,
            #[serde(default, alias = "includeGlobalLog")]
            include_global_log: Option<bool>,
        }
        let params: ExportSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // Resolve the Mirri home: the server's injected home (the NAPI
        // `createEngine(homeDir)` path) wins, then the env-based
        // `mirri_home_dir()` resolution (`MIRRICODE_HOME`, then
        // `$HOME/.mirri-code`). The former direct `$HOME` read bypassed both
        // and broke home isolation for injected-home engines.
        let mirri_home = self
            .home_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(mirri_core::config::mirri_home_dir)
            .map_err(|_| {
                EngineError::Internal(
                    "HOME is not set; cannot locate session export directory".to_string(),
                )
            })?;
        let export_root = mirri_home.clone();
        std::fs::create_dir_all(&export_root).map_err(|e| {
            EngineError::Internal(format!(
                "failed to prepare export directory '{}': {e}",
                export_root.display()
            ))
        })?;
        let output_path = match params.output_path {
            Some(raw_path) => {
                let requested = PathBuf::from(&raw_path);
                match validate_workspace_path(&export_root, &requested) {
                    Ok(validated) => Some(validated.to_string_lossy().into_owned()),
                    Err(error) => {
                        return Err(invalid_params(format!("output_path rejected: {error}")));
                    }
                }
            }
            None => None,
        };

        let exporter = SessionExporter::with_mirri_home(&mirri_home);
        let payload = ExportSessionPayload {
            session_id: params.session_id.clone(),
            output_path,
            include_global_log: params.include_global_log,
            include_desktop_log: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };
        let result = exporter
            .export(payload, None)
            .map_err(|e| EngineError::Internal(e.to_string()))?;

        // The v2 `ExportSessionResult` contract: {zipPath, entries,
        // sessionDir, manifest} — the manifest serialises camelCase via
        // mirri-core's serde attributes.
        to_result(ExportResult {
            zip_path: result.zip_path,
            entries: result.entries,
            session_dir: result.session_dir,
            manifest: result.manifest,
        })
    }

    /// `session.warnings` — session-level warning entries in the TS
    /// `SessionWarning[]` wire shape (bare array). The engine records no
    /// warnings anywhere yet, so the honest answer is an empty list (the
    /// client receives its expected shape instead of an error).
    pub(super) async fn dispatch_get_session_warnings(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        self.resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        Ok(Value::Array(Vec::new()))
    }

    /// `session.clearContext` — reset the agent's context memory, removing
    /// all history, open steps, and deferred messages.
    pub(super) async fn dispatch_clear_context(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        agent
            .context_memory()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        ack()
    }

    /// `session.undo` — remove the last `count` user-origin messages (and
    /// everything after them) from the agent's context memory.
    pub(super) async fn dispatch_undo_history(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct UndoHistoryParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(default)]
            count: Option<usize>,
        }
        let params: UndoHistoryParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = resolve_session_agent(&session, params.agent_id.as_deref())
            .await
            .ok_or_else(|| {
                not_found(format!("session '{}' has no agents yet", params.session_id))
            })?;
        let count = params.count.unwrap_or(1);
        // SDR-B2-01 task 2/4 reconciliation: the durable delivery marker is
        // valid only while the delivered notification message stays in
        // context. Undo truncates the history tail — a background-task
        // notification message in the removed tail re-arms its task's
        // delivery (the driver's marker filter alone would otherwise keep
        // the debt suppressed forever; the core contract is pinned by
        // `should_deliver_a_task_whose_notification_marker_was_cleared_when_undo_removed_the_message`).
        let removed_notification_task_ids = {
            let context_memory = agent.context_memory();
            let mut memory = context_memory.lock().unwrap_or_else(|e| e.into_inner());
            let before: Vec<Option<PromptOrigin>> =
                memory.history().iter().map(|m| m.origin.clone()).collect();
            memory.undo_count(count).map_err(engine_error)?;
            // TS parity (`conversationTime.ts` `context.undo`): the todo list
            // rolls back to the checkpoint at the popped depth. The pop
            // mirrors the reducer's fold — a count exceeding the stack
            // leaves the list unchanged.
            session.todo_list().pop_checkpoint(count);
            // The removed tail is the longest common SUFFIX of the origin
            // sequence, not a length diff: `undo_count` can also pop leading
            // ephemeral messages, shrinking history without removing the
            // tail — a length-based diff would sweep a KEPT notification
            // into the removed set and re-arm its delivery.
            let after: Vec<Option<PromptOrigin>> =
                memory.history().iter().map(|m| m.origin.clone()).collect();
            let mut common = 0;
            while common < before.len().min(after.len())
                && before[before.len() - 1 - common] == after[after.len() - 1 - common]
            {
                common += 1;
            }
            let removed = before[..before.len() - common]
                .iter()
                .filter_map(|origin| match origin {
                    Some(PromptOrigin::BackgroundTask { task_id, .. }) => Some(task_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            drop(memory);
            removed
        };
        for task_id in &removed_notification_task_ids {
            session.tasks().clear_notification_delivered(task_id);
        }
        to_result(UndoResult {
            ok: true,
            undone: count,
        })
    }

    /// `agent.getContext` — the addressed agent's context-memory snapshot:
    /// message count and estimated token usage.
    pub(super) async fn dispatch_get_context(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let memory = agent.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        to_result(GetContextResult {
            context: ContextSummary {
                message_count: memory.history().len(),
                used_tokens: memory.token_count(),
            },
        })
    }

    /// `session.getHistory` — the addressed agent's full context history as
    /// a bare array of serialized [`ContextMessage`]s (the flattened wire
    /// `Message` plus the `origin`/`isError`/`note` provenance metadata).
    ///
    /// The read face behind ACP `session/load` history replay (and, later,
    /// the web transcript endpoint): one read method, two consumers. The
    /// caller projects the wire messages into its own update vocabulary —
    /// the embedded engine never guesses a client-side shape, and the ACP
    /// layer never reads `wire.jsonl` directly (that would bypass the
    /// engine's resume-reconstructed context memory, the same source
    /// `agent.getContext` counts).
    pub(super) async fn dispatch_get_history(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let memory = agent.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        let history: Vec<Value> = memory
            .history()
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| EngineError::Internal(e.to_string()))?;
        Ok(Value::Array(history))
    }

    /// `session.search` — the P10 global literal search read face: a
    /// full-scan, no-index NFKC + case-folded substring match over every
    /// session's title and user/assistant message text, ordered
    /// time-desc, with opaque fingerprint cursor pagination (a
    /// mid-pagination condition change answers `invalid_params`, the TS
    /// fingerprint semantics).
    ///
    /// **W5 multi-workspace**: the scan crosses every registered workspace
    /// in the visibility scope (each runtime lazily built on the search's
    /// first touch), and every hit reports its session's OWN workspace id —
    /// never the bound root. Embedded hosts pin the scan to the default
    /// (pinned) workspace.
    ///
    /// Matching sources: message text through the same read path
    /// `session.getHistory` serves (live context memory, or an in-memory
    /// wire-log replay for persisted-only sessions — never a resume, never
    /// a log rewrite); titles through the session store metadata. The
    /// `terms` request mode is accepted but degrades to this literal scan
    /// (the response's `mode` echoes the effective mode; the word-level
    /// index is the recorded F-P10-1 followup). Tool-call content is not
    /// indexed (F-P10-2); hit `time` is the session's `updatedAt` epoch ms
    /// because context memory carries no per-message timestamps.
    pub(super) async fn dispatch_search(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SearchContainerParams {
            #[serde(default, alias = "sessionId")]
            session_id: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct SearchParams {
            query: String,
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            container: Option<SearchContainerParams>,
            #[serde(default)]
            role: Option<String>,
            #[serde(default, alias = "pageSize")]
            page_size: Option<u32>,
            #[serde(default, alias = "pageToken")]
            page_token: Option<String>,
        }
        let params: SearchParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        // Validate the query: the literal fold must reach at least two
        // normalized characters (an n-gram degenerate otherwise).
        let folded_query = search::fold_literal(&params.query);
        if !search::query_is_searchable(&folded_query) {
            return Err(invalid_params(
                "query must normalize to at least 2 characters for literal search",
            ));
        }
        // Validate the mode (terms degrades to literal below — the
        // response echoes the effective mode).
        if let Some(mode) = params.mode.as_deref()
            && !matches!(mode, "terms" | "literal")
        {
            return Err(invalid_params(format!("unknown search mode: {mode}")));
        }
        // Validate the role filter.
        let role = params.role.as_deref();
        if let Some(role) = role
            && !matches!(role, "user" | "assistant" | "title")
        {
            return Err(invalid_params(format!("unknown search role: {role}")));
        }
        // Clamp the page size (default 20, cap 50 — the TS bounds).
        let page_size = params
            .page_size
            .unwrap_or(search::PAGE_SIZE_DEFAULT)
            .clamp(1, search::PAGE_SIZE_MAX);
        // Resolve the cursor: the offset plus the fingerprint of the
        // query conditions the token was minted under.
        let container_session_id = params
            .container
            .as_ref()
            .and_then(|container| container.session_id.as_deref());
        let fp = search::fingerprint(&folded_query, container_session_id, role, page_size);
        let offset = match params.page_token.as_deref() {
            None => 0,
            Some(token) => {
                let (offset, token_fp) = search::decode_page_token(token)
                    .ok_or_else(|| invalid_params("page_token is not a valid search cursor"))?;
                if token_fp != fp {
                    return Err(invalid_params(
                        "page_token fingerprint mismatch: the search conditions changed mid-pagination",
                    ));
                }
                offset
            }
        };

        // Collect the scannable documents across every workspace in scope
        // (W5 multi-workspace: each registered workspace of the visibility
        // scope, its runtime lazily built on the search's first touch).
        // Search scans every session including archived ones (the same set
        // it saw before the archived listing filter existed) — a search
        // targeting an archived session must still find it.
        let scope_ids: Option<Vec<String>> = match self.listing_visibility {
            ListingVisibility::PinnedToDefault => self
                .registry
                .default_workspace_id()
                .map(|default| vec![default]),
            ListingVisibility::AllRegistered => None,
        };
        let in_scope = |workspace_id: &str| -> bool {
            match scope_ids.as_deref() {
                Some(ids) => ids.iter().any(|id| id == workspace_id),
                None => true,
            }
        };
        // The session → owning-workspace attribution of the scan (W5: true
        // attribution — the workspace whose bucket the session lives in,
        // never the bound root).
        let mut workspace_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // The scannable summaries collected first (owned), so the borrowed
        // `ScanDoc`s below can outlive the per-workspace build loop.
        let mut scanned: Vec<mirri_core::SessionInfo> = Vec::new();
        for registration in self.registry.registrations() {
            if !in_scope(&registration.id) {
                continue;
            }
            let runtime = match self.registry.ensure_built(&registration.id) {
                Some(Ok(runtime)) => runtime,
                Some(Err(error)) => {
                    tracing::warn!(
                        workspace = %registration.id,
                        %error,
                        "building the workspace for the search scan failed; its sessions degrade out of the scan"
                    );
                    continue;
                }
                None => continue,
            };
            for info in runtime.manager.list_all_sessions(true).await {
                workspace_of.insert(info.id.clone(), registration.id.clone());
                scanned.push(info);
            }
        }
        let mut docs: Vec<search::ScanDoc<'_>> = Vec::new();
        for info in &scanned {
            if let Some(wanted) = container_session_id
                && info.id != wanted
            {
                continue;
            }
            let updated_ms = info.updated_at.timestamp_millis();
            // The title document (the same doc shape the TS index keeps).
            if !info.title.is_empty() {
                docs.push(search::ScanDoc {
                    session_id: &info.id,
                    session_title: &info.title,
                    agent_id: "".to_string(),
                    role: "title",
                    text: info.title.clone(),
                    time: updated_ms,
                    turn: None,
                    ord: 0,
                });
            }
            // The message documents: user / assistant text through the
            // read-only history path (live memory, or the in-memory
            // replay of a persisted-only session) — resolved through the
            // routing seam onto the session's OWNING workspace's manager.
            let Ok(owner_manager) = self.resolve_session_manager(&info.id) else {
                continue;
            };
            let Some(histories) = owner_manager
                .read_only_histories(&SessionId::new(&info.id))
                .await
            else {
                // Neither live nor replayable — the session's messages
                // degrade out of the scan, never fail the query.
                continue;
            };
            for (agent_id, history) in histories {
                let mut turn: u64 = 0;
                for (index, message) in history.iter().enumerate() {
                    let doc_turn = match message.message.role {
                        mirri_core::Role::User => {
                            let at = turn;
                            turn += 1;
                            Some(at)
                        }
                        mirri_core::Role::Assistant => Some(turn.saturating_sub(1)),
                        // System / tool text is not indexed (F-P10-2).
                        _ => continue,
                    };
                    let text = message.message.text_content();
                    if text.is_empty() {
                        continue;
                    }
                    docs.push(search::ScanDoc {
                        session_id: &info.id,
                        session_title: &info.title,
                        agent_id: agent_id.clone(),
                        role: if message.message.role == mirri_core::Role::User {
                            "user"
                        } else {
                            "assistant"
                        },
                        text,
                        time: updated_ms,
                        turn: doc_turn,
                        ord: index as u64,
                    });
                }
            }
        }

        // Match, filter, sort (time-desc), and paginate.
        let hits = search::scan_docs(&docs, &folded_query, role);
        // Out-of-range offsets answer the empty page (the TS `Array.slice`
        // semantics) instead of panicking on an out-of-bounds slice; the
        // saturating add also guards `offset + page_size` against overflow
        // (F-P10-5).
        let (page, page_end) = if offset < hits.len() {
            let page_end = offset.saturating_add(page_size as usize).min(hits.len());
            let page = hits[offset..page_end]
                .iter()
                .map(|hit| SessionSearchHitResult {
                    session_id: hit.session_id.clone(),
                    // True attribution (W5): the workspace whose bucket the
                    // session was scanned in — never the bound root.
                    workspace_id: workspace_of
                        .get(&hit.session_id)
                        .cloned()
                        .unwrap_or_default(),
                    session_title: hit.session_title.clone(),
                    agent_id: hit.agent_id.clone(),
                    role: hit.role,
                    snippet: hit.snippet.clone(),
                    time: hit.time,
                    turn: hit.turn,
                })
                .collect();
            (page, page_end)
        } else {
            // The cursor is past the end: the empty page, and no next page
            // (page_end pinned to the list length so the token check
            // below sees an exhausted cursor).
            (Vec::new(), hits.len())
        };
        let next_page_token =
            (page_end < hits.len()).then(|| search::encode_page_token(page_end, &fp));
        to_result(SessionSearchResult {
            hits: page,
            mode: "literal",
            next_page_token,
        })
    }

    /// `agent.getUsage` — the addressed agent's accumulated token usage,
    /// broken down by model, plus the current-turn accumulator, answered as
    /// the bare TS `UsageStatus` (no `usage` envelope; buckets in the TS
    /// `TokenUsage` spelling — ADR-012).
    pub(super) async fn dispatch_get_usage(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let usage = agent
            .usage_service()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status();
        let by_model: Value = usage
            .by_model
            .iter()
            .map(|(model, usage)| {
                // Each by-model entry carries its provider family's
                // cache-write capability bit alongside the TS buckets, so
                // clients can hide the write bucket for families whose
                // protocol has none (SDR-003).
                let mut entry = token_usage_ts_json(usage);
                entry["cacheWriteReported"] = json!(self.model_reports_cache_write(model));
                (model.clone(), entry)
            })
            .collect();
        to_result(GetUsageResult {
            total: token_usage_ts_json(&usage.total),
            current_turn: token_usage_ts_json(&usage.current_turn),
            by_model,
        })
    }

    /// Whether the model alias's effective wire protocol is Anthropic-family
    /// — the only family whose protocol carries a cache-write bucket
    /// (`cache_creation_input_tokens`; SDR-003 / usage-visibility ADR-003).
    /// OpenAI-compatible and Google families have no write field in their
    /// protocols, so their bit is statically false. Derived from the
    /// provider/protocol declaration — never inferred from zero values at
    /// runtime.
    fn model_reports_cache_write(&self, alias: &str) -> bool {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        // The by-model key is either a configured alias (its entry names the
        // provider and may override the wire protocol) or a `provider/model`
        // composite (the key's head names the provider directly).
        let (provider_id, protocol) = match config.models.as_ref().and_then(|m| m.get(alias)) {
            Some(entry) => (entry.provider.clone(), entry.protocol.as_deref()),
            None => match alias.split_once('/') {
                Some((provider, _)) => (provider.to_string(), None),
                None => return false,
            },
        };
        // The provider type drives the protocol; a missing type defaults to
        // openai (the config_factory resolution default).
        let provider_type = config
            .providers
            .as_ref()
            .and_then(|providers| providers.get(&provider_id))
            .and_then(|entry| entry.r#type.as_deref())
            .unwrap_or("openai");
        super::model_handlers::resolve_wire_protocol(protocol, provider_type)
            .is_some_and(|wire_protocol| wire_protocol == "anthropic")
    }

    /// `agent.getModel` — the addressed agent's model id (a plain string,
    /// matching the TS wire type).
    pub(super) async fn dispatch_get_model(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let config_arc = agent.config();
        let model_id = config_arc
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone();
        Ok(Value::String(model_id))
    }

    /// `agent.setModel` — update the agent's model at runtime and echo the
    /// klient result shape: the applied model plus the serialized provider
    /// type (`providerName`, present whenever the config carries one).
    pub(super) async fn dispatch_set_model(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SetModelParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            model: String,
        }
        let params: SetModelParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let ref_params = AgentRefParams {
            session_id: params.session_id,
            agent_id: params.agent_id,
        };
        let agent = self.resolve_agent(&ref_params).await?;
        // Boundary (spec 11 §2.4 / ADR-006): the switch record is
        // session-level with no agent attribution, so the only legal target
        // is the session's MAIN agent (the same first-agent addressing rule
        // `message.send`'s switch uses). A subagent target is rejected with
        // the session-scoped/pinned guidance — its model is pinned at
        // dispatch time (A14), and accepting the switch would fold it onto
        // the main agent at resume (A17).
        if let Some(agent_id) = ref_params.agent_id.as_deref() {
            let session = self.resolve_session(&ref_params.session_id).await?;
            let main = resolve_session_agent(&session, None).await;
            if main.is_none_or(|main| main.id().as_str() != agent_id) {
                return Err(subagent_model_target_rejection(agent_id));
            }
        }
        // The shared switch path — the same one `session.updateMeta`'s model
        // branch and `message.send`'s model param walk: validate the alias,
        // dedupe, set, persist, push the context window, record the marker,
        // publish the event.
        switch_session_model(self, &ref_params.session_id, &agent, &params.model).await?;
        // Read the config back so the echo reflects the applied state. The
        // provider type is a non-optional unit-variant enum, so its
        // serialization is infallible and the klient `providerName` hint is
        // always present on the wire.
        let provider_name = {
            let config = agent.config();
            let config = config.read().unwrap_or_else(|e| e.into_inner());
            serde_json::to_value(config.provider_type)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        to_result(SetModelResult {
            model: params.model,
            provider_name,
        })
    }

    /// Whether `model` resolves to a configured model — the M1 two forms
    /// (spec 11 §2.2): an exact `[models]` alias key, or the canonical
    /// `provider/model` composite the web pickers compose (it resolves to
    /// the configured alias naming the same provider + model pair — the
    /// same membership `model.setDefaultModel` uses). The config's
    /// `default_model` pointer counts as configured by equality. Shared by
    /// every model-carrying entry point so all reject unknown names with
    /// the identical `model.not_found` domain error (R10/A11).
    fn model_exists(&self, model: &str) -> bool {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        config.resolve_model_alias(model).is_some()
            || config.default_model.as_deref() == Some(model)
    }

    /// Validate a requested thinking effort against the model's declared
    /// `support_efforts` — the TS v2 session-level assert
    /// (`assertThinkingEffortSupported`,
    /// `packages/agent-core-v2/src/agent/profile/profileService.ts:432-445`):
    /// no declaration imposes nothing; `none` is always allowed; anything
    /// else must appear in the list.
    fn validate_effort_for_model(
        &self,
        model_id: &str,
        effort: &ThinkingEffort,
    ) -> Result<(), EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let Some(alias_key) = config.resolve_model_alias(model_id) else {
            return Ok(());
        };
        let Some(alias) = config
            .models
            .as_ref()
            .and_then(|models| models.get(&alias_key))
        else {
            return Ok(());
        };
        let Some(supported) = alias.support_efforts.as_ref() else {
            return Ok(());
        };
        if supported.is_empty()
            || matches!(effort, ThinkingEffort::None)
            || supported.iter().any(|s| s == effort.as_str())
        {
            return Ok(());
        }
        Err(invalid_params(format!(
            "unsupported thinking effort '{}' for model {model_id}. Available: {}",
            effort.as_str(),
            supported.join(", ")
        )))
    }

    /// The available-model listing appended to every `model.not_found`
    /// rejection (R10/A11): rendered from the live config snapshot through
    /// the same catalog the dispatch tools, the subagent host, and the
    /// config-aware LLM factory use — one listing, wherever the rejection
    /// fired.
    fn available_model_listing(&self) -> String {
        let catalog = crate::model_catalog::ConfigModelCatalog::new(Arc::clone(&self.config));
        mirri_core::render_model_listing(&catalog)
    }

    /// `agent.setThinking` — update the agent's thinking effort at runtime.
    ///
    /// The canonical param spelling is `thinking`; the `effort` spelling is
    /// accepted as a sibling alias (the node-sdk client sends it). The
    /// klient contract models the method as `noResult`: JSON `null`.
    pub(super) async fn dispatch_set_thinking(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SetThinkingParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(alias = "effort")]
            thinking: ThinkingEffort,
        }
        let params: SetThinkingParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let ref_params = AgentRefParams {
            session_id: params.session_id,
            agent_id: params.agent_id,
        };
        let agent = self.resolve_agent(&ref_params).await?;
        // The per-model guard: an effort outside the model's declared
        // `support_efforts` is rejected here (`none` always allowed).
        let model_id = agent.model_id();
        self.validate_effort_for_model(&model_id, &params.thinking)?;
        agent.set_thinking(Some(params.thinking));
        Ok(Value::Null)
    }

    /// `agent.getEffectiveThinkingLevel` — the agent's currently effective
    /// thinking effort as the `ThinkingEffort` wire spelling: the rust
    /// spellings (`none` / `low` / `medium` / `high`) plus any model-declared
    /// effort string carried verbatim (e.g. `max`, `xhigh`) — custom
    /// spellings echo back exactly as they were set. The klient contract is
    /// a plain string (not nullable), so no thinking configured answers
    /// `"none"`.
    pub(super) async fn dispatch_get_effective_thinking_level(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let ref_params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&ref_params).await?;
        let effort = {
            let config = agent.config();
            let config = config.read().unwrap_or_else(|e| e.into_inner());
            config.thinking_effort.clone()
        };
        // The enum serializes to its rust spellings (the kosong `off`/`on`
        // aliases are input-only); `None` maps to the wire `"none"`.
        let wire = match effort {
            Some(effort) => serde_json::to_value(effort)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "none".to_string()),
            None => "none".to_string(),
        };
        Ok(Value::String(wire))
    }

    /// `agent.setPermission` — update the agent's permission mode at
    /// runtime. The klient contract models the method as `noResult`:
    /// JSON `null`.
    pub(super) async fn dispatch_set_permission(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SetPermissionParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            // Kept for contract compatibility: clients may still send
            // `agentId`, and serde consumes it during deserialization even
            // though the mode write is session-wide (SDR-004 d) and never
            // addresses a single agent.
            #[serde(default, alias = "agentId")]
            #[expect(dead_code)]
            agent_id: Option<String>,
            mode: PermissionMode,
        }
        let params: SetPermissionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        // Write-before-mutate: persist the permission.set_mode op before the
        // in-memory mode change, so a crash between the two leaves the wire
        // log ahead of memory (resume replays the wire log as the source of
        // truth). The mode serializes to the same lowercase wire spelling
        // ("manual" | "auto" | "yolo") the reducer parses back.
        let mode = serde_json::to_value(params.mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "manual".to_string());
        // Same-value no-op: a client re-pushing the mode the session
        // already runs at (the web client's post-create profile sync does
        // exactly that, right after the create-time seed journaled the
        // same mode) is not a state change — journaling it would duplicate
        // the `permission.set_mode` op in the wire log. Only a genuine
        // mode transition records. The main agent is the mode's
        // session-wide holder (SDR-004 d); if it is somehow missing, fall
        // back to recording (the wire log is the safer place to err).
        let mode_changed = session
            .get_agent(&AgentId::new(DEFAULT_MAIN_AGENT_ID))
            .await
            .is_none_or(|agent| agent.permission_manager().mode() != params.mode);
        if mode_changed {
            session.record_op(WireOp::PermissionSetMode { mode }).await;
        }
        // SDR-004 d: the mode is session-wide — broadcast to every live agent
        // (main + existing subagents + swarm members). The RPC's optional
        // agent_id stays accepted for contract compatibility but no longer
        // scopes the write: subagents have no independent permission mode.
        // The broadcast runs unconditionally (a same-value push still
        // re-syncs any straggler agent) — only the journal is gated.
        session.set_permission_mode(params.mode).await;
        Ok(Value::Null)
    }

    /// `agent.getPermission` — the addressed agent's permission mode (and an
    /// empty rule list; the embedded surface exposes no rule editing).
    pub(super) async fn dispatch_get_permission(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        to_result(GetPermissionResult {
            mode: serde_json::to_value(agent.permission_manager().mode())
                .unwrap_or_else(|_| Value::String("manual".to_string())),
            rules: Vec::new(),
        })
    }

    /// `agent.getConfig` — the addressed agent's resolved configuration
    /// (model, thinking effort, system prompt, permission mode, cwd).
    pub(super) async fn dispatch_get_config(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let config_arc = agent.config();
        let config = config_arc.read().unwrap_or_else(|e| e.into_inner());
        to_result(GetConfigResult {
            cwd: self
                .manager
                .workspace()
                .cwd()
                .to_string_lossy()
                .into_owned(),
            model: config.model_id.clone(),
            model_alias: config.model_id.clone(),
            thinking_effort: serde_json::to_value(config.thinking_effort.clone())
                .unwrap_or(Value::Null),
            system_prompt: config.system_prompt.clone(),
            permission_mode: serde_json::to_value(config.permission_mode).unwrap_or(Value::Null),
            max_turns: config.max_turns,
        })
    }

    /// `agent.setActiveTools` — restrict the addressed agent's tool set to
    /// the given names (the engine's `ToolManager` enforces the restriction
    /// on every subsequent turn).
    pub(super) async fn dispatch_set_active_tools(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SetActiveToolsParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            #[serde(default)]
            names: Vec<String>,
        }
        let params: SetActiveToolsParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: params.agent_id,
            })
            .await?;
        // Write-before-mutate: persist the tools.set_active_tools op before
        // the in-memory restriction, so a crash between the two leaves the
        // wire log ahead of memory (resume replays the restricted set into
        // the recreated agent's tool manager).
        session
            .record_op(WireOp::ToolsSetActiveTools {
                names: params.names.clone(),
            })
            .await;
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;
        agent
            .tool_manager()
            .write()
            .set_active_tools(params.names.clone());
        to_result(SetActiveToolsResult {
            ok: true,
            names: params.names,
        })
    }

    /// `agent.registerTool` — acknowledged: the engine has no executable
    /// user-tool type that could be built from the JSON description.
    pub(super) async fn dispatch_register_tool(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RegisterToolParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            name: String,
            description: String,
            #[serde(default)]
            parameters: Value,
        }
        let params: RegisterToolParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id,
                agent_id: params.agent_id,
            })
            .await?;

        // The session's reverse-RPC slot. RpcTool resolves the CURRENT
        // callback at call time — the same late-subscribe semantics as
        // AskUserQuestion — so the registration only requires a registered
        // engine-level callback NOW (the client-side availability gate),
        // and the tool keeps observing later serve-transport installs.
        let engine_callback = self
            .reverse_rpc_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                EngineError::Internal(
                    "registerTool requires a reverse-RPC callback to be registered on the session"
                        .to_string(),
                )
            })?;
        // The serve transport may install its per-connection callback into the
        // session slot later; seed the engine-level fallback so the tool is
        // callable immediately (a later install overwrites it).
        let callback_slot = session.reverse_rpc_slot();
        *callback_slot.write().unwrap_or_else(|e| e.into_inner()) = Some(engine_callback);

        // Write-before-mutate: persist the tools.register_user_tool op (the
        // klient registration shape) before the in-memory registration, so a
        // resume replays the registration back into the agent's tool manager.
        session
            .record_op(WireOp::ToolsRegisterUserTool {
                registration: json!({
                    "name": params.name,
                    "description": params.description,
                    "parameters": params.parameters,
                }),
            })
            .await;
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;

        let tool = Arc::new(RpcTool::new(
            params.name.clone(),
            params.description,
            params.parameters,
            callback_slot,
        ));
        agent.tool_manager().write().register_user_tool(tool);

        to_result(RegisterToolResult {
            ok: true,
            registered: true,
        })
    }

    /// `agent.unregisterTool` — removes a user-registered tool by name.
    pub(super) async fn dispatch_unregister_tool(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct UnregisterToolParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            name: String,
        }
        let params: UnregisterToolParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id,
                agent_id: params.agent_id,
            })
            .await?;

        // Write-before-mutate: persist the tools.unregister_user_tool op
        // before the in-memory removal, so a resume of the wire log drops the
        // registration instead of replaying it back.
        session
            .record_op(WireOp::ToolsUnregisterUserTool {
                name: params.name.clone(),
            })
            .await;
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;

        let removed = agent.tool_manager().write().remove_user_tool(&params.name);

        to_result(UnregisterToolResult {
            ok: true,
            unregistered: removed,
        })
    }

    /// `tool.listUserTools` — the user-tool management read face: every
    /// registered user tool with its registration payload (name /
    /// description / parameters) and its current LLM visibility. Unlike
    /// `tool.list` (the active table), disabled user tools stay listed —
    /// with `enabled: false`.
    pub(super) async fn dispatch_list_user_tools(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let tools: Vec<Value> = agent
            .tool_manager()
            .read()
            .user_tools_info()
            .into_iter()
            .map(|info| {
                json!({
                    "name": info.name,
                    "description": info.description,
                    "parameters": info.parameters,
                    "enabled": info.enabled,
                })
            })
            .collect();
        serde_json::to_value(tools).map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `tool.setUserToolEnabled` — enable or disable a registered user
    /// tool by name. Enabling adds the name to the agent's active set (a
    /// no-op in the unrestricted default, an explicit widening under a
    /// whitelist); disabling removes it — the tool stays registered (the
    /// removal RPC is `agent.unregisterTool`). An unknown name answers
    /// `registered: false` without touching state or the wire log.
    pub(super) async fn dispatch_set_user_tool_enabled(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(serde::Deserialize)]
        struct SetUserToolEnabledParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            name: String,
            enabled: bool,
        }
        let params: SetUserToolEnabledParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id,
                agent_id: params.agent_id,
            })
            .await?;

        // An unknown name is an honest miss, not an error: no wire op, no
        // mutation (there is nothing to persist).
        let registered = agent
            .tool_manager()
            .read()
            .user_tools_info()
            .iter()
            .any(|info| info.name == params.name);
        if !registered {
            return to_result(SetUserToolEnabledResult {
                ok: true,
                registered: false,
                enabled: params.enabled,
            });
        }

        // Write-before-mutate: persist the tools.set_user_tool_enabled op
        // before the in-memory toggle, so a resume replays the visibility
        // change back into the agent's tool manager.
        session
            .record_op(WireOp::ToolsSetUserToolEnabled {
                name: params.name.clone(),
                enabled: params.enabled,
            })
            .await;
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;

        let applied = agent
            .tool_manager()
            .write()
            .set_user_tool_enabled(&params.name, params.enabled);

        to_result(SetUserToolEnabledResult {
            ok: true,
            registered: applied,
            enabled: params.enabled,
        })
    }

    /// `tool.list` — the addressed agent's registered tool definitions (the
    /// same `{ name, description, parameters, deferred }` shape
    /// `mirri-server`'s `tool.list` returns).
    pub(super) async fn dispatch_get_tools(&self, params: Value) -> Result<Value, EngineError> {
        let params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&params).await?;
        let definitions = agent.tool_manager().read().loop_tools();
        let tools: Vec<Value> = definitions
            .into_iter()
            .map(|definition| {
                json!({
                    "name": definition.name,
                    "description": definition.description,
                    "parameters": definition.parameters,
                    "deferred": definition.deferred,
                })
            })
            .collect();
        Ok(Value::Array(tools))
    }

    /// `task.list` — the session's background tasks with `activeOnly` and
    /// `limit` filters applied (bare array). Each element is the
    /// serialization of `mirri_core::AgentTask` — the same shape
    /// `mirri-server`'s `task.list` returns (TSV row 80) — served through
    /// the summary view (see [`task_rows_value`]) unless `includeFull` opts
    /// into the full serialization (the detail face).
    pub(super) async fn dispatch_get_background(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct GetBackgroundParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "activeOnly")]
            active_only: Option<bool>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default, alias = "includeFull")]
            include_full: Option<bool>,
        }
        let params: GetBackgroundParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let active_only = params.active_only.unwrap_or(false);
        let tasks: Vec<AgentTask> = session
            .tasks()
            .list()
            .into_iter()
            .filter(|task| !active_only || matches!(task.status, TaskStatus::Running))
            .take(params.limit.unwrap_or(usize::MAX))
            .collect();
        task_rows_value(&tasks, params.include_full.unwrap_or(false))
    }

    /// `task.listWorkspace` — the workspace-aggregated task list (B7 D-1 /
    /// P-023): every BUILT workspace runtime's live sessions contribute
    /// their TaskManager's tasks (persisted-only sessions surface after
    /// they are opened — restore reconciles Running→Stopped, so a
    /// not-yet-opened session's tasks are not listed: the honest live
    /// view, not a scan of the disk).
    ///
    /// `workspaceId` scopes to one runtime (alias-folded, the `session.list`
    /// precedent); an unknown id answers the honest empty array — the
    /// overview is a view, not an addressing surface. The default scope
    /// follows the server's listing visibility (serve: every registered
    /// workspace; a single-workspace host: the pinned default).
    ///
    /// Bare array of `AgentTask` serializations served through the SAME
    /// summary view `task.list` uses (see [`task_rows_value`]; `session_id`
    /// carries the attribution; the
    /// web client joins the session title). Sessions order by
    /// `(updated_at DESC, id DESC)` — the merged listing order — and the
    /// tasks within one session by `(created_at ASC, id ASC)`.
    pub(super) async fn dispatch_list_workspace_tasks(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ListWorkspaceParams {
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            #[serde(default, alias = "activeOnly")]
            active_only: Option<bool>,
            #[serde(default)]
            limit: Option<usize>,
            #[serde(default, alias = "includeFull")]
            include_full: Option<bool>,
        }
        let params: ListWorkspaceParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let active_only = params.active_only.unwrap_or(false);

        let workspace_ids: Option<Vec<String>> = match params.workspace_id.as_deref() {
            Some(workspace_id) => Some(self.registry.alias_group_of(workspace_id)),
            None => match self.listing_visibility {
                ListingVisibility::PinnedToDefault => self
                    .registry
                    .default_workspace_id()
                    .map(|default| vec![default]),
                ListingVisibility::AllRegistered => None,
            },
        };

        let mut tasks: Vec<AgentTask> = Vec::new();
        for runtime in self.registry.built_runtimes() {
            if let Some(ids) = &workspace_ids
                && !ids.contains(&runtime.workspace_id)
            {
                continue;
            }
            let mut infos = runtime.manager.list_sessions().await;
            // (updated_at DESC, id DESC) — the merged listing order.
            infos.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            for info in infos {
                let Some(session) = runtime.manager.get_session(&SessionId::new(&info.id)).await
                else {
                    continue;
                };
                let mut session_tasks = session
                    .tasks()
                    .list()
                    .into_iter()
                    .filter(|task| !active_only || matches!(task.status, TaskStatus::Running))
                    .collect::<Vec<_>>();
                // created_at ASC (the registration order), id ASC as the
                // deterministic tiebreak.
                session_tasks.sort_by(|a, b| {
                    a.created_at
                        .cmp(&b.created_at)
                        .then_with(|| a.id.cmp(&b.id))
                });
                tasks.extend(session_tasks);
            }
        }
        if let Some(limit) = params.limit {
            tasks.truncate(limit);
        }
        task_rows_value(&tasks, params.include_full.unwrap_or(false))
    }

    /// `task.cancel` — stop one background task of the session.
    pub(super) async fn dispatch_stop_background(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct StopBackgroundParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "taskId")]
            task_id: String,
            #[serde(default)]
            reason: Option<String>,
        }
        let params: StopBackgroundParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        if session.tasks().get(&params.task_id).is_none() {
            return Err(not_found(format!("task not found: {}", params.task_id)));
        }
        session.tasks().stop(&params.task_id);
        if let Some(reason) = &params.reason {
            tracing::info!(
                task_id = %params.task_id,
                reason = %reason,
                "task stopped via task.cancel"
            );
        }
        ack()
    }

    /// `task.stop` — stop one background task with a reason, returning the
    /// stopped task in the `AgentTask` serialization (the klient
    /// `maybe(agentTaskInfoSchema)` shape — the same element shape
    /// `task.list` serves). The sister operation of `task.cancel` (the
    /// klient facade branches on the presence of a reason); an unknown
    /// taskId errors exactly like `task.cancel` does.
    pub(super) async fn dispatch_stop_task(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct StopTaskParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            // The klient payload spells `taskId` (camelCase); the embedded
            // double-spelling convention also accepts `task_id`.
            #[serde(alias = "taskId")]
            task_id: String,
            #[serde(default)]
            reason: Option<String>,
        }
        let params: StopTaskParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        if session.tasks().get(&params.task_id).is_none() {
            return Err(not_found(format!("task not found: {}", params.task_id)));
        }
        session.tasks().stop(&params.task_id);
        if let Some(reason) = &params.reason {
            tracing::info!(
                task_id = %params.task_id,
                reason = %reason,
                "task stopped via task.stop"
            );
        }
        let stopped = session.tasks().get(&params.task_id);
        serde_json::to_value(stopped).map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `task.output` — the accumulated output preview of one background task.
    pub(super) async fn dispatch_get_background_output(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct GetBackgroundOutputParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "taskId")]
            task_id: String,
            #[serde(default)]
            tail: Option<usize>,
        }
        let params: GetBackgroundOutputParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let output = session
            .tasks()
            .output(&params.task_id)
            .ok_or_else(|| not_found(format!("task not found: {}", params.task_id)))?;
        // An explicit `tail` caps the returned preview to its last
        // `tail` characters (the TS `GetBackgroundOutputPayload.tail`).
        let preview = match params.tail {
            Some(tail) if output.preview.chars().count() > tail => output
                .preview
                .chars()
                .skip(output.preview.chars().count() - tail)
                .collect(),
            _ => output.preview,
        };
        Ok(Value::String(preview))
    }

    /// `cron.list` — the session's cron tasks as `CronTaskSnapshot` wire
    /// entries (timestamps as epoch milliseconds).
    pub(super) async fn dispatch_get_cron_tasks(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let tasks: Vec<Value> = session
            .cron()
            .list()
            .into_iter()
            .map(|task| {
                json!({
                    "id": task.id,
                    "cron": task.cron,
                    "prompt": task.prompt,
                    "recurring": task.recurring,
                    "timezone": task.timezone,
                    "stale": session.cron().is_stale(&task, chrono::Utc::now()),
                    "createdAt": task.created_at.timestamp_millis(),
                    "lastFiredAt": task.last_fired.map(|at| at.timestamp_millis()),
                    "nextFireAt": task.next_fire_at.map(|at| at.timestamp_millis()),
                })
            })
            .collect();
        to_result(CronTasksResult { tasks })
    }

    /// `cron.delete` — remove one of the session's cron tasks by id (B6
    /// management write face). Thin wrapper over `CronManager::remove`: the
    /// durable `cron.delete` wire op lands before the in-memory removal
    /// (write-before-mutate), so the deletion survives resume. Unknown ids
    /// answer not_found (task.cancel parity).
    pub(super) async fn dispatch_delete_cron_task(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CronDeleteParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "taskId")]
            task_id: String,
        }
        let params: CronDeleteParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let deleted = session.cron().remove(&params.task_id).await;
        if !deleted {
            return Err(not_found(format!(
                "cron task not found: {}",
                params.task_id
            )));
        }
        to_result(CronDeleteResult {
            deleted: true,
            task_id: params.task_id,
        })
    }

    // (`cron.trigger`, the run-now sibling of this write face, lives in
    // `cron_driver.rs` next to the inject_cron_prompt pipeline it reuses —
    // the two handlers share only the route table.)

    // --- plan domain (Phase 4.4) ------------------------------------------

    /// `plan.enter` — enter plan mode with the given plan text (empty when
    /// the client sends none).
    pub(super) async fn dispatch_enter_plan(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct EnterPlanParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default)]
            plan: Option<String>,
            #[serde(default, alias = "planText")]
            plan_text: Option<String>,
        }
        let params: EnterPlanParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let plan_text = params.plan.or(params.plan_text).unwrap_or_default();
        let plan = session.plan();
        if !plan.is_active() {
            // Write-before-mutate: persist the plan_mode.enter op before
            // entering in memory, so a crash between the two leaves the wire
            // log ahead of memory (resume treats the wire log as the source
            // of truth). The `id` follows the TS wire contract — a plan
            // identifier, not the plan text. The rust dialect's optional
            // `planText` carries the plan content at entry (F12) so a
            // resume restores the real text instead of the plan id.
            let plan_id = format!("plan-{}", uuid::Uuid::new_v4());
            session
                .record_op(WireOp::PlanModeEnter {
                    id: plan_id.clone(),
                    plan_text: (!plan_text.is_empty()).then(|| plan_text.clone()),
                })
                .await;
            plan.enter(plan_text).map_err(engine_error)?;
            plan.set_plan_id(plan_id);
        } else {
            plan.enter(plan_text).map_err(engine_error)?;
        }
        ack()
    }

    /// `plan.cancel` — cancel plan mode, discarding the plan text.
    pub(super) async fn dispatch_cancel_plan(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        if session.plan().is_active() {
            // Write-before-mutate: persist the plan_mode.cancel op before
            // discarding the in-memory plan state.
            session.record_op(WireOp::PlanModeCancel { id: None }).await;
        }
        session.plan().cancel();
        ack()
    }

    /// `plan.clear` — clear the plan (the embedded `PlanManager` has no
    /// separate clear; cancelling discards the same state).
    pub(super) async fn dispatch_clear_plan(&self, params: Value) -> Result<Value, EngineError> {
        self.dispatch_cancel_plan(params).await
    }

    /// `plan.status` — the session's plan-mode state.
    pub(super) async fn dispatch_get_plan(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let plan = session.plan();
        to_result(PlanStatusResult {
            active: plan.is_active(),
            plan: plan.get_plan(),
            plan_file: Value::Null,
            revisions: Vec::new(),
        })
    }

    // --- goal domain (Phase 4.4) ------------------------------------------

    /// `goal.create` — create the session's single active goal. Returns the
    /// `GoalSnapshot` wire shape.
    pub(super) async fn dispatch_create_goal(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CreateGoalParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default)]
            objective: Option<String>,
            #[serde(default)]
            replace: Option<bool>,
        }
        let params: CreateGoalParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let goal = session.goal();
        if params.replace.unwrap_or(false) && goal.get().is_some() {
            // Write-before-mutate: the goal.clear op lands before the
            // in-memory clear.
            session.record_op(WireOp::GoalClear {}).await;
            goal.clear();
        }
        let objective = params
            .objective
            .ok_or_else(|| invalid_params("createGoal requires an 'objective' string"))?;
        if !params.replace.unwrap_or(false) && goal.get().is_some() {
            // Preserve the GoalManager's rejection (goal.create would fail
            // with the same message) without recording an op for a goal that
            // was never created.
            return Err(engine_error(mirri_core::EngineError::RequestInvalid(
                "a goal already exists; clear it before creating a new one".to_string(),
            )));
        }
        // Write-before-mutate: the goal.create op lands before the in-memory
        // goal is created, so a crash between the two leaves the wire log
        // ahead of memory (resume treats the wire log as the source of
        // truth).
        session
            .record_op(WireOp::GoalCreate {
                goal_id: format!("goal-{}", uuid::Uuid::new_v4()),
                objective: objective.clone(),
                completion_criterion: None,
                status: Some("active".to_string()),
                actor: Some("user".to_string()),
                budget_limits: None,
            })
            .await;
        goal.create(objective, None).map_err(engine_error)?;
        let snapshot = match goal.get() {
            Some(current) => goal_snapshot_json(&current, &goal),
            None => {
                return Err(EngineError::Internal(
                    "goal was created but is not readable".to_string(),
                ));
            }
        };
        // Announce the change on the session's event bus (the
        // `goal.updated` wire event).
        self.publish_session_event(
            &params.session_id,
            SessionEvent::GoalUpdated {
                snapshot: snapshot.clone(),
                change: Some(json!({
                    "kind": "lifecycle",
                    "status": "active",
                    "actor": "user",
                })),
            },
        );
        Ok(snapshot)
    }

    /// `goal.get` — the session's current goal as a `GoalToolResult`
    /// (`{ "goal": <snapshot|null> }`).
    pub(super) async fn dispatch_get_goal(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let goal = session.goal();
        let snapshot = goal
            .get()
            .map(|current| goal_snapshot_json(&current, &goal))
            .unwrap_or(Value::Null);
        to_result(GoalResult { goal: snapshot })
    }

    /// `goal.pause` — pause the current goal; returns its snapshot.
    pub(super) async fn dispatch_pause_goal(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let goal = session.goal();
        if goal.get().is_some() {
            // Write-before-mutate: the goal.update op lands before the
            // in-memory status change.
            session
                .record_op(WireOp::GoalUpdate {
                    goal_id: None,
                    status: Some(goal_status_wire(mirri_core::GoalStatus::Paused).to_string()),
                    reason: None,
                    turns_used: None,
                    tokens_used: None,
                    wall_clock_ms: None,
                    budget_limits: None,
                    actor: Some("user".to_string()),
                })
                .await;
        }
        goal.update(mirri_core::GoalStatus::Paused);
        let result = match goal.get() {
            Some(current) => goal_snapshot_json(&current, &goal),
            None => Value::Null,
        };
        // Announce the change on the session's event bus (the
        // `goal.updated` wire event).
        self.publish_session_event(
            &params.session_id,
            SessionEvent::GoalUpdated {
                snapshot: result.clone(),
                change: Some(json!({
                    "kind": "lifecycle",
                    "status": "paused",
                    "actor": "user",
                })),
            },
        );
        // SDR (goal-stop-freeze): pausing freezes the whole machine —
        // the in-flight turns are cancelled with the same semantics as
        // `message.abort` (the web's turn.ended events clear the working
        // UI), and the notification driver holds deliveries while the
        // goal is paused. Runs after the `goal.updated` publish so the
        // wire reports the pause BEFORE the freeze's turn endings.
        // Infallible from this path: `turn_id: None` can never produce
        // `not_found` (empty + no turn id → idempotent Ok), so the `?`
        // cannot leave a partially-applied goal state here.
        if goal.get().is_some() {
            self.cancel_in_flight_turns(&params.session_id, None, "the user via goal.pause")
                .await?;
        }
        if result.is_null() { ack() } else { Ok(result) }
    }

    /// `goal.resume` — resume the paused goal; returns its snapshot.
    pub(super) async fn dispatch_resume_goal(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let goal = session.goal();
        if goal.get().is_some() {
            // Write-before-mutate: the goal.update op lands before the
            // in-memory status change.
            session
                .record_op(WireOp::GoalUpdate {
                    goal_id: None,
                    status: Some(goal_status_wire(mirri_core::GoalStatus::Active).to_string()),
                    reason: None,
                    turns_used: None,
                    tokens_used: None,
                    wall_clock_ms: None,
                    budget_limits: None,
                    actor: Some("user".to_string()),
                })
                .await;
        }
        goal.update(mirri_core::GoalStatus::Active);
        let result = match goal.get() {
            Some(current) => goal_snapshot_json(&current, &goal),
            None => Value::Null,
        };
        // Announce the change on the session's event bus (the
        // `goal.updated` wire event).
        self.publish_session_event(
            &params.session_id,
            SessionEvent::GoalUpdated {
                snapshot: result.clone(),
                change: Some(json!({
                    "kind": "lifecycle",
                    "status": "active",
                    "actor": "user",
                })),
            },
        );
        if result.is_null() { ack() } else { Ok(result) }
    }

    /// `goal.cancel` — clear the current goal; returns `{ "ok": true }`
    /// when there was nothing to clear.
    pub(super) async fn dispatch_cancel_goal(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let goal = session.goal();
        let had_goal = goal.get().is_some();
        if had_goal {
            // Write-before-mutate: the goal.clear op lands before the
            // in-memory clear.
            session.record_op(WireOp::GoalClear {}).await;
        }
        goal.clear();
        // Announce the clear on the session's event bus (the
        // `goal.updated` wire event — the null snapshot says the goal is
        // gone).
        self.publish_session_event(
            &params.session_id,
            SessionEvent::GoalUpdated {
                snapshot: Value::Null,
                change: Some(json!({
                    "kind": "lifecycle",
                    "reason": "cancelled",
                    "actor": "user",
                })),
            },
        );
        if had_goal {
            // The cancelled-goal reminder tells the main agent to drop the
            // earlier active-goal reminders (TS `goal.cancel`'s injected
            // notice). Follows the goal.clear + event publish, so the wire
            // reports the clear before the reminder; the reminder's own
            // durable `context.append_message` op lands here too (ADR-009
            // replay fidelity — the swarm enter/exit reminder pattern).
            let main_agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
                not_found(format!(
                    "session '{}' has no agents yet; goal cancel needs a main agent to remind",
                    params.session_id
                ))
            })?;
            // The guard never crosses an await: the append completes and
            // the guard drops before the durable record.
            main_agent
                .context_memory()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_system_reminder(mirri_core::goal::GOAL_CANCELLED_REMINDER.to_string());
            session
                .record_op(mirri_core::system_reminder_wire::system_reminder_op(
                    mirri_core::goal::GOAL_CANCELLED_REMINDER,
                ))
                .await;
        }
        // SDR (goal-stop-freeze): cancelling freezes the machine too —
        // whatever is still running is cancelled (cancel means clear, so
        // this runs even when no goal exists). Unlike pause, there is no
        // resume: the freeze is permanent for the goal's turns. Runs as
        // the handler's LAST await, after the `goal.updated` publish, so
        // the wire reports the clear BEFORE the freeze's turn endings
        // (response-scanning clients would discard an event relayed ahead
        // of the response — see the kernel's publish/ack invariant).
        // Infallible from this path: `turn_id: None` can never produce
        // `not_found`, so the `?` cannot leave a partially-applied goal
        // state here.
        self.cancel_in_flight_turns(&params.session_id, None, "the user via goal.cancel")
            .await?;
        match goal.get() {
            Some(current) => Ok(goal_snapshot_json(&current, &goal)),
            None => ack(),
        }
    }

    // --- swarm domain (Phase 4.4) -----------------------------------------

    /// `swarm.enter` — enter swarm mode for the session and inject the
    /// swarm-mode reminder into the main agent's context. The wire method
    /// carries no trigger payload today, so the host-side enter is `manual`
    /// by definition. Idempotent (TS parity: a repeated enter is a no-op —
    /// no second op, no second reminder). Write-before-mutate: the durable
    /// `swarm_mode.enter` op lands before the in-memory enter; the reminder
    /// append follows its own durable `context.append_message` op
    /// (ADR-009 replay fidelity).
    pub(super) async fn dispatch_enter_swarm(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // A repeated enter is a no-op (TS `enter`'s active guard). The
        // guard is dropped before the first await.
        if session.swarm_mode().is_active() {
            return ack();
        }

        // Write-before-mutate (the goal.clear pattern): the op lands
        // before the in-memory enter.
        session
            .record_op(WireOp::SwarmModeEnter {
                trigger: json!("manual"),
            })
            .await;
        let inject = {
            let mut swarm = session.swarm_mode();
            swarm.enter(SwarmModeTrigger::Manual);
            // Manual != Tool, so the reminder is pending after the enter —
            // mark it injected together with the decision (TS `enter`
            // appends the reminder for every non-tool trigger).
            let inject = swarm.should_inject_reminder();
            if inject {
                swarm.mark_reminder_injected();
            }
            inject
        };
        if inject {
            let main_agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
                not_found(format!(
                    "session '{}' has no agents yet; swarm mode needs a main agent to remind",
                    params.session_id
                ))
            })?;
            main_agent
                .context_memory()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_system_reminder(mirri_core::swarm::SWARM_ENTER_REMINDER.to_string());
            session
                .record_op(mirri_core::system_reminder_wire::system_reminder_op(
                    mirri_core::swarm::SWARM_ENTER_REMINDER,
                ))
                .await;
        }
        ack()
    }

    /// `swarm.exit` — exit swarm mode for the session and retract the enter
    /// reminder when it is still the newest context message (TS
    /// `popSwarmModeReminder`); otherwise append the exit reminder. The
    /// retraction is the wire-replayed counterpart of the reducer's
    /// `swarm_mode.exit` fold, so both sides gate on the same predicate
    /// ([`mirri_proto::is_swarm_enter_reminder_text`]) and the live memory
    /// matches the replay. Idempotent (TS parity: exiting an inactive mode
    /// is a no-op).
    pub(super) async fn dispatch_exit_swarm(&self, params: Value) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // Exiting an inactive mode is a no-op (TS `exit`'s trigger guard).
        if !session.swarm_mode().is_active() {
            return ack();
        }

        // Write-before-mutate: the exit op lands before the in-memory exit
        // and before the reminder bookkeeping, so the reducer folds the
        // retraction exactly where the live side performs it.
        session.record_op(WireOp::SwarmModeExit {}).await;
        session.swarm_mode().exit();
        let main_agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
            not_found(format!(
                "session '{}' has no agents yet; swarm mode needs a main agent to remind",
                params.session_id
            ))
        })?;
        let memory = main_agent.context_memory();
        // The guard never crosses an await: the pop/append decision runs
        // in a block that ends before the durable record.
        let append_exit_reminder = {
            let mut guard = memory.lock().unwrap_or_else(|e| e.into_inner());
            let will_pop = guard.history().last().is_some_and(|m| {
                m.origin == Some(mirri_core::origin::PromptOrigin::Injection)
                    && mirri_proto::is_swarm_enter_reminder_text(&m.message.text_content())
            });
            if will_pop {
                // The enter reminder never reached the conversation as a
                // message between agents — pop it, and add nothing (TS
                // `popSwarmModeReminder`). The reducer's `swarm_mode.exit`
                // fold performs the same pop on replay, so no extra op is
                // recorded for the retraction.
                guard.pop_last_message();
                false
            } else {
                guard.append_system_reminder(mirri_core::swarm::SWARM_EXIT_REMINDER.to_string());
                true
            }
        };
        if append_exit_reminder {
            session
                .record_op(mirri_core::system_reminder_wire::system_reminder_op(
                    mirri_core::swarm::SWARM_EXIT_REMINDER,
                ))
                .await;
        }
        ack()
    }

    /// `swarm.get` — whether swarm mode is active (a plain boolean, matching
    /// the TS `getSwarmMode` wire type).
    pub(super) async fn dispatch_get_swarm_mode(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        Ok(Value::Bool(session.swarm_mode().is_active()))
    }

    // --- collaboration domain (mode gate for the Collaboration* tools) ---

    /// `collaboration.enter` — enter collaboration mode for the session and
    /// expose the 7 `Collaboration*` builtin tools to the LLM. Idempotent:
    /// a repeated enter keeps the flag set and the tools visible.
    pub(super) async fn dispatch_enter_collaboration(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: None,
            })
            .await?;
        session.collaboration_mode().enter();
        set_collaboration_tools_enabled(&agent, true);
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;
        ack()
    }

    /// `collaboration.exit` — exit collaboration mode for the session and
    /// hide the `Collaboration*` builtin tools from the LLM again.
    /// Idempotent: a repeated exit keeps the flag cleared and the tools
    /// hidden.
    pub(super) async fn dispatch_exit_collaboration(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = self
            .resolve_agent(&AgentRefParams {
                session_id: params.session_id.clone(),
                agent_id: None,
            })
            .await?;
        session.collaboration_mode().exit();
        set_collaboration_tools_enabled(&agent, false);
        // The tool table is part of the request envelope: the change is the
        // explicit prefix-drift marker (P8a-4).
        session
            .record_op(WireOp::RequestHeaderChanged {
                reason: "tools_changed".to_string(),
            })
            .await;
        ack()
    }

    // --- skill domain (Phase 4.4) -----------------------------------------

    /// `skill.list` — the workspace skill catalog (both the session-level
    /// `listSkills` and workspace-level `listWorkspaceSkills` wire names), in
    /// the TS `SkillSummary[]` wire shape (bare array). Plugin-provided
    /// skills surface as the TS `"extra"` source scope.
    ///
    /// Design 13 §3.6 / R5 — the only wire increment: the optional
    /// `disable_model_invocation` flag, emitted ONLY when set, byte-aligned
    /// with the TS REST `toProtocolSkill` (both the v1
    /// `agent-core/src/services/skill/skill.ts` and the v2
    /// `kap-server/src/routes/skills.ts` projections emit the snake_case
    /// key exactly when the flag is present). `is_sub_skill` is
    /// intentionally never emitted — the TS projections drop it, and the
    /// two ends stay aligned.
    pub(super) async fn dispatch_list_skills(&self, params: Value) -> Result<Value, EngineError> {
        // The workspace-scoped face (B8/W3): an optional `workspaceId`
        // routes to that workspace's own runtime catalog, an optional
        // `sessionId` to the session's owning workspace's catalog, and
        // without either the default workspace answers (the session-level
        // legacy contract — every session of the default workspace shares
        // it).
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ListSkillsParams {
            #[serde(default, alias = "workspace_id")]
            workspace_id: Option<String>,
            #[serde(default, alias = "session_id")]
            session_id: Option<String>,
        }
        let params: ListSkillsParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // The routed catalog: an explicit `workspaceId` addresses that
        // workspace's own runtime catalog; else a `sessionId` routes through
        // the live session's owning workspace (the composer `/` menu path —
        // every session shares its workspace's catalog, so a session of a
        // non-default workspace lists THAT workspace's project skills);
        // without either the default workspace answers (the session-level
        // legacy contract).
        let catalog: Arc<mirri_core::SkillCatalog> =
            if let Some(workspace_id) = params.workspace_id.as_deref() {
                // Build the addressed workspace's runtime on demand (the
                // `ensure_built` path the routing seam's bucket probe
                // uses); an unknown id answers the honest error instead of
                // silently falling back to the default workspace.
                let alias_ids = self.registry.alias_group_of(workspace_id);
                let runtime = self
                    .registry
                    .built_runtimes()
                    .into_iter()
                    .find(|runtime| alias_ids.contains(&runtime.workspace_id))
                    .or_else(|| {
                        alias_ids
                            .iter()
                            .filter_map(|id| self.registry.ensure_built(id))
                            .find_map(|result| result.ok())
                    });
                match runtime {
                    Some(runtime) => Arc::clone(runtime.manager.workspace().skills()),
                    // A registered-but-unbuildable row or an unknown id:
                    // report not-found — silently answering with the
                    // default workspace's catalog is the bug this routing
                    // fixes.
                    None => {
                        return Err(not_found(format!("workspace not found: {workspace_id}")));
                    }
                }
            } else if let Some(session_id) = params.session_id.as_deref() {
                // The session-level face (the `skill.getContent` precedent):
                // resolve through the routing seam so the session's OWN
                // workspace catalog answers — `skill.list { sessionId }` used
                // to drop the param and always answer the default
                // workspace's catalog.
                Arc::clone(
                    &self
                        .resolve_session(session_id)
                        .await
                        .map_err(|_| not_found(format!("session not found: {session_id}")))?
                        .skill_catalog(),
                )
            } else {
                Arc::clone(self.manager.workspace().skills())
            };
        let skills = catalog.list();
        let entries: Vec<Value> = skills
            .into_iter()
            .map(|skill| {
                let source = match skill.source {
                    mirri_core::SkillSource::Builtin => "builtin",
                    mirri_core::SkillSource::User => "user",
                    mirri_core::SkillSource::Project => "project",
                    mirri_core::SkillSource::Plugin => "extra",
                };
                // B8 D-32: the user-level enablement flag (additive field —
                // v2 clients without it treat absence as enabled).
                let enabled = !catalog.is_disabled(&skill.name);
                // Read before the projection consumes the definition.
                let disable_model_invocation = skill.disable_model_invocation;
                // ADR-012: the additive plugin id for plugin-contributed
                // skills — `source` stays `extra` (stable four-value
                // contract); absent for non-plugin skills and plugin skills
                // without context (frontend degrades to the generic 「额外」
                // badge).
                let plugin_id = skill.plugin.as_ref().map(|ctx| ctx.id.clone());
                // R1 (SDR-005 US3.6 path 口径): the TS v2 semantics — `path`
                // is the skill's SKILL.md FILE path (the message-inline
                // reference resolution authority, ADR-004), and `dir` the
                // containing directory (the `SkillDefinition.path/dir` pair
                // in `agent-core-v2/app/skillCatalog/types.ts`). Web
                // `findInvalidInlineReferences` / `useSlashMenu` /
                // `useSkillTriggerMenu` match the SKILL.md path for chip
                // insertion and pre-send validation; the legacy clients that
                // treated the old directory-as-path like a dir read it off
                // `dir` now. The builtin pseudo-path (`builtin://`) carries
                // no real file — keep it verbatim as both fields.
                let dir = skill.path.to_string_lossy().into_owned();
                let path = if skill.source == mirri_core::SkillSource::Builtin {
                    dir.clone()
                } else {
                    format!("{}/{}", dir, mirri_core::SKILL_MD_FILE)
                };
                let mut entry = json!({
                    "name": skill.name,
                    "description": skill.description,
                    "source": source,
                    "path": path,
                    "dir": dir,
                    "enabled": enabled,
                });
                if disable_model_invocation {
                    entry["disable_model_invocation"] = json!(true);
                }
                if let Some(plugin_id) = plugin_id {
                    entry["plugin"] = json!(plugin_id);
                }
                entry
            })
            .collect();
        Ok(Value::Array(entries))
    }

    /// `skill.getContent` — the skill's SKILL.md content for the web
    /// management view (B8). Workspace-scoped like `skill.list`: an optional
    /// `sessionId` routes to that session's (shared) catalog, else the
    /// default workspace catalog answers.
    pub(super) async fn dispatch_get_skill_content(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct GetSkillContentParams {
            name: String,
            #[serde(default, alias = "sessionId")]
            session_id: Option<String>,
        }
        let params: GetSkillContentParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let catalog = match &params.session_id {
            Some(session_id) => Arc::clone(
                &self
                    .resolve_session(session_id)
                    .await
                    .map_err(|_| not_found(format!("session not found: {session_id}")))?
                    .skill_catalog(),
            ),
            None => Arc::clone(self.manager.workspace().skills()),
        };
        // 已注册但 SKILL.md 不可读（IO 失败）也按 not_found 回答——与
        // `agent.activateSkill` 的读取路径同一诚实边界（IO 错误不外泄细节）。
        let content = catalog.read_content(&params.name).ok_or_else(|| {
            not_found_domain(
                "skill.not_found",
                format!("skill not found: {}", params.name),
            )
        })?;
        Ok(serde_json::to_value(SkillGetContentResult {
            name: params.name,
            content,
        })
        .expect("the content result is always serializable"))
    }

    /// `skill.setEnabled` — user-level per-skill enablement (B8). Persists the
    /// top-level `disabled_skills` config key through the standard config
    /// write pipeline (`replace_sections_raw`), so the config-write resync
    /// hook applies the new set to every built workspace runtime; the config
    /// write is skipped (registered:false) when the name is not in any
    /// built catalog.
    pub(super) async fn dispatch_set_skill_enabled(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct SetSkillEnabledParams {
            name: String,
            enabled: bool,
        }
        let params: SetSkillEnabledParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // registered 判定：任一 built runtime 的 catalog 命中即算（含默认）。
        let registered = self
            .workspace_registry()
            .built_runtimes()
            .iter()
            .any(|rt| rt.scope.skills().get(&params.name).is_some());
        let current: Vec<String> = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .disabled_skills
            .clone()
            .unwrap_or_default();
        if !registered {
            return Ok(serde_json::to_value(SkillSetEnabledResult {
                name: params.name,
                enabled: params.enabled,
                registered: false,
                disabled_skills: current,
            })
            .expect("the result is always serializable"));
        }
        // Read-compute-write INSIDE the standard pipeline's mutate closure:
        // the set is computed against the freshly loaded (without-env)
        // document, so a concurrent toggle or an external config.toml edit
        // cannot be lost to a whole-key overwrite (the closure's config is
        // the mutation base the pipeline persists).
        let mut next = current.clone();
        self.mutate_config_raw(|config| {
            let base = config.disabled_skills.clone().unwrap_or_default();
            let mut computed = base.clone();
            if params.enabled {
                computed.retain(|name| name != &params.name);
            } else if !computed.iter().any(|name| name == &params.name) {
                computed.push(params.name.clone());
            }
            // Idempotent writes short-circuit: a toggle landing on the
            // already-persisted set skips the disk round-trip.
            if computed != base {
                config
                    .replace_raw_sections(&[(
                        "disabled_skills".to_string(),
                        Some(serde_json::json!(computed)),
                    )])
                    .map_err(engine_error)?;
            }
            next = computed;
            Ok(())
        })?;
        Ok(serde_json::to_value(SkillSetEnabledResult {
            name: params.name,
            enabled: params.enabled,
            registered: true,
            disabled_skills: next,
        })
        .expect("the result is always serializable"))
    }

    /// `skill.reload` — the manual skill-catalog reload face (ADR-010 Task
    /// 6): rescan every built workspace runtime through the serialized
    /// [`SkillReloadOrchestrator`](crate::skill_reload::SkillReloadOrchestrator)
    /// (Manual trigger) and answer the per-workspace diff counts plus each
    /// workspace's watcher status (R4). The sweep also refreshes the lazy
    /// watcher mounts first (the same lazy-attach pass every dispatcher
    /// call runs, so a just-flipped flag or a recovered skill root mounts
    /// on the next reload). Registered-but-unbuilt runtimes pick the new
    /// catalog up when they are first built.
    pub(super) async fn dispatch_skill_reload(&self, params: Value) -> Result<Value, EngineError> {
        // Accept both the empty payload and an explicit {}-shaped object;
        // any other shape is rejected (the method has no parameters).
        if !params.is_null() && !params.as_object().is_some_and(|obj| obj.is_empty()) {
            return Err(invalid_params("skill.reload takes no parameters"));
        }
        // Refresh the lazy watcher mounts (flag flip / recovered roots
        // take effect on the manual reload too — the same pass every
        // dispatcher call runs).
        self.spawn_skill_watchers();
        self.await_skill_watcher_mounts().await;
        let config = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Snapshot the watching set (m3: the std lock must not be held
        // across the multi-workspace reload sweep — the sweep waits on the
        // orchestrator's global lock and joins marking threads, and every
        // inbound RPC's lazy mount pass needs this same map).
        let watching: std::collections::HashSet<String> = self
            .skill_watchers_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(id, watcher)| watcher.as_ref().map(|_| id.clone()))
            .collect();
        let mut workspaces = Vec::new();
        for runtime in self.workspace_registry().built_runtimes() {
            let ctx = crate::skill_reload::SkillReloadContext {
                config: &config,
                home_dir: self.home_dir.as_deref(),
                plugin_manager: self.plugin_manager.as_deref(),
                skill_dirs: &self.skill_dirs,
            };
            let outcome = self.skill_reload.reload_workspace(
                &runtime,
                &ctx,
                crate::skill_reload::SkillReloadTrigger::Manual,
            );
            workspaces.push(SkillReloadWorkspaceResult {
                workspace_id: runtime.workspace_id.clone(),
                added: outcome.diff.added.len(),
                removed: outcome.diff.removed.len(),
                updated: outcome.diff.updated.len(),
                watching: watching.contains(&runtime.workspace_id),
            });
        }
        Ok(serde_json::to_value(SkillReloadResult { workspaces })
            .expect("the result is always serializable"))
    }

    /// `agent.activateSkill` — drive a persisted skill-activation turn:
    /// the user message carries the rendered user-slash skill prompt with
    /// the `skill_activation` origin, the `skill.activate` fact lands on
    /// the wire log, and the turn runs like any fire-mode prompt so resume
    /// replays the message and clients see the turn lifecycle events.
    pub(super) async fn dispatch_activate_skill(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct ActivateSkillParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            name: String,
            #[serde(default)]
            args: Option<String>,
        }
        let params: ActivateSkillParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let skill = session
            .skill_catalog()
            .resolve(&params.name)
            .ok_or_else(|| not_found(format!("skill not found: {}", params.name)))?;
        // B8 D-32: user-disabled skills cannot be activated — the slash
        // command face honors the user-level enablement state. The gate is
        // evaluated AFTER resolution, so a plugin-qualified reference to a
        // user-disabled skill is a disabled error, not a not-found — but
        // the error message echoes the ORIGINAL input name verbatim.
        if session.skill_catalog().is_disabled(&skill.name) {
            return Err(invalid_params(format!(
                "skill '{}' is disabled by the user",
                params.name
            )));
        }
        // The v2 kernel contract (agent-core-v2 skillService.enqueueDeferred):
        // the activation is a user-slash TURN — the user message carries
        // the rendered skill prompt with the `skill_activation` origin
        // (identity payload included), the `skill.activate` fact lands on
        // the wire log, and the turn drives like any prompt so resume
        // replays the message and clients see turn lifecycle events.
        let skill_args = params.args.clone().unwrap_or_default();
        let activation_id = format!("act-{}", uuid::Uuid::new_v4());
        let skill_path = skill.path.display().to_string();
        let skill_source = match skill.source {
            mirri_core::SkillSource::Builtin => "builtin",
            mirri_core::SkillSource::User => "user",
            mirri_core::SkillSource::Project => "project",
            mirri_core::SkillSource::Plugin => "extra",
        }
        .to_string();
        let origin = PromptOrigin::SkillActivation {
            activation_id: Some(activation_id.clone()),
            skill_name: Some(skill.name.clone()),
            trigger: Some("user-slash".to_string()),
            skill_args: Some(skill_args.clone()),
            skill_path: Some(skill_path.clone()),
            skill_source: Some(skill_source.clone()),
        };
        let origin_wire =
            serde_json::to_value(&origin).unwrap_or_else(|_| json!({ "kind": "skill_activation" }));
        // Rendered by the formal name the resolution produced (a
        // plugin-qualified reference is not itself a catalog key); the
        // unreadable-content error still echoes the original input name.
        let prompt_text = session
            .skill_catalog()
            .render_user_slash_skill_prompt(&skill.name, &skill_args)
            .ok_or_else(|| not_found(format!("skill content unreadable: {}", params.name)))?;
        let agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
            not_found(format!("session '{}' has no agents yet", params.session_id))
        })?;
        // The skill-activation fact and its turn must be an atomic pair: the
        // provider is validated BEFORE the fact record — a missing or broken
        // provider config must never leave a fact with no turn (no
        // `skill.activate` without its `turn.prompt` /
        // `context.append_message`, no send-path `last_prompt` stamp or
        // session materialization either).
        self.resolve_turn_llm(&agent)?;
        // The fact record first (v2's dequeue-time recordActivation: fact
        // then event then turn), then the `skill.activated` announcement
        // with the SAME activation id the origin payload carries.
        session
            .record_op(WireOp::SkillActivate {
                origin: origin_wire.clone(),
            })
            .await;
        self.publish_session_event(
            &params.session_id,
            SessionEvent::SkillActivated {
                activation_id,
                skill_name: skill.name.clone(),
                args: params.args.clone(),
                trigger: "user-slash".to_string(),
                skill_path: Some(skill_path),
                skill_source: Some(skill_source),
            },
        );
        // Fire mode: the RPC resolves once the turn launches (v2's
        // `activate` awaits `handle.launched`, not completion); progress
        // streams to subscribers as turn events. The display override keeps
        // lastPrompt / auto-title at the short slash form while the LLM
        // prompt carries the full rendered body.
        let display_text = format!(
            "/{}{}",
            skill.name,
            if skill_args.is_empty() {
                String::new()
            } else {
                format!(" {skill_args}")
            }
        );
        self.send_message_with_origin(
            json!({
                "session_id": params.session_id,
                "agentId": agent.id().as_str(),
                "text": prompt_text,
                "mode": "fire",
            }),
            origin,
            origin_wire,
            Some(display_text),
        )
        .await?;
        to_result(ActivateSkillResult {
            ok: true,
            activated: true,
            skill: skill.name.clone(),
        })
    }

    // --- core info / misc (Phase 4.4) -------------------------------------

    /// `core.info` — the engine's identity: version and engine name.
    pub(super) async fn dispatch_get_core_info(&self) -> Result<Value, EngineError> {
        to_result(CoreInfoResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
            engine: "rust".to_string(),
        })
    }

    /// `config.get` — the current configuration as the MirriConfig wire
    /// shape (camelCase, every field present — null when unset).
    /// `version`/`serverId`/`engine`/`startedAt` live in `core.info`, not
    /// here.
    pub(super) async fn dispatch_get_mirri_config(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        // Redact api keys before exposing config to clients.
        Ok(redact_config_wire(config.to_mirri_config_wire()))
    }

    /// `config.set` — deep-merge a MirriConfig wire patch into the persisted
    /// config.toml, reload, and return the updated wire shape.
    ///
    /// Mirrors TypeScript `setMirriConfig`: the merge base is re-read from
    /// disk **without** env overrides (never the in-memory config, so
    /// env-injected values such as an injected `MIRRI_API_KEY` are not baked
    /// into config.toml); the merged config is written atomically (temp +
    /// fsync + rename, mode 0o600) and then reloaded **with** env overrides
    /// into the in-memory snapshot.
    pub(super) async fn dispatch_set_mirri_config(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("config.set requires a config file path; the server has none")
        })?;

        // B8: the pre-write IN-MEMORY snapshot (env-applied) for the
        // skill-derived-key resync hook — same plane as the post-write
        // comparison, so `${VAR}` entries cannot force a spurious resync.
        let before = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Merge base: the persisted config exactly as on disk, without env
        // overrides.
        let mut merged = ServerConfig::load_without_env(&config_path, &mirri_os::LocalSyncFs)
            .map_err(engine_error)?;
        // Deep-merge the camelCase wire patch (nulls stripped, objects merge
        // recursively, scalars/arrays replace).
        merged
            .merge_wire_patch(&normalize_params(params))
            .map_err(engine_error)?;
        // Persist atomically, then reload with env overrides into memory.
        merged.save_to(&config_path).map_err(engine_error)?;
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        let permission_changed = before.permission != reloaded.permission;
        // The `[experimental]` flag layer follows the config (TS parity: the
        // TS `FlagService` watches config changes) — extracted before the
        // config snapshot is moved into the in-memory slot.
        let experimental_overrides = reloaded.experimental.clone().unwrap_or_default();
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;
        self.flag_registry
            .set_config_overrides(experimental_overrides);
        self.resync_skill_catalogs_if_changed(&before);
        // D-34: a patch that touched the `[permission]` rules must reach the
        // live agents too — embedded hosts run no config watcher, so the
        // write pipeline itself pushes (same call the CRUD handlers and the
        // CLI hot-reload land on; idempotent for unchanged rules).
        if permission_changed {
            let rules = {
                let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
                snapshot
                    .permission
                    .as_ref()
                    .and_then(|permission| permission.rules.clone())
                    .unwrap_or_default()
            };
            self.apply_user_permission_rules(rules).await;
        }

        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(redact_config_wire(config.to_mirri_config_wire()))
    }

    /// `config.removeProvider` — delete a provider and every model alias
    /// pointing at it, then reload, mirroring TypeScript
    /// `removeMirriProvider`. The `defaultProvider`/`defaultModel` pointers
    /// are cleared when the removed provider/alias was the default.
    pub(super) async fn dispatch_remove_provider(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let provider_id = params
            .get("providerId")
            .and_then(Value::as_str)
            .or_else(|| params.get("provider_id").and_then(Value::as_str))
            .ok_or_else(|| invalid_params("removeProvider requires 'providerId'"))?
            .to_string();
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("removeProvider requires a config file path; the server has none")
        })?;

        let mut config = ServerConfig::load_without_env(&config_path, &mirri_os::LocalSyncFs)
            .map_err(engine_error)?;

        // Delete the provider itself.
        if let Some(providers) = config.providers.as_mut() {
            providers.remove(&provider_id);
        }

        // Cascade: delete every model alias whose `provider` field points at
        // the removed provider. Remember whether the default model alias was
        // among the deleted ones (matching TS, which only clears
        // `defaultModel` when its alias was actually deleted here).
        let mut removed_default_model = false;
        if let Some(models) = config.models.as_mut() {
            models.retain(|alias_id, alias| {
                let keep = alias.provider != provider_id;
                if !keep && config.default_model.as_deref() == Some(alias_id.as_str()) {
                    removed_default_model = true;
                }
                keep
            });
        }
        if removed_default_model {
            config.default_model = None;
        }
        if config.default_provider.as_deref() == Some(provider_id.as_str()) {
            config.default_provider = None;
        }

        config.save_to(&config_path).map_err(engine_error)?;
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;

        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(redact_config_wire(config.to_mirri_config_wire()))
    }

    /// `config.removeModel` — delete a model alias and record its underlying
    /// model id on the owning provider's `removed_model_ids` denylist (so a
    /// catalog refresh does not silently re-append it), then reload.
    /// Mirrors TypeScript `removeMirriModel`; the denylist append is skipped
    /// silently when the alias's provider no longer exists.
    pub(super) async fn dispatch_remove_model(&self, params: Value) -> Result<Value, EngineError> {
        let model_id = params
            .get("modelId")
            .and_then(Value::as_str)
            .or_else(|| params.get("model_id").and_then(Value::as_str))
            .ok_or_else(|| invalid_params("removeModel requires 'modelId'"))?
            .to_string();
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("removeModel requires a config file path; the server has none")
        })?;

        let mut config = ServerConfig::load_without_env(&config_path, &mirri_os::LocalSyncFs)
            .map_err(engine_error)?;

        // The alias must exist (matching TS: `model <id> does not exist`).
        let alias = config
            .models
            .as_ref()
            .and_then(|models| models.get(&model_id))
            .cloned()
            .ok_or_else(|| invalid_params(format!("model '{model_id}' does not exist")))?;
        if let Some(models) = config.models.as_mut() {
            models.remove(&model_id);
        }

        // Denylist the deleted alias's underlying model id on its provider
        // (deduped); silently skipped when the provider no longer exists.
        if let Some(providers) = config.providers.as_mut()
            && let Some(provider) = providers.get_mut(&alias.provider)
        {
            let removed_ids = provider.removed_model_ids.get_or_insert_with(Vec::new);
            if !removed_ids.contains(&alias.model) {
                removed_ids.push(alias.model.clone());
            }
        }

        if config.default_model.as_deref() == Some(model_id.as_str()) {
            config.default_model = None;
        }

        config.save_to(&config_path).map_err(engine_error)?;
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;

        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(redact_config_wire(config.to_mirri_config_wire()))
    }

    /// `session.addAdditionalDir` — resolve (and optionally persist) a
    /// workspace additional directory, the TS kernel's
    /// `appendWorkspaceAdditionalDir` semantics: find the project root by
    /// walking up for a `.git` marker, read
    /// `<projectRoot>/.mirri-code/local.toml`'s `workspace.additional_dir`
    /// list, resolve the new dir against the session cwd (absolute paths
    /// pass through), assert it exists, and — when `persist` — append it to
    /// the local.toml so a later session on the same workspace still sees
    /// the full list. The main agent also receives the added-dir context
    /// notification (the TS `notifyAdditionalDirAdded`).
    pub(super) async fn dispatch_add_additional_dir(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct AddAdditionalDirParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            path: String,
            #[serde(default = "default_true")]
            persist: bool,
        }
        fn default_true() -> bool {
            true
        }
        let params: AddAdditionalDirParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // The session must exist (the TS `requireSession` guard).
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // The resolution base is the session's own workspace root (W3
        // routing — a non-default session anchors its additional dirs on
        // its own workspace), falling back to the engine's bound root like
        // every session-root consumer.
        let cwd = session
            .workspace_root()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.manager.workspace().cwd().to_path_buf());
        let project_root = find_project_root(&cwd);
        let config_path = project_root.join(".mirri-code").join("local.toml");
        let file = read_workspace_local_toml(&config_path)?;
        let file_dirs = resolve_existing_additional_dirs(&project_root, file_dirs_of(&file)?);

        let trimmed = params.path.trim().to_string();
        if trimmed.is_empty() {
            return Err(invalid_params(
                "workspace.additional_dir must exist and be a directory",
            ));
        }
        // The NEW input resolves against the session cwd (the TS
        // `resolveAdditionalDir(kaos, workDir, ...)` base — F-P11a-4);
        // the persisted entries above stay project-root based, matching
        // the TS `resolveExistingAdditionalDirs`.
        let resolved = resolve_additional_dir(&cwd, &trimmed)?;

        let additional_dirs = if file_dirs.iter().any(|dir| paths_equal(dir, &resolved)) {
            // Already persisted — the TS early return keeps the file list
            // unchanged (no duplicate entry).
            file_dirs
        } else if params.persist {
            let mut dirs = file_dirs.clone();
            dirs.push(resolved.clone());
            write_workspace_local_toml(&config_path, &file, &dirs)?;
            dirs
        } else {
            let mut dirs = file_dirs.clone();
            dirs.push(resolved.clone());
            dirs
        };

        // The main agent learns the new directory is accessible (the TS
        // `notifyAdditionalDirAdded` — requireMainAgent, both persist arms).
        let main_agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
            not_found(format!(
                "session '{}' has no main agent to notify about the added dir",
                params.session_id
            ))
        })?;
        notify_additional_dir_added(
            &session,
            &main_agent.context_memory(),
            &trimmed,
            params.persist,
            &config_path,
        )
        .await;

        // The session's file-tool sandbox widens to the full list — the TS
        // `setAdditionalDirs` re-applies the whole list to every ready
        // agent; the session-shared handle gives the same effect for every
        // agent in the session (F-P11a-3a).
        if let Some(paths) = session.workspace_paths() {
            for dir in &additional_dirs {
                paths.add_additional(dir.clone());
            }
        }

        to_result(AddAdditionalDirResult {
            additional_dirs: additional_dirs
                .iter()
                .map(|dir| dir.to_string_lossy().into_owned())
                .collect(),
            project_root: project_root.to_string_lossy().into_owned(),
            config_path: config_path.to_string_lossy().into_owned(),
            persisted: params.persist,
        })
    }

    /// `shell.run` — run a user-initiated `!` command through the
    /// shell-command runner (the TS `AgentShellCommandService` semantics):
    /// the command and its output land in context memory as
    /// `shell_command`-origin messages (mirrored to the session's wire
    /// log via `context.append_message` records), the run streams
    /// `shell.started` / `shell.output` / `shell.completed` events on the
    /// session's bus when a `commandId` is given, and a run whose
    /// foreground window elapses is detached to a background task —
    /// steering the model with the backgrounded notice through the
    /// existing `message.steer` channel instead of emitting
    /// `shell.completed`.
    pub(super) async fn dispatch_run_shell_command(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct RunShellParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            command: String,
            #[serde(default, alias = "commandId")]
            command_id: Option<String>,
        }
        let params: RunShellParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if params.command.trim().is_empty() {
            return Err(invalid_params("shell.run requires a non-empty 'command'"));
        }

        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        let agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
            not_found(format!(
                "session '{}' has no agents yet; create a session before running shell commands",
                params.session_id
            ))
        })?;
        let artifacts = self
            .resolve_session_artifacts(&params.session_id)
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // The command text enters context memory before the run (the TS
        // `appendShellInput`): a `shell_command`-origin user message,
        // mirrored to the wire log so a resume reconstructs it.
        let memory = agent.context_memory();
        let input_text = format!(
            "<bash-input>\n{}\n</bash-input>",
            escape_shell_xml(&params.command)
        );
        append_shell_context_message(
            &session,
            &memory,
            &input_text,
            json!({ "kind": "shell_command", "phase": "input" }),
            false,
        )
        .await;

        // The backgrounded notice steers the model through the same
        // channel `message.steer` uses — parked in the agent's shared steer
        // buffer and consumed by the next step boundary (the running turn's
        // next request, or the next turn's first step). The handle, not a
        // flow lock: the notice can fire while the same agent's turn still
        // holds the flow's mutex for its whole run.
        let steer_buffer = self.steer_buffer_for(
            &params.session_id,
            agent.id().as_str(),
            &agent
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id,
        );
        let command_for_note = params.command.clone();
        let on_backgrounded = move |task_id: String| {
            let notice = format!(
                "task_id: {task_id}\ndescription: {command_for_note}\nstatus: running\n\
                 note: the shell command exceeded its foreground timeout and was detached to the \
                 background; you will be notified when it completes — do NOT wait or poll on it"
            );
            steer_buffer.lock().unwrap_or_else(|e| e.into_inner()).push(
                vec![mirri_core::ContentPart::Text { text: notice }],
                mirri_core::PromptOrigin::Injection,
            );
        };

        let result = mirri_core::shell_command::run_shell_command(
            mirri_core::shell_command::ShellRunTarget {
                session_id: params.session_id.clone(),
                registry: Arc::clone(&self.shell_commands),
                tasks: session.tasks(),
                event_bus: Arc::clone(&artifacts.event_bus),
                // Zero-IO T13a: the `!` command runs through the injected
                // process provider. The composition root keeps the engine
                // default (the local host process) — AgentScopeConfig's
                // `tool_io` default covers the agent-side tools.
                process: Arc::new(mirri_os::LocalProcess::new()),
            },
            &params.command,
            params.command_id.as_deref(),
            self.shell_foreground_timeout,
            on_backgrounded,
        )
        .await;

        // A detached run's output reports through the task lifecycle —
        // the context message covers settled runs only (the TS service
        // returns from its `backgrounded` branch before
        // `appendShellOutput`).
        if result.backgrounded.is_none() {
            let output_text = format!(
                "<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr>",
                escape_shell_xml(&result.stdout),
                escape_shell_xml(&result.stderr)
            );
            let mut origin = json!({ "kind": "shell_command", "phase": "output" });
            if result.is_error {
                origin["isError"] = json!(true);
            }
            append_shell_context_message(&session, &memory, &output_text, origin, result.is_error)
                .await;
        }

        to_result(ShellRunResult {
            stdout: result.stdout,
            stderr: result.stderr,
            is_error: result.is_error,
            // The wire flag is the boolean presence the TS contract spells
            // (`backgrounded?: boolean`) — the task id rides the
            // `shell.started` event, not the result.
            backgrounded: result.backgrounded.map(|_| true),
        })
    }

    /// `shell.cancel` — cancel a running foreground `!` command by its
    /// command id. An unknown id is a silent no-op (the TS semantics — the
    /// command may have settled between the UI's decision and the call).
    pub(super) async fn dispatch_cancel_shell_command(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CancelShellParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(alias = "commandId")]
            command_id: String,
        }
        let params: CancelShellParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // The session must exist (a consistent 404 surface), but the
        // cancellation itself only keys on the command id.
        self.resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;
        match self.shell_commands.get(&params.command_id) {
            Some(live) => live.cancel_user(),
            None => tracing::debug!(
                command_id = %params.command_id,
                "shell.cancel for an unknown or settled command; no-op"
            ),
        }
        ack()
    }

    /// `task.detach` — convert a running foreground `!` shell command into
    /// a background task (the TS `taskService.detach` semantics, realized
    /// over the shell runner's shared conversion point: the task is
    /// registered under the id the `shell.started` event carried, the
    /// process keeps running, and the caller is steered with the
    /// backgrounded notice).
    ///
    /// B7 D-3: an optional `toolCallId` addresses a running foreground
    /// **model tool call** instead (the Bash tool's tracked foreground
    /// registration; it takes precedence when both keys are given) — the
    /// same conversion through
    /// `mirri_core::TaskManager::detach_foreground_by_tool_call`. Unknown
    /// or settled ids answer `null` (the B7-adjudicated honest no-op —
    /// the TS `detachEntry` answers the task info there; the Rust surface
    /// keeps null so a stale call is visibly a no-op), on either face.
    ///
    /// Answers the converted task's [`AgentTask::background_task_info`]
    /// snapshot.
    pub(super) async fn dispatch_detach_task(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct DetachTaskParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "taskId")]
            task_id: Option<String>,
            // B7: the tool-call addressing face (a foreground model tool
            // call — the model-side call id, not a task id).
            #[serde(default, alias = "toolCallId")]
            tool_call_id: Option<String>,
        }
        let params: DetachTaskParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // The tool-call face first (the B7 model Bash foreground run; it
        // takes precedence when both keys are given); an unknown or settled
        // tool call answers null with no state change.
        if let Some(tool_call_id) = &params.tool_call_id {
            if let Some(task_id) = &params.task_id {
                tracing::debug!(
                    task_id,
                    tool_call_id,
                    "task.detach carrying both keys; toolCallId takes precedence"
                );
            }
            return match session.tasks().detach_foreground_by_tool_call(tool_call_id) {
                Some(task) => Ok(serde_json::to_value(task.background_task_info())
                    .expect("BackgroundTaskInfo is serializable")),
                None => {
                    tracing::debug!(
                        tool_call_id,
                        "task.detach for an unknown or settled tool call; answering null"
                    );
                    Ok(Value::Null)
                }
            };
        }

        // The legacy `!` shell-command face — unchanged (task id
        // addressing through the shell runner's registry).
        let Some(task_id) = params.task_id else {
            tracing::debug!("task.detach without a taskId or toolCallId; answering null");
            return Ok(Value::Null);
        };
        let Some(live) = self.shell_commands.detach_by_task(&task_id) else {
            tracing::debug!(
                task_id,
                "task.detach for an unknown or already-detached task; answering null"
            );
            return Ok(Value::Null);
        };
        match live.convert_to_background(&session.tasks()) {
            Some(task) => Ok(serde_json::to_value(task.background_task_info())
                .expect("BackgroundTaskInfo is serializable")),
            None => Ok(Value::Null),
        }
    }

    /// `btw.start` — fork the session's main agent into a behind-the-wheel
    /// ("BTW") side-channel agent: the fork inherits the main agent's model,
    /// provider, and context history (via [`AgentLifecycle::fork_agent`])
    /// but runs with all tools disabled and a system reminder explaining the
    /// side-channel role, so it can answer side questions without steering
    /// or disturbing the main agent's turn. Returns the new agent's id
    /// (the TS `btwService.startBtw` semantics, ported from mirri-server's
    /// `StartBtwHandler`).
    pub(super) async fn dispatch_start_btw(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct StartBtwParams {
            #[serde(alias = "sessionId")]
            session_id: String,
        }
        let params: StartBtwParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let session = self
            .resolve_session(&params.session_id)
            .await
            .map_err(|_| not_found(format!("session not found: {}", params.session_id)))?;

        // The session's first agent is the main agent — the same addressing
        // rule `message.send` uses when `agentId` is omitted.
        let main_agent = resolve_session_agent(&session, None).await.ok_or_else(|| {
            not_found(format!(
                "session '{}' has no agents yet; create a session before starting a btw conversation",
                params.session_id
            ))
        })?;

        // The BTW fork inherits the main agent's configuration except for the
        // tool set: all tools are disabled so the side channel answers from
        // context alone.
        let btw_config = {
            let config_arc = main_agent.config();
            let mut btw_config = config_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
            btw_config.enabled_tools = Some(Vec::new());
            btw_config
        };

        let lifecycle = AgentLifecycle::new(Arc::clone(&session));
        let btw_agent = lifecycle
            .fork_agent(&main_agent, btw_config)
            .await
            .map_err(session_error)?;

        let memory = btw_agent.context_memory();
        memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_system_reminder(BTW_SYSTEM_REMINDER.to_string());

        to_result(BtwStartResult {
            agent_id: btw_agent.id().as_str().to_string(),
        })
    }

    /// `model.list` / `listModels` — enumerate configured model aliases
    /// as a catalog. Each item follows the `modelCatalogItemSchema`
    /// wire shape (snake_case).
    ///
    /// Uses `to_mirri_config_wire()` so legacy `[model]` / `[provider]`
    /// config is automatically mapped to the multi-provider shape.
    ///
    /// The row set is exactly the config's alias set (D-M9): the
    /// models.dev directory (see `provider_handlers`) never merges rows
    /// into the picker surface — it only feeds the display-name
    /// resolution chain (D-M1): `alias.displayName ?? directory name ??
    /// omitted`, the last level omitted because the wire `model` field
    /// already carries the raw model name. The directory is `None` only
    /// when no cache file exists, `persist_cache` is off, or the cache
    /// failed to parse (P5: a restart with a valid cache file restores it
    /// lazily — C1, no refresh needed).
    ///
    /// Each row carries `id` — the `[models.<id>]` config key (the model
    /// alias, the canonical identifier every write path addresses models
    /// by; ADR-003). The klient `modelCatalogItemSchema` has no `id`
    /// field, but zod objects strip (never reject) unknown keys, so the
    /// addition is contract-safe; consumers that predate it compose the
    /// `provider/model` display key client-side.
    pub(super) async fn dispatch_model_list(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let wire = config.to_mirri_config_wire();

        let models = wire.get("models").and_then(|m| m.as_object());
        let providers = wire.get("providers").and_then(|p| p.as_object());

        // P5 C1: 进入名称解析前确保目录已从磁盘缓存恢复（懒加载）——重启
        // 后没有 refresh 时首个 list 也从缓存兜底名称；后台预热任务与本次
        // 调用先到先得（ensure 幂等，后到者直接返回）。
        self.ensure_models_dev_catalog_loaded();

        // The models.dev directory (may be `None` — see the doc comment).
        // Held once up front so the resolution chain below (D-M1) can
        // consult it per row without re-locking.
        let catalog = self
            .models_dev_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let items: Vec<Value> = match models {
            Some(models) => models
                .iter()
                .map(|(key, alias)| {
                    let provider_id = alias.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                    let provider_type = providers
                        .and_then(|p| p.get(provider_id))
                        .and_then(|p| p.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("openai");

                    // Build object with only present fields (zod optional rejects null)
                    let mut obj = serde_json::Map::new();
                    // The config key — the canonical alias id (ADR-003).
                    obj.insert("id".to_string(), json!(key));
                    obj.insert("provider".to_string(), json!(provider_id));
                    obj.insert(
                        "model".to_string(),
                        alias.get("model").cloned().unwrap_or(json!("")),
                    );
                    obj.insert(
                        "max_context_size".to_string(),
                        alias.get("maxContextSize").cloned().unwrap_or(json!(0)),
                    );
                    // Display-name resolution chain (D-M1): alias
                    // displayName ?? directory name ?? omitted — the
                    // wire `model` field is itself the raw model name,
                    // so the last chain level needs no rewrite.
                    let display_name = alias
                        .get("displayName")
                        .filter(|v| v.is_string())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            catalog_display_name(
                                catalog.as_ref(),
                                provider_id,
                                alias.get("model").and_then(Value::as_str).unwrap_or(""),
                            )
                        });
                    if let Some(dn) = display_name {
                        obj.insert("display_name".to_string(), json!(dn));
                    }
                    if let Some(desc) = alias.get("description").filter(|v| !v.is_null()) {
                        obj.insert("description".to_string(), desc.clone());
                    }
                    if let Some(caps) = alias.get("capabilities").filter(|v| !v.is_null()) {
                        obj.insert("capabilities".to_string(), caps.clone());
                    }
                    if let Some(se) = alias.get("supportEfforts").filter(|v| !v.is_null()) {
                        obj.insert("support_efforts".to_string(), se.clone());
                    }
                    if let Some(de) = alias.get("defaultEffort").filter(|v| !v.is_null()) {
                        obj.insert("default_effort".to_string(), de.clone());
                    }
                    // Include provider type as metadata (not in TS schema but useful)
                    let _ = provider_type; // provider_type used for potential future filtering
                    Value::Object(obj)
                })
                .collect(),
            None => vec![],
        };

        // The wire shape is the bare model-catalog array (the klient
        // `modelResolver.listModels` contract — TSV row 106) — NOT the
        // `{"models": [...]}` wrapper the engine previously sent.
        Ok(Value::Array(items))
    }

    /// `model.listProviders` / `listProviders` — enumerate configured
    /// providers with their connection status and model lists.
    /// Each item follows the `providerCatalogItemSchema` wire shape
    /// (snake_case).
    ///
    /// Uses `to_mirri_config_wire()` so legacy config is mapped.
    pub(super) async fn dispatch_model_list_providers(&self) -> Result<Value, EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let wire = config.to_mirri_config_wire();

        let providers = wire.get("providers").and_then(|p| p.as_object());
        let models = wire.get("models").and_then(|m| m.as_object());

        let items: Vec<Value> = match providers {
            Some(providers) => providers
                .iter()
                .map(|(id, entry)| {
                    // "Credential configured" spans every resolvable
                    // source: an active credential-store entry (any value
                    // type) OR the env chain answering — the config
                    // `apiKey` field no longer counts (field tier
                    // retired, D-M3).
                    let provider_type = entry
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("openai")
                        .trim()
                        .to_ascii_lowercase();
                    let has_api_key = self
                        .factory_credentials()
                        .map(|store| store.get(id).is_some())
                        .unwrap_or(false)
                        || resolve_provider_env_chain(entry, &provider_type).is_some();

                    let provider_models: Vec<String> = models
                        .map(|m| {
                            m.iter()
                                .filter(|(_, a)| {
                                    a.get("provider").and_then(|v| v.as_str()) == Some(id.as_str())
                                })
                                .map(|(_, a)| {
                                    a.get("model")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let mut obj = serde_json::Map::new();
                    obj.insert("id".to_string(), json!(id));
                    obj.insert(
                        "type".to_string(),
                        json!(
                            entry
                                .get("type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("openai")
                        ),
                    );
                    obj.insert("has_api_key".to_string(), json!(has_api_key));
                    obj.insert(
                        "status".to_string(),
                        json!(if has_api_key {
                            "connected"
                        } else {
                            "unconfigured"
                        }),
                    );
                    obj.insert("models".to_string(), json!(provider_models));
                    if let Some(bu) = entry.get("baseUrl").filter(|v| !v.is_null()) {
                        obj.insert("base_url".to_string(), bu.clone());
                    }
                    if let Some(dm) = entry.get("defaultModel").filter(|v| !v.is_null()) {
                        obj.insert("default_model".to_string(), dm.clone());
                    }
                    Value::Object(obj)
                })
                .collect(),
            None => vec![],
        };
        // The wire shape is the bare provider-catalog array (the klient
        // `modelResolver.listProviders` contract — TSV row 107) — NOT the
        // `{"providers": [...]}` wrapper the engine previously sent.
        Ok(Value::Array(items))
    }
}

/// The task-list summary budget: the last characters of a task's
/// accumulated output served in the summary view (the full output rides
/// `task.output`, which already takes an explicit `tail` cap).
const TASK_LIST_OUTPUT_PREVIEW_CHARS: usize = 2048;

/// The task-list summary budget for a process task's command line: the
/// first characters (a long build invocation is recognizable from its
/// head; the detail faces serve the full line).
const TASK_LIST_COMMAND_CHARS: usize = 256;

/// Serialize the task rows for a task-list face (S5: the task.list polling
/// amplifier — a polling client hauled every task's full `output` (up to
/// [`MAX_TASK_OUTPUT_BYTES`](mirri_core::task::MAX_TASK_OUTPUT_BYTES), 1MB)
/// and full `command` line once per second, so a heavy session's list
/// answered in the MBs every poll).
///
/// The DEFAULT is the summary view: the row shape is the direct
/// `AgentTask` serialization (unchanged keys) with `output` replaced by its
/// last [`TASK_LIST_OUTPUT_PREVIEW_CHARS`] characters — a tail, the part a
/// live log viewer actually wants — plus an `output_truncated: true` flag
/// and `output_bytes` (the untruncated length) riding only when the output
/// was actually cut, and `kind.command` capped to its first
/// [`TASK_LIST_COMMAND_CHARS`] characters. `includeFull: true` opts into
/// the direct `AgentTask` serialization (the detail face callers ask for
/// explicitly).
fn task_rows_value(tasks: &[AgentTask], include_full: bool) -> Result<Value, EngineError> {
    let rows: Result<Vec<Value>, _> = tasks
        .iter()
        .map(|task| {
            let mut row =
                serde_json::to_value(task).map_err(|e| EngineError::Internal(e.to_string()))?;
            if include_full {
                return Ok(row);
            }
            let output_chars = task.output.chars().count();
            if output_chars > TASK_LIST_OUTPUT_PREVIEW_CHARS {
                row["output"] = Value::String(
                    task.output
                        .chars()
                        .skip(output_chars - TASK_LIST_OUTPUT_PREVIEW_CHARS)
                        .collect(),
                );
                row["output_truncated"] = Value::Bool(true);
                // The TRUE byte length of the untruncated output (chars and
                // bytes differ for multi-byte content; the field name must
                // not lie — the v2 REST face serves real bytes too).
                row["output_bytes"] = Value::from(task.output.len());
            }
            if let Some(Value::Object(kind)) = row.get_mut("kind")
                && let Some(Value::String(command)) = kind.get_mut("command")
                && command.chars().count() > TASK_LIST_COMMAND_CHARS
            {
                *command = command
                    .chars()
                    .take(TASK_LIST_COMMAND_CHARS)
                    .collect::<String>();
                // Flagged inside the kind borrow scope (the row-level insert
                // would be a second mutable borrow).
                kind.insert("command_truncated".to_string(), Value::Bool(true));
            }
            Ok(row)
        })
        .collect();
    serde_json::to_value(rows?).map_err(|e| EngineError::Internal(e.to_string()))
}

/// D-M1 chain 的目录名兜底：两层 map 查找（provider → model → name）。
/// 目录 JSON 顶层以 provider id 为键（遍历路径与 provider_handlers 的
/// `merge_models_dev_catalog` 同源，以其既有访问式为准）；缺字段降级
/// `None` —— 解析失败即回到链的下一级，从不出错。
fn catalog_display_name(
    catalog: Option<&Arc<Value>>,
    provider: &str,
    model: &str,
) -> Option<String> {
    let name = catalog?
        .get(provider)?
        .get("models")?
        .get(model)?
        .get("name")?
        .as_str()?
        .to_string();
    Some(name)
}

// --- btw helpers ---------------------------------------------------------

/// System reminder appended to a BTW agent's context memory at fork time —
/// a byte-identical mirror of mirri-server's `BTW_SYSTEM_REMINDER`
/// (`crates/mirri-server/src/rpc.rs`), which itself mirrors the TS kernel's
/// `SIDE_QUESTION_SYSTEM_REMINDER`
/// (`packages/agent-core/src/session/subagent-host.ts`). Copied rather than
/// shared across crates: two crates depending on each other for a doc
/// string is a heavier coupling than the duplication.
const BTW_SYSTEM_REMINDER: &str = "\
This is a side-channel conversation with the user. You should answer user questions directly based on what you already know.

IMPORTANT:
- You are a separate, lightweight instance.
- The main agent continues independently; do not reference being interrupted.
- Do not call any tools. All tool calls are disabled and will be rejected.
- Respond only with text based on what you already know from the conversation and this side-channel conversation.
- Follow-up turns may happen in this side-channel conversation.
- If you do not know the answer, say so directly.";

// --- workspace-local additional-dir helpers (P11a F-P8-2) ------------------
//
// Mirrors the TS kernel's `packages/agent-core/src/config/workspace-local.ts`:
// the project root is the nearest ancestor carrying a `.git` marker (the
// workspace cwd itself when none exists), and the additional-dir list lives
// in `<projectRoot>/.mirri-code/local.toml` under
// `workspace.additional_dir`.

/// Seed a session's workspace sandbox with the additional dirs persisted in
/// the workspace-local `local.toml` — the TS `readWorkspaceAdditionalDirs`
/// read at session create/resume (F-P11a-3a): the dirs are effective the
/// moment the session exists, before any `addAdditionalDir` call.
///
/// Best-effort by design: a missing config leaves the sandbox root-only,
/// a malformed one degrades with a warning instead of failing the session
/// (the strict failure belongs to `session.addAdditionalDir`, which surfaces
/// the config error to the caller), and a dir deleted after it was
/// persisted drops out of the list (the `resolveExistingAdditionalDirs`
/// lenient resolution).
fn seed_workspace_additional_dirs(session: &Arc<mirri_core::SessionScope>, cwd: &std::path::Path) {
    let Some(paths) = session.workspace_paths() else {
        return;
    };
    let project_root = find_project_root(cwd);
    let config_path = project_root.join(".mirri-code").join("local.toml");
    let file = match read_workspace_local_toml(&config_path) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                %error,
                "could not read the workspace local.toml; the session sandbox stays root-only"
            );
            return;
        }
    };
    let dirs = match file_dirs_of(&file) {
        Ok(dirs) => dirs,
        Err(error) => {
            tracing::warn!(
                %error,
                "workspace.additional_dir is malformed; the session sandbox stays root-only"
            );
            return;
        }
    };
    for dir in resolve_existing_additional_dirs(&project_root, dirs) {
        paths.add_additional(dir);
    }
}

// --- additional-dir context notification (P11f F-P11a-3b) -----------------

/// Append the added-dir context notification for the main agent — the TS
/// kernel's `notifyAdditionalDirAdded` (session/index.ts): the main
/// agent's context receives a user-role message wrapped in
/// `<local-command-stdout>` tags telling it a new workspace directory is
/// accessible, naming the config file when the dir was persisted. Only the
/// main agent is notified (TS `requireMainAgent()`); the append rides both
/// the in-memory context and the durable `context.append_message` wire
/// record (the same record/resume contract the shell-command messages use),
/// so a resumed session replays the notification.
async fn notify_additional_dir_added(
    session: &Arc<mirri_core::SessionScope>,
    memory: &Arc<std::sync::Mutex<mirri_core::ContextMemory>>,
    path: &str,
    persisted: bool,
    config_path: &std::path::Path,
) {
    let content = if persisted {
        format!(
            "Added workspace directory:\n  {path}\n  Saved to:\n  {}",
            config_path.display()
        )
    } else {
        format!("Added workspace directory:\n  {path}\n  For this session only")
    };
    let text = format!(
        "<local-command-stdout>\n{}\n</local-command-stdout>",
        content.trim()
    );
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_message(mirri_core::ContextMessage {
            message: mirri_core::Message {
                role: mirri_core::Role::User,
                name: None,
                content: vec![mirri_core::ContentPart::Text { text: text.clone() }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            // The TS origin is `{kind: 'injection', variant:
            // 'local-command-stdout'}` — the engine's `PromptOrigin` is a
            // unit variant, so the variant rides the wire record below.
            origin: Some(mirri_core::PromptOrigin::Injection),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": text }],
                "origin": { "kind": "injection", "variant": "local-command-stdout" },
                "isError": false,
            }),
        })
        .await;
}

// --- shell command helpers ----------------------------------------------

/// Append one `shell_command`-origin context message: the in-memory append
/// on the agent's context plus the durable `context.append_message` wire
/// record carrying the full TS origin shape (`{kind, phase, isError?}`).
///
/// The engine's [`mirri_core::PromptOrigin`] is a unit variant (the rust
/// origin model carries no phase), so the phase and error flag ride the
/// wire record's origin object — replay deserializes the kind and ignores
/// the extra fields, and the persisted shape stays byte-compatible with
/// the TS engine's `context.jsonl`.
async fn append_shell_context_message(
    session: &Arc<mirri_core::SessionScope>,
    memory: &Arc<std::sync::Mutex<mirri_core::ContextMemory>>,
    text: &str,
    origin: Value,
    is_error: bool,
) {
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_message(mirri_core::ContextMessage {
            message: mirri_core::Message {
                role: mirri_core::Role::User,
                name: None,
                content: vec![mirri_core::ContentPart::Text {
                    text: text.to_string(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(mirri_core::PromptOrigin::ShellCommand),
            is_error,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": text }],
                "origin": origin,
                "isError": is_error,
            }),
        })
        .await;
}

/// Rename the wire-`Message`'s snake_case provenance keys onto the v1
/// camelCase spellings the TS replay contract expects (`toolCalls` /
/// `toolCallId` / `isError`), flattening each tool call onto the v1.1
/// `ToolCall` spelling (`{type, id, name, arguments}` at the top level —
/// the engine's internal serialization is the v1.0 nested OpenAI shape
/// `{type, id, function: {name, arguments}}`, the same fold the TS wire
/// migration `migrateV1_0ToV1_1` performs). The engine's internal
/// serialization (and the persisted wire log) keeps its own spellings —
/// this is a pure view-side rename so the resumed summary's
/// `context.history` / `replay` records carry the v1 shapes the SDK and
/// the TUI's replay renderer read.
fn context_message_to_wire(message: Value) -> Value {
    let mut map = match message {
        Value::Object(map) => map,
        other => return other,
    };
    if let Some(tool_calls) = map.remove("tool_calls") {
        let flattened = match tool_calls {
            Value::Array(calls) => Value::Array(calls.into_iter().map(tool_call_to_wire).collect()),
            other => other,
        };
        map.insert("toolCalls".to_string(), flattened);
    }
    if let Some(tool_call_id) = map.remove("tool_call_id") {
        map.insert("toolCallId".to_string(), tool_call_id);
    }
    if let Some(is_error) = map.remove("is_error") {
        map.insert("isError".to_string(), is_error);
    }
    Value::Object(map)
}

/// Flatten one serialized tool call's nested `function` wrapper onto the
/// element's top level (`name` / `arguments`); the `type` and `id` keys
/// pass through untouched.
fn tool_call_to_wire(tool_call: Value) -> Value {
    let mut map = match tool_call {
        Value::Object(map) => map,
        other => return other,
    };
    if let Some(Value::Object(function)) = map.remove("function") {
        if let Some(name) = function.get("name") {
            map.insert("name".to_string(), name.clone());
        }
        if let Some(arguments) = function.get("arguments") {
            map.insert("arguments".to_string(), arguments.clone());
        }
    }
    Value::Object(map)
}

/// Escape the five XML-significant characters for the `<bash-input>` /
/// `<bash-stdout>` wrapper tags (the TS `escapeXml`).
fn escape_shell_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Toggle the 7 `Collaboration*` builtin tools' visibility on the agent's
/// tool manager — the tool side of the `collaboration.enter` /
/// `collaboration.exit` mode gate. The tools stay registered (executable
/// via `find_tool`); only the LLM-visible table changes. Idempotent per
/// name, so a repeated enter/exit is a no-op on an already-toggled set.
fn set_collaboration_tools_enabled(agent: &Arc<mirri_core::AgentScope>, enabled: bool) {
    let tool_manager = agent.tool_manager();
    let mut tools = tool_manager.write();
    for name in mirri_core::tools::COLLABORATION_TOOL_NAMES {
        tools.set_builtin_tool_enabled(name, enabled);
    }
}

/// Apply a model switch — the ONE shared path every model-carrying entry
/// point walks (spec 11 §2.2): `agent.setModel`, `session.updateMeta`'s
/// model branch, and `message.send`'s model param all land here.
///
/// The five things, in order:
///
/// 1. validate the alias (`model.not_found` on an unknown name, with the
///    available-model listing — the R10/A11 single rejection face; the
///    membership test accepts the M1 two forms: exact alias key or
///    `provider/model` composite),
/// 2. set the agent's live scope config (idempotent no-op when unchanged —
///    the web sends the session model on every send, so a switch that
///    lands here must have actually changed the effective model; a no-op
///    journals nothing: no `config.update`, no marker, no event),
///    2.5. apply the new model's catalog default thinking effort (models
///    with no declaration keep the current effort),
/// 3. persist the change so it survives a resume (the reducer folds the
///    alias back into the agent state on replay),
/// 4. record the model-switched marker and publish the `ModelSwitched`
///    event (plus push the new model's context window AND its usage
///    attribution key into the live turn flow — the flow was built with
///    the previous model's window, and its usage `by_model` key must
///    re-attribute the post-switch steps, spec 11 A13).
pub(super) async fn switch_session_model(
    server: &EmbeddedServer,
    session_id: &str,
    agent: &Arc<mirri_core::AgentScope>,
    model: &str,
) -> Result<(), EngineError> {
    if !server.model_exists(model) {
        return Err(not_found_domain(
            "model.not_found",
            format!(
                "model {model} does not exist. {}",
                server.available_model_listing()
            ),
        ));
    }
    let current = agent
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .model_id
        .clone();
    if current == model {
        return Ok(());
    }
    agent.set_model(model.to_string());
    // Per-model default effort: the new model's catalog declaration
    // (`default_effort`, else the middle of `support_efforts`) replaces the
    // carried-over effort — the TS `defaultThinkingEffortForModel` model-
    // default branch. Models with no declaration keep the current effort
    // (deviation D2 from the TS 'on' fallback: the rust engine never
    // force-enables thinking). An explicit `thinking` param riding the same
    // updateMeta call is applied after this branch, so it still wins.
    let (support_efforts, default_effort) = {
        let config = server.config.read().unwrap_or_else(|e| e.into_inner());
        config
            .resolve_model_alias(model)
            .and_then(|alias_key| {
                config
                    .models
                    .as_ref()
                    .and_then(|models| models.get(&alias_key))
                    .map(|alias| {
                        (
                            alias.support_efforts.clone().unwrap_or_default(),
                            alias.default_effort.clone(),
                        )
                    })
            })
            .unwrap_or_default()
    };
    if let Some(effort) = mirri_core::model_catalog::default_thinking_effort(
        default_effort.as_deref(),
        &support_efforts,
    ) {
        agent.set_thinking(Some(effort));
    }
    server
        .record_session_config_op(session_id, model.to_string())
        .await?;
    server.update_turn_flow_model_window(session_id, agent.id().as_str(), model);
    // Re-attribute the flow's usage accounting too (A13): the switch
    // updates the turn flow's `by_model` key, so the usage recorded after
    // the switch point lands under the new model's alias — the same
    // alias-based attribution the wire `usage.record` rows carry.
    server.update_turn_flow_usage_model_key(session_id, agent.id().as_str(), model);
    record_model_switch(server, session_id, agent, &current, model).await
}

/// Record a model switch: append the UI-only marker message to the
/// agent's context (in-memory + durable `context.append_message`, the
/// same dual track the shell-command messages use) and publish the
/// `ModelSwitched` event so live clients can render the divider.
async fn record_model_switch(
    server: &EmbeddedServer,
    session_id: &str,
    agent: &Arc<mirri_core::AgentScope>,
    from: &str,
    to: &str,
) -> Result<(), EngineError> {
    // The agent's context memory — the same acquisition getHistory uses.
    let memory = agent.context_memory();
    // The session scope — the same resolve the updateMeta handler uses,
    // for the durable record_op.
    let session = server
        .resolve_session(session_id)
        .await
        .map_err(|_| not_found(format!("session not found: {session_id}")))?;
    let text = format!("Model switched from {from} to {to}");
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_message(mirri_core::ContextMessage {
            message: mirri_core::Message {
                role: mirri_core::Role::User,
                name: None,
                content: vec![ContentPart::Text { text: text.clone() }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(mirri_core::PromptOrigin::ModelSwitched {
                from: from.to_string(),
                to: to.to_string(),
            }),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": text }],
                "origin": { "kind": "model_switched", "from": from, "to": to },
                "isError": false,
            }),
        })
        .await;
    // Publish the live event — the same publish_session_event helper
    // the SessionMetaUpdated announcement uses.
    server.publish_session_event(
        session_id,
        SessionEvent::ModelSwitched {
            from: from.to_string(),
            to: to.to_string(),
        },
    );
    Ok(())
}
