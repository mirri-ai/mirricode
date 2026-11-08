//! B2: the notification delivery loop — the task driver's terminal
//! notification engine (D-51) and its busy/idle/wakeup paths.
//!
//! Every B2 embedded test lands here (the 800+-test `tests.rs` keeps a
//! single owner). The delivery engine: on each task-sweep tick, terminal
//! detached tasks whose notification key is neither in the context ledger
//! (delivered) nor in the steer buffer (parked) are delivered — folded
//! into the running turn when busy, run as their own turn when idle, and
//! rescued as a wakeup turn when a busy-parked notification's turn died
//! before flushing.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::json;

use super::*;

/// A B2 test bed: a server with the given LLM factory and a fast
/// task-sweep tick (the notification driver rides the same cadence as
/// the sweeper). The cron scheduler's tick is set to the SAME fast value
/// — every B2 cron test drives due fires through real ticks, so the bed
/// pre-wires it (no per-test `with_cron_tick` repetition).
fn b2_server_with_factory(
    factory: Arc<dyn LlmFactory>,
    tick: Duration,
) -> (EmbeddedServer, tempfile::TempDir) {
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
    let config = Arc::new(RwLock::new(parsed));
    let server =
        EmbeddedServer::with_providers(manager, Some(factory), None, None, None, config, None)
        .with_home_dir(Some(tmp.path().to_path_buf()))
            .with_task_sweep_tick(tick)
            .with_cron_tick(tick);
    (server, tmp)
}

/// A B2 test bed: a scripted-LLM server with a fast task-sweep tick (the
/// notification driver rides the same cadence as the sweeper).
fn b2_server(mock: Arc<ScriptedLlm>, tick: Duration) -> (EmbeddedServer, tempfile::TempDir) {
    b2_server_with_factory(Arc::new(ScriptedLlmFactory::new(mock)), tick)
}

/// Serialize every recorded LLM call's messages (the assertion substrate).
fn recorded_all(mock: &ScriptedLlm) -> serde_json::Value {
    let messages = mock
        .recorded_messages
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    serde_json::to_value(&messages).unwrap_or_default()
}

/// Whether any recorded LLM call already saw a notification XML.
fn notification_reached_model(mock: &ScriptedLlm) -> bool {
    recorded_all(mock).to_string().contains("<notification ")
}

/// Register a background task that has already completed with output —
/// the canonical due notification.
fn completed_background_task(session: &mirri_core::SessionScope, session_id: &str) -> String {
    let tasks = session.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("echo hi".into()),
                dispatcher: None,
            },
            "echo hi".into(),
            session_id.to_string(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "hi\n");
    tasks.complete(&task_id, 0);
    task_id
}

/// Given a completed detached task whose notification turn is running (the
/// driver-injected dispatch origin), when the engine-side in-flight
/// registry is read, then the session is reported in flight — the face the
/// transport read faces (`session.status` / `session.snapshot` /
/// `session.get`) now union in.
///
/// The session needs no prior user turn: the notification delivery runs
/// its own turn on the driver tick (the same setup
/// `should_cancel_the_notification_turn_when_message_abort_targets_the_session`
/// uses). A `converse_once` would dead-end here anyway — the
/// `CancelAwareBlockingLlm` blocks in the LLM call until cancelled, so a
/// wait-mode `message.send` would never resolve.
#[tokio::test]
async fn should_report_the_session_in_flight_while_a_notification_turn_runs() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({ "title": "engine busy" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    completed_background_task(&session, &session_id);

    // When the notification turn is running, the engine-side registry
    // reports the session in flight (the face the transport read faces
    // now union in).
    let mut in_flight = false;
    for _ in 0..100 {
        let sessions = mirri_transport::adapter::EngineAdapter::in_flight_sessions(&server).await;
        if sessions.iter().any(|sid| sid == &session_id) {
            in_flight = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(in_flight, "the notification turn must be engine-in-flight");
}

/// CR4 parity (task 2): a driver-injected notification turn must publish
/// the same structured agent-status frames (`event.agent.status.updated`) a
/// user-dispatched turn publishes. The flow is FIRST created by the
/// notification turn here (no prior `converse_once` — a pre-existing flow
/// would already carry the user turn's publisher), so the driver's own
/// wiring is what the assertion exercises.
#[tokio::test]
async fn should_publish_agent_status_frames_when_a_notification_turn_runs() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("notification handled");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "status frames" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let mut events = server.subscribe_events().await.expect("subscribe events");
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    completed_background_task(&session, &session_id);
    // Wait for the notification turn to actually reach the model (the
    // existing b2 helper).
    for _ in 0..100 {
        if notification_reached_model(&mock) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        notification_reached_model(&mock),
        "the notification turn must run"
    );

    // Then at least one agent-status frame was published for the main agent
    let mut saw_status = false;
    while let Ok(event) = events.try_recv() {
        if event.method == "event.agent.status.updated"
            && event.params.get("agentId").and_then(|v| v.as_str()) == Some("main")
        {
            saw_status = true;
        }
    }
    assert!(
        saw_status,
        "the notification turn must publish agent status frames"
    );
}

/// Given an idle session and a background task that completes, when the
/// notification driver ticks, then a fresh turn runs whose prompt is the
/// notification XML, and the second LLM call sees it in its input.
#[tokio::test]
async fn should_deliver_a_new_notification_turn_when_the_agent_is_idle() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("seen the notification");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);

    // Then within a few ticks a notification turn ran and fed the model
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And the notification names the task and its status, with the output.
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );
    assert!(all.contains("type=\\\"task.completed\\\""), "{all}");
    // And the turn's wire shape is the dispatched-turn form (cron parity):
    // turn.prompt + context.append_message, both carrying the
    // background_task origin.
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let prompt_index = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnPrompt { origin, .. } if origin["kind"] == "background_task"))
        .expect("the wire log must record turn.prompt with the notification origin");
    assert!(ops.iter().any(|op| matches!(
        op,
        WireOp::ContextAppendMessage { message } if message["origin"]["kind"] == "background_task"
    )));
    match &ops[prompt_index] {
        WireOp::TurnPrompt { input, .. } => {
            assert!(
                input[0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("<notification "),
                "the turn prompt is the notification XML: {input:?}"
            );
        }
        other => panic!("expected turn.prompt, got {other:?}"),
    }
}

/// Given a session with a Notification hook registered, When a background
/// task completes and the notification driver delivers it, Then the hook
/// fires exactly once with the TS `AgentTaskNotificationContext` shape plus
/// `sink: "context"` (TS parity: the `task.notified` event →
/// `notifyTaskNotification`).
#[tokio::test]
async fn should_fire_notification_hook_when_a_due_notification_delivers() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("seen the notification");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let marker =
        std::env::temp_dir().join(format!("notification_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    session.hook_engine().register(HookDef::new(
        HookEventType::Notification,
        format!("cat > {:?}", marker),
    ));

    let task_id = completed_background_task(&session, &session_id);

    // Then within a few ticks the hook fired with the TS input shape. Poll
    // until the marker is not just created but fully parseable — the shell
    // creates the file before writing stdin, so an empty read must retry.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let content = loop {
        if let Ok(content) = std::fs::read_to_string(&marker)
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content)
        {
            let _ = parsed;
            break content;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Notification hook never fired"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["sink"], serde_json::json!("context"));
    assert_eq!(
        input["notification_type"],
        serde_json::json!("task.completed")
    );
    assert_eq!(input["source_kind"], serde_json::json!("background_task"));
    assert_eq!(input["source_id"], serde_json::json!(task_id));
    assert_eq!(input["severity"], serde_json::json!("info"));
    assert!(
        input["title"].as_str().is_some_and(|t| !t.is_empty()),
        "title carries the notification title: {input}"
    );

    // And the notification itself still delivered (the hook observed but
    // could not alter the delivery). The hook marker settles before the
    // notification turn feeds the model, so poll for the delivery rather
    // than asserting once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn must still run"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = std::fs::remove_file(&marker);
}

/// Given the same task already delivered once, when several more ticks
/// pass, then no second notification turn runs (idempotent delivery).
#[tokio::test]
async fn should_not_deliver_a_duplicate_notification_on_later_ticks() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("seen the notification");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "deliver once" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);

    // Then within a few ticks a notification turn ran exactly once
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And six more ticks (300ms) deliver nothing new
    drive_notification_ticks(&server, &session_id, 6).await;
    let all = recorded_all(&mock).to_string();
    let occurrences = all.split(&format!("source_id=\\\"{task_id}\\\"")).count() - 1;
    assert_eq!(occurrences, 1, "exactly one delivery: {all}");
    assert_eq!(
        mock.chat_call_count(),
        2,
        "one user turn + one notification turn, no more"
    );
}

/// Given a turn in flight whose first LLM step answers a TOOL call (the
/// turn is mid-turn: a next step is coming), when a background task
/// completes, then the notification is parked as a steer and lands in the
/// SAME turn's next LLM request (the before_step flush — the mid-turn
/// admission, which the stop-boundary change preserves) — and the wire log
/// holds turn.steer with the background_task origin (carrying the
/// notificationId) BEFORE the turn.steer_flushed marker.
#[tokio::test]
async fn should_fold_the_notification_into_the_running_turn_when_busy() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated_tool_use(vec![LlmToolCall {
        id: "call-busy-fold-1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "true" }),
    }]);
    mock.push_text("Second reply");
    // A titled session — the test counts the mock's calls, and an untitled
    // session's settle would spend an extra scripted reply on the title
    // call.
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "busy fold" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "setPermission",
            json!({ "sessionId": &session_id, "mode": "yolo" }),
        )
        .await
        .expect("yolo permission");

    // Given a fire-mode turn whose first LLM call is gated on a tool reply
    // (busy, and a next step is guaranteed once the gate opens)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "original question", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // When the task completes mid-turn and the gate opens
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);
    // Hold the gate across at least one driver tick after the task settles,
    // so the busy-path park is deterministic (releasing immediately would
    // race the turn's end against the tick — the fold only happens when the
    // park precedes the flush).
    wait_for_parked_notification(&server, &session_id, &task_id).await;
    release.send(()).expect("the gate receiver is still alive");

    // Then the turn continues past the first reply (the notification steer
    // rides the turn's next step) and the next LLM request contains the
    // notification XML
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains("<notification "),
        "the folded notification must reach the next LLM request: {all}"
    );
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );
    // And the notification landed in the SAME turn (the post-tool step —
    // one turn.prompt for the original turn, no wakeup turn needed).
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let prompt_count = ops
        .iter()
        .filter(|op| matches!(op, WireOp::TurnPrompt { .. }))
        .count();
    assert_eq!(
        prompt_count, 1,
        "the mid-turn fold stays inside the running turn (no second turn): {ops:?}"
    );

    // And the wire log holds turn.steer with the background_task origin
    // (carrying the notificationId) before the flush marker
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let steer_index = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnSteer { origin, .. } if origin["kind"] == "background_task"))
        .expect("the wire log must record turn.steer with the notification origin");
    match &ops[steer_index] {
        WireOp::TurnSteer { input, origin } => {
            assert!(
                input[0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("<notification "),
                "the parked steer content is the notification XML: {input:?}"
            );
            assert_eq!(
                origin["notificationId"],
                format!("task:{task_id}:completed")
            );
        }
        other => panic!("expected turn.steer, got {other:?}"),
    }
    let flush_index = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnSteerFlushed { count } if *count >= 1))
        .expect("the wire log must record the flush marker");
    assert!(
        flush_index > steer_index,
        "the flush marker must follow the steer record: {ops:?}"
    );
    // And the durable append from the flush landed with the identity.
    assert!(ops.iter().any(|op| matches!(
        op,
        WireOp::ContextAppendMessage { message }
            if message["origin"]["kind"] == "background_task"
                && message["origin"]["notificationId"] == format!("task:{task_id}:completed")
    )));
}

/// Given a fire-mode turn whose model naturally stops while a background
/// task's notification is busy-parked in the steer buffer (the park precedes
/// the terminal stop — the final-summary hiding defect), when the turn ends
/// and the session goes idle, then the turn ends with NO extra LLM step
/// (the stop boundary leaves the notification parked; the summary stays the
/// last message) and the driver's idle wakeup delivers the notification as
/// its OWN turn on the next tick.
#[tokio::test]
async fn should_deliver_the_parked_notification_as_its_own_turn_when_the_turn_stops_naturally() {
    let mock = Arc::new(ScriptedLlm::new());
    // The main turn: a gated text step (the final summary), then the
    // wakeup turn's reply.
    let release = mock.push_gated("final summary");
    mock.push_text("notification ack");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session — the test counts the mock's calls, and an untitled
    // session's settle would spawn the side title call too.
    let created = server
        .call_method("session.create", json!({ "title": "stop-boundary" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Given a fire-mode turn whose only LLM step parks on a gated reply
    // (the turn is busy streaming its final summary)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "run the job", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // And the task completes while the turn is busy (the busy-path park)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let _task_id = completed_background_task(&session, &session_id);
    // Hold the turn across at least one driver tick so the park is
    // deterministic (the notification parks in the steer buffer while the
    // turn's final-summary step is still streaming — after every before_step
    // flush, so the only path that could fold it is the stop boundary).
    wait_for_parked_notification(&server, &session_id, &_task_id).await;

    // When the final summary completes the turn with a natural stop
    release.send(()).expect("the gate receiver is still alive");
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;

    // Then the notification reaches the model — as its own wakeup turn
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wakeup turn must deliver the parked notification"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // And the delivery is SEPARATE from the stopped turn: the wire log's
    // turn domain shows TWO turn.prompt records — the original turn (which
    // ended cleanly on its natural stop) and the wakeup's own turn — and the
    // turn's own LLM call never saw the notification (the summary stays the
    // turn's last model-visible message).
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let prompt_count = ops
        .iter()
        .filter(|op| matches!(op, WireOp::TurnPrompt { .. }))
        .count();
    assert_eq!(
        prompt_count, 2,
        "the parked notification must deliver as its own turn (a second \
         turn.prompt), not a forced continuation of the stopped turn: {ops:?}"
    );
    let first = mock.messages_of_call(0).to_string();
    assert!(
        !first.contains("<notification "),
        "the turn's own steps must not see the parked notification: {first}"
    );
}

/// Given an idle session whose steer buffer holds ONLY a notification steer
/// (busy-parked earlier, then the turn died before flushing — the stranding
/// states are a crash whose resume re-parks the buffer, or a bridge-level
/// turn timeout; a live abort always flushes via the continue hook's layer
/// 1), when the driver ticks, then it converts the parked notification into
/// a delivered turn — never stranded — with the wire log recording the flush
/// marker for the parked count and the buffer left empty.
#[tokio::test]
async fn should_convert_parked_notifications_into_a_turn_when_idle() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    mock.push_text("wakeup ack");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // Given a completed background task whose notification never delivered —
    // its busy-parked steer sits in the buffer (the stranded state a resume
    // restores, or a bridge-level turn timeout leaves behind)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);
    let model_id = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .model_id
        .clone();
    let buffer = server.steer_buffer_for(&session_id, "main", &model_id);
    let xml = format!(
        "<notification id=\"task:{task_id}:completed\" category=\"task\" \
         type=\"task.completed\" source_kind=\"background_task\" \
         source_id=\"{task_id}\">\nTitle: Background process completed\n\
         Severity: info\necho hi completed.\n</notification>"
    );
    buffer.lock().unwrap_or_else(|e| e.into_inner()).push(
        vec![mirri_core::ContentPart::Text { text: xml }],
        mirri_core::PromptOrigin::BackgroundTask {
            task_id: task_id.clone(),
            status: "completed".into(),
            notification_id: format!("task:{task_id}:completed"),
        },
    );

    // When the driver ticks while idle, then within a few ticks the wakeup
    // turn delivers the parked notification to the model
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the wakeup turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );

    // And the wire log recorded the flush marker covering the parked count
    // and the wakeup turn's own durable message.
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        ops.iter()
            .any(|op| matches!(op, WireOp::TurnSteerFlushed { count: 1 })),
        "the wakeup must record the parked-count flush marker: {ops:?}"
    );
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
        )),
        "the wakeup turn's message is durable: {ops:?}"
    );
    // And the steer buffer is empty afterwards.
    let buffer_empty = buffer.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    assert!(
        buffer_empty,
        "the steer buffer must be empty after the wakeup"
    );
}

