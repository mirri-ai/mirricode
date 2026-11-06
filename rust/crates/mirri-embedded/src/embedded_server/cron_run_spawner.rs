//! Stage-2 batch 4b's workspace run spawner (§8 / SDR-101): what a fired
//! workspace task actually does.
//!
//! Every fire runs the 5-step pipeline the architecture doc prescribes:
//!
//! 1. **Concurrency posture** (D-M3): a `Running` RunRecord for the task →
//!    skip (`RunStatus::Skipped`, "上次运行中" — the fire budget is NOT
//!    consumed, the 4a M1 fix preserved); the queue posture merges into
//!    the single backlogged slot.
//! 2. **Execution session** (B.1): `create_session_lazy` — the SDR-004
//!    lazy shape IS the unattended execution form: no `wire.jsonl`, no
//!    event relay (ADR-006: the metadata ROW does land at creation — its
//!    empty `last_prompt` keeps the session out of default listings). The
//!    session materializes only on the explicit-open path
//!    (`cron.run.open`) — this spawner NEVER calls `note_turn_settled`
//!    (which would force-persist through the metadata write-through + the
//!    index upsert).
//! 3. **Injection** (the `inject_cron_prompt` pipeline family): the fire's
//!    prompt runs as an engine turn with `PromptOrigin::CronJob` and the
//!    posture-mapped `TurnOriginContext::Cron`.
//! 4. **Finale** (C.2/D-M6): the turn's settle records the RunRecord
//!    terminal state + duration + the last-assistant-message truncation as
//!    the run's summary (D-M6's downgraded shape — zero extra LLM calls).
//! 5. **Lazy cleanup** (D-M5 b): the TTL sweep (7 days past the terminal
//!    state) drops never-opened execution sessions; the RunRecord keeps
//!    the summary.
//!
//! Fire-and-forget by design (R1): the executor records `Running` and
//! returns; the turn runs on its own spawned task.
//!
//! **Mode mapping** (batch 2 / A4 — the task's declared mode IS the
//! run's mode, no translation):
//!
//! - `manual` → the agent runs in Manual mode; a run has no live chat, so
//!   an ask parks on the execution session's approval gate and the run
//!   record reads `waiting_approval` (ADR-021) until the user decides in
//!   the run's session;
//! - `auto` → the agent runs in Auto mode (the ordinary session
//!   semantics);
//! - `yolo` → the agent runs in Yolo mode — never asks, everything is
//!   approved;
//! - a pre-batch-2 record (`ask-deny`, or an even older record with no
//!   posture at all) keeps the legacy ask rejection
//!   (`reject_asks`), and `read-only` keeps its write-class refusal.

use std::collections::HashMap;
use std::sync::Arc;

use mirri_core::cron::{OpenRunOutcome, PermissionPosture, RunRecord, RunStatus, TaskConcurrency};

use super::cron_driver::{
    ActivityTrackerMap, CompactionHandleMap, InjectedStatus, SteerBufferMap, TurnFlowMap,
    UsageModelKeyHandleMap, WorkspaceFireExecutor, cron_flow_config,
    inject_cron_prompt_into_session,
};
use super::goal_driver::{ChainResources, GoalChainStateMap};
use crate::LlmFactory;
use crate::embedded_server::EmbeddedServer;
use crate::workspace_registry::WorkspaceRegistry;

/// The run summary cap (D-M6's truncation): the last assistant message
/// truncated to this many characters rides the RunRecord and the terminal
/// notification.
pub const RUN_SUMMARY_MAX_CHARS: usize = 500;

/// How long an unopened execution session outlives its run's terminal
/// state before the sweep drops it (D-M5 b: 7 days — the same magnitude
/// the session-retention reasoning uses).
pub const UNOPENED_RUN_SESSION_TTL: chrono::Duration = chrono::Duration::days(7);

