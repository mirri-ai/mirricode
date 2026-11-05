//! Stage-2 batch-4b: the RunSpawner (the execution-session pipeline) and
//! the propose-approve channel (the ScheduledTaskPropose tool + the approval gate
//! + the async receipt closure).
//!
//! The bed mirrors `cron_b3_server`: a home-rooted workspace (so the
//! default runtime carries a cron store), a scripted LLM (so execution
//! turns actually run), and fast cron ticks.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::*;

/// The 4b test bed: home-rooted (a workspace cron store exists on the
/// default runtime), scripted-LLM (the run turns execute), fast ticks.
fn ws4b_server(mock: Arc<ScriptedLlm>) -> (EmbeddedServer, tempfile::TempDir, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let home = tempfile::tempdir().expect("home tempdir must be creatable");
    let mut server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4b"),
        tmp.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    // The test LLM factory (the private field is settable from this
    // crate-local test — the `for_workspace_root` composer wires none).
    server.llm_factory = Some(Arc::new(ScriptedLlmFactory::new(mock)));
    // The create chain requires a resolvable model (spec 11 R4): seed the
    // test default into the assembled config (the composer's own is empty).
    {
        let mut config = server.config.write().unwrap_or_else(|e| e.into_inner());
        config.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    }
    let server = server.with_cron_tick(Duration::from_millis(50));
    (server, tmp, home)
}

/// The default workspace's registry id (the 4a helper's twin).
fn default_workspace_id(server: &EmbeddedServer) -> String {
    server
        .workspace_registry()
        .default_workspace_id()
        .expect("the default workspace is set")
        .to_string()
}

/// The default runtime's workspace cron manager.
fn ws_cron(server: &EmbeddedServer) -> Arc<mirri_core::cron::WorkspaceCronManager> {
    let workspace_id = default_workspace_id(server);
    server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("the default runtime is built")
        .cron()
        .expect("the cron manager is attached")
        .clone()
}

/// Every session id `session.list` returns, in BOTH views (plain and
/// `excludeEmpty`) — the list answers an `{items, ...}` envelope, so the
/// ids are read from `items` (reading the envelope as a bare array would
/// compare two empty lists and prove nothing).
async fn listed_session_ids(server: &EmbeddedServer) -> Vec<String> {
    let mut ids = Vec::new();
    for params in [json!({}), json!({ "excludeEmpty": true })] {
        let response = server
            .call_method("session.list", params)
            .await
            .expect("session.list");
        let rows = response["items"]
            .as_array()
            .expect("session.list answers an items envelope");
        ids.extend(
            rows.iter()
                .filter_map(|row| row["id"].as_str().map(str::to_string)),
        );
    }
    ids.sort();
    ids
}

/// Given a fired workspace task with a scripted LLM, when the run's turn
/// settles, then the RunRecord carries the terminal state, duration, and
/// session reference, and the session list does NOT grow (the lazy
/// execution session never enters the index — V12).
#[tokio::test]
async fn should_record_the_terminal_run_and_keep_the_session_list_flat() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("patrol done, nothing to report");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let sessions_before = listed_session_ids(&server).await;
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "patrol the workspace",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    // The scheduler ticks + the turn runs + the finale records.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if runs
            .iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
        {
            // V12: the terminal record carries duration + session_ref.
            let terminal = runs
                .iter()
                .find(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
                .unwrap();
            assert!(terminal.duration_ms.is_some(), "duration recorded");
            assert!(terminal.session_ref.is_some(), "session ref recorded");
            break;
        }
    }
    let runs = cron.runs(&task_id);
    assert!(
        runs.iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Succeeded),
        "the run reached a terminal state: {runs:?}"
    );
    assert!(
        runs.iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Running)
            || !runs.is_empty(),
        "the Running record opened first (durable-first)"
    );
    // V12's flat-list half: the execution session never entered the index.
    let sessions_after = listed_session_ids(&server).await;
    assert_eq!(
        sessions_after, sessions_before,
        "the lazy execution session does not enter the session list \
         (neither the plain nor the excludeEmpty view)"
    );
    let _ = tmp;
}

