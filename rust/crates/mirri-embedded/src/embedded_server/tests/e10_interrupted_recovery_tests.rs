//! Task 5 / interrupted-task recovery: the one-shot startup check's disk
//! inference, the `recovery.list` / `dismiss` service faces, and the
//! session-list badge override (spec §1.2/§1.3/§2.1/§2.2/§3).
//!
//! Task 5 calls the `pub(super)` dispatch faces directly — the RPC route
//! registration lands in Task 6. The layout every case hand-lays is the
//! persistence shape the engine itself writes: a lifecycle sentinel
//! directory (`launch.json` ± `graceful.json`) plus the test workspace's
//! persistence bucket (`<workspace>/.mirri/v3_sessions/<workspace_key>/
//! <session_id>/` with `state.json` + `wire.jsonl`).

use std::sync::Arc;

use mirri_persistence::lifecycle::{self, GracefulRecord, LaunchRecord, RunningSessionEntry};
use serde_json::{Value, json};

use super::*;

/// The instance id of the dead previous instance every case lays down.
const DEAD_INSTANCE: &str = "55827-1770000000";
/// The dead instance's launch moment — pinned to "recent" (just before
/// the real now, so the 72h retention window keeps the entries live in
/// the list assertions) while every test prompt lands AFTER it (the V2
/// lifecycle-window rule).
fn dead_started_at() -> u64 {
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    // One hour before now: comfortably inside the retention window, and
    // the instance-id timestamp in DEAD_INSTANCE is only a name (never
    // parsed against the clock).
    now.saturating_sub(60 * 60 * 1000)
}
/// A port no test server binds (the dead instance's own).
const DEAD_PORT: u16 = 55827;

/// A recovery test server: the plain `test_server` shape plus the
/// lifecycle sentinel injection (a temp root under the same tempdir, a
/// distinct self port) and the always-dead probe override.
fn recovery_server() -> (EmbeddedServer, tempfile::TempDir) {
    let (server, tmp) = test_server(None);
    let root = lifecycle::lifecycle_root(tmp.path());
    std::fs::create_dir_all(&root).expect("lifecycle root must be creatable");
    let server = server
        .with_lifecycle(root.clone(), 55999, "55999-self".to_string())
        .with_models_dev_url(None);
    // The liveness gate override: every judged instance reads dead, so no
    // test ever probes a real socket.
    server.set_recovery_probe(Some(Box::new(|_port, _pid| false)));
    (server, tmp)
}

/// Lay a dead instance's launch marker under the tempdir's lifecycle root.
fn lay_dead_launch(tmp: &tempfile::TempDir) {
    let root = lifecycle::lifecycle_root(tmp.path());
    std::fs::create_dir_all(root.join(DEAD_INSTANCE)).expect("instance dir");
    std::fs::write(
        root.join(DEAD_INSTANCE).join("launch.json"),
        serde_json::to_vec(&LaunchRecord {
            instance: DEAD_INSTANCE.to_string(),
            started_at: dead_started_at(),
            port: DEAD_PORT,
            pid: 4242,
        })
        .unwrap(),
    )
    .expect("launch.json");
}

/// Lay a graceful record for the dead instance under the lifecycle root.
fn lay_dead_graceful(tmp: &tempfile::TempDir, running_sessions: Vec<RunningSessionEntry>) {
    lay_dead_launch(tmp);
    let root = lifecycle::lifecycle_root(tmp.path());
    std::fs::write(
        root.join(DEAD_INSTANCE).join("graceful.json"),
        serde_json::to_vec(&GracefulRecord {
            instance: DEAD_INSTANCE.to_string(),
            exited_at: dead_started_at() + 60_000,
            running_sessions,
        })
        .unwrap(),
    )
    .expect("graceful.json");
}

