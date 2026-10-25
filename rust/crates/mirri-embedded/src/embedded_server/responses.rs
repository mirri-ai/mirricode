//! Typed response structs for the embedded engine's RPC wire shapes.
//!
//! M5-3b sweep (§7 criterion 2): every handler response used to be
//! hand-built with a top-level `json!()` macro call; the structs here
//! declare the wire shapes instead, so a shape change is a type change,
//! not a macro edit. Serialization of these structs is infallible (plain
//! serde), so handlers convert them with [`to_result`].
//!
//! Where a response field is genuinely dynamic — a pre-serialized sub-view
//! (`token_usage_to_json`, `goal_snapshot_json`), an arbitrary model
//! object, or a wrapper around an already-built `Value` — the field stays a
//! [`serde_json::Value`] inside the typed struct: the leaf is dynamic, the
//! envelope is typed. No top-level `Ok(json!(...))` response remains.

use chrono::{DateTime, Utc};
use mirri_transport::adapter::EngineError;
use serde::Serialize;

/// Convert a typed response into the `Value` the engine dispatch returns.
///
/// The structs in this module serialize infallibly, so the error arm is a
/// defensive fallback rather than a reachable path.
pub(super) fn to_result(value: impl Serialize) -> Result<serde_json::Value, EngineError> {
    serde_json::to_value(value).map_err(|e| EngineError::Internal(e.to_string()))
}

/// The bare `{"ok": true}` acknowledgement every ack-shaped method answers.
#[derive(Debug, Serialize)]
pub(super) struct AckResult {
    pub ok: bool,
}

/// The shared `{"ok": true}` response shorthand.
pub(super) fn ack() -> Result<serde_json::Value, EngineError> {
    to_result(AckResult { ok: true })
}

/// `agent.listCommands` — one element of the klient `AgentCommandInfo[]`
/// wire shape (`agentCommandInfoSchema`: name, optional description,
/// required source). The source is the contributing plugin's id — the
/// real provenance the command registry records.
#[derive(Debug, Serialize)]
pub(super) struct AgentCommandInfoResult {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: String,
}

/// `agent.activity` — the klient `agentActivityStateSchema` wire shape
/// (`activity.ts:66-75`), converted from the engine's
/// `mirri_core::activity_view::AgentActivityState`. Field-for-field:
/// `lifecycle` (`ready`|`disposed`), the optional `turn` (turnId, origin,
/// phase, stream?, step, ending, endingReason?, retry?, pendingApprovals,
/// activeToolCalls, since), the optional `lastTurn` (turnId, reason,
/// durationMs?, at), and the `background` array (kind, id, since).
///
/// `turn.origin` mirrors the core `PromptOrigin` serialization
/// (`{"kind":"user"}` …) — the klient schema pins it as `z.unknown()`, so
/// any shape is acceptable. `turn.stream` is never set: the engine's hook
/// surface cannot distinguish assistant/thinking/tool-call deltas (the
/// schema marks the field optional).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityResult {
    pub lifecycle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<AgentActivityTurnResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<AgentActivityLastTurnResult>,
    pub background: Vec<AgentActivityBackgroundResult>,
}

/// The `turn` sub-object of `agent.activity`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityTurnResult {
    pub turn_id: u32,
    pub origin: serde_json::Value,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    pub step: u32,
    pub ending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ending_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<AgentActivityRetryResult>,
    pub pending_approvals: Vec<AgentActivityApprovalResult>,
    pub active_tool_calls: Vec<AgentActivityToolCallResult>,
    pub since: u64,
}

/// The `lastTurn` sub-object of `agent.activity`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityLastTurnResult {
    pub turn_id: u32,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub at: u64,
}

/// The `retry` sub-object of `agent.activity`'s `turn`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityRetryResult {
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

/// One `pendingApprovals` element of `agent.activity`'s `turn`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityApprovalResult {
    pub approval_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub since: u64,
}

/// One `activeToolCalls` element of `agent.activity`'s `turn`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityToolCallResult {
    pub tool_call_id: String,
    pub name: String,
    pub since: u64,
}

/// One `background` element of `agent.activity`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentActivityBackgroundResult {
    pub kind: String,
    pub id: String,
    pub since: u64,
}