/// Given a closed session, when the driver ticks, then it exits silently
/// (the loop breaks — no panic, no residual activity; B1 parity).
#[tokio::test]
async fn should_exit_the_driver_when_the_session_disappears() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "closing" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // When the session closes, the driver observes the disappearance on its
    // next pass and exits. The exit counter is the signal (not a tick
    // window): the loop stops counting passes once it breaks, so a tick
    // count cannot express this.
    let exits_before = server.task_driver_exit_count();
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    wait_for_task_driver_exit(&server, exits_before).await;

    // Then the driver exits silently: no panic, and the LLM saw only the
    // user turn (no notification turn ever ran for the closed session).
    assert_eq!(mock.chat_call_count(), 1);
}

/// Given a running background task stopped through the TaskStop tool, when
/// the notification driver ticks, then no notification turn ever runs —
/// the tool result is the model's only answer (G25) — and the persisted
/// task record shows notification_suppressed.
#[tokio::test]
async fn should_not_notify_when_the_task_was_stopped_by_taskstop() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    // One extra response would satisfy an erroneous notification turn.
    mock.push_text("should never run");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "task stopped" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = session
        .tasks()
        .register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "bg".into(),
            session_id.clone(),
            None,
        )
        .unwrap();
    run_main_agent_tool(
        &server,
        &session_id,
        "TaskStop",
        json!({ "task_id": task_id }),
    )
    .await;

    // Then across six ticks no LLM input ever contained a notification,
    // and the persisted record shows the suppression.
    drive_notification_ticks(&server, &session_id, 6).await;
    assert!(
        !notification_reached_model(&mock),
        "a TaskStop-stopped task must never notify"
    );
    assert_eq!(mock.chat_call_count(), 1);
    assert!(
        session
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_suppressed,
        "the suppression is recorded on the task"
    );
}

/// Given a session closed while a background task was still running, when
/// the session resumes, then the task reads stopped with no notification
/// delivered (the close suppressed it — TS stopAllOnExit parity).
#[tokio::test]
async fn should_suppress_notifications_for_tasks_killed_at_session_close() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // Given a still-running background task and a session close
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = session
        .tasks()
        .register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "orphan".into(),
            session_id.clone(),
            None,
        )
        .unwrap();
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server);

    // When a fresh server resumes the same session dir
    mock.push_text("resumed ack");
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
    let config = Arc::new(RwLock::new(parsed));
    let server_b = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");
    // A resumed-session turn runs (the resumed conversation's next step) —
    // enough ticks pass for any erroneously due notification to fire.
    converse_once(&server_b, &session_id).await;
    drive_notification_ticks(&server_b, &session_id, 6).await;

    // Then the task reads stopped, suppressed, and never notified.
    let session = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task = session.tasks().get(&task_id).expect("task survived");
    assert_eq!(task.status, mirri_core::task::TaskStatus::Stopped);
    assert!(
        task.notification_suppressed,
        "the close path must suppress the killed-notification"
    );
    assert!(
        !notification_reached_model(&mock),
        "no killed-notification may reach the model after the resume"
    );
    // And no message with a background_task origin exists in the context.
    let memory = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .context_memory();
    let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
    let history = binding.history();
    assert!(
        history.iter().all(|m| !matches!(
            m.origin,
            Some(mirri_core::PromptOrigin::BackgroundTask { .. })
        )),
        "the resumed context holds no notification message"
    );
}

/// Given a print-mode session configured with `[print backgroundMode=drain]`
/// whose background task settles during the drain wait, when the print
/// completion decision runs, then the drain suppresses the notification —
/// no notification turn ever runs (the print process is exiting).
#[tokio::test]
async fn should_suppress_notifications_when_print_mode_drains() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(test_manager_with_sqlite_store(workspace, tmp.path()));
    let mut parsed =
        ServerConfig::from_toml("[task]\nprintBackgroundMode = \"drain\"\n").expect("test config");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback): seed the test default.
    parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    let config = Arc::new(RwLock::new(parsed));
    let server = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // Given a background task that settles during the drain wait (it must
    // be still-running at drain entry — the drain suppresses the ACTIVE
    // tasks, TS printModeService parity)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = session
        .tasks()
        .register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "drained".into(),
            session_id.clone(),
            None,
        )
        .unwrap();
    let settle_tasks = session.tasks();
    let settle_id = task_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        settle_tasks.complete(&settle_id, 0);
    });

    // When the print completion decision runs (drain arm)
    let decision = server
        .call_method(
            "session.handlePrintMainTurnCompleted",
            json!({ "session_id": &session_id }),
        )
        .await
        .expect("decision");
    assert_eq!(decision["decision"], "finish", "a settled drain finishes");

    // Then several ticks pass with no notification turn.
    drive_notification_ticks(&server, &session_id, 6).await;
    assert!(
        !notification_reached_model(&mock),
        "a drained print-session task must not notify"
    );
    assert!(
        session
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_suppressed,
        "the drain suppresses the still-active tasks' notifications"
    );
}

/// Given a session whose disk-recorded task completed while the server was
/// down (the notification never delivered), when the session resumes, then
/// the notification message sits silently in context — no turn ran for it —
/// while a task whose notification WAS durably delivered pre-restart is not
/// re-appended.
#[tokio::test]
async fn should_silently_reappend_undelivered_notifications_on_resume() {
    // Server A: no LLM provider — the delivery debt stays whole.
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
    let config = Arc::new(RwLock::new(parsed));
    let server_a = EmbeddedServer::with_providers(manager, None, None, None, None, config, None)
        .with_home_dir(Some(tmp.path().to_path_buf()));
    let created = server_a
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;

    // One task whose notification is durably delivered (its
    // context.append_message is on the wire log)…
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let delivered_task = session_a
        .tasks()
        .register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "delivered".into(),
            session_id.clone(),
            None,
        )
        .unwrap();
    session_a.tasks().complete(&delivered_task, 0);
    let delivered_origin = json!({
        "kind": "background_task",
        "taskId": delivered_task,
        "status": "completed",
        "notificationId": format!("task:{delivered_task}:completed"),
    });
    session_a
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": "<notification id=\"task:delivered\" />" }],
                "origin": delivered_origin,
            }),
        })
        .await;
    // …and one whose notification never delivered (completed while the
    // server had no provider, then closed).
    let undelivered_task = session_a
        .tasks()
        .register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "undelivered".into(),
            session_id.clone(),
            None,
        )
        .unwrap();
    session_a.tasks().complete(&undelivered_task, 0);
    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);

    // When server B (with a provider and a fast tick) resumes the session
    let mock = Arc::new(ScriptedLlm::new());
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
    let config = Arc::new(RwLock::new(parsed));
    let server_b = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");

    // Then the undelivered task's notification sits silently in context —
    // no turn ran for it (the mock saw no LLM call at all).
    drive_notification_ticks(&server_b, &session_id, 6).await;
    assert_eq!(
        mock.chat_call_count(),
        0,
        "the restore appends silently — no notification turn"
    );
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    {
        // The guard's scope ends before the wire-log awaits below — a
        // memory guard held across an await deadlocks the single-threaded
        // test runtime against the notification driver's ledger scan.
        let memory = session_b
            .get_agent(&mirri_core::AgentId::new("main"))
            .await
            .expect("main agent")
            .context_memory();
        let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
        let history = binding.history();
        let undelivered_keys: Vec<&mirri_core::PromptOrigin> = history
            .iter()
            .filter_map(|m| m.origin.as_ref())
            .filter(|o| {
                matches!(
                    o,
                    mirri_core::PromptOrigin::BackgroundTask { task_id, .. }
                        if task_id.as_str() == undelivered_task.as_str()
                )
            })
            .collect();
        assert_eq!(undelivered_keys.len(), 1, "exactly one re-append");
        // And the delivered task's notification is present exactly once (not
        // re-appended on top of the restored one).
        let delivered_keys: Vec<&mirri_core::PromptOrigin> = history
            .iter()
            .filter_map(|m| m.origin.as_ref())
            .filter(|o| {
                matches!(
                    o,
                    mirri_core::PromptOrigin::BackgroundTask { task_id, .. }
                        if task_id.as_str() == delivered_task.as_str()
                )
            })
            .collect();
        assert_eq!(delivered_keys.len(), 1, "no duplicate re-append");
        // And the re-appended message carries the notification XML.
        let xml_texts: Vec<String> = history
            .iter()
            .filter(|m| {
                matches!(
                    m.origin,
                    Some(mirri_core::PromptOrigin::BackgroundTask { ref task_id, .. })
                        if task_id.as_str() == undelivered_task.as_str()
                )
            })
            .map(|m| m.message.text_content().to_string())
            .collect();
        assert!(
            xml_texts.iter().any(|t| t.contains("<notification ")),
            "{xml_texts:?}"
        );
    }
    // And no notification turn.prompt ever recorded post-resume (the wire
    // log holds the restore's context.append_message only).
    let wire_log_path = server_b
        .session_manager()
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        !ops.iter().any(|op| matches!(
            op,
            WireOp::TurnPrompt { origin, .. } if origin["kind"] == "background_task"
        )),
        "the resume must not launch notification turns"
    );
    assert!(
        ops.iter().any(
            |op| matches!(op, WireOp::ContextAppendMessage { message } if
                message["origin"]["taskId"] == undelivered_task.as_str())
        ),
        "the restore's durable append is on the wire log"
    );
}

/// Given a delivered notification turn, when the user undoes that turn,
/// then the driver re-delivers the notification (undo reconciliation —
/// the scan-based ledger: the undo removed the message, the key is no
/// longer in context, the debt is whole again).
#[tokio::test]
async fn should_redeliver_the_notification_after_the_turn_is_undone() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    mock.push_text("ack 1");
    converse_once(&server, &session_id).await;
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);

    // Given the notification delivered (exactly one model sighting)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let calls_after_delivery = mock.chat_call_count();

    // When the user undoes the notification turn
    mock.push_text("redelivered ack");
    server
        .call_method(
            "session.undo",
            json!({ "session_id": &session_id, "count": 1 }),
        )
        .await
        .expect("undo");

    // Then within a few ticks the notification XML appears in a fresh LLM
    // input again — exactly one re-delivery.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let sightings = recorded_all(&mock)
            .to_string()
            .split(&format!("source_id=\\\"{task_id}\\\""))
            .count()
            - 1;
        if sightings >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification was never re-delivered after the undo"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And a settling window keeps it at exactly one re-delivery.
    drive_notification_ticks(&server, &session_id, 6).await;
    let all = recorded_all(&mock).to_string();
    let sightings = all.split(&format!("source_id=\\\"{task_id}\\\"")).count() - 1;
    assert_eq!(sightings, 2, "original + exactly one re-delivery: {all}");
    assert_eq!(
        mock.chat_call_count(),
        calls_after_delivery + 1,
        "one redelivery turn ran"
    );
    // And the context's latest background_task-origin message postdates
    // the undo (a fresh message, not the pre-undo one).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let memory = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .context_memory();
    let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
    let history = binding.history();
    let notification_positions: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            matches!(
                m.origin,
                Some(mirri_core::PromptOrigin::BackgroundTask { ref task_id, .. })
                    if task_id.as_str() == task_id.as_str()
            )
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        notification_positions.len(),
        1,
        "{notification_positions:?}"
    );
    // And the context's latest background_task-origin message postdates
    // the undo (a fresh message, not the pre-undo one). Asserted by
    // origin: with the title reminder injector retired (spec 09's naming
    // is a side call that writes nothing to memory), nothing may follow
    // the re-delivered notification except the redelivery turn's own
    // assistant reply — and the reply must be the last entry.
    let trailing = &history[notification_positions[0] + 1..];
    assert!(
        trailing.iter().all(|m| {
            m.origin.is_none() && matches!(m.message.role, mirri_core::Role::Assistant)
        }),
        "only the redelivery turn's reply may \
         follow the re-delivered notification: {trailing:?}"
    );
    assert!(
        trailing
            .last()
            .is_some_and(|m| matches!(m.message.role, mirri_core::Role::Assistant)),
        "the redelivery turn's reply must be the latest entry: {trailing:?}"
    );
}

/// Given a delivered notification and NO undo, when ticks pass, then
/// nothing re-delivers (the negative control for the scan-based ledger).
#[tokio::test]
async fn should_keep_the_notification_delivered_when_nothing_is_undone() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    mock.push_text("ack 1");
    converse_once(&server, &session_id).await;
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Then six more ticks deliver nothing new.
    drive_notification_ticks(&server, &session_id, 6).await;
    let all = recorded_all(&mock).to_string();
    let sightings = all.split(&format!("source_id=\\\"{task_id}\\\"")).count() - 1;
    assert_eq!(sightings, 1, "exactly one delivery, no re-delivery: {all}");
}

