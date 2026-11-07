//! Batch-3 (stage-1 cron-scheduling): the user-creation RPC face (`cron.add`,
//! E.1 / A.1) and the cross-session aggregated listing (`cron.listAll`,
//! E.5 / SDR-008 — the stage-one session-level aggregation: every existing
//! session's tasks are visible without opening that session; delivery
//! stays session-scoped, only visibility is aggregated).
//!
//! The user face bypasses the model-side budget gate (A.1: the user's own
//! decision — the gate hangs on the tool face's tool-name match), but the
//! structural validations still apply (one-shot + max_fires rejected,
//! max_fires = 0 rejected, invalid expressions rejected).

use std::sync::Arc;
use std::time::Duration;

use chrono::Timelike as _;
use serde_json::json;

use super::*;

/// A batch-3 test bed: the same shape as `b2_server` (fast ticks, scripted
/// LLM). The B2 helpers live inside `b2_notification_tests` (private), so
/// this file owns its copy of the bed.
fn cron_b3_server(mock: Arc<ScriptedLlm>) -> (EmbeddedServer, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(test_manager_with_sqlite_store(workspace, tmp.path()));
    let mut parsed = ServerConfig::from_toml("").expect("test config");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback): seed the test default.
    parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    let config = Arc::new(std::sync::RwLock::new(parsed));
    let server = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(mock))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50))
    .with_cron_tick(Duration::from_millis(50));
    (server, tmp)
}

// ---------------------------------------------------------------------------
// cron.add — the user-creation RPC face (E.1 / A.1)
// ---------------------------------------------------------------------------

/// Given a live session, when the user creates a recurring task through
/// `cron.add` with a max_fires budget of 100, then the task is created with
/// origin "user" — the budget gate (N=3) does not apply to the user face,
/// while the same budget through the model tool face would be gated.
#[tokio::test]
async fn should_create_a_user_origin_task_with_a_high_fire_budget_via_cron_add() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({ "title": "user-face" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let added = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "morning briefing",
                "recurring": true,
                "maxFires": 100,
            }),
        )
        .await
        .expect("cron.add with a high budget succeeds on the user face");

    let task_id = added["taskId"]
        .as_str()
        .expect("taskId in the ack")
        .to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task = session.cron().get(&task_id).expect("task registered");
    assert_eq!(task.origin, mirri_core::cron::TaskOrigin::User);
    assert_eq!(task.max_fires, Some(100));
    assert!(task.recurring);
    assert_eq!(task.prompt, "morning briefing");
}

/// Given a live session, when `cron.add` still carries the retired
/// `unattended` key, then the creation lands under the declared mode and
/// the key has no effect — no record field grows back.
#[tokio::test]
async fn should_ignore_the_retired_unattended_key_when_creating_via_the_user_face() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "morning briefing",
                "recurring": true,
                "permissionMode": "auto",
                "unattended": true,
            }),
        )
        .await
        .expect("a retired key does not fail the request");

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task = &session.cron().list()[0];
    assert_eq!(
        task.permission_mode,
        Some(mirri_core::cron::PermissionMode::Auto),
        "the declared mode is the only ask law"
    );
    let rewritten = mirri_core::cron::cron_task_wire(task);
    assert!(
        rewritten.get("unattended").is_none(),
        "the retired key is never written back: {rewritten}"
    );
}

/// Given a live session, when the user creates a task via `cron.add` with no
/// explicit fields, then the defaults hold: origin user, no declared mode,
/// unbounded budget, recurring as declared.
#[tokio::test]
async fn should_default_to_user_origin_attended_and_unbounded_when_optional_fields_are_omitted() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let added = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "*/5 * * * *",
                "prompt": "tick",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add");

    let task_id = added["taskId"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task = session.cron().get(&task_id).expect("task registered");
    assert_eq!(task.origin, mirri_core::cron::TaskOrigin::User);
    assert_eq!(task.max_fires, None);
}

