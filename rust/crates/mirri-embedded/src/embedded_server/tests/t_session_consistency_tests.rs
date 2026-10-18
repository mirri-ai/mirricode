//! Session-consistency batch 1, Task 5: the foreground follow driver.
//!
//! The follow driver (behind the `session-consistency-follow` experimental
//! flag) is spawned on `session.resume` and ticks an incremental absorb of
//! foreign journal appends. These tests exercise the driver through two
//! `EmbeddedServer` instances sharing one home (sessions dir) and one
//! workspace root — the dual-instance shape the spec's US C1.2 describes.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::*;
use mirri_transport::adapter::EngineAdapter;
use mirri_transport::adapter::EngineError as RpcEngineError;

/// Build one dual-instance test server: a session manager whose persistence
/// directory sits under the SHARED home (both instances write the same
/// `<home>/sessions/<workspace-key>/<session>` bucket), the consistency
/// follow flag enabled through the config `[experimental]` layer (no env
/// mutation — parallel tests keep their process environment untouched),
/// and a fast follow tick.
fn consistency_server(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    tick: Duration,
) -> EmbeddedServer {
    consistency_server_with_llm(home, workspace_root, tick, None)
}

/// The LLM-capable variant used by the in-flight-turn test: the follow
/// wiring is identical, plus a scripted-LLM factory so real engine turns
/// can run (and park mid-stream) on this instance.
fn consistency_server_with_llm(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    tick: Duration,
    llm_factory: Option<Arc<dyn mirri_core::llm::LlmFactory>>,
) -> EmbeddedServer {
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("dual-instance-ws"),
        workspace_root.to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::with_persistence_dir(
        workspace,
        home.join("sessions"),
        Arc::new(mirri_os::LocalFs::new()),
    ));
    let mut parsed = ServerConfig::from_toml(&format!(
        "\n[experimental]\n\"{}\" = true\n",
        mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG
    ))
    .expect("test config parses");
    parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    EmbeddedServer::with_providers(
        manager,
        llm_factory,
        None,
        None,
        None,
        Arc::new(RwLock::new(parsed)),
        None,
    )
    .with_follow_tick(tick)
}

/// Given two servers sharing a home, when instance A records a message op on
/// a session that instance B resumed, then B's event stream carries the
/// corresponding `event.message.received` within the follow latency
/// budget, and later ticks do not duplicate it (the absorb is idempotent —
/// the anchor advanced past the foreign bytes).
#[tokio::test]
async fn should_follow_foreign_writes_when_another_server_records_ops_on_shared_home() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    let server_b = consistency_server(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
    );

    // A creates the session and materializes it (SDR-004: a created
    // session holds no journal until materialization — A's scope keeps the
    // live writer that records the foreign op below).
    let created = server_a
        .call_method("session.create", json!({ "title": "follow test" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("A's created session is live");
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session (journal on disk)");

    // B subscribes BEFORE resuming, so the relay is installed when the
    // absorbed event lands on the session bus.
    let mut events = server_b.subscribe_events().await.expect("B subscribes");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // A records one ContextAppendMessage op on the session (the foreign
    // write B must follow) — A's own writer appends at the true EOF.
    session_a
        .record_ops(&[mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "foreign hello from A",
            }),
        }])
        .await
        .expect("A records the foreign message");

    // Then: B's stream carries the message within the C1.2 latency budget
    // (5s), and exactly once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut seen = 0usize;
    while tokio::time::Instant::now() < deadline && seen == 0 {
        if let Ok(Some(notification)) =
            tokio::time::timeout(Duration::from_millis(200), events.recv()).await
            && notification.method == "event.message.received"
            && notification.params["sessionId"] == session_id
            && notification.params["contentSummary"]
                .as_str()
                .is_some_and(|s| s.contains("foreign hello from A"))
        {
            seen += 1;
        }
    }
    assert_eq!(seen, 1, "the foreign message must arrive exactly once");

    // And: three more follow ticks pass with no duplicate (idempotence —
    // the anchor advanced past the foreign bytes, Fresh on re-probe).
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(notification) = events.try_recv() {
        if notification.method == "event.message.received"
            && notification.params["sessionId"] == session_id
            && notification.params["contentSummary"]
                .as_str()
                .is_some_and(|s| s.contains("foreign hello from A"))
        {
            seen += 1;
        }
    }
    assert_eq!(
        seen, 1,
        "later ticks must not duplicate the absorbed message"
    );
}