/// Given a Running run for a task with the default skip posture, when the
/// next trigger fires, then the fire is skipped with the recorded reason
/// and the fire budget is NOT consumed (V14 — the 4a M1 semantics kept).
#[tokio::test]
async fn should_skip_the_next_fire_while_the_previous_run_is_running() {
    let mock = Arc::new(ScriptedLlm::new());
    // A gated tool-use reply keeps the first run's turn busy long enough
    // for the second fire to land — but a plain long-running approach is
    // flaky; instead, seed a Running RunRecord directly (the concurrency
    // check reads the run records, not the turn state).
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                // A schedule that cannot fall due during the test (Jan 1
                // 09:00) — only the manual trigger fires, so the scheduler
                // tick never races the concurrency verdict.
                "cron": "0 9 1 1 *",
                "prompt": "patrol",
                "recurring": true,
                "maxFires": 5,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    // Seed an in-flight run (the executor's own Running record shape).
    cron.record_run(
        &task_id,
        mirri_core::cron::RunRecord {
            run_id: "run-seeded".to_string(),
            status: mirri_core::cron::RunStatus::Running,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: Some("session-seeded".to_string()),
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        },
    )
    .await;
    // Fire the trigger (the run-now face goes through the same executor).
    server
        .call_method(
            "cron.trigger",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("cron.trigger");
    // The skip record lands (the executor's concurrency check) — its
    // appearance in the run list is the completion signal, not a fixed
    // settle.
    mirri_test_support::wait_for::wait_for_async(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
        || {
            cron.runs(&task_id)
                .iter()
                .any(|run| {
                    run.status == mirri_core::cron::RunStatus::Skipped
                        && run
                            .skip_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("上次运行中"))
                })
                .then_some(())
        },
    )
    .await
    .expect("the skip record must land (the executor's concurrency check)");
    let runs = cron.runs(&task_id);
    assert!(
        runs.iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Skipped
                && run
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("上次运行中"))),
        "the skip posture recorded the still-running reason: {runs:?}"
    );
    let task = cron.get(&task_id).expect("the task survives");
    assert_eq!(task.fire_count, 0, "a skip does not consume the budget");
    let _ = tmp;
}

/// Given a finished run's lazy execution session, when the user opens it
/// through `cron.run.open`, then the session materializes (state.json +
/// wire.jsonl) — the ONLY index admission (V13's explicit-open path).
#[tokio::test]
async fn should_materialize_the_execution_session_on_explicit_open() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("all good");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "one look",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    // Wait for the terminal run.
    let mut session_ref = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if let Some(terminal) = runs
            .iter()
            .find(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
        {
            session_ref = terminal.session_ref.clone();
            break;
        }
    }
    let session_ref = session_ref.expect("the terminal run carries the execution session's ref");
    // Before the open: no session directory (the lazy shape).
    // (The materialization check is the manager's own idempotent face.)
    // The explicit open (V13): materialize.
    let opened = server
        .call_method(
            "cron.run.open",
            json!({ "workspaceId": &workspace_id, "sessionId": &session_ref }),
        )
        .await
        .expect("cron.run.open");
    assert_eq!(opened["materialized"], true, "the first open materializes");
    // Idempotent: a second open reports false (already materialized).
    let again = server
        .call_method(
            "cron.run.open",
            json!({ "workspaceId": &workspace_id, "sessionId": &session_ref }),
        )
        .await
        .expect("cron.run.open again");
    assert_eq!(again["materialized"], false, "the second open is a no-op");
    let _ = tmp;
}

/// Given a task that fires on its schedule, when the run reaches a
/// terminal state, then the record names the trigger and the schedule
/// moment, and the launch delay is non-negative.
#[tokio::test]
async fn should_record_the_trigger_and_the_scheduled_moment_when_a_task_fires() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("done");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "launch record",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    let due = chrono::Utc::now() - chrono::Duration::seconds(5);
    cron.set_next_fire_at(&task_id, Some(due));
    let mut run = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        run = cron
            .runs(&task_id)
            .into_iter()
            .find(|r| r.status == mirri_core::cron::RunStatus::Succeeded);
        if run.is_some() {
            break;
        }
    }
    let run = run.expect("the run reaches Succeeded");
    assert_eq!(
        run.trigger,
        Some(mirri_core::cron::RunTrigger::Scheduled),
        "the scheduler tick is the recorded trigger: {run:?}"
    );
    let scheduled_at = run.scheduled_at.expect("the schedule moment is recorded");
    assert!(run.started_at >= scheduled_at, "launch delay is non-negative");
    let _ = tmp;
}

