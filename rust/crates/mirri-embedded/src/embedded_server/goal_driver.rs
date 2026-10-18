//! The goal settlement driver: folds a settled turn's outcome into the
//! session goal's terminal state.
//!
//! Mirrors the TS kernel's `goalService.settleAbnormalTurn` semantics: a
//! turn that ends abnormally (stalled / aborted / failed / blocked) must not
//! leave an Active goal dangling — the settlement moves it to Paused (or
//! Blocked) with a TS-parity reason, via the record-before-mutate three-step
//! the `message.abort` goal-pause block established (wire op first, then the
//! in-memory status, then the bus projection).
//!
//! Deliberately free functions (NOT methods on `EmbeddedServer`): the
//! fire/steer spawn blocks hold no `&EmbeddedServer`, and no spawned task
//! may capture `Arc<EmbeddedServer>` (see the cron driver's comment).
//!
//! Event publishing goes through the W3 routing seam
//! (`session_routing::resolve_artifacts_via`) — the same free-function
//! artifact-resolution path the cron/task drivers use to reach the
//! session's event bus without holding the server.

use std::collections::HashMap;
use std::sync::Arc;

use mirri_core::event_projection::turn_stop_reason_wire;
use mirri_core::{
    AgentId, CancelToken, ContentPart, GoalStatus, InteractionKernel, PromptOrigin, SessionEvent,
    SessionScope, TurnStopReason, goal_status_wire,
};
use mirri_proto::WireOp;
use serde_json::json;

use super::additional_dirs::session_grant_sink;
use super::cron_driver::{
    ActivityTrackerMap, CompactionHandleMap, SteerBufferMap, UsageModelKeyHandleMap,
};
use super::goal_snapshot_json;
use super::message_handlers::{EngineTurnError, run_engine_turn};
use super::session_index::note_turn_settled;
use super::session_routing::resolve_artifacts_via;
use super::{InFlightTurn, InFlightTurnsMap, WaitTurnGuard, coerce_aborted_if_cancelled};
use crate::LlmFactory;
use crate::workspace_registry::WorkspaceRegistry;
use mirri_session_index::SessionIndex;

/// How one settled turn leaves the session goal. Mirrors TS
/// goalService.settleAbnormalTurn semantics.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalSettlement {
    /// Goal moved out of Active with this terminal status and reason.
    Settled(GoalStatus, String),
    /// Goal untouched (no goal / not active / normal completion).
    Unchanged,
}

/// A settled turn's outcome, folded from the coerced stop reason or the
/// engine error arm.
#[derive(Debug)]
pub(super) enum TurnOutcome<'a> {
    Completed,
    Stalled,
    Aborted,
    Failed(&'a str),
    BlockedByHook,
}

/// Pure mapping from a settled turn's outcome to the goal's terminal
/// (status, reason). Completed keeps Active with an empty reason.
pub(super) fn outcome_goal_target(outcome: &TurnOutcome<'_>) -> (GoalStatus, String) {
    match outcome {
        TurnOutcome::Completed => (GoalStatus::Active, String::new()),
        TurnOutcome::Stalled => (GoalStatus::Paused, "Paused after stalled turn".into()),
        TurnOutcome::Aborted => (GoalStatus::Paused, "Paused after interruption".into()),
        TurnOutcome::Failed("") => (GoalStatus::Paused, "Paused after turn failure".into()),
        TurnOutcome::Failed(err) => (
            GoalStatus::Paused,
            format!("Paused after turn failure: {err}"),
        ),
        TurnOutcome::BlockedByHook => (
            GoalStatus::Blocked,
            "Blocked by UserPromptSubmit hook".into(),
        ),
    }
}

/// stop_reason -> TurnOutcome (Failed only comes from the Err arm).
pub(super) fn turn_outcome(stop_reason: &TurnStopReason) -> TurnOutcome<'static> {
    match stop_reason {
        TurnStopReason::EndTurn
        | TurnStopReason::MaxTokens
        | TurnStopReason::Filtered
        | TurnStopReason::Paused
        | TurnStopReason::Unknown => TurnOutcome::Completed,
        TurnStopReason::Stalled => TurnOutcome::Stalled,
        TurnStopReason::Aborted => TurnOutcome::Aborted,
        TurnStopReason::BlockedByHook => TurnOutcome::BlockedByHook,
    }
}

/// The embedded server's per-agent turn-flow map (the same shape
/// [`EmbeddedServer::turn_flows`](super::EmbeddedServer) owns) — the goal
/// chain's busy probe locks it, exactly the cron driver's probe.
pub(super) type TurnFlowMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<mirri_core::TurnFlow>>>>>;

/// The idle-streak burn guard: consecutive continuation turns with no tool
/// calls and no goal-state change that the chain task tolerates before
/// pausing the goal (rust-only hardening — TS relies on prompt semantics).
pub(super) const GOAL_MAX_IDLE_CONTINUATIONS: u32 = 3;
/// The absolute continuation burn cap: total continuations since the current
/// goal activation the chain task tolerates before pausing the goal
/// (rust-only hardening against a model that runs one trivial tool per turn).
pub(super) const GOAL_MAX_CONTINUATIONS: u32 = 20;

