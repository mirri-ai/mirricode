//! B7 R4: `task.listWorkspace` — the workspace-aggregated task overview
//! (P-023 / D-1). The live-session union over every BUILT workspace
//! runtime contributes its TaskManager's tasks; the element shape is the
//! direct `AgentTask` serialization `task.list` serves (`session_id`
//! carries the attribution).

use serde_json::json;

use super::*;

/// Register one background task on the given live session (routed through
/// the seam so a non-default workspace's session lands on its own
/// manager), returning the minted task id.
async fn seed_task(server: &EmbeddedServer, session_id: &str, description: &str) -> String {
    let session = server
        .resolve_session(session_id)
        .await
        .expect("session is live");
    session
        .tasks()
        .register_background(
            TaskKind::Process {
                command: Some(description.to_string()),
                dispatcher: None,
            },
            description.to_string(),
            session_id.to_string(),
            None,
        )
        .expect("the background registration fits")
}

/// Create one live session on the server (the default workspace).
async fn create_session(server: &EmbeddedServer, title: &str) -> String {
    let created = server
        .call_method("session.create", json!({ "title": title }))
        .await
        .expect("session.create");
    created["id"].as_str().expect("session id").to_string()
}

/// Given two live sessions each holding a background task, when
/// `task.listWorkspace` runs, then both tasks are listed with their
/// `session_id` attribution, sessions ordered by updated_at DESC.
#[tokio::test]
async fn should_list_the_tasks_of_every_live_session_in_the_workspace() {
    let (server, _tmp) = test_server(None);
    let older = create_session(&server, "older").await;
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let newer = create_session(&server, "newer").await;
    let older_task = seed_task(&server, &older, "watch older").await;
    let newer_task = seed_task(&server, &newer, "watch newer").await;

    let listed = server
        .call_method("task.listWorkspace", json!({}))
        .await
        .expect("task.listWorkspace");

    let rows = listed.as_array().expect("bare array");
    assert_eq!(rows.len(), 2, "both sessions' tasks, was: {listed}");
    // Session attribution rides `session_id`.
    let by_task: std::collections::HashMap<&str, &serde_json::Value> = rows
        .iter()
        .map(|row| (row["id"].as_str().expect("task id"), row))
        .collect();
    assert_eq!(
        by_task[older_task.as_str()]["session_id"],
        json!(older),
        "the older session's task carries its attribution"
    );
    assert_eq!(by_task[newer_task.as_str()]["session_id"], json!(newer));
    // The element shape is the direct AgentTask serialization task.list
    // serves (the tagged status object included).
    let row = by_task[older_task.as_str()];
    assert_eq!(row["description"], json!("watch older"));
    assert_eq!(row["status"], json!({ "status": "running" }));
    assert_eq!(
        row["kind"],
        json!({ "kind": "process", "command": "watch older" })
    );
    assert_eq!(row["detached"], json!(true));
    // Sessions order by updated_at DESC — the newer session's task first.
    assert_eq!(
        rows[0]["session_id"],
        json!(newer),
        "the newer session leads, was: {listed}"
    );
}

/// Given a completed and a running task, when `activeOnly=true`, then only
/// the running one is listed.
#[tokio::test]
async fn should_filter_to_running_tasks_when_active_only_is_set() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "mixed").await;
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    let running = seed_task(&server, &session_id, "still running").await;
    let done = seed_task(&server, &session_id, "already done").await;
    session.tasks().complete(&done, 0);

    let listed = server
        .call_method("task.listWorkspace", json!({ "activeOnly": true }))
        .await
        .expect("task.listWorkspace");

    let rows = listed.as_array().expect("bare array");
    assert_eq!(rows.len(), 1, "only the running row, was: {listed}");
    assert_eq!(rows[0]["id"], json!(running));
}