/// Given a delivered notification whose message a compaction drops from the
/// main agent's live context (the head/tail rewrite keeps only real user
/// input + summary + retention tail — the `background_task` origin is not
/// real user input), when the driver ticks, then nothing re-delivers: the
/// durable delivery marker (`notification_delivered_at`) keeps the debt
/// paid even though the scan-based ledger no longer holds the key (the
/// historical replay loop after context compaction).
#[tokio::test]
async fn should_not_redeliver_a_notification_when_compaction_drops_it_from_context() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("notification handled");
    mock.push_text("continuation one");
    mock.push_text("continuation two");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "compaction" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);

    // Given the notification delivered exactly once (one model sighting),
    // and the delivery settled (the durable marker is on the task record).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_turn_idle(&server, &session_id).await;
    assert_eq!(
        mock.chat_call_count(),
        2,
        "one user turn + one notification turn before the conversation continues"
    );
    assert!(
        session
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_some(),
        "the delivery marker must be durable before the compaction"
    );

    // When the conversation continues past the notification (the shape a
    // compaction round is triggered in)…
    for text in ["continue one", "continue two"] {
        server
            .call_method(
                "message.send",
                json!({ "session_id": &session_id, "text": text, "mode": "wait" }),
            )
            .await
            .expect("continuation turn");
    }

    // …and the live memory is compacted with the notification inside the
    // compacted prefix: the retention tail begins just past the notification
    // turn, so the delivered message is gone from context while the recent
    // continuation stays verbatim.
    let memory = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .context_memory();
    let outcome = {
        let mut binding = memory.lock().unwrap_or_else(|e| e.into_inner());
        let notification_index = {
            let history = binding.history();
            let index = history
                .iter()
                .position(|m| {
                    matches!(
                        m.origin,
                        Some(mirri_core::PromptOrigin::BackgroundTask { task_id: ref t, .. })
                            if t.as_str() == task_id.as_str()
                    )
                })
                .expect("the delivered notification must be in the live history");
            assert!(
                history.get(index + 1).is_some_and(|m| {
                    m.origin.is_none() && matches!(m.message.role, mirri_core::Role::Assistant)
                }),
                "the notification turn's reply follows the notification: {history:?}"
            );
            index
        };
        binding.apply_compaction(mirri_core::CompactionInput {
            summary: "compacted".to_string(),
            context_summary: "The conversation through the notification was compacted.".to_string(),
            compacted_count: 0,
            tokens_before: 0,
            dropped_count: 0,
            retention_tail_start: Some(notification_index + 2),
        })
    };
    assert!(
        outcome.applied,
        "the compaction apply must be accepted by the shrink check"
    );
    {
        let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
        let history = binding.history();
        assert!(
            history.iter().all(|m| !matches!(
                m.origin,
                Some(mirri_core::PromptOrigin::BackgroundTask { task_id: ref t, .. })
                    if t.as_str() == task_id.as_str()
            )),
            "the compaction must drop the delivered notification from live memory: {history:?}"
        );
    }

    // Then across ~600ms (a dozen driver ticks) nothing re-delivers, even
    // though the scan-based ledger lost the key: the durable marker keeps
    // the debt paid.
    let calls_after_compaction = mock.chat_call_count();
    assert_eq!(
        calls_after_compaction, 4,
        "one user turn + one notification turn + two continuation turns"
    );
    drive_notification_ticks(&server, &session_id, 12).await;
    assert_eq!(
        mock.chat_call_count(),
        calls_after_compaction,
        "no re-delivery turn may run after the compaction"
    );
    // And the wire log holds exactly the ONE notification turn.prompt and
    // the ONE durable delivery append — no re-delivery turn record after
    // the compaction (the pre-compaction continuation turns re-read the
    // still-present message, so recorded-input sighting counts are not the
    // proof here; the turn records are).
    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let notification_prompts = ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                WireOp::TurnPrompt { origin, .. } if origin["kind"] == "background_task"
            )
        })
        .count();
    assert_eq!(
        notification_prompts, 1,
        "exactly the one delivered notification turn, no re-delivery: {ops:?}"
    );
    let notification_appends = ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                WireOp::ContextAppendMessage { message }
                    if message["origin"]["kind"] == "background_task"
                        && message["origin"]["taskId"] == task_id.as_str()
            )
        })
        .count();
    assert_eq!(
        notification_appends, 1,
        "exactly the one durable delivery append, no re-append: {ops:?}"
    );

    // And the due adjudication itself stays empty — the durable-marker
    // guard inside the filter with the emptied delivered set.
    let due = {
        // The guard's scope ends before the assert — a memory guard held
        // across an await deadlocks the single-threaded test runtime
        // against the notification driver's ledger scan.
        let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
        let delivered = mirri_core::task_notification::delivered_keys(&binding);
        let parked = mirri_core::task_notification::parked_keys(&mirri_core::SteerBuffer::new());
        mirri_core::task_notification::due_notifications(&session.tasks(), &delivered, &parked)
    };
    assert!(
        due.is_empty(),
        "the ledgeless task must not adjudicate as due: {due:?}"
    );
}

/// Given a session that has already delivered its one notification, when
/// several more driver ticks pass, then no further LLM is created — the
/// per-tick cost stops once the ledger says nothing is due (a delivered
/// task's record stays terminal+detached+unsuppressed forever, so the
/// fast path's candidate peek alone cannot retire the tick).
#[tokio::test]
async fn should_not_create_an_llm_on_ticks_after_the_delivery_settles() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("notification ack");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
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
    let config = Arc::new(RwLock::new(parsed));
    let server = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(RecordingLlmFactory {
            llm: Arc::clone(&mock),
            seen: Arc::clone(&seen),
        })),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    completed_background_task(&session, &session_id);

    // Given the notification delivered (the factory served the turn)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let creations_after_delivery = seen.lock().unwrap_or_else(|e| e.into_inner()).len();

    // Then six more ticks create no further LLM (the empty-due guard
    // retires the tick before the gate).
    drive_notification_ticks(&server, &session_id, 6).await;
    let creations_total = seen.lock().unwrap_or_else(|e| e.into_inner()).len();
    assert_eq!(
        creations_total, creations_after_delivery,
        "no LLM creation on ticks after the delivery settled"
    );
}

/// Seed one overdue cron task on the session (the white-box seam the
/// existing cron e2e family uses).
fn seed_overdue_cron_task(session: &mirri_core::SessionScope, session_id: &str, prompt: &str) {
    session.cron().restore_tasks([mirri_core::cron::CronTask {
            session_id: session_id.to_string(),
            recurring: false,
            created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            scope: mirri_core::cron::TaskScope::Session,
            ..mirri_core::cron::cron_task_base("cron-b2", "* * * * *", prompt)
        }]);
    session.cron().set_next_fire_at(
        "cron-b2",
        Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
    );
}

/// Given a running main-agent turn, when a cron task comes due, then the
/// fire is held — no turn runs for it while the gate holds — and the first
/// idle tick injects it wrapped in the `<cron-fire>` envelope (B2 T9).
#[tokio::test]
async fn should_hold_the_cron_fire_until_idle_and_inject_it_enveloped() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("First reply");
    mock.push_text("cron reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Given a busy main-agent turn (the first LLM call parks on the gate)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "original question", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // And a cron task coming due mid-turn
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    seed_overdue_cron_task(&session, &session_id, "check the deployment");

    // Then across several driver ticks while the gate holds, the fire is
    // held: no cron turn ran (the call count stays at the busy turn's).
    wait_for_held_fire(&server, &session_id, "cron-b2").await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the cron fire must be held while the turn runs"
    );

    // When the gate opens and the turn completes
    release.send(()).expect("the gate receiver is still alive");
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;

    // Then the next idle tick injects the fire, enveloped: the model input
    // starts with <cron-fire, ends with </cron-fire>, and carries the raw
    // prompt verbatim inside the <prompt> body.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let enveloped_input = loop {
        let found = {
            let recorded = mock
                .recorded_messages
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            recorded
                .iter()
                .map(|messages| messages.to_string())
                .find(|text| text.contains("check the deployment") && text.contains("<cron-fire "))
        };
        if let Some(text) = found {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held fire was never injected: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        enveloped_input.contains("<cron-fire jobId=\\\"cron-b2\\\""),
        "the envelope names the firing task: {enveloped_input}"
    );
    assert!(
        enveloped_input.contains("</cron-fire>"),
        "the envelope closes: {enveloped_input}"
    );
}

/// Seed one overdue cron task with an explicit `recurring` flag on the
/// session (the held-fire delete tests' white-box seam: the existing
/// [`seed_overdue_cron_task`] hardcodes one-shot).
fn seed_overdue_cron_task_with_recurring(
    session: &mirri_core::SessionScope,
    session_id: &str,
    prompt: &str,
    recurring: bool,
) -> String {
    let id = format!("cron-b2-{recurring}");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
        session_id: session_id.to_string(),
        recurring,
        created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
        ..mirri_core::cron::cron_task_base(&id, "* * * * *", prompt)
    }]);
    session.cron().set_next_fire_at(
        &id,
        Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
    );
    id
}

/// Given a busy main-agent turn with a recurring cron fire already held,
/// when the task is deleted before the turn ends, then the held fire is
/// purged — the prompt is never injected (#177 P2).
#[tokio::test]
async fn should_not_inject_a_held_fire_when_a_recurring_task_is_deleted_mid_turn() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("First reply");
    mock.push_text("cron reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held purge recurring" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Given a busy main-agent turn (the first LLM call parks on the gate)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "original question", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // And a recurring task coming due mid-turn
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = seed_overdue_cron_task_with_recurring(
        &session,
        &session_id,
        "check the deployment",
        true,
    );

    // Then across several ticks while the gate holds, the fire is held
    wait_for_held_fire(&server, &session_id, &task_id).await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the cron fire must be held while the turn runs"
    );

    // When the task is deleted while the fire is still held
    assert!(
        session.cron().remove(&task_id).await,
        "the recurring task is still in the table when removed"
    );
    release.send(()).expect("the gate receiver is still alive");
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;

    // Then after several idle ticks the held fire was purged: the prompt
    // never reaches the model and no further turn runs.
    wait_for_held_fires_cleared(&server, &session_id).await;
    let all = recorded_all(&mock).to_string();
    assert!(
        !all.contains("<cron-fire "),
        "the deleted task's held fire must never be injected: {all}"
    );
    assert!(
        !all.contains("check the deployment"),
        "the deleted task's prompt must never reach the model: {all}"
    );
    assert_eq!(
        mock.chat_call_count(),
        1,
        "only the original busy turn ran"
    );
}

/// Given a busy main-agent turn with a one-shot cron fire already held,
/// when the task is deleted after its fire left the table (one-shots are
/// removed by `check_due` itself, so `remove` answers false), then the
/// held fire is still purged — the prompt is never injected (#177 P2;
/// the naive get()-filter would fail exactly here).
#[tokio::test]
async fn should_not_inject_a_held_one_shot_fire_when_its_task_is_deleted_after_firing() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("First reply");
    mock.push_text("cron reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held purge one-shot" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Given a busy main-agent turn (the first LLM call parks on the gate)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "original question", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // And a one-shot task coming due mid-turn (check_due fires and removes
    // it from the table; the fire waits in the driver's held buffer)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = seed_overdue_cron_task_with_recurring(
        &session,
        &session_id,
        "check the deployment",
        false,
    );

    // Then across several ticks while the gate holds, the fire is held
    wait_for_held_fire(&server, &session_id, &task_id).await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the cron fire must be held while the turn runs"
    );

    // When the task is deleted after firing: the one-shot already left the
    // table so remove answers false — yet the purge must take effect
    assert!(
        !session.cron().remove(&task_id).await,
        "the one-shot already left the table at check_due"
    );
    release.send(()).expect("the gate receiver is still alive");
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;

    // Then after several idle ticks the held fire was purged: the prompt
    // never reaches the model and no further turn runs.
    wait_for_held_fires_cleared(&server, &session_id).await;
    let all = recorded_all(&mock).to_string();
    assert!(
        !all.contains("<cron-fire "),
        "the deleted task's held fire must never be injected: {all}"
    );
    assert!(
        !all.contains("check the deployment"),
        "the deleted task's prompt must never reach the model: {all}"
    );
    assert_eq!(
        mock.chat_call_count(),
        1,
        "only the original busy turn ran"
    );
}

/// Given an idle session whose steer buffer holds a MIXED set (a user steer
/// parked ahead of a notification), when the driver ticks, then the wakeup
/// is skipped — the buffer survives untouched (FIFO intact, no turn runs)
/// — and the next engine turn's flush delivers both in order (the
/// scoping rule of the wakeup: converting a mixed buffer would break
/// user-steer FIFO).
#[tokio::test]
async fn should_leave_a_mixed_buffer_untouched_until_the_next_turn() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("after flush");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "mixed buffer" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // Given a completed background task and a buffer holding its parked
    // notification BEHIND a user steer (the mixed state)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = completed_background_task(&session, &session_id);
    let model_id = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .model_id
        .clone();
    let buffer = server.steer_buffer_for(&session_id, "main", &model_id);
    {
        let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(
            vec![mirri_core::ContentPart::Text {
                text: "user interjection".into(),
            }],
            mirri_core::PromptOrigin::User,
        );
        buf.push(
            vec![mirri_core::ContentPart::Text {
                text: "<notification id=\"task:probe\" />".into(),
            }],
            mirri_core::PromptOrigin::BackgroundTask {
                task_id: task_id.clone(),
                status: "completed".into(),
                notification_id: format!("task:{task_id}:completed"),
            },
        );
    }

    // When several driver ticks pass while idle
    drive_notification_ticks(&server, &session_id, 6).await;

    // Then the wakeup skipped: no notification turn ran (the parked key
    // covers the task's notification, so nothing was due either)…
    assert_eq!(
        mock.chat_call_count(),
        1,
        "a mixed buffer must not trigger the wakeup turn"
    );
    // …and the buffer survives untouched, in FIFO order.
    {
        let buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        let origins: Vec<bool> = buf
            .iter()
            .map(|s| matches!(s.origin, mirri_core::PromptOrigin::BackgroundTask { .. }))
            .collect();
        assert_eq!(
            origins,
            vec![false, true],
            "the mixed buffer keeps both steers, user first"
        );
    }

    // And the next engine turn's flush delivers both, in order, in one
    // request.
    converse_once(&server, &session_id).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let all = recorded_all(&mock).to_string();
        if all.contains("user interjection") && all.contains("<notification ") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mixed buffer never flushed into a turn: {all}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let all = recorded_all(&mock).to_string();
    let user_at = all.find("user interjection").expect("steer text present");
    let notification_at = all.find("<notification ").expect("notification present");
    assert!(
        user_at < notification_at,
        "the user steer flushes before the notification: {all}"
    );
    // The buffer is drained by the turn's `before_step` flush, which
    // `take_items()`s under its own lock — but the notification driver's
    // stranded-buffer wakeup ALSO takes the items and pushes them back
    // when the set is not notification-only (task_driver.rs), so a single
    // instantaneous read can catch that take/push window and see an empty
    // buffer that is not the flush's work. Poll for the settled state
    // (converged empty) instead of asserting on one instant: the durable
    // fact under test is "the steers reached the model in order and the
    // buffer no longer holds them", not "no thread held a transient take
    // at the instant of the read".
    let settle_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if buffer.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < settle_deadline,
            "the flush never emptied the buffer"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // And it stays empty: the wakeup path must not have re-parked a
    // notification the turn already consumed.
    drive_notification_ticks(&server, &session_id, 2).await;
    assert!(
        buffer.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "the flush emptied the buffer and nothing re-parked it"
    );
}