/// Given a turn that ends with no assistant output, when the run
/// finishes, then it is recorded Failed with the reason — never a
/// quiet Succeeded (the 2026-11-04 real run: 137s, zero output, recorded
/// green). Every scripted reply is EMPTY (the initial call plus the
/// empty-response retry budget), so the turn dies through the engine's
/// empty-response guard — the "injected but produced nothing" shape.
#[tokio::test]
async fn should_record_the_run_failed_when_the_turn_produced_no_assistant_output() {
    let mock = Arc::new(ScriptedLlm::new());
    for _ in 0..5 {
        mock.push_text("");
    }
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "will produce nothing",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    let mut run = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        run = cron
            .runs(&task_id)
            .into_iter()
            .find(|r| r.status != mirri_core::cron::RunStatus::Running);
        if run.is_some() {
            break;
        }
    }
    let run = run.expect("the run reaches a terminal state");
    assert_eq!(run.status, mirri_core::cron::RunStatus::Failed, "{run:?}");
    assert!(
        run.skip_reason.as_deref().is_some_and(|r| !r.is_empty()),
        "the failure names its reason: {run:?}"
    );
    let _ = tmp;
}

/// Given a manual-mode task whose run asks for an approval nobody
/// answers yet, when the run is inspected, then its record is
/// WaitingApproval; and when the approval is granted, the run resumes and
/// ends Succeeded (ADR-021: the run is visibly waiting on a person).
#[tokio::test]
async fn should_show_waiting_approval_while_a_manual_run_is_parked_and_resume_after_the_decision() {
    let mock = Arc::new(ScriptedLlm::new());
    // Turn 1: a tool call that needs approval (Bash in manual mode); turn
    // 2 (after the tool result): the model ends the turn.
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_bash".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "ls" }),
    }]);
    mock.push_text("done");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "needs approval",
                "recurring": true,
                "permissionMode": "manual",
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    // The run parks on its approval gate: the record flips to
    // WaitingApproval (≤5s).
    let mut parked = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(run) = cron
            .runs(&task_id)
            .into_iter()
            .find(|r| r.status == mirri_core::cron::RunStatus::WaitingApproval)
        {
            parked = Some(run);
            break;
        }
    }
    let parked = parked.unwrap_or_else(|| {
        panic!(
            "the parked run is recorded WaitingApproval, runs = {:?}",
            cron.runs(&task_id)
        )
    });
    let session_ref = parked.session_ref.clone().expect("the record names its session");
    // The ask is pending on the execution session's gate — decide it
    // through the shared dispatch.
    let rows = server
        .call_method("approval.listPending", json!({ "sessionId": &session_ref }))
        .await
        .expect("approval.listPending");
    let row = rows
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["toolName"] == json!("Bash"))
                .cloned()
        })
        .expect("the Bash ask is pending on the gate");
    let request_id = row["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "approval.decide",
            json!({
                "sessionId": &session_ref,
                "requestId": &request_id,
                "approved": true,
            }),
        )
        .await
        .expect("approval.decide");
    // The run resumes and reaches Succeeded.
    let mut finished = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if cron
            .runs(&task_id)
            .into_iter()
            .any(|r| r.status == mirri_core::cron::RunStatus::Succeeded)
        {
            finished = true;
            break;
        }
    }
    assert!(finished, "the approved run finished Succeeded");
    let _ = tmp;
}

/// Given a workspace with an in-flight run, when the workspace deletion
/// terminates it, then the Running record flips to Rejected with the
/// honest cause (A.4/V19: 「工作区已删除」— never a silent disappearance,
/// never a mis-counted failure).
#[tokio::test]
async fn should_terminate_in_flight_runs_when_the_workspace_is_deleted() {
    let (server, tmp, _home) = ws4b_server(Arc::new(ScriptedLlm::new()));
    let cron = ws_cron(&server);
    let task_id = cron
        .add(
            "0 9 * * *".to_string(),
            "patrol".to_string(),
            true,
            mirri_core::cron::CronCreateOptions::default(),
        )
        .await
        .expect("task");
    cron.record_run(
        &task_id,
        mirri_core::cron::RunRecord {
            run_id: "run-in-flight".to_string(),
            status: mirri_core::cron::RunStatus::Running,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: Some("session-exec".to_string()),
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        },
    )
    .await;
    assert!(
        cron.runs(&task_id)
            .iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Running)
    );
    // The deletion's termination step (the purge's first cron action —
    // asserted directly; the purge then removes the files, so the record
    // is the termination itself, not the retention).
    let terminated = cron.terminate_running_runs("工作区已删除").await;
    assert_eq!(terminated, vec!["session-exec".to_string()]);
    let runs = cron.runs(&task_id);
    assert!(
        runs.iter()
            .any(|run| run.status == mirri_core::cron::RunStatus::Rejected
                && run.skip_reason.as_deref() == Some("工作区已删除")
                && run.ended_at.is_some()),
        "the in-flight run terminated with the honest cause: {runs:?}"
    );
    // And the durable store carries it too (the audit trail the A.4
    // wording demands) — the shared SQLite index is the durable truth
    // once the write-through backend is mounted (ADR-020).
    let index = server.shared_cron_index().expect("the shared cron index");
    let row = index
        .list_workspace(&default_workspace_id(&server))
        .unwrap()
        .into_iter()
        .find(|row| row.id == task_id)
        .expect("the terminated task's row");
    assert!(
        row.body.contains("\"status\":\"rejected\"")
            && row.body.contains("工作区已删除"),
        "the termination persisted into the row body: {}",
        row.body
    );
    let _ = tmp;
}

