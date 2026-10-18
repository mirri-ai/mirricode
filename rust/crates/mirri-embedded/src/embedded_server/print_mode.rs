//! Print-mode background strategy (B1 T8, D-59): what `mirri print` does
//! with still-running background tasks when the main turn completes.
//!
//! Mirrors the TS `printModeService.ts` / `configSection.ts` semantics:
//! - mode resolution: an explicit `task.printBackgroundMode` wins; otherwise
//!   `keepAliveOnExit ? drain : steer`;
//! - `exit`: finish immediately — no waiting;
//! - `drain`: wait (bounded by `task.printWaitCeilingS`) for every task to
//!   reach a terminal status, then finish;
//! - `steer` (the default): keep the print loop alive while tasks run,
//!   capped by `task.printMaxTurns` (decision count) and the wait ceiling —
//!   whichever trips first finishes.
//!
//! Soft-bound caveat (documented TS deviation): each
//! `session.waitForBackgroundTasksOnPrint` call re-arms a fresh full
//! ceiling, so a steer loop can outlive the steer deadline by up to one
//! ceiling per continue-cycle; the deadline bounds the decision cadence,
//! not the inter-decision waits. TS parity is approximate the same way.
//!
//! Both RPCs this module implements (`session.waitForBackgroundTasksOnPrint`
//! and `session.handlePrintMainTurnCompleted`) were already registered in the
//! method registry — this is the implementation, not a contract addition.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mirri_core::config::{PrintBackgroundMode, ServerConfig};
use serde_json::json;

use super::EmbeddedServer;
use super::session_routing::resolve_session_via;

/// How often the drain wait polls the task registry.
const PRINT_DRAIN_POLL_MS: u64 = 100;

/// The default print wait ceiling: one year (`printDefaults.ts`).
const DEFAULT_PRINT_WAIT_CEILING_S: u64 = 315_360_000;

/// The default steer turn cap (`printDefaults.ts`).
const DEFAULT_PRINT_MAX_TURNS: u64 = 100_000;

/// The resolved print-mode policy for one server config.
#[derive(Debug, Clone)]
pub(super) struct PrintModePolicy {
    pub mode: PrintBackgroundMode,
    pub wait_ceiling: Duration,
    pub max_turns: u64,
}

/// Resolve the print-mode policy from the server config
/// (`configSection.ts:50-62`): the explicit `printBackgroundMode` wins;
/// absent that, `keepAliveOnExit` picks `drain`, the default is `steer`.
pub(super) fn print_mode_policy_from(server_config: &ServerConfig) -> PrintModePolicy {
    let task = server_config.resolve_task_config();
    let mode = task.print_background_mode.unwrap_or({
        if task.keep_alive_on_exit.unwrap_or(false) {
            PrintBackgroundMode::Drain
        } else {
            PrintBackgroundMode::Steer
        }
    });
    PrintModePolicy {
        mode,
        wait_ceiling: Duration::from_secs(
            task.print_wait_ceiling_s
                .unwrap_or(DEFAULT_PRINT_WAIT_CEILING_S),
        ),
        max_turns: task.print_max_turns.unwrap_or(DEFAULT_PRINT_MAX_TURNS),
    }
}

/// The per-session steer bookkeeping: how many `continue` decisions the
/// session has consumed and when its wait-ceiling clock started (the first
/// decision). In-memory only — nothing here is wire-persisted.
#[derive(Debug, Default)]
pub(super) struct PrintSteerState {
    turns_used: u64,
    first_decision: Option<Instant>,
}

