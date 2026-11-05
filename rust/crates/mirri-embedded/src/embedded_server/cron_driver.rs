//! The server-level cron scheduler (C.1 / ADR-003): the production caller
//! of [`CronManager::check_due`](mirri_core::cron::CronManager::check_due)
//! for EVERY live session of the server — one interval loop, spawned once
//! per server, replacing the retired per-session drivers (the check
//! clock moved up to the server; the injection path stays per live
//! session).
//!
//! Each tick walks the built workspaces' live sessions, runs `check_due`,
//! and turns every due fire into a **held** fire first
//! (write-before-inject, C.4): the fire is persisted to the session's
//! wire log (`cron.held`) before anything is injected, so a session that
//! closes with fires still waiting redelivers them on resume instead of
//! dropping the injections. Held fires are purged when their task was
//! deleted (#177), and drained oldest-first once the target agent is
//! idle — the injection itself (the canonical main agent, first-agent
//! fallback) is unchanged from the per-session driver era.
//!
//! The scheduler never exits: it is server-lifetime-resident (the
//! orphan-governance loop's shape), so one session closing never stops
//! the others' scheduling (V10).

use std::collections::HashMap;
use std::sync::Arc;

use mirri_core::{
    AgentId, CancelToken, InteractionKernel, PromptOrigin, SessionEvent, SteerBuffer,
    cron::format::render_cron_fire_envelope, event_projection::turn_stop_reason_wire,
};
use mirri_proto::WireOp;
use mirri_transport::adapter::EngineError;
use serde_json::json;

use super::additional_dirs::session_grant_sink;
use super::cron_run_spawner::WorkspaceRunSpawner;
use super::goal_driver::{
    ChainResources, TurnOutcome, maybe_launch_goal_chain, settle_goal_after_turn, turn_outcome,
};
use super::message_handlers::{EngineTurnError, run_engine_turn};
use super::session_index::note_turn_settled;
use super::session_routing::{resolve_artifacts_via, resolve_session_via};
use super::{
    EmbeddedServer, InFlightTurn, InFlightTurnsMap, WaitTurnGuard, coerce_aborted_if_cancelled,
};
use crate::LlmFactory;
use crate::workspace_registry::WorkspaceRegistry;
use mirri_session_index::SessionIndex;

/// The embedded server's per-agent turn-flow map (the same shape
/// [`EmbeddedServer::turn_flows`](super::EmbeddedServer) owns).
pub(crate) type TurnFlowMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<mirri_core::TurnFlow>>>>>;

/// The embedded server's per-agent activity-tracker map.
pub(crate) type ActivityTrackerMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<mirri_core::turn_flow::ActivityTracker>>>>;

/// The embedded server's per-agent compaction-handle map (the same shape
/// [`EmbeddedServer::compaction_handles`](super::EmbeddedServer) owns) —
/// registered here so the driver-created flows answer the mid-turn model
/// switch's window push.
pub(crate) type CompactionHandleMap = Arc<
    std::sync::Mutex<
        HashMap<String, Arc<std::sync::Mutex<mirri_core::compaction::FullCompaction>>>,
    >,
>;

/// The embedded server's per-agent usage-model-key-handle map (the same
/// shape [`EmbeddedServer::usage_model_key_handles`](super::EmbeddedServer)
/// owns) — registered here so the driver-created flows answer the mid-turn
/// model switch's usage re-attribution (spec 11 A13).
pub(crate) type UsageModelKeyHandleMap =
    Arc<std::sync::Mutex<HashMap<String, super::UsageModelKeyHandle>>>;

/// The executor face a workspace cron fire is handed to (stage-2 T5 /
/// batch 4b): the **RunSpawner** — the real execution pipeline (the §8
/// steps: concurrency posture, the lazy execution session, the posture-
/// mapped injection, the terminal RunRecord). Batch 4a shipped a skip-
/// recording placeholder here; this trait remains the scheduler's seam
/// (the spawner needs no `&EmbeddedServer` at trait level — the embedded
/// server's implementation is `spawn_workspace_run`).
pub trait WorkspaceFireExecutor: Send + Sync {
    /// Execute one fired workspace task: record `Running` (or the
    /// concurrency skip) and return — the turn runs fire-and-forget (R1).
    /// `trigger` / `scheduled_at` become the run's launch record
    /// (ADR-021): what started it, and the moment the schedule said it
    /// was due (`None` on run-now fires).
    fn execute(
        &self,
        workspace_id: &str,
        task_id: &str,
        trigger: mirri_core::cron::RunTrigger,
        scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    );
}

/// The one-shot latch for the stale-Running orphan sweep (M2): armed on
/// the scheduler's first workspace pass — every Running record the fresh
/// scheduler reads is a crash orphan (an execution turn cannot survive
/// a restart), so the first pass converges them to Failed. The latch is
/// per-SCHEDULER (spawned once per server via `maybe_spawn_cron_scheduler`),
/// NOT per-manager: the second manager face of the same workspace (or a
/// test server over the same home) must not re-sweep and kill the live
/// runs the first face opened — but a NEW server process (a restart) gets
/// its own scheduler, and its own first pass converges the previous
/// process's orphans.
type OrphanSweepLatch = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// The workspace arm of one scheduler tick (T5): every BUILT runtime's
/// workspace cron manager runs `check_due`; each returned fire is handed
/// to the [`WorkspaceFireExecutor`]. (The stage-2 4b proposal TTL sweep
/// and the proposal-receipt delivery retired with C2 — the create-decide
/// channel parks its result notices at decide time, off this pass.)
/// Spawned per workspace on its own task — the
/// per-session M2 panic isolation posture applies to the workspace faces
/// identically (a panic in one workspace's pass never takes the loop, or
/// the other faces, down).
///
/// B1 (the blocking-store seam): every store mutation the arm performs
/// (`check_due`'s read-transform-write, `record_run`'s append) rides a
/// `spawn_blocking` — the file lock is a `create_new` + 50 ms polling
/// loop, which must never spin on the async runtime's worker thread.
#[allow(clippy::too_many_arguments)]
async fn schedule_workspace_cron_pass(
    registry: Arc<WorkspaceRegistry>,
    executor: Arc<dyn WorkspaceFireExecutor>,
    _steer_buffers: SteerBufferMap,
    workspace_id: String,
    orphan_swept: OrphanSweepLatch,
) {
    let Some(runtime) = registry.built_runtime(&workspace_id) else {
        return;
    };
    let Some(_cron) = runtime.cron() else {
        // A runtime without a home-rooted bucket (no workspace store) has
        // no workspace face — nothing to schedule.
        return;
    };
    // M2 (orphan convergence): on THIS scheduler's first workspace pass,
    // every `Running` run record is a crash orphan — an execution turn
    // cannot survive a server restart, so a Running record read by a
    // fresh process is definitionally dead. The one-shot per-scheduler
    // sweep (the `orphan_swept` latch — NOT per-manager: two manager
    // faces of one workspace coexist in one process, and a per-manager
    // sweep would kill the other face's live run) flips every Running
    // record to Failed{reason: 服务重启中断} and clears the task's stuck
    // posture (a Skip/Queue task whose only record was the dead Running
    // would otherwise be wedged forever).
    if orphan_swept
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        let cron = std::sync::Arc::clone(_cron);
        let swept = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(cron.fail_orphan_running_runs("服务重启中断"))
        })
        .await
        .unwrap_or_else(|join_error| {
            tracing::warn!(error = %join_error, "the stale-Running sweep panicked");
            Vec::new()
        });
        if !swept.is_empty() {
            tracing::info!(
                count = swept.len(),
                workspace_id,
                "converged stale Running records left by the previous process"
            );
        }
    }
    // B1: the due check is one locked store write    // B1: the due check is one locked store write — take it off the
    // async worker before the blocking file-lock polling.
    let fires = {
        let cron = std::sync::Arc::clone(_cron);
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(cron.check_due())
        })
        .await
        .unwrap_or_else(|join_error| {
            tracing::warn!(
                error = %join_error,
                "workspace cron due check panicked; the pass is skipped"
            );
            Vec::new()
        })
    };
    for fire in fires {
        // 4b's RunSpawner: the executor records Running (or the
        // concurrency Skip) and spawns the execution turn — the fire's
        // cursor/finale transitions already advanced durably inside
        // check_due. No second skip record here (the spawner owns every
        // RunRecord of the fire).
        executor.execute(
            &workspace_id,
            &fire.task_id,
            mirri_core::cron::RunTrigger::Scheduled,
            fire.scheduled_at,
        );
    }
    // 4b (D-M5 b / §8.2 / ADR-012)    // 4b (D-M5 b / §8.2 / ADR-012): the unopened execution sessions whose
    // runs ended more than 7 days ago drop from the workspace's LIVE
    // session map — the RunRecord keeps the summary (the summary stays
    // queryable through `cron.runs.list`; only the process-lifetime
    // session object goes). A 7-day-old unopened lazy session never
    // materialized — nothing is on disk to clean (the discard is the
    // memory drop).
    sweep_unopened_execution_sessions(&registry, _cron).await;
}

