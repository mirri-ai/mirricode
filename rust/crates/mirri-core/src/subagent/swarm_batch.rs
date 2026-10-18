//! Swarm batch scheduler — the TS `agentRunBatch.ts` port (B3).
//!
//! Drives one `AgentSwarm` batch to completion:
//!
//! - **Ramp-up (D-42)**: the first [`INITIAL_LAUNCH_LIMIT`] members launch
//!   immediately, then one per [`INITIAL_LAUNCH_INTERVAL_MS`] — launch
//!   counting, independent of free concurrency slots
//!   (`scheduleNormalLaunch`). `host.spawn` is only called at actual launch
//!   time, so `subagent.spawned` events stream progressively (D-54).
//! - **Per-attempt timeout (D-45, B1)**: each attempt is wrapped in a lazy
//!   deadline; on expiry the member's own cancel token fires before the
//!   runner is awaited.
//! - **Cancellation (D-44)**: launched members settle `aborted`/`started`
//!   (their tokens are children of the batch token), unlaunched members
//!   `aborted`/`not_started` with zero spawns and zero events (D-53).
//! - **Rate-limit requeue (D-43)**: a member failing with a transient
//!   provider rate limit is parked (`subagent.suspended`), then re-launched
//!   on the SAME agent identity via a retry-shaped resume (no new prompt,
//!   no new `subagent.spawned`), with exponential backoff and a shrinking /
//!   recovering capacity state machine.
//!
//! Results come back in task order; a single member's spawn/resume failure
//! never aborts the whole batch (TS `failedAttemptOutcome`).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::cancel::CancelToken;
use crate::subagent::{
    ResumeRequest, SpawnRequest, SubagentCompletion, SubagentError, SubagentHandle, SubagentHost,
    SuspendCause, SuspensionNotice,
};
use crate::task::{TaskKind, TaskManager};

// ---------------------------------------------------------------------------
// Constants — TS agentRunBatch.ts line-for-line
// ---------------------------------------------------------------------------

/// The number of members the batch launches immediately before the
/// interval pacing kicks in.
pub const INITIAL_LAUNCH_LIMIT: usize = 5;
/// One additional launch per this interval once the initial burst is out.
pub const INITIAL_LAUNCH_INTERVAL_MS: u64 = 700;
/// The first backoff for a rate-limited member's retry.
pub const RATE_LIMIT_RETRY_BASE_MS: u64 = 3_000;
/// The backoff growth factor per requeue of the same member.
pub const RATE_LIMIT_RETRY_FACTOR: u32 = 2;
/// The keepalive cadence for the batch's stall-watchdog feed (ADR-004
/// batch a gap): while foreground members work silently, no engine live
/// event fires, so the parent turn's stall clock starves. Every tick the
/// batch samples each in-flight member's activity tracker and forwards a
/// touch-only update per member that actually advanced — the same
/// activity-as-liveness shape `agent_tool`'s
/// `drive_with_activity_forwarding` uses. 75s is a quarter of the
/// production heartbeat window's scale and well under the stall window
/// (the ask_user keepalive rides the same constant scale).
pub const SWARM_KEEPALIVE_INTERVAL_MS: u64 = 75_000;
/// While in rate-limit mode, the capacity shrinks by one per this interval.
pub const RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS: u64 = 2_000;
/// While in rate-limit mode, the capacity recovers by one per this interval
/// (measured from the last rate limit or the last recovery).
pub const RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS: u64 = 180_000;
/// The display string of [`SuspendCause::RateLimit`](crate::subagent::SuspendCause::RateLimit) — the
/// pre-ADR-011 free-form `subagent.suspended` reason a parked member used
/// to carry (ADR-011 migrates the const into the RateLimit variant's
/// Display; the text itself stays for human-readable logs).
pub const RATE_LIMIT_SUSPENDED_REASON: &str = "Provider rate limit; subagent requeued for retry.";
/// The result error for a launched member when the batch is cancelled
/// (TS agentRunBatch.ts:531-559, verbatim).
pub const CANCELLED_STARTED_MESSAGE: &str =
    "The user manually interrupted this subagent batch before this subagent finished.";
/// The result error for an unlaunched member when the batch is cancelled
/// (TS agentRunBatch.ts:531-559, verbatim).
pub const CANCELLED_NOT_STARTED_MESSAGE: &str =
    "The user manually interrupted this subagent batch before this subagent was started.";

// ---------------------------------------------------------------------------
// Task / result types
// ---------------------------------------------------------------------------

/// What the scheduler must do for one member.
#[derive(Debug, Clone)]
pub enum SwarmTaskKind {
    /// A fresh spawn of `profile_name`.
    Spawn {
        profile_name: String,
        model: Option<String>,
    },
    /// A resume of an existing agent with the member's own instruction.
    Resume { agent_id: String },
}

/// One member of a swarm batch.
#[derive(Debug, Clone)]
pub struct SwarmTask {
    pub kind: SwarmTaskKind,
    /// The member's prompt (template-expanded for spawns, the per-member
    /// instruction for resumes; empty for rate-limit retries).
    pub prompt: String,
    /// Per-member description (D-52: `"{swarm} #{n} ({profile})"`).
    pub description: String,
    /// The item label riding the result XML's `item` attribute.
    pub item: Option<String>,
    /// The member's 0-based position in the batch.
    pub swarm_index: u32,
}

/// The terminal outcome of a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwarmOutcome {
    Completed,
    Failed,
    Aborted,
}

/// Whether a member's attempt ever started (D-44 tri-state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwarmStartState {
    Started,
    NotStarted,
}

/// One member's final result, in task order.
#[derive(Debug, Clone)]
pub struct SwarmRunResult {
    pub task: SwarmTask,
    pub agent_id: Option<String>,
    pub outcome: SwarmOutcome,
    pub state: SwarmStartState,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Tunables for one batch run.
#[derive(Default)]
pub struct SwarmBatchOptions {
    /// The env override's concurrency ceiling (`None` = unbounded in
    /// normal mode).
    pub max_concurrency: Option<usize>,
    /// The per-attempt timeout (D-45; `None` = unbounded, lazy deadline
    /// preserved from B1).
    pub attempt_timeout: Option<Duration>,
    /// The requeue backoff base (test seam to shorten real waits; defaults
    /// to [`RATE_LIMIT_RETRY_BASE_MS`]).
    pub retry_base_ms: Option<u64>,
    /// Registers every launched member as a tracked [`TaskManager`] task so
    /// `task.list` can observe it and `task.cancel` / `task.stop` can stop
    /// it individually (US5.1 — the parity the single `Agent` delegation's
    /// foreground path already has). `None` keeps the batch untracked (the
    /// embedded/test assemblies and pre-wiring behavior).
    pub registry: Option<SwarmTaskRegistry>,
    /// The stall-watchdog feed for a FOREGROUND batch: the tool layer's
    /// live-update channel (`ctx.progress`). Every keepalive tick that
    /// observes an in-flight member's activity clock advance emits a
    /// touch-only `ToolUpdate` through it, resetting the parent turn's
    /// stall deadline — a silently working swarm is never mistaken for a
    /// stalled engine. A default (no-sink) handle is the background /
    /// untracked shape: the ticks become no-ops.
    pub progress: crate::tool::ToolProgressHandle,
}

/// The TaskManager face the batch registers its members under.
pub struct SwarmTaskRegistry {
    /// The session-scoped task manager.
    pub tasks: Arc<TaskManager>,
    /// The owning session's id (task rows carry it for `task.listWorkspace`).
    pub session_id: String,
}

/// Parse the max-concurrency env override — TS `resolveSwarmMaxConcurrency`
/// (agentRunBatch.ts:639-651). `None`/blank → `None` (no cap); a positive
/// integer → the cap; anything else → `Err` (the CALLER fails soft:
/// warn + ignore, unlike TS which would throw during session assembly).
pub fn parse_swarm_max_concurrency(raw: Option<&str>) -> Result<Option<usize>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!(
            "invalid MIRRI_AGENT_SWARM_MAX_CONCURRENCY '{trimmed}': must be a positive integer"
        )),
    }
}

// ---------------------------------------------------------------------------
// Scheduler internals
// ---------------------------------------------------------------------------

/// The per-member scheduler state riding the pending queue.
struct TaskState {
    /// The member's position in the original task list (results slot).
    index: usize,
    task: SwarmTask,
    /// The member's agent identity once an attempt has been launched.
    agent_id: Option<String>,
    /// The TaskManager task id the member is tracked under once registered
    /// ([`SwarmBatchOptions::registry`]); the id persists across
    /// rate-limit requeues so the member stays one observable task.
    tracked_task_id: Option<String>,
    /// How many times this member has been requeued for a rate limit.
    retry_count: u32,
    /// The last keepalive sample of the member's activity clock (the
    /// handle's `ActivityTracker` at launch): the next keepalive tick
    /// forwards a liveness signal only when the reading moved — a parked
    /// or dead member's clock is frozen, so it stays watchdog territory.
    last_seen_activity_ms: Option<u64>,
    /// The in-flight member's engine activity source (cloned from the
    /// handle at launch, before the completion future is pinned away).
    activity: Option<std::sync::Arc<crate::turn_flow::ActivityTracker>>,
    /// When a requeued member's next attempt may launch (`None` = normal
    /// launch, gated by the ramp instead).
    retry_ready_at: Option<Instant>,
}

/// The rate-limit mode state machine (TS agentRunBatch.ts:366-489). Once a
/// batch enters rate-limit mode it never exits (TS parity).
///
/// - `capacity` shrinks **event-driven** — one per requeue, gated by the
///   2-second shrink interval, floored at 1 (TS `shrinkRateLimitCapacity`;
///   the entry force-shrink is subsumed by the seed approximation, SDR-B3-01)
///   — and recovers one per 3-minute tick measured from the last rate limit
///   or recovery.
/// - `launch_gate` is the **global next-launch pacing** (TS
///   `nextRateLimitLaunchAt`): while active, every launch — normal or
///   retry — waits for it, and every launch/requeue re-arms/pushes it by
///   the base interval. This serializes post-429 relaunches one base
///   interval apart (the thundering-herd guard: simultaneous 429s must not
///   relaunch simultaneously). TS doubles the interval only for
///   *not-ready* attempts — a stage Rust cannot express (every Rust requeue
///   is a completion-stage failure, i.e. TS-ready), so the doubling branch
///   is deliberately absent and the pacing stays at the base everywhere
///   (never faster than TS). The recovery tick clears the gate entirely.
struct RateLimitMode {
    capacity: usize,
    last_shrink: Instant,
    last_rate_limit_or_recovery: Instant,
    launch_gate: Option<Instant>,
}

impl RateLimitMode {
    /// Enter the mode: seed the capacity (the onReady approximation,
    /// SDR-B3-01 — completed + in-flight members, floored at 1) and arm
    /// the launch gate at the base interval.
    fn enter(now: Instant, seed: usize, base: Duration) -> Self {
        Self {
            capacity: seed.max(1),
            last_shrink: now,
            last_rate_limit_or_recovery: now,
            launch_gate: Some(now + base),
        }
    }
}

/// What one settled attempt reports back to the main loop.
struct AttemptDone {
    index: usize,
    agent_id: Option<String>,
    outcome: AttemptOutcome,
}

enum AttemptOutcome {
    Completed(String),
    Failed(String),
    RateLimited(String),
    Cancelled,
}