impl AgentActivityResult {
    /// Convert the engine's folded state to the klient wire shape.
    pub(super) fn from_state(state: &mirri_core::activity_view::AgentActivityState) -> Self {
        let phase = |p: mirri_core::activity_view::TurnPhase| match p {
            mirri_core::activity_view::TurnPhase::Running => "running",
            mirri_core::activity_view::TurnPhase::Streaming => "streaming",
            mirri_core::activity_view::TurnPhase::ToolCall => "tool_call",
            mirri_core::activity_view::TurnPhase::Retrying => "retrying",
        };
        let ending_reason = |r: mirri_core::activity_view::EndingReason| match r {
            mirri_core::activity_view::EndingReason::Aborted => "aborted",
            mirri_core::activity_view::EndingReason::MaxSteps => "max_steps",
            mirri_core::activity_view::EndingReason::Error => "error",
        };
        let last_reason = |r: mirri_core::activity_view::TurnEndReason| match r {
            mirri_core::activity_view::TurnEndReason::Completed => "completed",
            mirri_core::activity_view::TurnEndReason::Cancelled => "cancelled",
            mirri_core::activity_view::TurnEndReason::Failed => "failed",
            mirri_core::activity_view::TurnEndReason::Blocked => "blocked",
            // The finer-grained rust-only variants fold into the klient
            // vocabulary's failure word (the engine never records them
            // today; the mapping lives here so the wire can never leak a
            // word outside `turnEndReasonSchema`).
            mirri_core::activity_view::TurnEndReason::MaxSteps
            | mirri_core::activity_view::TurnEndReason::Error => "failed",
        };
        Self {
            lifecycle: match state.lifecycle {
                mirri_core::activity_view::ActivityViewLifecycle::Ready => "ready",
                mirri_core::activity_view::ActivityViewLifecycle::Disposed => "disposed",
            }
            .to_string(),
            turn: state.turn.as_ref().map(|turn| AgentActivityTurnResult {
                turn_id: turn.turn_id,
                origin: serde_json::to_value(&turn.origin).unwrap_or(serde_json::Value::Null),
                phase: phase(turn.phase).to_string(),
                // No honest source for which stream is active (see the
                // struct doc) — the optional field is omitted.
                stream: None,
                step: turn.step,
                ending: turn.ending,
                ending_reason: turn.ending_reason.map(ending_reason).map(str::to_string),
                retry: turn.retry.as_ref().map(|retry| AgentActivityRetryResult {
                    failed_attempt: retry.failed_attempt,
                    next_attempt: retry.next_attempt,
                    max_attempts: retry.max_attempts,
                    delay_ms: retry.delay_ms,
                    error_name: retry.error_name.clone(),
                    status_code: retry.status_code,
                }),
                pending_approvals: turn
                    .pending_approvals
                    .iter()
                    .map(|a| AgentActivityApprovalResult {
                        approval_id: a.approval_id.clone(),
                        tool_call_id: a.tool_call_id.clone(),
                        since: a.since,
                    })
                    .collect(),
                active_tool_calls: turn
                    .active_tool_calls
                    .iter()
                    .map(|t| AgentActivityToolCallResult {
                        tool_call_id: t.tool_call_id.clone(),
                        name: t.name.clone(),
                        since: t.since,
                    })
                    .collect(),
                since: turn.since,
            }),
            last_turn: state
                .last_turn
                .as_ref()
                .map(|last| AgentActivityLastTurnResult {
                    turn_id: last.turn_id,
                    reason: last_reason(last.reason).to_string(),
                    duration_ms: last.duration_ms,
                    at: last.at,
                }),
            background: state
                .background
                .iter()
                .map(|b| AgentActivityBackgroundResult {
                    kind: b.kind.clone(),
                    id: b.id.clone(),
                    since: b.since,
                })
                .collect(),
        }
    }
}

/// `session.getMeta` — the session's metadata (id, title, timestamps,
/// agent ids, tags).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionMetaResult {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub agents: Vec<String>,
    pub tags: Vec<String>,
}

/// `session.get` — the session's summary view plus its agent list, the
/// same wire shape mirri-server's transport-owned handler serves (so both
/// backends answer identically for the B1 EMB backfill).
#[derive(Debug, Serialize)]
pub(super) struct SessionDetailsResult {
    #[serde(flatten)]
    pub view: super::SessionView,
    pub agents: Vec<SessionAgentResult>,
}

/// One agent entry of the `session.get` agent list.
#[derive(Debug, Serialize)]
pub(super) struct SessionAgentResult {
    pub id: String,
}

