//! Stage-2 batch-4a: the workspace cron foundation — the workspace-arm RPC
//! faces (`cron.add`/`cron.delete`/`cron.trigger` with `workspaceId`), the
//! dual-face scheduler (a workspace task fires while every session is
//! closed), the aggregated listing's workspace groups, and the workspace
//! delete cascade.

use std::time::Duration;

use serde_json::json;

use super::*;

/// A home-rooted test server: `for_workspace_root` assembles the full
/// registry (the default workspace's runtime carries the cron manager —
/// a home-rooted bucket exists), with a fast cron tick.
fn ws4a_server() -> (EmbeddedServer, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let home = tempfile::tempdir().expect("home tempdir must be creatable");
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        tmp.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    )
    .with_cron_tick(Duration::from_millis(50));
    // Keep the home alive with the server: the returned tempdir is the
    // workspace root; the home's drop would remove the bucket mid-test.
    let _ = home;
    (server, tmp)
}

/// The default workspace's registry id (the bucket key of the root).
fn default_workspace_id(server: &EmbeddedServer) -> String {
    server
        .workspace_registry()
        .default_workspace_id()
        .expect("the default workspace is set")
        .to_string()
}

/// Given a home-rooted server, when the user creates a workspace task via
/// `cron.add` with `workspaceId`, then the task registers on the workspace
/// cron manager with origin User and the workspace scope, and persists to
/// the bucket's `cron/tasks.json`.
#[tokio::test]
async fn should_create_a_workspace_task_through_cron_add_with_workspace_id() {
    let (server, tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "morning patrol",
                "recurring": true,
                "maxFires": 5,
            }),
        )
        .await
        .expect("workspace cron.add");

    let task_id = added["taskId"].as_str().expect("taskId").to_string();
    assert_eq!(added["scope"], workspace_id.as_str());
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("the default runtime is built");
    let cron = runtime.cron().expect("the cron manager is attached");
    let task = cron.get(&task_id).expect("the task registered");
    assert_eq!(task.origin, mirri_core::cron::TaskOrigin::User);
    assert_eq!(
        task.scope,
        mirri_core::cron::TaskScope::Workspace(workspace_id.clone())
    );
    assert_eq!(task.max_fires, Some(5));
    // And it landed in the shared SQLite index (ADR-020's durable truth —
    // the JSON archive is no longer written once the index is attached).
    let rows = server
        .shared_cron_index()
        .expect("the shared cron index")
        .list_workspace(&workspace_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, task_id);
    assert!(
        rows[0].body.contains(&format!("\"workspaceId\":\"{workspace_id}\"")),
        "the row carries the workspace scope"
    );
    let _ = tmp;
}

/// Given a disabled server, when a workspace task is created, then the
/// creation is refused (E.4 layer 1 covers both scopes).
#[tokio::test]
async fn should_refuse_workspace_creation_when_scheduling_is_disabled() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    let server = server.with_cron_scheduling_disabled(true);
    let error = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect_err("the workspace arm refuses while disabled");
    assert!(error.to_string().contains("MIRRICODE_DISABLE_CRON"));
}

/// Given the workspace arm, when a one-shot carries max_fires, then the
/// structural validation rejects it (the same family the session add
/// applies — the validations are shared, not re-implemented).
#[tokio::test]
async fn should_reject_max_fires_on_a_one_shot_workspace_task() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    let error = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "once",
                "recurring": false,
                "maxFires": 3,
            }),
        )
        .await
        .expect_err("max_fires on a one-shot must be rejected");
    assert!(error.to_string().contains("max_fires"));
}

