//! The per-session cron driver: the production caller of
//! [`CronManager::check_due`](mirri_core::cron::CronManager::check_due).
//!
//! `check_due` itself only detects and fires the due tasks (publishing the
//! `cron.fired` event on the session's bus); the driver owns the cadence —
//! a coarse interval tick per live session — and injects every fired
//! prompt into the session's canonical main agent (first-agent fallback)
//! as an engine turn with the `cron_job` prompt origin (mirroring the TS
//! kernel's `turn.steer`-style cron delivery).
//!
//! The driver exits when its session disappears (closed or archived): each
//! tick re-resolves the session and breaks when no workspace owns it
//! anymore. Sessions are unique per process, so a stale driver can never
//! attach to a successor.
//!
//! The session resolves through the W3 routing seam (`session_routing`) —
//! the driver holds the workspace registry, not a single manager, so a
//! non-default workspace's session keeps ticking.

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
pub(super) type ActivityTrackerMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<mirri_core::turn_flow::ActivityTracker>>>>;

/// The embedded server's per-agent compaction-handle map (the same shape
/// [`EmbeddedServer::compaction_handles`](super::EmbeddedServer) owns) —
/// registered here so the driver-created flows answer the mid-turn model
/// switch's window push.
pub(super) type CompactionHandleMap = Arc<
    std::sync::Mutex<
        HashMap<String, Arc<std::sync::Mutex<mirri_core::compaction::FullCompaction>>>,
    >,
>;

/// The embedded server's per-agent usage-model-key-handle map (the same
/// shape [`EmbeddedServer::usage_model_key_handles`](super::EmbeddedServer)
/// owns) — registered here so the driver-created flows answer the mid-turn
/// model switch's usage re-attribution (spec 11 A13).
pub(super) type UsageModelKeyHandleMap =
    Arc<std::sync::Mutex<HashMap<String, super::UsageModelKeyHandle>>>;

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
            session_id: String,
            #[serde(alias = "taskId")]
            task_id: String,
        }
        let params: CronTriggerParams = serde_json::from_value(super::normalize_params(params))
            .map_err(|e| super::invalid_params(e.to_string()))?;
        let session = resolve_session_via(&self.registry, &params.session_id)
            .await
            .map_err(|_| super::not_found(format!("session not found: {}", params.session_id)))?;
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
                        "no agent to run the cron task in: {}",
                        params.session_id
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
        let session_id = params.session_id.clone();
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
                &session_id,
                &target_agent_id,
                flow_config,
                fire,
                turn_stall_timeout,
                turn_backstop_timeout,
            )
            .await;
        });
        super::responses::to_result(super::responses::CronTriggerResult {
            triggered: true,
            task_id: params.task_id,
        })
    }

    /// Start the cron driver for one live session. Idempotent per tick
    /// state — the driver owns no registry entry; it exits by itself when
    /// the session disappears.
    pub(super) fn spawn_cron_driver(&self, session_id: &str) {
        // The routing seam (W3): hold the workspace registry, not one
        // manager — every tick re-resolves the session through it.
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
        // The cron-injected turns register in the shared in-flight registry
        // (the `message.abort` path), keyed like the dispatch turns.
        let in_flight_turns: InFlightTurnsMap = Arc::clone(&self.in_flight_turns);
        let tick = self.cron_tick;
        // The turn-settle write-through (ADR-009): the driver task holds no
        // `&EmbeddedServer` — the shared index travels with it so each
        // injected turn's settle can refresh the session.
        let settle_index = self.session_index();
        let flag_registry = Arc::clone(self.flag_registry());
        // The goal continuation decision runs after each cron turn's
        // settle — the driver task holds no `&EmbeddedServer`, so the
        // chain-state map travels with it as a cloned field Arc.
        let goal_chain_states = Arc::clone(&self.goal_chain_states);
        // The chain's injection resources, snapshotted before the spawn
        // (the driver task holds no `&EmbeddedServer` — the field Arcs
        // travel with it, the same stance as the captures above).
        let goal_chain_resources = self.goal_chain_resources();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            // The first tick completes immediately; a due task fires on
            // the second — acceptable for a minute-granularity scheduler.
            // Fires held while the target agent's turn was running (B2 T9,
            // TS "held until idle"): the cursor already advanced and the
            // `cron.fired` event already published, so the fire itself is
            // only deferred — injected on the first idle tick, oldest
            // first. Cron fires are NEVER folded into the running turn
            // (unlike notifications) — the TS scheduler holds them.
            let mut held: Vec<mirri_core::cron::CronFire> = Vec::new();
            loop {
                interval.tick().await;
                let Ok(session) = resolve_session_via(&registry, &sid).await else {
                    // The session closed or was archived — stop driving.
                    break;
                };
                // check_due publishes the `cron.fired` events on the
                // session bus (the manager's wired-in bus) and returns the
                // fired prompts for injection.
                let due = session.cron().check_due().await;
                held.extend(due);
                if held.is_empty() {
                    continue;
                }
                // Idle-only delivery (TS sessionCronServiceImpl): while
                // the target agent's turn runs, hold — the flow mutex is
                // held for the whole turn by the engine loop, so lock
                // contention is exactly "a turn is running". The target is
                // the session's canonical main agent (the same pin the
                // notification driver and the steer recording use — the
                // pre-B2 "first agent of a HashMap iteration" caliber was
                // seed-nondeterministic in multi-agent sessions).
                //
                // Held-fires-on-close semantics (review M2, TS parity):
                // `check_due` already advanced the cursor durably and
                // published the `cron.fired` event, so a session that closes
                // with fires still held drops the prompt injection — the
                // TS scheduler holds in service memory the same way. A
                // one-shot that comes due in a long busy window and never
                // sees an idle tick is lost permanently by design.
                let target_agent_id = match session
                    .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
                    .await
                {
                    Some(_) => super::DEFAULT_MAIN_AGENT_ID.to_string(),
                    None => {
                        match session.list_agents().await.into_iter().next() {
                            Some(agent_id) => agent_id.as_str().to_string(),
                            // No agents at all: keep holding (the inject
                            // path would drop the fire with a warning).
                            None => continue,
                        }
                    }
                };
                let busy = turn_flows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&format!("{sid}:{target_agent_id}"))
                    .is_some_and(|flow| flow.try_lock().is_err());
                if busy {
                    continue;
                }
                for fire in std::mem::take(&mut held) {
                    // The turn-flow config is built at the dispatch site
                    // (the spawned task owns the shared server config, not
                    // the server itself): the cron target agent's model
                    // id, read from its scope config, combines with the
                    // `[loop_control]` snapshot and the model's catalog
                    // window — the dispatch-site equivalent of
                    // `EmbeddedServer::turn_flow_config_for`.
                    let flow_config =
                        cron_flow_config(&session, &target_agent_id, &server_config).await;
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
                        &sid,
                        &target_agent_id,
                        flow_config,
                        fire,
                        turn_stall_timeout,
                        turn_backstop_timeout,
                    )
                    .await;
                }
            }
        });
    }
}