/// The D-M5 b-layer sweep: close (drop from the live map) every workspace
/// session that (1) an execution run referenced, (2) never materialized
/// (no wire log — the unopened lazy shape), and (3) whose referencing run
/// reached its terminal state more than
/// [`UNOPENED_RUN_SESSION_TTL`](super::cron_run_spawner::UNOPENED_RUN_SESSION_TTL)
/// ago. The run record itself is retained (the audit trail); only the
/// in-memory session object is reclaimed.
async fn sweep_unopened_execution_sessions(
    registry: &Arc<WorkspaceRegistry>,
    cron: &Arc<mirri_core::cron::WorkspaceCronManager>,
) {
    let horizon = chrono::Utc::now() - super::cron_run_spawner::UNOPENED_RUN_SESSION_TTL;
    // The sessions to drop: referenced by a terminal run older than the
    // horizon, still live, still unmaterialized.
    let mut drop_ids: Vec<String> = Vec::new();
    for task in cron.list() {
        for run in cron.runs(&task.id) {
            let Some(ended_at) = run.ended_at else {
                continue;
            };
            if ended_at > horizon {
                continue;
            }
            if let Some(session_ref) = &run.session_ref
                && !drop_ids.contains(session_ref)
            {
                drop_ids.push(session_ref.clone());
            }
        }
    }
    if drop_ids.is_empty() {
        return;
    }
    let Some(runtime) = registry.built_runtime(cron.workspace_id()) else {
        return;
    };
    for session_id in drop_ids {
        let sid = mirri_core::SessionId::new(&session_id);
        let Some(session) = runtime.manager.get_session(&sid).await else {
            continue;
        };
        // Only the never-materialized drop: an opened (materialized)
        // session is a real session — the a-layer retention domain owns
        // it, never this sweep.
        if session.wire_log_installed().await {
            continue;
        }
        let _ = runtime
            .manager
            .close_session(
                &sid,
                mirri_core::CloseReason::Error(
                    "unopened execution session past the 7-day retention".to_string(),
                ),
            )
            .await;
        // The session no longer exists: drop its routing entry too (the
        // spawner noted it active so a parked approval could route to it).
        registry.forget_active(&session_id);
        tracing::debug!(
            session_id,
            "dropped an unopened execution session past the 7-day retention"
        );
    }
}

