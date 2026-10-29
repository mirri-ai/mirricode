//! The per-session task driver: the production caller of
//! [`TaskManager::terminate_expired`](mirri_core::task::TaskManager::terminate_expired)
//! (B1 T4, D-40) and the background task **notification delivery engine**
//! (B2 T5, D-51).
//!
//! The sweeper half (B1): the recon found the timeout knobs were dead code
//! — `terminate_expired` had no production caller, so a runaway subagent
//! registered with an explicit deadline (or under the global fallback) ran
//! forever. This driver owns the cadence: a coarse interval tick per live
//! session that terminates expired tasks (a real cancellation — the signal
//! fires first, per the T2 fix) and logs — but never kills — stale-heartbeat
//! tasks (killing belongs to the timeout path).
//!
//! The notification half (B2): on the same tick, after the sweep, terminal
//! *detached* tasks whose notification key is in neither the context ledger
//! (delivered, the scan-based ruling SDR-B2-01) nor the steer buffer
//! (parked) are delivered to the main agent —
//! - **busy** (the main agent's turn-flow mutex is held): the notification
//!   XML is parked as a steer (write-before-mutate: the `turn.steer` record
//!   lands before the buffer push), and the running turn's `before_step`
//!   flush folds it into the next LLM request (TS `activeOrNewTurn`'s
//!   mid-turn admission). A NATURAL stop is NOT such a boundary: the
//!   continue hook leaves parked notifications in the buffer so the turn
//!   ends on its final summary message — the idle wakeup below delivers
//!   them on the next tick instead;
//! - **idle**: each due notification runs as its own engine turn (the
//!   cron-driver shape: `turn.prompt` + `context.append_message` + the
//!   turn bracket), with a confirmation gate — an unresolvable agent or
//!   LLM records nothing and keeps the debt for the next tick;
//! - **idle wakeup**: when the steer buffer holds ONLY notification steers
//!   (busy-parked, then the turn ended without flushing — a natural stop
//!   whose parked notifications were deliberately left for this path, a
//!   crash whose resume re-parks the buffer, or a bridge-level turn
//!   timeout), the driver records the flush marker for the parked count
//!   and converts each parked notification into a delivered turn, so
//!   nothing strands. A live abort never strands: the continue hook
//!   consumes every non-notification steer at the stop boundary, and any
//!   parked notifications ride the idle wakeup path.
//!
//! The driver exits when its session disappears (closed or archived): each
//! tick re-resolves the session through the W3 routing seam and breaks
//! when no workspace owns it anymore — the same shape as the cron driver.

use std::collections::HashMap;
use std::sync::Arc;

use mirri_core::{
    AgentId, CancelToken, InteractionKernel, PromptOrigin, SessionEvent, SteerBuffer,
    event_projection::turn_stop_reason_wire,
    task_notification::{
        NotificationDue, delivered_keys, due_notifications, fire_notification_hook, parked_keys,
        sweep_mark_delivered,
    },
};
use mirri_proto::WireOp;
use serde_json::json;