/// Given the user face, when a one-shot task carries max_fires, then the
/// creation is rejected (the structural validation the gate never waived).
#[tokio::test]
async fn should_reject_max_fires_on_a_one_shot_task_via_the_user_face() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let error = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "*/5 * * * *",
                "prompt": "once",
                "recurring": false,
                "maxFires": 3,
            }),
        )
        .await
        .expect_err("max_fires on a one-shot must be rejected");
    assert!(
        error.to_string().contains("max_fires"),
        "the reason names max_fires: {error}"
    );
}

/// Given the user face, when max_fires is zero, then the creation is
/// rejected (a zero budget would remove the task before it ever fired).
#[tokio::test]
async fn should_reject_a_zero_fire_budget_via_the_user_face() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let error = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "*/5 * * * *",
                "prompt": "p",
                "recurring": true,
                "maxFires": 0,
            }),
        )
        .await
        .expect_err("max_fires = 0 must be rejected");
    assert!(
        error.to_string().contains("max_fires"),
        "the reason names max_fires: {error}"
    );
}

/// Given the user face, when the cron expression is invalid, then the
/// creation is rejected and nothing registers.
#[tokio::test]
async fn should_reject_an_invalid_cron_expression_via_the_user_face() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let error = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "not a cron",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect_err("an invalid expression must be rejected");
    assert!(
        error.to_string().contains("cron"),
        "the reason names the expression: {error}"
    );
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    assert!(
        session.cron().list().is_empty(),
        "nothing registered on failure"
    );
}

/// Given a task created via the user face, when the session's cron list is
/// read back, then the task appears with the user-origin fields (E.2/F: the
/// per-session list carries origin, maxFires, fireCount).
#[tokio::test]
async fn should_list_a_user_created_task_with_the_new_fields_in_cron_list() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "user watch",
                "recurring": true,
                "maxFires": 5,
            }),
        )
        .await
        .expect("cron.add");

    let listed = server
        .call_method("cron.list", json!({ "sessionId": &session_id }))
        .await
        .expect("cron.list");
    let tasks = listed["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1);
    let row = &tasks[0];
    assert_eq!(row["origin"], "user");
    assert_eq!(row["maxFires"], 5);
    assert_eq!(row["fireCount"], 0);
}

/// Given a server started with cron scheduling disabled, when the user
/// creates a task through `cron.add`, then the creation is refused with the
/// documented reason (E.4 layer 1 — the switch covers the RPC creation face
/// exactly like the model tool face).
#[tokio::test]
async fn should_refuse_user_creation_when_scheduling_is_disabled() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let server = server.with_cron_scheduling_disabled(true);
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let error = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect_err("creation must be refused while disabled");
    assert!(
        error.to_string().contains("MIRRICODE_DISABLE_CRON"),
        "the refusal names the switch: {error}"
    );
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    assert!(
        session.cron().list().is_empty(),
        "nothing registered while disabled"
    );
}

/// Given a user-created task, when the session closes and resumes, then the
/// task survives with its origin (the user face records through the same
/// write-before-mutate wire op the tool face uses).
#[tokio::test]
async fn should_survive_close_and_resume_with_user_origin() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = cron_b3_server(Arc::clone(&mock));
    let created = server
        .call_method("session.create", json!({ "title": "resume-face" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    // Materialize the session so the close→resume flow has a wire log.
    converse_once(&server, &session_id).await;
    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "user watch",
                "recurring": true,
                "maxFires": 7,
            }),
        )
        .await
        .expect("cron.add");

    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .expect("resume");

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live after resume");
    let tasks = session.cron().list();
    assert_eq!(tasks.len(), 1, "the user task survives the resume");
    assert_eq!(tasks[0].origin, mirri_core::cron::TaskOrigin::User);
    assert_eq!(tasks[0].max_fires, Some(7));
}

// ---------------------------------------------------------------------------
// cron.listAll — the cross-session aggregated listing (E.5 / V8a)
// ---------------------------------------------------------------------------