impl EmbeddedServer {
    /// Drop a session's steer bookkeeping — called when the print loop
    /// finishes for it (its budget is spent) and when the session closes
    /// or archives (a same-id resume must not inherit the old budget).
    pub(super) fn forget_print_state(&self, session_id: &str) {
        self.print_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// `session.waitForBackgroundTasksOnPrint` — wait (bounded) for every
    /// background task of the session to reach a terminal status.
    ///
    /// Returns `{ waited, remaining }`: `waited` is `true` when the call
    /// actually had to wait (tasks were still active on entry), `remaining`
    /// is the still-running count when the call returned — zero on a
    /// settled drain, non-zero when the ceiling expired first.
    pub(super) async fn dispatch_wait_for_background_tasks_on_print(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, mirri_transport::adapter::EngineError> {
        let session_id = print_session_id(&params)?;
        let policy = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            print_mode_policy_from(&snapshot)
        };
        let session = resolve_session_via(&self.registry, &session_id)
            .await
            .map_err(|_| super::invalid_params(format!("session '{session_id}' not found")))?;
        let tasks = session.tasks();
        let waited = tasks.list().iter().any(|task| !task.status.is_terminal());
        let remaining = wait_for_settlement(&tasks, policy.wait_ceiling).await;
        Ok(json!({ "waited": waited, "remaining": remaining }))
    }

    /// `session.handlePrintMainTurnCompleted` — the post-turn decision of
    /// the print loop: `finish` (exit the print process) or `continue`
    /// (keep waiting for / steering on background work).
    pub(super) async fn dispatch_handle_print_main_turn_completed(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, mirri_transport::adapter::EngineError> {
        let session_id = print_session_id(&params)?;
        let policy = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            print_mode_policy_from(&snapshot)
        };
        let session = resolve_session_via(&self.registry, &session_id)
            .await
            .map_err(|_| super::invalid_params(format!("session '{session_id}' not found")))?;
        let tasks = session.tasks();
        let active = tasks
            .list()
            .iter()
            .filter(|task| !task.status.is_terminal())
            .count();

        let decision = match policy.mode {
            PrintBackgroundMode::Exit => "finish",
            PrintBackgroundMode::Drain => {
                // Drain: let the tasks settle (bounded by the ceiling), then
                // finish — a settled drain and an expired ceiling both end
                // the print process. The still-active tasks' terminal
                // notifications are suppressed first (TS printModeService
                // parity — it suppresses exactly the active set): the print
                // process is exiting, so a killed-notification would fire
                // into a dead loop. Scope boundary: a task that reached
                // terminal BEFORE the drain decision keeps its notification
                // (TS caliber — only the active set is suppressed).
                tasks.suppress_all_active();
                let _remaining = wait_for_settlement(&tasks, policy.wait_ceiling).await;
                "finish"
            }
            PrintBackgroundMode::Steer => {
                if active == 0 {
                    self.forget_print_state(&session_id);
                    "finish"
                } else {
                    let mut states = self.print_states.lock().unwrap_or_else(|e| e.into_inner());
                    let state = states.entry(session_id.clone()).or_default();
                    let now = Instant::now();
                    let started = *state.first_decision.get_or_insert(now);
                    state.turns_used += 1;
                    // Either cap trips — the steer turn count or the wait
                    // ceiling — and the print loop finishes. The entry is
                    // dropped with the finish: a later steer print for the
                    // same session starts a fresh budget.
                    if state.turns_used > policy.max_turns
                        || now.duration_since(started) >= policy.wait_ceiling
                    {
                        states.remove(&session_id);
                        "finish"
                    } else {
                        "continue"
                    }
                }
            }
        };
        Ok(json!({ "decision": decision }))
    }
}

/// Poll the task registry until every task is terminal or `ceiling`
/// elapses; returns the still-running count.
async fn wait_for_settlement(
    tasks: &Arc<mirri_core::task::TaskManager>,
    ceiling: Duration,
) -> usize {
    let deadline = Instant::now() + ceiling;
    loop {
        let remaining = tasks
            .list()
            .iter()
            .filter(|task| !task.status.is_terminal())
            .count();
        if remaining == 0 || Instant::now() >= deadline {
            return remaining;
        }
        tokio::time::sleep(Duration::from_millis(PRINT_DRAIN_POLL_MS)).await;
    }
}

/// Extract the session id from either wire spelling.
fn print_session_id(
    params: &serde_json::Value,
) -> Result<String, mirri_transport::adapter::EngineError> {
    params
        .get("session_id")
        .or_else(|| params.get("sessionId"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| super::invalid_params("missing 'sessionId' parameter"))
}

/// The per-session print-steer state map an [`EmbeddedServer`] owns —
/// declared here next to its only consumer.
pub(super) type PrintSteerStates = Arc<Mutex<HashMap<String, PrintSteerState>>>;