/// Drive one swarm batch to completion (or cancellation).
///
/// Spawns are paced: ≤ [`INITIAL_LAUNCH_LIMIT`] immediately, then one per
/// [`INITIAL_LAUNCH_INTERVAL_MS`] (launch counting, independent of free
/// slots). Results come back in task order. Cancellation (`batch_cancel`)
/// settles every member: launched ones cancelled-then-terminal, unlaunched
/// ones aborted without any spawn or event.
pub async fn run_swarm_batch(
    host: Arc<dyn SubagentHost>,
    tasks: Vec<SwarmTask>,
    parent_tool_call_id: Option<String>,
    batch_cancel: CancelToken,
    options: SwarmBatchOptions,
) -> Vec<SwarmRunResult> {
    let total = tasks.len();
    let mut pending: VecDeque<TaskState> = tasks
        .into_iter()
        .enumerate()
        .map(|(index, task)| TaskState {
            index,
            task,
            agent_id: None,
            tracked_task_id: None,
            retry_count: 0,
            last_seen_activity_ms: None,
            activity: None,
            retry_ready_at: None,
        })
        .collect();
    let mut results: Vec<Option<SwarmRunResult>> = vec![None; total];
    let mut active: FuturesUnordered<AttemptFuture> = FuturesUnordered::new();
    let mut in_flight: HashMap<usize, TaskState> = HashMap::new();
    // Members parked at the approval gate (ADR-011 c/d): their live
    // attempts wait here — unpolled (the budget clock is stopped) and out
    // of `active` (the concurrency slot is freed) — until the gate
    // resolves or the member is cancelled.
    let mut parked_waits: FuturesUnordered<ParkedWaitFuture> = FuturesUnordered::new();
    // Resumed members whose gate decision arrived while no slot was free:
    // they re-enter the slot competition ahead of fresh launches.
    let mut ready_queue: VecDeque<ParkedMember> = VecDeque::new();

    // Ramp pacing state (the normal-launch gate keeps the fixed base
    // interval; rate-limit pacing lives in `RateLimitMode`).
    let mut launches = 0usize;
    let mut last_normal_launch: Option<Instant> = None;

    // Rate-limit mode state (never exits within one batch — TS parity).
    let mut rate_limit: Option<RateLimitMode> = None;
    let mut completed_count = 0usize;
    let mut cancelled = false;

    let retry_base =
        Duration::from_millis(options.retry_base_ms.unwrap_or(RATE_LIMIT_RETRY_BASE_MS));

    // The keepalive cadence for the stall-watchdog feed (see the select
    // arm below). A fresh interval's first tick fires immediately —
    // harmless: the launch-time sample was just taken, so nothing has
    // advanced yet and the tick is a no-op; Delay only keeps a later burst
    // from firing back-to-back after a scheduler hiccup.
    let mut keepalive = tokio::time::interval(Duration::from_millis(SWARM_KEEPALIVE_INTERVAL_MS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if pending.is_empty()
            && active.is_empty()
            && ready_queue.is_empty()
            && parked_waits.is_empty()
        {
            break;
        }
        // Under cancellation the resumed-but-unlaunched members settle in
        // the final drain; the parked waits resolve through their member
        // tokens and settle inline.
        if cancelled && active.is_empty() && parked_waits.is_empty() {
            break;
        }

        // --- Launch loop: everything eligible fires now. ---
        loop {
            if cancelled {
                break;
            }
            let now = Instant::now();
            // Recovery maintenance (lazy, at launch-check time): one +1 per
            // 3-minute tick measured from the last rate limit or recovery —
            // and the recovery CLEARS the launch gate (TS
            // `recoverRateLimitCapacity`).
            if let Some(rl) = rate_limit.as_mut()
                && now.duration_since(rl.last_rate_limit_or_recovery)
                    >= Duration::from_millis(RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS)
            {
                rl.capacity += 1;
                rl.last_rate_limit_or_recovery = now;
                rl.launch_gate = None;
            }
            let cap = effective_capacity(
                options.max_concurrency,
                rate_limit.as_ref().map(|rl| rl.capacity),
            );
            if cap.is_some_and(|c| active.len() >= c) {
                break;
            }
            // Resumed members re-enter the competition FIRST — they were
            // already mid-flight; a fresh launch does not get to steal
            // their slot. The continuation re-arms with the frozen
            // remainder (its clock stopped while parked).
            if let Some(member) = ready_queue.pop_front() {
                active.push(wrap_attempt_parked(member));
                continue;
            }
            // The global rate-limit launch gate applies to every launch
            // while the mode is active (normal and retry alike).
            let rl_gate_open = rate_limit
                .as_ref()
                .is_none_or(|rl| rl.launch_gate.is_none_or(|g| now >= g));
            let ramp_open = launches < INITIAL_LAUNCH_LIMIT
                || last_normal_launch.is_none_or(|last| {
                    now >= last + Duration::from_millis(INITIAL_LAUNCH_INTERVAL_MS)
                });
            let gate_open = rl_gate_open && ramp_open;
            let pick = pending.iter().position(|state| match state.retry_ready_at {
                Some(ready) => ready <= now && rl_gate_open,
                None => gate_open,
            });
            let Some(pick) = pick else { break };
            let mut state = pending.remove(pick).expect("position is in range");

            // D-54: the host call happens at actual launch time — the
            // `subagent.spawned` events stream progressively with the ramp.
            // A requeued member re-enters through the RETRY-shaped resume on
            // the SAME identity (no new prompt, no new spawned — TS
            // retryTurn).
            let member_cancel = batch_cancel.child();
            let is_retry = state.retry_ready_at.is_some();
            let attempt = if is_retry {
                host.resume(ResumeRequest {
                    agent_id: state
                        .agent_id
                        .clone()
                        .expect("a requeued member always carries its identity"),
                    prompt: String::new(),
                    cancel: member_cancel.clone(),
                    retry: true,
                    suppress_rate_limit_failure_event: true,
                })
            } else {
                match &state.task.kind {
                    SwarmTaskKind::Spawn {
                        profile_name,
                        model,
                    } => host.spawn(SpawnRequest {
                        profile_name: profile_name.clone(),
                        prompt: state.task.prompt.clone(),
                        description: state.task.description.clone(),
                        model: model.clone(),
                        cancel: member_cancel.clone(),
                        run_in_background: false,
                        parent_tool_call_id: parent_tool_call_id.clone(),
                        swarm_index: Some(state.task.swarm_index),
                        swarm_item: state.task.item.clone(),
                        suppress_rate_limit_failure_event: true,
                    }),
                    SwarmTaskKind::Resume { agent_id } => host.resume(ResumeRequest {
                        agent_id: agent_id.clone(),
                        prompt: state.task.prompt.clone(),
                        cancel: member_cancel.clone(),
                        retry: false,
                        suppress_rate_limit_failure_event: true,
                    }),
                }
            };
            match attempt {
                Err(e) => {
                    // The member failed before any attempt ran — TS
                    // failedAttemptOutcome: state by agentId presence, batch
                    // continues.
                    let agent_id = state.agent_id.clone().or(match &state.task.kind {
                        SwarmTaskKind::Resume { agent_id } => Some(agent_id.clone()),
                        SwarmTaskKind::Spawn { .. } => None,
                    });
                    results[state.index] = Some(SwarmRunResult {
                        task: state.task,
                        agent_id: agent_id.clone(),
                        outcome: SwarmOutcome::Failed,
                        state: if agent_id.is_some() {
                            SwarmStartState::Started
                        } else {
                            SwarmStartState::NotStarted
                        },
                        result: None,
                        error: Some(e.to_string()),
                    });
                }
                Ok(handle) => {
                    state.agent_id = Some(handle.agent_id.clone());
                    // Track the member under the TaskManager so task.list
                    // observes it and task.cancel / task.stop can address it
                    // individually (US5.1). First launch registers; a
                    // rate-limit retry re-arms the signal so a stop during
                    // the retried attempt still cancels the live work.
                    if let Some(registry) = &options.registry {
                        if !is_retry {
                            let subagent_type = match &state.task.kind {
                                SwarmTaskKind::Spawn { profile_name, .. } => {
                                    Some(profile_name.clone())
                                }
                                SwarmTaskKind::Resume { .. } => None,
                            };
                            let task_id = registry.tasks.register(
                                TaskKind::Agent {
                                    agent_id: Some(handle.agent_id.clone()),
                                    subagent_type,
                                    parent_tool_call_id: parent_tool_call_id.clone(),
                                    swarm_index: Some(state.task.swarm_index),
                                    // The resolved model (explicit → profile
                                    // default → caller), not the request
                                    // value — a Spawn member asked to
                                    // inherit resolves here. Resume members
                                    // carry no model fact on their handle.
                                    model: handle.model.clone(),
                                },
                                state.task.description.clone(),
                                registry.session_id.clone(),
                            );
                            state.tracked_task_id = Some(task_id);
                        }
                        if let Some(task_id) = state.tracked_task_id.as_deref() {
                            registry.tasks.attach_signal(task_id, member_cancel.clone());
                        }
                    }
                    // Capture the member's engine activity source (the
                    // handle drops it into the completion future): the
                    // keepalive tick samples it to decide liveness
                    // (activity-as-liveness, the agent_tool foreground
                    // shape).
                    state.activity = handle.activity.clone();
                    state.last_seen_activity_ms =
                        state.activity.as_ref().map(|t| t.last_activity_ms());
                    let attempt_future =
                        wrap_attempt(state.index, handle, member_cancel, options.attempt_timeout);
                    active.push(attempt_future);
                    if !is_retry {
                        launches += 1;
                        last_normal_launch = Some(Instant::now());
                    }
                    // Every launch in rate-limit mode re-arms the global
                    // gate at the base interval (TS nextRateLimitLaunchAt).
                    if let Some(rl) = rate_limit.as_mut() {
                        rl.launch_gate = Some(Instant::now() + retry_base);
                    }
                    in_flight.insert(state.index, state);
                }
            }
        }

        if pending.is_empty()
            && active.is_empty()
            && ready_queue.is_empty()
            && parked_waits.is_empty()
        {
            break;
        }
        if cancelled && active.is_empty() && parked_waits.is_empty() {
            break;
        }

        // --- Next wake: earliest of the ramp gate, the global rate-limit
        // launch gate, a requeue backoff, and (in rate-limit mode) the
        // capacity recovery tick. ---
        let now = Instant::now();
        let mut wake: Option<Instant> = None;
        let consider = |deadline: Instant, wake: &mut Option<Instant>| {
            if deadline > now {
                *wake = Some(wake.map_or(deadline, |w: Instant| w.min(deadline)));
            }
        };
        if let Some(last) = last_normal_launch
            && launches >= INITIAL_LAUNCH_LIMIT
        {
            consider(
                last + Duration::from_millis(INITIAL_LAUNCH_INTERVAL_MS),
                &mut wake,
            );
        }
        if let Some(rl) = &rate_limit {
            if let Some(gate) = rl.launch_gate {
                consider(gate, &mut wake);
            }
            if !pending.is_empty() {
                consider(
                    rl.last_rate_limit_or_recovery
                        + Duration::from_millis(RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS),
                    &mut wake,
                );
            }
        }
        for state in &pending {
            if let Some(ready) = state.retry_ready_at
                && ready > now
            {
                consider(ready, &mut wake);
            }
        }

        tokio::select! {
            done = active.next(), if !active.is_empty() => {
                if let Some(disposition) = done {
                    match disposition {
                        AttemptDisposition::Done(done) => {
                            settle_attempt(
                                done,
                                &mut results,
                                &mut pending,
                                &mut in_flight,
                                &mut rate_limit,
                                &mut completed_count,
                                cancelled,
                                &*host,
                                retry_base,
                                options.registry.as_ref(),
                            );
                        }
                        AttemptDisposition::Parked(member) => {
                            // The member awaits a user decision: out of the
                            // active set (slot freed, clock stopped), into
                            // the parked wait.
                            parked_waits.push(park_member(member));
                        }
                    }
                }
            }
            ready = parked_waits.next(), if !parked_waits.is_empty() => {
                if let Some(ready) = ready {
                    match ready {
                        ParkedReady::Resumed(member) => {
                            // The gate resolved — the member re-competes for
                            // a slot (launch-loop capacity gate) with its
                            // frozen remainder.
                            ready_queue.push_back(member);
                        }
                        ParkedReady::Cancelled(member) => {
                            // Cancelled while parked (the batch's cancel or
                            // a single-member stop): settle aborted at once.
                            settle_parked_aborted(
                                member,
                                &mut results,
                                &mut in_flight,
                                options.registry.as_ref(),
                            );
                        }
                    }
                }
            }
            _ = batch_cancel.cancelled(), if !cancelled => {
                cancelled = true;
            }
            _ = sleep_until_wake(wake), if wake.is_some() => {}
            // ADR-004 batch a gap: the foreground batch's stall-watchdog
            // feed. While members work silently the engine emits no live
            // event, so the parent turn's stall clock starves (the
            // session-f1e24e04 shape, swarm edition). Every tick samples
            // each in-flight member's activity clock; a member that
            // advanced gets its tracked task's heartbeat bumped (the row
            // must not read stale) and a touch-only ToolUpdate forwarded
            // (the volatile signal that resets the parent turn's stall
            // deadline). A member whose clock is frozen (parked at a gate,
            // hung) forwards nothing — that member is genuinely silent
            // and stays watchdog territory, exactly like
            // `drive_with_activity_forwarding`'s per-child rule.
            _ = keepalive.tick() => {
                for state in in_flight.values_mut() {
                    let Some(tracker) = state.activity.as_ref() else {
                        continue;
                    };
                    let now = tracker.last_activity_ms();
                    if state.last_seen_activity_ms == Some(now) {
                        continue;
                    }
                    state.last_seen_activity_ms = Some(now);
                    if let Some(registry) = options.registry.as_ref()
                        && let Some(task_id) = state.tracked_task_id.as_deref()
                    {
                        registry.tasks.heartbeat(task_id);
                    }
                    options.progress.send(crate::tool::ToolUpdate {
                        kind: crate::tool::ToolUpdateKind::Custom,
                        text: None,
                        percent: None,
                        custom_kind: Some("swarm.activity".to_string()),
                        custom_data: Some(serde_json::json!({
                            "agentId": state.agent_id,
                            "lastActivityMs": now,
                        })),
                    });
                }
            }
        }
    }

    // Anything still pending only happens after a cancellation. Requeued
    // members (parked in a rate-limit backoff) already launched — they keep
    // their identity and the started verdict (TS finishWithUserCancellation
    // keys on started || agentId); genuinely unlaunched members are
    // not_started with zero spawns and zero events (D-53). A parked member's
    // tracked task never gets another attempt — settle it stopped so the
    // task list does not leak a running row for work that will never run.
    for state in pending.drain(..) {
        let started = state.agent_id.is_some();
        if started
            && let Some(task_id) = state.tracked_task_id.as_deref()
            && let Some(registry) = &options.registry
        {
            registry.tasks.stop(task_id);
        }
        results[state.index] = Some(SwarmRunResult {
            task: state.task,
            agent_id: state.agent_id,
            outcome: SwarmOutcome::Aborted,
            state: if started {
                SwarmStartState::Started
            } else {
                SwarmStartState::NotStarted
            },
            result: None,
            error: Some(
                if started {
                    CANCELLED_STARTED_MESSAGE
                } else {
                    CANCELLED_NOT_STARTED_MESSAGE
                }
                .to_string(),
            ),
        });
    }

    // Resumed members that never got their slot (the launch loop breaks on
    // cancellation) settle aborted like every other launched member.
    for member in ready_queue.drain(..) {
        settle_parked_aborted(
            member,
            &mut results,
            &mut in_flight,
            options.registry.as_ref(),
        );
    }

    results
        .into_iter()
        .map(|r| r.expect("every task settles with a result"))
        .collect()
}

/// The member's still-running attempt after it parked at the approval gate
/// (ADR-011 c/d): the continuation is unpolled — the attempt clock is
/// stopped and the concurrency slot is freed — until the gate resolves and
/// the member re-competes for a slot.
struct ParkedMember {
    /// The member's task index (the results slot).
    index: usize,
    /// The member's agent identity (mirrored from the in-flight TaskState
    /// so the settle row carries it).
    agent_id: String,
    /// The frozen remainder of the attempt budget at parking (D-45 /
    /// ADR-011 c: the clock resumes from this remainder, NOT a fresh
    /// budget).
    remaining: Option<Duration>,
    /// The member cancel token — the parked wait watches it so a batch
    /// cancellation (or a single-member stop) settles the parked member.
    member_cancel: CancelToken,
    /// The completion poller, kept alive (and unpolled) across the park.
    inner: AttemptInner,
    /// The member's suspension stream (Suspended → parking, Resumed →
    /// re-entry); `None` for a feed that vanished.
    rx: Option<UnboundedReceiver<SuspensionNotice>>,
}

/// What one poll of an active attempt yields.
enum AttemptDisposition {
    /// A terminal settle (completed / failed / rate-limited / cancelled).
    Done(AttemptDone),
    /// The member parked awaiting a user decision: the batch must free its
    /// slot and stop its clock until the gate resolves.
    Parked(ParkedMember),
}

/// What a parked member's wait resolves with.
enum ParkedReady {
    /// The gate resolved — the member re-enters the slot competition with
    /// its frozen remainder.
    Resumed(ParkedMember),
    /// The member was cancelled while parked — the batch settles it
    /// aborted without ever resuming its turn.
    Cancelled(ParkedMember),
}

/// The completion poller of one launched attempt.
type AttemptInner =
    std::pin::Pin<Box<dyn Future<Output = Result<SubagentCompletion, SubagentError>> + Send>>;

/// One active attempt's future (launched or resumed).
type AttemptFuture = std::pin::Pin<Box<dyn Future<Output = AttemptDisposition> + Send>>;

/// One parked member's wait future.
type ParkedWaitFuture = std::pin::Pin<Box<dyn Future<Output = ParkedReady> + Send>>;

/// ADR-011 c: only USER waits (approval / question) stop the member's
/// budget clock — a rate-limit backoff keeps counting (it is part of the
/// member's delivery cycle).
fn is_user_wait(cause: &SuspendCause) -> bool {
    matches!(
        cause,
        SuspendCause::Approval { .. } | SuspendCause::Question { .. }
    )
}

/// Wrap one launched member's completion with the per-attempt timeout
/// (D-45/B1 semantics: on expiry the member's own token fires first, then
/// the runner is awaited so a late settle cannot race the rendering), watch
/// its suspension stream, and — when the member parks awaiting a user
/// decision (ADR-011 c/d) — yield the still-alive attempt as a
/// [`ParkedMember`] with the frozen remaining budget.
fn wrap_attempt(
    index: usize,
    mut handle: SubagentHandle,
    member_cancel: CancelToken,
    attempt_timeout: Option<Duration>,
) -> AttemptFuture {
    let agent_id = handle.agent_id.clone();
    let inner: AttemptInner = Box::pin(handle.completion);
    let rx = handle.suspension_rx.take();
    wrap_attempt_inner(index, agent_id, inner, member_cancel, attempt_timeout, rx)
}

/// Re-arm a parked member's continuation (its resumed attempt) with the
/// frozen remainder.
fn wrap_attempt_parked(member: ParkedMember) -> AttemptFuture {
    wrap_attempt_inner(
        member.index,
        member.agent_id,
        member.inner,
        member.member_cancel,
        member.remaining,
        member.rx,
    )
}

/// A select-friendly optional wait — `None` stays PENDING forever (a
/// guard arm that is never ready). NOT `OptionFuture`: its `None` polls
/// `Ready(None)`, which would fire an arm that must never fire (a paused
/// deadline / an absent suspension stream).
type OptionalWait<'a> =
    std::pin::Pin<Box<dyn Future<Output = Option<SuspensionNotice>> + Send + 'a>>;

/// The optional suspension-stream arm: awaits the member's next gate
/// notice, or stays pending when the member has no feed.
fn optional_suspension(rx: &mut Option<UnboundedReceiver<SuspensionNotice>>) -> OptionalWait<'_> {
    match rx {
        Some(rx) => Box::pin(async move { rx.recv().await }),
        None => Box::pin(std::future::pending()),
    }
}