/// Per-session goal-chain bookkeeping. Lives in a server-level map
/// (`EmbeddedServer::goal_chain_states`) so the settlement points — the
/// fire/steer spawn blocks hold no `&EmbeddedServer` — and the chain task
/// can both reach it through a cloned Arc: no `Arc<EmbeddedServer>` capture
/// anywhere (the hard rule the cron/task drivers set).
///
/// Sessions that close leave stale entries behind; the map is bounded by
/// the process's session count, the same stance as `relay_seq_watermarks`.
#[derive(Default)]
pub(super) struct GoalChainState {
    /// A chain task for this session is running. Cleared by the chain task
    /// itself when its loop exits.
    pub(super) chain_running: bool,
    /// Consecutive continuation turns with no tool calls and no goal-state
    /// change — the idle-streak burn guard.
    pub(super) idle_streak: u32,
    /// Total continuations since the current goal activation — the absolute
    /// burn cap. Reset when a user-originated turn runs or the goal changes.
    pub(super) continuations: u32,
}

/// The server-level goal-chain bookkeeping map (session id -> state). A type
/// alias so the field declaration and every free-function signature agree
/// on the same shape as the sibling map fields (`relay_seq_watermarks`,
/// `turn_flows`, ...).
pub(super) type GoalChainStateMap = Arc<std::sync::Mutex<HashMap<String, GoalChainState>>>;

/// The record-before-mutate three-step the `message.abort` goal-pause block
/// established, shared by every goal-lifecycle writer (the abnormal-turn
/// settlement, the budget block, and the chain guards):
/// 1. the `goal.update` wire op lands first,
/// 2. then the in-memory status changes,
/// 3. then the `GoalUpdated` event reaches the session's bus.
///
/// The write is Active-only and unconditioned by the caller's intent: a goal
/// the user (or a prior writer) already parked must never be resurrected or
/// double-settled — the guard is here so every writer shares it.
pub(super) async fn write_goal_lifecycle(
    registry: &WorkspaceRegistry,
    session: &SessionScope,
    status: GoalStatus,
    reason: String,
    actor: &str,
) {
    let goal_manager = session.goal();
    let Some(goal) = goal_manager.get() else {
        return;
    };
    if goal.status != GoalStatus::Active {
        return;
    }
    // 1) record-before-mutate: the goal.update wire op lands before the
    //    in-memory status change.
    session
        .record_op(WireOp::GoalUpdate {
            goal_id: None,
            status: Some(goal_status_wire(status).to_string()),
            reason: Some(reason.clone()),
            turns_used: None,
            tokens_used: None,
            wall_clock_ms: None,
            budget_limits: None,
            actor: Some(actor.to_string()),
        })
        .await;
    // 2) in-memory mutation.
    goal_manager.update_with_reason(status, Some(reason.clone()));
    // 3) bus projection — the same artifact-resolution path the cron/task
    //    drivers use (a free function, so spawn blocks without `self` can
    //    reach the event bus).
    let session_id = session.id().as_str().to_string();
    if let Ok(artifacts) = super::session_routing::resolve_artifacts_via(registry, &session_id) {
        let snapshot = goal_manager
            .get()
            .map(|current| goal_snapshot_json(&current, &goal_manager))
            .unwrap_or(serde_json::Value::Null);
        artifacts.event_bus.publish(SessionEvent::GoalUpdated {
            snapshot,
            change: Some(json!({
                "kind": "lifecycle",
                "status": goal_status_wire(status),
                "reason": reason,
                "actor": actor,
            })),
        });
    }
}

/// Block the session goal after its budget ran out. The reason is the
/// TS-parity prefix; the TS detail suffix (`": turn budget 40, ..."`) is an
/// ADR open item.
pub(super) async fn block_goal_on_budget(registry: &WorkspaceRegistry, session: &SessionScope) {
    write_goal_lifecycle(
        registry,
        session,
        GoalStatus::Blocked,
        "Blocked after goal budget reached".into(),
        "runtime",
    )
    .await;
}