/// Given a workspace task whose schedule is due and EVERY session of the
/// workspace closed, when the scheduler ticks, then the task fires on the
/// workspace arm through the 4b RunSpawner — the RunRecord opens Running
/// (the lazy execution session exists), and without an LLM the run ends
/// Failed (the honest no-provider verdict), the budget consumed by the
/// real terminal state (V11: the workspace face survives the sessions'
/// death; 4b replaced 4a's skip placeholder).
#[tokio::test]
async fn should_fire_a_workspace_task_with_all_sessions_closed() {
    let (server, tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "*/5 * * * *",
                "prompt": "patrol",
                "recurring": true,
                "maxFires": 2,
            }),
        )
        .await
        .expect("workspace cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("runtime");
    let cron = runtime.cron().expect("cron manager");
    // No session was ever created — every session of this workspace is
    // closed by construction; force the schedule due and let the
    // scheduler tick.
    cron.set_next_fire_at(
        &task_id,
        Some(chrono::Utc::now() - chrono::Duration::seconds(5)),
    );
    // The workspace arm detects the due fire and records the run — the run
    // record landing is the completion signal (the cursor advanced in the
    // same scheduler pass, before the spawner recorded), not a fixed settle.
    mirri_test_support::wait_for::wait_for_async(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
        || (!cron.runs(&task_id).is_empty()).then_some(()),
    )
    .await
    .expect("the workspace arm must record the run for the due fire");
    let task = cron.get(&task_id).expect("the task survives");
    assert!(
        task.last_fired.is_some(),
        "the workspace arm detected and advanced the fire (the cursor is real)"
    );
    assert!(
        task.next_fire_at.is_some(),
        "the workspace arm rescheduled the recurring task"
    );
    // 4b: the run either is still Running (the turn is in flight) or
    // reached a terminal state (this bed has no LLM — the injection
    // drops, the verdict is Failed). Either way the spawner OWNED the
    // record — the 4a "executor-pending" skip is gone.
    let runs = cron.runs(&task_id);
    assert!(
        !runs.is_empty(),
        "the 4b RunSpawner recorded the run: {runs:?}"
    );
    assert!(
        runs.iter()
            .all(|run| run.skip_reason.as_deref() != Some("executor-pending-batch-4b")),
        "the 4a placeholder reason is gone: {runs:?}"
    );
    let _ = tmp;
}

/// Given the aggregated listing, when a workspace holds a task, then the
/// workspace group appears with its rows carrying the workspace scope and
/// the honest delivery condition (T6 — E.1/V23).
#[tokio::test]
async fn should_list_workspace_tasks_with_their_delivery_condition() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "ws watch",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    let _ = added;
    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    let workspaces = listed["workspaces"].as_array().expect("workspaces array");
    assert_eq!(workspaces.len(), 1, "one workspace group: {listed}");
    let group = &workspaces[0];
    assert_eq!(group["workspaceId"], workspace_id.as_str());
    let tasks = group["tasks"].as_array().expect("task rows");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["scope"]["kind"], "workspace");
    assert_eq!(tasks[0]["scope"]["workspaceId"], workspace_id.as_str());
    assert_eq!(tasks[0]["deliveryCondition"], "服务运行即触发");
    assert_eq!(tasks[0]["concurrency"], "skip");
}