/// The optional deadline arm: sleeps until the deadline, or stays pending
/// when the attempt has no budget bound.
fn optional_deadline(deadline: Option<Instant>) -> OptionalWait<'static> {
    match deadline {
        Some(deadline) => Box::pin(async move {
            tokio::time::sleep_until(deadline).await;
            None
        }),
        None => Box::pin(std::future::pending()),
    }
}

/// The shared attempt driver.
fn wrap_attempt_inner(
    index: usize,
    agent_id: String,
    inner: AttemptInner,
    member_cancel: CancelToken,
    budget: Option<Duration>,
    rx: Option<UnboundedReceiver<SuspensionNotice>>,
) -> AttemptFuture {
    Box::pin(async move {
        let mut inner = inner;
        let deadline: Option<Instant> = budget.map(|b| Instant::now() + b);
        let mut rx = rx;
        loop {
            let deadline_wait = optional_deadline(deadline);
            let suspension_wait = optional_suspension(&mut rx);
            tokio::select! {
                result = inner.as_mut() => {
                    return AttemptDisposition::Done(AttemptDone {
                        index,
                        agent_id: Some(agent_id),
                        outcome: match result {
                            Ok(completion) => AttemptOutcome::Completed(completion.result),
                            Err(SubagentError::TurnFailed(msg)) => AttemptOutcome::Failed(msg),
                            Err(SubagentError::RateLimited(msg)) => AttemptOutcome::RateLimited(msg),
                            Err(SubagentError::Cancelled) => AttemptOutcome::Cancelled,
                        },
                    });
                }
                _ = deadline_wait => {
                    member_cancel.cancel();
                    let _ = inner.as_mut().await;
                    return AttemptDisposition::Done(AttemptDone {
                        index,
                        agent_id: Some(agent_id),
                        outcome: AttemptOutcome::Failed(format!(
                            "Subagent timed out after {}s.",
                            budget.unwrap_or_default().as_secs()
                        )),
                    });
                }
                notice = suspension_wait => {
                    if let Some(SuspensionNotice::Suspended(cause)) = notice
                        && is_user_wait(&cause)
                    {
                        // Freeze the clock: hand the continuation back with
                        // the remaining budget. The member is no longer
                        // polled — its deadline cannot fire while parked.
                        let remaining =
                            deadline.map(|d| d.saturating_duration_since(Instant::now()));
                        return AttemptDisposition::Parked(ParkedMember {
                            index,
                            agent_id,
                            remaining,
                            member_cancel,
                            inner,
                            rx,
                        });
                    }
                    // Resumed noise, a rate-limit notice (never routed this
                    // way), or a vanished feed — keep running.
                }
            }
        }
    })
}