/// The spawner's captured dependencies — the same Arc-cloning stance the
/// scheduler loop itself uses (no `&EmbeddedServer` crosses a spawn).
#[derive(Clone)]
pub(crate) struct WorkspaceRunSpawner {
    pub(crate) registry: Arc<WorkspaceRegistry>,
    pub(crate) turn_flows: TurnFlowMap,
    pub(crate) activity_trackers: ActivityTrackerMap,
    pub(crate) steer_buffers: SteerBufferMap,
    pub(crate) compaction_handles: CompactionHandleMap,
    pub(crate) usage_model_key_handles: UsageModelKeyHandleMap,
    pub(crate) llm_factory: Option<Arc<dyn LlmFactory>>,
    pub(crate) in_flight_turns: super::InFlightTurnsMap,
    // NOTE (ADR-008): no settle-index handle travels with the spawner —
    // a run turn NEVER settles into the session index (the spawner calls
    // `inject_cron_prompt` with `index: None`); the explicit-open path
    // (`cron.run.open`) is the only admission.
    pub(crate) flag_registry: Arc<mirri_core::FlagRegistry>,
    pub(crate) goal_chain_states: GoalChainStateMap,
    pub(crate) goal_chain_resources: ChainResources,
    pub(crate) provider_auth_failures: Arc<std::sync::Mutex<HashMap<String, String>>>,
    pub(crate) server_config: Arc<std::sync::RwLock<mirri_core::ServerConfig>>,
    pub(crate) profile_manager: Option<Arc<mirri_core::ProfileManager>>,
    pub(crate) turn_stall_timeout: std::time::Duration,
    pub(crate) turn_backstop_timeout: std::time::Duration,
    pub(crate) tool_result_home: Option<std::path::PathBuf>,
}

impl EmbeddedServer {
    /// Build the workspace run spawner over the server's own fields (the
    /// run-now face's path — the scheduler loop builds the same shape from
    /// its captured Arcs at spawn time).
    pub(super) fn workspace_run_spawner(&self) -> WorkspaceRunSpawner {
        WorkspaceRunSpawner {
            registry: Arc::clone(&self.registry),
            turn_flows: Arc::clone(&self.turn_flows),
            activity_trackers: Arc::clone(&self.activity_trackers),
            steer_buffers: Arc::clone(&self.steer_buffers),
            compaction_handles: Arc::clone(&self.compaction_handles),
            usage_model_key_handles: Arc::clone(&self.usage_model_key_handles),
            llm_factory: self.llm_factory.clone(),
            in_flight_turns: Arc::clone(&self.in_flight_turns),
            flag_registry: Arc::clone(self.flag_registry()),
            goal_chain_states: Arc::clone(&self.goal_chain_states),
            goal_chain_resources: self.goal_chain_resources(),
            provider_auth_failures: Arc::clone(&self.provider_auth_failures),
            server_config: Arc::clone(&self.config),
            profile_manager: self.profile_manager.clone(),
            turn_stall_timeout: self.turn_stall_timeout,
            turn_backstop_timeout: self.turn_backstop_timeout,
            tool_result_home: self.home_dir().map(ToOwned::to_owned),
        }
    }
}

impl WorkspaceFireExecutor for WorkspaceRunSpawner {
    fn execute(
        &self,
        workspace_id: &str,
        task_id: &str,
        trigger: mirri_core::cron::RunTrigger,
        scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        // The blocking-store reads/writes ride the shared runtime (the
        // B1 seam's async wrapper — the spawn's body is the async pipeline).
        let spawner = self.clone();
        let (workspace_id, task_id) = (workspace_id.to_string(), task_id.to_string());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                spawner
                    .spawn(workspace_id, task_id, trigger, scheduled_at)
                    .await;
            });
        }
    }
}

/// The Queue-posture drain gateway: a `Send` re-entry into the spawner's
/// pipeline for the queued fire that survived the current run's terminal
/// state (the recursive `.await` inside the spawned finale would make the
/// outer future non-Send; this executor face is the Send-clean seam the
/// drain rides).
struct QueueDrainExecutor {
    spawner: WorkspaceRunSpawner,
}