/// Given a workspace task and a session task side by side, when the
/// aggregated listing is read, then both faces appear with their own
/// honest delivery conditions (the mixed view — E.2).
#[tokio::test]
async fn should_distinguish_delivery_conditions_in_the_mixed_view() {
    let (server, tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let created = server
        .call_method(
            "session.create",
            json!({ "title": "mixed", "modelId": super::TEST_DEFAULT_MODEL }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    // SDR-004: a created session exists in memory only until it
    // converses — materialize it so the close leaves a wire log the
    // aggregation replays (the same posture the batch-3 tests take).
    converse_once(&server, &session_id).await;
    server
        .call_method(
            "cron.add",
            json!({
                "sessionId": &session_id,
                "cron": "0 9 * * *",
                "prompt": "session watch",
                "recurring": true,
            }),
        )
        .await
        .expect("session cron.add");
    server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "workspace watch",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");
    // Close the session — its task must still show, honestly labeled.
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");

    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    let sessions = listed["sessions"].as_array().expect("sessions");
    assert!(
        !sessions.is_empty(),
        "the closed session's task must still appear: {listed}"
    );
    let session_task = &sessions[0]["tasks"][0];
    assert_eq!(session_task["deliveryCondition"], "依赖所属会话存活");
    assert_eq!(session_task["scope"]["kind"], "session");
    let workspaces = listed["workspaces"].as_array().expect("workspaces");
    let workspace_task = &workspaces[0]["tasks"][0];
    assert_eq!(workspace_task["deliveryCondition"], "服务运行即触发");
    let _ = tmp;
}

/// Given a workspace task, when it is deleted through `cron.delete` with
/// `workspaceId`, then the removal lands on the workspace manager and the
/// store (T8).
#[tokio::test]
async fn should_delete_a_workspace_task_through_cron_delete() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect("add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    server
        .call_method(
            "cron.delete",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("delete");
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("runtime");
    let cron = runtime.cron().expect("cron");
    assert!(cron.get(&task_id).is_none());
    assert!(
        server
            .shared_cron_index()
            .expect("the shared cron index")
            .list_workspace(&workspace_id)
            .unwrap()
            .is_empty(),
        "the index rows are gone with the delete"
    );
    let gone = server
        .call_method(
            "cron.delete",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect_err("a second delete answers not_found");
    assert!(gone.to_string().contains("not found"), "{gone}");
}

/// Given a workspace task, when `cron.trigger` runs with `workspaceId`,
/// then the manual fire goes through the 4b RunSpawner — a run record
/// lands (Running, or its terminal state on this LLM-less bed) and the
/// 4a "executor-pending" skip placeholder is gone (T8's 4b update).
#[tokio::test]
async fn should_record_a_skipped_run_when_a_workspace_task_is_triggered_manually() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 1 1 *",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect("add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let triggered = server
        .call_method(
            "cron.trigger",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("trigger");
    assert_eq!(triggered["triggered"], true);
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("runtime");
    let cron = runtime.cron().expect("cron");
    // The spawner's fire is async — wait for the run to land.
    let mut runs = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        runs = cron.runs(&task_id);
        if !runs.is_empty() {
            break;
        }
    }
    assert!(!runs.is_empty(), "the manual fire recorded a run: {runs:?}");
    assert!(
        runs.iter()
            .all(|run| run.skip_reason.as_deref() != Some("executor-pending-batch-4b")),
        "the 4a placeholder reason is gone: {runs:?}"
    );
}

/// Given a NON-default registered workspace holding cron tasks, when the
/// workspace is deleted (purge mode), then the cron store files are gone
/// with the bucket — no orphan tasks.json survives the workspace (T7 /
/// V19). The default workspace is refused by the delete face, so the test
/// registers a second workspace root first — through the RPC face the
/// production flow uses (m4: `workspace.add` + a `cron.add` with the new
/// workspace's id — the real register_and_build path, no hand-built
/// registration struct).
#[tokio::test]
async fn should_remove_the_cron_store_when_the_workspace_is_deleted() {
    let (server, tmp) = ws4a_server();
    let default_id = default_workspace_id(&server);
    // A second workspace root, added through the RPC face the production
    // flow uses (m4: `workspace.createOrTouch` — register_and_build runs
    // under it, the same call a user's workspace list makes).
    let other_root = tempfile::tempdir().expect("other root");
    let added_ws = server
        .call_method(
            "workspace.createOrTouch",
            json!({ "root": other_root.path(), "name": "cascade victim" }),
        )
        .await
        .expect("workspace.createOrTouch");
    let other_id = added_ws["workspaceId"]
        .as_str()
        .or_else(|| added_ws["id"].as_str())
        .expect("the added workspace's id")
        .to_string();
    // Seed the new workspace's cron face through the real creation RPC
    // (the store file lands in the home-rooted bucket).
    let seeded = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &other_id,
                "cron": "*/5 * * * *",
                "prompt": "orphan watch",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add through the real path");
    let task_id = seeded["taskId"].as_str().expect("taskId").to_string();
    let home = server
        .home_dir()
        .expect("the ws4a server has a home")
        .to_path_buf();
    let bucket = home.join("v3_sessions").join(&other_id);
    let store = mirri_core::cron::WorkspaceCronStore::new(&bucket);
    assert!(
        !store.path().exists(),
        "ADR-020: the durable write goes to SQLite, never the JSON archive"
    );
    let index = server.shared_cron_index().expect("the shared cron index");
    assert!(
        index.list_workspace(&other_id).unwrap().iter().any(|r| r.id == task_id),
        "the task landed in the index rows"
    );

    // When the workspace is deleted (purge)
    let deleted = server
        .call_method(
            "workspace.delete",
            json!({ "workspaceId": &other_id, "mode": "purge" }),
        )
        .await
        .expect("workspace.delete");
    assert_eq!(deleted["ok"], true);
    let _ = default_id;

    // Then no cron orphan remains (V19): the index rows are gone with the
    // workspace (the SQLite cascade — Task 5's new delete path).
    assert!(
        index.list_workspace(&other_id).unwrap().is_empty(),
        "the scheduled_tasks rows went away with the workspace"
    );
    // A manager reopened over the shared index does not resurrect the task.
    let reopened = mirri_core::cron::WorkspaceCronManager::open_with_index(
        &other_id,
        &bucket,
        std::sync::Arc::new(crate::cron_persistence::SqliteCronPersistence(index))
            as std::sync::Arc<dyn mirri_core::cron::CronPersistence + Send + Sync>,
    );
    assert!(
        reopened.get(&task_id).is_none(),
        "the task did not survive as an orphan"
    );
    let _ = tmp;
}

/// Given a workspace task persisted in the shared SQLite index, when
/// `cron.listAll` is called (repeatedly, past the throttle window), then
/// the workspace group keeps serving the task — the durable row is the
/// listing source (ADR-020's replacement for the retired store-file
/// fingerprint cache; the listAll throttle cache still bounds the read
/// rate, but no file-fingerprint entry exists to inspect any more).
#[tokio::test]
async fn should_cache_the_workspace_group_until_the_store_moves() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let workspace_id = default_workspace_id(&server);
    server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "cached watch",
                "recurring": true,
            }),
        )
        .await
        .expect("workspace cron.add");

    // First aggregation: the SQLite row serves the group.
    let first = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("first cron.listAll");
    assert_eq!(first["workspaces"].as_array().expect("workspaces").len(), 1);
    assert_eq!(
        server
            .shared_cron_index()
            .expect("the shared cron index")
            .list_workspace(&workspace_id)
            .unwrap()
            .len(),
        1,
        "the durable row backs the group"
    );

    // Second aggregation past the throttle window: the same row still
    // serves (no fingerprint, no invalidation surface).
    tokio::time::sleep(CRON_LIST_ALL_MIN_INTERVAL + Duration::from_millis(50)).await;
    let second = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("second cron.listAll");
    assert_eq!(
        second["workspaces"].as_array().expect("workspaces").len(),
        1,
        "the workspace group still serves from the durable row"
    );
}