/// Evaluate the TS `handleTurnEnded` continuation conditions for one settled
/// turn and, when a continuation is due, spawn the chain task.
///
/// Called at every settlement point after [`settle_goal_after_turn`] with
/// the turn the settled flow belongs to (the canonical main agent's, per
/// the "main agent goal only" scope). Free function: the fire/steer spawn
/// blocks hold no `&EmbeddedServer` (the hard rule), so the busy probe runs
/// against the shared `turn_flows` map and the background-wait probe against
/// the session's task manager — both reachable from a spawn block through
/// cloned field Arcs. Conditions, in the TS order:
/// 1. no goal / not Active → return; budget exhausted → block the goal;
/// 2. busy flow (a turn is in flight) → return (that turn's own settle
///    point is the wake-up path; the flow mutex is the sole arbiter);
/// 3. a non-terminal background task is pending → return (its notification
///    turn's settle point is the wake-up path);
/// 4. burn guards on the chain state (idle streak / absolute cap) → pause;
/// 5. no chain running → spawn one (the injection loop this task runs is
///    the "keep going until the goal decides" main line).
pub(super) async fn maybe_launch_goal_chain(
    registry: &Arc<WorkspaceRegistry>,
    states: &GoalChainStateMap,
    turn_flows: &TurnFlowMap,
    session: &Arc<SessionScope>,
    agent_id: &str,
    res: &ChainResources,
) {
    let goal_manager = session.goal();
    // 1. No goal / not Active: nothing to continue (a settle already parked
    //    it, or the user paused it).
    if !goal_manager.is_goal_active() {
        return;
    }
    if goal_manager.is_budget_exhausted() {
        block_goal_on_budget(registry, session).await;
        return;
    }
    // 2. Busy: a turn is in flight on the main agent's flow — its own settle
    //    point re-enters here (the flow mutex is the sole arbiter between
    //    the drivers; no data race, and the settle point runs on the engine
    //    task AFTER the loop releases the lock).
    let flow_key = format!("{}:{}", session.id().as_str(), agent_id);
    let busy = turn_flows
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&flow_key)
        .is_some_and(|flow| flow.try_lock().is_err());
    if busy {
        return;
    }
    // 3. A pending background task suspends the goal: the task's
    //    notification turn (its settle point) is the wake-up path. Only the
    //    main agent's goal is driven, so only tasks the main agent waits on
    //    matter — and every non-terminal task's notification eventually
    //    addresses the main agent (or the dispatcher, itself main-scoped).
    if session
        .tasks()
        .list()
        .iter()
        .any(|task| !task.status.is_terminal())
    {
        return;
    }
    // 4. Burn guards (rust-only hardening): a stalled-out or runaway chain
    //    parks the goal instead of burning the budget. The guards read
    //    under the lock and decide BEFORE any await: a std MutexGuard is
    //    not Send, and this function runs inside tokio::spawn'd tasks
    //    (the fire/steer settle blocks), so the guard must never live
    //    across an await.
    let guard_hit = {
        let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(session.id().as_str().to_string()).or_default();
        if state.idle_streak >= GOAL_MAX_IDLE_CONTINUATIONS {
            state.idle_streak = 0;
            Some(0)
        } else if state.continuations >= GOAL_MAX_CONTINUATIONS {
            Some(1)
        } else {
            None
        }
    };
    match guard_hit {
        Some(0) => {
            write_goal_lifecycle(
                registry,
                session,
                GoalStatus::Paused,
                "Paused after repeated idle goal turns".into(),
                "runtime",
            )
            .await;
            return;
        }
        Some(_) => {
            write_goal_lifecycle(
                registry,
                session,
                GoalStatus::Paused,
                "Paused after continuation limit".into(),
                "runtime",
            )
            .await;
            return;
        }
        None => {}
    }
    // 5. Launch the chain task. The task holds no `&EmbeddedServer` — only
    //    the field Arcs it needs (the hard rule). The map lock is taken
    //    once more here and dropped before the spawn — same not-Send rule
    //    as the guards above.
    let launched = {
        let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(session.id().as_str().to_string()).or_default();
        if state.chain_running {
            false
        } else {
            state.chain_running = true;
            true
        }
    };
    if launched {
        launch_goal_chain(
            ChainArgs {
                states: Arc::clone(states),
                turn_flows: Arc::clone(turn_flows),
                session: Arc::clone(session),
                agent_id: agent_id.to_string(),
            },
            ChainResources {
                activity_trackers: Arc::clone(&res.activity_trackers),
                steer_buffers: Arc::clone(&res.steer_buffers),
                compaction_handles: Arc::clone(&res.compaction_handles),
                usage_model_key_handles: Arc::clone(&res.usage_model_key_handles),
                llm_factory: res.llm_factory.clone(),
                in_flight_turns: Arc::clone(&res.in_flight_turns),
                server_config: Arc::clone(&res.server_config),
                flag_registry: Arc::clone(&res.flag_registry),
                turn_stall_timeout: res.turn_stall_timeout,
                turn_backstop_timeout: res.turn_backstop_timeout,
                index: res.index.clone(),
                registry: Arc::clone(registry),
            },
        );
    }
}

/// Everything the chain task needs, cloned from the settlement point's
/// captured field Arcs — deliberately NOT holding `&EmbeddedServer` or
/// `Arc<EmbeddedServer>` (the hard rule the cron/task drivers set).
struct ChainArgs {
    states: GoalChainStateMap,
    turn_flows: TurnFlowMap,
    session: Arc<SessionScope>,
    agent_id: String,
}

/// The chain task's captures: everything the injection loop needs,
/// cloned from the settlement point's already-captured field Arcs —
/// deliberately NOT holding `&EmbeddedServer` or `Arc<EmbeddedServer>`
/// (the hard rule the cron/task drivers set).
pub(super) struct ChainResources {
    registry: Arc<WorkspaceRegistry>,
    activity_trackers: ActivityTrackerMap,
    steer_buffers: SteerBufferMap,
    compaction_handles: CompactionHandleMap,
    usage_model_key_handles: UsageModelKeyHandleMap,
    llm_factory: Option<Arc<dyn LlmFactory>>,
    in_flight_turns: InFlightTurnsMap,
    server_config: Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    flag_registry: Arc<mirri_core::FlagRegistry>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    index: Option<Arc<SessionIndex>>,
}