/// Given two sessions (one live, one closed) each holding a cron task, when
/// `cron.listAll` is called with no session open dependency, then BOTH
/// tasks are aggregated in one view, each row carrying its session id and
/// an honest `sessionOpen` flag (the closed session's row reports false —
/// visibility is global, delivery stays session-scoped).
#[tokio::test]
async fn should_aggregate_tasks_from_live_and_closed_sessions_in_cron_list_all() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = cron_b3_server(Arc::clone(&mock));
    let first = server
        .call_method("session.create", json!({ "title": "live-one" }))
        .await
        .expect("create first");
    let second = server
        .call_method("session.create", json!({ "title": "closed-one" }))
        .await
        .expect("create second");
    let first_id = first["id"].as_str().unwrap().to_string();
    let second_id = second["id"].as_str().unwrap().to_string();

    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &first_id,
                "cron": "0 9 * * *",
                "prompt": "live session watch",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add first");
    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &second_id,
                "cron": "*/10 * * * *",
                "prompt": "closed session watch",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add second");
    // Materialize + close the second session (its task must STILL show).
    converse_once(&server, &second_id).await;
    server
        .call_method("session.close", json!({ "session_id": &second_id }))
        .await
        .expect("close second");

    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");

    assert_eq!(
        listed["disabled"], false,
        "the honest disabled flag rides along"
    );
    let sessions = listed["sessions"].as_array().expect("sessions array");
    let live_row = sessions
        .iter()
        .find(|row| row["sessionId"] == first_id.as_str())
        .expect("the live session's row");
    let closed_row = sessions
        .iter()
        .find(|row| row["sessionId"] == second_id.as_str())
        .expect("the closed session's row");
    assert_eq!(live_row["open"], true, "the live session reports open");
    assert_eq!(
        closed_row["open"], false,
        "the closed session reports closed"
    );
    let live_tasks = live_row["tasks"].as_array().expect("live tasks");
    let closed_tasks = closed_row["tasks"].as_array().expect("closed tasks");
    assert_eq!(live_tasks.len(), 1);
    assert_eq!(closed_tasks.len(), 1);
    assert_eq!(live_tasks[0]["prompt"], "live session watch");
    assert_eq!(closed_tasks[0]["prompt"], "closed session watch");
    assert_eq!(closed_tasks[0]["origin"], "user", "origin carried per task");
}

/// Given the aggregated view, when a session holds no tasks, then it does
/// not appear as an empty group (the view lists task-bearing sessions, not
/// every session).
#[tokio::test]
async fn should_omit_sessions_without_tasks_from_the_aggregated_view() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    server
        .call_method("session.create", json!({ "title": "empty" }))
        .await
        .expect("create");

    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    let sessions = listed["sessions"].as_array().expect("sessions array");
    assert!(sessions.is_empty(), "no tasks, no groups: {listed}");
}