use super::additional_dirs::session_grant_sink;
use super::goal_driver::{
    TurnOutcome, maybe_launch_goal_chain, settle_goal_after_turn, turn_outcome,
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
type TurnFlowMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<mirri_core::TurnFlow>>>>>;

/// The embedded server's per-agent activity-tracker map.
type ActivityTrackerMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<mirri_core::turn_flow::ActivityTracker>>>>;

/// The embedded server's per-agent shared-steer-buffer map (the same shape
/// [`EmbeddedServer::steer_buffers`](super::EmbeddedServer) owns).
type SteerBufferMap = Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<SteerBuffer>>>>>;

/// The embedded server's per-agent compaction-handle map (the same shape
/// [`EmbeddedServer::compaction_handles`](super::EmbeddedServer) owns) —
/// registered here so the driver-created flows answer the mid-turn model
/// switch's window push.
type CompactionHandleMap = Arc<
    std::sync::Mutex<
        HashMap<String, Arc<std::sync::Mutex<mirri_core::compaction::FullCompaction>>>,
    >,
>;

/// The embedded server's per-agent usage-model-key-handle map (the same
/// shape [`EmbeddedServer::usage_model_key_handles`](super::EmbeddedServer)
/// owns) — registered here so the driver-created flows answer the mid-turn
/// model switch's usage re-attribution (spec 11 A13).
type UsageModelKeyHandleMap = Arc<std::sync::Mutex<HashMap<String, super::UsageModelKeyHandle>>>;

impl EmbeddedServer {
    /// Start the task-lifecycle sweeper and notification driver for one
    /// live session. Idempotent per tick state — the driver owns no
    /// registry entry; it exits by itself when the session disappears.
    pub(super) fn spawn_task_driver(&self, session_id: &str) {
        // The routing seam (W3): hold the workspace registry, not one
        // manager — every tick re-resolves the session through it.
        let registry: Arc<WorkspaceRegistry> = Arc::clone(&self.registry);
        let turn_flows = Arc::clone(&self.turn_flows);
        let activity_trackers = Arc::clone(&self.activity_trackers);
        let steer_buffers = Arc::clone(&self.steer_buffers);
        let compaction_handles = Arc::clone(&self.compaction_handles);
        let usage_model_key_handles = Arc::clone(&self.usage_model_key_handles);
        let llm_factory = self.llm_factory.clone();
        let server_config = Arc::clone(&self.config);
        let turn_stall_timeout = self.turn_stall_timeout;
        let turn_backstop_timeout = self.turn_backstop_timeout;
        // The notification turns register in the shared in-flight registry
        // (the `message.abort` path), keyed like the dispatch turns.
        let in_flight_turns: InFlightTurnsMap = Arc::clone(&self.in_flight_turns);
        // The turn-settle write-through (ADR-009): the driver task holds no
        // `&EmbeddedServer` — the shared index travels with it so each
        // notification turn's settle can refresh the session.
        let settle_index = self.session_index();
        let flag_registry = Arc::clone(self.flag_registry());
        // The goal continuation decision runs after each notification
        // turn's settle — the driver task holds no `&EmbeddedServer`, so
        // the chain-state map travels with it as a cloned field Arc.
        let goal_chain_states = Arc::clone(&self.goal_chain_states);
        // The chain's injection resources, snapshotted before the spawn
        // (the driver task holds no `&EmbeddedServer` — the field Arcs
        // travel with it, the same stance as the captures above).
        let goal_chain_resources = self.goal_chain_resources();
        let tool_result_home = self.home_dir().map(ToOwned::to_owned);
        // O-5 surfaced-failure wiring (D.5): the driver task holds no
        // `&EmbeddedServer` — the shared auth-failure map travels with it as
        // a cloned field Arc (the same stance as the settle captures above).
        let provider_auth_failures = Arc::clone(&self.provider_auth_failures);
        let sid = session_id.to_string();
        let tick = self.task_sweep_tick;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            // The first tick completes immediately; an expired task
            // terminates on the second — acceptable for a sweeper whose
            // production tick is one second.
            // Edge-triggered stale warnings: one warn per task, on the
            // first tick it goes stale — a long-stuck task must not
            // replay the warning on every sweep (task ids are unique, so
            // the set only ever grows by the live task count).
            let mut stale_warned: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            loop {
                interval.tick().await;
                let Ok(session) = resolve_session_via(&registry, &sid).await else {
                    // The session closed or was archived — stop sweeping.
                    break;
                };
                let tasks = session.tasks();
                let terminated = tasks.terminate_expired();
                if !terminated.is_empty() {
                    tracing::info!(
                        session_id = %sid,
                        count = terminated.len(),
                        task_ids = ?terminated,
                        "terminated expired background tasks"
                    );
                }
                // Stale-heartbeat tasks are warn-only: staleness signals a
                // runner that died without reporting (e.g. an agent host
                // crash); the kill decision belongs to the timeout path,
                // which owns real cancellation.
                for task in tasks.list() {
                    if task.status.is_terminal() {
                        continue;
                    }
                    if tasks.is_stale(&task.id) && stale_warned.insert(task.id.clone()) {
                        tracing::warn!(
                            session_id = %sid,
                            task_id = %task.id,
                            description = %task.description,
                            "background task is stale (no heartbeat within the window)"
                        );
                    }
                }
                // The notification delivery rides the same tick, after the
                // sweep (a timeout-terminated task notifies in the same
                // pass — its `timed_out` wording rides the XML).
                deliver_due_notifications(
                    settle_index.clone(),
                    &registry,
                    &turn_flows,
                    &activity_trackers,
                    &steer_buffers,
                    &compaction_handles,
                    &usage_model_key_handles,
                    &llm_factory,
                    &server_config,
                    &flag_registry,
                    turn_stall_timeout,
                    turn_backstop_timeout,
                    &in_flight_turns,
                    &goal_chain_states,
                    &goal_chain_resources,
                    &provider_auth_failures,
                    tool_result_home.clone(),
                    &session,
                    &sid,
                )
                .await;
            }
        });
    }
}