/// Given two servers sharing one home, when instance A (with no injected
/// home, so no index write-through — the foreign-entry posture) renames a
/// materialized session on the shared sessions root, then instance B's
/// `session.list` answers the new title within the C1.1 freshness budget —
/// driven by B's per-server index refresh driver (the re-runnable
/// converge's mtime pass rebuilding the stale row), not by any
/// cross-process signaling.
#[tokio::test]
async fn should_refresh_session_list_when_another_server_renames_a_session_on_shared_home() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    // A: no injected home — its rename writes only `state.json` (the
    // foreign entry's footprint on the shared root).
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    // B: the home-injected listing engine — the shared index connection
    // installed on its registry (the composition-root wiring) plus a fast
    // index refresh tick.
    let mut server_b =
        consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2))
            .with_index_refresh_tick(Duration::from_millis(50));
    server_b.home_dir = Some(home.path().to_path_buf());
    server_b
        .registry
        .install_session_index(home.path().to_path_buf());

    // A creates and materializes the session (SDR-004: the state.json the
    // converge reads exists only after materialization).
    let created = server_a
        .call_method("session.create", json!({ "title": "index test" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session (state.json on the shared root)");

    // B's first listing converges the row from disk (the once-guarded
    // startup reconcile) — the OLD title is what B knows.
    let list_before = server_b
        .call_method("session.list", json!({}))
        .await
        .expect("B lists before the rename");
    assert!(
        list_before["items"]
            .as_array()
            .expect("the list answers items")
            .iter()
            .any(|item| item["id"] == session_id && item["title"] == "index test"),
        "B's startup converge must know A's session under its original title"
    );

    // A renames the session — its state.json rewrite moves the file's
    // mtime; A has no index, so no row write-through happens.
    server_a
        .call_method(
            "session.rename",
            json!({ "sessionId": &session_id, "title": "renamed from A" }),
        )
        .await
        .expect("A renames the shared session");

    // Then: B's list answers the new title within the freshness budget
    // (5s — comfortably above the 50ms test tick, far below the 10s
    // production ceiling).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let list = server_b
            .call_method("session.list", json!({}))
            .await
            .expect("B lists");
        let renamed = list["items"]
            .as_array()
            .expect("the list answers items")
            .iter()
            .any(|item| item["id"] == session_id && item["title"] == "renamed from A");
        if renamed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the index refresh driver must surface A's rename in B's listing \
             within the C1.1 freshness budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// A 5s follow-latency budget matches the other tests in this module (the
// US C1.2 ceiling), while staying far above the 50ms test ticks.
const FOLLOW_BUDGET: Duration = Duration::from_secs(5);

/// The number of `context.append_message` ops on the shared session's
/// main wire log whose user message text carries one of the marker
/// prefixes — the on-disk counterpart of [`marker_message_count`]'s
/// memory-side unit (own sends and absorbed foreign sends both land on
/// disk, so the five user markers must be durable). The journal lives
/// under `<home>/sessions/<workspace-key>/<session>/`.
fn journal_marker_message_count(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> usize {
    let bucket = mirri_core::session::workdir_key::encode_work_dir_key(workspace_root);
    let path = home
        .join("sessions")
        .join(bucket)
        .join(session_id)
        .join("wire.jsonl");
    std::fs::read_to_string(&path)
        .expect("the shared session's wire log exists")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<mirri_proto::WireOp>(line).ok())
        .filter(|op| {
            let mirri_proto::WireOp::ContextAppendMessage { message } = op else {
                return false;
            };
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                return false;
            }
            let text = wire_message_text(message);
            text.contains("marker from A") || text.contains("marker from B")
        })
        .count()
}

/// The joined text of a wire `message.content`: the send path writes an
/// array of `{"type":"text","text":...}` parts, so the text parts are
/// joined — the same joined text [`marker_message_count`] reads in
/// memory.
fn wire_message_text(message: &serde_json::Value) -> String {
    match message.get("content") {
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// How many of the given server's main-agent memory messages carry the
/// marker text — own sends and absorbed foreign messages both count (the
/// completeness assertion's unit).
async fn marker_message_count(server: &EmbeddedServer, session_id: &str, marker: &str) -> usize {
    let session = server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("the session is live on this server");
    let agent = session
        .get_agent(&AgentId::new(DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("the main agent is live");
    let memory = agent.context_memory();
    let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
    memory
        .messages()
        .iter()
        .filter(|m| m.text_content().contains(marker))
        .count()
}

/// Given two servers sharing one home, when both sides alternately send
/// real turns onto the same session (A sends three, B follows; B sends
/// two, A follows), then both servers' main-agent memory holds exactly
/// the five marker messages (own sends folded at send time, foreign sends
/// absorbed by the follow driver), the journal on disk holds every
/// recorded op, and an explicit drain after convergence reports nothing
/// new on either side (the anchors make the absorb idempotent).
#[tokio::test]
async fn should_keep_history_complete_when_both_sides_alternate_writes() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    // Both sides run real turns against the same scripted LLM (every
    // response ends its turn immediately), so every write goes through the
    // real send path: own ops are journaled AND folded into the sender's
    // memory — the same shape a foreign absorb converges to.
    let mock = Arc::new(ScriptedLlm::new());
    for _ in 0..5 {
        mock.push_text("acknowledged");
    }
    let factory: Arc<dyn mirri_core::llm::LlmFactory> =
        Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)));
    let server_a = consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
        Some(Arc::clone(&factory)),
    );
    let server_b = consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
        Some(factory),
    );

    // A creates the session (its first send materializes it — SDR-004);
    // B resumes it, which spawns B's follow driver.
    let created = server_a
        .call_method("session.create", json!({ "title": "alternate writes" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    // Materialize explicitly: B's resume needs the session discoverable on
    // disk (state.json), which SDR-004 defers until the first send — the
    // sends below are the live turn flow, not the materialization trigger.
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // A sends three marker messages (real wait-mode turns).
    for index in 0..3 {
        server_a
            .call_method(
                "message.send",
                json!({
                    "session_id": &session_id,
                    "text": format!("marker from A #{index}"),
                    "mode": "wait",
                }),
            )
            .await
            .expect("A's turn completes");
    }

    // B converges before sending: an explicit drain is pulled until it
    // reports nothing twice in a row (the drain-to-quiescence predicate).
    // A memory-level "3 markers" check would NOT suffice: the follow
    // driver may have folded the third marker MID-TURN while the turn's
    // trailing ops (loop events, turn.ended) were still being written —
    // the memory would hold all three markers while B's anchor still sat
    // below the journal's end, and the send-path commit fence (batch 2)
    // would correctly reject the send as Divergent. Quiescence — the
    // anchor caught up with the disk and a re-drain folds nothing — is
    // the true "Fresh" precondition the fence checks.
    {
        let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
        loop {
            let session = server_b
                .session_manager()
                .get_session(&SessionId::new(&session_id))
                .await
                .expect("the session stays live");
            let report = server_b
                .session_manager()
                .absorb_session_foreign_updates(&session)
                .await
                .expect("the convergence drain succeeds");
            if report.ops_absorbed == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "B must converge (its drains report nothing) within the latency budget"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    // The convergence is real for the assertion below too: B's memory
    // holds all three of A's markers once the drains went quiet.
    assert_eq!(
        marker_message_count(&server_b, &session_id, "marker from A").await,
        3,
        "B must follow A's three marker messages before sending its own"
    );

    // B sends two marker messages of its own — B's writer appends past the
    // bytes it already absorbed, so the anchors stay consistent.
    for index in 0..2 {
        server_b
            .call_method(
                "message.send",
                json!({
                    "session_id": &session_id,
                    "text": format!("marker from B #{index}"),
                    "mode": "wait",
                }),
            )
            .await
            .expect("B's turn completes");
    }

    // A follows to the two messages: A's memory holds all five markers.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    loop {
        if marker_message_count(&server_a, &session_id, "marker from B").await >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A must follow B's two messages within the follow latency budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Convergence wait: a marker arriving mid-chunk means the absorbing
    // driver may still owe the SAME turn's trailing ops (loop events, the
    // turn end) — the drain below is the convergence probe, so wait until
    // BOTH sides' explicit drains report nothing (the anchors then sit at
    // the journal's end, which is what the idempotence assertion needs).
    {
        let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
        loop {
            let mut pending = 0usize;
            for server in [&server_a, &server_b] {
                let session = server
                    .session_manager()
                    .get_session(&SessionId::new(&session_id))
                    .await
                    .expect("the session stays live");
                let report = server
                    .session_manager()
                    .absorb_session_foreign_updates(&session)
                    .await
                    .expect("the convergence probe drain succeeds");
                pending += report.ops_absorbed;
            }
            if pending == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "both sides must converge (their drains report nothing) within the latency budget"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // Then: both sides' memory is complete — each side holds every marker
    // message of BOTH sides (its own sends plus the absorbed foreign ones)
    // with no loss and no duplication.
    for (server, label) in [(&server_a, "A"), (&server_b, "B")] {
        assert_eq!(
            marker_message_count(server, &session_id, "marker from A").await,
            3,
            "side {label} must hold all three of A's marker messages"
        );
        assert_eq!(
            marker_message_count(server, &session_id, "marker from B").await,
            2,
            "side {label} must hold both of B's marker messages"
        );
    }
    // And: the disk journal holds the envelope plus the exact five marker
    // user messages (three from A, two from B) — the same unit the
    // memory-side completeness assertions above use, so both views of the
    // conversation agree on disk and in memory.
    assert_eq!(
        journal_marker_message_count(home.path(), workspace_root.path(), &session_id),
        5,
        "the journal must durably hold all five marker user messages (3 from A, 2 from B)"
    );

    // And: an explicit drain on each side (bypassing the tick cadence)
    // reports nothing — each anchor already sits at the bytes it folded,
    // so re-absorbing is a no-op (the drain is idempotent).
    for (server, label) in [(&server_a, "A"), (&server_b, "B")] {
        let session = server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("the session stays live");
        let report = server
            .session_manager()
            .absorb_session_foreign_updates(&session)
            .await
            .expect("the explicit drain succeeds");
        assert_eq!(
            report.ops_absorbed, 0,
            "side {label} must absorb nothing after convergence (idempotent drain)"
        );
        assert_eq!(
            report.bytes_consumed, 0,
            "side {label} must consume no bytes after convergence"
        );
    }
}

/// Given a resumed session whose follow driver ticks, when the local side
/// has a turn in flight (a gated scripted-LLM chat call parks the turn
/// mid-stream) while the foreign side appends a message, then the driver's
/// ticks skip the absorb — the foreign message does NOT reach the local
/// memory while the turn runs (invariant 3, ADR-005: no foreign fold
/// interleaves with a local turn's writes). The interleaved foreign bytes
/// (appended between the turn's own ops) leave the anchor short of the
/// true offsets — a known batch-1 clip the write-side fence (batch 2)
/// closes — so the post-turn recovery goes through the designed full
/// resume path: close → resume replays the whole journal, and the foreign
/// message is complete in memory again.
#[tokio::test]
async fn should_not_absorb_while_own_turn_is_in_flight() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    // The local side B runs real turns against a scripted LLM whose
    // first reply is gated: the wait-mode `message.send` parks inside the
    // LLM stream until the test releases it, holding B's turn in flight
    // for however long the test needs (the real in-flight posture the
    // driver guards against — a registered wait-mode turn).
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("B's own reply");
    // The server goes behind an Arc: the spawned message.send future must
    // be 'static, and EmbeddedServer is shared (not cloned) — the same
    // posture the EngineBridge consumers hold.
    let server_b = Arc::new(consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
    ));
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));

    // A creates and materializes; B resumes (spawning B's follow driver).
    let created = server_a
        .call_method("session.create", json!({ "title": "in-flight guard" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // B starts a wait-mode turn — the call parks in the gated LLM stream,
    // so the future only resolves after the release below. B's turn is
    // in flight (registered in the engine-wide in-flight registry the
    // follow driver consults).
    let send = tokio::spawn({
        let server = Arc::clone(&server_b);
        let session_id = session_id.clone();
        async move {
            let _ = server
                .call_method(
                    "message.send",
                    json!({ "session_id": session_id, "text": "B's prompt", "mode": "wait" }),
                )
                .await;
        }
    });
    // Wait until the turn is actually registered in flight (the send's
    // dispatch does async work before the registry insert — polling the
    // registry makes the ordering deterministic without racing it).
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    while !server_b.session_is_busy(&session_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "B's turn must register in flight"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A writes one foreign message while B's turn is parked mid-stream.
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("A's created session stays live");
    session_a
        .record_ops(&[mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "foreign while B is mid-turn",
            }),
        }])
        .await
        .expect("A records the foreign message");

    // Then: well past three follow ticks, B's memory does NOT hold the
    // foreign message — the driver skipped every absorb while the turn
    // was in flight (invariant 3).
    //
    // The busy check is a polling deadline, not a fixed sleep: under
    // parallel-suite load the gated LLM stream can starve longer than
    // the fixed 500ms, which used to flake this assertion (the turn
    // legitimately completed before the check ran). Polling keeps the
    // assertion's real contract — "still in flight when we assert the
    // skip" — deterministic under load.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    while !server_b.session_is_busy(&session_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "B's turn must still be in flight for the skip window to be meaningful"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        marker_message_count(&server_b, &session_id, "foreign while B is mid-turn").await,
        0,
        "the driver must not absorb the foreign message while B's own turn is in flight"
    );

    // When the turn settles (the gate opens, the scripted reply ends the
    // turn), the driver resumes ticking. The interleaved foreign bytes
    // (appended between the turn's own ops) sit below the anchor's floor
    // semantics — the drain stops at the torn line and absorbs nothing
    // (a known batch-1 clip; the write-side fence in batch 2 restores
    // anchor = offset). The driver stays healthy: no error, no exit.
    release.send(()).expect("the gated reply is released");
    let _ = send.await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        marker_message_count(&server_b, &session_id, "foreign while B is mid-turn").await,
        0,
        "the interleaved foreign message must not be folded by the incremental \
         absorb (batch-1 anchor-floor clip; recovered by the full resume below)"
    );
    // The driver did NOT exit over the interleaved journal: the session
    // stays live and its follow driver is still tracked (it keeps ticking
    // — the recovery is the resume, not a crash).
    assert!(
        server_b
            .follow_drivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&session_id),
        "the follow driver must stay alive over the interleaved journal (no exit path fired)"
    );

    // And: the designed recovery — close → resume replays the whole
    // journal (the interleaved message included), so B's memory is
    // complete again (spec §5: full resume is the truncation-class
    // recovery).
    server_b
        .call_method("session.close", json!({ "sessionId": &session_id }))
        .await
        .expect("B closes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the session (full replay)");
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    loop {
        if marker_message_count(&server_b, &session_id, "foreign while B is mid-turn").await > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the full resume must replay the interleaved foreign message into B's memory"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a resumed session with a running follow driver, when the session
/// closes, then the driver task finishes within a few ticks — asserted
/// directly on its tracked JoinHandle (a dead-looping driver would keep
/// the handle unfinished and fail the test).
#[tokio::test]
async fn should_exit_when_session_is_closed() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    let server = consistency_server(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
    );

    // A created session cannot resume (AlreadyExists — it is live), so the
    // flow is create → materialize → close → resume: the resume is what
    // spawns the follow driver.
    let created = server
        .call_method("session.create", json!({ "title": "close test" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("materialize");
    server
        .call_method("session.close", json!({ "sessionId": &session_id }))
        .await
        .expect("close the live session");
    server
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume spawns the follow driver");

    // The driver's handle is tracked in the follow-drivers map.
    let driver = server
        .follow_drivers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id)
        .expect("the resumed session's follow driver is tracked");
    assert!(
        !driver.is_finished(),
        "the driver runs while the session is live"
    );

    // When the session closes, the driver's next tick's resolution fails
    // and the task exits — within a few ticks (tick = 50ms).
    server
        .call_method("session.close", json!({ "sessionId": &session_id }))
        .await
        .expect("close");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !driver.is_finished() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        driver.is_finished(),
        "the follow driver must exit within a few ticks of the session closing"
    );

    // The closed session is no longer live (the exit condition the driver
    // observed).
    assert!(
        server
            .session_manager()
            .get_session(&SessionId::new(&session_id))
            .await
            .is_none(),
        "the closed session is no longer live"
    );
}

/// The dual-instance subagent follow helper: server A (the desktop, a real
/// turn pipeline) dispatches ONE real `Agent`-tool subagent whose child turn
/// records a marker message into its own child journal; server B (the web,
/// the following instance) must observe the child within the follow budget.
///
/// A's scripted LLM: the parent's first reply calls the Agent tool, the
/// child's reply ends its turn, the parent's second reply ends the parent
/// turn. The child agent is spawned through the REAL subagent host the
/// embedded create chain wires (`SessionSubagentHost`), so the
/// `subagent.registered` op, the child journal, and the lifecycle are all
/// the production shapes — the foreign writes B's follow driver absorbs.
async fn dispatch_real_subagent_on(
    server_a: &EmbeddedServer,
    session_id: &str,
    mock: &Arc<ScriptedLlm>,
) -> String {
    mock.push_tool_use(vec![LlmToolCall {
        id: "call-agent-child-1".to_string(),
        name: "Agent".to_string(),
        args: json!({
            "prompt": "child: run your delegated task",
            "description": "delegate the child task",
        }),
    }]);
    mock.push_text("the child agent replied");
    mock.push_text("parent turn done");

    // The wait-mode send runs the whole delegation inline: the parent's
    // turn ends only after the child's turn completed.
    server_a
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "please delegate to a child agent",
                "mode": "wait",
            }),
        )
        .await
        .expect("A's parent turn (with the delegated child) completes");

    // The child agent id the spawn minted — the registered subagent whose
    // journal B must follow. Polled from A's live registry (the spawn's
    // own fold), so the test never guesses the id.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    loop {
        let session_a = server_a
            .session_manager()
            .get_session(&SessionId::new(session_id))
            .await
            .expect("A's session stays live");
        let registrations = session_a.list_subagent_registrations().await;
        if let Some(first) = registrations.first() {
            return first.agent_id.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A's parent turn must register its child agent within the budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given two servers sharing one home (A with a real turn pipeline and a
/// profile-backed subagent host, B the following web instance), when A's
/// desktop turn delegates to a child agent (the real `Agent`-tool path —
/// the child's turn ops land on its OWN child journal), then B's follow
/// driver absorbs the registration op AND the child journal within the
/// 5s follow budget: B's event stream carries the child's message event
/// (attributed to the child's agent id) and B's live `agents` map holds
/// the child — the goal's "web shows desktop subagent activity" anchor.
#[tokio::test(flavor = "multi_thread")]
async fn should_show_subagent_activity_in_web_within_five_seconds_when_desktop_runs_child_task() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");

    // A: the desktop — a real LLM factory (scripted) PLUS a profile
    // manager, the two ingredients the create chain needs to wire the
    // production subagent host (the `Agent` tool) into the main agent.
    let mock = Arc::new(ScriptedLlm::new());
    let profile_manager = {
        let manager = mirri_core::ProfileManager::new();
        manager
            .register(mirri_core::AgentProfile {
                name: "coder".to_string(),
                model: TEST_DEFAULT_MODEL.to_string(),
                permission_mode: Some(mirri_core::PermissionMode::Yolo),
                ..Default::default()
            })
            .expect("the coder profile registers");
        manager
    };
    let server_a = {
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("dual-instance-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            home.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        ));
        let mut parsed = ServerConfig::from_toml(&format!(
            "\n[experimental]\n\"{}\" = true\n",
            mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG
        ))
        .expect("A's test config parses");
        parsed.default_model = Some(TEST_DEFAULT_MODEL.to_string());
        EmbeddedServer::with_providers(
            manager,
            Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
            Some(Arc::new(profile_manager)),
            None,
            None,
            Arc::new(RwLock::new(parsed)),
            None,
        )
        .with_follow_tick(Duration::from_millis(50))
    };
    // B: the web — no LLM needed, only the follow driver (50ms tick).
    let server_b = consistency_server(
        home.path(),
        workspace_root.path(),
        Duration::from_millis(50),
    );

    // A creates and materializes the shared session; B subscribes BEFORE
    // resuming (the relay is installed when the absorbed events land), then
    // resumes — B's follow driver starts ticking.
    let created = server_a
        .call_method("session.create", json!({ "title": "desktop child task" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    let mut events = server_b.subscribe_events().await.expect("B subscribes");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // When: A's parent turn delegates to a real child agent (the Agent
    // tool through the production subagent host — the child's turn ops
    // land on its own child journal, and the registration op on the main
    // journal, both under the shared home).
    let child_id = dispatch_real_subagent_on(&server_a, &session_id, &mock).await;

    // Then: B's event stream carries the child's message event within the
    // 5s follow budget — a MessageReceived attributed to the CHILD's agent
    // id, the event dimension the web's per-agent views key on. (The child
    // journal's turn.prompt records the delegated prompt as the child's
    // own user message, so the absorbed child content is observable.)
    //
    // And first — the review F1 anchor: the web builds its subagent card
    // from the raw `subagent.spawned`/`subagent.started` frames (the
    // agentEventProjector's cases), which have NO WireOp carrier and so
    // must be SYNTHESIZED by the absorb path (the registration op is
    // their only reconstructible source on a following instance). B's
    // stream must carry both, attributed to the child's id, before the
    // message content below.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    let mut saw_spawned = false;
    let mut saw_started = false;
    let mut saw_child_message = false;
    while tokio::time::Instant::now() < deadline
        && !(saw_spawned && saw_started && saw_child_message)
    {
        if let Ok(Some(notification)) =
            tokio::time::timeout(Duration::from_millis(200), events.recv()).await
            && notification.params["sessionId"] == session_id
        {
            if notification.method == "event.subagent.spawned"
                && notification.params["subagentId"].as_str() == Some(child_id.as_str())
            {
                saw_spawned = true;
            }
            if notification.method == "event.subagent.started"
                && notification.params["subagentId"].as_str() == Some(child_id.as_str())
            {
                saw_started = true;
            }
            if notification.method == "event.message.received"
                && notification.params["agentId"].as_str() == Some(child_id.as_str())
            {
                saw_child_message = true;
            }
        }
    }
    assert!(
        saw_spawned,
        "B must receive the synthesized subagent.spawned frame within the follow budget \
         (the web's child-card creation path)"
    );
    assert!(
        saw_started,
        "B must receive the synthesized subagent.started frame within the follow budget \
         (the web's child-card working-phase path)"
    );
    assert!(
        saw_child_message,
        "B must receive the child agent's message event within the follow budget"
    );

    // And: B's live agents map holds the child — the lazily materialized
    // scope the registration absorption built, observable through the
    // `session.get` agent list the web's agents view consumes.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    loop {
        let got = server_b
            .call_method("session.get", json!({ "sessionId": &session_id }))
            .await
            .expect("B reads the session view");
        let agents = got["agents"].as_array().cloned().unwrap_or_default();
        if agents
            .iter()
            .any(|agent| agent["id"].as_str() == Some(child_id.as_str()))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B's agents map must hold the absorbed child agent within the budget"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // And: the child's folded content is in B's memory — the child scope
    // the absorb built holds the delegated task's prompt (the marker the
    // assertions above keyed on, now verified in-memory, not only on the
    // event wire).
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("B's session stays live");
    let child_b = session_b
        .get_agent(&AgentId::new(&child_id))
        .await
        .expect("B materialized the child agent scope");
    let history_len = child_b
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .len();
    assert!(
        history_len > 0,
        "the child's folded history must be non-empty on B (got {history_len})"
    );
}

// ---------------------------------------------------------------------------
// Batch 2, Task 3 — the write-side commit fence (ADR-002)
// ---------------------------------------------------------------------------

/// The number of non-empty lines in the shared session's main wire log —
/// the disk-zero-write assertion's unit for the fence tests (a rejected
/// send must leave the line count exactly where it was).
fn wire_journal_line_count(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> usize {
    let bucket = mirri_core::session::workdir_key::encode_work_dir_key(workspace_root);
    let path = home
        .join("sessions")
        .join(bucket)
        .join(session_id)
        .join("wire.jsonl");
    std::fs::read_to_string(&path)
        .expect("the shared session's wire log exists")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

/// A `message.send` that carries the `session.conflict` domain code — the
/// structured, web-discriminable shape the fence must answer with (the
/// `EngineError::Rpc` the embedded dispatch surfaces, carrying
/// `data.domainCode == "session.conflict"`).
fn is_session_conflict(error: &RpcEngineError) -> bool {
    let RpcEngineError::Rpc {
        data: Some(data), ..
    } = error
    else {
        return false;
    };
    data.get("domainCode").and_then(|code| code.as_str()) == Some("session.conflict")
}

/// Given two servers sharing one home (both behind the consistency flag),
/// when a foreign write lands on the session's main journal between B's
/// absorb and B's send, then B's `message.send` is rejected with the
/// `session.conflict` domain code BEFORE the user message reaches the disk
/// — the main journal's line count is unchanged (ADR-002: the fence
/// intercepts before the first `record_op`, zero bytes written).
#[tokio::test]
async fn should_reject_send_with_session_conflict_when_foreign_writes_arrived_before_submit() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    // B needs a scripted LLM so a passing send would run a real turn —
    // the fence sits on the real send path, and a wrongly-allowed send
    // must produce turn ops, not a no-provider error.
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("B's reply");
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    let server_b = consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_secs(2),
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
    );

    // A creates and materializes the shared session; B resumes it.
    let created = server_a
        .call_method("session.create", json!({ "title": "fence conflict" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // B's follow driver must be OUT of the picture for this test: its
    // first `tokio::time::interval` tick completes immediately, so it
    // races the test body for the very absorb this test needs to see
    // UN-absorbed (spec §2.2: the submit-path check happens BEFORE any
    // absorb). Aborting the tracked handle pins the driver's
    // non-participation — the divergence stays unabsorbed no matter how
    // the test's awaits interleave with the scheduler.
    server_b
        .follow_drivers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id)
        .expect("B's follow driver is tracked after the resume")
        .abort();

    // A writes one foreign message op on the shared main journal — the
    // divergence B's fence must catch (B's anchor sits below the file's
    // end; the driver above was aborted, so the probe sees Divergent).
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("A's created session is live");
    session_a
        .record_ops(&[mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "foreign write before B submits",
            }),
        }])
        .await
        .expect("A records the foreign message");

    let lines_before = wire_journal_line_count(home.path(), workspace_root.path(), &session_id);

    // When: B sends a message while the journal diverged from B's anchor
    let error = server_b
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "B's prompt that must be fenced",
                "mode": "wait",
            }),
        )
        .await
        .expect_err("the diverged send must be rejected with session.conflict");

    // Then: the error carries the `session.conflict` domain code (the
    // structured shape the web branches on), and the main journal is
    // byte-for-byte untouched — zero new lines (ADR-002: the fence runs
    // before the first record_op).
    assert!(
        is_session_conflict(&error),
        "the rejection must carry the session.conflict domain code, got: {error}"
    );
    assert_eq!(
        wire_journal_line_count(home.path(), workspace_root.path(), &session_id),
        lines_before,
        "the rejected send must leave zero new lines on the main journal"
    );
}

/// Given a resumed session whose journal is Fresh (B's anchor sits at the
/// file's end — no foreign writes, or the follow driver already absorbed
/// them), when B sends a message, then the send proceeds normally and the
/// turn's ops land on the journal.
#[tokio::test]
async fn should_allow_send_when_journal_is_fresh() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("B's fresh reply");
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    let server_b = consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_secs(2),
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
    );

    let created = server_a
        .call_method("session.create", json!({ "title": "fence fresh" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    let lines_before = wire_journal_line_count(home.path(), workspace_root.path(), &session_id);

    // When: B sends on a fresh journal (no foreign writes at all)
    let result = server_b
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "B's prompt on a fresh journal",
                "mode": "wait",
            }),
        )
        .await;

    // Then: the send succeeds (the fence lets a Fresh journal through) and
    // the turn's ops landed on the journal.
    assert!(
        result.is_ok(),
        "a fresh-journal send must succeed, got: {:?}",
        result.err()
    );
    assert!(
        wire_journal_line_count(home.path(), workspace_root.path(), &session_id) > lines_before,
        "the allowed send must write its turn ops to the journal"
    );
}