impl EmbeddedServer {
    /// `cron.trigger` — run one cron task *now* (B6 management run-now
    /// face, D-2): the task's prompt is injected as an engine turn
    /// immediately, with the cron-job origin, through the same injection
    /// the driver applies — and the schedule is left untouched (no cursor
    /// advance, no reschedule, no one-shot consumption; the natural fire
    /// still happens per the expression). Fire-and-forget: the injection
    /// runs on its own task, and the RPC answers as soon as the fire is
    /// accepted (the driver's own posture). Unknown ids answer not_found.
    ///
    /// `not_found` is the blanket refusal for everything that cannot fire:
    /// an unknown session, an unknown task id, an agent-less session (no
    /// agent to run the prompt in — resolved before the fire so the event
    /// is not burned on an injection that would drop), and the
    /// teardown-window case of a manager already shut down (the session is
    /// going away; the driver's own tick would skip it the same way).
    ///
    /// The delete counterpart (`cron.delete`) lives in
    /// [`session_handlers`](super::session_handlers): it has no driver
    /// dependencies, while this handler sits next to the
    /// [`inject_cron_prompt`] pipeline it reuses.
    pub(super) async fn dispatch_trigger_cron_task(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, EngineError> {
        #[derive(Debug, serde::Deserialize)]
        struct CronTriggerParams {
            #[serde(alias = "sessionId")]
            #[serde(default)]
            session_id: Option<String>,
            /// The workspace the task lives in (stage-2 T8): carrying it
            /// routes the manual run to the workspace cron manager (the
            /// run-now face of the workspace arm — 4a records the same
            /// Skipped RunRecord the scheduled fires produce; the real
            /// spawned run lands with 4b's executor).
            #[serde(default, alias = "workspaceId")]
            workspace_id: Option<String>,
            #[serde(alias = "taskId")]
            task_id: String,
        }
        let params: CronTriggerParams = serde_json::from_value(super::normalize_params(params))
            .map_err(|e| super::invalid_params(e.to_string()))?;
        // The workspace arm (stage-2 T8 — C.1: the workspace task's
        // run-now goes through the real executor, 4b's RunSpawner): the
        // spawner's captured-Arcs shape — this handler HAS `&self`, so the
        // captures come straight off the server fields.
        if let Some(workspace_id) = params.workspace_id {
            let runtime = self
                .registry
                .built_runtime(&workspace_id)
                .ok_or_else(|| super::not_found(format!("workspace not found: {workspace_id}")))?;
            let cron = runtime
                .cron()
                .ok_or_else(|| super::not_found(format!("workspace not found: {workspace_id}")))?;
            let Some(_task) = cron.get(&params.task_id) else {
                return Err(super::not_found(format!(
                    "cron task not found: {}",
                    params.task_id
                )));
            };
            // 4b's RunSpawner (the real executor): records Running (or
            // the concurrency skip) and spawns the execution turn — the
            // same fire-and-forget posture the dispatch answers with.
            let spawner = self.workspace_run_spawner();
            spawner.execute(&workspace_id, &params.task_id, mirri_core::cron::RunTrigger::RunNow, None);
            self.emit_server_event(mirri_core::ServerEvent::CronWorkspaceChanged { workspace_id });
            return super::responses::to_result(super::responses::CronTriggerResult {
                triggered: true,
                task_id: params.task_id,
                held: Some(true),
            });
        }
        let Some(session_id) = params.session_id.clone() else {
            return Err(super::invalid_params(
                "cron.trigger requires sessionId or workspaceId",
            ));
        };
        let session = resolve_session_via(&self.registry, &session_id)
            .await
            .map_err(|_| super::not_found(format!("session not found: {session_id}")))?;
        // The injection targets the session's canonical main agent — the
        // same pin the driver uses (first-agent fallback for
        // agent-less-provisioned sessions). Resolved before the fire so
        // an agent-less session fails honestly instead of burning a fire
        // whose injection would drop.
        let target_agent_id = match session
            .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
            .await
        {
            Some(_) => super::DEFAULT_MAIN_AGENT_ID.to_string(),
            None => match session.list_agents().await.into_iter().next() {
                Some(agent_id) => agent_id.as_str().to_string(),
                None => {
                    return Err(super::not_found(format!(
                        "no agent to run the cron task in: {session_id}"
                    )));
                }
            },
        };
        // The event publish happens inside trigger() (under the task-table
        // lock, the same place check_due announces); only the injection is
        // deferred to the spawned task.
        let Some(fire) = session.cron().trigger(&params.task_id) else {
            return Err(super::not_found(format!(
                "cron task not found: {}",
                params.task_id
            )));
        };
        // C.3 / SDR-004 (trigger idle gate): a manual run-now obeys the
        // The run's ask rule (batch 2 / A4, simplified by batch 4): every
        // task carries a declared mode, so the mode alone decides — see
        // [`fire_reject_asks_for`].
        let fire_is_workspace = fire.is_workspace();
        let fire_reject_asks = fire_reject_asks_for(&session, &fire);
        // same busy discipline a natural fire does — while the target
        // agent's turn runs, the fire is held on the task side (the same
        // held channel the scheduler drains) instead of barging into the
        // running turn. The schedule itself is untouched (run-now never
        // moves the cursor — D-2 holds).
        let target_key = format!("{}:{}", session_id, target_agent_id);
        let busy = self
            .turn_flows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target_key)
            .is_some_and(|flow| flow.try_lock().is_err());
        if busy {
            session.cron().hold_fire(fire).await;
            // The ack promises the run: a watcher outside the busy turn's
            // lifetime redelivers it when the flow frees (see
            // [`spawn_held_redelivery`] — the abort of the very turn the
            // fire queued behind must not strand it).
            spawn_held_redelivery(
                Arc::clone(&self.registry),
                Arc::clone(&self.turn_flows),
                Arc::clone(&self.activity_trackers),
                Arc::clone(&self.steer_buffers),
                Arc::clone(&self.compaction_handles),
                Arc::clone(&self.usage_model_key_handles),
                self.llm_factory.clone(),
                Arc::clone(&self.in_flight_turns),
                self.session_index(),
                Arc::clone(self.flag_registry()),
                Arc::clone(&self.goal_chain_states),
                self.goal_chain_resources(),
                Arc::clone(&self.provider_auth_failures),
                Arc::clone(&self.config),
                self.turn_stall_timeout,
                self.turn_backstop_timeout,
                self.home_dir().map(ToOwned::to_owned),
                session_id.clone(),
                target_agent_id.clone(),
            );
            return super::responses::to_result(super::responses::CronTriggerResult {
                triggered: true,
                task_id: params.task_id,
                // m2: the fire was QUEUED behind the running turn, not
                // delivered — the caller sees the held channel's posture.
                held: Some(true),
            });
        }
        let registry = Arc::clone(&self.registry);
        let turn_flows = Arc::clone(&self.turn_flows);
        let activity_trackers = Arc::clone(&self.activity_trackers);
        let steer_buffers = Arc::clone(&self.steer_buffers);
        let compaction_handles = Arc::clone(&self.compaction_handles);
        let usage_model_key_handles = Arc::clone(&self.usage_model_key_handles);
        let llm_factory = self.llm_factory.clone();
        let server_config = Arc::clone(&self.config);
        let turn_stall_timeout = self.turn_stall_timeout;
        let turn_backstop_timeout = self.turn_backstop_timeout;
        // The cron-injected turn registers in the shared in-flight registry
        // (the `message.abort` path), keyed like the dispatch turns.
        let in_flight_turns: InFlightTurnsMap = Arc::clone(&self.in_flight_turns);
        // The turn-settle write-through (ADR-009): the injection runs on its
        // own task with no `&EmbeddedServer` — the shared index travels with
        // it so the injected turn's settle can refresh the session.
        let settle_index = self.session_index();
        let flag_registry = Arc::clone(self.flag_registry());
        // The goal continuation decision runs after the injected turn's
        // settle — the spawned task holds no `&EmbeddedServer`, so the
        // chain-state map travels with it as a cloned field Arc.
        let goal_chain_states = Arc::clone(&self.goal_chain_states);
        // The chain's injection resources, snapshotted before the spawn
        // (the spawned task holds no `&EmbeddedServer` — the field Arcs
        // travel with it, the same stance as the captures above).
        let goal_chain_resources = self.goal_chain_resources();
        let tool_result_home = self.home_dir().map(ToOwned::to_owned);
        // O-5 surfaced-failure wiring (D.5): the trigger task holds no
        // `&EmbeddedServer` — the shared auth-failure map travels with it as
        // a cloned field Arc (the same stance as the settle captures above).
        let provider_auth_failures = Arc::clone(&self.provider_auth_failures);
        let session_id = session_id.clone();
        tokio::spawn(async move {
            // Re-resolve inside the task (the driver's posture): the config
            // read and the injection both address the live session.
            let Ok(session) = resolve_session_via(&registry, &session_id).await else {
                return;
            };
            let flow_config = cron_flow_config(&session, &target_agent_id, &server_config).await;
            inject_cron_prompt(
                &registry,
                &turn_flows,
                &activity_trackers,
                &steer_buffers,
                &compaction_handles,
                &usage_model_key_handles,
                &llm_factory,
                &in_flight_turns,
                settle_index,
                &flag_registry,
                &goal_chain_states,
                &goal_chain_resources,
                &provider_auth_failures,
                &session_id,
                &target_agent_id,
                flow_config,
                fire,
                // The run-now face fires under the task's own declared
                // mode — see [`fire_reject_asks_for`].
                fire_reject_asks,
                // A session task's fire lands in a session the user
                // watches; a workspace task's run does not.
                fire_is_workspace,
                turn_stall_timeout,
                turn_backstop_timeout,
                tool_result_home,
            )
            .await;
        });
        super::responses::to_result(super::responses::CronTriggerResult {
            triggered: true,
            task_id: params.task_id,
            // m2: an idle direct inject carries no `held` key — the fire
            // was handed to the spawned injection task ("delivered", not
            // "queued").
            held: None,
        })
    }

    /// Lazily spawn the server-level cron scheduler exactly once per
    /// server (the `maybe_spawn_index_refresh_driver` pattern — the first
    /// `call_method` never precedes the Tokio runtime).
    pub(super) fn maybe_spawn_cron_scheduler(&self) {
        use std::sync::atomic::Ordering;
        // ADR-006 / E.4 layer 2: a disabled cron subsystem never enters
        // the check loop — existing tasks stay listed (unchanged,
        // unpruned) but nothing fires until the server restarts without
        // the switch. The manual run-now face (`cron.trigger`) is NOT
        // gated here: it is a management operation, not scheduled
        // delivery.
        if self.cron_scheduling_disabled {
            tracing::info!(
                "cron scheduling disabled via MIRRICODE_DISABLE_CRON — scheduler not started"
            );
            return;
        }
        if self.cron_scheduler_spawned.swap(true, Ordering::SeqCst) {
            return;
        }
        self.spawn_cron_scheduler();
    }