/// Given an idle session, when a cron task fires, then the injected prompt
/// carries the envelope and the raw prompt stays recoverable (the
/// extractCronPrompt caliber — no web change needed).
#[tokio::test]
async fn should_inject_the_enveloped_prompt_when_idle() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    mock.push_text("cron reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // When the task fires while idle
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    seed_overdue_cron_task(&session, &session_id, "report the build status");

    // Then within a few ticks the injected turn's LLM input carries the
    // full envelope with the raw prompt verbatim.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let all = recorded_all(&mock).to_string();
        if all.contains("report the build status")
            && all.contains("<cron-fire ")
            && all.contains("</cron-fire>")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the enveloped cron prompt never reached the model: {all}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And the durable turn.prompt recorded the enveloped text (the wire
    // log's cron turn is replayable, envelope included).
    let wire_log_path = server
        .session_manager()
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&wire_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WireOp::TurnPrompt { input, origin }
                if origin["kind"] == "cron_job"
                    && input[0]["text"].as_str().unwrap_or("").starts_with("<cron-fire ")
        )),
        "the turn.prompt carries the envelope: {ops:?}"
    );
}

/// Given a background question (AskUserQuestion run_in_background=true)
/// answered by the client, when the task settles, then the notification
/// delivers the answer text to the model (D-58 closure) — the notification
/// children's preview block carries the answer, and the next model turn
/// can read it (the recorded input proves the loop closed).
#[tokio::test]
async fn should_deliver_the_answer_when_a_background_question_completes() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("question acknowledged");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // The client side of the reverse RPC: the answer rides question.ask.
    let handler: ReverseRpcHandler = Arc::new(|_method, _params| {
        Box::pin(async { Ok(json!({ "answers": [{ "answer": "B" }] })) })
    });
    server.on_reverse_rpc(handler).await.expect("handler");
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    // When the model asks in the background and the client answers
    let result = run_main_agent_tool(
        &server,
        &session_id,
        "AskUserQuestion",
        json!({
            "questions": [{
                "question": "Which option?",
                "options": [{ "label": "A" }, { "label": "B" }],
            }],
            // The ask tool's background key is `background` (spec-aligned
            // rename in 06ef5fd9); the old `run_in_background` spelling
            // would fall back to blocking mode and answer synchronously.
            "background": true,
        }),
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    let text = match &result.output {
        mirri_core::ToolOutput::Text(text) => text.clone(),
        other => panic!("expected text output, got {other:?}"),
    };
    let task_id = text
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("the background ask returns a task id")
        .to_string();

    // Then within a few ticks the notification turn ran: the model input
    // carries the notification XML naming the question task as completed…
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let all = recorded_all(&mock).to_string();
        if all.contains("<notification ")
            && all.contains(&format!("source_id=\\\"{task_id}\\\""))
            && all.contains("Background question completed")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the question's answer never reached the model: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // …and the notification children carry the answer: the persisted-log
    // `<output-file>` block (the session has persistence — the TS shape),
    // with the answer text retrievable through the TaskOutput tool the
    // notification points at (the model can answer a follow-up about the
    // task).
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains("<output-file"),
        "the output block rides the notification: {all}"
    );
    let snapshot = run_main_agent_tool(
        &server,
        &session_id,
        "TaskOutput",
        json!({ "task_id": task_id }),
    )
    .await;
    let snapshot_text = match &snapshot.output {
        mirri_core::ToolOutput::Text(text) => text.clone(),
        other => panic!("expected text output, got {other:?}"),
    };
    assert!(
        snapshot_text.contains("\"answer\": \"B\""),
        "the answer text itself must be retrievable: {snapshot_text}"
    );
    assert!(all.contains("type=\\\"task.completed\\\""), "{all}");
}