/// Given a second registered workspace, when `workspaceId` scopes to it,
/// then only that workspace's sessions contribute — while the DEFAULT
/// scope of a pinned single-workspace host (`for_workspace_root`) covers
/// only the pinned default (D-2).
#[tokio::test]
async fn should_scope_to_one_workspace_when_workspace_id_is_given() {
    let (server, _workspace, _home) = assembled_server();
    let default_session = create_session(&server, "default-ws session").await;
    seed_task(&server, &default_session, "default-ws task").await;

    let new_root = tempfile::tempdir().expect("new workspace tempdir");
    let workspace_id = mirri_core::encode_work_dir_key(new_root.path());
    server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": new_root.path().to_string_lossy(), "name": "second" }),
        )
        .await
        .expect("createOrTouch resolves");
    let second_session = server
        .call_method(
            "session.create",
            json!({ "title": "second-ws session", "workspaceId": workspace_id }),
        )
        .await
        .expect("session.create routes to the workspace");
    let second_id = second_session["id"]
        .as_str()
        .expect("session id")
        .to_string();
    let second_task = seed_task(&server, &second_id, "second-ws task").await;

    // Scoped: only the second workspace's session contributes.
    let scoped = server
        .call_method("task.listWorkspace", json!({ "workspaceId": workspace_id }))
        .await
        .expect("task.listWorkspace");
    let rows = scoped.as_array().expect("bare array");
    assert_eq!(rows.len(), 1, "was: {scoped}");
    assert_eq!(rows[0]["id"], json!(second_task));
    assert_eq!(rows[0]["session_id"], json!(second_id));

    // The pinned host's default scope covers only the pinned default
    // workspace (D-2 — the serve face lists every registered workspace
    // instead).
    let unscoped = server
        .call_method("task.listWorkspace", json!({}))
        .await
        .expect("task.listWorkspace");
    let rows = unscoped.as_array().expect("bare array");
    assert_eq!(rows.len(), 1, "was: {unscoped}");
    assert_eq!(rows[0]["session_id"], json!(default_session));
}

/// Given an unknown workspaceId, when the list runs, then the answer is
/// an empty array (honest empty, not an error — the overview is a view,
/// not an addressing surface).
#[tokio::test]
async fn should_answer_an_empty_array_for_an_unknown_workspace() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "occupied").await;
    seed_task(&server, &session_id, "a live task").await;

    let listed = server
        .call_method(
            "task.listWorkspace",
            json!({ "workspaceId": "wd_does_not_exist" }),
        )
        .await
        .expect("the unknown workspace answers the honest empty array");

    assert_eq!(listed, json!([]));
}

/// Given a session whose task accumulated more output than the task-list
/// summary budget, when `task.list` runs with the default (summary) view,
/// then the `output` field carries only a tail of the accumulated output
/// with `output_truncated: true` — the summary view keeps a polling client's
/// per-second response bounded instead of hauling every task's full 1MB
/// cap along (S5: the task.list polling amplifier).
#[tokio::test]
async fn should_serve_a_truncated_output_tail_in_the_task_list_summary_view() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "chatty output").await;
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    let task_id = seed_task(&server, &session_id, "build").await;
    let big = "x".repeat(64 * 1024);
    session.tasks().append_output(&task_id, &big);

    let listed = server
        .call_method("task.list", json!({ "sessionId": session_id }))
        .await
        .expect("task.list");

    let row = listed
        .as_array()
        .expect("bare array")
        .iter()
        .find(|row| row["id"] == json!(task_id))
        .expect("the seeded task");
    let output = row["output"].as_str().expect("output is a string");
    assert!(
        output.len() < big.len(),
        "the summary must truncate, was {output_len}",
        output_len = output.len()
    );
    assert_eq!(row["output_truncated"], json!(true));
}

/// Given a task with a long command line, when `task.list` runs, then the
/// summary view truncates `kind.command` to the command budget (a long
/// build invocation is recognizable from its head; the full line lives in
/// the task detail faces).
#[tokio::test]
async fn should_truncate_the_command_line_in_the_task_list_summary_view() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "long command").await;
    let long_command = format!("echo {}", "y".repeat(2048));
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    session
        .tasks()
        .register_background(
            TaskKind::Process {
                command: Some(long_command),
                dispatcher: None,
            },
            "long".to_string(),
            session_id.clone(),
            None,
        )
        .expect("the background registration fits");

    let listed = server
        .call_method("task.list", json!({ "sessionId": session_id }))
        .await
        .expect("task.list");

    let row = listed
        .as_array()
        .expect("bare array")
        .iter()
        .find(|row| row["description"] == json!("long"))
        .expect("the seeded task");
    let command = row["kind"]["command"]
        .as_str()
        .expect("command is a string");
    assert!(
        command.chars().count() <= 256,
        "the summary command must be capped, was {}",
        command.chars().count()
    );
    assert!(
        command.starts_with("echo "),
        "the head of the command survives: {command}"
    );
}

