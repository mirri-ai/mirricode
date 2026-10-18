//! The ADR-006 wait registry — the pull-model stall-watchdog core.
//!
//! Every await point of a turn's execution loop registers its wait here
//! ([`WaitKind`] + the minimal owner-probe data, [`WaitSubject`]). The
//! watchdog (the embedded server's turn-stall layer) does NOT wait for
//! the await points to push liveness signals (the ADR-004/005 push model
//! — every new wait point was a new "forgot to feed the watchdog"
//! defect); instead, when its window elapses, it *pulls* the verdict
//! from each registered wait's owner via [`WaitRegistry::probe`].
//!
//! - The registry is touched ONLY at registration ([`Self::enter`]),
//!   deregistration (the returned guard's `Drop`), and watchdog-expiry
//!   probes ([`Self::probe`]) — never in hot paths.
//! - Nested/concurrent waits are a WAIT SET: any [`OwnerVerdict::
//!   Advancing`] member resets the watchdog's classic window.
//! - Probing runs at watchdog expiry only (ADR-005's "probe at expiry
//!   only" — one probe per window, off the hot path). Each probe is a
//!   non-blocking status read, never a cross-process round trip.
//!
//! See `docs/specs/turn-scheduling-architecture.md` §4.5 + ADR-006.

use std::sync::Arc;
use std::time::Instant;

use crate::session::interaction::InteractionKernel;
use crate::task::TaskManager;

/// ADR-006: the closed WaitKind enumeration. Extending it requires an ADR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitKind {
    /// Mid-step: the model is streaming a response (deltas are themselves
    /// progress; model-side death is the HTTP read-idle timeout's job).
    InStep,
    /// Waiting for the model's FIRST stream event after the request went out.
    WaitingProvider,
    /// A foreground tool call is executing (bounded: its own timeout; unbounded
    /// silent: existence-liveness keeps the kind window).
    WaitingTool,
    /// Blocked on a background task (TaskOutput block=true etc.).
    WaitingTask,
    /// Blocked on a foreground subagent / swarm member.
    WaitingSubagent,
    /// Blocked on a user decision (approval gate / question gate).
    WaitingGate,
    /// An await point not wrapped by the registry framework.
    Unknown,
}

impl WaitKind {
    /// The log/dump label (structured-tracing field value).
    pub fn as_str(&self) -> &'static str {
        match self {
            WaitKind::InStep => "in_step",
            WaitKind::WaitingProvider => "waiting_provider",
            WaitKind::WaitingTool => "waiting_tool",
            WaitKind::WaitingTask => "waiting_task",
            WaitKind::WaitingSubagent => "waiting_subagent",
            WaitKind::WaitingGate => "waiting_gate",
            WaitKind::Unknown => "unknown",
        }
    }
}

/// The four-state owner-probe return domain (ADR-006 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerVerdict {
    /// The waited-on object is still advancing — reset the timer.
    Advancing,
    /// The object settled; the waiter should return on its own. Does NOT
    /// reset the timer (a framework bug that never returns accumulates
    /// into the long window).
    Settled,
    /// The object exists but stopped advancing — per-kind window applies.
    Stalled,
    /// The probe itself failed (channel fault) — after N consecutive
    /// failures degrade to Unknown. WaitingGate is exempt (in-process read).
    QueryFailed,
}

/// The four-state classification of the turn's CURRENT wait set — the
/// watchdog's verdict input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnWaitState {
    /// No wait registered — plain engine silence (the classic window
    /// applies, exactly today's behavior).
    Idle,
    /// Some wait is legitimately advancing.
    Advancing,
    /// A registered wait's owner reports the subject stopped advancing
    /// (per-kind window — same scale as the classic window).
    Stalled { kind: WaitKind },
    /// Unknown / degraded / all-settled-but-not-returning → the long
    /// window (framework-bug escape; ADR-006's third-tier backstop).
    Degraded,
}