/// Given a terminal detached task dispatched by a live subagent (a nested
/// dispatch), When the driver ticks, Then the notification turn runs on the
/// DISPATCHING subagent — not the completed subject child and not the
/// session's main agent (B4 T9, A14 + review SF-3: the addressee is the
/// registry's parent link of the task's subject, not the subject itself).
#[tokio::test(flavor = "multi_thread")]
async fn should_deliver_a_notification_to_the_owning_agent_when_a_nested_subagent_finishes() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("the child saw the notification");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    // A titled session: the test counts LLM calls, and the untitled
    // session's post-settle side title call would add one more.
    let created = server
        .call_method("session.create", json!({ "title": "nested finish" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");

    // The nested-dispatch shape B4 T5 creates: a dispatcher subagent live
    // in the session whose spawned grandchild (the task's SUBJECT — the
    // `TaskKind::Agent` agent id stays the recovery/resume identity)
    // completed a background task. The subject→dispatcher edge is the
    // registry's parent link, folded live by `subagent.registered` (the
    // same op the host writes at spawn).
    let child_id = "agent-nested-1";
    let subject_id = "agent-grandchild-1";
    session
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("child agent");
    session
        .record_op(mirri_proto::WireOp::SubagentRegistered {
            agent_id: subject_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(child_id.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    let tasks = session.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Agent {
                agent_id: Some(subject_id.to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "nested work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "nested result\n");
    tasks.complete(&task_id, 0);

    // Then within a few ticks the notification turn ran — and it fed the
    // model exactly once beyond the main turn (the child's turn), with
    // the notification naming the task.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if mock.chat_call_count() >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_agent_turn_idle(&server, &session_id, child_id).await;
    assert_eq!(
        mock.chat_call_count(),
        2,
        "exactly the child's notification turn runs — main stays untouched"
    );
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );
    assert!(all.contains("type=\\\"task.completed\\\""), "{all}");

    // And the notification landed in the CHILD's context, not main's.
    let child = session
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("child agent");
    let child_history = {
        let memory = child.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        child_history.contains("<notification "),
        "the owning subagent must receive the notification: {child_history}"
    );
    let main = session
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let main_history = {
        let memory = main.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        !main_history.contains("<notification "),
        "the main agent must NOT receive the nested task's notification: {main_history}"
    );
}

/// Given a completed background PROCESS task registered with a live
/// subagent as its dispatcher (the bash background path a subagent started),
/// when the driver ticks, then the completion notification is delivered to
/// the DISPATCHING subagent — not the session's main agent (the
/// dispatcher-attribution half of review SF-3; the process arm of
/// `task_dispatcher` runs the same liveness check as the agent arm).
#[tokio::test(flavor = "multi_thread")]
async fn should_deliver_a_process_notification_to_the_subagent_that_dispatched_it() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("the owner saw the notification");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "bash owner" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    // A live subagent in the session — the dispatcher the bash task's
    // notification must be addressed to.
    session
        .create_agent(
            AgentId::new("agent-bash-owner"),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("owner agent");

    let tasks = session.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some("agent-bash-owner".to_string()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);

    // Then within a few ticks the notification turn ran on the OWNER —
    // exactly one extra model call (the owner's turn), main untouched
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if mock.chat_call_count() >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_agent_turn_idle(&server, &session_id, "agent-bash-owner").await;
    assert_eq!(
        mock.chat_call_count(),
        2,
        "exactly the owner's notification turn runs — main stays untouched"
    );

    // And the notification landed in the OWNER's context, not main's.
    let owner = session
        .get_agent(&AgentId::new("agent-bash-owner"))
        .await
        .expect("owner agent");
    let owner_history = {
        let memory = owner.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        owner_history.contains("<notification "),
        "the dispatching subagent must receive the notification: {owner_history}"
    );
    let main = session
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let main_history = {
        let memory = main.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        !main_history.contains("<notification "),
        "the main agent must NOT receive the subagent's bash task notification: {main_history}"
    );
}

/// Given a terminal detached process task dispatched by a live subagent
/// (the bash-background shape), when the notification turn has delivered
/// and the session is resumed over the same directory, then the
/// notification is NOT re-delivered as a fresh turn: its records ride the
/// ADDRESSEE's own journal (`agents/<id>/wire.jsonl`), so the rehydrated
/// subagent's rebuilt context carries the delivered message — and a second
/// restart stays quiet too (the restored context AND the durable delivery
/// marker both keep the debt satisfied).
#[tokio::test(flavor = "multi_thread")]
async fn should_not_redeliver_a_subagent_notification_after_the_session_is_resumed() {
    // Server A: a titled session (the counting tests avoid the untitled
    // session's post-settle side title call), a live subagent that is also
    // durably registered (parent = main) and owns a journal slot of its
    // own — the restored child must exist and rehydrate from that slot —
    // plus a completed detached process task it dispatched.
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("the child saw the notification");
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
    let config = Arc::new(RwLock::new(parsed));
    let server_a = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    let created = server_a
        .call_method("session.create", json!({ "title": "resume no-replay" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;

    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let child_id = "agent-resume-owner";
    session_a
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("owner agent");
    // The durable registration the RESUME face restores children from (the
    // live create_agent alone is in-memory): with parent main, exactly the
    // dispatch shape the process arm attributes back to the dispatcher.
    session_a
        .record_op(WireOp::SubagentRegistered {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(super::DEFAULT_MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    // The child's journal slot must exist for the resume to rehydrate the
    // child (restore skips a registered child without a journal) — the
    // host spawn creates the slot before the child's first turn; mirror it.
    session_a.child_wire_log(child_id).await;
    let tasks = session_a.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some(child_id.to_string()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);

    // Then the notification turn ran on the OWNER exactly once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_agent_turn_idle(&server_a, &session_id, child_id).await;
    let calls_after_delivery = mock.chat_call_count();
    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);

    // When server B (same tempdir, fast tick) resumes the session
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
    let config = Arc::new(RwLock::new(parsed));
    let server_b = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");

    // Then no fresh notification turn runs — the rehydrated owner's
    // context already holds the delivered message.
    drive_notification_ticks(&server_b, &session_id, 6).await;
    assert_eq!(
        mock.chat_call_count(),
        calls_after_delivery,
        "the resumed session must not re-deliver the subagent notification"
    );
    // And the routing contract: the subagent notification's records live on
    // the OWNER's own journal — the records the rehydrated context is
    // rebuilt from.
    let child_log_path = server_b
        .session_manager()
        .persistence()
        .agent_wire_log_path(&SessionId::new(&session_id), child_id);
    let log = JsonlAppendLog::open(&child_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
        )),
        "the subagent notification's context.append_message must ride the addressee journal"
    );

    // When the session is resumed a SECOND time (close/drop B, server C)
    server_b
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_b);
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
    let config = Arc::new(RwLock::new(parsed));
    let server_c = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    server_c
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");

    // Then it is still silent — the multi-restart lock holds.
    drive_notification_ticks(&server_c, &session_id, 6).await;
    assert_eq!(
        mock.chat_call_count(),
        calls_after_delivery,
        "a second restart must stay silent too"
    );
}

/// Given a subagent addressee mid-turn (a running host child) when a
/// detached task IT dispatched completes, then the notification parks as a
/// `turn.steer` on the CHILD journal (no durable delivery marker — FIX 1 —
/// and no model delivery). When the session resumes over the same directory,
/// then the pending steer is re-parked into the SUBAGENT's steer buffer —
/// the content survives the restart — and once the addressee is idle the
/// re-parked notification wakes into EXACTLY ONE delivery turn on the
/// child: never lost, never duplicated, and never re-parked into main's
/// buffer. (Task 8; the post-resume delivery mirrors the live busy→idle
/// wakeup path — the restart only preserves the parked content.)
#[tokio::test(flavor = "multi_thread")]
async fn should_re_park_a_pending_subagent_notification_steer_on_resume() {
    use mirri_core::{AgentProfile, PermissionMode, ProfileManager};
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    // The child's first LLM call is gated: the child is genuinely mid-turn
    // for as long as the test holds the release handle.
    let release_child_a = mock.push_gated("child working");
    // Both servers share the same tempdir (the restart continuity) and the
    // same scripted LLM (the call-count continuity across the restart).
    let build_server = || {
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(test_manager_with_sqlite_store(workspace, tmp.path()));
        // A profile manager wires the production subagent host into the
        // create path — the busy-path exclusion (the host's per-child
        // `running` flag) needs the host.
        let profiles = Arc::new(ProfileManager::new());
        profiles
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "mock-llm".to_string(),
                permission_mode: Some(PermissionMode::Yolo),
                ..Default::default()
            })
            .expect("profile registration");
        let mut parsed = ServerConfig::from_toml("").expect("test config");
        // The create chain requires a resolvable model (spec 11 R4 — no
        // built-in fallback): seed the test default.
        parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
        let config = Arc::new(RwLock::new(parsed));
        EmbeddedServer::with_providers(
            manager,
            Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
            Some(profiles),
            None,
            None,
            config,
            None,
        )
        .with_home_dir(Some(tmp.path().to_path_buf()))
        .with_task_sweep_tick(Duration::from_millis(50))
    };

    // Server A: a titled session (call-count determinism), one settled main
    // turn, then a child spawned through the production host and held
    // mid-turn on its gated first LLM call.
    let server_a = build_server();
    let created = server_a
        .call_method("session.create", json!({ "title": "re-park pending" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the main hello turn consumed the first reply"
    );
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let main_a = session_a
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let host = {
        let config = main_a.config();
        let guard = config.read().unwrap_or_else(|e| e.into_inner());
        guard
            .subagent_host
            .clone()
            .expect("the create chain wires the host when a profile manager is present")
    };
    let mut handle = host
        .spawn(mirri_core::SpawnRequest {
            profile_name: "coder".to_string(),
            prompt: "do the busy work".to_string(),
            description: "busy probe".to_string(),
            model: None,
            cancel: mirri_core::CancelToken::new(),
            run_in_background: false,
            parent_tool_call_id: None,
            swarm_index: None,
            swarm_item: None,
            suppress_rate_limit_failure_event: false,
            turn_origin: Default::default(),
        })
        .expect("spawn");
    // Drive the child turn on the runtime so it is genuinely mid-turn —
    // the host's completion future only progresses while polled.
    let child_completion =
        std::mem::replace(&mut handle.completion, Box::pin(std::future::pending()));
    let child_task = tokio::spawn(async move {
        let mut child_completion = child_completion;
        (&mut child_completion).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child's first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child_id = {
        let agents = session_a.list_agents().await;
        agents
            .iter()
            .find(|agent| agent.as_str() != super::DEFAULT_MAIN_AGENT_ID)
            .expect("the spawned child is a live session agent")
            .as_str()
            .to_string()
    };

    // A detached process task the child dispatched completes mid-turn.
    let tasks = session_a.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some(child_id.clone()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);

    // Then the busy path parks the notification: the `turn.steer` record
    // with the background_task origin lands on the CHILD journal, the task
    // stays UNMARKED (park no longer sets a durable marker — FIX 1; the
    // message not being in context means the debt is still whole), and the
    // model never sees the notification (no engine turn runs for it).
    let child_log_path = server_a
        .session_manager()
        .persistence()
        .agent_wire_log_path(&SessionId::new(&session_id), &child_id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let log = JsonlAppendLog::open(&child_log_path).await.unwrap();
        let ops = log.read_all().await.unwrap();
        if ops.iter().any(|op| {
            matches!(op, WireOp::TurnSteer { origin, .. } if origin["kind"] == "background_task")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the busy-path steer must land on the child journal"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !notification_reached_model(&mock),
        "the busy-parked notification must not reach the model before the restart"
    );
    assert_eq!(
        mock.chat_call_count(),
        2,
        "the main turn + the gated child turn, no delivery turn"
    );
    // The new contract (FIX 1, review M1): PARK no longer marks — the
    // steer is scheduled by the buffer/`parked` scan and, across a
    // restart, by the child-journal re-park; the durable marker is set
    // only once the message actually reaches context (the delivery turn
    // or the restore re-append). Let the tick settle, then assert the
    // task is still unmarked while the steer sits parked.
    drive_notification_ticks(&server_a, &session_id, 2).await;
    assert!(
        tasks
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_none(),
        "parking must not set the durable delivery marker (FIX 1)"
    );

    // End the child turn mid-flight: close the session while the child is
    // still gated, drop server A, then drop the gate (the pre-built reply
    // flows; the turn settles normally on the closed session).
    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);
    drop(release_child_a);
    // The turn must actually SETTLE before server B resumes: the mock is
    // shared across both servers, so a turn still running here would keep
    // calling it and surface later as a phantom duplicate delivery
    // (`chat_call_count` 3 -> 4). The old form discarded the timeout result
    // entirely, so that leftover read as a product bug instead of an
    // unmet precondition. The 60s bound only has to catch a hang, not to
    // enforce speed — a healthy settle is sub-second, and a slow runner
    // must not turn into a red here.
    match tokio::time::timeout(Duration::from_secs(60), child_task).await {
        Ok(Ok(_completion)) => {}
        Ok(Err(join_error)) => {
            panic!("the child turn panicked after the session closed: {join_error}")
        }
        Err(_) => panic!(
            "the child turn never settled after the session closed (60s). Until it \
             does, it keeps calling the mock shared with server B, and the delivery \
             count below would blame a duplicate that never happened"
        ),
    }

    // When server B resumes the session over the same tempdir
    let server_b = build_server();
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("resumed session is live");
    assert!(
        session_b
            .get_agent(&AgentId::new(&child_id))
            .await
            .is_some(),
        "the restored child is rehydrated from its journal"
    );

    // Then the re-parked notification wakes into EXACTLY ONE delivery turn
    // on the child — the restart preserved the parked content (the flush
    // marker below proves the wakeup consumed the re-parked steer) and
    // never duplicates it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the re-parked notification must deliver after the restart"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_agent_turn_idle(&server_b, &session_id, &child_id).await;
    assert_eq!(
        mock.chat_call_count(),
        3,
        "exactly one delivery turn beyond the pre-restart calls — no duplicate"
    );
    let all = recorded_all(&mock).to_string();
    assert_eq!(
        all.matches("<notification ").count(),
        1,
        "the notification XML reaches the model exactly once: {all}"
    );
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );

    // The wakeup's flush marker and the delivery's durable message ride
    // the CHILD journal — the addressee's own records survived intact.
    let child_log_path_b = server_b
        .session_manager()
        .persistence()
        .agent_wire_log_path(&SessionId::new(&session_id), &child_id);
    let log = JsonlAppendLog::open(&child_log_path_b).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        ops.iter()
            .any(|op| matches!(op, WireOp::TurnSteerFlushed { count } if *count >= 1)),
        "the wakeup must flush the re-parked steer with a marker on the child journal: {ops:?}"
    );
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
                    && message["origin"]["notificationId"] == format!("task:{task_id}:completed")
        )),
        "the delivery's durable message rides the child journal: {ops:?}"
    );
    assert!(
        session_b
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_some(),
        "the delivery marker survives the restart"
    );

    // And main never saw the subagent's notification: no background-task
    // steer in main's buffer, no background-task append on the session
    // journal.
    let main_b = session_b
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let main_model = {
        let config = main_b.config();
        let guard = config.read().unwrap_or_else(|e| e.into_inner());
        guard.model_id.clone()
    };
    let main_buffer =
        server_b.steer_buffer_for(&session_id, super::DEFAULT_MAIN_AGENT_ID, &main_model);
    {
        let buffer = main_buffer.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !buffer
                .iter()
                .any(|s| matches!(s.origin, mirri_core::PromptOrigin::BackgroundTask { .. })),
            "the subagent notification steer must never re-park into main's buffer"
        );
    }
    let main_log_path = server_b
        .session_manager()
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&main_log_path).await.unwrap();
    let main_ops = log.read_all().await.unwrap();
    assert!(
        !main_ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
        )),
        "the session journal must not carry the subagent notification delivery"
    );
}

/// Given a PRE-FIX session shape (the subagent notification steer recorded
/// on the MAIN journal — the routing that predates the child-journal
/// records), when it resumes, then the stale steer is NOT re-parked into
/// main's buffer: the main re-park loop skips background-task steers whose
/// owning task's addressee is a subagent, and the restore reconciliation
/// delivers the notification silently into the CHILD's context instead —
/// never a wrong-addressee main notification turn.
#[tokio::test(flavor = "multi_thread")]
async fn should_not_re_park_a_pre_fix_subagent_notification_steer_into_main_on_resume() {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let mock = Arc::new(ScriptedLlm::new());

    // Server A: NO LLM provider — the driver defers, so the pre-fix
    // notification debt (the main-journal steer + no delivery marker)
    // stays whole through the shutdown. A titled session with a durably
    // registered subagent (the pre-fix addressee) and a completed detached
    // task it dispatched — whose notification steer rides the MAIN journal
    // (the pre-fix routing).
    let build_server_a = || {
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
        let config = Arc::new(RwLock::new(parsed));
        EmbeddedServer::with_providers(manager, None, None, None, None, config, None)
        .with_home_dir(Some(tmp.path().to_path_buf()))
            .with_task_sweep_tick(Duration::from_millis(50))
    };
    let build_server_b = || {
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
        let config = Arc::new(RwLock::new(parsed));
        EmbeddedServer::with_providers(
            manager,
            Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
            None,
            None,
            None,
            config,
            None,
        )
        .with_home_dir(Some(tmp.path().to_path_buf()))
        .with_task_sweep_tick(Duration::from_millis(50))
    };

    // Server A: a titled session with a durably registered subagent (the
    // pre-fix addressee) and a completed detached task it dispatched —
    // whose notification steer rides the MAIN journal (the pre-fix shape).
    let server_a = build_server_a();
    let created = server_a
        .call_method("session.create", json!({ "title": "pre-fix steer" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let child_id = "agent-old-owner";
    session_a
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("owner agent");
    session_a
        .record_op(WireOp::SubagentRegistered {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(super::DEFAULT_MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    // The child's journal slot must exist for the resume to rehydrate it.
    session_a.child_wire_log(child_id).await;
    let tasks = session_a.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some(child_id.to_string()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);
    // The pre-fix routing: the notification steer (parked or pending) is a
    // MAIN-journal record, addressed to the subagent; no delivery marker.
    session_a
        .record_op(WireOp::TurnSteer {
            input: json!([{
                "type": "text",
                "text": format!("<notification id=\"task:{task_id}:completed\" />"),
            }]),
            origin: json!({
                "kind": "background_task",
                "taskId": task_id.clone(),
                "status": "completed",
                "notificationId": format!("task:{task_id}:completed"),
            }),
        })
        .await;

    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);

    // When server B resumes the session over the same tempdir
    let server_b = build_server_b();
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");
    drive_notification_ticks(&server_b, &session_id, 6).await;
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("resumed session is live");

    // Then the stale steer never re-parked into main's buffer,
    let main_b = session_b
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let main_model = {
        let config = main_b.config();
        let guard = config.read().unwrap_or_else(|e| e.into_inner());
        guard.model_id.clone()
    };
    let main_buffer =
        server_b.steer_buffer_for(&session_id, super::DEFAULT_MAIN_AGENT_ID, &main_model);
    {
        let buffer = main_buffer.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !buffer
                .iter()
                .any(|s| matches!(s.origin, mirri_core::PromptOrigin::BackgroundTask { .. })),
            "the pre-fix subagent steer must not re-park into main's buffer"
        );
    }
    // …no notification turn ran at all (server A ran LLM-free; server B's
    // reconcile is silent and the driver stays quiet)…
    assert_eq!(
        mock.chat_call_count(),
        0,
        "no notification turn may run for the pre-fix steer"
    );
    // …and the restore reconciliation re-appended the notification into
    // the SUBAGENT's rebuilt context, with the task durably marked. The
    // child is dormant after the lazy resume — waking it hydrates the
    // journal-rebuilt context the re-append landed in.
    session_b.hydrate_subagent(child_id).await;
    let child = session_b
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("restored child");
    let child_history = {
        let memory = child.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        child_history.contains("<notification "),
        "the pre-fix notification must reconcile into the CHILD's context: {child_history}"
    );
    let main_history = {
        let memory = main_b.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        !main_history.contains("<notification "),
        "main must never carry the subagent notification: {main_history}"
    );
    assert!(
        session_b
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_some(),
        "the reconcile marks the task durably delivered"
    );
}

/// Given the crash-window shape — a completed detached subagent-dispatched
/// task with NO delivery marker, whose child journal holds a
/// `turn.steer` (background_task origin) already consumed by a
/// `turn.steer_flushed` (the steer was flushed but the delivery turn's
/// records never landed: flushed, never delivered, unmarked) — when the
/// session resumes, then the notification self-heals via the restore
/// re-append: no LLM turn runs, exactly one background-task message lands
/// in the subagent's live context (recorded durably on the child journal),
/// and the persisted task record carries the delivery marker. The lock
/// pins FIX 1 (park no longer marks): the flushed-but-undelivered steer
/// must never leave the notification content lost forever.
#[tokio::test(flavor = "multi_thread")]
async fn should_silently_reappend_a_notification_whose_steer_was_flushed_but_never_delivered_on_resume()
 {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let mock = Arc::new(ScriptedLlm::new());

    // Server A: NO LLM provider — the driver defers, so the hand-shaped
    // crash-window state stays whole through the shutdown. A titled session
    // with a durably registered subagent (the addressee) and a completed
    // detached task it dispatched; the child journal then carries the
    // flushed-but-undelivered steer records.
    let build_server_a = || {
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
        let config = Arc::new(RwLock::new(parsed));
        EmbeddedServer::with_providers(manager, None, None, None, None, config, None)
        .with_home_dir(Some(tmp.path().to_path_buf()))
            .with_task_sweep_tick(Duration::from_millis(50))
    };
    let build_server_b = || {
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
        let config = Arc::new(RwLock::new(parsed));
        EmbeddedServer::with_providers(
            manager,
            Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
            None,
            None,
            None,
            config,
            None,
        )
        .with_home_dir(Some(tmp.path().to_path_buf()))
        .with_task_sweep_tick(Duration::from_millis(50))
    };

    let server_a = build_server_a();
    let created = server_a
        .call_method("session.create", json!({ "title": "flushed steer" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;
    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let child_id = "agent-flushed-owner";
    session_a
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("owner agent");
    session_a
        .record_op(WireOp::SubagentRegistered {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(super::DEFAULT_MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    // The child's journal slot must exist for the resume to rehydrate it.
    session_a.child_wire_log(child_id).await;
    let tasks = session_a.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some(child_id.to_string()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);
    // The crash-window state: the delivery steer was FLUSHED but the
    // delivery turn's prompt/append records never landed — the child
    // journal holds the `turn.steer` (background_task origin) consumed by
    // a `turn.steer_flushed` marker, and the task carries NO delivery
    // marker, so the notification is neither durably delivered nor parked.
    let child_log_a = session_a.child_wire_log(child_id).await;
    child_log_a
        .record_op(WireOp::TurnSteer {
            input: json!([{
                "type": "text",
                "text": format!("<notification id=\"task:{task_id}:completed\" />"),
            }]),
            origin: json!({
                "kind": "background_task",
                "taskId": task_id.clone(),
                "status": "completed",
                "notificationId": format!("task:{task_id}:completed"),
            }),
        })
        .await;
    child_log_a
        .record_op(WireOp::TurnSteerFlushed { count: 1 })
        .await;
    assert!(
        tasks
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_none(),
        "the task must start out undelivered (the crash-window premise)"
    );

    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);

    // When server B resumes the session over the same tempdir
    let server_b = build_server_b();
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");
    drive_notification_ticks(&server_b, &session_id, 6).await;
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("resumed session is live");

    // Then no LLM notification turn runs (the re-append is silent)…
    assert_eq!(
        mock.chat_call_count(),
        0,
        "no notification turn may run for the flushed steer"
    );
    // …exactly one background-task message lands in the SUBAGENT's live
    // context — the silent re-append…
    session_b.hydrate_subagent(child_id).await;
    let child = session_b
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("restored child");
    let child_notification_count = {
        let memory = child.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        memory
            .history()
            .iter()
            .filter(|m| {
                matches!(
                    &m.origin,
                    Some(mirri_core::PromptOrigin::BackgroundTask { .. })
                )
            })
            .count()
    };
    assert_eq!(
        child_notification_count, 1,
        "the flush-consumed steer must reconcile into exactly one child \
         context message"
    );
    // …the durable append record rides the child journal…
    let child_log_path_b = server_b
        .session_manager()
        .persistence()
        .agent_wire_log_path(&SessionId::new(&session_id), child_id);
    let child_log_b = JsonlAppendLog::open(&child_log_path_b).await.unwrap();
    let child_ops = child_log_b.read_all().await.unwrap();
    assert!(
        child_ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
                    && message["origin"]["notificationId"] == format!("task:{task_id}:completed")
        )),
        "the re-append's durable message rides the child journal: {child_ops:?}"
    );
    // …main never saw the subagent notification…
    let main_b = session_b
        .get_agent(&AgentId::new(super::DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("main agent");
    let main_history = {
        let memory = main_b.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", memory.history())
    };
    assert!(
        !main_history.contains("<notification "),
        "main must never carry the subagent notification: {main_history}"
    );
    // …and the persisted task json now carries the delivery marker.
    let task_json_path = server_b
        .session_manager()
        .persistence()
        .session_dir(&SessionId::new(&session_id))
        .join("tasks")
        .join(format!("{task_id}.json"));
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&task_json_path).expect("the task json must be persisted"),
    )
    .expect("the persisted task record must parse");
    assert!(
        persisted.get("notification_delivered_at").is_some(),
        "the persisted task record must carry the delivery marker: {persisted}"
    );
}

/// Given a terminal detached process task dispatched by a live subagent
/// whose notification never delivered (the server went down before any
/// delivery — the db-less server's driver defers, SDR-B2-01), when the
/// session resumes, then the restore reconciles the SUBAGENT's ledger too:
/// the notification message is re-appended silently into the child's
/// rebuilt context — no turn runs for it — the durable append rides the
/// child's OWN journal (not the session journal), and the persisted task
/// record carries the delivery marker so a later restart stays silent.
#[tokio::test(flavor = "multi_thread")]
async fn should_silently_reappend_undelivered_subagent_notifications_on_resume() {
    // Server A: no LLM provider — the delivery debt stays whole. The live
    // subagent is durably registered (parent = main) and owns a journal
    // slot of its own, exactly the shape the resume face restores children
    // from; the task it dispatched completed undelivered.
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
    let config = Arc::new(RwLock::new(parsed));
    let server_a = EmbeddedServer::with_providers(manager, None, None, None, None, config, None)
        .with_home_dir(Some(tmp.path().to_path_buf()));
    let created = server_a
        .call_method("session.create", json!({ "title": "resume child restore" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server_a, &session_id).await;

    let session_a = server_a
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let child_id = "agent-restore-owner";
    session_a
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("owner agent");
    // The durable registration the RESUME face restores children from (the
    // live create_agent alone is in-memory): with parent main, exactly the
    // dispatch shape the process arm attributes back to the dispatcher.
    session_a
        .record_op(WireOp::SubagentRegistered {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(super::DEFAULT_MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    // The child's journal slot must exist for the resume to rehydrate the
    // child (restore skips a registered child without a journal) — the
    // host spawn creates the slot before the child's first turn; mirror it.
    session_a.child_wire_log(child_id).await;
    let tasks = session_a.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("cargo test".to_string()),
                dispatcher: Some(child_id.to_string()),
            },
            "bash work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "bash result\n");
    tasks.complete(&task_id, 0);
    // The terminal task's notification is UNDELIVERED — the persisted
    // record carries no delivery marker.
    assert!(
        session_a
            .tasks()
            .get(&task_id)
            .unwrap()
            .notification_delivered_at
            .is_none(),
        "the task must start out undelivered"
    );
    server_a
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    drop(server_a);

    // When server B (with a provider and a fast tick) resumes the session
    let mock = Arc::new(ScriptedLlm::new());
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
    let config = Arc::new(RwLock::new(parsed));
    let server_b = EmbeddedServer::with_providers(
        manager,
        Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
        None,
        config,
        None,
    )
        .with_home_dir(Some(tmp.path().to_path_buf()))
    .with_task_sweep_tick(Duration::from_millis(50));
    server_b
        .call_method("session.resume", json!({ "sessionId": &session_id }))
        .await
        .expect("resume");

    // Then the subagent's notification is re-appended silently into its
    // rebuilt context — no turn ran for it (the mock saw no LLM call at
    // all).
    drive_notification_ticks(&server_b, &session_id, 6).await;
    assert_eq!(
        mock.chat_call_count(),
        0,
        "the restore appends silently — no notification turn"
    );
    let session_b = server_b
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    // The child is dormant after the lazy resume — waking it hydrates the
    // journal-rebuilt context the re-append landed in.
    session_b.hydrate_subagent(child_id).await;
    let child = session_b
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("the child rehydrates from its durable registration");
    let background_count: usize = {
        // The guard's scope ends before any await below — a memory guard
        // held across an await deadlocks the single-threaded test runtime
        // against the notification driver's ledger scan.
        let memory = child.context_memory();
        let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
        memory
            .history()
            .iter()
            .filter_map(|m| m.origin.as_ref())
            .filter(|o| {
                matches!(
                    o,
                    mirri_core::PromptOrigin::BackgroundTask { task_id: t, .. }
                        if t.as_str() == task_id.as_str()
                )
            })
            .count()
    };
    assert_eq!(
        background_count, 1,
        "exactly one re-append lands in the SUBAGENT's rebuilt context"
    );

    // And the routing contract: the restore's durable append rides the
    // child's OWN journal — the records a later restart's rehydrated
    // context is rebuilt from — not the session journal.
    let child_log_path = server_b
        .session_manager()
        .persistence()
        .agent_wire_log_path(&SessionId::new(&session_id), child_id);
    let log = JsonlAppendLog::open(&child_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
                    && message["origin"]["taskId"] == task_id.as_str()
        )),
        "the subagent notification's durable append must ride the addressee journal"
    );
    let session_log_path = server_b
        .session_manager()
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    let log = JsonlAppendLog::open(&session_log_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    assert!(
        !ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "background_task"
                    && message["origin"]["taskId"] == task_id.as_str()
        )),
        "the subagent notification's durable append must NOT ride the session journal"
    );

    // And the persisted task record now carries the delivery marker — the
    // durable half of the ledger, so a later restart stays silent even
    // without scanning the child's context.
    let task_json_path = server_b
        .session_manager()
        .persistence()
        .session_dir(&SessionId::new(&session_id))
        .join("tasks")
        .join(format!("{task_id}.json"));
    let task_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&task_json_path).expect("task json must be persisted"),
    )
    .expect("task json parses");
    assert!(
        task_json["notification_delivered_at"].is_string(),
        "the persisted task record must carry the delivery marker: {task_json}"
    );
}

/// Given a completed detached task whose notification turn is stuck in a
/// cancel-aware blocked LLM call, when `message.abort` targets the
/// session, then the notification turn is actually cancelled — it
/// settles with the aborted stop reason instead of running to
/// completion, and the session stops reporting busy.
#[tokio::test]
async fn should_cancel_the_notification_turn_when_message_abort_targets_the_session() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let _task_id = completed_background_task(&session, &session_id);

    // The notification turn starts and blocks inside the LLM call.
    let turn_started =
        next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;
    assert!(
        turn_started.params["turnId"]
            .as_str()
            .is_some_and(|id| id.starts_with("turn-")),
        "the notification turn carries the turn-<uuid> bracket id"
    );

    // When the user aborts the session's in-flight turn.
    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .expect("abort");
    assert_eq!(aborted["ok"], true);

    // Then the notification turn settles as cancelled — not running to
    // completion — and the turn.ended bracket names the aborted reason.
    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(
        ended.params["turnId"], turn_started.params["turnId"],
        "the cancelled turn is the notification turn"
    );

    // And the session stops reporting busy once the turn settles.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let listed = server.call_method("session.list", json!({})).await.unwrap();
        let view = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == json!(session_id))
            .expect("the session is listed");
        if view["busy"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session must stop reporting busy after the abort"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Given a paused goal and a completed detached task whose notification
/// would be due, when ticks pass, then no notification turn runs — the
/// delivery is held while the goal is paused; after `goal.resume`, the
/// notification is delivered on the next tick.
#[tokio::test]
async fn should_hold_the_notification_delivery_while_the_goal_is_paused() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("notification handled");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .goal()
        .create("Do the thing".to_string(), None)
        .unwrap();
    // Pause through the `goal.pause` RPC — the same handler the web calls,
    // so this also covers the handler's freeze path.
    let paused = server
        .call_method("goal.pause", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(paused["status"], "paused");
    let _task_id = completed_background_task(&session, &session_id);

    // Several ticks pass while paused — no notification turn runs.
    drive_notification_ticks(&server, &session_id, 6).await;
    assert!(
        !notification_reached_model(&mock),
        "no notification turn may run while the goal is paused"
    );

    // When the goal resumes, the held notification is delivered.
    let resumed = server
        .call_method("goal.resume", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(resumed["status"], "active");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held notification must be delivered after the goal resumes"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a session whose main agent's steer buffer holds a parked
/// notification (busy-parked and stranded — the state a resume re-parks
/// or a bridge-level turn timeout leaves behind) and whose goal is paused
/// via the `goal.pause` RPC, when several task-sweep ticks pass, then the
/// stranded notification is NOT flushed into a turn — the paused gate
/// returns before the stranded-buffer peek, so the parked steer stays
/// parked; after `goal.resume`, the held notification is flushed on the
/// next ticks (the model eventually sees the notification XML).
#[tokio::test]
async fn should_hold_the_stranded_notification_flush_while_the_goal_is_paused() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    mock.push_text("wakeup ack");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    converse_once(&server, &session_id).await;

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .goal()
        .create("Do the thing".to_string(), None)
        .unwrap();
    // Pause through the `goal.pause` RPC — the same handler the web calls.
    let paused = server
        .call_method("goal.pause", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(paused["status"], "paused");

    // Given the stranded state: a completed background task whose
    // notification never delivered — its busy-parked steer sits in the
    // buffer, pushed while the goal is already paused.
    let task_id = completed_background_task(&session, &session_id);
    let model_id = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .model_id
        .clone();
    let buffer = server.steer_buffer_for(&session_id, "main", &model_id);
    let xml = format!(
        "<notification id=\"task:{task_id}:completed\" category=\"task\" \
         type=\"task.completed\" source_kind=\"background_task\" \
         source_id=\"{task_id}\">\nTitle: Background process completed\n\
         Severity: info\necho hi completed.\n</notification>"
    );
    buffer.lock().unwrap_or_else(|e| e.into_inner()).push(
        vec![mirri_core::ContentPart::Text { text: xml }],
        mirri_core::PromptOrigin::BackgroundTask {
            task_id: task_id.clone(),
            status: "completed".into(),
            notification_id: format!("task:{task_id}:completed"),
        },
    );

    // When several ticks pass while paused — no wakeup turn flushes the
    // stranded steer.
    drive_notification_ticks(&server, &session_id, 6).await;
    assert!(
        !notification_reached_model(&mock),
        "no notification turn may run while the goal is paused"
    );

    // When the goal resumes, the held (parked) notification is flushed.
    let resumed = server
        .call_method("goal.resume", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(resumed["status"], "active");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held stranded notification must be flushed after the goal resumes"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a completed detached task whose notification turn is running and
/// stuck in a cancel-aware blocked LLM call, when `goal.pause` is called
/// (a goal exists first — created through the session's goal manager so
/// the pause handler sees one, either before or after the turn starts),
/// then the notification turn settles with `event.turn.ended` reason
/// `"cancelled"` — the pause froze the machine end to end: the
/// `goal.pause` handler → `cancel_in_flight_turns` → the notification
/// turn's Wait entry observes the cooperative token → the driver's inline
/// settle closes the bracket itself (self-settle). The seam pinned here is
/// the pause WAIT-arm composition; the fire-arm cause-naming
/// (`"turn aborted by the user via goal.pause"` on `error.message`) is
/// pinned separately by
/// `should_cancel_the_in_flight_turn_when_goal_pause_is_called` (a
/// wait-mode turn closes its own bracket with `TurnCompleted` and — like
/// every wait arm — carries no `error` object, reason `"cancelled"` being
/// the `Aborted` wire spelling).
#[tokio::test]
async fn should_cancel_the_notification_turn_when_goal_pause_is_called() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // Given a goal (the pause handler cancels only when one exists) and a
    // completed detached task whose notification turn starts and blocks
    // inside the cancel-aware LLM call.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .goal()
        .create("Do the thing".to_string(), None)
        .unwrap();
    let _task_id = completed_background_task(&session, &session_id);
    let turn_started =
        next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;

    // When `goal.pause` is called.
    let paused = server
        .call_method("goal.pause", json!({ "session_id": &session_id }))
        .await
        .expect("pause");
    assert_eq!(paused["status"], "paused");

    // Then the notification turn settles as cancelled — not running to
    // completion — and the turn.ended bracket names that same turn.
    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(
        ended.params["turnId"], turn_started.params["turnId"],
        "the cancelled turn is the notification turn"
    );
}

/// Given a completed detached task whose notification turn is running and
/// stuck in a cancel-aware blocked LLM call, when `goal.cancel` is called
/// (cancel is unconditional — it clears and freezes even when no goal is
/// present; this test creates one so the freeze leg's clear is observable),
/// then the notification turn settles with `event.turn.ended` reason
/// `"cancelled"` and the goal is cleared afterwards (`goal.get` reports
/// `null`). The seam pinned is the cancel composition — the handler → the
/// Wait entry's cooperative token → the driver's inline self-settle — AND
/// the clear half of "cancel", end to end. As with every wait arm, the
/// close is a `TurnCompleted` (reason `"cancelled"`, the `Aborted` wire
/// spelling) with no `error` object; the fire-arm cause-naming
/// (`"via goal.cancel"`) is the aborting side's `TurnFailed` shape, out of
/// scope for a wait-mode turn.
#[tokio::test]
async fn should_cancel_the_notification_turn_when_goal_cancel_is_called() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // Given a goal and a completed detached task whose notification turn
    // starts and blocks inside the cancel-aware LLM call.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .goal()
        .create("Do the thing".to_string(), None)
        .unwrap();
    let _task_id = completed_background_task(&session, &session_id);
    let turn_started =
        next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;

    // When `goal.cancel` is called.
    let cancelled = server
        .call_method("goal.cancel", json!({ "session_id": &session_id }))
        .await
        .expect("cancel");
    assert_eq!(cancelled["ok"], true);

    // Then the notification turn settles as cancelled — the very turn the
    // ticket started — and the goal is cleared afterwards.
    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(
        ended.params["turnId"], turn_started.params["turnId"],
        "the cancelled turn is the notification turn"
    );
    let got = server
        .call_method("goal.get", json!({ "session_id": &session_id }))
        .await
        .expect("goal.get");
    assert!(
        got["goal"].is_null(),
        "goal.cancel must clear the goal, got: {got}"
    );
}

/// Given an active goal and a completed detached task whose notification
/// turn is stuck in a cancel-aware blocked LLM call, when `message.abort`
/// cancels that turn, then the goal parks itself as paused (resumable) —
/// an interrupted turn must stop the goal loop, not hand it to the next
/// stranded notification — so a second completed task's notification is
/// held while the goal stays paused and is delivered again after
/// `goal.resume`.
#[tokio::test]
async fn should_pause_the_goal_when_message_abort_cancels_the_notification_turn() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // Given an active goal and a completed detached task whose
    // notification turn starts and blocks inside the cancel-aware LLM
    // call.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .goal()
        .create("Do the thing".to_string(), None)
        .unwrap();
    let _first_task = completed_background_task(&session, &session_id);
    let turn_started =
        next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;

    // When `message.abort` cancels the blocked notification turn.
    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .expect("abort");
    assert_eq!(aborted["ok"], true);

    // Then the notification turn settles as cancelled — the very turn the
    // abort stopped — and the goal parks as paused with the interruption
    // reason (not left active for the next driver tick to restart). Either
    // arrival order of the two events is accepted; the goal.updated is
    // captured if it lands while waiting for the ending.
    let mut goal_updated: Option<mirri_transport::frame::JsonRpcNotification> = None;
    let ended = next_event_where(
        &mut events,
        |n| n.method == "event.turn.ended",
        |n| {
            if n.method == "event.goal.updated" {
                goal_updated = Some(n.clone());
            }
        },
    )
    .await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(
        ended.params["turnId"], turn_started.params["turnId"],
        "the cancelled turn is the notification turn"
    );
    let goal_updated = match goal_updated {
        Some(event) => event,
        None => next_event_where(&mut events, |n| n.method == "event.goal.updated", |_| {}).await,
    };
    assert_eq!(goal_updated.params["snapshot"]["status"], "paused");
    assert_eq!(goal_updated.params["change"]["kind"], "lifecycle");
    assert_eq!(
        goal_updated.params["change"]["reason"],
        "Paused after interruption"
    );
    let goal = session.goal().get().expect("the goal survives the pause");
    assert_eq!(goal.status, mirri_core::GoalStatus::Paused);
    assert_eq!(
        goal.terminal_reason,
        Some("Paused after interruption".to_string())
    );

    // And across several driver ticks a second stranded notification does
    // NOT restart the loop — no new turn runs while the goal is parked.
    let _second_task = completed_background_task(&session, &session_id);
    drive_notification_ticks(&server, &session_id, 6).await;
    while let Ok(notification) = events.try_recv() {
        assert_ne!(
            notification.method, "event.turn.started",
            "no new turn may start while the goal is parked: {notification:?}"
        );
    }

    // And after `goal.resume` the held notification is delivered again as
    // a fresh turn (not the aborted one).
    let resumed = server
        .call_method("goal.resume", json!({ "session_id": &session_id }))
        .await
        .expect("resume");
    assert_eq!(resumed["status"], "active");
    let redelivered =
        next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;
    assert_ne!(
        redelivered.params["turnId"], turn_started.params["turnId"],
        "the resumed turn is the held second notification, not the aborted one"
    );

    // Teardown: settle the resumed (blocked) turn so the test ends clean.
    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .expect("abort");
    assert_eq!(aborted["ok"], true);
    let _ = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
}

// ── A12: engine-internal turns' model semantics (spec 11 R11) ────────

/// Given a session running "first-model" whose main agent switches to
/// "second-model" BEFORE a cron task fires, when the cron driver injects
/// the fired prompt as its engine turn, then the turn's LLM construction
/// sees the session's CURRENT model — "second-model" — not the create-time
/// "first-model": a cron turn is an engine-internal turn with main-agent
/// semantics, resolved per-request from the target agent's live scope
/// config (spec 11 R11/A12).
#[tokio::test]
async fn should_run_cron_turn_with_session_current_model_at_trigger() {
    // Given a two-model server with a recording factory and a fast cron
    // tick, and a session whose first turn settled on "first-model"
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("first reply");
    mock.push_text("cron model reply");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (server, _tmp, _config_path) =
        test_server_with_two_models_and_factory(Arc::new(RecordingLlmFactory {
            llm: Arc::clone(&mock),
            seen: Arc::clone(&seen),
        }));
    let server = server.with_cron_tick(Duration::from_millis(50));
    let created = server
        .call_method(
            "session.create",
            json!({ "title": "cron-model", "modelId": "first-model" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hello", "mode": "wait" }),
        )
        .await
        .expect("the first turn settles");
    assert_eq!(
        *seen.lock().unwrap_or_else(|e| e.into_inner()),
        vec!["first-model".to_string()],
        "the first turn must run on the create-time model"
    );

    // And the main agent switches to "second-model" BEFORE the trigger —
    // the live scope config the cron turn must resolve from
    server
        .call_method(
            "agent.setModel",
            json!({ "sessionId": &session_id, "model": "second-model" }),
        )
        .await
        .expect("setModel");

    // And an overdue cron task fires on the next driver tick
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    seed_overdue_cron_task(&session, &session_id, "check the schedule");

    // Then the injected cron turn fed the model (the envelope is the
    // proof the second turn is the cron turn, not a stray dispatch)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if recorded_all(&mock)
            .to_string()
            .contains("check the schedule")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cron turn never fed the model: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // And the cron turn's LLM construction resolved the session's current
    // model — the switch landed before the fire, so the injection must
    // see "second-model", never the create-time "first-model"
    let settled = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        settled,
        vec!["first-model".to_string(), "second-model".to_string()],
        "the cron turn must follow the session's current model at trigger (R11/A12)"
    );
}

/// Given a nested-dispatch task whose notification addressee is a subagent
/// pinned to "first-model", when the task driver delivers the notification
/// turn AFTER the main agent switched to "second-model", then the turn's
/// LLM construction sees the ADDRESSEE's pinned "first-model" — the
/// subagent's own scope config, not the main agent's live model (spec 11
/// R11/A12: a subagent-addressed internal turn keeps the pin).
#[tokio::test]
async fn should_use_pinned_model_for_notification_turn_addressed_to_subagent() {
    // Given a two-model server with a recording factory and a fast sweep
    // tick, and a session whose first turn settled on "first-model"
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("first reply");
    mock.push_text("the child saw the notification");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (server, _tmp, _config_path) =
        test_server_with_two_models_and_factory(Arc::new(RecordingLlmFactory {
            llm: Arc::clone(&mock),
            seen: Arc::clone(&seen),
        }));
    let server = server.with_task_sweep_tick(Duration::from_millis(50));
    let created = server
        .call_method(
            "session.create",
            json!({ "title": "pinned-model", "modelId": "first-model" }),
        )
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hello", "mode": "wait" }),
        )
        .await
        .expect("the first turn settles");
    assert_eq!(
        *seen.lock().unwrap_or_else(|e| e.into_inner()),
        vec!["first-model".to_string()],
        "the first turn must run on the create-time model"
    );

    // And the main agent switches to "second-model" — the live model the
    // notification turn must NOT follow
    server
        .call_method(
            "agent.setModel",
            json!({ "sessionId": &session_id, "model": "second-model" }),
        )
        .await
        .expect("setModel");

    // And a dispatcher subagent pinned to "first-model" whose spawned
    // grandchild (the task's SUBJECT — the `TaskKind::Agent` agent id)
    // completed a background task: the B4 T9 nested-dispatch shape whose
    // notification addressee is the dispatcher, not main
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let child_id = "agent-pinned-1";
    let subject_id = "agent-pinned-grandchild-1";
    session
        .create_agent(
            AgentId::new(child_id),
            mirri_core::AgentScopeConfig {
                profile_name: Some("coder".to_string()),
                model_id: "first-model".to_string(),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("child agent");
    session
        .record_op(mirri_proto::WireOp::SubagentRegistered {
            agent_id: subject_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(child_id.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        })
        .await;
    let tasks = session.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Agent {
                agent_id: Some(subject_id.to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "pinned work".into(),
            session_id.clone(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "pinned result\n");
    tasks.complete(&task_id, 0);

    // Then within a few ticks the notification turn ran — and it fed the
    // model exactly once beyond the settled first turn (the child's turn,
    // never a main turn)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if mock.chat_call_count() >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the notification turn never fed the model"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_agent_turn_idle(&server, &session_id, child_id).await;
    assert_eq!(
        mock.chat_call_count(),
        2,
        "exactly the child's notification turn runs — main stays untouched"
    );
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains(&format!("source_id=\\\"{task_id}\\\"")),
        "{all}"
    );

    // And the notification turn's LLM construction saw the child's pinned
    // "first-model" — never the main agent's "second-model", even though
    // main had just switched (the pin, not the session's live model)
    let settled = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        settled,
        vec!["first-model".to_string(), "first-model".to_string()],
        "the subagent-addressed notification turn must keep the addressee's pinned model (R11/A12)"
    );
    assert!(
        !settled.contains(&"second-model".to_string()),
        "the main agent's live model must not leak into the subagent's notification turn: {settled:?}"
    );
}

// ---------------------------------------------------------------------------
// MIRRICODE_DISABLE_CRON (ADR-006 / E.4): the scheduler-halt and honest-list
// layers, exercised through the embedded server's real wire-log resume path.
// The creation-refusal layer lives with the tool's unit tests
// (mirri-core tools/cron_create.rs); `cron.trigger` (manual run-now) stays
// outside the switch by design — E.4 covers creation, scheduling, and
// listing only.
// ---------------------------------------------------------------------------

/// Given a server started with cron scheduling disabled and a resumed
/// session holding an overdue task, when several driver ticks pass, then
/// the task never fires — no injection reaches the model and no
/// `cron.fired` event is published (the driver never enters its check
/// loop).
#[tokio::test]
async fn should_not_fire_overdue_tasks_when_scheduling_is_disabled() {
    let mock = Arc::new(ScriptedLlm::new());
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let server = server.with_cron_scheduling_disabled(true);
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    // Materialize the session (SDR-004 lazy physical creation) so the
    // close→append→resume flow below has a wire log to extend.
    converse_once(&server, &session_id).await;
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    // Craft an overdue task record straight onto the wire log (the F16
    // DTO shape: camelCase, epoch millis, tags.sessionId) — the same
    // hand-written-overdue-task pattern the tests.rs cron suite uses.
    {
        let manager = server.session_manager();
        let wire_log_path = manager
            .persistence()
            .wire_log_path(&mirri_core::SessionId::new(&session_id));
        let mut log = JsonlAppendLog::open_for_append(&wire_log_path)
            .await
            .expect("open wire log");
        let overdue_ms = (chrono::Utc::now() - chrono::Duration::minutes(16)).timestamp_millis();
        log.append(&WireOp::CronAdd {
            task: json!({
                "id": "cron-overdue-disabled",
                "cron": "*/5 * * * *",
                "prompt": "overdue tick",
                "recurring": true,
                "createdAt": overdue_ms,
                "lastFiredAt": overdue_ms,
                "tags": { "sessionId": &session_id },
            }),
        })
        .await
        .expect("append cron.add");
    }

    // Resume spawns the cron driver — which must return before entering
    // the check loop (ADR-006 layer 2).
    server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .expect("resume");
    let mut events = server.subscribe_events().await.expect("subscribe");

    // Then over many sub-second ticks the overdue task never fires: no
    // model injection, no cron.fired event on the wire.
    let chat_calls_at_start = mock.chat_call_count();
    for _ in 0..10 {
        let event = tokio::time::timeout(Duration::from_millis(50), events.recv()).await;
        if let Ok(notification) = event {
            let notification = notification.expect("event channel stays open");
            assert_ne!(
                notification.method, "event.cron.fired",
                "no cron.fired event while scheduling is disabled"
            );
        }
    }
    assert_eq!(
        mock.chat_call_count() - chat_calls_at_start,
        0,
        "no injection reaches the model while scheduling is disabled"
    );
    // The task itself is untouched — still listed, still overdue.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    assert!(
        session.cron().get("cron-overdue-disabled").is_some(),
        "the disabled driver leaves existing tasks alone (no pruning)"
    );
}

/// Given a server started with cron scheduling disabled and a session
/// with tasks, when `cron.list` is called, then the list is served and
/// reports `disabled: true` — the honest-list layer (E.4).
#[tokio::test]
async fn should_report_the_disabled_state_when_listing_cron_tasks_while_scheduling_is_disabled() {
    let (server, _tmp) = b2_server(Arc::new(ScriptedLlm::new()), Duration::from_millis(50));
    let server = server.with_cron_scheduling_disabled(true);
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
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
        .call_method("cron.list", json!({ "sessionId": &session_id }))
        .await
        .expect("cron.list");

    // The read face stays available and tells the truth about why nothing
    // will fire.
    assert_eq!(listed["disabled"], true);
    let tasks = listed["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1, "the existing task is still listed");
    // Sanity: the enabled default answers false on the same server shape.
    let (enabled_server, _tmp2) =
        b2_server(Arc::new(ScriptedLlm::new()), Duration::from_millis(50));
    let enabled_created = enabled_server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let enabled_id = enabled_created["id"].as_str().unwrap().to_string();
    let enabled_listed = enabled_server
        .call_method("cron.list", json!({ "sessionId": &enabled_id }))
        .await
        .expect("cron.list");
    assert_eq!(enabled_listed["disabled"], false);
}

// ===========================================================================
// Batch 2 — the unified server-level cron scheduler (C.1 / V10) and the
// held-fire persistence (C.4)
// ===========================================================================

/// Seed an overdue one-shot cron task directly into a live session's cron
/// manager (the white-box driver of the scheduler tests: the next
/// scheduler tick must pick it up).
async fn seed_overdue_one_shot(
    server: &EmbeddedServer,
    session_id: &str,
    task_id: &str,
    prompt: &str,
) {
    let session = server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
        session_id: session_id.to_string(),
        recurring: false,
        created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
        ..mirri_core::cron::cron_task_base(task_id, "* * * * *", prompt)
    }]);
    session
        .cron()
        .set_next_fire_at(task_id, Some(chrono::Utc::now() - chrono::Duration::minutes(1)));
}

/// Given two sessions under ONE server sharing one scheduler tick, when
/// both carry an overdue task, then BOTH receive their injection — the
/// server-level scheduler drives every live session, not one driver per
/// session (C.1 / V10).
#[tokio::test]
async fn should_inject_both_sessions_when_two_sessions_share_one_scheduler_tick() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("reply one");
    mock.push_text("reply two");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let first = server
        .call_method("session.create", json!({ "title": "first" }))
        .await
        .expect("create first");
    let second = server
        .call_method("session.create", json!({ "title": "second" }))
        .await
        .expect("create second");
    let first_id = first["id"].as_str().unwrap().to_string();
    let second_id = second["id"].as_str().unwrap().to_string();

    seed_overdue_one_shot(&server, &first_id, "cron-first", "first session tick").await;
    seed_overdue_one_shot(&server, &second_id, "cron-second", "second session tick").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let fed = recorded_all(&mock).to_string();
        if fed.contains("first session tick") && fed.contains("second session tick") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "both sessions must be injected by the shared scheduler: {fed}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given two sessions under one server, when one closes while the other
/// carries an overdue task, then the surviving session's task still fires —
/// a session closing never stops the others' scheduling (V10).
#[tokio::test]
async fn should_keep_scheduling_the_other_session_when_one_session_closes() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("survivor reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let closing = server
        .call_method("session.create", json!({ "title": "closing" }))
        .await
        .expect("create closing");
    let survivor = server
        .call_method("session.create", json!({ "title": "survivor" }))
        .await
        .expect("create survivor");
    let closing_id = closing["id"].as_str().unwrap().to_string();
    let survivor_id = survivor["id"].as_str().unwrap().to_string();

    seed_overdue_one_shot(&server, &survivor_id, "cron-survivor", "survivor tick").await;

    server
        .call_method("session.close", json!({ "session_id": &closing_id }))
        .await
        .expect("close");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if recorded_all(&mock).to_string().contains("survivor tick") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the surviving session must keep being scheduled: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a BUSY session, when its cron task is manually triggered
/// (run-now), then the fire is HELD — no concurrent injection while the
/// turn runs (C.3 / SDR-004) — and after the busy turn settles the held
/// fire is injected by the scheduler's idle drain, while the task's
/// schedule is untouched (run-now never moves the cursor).
#[tokio::test]
async fn should_hold_a_manual_trigger_while_the_session_is_busy_and_deliver_on_idle() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("busy reply");
    mock.push_text("delivered fire reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "trigger-gate" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // A running (busy) turn on the session's main agent
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hold the turn", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(tokio::time::Instant::now() < deadline, "the busy turn must start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // And a recurring cron task, manually triggered while the turn runs
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
            session_id: session_id.clone(),
            recurring: true,
            created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            scope: mirri_core::cron::TaskScope::Session,
            ..mirri_core::cron::cron_task_base("cron-gate", "*/5 * * * *", "gated manual fire")
        }]);
    // The natural schedule sits far in the future — only the manual
    // trigger can deliver this fire.
    session
        .cron()
        .set_next_fire_at("cron-gate", Some(chrono::Utc::now() + chrono::Duration::days(1)));
    drop(session);

    let triggered = server
        .call_method(
            "cron.trigger",
            json!({ "session_id": &session_id, "task_id": "cron-gate" }),
        )
        .await
        .expect("trigger");
    assert_eq!(triggered["triggered"], true, "the trigger is accepted");

    // Then while the gate holds, no second turn runs (C.3)
    wait_for_held_fire(&server, &session_id, "cron-gate").await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the manual trigger must not inject while the turn runs: {}",
        recorded_all(&mock)
    );

    // When the busy turn settles
    release.send(()).expect("the gate receiver is alive");

    // Then the held fire is delivered on the first idle tick
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if recorded_all(&mock).to_string().contains("gated manual fire") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held manual fire must be delivered after the busy turn: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // And the schedule is untouched: the task never fired naturally
    // (tomorrow's next_fire_at unchanged — run-now does not move the
    // cursor), and it is still registered.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task = session.cron().get("cron-gate").expect("task survives");
    assert!(
        task.next_fire_at.unwrap() > chrono::Utc::now() + chrono::Duration::hours(23),
        "run-now leaves the schedule untouched: {:?}",
        task.next_fire_at
    );
    assert_eq!(task.fire_count, 0, "run-now never counts (D-M7)");
    // The clear lands right after the injected turn settles — the same
    // scheduler iteration, but the assertion must tolerate the settle's
    // async tail (poll briefly instead of asserting instantaneously).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !session.cron().held_fires().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delivered fire cleared its hold"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a busy session whose task fires while busy, when the session
/// CLOSES with the fire still held and is then RESUMED, then the held fire
/// is redelivered after the reopen (C.4 — the envelope is visible to the
/// model; "closed means lost" is retired).
#[tokio::test]
async fn should_redeliver_a_held_fire_when_the_session_closes_and_reopens() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("busy reply");
    mock.push_text("redelivered fire reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held-close" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // A running (busy) turn — the fire below comes due while it runs
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hold the turn", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(tokio::time::Instant::now() < deadline, "the busy turn must start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A one-shot task coming due mid-turn: the scheduler holds its fire
    // (write-before-inject), the turn keeps running.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
        session_id: session_id.clone(),
        recurring: false,
        // Backdated enough that the close→reopen redelivery lands past
        // the C.2a lateness threshold — the envelope must carry the mark.
        created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
        ..mirri_core::cron::cron_task_base("cron-held", "* * * * *", "held across the close")
    }]);
    session
        .cron()
        .set_next_fire_at("cron-held", Some(chrono::Utc::now() - chrono::Duration::minutes(2)));
    drop(session);
    // Let the scheduler tick: the fire is detected and held (never
    // injected while the gate holds).
    wait_for_held_fire(&server, &session_id, "cron-held").await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the fire must be held while the turn runs"
    );

    // When the session CLOSES with the fire still held (the busy turn is
    // still gated — close does not wait for it here; the close path
    // settles in-flight turns itself)
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    // Release the gated turn so nothing dangles.
    release.send(()).expect("the gate receiver is alive");
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || (!server.session_is_busy(&session_id)).then_some(()),
    )
    .await
    .expect("the released turn must settle");

    // And the session is RESUMED
    server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .expect("resume");

    // Then the held fire is redelivered once the (now idle) session is
    // drained — the envelope is visible to the model, carrying the C.2a
    // lateness mark (the fire was scheduled two minutes before it landed).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let redelivered = loop {
        let found = {
            let recorded = mock
                .recorded_messages
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            recorded
                .iter()
                .map(|messages| messages.to_string())
                .find(|text| text.contains("held across the close") && text.contains("<cron-fire "))
        };
        if let Some(text) = found {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held fire must be redelivered after the reopen: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        redelivered.contains("lateByMs=\\\""),
        "the redelivered fire carries the lateness mark: {redelivered}"
    );
}

/// Given a session whose held fire's task is DELETED (the real
/// session-deletion cascade through the `retention.delete` RPC — the
/// unified delete executor), when the scheduler next drains, then no
/// injection happens and the session itself is gone from the live table —
/// the held fire dies with its session (#177 held-side semantics, the V4
/// cascade criterion: the deletion path, not a bare task remove).
#[tokio::test]
async fn should_cascade_clear_held_fires_when_the_session_is_deleted() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("materialize reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held-delete" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    // Materialize the session (SDR-004): the retention delete needs the
    // session directory on disk, and this is the only LLM call that may
    // EVER happen (the doomed fire must never reach the model).
    converse_once(&server, &session_id).await;

    // A held fire whose task exists (a recurring task, so the entry sits
    // on the live task table — the session deletion takes the task, the
    // hold, and the wire log with it)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
            session_id: session_id.clone(),
            recurring: true,
            created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            scope: mirri_core::cron::TaskScope::Session,
            ..mirri_core::cron::cron_task_base("cron-doomed", "0 0 1 1 *", "never naturally")
        }]);
    session
        .cron()
        .hold_fire(mirri_core::cron::CronFire {
            task_id: "cron-doomed".to_string(),
            prompt: "doomed held fire".to_string(),
            cron: "0 0 1 1 *".to_string(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly: false,
            scheduled_at: None,
            late_by_ms: None,
            scope: mirri_core::cron::TaskScope::Session,
        ended: false,
        })
        .await;
    drop(session);

    // When the SESSION is deleted through the real retention delete RPC
    // (the unified executor: directory, index row, the works)
    let deleted = server
        .call_method("retention.delete", json!({ "session_id": &session_id }))
        .await
        .expect("retention delete");
    assert_eq!(deleted["ok"], true, "the delete succeeded: {deleted}");

    // Then the session's on-disk directory is gone — the held fire, its
    // task, and the whole wire log left with it (the V4 cascade criterion:
    // the deletion path took the session, not a bare task remove).
    let manager = server.session_manager();
    let session_dir = manager.persistence().session_dir(&SessionId::new(&session_id));
    assert!(
        !session_dir.is_dir(),
        "the deleted session's directory must be gone: {}",
        session_dir.display()
    );

    // And across several scheduler ticks, NO injection happens (the only
    // LLM call ever made was the materializing turn).
    // Two SESSION-scoped cron passes. The session was deleted, so each pass
    // returns early at the session resolve — which is exactly what this
    // pins: a deleted session no longer reaches the due check.
    drive_session_cron_pass(&server, &session_id).await;
    drive_session_cron_pass(&server, &session_id).await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the held fire must not inject after its session was deleted: {}",
        recorded_all(&mock)
    );
}