/// `session.createChild` — the klient `sessionLifecycle` scope-handle wire
/// shape `{ id, kind }` (loose: extra fields may appear in-process).
#[derive(Debug, Serialize)]
pub(super) struct SessionHandleResult {
    pub id: String,
    /// The v2 `LifecycleScope` value of a session scope handle
    /// (`ScopeKind.Session = 2` in agent-core-v2).
    pub kind: u32,
}

/// `session.search` — one literal-mode hit (snake_case wire, the P10
/// rust-first read face). `turn` is the 0-based turn ordinal (absent for
/// title hits); `time` is the session's `updatedAt` epoch ms (context
/// memory carries no per-message timestamps — the documented v1
/// deviation).
#[derive(Debug, Serialize)]
pub(super) struct SessionSearchHitResult {
    pub session_id: String,
    pub workspace_id: String,
    pub session_title: String,
    /// `main` or a subagent id; empty for title-role hits.
    pub agent_id: String,
    /// `user` | `assistant` | `title`.
    pub role: &'static str,
    pub snippet: String,
    pub time: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
}

/// `session.search` — one literal-mode page (snake_case wire). The
/// `terms` request mode degrades to the literal scan; `mode` echoes the
/// effective mode. `next_page_token` is absent on the final page.
#[derive(Debug, Serialize)]
pub(super) struct SessionSearchResult {
    pub hits: Vec<SessionSearchHitResult>,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
}

/// The `LifecycleScope::Session` discriminator of the v2 scope handles
/// (`scope.ts`: App = 0, Workspace = 1, Session = 2, Agent = 3).
pub(super) const SESSION_SCOPE_KIND: u32 = 2;

/// `session.export` — the v2 `ExportSessionResult` wire contract.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ExportResult {
    pub zip_path: String,
    pub entries: Vec<String>,
    pub session_dir: String,
    pub manifest: mirri_core::session_export::ExportSessionManifest,
}

/// `session.undo` — how many user messages were removed.
#[derive(Debug, Serialize)]
pub(super) struct UndoResult {
    pub ok: bool,
    pub undone: usize,
}

/// `agent.getContext` — the addressed agent's context-memory snapshot.
#[derive(Debug, Serialize)]
pub(super) struct GetContextResult {
    pub context: ContextSummary,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ContextSummary {
    pub message_count: usize,
    pub used_tokens: u64,
}

/// `agent.getUsage` — the TS `UsageStatus` wire object: accumulated token
/// usage by model plus the current-turn accumulator, bare at the top level
/// (no `usage` envelope) with the TS `TokenUsage` bucket spelling
/// (`inputOther`/`output`/`inputCacheRead`/`inputCacheCreation`, ADR-012).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetUsageResult {
    pub total: serde_json::Value,
    pub current_turn: serde_json::Value,
    pub by_model: serde_json::Value,
}

/// `agent.setModel` — the applied model plus the serialized provider name.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SetModelResult {
    pub model: String,
    pub provider_name: String,
}

/// `agent.getPermission` — permission mode and the (always empty) rule list.
#[derive(Debug, Serialize)]
pub(super) struct GetPermissionResult {
    pub mode: serde_json::Value,
    pub rules: Vec<serde_json::Value>,
}

/// `agent.getConfig` — the resolved agent configuration.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetConfigResult {
    pub cwd: String,
    pub model: String,
    pub model_alias: String,
    pub thinking_effort: serde_json::Value,
    pub system_prompt: Option<String>,
    pub permission_mode: serde_json::Value,
    pub max_turns: u32,
}

/// `agent.setActiveTools` — the applied tool-name restriction.
#[derive(Debug, Serialize)]
pub(super) struct SetActiveToolsResult {
    pub ok: bool,
    pub names: Vec<String>,
}

/// `agent.registerTool` — acknowledgement plus the registered flag.
#[derive(Debug, Serialize)]
pub(super) struct RegisterToolResult {
    pub ok: bool,
    pub registered: bool,
}

/// `agent.unregisterTool` — acknowledgement plus whether the tool was
/// actually removed.
#[derive(Debug, Serialize)]
pub(super) struct UnregisterToolResult {
    pub ok: bool,
    pub unregistered: bool,
}

/// `tool.setUserToolEnabled` — acknowledgement plus whether the addressed
/// tool was registered and the enablement that was applied.
#[derive(Debug, Serialize)]
pub(super) struct SetUserToolEnabledResult {
    pub ok: bool,
    pub registered: bool,
    pub enabled: bool,
}