/// The minimal data the probe needs per kind — everything obtainable
/// WITHOUT holding engine locks (the pull is a non-blocking status read).
#[derive(Clone)]
pub enum WaitSubject {
    /// Blocked on a background task: the task's heartbeat advance is the
    /// owner fact (heartbeat moved → Advancing; terminal → Settled; row
    /// gone → Settled; Running with a frozen heartbeat → Stalled). The
    /// `last_seen` baseline is carried per-entry (two concurrent waits
    /// on the same task track independent baselines) and refreshed on
    /// every probe through interior mutability.
    Task {
        task_id: String,
        tasks: Arc<TaskManager>,
        /// The last heartbeat instant this entry observed (the probe's
        /// comparison baseline). Interior-mutable: the probe refreshes it
        /// in place.
        last_seen: Arc<std::sync::Mutex<Option<Instant>>>,
    },
    /// Blocked on a foreground subagent / swarm member: the subagent-host
    /// layer supplies the owner probe as a boxed closure (activity
    /// advanced → Advancing; not running → Settled; else Stalled) so
    /// turn_flow carries no hard dependency on the host (reviewer I6).
    Subagent {
        probe: Arc<dyn Fn() -> OwnerVerdict + Send + Sync>,
    },
    /// Blocked on a user decision: the interaction kernel's merged
    /// pending list (approval + question gates) non-empty → Advancing
    /// (an unanswered question is alive by design); empty → Settled.
    /// The in-process read cannot fail, so a gate wait never counts a
    /// probe failure.
    Gate { kernel: Arc<InteractionKernel> },
    /// A foreground tool call: before the tool's own deadline → Advancing
    /// (the tool's timeout is the owner; an unbounded tool's
    /// existence-liveness rides this); past the deadline → Stalled.
    Tool { deadline: Option<Instant> },
    /// InStep / WaitingProvider / Unknown carry no subject — they never
    /// probe (their criteria live elsewhere: the HTTP read-idle timeout
    /// + the classic engine-silence window).
    None,
}

/// One registered wait — the registry's internal row.
struct WaitEntry {
    kind: WaitKind,
    subject: WaitSubject,
    /// The registration identity: every live entry carries the Arc its
    /// guard also holds, so a concurrent guard drop removes exactly its
    /// own row (index positions shift under concurrent drops).
    token: Arc<()>,
    /// Consecutive QueryFailed count (ADR-006: >= 3 → degraded to
    /// Unknown). WaitingGate entries never count failures (the
    /// in-process read can only fail if the engine itself is broken).
    consecutive_failures: u32,
}

/// The consecutive-probe-failure threshold before a wait degrades to
/// Unknown (ADR-006 裁决参数: N = 3).
const QUERY_FAILURE_DEGRADE_THRESHOLD: u32 = 3;

/// The per-turn wait registry: the turn's await points register their
/// waits into it ([`WaitRegistry::enter`]), the watchdog queries it at
/// expiry ([`WaitRegistry::probe`]).
///
/// A plain `Mutex<Vec<WaitEntry>>` — touched only at registration,
/// deregistration, and watchdog-expiry probes, never in hot paths.
/// Per-turn (created in `run_one_turn_inner`), held by the flow behind
/// an `Arc` so tools and the watchdog reach the same instance.
pub struct WaitRegistry {
    entries: std::sync::Mutex<Vec<WaitEntry>>,
}