/// Given the consistency flag DISABLED on B (the default posture — the
/// fence is part of the same feature face as the follow driver), when a
/// foreign write diverged the journal and B sends, then the send proceeds
/// normally — the write-side fence is inert without the flag (legacy
/// single-instance behavior is unchanged).
#[tokio::test]
async fn should_allow_send_when_flag_disabled() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("B's unfenced reply");
    // B: the same dual-instance shape but WITHOUT the experimental flag —
    // the config layer leaves the consistency feature off (the default).
    let server_a = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    let server_b = {
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("dual-instance-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            home.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        ));
        let parsed = ServerConfig {
            default_model: Some(super::TEST_DEFAULT_MODEL.to_string()),
            ..ServerConfig::default()
        };
        EmbeddedServer::with_providers(
            manager,
            Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
            None,
            None,
            None,
            Arc::new(RwLock::new(parsed)),
            None,
        )
        .with_follow_tick(Duration::from_secs(2))
    };

    let created = server_a
        .call_method("session.create", json!({ "title": "fence disabled" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // The same foreign write shape as the conflict test — a divergence the
    // fence WOULD catch when enabled.
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("A's created session is live");
    session_a
        .record_ops(&[mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "foreign write with the flag off",
            }),
        }])
        .await
        .expect("A records the foreign message");

    // When: B sends with the flag disabled
    let result = server_b
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "B's prompt with the fence inert",
                "mode": "wait",
            }),
        )
        .await;

    // Then: the send succeeds — the fence does not intercept without the
    // flag (the legacy single-instance write posture is unchanged).
    assert!(
        result.is_ok(),
        "a diverged send must pass through when the consistency flag is off, got: {:?}",
        result.err()
    );
}