/// `skill.getContent` — the skill's SKILL.md content for the web management
/// view (B8 read face): the name plus the file body verbatim.
#[derive(Debug, Serialize)]
pub(super) struct SkillGetContentResult {
    pub name: String,
    pub content: String,
}

/// `skill.setEnabled` — acknowledgement plus the resulting user-level
/// disabled set (the whole set, so the client can reconcile its view).
#[derive(Debug, Serialize)]
pub(super) struct SkillSetEnabledResult {
    pub name: String,
    pub enabled: bool,
    pub registered: bool,
    pub disabled_skills: Vec<String>,
}

/// `skill.reload` — one workspace's rescan outcome (ADR-010 Task 6): the
/// per-bucket diff counts and whether that workspace's automatic
/// file-watcher is mounted (the R4 settings-view status; `false` means the
/// manual RPC is the only reload path — the flag off or the spawn failed).
#[derive(Debug, Serialize)]
pub(super) struct SkillReloadWorkspaceResult {
    #[serde(rename = "workspace_id")]
    pub workspace_id: String,
    pub added: usize,
    pub removed: usize,
    pub updated: usize,
    pub watching: bool,
}

/// `skill.reload` — the per-workspace outcomes of one manual reload sweep.
#[derive(Debug, Serialize)]
pub(super) struct SkillReloadResult {
    pub workspaces: Vec<SkillReloadWorkspaceResult>,
}

/// `cron.list` — the session's cron tasks as pre-serialized entries.
#[derive(Debug, Serialize)]
pub(super) struct CronTasksResult {
    pub tasks: Vec<serde_json::Value>,
}

/// `cron.delete` — acknowledgement of one removed cron task.
#[derive(Debug, Serialize)]
pub(super) struct CronDeleteResult {
    pub deleted: bool,
    #[serde(rename = "taskId")]
    pub task_id: String,
}

/// `cron.trigger` — acknowledgement of one accepted manual fire.
#[derive(Debug, Serialize)]
pub(super) struct CronTriggerResult {
    pub triggered: bool,
    #[serde(rename = "taskId")]
    pub task_id: String,
}

/// `plan.status` — the session's plan-mode state.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlanStatusResult {
    pub active: bool,
    pub plan: Option<String>,
    pub plan_file: serde_json::Value,
    pub revisions: Vec<serde_json::Value>,
}

/// `goal.get` — the current goal snapshot (or JSON `null`).
#[derive(Debug, Serialize)]
pub(super) struct GoalResult {
    pub goal: serde_json::Value,
}

/// `agent.activateSkill` — acknowledgement plus the activated skill name.
#[derive(Debug, Serialize)]
pub(super) struct ActivateSkillResult {
    pub ok: bool,
    pub activated: bool,
    pub skill: String,
}

/// `core.info` — the engine's identity.
#[derive(Debug, Serialize)]
pub(super) struct CoreInfoResult {
    pub version: String,
    pub engine: String,
}

/// `session.addAdditionalDir` — the resolved (and possibly persisted)
/// additional-dir list. Mirrors the TS `AddAdditionalDirResult` /
/// `WorkspaceAdditionalDirsLoadResult` shapes (camelCase wire keys).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AddAdditionalDirResult {
    pub additional_dirs: Vec<String>,
    pub project_root: String,
    pub config_path: String,
    pub persisted: bool,
}

/// `shell.run` — the foreground `!` command result. `backgrounded` is the
/// TS `RunShellCommandResult`'s boolean flag, present only when the
/// foreground window elapsed and the command was detached to a background
/// task (the task id itself rides the `shell.started` event — the TS
/// contract carries no id here).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ShellRunResult {
    pub stdout: String,
    pub stderr: String,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backgrounded: Option<bool>,
}

/// `btw.start` — the result: the forked side-channel agent's id under the
/// `agent_id` key. Mirrors mirri-server's `StartBtwResult` (the historical
/// snake_case wire key — the same shape the kap-server REST contract
/// `startBtwSessionResponseSchema` pins).
#[derive(Debug, Serialize)]
pub(super) struct BtwStartResult {
    pub agent_id: String,
}