impl super::EmbeddedServer {
    /// The chain resources snapshot for one settlement point — the same
    /// field-Arc cloning the cron driver's spawn site applies.
    pub(super) fn goal_chain_resources(&self) -> ChainResources {
        ChainResources {
            registry: Arc::clone(&self.registry),
            activity_trackers: Arc::clone(&self.activity_trackers),
            steer_buffers: Arc::clone(&self.steer_buffers),
            compaction_handles: Arc::clone(&self.compaction_handles),
            usage_model_key_handles: Arc::clone(&self.usage_model_key_handles),
            llm_factory: self.llm_factory.clone(),
            in_flight_turns: Arc::clone(&self.in_flight_turns),
            server_config: Arc::clone(&self.config),
            flag_registry: Arc::clone(self.flag_registry()),
            turn_stall_timeout: self.turn_stall_timeout,
            turn_backstop_timeout: self.turn_backstop_timeout,
            index: self.session_index(),
        }
    }
}

/// TS parity (goalService.GOAL_CONTINUATION_PROMPT): the user-role
/// message that launches each automatic goal continuation turn.
pub(super) const GOAL_CONTINUATION_PROMPT: &str = "Continue working toward the active goal. \
Keep the self-audit brief. Do not explore unrelated interpretations once the goal can be \
decided. If the objective is simple, already answered, impossible, unsafe, or contradictory, \
do not run another goal turn. Explain briefly if useful, then call UpdateGoal with `complete` \
or `blocked` in the same turn. Otherwise, weigh the objective and any completion criteria \
against the work done so far, choose one bounded, useful slice of work, and use the existing \
conversation context and your tools. Do not try to finish a broad goal in one turn unless the \
whole goal is genuinely small. Most goal turns should not call UpdateGoal: after completing \
a useful slice, if material work remains, end the turn normally without calling UpdateGoal \
so the runtime can continue the goal in the next turn. Call UpdateGoal with `complete` only \
when all required work is done, any stated validation has passed, and there is no useful next \
action. Completion audit: before calling `complete`, verify the current state against the \
actual objective and every explicit requirement. Treat weak or indirect evidence as not \
complete. Do not mark complete after only producing a plan, summary, first pass, or partial \
result. Do not mark complete merely because a budget is nearly exhausted or you want to stop. \
Blocked audit: do not call UpdateGoal with `blocked` the first time you hit a blocker. Use \
`blocked` only for a genuine impasse: an external condition, required user input, missing \
credentials or permissions, or a persistent technical failure. For those non-terminal \
blockers, the same blocking condition must repeat for at least 3 consecutive goal turns \
before you call `blocked`, counting the original/user-triggered turn and automatic \
continuations. If a previously blocked goal is resumed, treat the resumed run as a fresh \
blocked audit. Exception: if the objective itself is impossible, unsafe, or contradictory, \
call UpdateGoal with `blocked` in the same turn; do not run more goal turns just to satisfy \
the audit. Do not use `blocked` because the work is large, hard, slow, uncertain, incomplete, \
still needs validation, would benefit from clarification, or needs more goal turns. Once the \
3-turn threshold is met and you cannot make meaningful progress without user input or an \
external-state change, call UpdateGoal with `blocked`; do not keep reporting the blocker while \
leaving the goal active. Do not ask the user for input unless a real blocker prevents progress. \
Background task completions arrive automatically as messages — never poll for them (no \
repeated TaskOutput or TaskList status checks). When the next step truly depends on a \
background result, use TaskWait — one explicit blocking wait per task; to wait on several \
tasks, call TaskWait once per task in one parallel block instead of polling. When only \
waiting remains, end the turn without further tool calls; the engine suspends the goal loop \
and resumes automatically when a result arrives — this is the efficient path, not idleness. \
Do not report the goal as blocked merely because you are waiting on background work; \
suspension handles that. Parallel foreground work is fine while background tasks run.";