    /// The scheduler body: one Tokio task ticking every
    /// [`EmbeddedServer::with_cron_tick`] interval (the production default
    /// is [`CRON_SCHEDULER_TICK_SECS`](super::CRON_SCHEDULER_TICK_SECS)).
    ///
    /// The loop holds no `&EmbeddedServer` (the orphan-governance shape):
    /// the workspace registry and the injection field Arcs travel along as
    /// captured values, so every tick re-resolves the live sessions
    /// through the registry — a session that closed simply stops being
    /// visited (its held fires stay durable in the wire log for the
    /// resume), and the other sessions keep being scheduled (V10).
    fn spawn_cron_scheduler(&self) {
        // ADR-003 c: the scheduler's first tick does not run behind the
        // startup housekeeping — a cron-injected turn is a WRITE path
        // outside the RPC surface, so the route classes cannot hold it. It
        // waits for the census (a turn's tools may register tasks the census
        // would otherwise misread as orphans — ADR-012 c) and for the
        // recovery check (a turn writing into a recovery CANDIDATE session
        // before the check settles would look like the previous instance's
        // interrupted turn; the check's pre-write re-read cannot save that,
        // because the write is this instance's own live turn).
        let housekeeping = self.housekeeping_settled();
        let registry = Arc::clone(&self.registry);
        let turn_flows = Arc::clone(&self.turn_flows);
        let activity_trackers = Arc::clone(&self.activity_trackers);
        let steer_buffers = Arc::clone(&self.steer_buffers);
        let compaction_handles = Arc::clone(&self.compaction_handles);
        let usage_model_key_handles = Arc::clone(&self.usage_model_key_handles);
        let llm_factory = self.llm_factory.clone();
        let server_config = Arc::clone(&self.config);
        let profile_manager = self.profile_manager.clone();
        let turn_stall_timeout = self.turn_stall_timeout;
        let turn_backstop_timeout = self.turn_backstop_timeout;
        // The cron-injected turns register in the shared in-flight registry
        // (the `message.abort` path), keyed like the dispatch turns.
        let in_flight_turns: InFlightTurnsMap = Arc::clone(&self.in_flight_turns);
        let tick = self.cron_tick;
        // The turn-settle write-through (ADR-009): the scheduler task
        // holds no `&EmbeddedServer` — the shared index travels with it
        // so each injected turn's settle can refresh the session.
        let settle_index = self.session_index();
        let flag_registry = Arc::clone(self.flag_registry());
        // The goal continuation decision runs after each cron turn's
        // settle — the scheduler task holds no `&EmbeddedServer`, so the
        // chain-state map travels with it as a cloned field Arc.
        let goal_chain_states = Arc::clone(&self.goal_chain_states);
        // The chain's injection resources, snapshotted before the spawn
        // (the scheduler task holds no `&EmbeddedServer` — the field Arcs
        // travel with it, the same stance as the captures above).
        let goal_chain_resources = self.goal_chain_resources();
        let tool_result_home = self.home_dir().map(ToOwned::to_owned);
        // O-5 surfaced-failure wiring (D.5): the scheduler task holds no
        // `&EmbeddedServer` — the shared auth-failure map travels with it
        // as a cloned field Arc (the same stance as the settle captures).
        let provider_auth_failures = Arc::clone(&self.provider_auth_failures);
        // The M2 panic seam travels along as the shared live slot (tests
        // arm it any time — the first RPC already latched the scheduler).
        #[cfg(test)]
        let panic_session = Arc::clone(&self.cron_pass_panic_session);
        // 4b's RunSpawner (the real workspace executor): the same captured
        // Arcs the session passes take — the spawner needs no `&self`.
        let run_spawner: Arc<dyn WorkspaceFireExecutor> = Arc::new(WorkspaceRunSpawner {
            registry: Arc::clone(&registry),
            turn_flows: Arc::clone(&turn_flows),
            activity_trackers: Arc::clone(&activity_trackers),
            steer_buffers: Arc::clone(&steer_buffers),
            compaction_handles: Arc::clone(&compaction_handles),
            usage_model_key_handles: Arc::clone(&usage_model_key_handles),
            llm_factory: llm_factory.clone(),
            in_flight_turns: Arc::clone(&in_flight_turns),
            flag_registry: Arc::clone(&flag_registry),
            goal_chain_states: Arc::clone(&goal_chain_states),
            goal_chain_resources: goal_chain_resources.clone(),
            provider_auth_failures: Arc::clone(&provider_auth_failures),
            server_config: Arc::clone(&server_config),
            profile_manager: profile_manager.clone(),
            turn_stall_timeout,
            turn_backstop_timeout,
            tool_result_home: tool_result_home.clone(),
        });
        // The M2 orphan-sweep latch (per-scheduler): the first workspace
        // pass converges the previous process's stale Running records.
        let orphan_swept: OrphanSweepLatch =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            // The startup housekeeping settles first (ADR-003 c) — a
            // WAIT-ONLY hold: these cells are initialized solely by the
            // settlement paths themselves (the dispatch gates and
            // await_settled test seam), never here — a `get_or_init` with
            // a no-op would claim the cell and swallow the real census
            // and recovery bodies when the scheduler wins the race.
            housekeeping.await_settled(&housekeeping.census).await;
            housekeeping.await_settled(&housekeeping.recovery).await;
            let mut interval = tokio::time::interval(tick);
            // The first tick completes immediately; a due task fires on
            // the second — acceptable for a minute-granularity scheduler.
            loop {
                interval.tick().await;
                // One pass: every built workspace's live sessions, each
                // visited through the registry (the W3 seam — a
                // non-default workspace's sessions are scheduled too).
                for runtime in registry.built_runtimes() {
                    // The workspace arm (stage-2 T5): one pass per built
                    // runtime's workspace face, on its own spawned task —
                    // the same M2 panic posture the per-session passes
                    // use (a workspace panic never strands the loop).
                    // B1: NOT awaited inline — an inline await would
                    // serialize every workspace face behind one
                    // another's blocking store writes and stall the
                    // session passes behind them (a spawn-then-await is
                    // no parallelism at all). Instead each tick's
                    // workspace handles are collected and joined at the
                    // END of the tick: the session passes below run
                    // while the workspace passes work, and a workspace
                    // panic still surfaces as a JoinError at the join —
                    // never an unwind of the loop.
                    let mut workspace_handles = Vec::new();
                    if runtime.cron().is_some() {
                        workspace_handles.push(tokio::spawn(schedule_workspace_cron_pass(
                            Arc::clone(&registry),
                            // 4b's RunSpawner (the real executor).
                            Arc::clone(&run_spawner),
                            Arc::clone(&steer_buffers),
                            runtime.workspace_id.clone(),
                            Arc::clone(&orphan_swept),
                        )));
                    }
                    let session_ids = runtime.manager.workspace().list_sessions().await;
                    for session_id in session_ids {
                        let sid = session_id.as_str().to_string();
                        // M2 (panic isolation): each session's whole
                        // pass — the due checks, the holds, the drain —
                        // runs on its OWN spawned task and is awaited
                        // through the JoinHandle. A panic inside one
                        // session's injection (a bug in the engine turn
                        // path, say) surfaces as a JoinError here instead
                        // of unwinding the scheduler task itself: the
                        // loop logs it and moves to the NEXT session, so
                        // `cron_scheduler_spawned` (already latched)
                        // never strands the other sessions without a
                        // scheduler. The awaits inside the body are on
                        // the spawned task's JoinHandle, never inline.
                        let handle = tokio::spawn(schedule_session_cron_pass(
                            Arc::clone(&registry),
                            Arc::clone(&turn_flows),
                            Arc::clone(&activity_trackers),
                            Arc::clone(&steer_buffers),
                            Arc::clone(&compaction_handles),
                            Arc::clone(&usage_model_key_handles),
                            llm_factory.clone(),
                            Arc::clone(&in_flight_turns),
                            settle_index.clone(),
                            Arc::clone(&flag_registry),
                            Arc::clone(&goal_chain_states),
                            goal_chain_resources.clone(),
                            Arc::clone(&provider_auth_failures),
                            Arc::clone(&server_config),
                            turn_stall_timeout,
                            turn_backstop_timeout,
                            tool_result_home.clone(),
                            sid.clone(),
                            #[cfg(test)]
                            panic_session.clone(),
                        ));
                        if let Err(join_error) = handle.await {
                            tracing::warn!(
                                error = %join_error,
                                "cron scheduler: a session's pass panicked; the other \
                                 sessions keep being scheduled"
                            );
                        }
                    }
                    // B1 (end of tick): join the workspace passes spawned
                    // at the top of this runtime's iteration. The next
                    // tick's interval waits for the handles to settle
                    // (no unbounded pile-up — one outstanding batch per
                    // tick), and a workspace panic surfaces as a JoinError
                    // here instead of unwinding the loop.
                    for handle in workspace_handles {
                        if let Err(join_error) = handle.await {
                            tracing::warn!(
                                error = %join_error,
                                "cron scheduler: a workspace's pass panicked; the other \
                                 faces keep being scheduled"
                            );
                        }
                    }
                }
            }
        });
    }
}