/// Given a busy session with two due periods of one task, when both fires
/// are held, then after the busy window the drain delivers ONE merged
/// injection with the counts accumulated (C.4's coalesce-on-hold).
#[tokio::test]
async fn should_merge_a_new_due_fire_into_the_held_one_while_busy() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("busy reply");
    mock.push_text("merged fire reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held-merge" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // A running (busy) turn
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hold the turn", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(tokio::time::Instant::now() < deadline, "the busy turn must start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A recurring task with two consecutive due periods (the first fire is
    // held; the rescheduled second comes due on the next tick and merges)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
            session_id: session_id.clone(),
            recurring: true,
            created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            scope: mirri_core::cron::TaskScope::Session,
            ..mirri_core::cron::cron_task_base("cron-merge", "* * * * *", "merge me")
        }]);
    session
        .cron()
        .set_next_fire_at("cron-merge", Some(chrono::Utc::now()));
    drop(session);
    // The first due pass holds the fire (still busy).
    wait_for_held_fire(&server, &session_id, "cron-merge").await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the first fire is held while the turn runs"
    );

    // Force a second due period (the white-box cursor reset — the next
    // scheduler tick fires and merges into the held one).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .cron()
        .set_next_fire_at("cron-merge", Some(chrono::Utc::now()));
    drop(session);

    // When the busy turn settles
    release.send(()).expect("the gate receiver is alive");

    // Then ONE merged injection is delivered, carrying both periods
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let merged = loop {
        let found = {
            let recorded = mock
                .recorded_messages
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            recorded
                .iter()
                .map(|messages| messages.to_string())
                .find(|text| text.contains("merge me") && text.contains("<cron-fire "))
        };
        if let Some(text) = found {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the merged held fire must be delivered: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        merged.contains("coalescedCount=\\\""),
        "the merged injection carries the held periods: {merged}"
    );
    // The white-box cursor resets make every 50ms tick fire a period —
    // far more than the two the test forced, but the assertion's point
    // is the MERGE (one injection, a count > 1 accumulating the holds),
    // not the exact number.
    let count = merged
        .split("coalescedCount=\\\"")
        .nth(1)
        .and_then(|rest| rest.split('\\').next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0);
    assert!(
        count >= 2,
        "the merged injection accumulated the held periods (count={count}): {merged}"
    );
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    // The clear lands right after the injected turn settles (the same
    // scheduler iteration's async tail — poll briefly).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !session.cron().held_fires().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delivered merged fire cleared its hold"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Given a session with an overdue one-shot task but a server whose LLM
/// factory always FAILS to build the provider, when the scheduler drains
/// the held fire, then the hold is CLEARED (M3) — the injection can
/// never succeed, so an every-tick retry queue would only spin forever.
#[tokio::test]
async fn should_clear_the_held_fire_when_the_llm_provider_cannot_be_created() {
    // A factory that always fails create_llm — visible to the b2 module
    // through the tests.rs module tree.
    struct B2FailingLlmFactory;
    impl LlmFactory for B2FailingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Err("broken provider config".to_string())
        }
    }
    let (server, _tmp) =
        b2_server_with_factory(Arc::new(B2FailingLlmFactory), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "held-llm-fail" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    seed_overdue_one_shot(&server, &session_id, "cron-llm-fail", "never delivered").await;

    // Then the fire is held, the drain tries to inject, the provider
    // build fails — and the hold is cleared within a few ticks (no
    // unbounded retry, no held-queue buildup).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !session.cron().held_fires().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the undeliverable fire must clear its hold instead of retrying forever"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And the hold STAYS cleared across further ticks (the one-shot task
    // itself is gone — check_via_due removed it when it fired — so no new
    // fire can re-enter the queue either).
    // Two more session cron passes: the cleared hold must not rebuild (the
    // one-shot task left the table when it fired, so no new fire can enter
    // the queue either).
    drive_session_cron_pass(&server, &session_id).await;
    drive_session_cron_pass(&server, &session_id).await;
    assert!(
        session.cron().held_fires().is_empty(),
        "the held queue must not rebuild from a permanent failure"
    );
}