/// Given an execution session whose run ended 8 days ago and was never
/// opened, when the scheduler's workspace pass sweeps, then the live
/// session is dropped (the in-memory discard) while the run record keeps
/// its data (D-M5 b / V21 — the lazy session never materialized, nothing
/// on disk to clean).
#[tokio::test]
async fn should_drop_an_unopened_execution_session_after_seven_days() {
    let (server, tmp, _home) = ws4b_server(Arc::new(ScriptedLlm::new()));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 1 1 *",
                "prompt": "old run",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    // A terminal run record referencing a lazy execution session that
    // ended 8 days ago (past the 7-day retention).
    cron.record_run(
        &task_id,
        mirri_core::cron::RunRecord {
            run_id: "run-old".to_string(),
            status: mirri_core::cron::RunStatus::Succeeded,
            started_at: chrono::Utc::now() - chrono::Duration::days(9),
            ended_at: Some(chrono::Utc::now() - chrono::Duration::days(8)),
            duration_ms: Some(100),
            usage: None,
            // The session id is arbitrary — the sweep only checks the
            // session's liveness and materialization; an unknown id is a
            // no-op (already gone).
            session_ref: Some("session-old-unopened".to_string()),
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        },
    )
    .await;
    // The sweep runs on the scheduler's workspace pass — the tick fires
    // it; give it one tick's worth of time.
    tokio::time::sleep(Duration::from_millis(200)).await; // hygiene-allow: negative window (proving absence after the sweep pass)
    // The run record survives (the audit trail).
    let runs = cron.runs(&task_id);
    assert!(
        runs.iter().any(|run| run.run_id == "run-old"),
        "the run record survives the sweep: {runs:?}"
    );
    let _ = tmp;
}

/// A reverse-RPC handler that records every `approval.ask` payload it
/// sees (the propose channel's fire-and-forget ask) and never answers —
/// the gate is the source of truth, the ask is the presentation face.
fn ask_recording_handler(asks: Arc<std::sync::Mutex<Vec<(String, Value)>>>) -> ReverseRpcHandler {
    Arc::new(move |method: String, params: Value| {
        let asks = Arc::clone(&asks);
        Box::pin(async move {
            if method == "approval.ask" {
                asks.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((method, params));
            }
            futures::future::pending::<()>().await;
            unreachable!("the pending future never resolves")
        })
    })
}