/// One session's slice of a scheduler tick: `check_due` → hold → purge →
/// idle gate → drain (the per-session body the scheduler loop spawns so a
/// panic in one session cannot take the loop down — M2). Self-contained:
/// every dependency arrives as a cloned field Arc, the same captures the
/// scheduler task itself holds.
#[allow(clippy::too_many_arguments)]
async fn schedule_session_cron_pass(
    registry: Arc<WorkspaceRegistry>,
    turn_flows: TurnFlowMap,
    activity_trackers: ActivityTrackerMap,
    steer_buffers: SteerBufferMap,
    compaction_handles: CompactionHandleMap,
    usage_model_key_handles: UsageModelKeyHandleMap,
    llm_factory: Option<Arc<dyn LlmFactory>>,
    in_flight_turns: InFlightTurnsMap,
    settle_index: Option<Arc<SessionIndex>>,
    flag_registry: Arc<mirri_core::FlagRegistry>,
    goal_chain_states: super::goal_driver::GoalChainStateMap,
    goal_chain_resources: ChainResources,
    provider_auth_failures: Arc<std::sync::Mutex<HashMap<String, String>>>,
    server_config: Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    tool_result_home: Option<std::path::PathBuf>,
    sid: String,
    #[cfg(test)] panic_session: Arc<std::sync::Mutex<Option<String>>>,
) {
    // The M2 panic seam: a test-armed session id panics here, INSIDE the
    // spawned per-session body — the JoinHandle surfaces it to the loop's
    // warn-and-continue arm, never unwinding the scheduler itself.
    #[cfg(test)]
    if panic_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        == Some(sid.as_str())
    {
        panic!("test seam: the cron pass for {sid} panics");
    }
    let Ok(session) = resolve_session_via(&registry, &sid).await else {
        // The session closed between the listing and the resolve —
        // nothing to schedule here (its held fires stay durable for
        // the resume).
        return;
    };
    let cron = session.cron();
    // check_due publishes the `cron.fired` events on the session bus (the
    // manager's wired-in bus) and returns the fired prompts.
    // Write-before-inject (C.4): each fire is held durably on the task
    // side BEFORE any injection is attempted — a close race past this
    // point drops the injection at most once, never the fire.
    for fire in cron.check_due().await {
        cron.hold_fire(fire).await;
    }
    // Purge held fires whose task was deleted after firing (#177): a
    // delete during the busy hold cancels the pending injection.
    cron.purge_deleted_held().await;
    if cron.held_fires().is_empty() {
        return;
    }
    // The injection targets the session's canonical main agent (the
    // same pin the notification driver and the steer recording use —
    // the first-agent fallback covers agent-less provisioned
    // sessions).
    let target_agent_id = match session
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
    {
        Some(_) => super::DEFAULT_MAIN_AGENT_ID.to_string(),
        None => match session.list_agents().await.into_iter().next() {
            Some(agent_id) => agent_id.as_str().to_string(),
            // No agents at all: keep holding (the inject path would
            // drop the fire with a warning).
            None => return,
        },
    };
    // Idle-only delivery (TS sessionCronServiceImpl): while the target
    // agent's turn runs, hold — the flow mutex is held for the whole
    // turn by the engine loop, so lock contention is exactly "a turn is
    // running". Held fires survive the wait durably (C.4) — they
    // redeliver after the busy window, or after a close→resume.
    let busy = turn_flows
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&format!("{sid}:{target_agent_id}"))
        .is_some_and(|flow| flow.try_lock().is_err());
    if busy {
        return;
    }
    // Drain the held fires oldest-first (C.4): each successfully injected
    // turn settles its hold durably (`cron.held_clear`); a session that
    // closes mid-drain keeps the rest held for the resume's redelivery
    // (at-least-once).
    for fire in cron.held_fires() {
        // The turn-flow config is built at the dispatch site (the
        // spawned task owns the shared server config, not the server
        // itself): the cron target agent's model id, read from its
        // scope config, combines with the `[loop_control]` snapshot and
        // the model's catalog window — the dispatch-site equivalent of
        // `EmbeddedServer::turn_flow_config_for`.
        let flow_config = cron_flow_config(&session, &target_agent_id, &server_config).await;
        inject_cron_prompt(
            &registry,
            &turn_flows,
            &activity_trackers,
            &steer_buffers,
            &compaction_handles,
            &usage_model_key_handles,
            &llm_factory,
            &in_flight_turns,
            settle_index.clone(),
            &flag_registry,
            &goal_chain_states,
            &goal_chain_resources,
            &provider_auth_failures,
            &sid,
            &target_agent_id,
            flow_config,
            fire.clone(),
            // The drained fire's ask rule (batch 2 / A4): the same lookup
            // the run-now face uses — a task with a declared mode never
            // rejects, a legacy record keeps the rejection.
            fire_reject_asks_for(&session, &fire),
            fire.is_workspace(),
            turn_stall_timeout,
            turn_backstop_timeout,
            tool_result_home.clone(),
        )
        .await;
        // The injection turn settled (or never started) — clear the
        // hold (C.4's write-before-inject settle). M3: the early-exit
        // failures clear too — an injection that will never succeed
        // (agent vanished, no provider, provider build failed) must not
        // be retried forever on every tick (a bounded loss beats an
        // unbounded retry queue; the task's next natural fire rides the
        // normal schedule). The session is re-resolved so a close
        // mid-injection surfaces as "session gone": the fire stays held
        // and the resume redelivers (at-least-once).
        if let Ok(live) = resolve_session_via(&registry, &sid).await {
            live.cron().clear_held(&fire.task_id).await;
        }
    }
}

/// Settle ONE fire's injection, then release the session's held copy of it
/// (C.4's write-before-inject settle). The drain's per-fire body, split out
/// so the abort-time redelivery below can reuse it verbatim.
#[allow(clippy::too_many_arguments)]
async fn inject_and_clear_held(
    registry: &Arc<WorkspaceRegistry>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    llm_factory: &Option<Arc<dyn LlmFactory>>,
    in_flight_turns: &InFlightTurnsMap,
    index: Option<Arc<SessionIndex>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    goal_chain_states: &super::goal_driver::GoalChainStateMap,
    goal_chain_resources: &ChainResources,
    provider_auth_failures: &std::sync::Mutex<HashMap<String, String>>,
    server_config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    tool_result_home: Option<std::path::PathBuf>,
    sid: &str,
    target_agent_id: &str,
    fire: &mirri_core::cron::CronFire,
    session: &Arc<mirri_core::SessionScope>,
) {
    let flow_config = cron_flow_config(session, target_agent_id, server_config).await;
    inject_cron_prompt(
        registry,
        turn_flows,
        activity_trackers,
        steer_buffers,
        compaction_handles,
        usage_model_key_handles,
        llm_factory,
        in_flight_turns,
        index,
        flag_registry,
        goal_chain_states,
        goal_chain_resources,
        provider_auth_failures,
        sid,
        target_agent_id,
        flow_config,
        fire.clone(),
        fire_reject_asks_for(session, fire),
        // A session fire lands in a session the user watches; a
        // workspace task's run does not (the held drain covers session
        // tasks only).
        false,
        turn_stall_timeout,
        turn_backstop_timeout,
        tool_result_home,
    )
    .await;
    // The injection turn settled (or never started) — clear the hold
    // (C.4's write-before-inject settle). M3: the early-exit failures
    // clear too — an injection that will never succeed must not be
    // retried forever (a bounded loss beats an unbounded retry queue;
    // the task's next natural fire rides the normal schedule). The
    // session is re-resolved so a close mid-injection surfaces as
    // "session gone": the fire stays held and the resume redelivers
    // (at-least-once).
    if let Ok(live) = resolve_session_via(registry, sid).await {
        live.cron().clear_held(&fire.task_id).await;
    }
}

