//! `TurnFlowHooks` — the `LoopHooks` implementation for the stateful host.
//!
//! This struct bridges the stateless loop engine to the stateful `TurnFlow`
//! host. It holds shared state via `Arc<Mutex<>>` references:
//!
//! - **Memory** — flushed steers, injected reminders.
//! - **Compaction** — auto-compaction checks before each step.
//! - **Steer buffer** — mid-turn user interjections, flushed at `before_step`.
//! - **Deduplicator** — same-step and cross-step tool call dedup.
//! - **Injection manager** — per-step system reminders.
//!
//! All hook methods clone the `Arc` references, move them into an `async`
//! block, and lock only for short synchronous critical sections — never across
//! `.await` points.
//!
//! ## Ask-lifecycle backstop (ADR-011, fix wave F1)
//!
//! A gate-registered approval ask is normally withdrawn by the turn's own
//! `ctx_cancel` select arm. That arm only runs while the ask future is
//! POLLED — when the ask's owning turn is dropped mid-wait (a swarm member
//! settled aborted without resuming its turn), the withdraw would never
//! run and the gate would list a zombie request forever. [`PendingAskGuard`]
//! covers that path: it withdraws the entry on drop unless a resolution
//! path marked it settled (mirroring the host's `RunningGuard` pattern).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;

use crate::cancel::CancelToken;
use crate::compaction::{
    CompactionBeginParams, CompactionSource, FullCompaction, hook_additional_context_messages,
};
use crate::context_memory::ContextMemory;
use crate::error::{EngineError, EngineResult};
use crate::goal::GoalManager;
use crate::hook_engine::{
    HookEngine, HookEventType, PermissionResultOutcome, fire_permission_result,
};
use crate::hooks::{
    AfterStepContext, AfterStepResult, AuthorizeToolContext, AuthorizeToolResult,
    BeforeStepContext, BeforeStepResult, ContinueAfterStopContext, ContinueAfterStopResult,
    FinalizeToolContext, FinalizeToolResult, HookFuture, LoopHooks, PrepareToolContext,
    PrepareToolResult,
};
use crate::llm::{Llm, LlmChatParams, LlmChatResponse};
use crate::loop_engine::WireOpRecorder;
use crate::message::ContentPart;
use crate::origin::PromptOrigin;
use crate::permission::{
    OUTSIDE_SANDBOX_DENIAL_CONTRACT, PermissionManager, PermissionPolicyContext,
    PermissionPolicyResult,
};
use crate::reverse_rpc::ReverseRpcCallback;
use crate::session::events::{EventBus, SessionEvent};
use crate::session::interaction::InteractionKernel;
use crate::system_reminder_wire::record_system_reminders;
use crate::task::{AgentTask, TaskManager, TaskStatus};
use crate::telemetry::{TelemetryEvent, TelemetryService};
use crate::tool::{AccessKind, AccessOperation, ToolAccess, ToolResult};
use crate::tool_result_truncation::ToolResultTruncator;
use crate::turn_flow::activity::ActivityTracker;
use crate::turn_flow::dedup::ToolCallDeduplicator;
use crate::turn_flow::injection::InjectionManager;
use crate::turn_flow::steering::{BufferedSteer, SteerBuffer};
use crate::turn_flow::wait_registry::{WaitKind, WaitSubject};
use mirri_proto::WireOp;

/// Configuration shared with the hooks (cloned from `TurnFlowConfig`).
#[derive(Clone)]
pub struct TurnFlowHooksConfig {
    /// Whether compaction should throw on limit exceeded.
    pub compaction_throw_on_limit: bool,
    /// Optional wire-op recorder: the turn-internal auto-compaction's apply
    /// persists a `context.apply_compaction` record (plus the follow-up
    /// context-size measurement) so `session.resume` replays the summary
    /// instead of the compacted-away history (TS parity:
    /// `contextMemoryService.applyCompaction` dispatches both ops).
    pub wire_recorder: Option<Arc<dyn WireOpRecorder>>,
    /// Optional permission manager for tool-call authorization.
    ///
    /// When `Some`, the `authorize_tool_execution` hook delegates to
    /// [`PermissionManager::before_tool_call`], running the full 19-policy
    /// chain. When `None`, all tool calls are auto-approved (the trait
    /// default).
    pub permission_manager: Option<Arc<PermissionManager>>,
    /// Optional goal manager for budget enforcement.
    ///
    /// When `Some`, the `should_continue_after_stop` hook checks goal
    /// status and budget to decide whether to continue the turn. When
    /// `None`, the hook stops after steer flushing (the default).
    pub goal_manager: Option<Arc<GoalManager>>,
    /// Optional external-command hook engine whose `PreCompact` /
    /// `PostCompact` events fire around auto-compaction, and whose `Stop`
    /// event fires at the natural-stop boundary (a blocking Stop hook
    /// forces one continuation with the reason injected).
    ///
    /// `None` (the default) disables external compaction and Stop hooks.
    pub hook_engine: Option<Arc<HookEngine>>,
    /// Base environment merged into every external compaction hook
    /// invocation (used together with `hook_engine`).
    pub hook_env: std::collections::HashMap<String, String>,
    /// Optional interaction kernel (B4-a): binds the session's approval
    /// gate to the permission `Ask` flow so the gate is the pending source
    /// of truth and `approval.decide` converges with the reverse-RPC reply
    /// on the same oneshot. `None` (the default) keeps the pure reverse-RPC
    /// ask of the pre-B4-a engine.
    pub interaction_kernel: Option<Arc<InteractionKernel>>,
    /// Optional suspension-boundary callback (ADR-011): the approval-gate
    /// edges of a member flow report into it (register → `Suspended`,
    /// resolve → `Resumed`) so the wiring host publishes the
    /// `subagent.suspended` / `subagent.started` pairing. `None` (the
    /// default — the main agent's flow, standalone flows) publishes
    /// nothing.
    pub suspension_notify: Option<crate::subagent::SuspensionNotify>,
    /// The SCHEDULER-ONLY resume feed (ADR-011, F3): the gate's decision-
    /// side wake delivers into this sink instead of `suspension_notify`,
    /// so the public `subagent.started` publishes once — from the turn-side
    /// `Resumed`, when the member actually resumes running — never at
    /// decide time (a decided member may still be waiting in the batch's
    /// ready queue for a slot). `None` (the default) keeps the gate-side
    /// wake silent.
    pub suspension_resume: Option<crate::subagent::SuspensionNotify>,
    /// The wire turn id (`turn-<uuid>`) the step events publish under — the
    /// same key the wire's `turn.started` / `turn.ended` carry, plumbed down
    /// from the message-send dispatch through
    /// [`TurnFlow::set_wire_turn_id`](crate::turn_flow::TurnFlow::set_wire_turn_id).
    /// `None` for a standalone flow (tests, no dispatch) — the step events
    /// then fall back to the engine's numeric per-flow counter.
    pub wire_turn_id: Option<String>,
    /// The agent this flow runs as (supervision B1) — the full agent id
    /// string (`agent-<uuid>` for subagent flows; `None` for the main
    /// agent's flow and standalone test flows). Rides the approval asks the
    /// flow issues so clients can attribute the request to the requesting
    /// member (see [`member_agent_uuid`]), and the auto-compaction
    /// lifecycle emits — normalized to `"main"` when `None` (the same
    /// `None ≡ "main"` convention
    /// [`TurnFlow::agent_id_or_main`](crate::turn_flow::TurnFlow::agent_id_or_main)
    /// applies) so every event names one consistent agent.
    pub agent_id: Option<String>,
    /// The pending goal-budget grace reminder (T9): the `record_step_usage`
    /// callback parks the
    /// [`GOAL_BUDGET_STOP_REMINDER`](crate::goal::GOAL_BUDGET_STOP_REMINDER)
    /// here when it grants the one-per-turn grace step; `after_step`
    /// flushes it into context memory (plus the durable
    /// `context.append_message` wire record). The deferral exists because
    /// the callback runs before the step's tool results are recorded —
    /// flushing here lands the reminder AFTER them, as the trailing
    /// message of the next LLM request (TS `handleAfterStep` parity).
    /// `None` (the default) disables the flush.
    pub pending_budget_reminder: Option<Arc<Mutex<Option<String>>>>,
    /// The usage sink's shared by-model key handle (M1): the agent's
    /// configured model ALIAS the step usage records under (TS parity,
    /// N4). `after_step` reads it at each step's completion — the same
    /// key the `record_step_usage` closure uses — so a mid-turn model
    /// switch re-attributes the post-switch steps (spec 11 A13).
    /// Boundary: within a single step, the accounting closure (right
    /// after the LLM response) and `after_step` (after the step's tool
    /// results land) read the handle at two different instants — a
    /// switch landing between those two reads attributes that step's
    /// EVENT to the new alias while its ACCOUNTING kept the old one (a
    /// one-step skew on the event side only; the by-model ledger is
    /// unaffected).
    /// `None` (the default) leaves `turn.step.completed` without model
    /// attribution.
    pub usage_model_key: Option<Arc<Mutex<Option<String>>>>,
    /// The turn-start `llm.model_name()` — the standalone-flow fallback
    /// for the `turn.step.completed` model attribution when the
    /// [`Self::usage_model_key`] handle never got an alias wired (the
    /// same fallback the `record_step_usage` closure applies).
    /// `None` (the default) disables the fallback.
    pub turn_start_model: Option<String>,
    /// The engine's flag registry, wired via
    /// [`TurnFlow::set_flag_registry`](crate::turn_flow::TurnFlow::set_flag_registry)
    /// — the composition root installs the config `[experimental]` layer
    /// into it (TS parity: the TS `FlagService` reads env, config, and
    /// defaults). Gates the PreToolUse `updatedInput` rewrite
    /// ([`crate::flags::HOOK_COMMAND_REWRITE_FLAG`]).
    /// `None` (the default — standalone flows, tests) falls back to the
    /// env-only chain ([`crate::flags::env_flag_enabled`]).
    pub flag_registry: Option<Arc<crate::flags::FlagRegistry>>,
}

impl Default for TurnFlowHooksConfig {
    fn default() -> Self {
        Self {
            compaction_throw_on_limit: true,
            permission_manager: None,
            goal_manager: None,
            hook_engine: None,
            hook_env: std::collections::HashMap::new(),
            interaction_kernel: None,
            suspension_notify: None,
            suspension_resume: None,
            wire_recorder: None,
            wire_turn_id: None,
            agent_id: None,
            pending_budget_reminder: None,
            usage_model_key: None,
            turn_start_model: None,
            flag_registry: None,
        }
    }
}

/// The member identity of a flow's agent id (supervision B1): the id names a
/// member when it parses as the subagent namespace (`agent-<uuid>`, the form
/// the host mints at spawn); the main agent (`main`) and unattributed flows
/// do not. The parsed uuid is what the approval-gate snapshot records
/// (round-trippable back to the full id by the consumers that know the
/// session roster).
/// Drop-backstop for a gate-registered approval ask (ADR-011, F1): the
/// `ctx_cancel` arm withdraws the gate entry, but it only runs while the
/// ask future is polled. When the ask's owning turn is DROPPED mid-wait —
/// the swarm scheduler settles a parked member aborted and abandons the
/// attempt without polling it — the withdraw never runs and the gate would
/// list a zombie request forever (every `approval.listPending` /
/// reconnecting client keeps seeing an approval card for a member already
/// shown aborted, and `event.approval.resolved` never fires). This guard
/// withdraws on drop unless a resolution path marked it settled — the same
/// drop-guard pattern as the host's `RunningGuard` (host.rs).
struct PendingAskGuard {
    kernel: Arc<InteractionKernel>,
    request_id: uuid::Uuid,
    settled: std::sync::atomic::AtomicBool,
}