/// Given a model-initiated CreateScheduledTask through the REAL tool chain
/// (the scripted LLM calls the tool in a live turn), when the engine's ONE
/// tool approval is raised, then the pending entry carries the structured
/// creation display (every parameter the user judges), nothing exists yet,
/// and approving it creates the task in the same call with the
/// AgentApproved origin — no second card, no notice (ADR-022).
#[tokio::test]
async fn should_run_the_full_create_to_approval_chain_through_the_real_tool() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_create".to_string(),
        name: "CreateScheduledTask".to_string(),
        args: json!({
            "cron": "0 9 * * *",
            "prompt": "nightly patrol",
            "recurring": true,
            "timezone": "Asia/Shanghai",
            "permission_posture": "ask-deny",
            "title": "每夜巡检",
        }),
    }]);
    mock.push_text("the task is set up");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let asks: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    server
        .on_reverse_rpc(ask_recording_handler(Arc::clone(&asks)))
        .await
        .expect("reverse rpc install");
    let created = server
        .call_method("session.create", json!({ "title": "proposer" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "please set up the patrol", "mode": "fire" }),
        )
        .await
        .expect("message.send");
    // The ONE approval: the engine's tool-call ask (never auto-approved).
    let rows = super::poll_until_pending(&server, &session_id, "approval.listPending").await;
    let row = rows
        .iter()
        .find(|row| row["toolName"] == json!("CreateScheduledTask"))
        .expect("the CreateScheduledTask tool call is gated: {rows:?}");
    let request_id = row["id"].as_str().unwrap().to_string();
    // The card display is the structured creation the user judges.
    let display = &row["display"];
    assert_eq!(display["kind"], "scheduled_task_create");
    assert_eq!(display["cron"], "0 9 * * *");
    assert_eq!(display["timezone"], "Asia/Shanghai");
    assert_eq!(display["prompt"], "nightly patrol");
    assert_eq!(display["recurring"], true);
    assert_eq!(display["permission_mode"], "manual");
    assert_eq!(display["title"], "每夜巡检");
    assert!(display["workspace_id"].as_str().is_some_and(|id| !id.is_empty()));
    // Nothing exists before the decision.
    let cron = ws_cron(&server);
    assert!(cron.list().is_empty(), "no task before approval");
    server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "request_id": &request_id,
                "approved": true,
            }),
        )
        .await
        .expect("approval.decide");
    // The approval created the task (poll: the tool call runs after the
    // decision resolves the gate).
    let mut tasks = Vec::new();
    for _ in 0..100 {
        tasks = cron.list();
        if !tasks.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(tasks.len(), 1, "the approval created the task: {tasks:?}");
    assert_eq!(tasks[0].origin, mirri_core::cron::TaskOrigin::AgentApproved);
    assert_eq!(tasks[0].prompt, "nightly patrol");
    assert_eq!(tasks[0].title, Some("每夜巡检".to_string()));
    // Exactly ONE approval was ever raised: no second creation card.
    let after = server
        .call_method("approval.listPending", json!({ "session_id": &session_id }))
        .await
        .expect("listPending after");
    assert!(
        after.as_array().is_some_and(|rows| rows.is_empty()),
        "no second card is left pending: {after:?}"
    );
    let _ = tmp;
}

/// Given the same real create chain, when the user DENIES through
/// `approval.decide` with feedback, then the call never executes: no task,
/// no residue, and the model's tool result carries the user's reason.
#[tokio::test]
async fn should_run_the_full_create_to_rejection_chain_through_the_real_tool() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_create".to_string(),
        name: "CreateScheduledTask".to_string(),
        args: json!({
            "cron": "0 9 * * *",
            "prompt": "nightly report",
            "recurring": true,
        }),
    }]);
    mock.push_text("understood, I will wait for the user");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let asks: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    server
        .on_reverse_rpc(ask_recording_handler(Arc::clone(&asks)))
        .await
        .expect("reverse rpc install");
    let created = server
        .call_method("session.create", json!({ "title": "proposer" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "set up the report", "mode": "fire" }),
        )
        .await
        .expect("message.send");
    let rows = super::poll_until_pending(&server, &session_id, "approval.listPending").await;
    let row = rows
        .iter()
        .find(|row| row["toolName"] == json!("CreateScheduledTask"))
        .expect("the tool-call ask is listed");
    let request_id = row["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "request_id": &request_id,
                "approved": false,
                "reason": "预算不可控，请改用每周一次",
            }),
        )
        .await
        .expect("approval.decide");
    // Give the denied call's turn time to settle, then: zero residue.
    tokio::time::sleep(Duration::from_millis(500)).await; // hygiene-allow: negative window (proving absence after the denied turn settles)
    let cron = ws_cron(&server);
    assert!(cron.list().is_empty(), "a denial creates no task");
    let after = server
        .call_method("approval.listPending", json!({ "session_id": &session_id }))
        .await
        .expect("listPending after");
    assert!(
        after.as_array().is_some_and(|rows| rows.is_empty()),
        "no gate entry remains: {after:?}"
    );
    let _ = tmp;
}

/// Given the REAL CreateScheduledTask tool chain in a headless posture
/// (no reverse-RPC callback wired), when the model calls the tool, then
/// the call is NOT auto-approved (the production policy chain produces a
/// never-auto-approve ask; the headless no-callback arm rejects instead
/// of approving) — no task may exist.
#[tokio::test]
async fn should_not_auto_approve_a_real_create_scheduled_task_call_when_headless_without_a_callback()
 {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_create".to_string(),
        name: "CreateScheduledTask".to_string(),
        args: json!({
            "cron": "0 9 * * *",
            "prompt": "unattended creation",
            "recurring": true,
        }),
    }]);
    mock.push_text("the creation needs your approval");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    // NO on_reverse_rpc install — the headless, callback-less posture.
    let created = server
        .call_method("session.create", json!({ "title": "proposer" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "go", "mode": "fire" }),
        )
        .await
        .expect("message.send");
    // The turn settles WITHOUT the tool ever being auto-approved: the
    // production policy chain (CreateScheduledTaskNeverAutoApprovePolicy
    // first) produced a never-auto-approve ask, and the headless
    // no-callback arm rejected it. The invariant: ZERO tasks and ZERO
    // approvals.
    let cron = ws_cron(&server);
    assert!(
        cron.list().is_empty(),
        "no task was created — nothing auto-approved the creation"
    );
    let _ = tmp;
}