/// Given a server with scheduling disabled, when `cron.listAll` is called,
/// then the aggregation is served with `disabled: true` (the honest-list
/// layer rides the aggregated face too).
#[tokio::test]
async fn should_report_the_disabled_state_on_the_aggregated_view() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let server = server.with_cron_scheduling_disabled(true);
    let created = server
        .call_method("session.create", json!({ "title": "disabled-view" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    // Seed through the manager (the user RPC face refuses while disabled).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .cron()
        .add(
            "0 9 * * *".into(),
            "morning".into(),
            session_id.clone(),
            true,
            mirri_core::cron::CronCreateOptions::default(),
        )
        .await
        .expect("seed task");

    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    assert_eq!(listed["disabled"], true);
    let sessions = listed["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1, "the task is still listed honestly");
}

// ---------------------------------------------------------------------------
// cron.listAll cost governance (review Major 1) — the per-session replay
// cache + the top-level throttle
// ---------------------------------------------------------------------------

/// Given a persisted-only session with an unchanged wire log, when
/// `cron.listAll` is called twice, then the second aggregation answers the
/// CACHED replay (the per-session cache entry is present) — and after the
/// log's fingerprint moves, the cache invalidates and the fresh replay
/// serves the appended task.
#[tokio::test(start_paused = true)]
async fn should_cache_a_persisted_sessions_replay_and_invalidate_when_the_log_moves() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = cron_b3_server(Arc::clone(&mock));
    let created = server
        .call_method("session.create", json!({ "title": "cached-replay" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "first task",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add");
    converse_once(&server, &session_id).await;
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");

    // First aggregation: the replay populates the cache.
    let first = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("first cron.listAll");
    assert_eq!(first["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(
        server.session_manager().cron_replay_count(),
        1,
        "the first aggregation replays and caches the persisted session"
    );

    // Second aggregation (past the throttle window, so the walk itself runs):
    // the unchanged wire log answers from the per-session cache — the
    // top-level 1s throttle would also serve the stored answer, so the
    // observable here is the per-session cache entry surviving the walk.
    tokio::task::yield_now().await;
    tokio::time::advance(CRON_LIST_ALL_MIN_INTERVAL + Duration::from_millis(50)).await;
    let second = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("second cron.listAll");
    assert_eq!(
        second["sessions"].as_array().expect("sessions").len(),
        1,
        "the cached aggregate still lists the session"
    );
    assert_eq!(
        server.session_manager().cron_replay_count(),
        1,
        "the unchanged wire log did NOT replay again"
    );

    // Move the log's fingerprint (a rewrite of the same bytes: the mtime
    // moves, the content stays valid) and aggregate again: the cache
    // invalidates and the fresh replay re-serves the same task. The stat
    // behind the fingerprint is second-precision (`SyncFs::stat`), so the
    // paused clock's frozen real time would keep a same-second rewrite
    // inside the cached second — pin the mtime explicitly instead (the
    // `set_times` technique of `mirri-os`'s mtime tests).
    let wire_log = find_wire_log(_tmp.path());
    let bytes = std::fs::read(&wire_log).expect("read the wire log");
    std::fs::write(&wire_log, &bytes).expect("rewrite the wire log (mtime moves)");
    {
        use std::fs::FileTimes;
        let file = std::fs::File::open(&wire_log).expect("open the wire log");
        let times = FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(3_600));
        file.set_times(times).expect("pin the wire log mtime");
    }
    tokio::task::yield_now().await;
    tokio::time::advance(CRON_LIST_ALL_MIN_INTERVAL + Duration::from_millis(50)).await;
    let third = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("third cron.listAll after the log moved");
    assert_eq!(third["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(
        server.session_manager().cron_replay_count(),
        2,
        "the moved fingerprint invalidated the cached replay"
    );
}

/// Locate the test workspace's single `wire.jsonl` under the sessions root
/// (the bucket key is internal — a recursive walk of the tempdir finds the
/// one file the single session wrote).
fn find_wire_log(root: &std::path::Path) -> std::path::PathBuf {
    fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.file_name().is_some_and(|name| name == "wire.jsonl") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    assert_eq!(found.len(), 1, "exactly one wire log under the tempdir");
    found.into_iter().next().expect("the wire log")
}

/// Given an aggregation answered within the last second, when
/// `cron.listAll` is called again immediately, then the stored sessions are
/// returned verbatim — the top-level throttle (verified through the
/// per-session cache entry count staying put while a NEW persisted session
/// replays only once across both calls).
#[tokio::test]
async fn should_answer_a_back_to_back_aggregation_from_the_throttle_snapshot() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = cron_b3_server(Arc::clone(&mock));
    let created = server
        .call_method("session.create", json!({ "title": "throttled" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "tick",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add");
    converse_once(&server, &session_id).await;
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");

    let first = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("first cron.listAll");
    assert_eq!(first["sessions"].as_array().expect("sessions").len(), 1);

    // The immediate second call lands inside the throttle window: it must
    // answer the stored sessions (the observable difference — a live-walk
    // would rebuild identical rows, but the walk's own per-session replay
    // seam cannot run inside the window at all).
    let second = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("second cron.listAll");
    assert_eq!(
        serde_json::to_value(&second["sessions"]).unwrap(),
        serde_json::to_value(&first["sessions"]).unwrap(),
        "the throttled answer is verbatim"
    );
    assert_eq!(
        server.session_manager().cron_replay_count(),
        1,
        "the throttled second call performed no additional walk work"
    );
}

// --- form batch 6: cron.preview + the task-form fields on the user faces ---

/// Given a cron expression, when `cron.preview` is called, then the
/// occurrences answer as epoch-millis instants (the read-only preview
/// face, no task side effects).
#[tokio::test]
async fn should_answer_the_next_occurrences_when_previewing_a_cron_expression() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let previewed = server
        .call_method(
            "cron.preview",
            json!({ "cron": "*/5 * * * *", "timezone": "UTC", "count": 2 }),
        )
        .await
        .expect("cron.preview");
    let occurrences = previewed["occurrences"].as_array().expect("occurrences");
    assert_eq!(occurrences.len(), 2, "count is honored: {previewed}");
    // Every occurrence is a future epoch-millis instant, strictly increasing.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut previous = 0i64;
    for occurrence in occurrences {
        let millis = occurrence.as_i64().expect("epoch millis");
        assert!(
            millis > now_ms,
            "the occurrence is in the future: {previewed}"
        );
        assert!(millis > previous, "strictly increasing: {previewed}");
        previous = millis;
    }
}

/// Given an interval schedule, when `cron.preview` is called, then the
/// interval branch answers the same face (the two schedule shapes share
/// the one RPC).
#[tokio::test]
async fn should_answer_interval_occurrences_when_previewing_an_interval_schedule() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let today = chrono::Utc::now().date_naive();
    let previewed = server
        .call_method(
            "cron.preview",
            json!({
                "interval": {
                    "every": 2,
                    "unit": "day",
                    "time": "23:45",
                    "anchorDate": today.to_string(),
                },
                "timezone": "UTC",
            }),
        )
        .await
        .expect("cron.preview");
    let occurrences = previewed["occurrences"].as_array().expect("occurrences");
    assert!(!occurrences.is_empty(), "the interval answers: {previewed}");
    // The first occurrence lands at the spec's time of day.
    let first = chrono::DateTime::from_timestamp_millis(occurrences[0].as_i64().unwrap())
        .unwrap()
        .with_timezone(&chrono_tz::UTC);
    assert_eq!(
        (first.hour(), first.minute()),
        (23, 45),
        "the occurrence carries the spec's time: {previewed}"
    );
}

/// Given a cron expression and an explicit anchor, when `cron.preview` is
/// called, then `previousT0` is the theoretical past occurrence at or
/// before the anchor — the same kernel the next occurrences come from
/// (batch 2 / A5).
#[tokio::test]
async fn should_answer_the_theoretical_past_when_previewing_a_cron_expression() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let anchor = chrono::DateTime::parse_from_rfc3339("2026-11-01T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let previewed = server
        .call_method(
            "cron.preview",
            json!({
                "cron": "0 9 * * *",
                "timezone": "UTC",
                "count": 2,
                "anchor": anchor.timestamp_millis(),
            }),
        )
        .await
        .expect("cron.preview");
    let past = previewed["previousT0"]
        .as_i64()
        .expect("previousT0 in millis");
    assert_eq!(
        past,
        chrono::DateTime::parse_from_rfc3339("2026-11-01T09:00:00Z")
            .unwrap()
            .timestamp_millis(),
        "the past is the last occurrence at or before the anchor: {previewed}"
    );
    // The occurrence itself when the anchor SITS on a fire (inclusive).
    let on_fire = server
        .call_method(
            "cron.preview",
            json!({
                "cron": "0 9 * * *",
                "timezone": "UTC",
                "anchor": chrono::DateTime::parse_from_rfc3339("2026-11-01T09:00:00Z")
                    .unwrap().timestamp_millis(),
            }),
        )
        .await
        .expect("cron.preview");
    assert_eq!(
        on_fire["previousT0"].as_i64().unwrap(),
        chrono::DateTime::parse_from_rfc3339("2026-11-01T09:00:00Z")
            .unwrap()
            .timestamp_millis(),
        "an anchor on an occurrence owns it: {on_fire}"
    );
}

/// Given an interval schedule whose anchor lies in the future, when
/// `cron.preview` is called, then `previousT0` is null — a one-off-shaped
/// rule has no theoretical past and the form renders "—" instead of a
/// fabricated instant.
#[tokio::test]
async fn should_answer_a_null_past_when_previewing_a_schedule_that_never_fired() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let previewed = server
        .call_method(
            "cron.preview",
            json!({
                "interval": { "every": 1, "unit": "month", "time": "09:00", "anchorDate": "2099-01-01" },
                "timezone": "UTC",
            }),
        )
        .await
        .expect("cron.preview");
    assert!(
        previewed["previousT0"].is_null(),
        "no theoretical past answers null: {previewed}"
    );
}

/// Given both a cron and an interval (or neither), when `cron.preview`
/// is called, then the request is refused (the mutual exclusion).
#[tokio::test]
async fn should_refuse_the_preview_when_the_schedule_is_ambiguous() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let both = server
        .call_method(
            "cron.preview",
            json!({ "cron": "* * * * *", "interval": { "every": 1, "unit": "day", "time": "09:00", "anchorDate": "2026-11-01" } }),
        )
        .await;
    assert!(both.is_err(), "both shapes are refused");
    let neither = server.call_method("cron.preview", json!({})).await;
    assert!(neither.is_err(), "neither shape is refused");
}

/// Given a task created with the form's optional fields, when the row is
/// listed, then every field rides the projection (title with the
/// fallback, the three-moment projections, the interval shape).
#[tokio::test]
async fn should_expose_the_task_form_fields_and_projections_on_the_user_face() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let added = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "interval": { "every": 2, "unit": "day", "time": "09:30", "anchorDate": chrono::Utc::now().date_naive().to_string() },
                "timezone": "UTC",
                "prompt": "sweep",
                "recurring": true,
                "title": "双日巡检",
                "model": "claude-sonnet-4",
                "effort": "low",
                "workdir": "/tmp/proj",
            }),
        )
        .await
        .expect("cron.add with the form fields");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let listed = server
        .call_method("cron.list", json!({ "sessionId": &session_id }))
        .await
        .expect("cron.list");
    let row = listed["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|row| row["id"] == json!(&task_id))
        .expect("the created task is listed");
    assert_eq!(row["title"], json!("双日巡检"));
    assert_eq!(row["model"], json!("claude-sonnet-4"));
    assert_eq!(row["effort"], json!("low"));
    assert_eq!(row["workdir"], json!("/tmp/proj"));
    assert_eq!(row["interval"]["every"], json!(2));
    assert_eq!(row["interval"]["unit"], json!("day"));
    // The three-moment projections: two future occurrences, no runs yet.
    let two = row["nextTwoFires"].as_array().expect("nextTwoFires");
    assert_eq!(two.len(), 2, "the residency state carries two moments");
    assert!(
        row["lastRunFromRuns"].is_null(),
        "no runs: null, not a cursor"
    );
}

/// Given a task with no title, when the row is listed, then the title
/// answers `null` — the server never hardcodes a locale (review m6: the
/// untitled label is the presentation layer's i18n fallback).
#[tokio::test]
async fn should_answer_a_null_title_when_no_title_was_set() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "cron.add",
            json!({ "sessionId": &session_id, "cron": "*/5 * * * *", "prompt": "tick", "recurring": true }),
        )
        .await
        .expect("cron.add");
    let listed = server
        .call_method("cron.list", json!({ "sessionId": &session_id }))
        .await
        .expect("cron.list");
    let row = &listed["tasks"].as_array().expect("tasks")[0];
    assert!(
        row["title"].is_null(),
        "no title: null, never a locale string"
    );
}

/// Given an ends_at before the first fire day, when the user face
/// creates the task, then the create is refused with the rule's message.
#[tokio::test]
async fn should_refuse_the_user_face_create_when_ends_at_precedes_the_first_fire() {
    let (server, _tmp) = cron_b3_server(Arc::new(ScriptedLlm::new()));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let error = server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "timezone": "UTC",
                "prompt": "p",
                "recurring": true,
                "endsAt": (chrono::Utc::now().date_naive() - chrono::Duration::days(2)).to_string(),
            }),
        )
        .await
        .expect_err("the create is refused");
    assert!(
        error.to_string().contains("结束日期不能早于首次运行日"),
        "the refusal names the rule: {error}"
    );
}