impl PendingAskGuard {
    fn new(kernel: Arc<InteractionKernel>, request_id: uuid::Uuid) -> Self {
        Self {
            kernel,
            request_id,
            settled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Mark that the ask reached a resolution path (RPC reply, gate
    /// decision, or the turn's own cancellation arm) — the Drop-side
    /// withdraw backstop must not fire.
    fn mark_settled(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for PendingAskGuard {
    fn drop(&mut self) {
        if !self.settled.load(std::sync::atomic::Ordering::SeqCst) {
            self.kernel.approval_gate().withdraw(self.request_id);
        }
    }
}

fn member_agent_uuid(agent_id: Option<&str>) -> Option<uuid::Uuid> {
    agent_id
        .and_then(|id| id.strip_prefix("agent-"))
        .and_then(|tail| tail.parse::<uuid::Uuid>().ok())
}

impl std::fmt::Debug for TurnFlowHooksConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnFlowHooksConfig")
            .field("compaction_throw_on_limit", &self.compaction_throw_on_limit)
            .field("permission_manager", &self.permission_manager.is_some())
            .field("goal_manager", &self.goal_manager.is_some())
            .field("hook_engine", &self.hook_engine.is_some())
            .field("interaction_kernel", &self.interaction_kernel.is_some())
            .field("suspension_notify", &self.suspension_notify.is_some())
            .field("suspension_resume", &self.suspension_resume.is_some())
            .finish()
    }
}

/// Interpret one klient `approvalResponse` (`{decision, scope?,
/// feedback?}`) for the ask at hand (plan D3) — the mapping shared by the
/// reverse-RPC reply branch so a grant-carrying ask approved with
/// `session`/`persist` scope applies the directory grant through the sink
/// and resolves plain `Approved` (never the session-scoped variant — a
/// directory grant is its own memory; the tool-name pattern must not be
/// memorized). Parsed leniently like every klient payload: an unknown
/// decision is a deny with the default feedback.
fn interpret_approval_response(
    response: &serde_json::Value,
    grant_dir: Option<&std::path::Path>,
    sink: Option<&dyn crate::session::gates::AdditionalDirGrantSink>,
) -> crate::session::gates::ApprovalDecision {
    let decision = response
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("rejected");
    if decision != "approved" {
        return crate::session::gates::ApprovalDecision::Denied(
            response
                .get("feedback")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("denied by user")
                .to_string(),
        );
    }
    let scope = response.get("scope").and_then(serde_json::Value::as_str);
    if let Some(dir) = grant_dir {
        match scope {
            Some("session") => {
                if let Some(sink) = sink {
                    sink.apply_grant(dir, false);
                }
                crate::session::gates::ApprovalDecision::Approved
            }
            Some("persist") => {
                if let Some(sink) = sink {
                    sink.apply_grant(dir, true);
                }
                crate::session::gates::ApprovalDecision::Approved
            }
            // `once` or an absent scope: a one-off approval — no state
            // change, matching the pre-D3 mapping.
            _ => crate::session::gates::ApprovalDecision::Approved,
        }
    } else {
        crate::session::gates::ApprovalDecision::Approved
    }
}

/// Append the NEVER-bypass contract (plan D4) to a denial text when the ask
/// carried a grantable dir: the user declined to grant the directory, so the
/// reason the model sees must add that the only legitimate route is asking
/// the user again — never routing around the sandbox through other tools,
/// shell commands, sub-agents, or alternate path spellings. Plain asks (no
/// grant dir) keep their pre-D4 text verbatim.
fn denial_contract_appended(feedback: &str, grant_dir: Option<&std::path::Path>) -> String {
    match grant_dir {
        Some(_) => format!("{feedback}. {OUTSIDE_SANDBOX_DENIAL_CONTRACT}"),
        None => feedback.to_string(),
    }
}

/// The `LoopHooks` implementation for `TurnFlow`.
///
/// Holds shared state via `Arc<Mutex<>>` so both the loop engine and the hooks
/// can access memory concurrently. Locks are held only for short synchronous
/// critical sections.
pub struct TurnFlowHooks {
    /// Shared context memory.
    memory: Arc<Mutex<ContextMemory>>,
    /// Shared compaction orchestrator.
    compaction: Arc<Mutex<FullCompaction>>,
    /// Shared steer buffer.
    steer_buffer: Arc<Mutex<SteerBuffer>>,
    /// Shared tool-call deduplicator.
    deduper: Arc<Mutex<ToolCallDeduplicator>>,
    /// Shared injection manager.
    injection: Arc<InjectionManager>,
    /// Cancellation token (cloned — the hooks don't own the cancel lifecycle).
    #[allow(dead_code)]
    cancel: CancelToken,
    /// Hooks configuration.
    config: TurnFlowHooksConfig,
    /// Optional interaction kernel (B4-a): the permission `Ask` flow's
    /// approval-gate seam (see [`TurnFlowHooksConfig::interaction_kernel`]).
    interaction_kernel: Option<Arc<InteractionKernel>>,
    /// The turn's ADR-006 wait registry — the permission `Ask` flow's gate
    /// wait registers its WaitingGate entry into it so the watchdog can
    /// pull the kernel's pending list at window expiry. Set per turn by
    /// the flow (the same fresh handle the turn body registers InStep
    /// with); `None` (tests, non-flow constructions) registers nothing.
    wait_registry: Option<Arc<crate::turn_flow::WaitRegistry>>,
    /// Optional suspension-boundary callback (ADR-011): the approval-gate
    /// edges report into it (see [`TurnFlowHooksConfig::suspension_notify`]).
    suspension_notify: Option<crate::subagent::SuspensionNotify>,
    /// The scheduler-only resume feed (see
    /// [`TurnFlowHooksConfig::suspension_resume`]).
    suspension_resume: Option<crate::subagent::SuspensionNotify>,
    /// LLM adapter (for auto-compaction).
    llm: Arc<dyn Llm>,
    /// Optional permission manager for the `authorize_tool_execution` hook.
    permission_manager: Option<Arc<PermissionManager>>,
    /// Optional reverse-RPC callback for `approval.ask` when the permission
    /// policy returns `Ask`. When `None`, the hook falls back to headless
    /// auto-approval or blocking.
    reverse_rpc_callback: Option<Arc<dyn ReverseRpcCallback>>,
    /// Optional goal manager for the `should_continue_after_stop` hook.
    goal_manager: Option<Arc<GoalManager>>,
    /// Optional telemetry service for emitting structured events.
    telemetry: Option<TelemetryService>,
    /// Timestamp of the most recent `pre_llm_request` call, used to compute
    /// LLM request duration in `post_llm_request`.
    llm_request_start: Mutex<Option<Instant>>,
    /// Start timestamps for in-flight tool calls, keyed by `tool_call_id`.
    /// Populated in `prepare_tool_execution` and consumed in
    /// `finalize_tool_result` to compute tool call duration for telemetry.
    tool_call_starts: Mutex<std::collections::HashMap<String, Instant>>,
    /// Optional tool result truncator for oversized tool outputs.
    tool_result_truncator: Option<ToolResultTruncator>,
    /// Optional task manager for post-compaction background task injection.
    task_manager: Option<Arc<TaskManager>>,
    /// The session's event bus (from the owning `TurnFlow`): the
    /// turn-internal auto-compaction lifecycle publishes its
    /// `SessionEvent`s here — the embedded layer cannot observe the
    /// compaction happening inside the turn loop.
    event_bus: Option<Arc<EventBus>>,
    /// The per-agent activity fold (observe-only recordings; never alters
    /// hook decisions).
    activity: Arc<ActivityTracker>,
    /// Whether the external Stop hook already forced one continuation this
    /// turn (TS `stopHookContinuationUsed` parity). The guard lives on the
    /// hooks instance because a fresh `TurnFlowHooks` is built per turn.
    stop_hook_continuation_used: Arc<AtomicBool>,
}

impl TurnFlowHooks {
    /// Create a new hooks instance.
    ///
    /// All shared state is passed as `Arc` clones from the `TurnFlow`.
    /// The `permission_manager` and `goal_manager` are optional — when
    /// `None`, the corresponding hooks use trait defaults.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory: Arc<Mutex<ContextMemory>>,
        compaction: Arc<Mutex<FullCompaction>>,
        steer_buffer: Arc<Mutex<SteerBuffer>>,
        deduper: Arc<Mutex<ToolCallDeduplicator>>,
        injection: Arc<InjectionManager>,
        cancel: CancelToken,
        config: TurnFlowHooksConfig,
        llm: Arc<dyn Llm>,
        permission_manager: Option<Arc<PermissionManager>>,
        goal_manager: Option<Arc<GoalManager>>,
        telemetry: Option<TelemetryService>,
        reverse_rpc_callback: Option<Arc<dyn ReverseRpcCallback>>,
        tool_result_truncator: Option<ToolResultTruncator>,
        task_manager: Option<Arc<TaskManager>>,
        event_bus: Option<Arc<EventBus>>,
        activity: Arc<ActivityTracker>,
        wait_registry: Option<Arc<crate::turn_flow::WaitRegistry>>,
    ) -> Self {
        Self {
            memory,
            compaction,
            steer_buffer,
            deduper,
            injection,
            cancel,
            permission_manager: config.permission_manager.clone().or(permission_manager),
            goal_manager: config.goal_manager.clone().or(goal_manager),
            interaction_kernel: config.interaction_kernel.clone(),
            wait_registry,
            suspension_notify: config.suspension_notify.clone(),
            suspension_resume: config.suspension_resume.clone(),
            reverse_rpc_callback,
            config,
            llm,
            telemetry,
            llm_request_start: Mutex::new(None),
            tool_call_starts: Mutex::new(std::collections::HashMap::new()),
            tool_result_truncator,
            task_manager,
            event_bus,
            activity,
            stop_hook_continuation_used: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl LoopHooks for TurnFlowHooks {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let memory = self.memory.clone();
        let compaction = self.compaction.clone();
        let steer_buffer = self.steer_buffer.clone();
        let deduper = self.deduper.clone();
        let injection = self.injection.clone();
        let throw_on_limit = self.config.compaction_throw_on_limit;
        let wire_recorder = self.config.wire_recorder.clone();
        let llm = self.llm.clone();
        let cancel = _ctx.cancel.clone();
        let telemetry = self.telemetry.clone();
        let hook_engine = self.config.hook_engine.clone();
        let hook_env = self.config.hook_env.clone();
        let task_manager = self.task_manager.clone();
        let event_bus = self.event_bus.clone();
        let activity = Arc::clone(&self.activity);
        let step = _ctx.step;
        let wire_turn_id = self.config.wire_turn_id.clone();
        // The flow's agent identity for the compaction lifecycle emits,
        // normalized the same way `TurnFlow::agent_id_or_main` does
        // (None ≡ "main": an unattributed flow is by definition the
        // session's main agent).
        let agent_id = self
            .config
            .agent_id
            .clone()
            .unwrap_or_else(|| "main".to_string());

        Box::pin(async move {
            // Announce the step on the session bus before anything else in
            // the step runs (the `turn.step.started` wire event). The turn
            // key is the wire turn id (`turn-<uuid>`) the dispatch plumbed
            // down — the same spelling `turn.started` / `turn.ended` carry;
            // a standalone flow falls back to the engine's numeric
            // per-flow counter.
            if let Some(ref event_bus) = event_bus {
                event_bus.publish(SessionEvent::TurnStepStarted {
                    turn_id: wire_turn_id.clone().unwrap_or_else(|| _ctx.turn_id.clone()),
                    step: _ctx.step,
                });
            }
            // Activity fold: the step counter advances before anything else
            // in the step runs (observe-only).
            activity.on_step(step);
            // 1. Auto-compaction check. If compaction should fire, start it
            //    in the background, then wait for it and apply the result to
            //    memory. This ensures the context is compacted before the
            //    LLM step runs.
            //
            // The begin() call is synchronous (it spawns a tokio task), so we
            // hold the compaction lock only for that short duration. We then
            // take the JoinHandle out, release the lock, await the task, and
            // re-lock to apply the result — never holding a std::sync::Mutex
            // guard across an .await point.
            {
                let should_compact = {
                    let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
                    let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                    comp.check_auto_compaction(&mem, throw_on_limit)?
                };

                if should_compact {
                    // Fire the PreCompact external hook before the compaction
                    // task starts (TS input shape: `{ trigger, tokenCount }`).
                    // Fire-and-forget: a block decision does not cancel
                    // compaction. The hooks' `additionalContext` rides along
                    // on the compaction request (TS parity:
                    // `injectHookAdditionalContext` + `historyForModel`).
                    let mut hook_additional_context = Vec::new();
                    if let Some(engine) = &hook_engine {
                        let tokens_before = memory
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .token_count_with_pending();
                        let input = serde_json::json!({
                            "trigger": "auto",
                            "tokenCount": tokens_before,
                        });
                        let result = engine
                            .trigger(HookEventType::PreCompact, input, &hook_env)
                            .await;
                        hook_additional_context =
                            hook_additional_context_messages(&result.additional_context);
                    }

                    // Start the background compaction (synchronous spawn).
                    {
                        let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
                        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                        let begin_result = comp.begin(
                            CompactionSource::Auto,
                            CompactionBeginParams {
                                llm: llm.clone(),
                                cancel: cancel.clone(),
                                custom_instruction: None,
                                max_output_size: None,
                                hook_additional_context,
                            },
                            &mem,
                        );
                        if let Err(ref e) = begin_result {
                            // Emit telemetry: compaction failed to begin.
                            if let Some(ref telemetry) = telemetry {
                                telemetry.track(TelemetryEvent::CompactionFailed {
                                    reason: e.to_string(),
                                });
                            }
                        }
                        begin_result?;
                        // Activity fold: the compaction window opened — its
                        // background entry gets a real start timestamp.
                        activity.on_compaction_started();
                        // Announce the auto compaction start on the session
                        // bus (mirrors the TS `compaction.started` emit at
                        // begin; instruction is always absent on the auto
                        // path).
                        if let Some(ref event_bus) = event_bus {
                            event_bus.publish(SessionEvent::CompactionStarted {
                                agent_id: agent_id.clone(),
                                trigger: CompactionSource::Auto,
                                instruction: None,
                            });
                        }
                    }

                    // Take the task handle out of the orchestrator so we can
                    // await it without holding the compaction lock.
                    let task_handle = {
                        let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
                        comp.take_active_task()
                    };

                    if let Some(handle) = task_handle {
                        // The step blocks on the summarizer for its whole
                        // duration (rust auto compaction always blocks) —
                        // announce the blocked turn before awaiting.
                        if let Some(ref event_bus) = event_bus {
                            event_bus.publish(SessionEvent::CompactionBlocked {
                                agent_id: agent_id.clone(),
                                turn_id: None,
                            });
                        }
                        // Await the background task, honoring cancellation.
                        let compaction_result = tokio::select! {
                            r = handle => {
                                r.map_err(|e| {
                                    let reason = format!("compaction task panicked: {}", e);
                                    // Emit telemetry: compaction failed due to
                                    // task panic.
                                    if let Some(ref telemetry) = telemetry {
                                        telemetry.track(TelemetryEvent::CompactionFailed {
                                            reason: reason.clone(),
                                        });
                                    }
                                    EngineError::CompactionFailed(reason)
                                })?.inspect_err(|e| {
                                    // Emit telemetry: compaction failed due to
                                    // an engine error from the task.
                                    if let Some(ref telemetry) = telemetry {
                                        telemetry.track(TelemetryEvent::CompactionFailed {
                                            reason: e.to_string(),
                                        });
                                    }
                                })?
                            }
                            _ = cancel.cancelled() => {
                                // The turn's cancellation interrupts the
                                // compaction the step is blocked on — the TS
                                // kernel's cancel() announces it.
                                if let Some(ref event_bus) = event_bus {
                                    event_bus.publish(SessionEvent::CompactionCancelled {
                                        agent_id: agent_id.clone(),
                                    });
                                }
                                return Err(EngineError::Aborted(
                                    "cancelled while waiting for auto-compaction".to_string(),
                                ));
                            }
                        };

                        // Capture token counts before applying, so we can emit
                        // CompactionFinished telemetry.
                        let tokens_before = compaction_result.tokens_before;

                        // Apply the compaction result to memory.
                        let applied_result = {
                            let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
                            let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                            comp.apply_compaction_result(&mut mem, compaction_result)
                        };

                        // Persist the applied compaction (TS parity:
                        // `contextMemoryService.applyCompaction` dispatches
                        // the apply op plus a follow-up size measurement) so
                        // `session.resume` replays the summary instead of
                        // the compacted-away history. A summary that failed
                        // the shrink check (变小校验) was never applied —
                        // persisting an apply op would corrupt resume, so
                        // nothing is recorded.
                        if applied_result.applied
                            && let Some(recorder) = wire_recorder.as_deref()
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
                            ) = crate::compaction::apply_compaction_op_fields(&applied_result);
                            recorder
                                .record_op(WireOp::ContextApplyCompaction {
                                    summary: applied_result.summary.clone(),
                                    compacted_count: applied_result.compacted_count as u32,
                                    tokens_before: Some(applied_result.tokens_before),
                                    tokens_after: Some(applied_result.tokens_after),
                                    context_summary: Some(applied_result.context_summary.clone()),
                                    dropped_count: Some(applied_result.dropped_count as u32),
                                    retained_tail_count: Some(
                                        applied_result.retained_tail_count as u32,
                                    ),
                                    // TS-dialect reader field — the rust
                                    // kernel never writes it (ADR-008).
                                    kept_user_message_count: None,
                                    kept_head_user_message_count: Some(
                                        applied_result.kept_head_user_message_count as u32,
                                    ),
                                    elision_message: applied_result.elision_message.clone(),
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
                                    tokens: applied_result.tokens_after,
                                })
                                .await;
                        }

                        // Announce the applied compaction on the session bus
                        // (the returned result carries the computed
                        // tokens_after).
                        if let Some(ref event_bus) = event_bus {
                            event_bus.publish(SessionEvent::CompactionCompleted {
                                agent_id: agent_id.clone(),
                                result: applied_result.clone(),
                            });
                        }

                        // Fire the PostCompact external hook with the
                        // post-compaction token count (TS input shape:
                        // `{ trigger, estimatedTokenCount }`). Fire-and-forget.
                        if let Some(engine) = &hook_engine {
                            let input = serde_json::json!({
                                "trigger": "auto",
                                "estimatedTokenCount": applied_result.tokens_after,
                            });
                            let _ = engine
                                .trigger(HookEventType::PostCompact, input, &hook_env)
                                .await;
                        }

                        // Emit telemetry: compaction finished successfully.
                        if let Some(ref telemetry) = telemetry {
                            telemetry.track(TelemetryEvent::CompactionFinished {
                                tokens_before,
                                tokens_after: applied_result.tokens_after,
                            });
                        }

                        // Post-compaction injection: re-surface still-running
                        // background tasks. Folding the live context to
                        // [recent user prompts, summary] drops the messages
                        // that started them and their status updates, so
                        // without this the model can forget a task is running
                        // and spawn a duplicate. Mirrors the TS
                        // `injectActiveBackgroundTasks` behavior. The
                        // reminder is recorded as a durable
                        // `context.append_message` op, exactly like the
                        // per-step injections below — replay (and the
                        // restored snapshot) must see the same reminder.
                        let mut post_compaction_reminders: Vec<String> = Vec::new();
                        {
                            if let Some(ref tm) = task_manager {
                                let running: Vec<AgentTask> = tm
                                    .list()
                                    .into_iter()
                                    .filter(|t| t.status == TaskStatus::Running)
                                    .collect();
                                if !running.is_empty() {
                                    let reminder = format_background_task_reminder(&running);
                                    memory
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .append_system_reminder(reminder.clone());
                                    post_compaction_reminders.push(reminder);
                                }
                            }
                        }
                        record_system_reminders(
                            wire_recorder.as_deref(),
                            &post_compaction_reminders,
                        )
                        .await;
                    }
                }
            }

            // 2. Flush steers (after compaction, so steers land in
            //    post-compaction context). The flush is durable (T3): the
            //    consumption marker `turn.steer_flushed` is recorded for
            //    the exact count taken, before the steers enter context
            //    memory, so a resume restores only the steers that never
            //    reached context. The take empties the buffer first — a
            //    steer pushed while the record is in flight lands in the
            //    next flush instead of being miscounted.
            {
                let taken = {
                    let mut sb = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
                    sb.take_items()
                };
                if !taken.is_empty() {
                    if let Some(recorder) = &wire_recorder {
                        recorder
                            .record_op(WireOp::TurnSteerFlushed {
                                count: taken.len() as u32,
                            })
                            .await;
                    }
                    record_notification_flush(&wire_recorder, &taken).await;
                    let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                    for steer in taken {
                        mem.append_user_message(steer.content, steer.origin);
                    }
                }
            }

            // 3. Inject per-step reminders. Each injected reminder is ALSO
            //    recorded as a durable `context.append_message` op — the
            //    wire replay must materialize the exact same system-reminder
            //    message the live session appended, or the restored
            //    history (and therefore the session snapshot) drifts from
            //    the live one (the wire replay equivalence invariant,
            //    ADR-009). Without the record the injected message exists
            //    only in the live memory.
            let injected_reminders = {
                let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                injection.inject(&mut mem)
            };
            record_system_reminders(wire_recorder.as_deref(), &injected_reminders).await;

            // 4. Begin dedup step.
            {
                deduper
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .begin_step();
            }

            Ok(BeforeStepResult::default())
        })
    }

    fn after_step(&self, _ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let deduper = self.deduper.clone();
        let event_bus = self.event_bus.clone();
        let wire_turn_id = self.config.wire_turn_id.clone();
        let usage_model_key = self.config.usage_model_key.clone();
        let turn_start_model = self.config.turn_start_model.clone();
        let memory = self.memory.clone();
        let wire_recorder = self.config.wire_recorder.clone();
        let pending_budget_reminder = self.config.pending_budget_reminder.clone();
        Box::pin(async move {
            deduper.lock().unwrap_or_else(|e| e.into_inner()).end_step();
            // The step's usage-attribution model: the same by-model key the
            // `record_step_usage` closure records under — the shared alias
            // handle read at completion time (so a mid-turn model switch
            // re-attributes the post-switch steps), falling back to the
            // turn-start `llm.model_name()` for standalone flows that never
            // wired an agent config (TS parity N4).
            let model = usage_model_key
                .and_then(|handle| handle.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .or_else(|| turn_start_model.clone());
            // Announce the step's completion on the session bus (the
            // `turn.step.completed` wire event, carrying the step's token
            // usage — the same wire turn id the started event uses).
            if let Some(ref event_bus) = event_bus {
                event_bus.publish(SessionEvent::TurnStepCompleted {
                    turn_id: wire_turn_id.unwrap_or_else(|| _ctx.turn_id.clone()),
                    step: _ctx.step,
                    input_tokens: _ctx.input_tokens,
                    output_tokens: _ctx.output_tokens,
                    cache_read_tokens: _ctx.cache_read_tokens,
                    cache_creation_tokens: _ctx.cache_creation_tokens,
                    model,
                });
            }
            // Goal hard-budget grace reminder (T9): the record_step_usage
            // callback granted the one-per-turn grace step and parked the
            // stop reminder. Flushed HERE — after the step's tool results
            // are recorded — so the next LLM request sees the reminder as
            // the trailing message (TS `handleAfterStep` parity). The
            // reminder is ALSO recorded durably: a session restored from
            // the wire log must see the same message the live session
            // appended (the wire replay equivalence invariant, ADR-009).
            // The `take` makes the flush one-shot by construction.
            if let Some(text) = pending_budget_reminder
                .as_ref()
                .and_then(|pending| pending.lock().unwrap_or_else(|e| e.into_inner()).take())
            {
                memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append_system_reminder(text.clone());
                record_system_reminders(wire_recorder.as_deref(), &[text]).await;
            }
            Ok(AfterStepResult::default())
        })
    }

    fn pre_llm_request(&self, _params: &LlmChatParams) {
        // Activity fold: the LLM request window opened (phase `streaming`).
        // Observe-only, always recorded — the fold must not depend on
        // telemetry being wired.
        self.activity.on_llm_request();
        // Record the start timestamp so `post_llm_request` can compute duration.
        if self.telemetry.is_some() {
            *self
                .llm_request_start
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    fn post_llm_request(&self, _response: &LlmChatResponse) {
        // Activity fold: the response arrived (phase back to `running`).
        self.activity.on_llm_response();
        if let Some(ref telemetry) = self.telemetry {
            let duration_ms = self
                .llm_request_start
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .map(|start| start.elapsed().as_millis() as u64)
                .unwrap_or(0);
            // Emit a Custom event for the LLM request completion. The
            // telemetry catalog does not have a dedicated LLM-request event
            // variant, so we use Custom with a descriptive name and the
            // measured duration.
            telemetry.track(TelemetryEvent::Custom {
                event_name: "llm_request_complete".to_string(),
                properties: {
                    let mut props = std::collections::HashMap::new();
                    props.insert(
                        "duration_ms".to_string(),
                        crate::telemetry::TelemetryValue::from(duration_ms),
                    );
                    props
                },
            });
        }
    }

    fn prepare_tool_execution(
        &self,
        ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        let deduper = self.deduper.clone();
        let memory = self.memory.clone();
        let hook_engine = self.config.hook_engine.clone();
        let hook_env = self.config.hook_env.clone();
        let flag_registry = self.config.flag_registry.clone();
        let tool_name = ctx.tool_name.to_string();
        let args = ctx.args.clone();
        let call_id = ctx.tool_call_id.clone();
        let turn_id = ctx.turn_id.to_string();
        let step = ctx.step as u64;
        let telemetry = self.telemetry.clone();

        // Activity fold: the tool call's execution window opened. Recorded
        // before the dedup decision so the window brackets
        // `prepare_tool_execution` → `finalize_tool_result` exactly (the
        // finalize hook runs for every outcome — allowed, blocked, or
        // synthetic — see `run_tool_batch`).
        self.activity
            .on_tool_call(&ctx.tool_call_id, &ctx.tool_name);

        // Record the start time for telemetry, keyed by tool_call_id.
        if self.telemetry.is_some() {
            let mut starts = self
                .tool_call_starts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            starts.insert(call_id.clone(), Instant::now());
        }

        Box::pin(async move {
            // Dedup (scoped: the guard must not live across the external
            // PreToolUse hook's await below).
            let duplicate = {
                let mut dedup = deduper.lock().unwrap_or_else(|e| e.into_inner());
                dedup.check_same_step(&tool_name, &args, &call_id)
            };
            if let Some(placeholder) = duplicate {
                // Emit telemetry: a same-step duplicate tool call was detected
                // and replaced with a synthetic result.
                if let Some(ref telemetry) = telemetry {
                    telemetry.track(TelemetryEvent::ToolCallDedupDetected {
                        tool_name: tool_name.clone(),
                        step,
                    });
                }
                return Ok(PrepareToolResult::Synthetic {
                    result: placeholder,
                });
            }

            // External PreToolUse hook (TS parity: `runPreToolUse`): fires
            // once the internal gates (dedup) have passed. `additionalContext`
            // is injected into context memory on both the allow and deny
            // paths (TS `includeBlock: true`); a deny blocks the call; a
            // returned `updatedInput` replaces the tool arguments (the TS
            // `hook-command-rewrite` enabled behavior — gated by the same
            // flag on both engines: default OFF, env
            // `MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE` to enable).
            if let Some(engine) = &hook_engine {
                let input = json!({
                    "tool_name": tool_name,
                    "tool_input": args,
                    "tool_call_id": call_id,
                    "step": step,
                    "turn": turn_id,
                });
                let result = engine
                    .trigger(HookEventType::PreToolUse, input, &hook_env)
                    .await;
                if !result.additional_context.is_empty() {
                    let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                    mem.append_user_message(
                        vec![ContentPart::Text {
                            text: result.additional_context.clone(),
                        }],
                        PromptOrigin::HookAdditionalContext,
                    );
                }
                if result.blocked {
                    return Ok(PrepareToolResult::Blocked {
                        reason: result
                            .block_reason
                            .clone()
                            .unwrap_or_else(|| "Blocked by PreToolUse hook".to_string()),
                    });
                }
                if let Some(updated) = result.updated_input {
                    // TS parity (`hook-command-rewrite` flag check before
                    // `event.rewriteArgs`): with the flag disabled the
                    // `updatedInput` is ignored and the call proceeds with
                    // its original arguments. The registry path resolves
                    // env → config → defaults (the TS `FlagService` chain);
                    // a registry-less flow falls back to the env-only chain.
                    let rewrite_enabled = match &flag_registry {
                        Some(registry) => registry.enabled(crate::flags::HOOK_COMMAND_REWRITE_FLAG),
                        None => {
                            crate::flags::env_flag_enabled(crate::flags::HOOK_COMMAND_REWRITE_FLAG)
                        }
                    };
                    if rewrite_enabled {
                        return Ok(PrepareToolResult::Rewritten { args: updated });
                    }
                    tracing::debug!(
                        tool_call_id = %call_id,
                        "PreToolUse updatedInput ignored: hook-command-rewrite flag disabled"
                    );
                }
            }

            Ok(PrepareToolResult::Allowed)
        })
    }

    fn authorize_tool_execution(
        &self,
        ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        // If no permission manager is wired, fall through to the default
        // (Approved). This preserves backward compatibility for callers that
        // haven't set up the permission system.
        let Some(pm) = &self.permission_manager else {
            return Box::pin(async { Ok(AuthorizeToolResult::Approved) });
        };

        // Build the PermissionPolicyContext from the AuthorizeToolContext.
        //
        // The AuthorizeToolContext carries the tool call info from the loop
        // engine. The PermissionPolicyContext additionally requires the
        // permission mode, rules, and session approvals — these live inside
        // the PermissionManager. We fetch owned copies via the public accessor
        // methods, then build the context with references to those locals.
        //
        // `accesses` is not carried by AuthorizeToolContext, so we extract it
        // from the tool call's arguments here. Only Read/Write/Edit carry a
        // reliable `file_path` argument; other tools (Bash, Glob, Grep) are
        // skipped because their path arguments are unreliable or not file
        // paths. The extracted accesses feed SensitiveFileAccessAskPolicy and
        // GitControlPathAccessAskPolicy, which gate access to sensitive files
        // and `.git/` control paths.
        let rules = pm.effective_rules();
        let approvals: std::collections::HashSet<String> =
            pm.all_session_approvals().into_iter().collect();
        let mode = pm.mode();
        let accesses = extract_file_accesses(&ctx.tool_name, ctx.args);

        let policy_ctx = PermissionPolicyContext {
            tool_name: &ctx.tool_name,
            tool_call_id: &ctx.tool_call_id,
            args: ctx.args,
            approval_rule: ctx.approval_rule,
            display: ctx.display,
            accesses: &accesses,
            mode,
            rules: &rules,
            session_approvals: &approvals,
            cancel: &ctx.cancel,
        };

        // Evaluate the policy chain (sync) — translation to AuthorizeToolResult
        // happens in the async block so we can issue a reverse RPC when the
        // policy returns `Ask`.
        let policy_result = pm.evaluate(&policy_ctx);

        // Emit telemetry: the permission policy chain made a decision.
        if let Some(ref telemetry) = self.telemetry {
            let decision = match &policy_result {
                PermissionPolicyResult::Approve { .. } => "approved",
                PermissionPolicyResult::Deny { .. } => "denied",
                PermissionPolicyResult::Ask { .. } => "ask",
                PermissionPolicyResult::Pass => "pass",
            };
            telemetry.track(TelemetryEvent::PermissionPolicyDecision {
                tool_name: ctx.tool_name.to_string(),
                decision: decision.to_string(),
                policy_name: "permission_manager".to_string(),
            });
        }

        let reverse_rpc = self.reverse_rpc_callback.clone();
        let activity = Arc::clone(&self.activity);
        let headless = pm.is_headless();
        let tool_name = ctx.tool_name.to_string();
        let tool_call_id = ctx.tool_call_id.to_string();
        let action = format!("execute {tool_name}");
        let turn_id = ctx.turn_id.parse::<u64>().unwrap_or(0);
        let ctx_display = serde_json::to_value(ctx.display).unwrap_or(json!({}));
        let ctx_cancel = ctx.cancel.clone();
        // The tool's input — rides the PermissionRequest observation payload
        // (TS parity: `toolInput` in `permission.approval.requested`).
        let tool_input = ctx.args.clone();
        // The approval-rule pattern this tool call was gated on — the
        // pattern a session-scoped approval memorizes (TS parity: the v2
        // kernel derives `sessionApprovalRule` from the execution's
        // `approvalRule` when the response is approved with session scope).
        let approval_rule = ctx.approval_rule.to_string();
        // The external hook engine whose `PermissionResult` hook fires after
        // the decision is made. Wired via `TurnFlow::set_external_hooks`;
        // `None` disables the fire.
        let hook_engine = self.config.hook_engine.clone();
        let hook_env = self.config.hook_env.clone();
        // The session's interaction kernel (B4-a): registers the ask with
        // the approval gate so the pending list and the decide RPC see it.
        let interaction_kernel = self.interaction_kernel.clone();
        // The suspension-boundary callback (ADR-011): the gate edges of a
        // member flow report into it — register announces `Suspended`, every
        // resolution path announces `Resumed` — so the wiring host publishes
        // the `subagent.suspended` / `subagent.started` pairing. `None`
        // (the main agent's flow, standalone flows) publishes nothing.
        let suspension_notify = self.suspension_notify.clone();
        // The scheduler-only resume feed (F3): the gate's decision-side
        // wake delivers into it — no bus publish — so `subagent.started`
        // stays with the turn-side Resumed.
        let suspension_resume = self.suspension_resume.clone();
        // The agent this flow runs as (supervision B1) — rides the ask
        // payload and the gate snapshot so the approval is attributable to
        // the requesting member. `None` (main agent / standalone flows)
        // leaves the rows unattributed, exactly as before.
        let flow_agent_id = self.config.agent_id.clone();
        // The wire recorder so the approval result is persisted as a
        // `permission.record_approval_result` record (TS parity:
        // `toolApprovalService` records every approval result; the reducer
        // derives the session-approval patterns on replay).
        let wire_recorder = self.config.wire_recorder.clone();
        // The permission manager, so a session-scoped approval is recorded
        // into the live session-approval set (the policy chain consults it
        // for subsequent calls of the same pattern).
        let permission_manager = Arc::clone(pm);
        // The turn's ADR-006 wait registry — cloned out of `&self` before
        // the async block below (the block outlives the borrow).
        let wait_registry = self.wait_registry.clone();

        Box::pin(async move {
            let outcome = match policy_result {
                PermissionPolicyResult::Approve { .. } => PermissionResultOutcome::Approved,
                PermissionPolicyResult::Deny { message, .. } => PermissionResultOutcome::Rejected {
                    feedback: message.unwrap_or_else(|| "Permission denied".to_string()),
                },
                PermissionPolicyResult::Ask {
                    reason, grant_dir, ..
                } => {
                    if let Some(ref callback) = reverse_rpc {
                        // Issue an `approval.ask` reverse RPC to the client.
                        // The client presents the approval prompt and returns
                        // `{ "decision": "approved"|"rejected"|"cancelled",
                        //   "scope"?: "session", "feedback"?: string }`
                        // (TS protocol shape) — `"scope": "session"` on an
                        // approved reply memorizes the approval-rule pattern
                        // for the rest of the session (parsed below).
                        //
                        // Activity fold: the approval wait is about to
                        // begin — record it as pending (keyed by the tool
                        // call id), and clear it on every resolution path
                        // (approved, rejected, errored, cancelled).
                        // Observe-only.
                        activity.on_approval_pending(&tool_call_id);
                        let mut approval_request = serde_json::json!({
                            "tool_name": tool_name,
                            "tool_call_id": tool_call_id.clone(),
                            "action": action,
                            "display": ctx_display,
                            "reason": reason.unwrap_or_else(|| "approval required".to_string()),
                        });
                        // B9: the engine approval-rule facet the call was
                        // gated on — the pattern an "always allow" upgrade
                        // or a session-scoped approval memorizes. Emitted
                        // only when the tool reported one (every builtin
                        // self-reports its name; the klient field is
                        // additive and optional).
                        if !approval_rule.is_empty() {
                            approval_request["approval_rule"] =
                                serde_json::json!(approval_rule.clone());
                        }
                        // Plan D3: a grant-carrying ask names the directory
                        // the user may grant into the sandbox. The response
                        // scope (`session`/`persist`) applies it — see
                        // `interpret_approval_response` below. Additive:
                        // absent when the policy asked without a grantable
                        // dir, and existing reverse-RPC consumers ignore it.
                        if let Some(dir) = &grant_dir {
                            approval_request["grant"] = serde_json::json!({ "dir": dir });
                        }
                        // B1 (supervision US5.3): the requesting member's
                        // identity — the full agent id (`agent-<uuid>`),
                        // emitted only when the flow runs as a member (the
                        // main agent's flow carries no id, so single-agent
                        // sessions stay unchanged). Additive: existing
                        // reverse-RPC consumers ignore the field.
                        if let Some(agent_id) = flow_agent_id.as_deref()
                            && member_agent_uuid(Some(agent_id)).is_some()
                        {
                            approval_request["agent_id"] = serde_json::json!(agent_id);
                        }
                        // TS parity (`permission.approval.requested`): the
                        // PermissionRequest external hook observes the ask —
                        // observation only, never a gate (a blocking hook
                        // result is discarded, exactly like the TS
                        // `registerPermissionHooks` fire-and-forget).
                        if let Some(engine) = &hook_engine {
                            let mut request_input = approval_request.clone();
                            request_input["tool_input"] = tool_input.clone();
                            let _ = engine
                                .trigger(HookEventType::PermissionRequest, request_input, &hook_env)
                                .await;
                        }
                        // B4-a: when the session's interaction kernel is
                        // wired, register the ask with the approval gate
                        // first — the gate is the pending source of truth
                        // (`approval.listPending` / `interaction.*` read
                        // it) — and stamp the minted request id into the
                        // ask payload (additive: existing reverse-RPC
                        // consumers ignore it). Both response channels
                        // then converge, first one wins: the reverse-RPC
                        // reply resolves the gate (so a later
                        // `approval.decide` finds the id gone), and an
                        // `approval.decide` / `interaction.respond`
                        // through the gate receiver resolves the ask while
                        // the reverse-RPC request is abandoned. Without a
                        // kernel the flow is byte-for-byte the pre-B4-a
                        // reverse-RPC ask.
                        let gate = interaction_kernel.as_ref().map(|kernel| {
                            let snapshot = crate::session::gates::ApprovalSnapshot {
                                tool_name: tool_name.clone(),
                                action: action.clone(),
                                display: approval_request["display"].clone(),
                                // B1: the requesting member's uuid when the
                                // flow runs as a member (the snapshot's
                                // `agent_id` contract), nil when
                                // unattributed — the pre-supervision form.
                                agent_id: member_agent_uuid(flow_agent_id.as_deref())
                                    .unwrap_or_else(uuid::Uuid::nil),
                                turn_id: Some(turn_id),
                                tool_call_id: Some(tool_call_id.clone()),
                                created_at: crate::session::gates::now_ms(),
                                // Same discipline as the ask payload: no
                                // facet reported → no approvalRule on the
                                // pending rows (never an empty string).
                                approval_rule: (!approval_rule.is_empty())
                                    .then(|| approval_rule.clone()),
                                // Plan D3: the grantable dir rides the
                                // snapshot (additive; `None` = a plain ask)
                                // so the decide/respond surfaces can apply a
                                // session/persist grant.
                                grant_dir: grant_dir.clone(),
                            };
                            let (request_id, receiver) = kernel
                                .approval_gate()
                                .register_with_resume_notify(
                                    snapshot,
                                    // ADR-011 (swarm T4): the DECISION-side
                                    // wake — the gate's `resolve` (the
                                    // `approval.decide` RPC handler) invokes
                                    // this independently of the asker's turn
                                    // polling, so a member parked by the
                                    // batch scheduler (whose turn is held
                                    // unpolled) still learns the decision.
                                    // The turn-side Resumed below only
                                    // fires when the flow is next polled.
                                    // The sink is the SCHEDULER-ONLY resume
                                    // feed (F3) — the public
                                    // `subagent.started` stays with the
                                    // turn-side Resumed, once per
                                    // suspension episode.
                                    suspension_resume.as_ref().map(|resume| {
                                        let resume = Arc::clone(resume);
                                        Box::new(
                                            move |_decision: crate::session::gates::ApprovalDecision| {
                                                resume(crate::subagent::SuspensionNotice::Resumed)
                                            },
                                        ) as Box<
                                            dyn FnOnce(crate::session::gates::ApprovalDecision)
                                                + Send,
                                        >
                                    }),
                                );
                            approval_request["request_id"] =
                                serde_json::json!(request_id.to_string());
                            // ADR-011: the gate registration is the
                            // member's suspension edge — announce the
                            // parking with the structured approval cause
                            // (the minted request id + the gated tool call
                            // id). The host's wired callback publishes
                            // `subagent.suspended`.
                            if let Some(notify) = suspension_notify.as_ref() {
                                notify(crate::subagent::SuspensionNotice::Suspended(
                                    crate::subagent::SuspendCause::Approval {
                                        request_id: request_id.to_string(),
                                        tool_call_id: Some(tool_call_id.clone()),
                                    },
                                ));
                            }
                            (kernel.clone(), request_id, receiver)
                        });
                        // Which response channel resolved the ask first.
                        enum AskChannel {
                            /// The reverse-RPC reply (or its delivery error).
                            Rpc(Result<serde_json::Value, String>),
                            /// The gate receiver: `approval.decide` /
                            /// `interaction.respond` won the race.
                            Gate(crate::session::gates::ApprovalDecision),
                            /// The turn was cancelled before either channel
                            /// answered.
                            Cancelled,
                        }
                        // ADR-006: the approval-gate wait registers its
                        // WaitingGate entry — the watchdog pulls the
                        // kernel's merged pending list at window expiry
                        // (a pending user decision is alive by design;
                        // an empty list means the ask settled and the
                        // waiter should return). Registered for the
                        // whole gate select below; the guard drops when
                        // the ask resolves.
                        let _approval_gate_wait = wait_registry.as_ref().and_then(|registry| {
                            gate.as_ref().map(|(kernel, _, _)| {
                                registry.enter(
                                    WaitKind::WaitingGate,
                                    WaitSubject::Gate {
                                        kernel: Arc::clone(kernel),
                                    },
                                )
                            })
                        });
                        let channel = match gate {
                            Some((kernel, request_id, mut receiver)) => {
                                // F1 backstop: if this ask future is dropped
                                // while still waiting (the swarm scheduler
                                // settles a parked member aborted without
                                // polling its turn), the `ctx_cancel` arm
                                // below never runs — the guard withdraws the
                                // gate entry on drop instead.
                                let ask_guard =
                                    PendingAskGuard::new(Arc::clone(&kernel), request_id);
                                // ADR-011 e (swarm T5): the gate is the ONLY
                                // source of truth for whether the ask is
                                // pending — the reverse-RPC channel is a
                                // pure notify face. The first delivery attempt
                                // races the gate receiver and the turn's
                                // cancellation; a delivery failure (dropped
                                // connection / no subscriber) NEVER resolves
                                // the ask — the entry stays listed and the
                                // flow keeps waiting on the gate receiver
                                // (`approval.decide` / `interaction.respond`
                                // still decide). Only the turn's own
                                // cancellation withdraws the entry.
                                let mut channel = tokio::select! {
                                    r = callback.call("approval.ask", approval_request) => {
                                        match &r {
                                            // The reverse-RPC reply arrived
                                            // first: resolve the gate so the
                                            // resolution is recorded and a
                                            // later `approval.decide` reports
                                            // not-found; the mapping below is
                                            // the pre-B4-a one, verbatim.
                                            Ok(response) => {
                                                // The reverse-RPC reply
                                                // decision for THIS ask: a
                                                // grant-carrying ask approved
                                                // with session/persist scope
                                                // applies the directory grant
                                                // through the kernel's sink
                                                // BEFORE the gate resolves
                                                // (plan D3 — the same
                                                // interpretation
                                                // `approval.decide` /
                                                // `interaction.respond`
                                                // apply at their decision
                                                // sources).
                                                let decision =
                                                    interpret_approval_response(
                                                        response,
                                                        grant_dir.as_deref(),
                                                        kernel
                                                            .grant_sink()
                                                            .as_deref(),
                                                    );
                                                let _ = kernel
                                                    .approval_gate()
                                                    .resolve(request_id, decision);
                                            }
                                            Err(err) => {
                                                // Delivery failed: keep the
                                                // ask pending on the gate —
                                                // this is not a user decision
                                                // and not a cancellation.
                                                tracing::warn!(
                                                    request_id = %request_id,
                                                    error = %err,
                                                    "approval.ask delivery failed; \
                                                     keeping the ask pending on the gate"
                                                );
                                            }
                                        };
                                        AskChannel::Rpc(r)
                                    }
                                    decision = &mut receiver => AskChannel::Gate(decision.unwrap_or(
                                        crate::session::gates::ApprovalDecision::Timeout,
                                    )),
                                    _ = ctx_cancel.cancelled() => {
                                        kernel.approval_gate().withdraw(request_id);
                                        AskChannel::Cancelled
                                    }
                                };
                                // A delivery failure is not a resolution:
                                // keep waiting on the gate receiver until a
                                // decision arrives or the turn is cancelled
                                // (cancellation semantics unchanged — the
                                // `ctx_cancel` arm still withdraws the entry
                                // and maps to `Cancelled`).
                                if matches!(channel, AskChannel::Rpc(Err(_))) {
                                    channel = tokio::select! {
                                        decision = &mut receiver => AskChannel::Gate(decision.unwrap_or(
                                            crate::session::gates::ApprovalDecision::Timeout,
                                        )),
                                        _ = ctx_cancel.cancelled() => {
                                            kernel.approval_gate().withdraw(request_id);
                                            AskChannel::Cancelled
                                        }
                                    };
                                }
                                // ADR-011: every gate-registered ask ends
                                // here — the reverse-RPC reply, the gate
                                // decision, or the turn's cancellation — so
                                // the member is no longer suspended. Announce
                                // the resumption edge so the suspension
                                // pairing stays closed even when the ask
                                // resolves into a turn failure (the terminal
                                // event follows). The guard marks settled so
                                // its drop-side withdraw backstop stays
                                // silent on resolution paths.
                                ask_guard.mark_settled();
                                if let Some(notify) = suspension_notify.as_ref() {
                                    notify(crate::subagent::SuspensionNotice::Resumed);
                                }
                                channel
                            }
                            None => match tokio::select! {
                                r = callback.call("approval.ask", approval_request) => Some(r),
                                _ = ctx_cancel.cancelled() => None,
                            } {
                                Some(r) => AskChannel::Rpc(r),
                                None => AskChannel::Cancelled,
                            },
                        };
                        let (resolved, record_result) = match channel {
                            AskChannel::Rpc(Ok(response)) => {
                                // The reverse-RPC reply carries the TS
                                // `ApprovalResponse` shape: `{ "decision",
                                // "scope"?, "feedback"? }`. The same value is
                                // persisted verbatim as the approval
                                // record's `result` so replay derives the
                                // same session-approval patterns.
                                let mut record_result = serde_json::Map::new();
                                if let Some(word) =
                                    response.get("decision").and_then(serde_json::Value::as_str)
                                {
                                    record_result.insert("decision".to_string(), json!(word));
                                }
                                if let Some(scope) =
                                    response.get("scope").and_then(serde_json::Value::as_str)
                                {
                                    record_result.insert("scope".to_string(), json!(scope));
                                }
                                if let Some(feedback) =
                                    response.get("feedback").and_then(serde_json::Value::as_str)
                                {
                                    record_result.insert("feedback".to_string(), json!(feedback));
                                }
                                let decision = response
                                    .get("decision")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("rejected");
                                let resolved = match decision {
                                    "approved" => PermissionResultOutcome::Approved,
                                    _ => {
                                        let feedback = response
                                            .get("feedback")
                                            .and_then(serde_json::Value::as_str)
                                            .unwrap_or("denied by user");
                                        // Plan D4: a denied/cancelled GRANT
                                        // ask appends the NEVER-bypass
                                        // contract — see
                                        // `denial_contract_appended`.
                                        PermissionResultOutcome::Rejected {
                                            feedback: denial_contract_appended(
                                                feedback,
                                                grant_dir.as_deref(),
                                            ),
                                        }
                                    }
                                };
                                (resolved, Some(serde_json::Value::Object(record_result)))
                            }
                            AskChannel::Rpc(Err(err)) => (
                                PermissionResultOutcome::Error {
                                    error: format!("approval request failed: {err}"),
                                },
                                None,
                            ),
                            AskChannel::Gate(crate::session::gates::ApprovalDecision::Approved) => {
                                (
                                    PermissionResultOutcome::Approved,
                                    Some(json!({ "decision": "approved" })),
                                )
                            }
                            AskChannel::Gate(
                                crate::session::gates::ApprovalDecision::ApprovedForSession,
                            ) => (
                                PermissionResultOutcome::Approved,
                                Some(json!({ "decision": "approved", "scope": "session" })),
                            ),
                            AskChannel::Gate(crate::session::gates::ApprovalDecision::Denied(
                                feedback,
                            )) => (
                                // Plan D4: a denied GRANT ask appends the
                                // NEVER-bypass contract to the reason the
                                // model sees; the wire record keeps the raw
                                // user feedback for replay fidelity.
                                PermissionResultOutcome::Rejected {
                                    feedback: denial_contract_appended(
                                        &feedback,
                                        grant_dir.as_deref(),
                                    ),
                                },
                                Some(json!({ "decision": "rejected", "feedback": feedback })),
                            ),
                            // A withdrawn/cancelled ask (Timeout) maps to the
                            // same cancellation error the legacy cancel path
                            // reports. Plan D4: a cancelled GRANT ask
                            // appends the NEVER-bypass contract as well —
                            // the outside path stays unauthorized, so the
                            // model must ask again, never bypass.
                            AskChannel::Gate(crate::session::gates::ApprovalDecision::Timeout)
                            | AskChannel::Cancelled => (
                                PermissionResultOutcome::Error {
                                    error: denial_contract_appended(
                                        "cancelled by user",
                                        grant_dir.as_deref(),
                                    ),
                                },
                                None,
                            ),
                        };
                        activity.on_approval_resolved(&tool_call_id);

                        // Persist the approval result as a
                        // `permission.record_approval_result` wire record (TS
                        // parity: `toolApprovalService` records every resolved
                        // approval). A record whose decision was approved with
                        // session scope also memorizes the approval-rule
                        // pattern — into the live session-approval set here,
                        // and (via the reducer) into the replayed state so a
                        // resume restores it.
                        if let Some(result) = &record_result {
                            let session_approved = result.get("decision")
                                == Some(&serde_json::Value::String("approved".to_string()))
                                && result.get("scope")
                                    == Some(&serde_json::Value::String("session".to_string()));
                            // Plan D3: a GRANT-carrying ask is never
                            // memorized as a session approval-rule pattern —
                            // the directory grant itself is the memory
                            // (memorizing "Read" for the session would
                            // over-approve every later read). The scope was
                            // applied as a directory grant at the decision
                            // source (the reply branch above, or the
                            // `approval.decide` / `interaction.respond`
                            // handlers that resolved the gate).
                            let grant_ask = grant_dir.is_some();
                            let mut record = serde_json::Map::new();
                            record.insert("turnId".to_string(), json!(turn_id));
                            record.insert("toolCallId".to_string(), json!(tool_call_id.clone()));
                            record.insert("toolName".to_string(), json!(tool_name.clone()));
                            record.insert("action".to_string(), json!(action.clone()));
                            if session_approved && !grant_ask {
                                record.insert(
                                    "sessionApprovalRule".to_string(),
                                    json!(approval_rule.clone()),
                                );
                            }
                            record.insert("result".to_string(), result.clone());
                            if let Some(recorder) = &wire_recorder {
                                recorder
                                    .record_op(WireOp::PermissionRecordApprovalResult {
                                        record: serde_json::Value::Object(record),
                                    })
                                    .await;
                            }
                            if session_approved && !grant_ask {
                                permission_manager.record_approval(approval_rule.clone());
                            }
                        }

                        resolved
                    } else if headless {
                        // No callback wired and headless mode: auto-approve
                        // (the host is expected to have its own approval flow
                        // or has opted into auto-approval).
                        PermissionResultOutcome::Approved
                    } else {
                        // No callback wired and non-headless: block to
                        // prevent silent auto-approval of tools that require
                        // user confirmation.
                        PermissionResultOutcome::Rejected {
                            feedback: "approval required but no approval mechanism available"
                                .to_string(),
                        }
                    }
                }
                PermissionPolicyResult::Pass => PermissionResultOutcome::Approved,
            };

            // Fire the PermissionResult external hook with the final decision
            // (observation-only, fire-and-forget). `PermissionResult` is
            // wired through every decision path: approved, rejected, error.
            if let Some(engine) = &hook_engine {
                fire_permission_result(
                    engine,
                    &hook_env,
                    turn_id,
                    &tool_call_id,
                    &tool_name,
                    &action,
                    &outcome,
                )
                .await;
            }

            Ok(outcome.into_authorize_tool_result())
        })
    }

    fn finalize_tool_result(
        &self,
        ctx: FinalizeToolContext<'_>,
    ) -> HookFuture<EngineResult<FinalizeToolResult>> {
        // Activity fold: the tool call's execution window closed (paired
        // with the `prepare_tool_execution` recording, whatever the result).
        self.activity.on_tool_call_end(&ctx.tool_call_id);
        // Do the work synchronously — the dedup lock is a std::sync::Mutex
        // which doesn't need an async context, and ctx.result is borrowed
        // so it can't be moved into a 'static async block.

        // Apply tool result truncation FIRST: if the output exceeds MAX_CHARS,
        // persist the full text to disk and replace the model-visible output
        // with a bounded preview. This must happen before dedup so that:
        // (a) the dedup cache stores the truncated preview, not the full output,
        // and (b) dedup escalation reminders are appended to the preview and
        // not subsequently overwritten by truncation.
        // Mirrors the TS `budgetToolResultForModel`.
        if let Some(ref truncator) = self.tool_result_truncator {
            let placeholder = ToolResult::success(String::new());
            let current = std::mem::replace(ctx.result, placeholder);
            let truncated =
                truncator.truncate_for_model(&ctx.tool_name, &ctx.tool_call_id, current);
            *ctx.result = truncated;
        }

        let mut dedup = self.deduper.lock().unwrap_or_else(|e| e.into_inner());
        dedup.finalize_result(&ctx.tool_call_id, ctx.result);
        drop(dedup);

        // Emit ToolCall telemetry event with duration and success flag.
        if let Some(ref telemetry) = self.telemetry {
            let duration_ms = self
                .tool_call_starts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&ctx.tool_call_id)
                .map(|start| start.elapsed().as_millis() as u64)
                .unwrap_or(0);
            telemetry.track(TelemetryEvent::ToolCall {
                tool_name: ctx.tool_name.to_string(),
                success: !ctx.result.is_error,
                duration_ms,
            });
        }

        Box::pin(async { Ok(FinalizeToolResult::default()) })
    }

    fn should_continue_after_stop(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        let memory = self.memory.clone();
        let steer_buffer = self.steer_buffer.clone();
        let goal_manager = self.goal_manager.clone();
        let activity = Arc::clone(&self.activity);
        let wire_recorder = self.config.wire_recorder.clone();
        let hook_engine = self.config.hook_engine.clone();
        let hook_env = self.config.hook_env.clone();
        let stop_hook_used = Arc::clone(&self.stop_hook_continuation_used);

        Box::pin(async move {
            // Activity fold: the turn reached a terminal stop and the host
            // is deciding its fate — mark the ending phase (observe-only;
            // cleared below when the decision is to continue).
            activity.on_ending(None);

            // Layer 0: flush NON-notification steers first (always, even if
            // stopping). The flush records its durable consumption marker
            // (`turn.steer_flushed`) for exactly the non-notification steers it
            // consumes, before they enter context memory, so a resume restores
            // only steers that never reached context (T3).
            //
            // Background-task notification steers are deliberately LEFT
            // PARKED: folding them at the stop boundary lands the
            // notification after the turn's final summary and forces a
            // perfunctory extra model step, hiding the real summary (the
            // turn-final-summary defect). The parked notification instead
            // delivers via the task driver's idle wakeup as its own turn on
            // the next tick — the stranded-buffer conversion in
            // `task_driver.rs` records the matching flush marker, so the
            // parked `turn.steer` wire records stay replay-consistent
            // (SDR-B2-01). Notifications re-parked here keep their FIFO
            // relative order.
            let flushed_non_notifications = {
                let to_flush = {
                    let mut sb = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let taken = sb.take_items();
                    let mut to_flush = Vec::with_capacity(taken.len());
                    let mut to_park = Vec::new();
                    for steer in taken {
                        if matches!(steer.origin, PromptOrigin::BackgroundTask { .. }) {
                            to_park.push(steer);
                        } else {
                            to_flush.push(steer);
                        }
                    }
                    for steer in to_park {
                        sb.push(steer.content, steer.origin);
                    }
                    to_flush
                };
                if to_flush.is_empty() {
                    false
                } else {
                    // The marker covers only the non-notification steers
                    // consumed here (user steers keep their unrecorded
                    // replay contract, so no `record_notification_flush`
                    // call — the taken set holds no background-task origin
                    // by construction).
                    if let Some(recorder) = &wire_recorder {
                        recorder
                            .record_op(WireOp::TurnSteerFlushed {
                                count: to_flush.len() as u32,
                            })
                            .await;
                    }
                    let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                    for steer in to_flush {
                        mem.append_user_message(steer.content, steer.origin);
                    }
                    true
                }
            };

            // Layer 1: if non-notification steers were flushed, continue the
            // turn so the model can respond to the steered content. A
            // parked notification alone does not continue the turn (see
            // Layer 0).
            if flushed_non_notifications {
                activity.on_ending_cleared();
                return Ok(ContinueAfterStopResult {
                    continue_turn: true,
                });
            }

            // Layer 2: goal budget enforcement.
            //
            // If a goal manager is wired and a goal is active:
            // - If the goal is complete, stop (the objective was achieved).
            // - If the goal's budget is exhausted, stop (no more resources).
            // - If the goal is still active with budget remaining, continue
            //   the turn so the model can make further progress.
            //
            // If no goal manager is wired, or no goal is active, fall through
            // to the default (stop).
            if let Some(gm) = &goal_manager
                && gm.is_goal_active()
            {
                if gm.is_goal_complete() {
                    // Goal is complete — stop.
                    return Ok(ContinueAfterStopResult {
                        continue_turn: false,
                    });
                }
                if gm.is_budget_exhausted() {
                    // Budget exhausted — stop.
                    return Ok(ContinueAfterStopResult {
                        continue_turn: false,
                    });
                }
                // Goal is active with budget remaining — continue.
                activity.on_ending_cleared();
                return Ok(ContinueAfterStopResult {
                    continue_turn: true,
                });
            }

            // Layer 3: external Stop hook (TS parity:
            // `AgentExternalHooksService.runStop`): a blocking Stop hook
            // forces the turn to continue, with the block reason appended
            // to context memory as a system-triggered user message (TS
            // origin `system_trigger`/`stop_hook`). The once-per-turn guard
            // mirrors the TS `stopHookContinuationUsed` state — a second
            // natural stop ends the turn even if the hook keeps blocking,
            // so a misbehaving hook cannot loop the turn forever.
            if let Some(engine) = &hook_engine
                && !stop_hook_used.load(Ordering::Acquire)
            {
                let input = json!({
                    "stop_hook_active": false,
                    "stop_reason": ctx.stop_reason,
                    "turn": ctx.turn_id,
                });
                let result = engine.trigger(HookEventType::Stop, input, &hook_env).await;
                if result.blocked {
                    let reason = result
                        .block_reason
                        .clone()
                        .unwrap_or_else(|| "Blocked by Stop hook".to_string());
                    stop_hook_used.store(true, Ordering::Release);
                    {
                        let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
                        mem.append_user_message(
                            vec![ContentPart::Text {
                                text: reason.clone(),
                            }],
                            PromptOrigin::SystemTrigger,
                        );
                    }
                    record_stop_hook_reason(&wire_recorder, &reason).await;
                    activity.on_ending_cleared();
                    return Ok(ContinueAfterStopResult {
                        continue_turn: true,
                    });
                }
            }

            // Layer 4: no steers, no active goal, no blocking Stop hook —
            // stop.
            Ok(ContinueAfterStopResult {
                continue_turn: false,
            })
        })
    }
}

/// Extract file-path accesses from a tool call's arguments.
///
/// Only `Read`, `Write`, and `Edit` carry a reliable `path` string
/// argument. Other tools (Bash, Glob, Grep) are skipped — their path-like
/// arguments are either unreliable (Bash commands embed paths in arbitrary
/// shell syntax) or not individual file paths (Glob/Grep patterns).
///
/// The returned accesses feed `SensitiveFileAccessAskPolicy` and
/// `GitControlPathAccessAskPolicy`, which gate access to sensitive files
/// (`.env`, SSH keys, credentials) and `.git/` control paths.
fn extract_file_accesses(tool_name: &str, args: &serde_json::Value) -> Vec<ToolAccess> {
    let operation = match tool_name {
        "Read" => AccessOperation::Read,
        "Write" | "Edit" => AccessOperation::Write,
        _ => return Vec::new(),
    };
    if let Some(path_str) = args.get("path").and_then(|v| v.as_str()) {
        vec![ToolAccess {
            kind: AccessKind::File,
            path: Some(path_str.to_string()),
            operation,
        }]
    } else {
        Vec::new()
    }
}

/// Guidance text prepended to the post-compaction background task reminder.
///
/// Mirrors the TS `ACTIVE_BACKGROUND_TASK_GUIDANCE` constant verbatim —
/// including the trailing "completion arrives via automatic notification",
/// the correct guidance once the B2 notification loop exists (the pre-B2
/// wording pointed the model at polling instead).
const ACTIVE_BACKGROUND_TASK_GUIDANCE: &str = "\
The conversation was compacted, so the earlier messages that started these background tasks are gone — but the tasks are still running from before. \
Do not start duplicates. Use TaskList to list them, TaskOutput for a non-blocking status/output snapshot, and TaskStop to cancel one — completion arrives via automatic notification.";

/// Record one durable `context.append_message` per background-task
/// notification steer a flush consumed (SDR-B2-01's narrow durability
/// scope): a notification parked mid-turn must survive a crash between the
/// flush and the turn's own context append — the wire record replays it on
/// resume. User steers deliberately keep the pre-existing unrecorded replay
/// contract (generalizing that is a separate engagement).
async fn record_notification_flush(
    wire_recorder: &Option<Arc<dyn WireOpRecorder>>,
    taken: &[BufferedSteer],
) {
    let Some(recorder) = wire_recorder else {
        return;
    };
    for steer in taken {
        if !matches!(steer.origin, PromptOrigin::BackgroundTask { .. }) {
            continue;
        }
        let Ok(content) = serde_json::to_value(&steer.content) else {
            continue;
        };
        let Ok(origin) = serde_json::to_value(&steer.origin) else {
            continue;
        };
        recorder
            .record_op(WireOp::ContextAppendMessage {
                message: serde_json::json!({
                    "role": "user",
                    "content": content,
                    "origin": origin,
                }),
            })
            .await;
    }
}

/// Record the durable `context.append_message` wire op for a Stop-hook
/// continuation reason: the injected message must survive a crash between
/// the forced continuation and the turn's own context append, so
/// `session.resume` replays the reason instead of losing it (the same
/// durability scope the background-task notification flush applies).
async fn record_stop_hook_reason(wire_recorder: &Option<Arc<dyn WireOpRecorder>>, reason: &str) {
    let Some(recorder) = wire_recorder else {
        return;
    };
    recorder
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": reason }],
                "origin": { "kind": "system_trigger" },
            }),
        })
        .await;
}