/// The session's run-now face when a turn is in flight: hold the fire and
/// WAIT — outside the in-flight turn — for it to land.
///
/// The run-now ack promises a run, but the fire's only releaser used to be
/// the scheduler tick, and a held fire outlives the turn it queued behind
/// only if the scheduler runs. Two ways the promise broke without one:
///
/// - the tick is not running at all (a server built without the scheduler
///   entering the loop — every test server);
/// - the queued-behind turn is HARD-ABORTED (a fire-mode `message.send`
///   whose task dies), and the released flow keeps its in-flight state, so
///   the agent is busy until the abort clears it.
///
/// The per-fire watcher below is attached to the FIRE, not to any turn: it
/// polls the agent's flow and, the first moment the flow admits it, drains
/// the holds present then (the same drain the scheduler's idle gate
/// performs) and exits. A fire survives every turn it waited behind.
///
/// One watcher per session (a repeated run-now while busy reuses it), and
/// bounded: a flow that never frees leaves the hold to the scheduler/tick
/// resume path and the watcher gives up after the turn backstop horizon —
/// a pathological session pays one watcher, never an unbounded retry loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_held_redelivery(
    registry: Arc<WorkspaceRegistry>,
    turn_flows: TurnFlowMap,
    activity_trackers: ActivityTrackerMap,
    steer_buffers: SteerBufferMap,
    compaction_handles: CompactionHandleMap,
    usage_model_key_handles: UsageModelKeyHandleMap,
    llm_factory: Option<Arc<dyn LlmFactory>>,
    in_flight_turns: InFlightTurnsMap,
    index: Option<Arc<SessionIndex>>,
    flag_registry: Arc<mirri_core::FlagRegistry>,
    goal_chain_states: super::goal_driver::GoalChainStateMap,
    goal_chain_resources: ChainResources,
    provider_auth_failures: Arc<std::sync::Mutex<HashMap<String, String>>>,
    server_config: Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    tool_result_home: Option<std::path::PathBuf>,
    sid: String,
    target_agent_id: String,
) {
    /// The give-up horizon: the longest a turn may legitimately stay
    /// in flight before its backstop reclaims it.
    const HELD_WAIT_CEILING: std::time::Duration = std::time::Duration::from_secs(3600);
    tokio::spawn(async move {
        let target_key = format!("{sid}:{target_agent_id}");
        // Fast path: the fire may have been held a moment before the turn
        // ended, so the first check may already admit it.
        let deadline = tokio::time::Instant::now() + HELD_WAIT_CEILING;
        loop {
            // "Idle" is decided by TAKING the flow for an instant: a turn
            // that holds it never yields it, so a successful try is proof
            // the agent is free right now (the guard drops immediately —
            // the drain below re-checks through the injection's own lock).
            let free = turn_flows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&target_key)
                .is_some_and(|flow| {
                    let Ok(guard) = flow.try_lock() else {
                        return false;
                    };
                    drop(guard);
                    true
                });
            if free {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %sid,
                    "cron held redelivery gave up: the agent's flow stayed busy \
                     for the whole wait horizon (the scheduler's idle tick \
                     redelivers the hold, or the session's resume does)"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // The session is re-resolved at drain time: its manager may have
        // been swapped by a close→resume while the fire waited.
        let Ok(session) = resolve_session_via(&registry, &sid).await else {
            return;
        };
        let cron = session.cron();
        // The same purge the scheduler performs: a task deleted while its
        // fire waited is not redelivered.
        cron.purge_deleted_held().await;
        for fire in cron.held_fires() {
            inject_and_clear_held(
                &registry,
                &turn_flows,
                &activity_trackers,
                &steer_buffers,
                &compaction_handles,
                &usage_model_key_handles,
                &llm_factory,
                &in_flight_turns,
                index.clone(),
                &flag_registry,
                &goal_chain_states,
                &goal_chain_resources,
                &provider_auth_failures,
                &server_config,
                turn_stall_timeout,
                turn_backstop_timeout,
                tool_result_home.clone(),
                &sid,
                &target_agent_id,
                &fire,
                &session,
            )
            .await;
        }
    });
}

/// The embedded server's per-agent shared-steer-buffer map (the same shape
/// [`EmbeddedServer::steer_buffers`](super::EmbeddedServer) owns).
pub(crate) type SteerBufferMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<SteerBuffer>>>>>;

/// The turn-flow config for the cron target agent: the model id is read
/// from the caller-resolved target agent's scope config (the driver pins
/// the canonical main agent, falling back to the session's first agent)
/// and combined with the server's `[loop_control]` snapshot and the
/// model's catalog window. An unresolvable session/agent yields the
/// unknown-window config — ratio triggers stay off until an overflow
/// teaches the engine the real limit (and the injection drops anyway).
pub(crate) async fn cron_flow_config(
    session: &mirri_core::SessionScope,
    target_agent_id: &str,
    server_config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
) -> mirri_core::TurnFlowConfig {
    let model_id = match session.get_agent(&AgentId::new(target_agent_id)).await {
        Some(agent) => agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone(),
        None => String::new(),
    };
    super::turn_flow_config_for_shared(server_config, &model_id)
}

/// Whether one cron injection actually started a turn (M3's reporting):
///
/// - [`InjectedStatus::Injected`] — the turn ran to its settle (success or
///   engine failure, either way the injection happened and the hold's
///   clear is the write-before-inject settle);
/// - [`InjectedStatus::Dropped`] — an early-exit failure (agent vanished,
///   no provider, provider build failed): the injection will NEVER succeed
///   on retry — the caller clears the hold instead (a bounded loss beats
///   an unbounded retry queue).
///
/// Distinct from a panic, which never returns (M2 isolates it at the
/// scheduler's per-session spawn boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InjectedStatus {
    Injected,
    Dropped,
    /// The injection landed but the turn ended in an engine error (the
    /// reason text) — the run did not complete (ADR-021's truthful
    /// success: the executor records Failed with this reason).
    TurnFailed(String),
}

/// Inject one fired cron prompt into the session's target agent (the
/// caller-resolved pin — canonical main agent, first-agent fallback) as an
/// engine turn — a lean mirror of `message.send`'s wait path with the
/// `cron_job` prompt origin (the TS kernel delivers cron prompts as
/// user-role context records carrying the cron-job origin, then runs the
/// agent turn).
///
/// Fire-and-forget and best-effort: a missing provider or agent logs a
/// warning and answers [`InjectedStatus::Dropped`] — the caller clears
/// the hold (M3: these failures are permanent per fire, an every-tick
/// retry would spin forever; the next NATURAL fire of the task rides its
/// normal schedule).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn inject_cron_prompt(
    registry: &Arc<WorkspaceRegistry>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    llm_factory: &Option<Arc<dyn LlmFactory>>,
    in_flight_turns: &InFlightTurnsMap,
    index: Option<Arc<SessionIndex>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    goal_chain_states: &super::goal_driver::GoalChainStateMap,
    goal_chain_resources: &super::goal_driver::ChainResources,
    provider_auth_failures: &std::sync::Mutex<HashMap<String, String>>,
    session_id: &str,
    target_agent_id: &str,
    flow_config: mirri_core::TurnFlowConfig,
    fire: mirri_core::cron::CronFire,
    // reject_asks: whether this fire's asks are rejected outright (the
    // legacy ask-deny posture). Batch 2 (A4): a `manual` task's asks park
    // on the approval gate instead; `yolo` never asks.
    reject_asks: bool,
    workspace_run: bool,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    tool_result_home: Option<std::path::PathBuf>,
) -> InjectedStatus {
    let Ok(session) = resolve_session_via(registry, session_id).await else {
        return InjectedStatus::Injected;
    };
    inject_cron_prompt_into_session(
        registry,
        session,
        turn_flows,
        activity_trackers,
        steer_buffers,
        compaction_handles,
        usage_model_key_handles,
        llm_factory,
        in_flight_turns,
        index,
        flag_registry,
        goal_chain_states,
        goal_chain_resources,
        provider_auth_failures,
        session_id,
        target_agent_id,
        flow_config,
        fire,
        reject_asks,
        workspace_run,
        turn_stall_timeout,
        turn_backstop_timeout,
        tool_result_home,
        // Session-timer fires have no RunRecord to flip (ADR-021).
        None,
    )
    .await
}