impl QueueDrainExecutor {
    fn execute(&self, workspace_id: &str, task_id: &str, run_id: &str) {
        let spawner = self.spawner.clone();
        let (workspace_id, task_id, run_id) = (
            workspace_id.to_string(),
            task_id.to_string(),
            run_id.to_string(),
        );
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                spawner
                    .spawn_inner(
                        workspace_id,
                        task_id,
                        Some(run_id),
                        mirri_core::cron::RunTrigger::QueueDrain,
                        None,
                    )
                    .await;
            });
        }
    }
}

impl WorkspaceRunSpawner {
    /// The §8 pipeline. Fire-and-forget (R1): records `Running` (or the
    /// concurrency skip) and returns; the turn runs on its own task.
    /// `queued_run_id` (Some): the drain side of the Queue posture — the
    /// run already holds a queued record with this id, so the opener step
    /// re-opens THAT record instead of minting a new one.
    pub(crate) async fn spawn(
        &self,
        workspace_id: String,
        task_id: String,
        trigger: mirri_core::cron::RunTrigger,
        scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.spawn_inner(workspace_id, task_id, None, trigger, scheduled_at)
            .await;
    }

    async fn spawn_inner(
        &self,
        workspace_id: String,
        task_id: String,
        queued_run_id: Option<String>,
        trigger: mirri_core::cron::RunTrigger,
        scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let Some(runtime) = self.registry.built_runtime(&workspace_id) else {
            return;
        };
        let Some(cron) = runtime.cron() else {
            return;
        };
        let Some(task) = cron.get(&task_id) else {
            return;
        };

        // 1. Concurrency posture (D-M3) + M4 (TOCTOU): the has-Running
        // check and the Running opener land in ONE manager-level critical
        // section (`try_open_run` holds the runs mutex across the check,
        // the durable write, and the mirror) — a run-now trigger racing a
        // scheduler tick can never double-open the task. The opener's
        // run_id is the run's identity: the same id rides the terminal
        // record (the opener and the finale are one run). A drained
        // queued run SKIPS the opener step entirely — `drain_queued_run`
        // already re-opened its own record as Running (calling
        // `try_open_run` again would append a duplicate opener).
        let started_at = chrono::Utc::now();
        let opened_run_id = if let Some(run_id) = queued_run_id {
            run_id
        } else {
            let run_id = format!("run-{}", uuid::Uuid::new_v4());
            let queue = task.concurrency == TaskConcurrency::Queue;
            let opener = RunRecord {
                run_id: run_id.clone(),
                status: RunStatus::Running,
                started_at,
                ended_at: None,
                duration_ms: None,
                usage: None,
                session_ref: None,
                skip_reason: None,
                asked_ops: Vec::new(),
                trigger: Some(trigger),
                scheduled_at,
            };
            match cron.try_open_run(&task_id, queue, opener).await {
                OpenRunOutcome::Opened(run_id) => run_id,
                OpenRunOutcome::Queued(_run_id) => {
                    // The Queue posture parked this fire in the single
                    // backlog slot (merged one-deep): do NOT execute now —
                    // the terminal side of the current run drains it once.
                    return;
                }
                OpenRunOutcome::Declined(mut skip) => {
                    // The Skip posture: record and leave (the fire budget is
                    // NOT consumed — record_run only counts non-Skipped
                    // terminal states, the 4a M1 fix preserved).
                    skip.status = RunStatus::Skipped;
                    skip.skip_reason = Some("上次运行中（并发姿态：跳过）".to_string());
                    skip.ended_at = Some(chrono::Utc::now());
                    skip.duration_ms = Some(0);
                    cron.record_run(&task_id, skip).await;
                    return;
                }
            }
        };

        // 2. The execution session (B.1): the SDR-004 lazy shape. Created
        // in the task's OWN workspace; never relayed, never indexed.
        // ADR-019 / VU14 (review Major 3): the task's workdir must
        // ACTUALLY constrain the run — the tools' cwd comes from the
        // session's workspace sandbox, not `SessionMetadata.work_dir`
        // (ownership/display). The workdir is resolved against the
        // workspace root FIRST: an inside path roots the execution
        // session's sandbox there through the create; anything else
        // (escape, `..`, an absolute path outside) refuses the run — a
        // model-proposed workdir must never widen the sandbox. `None` =
        // the workspace default (the existing behavior).
        let workdir = task
            .workdir
            .as_deref()
            .filter(|dir| !dir.trim().is_empty())
            .map(str::to_string);
        let workspace_root = runtime.scope.cwd();
        let sandbox_root = match workdir.as_deref() {
            None => None,
            Some(workdir) => {
                match mirri_core::tools::WorkspacePaths::new(workspace_root.to_path_buf())
                    .validate(std::path::Path::new(workdir))
                {
                    Ok(resolved) => Some(resolved),
                    Err(_) => {
                        tracing::warn!(
                            %task_id,
                            workdir,
                            "workspace cron run refused: workdir escapes the workspace root"
                        );
                        cron.record_run(
                            &task_id,
                            RunRecord {
                                run_id: opened_run_id,
                                status: RunStatus::Failed,
                                started_at: chrono::Utc::now(),
                                ended_at: Some(chrono::Utc::now()),
                                duration_ms: Some(0),
                                usage: None,
                                session_ref: None,
                                skip_reason: Some(format!(
                                    "工作目录越界（必须位于工作区内）：{workdir}"
                                )),
                                asked_ops: Vec::new(),
                                trigger: None,
                                scheduled_at: None,
                            },
                        )
                        .await;
                        return;
                    }
                }
            }
        };
        let manager = Arc::clone(&runtime.manager);
        let metadata = mirri_core::SessionMetadata {
            title: run_session_title(task.title.as_deref(), &task.prompt, started_at),
            is_custom_title: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tags: std::collections::HashSet::from([format!("cron-task:{}", task.id)]),
            archived: false,
            work_dir: workdir,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
            last_used_at: None,
        };
        let scope_config = {
            let snapshot = self.server_config.read().unwrap_or_else(|e| e.into_inner());
            mirri_core::session_scope_config_from(&snapshot)
        };
        // ADR-008 / ADR-006 Task 13: the execution session is ROW-EXEMPT —
        // no metadata row until the user explicitly opens the run
        // (materialization writes it; the routing note below keeps the
        // session reachable for approval in the meantime).
        let session: Option<Arc<mirri_core::SessionScope>> = match manager
            .create_session_lazy_with_root_ex(metadata, scope_config, sandbox_root, true)
            .await
        {
            Ok(session) => {
                // ADR-021 (manual runs' approval face): note the lazy
                // execution session as ACTIVE in its workspace so the
                // approval RPCs (`approval.listPending` / `approval.decide`)
                // can route to it while its turn is parked on the gate.
                // This is the ROUTING table only — the session list and
                // the index stay flat (ADR-008's explicit-open admission is
                // untouched; `note_active` never writes the index).
                self.registry
                    .note_active(session.id().as_str(), &workspace_id);
                Some(session)
            }
            Err(_) => None::<Arc<mirri_core::SessionScope>>,
        };
        let Some(session) = session else {
            tracing::warn!(%task_id, "workspace cron run failed: execution session create failed");
            // The opener's run converges honestly: the session-create
            // failure is the run's failure (never a wedged Running).
            cron.record_run(
                &task_id,
                RunRecord {
                    run_id: opened_run_id,
                    status: RunStatus::Failed,
                    started_at: chrono::Utc::now(),
                    ended_at: Some(chrono::Utc::now()),
                    duration_ms: Some(0),
                    usage: None,
                    session_ref: None,
                    skip_reason: Some("执行会话创建失败".to_string()),
                    asked_ops: Vec::new(),
                    trigger: Some(trigger),
                    scheduled_at,
                },
            )
            .await;
            return;
        };
        let session_id = session.id().as_str().to_string();
        // Stamp the session ref onto the SAME run record (the opener
        // opened without one — the session did not exist yet): a full
        // re-record is the store's append discipline's only write face
        // for a status-carrying update, so the opener slot is updated
        // through the manager's store-mutating seam rather than a second
        // append (a duplicate Running would break the concurrency check).
        cron.set_run_session_ref(&task_id, &opened_run_id, &session_id)
            .await;

        // 3-4. The injection + finale travel on their own task. The
        // session Arc travels with it (ADR-008: the execution session is
        // NOT in the routing tables — the active table and the index only
        // learn it on the explicit-open path — so `resolve_session_via`
        // would fail; the create returned the authoritative Arc). The
        // SAME run id rides the terminal record (the opener and the
        // finale are one run — review Minor's unified run_id).
        let spawner = self.clone();
        let task = task.clone();
        let cron = Arc::clone(cron);
        let workspace_id = workspace_id.clone();
        let run_id = opened_run_id;
        // ADR-021: the run-level approval-wait signal — the turn's parked
        // ask flips this run's record to WaitingApproval (and back to
        // Running when the wait resolves, any exit included). Fire-and-
        // forget: the hook never blocks the authorize path.
        let run_hook: mirri_core::turn_flow::ApprovalWaitHook = {
            let cron = Arc::clone(&cron);
            let (task_id, run_id) = (task.id.clone(), run_id.clone());
            std::sync::Arc::new(move |waiting: bool| {
                let (cron, task_id, run_id) = (Arc::clone(&cron), task_id.clone(), run_id.clone());
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let status = if waiting {
                            mirri_core::cron::RunStatus::WaitingApproval
                        } else {
                            mirri_core::cron::RunStatus::Running
                        };
                        cron.set_run_status(&task_id, &run_id, status).await;
                    });
                }
            })
        };
        tokio::spawn(async move {
            // ADR-021's launch record rides the terminal too (Copy pair).
            let (trigger, scheduled_at) = (trigger, scheduled_at);
            let started = std::time::Instant::now();
            let outcome = spawner.run_turn(session, &task, run_hook).await;
            let (status, usage, summary, skip_reason, asked_ops) = outcome;
            let duration_ms = started.elapsed().as_millis() as u64;
            let ended_at = chrono::Utc::now();
            let mut record = RunRecord {
                run_id: run_id.clone(),
                status,
                started_at: ended_at - chrono::Duration::milliseconds(duration_ms.max(1) as i64),
                ended_at: Some(ended_at),
                duration_ms: Some(duration_ms),
                usage,
                session_ref: Some(session_id.clone()),
                skip_reason,
                // V15 (stage-2 D.2): the run's asked-ops trace (D-M2's
                // leave-a-trace contract — 哪次运行、哪个操作、结果).
                asked_ops,
                trigger: Some(trigger),
                scheduled_at,
            };
            // D-M6's summary: the last assistant message, truncated —
            // carried in the record's usage slot as a sibling key (the
            // RunRecord's own shape has no dedicated field; the summary
            // rides `usage`'s JSON object under `summary`).
            if let Some(summary) = summary {
                let usage = record.usage.get_or_insert_with(|| serde_json::json!({}));
                if let Some(map) = usage.as_object_mut() {
                    map.insert("summary".to_string(), serde_json::json!(summary));
                }
            }
            // The terminal record REPLACES the opener (the same run id —
            // a plain append would leave the stale Running behind and
            // wedge the concurrency check). After the terminal lands, the
            // Queue posture's single backlog slot drains once (D-M3's
            // queue: one queued execution, merged — the drain re-opens
            // the queued run id as this executor's next spawn).
            cron.replace_run(&task.id, record).await;
            if task.concurrency == TaskConcurrency::Queue
                && let Some((task_id, run_id)) = cron.drain_queued_run(&task.id).await
            {
                // The drained queued fire runs as its OWN spawned task (a
                // recursive `.await` would make this future non-Send) —
                // re-entered through the executor face, which IS Send
                // (the captured-Arcs shape), running the same pipeline.
                let executor = QueueDrainExecutor {
                    spawner: spawner.clone(),
                };
                let workspace_id = workspace_id.clone();
                tokio::spawn(async move {
                    executor.execute(&workspace_id, &task_id, &run_id);
                });
            }
        });
    }

    /// Steps 3-4: the execution turn + the terminal outcome bundle. The
    /// execution session arrives as the create-returned Arc (it is not in
    /// the routing tables — see `spawn`).
    async fn run_turn(
        &self,
        session: Arc<mirri_core::SessionScope>,
        task: &mirri_core::cron::CronTask,
        run_hook: mirri_core::turn_flow::ApprovalWaitHook,
    ) -> (
        RunStatus,
        Option<serde_json::Value>,
        Option<String>,
        Option<String>,
        Vec<serde_json::Value>,
    ) {
        let session_id = session.id().as_str().to_string();
        let readonly = task.permission_posture == Some(PermissionPosture::ReadOnly);
        let fire = mirri_core::cron::CronFire {
            task_id: task.id.clone(),
            prompt: task.prompt.clone(),
            cron: task.cron.clone(),
            recurring: task.recurring,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly,
            scheduled_at: None,
            late_by_ms: None,
            scope: task.scope.clone(),
            ended: false,
        };
        // Mode mapping (batch 2 / A4): the task's mode is the run's mode.
        // A run is always UNATTENDED (no live chat client watches it); the
        // mode decides what that means — manual parks asks on the
        // approval gate (the A13 channel), auto approves as usual, yolo
        // never asks. A pre-batch-2 record without a mode reads back its
        // legacy posture: ask-deny keeps the rejection, everything else
        // behaves as before.
        let runtime_mode = match task.permission_mode {
            Some(declared) => match declared {
                mirri_core::cron::PermissionMode::Manual => {
                    mirri_core::permission::PermissionMode::Manual
                }
                mirri_core::cron::PermissionMode::Auto => {
                    mirri_core::permission::PermissionMode::Auto
                }
                mirri_core::cron::PermissionMode::Yolo => {
                    mirri_core::permission::PermissionMode::Yolo
                }
            },
            None => match task.permission_posture {
                Some(posture) => {
                    // The engine-side mode is the same three names.
                    let mode = mirri_core::cron::PermissionMode::from_posture_record(posture);
                    match mode {
                        mirri_core::cron::PermissionMode::Manual => {
                            mirri_core::permission::PermissionMode::Manual
                        }
                        mirri_core::cron::PermissionMode::Auto => {
                            mirri_core::permission::PermissionMode::Auto
                        }
                        mirri_core::cron::PermissionMode::Yolo => {
                            mirri_core::permission::PermissionMode::Yolo
                        }
                    }
                }
                // A pre-A4 record with NO posture keeps the agent's own
                // (Manual) mode for the turn, but the `reject_asks` bit
                // below still REJECTS its asks — the old ask-deny ruling
                // wins, so the run never parks an ask on the gate.
                None => mirri_core::permission::PermissionMode::Manual,
            },
        };
        // A pre-batch-2 record (no declared mode) keeps its old ask rule:
        // the legacy postures reject, and a pre-A4 record with NO posture
        // rejects too (its Manual mode never parks the ask — this bit is
        // what refuses it).
        let reject_asks = task.permission_mode.is_none()
            && matches!(
                task.permission_posture,
                Some(PermissionPosture::AskDeny) | Some(PermissionPosture::ReadOnly) | None
            );
        // The lazy execution session has NO agents (SDR-004 defers
        // everything) — the 4b defect's second half: the injection's
        // `get_agent(main)` answered None and dropped the turn. The run's
        // main agent is created HERE, the same shape the session-create
        // path builds: the config's default model (the spawner's create
        // already required one to exist), the essential profile's tool
        // whitelist, and the posture-mapped permission mode.
        let main_agent_id = mirri_core::AgentId::new(super::DEFAULT_MAIN_AGENT_ID);
        if session.get_agent(&main_agent_id).await.is_none() {
            // ADR-019 / VU14: the model preference order — the task's own
            // `model` alias (the same slot `session.create.model_id`
            // fills), then the config's `default_model`. An
            // explicit-but-EMPTY task model falls through to the default
            // (the wire's "no preference" spelling).
            let model_alias = task
                .model
                .clone()
                .filter(|model| !model.trim().is_empty())
                .or_else(|| {
                    let config = self.server_config.read().unwrap_or_else(|e| e.into_inner());
                    config.default_model.clone()
                })
                .unwrap_or_default();
            let mut agent_config = mirri_core::AgentScopeConfig {
                model_id: model_alias,
                permission_mode: runtime_mode,
                ..mirri_core::AgentScopeConfig::default()
            };
            // ADR-019 / VU11: the task's thinking effort fills the same
            // `AgentScopeConfig.thinking_effort` slot the session create
            // path reads; `None` = the model catalog's default.
            agent_config.thinking_effort = task.effort.clone();
            if let Some(profile_manager) = &self.profile_manager
                && let Ok(profile_config) = profile_manager
                    .resolve(mirri_core::ESSENTIAL_PROFILE_NAME, &mirri_os::LocalSyncFs)
            {
                if agent_config.enabled_tools.is_none() {
                    agent_config.enabled_tools = profile_config.enabled_tools;
                }
                if agent_config.role_additional.is_none() {
                    agent_config.role_additional = profile_config.role_additional;
                }
            }
            let agent = session
                .create_agent(main_agent_id.clone(), agent_config)
                .await
                .map_err(|e| {
                    tracing::warn!(task_id = %task.id, error = %e, "cron run: main agent create failed");
                    e
                })
                .ok();
            if agent.is_none() {
                return (
                    RunStatus::Failed,
                    None,
                    None,
                    Some("执行会话主代理创建失败".to_string()),
                    Vec::new(),
                );
            }
        }
        // The agent's mode IS the task's mode (A4) — the lazy session may
        // have carried a pre-existing agent, so stamp it explicitly too.
        if let Some(agent) = session.get_agent(&main_agent_id).await {
            agent.set_permission_mode(runtime_mode);
        }
        let flow_config =
            cron_flow_config(&session, super::DEFAULT_MAIN_AGENT_ID, &self.server_config).await;
        // D.2 m4 review fix: the spawner injects the ALREADY-HELD session
        // Arc — the lazy execution session never enters the routing tables
        // (ADR-008), so the resolve-based face would answer `Injected`
        // without ever running the turn (the 4b defect).
        let status = inject_cron_prompt_into_session(
            &self.registry,
            Arc::clone(&session),
            &self.turn_flows,
            &self.activity_trackers,
            &self.steer_buffers,
            &self.compaction_handles,
            &self.usage_model_key_handles,
            &self.llm_factory,
            &self.in_flight_turns,
            // ADR-008: the run turn never settles into the index.
            None,
            &self.flag_registry,
            &self.goal_chain_states,
            &self.goal_chain_resources,
            &self.provider_auth_failures,
            &session_id,
            super::DEFAULT_MAIN_AGENT_ID,
            flow_config,
            fire,
            reject_asks,
            // A workspace run: nobody watches its chat, so a manual ask
            // parks on the gate (A4 / ADR-021).
            true,
            self.turn_stall_timeout,
            self.turn_backstop_timeout,
            self.tool_result_home.clone(),
            Some(run_hook),
        )
        .await;
        // V15 (stage-2 D.2): drain the run turn's asked-ops ledger — the
        // execution flow records every non-approved permission outcome
        // (which operation, what result) into it; the RunRecord's
        // `asked_ops` slot is this trace. The ledger lives on the flow in
        // the server's flow map (the same handle the injection path
        // get-or-created).
        let asked_ops = {
            let key = format!("{}:{}", session_id, super::DEFAULT_MAIN_AGENT_ID);
            self.turn_flows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .map(|flow| {
                    flow.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take_asked_ops()
                })
                .unwrap_or_default()
        };
        match status {
            InjectedStatus::Injected => {
                // ADR-021's truthful success: Succeeded means the turn
                // ended normally AND said something — an injection that
                // produced no assistant output is a failed run, not a
                // quiet success (the 2026-11-04 10:22 run: 137s, zero
                // output, recorded green). The session is fresh per fire,
                // so the last assistant message IS this run's output.
                let summary = last_assistant_summary(&session).await;
                match summary {
                    Some(summary) => (RunStatus::Succeeded, None, Some(summary), None, asked_ops),
                    None => (
                        RunStatus::Failed,
                        None,
                        None,
                        Some("回合结束但没有任何助手产出".to_string()),
                        asked_ops,
                    ),
                }
            }
            // ADR-021: the turn itself failed (engine error) — the
            // injection landed, the run did not complete.
            InjectedStatus::TurnFailed(reason) => (
                RunStatus::Failed,
                None,
                None,
                Some(format!("回合失败：{reason}")),
                asked_ops,
            ),
            // ADR-019 / VU14: an early-exit failure names the model the
            // task pinned when one exists — the honest "选 A 不跑 B" trail
            // (the injection path NEVER falls back to a default model: a
            // task model that cannot be built fails the run outright).
            InjectedStatus::Dropped => {
                let skip_reason = task
                    .model
                    .as_deref()
                    .map(|model| format!("模型解析失败：{model}"));
                (RunStatus::Failed, None, None, skip_reason, asked_ops)
            }
        }
    }
}