/// Spawn the goal continuation chain task — the "keep going until the
/// goal decides" main line. The task first try-locks the turn flow: a
/// user turn that raced in owns the session, so the chain resets its
/// bookkeeping and exits WITHOUT writing anything (this dissolves the
/// v1 TOCTOU: the wire bracket is written only after the flow lock is
/// held). Once it holds the flow it loops: re-check conditions, inject
/// one continuation turn, settle it, update the chain state — exiting
/// the moment a condition fails.
fn launch_goal_chain(args: ChainArgs, res: ChainResources) {
    tokio::spawn(async move {
        let ChainArgs {
            states,
            turn_flows,
            session,
            agent_id,
        } = args;
        let ChainResources {
            registry,
            activity_trackers,
            steer_buffers,
            compaction_handles,
            usage_model_key_handles,
            llm_factory,
            in_flight_turns,
            server_config,
            flag_registry,
            turn_stall_timeout,
            turn_backstop_timeout,
            index,
            ..
        } = res;
        let session_id = session.id().as_str().to_string();
        let flow_key = format!("{session_id}:{agent_id}");
        // The loop's per-iteration state: whether the previous iteration
        // yielded the flow to a user turn (its settlement re-enters the
        // gate, so the chain exits instead of spinning).
        loop {
            // (a) Re-check the launch conditions — anything the user or
            // another driver did since the spawn parks the goal instead.
            if !chain_should_continue(&registry, &states, &session, &flow_key, &session_id).await {
                break;
            }
            // (b) Try-lock the flow: a user turn that grabbed it first
            // means the chain yields — no turn is injected, nothing is
            // written (the user turn's settlement re-enters the gate).
            // The injection holds the lock only across the synchronous
            // flow get-or-create + the wire bracket writes (the TOCTOU
            // dissolve: the bracket writes only while this task owns the
            // flow) — `run_engine_turn` takes its own lock afterwards.
            let outcome = inject_goal_continuation_turn(
                &registry,
                &turn_flows,
                &activity_trackers,
                &steer_buffers,
                &compaction_handles,
                &usage_model_key_handles,
                &llm_factory,
                &in_flight_turns,
                &server_config,
                &flag_registry,
                index.clone(),
                turn_stall_timeout,
                turn_backstop_timeout,
                &session,
                &agent_id,
                &session_id,
            )
            .await;
            // A `None` outcome means the flow was busy (a user turn won
            // the race) — exit; the user turn's settlement re-enters the
            // gate.
            let Some(outcome) = outcome else {
                break;
            };
            // (d) Update the chain bookkeeping from the settled turn.
            if !note_chain_outcome(&states, &session, &session_id, outcome) {
                break;
            }
        }
        // Loop exit: clear the running flag, and drop the session's entry
        // when the goal is terminal (the map keeps stale entries for
        // closed sessions only — the documented bounded stance).
        let goal_terminal = session
            .goal()
            .get()
            .is_none_or(|goal| goal.status != GoalStatus::Active);
        let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
        match states.get_mut(&session_id) {
            Some(state) => {
                state.chain_running = false;
                if goal_terminal {
                    states.remove(&session_id);
                }
            }
            None => {
                states.entry(session_id).or_default().chain_running = false;
            }
        }
    });
}

/// One settled continuation turn, as the chain bookkeeping sees it. The
/// goal settlement already ran inside the injection (the settle block
/// the cron site established) — this struct only carries the signals the
/// chain bookkeeping needs.
struct ChainTurnOutcome {
    /// The engine error text, when the turn failed outright (a failed
    /// turn is never idle — the chain parks the goal through the
    /// settlement, but the bookkeeping still counts it).
    error_text: Option<String>,
    /// The turn's step count — the idle signal (0 steps = idle).
    steps: u32,
    /// Whether the settlement moved the goal out of Active.
    settled: bool,
}

/// The per-iteration re-check (the TS handleTurnEnded conditions minus
/// the launch gate — the chain already launched). `false` means the
/// loop must exit; a guard trip writes the pause here.
async fn chain_should_continue(
    registry: &Arc<WorkspaceRegistry>,
    states: &GoalChainStateMap,
    session: &SessionScope,
    flow_key: &str,
    session_id: &str,
) -> bool {
    let goal_manager = session.goal();
    // Goal gone or no longer Active (user paused / settled it): exit.
    if !goal_manager.is_goal_active() {
        return false;
    }
    // Budget exhausted between iterations: block and exit.
    if goal_manager.is_budget_exhausted() {
        block_goal_on_budget(registry, session).await;
        return false;
    }
    // A pending background task suspends the chain: exit (the task's
    // notification turn re-enters the gate).
    if session
        .tasks()
        .list()
        .iter()
        .any(|task| !task.status.is_terminal())
    {
        return false;
    }
    // Burn guards (the same two the launch gate applies, re-checked per
    // iteration so an in-loop trip parks the goal here).
    let guard_hit = {
        let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(session_id.to_string()).or_default();
        if state.idle_streak >= GOAL_MAX_IDLE_CONTINUATIONS {
            state.idle_streak = 0;
            Some(0)
        } else if state.continuations >= GOAL_MAX_CONTINUATIONS {
            Some(1)
        } else {
            None
        }
    };
    match guard_hit {
        Some(0) => {
            write_goal_lifecycle(
                registry,
                session,
                GoalStatus::Paused,
                "Paused after repeated idle goal turns".into(),
                "runtime",
            )
            .await;
            false
        }
        Some(_) => {
            write_goal_lifecycle(
                registry,
                session,
                GoalStatus::Paused,
                "Paused after continuation limit".into(),
                "runtime",
            )
            .await;
            false
        }
        None => {
            let _ = flow_key;
            true
        }
    }
}

/// The user-turn reset: a user-originated turn's settlement clears the
/// burn guards (the human is steering again — the counters restart).
/// Called from the message.send settlement points (wait and fire arms —
/// the two user-originated dispatch paths) BEFORE the chain gate.
pub(super) fn reset_goal_chain_guards(states: &GoalChainStateMap, session_id: &str) {
    let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = states.get_mut(session_id) {
        state.idle_streak = 0;
        state.continuations = 0;
    }
}