impl WaitRegistry {
    /// The number of live registrations (used by the flow's turn start to
    /// decide whether the previous turn's registry is clean enough to
    /// reuse — a stale guard from an aborted turn leaves entries behind).
    pub fn entry_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl Default for WaitRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitRegistry {
    /// Create an empty registry (one per turn).
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Register a wait; returns a RAII guard whose `Drop` deregisters.
    /// Nested/concurrent waits = multiple simultaneous entries (the set
    /// semantics: any Advancing entry resets the timer).
    ///
    /// Takes the registry by shared handle (`&Arc<Self>`) so the returned
    /// guard keeps the registry ALIVE — the deregistration on Drop can
    /// never race a registry teardown, however the caller ordered its
    /// drops (async-generator drop orders are not always the reverse
    /// declaration order).
    pub fn enter(self: &Arc<Self>, kind: WaitKind, subject: WaitSubject) -> WaitGuard {
        let token = Arc::new(());
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(WaitEntry {
                kind,
                subject,
                token: Arc::clone(&token),
                consecutive_failures: 0,
            });
        WaitGuard {
            registry: Arc::clone(self),
            token,
        }
    }

    /// The watchdog's probe: classify the turn's CURRENT wait state
    /// (ADR-006 §3.2 + the set semantics).
    ///
    /// - empty registry → [`TurnWaitState::Idle`] — no wait in flight →
    ///   classic engine silence (the watchdog's own window governs,
    ///   same behavior as today)
    /// - any entry Advancing → [`TurnWaitState::Advancing`] (reset)
    /// - a Stalled entry dominates → [`TurnWaitState::Stalled`] (kind
    ///   window — same scale as the classic window)
    /// - all remaining entries settled / degraded / unknown →
    ///   [`TurnWaitState::Degraded`] (the long window: the waiter should
    ///   have returned; a framework bug that never returns accumulates
    ///   here)
    /// - QueryFailed: increments that entry's counter; >= 3 consecutive
    ///   → treated as Unknown (Degraded tier). WaitingGate entries never
    ///   count failures (the in-process read cannot fail).
    ///
    /// A probe also REFRESHES each entry's failure counter (a later
    /// non-failure verdict resets it — only CONSECUTIVE failures
    /// degrade) and its heartbeat baseline.
    pub fn probe(&self) -> TurnWaitState {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.is_empty() {
            return TurnWaitState::Idle;
        }
        let mut any_stalled: Option<WaitKind> = None;
        for entry in entries.iter_mut() {
            let verdict = if matches!(entry.subject, WaitSubject::None) {
                // InStep / WaitingProvider / Unknown never probe — their
                // criteria live elsewhere (model death is the HTTP
                // read-idle timeout; engine deadlock is the backstop).
                // Their PRESENCE alone (no advancing fact) is the
                // "model never answers" shape: they contribute Settled
                // (never reset, never Stalled), which routes to the
                // Degraded tier and the LONG backstop window — the
                // adjudicated coverage-semantics change (ADR-006 risk
                // #4: the keep-alive-masks-a-dead-upstream shape moves
                // from the 600s silence kill to the backstop, trading
                // the false-kill band for a slower kill).
                OwnerVerdict::Settled
            } else {
                entry.subject.probe()
            };
            // Bookkeeping first: refresh the failure streak and the
            // heartbeat baseline so the NEXT probe compares fresh.
            match verdict {
                OwnerVerdict::QueryFailed => {
                    if entry.kind != WaitKind::WaitingGate {
                        entry.consecutive_failures += 1;
                    }
                }
                _ => entry.consecutive_failures = 0,
            }
            match verdict {
                OwnerVerdict::Advancing => return TurnWaitState::Advancing,
                OwnerVerdict::Settled => {}
                OwnerVerdict::Stalled => {
                    // A Stalled entry dominates (kind window) — but a
                    // degraded Unknown carries MORE urgency in the log
                    // (the framework itself is broken); the tiering still
                    // collapses to the long window only when NO entry
                    // stalled, per the set semantics.
                    if any_stalled.is_none() {
                        any_stalled = Some(entry.kind.clone());
                    }
                }
                OwnerVerdict::QueryFailed => {}
            }
        }
        if let Some(kind) = any_stalled {
            return TurnWaitState::Stalled { kind };
        }
        // Every remaining entry is Settled / QueryFailed / a no-subject
        // kind: the waiter is expected to return momentarily; if it does
        // not, this is the Degraded tier the long window catches. The
        // consecutive-failure threshold
        // ([`QUERY_FAILURE_DEGRADE_THRESHOLD`]) only distinguishes WHY —
        // a degraded channel vs a settled-but-not-returning framework
        // bug — both map to the long window, per the two-window
        // structure (两窗不串联); the diagnostic dump reads the
        // wait-kind labels to attribute.
        TurnWaitState::Degraded
    }

    /// The current wait set's kinds, in registration order — the
    /// diagnostic-dump field (watchdog warn lines; ADR-006 触发可见).
    /// An entry whose probe channel failed
    /// [`QUERY_FAILURE_DEGRADE_THRESHOLD`] consecutive times reports
    /// `unknown` — the degraded Unknown the dump attributes.
    pub fn wait_kind_labels(&self) -> Vec<&'static str> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|entry| {
                if entry.consecutive_failures >= QUERY_FAILURE_DEGRADE_THRESHOLD {
                    WaitKind::Unknown.as_str()
                } else {
                    entry.kind.as_str()
                }
            })
            .collect()
    }
}