/// The execution session's title (ADR-021): the task's own title (or,
/// when it has none, the prompt's first line), single-line, with the
/// run's local date — so two runs of the same task are tellable apart in
/// the session tree.
fn run_session_title(
    task_title: Option<&str>,
    prompt: &str,
    started: chrono::DateTime<chrono::Utc>,
) -> String {
    const MAX_CHARS: usize = 60;
    let base = task_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
        });
    let base: String = base.split_whitespace().collect::<Vec<_>>().join(" ");
    let base: String = base.chars().take(MAX_CHARS).collect();
    let when = started.with_timezone(&chrono::Local).format("%m-%d %H:%M");
    format!("{base} · {when}")
}

/// The D-M6 run summary: the session's LAST assistant message truncated
/// to [`RUN_SUMMARY_MAX_CHARS`] — the notification's carried content (the
/// model's own words, no template regeneration).
async fn last_assistant_summary(session: &Arc<mirri_core::SessionScope>) -> Option<String> {
    let agent = session
        .get_agent(&mirri_core::AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await?;
    let memory = agent.context_memory();
    let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
    let text = mem
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == mirri_core::Role::Assistant)
        .map(|message| message.text_content())?;
    let truncated: String = if text.chars().count() > RUN_SUMMARY_MAX_CHARS {
        text.chars().take(RUN_SUMMARY_MAX_CHARS).collect()
    } else {
        text
    };
    (!truncated.trim().is_empty()).then_some(truncated)
}