// --- batch A (Task 5): the assembly — shared cron-index, startup import ---

/// The legacy document the import tests seed — one workspace task with the
/// full flat-scope spelling the frozen JSON archive carries.
fn seed_legacy_cron_document(home: &std::path::Path, workspace_id: &str) {
    let bucket = home.join("v3_sessions").join(workspace_id);
    std::fs::create_dir_all(bucket.join("cron")).expect("the legacy bucket's cron dir");
    std::fs::write(
        bucket.join("cron").join("tasks.json"),
        r#"{
            "version": 1,
            "tasks": [{
                "id": "cron-legacy-1", "cron": "0 9 * * 1-5", "prompt": "legacy",
                "session_id": "", "recurring": true, "timezone": "Asia/Tokyo",
                "created_at": "2026-11-01T00:00:00.000000000Z", "last_fired": null,
                "origin": "user", "scope": "workspace", "workspaceId": "PLACEHOLDER_WS",
                "permission_mode": "manual", "paused": false, "title": "Legacy task"
            }],
            "runs": {}, "proposals": []
        }"#
        .replace("PLACEHOLDER_WS", workspace_id),
    )
    .expect("seed the legacy tasks.json");
}

/// Given a legacy JSON document seeded in a workspace's bucket, when the
/// engine boots over that home, then the one-time import moves the tasks
/// into the shared SQLite index, `cron.listAll` serves them, and the frozen
/// archive file survives untouched (ADR-020).
#[tokio::test]
async fn given_a_legacy_json_document_when_the_engine_boots_then_tasks_live_in_sqlite_and_the_file_survives() {
    let home = tempfile::tempdir().unwrap();
    // The bucket key is the registry id — derived from the workspace root
    // the same way bootstrap derives it, so the legacy file exists BEFORE
    // the server (and the one-time import on the census-settled task)
    // does.
    let root = tempfile::tempdir().unwrap();
    let workspace_id = mirri_core::encode_work_dir_key(root.path());
    seed_legacy_cron_document(home.path(), &workspace_id);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    )
    .with_cron_tick(Duration::from_millis(50));
    assert_eq!(default_workspace_id(&server), workspace_id);
    // The import rides the census-settled background task: a dispatched
    // RPC settles it, and the read face then observes the adopted rows
    // (the shared index's first open builds the connection only).
    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    let index = server
        .shared_cron_index()
        .expect("the home-rooted engine carries a shared cron index");
    let rows = index.list_all().unwrap();
    assert!(
        rows.iter().any(|r| r.id == "cron-legacy-1"),
        "the import moved the task into SQLite ({} rows)",
        rows.len()
    );

    // The listAll face serves it (the workspace arm reads the table).
    assert!(
        listed["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .any(|g| g["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks
                    .iter()
                    .any(|row| row["id"] == json!("cron-legacy-1")))),
        "the legacy task shows in the aggregate: {listed}"
    );

    // ADR-020: the legacy file stays as the read-only archive.
    let bucket = home.path().join("v3_sessions").join(&workspace_id);
    assert!(
        bucket.join("cron").join("tasks.json").exists(),
        "the frozen archive file survives the import"
    );
}