/// One notification-delivery tick: adjudicate the due set against the
/// scan-based ledger, then deliver — park when busy, run turns when idle,
/// and convert an all-notification steer buffer (the stranded state) into
/// delivered turns. Serialized inside the driver loop (no new tasks are
/// spawned — the delivery is naturally mutex-exclusive with the sweep).
#[allow(clippy::too_many_arguments)]
async fn deliver_due_notifications(
    index: Option<Arc<SessionIndex>>,
    registry: &Arc<WorkspaceRegistry>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    llm_factory: &Option<Arc<dyn LlmFactory>>,
    server_config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    in_flight_turns: &InFlightTurnsMap,
    goal_chain_states: &super::goal_driver::GoalChainStateMap,
    goal_chain_resources: &super::goal_driver::ChainResources,
    provider_auth_failures: &std::sync::Mutex<HashMap<String, String>>,
    tool_result_home: Option<std::path::PathBuf>,
    session: &Arc<mirri_core::SessionScope>,
    session_id: &str,
) {
    // SDR (goal-stop-freeze): a user-paused goal freezes the
    // notification loop — due (and stranded) notifications stay pending
    // until the goal resumes. Pausing must stop the machine, not just
    // flip a state bit the injected turns ignore.
    if session
        .goal()
        .get()
        .is_some_and(|goal| goal.status == mirri_core::GoalStatus::Paused)
    {
        return;
    }

    // The notification loop addresses the session's canonical main agent
    // (the same pin both the steer recording and the resume restore use) —
    // unless the task was dispatched BY a live subagent (B4 T9, A14: the
    // session+agent routing the B2 loop reserved): a task a nested
    // subagent dispatched delivers to that DISPATCHER, not the main agent.
    //
    // Review SF-3: the addressee is resolved through the task's SUBJECT
    // (the `TaskKind::Agent` agent id — the spawned child, which stays the
    // recovery/resume identity everywhere else) and the session's live
    // registry (`subagent.registered`'s parent link): a completed child has
    // nothing to say about its own completion, its dispatcher does. A
    // subject whose parent is the main agent — or is unresolvable —
    // delivers to main (the pre-B4 behavior for every flat dispatch).
    //
    // Review N4: a tick addresses ONE addressee. The candidates are walked
    // in task-list order (main last, covering the stranded-only case) and
    // the first addressee with something actually due anchors the tick —
    // already-satisfied dispatchers are skipped, so several agents'
    // pending notifications converge across ticks instead of funneling
    // into one turn.
    let Some(main_agent) = session
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
    else {
        return;
    };
    let candidates: Vec<mirri_core::task::AgentTask> = session
        .tasks()
        .list()
        .into_iter()
        .filter(|task| task.status.is_terminal() && task.detached && !task.notification_suppressed)
        .collect();
    // The idle-common fast path (the original peek): nothing to deliver
    // unless some task is terminal+detached+unsuppressed (the due filter's
    // own first cut) or a steer buffer holds a stranded notification. The
    // peek avoids locking the context memory (and resolving an LLM) on
    // every tick of every session — the sweeper's common case is no
    // notifications at all.
    if candidates.is_empty() && !any_stranded_buffer(steer_buffers) {
        return;
    }

    let mut addressees: Vec<String> = Vec::new();
    for task in &candidates {
        let addressee = task_addressee_id(session, task).await;
        if !addressees.contains(&addressee) {
            addressees.push(addressee);
        }
    }
    // Main last: a stranded-only buffer delivers even when no candidate
    // task resolves a dispatcher, and a flat dispatch's addressee is main
    // anyway.
    addressees.push(super::DEFAULT_MAIN_AGENT_ID.to_string());
    let mut tried = std::collections::HashSet::new();
    let mut anchor: Option<(std::sync::Arc<mirri_core::AgentScope>, String)> = None;
    // The anchored addressee's agent id (Q-2: the notification turn's
    // identity — hook env, live events, failure events — must name the
    // addressee, not main).
    let mut anchored_addressee = String::new();
    let mut due: Vec<NotificationDue> = Vec::new();
    let mut has_stranded = false;
    // The subagent claim (review round 2, F1), held for the anchored
    // notification turn's whole duration and dropped at the tick's end.
    let mut subagent_claim: Option<mirri_core::NotificationRunGuard> = None;
    for addressee_id in addressees {
        if !tried.insert(addressee_id.clone()) {
            continue;
        }
        let Some(addressee) = resolve_addressee(
            session,
            steer_buffers,
            &main_agent,
            session_id,
            &addressee_id,
        )
        .await
        else {
            continue;
        };
        if addressee.due.is_empty() && !addressee.has_stranded {
            continue;
        }
        // Busy adjudication (review S3 + round 2, F1/Q-1). Main's busy
        // signal is the shared flow-map mutex (the dispatch paths run on
        // the same map — `try_lock` failing means a turn of main is
        // running). A subagent addressee's own runs live in its parent
        // host's completion future — a flow the driver's map never sees —
        // so the exclusion is the host's per-child `running` flag,
        // ATOMICALLY claimed here: the claim closes the probe's TOCTOU (a
        // host spawn/resume started right after a point-in-time probe
        // would have raced the notification turn) because the claim and
        // those paths take-and-test the same flag under the same lock.
        // Either way a busy addressee parks and the loop CONTINUES (round
        // 2, Q-1): one busy agent no longer blocks the rest of the tick;
        // the parked debt rides its own buffer to a later idle tick.
        if addressee_id == super::DEFAULT_MAIN_AGENT_ID {
            let busy = {
                let flows = turn_flows.lock().unwrap_or_else(|e| e.into_inner());
                flows
                    .get(&addressee.agent_key)
                    .is_some_and(|flow| flow.try_lock().is_err())
            };
            if busy {
                park_due_notifications(
                    session,
                    steer_buffers,
                    &addressee_id,
                    &addressee.agent_key,
                    session_id,
                    addressee.due,
                )
                .await;
                continue;
            }
        } else {
            match claim_subagent_addressee(session, &addressee_id).await {
                Some(claim) => subagent_claim = Some(claim),
                None => {
                    park_due_notifications(
                        session,
                        steer_buffers,
                        &addressee_id,
                        &addressee.agent_key,
                        session_id,
                        addressee.due,
                    )
                    .await;
                    continue;
                }
            }
        }
        anchor = Some((addressee.agent, addressee.agent_key));
        anchored_addressee = addressee_id;
        due = addressee.due;
        has_stranded = addressee.has_stranded;
        break;
    }
    let Some((agent, agent_key)) = anchor else {
        // Every candidate's ledger is already satisfied.
        return;
    };
    // `subagent_claim` (if any) stays live until the end of this function —
    // the notification turns below run exclusively on the addressee.

    // Idle. The confirmation gate runs BEFORE anything is consumed or
    // recorded — an unresolvable provider keeps the debt whole for the
    // next tick instead of half-recording a turn (or draining the buffer).
    let Some(llm_factory) = llm_factory else {
        if !due.is_empty() || has_stranded {
            tracing::warn!(
                session_id,
                "notification delivery deferred: no LLM provider configured"
            );
        }
        return;
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
                tracing::warn!(session_id, %reason, "notification delivery deferred: LLM creation failed");
                return;
            }
        }
    };
    let Ok(artifacts) = resolve_artifacts_via(registry, session_id) else {
        return;
    };

    // The stranded-buffer wakeup (FIFO: parked notifications predate the
    // fresh ones) — but only when the buffer holds nothing but
    // notifications: a user steer in the mix must keep its flush order, and
    // the next turn's before_step delivers the whole buffer in order. The
    // take runs under the map+buffer locks only; the durable flush marker
    // records after the guards drop (a guard across an await is not Send).
    let stranded: Vec<NotificationDelivery> = {
        let taken = {
            let buffers = steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
            match buffers.get(&agent_key).cloned() {
                None => None,
                Some(buffer) => {
                    let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let taken = buffer.take_items();
                    if taken.is_empty()
                        || taken
                            .iter()
                            .any(|s| !matches!(s.origin, PromptOrigin::BackgroundTask { .. }))
                    {
                        // Not stranded (or empty): put everything back
                        // untouched — still under the buffer lock.
                        for steer in taken {
                            buffer.push(steer.content, steer.origin);
                        }
                        None
                    } else {
                        Some(taken)
                    }
                }
            }
        };
        match taken {
            None => Vec::new(),
            Some(taken) => {
                // The wakeup consumes the parked notifications: the flush
                // marker makes the parked `turn.steer` records consumed on
                // replay; each notification then delivers as its own turn.
                // The marker rides the addressee's journal with its steer
                // records (the replayed flush marker must fold at the same
                // agent the replayed steers fold at).
                let recorder = notification_recorder(session, &anchored_addressee).await;
                recorder
                    .record_op(WireOp::TurnSteerFlushed {
                        count: taken.len() as u32,
                    })
                    .await;
                taken
                    .into_iter()
                    .map(|steer| NotificationDelivery {
                        key: origin_key(&steer.origin),
                        xml: steer
                            .content
                            .into_iter()
                            .filter_map(|part| match part {
                                mirri_core::ContentPart::Text { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    })
                    .collect()
            }
        }
    };

    // Fire the Notification external hook once per fresh due notification
    // (TS parity: the `task.notified` event → `notifyTaskNotification` —
    // observation only, never a gate). Parked deliveries fire at park time;
    // the stranded wakeup below does not re-fire them.
    for due_item in &due {
        fire_notification_hook(
            &session.hook_engine(),
            &notification_hook_env(session_id, &agent_key),
            &due_item.notification,
        )
        .await;
    }

    for delivery in stranded
        .into_iter()
        .chain(due.into_iter().map(|d| NotificationDelivery {
            key: d.key,
            xml: d.xml,
        }))
    {
        run_notification_turn(
            index.clone(),
            registry,
            turn_flows,
            activity_trackers,
            steer_buffers,
            compaction_handles,
            usage_model_key_handles,
            server_config,
            flag_registry,
            turn_stall_timeout,
            turn_backstop_timeout,
            in_flight_turns,
            goal_chain_states,
            goal_chain_resources,
            provider_auth_failures,
            tool_result_home.clone(),
            session,
            session_id,
            &agent_key,
            &anchored_addressee,
            &agent,
            llm.clone(),
            model_alias.clone(),
            &artifacts,
            delivery,
        )
        .await;
    }
    // Release the anchored addressee's claim (if any) only after every
    // notification turn of this tick finished — the early-return paths
    // above drop it at their scope exit, which is equally correct (the
    // deferral re-claims on the next tick).
    drop(subagent_claim);
}

/// One notification's delivery payload: the dedup key (its wire origin)
/// and the rendered XML.
struct NotificationDelivery {
    key: mirri_core::task_notification::NotificationKey,
    xml: String,
}

/// The key of a parked notification origin (None for anything else —
/// only reachable for BackgroundTask by construction).
fn origin_key(origin: &PromptOrigin) -> mirri_core::task_notification::NotificationKey {
    match origin {
        PromptOrigin::BackgroundTask {
            task_id,
            status,
            notification_id,
        } => mirri_core::task_notification::NotificationKey {
            task_id: task_id.clone(),
            status: status.clone(),
            notification_id: notification_id.clone(),
        },
        _ => unreachable!("the wakeup path only takes background-task steers"),
    }
}

/// The base env for the `Notification` external hook fire (the same seam the
/// compaction handlers use for their hook fires).
fn notification_hook_env(session_id: &str, agent_key: &str) -> HashMap<String, String> {
    HashMap::from([
        ("MIRRI_SESSION_ID".to_string(), session_id.to_string()),
        ("MIRRI_AGENT_ID".to_string(), agent_key.to_string()),
    ])
}

/// Recorder that routes agent-domain wire ops to the addressee's journal —
/// the session journal for main, the child journal for subagent addressees —
/// mirroring the routed recorder the subagent host installs for child turns.
///
/// The subagent notification fix (SDR-B2-01, task 4): the notification
/// turn's context/turn records must ride the ADDRESSEE's own journal, or a
/// resumed session rehydrates the subagent from a journal without the
/// delivered message and re-delivers the notification as a fresh turn.
pub(super) async fn notification_recorder(
    session: &mirri_core::SessionScope,
    addressee_id: &str,
) -> std::sync::Arc<dyn mirri_core::WireOpRecorder> {
    let session_log = session.wire_recorder();
    if addressee_id == super::DEFAULT_MAIN_AGENT_ID {
        return std::sync::Arc::new(mirri_core::session::journal_route::RoutedWireRecorder::new(
            session_log.clone(),
            session_log,
        ));
    }
    let child_log = session.child_wire_log(addressee_id).await;
    std::sync::Arc::new(mirri_core::session::journal_route::RoutedWireRecorder::new(
        child_log,
        session_log,
    ))
}

/// Park the due notifications into the addressee's steer buffer
/// (write-before-mutate: the `turn.steer` record with the notification
/// identity rides before the buffer push, so a crash between the two never
/// strands an unrecorded notification — the resume re-parks from the
/// record).
async fn park_due_notifications(
    session: &mirri_core::SessionScope,
    steer_buffers: &SteerBufferMap,
    addressee_id: &str,
    agent_key: &str,
    session_id: &str,
    due: Vec<NotificationDue>,
) {
    if due.is_empty() {
        return;
    }
    // The routed recorder: a subagent addressee's parked steers ride its own
    // journal (the resume re-parks from the same records) — main's stay on
    // the session journal.
    let recorder = notification_recorder(session, addressee_id).await;
    // Get-or-create (review round 2, F2): a subagent addressee's first
    // busy tick has no driver-side entry yet (only a previous notification
    // turn creates one; the host's own runs flush the host flow's buffer,
    // not this map). Creating the entry here parks the debt for real —
    // the stranded-buffer wakeup on a later idle tick drains it — instead
    // of no-op-ing with a warn on every tick of the whole busy window.
    let buffer = {
        let mut buffers = steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            buffers
                .entry(agent_key.to_string())
                .or_insert_with(|| Arc::new(std::sync::Mutex::new(SteerBuffer::new()))),
        )
    };
    for notification in due {
        // Fire the Notification external hook at park time (the busy-path
        // delivery point — TS parity: `task.notified`). The stranded wakeup
        // that later converts parked notifications into turns does not
        // re-fire.
        fire_notification_hook(
            &session.hook_engine(),
            &notification_hook_env(session_id, agent_key),
            &notification.notification,
        )
        .await;
        let content = vec![mirri_core::ContentPart::Text {
            text: notification.xml,
        }];
        recorder
            .record_op(WireOp::TurnSteer {
                input: json!([{ "type": "text", "text": content_text(&content) }]),
                origin: notification.key.wire(),
            })
            .await;
        buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(content, notification.key.origin());
    }
}

/// The text of a single-part text content (the notification XML).
fn content_text(content: &[mirri_core::ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            mirri_core::ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one notification as its own engine turn — the cron-driver shape:
/// the durable `turn.prompt` + `context.append_message` records with the
/// notification origin, the turn bracket events, and the engine turn
/// itself. The message landing in context is the delivery (durable even if
/// the turn then fails — the turn bracket reports the failure, the
/// notification debt is paid).
#[allow(clippy::too_many_arguments)]
async fn run_notification_turn(
    index: Option<Arc<SessionIndex>>,
    registry: &Arc<WorkspaceRegistry>,
    turn_flows: &TurnFlowMap,
    activity_trackers: &ActivityTrackerMap,
    steer_buffers: &SteerBufferMap,
    compaction_handles: &CompactionHandleMap,
    usage_model_key_handles: &UsageModelKeyHandleMap,
    server_config: &Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    flag_registry: &Arc<mirri_core::FlagRegistry>,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
    in_flight_turns: &InFlightTurnsMap,
    goal_chain_states: &super::goal_driver::GoalChainStateMap,
    goal_chain_resources: &super::goal_driver::ChainResources,
    provider_auth_failures: &std::sync::Mutex<HashMap<String, String>>,
    tool_result_home: Option<std::path::PathBuf>,
    session: &Arc<mirri_core::SessionScope>,
    session_id: &str,
    agent_key: &str,
    addressee_id: &str,
    agent: &Arc<mirri_core::AgentScope>,
    llm: Arc<dyn mirri_core::llm::Llm>,
    model_alias: String,
    artifacts: &mirri_core::SessionArtifacts,
    delivery: NotificationDelivery,
) {
    let prompt = delivery.xml;
    let origin = delivery.key.origin();
    let origin_wire = delivery.key.wire();
    let prompt_wire = json!([{ "type": "text", "text": prompt }]);
    // The routed recorder (SDR-B2-01, task 4): the notification turn's
    // agent-domain records ride the ADDRESSEE's own journal — main's on the
    // session journal, a subagent addressee's on its child journal — so a
    // resumed session rehydrates the addressee WITH the delivered message
    // (the ledger that decides whether the debt is paid).
    let recorder = notification_recorder(session, addressee_id).await;

    // Persist the turn's opening (write-before-mutate) with the
    // notification identity — the exact `logRecord` pattern the cron and
    // message-send dispatches apply.
    recorder
        .record_op(WireOp::TurnPrompt {
            input: prompt_wire.clone(),
            origin: origin_wire.clone(),
        })
        .await;
    recorder
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": prompt_wire,
                "origin": origin_wire,
            }),
        })
        .await;
    // The durable delivery marker (SDR-B2-01, task 2): the message landing
    // in context is the delivery — the task record carries the fact so a
    // rebuilt context (resume, compaction, subagent journal replay) never
    // re-arms the debt.
    session
        .tasks()
        .mark_notification_delivered(&delivery.key.task_id);

    // Announce the turn bracket on the session bus.
    let event_bus = &artifacts.event_bus;
    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    event_bus.publish(SessionEvent::TurnStarted {
        agent_id: addressee_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.clone(),
    });

    // Register the turn for cancellation (the `message.abort` path): the
    // notification turn runs inline on the driver's tick future and
    // closes its own bracket in both arms below — exactly the WAIT
    // contract, so only the cooperative cancel token is registered.
    let cancel_token = CancelToken::new();
    let in_flight_key = format!("{session_id}:{addressee_id}:{turn_id}");
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
    // steer-buffer registration the dispatch paths apply). Hand-rolled
    // rather than reusing `EmbeddedServer::get_or_create_turn_flow` because
    // the driver task holds only the shared maps, not `&self` — the shapes
    // must stay aligned with that helper (review note: duplication is the
    // price of the map-only ownership; a shared free function would remove
    // the drift risk if a third caller appears).
    let flow_config = {
        let config_arc = agent.config();
        let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
        super::turn_flow_config_for_shared(server_config, &config_guard.model_id)
    };
    let flow_handle = {
        let mut flows = turn_flows.lock().unwrap_or_else(|e| e.into_inner());
        if !flows.contains_key(agent_key) {
            let mut turn_flow = mirri_core::TurnFlow::new(flow_config);
            turn_flow.set_event_bus(Arc::clone(event_bus));
            // Oversized tool outputs are persisted under the server-injected
            // home's `tool-results/` (falling back to the env-based brand
            // home) and replaced with a bounded preview — the same home
            // semantics as the server's get_or_create_turn_flow.
            turn_flow.set_tool_result_truncator(super::tool_result_truncator_for(
                tool_result_home.as_deref(),
            ));
            // The session's turn-id allocator (task turns share the session's
            // single numeric turn namespace with the main agent's turns).
            turn_flow.set_turn_id_source(artifacts.turn_id_counter());
            let tracker = turn_flow.activity_tracker();
            activity_trackers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(agent_key.to_string(), tracker);
            let steer_buffer = turn_flow.steer_buffer_handle();
            steer_buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(agent_key.to_string(), steer_buffer);
            // The compaction and usage-model-key handles (the same side-map
            // registrations the dispatch path's `get_or_create_turn_flow`
            // applies — review R1/S-1): a flow FIRST created by a
            // notification turn must still answer the interactive
            // `setModel` window push and usage re-attribution, or those
            // updates silently no-op for the rest of the session.
            let compaction_handle = turn_flow.compaction_handle();
            compaction_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(agent_key.to_string(), compaction_handle);
            let usage_model_key = turn_flow.usage_model_key_handle();
            usage_model_key_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(agent_key.to_string(), usage_model_key);
            turn_flow.set_usage_sink(agent.usage_service());
            // The by_model key is the agent's model alias (TS parity, N4).
            turn_flow.set_usage_model_key(agent.model_id());
            turn_flow.set_goal_usage_accounting(agent.goal());
            flows.insert(
                agent_key.to_string(),
                Arc::new(std::sync::Mutex::new(turn_flow)),
            );
        }
        Arc::clone(&flows[agent_key])
    };

    // The structured agent-status publisher rides along (CR4 parity with
    // `message.send`): the notification turn publishes its phase
    // transitions (`event.agent.status.updated`) the same way a
    // user-dispatched turn does — including when this driver turn is what
    // first created the flow.
    super::wire_agent_status_publisher(event_bus, agent, &flow_handle);

    let mut external_hook_env = HashMap::new();
    external_hook_env.insert("MIRRI_SESSION_ID".to_string(), session_id.to_string());
    // The addressee's own id, not main (review round 2, Q-2): a
    // subagent-dispatched task's notification turn runs on the dispatcher,
    // and the hook env must name the agent actually turning.
    external_hook_env.insert("MIRRI_AGENT_ID".to_string(), addressee_id.to_string());
    let interaction_kernel = Some(Arc::new(
        InteractionKernel::new(
            Arc::clone(&artifacts.approval_gate),
            Arc::clone(&artifacts.question_gate),
        )
        // Plan D3: grant sink wiring parity with the message-send path.
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
    // ADR-009 (audit P0-1): mirror the callback this notification turn
    // dispatches on into the agent's live config, so a subagent spawned
    // mid-turn inherits it (the serve transport stamps the callback into the
    // session artifacts at subscribe — later than the agent's create-time
    // snapshot).
    super::mirror_reverse_rpc_callback_into_agent_config(agent, &reverse_rpc_callback);

    let turn_end = run_engine_turn(
        flow_handle,
        agent.hook_engine(),
        external_hook_env,
        Arc::clone(flag_registry),
        agent_tools,
        memory,
        // The notification turn's provider is built once per dispatch —
        // fixed resolver (pre-switch semantics, ADR-007), reporting the
        // alias it was pinned from so the usage rows carry the alias (A13).
        std::sync::Arc::new(mirri_core::FixedLlmResolver::with_alias(llm, model_alias)),
        vec![mirri_core::ContentPart::Text { text: prompt }],
        origin,
        // Notification turns never carry an admission stamp (ADR-006).
        None,
        Some(super::event_relay::live_chunk_callback(
            event_bus,
            addressee_id,
            session_id,
            Some(turn_id.clone()),
        )),
        turn_id.clone(),
        Some(Arc::clone(&recorder)),
        permission_manager,
        reverse_rpc_callback,
        session.tasks(),
        session.todo_list(),
        interaction_kernel,
        cancel_token.clone(),
        turn_stall_timeout,
        turn_backstop_timeout,
        // No mid-turn guard: the notification turn's consistency posture is
        // out of invariant 2's scope (the interactive turn's journal is).
        None,
        (session_id.to_string(), addressee_id.to_string()),
    )
    .await;

    match turn_end {
        Ok(turn_end) => {
            // A cancelled notification turn reports `Aborted` even when
            // the provider stream ended cleanly after observing the
            // token — the awaiting client cancelled and must hear the
            // turn ended because of that (mirrors the message.send wait
            // arm).
            let turn_end = coerce_aborted_if_cancelled(turn_end, &cancel_token);
            recorder
                .record_op(WireOp::TurnEnded {
                    turn_id: turn_end.turn_id,
                    reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                    error: None,
                    duration_ms: Some(turn_end.duration_ms),
                })
                .await;
            event_bus.publish(SessionEvent::TurnCompleted {
                agent_id: addressee_id.to_string(),
                session_id: session_id.to_string(),
                turn_id,
                stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                wire_turn_id: turn_end.turn_id,
                duration_ms: Some(turn_end.duration_ms),
            });
            // ADR-009 turn settle: the notification turn ended — refresh
            // the session's recency and persist it.
            note_turn_settled(index, registry, session).await;
            // Goal settlement (TS settleAbnormalTurn parity): an
            // abnormally-ended notification turn must not leave an Active
            // goal dangling.
            let _ = settle_goal_after_turn(registry, session, turn_outcome(&turn_end.stop_reason))
                .await;
            // Goal continuation gating (TS handleTurnEnded parity): a
            // main-agent notification turn is the background wait's wake-up
            // path — after it settles, the chain decision runs (subagent
            // addressees exit early inside: only main's goal is driven).
            maybe_launch_goal_chain(
                registry,
                goal_chain_states,
                turn_flows,
                session,
                addressee_id,
                goal_chain_resources,
            )
            .await;
        }
        Err(error) => {
            // The notification debt is paid (the message is in context and
            // durable); only the turn bracket reports the failure.
            // O-5 surfaced-failure wiring (D.5): an attributed auth failure
            // is noted BEFORE the TurnFailed publish (the same race-free
            // ordering the message-send arms apply).
            if let EngineTurnError::Engine(core_error) = &error {
                super::credential_handlers::report_auth_failure_if_attributed(
                    provider_auth_failures,
                    core_error,
                );
            }
            tracing::warn!(
                session_id,
                turn_id = %turn_id,
                error = %error.message(),
                "notification turn failed after delivery"
            );
            event_bus.publish(SessionEvent::TurnFailed {
                agent_id: addressee_id.to_string(),
                turn_id,
                error: error.message(),
                error_code: error.domain_code().to_string(),
                wire_turn_id: 0,
            });
            // A failed turn is still conversation activity (ADR-009) —
            // settle before the delivery returns.
            note_turn_settled(index, registry, session).await;
            // Goal settlement (TS settleAbnormalTurn parity): a failed
            // notification turn parks an Active goal as paused with the
            // failure reason.
            let error_text = match error {
                EngineTurnError::Engine(core_error) => core_error.to_string(),
                other => other.message(),
            };
            let _ =
                settle_goal_after_turn(registry, session, TurnOutcome::Failed(&error_text)).await;
            // Goal continuation gating (TS handleTurnEnded parity): even a
            // failed notification turn runs the chain decision after its
            // settle (the budget block can still fire).
            maybe_launch_goal_chain(
                registry,
                goal_chain_states,
                turn_flows,
                session,
                addressee_id,
                goal_chain_resources,
            )
            .await;
        }
    }
}

// --- addressee resolution (B4 T9, reviews SF-3/N4/S3) ------------------------

/// One candidate notification addressee (review N4): the agent to address,
/// its steer-buffer key, and what is due for it this tick.
struct NotificationAddressee {
    agent: std::sync::Arc<mirri_core::AgentScope>,
    agent_key: String,
    due: Vec<NotificationDue>,
    has_stranded: bool,
}

/// The notification addressee for one terminal task (review SF-3): an
/// agent-kind task's SUBJECT (the `TaskKind::Agent` agent id — the spawned
/// child, which stays the recovery/resume identity everywhere else)
/// resolved through the session's live registry to its parent — the agent
/// that dispatched it; a process-kind task carries its dispatcher
/// attribution directly (`TaskKind::Process.dispatcher`, stamped by the
/// Bash tool at registration — a subagent's background bash task delivers
/// its notification back to that subagent). `None` (→ main) when the
/// dispatcher is the main agent, unresolvable, or no longer live: main is
/// the default addressee, not an anchor.
async fn task_dispatcher(
    session: &mirri_core::SessionScope,
    task: &mirri_core::task::AgentTask,
) -> Option<String> {
    let dispatcher = match &task.kind {
        mirri_core::task::TaskKind::Agent {
            agent_id: Some(subject),
            ..
        } => {
            let parent = session.subagent_parent(subject).await?;
            if parent == super::DEFAULT_MAIN_AGENT_ID {
                return None;
            }
            parent
        }
        mirri_core::task::TaskKind::Process {
            dispatcher: Some(dispatcher),
            ..
        } => dispatcher.clone(),
        _ => return None,
    };
    // The liveness fallback (review SF-3): a dispatcher that is no longer
    // live delivers to main, whose own resolution carries the task. Under
    // lazy subagent restore (ADR-006/ADR-017) a registered dispatcher is
    // DORMANT until woken — so liveness is checked AFTER attempting to
    // hydrate it: a registered-but-dormant child's notifications must keep
    // routing to it (their steers ride its own journal), not fall back to
    // main. A child that genuinely cannot hydrate (missing journal /
    // unresolvable model) falls back to main as before.
    if session.hydrate_subagent(&dispatcher).await {
        return Some(dispatcher);
    }
    session
        .get_agent(&AgentId::new(&dispatcher))
        .await
        .map(|_| dispatcher)
}

/// [`task_dispatcher`] with main as the default: the addressee every task's
/// notification is grouped under. `pub(super)` — the resume-time restore
/// reconciliation (session_handlers) groups by the same attribution.
pub(super) async fn task_addressee_id(
    session: &mirri_core::SessionScope,
    task: &mirri_core::task::AgentTask,
) -> String {
    task_dispatcher(session, task)
        .await
        .unwrap_or_else(|| super::DEFAULT_MAIN_AGENT_ID.to_string())
}

/// Resolve one addressee's ledger for the tick (review N4): the scan-based
/// delivered/parked sets, the due set filtered to tasks actually dispatched
/// by this addressee, and the stranded-buffer flag. `None` when the
/// addressee agent is not live (an unresolvable subagent dispatcher's tasks
/// fall back to main, whose own resolution carries them).
async fn resolve_addressee(
    session: &mirri_core::SessionScope,
    steer_buffers: &SteerBufferMap,
    main_agent: &std::sync::Arc<mirri_core::AgentScope>,
    session_id: &str,
    addressee_id: &str,
) -> Option<NotificationAddressee> {
    let (agent, agent_key) = if addressee_id == super::DEFAULT_MAIN_AGENT_ID {
        (
            std::sync::Arc::clone(main_agent),
            format!("{session_id}:{}", super::DEFAULT_MAIN_AGENT_ID),
        )
    } else {
        let agent = session.get_agent(&AgentId::new(addressee_id)).await?;
        (agent, format!("{session_id}:{addressee_id}"))
    };
    // The scan-based ledger (SDR-B2-01): delivered = the context history's
    // background-task origins; parked = the steer buffer's.
    let delivered = {
        let memory = agent.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        delivered_keys(&memory)
    };
    // The opportunistic marker sweep (review M1): a task whose notification
    // is present in this addressee's live context — the busy-path flush
    // lands the message in memory without a marker (park no longer marks) —
    // gets its durable delivery marker set here, within one driver tick, so
    // a later compaction or context rebuild can never re-arm the debt.
    sweep_mark_delivered(&session.tasks(), &delivered);
    let parked = {
        let buffers = steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
        buffers
            .get(&agent_key)
            .map(|buffer| parked_keys(&buffer.lock().unwrap_or_else(|e| e.into_inner())))
            .unwrap_or_default()
    };
    let has_stranded = {
        let buffers = steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
        buffers.get(&agent_key).is_some_and(|buffer| {
            let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
            !buffer.is_empty()
                && buffer
                    .iter()
                    .all(|s| matches!(s.origin, PromptOrigin::BackgroundTask { .. }))
        })
    };
    let mut due = due_notifications(&session.tasks(), &delivered, &parked);
    // Only this addressee's own tasks ride its turn; the rest stay due for
    // their own later tick.
    let mut owned = Vec::with_capacity(due.len());
    for item in due {
        if task_addressee_id(session, &item.task).await == addressee_id {
            owned.push(item);
        }
    }
    due = owned;
    Some(NotificationAddressee {
        agent,
        agent_key,
        due,
        has_stranded,
    })
}

/// Whether any steer buffer in the session holds a stranded
/// (all-notification) buffer — the cheap peek that keeps the fast path
/// from locking context memories when nothing can be due.
fn any_stranded_buffer(steer_buffers: &SteerBufferMap) -> bool {
    let buffers = steer_buffers.lock().unwrap_or_else(|e| e.into_inner());
    buffers.values().any(|buffer| {
        let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
        !buffer.is_empty()
            && buffer
                .iter()
                .all(|s| matches!(s.origin, PromptOrigin::BackgroundTask { .. }))
    })
}

/// Claim a subagent addressee's notification-turn exclusion (review S3 +
/// round 2, F1): resolve the addressee's parent host (the live registry's
/// `subagent.registered` parent link — the same resolution
/// [`task_dispatcher`] applies) and atomically claim the child's running
/// flag on it. `None` when the addressee is currently running a
/// host-driven turn — the caller parks and retries a later tick. An
/// addressee whose parent or host is not resolvable claims trivially (a
/// no-op guard): without a host it has no host-driven runs to exclude
/// against (the restored-children shape, SF-5/O12), so delivery must not
/// stall on it.
async fn claim_subagent_addressee(
    session: &mirri_core::SessionScope,
    addressee_id: &str,
) -> Option<mirri_core::NotificationRunGuard> {
    let Some(parent_id) = session.subagent_parent(addressee_id).await else {
        return Some(mirri_core::NotificationRunGuard::noop());
    };
    let Some(parent_agent) = session.get_agent(&AgentId::new(&parent_id)).await else {
        return Some(mirri_core::NotificationRunGuard::noop());
    };
    let host = {
        let config = parent_agent.config();
        let config = config.read().unwrap_or_else(|e| e.into_inner());
        config.subagent_host.clone()
    };
    match host {
        Some(host) => host.claim_child_notification_turn(addressee_id),
        None => Some(mirri_core::NotificationRunGuard::noop()),
    }
}