/// Inject one goal continuation turn — the cron `inject_cron_prompt`
/// flow with the goal continuation origin/prompt, and the settle block
/// the cron settle site established. The caller (the chain loop) holds
/// the flow's try-lock for the whole call.
#[allow(clippy::too_many_arguments)]
async fn inject_goal_continuation_turn(
    registry: &Arc<WorkspaceRegistry>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    llm_factory: &Option<Arc<dyn LlmFactory>>,
    in_flight_turns: &InFlightTurnsMap,
    server_config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    index: Option<Arc<SessionIndex>>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    session: &Arc<SessionScope>,
    agent_id: &str,
    session_id: &str,
) -> Option<ChainTurnOutcome> {
    // The try-lock gate (the TOCTOU dissolve): a user turn that already
    // owns the flow means the chain yields — NOTHING is written (no wire
    // bracket, no event). The user turn's own settlement re-enters the
    // gate later. The probe is a synchronous moment on the flow's mutex
    // — the same sole-arbiter stance the cron driver's busy probe and
    // the launch gate above apply.
    {
        let flows = turn_flows.lock().unwrap_or_else(|e| e.into_inner());
        let flow_key = format!("{}:{}", session_id, agent_id);
        let busy = flows
            .get(&flow_key)
            .is_some_and(|flow| flow.try_lock().is_err());
        if busy {
            return None;
        }
    }
    let idle_outcome = |reason: String| ChainTurnOutcome {
        error_text: Some(reason),
        steps: 0,
        settled: false,
    };
    // The injection is best-effort: a missing agent or provider logs a
    // warning and the chain parks the turn as failed (the settlement
    // then pauses the goal — a broken provider must not spin the chain).
    let Some(agent) = session.get_agent(&AgentId::new(agent_id)).await else {
        tracing::warn!(
            session_id,
            agent_id,
            "goal continuation dropped: agent vanished"
        );
        return Some(idle_outcome("goal continuation agent vanished".to_string()));
    };
    let Some(llm_factory) = llm_factory else {
        tracing::warn!(
            session_id,
            "goal continuation dropped: no LLM provider configured"
        );
        return Some(idle_outcome(
            "goal continuation has no LLM provider".to_string(),
        ));
    };
    let (llm, model_alias) = {
        let config_arc = agent.config();
        let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
        let model_alias = config_guard.model_id.clone();
        match llm_factory.create_llm(&config_guard) {
            Ok(llm) => (llm, model_alias),
            Err(reason) => {
                tracing::warn!(session_id, %reason, "goal continuation dropped: LLM creation failed");
                return Some(idle_outcome(format!(
                    "goal continuation LLM creation failed: {reason}"
                )));
            }
        }
    };

    // The wire-layer origin object (the engine call below carries the
    // typed enum `PromptOrigin::SystemTrigger` — the two-track split the
    // cron driver established: the JSON never reaches the engine).
    let origin = json!({
        "kind": "system_trigger",
        "name": "goal_continuation",
    });
    let prompt_wire = json!([{ "type": "text", "text": GOAL_CONTINUATION_PROMPT }]);
    // Persist the turn's opening (the `turn.prompt` bracket and the
    // durable user message) with the goal-continuation origin — written
    // only now that the flow lock is held (the TOCTOU dissolve).
    session
        .record_op(WireOp::TurnPrompt {
            input: prompt_wire.clone(),
            origin: origin.clone(),
        })
        .await;
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": prompt_wire,
                "origin": origin,
            }),
        })
        .await;

    // Announce the turn bracket on the session bus (the same
    // `turn-<uuid>` convention the message-send dispatch uses).
    let Ok(artifacts) = resolve_artifacts_via(registry, session_id) else {
        return Some(idle_outcome(
            "goal continuation session vanished".to_string(),
        ));
    };
    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    artifacts.event_bus.publish(SessionEvent::TurnStarted {
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.clone(),
    });

    // Register the turn for cancellation (the `message.abort` path) —
    // the WAIT contract the cron injection applies.
    let cancel_token = CancelToken::new();
    let in_flight_key = format!("{session_id}:{agent_id}:{turn_id}");
    in_flight_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            in_flight_key.clone(),
            InFlightTurn::Wait {
                cancel: cancel_token.clone(),
            },
        );
    let _turn_registration = WaitTurnGuard {
        turns: Arc::clone(in_flight_turns),
        key: in_flight_key,
    };

    // Get-or-create the agent's turn flow — the chain task already
    // holds the lock (the loop's try-lock), so the get here cannot
    // race a user turn; the side-map registrations mirror the dispatch
    // path's `get_or_create_turn_flow`.
    let flow_handle = {
        let key = format!("{}:{}", session_id, agent_id);
        let mut flows = turn_flows.lock().unwrap_or_else(|e| e.into_inner());
        if !flows.contains_key(&key) {
            let model_id = agent
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id
                .clone();
            let mut turn_flow = mirri_core::TurnFlow::new(super::turn_flow_config_for_shared(
                server_config,
                &model_id,
            ));
            turn_flow.set_event_bus(Arc::clone(&artifacts.event_bus));
            turn_flow.set_turn_id_source(artifacts.turn_id_counter());
            let tracker = turn_flow.activity_tracker();
            activity_trackers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), tracker);
            let steer_buffer = turn_flow.steer_buffer_handle();
            steer_buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), steer_buffer);
            let compaction_handle = turn_flow.compaction_handle();
            compaction_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), compaction_handle);
            let usage_model_key = turn_flow.usage_model_key_handle();
            usage_model_key_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), usage_model_key);
            turn_flow.set_usage_sink(agent.usage_service());
            turn_flow.set_usage_model_key(agent.model_id());
            // Accounting-only, like the message.send / cron turn paths
            // (F11): the flow's goal usage accounting counts this turn
            // against the budget; the cross-turn continuation is this
            // chain's own job.
            turn_flow.set_goal_usage_accounting(agent.goal());
            flows.insert(key.clone(), Arc::new(std::sync::Mutex::new(turn_flow)));
        }
        Arc::clone(&flows[&key])
    };

    // The structured agent-status publisher rides along (CR4 parity).
    super::wire_agent_status_publisher(&artifacts.event_bus, &agent, &flow_handle);

    let external_hook_env = {
        let mut env = HashMap::new();
        env.insert("MIRRI_SESSION_ID".to_string(), session_id.to_string());
        env.insert("MIRRI_AGENT_ID".to_string(), agent_id.to_string());
        env
    };
    let interaction_kernel = Some(Arc::new(
        InteractionKernel::new(
            Arc::clone(&artifacts.approval_gate),
            Arc::clone(&artifacts.question_gate),
        )
        .with_grant_sink(session_grant_sink(session)),
    ));
    let memory = agent.context_memory();
    let agent_tools = agent.tool_manager();
    let permission_manager = agent.permission_manager();
    let reverse_rpc_callback = artifacts
        .reverse_rpc_callback
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    super::mirror_reverse_rpc_callback_into_agent_config(&agent, &reverse_rpc_callback);

    let turn_end = run_engine_turn(
        flow_handle,
        agent.hook_engine(),
        external_hook_env,
        Arc::clone(flag_registry),
        agent_tools,
        memory,
        std::sync::Arc::new(mirri_core::FixedLlmResolver::with_alias(llm, model_alias)),
        vec![ContentPart::Text {
            text: GOAL_CONTINUATION_PROMPT.to_string(),
        }],
        PromptOrigin::SystemTrigger,
        // Continuation turns never carry an admission stamp (ADR-006).
        None,
        Some(super::event_relay::live_chunk_callback(
            &artifacts.event_bus,
            agent_id,
            session_id,
            Some(turn_id.clone()),
        )),
        turn_id.clone(),
        Some(Arc::clone(session) as Arc<dyn mirri_core::WireOpRecorder>),
        permission_manager,
        reverse_rpc_callback,
        session.tasks(),
        interaction_kernel,
        cancel_token.clone(),
        turn_stall_timeout,
        turn_backstop_timeout,
        // No mid-turn guard: the continuation turn's consistency posture
        // is out of invariant 2's scope (the interactive turn's journal
        // is).
        None,
        (session_id.to_string(), agent_id.to_string()),
    )
    .await;

    // The settle block the cron settle site established: coerce →
    // TurnEnded wire → TurnCompleted/TurnFailed → note_turn_settled →
    // the goal settlement.
    match turn_end {
        Ok(turn_end) => {
            let turn_end = coerce_aborted_if_cancelled(turn_end, &cancel_token);
            let steps = turn_end.steps;
            session
                .record_op(WireOp::TurnEnded {
                    turn_id: turn_end.turn_id,
                    reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                    error: None,
                    duration_ms: Some(turn_end.duration_ms),
                })
                .await;
            artifacts.event_bus.publish(SessionEvent::TurnCompleted {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                turn_id,
                stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                wire_turn_id: turn_end.turn_id,
                duration_ms: Some(turn_end.duration_ms),
            });
            note_turn_settled(index, registry, session).await;
            let settled =
                settle_goal_after_turn(registry, session, turn_outcome(&turn_end.stop_reason))
                    .await;
            Some(ChainTurnOutcome {
                error_text: None,
                steps,
                settled: settled != GoalSettlement::Unchanged,
            })
        }
        Err(error) => {
            let error_text = match &error {
                EngineTurnError::Engine(core_error) => core_error.to_string(),
                other => other.message(),
            };
            artifacts.event_bus.publish(SessionEvent::TurnFailed {
                agent_id: agent_id.to_string(),
                turn_id,
                error: error.message(),
                error_code: error.domain_code().to_string(),
                wire_turn_id: 0,
            });
            note_turn_settled(index, registry, session).await;
            let settled =
                settle_goal_after_turn(registry, session, TurnOutcome::Failed(&error_text)).await;
            Some(ChainTurnOutcome {
                error_text: Some(error_text),
                steps: 0,
                settled: settled != GoalSettlement::Unchanged,
            })
        }
    }
}

