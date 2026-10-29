//! `TurnFlow` — the stateful host wrapping the stateless loop engine.
//!
//! The loop engine (`run_turn`, `execute_loop_step`) is stateless: it takes
//! references and runs a turn. `TurnFlow` adds the stateful layer:
//!
//! - **Turn ID allocation** — monotonic counter, drawn from the session's
//!   shared allocator when one is wired (see
//!   [`TurnFlow::set_turn_id_source`]).
//! - **Steer buffering** — mid-turn user messages are buffered and flushed at
//!   `before_step`.
//! - **Compaction** — auto-compaction checks and overflow recovery.
//! - **Tool-call dedup** — same-step and cross-step streak detection.
//! - **Injection** — per-step system reminders.
//!
//! ## Architecture
//!
//! `TurnFlow` holds `steer_buffer` and `compaction` as `Arc<Mutex<>>` so that
//! the `TurnFlowHooks` (which implements `LoopHooks`) can clone the `Arc`
//! references and access the same state. Memory is also `Arc<Mutex<>>` for the
//! same reason. All locks are held only for short synchronous critical
//! sections, never across `.await` points.

pub mod activity;
pub mod composite;
pub mod dedup;
pub mod hooks;
pub mod injection;
pub mod steering;
pub mod wait_registry;

pub use activity::{ActivityTracker, map_stop_reason};
pub use composite::CompositeLoopHooks;
pub use dedup::ToolCallDeduplicator;
pub use hooks::{TurnFlowHooks, TurnFlowHooksConfig};
pub use injection::{
    CapabilityGapInjector, GoalInjector, InjectionManager, Injector, PermissionModeInjector,
    PlanModeInjector, PluginSessionStartInjector, TodoListReminderInjector, ToolsDiffInjector,
};
pub use steering::{BufferedSteer, SteerBuffer};
pub use wait_registry::{
    OwnerVerdict, TurnWaitState, WaitGuard, WaitKind, WaitRegistry, WaitSubject,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::activity_view::TurnEndReason;
use crate::cancel::CancelToken;
use crate::capability_gap::{
    ModelCapabilities as GapModelCapabilities, catalog_capabilities, profile_infos_from_manager,
};
use crate::compaction::{
    CompactionBeginParams, CompactionSource, CompactionStrategy, FullCompaction,
    ManualCompactionHandle, hook_additional_context_messages,
};
use crate::context::ContextMessage;
use crate::context_memory::ContextMemory;
use crate::error::{EngineError, EngineResult};
use crate::goal::{GoalManager, GoalStatus};
use crate::hook_engine::{ExternalHooksBridge, HookEngine, HookEventType};
use crate::hooks::HookFuture;
use crate::llm::{Llm, LlmResolver, LlmUsage};
use crate::loop_engine::{
    LiveEvent, LiveEventCallback, MessageBuilders, RecordStepUsageFn, RunTurnConfig,
    StepRetryService, StepStopReason, TurnAbortGuard, TurnResult, TurnStopReason, WireOpRecorder,
};
use crate::message::ContentPart;
use crate::model_catalog::ModelCatalog;
use crate::origin::PromptOrigin;
use crate::permission::PermissionManager;
use crate::plan::PlanManager;
use crate::profile::ProfileManager;
use crate::reverse_rpc::ReverseRpcCallback;
use crate::session::events::{EventBus, SessionEvent};
use crate::task::{AgentTask, TaskManager, TaskStatus};
use crate::telemetry::{TelemetryEvent, TelemetryService};
use crate::todo::TodoList;
use crate::tool_registry::{ToolManager, ValidatorCache};
use crate::tool_result_truncation::ToolResultTruncator;
use mirri_proto::WireOp;

/// Configuration for `TurnFlow`.
#[derive(Debug, Clone)]
pub struct TurnFlowConfig {
    /// Maximum number of steps per turn (None or 0 = unlimited).
    pub max_steps: Option<u32>,
    /// Maximum retry attempts for `chat_with_retry`.
    pub max_retry_attempts: u32,
    /// Compaction strategy (trigger ratios, limits, etc.).
    pub compaction_strategy: CompactionStrategy,
}

impl Default for TurnFlowConfig {
    fn default() -> Self {
        Self {
            // TS parity (`maxStepsPerTurn: 0`): unlimited by default —
            // runaway turns are bounded by the caller's watchdogs and
            // cancel tokens, not a step cap that kills legitimate long
            // agentic turns. Users opt into a limit via
            // `[loop_control] max_steps_per_turn` in config.toml.
            max_steps: None,
            max_retry_attempts: 10,
            compaction_strategy: CompactionStrategy::default(),
        }
    }
}

/// Result of a completed turn.
#[derive(Debug, Clone)]
pub struct TurnEndResult {
    /// The allocated turn ID.
    pub turn_id: u64,
    /// Why the turn stopped.
    pub stop_reason: TurnStopReason,
    /// Number of steps executed.
    pub steps: u32,
    /// Aggregated token usage.
    pub usage: LlmUsage,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the turn was blocked by a hook.
    ///
    /// When the `before_step` hook blocks a step (returns `block: true`),
    /// `run_one_turn` catches the `EngineError::HookBlocked` error and returns
    /// a `TurnEndResult` with this flag set to `true` and
    /// `stop_reason: TurnStopReason::BlockedByHook`, instead of propagating
    /// the error. This lets callers distinguish hook-blocked turns from
    /// generic errors. A blocking `UserPromptSubmit` external hook sets the
    /// same flag (the turn ends before any LLM step runs).
    pub blocked_by_prompt_hook: bool,
}

/// The stateful turn orchestrator.
///
/// Wraps the stateless loop engine with shared state for steering, compaction,
/// deduplication, and injection. The hooks (`TurnFlowHooks`) access the same
/// shared state via `Arc<Mutex<>>` clones.
///
/// # Lock Ordering
///
/// When multiple locks must be held simultaneously, they must be acquired
/// in this order to prevent deadlock:
///
/// 1. `TurnFlow` (outer mutex in `embedded_server.rs`)
/// 2. `compaction` (Arc<Mutex<FullCompaction>>)
/// 3. `steer_buffer` (Arc<Mutex<SteerBuffer>>)
/// 4. `memory` (Arc<Mutex<ContextMemory>>)
/// 5. `deduper` (local Mutex<Deduper>)
///
/// All locks are held only for short synchronous critical sections,
/// never across `.await` points.
pub struct TurnFlow {
    /// Next turn ID to allocate (monotonic). The LOCAL fallback, used only
    /// when no session-scoped source is wired (standalone tests, hosts
    /// without a session): a flow in a session allocates through
    /// [`Self::turn_id_source`] instead, so the ids continue the session's
    /// single numeric namespace across flow rebuilds and restarts.
    next_turn_id: u64,
    /// The session-scoped turn-id allocator this flow draws from (see
    /// [`SessionArtifacts::turn_id_counter`](crate::session::SessionArtifacts::turn_id_counter)).
    /// `None` (standalone flows, tests) keeps the local counter above —
    /// the numbering is then per-flow, never persisted.
    turn_id_source: Option<Arc<AtomicU64>>,
    /// Shared steer buffer (mid-turn user interjections).
    steer_buffer: Arc<Mutex<SteerBuffer>>,
    /// The active turn ID, if any.
    active_turn_id: Option<u64>,
    /// Shared compaction orchestrator.
    compaction: Arc<Mutex<FullCompaction>>,
    /// Configuration.
    config: TurnFlowConfig,
    /// Optional permission manager for tool-call authorization.
    ///
    /// When set, the `authorize_tool_execution` hook delegates to the
    /// permission manager's 19-policy chain. When `None`, all tool calls
    /// are auto-approved.
    permission_manager: Option<Arc<PermissionManager>>,
    /// Optional reverse-RPC callback snapshot for `approval.ask` and
    /// `question.ask`.
    ///
    /// When set, the `authorize_tool_execution` hook uses it to ask the client
    /// for approval when the permission policy returns `Ask`, instead of
    /// short-circuiting to `Blocked` or `Approved`. This is the turn-flow's
    /// per-turn SNAPSHOT (set by hosts at turn dispatch — see
    /// [`TurnFlow::set_reverse_rpc_callback`]); the `AskUserQuestion` tool
    /// uses the session's live slot instead, but both resolve to the same
    /// transport callback.
    reverse_rpc_callback: Option<Arc<dyn ReverseRpcCallback>>,
    /// Optional interaction kernel (B4-a): the permission `Ask` flow's
    /// approval-gate seam. When set, the ask registers with the session's
    /// approval gate (the pending source of truth) and the `approval.decide`
    /// / `interaction.respond` RPCs converge with the reverse-RPC reply on
    /// the gate's oneshot.
    interaction_kernel: Option<Arc<crate::session::interaction::InteractionKernel>>,
    /// Optional suspension-boundary callback (ADR-011): the member's
    /// approval-gate edges report into it so the host publishes the
    /// `subagent.suspended` / `subagent.started` pairing. Wired by the
    /// subagent host at each spawn/resume face (the same injection pattern
    /// as the interaction kernel); `None` — the default, and the main
    /// agent's flow — publishes nothing.
    suspension_notify: Option<crate::subagent::SuspensionNotify>,
    /// The scheduler-only resume feed (ADR-011, F3): the gate's decision-
    /// side wake delivers into this sink instead of `suspension_notify`,
    /// so the public `subagent.started` publishes once, from the turn-side
    /// `Resumed`. Set by hosts that wire a swarm scheduler.
    suspension_resume: Option<crate::subagent::SuspensionNotify>,
    /// Optional goal manager for budget enforcement.
    ///
    /// When set, the `record_step_usage` callback records usage against
    /// the goal's budget, and `should_continue_after_stop` checks goal
    /// status and budget to decide whether to continue the turn.
    goal_manager: Option<Arc<GoalManager>>,
    /// Goal manager wired for usage accounting only (F11): the
    /// `record_step_usage` callback records the goal's running totals
    /// (in memory and on the wire) and reports budget exhaustion at the
    /// step level, but the `should_continue_after_stop` goal-continuation
    /// layer stays off. The embedded main-turn and cron flows use this —
    /// wiring the full `goal_manager` there would flip every turn of a
    /// goal-holding session into goal-driven continuation (a product
    /// decision outside T6's persistence scope).
    goal_usage_accounting: Option<Arc<GoalManager>>,
    /// Optional live-usage accumulator (the agent's
    /// [`AgentUsageService`](crate::usage::AgentUsageService)): each step's
    /// LlmUsage is recorded into it, so `agent.getUsage` and the session
    /// view's usage report live totals without a resume (F5, P7c).
    usage_sink: Option<Arc<Mutex<crate::usage::AgentUsageService>>>,
    /// The `by_model` key used when recording into the usage sink — the
    /// agent's configured model ALIAS (TS parity, N4: the TS kernel records
    /// under `config.model`, the alias — `agent/usage/index.ts`; the
    /// embedded engine's `llm.model_name()` is the *resolved upstream*
    /// provider model when a config alias map is present). `None` falls
    /// back to `llm.model_name()` for standalone flows that never wire an
    /// agent config.
    ///
    /// A shared handle (not a turn-start snapshot), read at each step's
    /// recording: a mid-turn model switch updates it through
    /// [`Self::set_usage_model_key`] (via the switch path's side map —
    /// the turn-long `TurnFlow` mutex cannot be taken mid-turn), so the
    /// usage recorded after the switch point is attributed to the new
    /// model (spec 11 A13). The value the dispatch wires before the turn
    /// is the initial key.
    usage_model_key: Arc<Mutex<Option<String>>>,
    /// Optional telemetry service for emitting structured events.
    ///
    /// When set, the hooks emit `TurnStarted`, `TurnEnded`, `ToolCall`,
    /// and LLM request events. When `None`, no telemetry is emitted.
    telemetry: Option<TelemetryService>,
    /// The session's event bus, when the flow was created in a session
    /// context: the turn-internal auto-compaction lifecycle publishes
    /// `SessionEvent`s here (started/blocked/completed/cancelled) — the
    /// embedded layer cannot observe those, they happen inside the turn
    /// loop. `None` for a standalone flow (no session, no subscribers).
    event_bus: Option<Arc<EventBus>>,
    /// Optional profile manager whose profiles feed capability-gap hints.
    ///
    /// When set, each turn registers a [`CapabilityGapInjector`] that checks
    /// the last user message against the current model's capabilities and
    /// injects a subagent-dispatch hint when a feasible alternative profile
    /// exists. When `None`, no capability-gap injection happens.
    profile_manager: Option<Arc<ProfileManager>>,
    /// Optional model catalog feeding the capability-gap injector's
    /// profile name-pointing: profile default models resolve their real
    /// capabilities through it. `None` (standalone flows, tests) keeps
    /// the previous conservative behavior — no profile is named.
    model_catalog: Option<Arc<dyn crate::model_catalog::ModelCatalog>>,
    /// Optional plan manager for plan-mode injection.
    plan_manager: Option<Arc<PlanManager>>,
    /// Optional todo list for todo-reminder injection.
    todo_list: Option<Arc<TodoList>>,
    /// Persistent permission-mode injector (state survives across turns).
    permission_injector: Option<Arc<PermissionModeInjector>>,
    /// Persistent plan-mode injector (state survives across turns).
    plan_injector: Option<Arc<PlanModeInjector>>,
    /// Persistent goal injector (reads current goal each call).
    goal_injector: Option<Arc<GoalInjector>>,
    /// Persistent plugin `sessionStart` injector (item I) — the dispatch
    /// host mounts it on the MAIN agent's flow with the enabled plugins'
    /// session starts and the session's skill catalog; it injects one
    /// `<plugin_session_start>` reminder per session (history-deduped).
    plugin_session_start_injector: Option<Arc<PluginSessionStartInjector>>,
    /// Persistent todo-list reminder injector.
    todo_injector: Option<Arc<TodoListReminderInjector>>,
    /// US4.2 inject-time gate for the stale-todo reminder: whether
    /// `TodoList` is active on the owning agent's tool face. `None` — the
    /// flow's wiring site did not supply a tool face (subagent host, tests),
    /// so the injector is inert. Evaluated per inject call, not at
    /// construction: a mid-session disable silences the next injection.
    todo_tool_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Optional external-command hook engine, wired via
    /// [`set_external_hooks`](Self::set_external_hooks).
    ///
    /// When set, an [`ExternalHooksBridge`] is chained after the internal
    /// `TurnFlowHooks` in a [`CompositeLoopHooks`], and lifecycle events
    /// (`UserPromptSubmit`, `PreCompact`, `PostCompact`) fire directly on the
    /// engine. When `None`, no external hooks run.
    external_hooks: Option<Arc<HookEngine>>,
    /// Base environment merged into every external hook command invocation.
    external_hook_env: HashMap<String, String>,
    /// The engine's flag registry (the config `[experimental]` layer rides
    /// in it), wired via
    /// [`set_flag_registry`](Self::set_flag_registry). Gates the PreToolUse
    /// `updatedInput` rewriting. `None` (the default) falls back to the
    /// env-only flag chain.
    flag_registry: Option<Arc<crate::flags::FlagRegistry>>,
    /// Step-level retry service, wired into every turn's `run_turn` call.
    ///
    /// Budgets are per-turn (`reset_for_turn` runs before each turn), and the
    /// service is shared with the step loop for the lifetime of this `TurnFlow`
    /// (i.e. per agent).
    step_retry: Arc<StepRetryService>,
    /// Optional wire-op recorder (e.g. the session's `wire.jsonl` writer),
    /// installed via [`set_wire_recorder`](Self::set_wire_recorder).
    ///
    /// When set, every turn's loop events and step usage are persisted
    /// through it. `None` (the default) disables recording — unit tests and
    /// replay never write.
    wire_recorder: Option<Arc<dyn WireOpRecorder>>,
    /// Optional tool result truncator for oversized tool outputs.
    ///
    /// When set, tool results exceeding `MAX_CHARS` are persisted to disk
    /// and replaced with a bounded preview before being appended to context
    /// memory. Mirrors the TS `budgetToolResultForModel` behavior.
    tool_result_truncator: Option<ToolResultTruncator>,
    /// Optional task manager for post-compaction background task injection.
    ///
    /// When set, after each auto-compaction the hooks inject a reminder
    /// listing still-running background tasks so the model does not spawn
    /// duplicates. Mirrors the TS `injectActiveBackgroundTasks` behavior.
    task_manager: Option<Arc<TaskManager>>,
    /// The structured agent-status publisher (CR4), kept so a
    /// [`reset_activity_tracker`](Self::reset_activity_tracker) swap can
    /// re-mirror it into the fresh fold. The live copy the phase
    /// transitions read lives in the tracker's own slot (see
    /// [`ActivityTracker::set_status_publisher`]).
    status_publisher: Option<Arc<crate::session::agent_status::AgentStatusPublisher>>,
    /// The per-agent activity fold backing the `agent.activity` view.
    ///
    /// Holds an [`AgentActivityView`] behind its own mutex (not the
    /// `TurnFlow` mutex) so an `agent.activity` read succeeds while a
    /// fire-mode turn holds this `TurnFlow` locked. Observe-only: the
    /// recordings never alter loop control flow.
    activity: Arc<ActivityTracker>,
    /// The wire turn id (`turn-<uuid>`) of the turn this flow is about to
    /// run — plumbed down from the message-send dispatch (or the subagent
    /// host) via [`set_wire_turn_id`](Self::set_wire_turn_id) so the
    /// turn-internal step events publish the same turn key the wire's
    /// `turn.started` / `turn.ended` carry. `None` for a standalone flow
    /// (tests, no dispatch) — the step events then fall back to the
    /// engine's numeric turn counter (the session's shared allocator when
    /// one is wired, else the flow-local one). Cleared when the turn ends.
    wire_turn_id: Option<String>,
    /// The agent this flow runs as (supervision B1) — the full agent id
    /// string, set by the subagent host at spawn/adopt for member flows;
    /// `None` for the main agent's flow and standalone test flows. Rides
    /// the approval asks the flow issues (see
    /// [`set_agent_id`](Self::set_agent_id)).
    agent_id: Option<String>,
    /// The turn-abort guard (session-consistency invariant 2, spec §2.2
    /// ③): polled by the engine loop before each step and each tool
    /// batch. Installed by the embedded dispatch behind the
    /// `session-consistency-follow` flag via
    /// [`set_turn_abort_guard`](Self::set_turn_abort_guard); a tripped
    /// guard aborts the turn through the existing cancellation path. The
    /// turn's own journal writes advance the anchor synchronously at the
    /// write point, so the guard never sees its own partial writes as
    /// foreign. `None` (the default) keeps the loop guard-free.
    turn_abort_guard: Option<TurnAbortGuard>,
    /// The ADR-006 pull-model wait registry — created per turn in
    /// `run_one_turn_inner`, stored on the flow (alongside
    /// [`Self::activity`]) so the turn's await points (tools, stream
    /// collection, gate waits) and the watchdog both reach the SAME
    /// instance. The watchdog fetches it via
    /// [`Self::wait_registry`](Self::wait_registry) before spawning the
    /// turn and probes it at window expiry.
    wait_registry: Arc<WaitRegistry>,
    /// The fork chain's cache root id (the provider prompt-cache routing
    /// key, TS parity: the TS kernel passes the session id as
    /// `promptCacheKey`, our chain root is the session that started the
    /// fork lineage). Wired by the dispatch host from the session's
    /// persisted `cacheRootId` via [`set_cache_root_id`](Self::set_cache_root_id);
    /// `None` on fresh/legacy sessions — every turn's provider request
    /// then omits the cache-routing field.
    cache_root_id: Option<String>,
    /// The session's pending skill-catalog reminder handle (ADR-010
    /// decision 5: park-and-flush). Wired ONLY into the interactive main
    /// agent's flow — the dispatch's user-message path injects it via
    /// [`set_pending_skill_reminder`](Self::set_pending_skill_reminder);
    /// subagent hosts, cron and goal drivers construct their flows without
    /// it, so they never consume the interactive session's pending signal
    /// nor inject a listing into the wrong context. `None` (the default)
    /// keeps every turn reminder-free.
    pending_skill_reminder: Option<crate::scope::session_scope::PendingSkillReminder>,
}

impl TurnFlow {
    /// Create a new `TurnFlow` with the given configuration.
    pub fn new(config: TurnFlowConfig) -> Self {
        let compaction = Arc::new(Mutex::new(FullCompaction::new(
            config.compaction_strategy.clone(),
        )));
        Self {
            next_turn_id: 1,
            turn_id_source: None,
            steer_buffer: Arc::new(Mutex::new(SteerBuffer::new())),
            active_turn_id: None,
            activity: Arc::new(ActivityTracker::new(Arc::clone(&compaction))),
            compaction,
            config,
            permission_manager: None,
            reverse_rpc_callback: None,
            interaction_kernel: None,
            suspension_notify: None,
            suspension_resume: None,
            goal_manager: None,
            goal_usage_accounting: None,
            usage_sink: None,
            usage_model_key: Arc::new(Mutex::new(None)),
            telemetry: None,
            event_bus: None,
            profile_manager: None,
            model_catalog: None,
            plan_manager: None,
            todo_list: None,
            permission_injector: None,
            plan_injector: None,
            goal_injector: None,
            plugin_session_start_injector: None,
            todo_injector: None,
            todo_tool_gate: None,
            external_hooks: None,
            external_hook_env: HashMap::new(),
            flag_registry: None,
            step_retry: Arc::new(StepRetryService::with_defaults()),
            wire_recorder: None,
            tool_result_truncator: None,
            task_manager: None,
            status_publisher: None,
            wire_turn_id: None,
            agent_id: None,
            turn_abort_guard: None,
            wait_registry: Arc::new(WaitRegistry::new()),
            cache_root_id: None,
            pending_skill_reminder: None,
        }
    }

    /// Whether a turn is currently active.
    pub fn is_turn_active(&self) -> bool {
        self.active_turn_id.is_some()
    }

    /// The active turn ID, if any.
    pub fn active_turn_id(&self) -> Option<u64> {
        self.active_turn_id
    }

    /// Allocate the next turn ID.
    ///
    /// Draws from the session-scoped source when one is wired
    /// ([`Self::set_turn_id_source`]) — every flow of the session (main,
    /// cron/task, subagent) shares it, so flow disposal and rebuild never
    /// reset the numbering and no `turn.ended` id the wire log already holds
    /// is minted again. Without a source (standalone flows, tests) the
    /// flow-local counter applies.
    fn allocate_turn_id(&mut self) -> u64 {
        match &self.turn_id_source {
            Some(source) => source.fetch_add(1, Ordering::Relaxed),
            None => {
                let id = self.next_turn_id;
                self.next_turn_id += 1;
                id
            }
        }
    }

    /// Steer the active turn by buffering a message.
    ///
    /// If a turn is active, the message is buffered and will be flushed at the
    /// next `before_step` hook. If no turn is active, the message is dropped —
    /// the caller should launch a new turn with the message as the initial
    /// prompt instead.
    pub fn steer(&mut self, content: Vec<ContentPart>, origin: PromptOrigin) {
        if self.is_turn_active() {
            self.steer_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(content, origin);
        }
        // If no turn is active, the steer is silently dropped. The caller
        // should check `is_turn_active()` before calling `steer()` and launch
        // a new turn if needed.
    }

    /// The shared steer-buffer handle.
    ///
    /// Hosts that park steers while a turn is running must NOT go through
    /// [`Self::steer`]: the [`TurnFlow`] mutex is held for the whole turn
    /// by the run loop, so a mid-turn `steer()` would block until the turn
    /// ends (parking the content one turn late) or deadlock a
    /// single-threaded runtime. This handle pushes straight into the same
    /// buffer the turn's hooks flush, without contending the turn-long
    /// mutex.
    pub fn steer_buffer_handle(&self) -> Arc<Mutex<SteerBuffer>> {
        Arc::clone(&self.steer_buffer)
    }

    /// Flush the steer buffer into context memory.
    ///
    /// Returns `true` if any steers were flushed.
    pub fn flush_steer_buffer(&self, memory: &mut ContextMemory) -> bool {
        self.steer_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush(memory)
    }

    /// Whether the steer buffer has pending steers.
    pub fn has_pending_steers(&self) -> bool {
        !self
            .steer_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Set the permission manager for tool-call authorization.
    ///
    /// When set, the `authorize_tool_execution` hook delegates to the
    /// permission manager's policy chain. This should be called before
    /// starting a turn.
    pub fn set_permission_manager(&mut self, pm: Arc<PermissionManager>) {
        self.permission_injector = Some(Arc::new(PermissionModeInjector::new(pm.clone())));
        self.permission_manager = Some(pm);
    }

    /// Wire (or clear) the reverse-RPC callback for `approval.ask` and
    /// `question.ask`.
    ///
    /// When `Some`, the `authorize_tool_execution` hook uses it to ask the
    /// client for approval when the permission policy returns `Ask`. When
    /// `None`, the hook falls back to the existing behavior (headless →
    /// Approved, non-headless → Blocked). Should be called before starting a
    /// turn. Passing `None` clears any callback from a previous turn, ensuring
    /// stale callbacks do not persist.
    pub fn set_reverse_rpc_callback(&mut self, callback: Option<Arc<dyn ReverseRpcCallback>>) {
        self.reverse_rpc_callback = callback;
    }

    /// Set the interaction kernel for the permission `Ask` flow (B4-a).
    ///
    /// When set, approval asks register with the session's approval gate
    /// (the pending source of truth) before the reverse RPC is issued, and
    /// the gate receiver joins the ask's select so `approval.decide` /
    /// `interaction.respond` resolve the ask directly. `None` (the default)
    /// keeps the pure reverse-RPC ask. Should be called before starting a
    /// turn, like [`Self::set_reverse_rpc_callback`].
    pub fn set_interaction_kernel(
        &mut self,
        kernel: Option<Arc<crate::session::interaction::InteractionKernel>>,
    ) {
        self.interaction_kernel = kernel;
    }

    /// Set the suspension-boundary callback (ADR-011): the child turn's
    /// approval-gate register/resolve edges report into it so the host
    /// publishes the `subagent.suspended` / `subagent.started` pairing.
    /// `None` (the default) keeps the flow silent about suspensions — the
    /// main agent's flow and standalone test flows never wire one. Should
    /// be called before starting a turn, like
    /// [`Self::set_interaction_kernel`].
    pub fn set_suspension_notify(&mut self, notify: Option<crate::subagent::SuspensionNotify>) {
        self.suspension_notify = notify;
    }

    /// Set the SCHEDULER-ONLY resume feed (ADR-011, F3): the gate's
    /// decision-side wake delivers into this sink (no bus publish) so the
    /// public `subagent.started` publishes once, from the turn-side
    /// `Resumed`, when the member actually resumes running. `None` (the
    /// default) keeps the decide-side wake silent.
    pub fn set_suspension_resume(&mut self, resume: Option<crate::subagent::SuspensionNotify>) {
        self.suspension_resume = resume;
    }

    /// Set the goal manager for budget enforcement.
    ///
    /// When set, the `record_step_usage` callback records usage against
    /// the goal's budget, and `should_continue_after_stop` checks goal
    /// status and budget. This should be called before starting a turn.
    /// Wire a goal manager for usage accounting only (F11): the turn's
    /// steps record the goal's running totals and enforce budget
    /// exhaustion at the step level, without enabling the
    /// `should_continue_after_stop` goal-continuation layer.
    pub fn set_goal_usage_accounting(&mut self, gm: Arc<GoalManager>) {
        self.goal_usage_accounting = Some(gm);
    }

    pub fn set_goal_manager(&mut self, gm: Arc<GoalManager>) {
        self.goal_injector = Some(Arc::new(GoalInjector::new(gm.clone())));
        self.goal_manager = Some(gm);
    }

    /// Mount or unmount the plugin `sessionStart` injector (item I). The
    /// dispatch host mounts it on the main agent's flow with the enabled
    /// plugins' session starts and the session's skill catalog; `None`
    /// unmounts (non-main agents, sessions without plugin session starts).
    /// The injector itself is idempotent per session (history-deduped), so a
    /// re-mount on a later turn never duplicates the reminder.
    pub fn set_plugin_session_start_injector(
        &mut self,
        injector: Option<Arc<PluginSessionStartInjector>>,
    ) {
        self.plugin_session_start_injector = injector;
    }

    /// Set the live-usage accumulator (the agent's
    /// [`AgentUsageService`](crate::usage::AgentUsageService)): each step's
    /// usage is recorded into it (F5, P7c), so usage views report live
    /// totals without a resume.
    pub fn set_usage_sink(&mut self, sink: Arc<Mutex<crate::usage::AgentUsageService>>) {
        self.usage_sink = Some(sink);
    }

    /// Set the `by_model` key the usage sink records under — the agent's
    /// configured model ALIAS (TS parity, N4; see [`Self::usage_model_key`]).
    /// Wire it alongside [`Self::set_usage_sink`] wherever the agent config
    /// is in scope; without it the fallback is `llm.model_name()`. Safe to
    /// call mid-turn through the shared handle (see
    /// [`Self::usage_model_key_handle`]): the turn's step-recording closure
    /// reads the handle at each step, so the update applies from the next
    /// recorded usage onward.
    pub fn set_usage_model_key(&mut self, key: impl Into<String>) {
        *self
            .usage_model_key
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(key.into());
    }

    /// A shared handle to the usage sink's `by_model` key (the alias M1) —
    /// the same `Arc<Mutex<Option<String>>>` the turn's step-recording
    /// closure reads. Hosts that cannot take the `TurnFlow` mutex mid-turn
    /// (a running turn holds it for the whole turn) push a model switch's
    /// new alias through this handle instead, mirroring
    /// [`Self::compaction_handle`].
    pub fn usage_model_key_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.usage_model_key)
    }

    /// Set the telemetry service for emitting structured events.
    ///
    /// When set, the hooks emit `TurnStarted`, `TurnEnded`, `ToolCall`,
    /// and LLM request events to the service. This should be called before
    /// starting a turn.
    pub fn set_telemetry(&mut self, telemetry: TelemetryService) {
        self.telemetry = Some(telemetry);
    }

    /// Set the session event bus the turn-internal auto-compaction
    /// lifecycle publishes on. Set when the flow is created inside a
    /// session (the embedded engine wires its session's bus here); a
    /// standalone flow leaves it `None` and stays silent.
    pub fn set_event_bus(&mut self, event_bus: Arc<EventBus>) {
        self.event_bus = Some(event_bus);
    }

    /// Set the wire turn id (`turn-<uuid>`) the next `run_one_turn` on this
    /// flow publishes its step events under. Call before running the turn —
    /// the message-send dispatch (and the subagent host) mint the id where
    /// `turn.started` / `turn.ended` do, and this plumbs it down to the
    /// turn loop so `turn.step.*` carries the same key. Cleared when the
    /// turn ends (a reused flow's next turn must set a fresh id, never
    /// silently inherit the previous one).
    pub fn set_wire_turn_id(&mut self, wire_turn_id: Option<String>) {
        self.wire_turn_id = wire_turn_id;
    }

    /// Set the agent this flow runs as (supervision B1) — the full agent id
    /// string. The subagent host calls this at spawn/adopt so member flows
    /// attribute their approval asks; the embedded main path pins the
    /// session's agent id at flow creation (`AgentId::as_str()`, `"main"`
    /// for the main agent), so production flows always carry a real id.
    pub fn set_agent_id(&mut self, agent_id: Option<String>) {
        self.agent_id = agent_id;
    }

    /// Set the fork chain's cache root id (the provider prompt-cache
    /// routing key) for every turn this flow runs. The dispatch host wires
    /// it from the session's persisted `cacheRootId` (TS parity: the TS
    /// kernel passes the session id as `promptCacheKey`; our chain root is
    /// the session that started the fork lineage, so nested forks keep
    /// routing at the same cache shard). `None` — the default — omits the
    /// provider's cache-routing field on every request.
    pub fn set_cache_root_id(&mut self, cache_root_id: Option<String>) {
        self.cache_root_id = cache_root_id;
    }

    /// Wire the session's pending skill-catalog reminder handle (ADR-010
    /// decision 5: park-and-flush). Call before running the turn — the
    /// interactive main agent's dispatch (the user-message path in the
    /// embedded message handlers) injects it where the flow is created, so
    /// the turn-start flush consumes the session's parked refresh and
    /// appends the current listing as a system reminder. Flows that do NOT
    /// carry the handle (subagent hosts, cron and goal drivers, tests)
    /// neither consume the pending nor inject — the wiring scope IS the
    /// gate. `None` removes a previously installed handle.
    pub fn set_pending_skill_reminder(
        &mut self,
        handle: Option<crate::scope::session_scope::PendingSkillReminder>,
    ) {
        self.pending_skill_reminder = handle;
    }

    /// Install the turn-abort guard (session-consistency invariant 2, spec
    /// §2.2 ③): the predicate the engine loop polls before each step and
    /// each tool batch. Call before running the turn — the embedded
    /// dispatch installs it behind the `session-consistency-follow` flag
    /// with a single-`stat` main-journal probe against the session's
    /// known-size anchor (the same verdict Task 3's submit fence applies;
    /// a common helper decides both). A tripped guard aborts the turn
    /// through the existing cancellation path (`TurnStopReason::Aborted`,
    /// wire reason `cancelled`), preserving the already-recorded partial
    /// work (方式 B append-only closure). `None` removes a previously
    /// installed guard.
    pub fn set_turn_abort_guard(&mut self, guard: Option<TurnAbortGuard>) {
        self.turn_abort_guard = guard;
    }

    /// Wire the session-scoped turn-id allocator this flow draws from — the
    /// handle [`SessionArtifacts::turn_id_counter`] hands out (see the
    /// struct-level docs of [`crate::session::SessionArtifacts`]). Every
    /// creation point of a session's flows wires the same handle, so the
    /// session's `turn.ended` ids form ONE numeric namespace: a close →
    /// resume (the fresh flow) continues the replayed wire log's maximum
    /// instead of restarting at 1, and duplicate numeric ids can never make a
    /// turn-addressable surface (fork's `turnId` anchor, undo-by-turn)
    /// ambiguous about which bracket a caller means. The source is seeded
    /// once by the session manager at resume; this call only points the flow
    /// at it.
    pub fn set_turn_id_source(&mut self, source: Arc<AtomicU64>) {
        self.turn_id_source = Some(source);
    }

    /// The flow's agent identity for event attribution — the full agent id
    /// string, defaulting to `"main"` when none was set. `None ≡ "main"` is
    /// the single-agent-session semantics: a flow that never had an identity
    /// pinned (a standalone test flow, or a host that predates attribution)
    /// is by definition the session's main agent. The single read point for
    /// the turn-internal compaction emit sites (the hooks path reads the
    /// same value through [`TurnFlowHooksConfig::agent_id`], normalized the
    /// same way) so every event a flow publishes names one consistent agent.
    /// The embedded main path now sets the id explicitly at flow creation,
    /// making `None` a defensive floor rather than an expected production
    /// state — the subagent host already sets real ids.
    pub fn agent_id_or_main(&self) -> String {
        self.agent_id.clone().unwrap_or_else(|| "main".to_string())
    }

    /// Wire a profile manager so turns inject capability-gap hints.
    ///
    /// When set, every turn registers a [`CapabilityGapInjector`] built from
    /// the current model's capabilities and the manager's profiles. This
    /// should be called before starting a turn; profile changes after this
    /// call are reflected per turn (the injector is rebuilt each
    /// `run_step_loop`).
    pub fn set_profile_manager(&mut self, profile_manager: Arc<ProfileManager>) {
        self.profile_manager = Some(profile_manager);
    }

    /// Wire the host's model catalog so capability-gap hints resolve
    /// profile default-model capabilities for real (closes the TODO at
    /// the injector wiring site).
    pub fn set_model_catalog(&mut self, catalog: Option<Arc<dyn ModelCatalog>>) {
        self.model_catalog = catalog;
    }

    /// Set the plan manager for plan-mode injection.
    pub fn set_plan_manager(&mut self, pm: Arc<PlanManager>) {
        self.plan_injector = Some(Arc::new(PlanModeInjector::new(pm.clone())));
        self.plan_manager = Some(pm);
    }

    /// Set the todo list for todo-reminder injection.
    pub fn set_todo_list(&mut self, todo: Arc<TodoList>) {
        self.todo_list = Some(Arc::clone(&todo));
        self.rebuild_todo_injector();
        // US3.1 + Task 4 follow-up: the compaction orchestrator shares the
        // same session-scoped handle — non-empty lists are appended to
        // compaction summaries, and each live apply clears the undo
        // checkpoint stack (the memory-side mirror of the reducer's
        // `context.apply_compaction` rule).
        self.compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_todo_list(Some(todo));
    }

    /// US4.2: gate the stale-todo reminder on the `TodoList` tool being
    /// active on the owning agent's tool face. The gate is evaluated at
    /// every inject call (a mid-session disable silences the next
    /// injection), and must take its own lock per call. Call order with
    /// [`set_todo_list`](Self::set_todo_list) does not matter; without a
    /// gate (default, subagent host) the injector is inert.
    pub fn set_todo_tool_gate(&mut self, gate: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.todo_tool_gate = Some(gate);
        self.rebuild_todo_injector();
    }

    /// Rebuild the persistent todo-list reminder injector from the current
    /// `todo_list` + `todo_tool_gate` fields. Holding the gate on the flow
    /// and rebuilding on each setter keeps both setters independent of call
    /// order, and the assembly site (line ~1646) keeps consuming
    /// `self.todo_injector` unchanged.
    fn rebuild_todo_injector(&mut self) {
        self.todo_injector = self.todo_list.as_ref().map(|todo| {
            let injector = TodoListReminderInjector::new(Arc::clone(todo));
            match &self.todo_tool_gate {
                Some(gate) => Arc::new(injector.with_tool_gate(Arc::clone(gate))),
                None => Arc::new(injector),
            }
        });
    }

    /// Set the task manager for post-compaction background task injection.
    ///
    /// When set, after each auto-compaction the hooks inject a reminder
    /// listing still-running background tasks so the model does not spawn
    /// duplicates. This should be called before starting a turn.
    pub fn set_task_manager(&mut self, task_manager: Arc<TaskManager>) {
        // Mirror the manager into the activity fold so its `background`
        // derivation (running tasks) sees the same source.
        self.activity
            .set_task_manager(Some(Arc::clone(&task_manager)));
        self.task_manager = Some(task_manager);
    }

    /// Wire (or clear) the structured agent-status publisher (CR4) — the
    /// `event.agent.status.updated` emitter the activity fold's phase
    /// transitions drive. Should be called before starting a turn, like
    /// [`Self::set_usage_sink`]; the publisher reads live handles (agent
    /// config, usage service, compaction window), so a re-wire is an
    /// idempotent overwrite and mid-turn model switches need no re-wire.
    pub fn set_status_publisher(
        &mut self,
        publisher: Option<Arc<crate::session::agent_status::AgentStatusPublisher>>,
    ) {
        self.activity.set_status_publisher(publisher.clone());
        self.status_publisher = publisher;
    }

    /// A shared handle to the per-agent activity fold backing the
    /// `agent.activity` view. Cheap to clone; snapshots never contend with
    /// a running turn (the fold has its own mutex).
    pub fn activity_tracker(&self) -> Arc<ActivityTracker> {
        Arc::clone(&self.activity)
    }

    /// A shared handle to the CURRENT turn's wait registry (ADR-006) —
    /// the pull-model fact source the watchdog probes at window expiry.
    /// Recreated per turn (see [`Self::reset_wait_registry`]).
    pub fn wait_registry(&self) -> Arc<WaitRegistry> {
        Arc::clone(&self.wait_registry)
    }

    /// Replace the wait registry with a fresh (empty) one and return it —
    /// the per-turn reset, run at the top of `run_one_turn_inner` so a
    /// stale entry from a prior turn never leaks into the next turn's
    /// wait set.
    pub fn reset_wait_registry(&mut self) -> Arc<WaitRegistry> {
        let registry = Arc::new(WaitRegistry::new());
        self.wait_registry = Arc::clone(&registry);
        registry
    }

    /// Replace the activity fold with a fresh (ready, empty) tracker and
    /// return it.
    ///
    /// Used when a session that was closed/archived (its view disposed)
    /// is restored: the restored agent mirrors the TS positioning — the
    /// view dies with the agent, and a restored agent gets a new view. The
    /// old fold owns no authoritative state, so discarding it is safe. The
    /// caller holds the `TurnFlow` lock; a turn running on the old fold
    /// would keep recording into it (its hooks hold the old `Arc`), which
    /// is only reachable by restoring a session while one of its agents
    /// still has a turn in flight — a pathological call pattern.
    pub fn reset_activity_tracker(&mut self) -> Arc<ActivityTracker> {
        let tracker = Arc::new(ActivityTracker::new(Arc::clone(&self.compaction)));
        if let Some(tm) = &self.task_manager {
            tracker.set_task_manager(Some(Arc::clone(tm)));
        }
        // The publisher survives the fold swap — a restored agent's turns
        // keep publishing their status on the same session bus.
        if let Some(publisher) = &self.status_publisher {
            tracker.set_status_publisher(Some(Arc::clone(publisher)));
        }
        self.activity = Arc::clone(&tracker);
        tracker
    }

    /// Wire the session's external-command hook engine into this turn flow.
    ///
    /// When set, every turn chains an [`ExternalHooksBridge`] after the
    /// internal `TurnFlowHooks` so user-configured command hooks fire at the
    /// loop's lifecycle points, and the `UserPromptSubmit` /
    /// `PreCompact` / `PostCompact` events fire directly on the engine.
    ///
    /// `base_env` is merged into every hook command's environment on top of
    /// the hook's own `env` map and the process environment. Calling this
    /// again replaces the engine and base env (hooks registered on the engine
    /// after this call are still picked up — the bridge reads the engine at
    /// trigger time). This should be called before starting a turn.
    pub fn set_external_hooks(
        &mut self,
        engine: Arc<HookEngine>,
        base_env: HashMap<String, String>,
    ) {
        self.external_hooks = Some(engine);
        self.external_hook_env = base_env;
    }

    /// Install the engine's flag registry so the turn's hooks resolve
    /// experimental flags through the env → config → defaults chain
    /// (the composition root seeds the config `[experimental]` layer into
    /// the registry). Without one the flows fall back to the env-only
    /// chain. Should be called before starting a turn; the registry stays
    /// installed for all subsequent turns until replaced.
    pub fn set_flag_registry(&mut self, registry: Arc<crate::flags::FlagRegistry>) {
        self.flag_registry = Some(registry);
    }

    /// Install a wire-op recorder so loop events and step usage produced by
    /// every turn are persisted (e.g. to the session's `wire.jsonl`).
    ///
    /// `None` disables recording — the default, used by unit tests and
    /// replay. Should be called before starting a turn; the recorder stays
    /// installed for all subsequent turns until replaced.
    pub fn set_wire_recorder(&mut self, recorder: Option<Arc<dyn WireOpRecorder>>) {
        self.wire_recorder = recorder;
    }

    /// Wire a tool result truncator so oversized tool outputs are persisted
    /// to disk and replaced with a bounded preview before entering context
    /// memory.
    pub fn set_tool_result_truncator(&mut self, truncator: Option<ToolResultTruncator>) {
        self.tool_result_truncator = truncator;
    }

    /// The directory oversized tool results are persisted under, when the
    /// truncator is wired (`None` when truncation is off). Test/inspection
    /// seam — production code configures via [`Self::set_tool_result_truncator`].
    pub fn tool_result_truncator_dir(&self) -> Option<&std::path::Path> {
        self.tool_result_truncator.as_ref().map(|t| t.home_dir.as_path())
    }

    /// Update the compaction window on the live flow (model switch) —
    /// mutates the shared FullCompaction in place so the ActivityTracker's
    /// clone of the Arc observes the same engine. Takes `&self` (only the
    /// inner compaction mutex is touched) so mid-turn model switches can
    /// update the window without contending the turn-long TurnFlow mutex.
    pub fn set_compaction_max_context_tokens(&self, tokens: u64) {
        self.compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_max_context_tokens(tokens);
    }

    /// Get a shared handle to the compaction orchestrator.
    ///
    /// Used by manual compaction dispatch to apply results from a background
    /// task after the RPC has returned.
    pub fn compaction_handle(&self) -> Arc<Mutex<FullCompaction>> {
        Arc::clone(&self.compaction)
    }

    /// Request a manual compaction (triggered by `beginCompaction` RPC).
    ///
    /// This starts a fire-and-forget background compaction task. The caller
    /// should spawn a separate task to await the returned
    /// [`ManualCompactionHandle`], apply the result to memory, and emit
    /// events. The handle carries the attempt's cancellation channel and
    /// identity — the apply task must refuse to apply once the channel is
    /// cancelled, and settle the orchestrator's registration with the
    /// attempt id so a stale attempt cannot clear a newer one's.
    ///
    /// Returns `Err` if:
    /// - A turn is currently active (manual compaction requires a clean
    ///   boundary — the turn mutates context while the summarizer is in
    ///   flight, and the output would be neither summarized nor preserved).
    /// - A compaction is already in flight (a running turn's auto
    ///   compaction, or a manual one whose task handle this method
    ///   previously handed out).
    /// - The context history is empty.
    ///
    /// # Lock ordering (SDR-012)
    ///
    /// The caller must already hold the `TurnFlow` outer mutex (the
    /// turn-active check is only atomic against turn activation under it).
    /// The memory guard is acquired INSIDE, after the compaction
    /// orchestrator — the documented order (TurnFlow → compaction → … →
    /// memory). Taking the memory guard before calling this method is an
    /// ABBA against the apply task (`apply_compaction_result` holds
    /// compaction while taking memory), so the parameter is the shared
    /// `Arc`, not a borrowed guard.
    pub fn request_manual_compaction(
        &self,
        memory: &Arc<Mutex<ContextMemory>>,
        llm: Arc<dyn Llm>,
        instruction: Option<String>,
        max_output_size: Option<u64>,
        hook_additional_context: Vec<ContextMessage>,
    ) -> EngineResult<ManualCompactionHandle> {
        if self.is_turn_active() {
            return Err(EngineError::CompactionFailed(
                "cannot compact while a turn is active".to_string(),
            ));
        }
        let cancel = CancelToken::new();
        let mut comp = self.compaction.lock().unwrap_or_else(|e| e.into_inner());
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        // Check in-flight state before calling begin: a live auto
        // compaction (is_compacting) or a manual one still running
        // (manual_cancel — its handle lives with the caller's apply task).
        if comp.is_compacting() || comp.manual_in_flight() {
            return Err(EngineError::CompactionFailed(
                "a compaction is already in flight".to_string(),
            ));
        }
        comp.begin(
            CompactionSource::Manual,
            CompactionBeginParams {
                llm,
                cancel: cancel.clone(),
                custom_instruction: instruction,
                max_output_size,
                hook_additional_context,
            },
            &memory,
        )?;
        // The activity view's background derivation reads the orchestrator
        // live; the start timestamp is recorded here so the entry carries a
        // real `since`.
        self.activity.on_compaction_started();
        comp.take_manual_task().ok_or_else(|| {
            EngineError::CompactionFailed("begin() did not start a compaction task".to_string())
        })
    }

    /// Cancel an in-flight manual compaction.
    ///
    /// Returns `true` if a manual compaction was in flight and cancelled.
    /// A compaction running inside a turn is owned by that turn's cancel
    /// token (the turn loop holds its task handle), so it is not reachable
    /// from here.
    pub fn cancel_compaction(&self) -> bool {
        let mut comp = self.compaction.lock().unwrap_or_else(|e| e.into_inner());
        comp.cancel_manual()
    }

    /// Run a single turn.
    ///
    /// Appends the user message, runs the step loop (with hooks), and returns
    /// the turn result. The LLM provider is resolved through `llm_resolver`
    /// at every loop step, so a mid-turn model switch applies on the next
    /// LLM call (see [`crate::loop_engine::run_turn`]).
    ///
    /// `with_skills` is the ADR-006 admission stamp: the deduped activated
    /// skill list attached to THIS user prompt's in-memory `ContextMessage`
    /// (product-record only — never projected to the model). `None` leaves
    /// the field absent, exactly like every non-inline turn.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_one_turn(
        &mut self,
        prompt: Vec<ContentPart>,
        origin: PromptOrigin,
        memory: Arc<Mutex<ContextMemory>>,
        tools: &ToolManager,
        llm_resolver: Arc<dyn LlmResolver>,
        cancel: &CancelToken,
        validators: &mut ValidatorCache,
        live_callback: Option<LiveEventCallback>,
        with_skills: Option<Vec<crate::message::WithSkillsEntry>>,
    ) -> EngineResult<TurnEndResult> {
        self.run_one_turn_inner(
            Some(prompt),
            origin,
            memory,
            tools,
            llm_resolver,
            cancel,
            validators,
            live_callback,
            // `Some` tri-state only *inside* `run_one_turn_inner`; here the
            // caller picked the append shape, so `None` means "no stamp".
            Some(with_skills),
        )
        .await
    }

    /// Re-run the previously failed turn WITHOUT appending a new user
    /// message — the rate-limit requeue path (TS `promptService.retry()`,
    /// runAgentTurn.ts:52-60). Same turn machinery as [`Self::run_one_turn`]
    /// (compaction settle, activity, live events, the step loop); the
    /// memory's last user message drives the loop.
    pub async fn retry_one_turn(
        &mut self,
        memory: Arc<Mutex<ContextMemory>>,
        tools: &ToolManager,
        llm_resolver: Arc<dyn LlmResolver>,
        cancel: &CancelToken,
        validators: &mut ValidatorCache,
        live_callback: Option<LiveEventCallback>,
    ) -> EngineResult<TurnEndResult> {
        self.run_one_turn_inner(
            None,
            // Unused when no prompt rides along (the UserPromptSubmit hook
            // only fires for `User` origins and no message is appended);
            // SystemTrigger mirrors the subagent spawn face.
            PromptOrigin::SystemTrigger,
            memory,
            tools,
            llm_resolver,
            cancel,
            validators,
            live_callback,
            // Retry shape: no new user message to stamp — the last message
            // already carries (or lacks) its admission stamp.
            None,
        )
        .await
    }

    /// The shared turn body — `prompt = None` is the retry shape: the
    /// UserPromptSubmit hook block and the user-message append are skipped,
    /// everything else (buffers, activity, step loop, compaction recovery)
    /// is identical. `with_skills` is tri-state: the outer `Option` is
    /// `None` exactly when `prompt` is `None` (retry — nothing to stamp);
    /// when the prompt rides along, it is the ADR-006 stamp itself.
    #[allow(clippy::too_many_arguments)]
    async fn run_one_turn_inner(
        &mut self,
        prompt: Option<Vec<ContentPart>>,
        origin: PromptOrigin,
        memory: Arc<Mutex<ContextMemory>>,
        tools: &ToolManager,
        llm_resolver: Arc<dyn LlmResolver>,
        cancel: &CancelToken,
        validators: &mut ValidatorCache,
        live_callback: Option<LiveEventCallback>,
        with_skills: Option<Option<Vec<crate::message::WithSkillsEntry>>>,
    ) -> EngineResult<TurnEndResult> {
        // Buffer phase (TS parity: `promptService.startNext` defers while a
        // full compaction is in flight): wait for any in-flight manual
        // compaction to settle before the turn appends its prompt. The
        // compaction's apply rebuilds memory from the snapshot taken at
        // `begin`, so a turn appending during the window would be clobbered.
        // This runs under the turn-flow mutex (the engine-loop wrapper holds
        // it for the whole turn), so no new manual compaction can begin
        // behind the check — `request_manual_compaction` rejects while a
        // turn is active and needs the same mutex.
        crate::compaction::wait_manual_compaction_settle(&self.compaction).await;

        // ADR-010 decision 5: flush the session's parked skill-catalog
        // refresh BEFORE the turn starts (ahead of the activity tracker and
        // the user-message append, so the reminder rides the conversation
        // the turn's first step sees). Gated on the tool face actually
        // carrying the Skill tool — a session that cannot invoke skills
        // gets no listing (the pending flag stays parked for a future
        // turn whose face does carry it). A flow without the handle (a
        // subagent/cron/goal turn) flushes nothing by construction.
        if let Some(handle) = &self.pending_skill_reminder
            && tools.is_tool_active("Skill")
            && handle.take_pending()
        {
            let reminder = render_skill_catalog_refresh_reminder(handle.catalog());
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_system_reminder(reminder.clone());
            crate::system_reminder_wire::record_system_reminders(
                self.wire_recorder.as_deref(),
                &[reminder],
            )
            .await;
        }

        let turn_id = self.allocate_turn_id();
        self.active_turn_id = Some(turn_id);
        self.activity.on_turn_started(turn_id, origin.clone());
        self.compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset_for_turn();
        // ADR-006: the per-turn wait registry. The DISPATCH resets it
        // (under its own flow lock, BEFORE the watchdog takes its probe
        // handle — see run_engine_turn's closure) so the deadline loop and
        // this turn's registrations share one instance. Standalone callers
        // (tests, the subagent host's direct run_one_turn) reach here with
        // the flow's previous registry: a clean (entry-less) one is safe
        // to reuse; anything else (a stale guard from an aborted prior
        // turn) mints a fresh one.
        let wait_registry = {
            let existing = Arc::clone(&self.wait_registry);
            if existing.entry_count() == 0 {
                existing
            } else {
                self.reset_wait_registry()
            }
        };

        // Wrap the host's live callback so the activity fold observes the
        // loop's live events (`StepRetrying` is the only retry observation
        // point — the backoff lives inside `chat_with_retry`, behind no
        // hook; `TurnInterrupted` marks the abort ending). Observe-only:
        // the wrap forwards every event unchanged.
        let live_callback = live_callback.map(|cb| {
            let tracker = Arc::clone(&self.activity);
            Arc::new(move |event: LiveEvent| {
                tracker.observe_live(&event);
                cb(event);
            }) as LiveEventCallback
        });
        let live_ref = live_callback.as_ref();

        // Apply the strategy's message-count ceilings to the memory so the
        // configured caps (defaults: 10000 history / 1000 deferred) take
        // effect on every turn. Existing messages are never evicted — only
        // future appends are gated.
        {
            let mut memory_guard = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory_guard.set_limits(
                self.config.compaction_strategy.max_history_messages,
                self.config.compaction_strategy.max_deferred_messages,
            );
        }

        let started_at = std::time::Instant::now();

        // Emit TurnStarted telemetry event.
        if let Some(ref telemetry) = self.telemetry {
            telemetry.track(TelemetryEvent::TurnStarted {
                turn_index: turn_id,
            });
        }

        // Fire the UserPromptSubmit external hook (TS input shape:
        // `{ prompt, isSteer }`). The initial prompt of a turn is never a
        // steer (mid-turn interjections flow through the steer buffer), so
        // `isSteer` is always false here.
        //
        // A blocking hook consumes the prompt: the turn ends immediately
        // with `BlockedByHook` and no LLM step runs, mirroring the TS
        // behavior of swallowing the prompt after a block. The hook's
        // rendered result (a passing hook's display message as a user
        // message, a blocking hook's message as an assistant message) is
        // appended to memory and announced on the event bus as a
        // `HookResult`, mirroring the TS `hook.result` emit — the only hook
        // event TS publishes a result for.
        let (prompt_hook_blocked, additional_context, hook_result) =
            if let Some(prompt) = prompt.as_ref() {
                // Serialize before the append moves `prompt` into memory.
                let prompt_hook_input =
                    serde_json::to_value(prompt).unwrap_or_else(|_| serde_json::json!([]));
                let mut blocked = false;
                let mut additional_context = String::new();
                // (message, blocked?) of the rendered result, per the TS
                // renderUserPromptHook(Block)Result chain.
                let mut hook_result: Option<(String, Option<bool>)> = None;
                if let Some(engine) = &self.external_hooks {
                    // Only real user prompts trigger the hook, mirroring the TS
                    // check `(origin ?? USER_PROMPT_ORIGIN).kind !== 'user'`.
                    if origin == PromptOrigin::User {
                        let input = serde_json::json!({
                            "prompt": prompt_hook_input,
                            "isSteer": false,
                        });
                        let result = engine
                            .trigger(
                                HookEventType::UserPromptSubmit,
                                input,
                                &self.external_hook_env,
                            )
                            .await;
                        blocked = result.blocked;
                        additional_context = result.additional_context;
                        if blocked {
                            // Block content chain (TS renderUserPromptHookBlockResult):
                            // message -> reason -> the default text, all trimmed.
                            let content = result
                                .block_message
                                .as_deref()
                                .map(str::trim)
                                .filter(|m| !m.is_empty())
                                .or_else(|| {
                                    result
                                        .block_reason
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|r| !r.is_empty())
                                })
                                .unwrap_or("Blocked by UserPromptSubmit hook")
                                .to_string();
                            hook_result = Some((content, Some(true)));
                        } else if let Some(message) = result
                            .message
                            .as_deref()
                            .map(str::trim)
                            .filter(|m| !m.is_empty())
                        {
                            hook_result = Some((message.to_string(), None));
                        }
                    }
                }
                (blocked, additional_context, hook_result)
            } else {
                // Retry shape (B3): no prompt rides along — no
                // UserPromptSubmit hook and no message to consume.
                (false, String::new(), None)
            };

        // Render a passing hook's display message into memory as a user
        // message ahead of the additional context, mirroring the TS v1
        // order (appendUserMessage(hookResult) then
        // injectHookAdditionalContext).
        if let Some((content, None)) = hook_result.as_ref() {
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_user_message(
                    vec![ContentPart::Text {
                        text: render_hook_result_envelope(content),
                    }],
                    PromptOrigin::HookResult,
                );
        }

        // Inject hook additional context before the user prompt, mirroring the
        // TS v1 (agent-core) `injectHookAdditionalContext` behavior: the
        // context is appended as a separate user message with
        // `HookAdditionalContext` origin so that compaction can treat it
        // differently from real user input.
        if !additional_context.is_empty() {
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_user_message(
                    vec![ContentPart::Text {
                        text: additional_context,
                    }],
                    PromptOrigin::HookAdditionalContext,
                );
        }

        // Append user message — skipped in the retry shape (B3): the failed
        // turn's prompt already sits in memory and drives the re-run. The
        // with_skills stamp (ADR-006) rides ONLY this append: the inline
        // admission list that goes with the rewritten body. An empty list
        // normalizes to `None` (TS parity: the field is omitted when
        // nothing activated).
        if let Some(prompt) = prompt {
            let stamp = with_skills.flatten().filter(|list| !list.is_empty());
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_user_message_with_skills(prompt, origin, stamp);
        }

        // Render a blocking hook's message as an assistant message (after
        // the additional context and the consumed prompt, mirroring the
        // TS block-branch order), then announce the rendered result.
        if let Some((content, Some(true))) = hook_result.as_ref() {
            let mut memory_guard = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory_guard.append_message(
                crate::context::ContextMessage::from_message(crate::message::Message::assistant(
                    render_hook_result_envelope(content),
                ))
                .with_origin(PromptOrigin::HookResult),
            );
        }

        if let Some((content, blocked)) = hook_result.as_ref()
            && let Some(event_bus) = &self.event_bus
        {
            event_bus.publish(SessionEvent::HookResult {
                hook_event: "UserPromptSubmit".to_string(),
                content: content.clone(),
                blocked: *blocked,
            });
        }

        if prompt_hook_blocked {
            let duration_ms = started_at.elapsed().as_millis() as u64;
            self.active_turn_id = None;
            self.wire_turn_id = None;
            self.activity
                .on_turn_ended(turn_id, TurnEndReason::Blocked, Some(duration_ms));
            // Emit TurnEnded telemetry so the blocked turn is observable.
            if let Some(ref telemetry) = self.telemetry {
                telemetry.track(TelemetryEvent::TurnEnded {
                    turn_index: turn_id,
                    duration_ms,
                });
            }
            return Ok(TurnEndResult {
                turn_id,
                stop_reason: TurnStopReason::BlockedByHook,
                steps: 0,
                usage: LlmUsage::default(),
                duration_ms,
                blocked_by_prompt_hook: true,
            });
        }

        // Step-level retry budgets are scoped to a turn; reset them before
        // the step loop runs so a transient failure in one turn does not
        // penalize the next.
        self.step_retry.reset_for_turn();

        // Run step loop.
        let result = self
            .run_step_loop(
                turn_id,
                memory.clone(),
                tools,
                llm_resolver,
                cancel,
                validators,
                live_ref,
                self.cache_root_id.clone(),
                wait_registry,
            )
            .await;

        let duration_ms = started_at.elapsed().as_millis() as u64;
        self.active_turn_id = None;
        self.wire_turn_id = None;

        // Emit TurnEnded telemetry event.
        if let Some(ref telemetry) = self.telemetry {
            telemetry.track(TelemetryEvent::TurnEnded {
                turn_index: turn_id,
                duration_ms,
            });
        }

        match result {
            Ok(turn_result) => {
                // Fold the outcome into the activity view's `lastTurn`,
                // mapping the stop reason to the klient vocabulary.
                self.activity.on_turn_ended(
                    turn_id,
                    map_stop_reason(&turn_result.stop_reason),
                    Some(duration_ms),
                );
                // Emit telemetry if the turn was cancelled mid-step.
                if turn_result.stop_reason == TurnStopReason::Aborted {
                    if let Some(ref telemetry) = self.telemetry {
                        telemetry.track(TelemetryEvent::Cancel {
                            turn_index: turn_id,
                        });
                    }

                    // Fire the Interrupt external hook (TS input shape:
                    // `{ turn_id, reason }`) when the turn was cancelled by
                    // the user. Programmatic (system) aborts — `cancel()` —
                    // do NOT fire Interrupt; only user cancellations do.
                    if cancel.is_user_cancelled()
                        && let Some(engine) = &self.external_hooks
                    {
                        let input = serde_json::json!({
                            "turn_id": turn_id,
                            "reason": "cancelled",
                        });
                        let _ = engine
                            .trigger(HookEventType::Interrupt, input, &self.external_hook_env)
                            .await;
                    }

                    // Deliver the interruption reminder and record the
                    // durable wire facts when the turn was cancelled by the
                    // user (see `deliver_interruption_reminder`). The
                    // embedded layer records `turn.ended` after this turn
                    // returns, so the append-only op order is preserved.
                    if cancel.is_user_cancelled() {
                        self.deliver_interruption_reminder(turn_id, &memory).await;
                    }
                }
                Ok(TurnEndResult {
                    turn_id,
                    stop_reason: turn_result.stop_reason,
                    steps: turn_result.steps,
                    usage: turn_result.usage,
                    duration_ms,
                    blocked_by_prompt_hook: false,
                })
            }
            Err(e) if e.is_abort() => {
                // Fold the cancellation outcome into `lastTurn`.
                self.activity
                    .on_turn_ended(turn_id, TurnEndReason::Cancelled, Some(duration_ms));
                // Cancellation that occurred before or between `run_turn`
                // calls (e.g. at the top of the overflow-recovery loop, or
                // during overflow compaction). In these paths `run_turn`
                // either hasn't started or hasn't returned a partial result,
                // so steps and usage are genuinely zero. Mid-step cancellation
                // is handled inside `run_turn` which returns Ok(Aborted) with
                // accumulated steps/usage and lands in the Ok branch above.

                // Emit telemetry: the turn was cancelled.
                if let Some(ref telemetry) = self.telemetry {
                    telemetry.track(TelemetryEvent::Cancel {
                        turn_index: turn_id,
                    });
                }

                // Fire the Interrupt external hook (TS input shape:
                // `{ turn_id, reason }`) when the cancellation was
                // user-initiated. Programmatic aborts
                // (`EngineError::Aborted`) do NOT fire Interrupt.
                if e.is_user_cancellation()
                    && let Some(engine) = &self.external_hooks
                {
                    let input = serde_json::json!({
                        "turn_id": turn_id,
                        "reason": "cancelled",
                    });
                    let _ = engine
                        .trigger(HookEventType::Interrupt, input, &self.external_hook_env)
                        .await;
                }

                // Deliver the interruption reminder and record the durable
                // wire facts when the cancellation was user-initiated (see
                // `deliver_interruption_reminder`) — the loop-top abort
                // check surfaces pre-step user cancellations as
                // `EngineError::UserCancelled` through this arm.
                if e.is_user_cancellation() {
                    self.deliver_interruption_reminder(turn_id, &memory).await;
                }

                Ok(TurnEndResult {
                    turn_id,
                    stop_reason: TurnStopReason::Aborted,
                    steps: 0,
                    usage: LlmUsage::default(),
                    duration_ms,
                    blocked_by_prompt_hook: false,
                })
            }
            Err(EngineError::HookBlocked(_)) => {
                // Fold the hook-blocked outcome into `lastTurn`.
                self.activity
                    .on_turn_ended(turn_id, TurnEndReason::Blocked, Some(duration_ms));
                // The `before_step` hook blocked the turn. Surface this as a
                // successful `TurnEndResult` with `blocked_by_prompt_hook: true`
                // so callers can distinguish it from a generic error.
                Ok(TurnEndResult {
                    turn_id,
                    stop_reason: TurnStopReason::BlockedByHook,
                    steps: 0,
                    usage: LlmUsage::default(),
                    duration_ms,
                    blocked_by_prompt_hook: true,
                })
            }
            Err(e) => {
                // Fold the engine-error outcome into `lastTurn`. There is no
                // stop reason to map in this arm (the turn ended in `Err`),
                // so it is recorded as `failed` — the klient vocabulary has
                // no finer-grained error word. This includes the
                // `MaxStepsExceeded` overflow surfaced by `run_step_loop`.
                self.activity
                    .on_turn_ended(turn_id, TurnEndReason::Failed, Some(duration_ms));
                // A turn ending with an unexpected error fires the
                // StopFailure external hook (TS input shape:
                // `{ error_type, error_message }`). User/system
                // cancellations never reach this arm (the abort arm above
                // catches them), but guard defensively so a future reorder
                // cannot classify a cancellation as a failure — those are
                // reported via Interrupt instead.
                if !e.is_abort()
                    && let Some(engine) = &self.external_hooks
                {
                    let input = serde_json::json!({
                        "error_type": "EngineError",
                        "error_message": e.to_string(),
                    });
                    let _ = engine
                        .trigger(HookEventType::StopFailure, input, &self.external_hook_env)
                        .await;
                }
                Err(e)
            }
        }
    }

    /// Deliver the interruption reminder and record the durable wire facts
    /// for a user-cancelled turn (TS `interruptionReminderService`
    /// semantics).
    ///
    /// Called from `run_one_turn_inner`'s two user-cancelled收口 arms (the
    /// `Ok(Aborted)` arm for mid-step cancellations and the
    /// `Err(UserCancelled)` arm for loop-top cancellations) — the same arms
    /// that fire the Interrupt external hook. Order: `turn.cancel` first
    /// (write-before-reminder), then the reminder's
    /// `context.append_message`, then `interruptionReminder.recorded`.
    /// Replay rebuilds the same conversation and a drained pending set.
    ///
    /// A second consecutive user cancellation with no message in between
    /// dedups on the memory's last message — the reminder is not
    /// re-appended, but both marker ops are still recorded (TS behavior).
    /// No-op when no wire recorder is installed. The embedded layer records
    /// `turn.ended` after the turn returns, so the append-only op order is
    /// preserved.
    async fn deliver_interruption_reminder(
        &self,
        turn_id: u64,
        memory: &Arc<Mutex<ContextMemory>>,
    ) {
        let Some(recorder) = self.wire_recorder.as_deref() else {
            return;
        };
        recorder
            .record_op(WireOp::TurnCancel {
                turn_id: Some(turn_id),
                target: Some("active".to_string()),
                reason: Some("user_cancelled".to_string()),
            })
            .await;
        let already_delivered = memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history()
            .last()
            .is_some_and(|m| {
                m.origin == Some(PromptOrigin::Injection)
                    && m.message.text_content()
                        == crate::system_reminder_wire::system_reminder_text(
                            crate::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
                        )
            });
        if !already_delivered {
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_system_reminder(
                    crate::interruption_reminder::INTERRUPTION_REMINDER_TEXT.to_string(),
                );
            recorder
                .record_op(crate::system_reminder_wire::system_reminder_op(
                    crate::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
                ))
                .await;
        }
        recorder
            .record_op(WireOp::InterruptionReminderRecorded { turn_id })
            .await;
    }

    /// Run the step loop — the outer while loop wrapping `run_turn`.
    ///
    /// This handles overflow recovery: if `run_turn` returns a
    /// `ContextOverflow` error, the compaction orchestrator learns the max
    /// context and the loop retries.
    #[allow(clippy::too_many_arguments)]
    async fn run_step_loop(
        &mut self,
        turn_id: u64,
        memory: Arc<Mutex<ContextMemory>>,
        tools: &ToolManager,
        llm_resolver: Arc<dyn LlmResolver>,
        cancel: &CancelToken,
        validators: &mut ValidatorCache,
        live_callback: Option<&LiveEventCallback>,
        cache_root_id: Option<String>,
        wait_registry: Arc<WaitRegistry>,
    ) -> EngineResult<TurnResult> {
        let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));

        // Resolve ONCE for the turn-level reads: the capability-gap
        // injection and the usage-sink model-key fallback.
        //
        // Known limitation (accepted): the capability-gap injection is
        // sticky to the turn-start model — a mid-turn model switch does not
        // re-compute the gap injection until the next turn. The step's LLM
        // calls themselves always use the per-step resolution inside
        // `run_turn`. The auto-compaction summarizer (TurnFlowHooks) also
        // resolves the provider once per outer step-loop iteration (per
        // `run_turn` invocation), not per step — a mid-turn switch takes
        // effect on its next iteration.
        let turn_llm = crate::loop_engine::resolve_step_llm(llm_resolver.as_ref())?;

        // The wire turn id (`turn-<uuid>`) this turn's step events publish
        // under — set by the dispatch (or the subagent host) before the
        // turn ran. A standalone flow (tests, no dispatch) falls back to
        // the engine's numeric turn counter stringified, so the events
        // always carry *some* turn key.
        let wire_turn_id = self
            .wire_turn_id
            .clone()
            .unwrap_or_else(|| turn_id.to_string());

        // Register the capability-gap injector when a profile manager is
        // wired, so per-step injection includes modality-gap dispatch hints.
        // Built fresh per step-loop so profile changes are picked up between
        // turns.
        let mut injection = InjectionManager::new();
        // Per-turn injectors (fresh data each turn).
        if let Some(profile_manager) = &self.profile_manager {
            injection.add(Box::new(CapabilityGapInjector::new(
                GapModelCapabilities::from(turn_llm.capability()),
                profile_infos_from_manager(profile_manager, self.model_catalog.as_deref()),
                catalog_capabilities(self.model_catalog.as_deref()),
            )));
        }
        // Per-turn injector: tools diff. Disclosure-gated and
        // dynamic-scoped (TS `loadableDynamicToolNames` parity, ADR-3-R2):
        // announcements exist only for the progressive-disclosure
        // protocol, and only name the loadable dynamic (MCP) tools — the
        // static table (builtins + user tools, and every MCP tool when
        // disclosure is off) is fully visible in each request's tool
        // definitions, so announcing it would duplicate what the model
        // already has.
        if tools.disclosure_enabled() {
            injection.add(Box::new(ToolsDiffInjector::new(
                tools.loadable_dynamic_tool_names(),
                true,
            )));
        }
        // Persistent injectors (state survives across turns, same Arc'd instances).
        if let Some(pi) = &self.permission_injector {
            injection.add_shared(Arc::clone(pi) as Arc<dyn Injector>);
        }
        if let Some(pli) = &self.plan_injector {
            injection.add_shared(Arc::clone(pli) as Arc<dyn Injector>);
        }
        if let Some(gi) = &self.goal_injector {
            injection.add_shared(Arc::clone(gi) as Arc<dyn Injector>);
        }
        if let Some(psi) = &self.plugin_session_start_injector {
            injection.add_shared(Arc::clone(psi) as Arc<dyn Injector>);
        }
        if let Some(ti) = &self.todo_injector {
            injection.add_shared(Arc::clone(ti) as Arc<dyn Injector>);
        }
        let injection = Arc::new(injection);

        // Build message builders that lock memory for short critical sections.
        // The production builders share the sticky format-rejected flag with
        // the media fallback chain (US6.2 placeholder cause selection).
        let message_builders = MessageBuilders::from_shared_memory(memory.clone());

        // The pending goal-budget grace reminder (T9): the
        // record_step_usage callback parks the stop reminder here when it
        // grants the one-per-turn grace step; the TurnFlowHooks' after_step
        // (wired below through the config) flushes it into memory + the
        // wire log AFTER the step's tool results. The slot is created once
        // per step loop (= one turn), so the grace is per-turn by
        // construction (TS `budgetGraceTurns`).
        let pending_budget_reminder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Build the record_step_usage callback when a goal manager (budget
        // enforcement) or a live usage sink (F5, P7c: the agent's
        // AgentUsageService, so `agent.getUsage` / the session view report
        // live totals without a resume) is wired.
        //
        // The callback is called after each LLM response with the token
        // usage, the response's natural stop reason, and the 1-based step
        // number. It records usage against the goal's budget and returns:
        // - Ok(None) — continue normally (no goal or budget not exhausted).
        // - Ok(Some(true)) — stop the turn (budget exhausted; no grace
        //   available — not a tool_calls stop, the grace already spent, or
        //   no step budget left).
        // - Ok(Some(false)) — the usage was accounted but the step is
        //   released (budget remaining, or the grace step was granted so
        //   the model can write its final message).
        // - Err(_) — propagate the error.
        let record_step_usage: Option<RecordStepUsageFn> = if self.goal_manager.is_some()
            || self.goal_usage_accounting.is_some()
            || self.usage_sink.is_some()
        {
            // The accounting target: the full goal manager (subagent
            // flows — Layer-2 continuation also active) or the
            // accounting-only slot (embedded main/cron flows — no
            // continuation semantics change).
            let gm = self
                .goal_manager
                .clone()
                .or_else(|| self.goal_usage_accounting.clone());
            let sink = self.usage_sink.clone();
            let wire_recorder = self.wire_recorder.clone();
            // TS parity (N4): the by_model key is the agent's configured
            // model alias, not the resolved upstream provider model — the
            // SHARED `usage_model_key` handle (read at each step's
            // recording, so a mid-turn model switch re-attributes the
            // post-switch steps, spec 11 A13); the turn-start
            // `llm.model_name()` is the initial fallback for standalone
            // flows that never wire an agent config.
            let usage_model_key = Arc::clone(&self.usage_model_key);
            let turn_start_model = turn_llm.model_name().to_string();
            // T9: one grace step per turn — the closure lives for one step
            // loop (= one turn), so a plain flag suffices (TS
            // `budgetGraceTurns.has(turnId)`). The max-steps gate mirrors
            // the TS `hasStepBudgetRemaining` (LoopControl.maxStepsPerTurn
            // here is `TurnFlowConfig::max_steps`): the grace only fires
            // while the turn's own step budget has room for one more
            // LLM request.
            let grace_used = Arc::new(AtomicBool::new(false));
            let pending_budget_reminder = Arc::clone(&pending_budget_reminder);
            let max_steps = self.config.max_steps;
            Some(Box::new(
                move |usage: LlmUsage,
                      natural_stop: StepStopReason,
                      step_index: usize|
                      -> HookFuture<EngineResult<Option<bool>>> {
                    let gm = gm.clone();
                    let sink = sink.clone();
                    let wire_recorder = wire_recorder.clone();
                    let usage_model_key = Arc::clone(&usage_model_key);
                    let turn_start_model = turn_start_model.clone();
                    let grace_used = Arc::clone(&grace_used);
                    let pending_budget_reminder = Arc::clone(&pending_budget_reminder);
                    Box::pin(async move {
                        let model = usage_model_key
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                            .unwrap_or_else(|| turn_start_model.clone());
                        if let Some(sink) = &sink {
                            sink.lock().unwrap_or_else(|e| e.into_inner()).record(
                                &model,
                                crate::usage::TokenUsage {
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cache_read_tokens: usage.cache_read_tokens,
                                    cache_creation_tokens: usage.cache_creation_tokens,
                                },
                                None,
                            );
                        }
                        let Some(gm) = gm else {
                            return Ok(None);
                        };
                        // Usage-only goal persistence (F11): while a
                        // goal is active, record its running totals
                        // (absolute values — the same semantics as
                        // the TS `goal.update` usage fields) as a
                        // `goal.update` op *before* the in-memory
                        // counters advance, so a crash between the
                        // two leaves the wire log ahead of memory
                        // and a resume replays the consumed budget
                        // back. No status or budget change rides
                        // the op; `wallClockMs` only appears while
                        // the wall-clock budget timer runs (it
                        // cannot be restored across a restart —
                        // see the T6 brief §1).
                        if gm.get().is_some()
                            && let Some(recorder) = wire_recorder.as_deref()
                        {
                            let next_turns = gm.used_turns() + 1;
                            let next_tokens =
                                gm.used_tokens() + usage.input_tokens + usage.output_tokens;
                            let wall_clock_ms = gm.wall_clock_ms();
                            recorder
                                .record_op(WireOp::GoalUpdate {
                                    goal_id: None,
                                    status: None,
                                    reason: None,
                                    turns_used: Some(next_turns),
                                    tokens_used: Some(next_tokens),
                                    wall_clock_ms: (wall_clock_ms > 0).then_some(wall_clock_ms),
                                    budget_limits: None,
                                    actor: Some("runtime".to_string()),
                                })
                                .await;
                        }
                        let result = gm.record_step_usage(usage);
                        if !matches!(result, Ok(Some(true))) {
                            return result;
                        }
                        // Hard budget reached: transition the goal to
                        // blocked exactly once (the reminder text asserts
                        // it, so the state must be true). Idempotent: a
                        // later exhaustion (e.g. the post-grace step)
                        // must not emit a second op or reset the reason.
                        let already_blocked = gm.get().is_some_and(|g| {
                            g.status == GoalStatus::Blocked
                                && g.terminal_reason
                                    .as_deref()
                                    .is_some_and(|r| r.starts_with("hard budget"))
                        });
                        if !already_blocked {
                            // Write-before-mutate: the blocked `goal.update`
                            // op lands before the in-memory transition.
                            if let Some(recorder) = wire_recorder.as_deref() {
                                recorder
                                    .record_op(WireOp::GoalUpdate {
                                        goal_id: None,
                                        status: Some("blocked".to_string()),
                                        reason: Some("hard budget reached".to_string()),
                                        turns_used: None,
                                        tokens_used: None,
                                        wall_clock_ms: None,
                                        budget_limits: None,
                                        actor: Some("runtime".to_string()),
                                    })
                                    .await;
                            }
                            gm.update_with_reason(
                                GoalStatus::Blocked,
                                Some("hard budget reached".to_string()),
                            );
                        }
                        // One grace step (TS `stopAfterBudgetReached`): when
                        // the exhausted step ended naturally in tool calls,
                        // this is the FIRST blocked transition of the goal
                        // (`!already_blocked`), the grace was not spent this
                        // turn, and the turn's step budget has room, release
                        // the step so the model can write its final message
                        // after seeing the stop reminder. The next exhaustion
                        // forces the stop (grace_used). TS
                        // `hasStepBudgetRemaining`: no limit, a non-positive
                        // limit, or the current step below the limit.
                        //
                        // The `!already_blocked` gate keeps the grace and the
                        // reminder tied to the blocked transition itself (TS
                        // `handleTurnStarted` bails on non-active goals): a
                        // goal that was ALREADY blocked when the step ran
                        // (e.g. a fresh user message under a budget-exhausted
                        // blocked goal — `record_step_usage` does not look at
                        // the status, so it keeps returning `Some(true)`)
                        // must hard-stop without re-granting the grace or
                        // re-injecting the reminder; `grace_used` alone is
                        // per-turn state and resets on every new turn.
                        let step_budget_remaining =
                            max_steps.is_none_or(|max| max == 0 || step_index < max as usize);
                        if natural_stop == StepStopReason::ToolUse
                            && !already_blocked
                            && !grace_used.swap(true, Ordering::SeqCst)
                            && step_budget_remaining
                        {
                            // The reminder is appended by the after_step
                            // hook (flushed after this step's tool results)
                            // rather than here: appending at callback time
                            // would land it before the step's own assistant
                            // message and tool results, and recording the
                            // durable wire op here would diverge the wire
                            // log's message order from live memory.
                            *pending_budget_reminder
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) =
                                Some(crate::goal::GOAL_BUDGET_STOP_REMINDER.to_string());
                            return Ok(Some(false)); // release: model writes final message
                        }
                        Ok(Some(true))
                    })
                },
            ) as RecordStepUsageFn)
        } else {
            None
        };

        loop {
            cancel.check()?;

            // Resolve the provider for this iteration of the step loop:
            // the hooks (auto-compaction inside `before_step`) and the
            // overflow-recovery summarizer run with the currently resolved
            // provider. (The steps themselves resolve again per step inside
            // `run_turn`.)
            let llm = crate::loop_engine::resolve_step_llm(llm_resolver.as_ref())?;

            // Build hooks with current shared state: the internal
            // TurnFlowHooks run first (compaction, steering, injection,
            // dedup, permissions), then the external command hooks via an
            // ExternalHooksBridge, chained by CompositeLoopHooks.
            let hooks_config = TurnFlowHooksConfig {
                hook_engine: self.external_hooks.clone(),
                hook_env: self.external_hook_env.clone(),
                interaction_kernel: self.interaction_kernel.clone(),
                suspension_notify: self.suspension_notify.clone(),
                suspension_resume: self.suspension_resume.clone(),
                wire_recorder: self.wire_recorder.clone(),
                wire_turn_id: Some(wire_turn_id.clone()),
                agent_id: self.agent_id.clone(),
                pending_budget_reminder: Some(Arc::clone(&pending_budget_reminder)),
                // The same by-model key the `record_step_usage` closure
                // records under (the shared alias handle, read at each
                // step's completion), with the turn-start `llm.model_name()`
                // as the standalone-flow fallback — so `turn.step.completed`
                // attributes the step to the model its usage was accounted
                // against (TS parity N4).
                usage_model_key: Some(Arc::clone(&self.usage_model_key)),
                turn_start_model: Some(turn_llm.model_name().to_string()),
                flag_registry: self.flag_registry.clone(),
                // The session-scoped todo list (when wired): the steer flush
                // pushes live undo checkpoints for real-user-input steers —
                // the memory-side mirror of the reducer's fold rule.
                todo_list: self.todo_list.clone(),
                ..TurnFlowHooksConfig::default()
            };

            let turn_flow_hooks = TurnFlowHooks::new(
                memory.clone(),
                self.compaction.clone(),
                self.steer_buffer.clone(),
                deduper.clone(),
                injection.clone(),
                cancel.clone(),
                hooks_config,
                llm.clone(),
                self.permission_manager.clone(),
                self.goal_manager.clone(),
                self.telemetry.clone(),
                self.reverse_rpc_callback.clone(),
                self.tool_result_truncator.clone(),
                self.task_manager.clone(),
                self.event_bus.clone(),
                Arc::clone(&self.activity),
                Some(Arc::clone(&wait_registry)),
            );

            let external_bridge = self.external_hooks.as_ref().map(|engine| {
                ExternalHooksBridge::new(Arc::clone(engine), self.external_hook_env.clone())
            });
            let hooks = CompositeLoopHooks::new(turn_flow_hooks, external_bridge);

            let config = RunTurnConfig {
                turn_id: turn_id.to_string(),
                max_steps: self.config.max_steps,
                max_retry_attempts: self.config.max_retry_attempts,
                step_retry: Some(Arc::clone(&self.step_retry)),
                turn_abort_guard: self.turn_abort_guard.clone(),
                cache_root_id: cache_root_id.clone(),
                wait_registry: Some(Arc::clone(&wait_registry)),
            };

            // Run the inner loop — under the ADR-006 InStep registration:
            // one entry per step-iterating run, covering the whole model
            // call (stream collection → WaitingProvider inside) and the
            // tool batch (→ WaitingTool inside execute_tool_calls).
            // Nested = set semantics (both present; any Advancing member
            // resets the watchdog's classic window).
            let _in_step_guard = wait_registry.enter(WaitKind::InStep, WaitSubject::None);
            let result = crate::loop_engine::run_turn(
                config,
                llm_resolver.as_ref(),
                &hooks,
                tools,
                validators,
                memory.clone(),
                cancel,
                &message_builders,
                live_callback,
                record_step_usage.as_ref(),
                self.wire_recorder.as_deref(),
            )
            .await;

            match result {
                Ok(turn_result) => return Ok(turn_result),
                Err(e) => {
                    // Check for overflow error.
                    let is_overflow = matches!(e, EngineError::ContextOverflow { .. });

                    if is_overflow {
                        // Overflow recovery: learn max context, compact, and
                        // retry with the compacted context.
                        let token_count = memory
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .token_count_with_pending();
                        {
                            let mut comp =
                                self.compaction.lock().unwrap_or_else(|e| e.into_inner());
                            comp.observe_context_overflow(token_count);
                        }

                        // handle_overflow_error increments the consecutive
                        // overflow counter and returns Err if the limit is
                        // exceeded.
                        {
                            let mut comp =
                                self.compaction.lock().unwrap_or_else(|e| e.into_inner());
                            let mem_guard = memory.lock().unwrap_or_else(|e| e.into_inner());
                            comp.handle_overflow_error(&mem_guard, cancel, token_count)?;
                        }

                        // Actually perform compaction: begin a background
                        // compaction, await it, and apply the result to
                        // memory so the retry uses the compacted context.
                        //
                        // Fire the PreCompact external hook (TS input shape:
                        // `{ trigger, tokenCount }`) before the task starts.
                        // Fire-and-forget: a block decision does not cancel
                        // overflow recovery. The hooks' `additionalContext`
                        // rides along on the compaction request (TS parity:
                        // `injectHookAdditionalContext` + `historyForModel`).
                        let mut hook_additional_context = Vec::new();
                        if let Some(engine) = &self.external_hooks {
                            let input = serde_json::json!({
                                "trigger": "overflow",
                                "tokenCount": token_count,
                            });
                            let result = engine
                                .trigger(HookEventType::PreCompact, input, &self.external_hook_env)
                                .await;
                            hook_additional_context =
                                hook_additional_context_messages(&result.additional_context);
                        }
                        {
                            let mut comp =
                                self.compaction.lock().unwrap_or_else(|e| e.into_inner());
                            let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                            comp.begin(
                                CompactionSource::Auto,
                                CompactionBeginParams {
                                    llm: llm.clone(),
                                    cancel: cancel.clone(),
                                    custom_instruction: None,
                                    max_output_size: None,
                                    hook_additional_context,
                                },
                                &mem,
                            )?;
                            // Announce the overflow-recovery compaction start
                            // on the session bus.
                            if let Some(ref event_bus) = self.event_bus {
                                event_bus.publish(SessionEvent::CompactionStarted {
                                    agent_id: self.agent_id_or_main(),
                                    trigger: CompactionSource::Auto,
                                    instruction: None,
                                });
                            }
                        }

                        // Take the task handle out so we can await it without
                        // holding the compaction lock.
                        let task_handle = {
                            let mut comp =
                                self.compaction.lock().unwrap_or_else(|e| e.into_inner());
                            comp.take_active_task()
                        };

                        if let Some(handle) = task_handle {
                            // The turn blocks on the summarizer for the whole
                            // overflow recovery — announce it before awaiting.
                            if let Some(ref event_bus) = self.event_bus {
                                event_bus.publish(SessionEvent::CompactionBlocked {
                                    agent_id: self.agent_id_or_main(),
                                    turn_id: None,
                                });
                            }
                            let compaction_result = tokio::select! {
                                r = handle => {
                                    r.map_err(|join_err| {
                                        EngineError::CompactionFailed(
                                            format!("compaction task panicked: {}", join_err)
                                        )
                                    })??
                                }
                                _ = cancel.cancelled() => {
                                    // The turn's cancellation interrupts the
                                    // compaction the turn is blocked on.
                                    if let Some(ref event_bus) = self.event_bus {
                                        event_bus.publish(SessionEvent::CompactionCancelled {
                                            agent_id: self.agent_id_or_main(),
                                        });
                                    }
                                    return Err(EngineError::Aborted(
                                        "cancelled during overflow compaction".to_string(),
                                    ));
                                }
                            };

                            // Apply the compaction result to memory. The guards are
                            // dropped before any await below.
                            let applied = {
                                let mut comp =
                                    self.compaction.lock().unwrap_or_else(|e| e.into_inner());
                                let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                                comp.apply_compaction_result(&mut mem, compaction_result)
                            };

                            // Persist the applied compaction (TS parity:
                            // `contextMemoryService.applyCompaction`) so
                            // `session.resume` replays the summary instead of
                            // the compacted-away history. A summary that failed
                            // the shrink check (变小校验) was never applied —
                            // persisting an apply op would corrupt resume, so
                            // nothing is recorded.
                            if applied.applied
                                && let Some(recorder) = self.wire_recorder.as_deref()
                            {
                                // The compaction replaced the prior context —
                                // the request envelope drifted (P8a-4).
                                recorder
                                    .record_op(WireOp::RequestHeaderChanged {
                                        reason: "compaction_applied".to_string(),
                                    })
                                    .await;
                                let length = memory
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .history()
                                    .len() as u64;
                                let (
                                    first_replaced_entry_seq,
                                    last_replaced_entry_seq,
                                    dropped_entry_seqs,
                                ) = crate::compaction::apply_compaction_op_fields(&applied);
                                recorder
                                    .record_op(WireOp::ContextApplyCompaction {
                                        summary: applied.summary.clone(),
                                        compacted_count: applied.compacted_count as u32,
                                        tokens_before: Some(applied.tokens_before),
                                        tokens_after: Some(applied.tokens_after),
                                        context_summary: Some(applied.context_summary.clone()),
                                        dropped_count: Some(applied.dropped_count as u32),
                                        retained_tail_count: Some(
                                            applied.retained_tail_count as u32,
                                        ),
                                        // TS-dialect reader field — the
                                        // rust kernel never writes it
                                        // (ADR-008).
                                        kept_user_message_count: None,
                                        kept_head_user_message_count: Some(
                                            applied.kept_head_user_message_count as u32,
                                        ),
                                        elision_message: applied.elision_message.clone(),
                                        // ADR-007 provenance: the replaced
                                        // range and the ladder's dropped
                                        // entry numbers, mapped once for
                                        // every apply-record write point.
                                        first_replaced_entry_seq,
                                        last_replaced_entry_seq,
                                        dropped_entry_seqs,
                                    })
                                    .await;
                                recorder
                                    .record_op(WireOp::ContextSizeMeasured {
                                        length,
                                        tokens: applied.tokens_after,
                                    })
                                    .await;
                            }

                            // Announce the applied compaction on the session
                            // bus.
                            if let Some(ref event_bus) = self.event_bus {
                                event_bus.publish(SessionEvent::CompactionCompleted {
                                    agent_id: self.agent_id_or_main(),
                                    result: applied.clone(),
                                });
                            }

                            // Fire the PostCompact external hook (TS input
                            // shape: `{ trigger, estimatedTokenCount }`).
                            if let Some(engine) = &self.external_hooks {
                                let input = serde_json::json!({
                                    "trigger": "overflow",
                                    "estimatedTokenCount": applied.tokens_after,
                                });
                                let _ = engine
                                    .trigger(
                                        HookEventType::PostCompact,
                                        input,
                                        &self.external_hook_env,
                                    )
                                    .await;
                            }

                            // Post-compaction injection: re-surface still-running
                            // background tasks so the LLM doesn't re-spawn them
                            // (same rationale as the auto-compaction path in
                            // `before_step`). The reminder is ALSO recorded as
                            // a durable `context.append_message` op — replay
                            // must see the same reminder the live session
                            // appended (the wire replay equivalence invariant,
                            // ADR-009).
                            let mut post_compaction_reminders: Vec<String> = Vec::new();
                            {
                                if let Some(ref tm) = self.task_manager {
                                    let running: Vec<AgentTask> = tm
                                        .list()
                                        .into_iter()
                                        .filter(|t| t.status == TaskStatus::Running)
                                        .collect();
                                    if !running.is_empty() {
                                        let reminder =
                                            hooks::format_background_task_reminder(&running);
                                        memory
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .append_system_reminder(reminder.clone());
                                        post_compaction_reminders.push(reminder);
                                    }
                                }
                            }
                            crate::system_reminder_wire::record_system_reminders(
                                self.wire_recorder.as_deref(),
                                &post_compaction_reminders,
                            )
                            .await;
                        }

                        continue;
                    }

                    return Err(e);
                }
            }
        }
    }
}