/// The `server` object inside an `mcp.global.connect` result.
#[derive(Debug, Serialize)]
pub(super) struct McpServerState {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `mcp.global.connect` / `mcp.global.create` — server state plus the
/// server's tools. `server` is `null` when no MCP manager is configured.
#[derive(Debug, Serialize)]
pub(super) struct McpConnectResult {
    pub server: Option<McpServerState>,
    pub tools: Vec<serde_json::Value>,
}

/// `mcp.global.testConnect` — connectivity probe result.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpTestConnectResult {
    pub status: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// `mcp.config.test` — the fresh-connect config test result (spec §4.1:
/// the management UI's "test connection" backend; the same shape the
/// retired `mcp.global.testConnect` probe answered with).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpConfigTestResult {
    pub status: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// `mcp.toggleState` / `mcp.global.toggleState` — the disabled servers and
/// per-server tool denylist entries.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpToggleStateResult {
    pub disabled_servers: Vec<String>,
    pub disabled_tools: Vec<String>,
}

/// `plugin.reload` — the TS `ReloadSummary` wire shape (`{added, removed,
/// errors}`): the full-reload diff plus per-record materialization failures
/// (item C3). Error-state plugins are not errors — they stay in the list
/// with `state: 'error'` (B2), so `errors` only carries records that failed
/// to materialize (currently unreachable: every rust materialize step is
/// tolerant, matching the TS try/catch scope).
#[derive(Debug, Serialize)]
pub(super) struct ReloadSummaryResult {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<serde_json::Value>,
}

/// `plugin.activateCommand` — the rendered command output.
#[derive(Debug, Serialize)]
pub(super) struct CommandOutputResult {
    pub output: String,
}

/// `plugin.checkUpdates` — one row of the klient
/// `pluginUpdateStatusSchema[]` wire shape (`plugins.ts:154-161`):
/// `{ id, source: 'github', current?, latest, displayVersion,
/// updateAvailable }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PluginUpdateStatusEntry {
    pub id: String,
    /// Always `"github"` — only github-sourced plugins are checked.
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<PluginGithubRefEntry>,
    pub latest: PluginGithubRefEntry,
    pub display_version: String,
    pub update_available: bool,
}

/// One `{ kind, value }` GitHub ref — the klient `pluginGithubRefSchema`.
#[derive(Debug, Serialize)]
pub(super) struct PluginGithubRefEntry {
    /// `"branch"` | `"tag"` | `"sha"`.
    pub kind: &'static str,
    pub value: String,
}

impl From<&mirri_plugin_host::GithubRef> for PluginGithubRefEntry {
    fn from(ref_: &mirri_plugin_host::GithubRef) -> Self {
        Self {
            kind: match ref_.kind {
                mirri_plugin_host::GithubRefKind::Branch => "branch",
                mirri_plugin_host::GithubRefKind::Tag => "tag",
                mirri_plugin_host::GithubRefKind::Sha => "sha",
            },
            value: ref_.value.clone(),
        }
    }
}

impl From<mirri_plugin_host::PluginUpdateStatus> for PluginUpdateStatusEntry {
    fn from(status: mirri_plugin_host::PluginUpdateStatus) -> Self {
        Self {
            id: status.id,
            source: "github",
            current: status.current.as_ref().map(PluginGithubRefEntry::from),
            latest: PluginGithubRefEntry::from(&status.latest),
            display_version: status.display_version,
            update_available: status.update_available,
        }
    }
}

/// `message.send` wait-mode result — `turn` carries the full `TurnEnd` view.
#[derive(Debug, Serialize)]
pub(super) struct MessageSendWaitResult {
    pub ok: bool,
    pub session_id: String,
    pub agent_id: String,
    pub turn: serde_json::Value,
}

/// `message.send` fire-mode result — the launched turn id.
#[derive(Debug, Serialize)]
pub(super) struct MessageSendFireResult {
    pub ok: bool,
    pub session_id: String,
    pub agent_id: String,
    pub turn_id: String,
}

/// `config.diagnostics` — the advisory warnings list (the klient
/// `ConfigDiagnostics` contract: plain strings).
#[derive(Debug, Serialize)]
pub(super) struct ConfigDiagnosticsResult {
    pub warnings: Vec<String>,
}

/// `mcp.startupMetrics` — the measured startup window: `durationMs` from
/// the first spawned background connect until the last settles (elapsed
/// so far while in flight, 0 when the engine has no startup connect set).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct McpStartupMetricsResult {
    pub duration_ms: u32,
}