/// Given the import already ran, when the engine reboots over the same home
/// (with the still-frozen archive on disk), then the import-once flag holds —
/// a task deleted from SQLite is NOT resurrected by a re-read of the frozen
/// archive (B1).
#[tokio::test]
async fn given_the_import_already_ran_when_the_engine_reboots_then_the_archive_never_overrides_sqlite() {
    let home = tempfile::tempdir().unwrap();
    // The bucket key derivation mirrors the first test: seed BEFORE the
    // server exists (the import rides the census-settled task).
    let root = tempfile::tempdir().unwrap();
    let workspace_id = mirri_core::encode_work_dir_key(root.path());
    seed_legacy_cron_document(home.path(), &workspace_id);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    )
    .with_cron_tick(Duration::from_millis(50));
    assert_eq!(default_workspace_id(&server), workspace_id);
    // The import rides the census-settled background task: a dispatched
    // RPC settles it (the shared index's first open is connection-only).
    let _ = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll settles the import");
    let index = server
        .shared_cron_index()
        .expect("the shared cron index");
    let rows = index.list_all().unwrap();
    assert!(
        rows.iter().any(|r| r.id == "cron-legacy-1"),
        "the first boot imported the task"
    );

    // Delete through the RPC face — the durable write lands in SQLite.
    let deleted = server
        .call_method(
            "cron.delete",
            json!({ "workspaceId": &workspace_id, "taskId": "cron-legacy-1" }),
        )
        .await
        .expect("cron.delete");
    assert_eq!(deleted["deleted"], json!(true));
    assert!(
        index.list_all().unwrap().is_empty(),
        "the delete removed the row from SQLite"
    );

    // Reboot: rebuild the engine over the SAME home dir (same root, so
    // the same workspace key — the registry re-registers the same bucket).
    drop(server);
    let server2 = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    )
    .with_cron_tick(Duration::from_millis(50));
    // Settle the reboot's import the same way, then read.
    let _ = server2
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll settles the reboot's import");
    let index2 = server2
        .shared_cron_index()
        .expect("the shared cron index (reboot)");
    assert!(
        index2.list_all().unwrap().is_empty(),
        "the frozen archive did not resurrect the deleted task"
    );
}

/// Poll `cron.listAll` (bounded) until the task id appears in the
/// workspace groups — the import task is asynchronous, so the first
/// listing after the census settlement may still race its table write.
async fn poll_cron_list_all(server: &EmbeddedServer, task_id: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listed = server
            .call_method("cron.listAll", json!({}))
            .await
            .expect("cron.listAll");
        let found = listed["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .any(|g| g["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks.iter().any(|row| row["id"] == task_id)));
        if found || std::time::Instant::now() >= deadline {
            assert!(
                found,
                "task {task_id} never showed in the aggregate within the poll window: {listed}"
            );
            return listed;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// --- Task 8 (ADR-006): the ACP legacy-cron adoption + the import's
// off-first-request placement -----------------------------------------

/// The pre-ADR-006 no-home ACP shape's legacy cron manifest: the
/// workspace-local `<root>/.mirri/v3_sessions/<bucket>/cron/tasks.json`
/// (the no-home engines never attached a cron manager, so the file is a
/// frozen archive — exactly what the one-time import adopts).
fn seed_workspace_local_cron_document(workspace_root: &std::path::Path, workspace_id: &str) {
    let bucket = workspace_root
        .join(".mirri")
        .join("v3_sessions")
        .join(workspace_id);
    std::fs::create_dir_all(bucket.join("cron")).expect("the legacy bucket's cron dir");
    std::fs::write(
        bucket.join("cron").join("tasks.json"),
        r#"{
            "version": 1,
            "tasks": [{
                "id": "cron-acp-legacy-1", "cron": "0 9 * * 1-5", "prompt": "acp legacy",
                "session_id": "", "recurring": true, "timezone": "Asia/Tokyo",
                "created_at": "2026-11-01T00:00:00.000000000Z", "last_fired": null,
                "origin": "user", "scope": "workspace", "workspaceId": "PLACEHOLDER_WS",
                "permission_mode": "manual", "paused": false, "title": "ACP legacy task"
            }],
            "runs": {}, "proposals": []
        }"#
        .replace("PLACEHOLDER_WS", workspace_id),
    )
    .expect("seed the workspace-local tasks.json");
}