/// Wrap a rendered hook result message in the model-visible envelope (the TS
/// `renderHookResult` shape: a tagged wrapper so the model can tell hook
/// output from real user/assistant messages).
fn render_hook_result_envelope(content: &str) -> String {
    format!("<hook_result hook_event=\"UserPromptSubmit\">\n{content}\n</hook_result>")
}

/// Render the skill-catalog refresh reminder (ADR-010 decision 4): the
/// current catalog listing (the same
/// [`render_skills_section`](crate::scope::system_prompt::render_skills_section)
/// the system prompt embeds, re-evaluated at flush time so the listing is
/// the live table) preceded by a one-line change summary. An empty listing
/// (the diff removed every skill) renders the cleared-catalog declaration
/// instead — a neutral statement that the previously listed skills are no
/// longer available, mirroring the plugin session-start invalidation
/// declaration's spirit — never a stale listing and never an empty text.
fn render_skill_catalog_refresh_reminder(catalog: &crate::skill::SkillCatalog) -> String {
    let listing = crate::scope::system_prompt::render_skills_section(catalog);
    if listing.is_empty() {
        return "Skill catalog updated. All previously listed skills are no longer available; \
            there are currently no model-invocable skills."
            .to_string();
    }
    format!("Skill catalog updated. {listing}")
}