/// The embedded server's per-agent shared-steer-buffer map (the same shape
/// [`EmbeddedServer::steer_buffers`](super::EmbeddedServer) owns).
pub(super) type SteerBufferMap =
    Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<SteerBuffer>>>>>;

/// The turn-flow config for the cron target agent: the model id is read
/// from the caller-resolved target agent's scope config (the driver pins
/// the canonical main agent, falling back to the session's first agent)
/// and combined with the server's `[loop_control]` snapshot and the
/// model's catalog window. An unresolvable session/agent yields the
/// unknown-window config — ratio triggers stay off until an overflow
/// teaches the engine the real limit (and the injection drops anyway).
async fn cron_flow_config(
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

/// Inject one fired cron prompt into the session's target agent (the
/// caller-resolved pin — canonical main agent, first-agent fallback) as an
/// engine turn — a lean mirror of `message.send`'s wait path with the
/// `cron_job` prompt origin (the TS kernel delivers cron prompts as
/// user-role context records carrying the cron-job origin, then runs the
/// agent turn).
///
/// Fire-and-forget and best-effort: a missing provider or agent logs a
/// warning and drops the injection (the `cron.fired` event already reached
/// the subscribers; the next driver tick reschedules per the task's
/// expression).
#[allow(clippy::too_many_arguments)]
async fn inject_cron_prompt(
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
    session_id: &str,
    target_agent_id: &str,
    flow_config: mirri_core::TurnFlowConfig,
    fire: mirri_core::cron::CronFire,
    turn_stall_timeout: std::time::Duration,
    turn_backstop_timeout: std::time::Duration,
) {
    let Ok(session) = resolve_session_via(registry, session_id).await else {
        return;
    };
    // The caller resolved the target (the canonical main agent, falling
    // back to the first agent for agent-less-provisioned sessions) — the
    // injection and its idle gate key the same flow.
    let agent_id = target_agent_id.to_string();
    let Some(agent) = session.get_agent(&AgentId::new(agent_id.as_str())).await else {
        tracing::warn!(session_id, agent_id = %agent_id, "cron fire dropped: agent vanished");
        return;
    };
    let Some(llm_factory) = llm_factory else {
        tracing::warn!(session_id, "cron fire dropped: no LLM provider configured");
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
                tracing::warn!(session_id, %reason, "cron fire dropped: LLM creation failed");
                return;
            }
        }
    };

    // The cron-job origin object the TS kernel's `CronJobOrigin` declares:
    // the firing task's identity plus the real coalesced/stale values
    // (B5) — the prompt itself is injected as the engine turn's content
    // wrapped in the `<cron-fire>` envelope (B2 T9: the model reads the
    // firing context, the web client peels it back with
    // `extractCronPrompt`).
    let origin = json!({
        "kind": "cron_job",
        "jobId": fire.task_id,
        "cron": fire.cron,
        "recurring": fire.recurring,
        "coalescedCount": fire.coalesced_count,
        "stale": fire.stale,
    });
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
    let Ok(artifacts) = resolve_artifacts_via(registry, session_id) else {
        return;
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
        if !flows.contains_key(&key) {
            let mut turn_flow = mirri_core::TurnFlow::new(flow_config);
            turn_flow.set_event_bus(Arc::clone(&artifacts.event_bus));
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
                })
                .await;
            artifacts.event_bus.publish(SessionEvent::TurnCompleted {
                agent_id: agent_id.clone(),
                session_id: session_id.to_string(),
                turn_id,
                stop_reason: turn_stop_reason_wire(&turn_end.stop_reason).to_string(),
                wire_turn_id: turn_end.turn_id,
                duration_ms: Some(turn_end.duration_ms),
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
            artifacts.event_bus.publish(SessionEvent::TurnFailed {
                agent_id: agent_id.clone(),
                turn_id,
                error: error.message(),
                error_code: error.domain_code().to_string(),
                wire_turn_id: 0,
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
        }
    }
}