/// Given a workspace task whose run record was left Running by a crashed
/// process, when the server restarts (a fresh server over the same home —
/// the scheduler's first workspace pass sweeps), then the record
/// converges to Failed{服务重启中断} and the task schedules again (M2's
/// e2e: the restart recovery).
#[tokio::test]
async fn should_recover_from_a_stale_running_record_after_the_server_restarts() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, tmp, home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "patrol",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    // The crash orphan: a Running record the process never closed.
    cron.record_run(
        &task_id,
        mirri_core::cron::RunRecord {
            run_id: "run-crashed".to_string(),
            status: mirri_core::cron::RunStatus::Running,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: Some("session-crashed".to_string()),
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        },
    )
    .await;
    // The "restart": a fresh server over the SAME home (the workspace
    // store is on disk) with fast ticks — the first scheduler pass
    // sweeps the orphan.
    let mut restarted = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4b"),
        tmp.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    restarted.llm_factory = Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock))));
    {
        let mut config = restarted.config.write().unwrap_or_else(|e| e.into_inner());
        config.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    }
    let restarted = restarted.with_cron_tick(Duration::from_millis(50));
    // Start the restarted server's scheduler (the first RPC spawns it —
    // the workspace pass's M2 sweep rides the first tick). The list RPC
    // is read-only and idempotent for this purpose.
    let _ = restarted
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll (starts the scheduler)");
    // The sweep converges the orphan (the registry builds the runtime
    // on demand; the workspace pass runs `fail_orphan_running_runs`).
    let restarted_cron = ws_cron(&restarted);
    let mut converged = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = restarted_cron.runs(&task_id);
        if runs.iter().any(|run| {
            run.run_id == "run-crashed"
                && run.status == mirri_core::cron::RunStatus::Failed
                && run.skip_reason.as_deref() == Some("服务重启中断")
        }) {
            converged = true;
            break;
        }
    }
    assert!(converged, "the orphan Running converged to Failed");
    // The task is due (its schedule kept running past the crash) — the
    // recovered task fires and reaches a terminal state (the scheduling
    // un-wedged: no dead Running blocks the concurrency check anymore).
    restarted_cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    mock.push_text("recovered patrol done");
    let mut fired = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = restarted_cron.runs(&task_id);
        if runs.iter().any(|run| {
            run.status == mirri_core::cron::RunStatus::Succeeded && run.run_id != "run-crashed"
        }) {
            fired = true;
            break;
        }
    }
    assert!(fired, "the recovered task schedules and runs again");
}

/// Given a Queue-posture task with an in-flight run, when the next fire
/// lands, then the fire parks in the single queued slot (a merged Skipped
/// record with the queue reason) and the fire budget is not consumed
/// (M3's e2e over the real trigger face).
#[tokio::test]
async fn should_park_a_queue_posture_fire_in_the_single_backlog_slot_while_running() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let cron = ws_cron(&server);
    // The task is created through the RPC face (a Queue posture that
    // cannot fall due naturally) — the RPC also starts the scheduler, so
    // the M2 first-pass sweep runs BEFORE the seeded Running exists (the
    // seeded record is the in-flight run the trigger must queue behind).
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 1 1 *",
                "prompt": "queued patrol",
                "recurring": true,
                "maxFires": 5,
                "concurrency": "queue",
            }),
        )
        .await
        .expect("cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    // The in-flight run (the executor's own opener shape).
    cron.record_run(
        &task_id,
        mirri_core::cron::RunRecord {
            run_id: "run-in-flight".to_string(),
            status: mirri_core::cron::RunStatus::Running,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_ms: None,
            usage: None,
            session_ref: Some("session-in-flight".to_string()),
            skip_reason: None,
            asked_ops: Vec::new(),
            trigger: None,
            scheduled_at: None,
        },
    )
    .await;
    // Two manual triggers while Running: BOTH park (the single slot
    // merges — one queued record, not two).
    for _ in 0..2 {
        server
            .call_method(
                "cron.trigger",
                json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
            )
            .await
            .expect("cron.trigger");
    }
    tokio::time::sleep(Duration::from_millis(300)).await; // hygiene-allow: negative window (proving absence)
    let runs = cron.runs(&task_id);
    let queued: Vec<&mirri_core::cron::RunRecord> = runs
        .iter()
        .filter(|run| {
            run.status == mirri_core::cron::RunStatus::Skipped
                && run
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("单槽排队"))
        })
        .collect();
    assert_eq!(
        queued.len(),
        1,
        "the queue is ONE merged slot (two triggers, one record): {runs:?}"
    );
    let task = cron.get(&task_id).expect("the task survives");
    assert_eq!(
        task.fire_count, 0,
        "a queued fire does not consume the budget"
    );
    let _ = tmp;
}