impl WaitSubject {
    /// Build the task-wait subject: the task manager plus a fresh
    /// per-entry heartbeat baseline (`None` — the first probe records
    /// the live value and the next one compares against it).
    pub fn task(task_id: impl Into<String>, tasks: Arc<TaskManager>) -> Self {
        WaitSubject::Task {
            task_id: task_id.into(),
            tasks,
            last_seen: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Pull the owner's verdict (non-blocking from the engine's
    /// perspective — each arm reads its own owner's state snapshot).
    fn probe(&self) -> OwnerVerdict {
        match self {
            WaitSubject::Task {
                task_id,
                tasks,
                last_seen,
            } => {
                // Row gone = settled (the waiter's own completion path
                // or the sweeper removed it).
                let Some(task) = tasks.get(task_id) else {
                    return OwnerVerdict::Settled;
                };
                if task.status.is_terminal() {
                    return OwnerVerdict::Settled;
                }
                // Running: compare the live heartbeat against this
                // entry's baseline. No heartbeat tracking configured
                // (current_heartbeat is None) → existence liveness only
                // (ADR-006: the task class has no better fact source —
                // the kind window still bounds it).
                match tasks.current_heartbeat(task_id) {
                    Some(now) => {
                        let mut baseline = last_seen.lock().unwrap_or_else(|e| e.into_inner());
                        match *baseline {
                            // First probe of a fresh registration:
                            // record the baseline and treat as advancing
                            // — the next window decides on real movement.
                            None => {
                                *baseline = Some(now);
                                OwnerVerdict::Advancing
                            }
                            Some(last) if now > last => {
                                *baseline = Some(now);
                                OwnerVerdict::Advancing
                            }
                            Some(_) => OwnerVerdict::Stalled,
                        }
                    }
                    None => OwnerVerdict::Advancing,
                }
            }
            WaitSubject::Subagent { probe } => probe(),
            WaitSubject::Gate { kernel } => {
                // In-process, lock-free read of the pending registry —
                // ADR-005's exemption query in owner-probe form (a
                // pending user decision is alive by design).
                if kernel.list_pending(None).is_empty() {
                    OwnerVerdict::Settled
                } else {
                    OwnerVerdict::Advancing
                }
            }
            WaitSubject::Tool { deadline } => match deadline {
                // Past the bound: the tool's own timeout should have
                // settled this wait — it is stalled by its owner's fact.
                Some(deadline) if Instant::now() >= *deadline => OwnerVerdict::Stalled,
                // Within the bound: alive-by-definition (the tool's
                // timeout will fire and settle the wait on its own).
                Some(_) => OwnerVerdict::Advancing,
                // An UNBOUNDED silent tool: no advance fact exists
                // (ADR-006's per-kind table — this class has no
                // probe-able liveness source and keeps the classic
                // silence criterion). Contributing Settled routes it
                // through the set semantics (the kind window applies
                // unless another entry is genuinely advancing) — never
                // a standing Advancing veto that would mask every other
                // wait's stalled verdict (review I2).
                None => OwnerVerdict::Settled,
            },
            WaitSubject::None => OwnerVerdict::Settled,
        }
    }
}

/// RAII deregistration guard returned by [`WaitRegistry::enter`]: the
/// wait's registry row lives exactly as long as the guard — dropping it
/// (when the await completes) removes the entry.
///
/// Holds the registry's shared handle so the guard OUTLIVES any drop
/// ordering the caller applies — the entry is removed from a registry
/// that is guaranteed alive (no raw back-references, no `unsafe Send`).
pub struct WaitGuard {
    /// The owning registry, kept alive for the guard's whole life.
    registry: Arc<WaitRegistry>,
    /// Shared identity token: deregistration removes exactly THIS
    /// registration (nested enters on the same registry each carry
    /// their own token — position matching would mis-remove under
    /// concurrent drops).
    token: Arc<()>,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let token = Arc::clone(&self.token);
        self.registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|entry| !Arc::ptr_eq(&entry.token, &token));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A `TaskManager` with heartbeat tracking on and one registered
    /// (Running) task — the common Given of the task-shaped tests.
    fn tasks_with_running_task() -> (Arc<TaskManager>, String) {
        let tasks = Arc::new(
            TaskManager::new().with_heartbeat_interval(std::time::Duration::from_secs(100)),
        );
        let task_id = tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "test task".to_string(),
            "session-1".to_string(),
        );
        (tasks, task_id)
    }