/// Given a session whose held fire exists, when a run-now trigger holds a
/// second fire of the SAME task while busy, then the busy-branch answer
/// reports `held: true` (m2) — and the idle direct-inject answer carries
/// no `held` key at all ("delivered", not "queued").
#[tokio::test]
async fn should_report_held_true_when_a_manual_trigger_queues_behind_a_busy_turn() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("busy reply");
    mock.push_text("delivered fire reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "trigger-held-key" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // A running (busy) turn
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "hold the turn", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(tokio::time::Instant::now() < deadline, "the busy turn must start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.cron().restore_tasks([mirri_core::cron::CronTask {
            session_id: session_id.clone(),
            recurring: true,
            created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
            scope: mirri_core::cron::TaskScope::Session,
            ..mirri_core::cron::cron_task_base("cron-key", "*/5 * * * *", "gated manual fire")
        }]);
    session
        .cron()
        .set_next_fire_at("cron-key", Some(chrono::Utc::now() + chrono::Duration::days(1)));
    drop(session);

    // When the trigger runs while the turn holds the flow (busy branch)
    let triggered = server
        .call_method(
            "cron.trigger",
            json!({ "session_id": &session_id, "task_id": "cron-key" }),
        )
        .await
        .expect("trigger");
    // Then the ack distinguishes "queued" from "delivered": held == true
    assert_eq!(triggered["triggered"], true, "the trigger is accepted");
    assert_eq!(
        triggered["held"], true,
        "the busy branch reports held:true (queued, not delivered): {triggered}"
    );

    // And once the turn settles and the held fire drains, a SECOND
    // trigger on the now-idle session answers with NO held key (the
    // direct-inject posture).
    release.send(()).expect("the gate receiver is alive");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if recorded_all(&mock).to_string().contains("gated manual fire") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held manual fire must be delivered after the busy turn: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // The delivered fire's turn must fully settle before the second
    // trigger: the held-clear lands in the same scheduler iteration's
    // async tail (poll the hold queue, then let one settle beat pass so
    // the flow lock is visibly released).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !session.cron().held_fires().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delivered fire cleared its hold"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || (!server.session_is_busy(&session_id)).then_some(()),
    )
    .await
    .expect("the released turn must settle");
    mock.push_text("second direct fire reply");
    let triggered_again = server
        .call_method(
            "cron.trigger",
            json!({ "session_id": &session_id, "task_id": "cron-key" }),
        )
        .await
        .expect("second trigger");
    assert_eq!(triggered_again["triggered"], true);
    assert!(
        triggered_again.get("held").is_none(),
        "the idle direct inject carries no held key (delivered): {triggered_again}"
    );
}