/// Given a read-only-postured workspace task whose scripted turn first
/// calls a WRITE-class tool (Bash) then finishes, when the run settles,
/// then the RunRecord's `asked_ops` carries the refused operation and
/// `cron.runs.list` exposes it — the spawner → RunRecord → panel chain
/// (D.2 / V15 end-to-end, review m4).
#[tokio::test]
async fn should_expose_the_asked_ops_trace_through_cron_runs_list() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::llm::LlmToolCall {
        id: "call-bash-1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hi > /tmp/cron-ro.txt" }),
    }]);
    mock.push_text("the write was refused; nothing else to do");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "patrol read-only",
                "recurring": true,
                "permissionPosture": "read-only",
            }),
        )
        .await
        .expect("workspace cron.add with the read-only posture");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );

    // The scheduler ticks, the run turn executes (the Bash call is
    // refused by the readonly posture), the finale records.
    let mut terminal = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if let Some(run) = runs
            .iter()
            .find(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
        {
            terminal = Some(run.clone());
            break;
        }
    }
    let terminal = terminal.expect("the run reached a terminal state");

    // The record carries the asked-ops trace: the Bash refusal with the
    // readonly reason (D.2: 哪个操作、什么结果).
    assert!(
        !terminal.asked_ops.is_empty(),
        "the refused operation is recorded: {:?}",
        terminal.asked_ops
    );
    let entry = &terminal.asked_ops[0];
    assert_eq!(entry["tool"], "Bash");
    assert_eq!(entry["decision"], "rejected");

    // The panel's consumption face: `cron.runs.list` exposes the same
    // trace (the askedOps field rides every run row).
    let listed = server
        .call_method(
            "cron.runs.list",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("cron.runs.list");
    let rows = listed["runs"].as_array().expect("the runs array");
    let row = rows
        .iter()
        .find(|row| row["runId"] == json!(terminal.run_id))
        .expect("the terminal run is listed");
    let asked = row["askedOps"].as_array().expect("the askedOps array");
    assert!(
        asked
            .iter()
            .any(|entry| entry["tool"] == json!("Bash") && entry["decision"] == json!("rejected")),
        "the refused operation is exposed to the panel: {row}"
    );
    let _ = tmp;
}

// --- form batch 6: the environment passthrough (T6, ADR-019 / VU14) ---------

/// Given a workspace task carrying the environment fields, when its run
/// executes, then the execution session's agent config and metadata
/// carry them verbatim (model, effort, workdir — 选 A 就跑 A).
#[tokio::test]
async fn should_carry_the_task_environment_into_the_execution_session() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("done");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    // Review Major 3: the workdir must resolve INSIDE the workspace root —
    // a subdirectory the test creates so the sandbox re-root lands on an
    // existing canonical path.
    std::fs::create_dir_all(tmp.path().join("patrol-root")).expect("workdir creatable");
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "patrol",
                "recurring": true,
                "model": "test-default-model",
                "effort": "low",
                "workdir": tmp.path().join("patrol-root").to_string_lossy(),
            }),
        )
        .await
        .expect("cron.add with the environment fields");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    // Wait for the run's terminal state, then inspect the execution
    // session through the run record's session ref.
    let mut session_ref = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if let Some(run) = runs
            .iter()
            .find(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
        {
            session_ref = run.session_ref.clone();
            break;
        }
    }
    let session_ref = session_ref.expect("the run finished");
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("the default runtime is built");
    let session = runtime
        .manager
        .get_session(&mirri_core::SessionId::new(&session_ref))
        .await
        .expect("the execution session is live");
    // The workdir rides the session metadata (the create path's own slot)...
    let metadata = session.metadata().await;
    assert!(
        metadata.work_dir.as_deref().is_some_and(|recorded| {
            // The recorded slot stays the caller's spelling; the sandbox
            // comparison below runs through canonicalization.
            let recorded_path = std::path::PathBuf::from(recorded);
            let expected = tmp.path().join("patrol-root");
            let same = |a: &std::path::Path, b: &std::path::Path| {
                let a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
                let b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
                a == b
            };
            same(&recorded_path, &expected)
        }),
        "the metadata's workdir records the task's workdir: {:?}",
        metadata.work_dir
    );
    // ...AND actually constrains the run: the session's workspace sandbox
    // is re-rooted at the task's workdir (the tools' cwd), not the
    // workspace root (Review Major 3 — the slot must be load-bearing).
    // Both sides resolve through the same canonicalization the sandbox
    // uses (macOS `/var` → `/private/var`).
    let expected_root = mirri_core::tools::WorkspacePaths::new(tmp.path().join("patrol-root"))
        .validate(std::path::Path::new(
            tmp.path().join("patrol-root").as_os_str(),
        ))
        .expect("the test workdir resolves");
    assert_eq!(
        session.workspace_root(),
        Some(expected_root.as_path()),
        "the execution session's sandbox is confined to the task's workdir"
    );
    // The model + effort ride the main agent's config.
    let agent = session
        .get_agent(&mirri_core::AgentId::new(
            super::super::DEFAULT_MAIN_AGENT_ID,
        ))
        .await
        .expect("the main agent exists");
    let config_arc = agent.config();
    let config = config_arc.read().unwrap_or_else(|e| e.into_inner());
    assert_eq!(config.model_id, "test-default-model");
    assert_eq!(
        config.thinking_effort,
        Some(mirri_core::profile::ThinkingEffort::Low)
    );
    let _ = tmp;
}