/// Park one member: the wait future resolves on the gate's decision
/// (Resumed — the member re-enters the slot competition) or on the
/// member's cancellation (batch cancellation / a single-member stop).
/// A vanished feed resumes at once rather than hang.
fn park_member(member: ParkedMember) -> ParkedWaitFuture {
    Box::pin(async move {
        let mut member = member;
        if let Some(mut rx) = member.rx.take() {
            let resumed = tokio::select! {
                _ = rx.recv() => true,
                _ = member.member_cancel.cancelled() => false,
            };
            member.rx = Some(rx);
            if resumed {
                ParkedReady::Resumed(member)
            } else {
                ParkedReady::Cancelled(member)
            }
        } else {
            ParkedReady::Resumed(member)
        }
    })
}

/// Settle a parked member's aborted terminal: its identity, the started
/// verdict, the tracked task stopped, and the in-flight state dropped.
fn settle_parked_aborted(
    member: ParkedMember,
    results: &mut [Option<SwarmRunResult>],
    in_flight: &mut HashMap<usize, TaskState>,
    registry: Option<&SwarmTaskRegistry>,
) {
    let Some(state) = in_flight.remove(&member.index) else {
        return;
    };
    member.member_cancel.cancel();
    if let Some(task_id) = state.tracked_task_id.as_deref()
        && let Some(registry) = registry
    {
        registry.tasks.stop(task_id);
    }
    results[member.index] = Some(SwarmRunResult {
        task: state.task,
        agent_id: Some(member.agent_id),
        outcome: SwarmOutcome::Aborted,
        state: SwarmStartState::Started,
        result: None,
        error: Some(CANCELLED_STARTED_MESSAGE.to_string()),
    });
}

/// Settle one completed attempt: write the terminal result, or requeue the
/// member for a rate-limit retry (D-43). A tracked member's TaskManager
/// task settles with the matching terminal status; a rate-limit requeue
/// keeps it running across the backoff.
#[allow(clippy::too_many_arguments)]
fn settle_attempt(
    done: AttemptDone,
    results: &mut [Option<SwarmRunResult>],
    pending: &mut VecDeque<TaskState>,
    in_flight: &mut HashMap<usize, TaskState>,
    rate_limit: &mut Option<RateLimitMode>,
    completed_count: &mut usize,
    cancelled: bool,
    host: &dyn SubagentHost,
    retry_base: Duration,
    registry: Option<&SwarmTaskRegistry>,
) {
    let Some(mut state) = in_flight.remove(&done.index) else {
        return;
    };
    if let Some(agent_id) = done.agent_id.as_ref() {
        state.agent_id = Some(agent_id.clone());
    }
    let agent_id = state.agent_id.clone();
    let index = state.index;
    match done.outcome {
        AttemptOutcome::Completed(result) => {
            *completed_count += 1;
            if let Some(task_id) = state.tracked_task_id.as_deref()
                && let Some(registry) = registry
            {
                registry.tasks.complete(task_id, 0);
            }
            results[index] = Some(SwarmRunResult {
                task: state.task,
                agent_id,
                outcome: SwarmOutcome::Completed,
                state: SwarmStartState::Started,
                result: Some(result),
                error: None,
            });
        }
        AttemptOutcome::Failed(msg) => {
            if let Some(task_id) = state.tracked_task_id.as_deref()
                && let Some(registry) = registry
            {
                registry.tasks.fail(task_id, msg.clone());
            }
            results[index] = Some(SwarmRunResult {
                task: state.task,
                agent_id,
                outcome: SwarmOutcome::Failed,
                state: SwarmStartState::Started,
                result: None,
                error: Some(msg),
            });
        }
        AttemptOutcome::Cancelled => {
            if let Some(task_id) = state.tracked_task_id.as_deref()
                && let Some(registry) = registry
            {
                registry.tasks.stop(task_id);
            }
            results[index] = Some(SwarmRunResult {
                task: state.task,
                agent_id,
                outcome: SwarmOutcome::Aborted,
                state: SwarmStartState::Started,
                result: None,
                error: Some(CANCELLED_STARTED_MESSAGE.to_string()),
            });
        }
        AttemptOutcome::RateLimited(msg) => {
            // A rate limit racing the batch's cancellation settles aborted —
            // the cancel verdict owns the member's terminal state.
            if cancelled {
                if let Some(task_id) = state.tracked_task_id.as_deref()
                    && let Some(registry) = registry
                {
                    registry.tasks.stop(task_id);
                }
                results[index] = Some(SwarmRunResult {
                    task: state.task,
                    agent_id,
                    outcome: SwarmOutcome::Aborted,
                    state: SwarmStartState::Started,
                    result: None,
                    error: Some(CANCELLED_STARTED_MESSAGE.to_string()),
                });
                return;
            }
            // TS isOnlyUnfinishedTask: with no other member in flight or
            // queued, requeueing would spin forever — the member takes the
            // terminal failed verdict instead (no suspended event).
            let unfinished = pending.len() + in_flight.len();
            if unfinished == 0 {
                if let Some(task_id) = state.tracked_task_id.as_deref()
                    && let Some(registry) = registry
                {
                    registry.tasks.fail(task_id, msg.clone());
                }
                results[index] = Some(SwarmRunResult {
                    task: state.task,
                    agent_id,
                    outcome: SwarmOutcome::Failed,
                    state: SwarmStartState::Started,
                    result: None,
                    error: Some(msg),
                });
                return;
            }
            // Park the member: the structured suspended wire event, then a
            // retry-shaped requeue on the SAME identity after the
            // exponential backoff (ADR-011: the reason is machine-readable
            // `SuspendCause::RateLimit`, not a free-form string).
            host.notify_suspended(
                state.agent_id.as_deref().unwrap_or_default(),
                &SuspendCause::RateLimit,
            );
            state.retry_count += 1;
            let backoff = retry_base * RATE_LIMIT_RETRY_FACTOR.pow(state.retry_count - 1);
            state.retry_ready_at = Some(Instant::now() + backoff);
            // Enter / refresh rate-limit mode. The capacity seed is the
            // onReady approximation (SDR-B3-01): completed + in-flight
            // members, floored at 1. An already-active mode shrinks
            // event-driven — one per requeue gated by the 2s interval, floor
            // 1 (TS shrinkRateLimitCapacity) — pushes the global launch gate
            // out by the base interval (TS: ready requeues extend
            // nextRateLimitLaunchAt by the base — the not-ready doubling
            // branch has no Rust-representable stage), and refreshes the
            // recovery baseline (TS lastRateLimitAt: a sustained storm
            // keeps deferring the 3-minute recovery).
            let now = Instant::now();
            match rate_limit.as_mut() {
                None => {
                    let seed = *completed_count + in_flight.len();
                    *rate_limit = Some(RateLimitMode::enter(now, seed, retry_base));
                }
                Some(rl) => {
                    if now.duration_since(rl.last_shrink)
                        >= Duration::from_millis(RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS)
                        && rl.capacity > 1
                    {
                        rl.capacity -= 1;
                        rl.last_shrink = now;
                    }
                    let next = now + retry_base;
                    rl.launch_gate = Some(rl.launch_gate.map_or(next, |g| g.max(next)));
                    rl.last_rate_limit_or_recovery = now;
                }
            }
            pending.push_front(state);
        }
    }
}

/// The effective concurrency ceiling: the env override and the rate-limit
/// capacity both apply — whichever is tighter.
fn effective_capacity(
    max_concurrency: Option<usize>,
    rate_limit_capacity: Option<usize>,
) -> Option<usize> {
    match (max_concurrency, rate_limit_capacity) {
        (Some(env_cap), Some(rl_cap)) => Some(env_cap.min(rl_cap)),
        (env_cap, rl_cap) => env_cap.or(rl_cap),
    }
}