/// Given a server whose scheduler pass panics for ONE session, when
/// another session's task is due, then the other session's fire still
/// injects (M2 — a per-session panic never takes the scheduler loop
/// down; `cron_scheduler_spawned` stays latched with a live loop).
#[tokio::test]
async fn should_keep_scheduling_other_sessions_when_one_sessions_pass_panics() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("survivor reply");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let panicking = server
        .call_method("session.create", json!({ "title": "panicking" }))
        .await
        .expect("create panicking");
    let survivor = server
        .call_method("session.create", json!({ "title": "survivor" }))
        .await
        .expect("create survivor");
    let panicking_id = panicking["id"].as_str().unwrap().to_string();
    let survivor_id = survivor["id"].as_str().unwrap().to_string();

    // The panicking session holds an overdue one-shot task; the M2 test
    // seam arms its pass to panic at the entry of EVERY scheduler visit
    // (a genuine panic inside the spawned per-session body).
    seed_overdue_one_shot(&server, &panicking_id, "cron-poison", "poison fire").await;
    server.set_cron_pass_panic_session(Some(panicking_id.clone()));

    // And the survivor session's task is due too (the pass order within a
    // tick is the listing's; over enough ticks both get their chance).
    seed_overdue_one_shot(&server, &survivor_id, "cron-survivor", "survivor fire").await;

    // Then across many ticks the survivor's fire is delivered — the
    // panicking session's pass failure (a JoinError each tick) never
    // stopped the scheduler loop.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if recorded_all(&mock).to_string().contains("survivor fire") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the surviving session must keep being scheduled while the \
             other session's pass panics: {}",
            recorded_all(&mock)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // And the panicking session's own fire was never injected (its pass
    // died before any injection every tick).
    assert!(
        !recorded_all(&mock).to_string().contains("poison fire"),
        "the panicking session's fire must not inject"
    );
    // Disarm the seam BEFORE the test ends: the scheduler task outlives
    // the server (its captured Arcs keep it alive), and an armed seam
    // would keep panicking every tick for the rest of the process — the
    // panic storm starves the OTHER tests sharing this runtime.
    server.set_cron_pass_panic_session(None);
}