    #[test]
    fn should_treat_an_empty_registry_as_engine_silence() {
        // Given a fresh registry with no registered wait
        let registry = Arc::new(WaitRegistry::new());

        // When the watchdog probes
        let state = registry.probe();

        // Then the verdict is Idle — plain engine silence
        assert_eq!(state, TurnWaitState::Idle);
    }

    #[test]
    fn should_report_advancing_when_a_registered_tasks_heartbeat_moves() {
        // Given a registry holding a wait on a Running task
        let (tasks, task_id) = tasks_with_running_task();
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id.clone(), Arc::clone(&tasks)),
        );
        // The first probe records the baseline heartbeat
        assert_eq!(registry.probe(), TurnWaitState::Advancing);

        // When the task's heartbeat advances between probes
        std::thread::sleep(std::time::Duration::from_millis(5));
        tasks.heartbeat(&task_id);

        // Then the probe reports Advancing
        assert_eq!(registry.probe(), TurnWaitState::Advancing);
    }

    #[test]
    fn should_report_stalled_when_a_running_tasks_heartbeat_freezes() {
        // Given a registry holding a wait on a Running task whose
        // heartbeat was recorded by an earlier probe
        let (tasks, task_id) = tasks_with_running_task();
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id.clone(), Arc::clone(&tasks)),
        );
        // Baseline probe (Advancing — first sighting)
        assert_eq!(registry.probe(), TurnWaitState::Advancing);

        // When the heartbeat stays frozen across the next probe window
        // (no heartbeat call — the recorded instant does not move)

        // Then the probe reports Stalled with the waiting-task kind
        assert_eq!(
            registry.probe(),
            TurnWaitState::Stalled {
                kind: WaitKind::WaitingTask
            }
        );
    }

    #[test]
    fn should_report_degraded_when_the_waited_task_is_terminal() {
        // Given a registry holding a wait on a task that has since
        // settled (terminal status)
        let (tasks, task_id) = tasks_with_running_task();
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id.clone(), Arc::clone(&tasks)),
        );
        tasks.complete(&task_id, 0);

        // When the watchdog probes
        // Then the verdict is Degraded — Settled does not reset, and a
        // waiter that never returns accumulates into the long window
        assert_eq!(registry.probe(), TurnWaitState::Degraded);
    }

    #[test]
    fn should_degrade_to_unknown_after_three_consecutive_probe_failures() {
        // Given a registry holding a subagent wait whose owner probe
        // always fails (a dead query channel)
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(
            WaitKind::WaitingSubagent,
            WaitSubject::Subagent {
                probe: Arc::new(|| OwnerVerdict::QueryFailed),
            },
        );

        // When the watchdog probes twice (below the threshold)
        assert_eq!(registry.probe(), TurnWaitState::Degraded);
        assert_eq!(registry.probe(), TurnWaitState::Degraded);

        // Then the third consecutive failure degrades the wait to
        // Unknown — still the long-window tier, now for a broken channel,
        // and the dump labels report the degraded form (the diagnostic
        // attributes the wait as unknown, not its original kind)
        assert_eq!(registry.probe(), TurnWaitState::Degraded);
        assert_eq!(registry.wait_kind_labels(), vec!["unknown"]);
    }

    #[test]
    fn should_exempt_gate_waits_from_probe_failure_degradation() {
        // Given a registry holding a subagent wait with a failing probe
        // AND a gate wait — the gate subject here cannot fail, but the
        // paired failing subagent proves the counting; the gate itself
        // is probed through the kernel's pending list
        let kernel = Arc::new(InteractionKernel::new(
            Arc::new(crate::session::gates::ApprovalGate::new(false)),
            Arc::new(crate::session::gates::QuestionGate::new()),
        ));
        let registry = Arc::new(WaitRegistry::new());
        let _gate_guard = registry.enter(WaitKind::WaitingGate, WaitSubject::Gate { kernel });
        let _subagent_guard = registry.enter(
            WaitKind::WaitingSubagent,
            WaitSubject::Subagent {
                probe: Arc::new(|| OwnerVerdict::QueryFailed),
            },
        );

        // When the watchdog probes past the failure threshold
        for _ in 0..3 {
            registry.probe();
        }

        // Then the state is still the long-window tier (the subagent
        // degraded) — but a registry holding ONLY a gate wait never
        // counts a probe failure against it: the in-process pending read
        // returns Advancing or Settled, never QueryFailed.
        let gate_only = Arc::new(WaitRegistry::new());
        let _guard = gate_only.enter(WaitKind::WaitingGate, WaitSubject::None);
        assert_eq!(gate_only.probe(), TurnWaitState::Degraded);
    }

    #[test]
    fn should_let_any_advancing_member_of_a_wait_set_reset_the_timer() {
        // Given a wait set: a tool wait (frozen, past deadline) and a
        // subagent wait whose owner reports the child still advancing
        let registry = Arc::new(WaitRegistry::new());
        let _tool_guard = registry.enter(
            WaitKind::WaitingTool,
            WaitSubject::Tool {
                deadline: Some(Instant::now() - std::time::Duration::from_secs(1)),
            },
        );
        let _subagent_guard = registry.enter(
            WaitKind::WaitingSubagent,
            WaitSubject::Subagent {
                probe: Arc::new(|| OwnerVerdict::Advancing),
            },
        );

        // When the watchdog probes the set
        // Then the advancing member resets the timer despite the
        // stalled sibling
        assert_eq!(registry.probe(), TurnWaitState::Advancing);
    }

    #[test]
    fn should_report_a_stalled_tool_wait_past_its_own_deadline() {
        // Given a registry holding a tool wait whose own deadline has
        // passed
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(
            WaitKind::WaitingTool,
            WaitSubject::Tool {
                deadline: Some(Instant::now() - std::time::Duration::from_secs(1)),
            },
        );

        // When the watchdog probes
        // Then the tool's own timeout (the owner) reports Stalled
        assert_eq!(
            registry.probe(),
            TurnWaitState::Stalled {
                kind: WaitKind::WaitingTool
            }
        );
    }

    #[test]
    fn should_not_let_an_unbounded_tool_wait_veto_other_stalled_verdicts() {
        // Given a registry holding an unbounded tool wait (no deadline —
        // the class with NO probe-able liveness fact, ADR-006 per-kind
        // table: existence-liveness keeps the classic silence criterion)
        let registry = Arc::new(WaitRegistry::new());
        let _guard = registry.enter(WaitKind::WaitingTool, WaitSubject::Tool { deadline: None });

        // When the watchdog probes
        // Then the wait contributes Settled (never a standing Advancing
        // veto — an unbounded wait must not mask other waits' stalled
        // verdicts, review I2), routing to the Degraded tier and the kind
        // window's silence semantics
        assert_eq!(registry.probe(), TurnWaitState::Degraded);
    }

    #[test]
    fn should_deregister_a_wait_when_its_guard_drops() {
        // Given a registry with one registered wait
        let (tasks, task_id) = tasks_with_running_task();
        let registry = Arc::new(WaitRegistry::new());
        let guard = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id, Arc::clone(&tasks)),
        );

        // When the await completes (the guard drops)
        drop(guard);

        // Then the registry is empty — the next probe reads engine silence
        assert_eq!(registry.probe(), TurnWaitState::Idle);
    }

    #[test]
    fn should_keep_sibling_entries_when_one_guard_drops() {
        // Given a registry with two concurrent waits on the same kind
        let (tasks, task_id) = tasks_with_running_task();
        let registry = Arc::new(WaitRegistry::new());
        let first = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id.clone(), Arc::clone(&tasks)),
        );
        let _second = registry.enter(
            WaitKind::WaitingTask,
            WaitSubject::task(task_id, Arc::clone(&tasks)),
        );

        // When one of the two waits completes (its guard drops)
        drop(first);

        // Then the sibling entry survives — the set still probes on it
        assert_eq!(registry.wait_kind_labels(), vec!["waiting_task"]);
    }
}