/// Given a resumed session whose turn is running with tool calls (behind
/// the consistency flag), when a foreign write lands on the main journal
/// WHILE the turn is mid-flight, then the turn ABORTS before its next
/// tool batch (invariant 2, spec §2.2 ①+③): the in-flight turn settles
/// with the existing cancel vocabulary (`turn.ended` reason
/// `cancelled`), the turn's already-recorded ops stay on the journal
/// (已完成部分保留 — append-only closure, 方式 B), and B observes an
/// observable abort signal (the wait-mode send resolves with the
/// aborted-turn result instead of a clean turn).
#[tokio::test(flavor = "multi_thread")]
async fn should_abort_own_turn_before_next_tool_batch_when_foreign_writes_arrive_mid_turn() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace tempdir");
    // B's turn runs real steps against a scripted LLM: step 1 calls the
    // Bash tool (so tool events and the turn's opening ops are recorded
    // durably before the divergence), step 2 is a GATED tool call — the
    // turn parks inside the second LLM stream while the test writes the
    // foreign op, and when the gate opens the step settles and the turn
    // goes for ANOTHER tool batch — the exact boundary the mid-turn guard
    // must intercept (step 3's LLM call / tool dispatch never runs).
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call-bash-1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo first-batch-done" }),
    }]);
    let release = mock.push_gated_tool_use(vec![mirri_core::LlmToolCall {
        id: "call-bash-2".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo second-batch-must-not-run" }),
    }]);
    let server_a = Arc::new(consistency_server(
        home.path(),
        workspace_root.path(),
        Duration::from_secs(2),
    ));
    let server_b = Arc::new(consistency_server_with_llm(
        home.path(),
        workspace_root.path(),
        Duration::from_secs(2),
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
    ));

    // A creates and materializes the shared session; B resumes it.
    let created = server_a
        .call_method("session.create", json!({ "title": "mid-turn abort" }))
        .await
        .expect("A creates the session");
    let session_id = created["id"].as_str().unwrap().to_string();
    server_a
        .session_manager()
        .materialize_session(&SessionId::new(&session_id))
        .await
        .expect("A materializes the session");
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("B resumes the shared session");

    // B's wait-mode turn: step 1's tool call settles (the first tool
    // batch completes durably), then the turn parks inside the gated
    // second LLM call. The send future only resolves when the turn ends.
    let send = tokio::spawn({
        let server = Arc::clone(&server_b);
        let session_id = session_id.clone();
        async move {
            server
                .call_method(
                    "message.send",
                    json!({
                        "session_id": session_id,
                        "text": "B's tool-using prompt",
                        "mode": "wait",
                    }),
                )
                .await
        }
    });
    // The turn must actually reach its gated second LLM call before the
    // foreign write lands — the first tool batch is complete at that
    // point, so the journal holds the turn's partial work (the "already
    // recorded" prefix) and the only remaining boundary is the next
    // tool batch.
    let deadline = tokio::time::Instant::now() + FOLLOW_BUDGET;
    while mock.chat_call_count() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "B's turn must reach its gated second LLM call"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A writes one foreign message op on the shared main journal while
    // B's turn is parked mid-step — the divergence B's mid-turn guard
    // must see before the next tool batch (the follow driver skips
    // absorbs while B's own turn is in flight, so the anchor stays put).
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("A's created session stays live");
    session_a
        .record_ops(&[mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "foreign write while B is mid-turn",
            }),
        }])
        .await
        .expect("A records the foreign message");

    // The lines B's turn has durably recorded at the moment of the
    // divergence — the partial work the abort must PRESERVE (append-only
    // closure, 方式 B): the turn prompt, the user message, the first
    // batch's tool events.
    let lines_at_divergence =
        wire_journal_line_count(home.path(), workspace_root.path(), &session_id);

    // When: the gate opens — the second tool call settles, and the turn
    // goes for the next tool batch (the third LLM step).
    release
        .send(())
        .expect("the gated tool-use reply is released");
    let send_result = send.await.expect("the send task must not panic").expect(
        "the wait-mode send must answer (an aborted turn is a settled result, not an error)",
    );

    // Then: the turn was aborted — the send result carries the aborted
    // stop reason (B's observable abort signal for the wait-mode caller).
    assert_eq!(
        send_result["turn"]["stopReason"].as_str(),
        Some("aborted"),
        "B's turn must settle as aborted after the mid-turn divergence, got: {send_result}"
    );

    // The turn's already-recorded partial work is preserved on the
    // journal: the aborted turn's closure (方式 B) may only APPEND — the
    // pre-divergence lines must all still be there, byte-for-byte in
    // count, and no tool result for the second (guarded) batch ran.
    let lines_after = wire_journal_line_count(home.path(), workspace_root.path(), &session_id);
    assert!(
        lines_after >= lines_at_divergence,
        "the abort must preserve the turn's already-recorded partial work \
         (append-only closure), before={lines_at_divergence} after={lines_after}"
    );
    // The guarded tool batch never executed: its command string appears
    // nowhere in the journal (only tool RESULTS carry executed output —
    // an unexecuted batch leaves no trace of having run).
    let journal_text = wire_journal_text(home.path(), workspace_root.path(), &session_id);
    assert!(
        !journal_text.contains("second-batch-must-not-run"),
        "the turn must abort BEFORE the next tool batch executes — the \
         guarded batch's tool call must not run"
    );
    // And the turn never made the third LLM call (the abort landed at the
    // step boundary, not after another full step).
    assert!(
        mock.chat_call_count() < 3,
        "the aborted turn must not run further LLM steps"
    );

    // The journal's turn bracket closes with the existing cancel
    // vocabulary: the `turn.ended` record for B's turn carries reason
    // "cancelled" (the wire spelling of TurnStopReason::Aborted — no new
    // reason word is invented).
    assert!(
        journal_turn_ended_reasons(home.path(), workspace_root.path(), &session_id)
            .iter()
            .any(|reason| reason == "cancelled"),
        "the journal's turn bracket must close with reason=cancelled"
    );

    // The follow driver stays healthy across the aborted turn (no exit
    // path fired) — the recovery is a later resume/absorb, not a crash.
    assert!(
        server_b
            .follow_drivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&session_id),
        "the follow driver must stay alive across the aborted turn"
    );
}