/// A select-friendly sleep that only sleeps when a wake instant exists.
async fn sleep_until_wake(wake: Option<Instant>) {
    if let Some(wake) = wake {
        tokio::time::sleep_until(wake).await;
    } else {
        std::future::pending::<()>().await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::error::EngineResult;
    use crate::subagent::{
        ResumeRequest, SpawnRequest, SubagentCompletion, SubagentError, SubagentHost, SuspendCause,
        SuspensionNotice,
    };
    use std::sync::Mutex;

    struct RecordingHost {
        spawns: Mutex<Vec<(String, tokio::time::Instant)>>,
        resumes: Mutex<Vec<(String, bool)>>,
        suspended: Mutex<Vec<String>>,
    }

    impl SubagentHost for RecordingHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let id = format!("agent-{}", self.spawns.lock().unwrap().len());
            self.spawns
                .lock()
                .unwrap()
                .push((id.clone(), tokio::time::Instant::now()));
            Ok(SubagentHandle::from_result(
                id,
                request.profile_name,
                request.model,
                "ok".into(),
            ))
        }
        fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
            self.resumes
                .lock()
                .unwrap()
                .push((request.agent_id.clone(), request.retry));
            Ok(SubagentHandle::from_result(
                request.agent_id,
                "coder".into(),
                None,
                "retried".into(),
            ))
        }
        fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
            self.suspended.lock().unwrap().push(agent_id.to_string());
        }
    }

    fn recording_host() -> Arc<RecordingHost> {
        Arc::new(RecordingHost {
            spawns: Mutex::new(Vec::new()),
            resumes: Mutex::new(Vec::new()),
            suspended: Mutex::new(Vec::new()),
        })
    }

    fn spawn_task(i: u32) -> SwarmTask {
        SwarmTask {
            kind: SwarmTaskKind::Spawn {
                profile_name: "coder".into(),
                model: None,
            },
            prompt: format!("p{i}"),
            description: format!("swarm #{i} (coder)"),
            item: Some(format!("item-{i}")),
            swarm_index: i,
        }
    }

    /// Given 8 members that complete immediately, When the batch runs under a
    /// paused clock, Then the first five launch at t≈0 and each subsequent
    /// launch trails one interval behind (TS INITIAL_LAUNCH_LIMIT/INTERVAL).
    #[tokio::test(start_paused = true)]
    async fn should_launch_five_immediately_then_one_per_interval() {
        let host = recording_host();
        let tasks: Vec<_> = (0..8).map(spawn_task).collect();
        let start = tokio::time::Instant::now();
        let results = run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            tasks,
            None,
            CancelToken::new(),
            SwarmBatchOptions::default(),
        )
        .await;
        assert!(results.iter().all(|r| r.outcome == SwarmOutcome::Completed));
        let spawns = host.spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 8);
        for (i, (_, at)) in spawns.iter().enumerate() {
            let expected = if i < INITIAL_LAUNCH_LIMIT {
                0
            } else {
                (i - INITIAL_LAUNCH_LIMIT + 1) as u64
            };
            assert!(
                at.duration_since(start)
                    >= Duration::from_millis(expected * INITIAL_LAUNCH_INTERVAL_MS),
                "launch {i} fired too early (at {:?} after start)",
                at.duration_since(start)
            );
        }
    }

    /// Given members that hang until their cancel token fires and a
    /// max_concurrency of 2, When the batch runs, Then never more than 2
    /// attempts are in flight (the env override's cap).
    #[tokio::test(start_paused = true)]
    async fn should_cap_concurrent_attempts_at_max_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CapHost {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }
        impl SubagentHost for CapHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                let cancel = request.cancel.clone();
                let active = Arc::clone(&self.active);
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.swarm_index.unwrap_or(0)),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        cancel.cancelled().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Err(SubagentError::Cancelled)
                    }),
                ))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
        }
        let host = Arc::new(CapHost {
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        });
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..6).map(spawn_task).collect(),
            None,
            cancel.clone(),
            SwarmBatchOptions {
                max_concurrency: Some(2),
                ..Default::default()
            },
        ));
        tokio::time::advance(Duration::from_secs(10)).await;
        cancel.cancel();
        let results = batch.await.unwrap();
        assert!(
            host.peak.load(Ordering::SeqCst) <= 2,
            "concurrency cap violated"
        );
        assert!(results.iter().all(|r| r.outcome == SwarmOutcome::Aborted));
    }

    #[test]
    fn should_parse_the_max_concurrency_env_value_like_the_ts_baseline() {
        assert_eq!(parse_swarm_max_concurrency(None), Ok(None));
        assert_eq!(parse_swarm_max_concurrency(Some("  ")), Ok(None));
        assert_eq!(parse_swarm_max_concurrency(Some("3")), Ok(Some(3)));
        assert!(parse_swarm_max_concurrency(Some("0")).is_err());
        assert!(parse_swarm_max_concurrency(Some("fast")).is_err());
    }

    // ===== T4: cancellation semantics =====

    /// Given a 7-member swarm where the first five hang until cancelled,
    /// When the batch is cancelled after the initial burst, Then every
    /// member settles aborted — launched ones `started`, the two still in
    /// the ramp queue `not_started` with no agent_id and no spawn at all.
    #[tokio::test(start_paused = true)]
    async fn should_mark_unlaunched_members_aborted_not_started_on_cancellation() {
        struct HangingHost {
            inner: RecordingHost,
        }
        impl SubagentHost for HangingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let cancel = request.cancel.clone();
                let id = format!("agent-{}", self.inner.spawns.lock().unwrap().len());
                self.inner
                    .spawns
                    .lock()
                    .unwrap()
                    .push((id.clone(), tokio::time::Instant::now()));
                Ok(SubagentHandle::new(
                    id,
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        cancel.cancelled().await;
                        Err(SubagentError::Cancelled)
                    }),
                ))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
        }
        let host = Arc::new(HangingHost {
            inner: RecordingHost {
                spawns: Mutex::new(Vec::new()),
                resumes: Mutex::new(Vec::new()),
                suspended: Mutex::new(Vec::new()),
            },
        });
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..7).map(spawn_task).collect(),
            None,
            cancel.clone(),
            SwarmBatchOptions::default(),
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await; // let the initial burst launch
        cancel.cancel();
        let results = batch.await.unwrap();
        // Then only the initial burst ever spawned (D-54: no eager spawns)
        assert_eq!(
            host.inner.spawns.lock().unwrap().len(),
            INITIAL_LAUNCH_LIMIT
        );
        for (i, r) in results.iter().enumerate() {
            assert!(
                r.outcome == SwarmOutcome::Aborted,
                "member {i}: {:?}",
                r.outcome
            );
            if i < INITIAL_LAUNCH_LIMIT {
                assert!(r.state == SwarmStartState::Started);
                assert!(r.agent_id.is_some());
                assert!(
                    r.error
                        .as_deref()
                        .unwrap()
                        .contains("before this subagent finished"),
                    "error: {:?}",
                    r.error
                );
            } else {
                assert!(r.state == SwarmStartState::NotStarted);
                assert!(r.agent_id.is_none());
                assert!(
                    r.error
                        .as_deref()
                        .unwrap()
                        .contains("before this subagent was started"),
                    "error: {:?}",
                    r.error
                );
            }
        }
    }

    // ===== T5: rate-limit requeue =====

    /// Given a member whose first attempt fails with a rate limit and whose
    /// retry succeeds, When the batch runs under a paused clock, Then the
    /// member is requeued on the SAME agent identity via a retry-shaped
    /// resume after the base backoff, a suspended event fires, and the
    /// final outcome is completed.
    #[tokio::test(start_paused = true)]
    async fn should_requeue_a_rate_limited_member_on_the_same_identity_after_backoff() {
        struct RateLimitHost {
            inner: RecordingHost,
            resumed_at: Mutex<Vec<tokio::time::Instant>>,
        }
        impl SubagentHost for RateLimitHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let id = format!("agent-{}", self.inner.spawns.lock().unwrap().len());
                self.inner
                    .spawns
                    .lock()
                    .unwrap()
                    .push((id.clone(), tokio::time::Instant::now()));
                Ok(SubagentHandle::new(
                    id.clone(),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        if request.swarm_index == Some(0) {
                            Err(SubagentError::RateLimited(
                                "HTTP 429: too many requests".into(),
                            ))
                        } else {
                            // A short delay keeps this member observably
                            // in-flight when member 0's rate limit settles,
                            // so the requeue branch (not the only-unfinished
                            // verdict) runs.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok(SubagentCompletion::from_text("ok"))
                        }
                    }),
                ))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                assert!(request.retry, "requeue must use the retry shape");
                self.resumed_at
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now());
                Ok(SubagentHandle::from_result(
                    request.agent_id,
                    "coder".into(),
                    None,
                    "recovered".into(),
                ))
            }
            fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
                self.inner
                    .suspended
                    .lock()
                    .unwrap()
                    .push(agent_id.to_string());
            }
        }
        let host = Arc::new(RateLimitHost {
            inner: RecordingHost {
                spawns: Mutex::new(Vec::new()),
                resumes: Mutex::new(Vec::new()),
                suspended: Mutex::new(Vec::new()),
            },
            resumed_at: Mutex::new(Vec::new()),
        });
        let results = run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions {
                retry_base_ms: Some(3_000),
                ..Default::default()
            },
        )
        .await;
        // Then member 0 completed via the retry, on its original identity
        assert!(results[0].outcome == SwarmOutcome::Completed);
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-0"));
        assert_eq!(results[0].result.as_deref(), Some("recovered"));
        // And the requeue waited the base backoff after the failure
        let resumed_at = *host.resumed_at.lock().unwrap().first().unwrap();
        let failed_at = host.inner.spawns.lock().unwrap()[0].1;
        assert!(
            resumed_at.duration_since(failed_at) >= Duration::from_millis(3_000),
            "backoff not honored"
        );
        // And exactly one suspended notification fired for the member
        assert_eq!(
            *host.inner.suspended.lock().unwrap(),
            vec!["agent-0".to_string()]
        );
    }

    /// Given a single-member (resume-only) swarm whose attempts always fail
    /// with a rate limit, When the batch runs, Then it terminates failed
    /// instead of retrying forever (TS isOnlyUnfinishedTask).
    #[tokio::test(start_paused = true)]
    async fn should_fail_the_last_unfinished_member_instead_of_retrying_forever() {
        struct AlwaysRateLimitedHost {
            resumed: Mutex<usize>,
            suspended: Mutex<Vec<String>>,
        }
        impl SubagentHost for AlwaysRateLimitedHost {
            fn spawn(&self, _request: SpawnRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "no spawns in this test".into(),
                ))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                *self.resumed.lock().unwrap() += 1;
                Ok(SubagentHandle::new(
                    request.agent_id,
                    "coder".into(),
                    None,
                    Box::pin(async move {
                        Err(SubagentError::RateLimited(
                            "HTTP 429: too many requests".into(),
                        ))
                    }),
                ))
            }
            fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
                self.suspended.lock().unwrap().push(agent_id.to_string());
            }
        }
        let host = Arc::new(AlwaysRateLimitedHost {
            resumed: Mutex::new(0),
            suspended: Mutex::new(Vec::new()),
        });
        let results = run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            vec![SwarmTask {
                kind: SwarmTaskKind::Resume {
                    agent_id: "agent-old".into(),
                },
                prompt: "continue the work".into(),
                description: "swarm #1 (resume)".into(),
                item: None,
                swarm_index: 0,
            }],
            None,
            CancelToken::new(),
            SwarmBatchOptions::default(),
        )
        .await;
        assert!(
            results[0].outcome == SwarmOutcome::Failed,
            "{:?}",
            results[0]
        );
        assert!(results[0].state == SwarmStartState::Started);
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-old"));
        assert!(results[0].error.as_deref().unwrap().contains("429"));
        // No suspended notification and exactly one attempt — the terminal
        // verdict, not an infinite requeue loop.
        assert!(host.suspended.lock().unwrap().is_empty());
        assert_eq!(*host.resumed.lock().unwrap(), 1);
    }

    /// Given a member parked in a rate-limit backoff (and siblings in
    /// flight), When the batch is cancelled during the backoff window,
    /// Then the parked member settles aborted with its agent identity and
    /// the started message — it DID launch (TS finishWithUserCancellation
    /// keys on started || agentId; the pre-fix drain mislabeled it
    /// not_started and dropped the id, breaking resume_agent_ids recovery).
    #[tokio::test(start_paused = true)]
    async fn should_abort_a_rate_limited_member_in_backoff_as_started_with_its_identity() {
        struct BackoffHost {
            inner: RecordingHost,
        }
        impl SubagentHost for BackoffHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let id = format!("agent-{}", self.inner.spawns.lock().unwrap().len());
                self.inner
                    .spawns
                    .lock()
                    .unwrap()
                    .push((id.clone(), tokio::time::Instant::now()));
                let cancel = request.cancel.clone();
                Ok(SubagentHandle::new(
                    id,
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        if request.swarm_index == Some(0) {
                            Err(SubagentError::RateLimited(
                                "HTTP 429: too many requests".into(),
                            ))
                        } else {
                            cancel.cancelled().await;
                            Err(SubagentError::Cancelled)
                        }
                    }),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
            fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
                self.inner
                    .suspended
                    .lock()
                    .unwrap()
                    .push(agent_id.to_string());
            }
        }
        let host = Arc::new(BackoffHost {
            inner: RecordingHost {
                spawns: Mutex::new(Vec::new()),
                resumes: Mutex::new(Vec::new()),
                suspended: Mutex::new(Vec::new()),
            },
        });
        let cancel = CancelToken::new();
        // A long backoff keeps the retry parked for the whole test.
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..3).map(spawn_task).collect(),
            None,
            cancel.clone(),
            SwarmBatchOptions {
                retry_base_ms: Some(60_000),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await; // let the burst launch and member 0 park
        cancel.cancel();
        let results = batch.await.unwrap();

        assert_eq!(
            *host.inner.suspended.lock().unwrap(),
            vec!["agent-0".to_string()]
        );
        assert!(host.inner.resumes.lock().unwrap().is_empty());
        for (i, r) in results.iter().enumerate() {
            assert!(
                r.outcome == SwarmOutcome::Aborted,
                "member {i}: {:?}",
                r.outcome
            );
            assert!(
                r.state == SwarmStartState::Started,
                "member {i} launched — started verdict: {:?}",
                r.state
            );
            assert!(r.agent_id.is_some(), "member {i} keeps its identity");
            assert!(
                r.error
                    .as_deref()
                    .unwrap()
                    .contains("before this subagent finished"),
                "error: {:?}",
                r.error
            );
        }
        // The parked member specifically keeps ITS identity.
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-0"));
    }

    /// Given a member whose rate-limit failure settles AFTER the batch's
    /// cancellation (it does not watch its token), When the settle runs,
    /// Then the cancel verdict owns the terminal state — aborted/started
    /// with the user-interruption message, not a failed row.
    #[tokio::test(start_paused = true)]
    async fn should_settle_a_rate_limit_racing_cancellation_as_aborted() {
        struct RacingHost {
            inner: RecordingHost,
        }
        impl SubagentHost for RacingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let id = format!("agent-{}", self.inner.spawns.lock().unwrap().len());
                self.inner
                    .spawns
                    .lock()
                    .unwrap()
                    .push((id.clone(), tokio::time::Instant::now()));
                Ok(SubagentHandle::new(
                    id,
                    request.profile_name,
                    request.model,
                    // The failure settles after the cancellation but does
                    // not watch the token — the exact race shape.
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Err(SubagentError::RateLimited(
                            "HTTP 429: too many requests".into(),
                        ))
                    }),
                ))
            }
            fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
            fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
                self.inner
                    .suspended
                    .lock()
                    .unwrap()
                    .push(agent_id.to_string());
            }
        }
        let host = Arc::new(RacingHost {
            inner: RecordingHost {
                spawns: Mutex::new(Vec::new()),
                resumes: Mutex::new(Vec::new()),
                suspended: Mutex::new(Vec::new()),
            },
        });
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            vec![spawn_task(0), spawn_task(1)],
            None,
            cancel.clone(),
            SwarmBatchOptions::default(),
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await; // let the burst launch
        cancel.cancel(); // the batch observes cancellation...
        // ...but both members still settle with their own outcomes at t=100.
        let results = batch.await.unwrap();
        for (i, r) in results.iter().enumerate() {
            assert!(
                r.outcome == SwarmOutcome::Aborted,
                "member {i}: {:?}",
                r.outcome
            );
            assert!(r.state == SwarmStartState::Started);
            assert!(
                r.error
                    .as_deref()
                    .unwrap()
                    .contains("before this subagent finished"),
                "error: {:?}",
                r.error
            );
        }
        // The racing rate limits never park (no suspended, no retry).
        assert!(host.inner.suspended.lock().unwrap().is_empty());
        assert!(host.inner.resumes.lock().unwrap().is_empty());
    }

    /// Given a burst where four members fail with rate limits and one
    /// completes, When the retries launch after the backoff, Then the
    /// post-rate-limit concurrency stays within the rate-limit pacing — the
    /// global launch gate serializes the retries one base interval apart
    /// (seed capacity = 0 completed + 4 in-flight at the first failure; the
    /// 2s-gated requeue shrink never fires here because all four requeues
    /// land within the window, so the bound is enforced by the gate; an
    /// unpaced scheduler would relaunch all four retries at once, peak 4).
    /// NOTE: the seed math assumes FuturesUnordered's insertion-order
    /// polling — the first settling member sees the other four in flight.
    #[tokio::test(start_paused = true)]
    async fn should_shrink_concurrency_to_the_rate_limit_capacity() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        struct RateLimitCapHost {
            active: Arc<AtomicUsize>,
            peak_overall: Arc<AtomicUsize>,
            peak_after_rate_limit: Arc<AtomicUsize>,
            rate_limit_entered: Arc<AtomicBool>,
        }
        impl SubagentHost for RateLimitCapHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak_overall.fetch_max(now, Ordering::SeqCst);
                if self.rate_limit_entered.load(Ordering::SeqCst) {
                    self.peak_after_rate_limit.fetch_max(now, Ordering::SeqCst);
                }
                let rate_limited = request.swarm_index.is_some_and(|i| i < 4);
                let active = Arc::clone(&self.active);
                let peak_after = Arc::clone(&self.peak_after_rate_limit);
                let entered = Arc::clone(&self.rate_limit_entered);
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.swarm_index.unwrap_or(0)),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        if entered.load(Ordering::SeqCst) {
                            peak_after.fetch_max(0, Ordering::SeqCst);
                        }
                        if rate_limited {
                            Err(SubagentError::RateLimited(
                                "HTTP 429: too many requests".into(),
                            ))
                        } else {
                            Ok(SubagentCompletion::from_text("ok"))
                        }
                    }),
                ))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                assert!(request.retry, "requeue must use the retry shape");
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak_overall.fetch_max(now, Ordering::SeqCst);
                if self.rate_limit_entered.load(Ordering::SeqCst) {
                    self.peak_after_rate_limit.fetch_max(now, Ordering::SeqCst);
                }
                let active = Arc::clone(&self.active);
                let peak_after = Arc::clone(&self.peak_after_rate_limit);
                let entered = Arc::clone(&self.rate_limit_entered);
                Ok(SubagentHandle::new(
                    request.agent_id,
                    "coder".into(),
                    None,
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        if entered.load(Ordering::SeqCst) {
                            peak_after.fetch_max(0, Ordering::SeqCst);
                        }
                        Ok(SubagentCompletion::from_text("recovered"))
                    }),
                ))
            }
            fn notify_suspended(&self, _agent_id: &str, _cause: &SuspendCause) {
                self.rate_limit_entered.store(true, Ordering::SeqCst);
            }
        }
        let host = Arc::new(RateLimitCapHost {
            active: Arc::new(AtomicUsize::new(0)),
            peak_overall: Arc::new(AtomicUsize::new(0)),
            peak_after_rate_limit: Arc::new(AtomicUsize::new(0)),
            rate_limit_entered: Arc::new(AtomicBool::new(false)),
        });
        let results = run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..6).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions::default(),
        )
        .await;
        assert!(
            results.iter().all(|r| r.outcome == SwarmOutcome::Completed),
            "{results:?}"
        );
        // The initial burst runs 5-wide; the post-rate-limit retries are
        // paced by the global launch gate: capacity stays at the seed 4 (the
        // 2s-gated requeue shrink never fires in this window), and the gate
        // serializes the retries one base interval apart (actual overlap
        // peak 1). An unpaced scheduler would relaunch all four at once
        // (peak 4).
        assert_eq!(host.peak_overall.load(Ordering::SeqCst), 5);
        assert!(
            host.peak_after_rate_limit.load(Ordering::SeqCst) <= 3,
            "rate-limit pacing must bound retry concurrency"
        );
    }

    // ===== US5.1: member tracking under the TaskManager =====

    /// Given a registry-wired batch whose members complete, When the batch
    /// settles, Then every member was registered as a foreground agent task
    /// (agent_id + swarm_index + parent tool call carried for task.list) and
    /// its tracked task settled completed.
    #[tokio::test]
    async fn should_register_completed_members_and_settle_their_tracked_tasks() {
        let host = recording_host();
        let manager = Arc::new(TaskManager::new());
        let results = run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            Some("toolcall-1".to_string()),
            CancelToken::new(),
            SwarmBatchOptions {
                registry: Some(SwarmTaskRegistry {
                    tasks: Arc::clone(&manager),
                    session_id: "session-1".to_string(),
                }),
                ..Default::default()
            },
        )
        .await;
        assert!(results.iter().all(|r| r.outcome == SwarmOutcome::Completed));
        let mut tasks = manager.list();
        tasks.sort_by_key(|task| match &task.kind {
            TaskKind::Agent { swarm_index, .. } => swarm_index.unwrap_or(0),
            _ => 0,
        });
        assert_eq!(tasks.len(), 2, "one tracked task per member");
        for (i, task) in tasks.iter().enumerate() {
            assert_eq!(task.session_id, "session-1");
            let TaskKind::Agent {
                agent_id,
                subagent_type,
                parent_tool_call_id,
                swarm_index,
                ..
            } = &task.kind
            else {
                panic!("member {i} must register as an agent-kind task");
            };
            assert!(agent_id.is_some(), "member {i} carries its agent id");
            assert_eq!(subagent_type.as_deref(), Some("coder"));
            assert_eq!(parent_tool_call_id.as_deref(), Some("toolcall-1"));
            assert_eq!(*swarm_index, Some(i as u32));
            assert!(
                matches!(
                    task.status,
                    crate::task::TaskStatus::Completed { exit_code: 0 }
                ),
                "member {i} settled completed, got {:?}",
                task.status
            );
        }
    }

    /// Given a registry-wired batch whose Spawn members resolve a model, When
    /// a member launches, Then the tracked task row carries the RESOLVED model
    /// the handle reports — not the raw request value — so task-list
    /// consumers (the web's per-agent model badge) see the same model across
    /// polling rebuilds.
    #[tokio::test]
    async fn should_register_members_with_the_resolved_model_when_the_host_resolves_one() {
        struct ResolvingHost;
        impl SubagentHost for ResolvingHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let cancel = request.cancel.clone();
                let id = format!("agent-{}", request.swarm_index.unwrap_or(0));
                Ok(SubagentHandle::new(
                    id,
                    request.profile_name,
                    // The host's resolution (explicit → profile default →
                    // caller) — deliberately DIFFERENT from the raw request
                    // (None = inherit) to pin the two apart.
                    Some("resolved-model".to_string()),
                    Box::pin(async move {
                        cancel.cancelled().await;
                        Err(SubagentError::Cancelled)
                    }),
                ))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
        }
        let manager = Arc::new(TaskManager::new());
        let batch = tokio::spawn(run_swarm_batch(
            Arc::new(ResolvingHost) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            Some("toolcall-1".to_string()),
            CancelToken::new(),
            SwarmBatchOptions {
                registry: Some(SwarmTaskRegistry {
                    tasks: Arc::clone(&manager),
                    session_id: "session-1".to_string(),
                }),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Then every tracked task row carries the resolved model
        let mut tasks = manager.list();
        assert_eq!(tasks.len(), 2, "one tracked task per launched member");
        tasks.retain(|t| {
            matches!(
                &t.kind,
                TaskKind::Agent { agent_id: Some(_), model: Some(m), .. } if m == "resolved-model"
            )
        });
        assert_eq!(
            tasks.len(),
            2,
            "both members' rows carry the resolved model"
        );
        batch.abort();
    }

    /// Given a registry-wired batch where member 0 hangs until its cancel
    /// token fires and member 1 completes, When member 0's tracked task is
    /// stopped through the TaskManager (the `task.cancel` path the web's
    /// Stop button rides), Then only member 0 settles aborted — member 1 is
    /// untouched — and the stopped member's task row reads stopped.
    #[tokio::test]
    async fn should_stop_a_single_member_when_its_tracked_task_is_cancelled() {
        struct MixedHost;
        impl SubagentHost for MixedHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let id = format!("agent-{}", request.swarm_index.unwrap_or(0));
                if request.swarm_index == Some(0) {
                    let cancel = request.cancel.clone();
                    Ok(SubagentHandle::new(
                        id,
                        request.profile_name,
                        request.model,
                        Box::pin(async move {
                            cancel.cancelled().await;
                            Err(SubagentError::Cancelled)
                        }),
                    ))
                } else {
                    Ok(SubagentHandle::from_result(
                        id,
                        request.profile_name,
                        request.model,
                        "done".into(),
                    ))
                }
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
        }
        let host = Arc::new(MixedHost);
        let manager = Arc::new(TaskManager::new());
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions {
                registry: Some(SwarmTaskRegistry {
                    tasks: Arc::clone(&manager),
                    session_id: "session-1".to_string(),
                }),
                ..Default::default()
            },
        ));

        // Wait until member 0's tracked task exists, then stop it exactly
        // like the engine's task.cancel handler does (manager.stop fires the
        // attached member cancel signal).
        let mut member0_task_id = None;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if let Some(task) = manager.list().into_iter().find(|t| {
                matches!(
                    &t.kind,
                    TaskKind::Agent { agent_id: Some(id), .. } if id == "agent-0"
                )
            }) {
                member0_task_id = Some(task.id.clone());
                break;
            }
        }
        let member0_task_id = member0_task_id.expect("member 0 registers a tracked task");
        manager.stop(&member0_task_id);

        let results = batch.await.unwrap();
        assert_eq!(results[0].outcome, SwarmOutcome::Aborted);
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);

        let tasks = manager.list();
        let member0 = tasks
            .iter()
            .find(|t| {
                matches!(&t.kind, TaskKind::Agent { agent_id: Some(id), .. } if id == "agent-0")
            })
            .expect("member 0 task row exists");
        assert!(
            matches!(member0.status, crate::task::TaskStatus::Stopped),
            "the stopped member's task row reads stopped, got {:?}",
            member0.status
        );
        let member1 = tasks
            .iter()
            .find(|t| {
                matches!(&t.kind, TaskKind::Agent { agent_id: Some(id), .. } if id == "agent-1")
            })
            .expect("member 1 task row exists");
        assert!(
            matches!(member1.status, crate::task::TaskStatus::Completed { .. }),
            "the untouched member's task row reads completed, got {:?}",
            member1.status
        );
    }

    // ===== T6: approval-gate parking (ADR-011 c/d) =====

    /// A host whose members' completions block until the test releases
    /// them, and whose approval-gate boundary is scripted by the test
    /// through the per-member suspension channel the batch wires into the
    /// handle (the same channel the production host's gate hooks feed —
    /// the host callback seam, one channel per member, not a global bus).
    struct GateHost {
        inner: RecordingHost,
        gates: Mutex<HashMap<String, GateControl>>,
        /// When each member's completion resolved, in virtual time.
        completed_at: Arc<Mutex<Vec<(String, tokio::time::Instant)>>>,
    }

    struct GateControl {
        /// The per-member suspension feed (Suspended on register, Resumed
        /// on resolve — the exact wire the gate hooks ride).
        tx: tokio::sync::mpsc::UnboundedSender<SuspensionNotice>,
        /// Releases the member's completion (the test's scripted result).
        /// `None` once released.
        release_tx: Option<tokio::sync::oneshot::Sender<Result<SubagentCompletion, SubagentError>>>,
    }

    impl GateHost {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: RecordingHost {
                    spawns: Mutex::new(Vec::new()),
                    resumes: Mutex::new(Vec::new()),
                    suspended: Mutex::new(Vec::new()),
                },
                gates: Mutex::new(HashMap::new()),
                completed_at: Arc::new(Mutex::new(Vec::new())),
            })
        }

        fn spawn_count(&self) -> usize {
            self.inner.spawns.lock().unwrap().len()
        }

        /// The test's gate hook: the member's approval ask registers.
        fn park_for_approval(&self, agent_id: &str) {
            let gates = self.gates.lock().unwrap();
            let control = gates
                .get(agent_id)
                .expect("a launched member has a gate control");
            control
                .tx
                .send(SuspensionNotice::Suspended(SuspendCause::Approval {
                    request_id: format!("req-{agent_id}"),
                    tool_call_id: None,
                }))
                .expect("the batch reads the suspension feed");
        }

        /// The test's gate hook: the approval resolves.
        fn decide(&self, agent_id: &str) {
            let gates = self.gates.lock().unwrap();
            let control = gates
                .get(agent_id)
                .expect("a launched member has a gate control");
            control
                .tx
                .send(SuspensionNotice::Resumed)
                .expect("the batch reads the suspension feed");
        }

        /// Release the member's completion with a scripted result.
        fn release(&self, agent_id: &str, result: Result<SubagentCompletion, SubagentError>) {
            let mut gates = self.gates.lock().unwrap();
            let control = gates
                .get_mut(agent_id)
                .expect("a launched member has a gate control");
            control
                .release_tx
                .take()
                .expect("the completion still waits")
                .send(result)
                .expect("the completion still waits");
        }

        fn completion_time(&self, agent_id: &str) -> tokio::time::Instant {
            self.completed_at
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| id == agent_id)
                .expect("the member completed")
                .1
        }
    }

    impl SubagentHost for GateHost {
        fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
            let id = format!("agent-{}", self.inner.spawns.lock().unwrap().len());
            self.inner
                .spawns
                .lock()
                .unwrap()
                .push((id.clone(), tokio::time::Instant::now()));
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            self.gates.lock().unwrap().insert(
                id.clone(),
                GateControl {
                    tx,
                    release_tx: Some(release_tx),
                },
            );
            let cancel = request.cancel.clone();
            let completed_at = Arc::clone(&self.completed_at);
            let id_for_log = id.clone();
            let handle = SubagentHandle::new(
                id,
                request.profile_name,
                request.model,
                Box::pin(async move {
                    let result = tokio::select! {
                        r = release_rx => r.expect("the test releases the member"),
                        _ = cancel.cancelled() => Err(SubagentError::Cancelled),
                    };
                    completed_at
                        .lock()
                        .unwrap()
                        .push((id_for_log, tokio::time::Instant::now()));
                    result
                }),
            )
            .with_suspension_rx(rx);
            Ok(handle)
        }
        fn resume(&self, _request: ResumeRequest) -> EngineResult<SubagentHandle> {
            Err(crate::error::EngineError::RequestInvalid(
                "not implemented".into(),
            ))
        }
        fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
            self.inner
                .suspended
                .lock()
                .unwrap()
                .push(agent_id.to_string());
        }
    }

    /// Given a 5s per-attempt budget and a member that runs 1s then parks
    /// at the approval gate, When the decision is held for 10s (double the
    /// whole budget) and then resolves, Then the member continues from the
    /// FROZEN remainder (4s) — it must not time out while parked, and it
    /// must time out only after consuming the remainder past the resume,
    /// not a renewed 5s budget (ADR-011 c: 停表，剩余额度续跑).
    #[tokio::test(start_paused = true)]
    async fn should_freeze_the_attempt_budget_while_a_member_awaits_user_approval() {
        let host = GateHost::new();
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            cancel.clone(),
            SwarmBatchOptions {
                attempt_timeout: Some(Duration::from_secs(5)),
                max_concurrency: Some(2),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await; // the burst launches
        host.release("agent-1", Ok(SubagentCompletion::from_text("ok")));
        // Member 0 consumes 1s of its 5s budget, then parks at the gate.
        tokio::time::advance(Duration::from_secs(1)).await;
        host.park_for_approval("agent-0");
        tokio::task::yield_now().await;
        // The user holds the ask for 10s — far past the 5s budget. The
        // clock must be STOPPED for the wait: no timeout may settle the
        // member while it is parked.
        tokio::time::advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert!(
            !batch.is_finished(),
            "the member must not time out while it awaits the user"
        );
        // When the approval resolves at t=10, the member continues with
        // the frozen remainder (4s), not a fresh budget.
        host.decide("agent-0");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(3_500)).await;
        tokio::task::yield_now().await;
        assert!(
            !batch.is_finished(),
            "3.5s past the resume is still inside the 4s remainder"
        );
        // 4.5s past the resume crosses the remainder — and stays short of
        // a renewed 5s budget, so only a true freeze (not a renewal) times
        // the member out here.
        tokio::time::advance(Duration::from_millis(1_000)).await;
        tokio::task::yield_now().await;
        assert!(batch.is_finished(), "the frozen remainder expired at t=14");
        let results = batch.await.unwrap();
        assert_eq!(results[0].outcome, SwarmOutcome::Failed);
        assert_eq!(results[0].state, SwarmStartState::Started);
        assert!(
            results[0].error.as_deref().unwrap().contains("timed out"),
            "error: {:?}",
            results[0].error
        );
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);
    }

    /// Given max_concurrency=1 and a member A parked at the approval gate,
    /// When A's approval resolves while member B still holds the slot,
    /// Then A yields its slot while parked (B launches), resumes on its
    /// SAME identity with no re-spawn and no resume (无双跑), and re-
    /// COMPETES for the slot — it continues only after B's slot frees.
    #[tokio::test(start_paused = true)]
    async fn should_yield_the_concurrency_slot_while_a_member_awaits_user_approval() {
        let host = GateHost::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions {
                max_concurrency: Some(1),
                attempt_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(host.spawn_count(), 1, "A takes the only slot");
        // A parks — the slot must free for the queued member B.
        host.park_for_approval("agent-0");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            host.spawn_count(),
            2,
            "the parked member's slot frees: B launches while A waits"
        );
        // A's approval resolves while B still runs. A must re-enter the
        // competition and WAIT for the slot — releasing A's completion
        // now cannot settle it before B.
        host.decide("agent-0");
        tokio::task::yield_now().await;
        host.release("agent-0", Ok(SubagentCompletion::from_text("a done")));
        tokio::task::yield_now().await;
        host.release("agent-1", Ok(SubagentCompletion::from_text("b done")));
        let results = batch.await.unwrap();
        assert_eq!(results[0].outcome, SwarmOutcome::Completed);
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);
        assert_eq!(
            host.spawn_count(),
            2,
            "the resume reuses A's identity — no re-spawn, no double run"
        );
        assert!(
            host.inner.resumes.lock().unwrap().is_empty(),
            "the resume is a continuation, not a resume-shaped relaunch"
        );
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-0"));
        // The slot re-competition: A's continuation ran only AFTER B's slot
        // freed (its release earlier could not settle it while B held the
        // slot).
        assert!(
            host.completion_time("agent-0") >= host.completion_time("agent-1"),
            "A must finish after the slot frees, not while B runs"
        );
    }

    /// Given a 5s per-attempt budget and a member whose first attempt
    /// fails with a rate limit after 1s of run (backoff 6s), When the
    /// retry relaunches, Then the retry consumes a FRESH full budget — the
    /// approval-pause logic must NOT leak into the rate-limit backoff
    /// (ADR-011 c 对照: 退避计入交付周期，不停表).
    #[tokio::test(start_paused = true)]
    async fn should_keep_counting_the_clock_during_rate_limit_backoff() {
        struct RetryBudgetHost {
            spawns: Mutex<Vec<tokio::time::Instant>>,
            resumed_at: Mutex<Vec<(String, tokio::time::Instant)>>,
            /// When the retry's cancel token fired (the timeout expiry —
            /// the one clock event under test).
            cancelled_at: Arc<Mutex<Vec<tokio::time::Instant>>>,
            suspended: Mutex<Vec<String>>,
            /// Releases member 1 (the sibling that keeps the batch alive
            /// past the first settle — it must not share the 5s budget
            /// under test).
            release_1: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }
        impl SubagentHost for RetryBudgetHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                self.spawns
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now());
                let id = format!("agent-{}", request.swarm_index.unwrap_or(0));
                let cancel = request.cancel.clone();
                let completion: crate::subagent::CompletionFuture = if request.swarm_index
                    == Some(0)
                {
                    // Member 0's first attempt: 1s of real budget consumed,
                    // then the rate limit.
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Err(SubagentError::RateLimited(
                            "HTTP 429: too many requests".into(),
                        ))
                    })
                } else {
                    // Member 1 holds the batch alive so the requeue branch
                    // (not the only-unfinished verdict) runs; the test
                    // releases it once the requeue is recorded.
                    let (release_1_tx, release_1_rx) = tokio::sync::oneshot::channel();
                    *self.release_1.lock().unwrap() = Some(release_1_tx);
                    Box::pin(async move {
                        tokio::select! {
                            _ = release_1_rx => Ok(SubagentCompletion::from_text("sibling done")),
                            _ = cancel.cancelled() => Err(SubagentError::Cancelled),
                        }
                    })
                };
                Ok(SubagentHandle::new(
                    id,
                    request.profile_name,
                    request.model,
                    completion,
                ))
            }
            fn resume(&self, request: ResumeRequest) -> EngineResult<SubagentHandle> {
                self.resumed_at
                    .lock()
                    .unwrap()
                    .push((request.agent_id.clone(), tokio::time::Instant::now()));
                let cancel = request.cancel.clone();
                let cancelled_at = Arc::clone(&self.cancelled_at);
                Ok(SubagentHandle::new(
                    request.agent_id,
                    "coder".into(),
                    None,
                    Box::pin(async move {
                        cancel.cancelled().await;
                        cancelled_at
                            .lock()
                            .unwrap()
                            .push(tokio::time::Instant::now());
                        Err(SubagentError::Cancelled)
                    }),
                ))
            }
            fn notify_suspended(&self, agent_id: &str, _cause: &SuspendCause) {
                self.suspended.lock().unwrap().push(agent_id.to_string());
            }
        }
        let host = Arc::new(RetryBudgetHost {
            spawns: Mutex::new(Vec::new()),
            resumed_at: Mutex::new(Vec::new()),
            cancelled_at: Arc::new(Mutex::new(Vec::new())),
            suspended: Mutex::new(Vec::new()),
            release_1: Mutex::new(None),
        });
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions {
                attempt_timeout: Some(Duration::from_secs(5)),
                retry_base_ms: Some(6_000),
                max_concurrency: Some(2),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // First attempt fails at t=1; the retry must wait out the 6s
        // backoff — the backoff counts in wall time.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            host.resumed_at.lock().unwrap().len(),
            0,
            "the retry waits out its backoff"
        );
        assert_eq!(*host.suspended.lock().unwrap(), vec!["agent-0".to_string()]);
        // Member 1's keep-alive job is done once the requeue is recorded.
        host.release_1
            .lock()
            .unwrap()
            .take()
            .expect("member 1's blocker exists")
            .send(())
            .expect("member 1 still waits");
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            host.resumed_at.lock().unwrap().len(),
            1,
            "the retry relaunches"
        );
        let retry_at = host.resumed_at.lock().unwrap()[0].1;
        let failed_at = host.spawns.lock().unwrap()[0];
        assert!(
            retry_at.duration_since(failed_at) >= Duration::from_secs(6),
            "the backoff counted in the member's delivery cycle"
        );
        // 3.5s into the retry: nothing has timed out.
        tokio::time::advance(Duration::from_millis(3_500)).await;
        tokio::task::yield_now().await;
        assert!(
            host.cancelled_at.lock().unwrap().is_empty(),
            "3.5s into the retry: still inside the fresh budget"
        );
        // 4.1s into the retry: a frozen REMAINDER (5-1=4s) would already
        // have expired — the 对照: the retry must NOT inherit an
        // approval-pause-style frozen remainder.
        tokio::time::advance(Duration::from_millis(600)).await;
        tokio::task::yield_now().await;
        assert!(
            host.cancelled_at.lock().unwrap().is_empty(),
            "the retry runs on a renewed full budget, not the frozen remainder"
        );
        // The retry expires only on its own full 5s clock (t=12).
        tokio::time::advance(Duration::from_millis(1_000)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            host.cancelled_at.lock().unwrap().len(),
            1,
            "the retry times out"
        );
        let at = host.cancelled_at.lock().unwrap()[0];
        assert!(
            at.duration_since(retry_at) >= Duration::from_secs(5),
            "the retry consumed its full budget"
        );
        let results = batch.await.unwrap();
        assert!(results[0].outcome == SwarmOutcome::Failed);
        assert!(
            results[0].error.as_deref().unwrap().contains("timed out"),
            "error: {:?}",
            results[0].error
        );
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);
        assert_eq!(host.resumed_at.lock().unwrap().len(), 1);
    }

    /// Given a member parked at the approval gate, When the batch is
    /// cancelled while the ask is pending, Then the member settles
    /// aborted/started with its identity (ADR-011 f: 等待用户期间的批次取消
    /// 按 aborted 定局) — the continuation is dropped, never resumed, and
    /// the sibling's result is untouched.
    #[tokio::test(start_paused = true)]
    async fn should_settle_a_member_parked_at_the_gate_aborted_on_batch_cancellation() {
        let host = GateHost::new();
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            cancel.clone(),
            SwarmBatchOptions {
                max_concurrency: Some(2),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        host.park_for_approval("agent-0");
        host.release("agent-1", Ok(SubagentCompletion::from_text("ok")));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        cancel.cancel();
        let results = batch.await.unwrap();
        assert_eq!(results[0].outcome, SwarmOutcome::Aborted);
        assert_eq!(results[0].state, SwarmStartState::Started);
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-0"));
        assert!(
            results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("before this subagent finished"),
            "error: {:?}",
            results[0].error
        );
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);
        assert_eq!(
            host.spawn_count(),
            2,
            "no member is re-launched by the cancellation"
        );
        assert!(host.inner.resumes.lock().unwrap().is_empty());
    }

    /// Given a member that parks and resumes repeatedly (two approval
    /// asks in one attempt), When each decision arrives, Then the member
    /// survives the round trips on the SAME identity — the suspension
    /// feed is multi-shot across the continuation.
    #[tokio::test(start_paused = true)]
    async fn should_park_and_resume_a_member_across_repeated_approval_asks() {
        let host = GateHost::new();
        let batch = tokio::spawn(run_swarm_batch(
            Arc::clone(&host) as Arc<dyn SubagentHost>,
            (0..2).map(spawn_task).collect(),
            None,
            CancelToken::new(),
            SwarmBatchOptions {
                max_concurrency: Some(1),
                attempt_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            },
        ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(host.spawn_count(), 1);
        // First ask: park → slot frees → B launches.
        host.park_for_approval("agent-0");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(host.spawn_count(), 2);
        // Decide → the slot is still B's, so A waits; B finishes, then A
        // re-enters and asks AGAIN.
        host.decide("agent-0");
        host.release("agent-1", Ok(SubagentCompletion::from_text("b done")));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        host.park_for_approval("agent-0");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(host.spawn_count(), 2, "resume reuses the identity");
        // Second ask resolves, then the member finishes.
        host.decide("agent-0");
        host.release("agent-0", Ok(SubagentCompletion::from_text("a done")));
        let results = batch.await.unwrap();
        assert_eq!(results[0].outcome, SwarmOutcome::Completed);
        assert_eq!(results[1].outcome, SwarmOutcome::Completed);
        assert_eq!(results[0].agent_id.as_deref(), Some("agent-0"));
        assert!(host.inner.resumes.lock().unwrap().is_empty());
    }

    // ===== Keepalive: the foreground batch's stall-watchdog feed =====

    fn test_activity_tracker() -> std::sync::Arc<crate::turn_flow::ActivityTracker> {
        std::sync::Arc::new(crate::turn_flow::ActivityTracker::new(std::sync::Arc::new(
            std::sync::Mutex::new(crate::compaction::FullCompaction::new(
                crate::compaction::CompactionStrategy::default(),
            )),
        )))
    }

    /// Given a foreground swarm whose member works silently (its engine
    /// activity clock keeps advancing while no completion settles and no
    /// engine live event reaches the parent turn), When the keepalive tick
    /// samples the in-flight member, Then the batch forwards a touch-only
    /// `swarm.activity` ToolUpdate through the progress channel (the
    /// stall-watchdog feed) — and When the member's clock freezes, Then
    /// the next tick forwards nothing (a silent member stays watchdog
    /// territory).
    #[tokio::test(start_paused = true)]
    async fn should_feed_the_stall_watchdog_only_while_members_advance() {
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        let live_clock = test_activity_tracker();
        let live_for_host = std::sync::Arc::clone(&live_clock);
        let release = Arc::new(AtomicBool::new(false));

        struct SilentHost {
            live: std::sync::Arc<crate::turn_flow::ActivityTracker>,
            release: Arc<AtomicBool>,
        }
        impl SubagentHost for SilentHost {
            fn spawn(&self, request: SpawnRequest) -> EngineResult<SubagentHandle> {
                let cancel = request.cancel.clone();
                let release = Arc::clone(&self.release);
                let is_live = request.swarm_index == Some(0);
                let activity = if is_live {
                    std::sync::Arc::clone(&self.live)
                } else {
                    test_activity_tracker()
                };
                let activity_for_handle = std::sync::Arc::clone(&activity);
                Ok(SubagentHandle::new(
                    format!("agent-{}", request.swarm_index.unwrap_or(0)),
                    request.profile_name,
                    request.model,
                    Box::pin(async move {
                        if is_live {
                            // Silent work: the clock advances with no
                            // completion and no engine live event.
                            crate::turn_flow::ActivityTracker::on_step(&activity, 1);
                        }
                        loop {
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    return Err(SubagentError::Cancelled);
                                }
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                                    if release.load(Ordering::SeqCst) {
                                        return Ok(SubagentCompletion::from_text("done"));
                                    }
                                    if is_live {
                                        crate::turn_flow::ActivityTracker::on_step(
                                            &activity, 2,
                                        );
                                    }
                                }
                            }
                        }
                    }),
                )
                .with_activity(activity_for_handle))
            }
            fn resume(&self, _r: ResumeRequest) -> EngineResult<SubagentHandle> {
                Err(crate::error::EngineError::RequestInvalid(
                    "not implemented".into(),
                ))
            }
        }

        // The captured watchdog feed: every ToolUpdate the batch forwards.
        let forwarded: Arc<StdMutex<Vec<crate::tool::ToolUpdate>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let forwarded_for_sink = Arc::clone(&forwarded);
        let progress = crate::tool::ToolProgressHandle::with_sink(Arc::new(move |update| {
            forwarded_for_sink.lock().unwrap().push(update);
        }));

        let host = Arc::new(SilentHost {
            live: live_for_host,
            release: Arc::clone(&release),
        });
        let cancel = CancelToken::new();
        let batch = tokio::spawn(run_swarm_batch(
            host as Arc<dyn SubagentHost>,
            vec![spawn_task(0), spawn_task(1)],
            None,
            cancel.clone(),
            SwarmBatchOptions {
                progress,
                ..Default::default()
            },
        ));

        // Cross one keepalive tick with the live member's clock advanced
        // twice (launch stamp + one more), then freeze time so the frozen
        // member's clock never moves.
        tokio::time::advance(Duration::from_millis(SWARM_KEEPALIVE_INTERVAL_MS)).await;
        release.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_millis(50)).await;
        let results = batch.await.unwrap();
        assert_eq!(results.len(), 2);

        let feed = forwarded.lock().unwrap();
        let live_ticks = feed
            .iter()
            .filter(|u| u.custom_kind.as_deref() == Some("swarm.activity"))
            .count();
        assert!(
            live_ticks >= 1,
            "the silently-working member must feed the stall watchdog, got {live_ticks} ticks"
        );
        // The frozen member (agent-1) never advances, so no tick may carry
        // its identity.
        assert!(
            feed.iter().all(|u| u
                .custom_data
                .as_ref()
                .and_then(|d| d.get("agentId"))
                .and_then(|v| v.as_str())
                != Some("agent-1")),
            "a frozen member must not feed the stall watchdog"
        );
    }
}