/// Format the post-compaction reminder for still-running background tasks.
///
/// Each task is listed with its id, description, and status. The reminder is
/// appended as a system reminder so the next compaction drops and rebuilds
/// it — kept fresh, never stacked.
pub(crate) fn format_background_task_reminder(tasks: &[AgentTask]) -> String {
    let mut text = format!("{ACTIVE_BACKGROUND_TASK_GUIDANCE}\n\n");
    for (i, task) in tasks.iter().enumerate() {
        text.push_str(&format!(
            "{}. [{}] {} (id: {})\n",
            i + 1,
            match &task.status {
                TaskStatus::Running => "running",
                TaskStatus::Completed { .. } => "completed",
                TaskStatus::Failed(_) => "failed",
                TaskStatus::Stopped => "stopped",
            },
            task.description,
            task.id,
        ));
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::permission::{
        PermissionManager, PermissionMode, PermissionPolicyContext, PermissionPolicyResult,
    };
    use crate::task::TaskKind;
    use crate::tool::ToolInputDisplay;
    use std::collections::HashSet;

    /// Given a Read tool call on `.env`, when the permission policy chain is
    /// evaluated, then the `SensitiveFileAccessAskPolicy` (policy #13 in the
    /// chain) should return `Ask` — proving that `accesses` is correctly
    /// populated and the policy can now trigger.
    ///
    /// Without the fix (empty `accesses`), the chain would fall through to
    /// `DefaultToolApprovePolicy` (#17) which auto-approves `Read`, so the
    /// result would be `Approve` instead of `Ask`.
    #[test]
    fn should_trigger_sensitive_file_access_policy_when_reading_env_file() {
        let manager = PermissionManager::new(PermissionMode::Manual, Vec::new());

        let args = serde_json::json!({"path": ".env"});
        let accesses = extract_file_accesses("Read", &args);
        assert_eq!(accesses.len(), 1, "Read on .env should produce one access");

        let cancel = CancelToken::new();
        let display = ToolInputDisplay::FileIo {
            path: ".env".into(),
            operation: "read".into(),
        };
        let ctx = PermissionPolicyContext {
            tool_name: "Read",
            tool_call_id: "call-1",
            args: &args,
            approval_rule: "",
            display: &display,
            accesses: &accesses,
            mode: PermissionMode::Manual,
            rules: &[],
            session_approvals: &HashSet::new(),
            cancel: &cancel,
        };

        // Evaluate the full policy chain. Without the fix the result would be
        // Approve (from DefaultToolApprovePolicy). With the fix the
        // SensitiveFileAccessAskPolicy fires first and returns Ask.
        let result = manager.evaluate(&ctx);
        assert!(
            matches!(result, PermissionPolicyResult::Ask { .. }),
            "expected Ask for reading .env, got {result:?}"
        );
    }

    /// Given a Write tool call on a regular file, when accesses are extracted,
    /// then the access should have `AccessOperation::Write`.
    #[test]
    fn should_extract_write_access_when_tool_is_write() {
        let args = serde_json::json!({"path": "src/main.rs"});
        let accesses = extract_file_accesses("Write", &args);
        assert_eq!(accesses.len(), 1);
        assert_eq!(accesses[0].kind, AccessKind::File);
        assert_eq!(accesses[0].operation, AccessOperation::Write);
        assert_eq!(accesses[0].path.as_deref(), Some("src/main.rs"));
    }

    /// Given an Edit tool call on a regular file, when accesses are extracted,
    /// then the access should have `AccessOperation::Write`.
    #[test]
    fn should_extract_write_access_when_tool_is_edit() {
        let args = serde_json::json!({"path": "src/lib.rs"});
        let accesses = extract_file_accesses("Edit", &args);
        assert_eq!(accesses.len(), 1);
        assert_eq!(accesses[0].operation, AccessOperation::Write);
    }

    /// Given a Read tool call, when accesses are extracted, then the access
    /// should have `AccessOperation::Read`.
    #[test]
    fn should_extract_read_access_when_tool_is_read() {
        let args = serde_json::json!({"path": "README.md"});
        let accesses = extract_file_accesses("Read", &args);
        assert_eq!(accesses.len(), 1);
        assert_eq!(accesses[0].operation, AccessOperation::Read);
    }

    /// Given a non-file tool (Bash, Glob, Grep), when accesses are extracted,
    /// then no accesses should be produced.
    #[test]
    fn should_produce_no_accesses_when_tool_is_not_file_based() {
        for tool_name in ["Bash", "Glob", "Grep", "Agent", "WebSearch"] {
            let args = serde_json::json!({"command": "ls", "pattern": "*.rs"});
            let accesses = extract_file_accesses(tool_name, &args);
            assert!(
                accesses.is_empty(),
                "tool '{tool_name}' should produce no file accesses"
            );
        }
    }

    /// Given a Read tool call without a `path` argument, when accesses are
    /// extracted, then no accesses should be produced.
    #[test]
    fn should_produce_no_accesses_when_path_argument_is_missing() {
        let args = serde_json::json!({});
        let accesses = extract_file_accesses("Read", &args);
        assert!(accesses.is_empty());
    }

    // -----------------------------------------------------------------------
    // format_background_task_reminder
    // -----------------------------------------------------------------------

    /// Given running background tasks, when formatting the reminder, then the
    /// guidance text and task details are included.
    #[test]
    fn should_include_guidance_and_task_details_when_formatting_running_tasks() {
        let tm = TaskManager::new();
        let id = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build watcher".into(),
            "session-1".into(),
        );

        let tasks = tm.list();
        let reminder = format_background_task_reminder(&tasks);

        assert!(reminder.contains("conversation was compacted"));
        assert!(reminder.contains("Do not start duplicates"));
        assert!(
            reminder.contains("completion arrives via automatic notification"),
            "the guidance must tell the model notifications arrive on their own (TS ACTIVE_BACKGROUND_TASK_GUIDANCE): {reminder}"
        );
        assert!(reminder.contains("build watcher"));
        assert!(reminder.contains(&id));
        assert!(reminder.contains("[running]"));
    }

    /// Given no running tasks, when formatting the reminder, then only the
    /// guidance text is present with no task entries.
    #[test]
    fn should_contain_only_guidance_when_no_tasks_are_provided() {
        let reminder = format_background_task_reminder(&[]);
        assert!(reminder.contains("conversation was compacted"));
        assert!(!reminder.contains("1."));
    }

    /// Given completed and failed tasks, when formatting the reminder, then
    /// their statuses are reflected in the output.
    #[test]
    fn should_reflect_non_running_statuses_when_formatting_mixed_tasks() {
        let tm = TaskManager::new();
        let _running_id = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "running task".into(),
            "s1".into(),
        );
        let completed_id = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done task".into(),
            "s1".into(),
        );
        let failed_id = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "errored task".into(),
            "s1".into(),
        );
        tm.complete(&completed_id, 0);
        tm.fail(&failed_id, "kaboom".into());

        let tasks = tm.list();
        let reminder = format_background_task_reminder(&tasks);

        assert!(reminder.contains("[running]"));
        assert!(reminder.contains("[completed]"));
        assert!(reminder.contains("[failed]"));
        assert!(reminder.contains("running task"));
        assert!(reminder.contains("done task"));
        assert!(reminder.contains("errored task"));
    }

    // -----------------------------------------------------------------------
    // interpret_approval_response (plan D3 grant semantics)
    // -----------------------------------------------------------------------

    /// A recording grant sink for the pure-function tests.
    #[derive(Default)]
    struct RecordingGrantSink {
        calls: Arc<Mutex<Vec<(std::path::PathBuf, bool)>>>,
    }

    impl crate::session::gates::AdditionalDirGrantSink for RecordingGrantSink {
        fn apply_grant(&self, dir: &std::path::Path, persist: bool) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((dir.to_path_buf(), persist));
        }
    }

    /// Given a grant-carrying ask, when the reply approves with session
    /// scope, then the dir grant reaches the sink (not persisted) and the
    /// returned decision is plain `Approved` — never the session-scoped
    /// variant.
    #[test]
    fn should_apply_session_dir_grant_when_reply_approves_a_grant_ask_with_session_scope() {
        // Given a sink and a grantable dir
        let sink = RecordingGrantSink::default();
        let dir = std::path::Path::new("/tmp/outside-dir");

        // When the reply approves with session scope
        let decision = interpret_approval_response(
            &serde_json::json!({ "decision": "approved", "scope": "session" }),
            Some(dir),
            Some(&sink),
        );

        // Then the grant is applied without persisting and the decision is
        // plain Approved
        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls, vec![(dir.to_path_buf(), false)]);
        assert!(matches!(
            decision,
            crate::session::gates::ApprovalDecision::Approved
        ));
    }

    /// Given a grant-carrying ask, when the reply approves with persist
    /// scope, then the dir grant reaches the sink with persist=true.
    #[test]
    fn should_apply_persisted_dir_grant_when_reply_approves_a_grant_ask_with_persist_scope() {
        let sink = RecordingGrantSink::default();
        let dir = std::path::Path::new("/tmp/outside-dir");

        let decision = interpret_approval_response(
            &serde_json::json!({ "decision": "approved", "scope": "persist" }),
            Some(dir),
            Some(&sink),
        );

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls, vec![(dir.to_path_buf(), true)]);
        assert!(matches!(
            decision,
            crate::session::gates::ApprovalDecision::Approved
        ));
    }

    /// Given a grant-carrying ask, when the reply approves with once scope
    /// (or has no scope), then no grant is applied — a one-off approval.
    #[test]
    fn should_not_apply_dir_grant_when_reply_approves_a_grant_ask_without_session_or_persist_scope()
    {
        for response in [
            serde_json::json!({ "decision": "approved" }),
            serde_json::json!({ "decision": "approved", "scope": "once" }),
        ] {
            let sink = RecordingGrantSink::default();

            let decision = interpret_approval_response(
                &response,
                Some(std::path::Path::new("/tmp/x")),
                Some(&sink),
            );

            let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
            assert!(calls.is_empty(), "no scope must mean a one-off approval");
            assert!(matches!(
                decision,
                crate::session::gates::ApprovalDecision::Approved
            ));
        }
    }

    /// Given a grant-carrying ask, when the reply rejects (or cancels), then
    /// no grant is applied and the decision carries the feedback.
    #[test]
    fn should_not_apply_dir_grant_when_reply_rejects_a_grant_ask() {
        let sink = RecordingGrantSink::default();

        let decision = interpret_approval_response(
            &serde_json::json!({ "decision": "rejected", "feedback": "no" }),
            Some(std::path::Path::new("/tmp/x")),
            Some(&sink),
        );

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(calls.is_empty(), "a rejected ask must not apply any grant");
        assert!(matches!(
            decision,
            crate::session::gates::ApprovalDecision::Denied(feedback) if feedback == "no"
        ));
    }

    /// Given a plain ask without a grant dir, when the reply approves with
    /// session scope, then the sink is untouched and the decision stays the
    /// pre-D3 mapping (Approved — the pattern memorization happens in the
    /// record branch, not here).
    #[test]
    fn should_not_apply_dir_grant_when_reply_approves_a_nongrant_ask_with_session_scope() {
        let sink = RecordingGrantSink::default();

        let decision = interpret_approval_response(
            &serde_json::json!({ "decision": "approved", "scope": "session" }),
            None,
            Some(&sink),
        );

        let calls = sink.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(calls.is_empty(), "a nongrant ask must never touch the sink");
        assert!(matches!(
            decision,
            crate::session::gates::ApprovalDecision::Approved
        ));
    }

    /// Given a grant-carrying ask, when its denial text is assembled for the
    /// model, then the NEVER-bypass contract (plan D4) is appended to the
    /// user feedback — the user declined to grant the directory, so the only
    /// legitimate route is asking again, never routing around the sandbox.
    #[test]
    fn should_append_the_never_bypass_contract_when_a_grant_ask_is_rejected() {
        let dir = std::path::Path::new("/tmp/outside-dir");
        assert_eq!(
            denial_contract_appended("no", Some(dir)),
            format!("no. {OUTSIDE_SANDBOX_DENIAL_CONTRACT}")
        );
        assert_eq!(
            denial_contract_appended("cancelled by user", Some(dir)),
            format!("cancelled by user. {OUTSIDE_SANDBOX_DENIAL_CONTRACT}")
        );
    }

    /// Given a plain ask without a grant dir, when its denial text is
    /// assembled, then the text stays verbatim — the NEVER-bypass contract
    /// belongs to outside-sandbox authorization outcomes only.
    #[test]
    fn should_keep_denial_text_verbatim_when_an_ask_carries_no_grant_dir() {
        assert_eq!(denial_contract_appended("no", None), "no");
        assert_eq!(
            denial_contract_appended("cancelled by user", None),
            "cancelled by user"
        );
    }
}