/// The journal file's full text — the abort test's inspection unit.
fn wire_journal_text(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> String {
    let bucket = mirri_core::session::workdir_key::encode_work_dir_key(workspace_root);
    let path = home
        .join("sessions")
        .join(bucket)
        .join(session_id)
        .join("wire.jsonl");
    std::fs::read_to_string(&path).expect("the shared session's wire log exists")
}

/// The reasons of every `turn.ended` record on the session's journal —
/// the wire-vocabulary assertion unit for the abort test.
fn journal_turn_ended_reasons(
    home: &std::path::Path,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> Vec<String> {
    wire_journal_text(home, workspace_root, session_id)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|op| op.get("type").and_then(|t| t.as_str()) == Some("turn.ended"))
        .filter_map(|op| {
            op.get("reason")
                .and_then(|r| r.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Given TWO home-injected engines sharing one home with sessions on disk
/// (the dual-instance shape, both behind the consistency flag), when both
/// engines run an index reconcile AT THE SAME TIME (each spawned onto its
/// own task, started together — the exact interleaving the D8 lock exists
/// for), then every session's row survives in the shared
/// `session-index.db` (no row is lost to interleaved passes — ADR-004's
/// cross-process serialization), the lock file is released afterwards
/// (no residue), and a third pass still converges cleanly (the idempotent
/// settle).
#[tokio::test(flavor = "multi_thread")]
async fn should_serialize_concurrent_reconciles_without_losing_rows() {
    let home = tempfile::tempdir().expect("home tempdir");
    let workspace_root = tempfile::tempdir().expect("workspace root tempdir");

    // Two home-injected listing engines on the shared home — both install
    // the shared session-index connection on their registries (the
    // composition-root wiring).
    let make_server = || {
        let mut server =
            consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2))
                .with_index_refresh_tick(Duration::from_secs(3600)); // the driver must NOT tick during the test — the explicit reconciles below are the only passes
        server.home_dir = Some(home.path().to_path_buf());
        server
            .registry
            .install_session_index(home.path().to_path_buf());
        server
    };
    let server_a = Arc::new(make_server());
    let server_b = Arc::new(make_server());

    // A third server (no injected home) creates and materializes several
    // sessions on the shared root — the foreign entry whose rows BOTH
    // reconciles must converge (each pass sees all of them on disk).
    let writer = consistency_server(home.path(), workspace_root.path(), Duration::from_secs(2));
    let mut session_ids = Vec::new();
    for n in 0..5 {
        let created = writer
            .call_method("session.create", json!({ "title": format!("row {n}") }))
            .await
            .expect("create");
        let session_id = created["id"].as_str().unwrap().to_string();
        writer
            .session_manager()
            .materialize_session(&SessionId::new(&session_id))
            .await
            .expect("the session materializes (state.json on the shared root)");
        session_ids.push(session_id);
    }

    // When both engines reconcile at the same time — two spawned tasks,
    // started together, each holding its own server. Without the D8 lock
    // the two read-disk-compare-upsert passes interleave freely; with it,
    // they serialize (one full pass, then the other).
    let (a, b) = tokio::join!(
        tokio::spawn({
            let server = Arc::clone(&server_a);
            async move { server.reconcile_session_index().await }
        }),
        tokio::spawn({
            let server = Arc::clone(&server_b);
            async move { server.reconcile_session_index().await }
        }),
    );
    a.expect("A's reconcile task joins");
    b.expect("B's reconcile task joins");

    // Then every session's row is present in the shared index (no row
    // lost to the interleaving) — read through either engine's connection.
    let index = server_a
        .session_index()
        .expect("A's shared index connection is installed");
    for session_id in &session_ids {
        assert!(
            index.get_session(session_id).is_some(),
            "the row of session {session_id} must survive the concurrent reconciles"
        );
    }

    // And the reconcile lock is released (the lock file is the lock — no
    // residue after both passes settle).
    let lock_path =
        mirri_session_index::lock_file_path(&home.path().join("session-index.reconcile"));
    assert!(
        !lock_path.exists(),
        "the reconcile lock must be released after both passes settle"
    );

    // And a third pass over the settled state converges cleanly (the
    // idempotent settle — a serialized rerun is a no-op, not an error).
    server_b.reconcile_session_index().await;
    for session_id in &session_ids {
        assert!(
            index.get_session(session_id).is_some(),
            "the rows must survive the third settle pass"
        );
    }
}