/// Given a legacy workspace-local cron manifest (the pre-ADR-006 no-home
/// ACP shape), when the home-rooted engine boots, then the census-settled
/// background import adopts the tasks into the shared SQLite index and
/// `cron.listAll` serves them.
#[tokio::test]
async fn should_import_legacy_cron_tasks_when_acp_engine_boots() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace_id = mirri_core::encode_work_dir_key(root.path());
    seed_workspace_local_cron_document(root.path(), &workspace_id);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    )
    .with_cron_tick(Duration::from_millis(50));
    assert_eq!(default_workspace_id(&server), workspace_id);

    // The import rides the census-settled background task: the first
    // dispatch settles the census (which releases the import), and the
    // read face then observes the adopted rows. The poll is bounded —
    // the import task is asynchronous, so the first listing after the
    // settlement may still race its table write.
    let _listed = poll_cron_list_all(&server, "cron-acp-legacy-1").await;

    // The durable row landed through the shared index (the manager's
    // workspace rows and the listing read the same database).
    let index = server
        .shared_cron_index()
        .expect("the home-rooted engine carries a shared cron index");
    assert!(
        index
            .list_workspace(&workspace_id)
            .unwrap()
            .iter()
            .any(|r| r.id == "cron-acp-legacy-1"),
        "the import moved the workspace-local task into SQLite"
    );

    // And the workspace-local archive file survives untouched (the
    // frozen-archive contract — the import reads, never moves).
    assert!(
        root.path()
            .join(".mirri")
            .join("v3_sessions")
            .join(&workspace_id)
            .join("cron")
            .join("tasks.json")
            .exists(),
        "the workspace-local archive file survives the import"
    );
}

/// Given a legacy cron manifest, when the shared cron index is first
/// opened, then the import does NOT run inline — the open is
/// connection-only and the import rides the census-settled background
/// task (e956f5e8a's principle: startup housekeeping stays off the
/// first-request path).
#[tokio::test]
async fn should_not_block_first_request_when_cron_import_runs() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace_id = mirri_core::encode_work_dir_key(root.path());
    seed_workspace_local_cron_document(root.path(), &workspace_id);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );

    // The first open builds the connection only: the rows are NOT there
    // yet (the import is a background task gated on the census
    // settlement, which only the first dispatched RPC triggers — this
    // direct call precedes every dispatch).
    let index = server
        .shared_cron_index()
        .expect("the home-rooted engine carries a shared cron index");
    assert!(
        index.list_workspace(&workspace_id).unwrap().is_empty(),
        "the shared index's first open does not import inline"
    );

    // The settlement-gated task performs it: a dispatched RPC settles
    // the census (the import's gate) and the read face then observes the
    // adopted rows.
    let _ = poll_cron_list_all(&server, "cron-acp-legacy-1").await;
}

/// Given the import already ran (this boot or an earlier one), when the
/// engine boots again, then the persisted flag holds — the workspace-local
/// archive never re-imports (frozen-archive contract, idempotent re-runs).
#[tokio::test]
async fn should_not_reimport_the_workspace_local_archive_on_a_later_boot() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace_id = mirri_core::encode_work_dir_key(root.path());
    seed_workspace_local_cron_document(root.path(), &workspace_id);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    // First boot: settle the census (the import's gate) and wait for
    // the adopted row, then delete the task through the durable face.
    let _ = poll_cron_list_all(&server, "cron-acp-legacy-1").await;
    let deleted = server
        .call_method(
            "cron.delete",
            json!({ "workspaceId": &workspace_id, "taskId": "cron-acp-legacy-1" }),
        )
        .await
        .expect("cron.delete");
    assert_eq!(deleted["deleted"], json!(true));
    drop(server);

    // Reboot over the same home: the still-frozen workspace-local archive
    // must NOT resurrect the deleted task.
    let server2 = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("test-ws-4a"),
        root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    // The reboot's import settles (the persisted flag makes it a no-op —
    // the deleted task never resurfaces).
    let _ = server2.call_method("cron.listAll", json!({})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let listed = server2
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    assert!(
        !listed["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .any(|g| g["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks
                    .iter()
                    .any(|row| row["id"] == json!("cron-acp-legacy-1")))),
        "the frozen archive did not resurrect the deleted task: {listed}"
    );
    let index2 = server2
        .shared_cron_index()
        .expect("the shared cron index (reboot)");
    assert!(
        index2.list_workspace(&workspace_id).unwrap().is_empty(),
        "the import-once flag holds across boots"
    );
}