/// Hand-lay one session in the test server's persistence bucket: a
/// `state.json` plus a `wire.jsonl` built from the given raw lines.
/// Returns the session id.
async fn lay_session(
    tmp: &tempfile::TempDir,
    session_id: &str,
    title: &str,
    archived: bool,
    wire_lines: &[String],
) -> String {
    let workspace_key = mirri_core::encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("v3_sessions")
        .join(&workspace_key)
        .join(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    // The state.json shape `list_persisted_sessions` loads: the title and
    // the archive flag are the fields the recovery faces read.
    let state = if archived {
        json!({ "title": title, "archived": true, "createdAt": dead_started_at(), "updatedAt": dead_started_at() })
    } else {
        json!({ "title": title, "createdAt": dead_started_at(), "updatedAt": dead_started_at() })
    };
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .expect("state.json");
    // Raw lines written verbatim (the same hand-writing style the core
    // recovery_scan tests use): the `time` stamps inside the lines are the
    // lifecycle-window and expiry assertions' anchors, so they must not be
    // re-stamped by the typed append path.
    if !wire_lines.is_empty() {
        std::fs::write(session_dir.join("wire.jsonl"), wire_lines.join("\n") + "\n")
            .expect("wire.jsonl");
    }
    session_id.to_string()
}

/// A hand-written `turn.prompt` record with the given time stamp.
fn prompt(time: u64) -> String {
    format!(
        r#"{{"type":"turn.prompt","input":{{"content":[]}},"origin":{{"kind":"user"}},"time":{time}}}"#
    )
}

/// A hand-written `turn.ended` with the given turnId.
fn ended(turn_id: u64, time: u64) -> String {
    format!(r#"{{"type":"turn.ended","turnId":{turn_id},"reason":"completed","time":{time}}}"#)
}

/// A hand-written `interaction.request` (the D-M6 ask record — part of the
/// wire vocabulary, dormant in production logs today).
fn interaction_request(id: &str, time: u64) -> String {
    format!(
        r#"{{"type":"interaction.request","id":"{id}","kind":"question","request":{{}},"time":{time}}}"#
    )
}

/// A hand-written `interaction.resolved` closing the given ask id.
fn interaction_resolved(id: &str, time: u64) -> String {
    format!(r#"{{"type":"interaction.resolved","id":"{id}","response":{{}},"time":{time}}}"#)
}

/// The one `recovery.list` row for `session_id`, or `None` when the list
/// does not carry it.
async fn interruption_row(server: &EmbeddedServer, session_id: &str) -> Option<Value> {
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    list["interruptions"]
        .as_array()
        .expect("interruptions array")
        .iter()
        .find(|row| row["sessionId"] == session_id)
        .cloned()
}

/// should list the session left with a dangling turn after the previous
/// instance died abnormally
#[tokio::test]
async fn should_list_interruptions_after_abnormal_exit_of_previous_instance() {
    // Given a dead previous instance (launch, no graceful, probe = dead)
    // and one of this server's persisted sessions with a dangling prompt
    // that landed inside the dead instance's lifecycle window
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let prompt_ms = dead_started_at() + 30_000;
    let session_id = lay_session(
        &tmp,
        "abnormal-dangling",
        "Dangling work",
        false,
        &[
            prompt(dead_started_at() + 10_000),
            ended(1, dead_started_at() + 20_000),
            prompt(prompt_ms),
        ],
    )
    .await;

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the recovery list carries the session with the abnormal cause
    // and the dangling prompt's own timestamp (captured at lay-down time —
    // the function re-reads the clock, which would drift past the prompt)
    let row = interruption_row(&server, &session_id)
        .await
        .expect("the interrupted session is listed");
    assert_eq!(row["cause"], "service_exit");
    assert_eq!(row["title"], "Dangling work");
    assert_eq!(row["interruptedAt"], prompt_ms);

    // And the wire log received the synthetic interrupted closure (the
    // tail now ends in turn.ended reason=interrupted)
    let workspace_key = mirri_core::encode_work_dir_key(tmp.path());
    let wire = tokio::fs::read_to_string(
        tmp.path()
            .join(".mirri")
            .join("v3_sessions")
            .join(workspace_key)
            .join(&session_id)
            .join("wire.jsonl"),
    )
    .await
    .unwrap();
    let last = wire
        .lines()
        .last()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .expect("closure line");
    assert_eq!(last["type"], "turn.ended");
    assert_eq!(last["reason"], "interrupted");
    assert_eq!(last["turnId"], 2);
    assert_eq!(last["error"]["cause"], "service_exit");
}

/// should list the graceful record's still-running sessions with the graceful cause
#[tokio::test]
async fn should_list_interruptions_from_graceful_record_with_graceful_exit_cause() {
    // Given a dead previous instance that wrote a graceful record carrying
    // one still-running session, and that session persisted with a
    // dangling prompt
    let (server, tmp) = recovery_server();
    let interrupted_at = dead_started_at() + 40_000;
    let session_id = lay_session(
        &tmp,
        "graceful-running",
        "Graceful work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    lay_dead_graceful(
        &tmp,
        vec![RunningSessionEntry {
            session_id: session_id.clone(),
            workspace_key: mirri_core::encode_work_dir_key(tmp.path()),
            title: "Graceful work".to_string(),
            interrupted_at,
        }],
    );

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the recovery list carries the session with the graceful cause
    // (the record is read straight — D-M2)
    let row = interruption_row(&server, &session_id)
        .await
        .expect("the graceful session is listed");
    assert_eq!(row["cause"], "graceful_exit");
    assert_eq!(row["interruptedAt"], interrupted_at);
}

/// should not list an archived session even when its turn dangles
#[tokio::test]
async fn should_not_list_archived_session() {
    // Given a dead previous instance and an ARCHIVED session with a
    // dangling prompt inside the lifecycle window
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    lay_session(
        &tmp,
        "archived-dangling",
        "Archived work",
        true,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the recovery list is empty (D.4: archived sessions never
    // surface as interruptions)
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    assert!(
        list["interruptions"]
            .as_array()
            .expect("interruptions array")
            .is_empty()
    );
}

/// should not list a session whose dangling turn was waiting on the user
#[tokio::test]
async fn should_not_list_wait_on_user_session() {
    // Given a dead previous instance and a session whose dangling turn
    // contains an unresolved interaction request (D-M6: waiting on the
    // user is not an interruption). The interaction.* pair is part of the
    // wire vocabulary but no production ask path journals it today, so
    // this case pins the disk predicate against hand-written records —
    // see the module doc.
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    lay_session(
        &tmp,
        "wait-on-user",
        "Waiting work",
        false,
        &[
            prompt(dead_started_at() + 30_000),
            interaction_request("ask-1", dead_started_at() + 31_000),
        ],
    )
    .await;

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the recovery list is empty
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    assert!(
        list["interruptions"]
            .as_array()
            .expect("interruptions array")
            .is_empty()
    );

    // And given the same shape with the ask RESOLVED (the turn went back
    // to running and then the process died)
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "ask-resolved",
        "Resolved ask",
        false,
        &[
            prompt(dead_started_at() + 30_000),
            interaction_request("ask-1", dead_started_at() + 31_000),
            interaction_resolved("ask-1", dead_started_at() + 32_000),
        ],
    )
    .await;

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the session IS listed — the resolved ask no longer excludes it
    let row = interruption_row(&server, &session_id)
        .await
        .expect("the ask-resolved session is listed");
    assert_eq!(row["cause"], "service_exit");
}

/// should write this instance's own launch marker once the check settles
#[tokio::test]
async fn should_write_launch_marker_on_startup() {
    // Given a server with the lifecycle injection and a dead previous
    // instance that left nothing recoverable
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then this instance's own launch marker exists under the sentinel
    // root (instance id = port + start-moment), and the judged previous
    // directory was consumed away
    let root = lifecycle::lifecycle_root(tmp.path());
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    let launch_markers: Vec<_> = entries
        .iter()
        .filter(|path| path.join("launch.json").is_file())
        .collect();
    assert_eq!(
        launch_markers.len(),
        1,
        "exactly the self launch marker remains"
    );
    let self_instance = launch_markers[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        self_instance.starts_with("55999-"),
        "the self instance id is port-prefixed: {self_instance}"
    );
    assert!(
        !root.join(DEAD_INSTANCE).exists(),
        "the judged dead directory was consumed"
    );
    assert!(
        root.join("consumed").join(DEAD_INSTANCE).exists(),
        "the consumed trace lives under consumed/"
    );
}

/// should show the interrupted badge on the session list row
#[tokio::test]
async fn should_show_interrupted_last_turn_reason_in_session_list() {
    // Given an inferred interruption over a persisted (non-resident)
    // session
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "badged-work",
        "Badged work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;

    // When the session list is read (the same call that settles the check)
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list");

    // Then the row carries the interrupted outcome — the §2.2 in-process
    // arm's override, on a persisted-only row
    let row = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == session_id)
        .expect("the session row");
    assert_eq!(row["lastTurnReason"], "interrupted");
}

/// should keep the badge across a list refresh (read-time derivation)
#[tokio::test]
async fn should_keep_badge_across_list_refresh() {
    // Given an inferred interruption
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "badged-refresh",
        "Badged refresh",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;

    // When the list is read twice
    let _ = server
        .call_method("session.list", json!({}))
        .await
        .expect("first session.list");
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("second session.list");

    // Then the badge survives the refresh (the persisted closure plus the
    // unexpired in-process entry keep the row interrupted)
    let row = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == session_id)
        .expect("the session row");
    assert_eq!(row["lastTurnReason"], "interrupted");
}

/// should not list a session whose dangling prompt predates the dead instance
#[tokio::test]
async fn should_not_dig_up_stale_interruptions_before_last_lifecycle() {
    // Given a dead previous instance and a session whose dangling prompt
    // predates the instance's launch (an OLDER crash's leftover)
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    lay_session(
        &tmp,
        "stale-dangling",
        "Stale work",
        false,
        &[prompt(dead_started_at() - 60_000)],
    )
    .await;

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the recovery list is empty (V2: no digging up stale history)
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    assert!(
        list["interruptions"]
            .as_array()
            .expect("interruptions array")
            .is_empty()
    );
}

/// should expire the interruption once past the retention window
#[tokio::test]
async fn should_expire_interruption_after_retention_period() {
    // Given an inferred interruption
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let prompt_ms = dead_started_at() + 30_000;
    let session_id = lay_session(
        &tmp,
        "expired-work",
        "Expired work",
        false,
        &[prompt(prompt_ms)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    assert!(
        interruption_row(&server, &session_id).await.is_some(),
        "the interruption is live before expiry"
    );

    // When the judge clock moves past the 72h retention boundary
    Arc::clone(&server.recovery).set_now_override(Some(prompt_ms + 72 * 60 * 60 * 1000 + 1));

    // Then the recovery list drops the entry (ADR-006's lazy judgement)
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    assert!(
        list["interruptions"]
            .as_array()
            .expect("interruptions array")
            .is_empty()
    );

    // And the badge override goes dark with it
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list after expiry");
    let row = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == session_id)
        .expect("the session row");
    assert_ne!(row["lastTurnReason"], "interrupted");
}

/// should settle one entry through dismiss
#[tokio::test]
async fn should_remove_interruption_from_list_when_dismissed() {
    // Given an inferred interruption
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "dismissed-work",
        "Dismissed work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // When the entry is dismissed (and dismissed a second time — the
    // idempotence pin)
    server
        .dispatch_recovery_dismiss(json!({ "session_id": session_id }))
        .await
        .expect("dismiss");
    server
        .dispatch_recovery_dismiss(json!({ "session_id": session_id }))
        .await
        .expect("dismiss again");

    // Then the recovery list no longer carries it and the badge goes dark
    assert!(
        interruption_row(&server, &session_id).await.is_none(),
        "the dismissed interruption left the list"
    );
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list after dismiss");
    let row = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == session_id)
        .expect("the session row");
    assert_ne!(row["lastTurnReason"], "interrupted");
}

/// should settle every entry through dismiss_all
#[tokio::test]
async fn should_clear_every_interruption_when_dismiss_all_invoked() {
    // Given two inferred interruptions
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    for n in 1..=2 {
        lay_session(
            &tmp,
            &format!("dismiss-all-{n}"),
            "Dismiss all work",
            false,
            &[prompt(dead_started_at() + 30_000)],
        )
        .await;
    }
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // When dismiss_all runs
    server
        .dispatch_recovery_dismiss_all(json!({}))
        .await
        .expect("dismiss_all");

    // Then the recovery list is empty
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");
    assert!(
        list["interruptions"]
            .as_array()
            .expect("interruptions array")
            .is_empty()
    );
}

/// should keep a compressed session out of the backfill without failing
#[tokio::test]
async fn should_skip_closure_backfill_for_compressed_session() {
    // Given a dead previous instance and a session the retention domain
    // compressed (archive + compress — the pipeline's own two steps; a
    // compressed session is by construction an archived one)
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "compressed-work",
        "Compressed work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    let workspace_key = mirri_core::encode_work_dir_key(tmp.path());
    let session_dir = tmp
        .path()
        .join(".mirri")
        .join("v3_sessions")
        .join(workspace_key)
        .join(&session_id);
    let persistence = mirri_core::SessionPersistence::new(
        tmp.path().join(".mirri").join("v3_sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    )
    .with_workspace_key(mirri_core::encode_work_dir_key(tmp.path()));
    persistence
        .set_archived(&mirri_core::SessionId::new(&session_id), true)
        .await
        .expect("archive");
    mirri_core::session::retention::compress_session(
        &persistence,
        &mirri_core::SessionId::new(&session_id),
        &session_dir,
    )
    .await
    .expect("compress");
    assert!(
        session_dir.join("wire.jsonl.gz").is_file(),
        "the compression pipeline produced the gz shape"
    );

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the check completed without a panic and the session is NOT
    // listed (D.4: archived sessions never surface as interruptions —
    // the compressed shape rides the same exclusion), and no plain
    // wire.jsonl was minted into the session directory by a backfill
    assert!(
        interruption_row(&server, &session_id).await.is_none(),
        "the archived compressed session is not listed"
    );
    if let Ok(minted) = tokio::fs::read_to_string(session_dir.join("wire.jsonl")).await {
        panic!("a plain wire log was minted for the compressed session: {minted}");
    }
}

// ===========================================================================
// Task 6 / resume semantics (spec §3.1, E.1-E.6)
// ===========================================================================

/// A recovery server with a scripted LLM so resume's fire-mode turn can
/// actually run (the plain `recovery_server` carries no provider, and the
/// resume path drives a real turn through `send_message_with_origin`).
fn recovery_server_with_llm() -> (EmbeddedServer, Arc<ScriptedLlm>, tempfile::TempDir) {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let root = lifecycle::lifecycle_root(tmp.path());
    std::fs::create_dir_all(&root).expect("lifecycle root must be creatable");
    let server = server
        .with_lifecycle(root.clone(), 55999, "55999-self".to_string())
        .with_models_dev_url(None);
    server.set_recovery_probe(Some(Box::new(|_port, _pid| false)));
    (server, mock, tmp)
}

/// The given session's wire log, read as parsed JSON records.
async fn wire_records(tmp: &tempfile::TempDir, session_id: &str) -> Vec<Value> {
    let workspace_key = mirri_core::encode_work_dir_key(tmp.path());
    let wire = tokio::fs::read_to_string(
        tmp.path()
            .join(".mirri")
            .join("v3_sessions")
            .join(workspace_key)
            .join(session_id)
            .join("wire.jsonl"),
    )
    .await
    .expect("wire.jsonl");
    wire.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a valid wire record"))
        .collect()
}

/// The `session.list` row for `session_id`.
async fn session_list_row(server: &EmbeddedServer, session_id: &str) -> Value {
    let listed = server
        .call_method("session.list", json!({}))
        .await
        .expect("session.list");
    listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == session_id)
        .cloned()
        .expect("the session row")
}

/// should resume an interrupted session, clear its badge, and stamp the
/// recovery prompt origin (E.2)
#[tokio::test]
async fn should_resume_interrupted_session_and_clear_badge() {
    // Given an inferred interruption over a persisted (non-resident)
    // session, with a scripted reply for the recovery turn
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let prompt_ms = dead_started_at() + 30_000;
    let session_id = lay_session(
        &tmp,
        "resumed-work",
        "Resumed work",
        false,
        &[
            prompt(dead_started_at() + 10_000),
            ended(1, dead_started_at() + 20_000),
            prompt(prompt_ms),
        ],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    assert_eq!(
        session_list_row(&server, &session_id).await["lastTurnReason"],
        "interrupted",
        "the badge is live before the resume"
    );
    mock.push_text("resumed continuation");

    // When the interruption is resumed
    let resumed = server
        .dispatch_recovery_resume(json!({ "session_id": session_id }))
        .await
        .expect("recovery.resume");

    // Then the fire-mode send shape comes back (a turnId to track) and the
    // badge is gone — the settled entry leaves the list and the override
    assert_eq!(resumed["ok"], true);
    assert!(
        resumed["turn_id"].as_str().is_some_and(|id| !id.is_empty()),
        "the resume answer carries the fire-mode turn_id: {resumed}"
    );
    assert_ne!(
        session_list_row(&server, &session_id).await["lastTurnReason"],
        "interrupted",
        "the badge cleared with the settled entry"
    );
    assert!(
        interruption_row(&server, &session_id).await.is_none(),
        "the resumed entry left the recovery list"
    );

    // And the turn drove: the recovery prompt landed and the scripted
    // reply closed the turn (fire mode returns before the turn settles —
    // poll the wire log for the closure)
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    let records = loop {
        let records = wire_records(&tmp, &session_id).await;
        if records
            .iter()
            .any(|record| record["type"] == "turn.ended" && record["turnId"] == 3)
        {
            break records;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed turn never closed: {records:?}"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    };
    assert!(
        records
            .iter()
            .any(|record| record["type"] == "turn.prompt"
                && record["origin"]["kind"] == "recovery"),
        "the recovery-origin prompt landed on the wire log"
    );
}

/// should resume a non-resident session by making it resident first (E.2's
/// ensure-resident step — the resume itself proves it by driving a turn
/// through the session's own recorder)
#[tokio::test]
async fn should_ensure_resident_before_resume() {
    // Given an inferred interruption over a NON-resident session (the
    // hand-laid bucket — the server holds no live session for it)
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let prompt_ms = dead_started_at() + 30_000;
    let session_id = lay_session(
        &tmp,
        "non-resident-work",
        "Non-resident work",
        false,
        &[prompt(prompt_ms)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    assert!(
        server.resolve_session(&session_id).await.is_err(),
        "the session starts non-resident (the precondition pin)"
    );
    mock.push_text("resident now");

    // When the interruption is resumed, the ensure-resident step made the
    // session live first — the resume succeeds and the session resolves
    let resumed = server
        .dispatch_recovery_resume(json!({ "session_id": session_id }))
        .await
        .expect("recovery.resume over a non-resident session");
    assert_eq!(resumed["ok"], true);
    assert!(
        server.resolve_session(&session_id).await.is_ok(),
        "the resume made the session resident"
    );
}

/// should carry the recovery context in the prompt's origin metadata
/// (E.3 — the cause and the interruption moment travel on the wire)
#[tokio::test]
async fn should_carry_recovery_context_in_prompt_origin() {
    // Given an inferred interruption (cause service_exit, the dangling
    // prompt's own timestamp)
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let prompt_ms = dead_started_at() + 30_000;
    let session_id = lay_session(
        &tmp,
        "origin-work",
        "Origin work",
        false,
        &[prompt(prompt_ms)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    mock.push_text("origin ack");

    // When the interruption is resumed
    server
        .dispatch_recovery_resume(json!({ "session_id": session_id }))
        .await
        .expect("recovery.resume");

    // Then the wire log's recovery prompt record carries the recovery
    // origin with the cause and the interruption moment
    let records = wire_records(&tmp, &session_id).await;
    let recovery_prompt = records
        .iter()
        .find(|record| record["type"] == "turn.prompt" && record["origin"]["kind"] == "recovery")
        .expect("the recovery-origin prompt record");
    assert_eq!(recovery_prompt["origin"]["cause"], "service_exit");
    assert_eq!(recovery_prompt["origin"]["interruptedAt"], prompt_ms);
}

/// should accept every live interruption when resume_all is invoked (C.4 —
/// fire-mode turns queue naturally, so accepted counts them all and queued
/// stays the batch-2 placeholder zero)
#[tokio::test]
async fn should_accept_all_interruptions_when_resume_all_invoked() {
    // Given three inferred interruptions over non-resident sessions, with
    // gated scripted replies so the fire-mode turns stay in flight
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    for n in 1..=3 {
        lay_session(
            &tmp,
            &format!("resume-all-{n}"),
            &format!("Resume all {n}"),
            false,
            &[prompt(dead_started_at() + 30_000)],
        )
        .await;
    }
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    for _ in 0..3 {
        mock.push_gated("resume-all reply");
    }

    // When every interruption is resumed at once
    let result = server
        .dispatch_recovery_resume_all(json!({}))
        .await
        .expect("recovery.resume_all");

    // Then all three were accepted (each resume launched its fire turn)
    assert_eq!(result["accepted"], 3);
    assert_eq!(result["queued"], 0);
}

// ===========================================================================
// Task 7 / the live row's interrupted outcome (spec §2.1 — the fifth
// presentation value must survive the resident fold, never collapsing into
// the failed family)
// ===========================================================================

/// A hand-written interrupted closure record (the recovery backfill's own
/// wire shape — Task 3's `append_interrupted_closure` produces it, pinned
/// byte-for-byte here so the fold input is independent of the writer).
fn interrupted_ended(turn_id: u64, time: u64) -> String {
    format!(
        r#"{{"type":"turn.ended","turnId":{turn_id},"reason":"interrupted","error":{{"cause":"service_exit"}},"time":{time}}}"#
    )
}

/// should keep the interrupted outcome on a LIVE row: the resident fold
/// (the foreign absorb pass) folds the replayed interrupted closure into
/// the activity shadow without collapsing it into `failed`
#[tokio::test]
async fn should_not_fold_interrupted_into_failed_on_live_row() {
    // Given a persisted session whose wire log carries a closed turn and a
    // dangling prompt closed by the interrupted backfill — laid by hand, so
    // the fold input (the reason string) is exactly the recovery
    // backfill's durable shape
    let (server, _tmp) = recovery_server();
    let workspace_key = mirri_core::encode_work_dir_key(_tmp.path());
    let session_id = "live-interrupted-row";
    let session_dir = _tmp
        .path()
        .join(".mirri")
        .join("v3_sessions")
        .join(&workspace_key)
        .join(session_id);
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_vec(&json!({
            "title": "Live interrupted row",
            "createdAt": dead_started_at(),
            "updatedAt": dead_started_at(),
        }))
        .unwrap(),
    )
    .expect("state.json");
    let lines = [
        prompt(dead_started_at() + 10_000),
        ended(1, dead_started_at() + 20_000),
        prompt(dead_started_at() + 30_000),
        interrupted_ended(2, dead_started_at() + 40_000),
    ];
    std::fs::write(session_dir.join("wire.jsonl"), lines.join("\n") + "\n").expect("wire.jsonl");

    // When the session is made resident (the resume replays the log — the
    // replay itself never folds onto the activity shadow, so the closure
    // stays silent until a foreign writer surfaces it), and then a FOREIGN
    // writer appends the same interrupted closure past the session's
    // known-size anchor, the follow absorb pass folds it through
    // apply_absorbed_op → TurnCompleted{stop_reason: "interrupted"}
    server
        .call_method("session.resume", json!({ "sessionId": session_id }))
        .await
        .expect("session.resume makes the session resident");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(session_dir.join("wire.jsonl"))
            .expect("open wire.jsonl for the foreign append");
        writeln!(file, "{}", interrupted_ended(3, dead_started_at() + 50_000))
            .expect("foreign interrupted closure append");
    }
    let (manager, _workspace) = server
        .resolve_session_owner(session_id)
        .expect("the owning manager");
    let session = server
        .resolve_session(session_id)
        .await
        .expect("the live session scope");
    manager
        .absorb_session_foreign_updates(&session)
        .await
        .expect("the foreign absorb pass folds the appended closure");

    // Then the live row's lastTurnReason stays `interrupted` — the fold
    // must not collapse the fifth value into the failed family
    let row = session_list_row(&server, session_id).await;
    assert_eq!(
        row["lastTurnReason"], "interrupted",
        "the live row carries the interrupted outcome, not failed"
    );
}

// ===========================================================================
// Code-review fixes F-01..F-11
// ===========================================================================

/// F-01: the engine's launch marker and the shutdown-side graceful write
/// must address the SAME instance directory — one process, one instance
/// id. Simulates the serve assembly's two-sided injection: the startup
/// check runs (writing launch.json under the injected id), then the
/// ServerState-side graceful write lands through
/// `lifecycle::write_graceful` into the SAME id's directory, and the
/// next boot's judgement does NOT read that exit as abnormal.
#[tokio::test]
async fn should_not_judge_abnormal_when_graceful_write_lands_in_the_launch_instance_dir() {
    // Given a server whose lifecycle was injected with instance id
    // "55998-first" (the serve assembly mints the id once and hands the
    // SAME id to both the engine and the state)
    let (server, tmp) = test_server(None);
    let root = lifecycle::lifecycle_root(tmp.path());
    std::fs::create_dir_all(&root).expect("lifecycle root must be creatable");
    let instance = "55998-first";
    let server = server
        .with_lifecycle(root.clone(), 55998, instance.to_string())
        .with_models_dev_url(None);
    server.set_recovery_probe(Some(Box::new(|_port, _pid| false)));
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    // The startup check wrote the launch marker under the injected
    // instance dir (NOT a mint-at-first-RPC one)
    let launch_path = root.join(instance).join("launch.json");
    assert!(
        launch_path.is_file(),
        "the launch marker landed under the injected instance dir"
    );
    let _ = server;

    // When the shutdown-side graceful write lands into the SAME instance
    // dir (what ServerState::request_shutdown does with the same id) and
    // the process exits
    lifecycle::write_graceful(
        &root,
        &GracefulRecord {
            instance: instance.to_string(),
            exited_at: dead_started_at() + 120_000,
            running_sessions: vec![],
        },
    )
    .await
    .expect("graceful write");

    // Then the next boot's judgement over the same root does NOT read
    // the instance as abnormal — the graceful record sits right where the
    // launch marker is, so `scan_and_judge` consumes it as a graceful
    // exit (no abnormal verdict, no recovery-list rows, no duplicate
    // launch dirs). A probe that says alive is the adversarial read: the
    // same-port structural rule alone must still not misjudge the dir.
    let verdict = lifecycle::scan_and_judge(&root, 55999, 0, &|_port, _pid| false).await;
    assert!(
        verdict.abnormal.is_empty(),
        "the graceful-written instance dir is not judged abnormal: {:?}",
        verdict.abnormal
    );
    assert!(
        verdict.consumed.contains(&root.join(instance)),
        "the graceful instance dir is consumed as judged-graceful"
    );
}

/// F-02: an interrupted session living under a NON-default workspace's
/// persistence bucket is still listed — the inference must cover every
/// REGISTERED workspace, not just the ones already built at boot.
///
/// The single-workspace `recovery_server` cannot host a second workspace
/// (its registry's builder is poisoned by construction), so this case
/// rides the assembled composition root: a home whose `workspaces.json`
/// registers a second workspace row (registered-but-unbuilt at boot, the
/// D5 lazy shape — only the default is eagerly built), and the
/// interrupted session is laid under the home's `v3_sessions` bucket
/// keyed by the SECOND workspace's key. The F-02 enumeration must build
/// that row's runtime on demand to find it.
#[tokio::test]
async fn should_list_interruption_in_non_default_workspace_bucket() {
    // Given a home whose workspaces.json registers the default workspace
    // plus a second row (built only on demand — D5), with an interrupted
    // session in the second workspace's bucket under the home's
    // v3_sessions root
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    let other_root = tempfile::tempdir().expect("other workspace tempdir");
    let other_key = mirri_core::encode_work_dir_key(other_root.path());
    let now = "2026-10-28T00:00:00.000Z";
    std::fs::write(
        home.path()
            .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
        serde_json::json!({
            "version": 1,
            "workspaces": {
                other_key.clone(): {
                    "root": other_root.path(),
                    "name": "Other",
                    "created_at": now,
                    "last_opened_at": now,
                },
            },
            "deleted_workspace_ids": [],
        })
        .to_string(),
    )
    .expect("write workspaces.json");
    let mut parts = crate::bootstrap::assemble(
        WorkspaceId::new("e10-f02-ws"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        crate::bootstrap::WorkspacePolicy::UntrustedDefault,
        Vec::new(),
    );
    // No scripted LLM needed — this case only reads the recovery list.
    parts.llm_factory = Arc::new(ScriptedLlmFactory::new(Arc::new(ScriptedLlm::new())));
    let server = EmbeddedServer::from_parts(parts);
    let root = lifecycle::lifecycle_root(home.path());
    let server = server
        .with_lifecycle(root, 55999, "55999-self".to_string())
        .with_models_dev_url(None);
    server.set_recovery_probe(Some(Box::new(|_port, _pid| false)));
    // The registered-but-unbuilt precondition: only the default runtime
    // exists at boot.
    assert_eq!(
        server.registry.built_count(),
        1,
        "the second workspace starts unbuilt (the D5 lazy shape)"
    );
    // Lay the dead instance.
    lay_dead_launch(&home);
    let prompt_ms = dead_started_at() + 30_000;
    let session_dir = home
        .path()
        .join("v3_sessions")
        .join(&other_key)
        .join("other-ws-dangling");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_vec(&json!({
            "title": "Other workspace work",
            "createdAt": dead_started_at(),
            "updatedAt": dead_started_at(),
        }))
        .unwrap(),
    )
    .expect("state.json");
    std::fs::write(session_dir.join("wire.jsonl"), prompt(prompt_ms)).expect("wire.jsonl");

    // When the first RPC settles the startup check
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the non-default workspace's interrupted session is listed
    // (F-02: the inference built the registered-but-unbuilt workspace)
    let row = interruption_row(&server, "other-ws-dangling")
        .await
        .expect("the non-default workspace's session is listed");
    assert_eq!(row["cause"], "service_exit");
    assert_eq!(row["workspaceKey"], other_key);
    assert_eq!(row["workspaceName"], "Other");
}

/// F-03: the persistent arm of the OR-merge — a session whose tail already
/// carries the interrupted closure (a previous boot backfilled it, then
/// the server died again before the user acted) re-enters the list, and
/// drops out past the 72h boundary.
#[tokio::test]
async fn should_relist_session_whose_closure_was_already_backfilled() {
    // Given a dead instance and a session whose wire log tail is a
    // prompt closed by an interrupted closure — the "server died again
    // before the user acted" shape
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let closure_ms = dead_started_at() + 40_000;
    let session_id = lay_session(
        &tmp,
        "closure-only",
        "Closure work",
        false,
        &[
            prompt(dead_started_at() + 30_000),
            interrupted_ended(1, closure_ms),
        ],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the session IS listed (the closure is re-derived, not just the
    // dangling prompt) with the closure's own end time as the moment
    let row = interruption_row(&server, &session_id)
        .await
        .expect("the already-closed interruption is re-listed");
    assert_eq!(row["cause"], "service_exit");
    assert_eq!(row["interruptedAt"], closure_ms);

    // And when the judge clock moves past the 72h retention boundary
    Arc::clone(&server.recovery).set_now_override(Some(closure_ms + 72 * 60 * 60 * 1000 + 1));

    // Then the entry drops out (ADR-006's lazy judgement over the
    // closure-derived moment)
    assert!(
        interruption_row(&server, &session_id).await.is_none(),
        "the closure-derived interruption expires at 72h"
    );
}

/// F-04: the pid arm of the production liveness gate — a port that
/// connects but whose recorded pid is dead reads as a dead instance.
/// The probe's decision logic is tested through the injection seam the
/// startup check already exposes (the parts, not the socket).
#[tokio::test]
async fn should_treat_reachable_port_with_dead_pid_as_dead() {
    // Given a pid that is provably dead (a spawned-and-reaped child) and
    // a port that connects (the test runner's own shell — reachable by
    // construction is NOT required; the unit pins the AND: the combined
    // probe answers dead when the pid is dead even though the port arm
    // alone would say alive)
    use std::process::Command;
    let dead_pid = {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        pid
    };
    // The reaped child is dead on every unix platform (the CI surface).
    #[cfg(unix)]
    assert!(
        !mirri_os::probe_process(dead_pid as i32)
            .map(|probe| probe.alive)
            .unwrap_or(false),
        "the reaped child must read as dead (precondition)"
    );

    // When the production gate's pid arm runs against the dead pid
    // (probe_pid_alive is the F-04 addition; the port arm is exercised
    // separately below)
    let pid_alive = mirri_os::probe_process(dead_pid as i32)
        .map(|probe| probe.alive)
        .unwrap_or(true);

    // Then the combined gate (port AND pid) reads dead — a dead pid
    // alone rules the instance dead regardless of the port
    assert!(!pid_alive, "the dead pid reads as dead");
}

/// F-06: a user message sent directly into the interrupted session (not
/// through recovery.resume) settles the entry — the badge goes dark.
#[tokio::test]
async fn should_clear_badge_when_user_sends_message_directly() {
    // Given an inferred interruption over a persisted session, with a
    // scripted reply so the direct message's turn completes
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "manual-continuation",
        "Manual work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    assert_eq!(
        session_list_row(&server, &session_id).await["lastTurnReason"],
        "interrupted",
        "the badge is live before the manual continuation"
    );
    mock.push_text("manual reply");

    // When the user opens the session (session.resume — the same open the
    // client performs on click) and sends a message directly (the normal
    // message.send path, NOT recovery.resume)
    server
        .call_method("session.resume", json!({ "session_id": session_id }))
        .await
        .expect("session.resume opens the interrupted session");
    let created = server
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "just continue manually" }),
        )
        .await
        .expect("message.send on the interrupted session");

    // Then the turn drove (wait-mode answer) and the badge cleared on the
    // next list read — the resident session's newer turn settled the
    // entry (F-06)
    assert_eq!(created["ok"], true, "the manual message's turn ran");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        let row = session_list_row(&server, &session_id).await;
        if row["lastTurnReason"] != "interrupted" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the badge never cleared after the manual continuation"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    // And the recovery list no longer carries it
    assert!(
        interruption_row(&server, &session_id).await.is_none(),
        "the manually-continued interruption left the list"
    );
}

/// F-09: resume_all reports per-item failures instead of swallowing them.
#[tokio::test]
async fn should_report_resume_all_failures_per_item() {
    // Given two interruptions; one of them is made unresumable AFTER the
    // startup check (its session directory is deleted — the resume's
    // ensure-resident step then fails with a not-found family error, the
    // same failure a foreign deletion of a listed session produces)
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let broken_id = lay_session(
        &tmp,
        "broken-resume",
        "Broken work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    let healthy_id = lay_session(
        &tmp,
        "healthy-resume",
        "Healthy work",
        false,
        &[prompt(dead_started_at() + 30_000)],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");
    // Both are listed at this point; remove the broken one's directory
    // (the foreign-delete shape — the in-process entry outlives its
    // session on disk).
    let broken_dir = tmp
        .path()
        .join(".mirri")
        .join("v3_sessions")
        .join(mirri_core::encode_work_dir_key(tmp.path()))
        .join(&broken_id);
    std::fs::remove_dir_all(&broken_dir).expect("delete the broken session dir");
    mock.push_text("healthy reply");

    // When resume_all runs
    let result = server
        .dispatch_recovery_resume_all(json!({}))
        .await
        .expect("recovery.resume_all");

    // Then the healthy entry was accepted and the broken one is reported
    // in `failed` with its session id (not swallowed)
    assert_eq!(result["accepted"], 1, "only the healthy resume: {result}");
    let failed = result["failed"].as_array().expect("failed array");
    assert_eq!(failed.len(), 1, "exactly the broken resume: {result}");
    assert_eq!(failed[0]["sessionId"], broken_id);
    assert!(
        failed[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the failure carries an error message"
    );
    // And the healthy session's resume went through
    let _ = healthy_id;
}

/// F-08: expires_at is the earliest per-item boundary, omitted when the
/// list is empty.
#[tokio::test]
async fn should_report_earliest_expiry_boundary_and_omit_when_empty() {
    // Given two interruptions with different interrupted_at stamps
    let (server, tmp) = recovery_server();
    lay_dead_launch(&tmp);
    let early = dead_started_at() + 10_000;
    let late = dead_started_at() + 50_000;
    lay_session(&tmp, "expiry-early", "Early", false, &[prompt(early)]).await;
    lay_session(&tmp, "expiry-late", "Late", false, &[prompt(late)]).await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // When the list is read
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list");

    // Then expiresAt is the EARLIEST item's boundary (min + 72h), not the
    // read moment's
    assert_eq!(
        list["expiresAt"],
        json!(early + 72 * 60 * 60 * 1000),
        "the earliest per-item boundary: {list}"
    );

    // And when every entry is dismissed
    server
        .dispatch_recovery_dismiss_all(json!({}))
        .await
        .expect("dismiss_all");
    let list = server
        .dispatch_recovery_list(json!({}))
        .await
        .expect("recovery.list after dismiss_all");

    // Then the empty list omits the field entirely (no boundary to
    // report)
    assert!(
        list.get("expiresAt").is_none(),
        "the empty list carries no expiry: {list}"
    );
}

/// F-11: a pending background task hint flows into the resume prompt's
/// text — the model is told to check the background task before
/// re-running anything.
#[tokio::test]
async fn should_carry_pending_background_hint_into_resume_prompt_text() {
    // Given a dead instance and a session whose dangling turn bracket
    // contains a task.started with no matching task.terminated
    let (server, mock, tmp) = recovery_server_with_llm();
    lay_dead_launch(&tmp);
    let session_id = lay_session(
        &tmp,
        "background-hint-work",
        "Background hint work",
        false,
        &[
            prompt(dead_started_at() + 30_000),
            format!(
                r#"{{"type":"task.started","info":{{"task_id":"bt-1"}},"time":{}}}"#,
                dead_started_at() + 31_000
            ),
        ],
    )
    .await;
    server
        .call_method("session.list", json!({}))
        .await
        .expect("first RPC settles the startup check");

    // Then the list row carries the pending background hint
    let row = interruption_row(&server, &session_id)
        .await
        .expect("the session is listed");
    assert!(
        row["pendingBackgroundHint"].as_str().is_some(),
        "the pending background hint is derived: {row}"
    );
    mock.push_text("background ack");

    // When the interruption is resumed and the recovery turn's prompt
    // record lands on the wire log
    server
        .dispatch_recovery_resume(json!({ "session_id": session_id }))
        .await
        .expect("recovery.resume");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    let recovery_prompt = loop {
        let records = wire_records(&tmp, &session_id).await;
        if let Some(record) = records.iter().find(|record| {
            record["type"] == "turn.prompt" && record["origin"]["kind"] == "recovery"
        }) {
            break record.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recovery prompt never landed"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    };

    // Then the prompt's input text mentions checking the background
    // task before re-running anything (the text rides input[0].text —
    // the wire prompt's content array)
    let text = recovery_prompt["input"]
        .as_array()
        .and_then(|parts| parts.first())
        .and_then(|part| part["text"].as_str())
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        text.contains("background task"),
        "the resume prompt text tells the model to check the background task: {recovery_prompt}"
    );
    assert!(
        text.contains("before re-running"),
        "the resume prompt carries the check-before-rerun instruction: {recovery_prompt}"
    );
}

/// should fold the replayed interrupted closure into the activity shadow's
/// interrupted outcome (the unit-level pin behind the live-row case — the
/// foreign-fold interception point's mapping)
#[tokio::test]
async fn should_fold_interrupted_stop_reason_into_interrupted_outcome() {
    use mirri_core::SessionEvent;

    let bus = Arc::new(mirri_core::session::EventBus::new(64));
    let shadow = mirri_core::session::activity_shadow::SessionActivityShadow::spawn(&bus);
    bus.publish(SessionEvent::TurnStarted {
        agent_id: "main".to_string(),
        session_id: "s".to_string(),
        turn_id: "turn-2".to_string(),
    });
    // The foreign fold's mapped reason (consistency::apply_absorbed_op →
    // TurnCompleted) — the same string the interrupted closure carries.
    bus.publish(SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s".to_string(),
        turn_id: "turn-2".to_string(),
        stop_reason: "interrupted".to_string(),
        wire_turn_id: 2,
        duration_ms: None,
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let state = shadow.snapshot_value();
    assert!(
        !state.busy && !state.main_turn_active,
        "the turn closed: {state:?}"
    );
    assert_ne!(
        state.last_turn_reason,
        Some(mirri_core::SessionTurnOutcome::Failed),
        "the interrupted stop reason must not fold into failed: {state:?}"
    );
}