/// Update the chain bookkeeping from one settled continuation turn.
/// `false` means the loop must exit (goal terminal or budget gone).
fn note_chain_outcome(
    states: &GoalChainStateMap,
    session: &SessionScope,
    session_id: &str,
    outcome: ChainTurnOutcome,
) -> bool {
    let goal_manager = session.goal();
    // Count the continuation and judge idleness: a turn is idle when it
    // ran no tool round (a bare-text answer is exactly one engine step —
    // every tool call forces a second) and settled no goal state. This is
    // the `steps` signal the plan's fallback named; verified against the
    // wire log shape (a tool turn records step.begin + tool.call +
    // step.end + a second step.begin/step.end pair).
    let idle = outcome.steps <= 1 && !outcome.settled && outcome.error_text.is_none();
    let budget_exhausted = goal_manager.is_budget_exhausted();
    let goal_active = goal_manager.is_goal_active();
    let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
    let state = states.entry(session_id.to_string()).or_default();
    state.continuations += 1;
    if idle {
        state.idle_streak += 1;
    } else {
        state.idle_streak = 0;
    }
    // Terminal conditions: the goal left Active (settled by this turn or
    // parked by the user mid-flight) or the budget ran out.
    goal_active && !budget_exhausted
}

/// Settle the session goal after one turn settles with `outcome` — the
/// record-before-mutate three-step the `message.abort` goal-pause block
/// established:
/// 1. the `goal.update` wire op lands first,
/// 2. then the in-memory status changes,
/// 3. then the `GoalUpdated` event reaches the session's bus.
///
/// Free function — deliberately NOT a method on `EmbeddedServer`: fire/steer
/// spawn blocks hold no `&EmbeddedServer`, and no spawned task may capture
/// `Arc<EmbeddedServer>`. The registry the spawn blocks already capture for
/// `note_turn_settled` feeds the artifact resolution.
pub(super) async fn settle_goal_after_turn(
    registry: &WorkspaceRegistry,
    session: &SessionScope,
    outcome: TurnOutcome<'_>,
) -> GoalSettlement {
    let goal_manager = session.goal();
    // Active-only guard: a settlement must never resurrect or double-settle
    // a goal the user (or a prior settlement / dispatch_abort) already
    // parked. This is also the dedupe against cancel_in_flight_turns' pause
    // (whose record->mutate->publish completes before the cancel token
    // fires, so the turn's Aborted settlement always sees the new status).
    let Some(goal) = goal_manager.get() else {
        return GoalSettlement::Unchanged;
    };
    if goal.status != GoalStatus::Active {
        return GoalSettlement::Unchanged;
    }
    let (status, reason) = outcome_goal_target(&outcome);
    if status == GoalStatus::Active {
        return GoalSettlement::Unchanged;
    }
    write_goal_lifecycle(registry, session, status, reason.clone(), "runtime").await;
    GoalSettlement::Settled(status, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given each abnormal turn outcome, when the settlement mapping runs,
    /// then each lands on its TS-parity goal status and reason.
    #[test]
    fn should_map_each_abnormal_outcome_to_its_goal_status_and_reason() {
        let cases: Vec<(TurnOutcome, GoalStatus, &str)> = vec![
            (
                TurnOutcome::Stalled,
                GoalStatus::Paused,
                "Paused after stalled turn",
            ),
            (
                TurnOutcome::Aborted,
                GoalStatus::Paused,
                "Paused after interruption",
            ),
            (
                TurnOutcome::Failed("boom"),
                GoalStatus::Paused,
                "Paused after turn failure: boom",
            ),
            (
                TurnOutcome::Failed(""),
                GoalStatus::Paused,
                "Paused after turn failure",
            ),
            (
                TurnOutcome::BlockedByHook,
                GoalStatus::Blocked,
                "Blocked by UserPromptSubmit hook",
            ),
        ];
        for (outcome, status, reason) in cases {
            let (s, r) = outcome_goal_target(&outcome);
            assert_eq!(s, status, "outcome {outcome:?}");
            assert_eq!(r, reason);
        }
        // Completed leaves the goal alone — represented by Active + empty reason.
        assert_eq!(
            outcome_goal_target(&TurnOutcome::Completed),
            (GoalStatus::Active, String::new())
        );
    }

    /// Given every TurnStopReason variant, when folded to a TurnOutcome,
    /// then the mapping is total and matches the wire semantics
    /// (EndTurn/MaxTokens/Filtered/Paused/Unknown -> Completed, etc.).
    #[test]
    fn should_fold_every_stop_reason_into_a_turn_outcome() {
        use TurnStopReason::*;
        for (reason, completed) in [
            (EndTurn, true),
            (MaxTokens, true),
            (Filtered, true),
            (Paused, true),
            (Unknown, true),
            (Aborted, false),
            (Stalled, false),
            (BlockedByHook, false),
        ] {
            assert_eq!(
                matches!(turn_outcome(&reason), TurnOutcome::Completed),
                completed,
                "reason {reason:?}"
            );
        }
    }
}