/// `question.dismiss` result (B4-a) — the `{"ok": true, "dismissed": true}`
/// acknowledgement (the same wire shape mirri-server's handler serves).
#[derive(Debug, Serialize)]
pub(super) struct DismissQuestionResult {
    pub ok: bool,
    pub dismissed: bool,
}

// --- workspace domain (B4-b) ---------------------------------------------

/// `workspace.createOrTouch` / `workspace.get` result — the bound
/// workspace's info (camelCase, RFC3339 string timestamps), the same wire
/// shape mirri-server's handler serves. W8 adds the live trust verdict
/// (`trusted`: default-workspace rows are always `true` — trusted by
/// construction; the answer is read from the home-level trust store, never
/// a frozen runtime snapshot, so a `workspace.setTrust` flip is visible
/// to the very next listing).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceView {
    pub id: String,
    pub root: String,
    pub name: String,
    pub created_at: String,
    pub last_opened_at: String,
    pub session_count: usize,
    pub trusted: bool,
}

/// `workspace.setTrust` result (W8): the new trust state, matching the TS
/// v2 REST trust face's `workspaceTrustResponseSchema` (`{ trusted }` —
/// kap-server `workspaces.ts` GET/POST /trust / POST /untrust).
#[derive(Debug, Serialize)]
pub(super) struct WorkspaceTrustResult {
    pub trusted: bool,
}

/// `workspace.delete` result (W6, the D4 three-choice unregister): the
/// executed mode, the workspace id, and the number of session index rows
/// the purge actually removed (`0` for remove — that mode preserves every
/// row and byte; a re-registration restores the listing).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceDeletedResult {
    pub ok: bool,
    /// `"purge"` | `"remove"` — the echo of the requested mode.
    pub mode: String,
    pub workspace_id: String,
    /// Index session rows deleted (purge only; remove preserves data).
    pub deleted_sessions: usize,
}

/// `workspace.handlerFor` result — the klient `handleWireSchema` loose
/// shape `{ id, kind }` for the bound workspace.
#[derive(Debug, Serialize)]
pub(super) struct WorkspaceHandleResult {
    pub id: String,
    /// The v2 `LifecycleScope` value of a workspace scope handle
    /// (`ScopeKind.Workspace = 1` in agent-core-v2).
    pub kind: u32,
}

/// The `LifecycleScope::Workspace` discriminator of the v2 scope handles
/// (`scope.ts`: App = 0, Workspace = 1, Session = 2, Agent = 3).
pub(super) const WORKSPACE_SCOPE_KIND: u32 = 1;

// --- env domain (B5-a) -----------------------------------------------------

/// `env.clientIdentity` — the klient `MirriHostIdentity` camelCase shape
/// (`productName` / `version` / `platform`, optional `userAgentSuffix`).
/// The embedded engine states its own identity (no host-declared channel
/// exists yet); `userAgentSuffix` is omitted when no honest source exists.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClientIdentityResult {
    pub product_name: String,
    pub version: String,
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent_suffix: Option<String>,
}

// --- flag domain (B5-a) ----------------------------------------------------

/// `flag.explain` / `flag.explainAll` — the klient
/// `experimentalFeatureStateSchema` wire shape, converted from the engine's
/// [`mirri_core::FlagState`].
///
/// Rich-field degradations (recorded as the DRIFT note in the TSV rows):
/// `title` falls back to the flag id, `surface` defaults to `core` (the
/// rust registry registers engine flags only), `source` reports
/// `master-env` / `env` / `default` — never `config`, because the rust
/// [`mirri_core::FlagRegistry`] has no config override chain — and
/// `configValue` is omitted (no config value exists to report).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FlagStateResult {
    pub id: String,
    pub title: String,
    pub description: String,
    pub surface: String,
    pub env: String,
    pub default_enabled: bool,
    pub enabled: bool,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_value: Option<bool>,
}

impl From<mirri_core::FlagState> for FlagStateResult {
    fn from(state: mirri_core::FlagState) -> Self {
        Self {
            id: state.id,
            title: state.title,
            description: state.description,
            surface: match state.surface {
                mirri_core::FlagSurface::Core => "core".to_string(),
                mirri_core::FlagSurface::Tui => "tui".to_string(),
                mirri_core::FlagSurface::Both => "both".to_string(),
            },
            env: state.env,
            default_enabled: state.default_enabled,
            enabled: state.enabled,
            source: match state.source {
                mirri_core::FlagSource::MasterEnv => "master-env".to_string(),
                mirri_core::FlagSource::Env => "env".to_string(),
                mirri_core::FlagSource::Config => "config".to_string(),
                mirri_core::FlagSource::Default => "default".to_string(),
            },
            config_value: state.config_value,
        }
    }
}