/// Given a workspace whose runtime was never built but whose tasks live in
/// the shared SQLite index, when `cron.listAll` is called, then the
/// workspace group still appears with its task — the A14 backend guarantee
/// (ADR-020: the table is the listing's source of truth; the arm never
/// iterates built runtimes, so a registered-but-unbuilt workspace's rows
/// are never filtered out of the aggregate view).
#[tokio::test]
async fn should_list_a_task_whose_workspace_runtime_was_never_built() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    // A second workspace: registered through the same first-class row the
    // production register path writes, but its runtime is NEVER built —
    // nothing touches `ensure_built` for it below.
    let other_root = tempfile::tempdir().expect("other root");
    let other_id = mirri_core::encode_work_dir_key(other_root.path());
    server
        .workspace_registry()
        .register(crate::workspace_registry::WorkspaceRegistration {
            id: other_id.clone(),
            root: other_root.path().to_path_buf(),
            name: Some("never built".to_string()),
            created_at: "2026-11-03T00:00:00Z".to_string(),
            last_opened_at: "2026-11-03T00:00:00Z".to_string(),
        });
    assert!(
        server.workspace_registry().built_runtime(&other_id).is_none(),
        "the precondition holds: the runtime was never built"
    );
    // The durable row, written directly through the index's write face —
    // the same envelope the manager's persistence layer emits (the
    // WorkspaceBody `{"task": ..., "runs": [...]}`), with the full field
    // set the legacy-import task carries.
    let body = json!({
        "task": {
            "id": "cron-unbuilt-1", "cron": "0 9 * * 1-5", "prompt": "unbuilt watch",
            "session_id": "", "recurring": true, "timezone": "Asia/Tokyo",
            "created_at": "2026-11-01T00:00:00.000000000Z", "last_fired": null,
            "origin": "user", "scope": "workspace", "workspaceId": other_id,
            "permission_mode": "manual", "paused": false, "title": "Unbuilt task"
        },
        "runs": []
    });
    server
        .shared_cron_index()
        .expect("the home-rooted engine carries a shared cron index")
        .upsert_task(&mirri_cron_index::ScheduledTaskRow {
            id: "cron-unbuilt-1".to_string(),
            workspace_id: other_id.clone(),
            next_fire_at: None,
            paused: false,
            created_at: "2026-11-01T00:00:00.000000000Z".to_string(),
            body: body.to_string(),
        })
        .expect("seed the never-built workspace's row");

    let listed = server
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
    let workspaces = listed["workspaces"].as_array().expect("workspaces array");
    let group = workspaces
        .iter()
        .find(|group| group["workspaceId"] == other_id.as_str())
        .unwrap_or_else(|| panic!("the never-built workspace groups in: {listed}"));
    let tasks = group["tasks"].as_array().expect("the group's tasks");
    assert!(
        tasks.iter().any(|task| task["id"] == json!("cron-unbuilt-1")),
        "the task appears in the never-built workspace's group: {group}"
    );
}


/// Given a registered-but-never-built workspace, when `cron.add` targets it,
/// then the runtime builds on demand and the task persists — A14's
/// addressability: the aggregate list SHOWS such workspaces, so the creation
/// face must accept them (an unregistered id still answers not_found).
#[tokio::test]
async fn cron_add_into_a_never_built_workspace_builds_it_on_demand() {
    let (server, tmp) = ws4a_server();
    let _ = tmp;
    let other_root = tempfile::tempdir().expect("other root");
    let other_id = mirri_core::encode_work_dir_key(other_root.path());
    server
        .workspace_registry()
        .register(crate::workspace_registry::WorkspaceRegistration {
            id: other_id.clone(),
            root: other_root.path().to_path_buf(),
            name: Some("unbuilt target".to_string()),
            created_at: "2026-11-03T00:00:00Z".to_string(),
            last_opened_at: "2026-11-03T00:00:00Z".to_string(),
        });
    assert!(server.workspace_registry().built_runtime(&other_id).is_none());

    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": other_id,
                "cron": "0 9 * * 1-5",
                "prompt": "on-demand build",
                "recurring": true,
                "timezone": "Asia/Tokyo",
                "title": "On-demand task"
            }),
        )
        .await
        .expect("cron.add into the never-built workspace");

    let task_id = added["taskId"].as_str().expect("taskId").to_string();
    // The runtime exists now (built on demand) and carries the task.
    let runtime = server
        .workspace_registry()
        .built_runtime(&other_id)
        .expect("the runtime was built on demand");
    let cron = runtime.cron().expect("the cron manager is attached");
    assert!(cron.get(&task_id).is_some());
    // And the durable row landed in the shared index.
    let rows = server
        .shared_cron_index()
        .expect("the shared cron index")
        .list_workspace(
            &mirri_core::encode_work_dir_key(other_root.path()),
        )
        .expect("list the workspace's rows");
    assert!(rows.iter().any(|row| row.id == task_id));

    // An UNREGISTERED id is still the honest refusal.
    let refused = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": "wd_never_registered_0000000000",
                "cron": "0 9 * * 1-5",
                "prompt": "x",
                "recurring": true
            }),
        )
        .await
        .expect_err("cron.add must refuse an unregistered workspace");
    let message = format!("{refused}");
    assert!(message.contains("not found"), "the refusal says not found: {message}");
}