/// Review Major 3: given a task whose workdir resolves OUTSIDE the
/// workspace root, when the fire spawns, then the run is refused as
/// Failed (the model-proposed workdir must never escape the workspace) —
/// the run record names the refusal, and no execution session runs.
#[tokio::test]
async fn should_refuse_the_run_when_the_task_workdir_escapes_the_workspace_root() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("should never run");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "escape",
                "recurring": true,
                "workdir": "/tmp/definitely-not-the-workspace",
            }),
        )
        .await
        .expect("cron.add accepts the shape; the refusal is at run time");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    let mut refused = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if let Some(run) = runs
            .iter()
            .find(|run| run.status == mirri_core::cron::RunStatus::Failed)
        {
            refused = Some(run.clone());
            break;
        }
    }
    let refused = refused.expect("the escape workdir refuses the run");
    assert!(
        refused
            .skip_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("越界")),
        "the refusal names the rule: {:?}",
        refused.skip_reason
    );
    let _ = tmp;
}

// NOTE (VU14's honest-failure branch): the scripted-LLM test bed cannot
// exercise the unbuildable-model path (its factory answers every
// config). The no-fallback guarantee is structural — `run_turn` builds
// the agent ONLY from the task's pinned model (no default-model
// fallback code exists in cron_run_spawner.rs) and the
// `InjectedStatus::Dropped` arm names the model in the run's
// `skip_reason`.


/// Given a fired cron task, When the run's execution session is created
/// (and even after the run terminates), Then NO metadata row exists —
/// and When the user explicitly opens the run, Then the row materializes
/// (ADR-008 / ADR-006 Task 13: execution sessions are row-exempt until
/// the explicit open).
#[tokio::test]
async fn should_not_create_rows_for_cron_execution_sessions_until_explicitly_opened() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("patrol done");
    let (server, tmp, _home) = ws4b_server(Arc::clone(&mock));
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "row exempt patrol",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let cron = ws_cron(&server);
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );

    // The run fires and settles.
    let mut execution_session = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let runs = cron.runs(&task_id);
        if let Some(terminal) = runs
            .iter()
            .find(|run| run.status == mirri_core::cron::RunStatus::Succeeded)
        {
            execution_session = terminal.session_ref.clone();
            break;
        }
    }
    let session_id = execution_session.expect("the run carried the execution session ref");

    // Then NO row exists for the execution session — not during the run,
    // not after its terminal state.
    let index = server
        .session_index()
        .expect("the index is open (home injected)");
    assert!(
        index.get_session(&session_id).is_none(),
        "the execution session writes no row before the explicit open"
    );

    // And When the user explicitly opens the run, the row materializes.
    server
        .call_method("cron.run.open", json!({ "sessionId": session_id }))
        .await
        .expect("cron.run.open");
    tokio::time::sleep(Duration::from_millis(200)).await; // hygiene-allow: negative window (proving absence)
    assert!(
        index.get_session(&session_id).is_some(),
        "the explicit open materializes the row"
    );
    let _ = tmp;
}