/// The ask rule of one session fire (batch 2 / A4; batch 4 retires the
/// legacy `unattended` deduction): a task that declares a mode never
/// rejects its asks — manual parks them on the approval gate, auto runs
/// the ordinary auto flow, yolo never asks. Only a record with NO
/// declared law keeps the pre-A4 ask-deny (its Manual mode would
/// otherwise park an ask nobody is around to answer).
fn fire_reject_asks_for(
    session: &Arc<mirri_core::SessionScope>,
    fire: &mirri_core::cron::CronFire,
) -> bool {
    session
        .cron()
        .get(&fire.task_id)
        .is_some_and(|task| task.permission_mode.is_none())
}

/// The shared injection body over an already-resolved session Arc —
/// the workspace run spawner's face (D.2 m4 review fix): the execution
/// session is created LAZY (SDR-004) and by design never enters the
/// server's routing tables (ADR-008 — only the explicit-open path admits
/// it), so the spawner CANNOT go through `resolve_session_via` (which
/// answers `Injected` for an unroutable session, the "redelivery is
/// pointless" arm — silently skipping the turn, the 4b defect: a run
/// recorded Succeeded without its turn ever executing).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn inject_cron_prompt_into_session(
    registry: &Arc<WorkspaceRegistry>,
    session: Arc<mirri_core::SessionScope>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    llm_factory: &Option<Arc<dyn LlmFactory>>,
    in_flight_turns: &InFlightTurnsMap,
    index: Option<Arc<SessionIndex>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    goal_chain_states: &super::goal_driver::GoalChainStateMap,
    goal_chain_resources: &super::goal_driver::ChainResources,
    provider_auth_failures: &std::sync::Mutex<HashMap<String, String>>,
    session_id: &str,
    target_agent_id: &str,
    flow_config: mirri_core::TurnFlowConfig,
    fire: mirri_core::cron::CronFire,
    // reject_asks / workspace_run: see `inject_cron_prompt` (batch 2 /
    // A4's ask rule; a workspace task's execution session is unattended,
    // a session timer's fire keeps the attended chat ask).
    reject_asks: bool,
    workspace_run: bool,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    tool_result_home: Option<std::path::PathBuf>,
    run_approval_wait_hook: Option<mirri_core::turn_flow::ApprovalWaitHook>,
) -> InjectedStatus {
    // C.2a (stage-1 batch-2): the lateness mark is stamped HERE — at the
    // injection site, never at fire time — so a fire held through a busy
    // window, a restart, or a closed session's redelivery computes its
    // lateness fresh against the moment it finally lands. A run-now fire
    // (`scheduled_at == None`) never carries the mark (early by design,
    // not late).
    let mut fire = fire;
    fire.late_by_ms = fire.scheduled_at.and_then(|scheduled_at| {
        let elapsed = chrono::Utc::now() - scheduled_at;
        (elapsed >= mirri_core::cron::CRON_LATE_MARK_THRESHOLD)
            .then(|| elapsed.num_milliseconds().max(0) as u64)
    });
    // The caller resolved the target (the canonical main agent, falling
    // back to the first agent for agent-less-provisioned sessions) — the
    // injection and its idle gate key the same flow.
    let agent_id = target_agent_id.to_string();
    let Some(agent) = session.get_agent(&AgentId::new(agent_id.as_str())).await else {
        tracing::warn!(session_id, agent_id = %agent_id, "cron fire dropped: agent vanished");
        return InjectedStatus::Dropped;
    };
    let Some(llm_factory) = llm_factory else {
        tracing::warn!(session_id, "cron fire dropped: no LLM provider configured");
        return InjectedStatus::Dropped;
    };
    let (llm, model_alias) = {
        let config_arc = agent.config();
        let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
        // The alias (M1) the pinned provider is built from — captured from
        // the SAME config read so the turn's usage attribution cannot
        // drift from the provider it pinned (spec 11 A13: the wire
        // usage.record rows carry the alias, not the raw upstream name).
        let model_alias = config_guard.model_id.clone();
        match llm_factory.create_llm(&config_guard) {
            Ok(llm) => (llm, model_alias),
            Err(reason) => {
                tracing::warn!(session_id, %reason, "cron fire dropped: LLM creation failed");
                return InjectedStatus::Dropped;
            }
        }
    };

    // The cron-job origin object the TS kernel's `CronJobOrigin` declares:
    // the firing task's identity plus the real coalesced/stale values
    // (B5) — the prompt itself is injected as the engine turn's content
    // wrapped in the `<cron-fire>` envelope (B2 T9: the model reads the
    // firing context, the web client peels it back with
    // `extractCronPrompt`).
    let mut origin = json!({
        "kind": "cron_job",
        "jobId": fire.task_id,
        "cron": fire.cron,
        "recurring": fire.recurring,
        "coalescedCount": fire.coalesced_count,
        "stale": fire.stale,
        // The stage-1 terminal key (`maxFires`) rides here too — the
        // origin json stays aligned with the event.
        "maxFires": fire.maxed_out,
    });
    // C.2a (stage-1 batch-2): the lateness mark rides the origin too —
    // only when the fire is actually late (a conditional key, so the
    // aligned consumers see no change on on-time fires).
    if let Some(late_by_ms) = fire.late_by_ms {
        origin["lateByMs"] = json!(late_by_ms);
    }
    let prompt = render_cron_fire_envelope(&fire);
    let prompt_wire = json!([{ "type": "text", "text": prompt }]);
    // Persist the turn's opening (the `turn.prompt` bracket and the durable
    // user message) with the cron-job origin, exactly the `logRecord`
    // pattern the message-send dispatch applies.
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
    // ADR-008 (D.2 m4): the spawner's lazy execution session is NOT in
    // the routing tables, so `resolve_artifacts_via` misses — fall back
    // to the session's OWN freshly-minted artifacts (the bus has no
    // external subscribers by design; the run's transcript opens on the
    // explicit-open path).
    let artifacts = match resolve_artifacts_via(registry, session_id) {
        Ok(artifacts) => artifacts,
        Err(_) => session.adopt_reverse_rpc_slot(),
    };
    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    artifacts.event_bus.publish(SessionEvent::TurnStarted {
        agent_id: agent_id.clone(),
        session_id: session_id.to_string(),
        turn_id: turn_id.clone(),
    });

    // Register the turn for cancellation (the `message.abort` path): the
    // cron-injected turn runs inline on the driver's tick future (or the
    // trigger dispatch's spawned task) and closes its own bracket in both
    // arms below — exactly the WAIT contract, so only the cooperative
    // cancel token is registered.
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

    // Get-or-create the agent's turn flow (the same keying, tracker, and
    // steer-buffer registration the dispatch path applies).
    let flow_handle = {
        let key = format!("{}:{}", session_id, agent_id.as_str());
        let mut flows = turn_flows.lock().unwrap_or_else(|e| e.into_inner());
        // S7 / ADR-001 (B1 hardening): the fired turn's origin is NOT
        // stamped here on an existing flow — the try_lock race (skip the
        // stamp when a turn is running) let the fire start Interactive
        // after the in-flight turn's top-of-turn reset, bypassing the
        // lineage defense and the ask-deny. Instead the origin rides into
        // `run_engine_turn`, which stamps it under the blocking flow lock
        // — queued behind any in-flight turn, applied immediately before
        // the new turn starts. The new-flow arm still stamps at creation
        // (the flow is private until released).
        if !flows.contains_key(&key) {
            let mut turn_flow = mirri_core::TurnFlow::new(flow_config);
            turn_flow.set_turn_origin(mirri_core::permission::TurnOriginContext::Cron {
                reject_asks,
                workspace: workspace_run,
                readonly: fire.readonly,
                task_id: fire.task_id.clone(),
            });
            // ADR-021: the run's approval-wait signal rides the flow —
            // a workspace run's parked ask flips the RunRecord to
            // WaitingApproval until the user answers.
            if let Some(hook) = &run_approval_wait_hook {
                turn_flow.set_approval_wait_hook(std::sync::Arc::clone(hook));
            }
            turn_flow.set_event_bus(Arc::clone(&artifacts.event_bus));
            // Oversized tool outputs are persisted under the server-injected
            // home's `tool-results/` (falling back to the env-based brand
            // home) and replaced with a bounded preview — the same home
            // semantics as the server's get_or_create_turn_flow.
            turn_flow.set_tool_result_truncator(super::tool_result_truncator_for(
                tool_result_home.as_deref(),
            ));
            // The session's turn-id allocator (cron turns share the session's
            // single numeric turn namespace with the main agent's turns).
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
            // The compaction and usage-model-key handles (the same side-map
            // registrations the dispatch path's `get_or_create_turn_flow`
            // applies — review R1/S-1): a flow FIRST created by a cron turn
            // must still answer the interactive `setModel` window push and
            // usage re-attribution, or those updates silently no-op for the
            // rest of the session.
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
            // The by_model key is the agent's model alias (TS parity, N4).
            turn_flow.set_usage_model_key(agent.model_id());
            // The goal manager rides the same flow for usage accounting
            // (F11, wiring parity with the message.send / steer turn
            // paths — accounting-only, like those).
            turn_flow.set_goal_usage_accounting(agent.goal());
            flows.insert(key.clone(), Arc::new(std::sync::Mutex::new(turn_flow)));
        }
        Arc::clone(&flows[&key])
    };

    // The structured agent-status publisher rides along (CR4 parity with
    // `message.send`): the cron turn publishes its phase transitions
    // (`event.agent.status.updated`) the same way a user-dispatched turn
    // does — including when this cron turn is what first created the flow.
    super::wire_agent_status_publisher(&artifacts.event_bus, &agent, &flow_handle);

    let external_hook_env = {
        let mut env = HashMap::new();
        env.insert("MIRRI_SESSION_ID".to_string(), session_id.to_string());
        env.insert("MIRRI_AGENT_ID".to_string(), agent_id.as_str().to_string());
        env
    };
    let interaction_kernel = Some(Arc::new(
        InteractionKernel::new(
            Arc::clone(&artifacts.approval_gate),
            Arc::clone(&artifacts.question_gate),
        )
        // Plan D3: grant sink wiring parity with the message-send path.
        .with_grant_sink(session_grant_sink(&session)),
    ));
    let memory = agent.context_memory();
    let agent_tools = agent.tool_manager();
    let permission_manager = agent.permission_manager();
    let reverse_rpc_callback = artifacts
        .reverse_rpc_callback
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // ADR-009 (audit P0-1): mirror the callback this cron turn dispatches on
    // into the agent's live config, so a subagent spawned mid-turn inherits
    // it (the serve transport stamps the callback into the session artifacts
    // at subscribe — later than the agent's create-time snapshot).
    super::mirror_reverse_rpc_callback_into_agent_config(&agent, &reverse_rpc_callback);

    let turn_end = run_engine_turn(
        flow_handle,
        // S7 / ADR-001 (B1): the Cron origin rides INTO the critical
        // section — stamped under the blocking flow lock right before the
        // turn starts, so no fire can start Interactive behind a busy turn.
        Some(mirri_core::permission::TurnOriginContext::Cron {
            reject_asks,
            workspace: workspace_run,
            readonly: fire.readonly,
            task_id: fire.task_id.clone(),
        }),
        agent.hook_engine(),
        external_hook_env,
        Arc::clone(flag_registry),
        agent_tools,
        memory,
        // The cron turn's provider is built once per dispatch — fixed
        // resolver (pre-switch semantics, ADR-007), reporting the alias it
        // was pinned from so the usage rows carry the alias (A13).
        std::sync::Arc::new(mirri_core::FixedLlmResolver::with_alias(llm, model_alias)),
        vec![mirri_core::ContentPart::Text { text: prompt }],
        PromptOrigin::CronJob,
        // Cron turns never carry an admission stamp (ADR-006).
        None,
        Some(super::event_relay::live_chunk_callback(
            &artifacts.event_bus,
            agent_id.as_str(),
            session_id,
            Some(turn_id.clone()),
        )),
        turn_id.clone(),
        Some(Arc::clone(&session) as Arc<dyn mirri_core::WireOpRecorder>),
        permission_manager,
        reverse_rpc_callback,
        session.tasks(),
        session.todo_list(),
        interaction_kernel,
        cancel_token.clone(),
        turn_stall_timeout,
        turn_backstop_timeout,
        // No mid-turn guard: the cron turn's consistency posture is out of
        // invariant 2's scope (the interactive turn's journal is).
        None,
        (session_id.to_string(), agent_id.to_string()),
    )
    .await;

    match turn_end {
        Ok(turn_end) => {
            // A cancelled cron-injected turn reports `Aborted` even when
            // the provider stream ended cleanly after observing the
            // token — the awaiting client cancelled and must hear the
            // turn ended because of that (mirrors the message.send wait
            // arm).
            let turn_end = coerce_aborted_if_cancelled(turn_end, &cancel_token);
            session
                .record_op(WireOp::TurnEnded {
                    turn_id: turn_end.turn_id,
                    reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                    error: None,
                    duration_ms: Some(turn_end.duration_ms),
                    ended_at: Some(turn_end.ended_at),
                })
                .await;
            artifacts.event_bus.publish(SessionEvent::TurnCompleted {
                agent_id: agent_id.clone(),
                session_id: session_id.to_string(),
                turn_id,
                stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                wire_turn_id: turn_end.turn_id,
                duration_ms: Some(turn_end.duration_ms),
                ended_at_ms: Some(turn_end.ended_at),
            });
            // ADR-009 turn settle: the cron-injected turn ended — refresh
            // the session's recency and persist it.
            note_turn_settled(index, registry, &session).await;
            // Goal settlement (TS settleAbnormalTurn parity): an
            // abnormally-ended cron turn must not leave an Active goal
            // dangling.
            let _ = settle_goal_after_turn(registry, &session, turn_outcome(&turn_end.stop_reason))
                .await;
            // Goal continuation gating (TS handleTurnEnded parity): the
            // cron turn settled — the chain decision runs (a settled goal
            // exits early inside; a subagent target's non-main goal is not
            // driven, the main-agent scope rule).
            maybe_launch_goal_chain(
                registry,
                goal_chain_states,
                turn_flows,
                &session,
                &agent_id,
                goal_chain_resources,
            )
            .await;
        }
        Err(error) => {
            // O-5 surfaced-failure wiring (D.5): an attributed auth failure
            // is noted BEFORE the TurnFailed publish (the same race-free
            // ordering the message-send arms apply).
            if let EngineTurnError::Engine(core_error) = &error {
                super::credential_handlers::report_auth_failure_if_attributed(
                    provider_auth_failures,
                    core_error,
                );
            }
            artifacts.event_bus.publish(SessionEvent::TurnFailed {
                agent_id: agent_id.clone(),
                turn_id,
                error: error.message(),
                error_code: error.domain_code().to_string(),
                wire_turn_id: 0,
                // No TurnEndResult on this path — the failure moment
                // is the terminal instant.
                ended_at_ms: Some(super::message_handlers::now_epoch_ms()),
            });
            // A failed turn is still conversation activity (ADR-009) —
            // settle before the injection returns.
            note_turn_settled(index, registry, &session).await;
            // Goal settlement (TS settleAbnormalTurn parity): a failed cron
            // turn parks an Active goal as paused with the failure reason.
            let error_text = match error {
                EngineTurnError::Engine(core_error) => core_error.to_string(),
                other => other.message(),
            };
            let _ =
                settle_goal_after_turn(registry, &session, TurnOutcome::Failed(&error_text)).await;
            let failure = InjectedStatus::TurnFailed(error_text);
            // Goal continuation gating (TS handleTurnEnded parity): even a
            // failed cron turn runs the chain decision after its settle
            // (the budget block can still fire).
            maybe_launch_goal_chain(
                registry,
                goal_chain_states,
                turn_flows,
                &session,
                &agent_id,
                goal_chain_resources,
            )
            .await;
            return failure;
        }
    }
    InjectedStatus::Injected
}