// --- fs domain (B5-a) ------------------------------------------------------

/// `fs.browse` — one directory row of the klient `fsBrowseResponseSchema`
/// entries (snake_case wire, the `is_dir: true` literal — only directories
/// are listed).
#[derive(Debug, Serialize)]
pub(super) struct FsBrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// `fs.browse` — the klient `fsBrowseResponseSchema` snake_case shape
/// (path, nullable parent, directory entries).
#[derive(Debug, Serialize)]
pub(super) struct FsBrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsBrowseEntry>,
}

/// `fs.home` — the klient `fsHomeResponseSchema` snake_case shape. The rust
/// engine has no recent-workspace history store, so `recent_roots` is the
/// honest empty list.
#[derive(Debug, Serialize)]
pub(super) struct FsHomeResult {
    pub home: String,
    pub recent_roots: Vec<String>,
}

// --- providerDiscovery domain (B5-a) ---------------------------------------

/// `providerDiscovery.refreshProviderModels` — the klient
/// `refreshProviderModelsResponseSchema` snake_case shape.
#[derive(Debug, Serialize)]
pub(super) struct RefreshProviderModelsResult {
    pub changed: Vec<RefreshProviderChangedEntry>,
    pub unchanged: Vec<String>,
    pub failed: Vec<RefreshProviderFailedEntry>,
}

/// One `changed` row: the provider whose model list moved, and by how much.
#[derive(Debug, Serialize)]
pub(super) struct RefreshProviderChangedEntry {
    pub provider_id: String,
    pub provider_name: String,
    pub added: usize,
    pub removed: usize,
}

/// One `failed` row: the provider that could not be refreshed, and why.
#[derive(Debug, Serialize)]
pub(super) struct RefreshProviderFailedEntry {
    pub provider: String,
    pub reason: String,
}

impl RefreshProviderModelsResult {
    /// The honest empty success — the engine has no remote provider-model
    /// discovery wired (B5-a adjudication (a); see the provider handlers'
    /// module doc).
    pub(super) fn empty() -> Self {
        Self {
            changed: Vec::new(),
            unchanged: Vec::new(),
            failed: Vec::new(),
        }
    }
}

/// `providerDiscovery.listCatalog` — one browsable models.dev directory
/// entry (the P6 B4a pruned browse shape: the TS `listCatalogProviders`
/// item field set minus `guessed` / `rejected` / `reject_reason`). The
/// rows are directory facts — never the configured set (US-5) — and never
/// carry credentials (N4).
#[derive(Debug, Serialize)]
pub(super) struct CatalogProviderEntry {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire_type: Option<String>,
    pub needs_base_url: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    pub models: Vec<CatalogModelEntry>,
}

/// One browsable directory model — the `catalogModelItem` pruned shape
/// plus the directory's capability facts (ADR-004, additive): the same
/// derivation the import writes, so the browse face shows exactly what an
/// import would land. Missing fields degrade to omission (R2 lenient
/// parsing); an unusable model never appears (one shared filter).
#[derive(Debug, Serialize)]
pub(super) struct CatalogModelEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
}

/// `model.generate` — the aggregated (authoritative) response of the one
/// streaming contract: the requestId the chunk frames are keyed by, the
/// provider's finish reason, the token accounting, and the assembled
/// assistant message (a dynamic value — its parts depend on what the model
/// produced: text, thinking, and/or tool calls).
///
/// The best-effort `event.generate.chunk` notifications carry the same
/// requestId; when a chunk is dropped (subscriber channel full) the
/// aggregated response still answers the call — that asymmetry is the
/// controller-adjudicated delivery contract (design notes §3).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelGenerateResult {
    pub request_id: String,
    pub finish_reason: String,
    pub usage: ModelGenerateUsage,
    pub message: serde_json::Value,
}

/// The four-field token accounting the rust adapters surface
/// (`inputTokens` / `outputTokens` / `cacheReadTokens` /
/// `cacheCreationTokens`). The klient `TokenUsage` spells the input-side
/// buckets `inputOther` / `inputCacheRead` / `inputCacheCreation` — the
/// naming difference is recorded in the TSV row; the values are aligned
/// since the P1b three-bucket carry-through.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelGenerateUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}