#[cfg(test)]
mod title_tests {
    use super::run_session_title;

    /// Given a task with a title, when the run session is titled, then the
    /// title leads and the run's local date follows.
    #[test]
    fn should_title_the_run_session_with_the_task_title_when_one_exists() {
        let at = "2026-11-04T01:22:57Z".parse().unwrap();
        let title = run_session_title(Some("Pick Up Review tickets"), "ignored prompt", at);
        assert!(title.starts_with("Pick Up Review tickets · "), "{title}");
        assert!(!title.contains('\n'));
    }

    /// Given a task without a title, when the run session is titled,
    /// then the prompt's first line (single-line, truncated) stands in.
    #[test]
    fn should_fall_back_to_the_prompt_first_line_when_the_task_has_no_title() {
        let at = "2026-11-04T01:22:57Z".parse().unwrap();
        let title = run_session_title(None, "You are an agent.\n\nStep 1 — Fetch", at);
        assert!(title.starts_with("You are an agent. · "), "{title}");
        assert!(!title.contains('\n'));
    }

    /// Given an overlong first line, when titled, then it is truncated.
    #[test]
    fn should_truncate_an_overlong_title_when_the_first_line_is_long() {
        let at = "2026-11-04T01:22:57Z".parse().unwrap();
        let long = "x".repeat(200);
        let title = run_session_title(None, &long, at);
        assert!(title.chars().count() < 100, "{title}");
    }
}