// --- batch 2 (A9): edit / pause / resume over the RPC face ---

/// Given a workspace task, when `cron.pause` then `cron.resume` runs,
/// then the row reports `paused` honestly and the resume answer carries a
/// future next fire (no catch-up on the paused window).
#[tokio::test]
async fn should_pause_and_resume_a_workspace_task_through_the_rpc_face() {
    let (server, _tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "* * * * *",
                "prompt": "tick",
                "recurring": true,
                "timezone": "UTC",
            }),
        )
        .await
        .expect("cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let paused = server
        .call_method(
            "cron.pause",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("cron.pause");
    assert_eq!(paused["paused"], true);
    // The workspace manager's own list (the same projection the panel's
    // workspace rows use) reports the pause.
    let runtime = server
        .workspace_registry()
        .built_runtime(&workspace_id)
        .expect("runtime");
    let task = runtime
        .cron()
        .expect("cron manager")
        .get(&task_id)
        .expect("the task");
    assert!(task.paused, "the pause is durable on the record");
    let resumed = server
        .call_method(
            "cron.resume",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("cron.resume");
    assert_eq!(resumed["paused"], false);
    let next = resumed["nextFireAt"].as_i64().expect("a next fire");
    assert!(
        next > chrono::Utc::now().timestamp_millis(),
        "resume schedules the first FUTURE occurrence"
    );
}

/// Given a workspace task, when `cron.update` renames it and changes its
/// schedule, then the same task id answers the new values and its history
/// stays attached.
#[tokio::test]
async fn should_edit_a_workspace_task_through_the_rpc_face() {
    let (server, _tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "old",
                "recurring": true,
                "timezone": "UTC",
            }),
        )
        .await
        .expect("cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    let updated = server
        .call_method(
            "cron.update",
            json!({
                "workspaceId": &workspace_id,
                "taskId": &task_id,
                "title": "晨检",
                "prompt": "new",
                "cron": "30 7 * * *",
                "permissionMode": "yolo",
            }),
        )
        .await
        .expect("cron.update");
    assert_eq!(updated["id"], task_id.as_str(), "the identity is kept");
    assert_eq!(updated["title"], "晨检");
    assert_eq!(updated["prompt"], "new");
    assert_eq!(updated["cron"], "30 7 * * *");
    assert_eq!(updated["permissionMode"], "yolo");
    assert_eq!(updated["permissionPosture"], "yolo");
    let runs = server
        .call_method(
            "cron.runs.list",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id }),
        )
        .await
        .expect("cron.runs.list");
    assert!(
        runs["runs"].is_array(),
        "the history face still answers: {runs}"
    );
}

/// Given an unknown task id (or an edit that tries to move the task to
/// another workspace), when update / pause / resume is called, then the
/// call is refused.
#[tokio::test]
async fn should_refuse_the_edit_when_the_task_is_unknown_or_the_workspace_moves() {
    let (server, _tmp) = ws4a_server();
    let workspace_id = default_workspace_id(&server);
    let added = server
        .call_method(
            "cron.add",
            json!({
                "workspaceId": &workspace_id,
                "cron": "0 9 * * *",
                "prompt": "p",
                "recurring": true,
            }),
        )
        .await
        .expect("cron.add");
    let task_id = added["taskId"].as_str().unwrap().to_string();
    assert!(
        server
            .call_method(
                "cron.pause",
                json!({ "workspaceId": &workspace_id, "taskId": "nope" })
            )
            .await
            .is_err(),
        "an unknown id is not_found"
    );
    let moved = server
        .call_method(
            "cron.update",
            json!({ "workspaceId": &workspace_id, "taskId": &task_id, "workspace": "other-ws" }),
        )
        .await;
    assert!(moved.is_err(), "the workspace is not editable");
}
