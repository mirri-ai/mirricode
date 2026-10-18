//! B1 T8: print-mode background strategies (D-59) — exit / drain / steer,
//! their caps, and the `keepAliveOnExit ? drain : steer` default.

use std::sync::{Arc, RwLock};

use serde_json::json;

use super::*;

/// A throwaway server with a config parsed from the given TOML.
fn b1_print_server(toml_contents: &str) -> (EmbeddedServer, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    let mut parsed = ServerConfig::from_toml(toml_contents).expect("test config should parse");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback): seed the test default unless the TOML sets one.
    if parsed.default_model.is_none() {
        parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    }
    let config = Arc::new(RwLock::new(parsed));
    let server = EmbeddedServer::with_providers(manager, None, None, None, None, config, None);
    (server, tmp)
}

/// Create a session and return its id plus the live task manager handle.
async fn b1_print_session(server: &EmbeddedServer) -> (String, Arc<mirri_core::task::TaskManager>) {
    let created = server
        .call_method("session.create", json!({ "title": "print" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    (session_id, session.tasks())
}

/// Given a session with a running background task and print mode config
/// exit, When the print main turn completes, Then the decision is finish —
/// no waiting.
#[tokio::test]
async fn should_finish_immediately_when_print_background_mode_is_exit() {
    let (server, _tmp) = b1_print_server(
        r#"
[task]
printBackgroundMode = "exit"
"#,
    );
    let (session_id, tasks) = b1_print_session(&server).await;
    tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: Some("sleep 60".into()),
            dispatcher: None,
        },
        "still running".into(),
        session_id.clone(),
        None,
    );

    let started = std::time::Instant::now();
    let decision = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("decision");
    assert_eq!(decision["decision"], "finish");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "exit must not wait for background tasks"
    );
}

/// Given drain mode with a ceiling of 1s and a task that completes in
/// 300ms, When waitForBackgroundTasksOnPrint runs, Then it returns after
/// the task settles (under the ceiling) and the subsequent decision is
/// finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_wait_for_tasks_to_settle_when_print_background_mode_is_drain() {
    let (server, _tmp) = b1_print_server(
        r#"
[task]
printBackgroundMode = "drain"
printWaitCeilingS = 1
"#,
    );
    let (session_id, tasks) = b1_print_session(&server).await;
    let task_id = tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "settles in 300ms".into(),
        session_id.clone(),
        None,
    );
    // The runner: settles 300ms after registration.
    let settle_tasks = tasks.clone();
    let settle_id = task_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        settle_tasks.complete(&settle_id, 0);
    });

    let started = std::time::Instant::now();
    let waited = server
        .call_method(
            "session.waitForBackgroundTasksOnPrint",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("wait");
    assert_eq!(waited["waited"], true);
    assert_eq!(waited["remaining"], 0);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "the drain returns as soon as the task settles"
    );

    let decision = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("decision");
    assert_eq!(decision["decision"], "finish");
}

/// Given steer mode with printMaxTurns = 1 and an active task, When the
/// main turn completes, Then the first decision is continue and the second
/// is finish (turn cap). The duration ceiling is tested analogously with
/// printWaitCeilingS = 1.
#[tokio::test]
async fn should_continue_then_finish_when_steer_hits_the_turn_cap() {
    let (server, _tmp) = b1_print_server(
        r#"
[task]
printMaxTurns = 1
"#,
    );
    let (session_id, tasks) = b1_print_session(&server).await;
    tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: Some("sleep 60".into()),
            dispatcher: None,
        },
        "active".into(),
        session_id.clone(),
        None,
    );

    let first = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("first decision");
    assert_eq!(first["decision"], "continue", "the first decision steers");

    let second = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("second decision");
    assert_eq!(
        second["decision"], "finish",
        "the turn cap finishes the loop"
    );
}

/// Given steer mode with a 1s wait ceiling and a task that never settles,
/// When decisions are taken past the ceiling, Then the loop finishes (the
/// duration cap, not the turn cap, trips).
#[tokio::test]
async fn should_finish_when_steer_exceeds_the_wait_ceiling() {
    let (server, _tmp) = b1_print_server(
        r#"
[task]
printWaitCeilingS = 1
"#,
    );
    let (session_id, tasks) = b1_print_session(&server).await;
    tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: Some("sleep 600".into()),
            dispatcher: None,
        },
        "never settles".into(),
        session_id.clone(),
        None,
    );

    let first = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("first decision");
    assert_eq!(first["decision"], "continue");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let second = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("second decision");
    assert_eq!(
        second["decision"], "finish",
        "the wait ceiling finishes the loop"
    );
}

/// Given no explicit print config and keepAliveOnExit = true, When the mode
/// is resolved, Then it defaults to drain (`configSection.ts:58-62`): a
/// stuck task makes the completion decision WAIT for the ceiling and then
/// finish — where steer would answer continue immediately.
#[tokio::test]
async fn should_default_to_drain_when_keep_alive_on_exit_and_no_explicit_mode() {
    let (server, _tmp) = b1_print_server(
        r#"
[task]
keepAliveOnExit = true
printWaitCeilingS = 1
"#,
    );
    let (session_id, tasks) = b1_print_session(&server).await;
    tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: Some("sleep 600".into()),
            dispatcher: None,
        },
        "stuck".into(),
        session_id.clone(),
        None,
    );

    let started = std::time::Instant::now();
    let decision = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("decision");
    assert_eq!(decision["decision"], "finish");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(1),
        "drain waits out the ceiling before finishing (took {:?}) — a steer default would have answered immediately",
        started.elapsed()
    );
}

/// Given no print config at all, When the mode is resolved, Then it
/// defaults to steer: an active task yields continue (not the drain wait).
#[tokio::test]
async fn should_default_to_steer_when_no_print_config_is_present() {
    let (server, _tmp) = b1_print_server("");
    let (session_id, tasks) = b1_print_session(&server).await;
    tasks.register_with_timeout(
        mirri_core::task::TaskKind::Process {
            command: Some("sleep 60".into()),
            dispatcher: None,
        },
        "active".into(),
        session_id.clone(),
        None,
    );

    let started = std::time::Instant::now();
    let decision = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "sessionId": session_id }),
        )
        .await
        .expect("decision");
    assert_eq!(decision["decision"], "continue");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "steer answers immediately (no drain wait)"
    );
}