/// Given the summary view, when `includeFull: true` rides the call, then
/// the full accumulated output and command answer (the detail face the
/// callers opt into explicitly).
#[tokio::test]
async fn should_serve_the_full_output_when_include_full_is_set() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "full view").await;
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    let task_id = seed_task(&server, &session_id, "build").await;
    let big = "x".repeat(64 * 1024);
    session.tasks().append_output(&task_id, &big);

    let listed = server
        .call_method(
            "task.list",
            json!({ "sessionId": session_id, "includeFull": true }),
        )
        .await
        .expect("task.list");

    let row = listed
        .as_array()
        .expect("bare array")
        .iter()
        .find(|row| row["id"] == json!(task_id))
        .expect("the seeded task");
    assert_eq!(row["output"].as_str().expect("output"), big);
    assert!(row.get("output_truncated").is_none());
}

/// Given a task whose output fits the summary budget, when `task.list`
/// runs, then the output answers whole and no `output_truncated` key rides
/// the row (the summary only flags what it actually cut).
#[tokio::test]
async fn should_not_flag_truncation_when_the_output_fits_the_summary_budget() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "small output").await;
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    let task_id = seed_task(&server, &session_id, "build").await;
    session.tasks().append_output(&task_id, &"ok".repeat(64));

    let listed = server
        .call_method("task.list", json!({ "sessionId": session_id }))
        .await
        .expect("task.list");

    let row = listed
        .as_array()
        .expect("bare array")
        .iter()
        .find(|row| row["id"] == json!(task_id))
        .expect("the seeded task");
    assert_eq!(row["output"], json!("ok".repeat(64)));
    assert!(row.get("output_truncated").is_none());
}

/// Given a session whose task accumulated more output than the summary
/// budget, when `task.listWorkspace` runs, then the SAME summary view
/// answers (the overview shares the serialization — a capped row there
/// too, or the aggregation face becomes the next amplifier).
#[tokio::test]
async fn should_apply_the_summary_view_to_task_list_workspace_too() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "ws summary").await;
    let session = server
        .resolve_session(&session_id)
        .await
        .expect("session is live");
    let task_id = seed_task(&server, &session_id, "build").await;
    let big = "x".repeat(64 * 1024);
    session.tasks().append_output(&task_id, &big);

    let listed = server
        .call_method("task.listWorkspace", json!({}))
        .await
        .expect("task.listWorkspace");

    let row = listed
        .as_array()
        .expect("bare array")
        .iter()
        .find(|row| row["id"] == json!(task_id))
        .expect("the seeded task");
    let output = row["output"].as_str().expect("output is a string");
    assert!(
        output.len() < big.len(),
        "the workspace summary must truncate"
    );
    assert_eq!(row["output_truncated"], json!(true));
}

/// Given no live sessions, when the list runs, then the answer is [].
#[tokio::test]
async fn should_answer_an_empty_array_when_no_session_is_live() {
    let (server, _tmp) = test_server(None);

    let listed = server
        .call_method("task.listWorkspace", json!({}))
        .await
        .expect("task.listWorkspace");

    assert_eq!(listed, json!([]));
}

/// Given more tasks than `limit`, when the list runs capped, then only the
/// first `limit` rows answer (the merged order truncates).
#[tokio::test]
async fn should_cap_the_answer_when_limit_is_given() {
    let (server, _tmp) = test_server(None);
    let session_id = create_session(&server, "chatty").await;
    seed_task(&server, &session_id, "one").await;
    seed_task(&server, &session_id, "two").await;
    seed_task(&server, &session_id, "three").await;

    let listed = server
        .call_method("task.listWorkspace", json!({ "limit": 2 }))
        .await
        .expect("task.listWorkspace");

    assert_eq!(listed.as_array().expect("bare array").len(), 2);
}
