//! BH8 (B-ADR-16 ①): the silence-warning engine — the task driver's
//! mid-flight warning scan and its busy/idle injection paths.
//!
//! Every BH8 embedded test lands here (the single-owner rule the B2/B1
//! modules set): a running process-kind task that goes silent past the
//! `[task]` `silenceWarnSecs` threshold gets one synthetic user message
//! per silence episode — parked to the steer buffer when main is busy,
//! its own engine turn when idle — and the episode re-arms when output
//! resumes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::json;

use super::*;
use b1_task_lifecycle_tests::await_chat_message;
use b2_notification_tests::recorded_all;

/// A BH8 test bed: a scripted-LLM server whose `[task]` silenceWarnSecs
/// is shortened so a silence episode matures within the test's window,
/// with a fast task-sweep tick (the warning scan rides the driver's
/// cadence).
fn bh8_server(mock: Arc<ScriptedLlm>, toml_contents: &str) -> (EmbeddedServer, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(test_manager_with_sqlite_store(workspace, tmp.path()));
    let mut parsed = ServerConfig::from_toml(toml_contents).expect("test config");
    parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    let config = Arc::new(RwLock::new(parsed));
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
    .with_task_sweep_tick(Duration::from_millis(50));
    (server, tmp)
}

/// The minimal silence config: a 1s threshold (u64 seconds — the field's
/// type) so the episode matures within the test's window (the default
/// 300s would need real waiting).
const BH8_TOML: &str = "[task]\nsilenceWarnSecs = 1\n";

/// Register a running process-kind background task with some output.
fn running_process_task(session: &mirri_core::SessionScope, session_id: &str) -> String {
    let tasks = session.tasks();
    let task_id = tasks
        .register_background(
            TaskKind::Process {
                command: Some("npm run watch".into()),
                dispatcher: None,
            },
            "watch build".into(),
            session_id.to_string(),
            None,
        )
        .expect("background registration");
    tasks.append_output(&task_id, "started\n");
    task_id
}

/// Whether any recorded LLM call saw the silence-warning tag.
fn silence_warning_reached_model(mock: &ScriptedLlm) -> bool {
    recorded_all(mock)
        .to_string()
        .contains("<background-task-silence-warning")
}

/// Given a running process-kind background task that produced output and
/// then went silent past the threshold, when the driver's silence scan
/// ticks, then exactly ONE warning lands — the episode dedup holds across
/// ticks while the silence persists (BH8, B-ADR-16 ①).
///
/// Runs the REAL spawned driver loop (no manual tick): the episode-dedup
/// state lives inside the loop, so a manual drive with fresh state would
/// inject alongside the loop's own injection and prove nothing.
#[tokio::test]
async fn should_warn_once_when_a_process_task_goes_silent_beyond_the_threshold() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "silence once" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let _task_id = running_process_task(&session, &session_id);

    // When the real loop ticks past the threshold while the task stays
    // silent, then waits long enough for a SECOND episode-worth of ticks
    // (the dedup must hold)
    await_chat_message(&mock, "<background-task-silence-warning").await;
    tokio::time::sleep(Duration::from_millis(1500)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)

    // Then exactly one warning turn ran (episode dedup — the silence
    // persists, so no second warning). Counted as NEW occurrences vs the
    // previous turn's input: each recorded request replays delivered
    // warnings in history, so a raw total would double-count — the delta
    // between consecutive calls is the freshly injected message(s).
    // Call 0 IS the warning turn (its opening message is the warning), so
    // the count starts there.
    let count_at = |call: usize| {
        mock.messages_of_call(call)
            .to_string()
            .matches("<background-task-silence-warning")
            .count()
    };
    let last = mock.chat_call_count() as usize;
    let mut total_new = count_at(0);
    for call in 1..last {
        total_new += count_at(call).saturating_sub(count_at(call - 1));
    }
    assert_eq!(
        total_new,
        1,
        "exactly one warning per episode: {}",
        recorded_all(&mock)
    );
    assert_eq!(mock.chat_call_count(), 1, "one warning turn, nothing more");
}

/// Given a warned task whose output resumes and then goes silent AGAIN,
/// when the driver's silence scan ticks, then a SECOND warning lands —
/// the `last_output_at` advance reset the episode (BH8, B-ADR-16 ①).
///
/// Runs the REAL spawned driver loop (the episode state lives inside it).
#[tokio::test]
async fn should_warn_again_when_output_resumes_then_goes_silent_again() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack 1");
    mock.push_text("ack 2");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "silence twice" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = running_process_task(&session, &session_id);

    // When the first episode matures and the warning fires (the real
    // loop's own state dedups it)
    await_chat_message(&mock, "<background-task-silence-warning").await;

    // And the output resumes (the episode resets), then goes silent
    // again: wait past the threshold so the second episode's warning
    // turn runs, then let further ticks pass (the dedup must hold again).
    session.tasks().append_output(&task_id, "progress\n");
    await_chat_message(&mock, "started\\nprogress").await;
    tokio::time::sleep(Duration::from_millis(1500)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)

    // Then a second warning landed (the reset re-armed the episode).
    // Counted as NEW occurrences vs the previous turn's input: each
    // recorded request replays delivered warnings in history, so a raw
    // total would double-count — the delta between consecutive calls is
    // the freshly injected message(s).
    let count_at = |call: usize| {
        mock.messages_of_call(call)
            .to_string()
            .matches("<background-task-silence-warning")
            .count()
    };
    let last = mock.chat_call_count() as usize;
    let mut total_new = count_at(0);
    for call in 1..last {
        total_new += count_at(call).saturating_sub(count_at(call - 1));
    }
    assert_eq!(
        total_new,
        2,
        "the reset episode must warn again: {}",
        recorded_all(&mock)
    );
}

/// Given a running agent-kind task that goes silent, when the driver's
/// silence scan ticks, then NO warning lands — only process-kind tasks
/// warn (agent liveness belongs to the heartbeat machinery).
#[tokio::test]
async fn should_not_warn_for_an_agent_kind_task() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "agent silent" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session
        .tasks()
        .register_background(
            TaskKind::Agent {
                agent_id: None,
                subagent_type: None,
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "research".into(),
            session_id.clone(),
            None,
        )
        .unwrap();

    // When the silence window matures and several ticks pass
    tokio::time::sleep(Duration::from_millis(1200)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)
    let mut warned = HashSet::new();
    let mut snapshots = HashMap::new();
    for _ in 0..3 {
        drive_silence_tick(&server, &session_id, &mut warned, &mut snapshots).await;
    }

    // Then no warning reached the model
    assert!(
        !silence_warning_reached_model(&mock),
        "an agent-kind task must never trigger the silence warning"
    );
}

/// Given a server configured with `[task] silenceWarnSecs = 0`, when a
/// process task goes silent and the scan ticks, then NO warning lands —
/// `0` disables the warning entirely (the `0 = off` semantic family).
///
/// The same-window CONTROL arm (`silenceWarnSecs = 1`) proves the
/// mechanism itself fires in this window (it must warn) — so the zero
/// arm's silence is attributable to the zero, not to a dead mechanism.
#[tokio::test]
async fn should_not_warn_when_silence_warn_secs_is_zero() {
    // Control arm: a 1s threshold in the same wall-clock window MUST
    // warn (the tick loop, the episode state, and the injection are all
    // live) — otherwise the zero arm below would prove nothing.
    let control_mock = Arc::new(ScriptedLlm::new());
    control_mock.push_text("control ack");
    let (control_server, _control_tmp) =
        bh8_server(Arc::clone(&control_mock), "[task]\nsilenceWarnSecs = 1\n");
    let control_created = control_server
        .call_method("session.create", json!({ "title": "silence control" }))
        .await
        .expect("create");
    let control_session_id = control_created["id"].as_str().unwrap().to_string();
    let control_session = control_server
        .session_manager()
        .get_session(&SessionId::new(&control_session_id))
        .await
        .expect("control session is live");
    let _control_task = running_process_task(&control_session, &control_session_id);
    tokio::time::sleep(Duration::from_millis(1200)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)
    let mut control_warned = HashSet::new();
    let mut control_snapshots = HashMap::new();
    for _ in 0..3 {
        drive_silence_tick(
            &control_server,
            &control_session_id,
            &mut control_warned,
            &mut control_snapshots,
        )
        .await;
    }
    assert!(
        silence_warning_reached_model(&control_mock),
        "CONTROL ARM FAILED: the 1s-threshold session must warn in this window — a silent pass here means the mechanism is dead and the zero arm below proves nothing: {}",
        recorded_all(&control_mock)
    );

    // Zero arm: the same window, with `0` — nothing may reach the model.
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), "[task]\nsilenceWarnSecs = 0\n");
    let created = server
        .call_method("session.create", json!({ "title": "silence off" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let _task_id = running_process_task(&session, &session_id);

    // And the manager carries the explicit-off sentinel (`Some(ZERO)`, NOT
    // the constructor's `None` — an explicit disable must be distinguishable
    // from "never configured", or the driver's 300s fallback would win and
    // the user's `0` would warn after 5 minutes anyway)
    assert!(
        session
            .tasks()
            .silence_warn_secs()
            .is_some_and(|threshold| threshold.is_zero()),
        "silenceWarnSecs = 0 must reach the manager as the explicit-off sentinel: {:?}",
        session.tasks().silence_warn_secs()
    );

    // When the silence window matures and several ticks pass (the same
    // 1.2s window the control arm proved sufficient)
    tokio::time::sleep(Duration::from_millis(1200)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)
    let mut warned = HashSet::new();
    let mut snapshots = HashMap::new();
    for _ in 0..3 {
        drive_silence_tick(&server, &session_id, &mut warned, &mut snapshots).await;
    }

    // Then no warning reached the model
    assert!(
        !silence_warning_reached_model(&mock),
        "silenceWarnSecs = 0 must disable the warning entirely"
    );
}

/// Given a TERMINAL process task (completed) whose last output predates
/// the threshold, when the driver's silence scan ticks, then NO warning
/// lands — terminal tasks never warn (the terminal notification owns
/// them).
#[tokio::test]
async fn should_not_warn_for_a_terminal_task() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ack");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "terminal" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = running_process_task(&session, &session_id);
    session.tasks().complete(&task_id, 0);

    // When the silence window matures and several ticks pass
    tokio::time::sleep(Duration::from_millis(1200)).await; // hygiene-allow: wall-clock silence-window maturation (a real 1s threshold elapses in wall time — no in-process observable)
    let mut warned = HashSet::new();
    let mut snapshots = HashMap::new();
    for _ in 0..3 {
        drive_silence_tick(&server, &session_id, &mut warned, &mut snapshots).await;
    }

    // Then no warning reached the model
    assert!(
        !silence_warning_reached_model(&mock),
        "a terminal task must never warn"
    );
}

/// Given a due silence warning while main is BUSY (a turn in flight whose
/// first step answered a tool call), when the scan ticks, then the
/// warning is PARKED as a steer and lands in the SAME turn's next LLM
/// request — the busy-path park, the terminal notifications' shape
/// (write-before-mutate: the `turn.steer` record precedes the buffer
/// push) with an independent origin.
#[tokio::test]
async fn should_park_the_silence_warning_into_the_steer_buffer_when_busy() {
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated_tool_use(vec![LlmToolCall {
        id: "call-bh8-busy-1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "true" }),
    }]);
    mock.push_text("Second reply");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "busy silence" }))
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

    // Given a fire-mode turn whose first LLM call is gated on a tool
    // reply (busy, and a next step is guaranteed once the gate opens)
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "watch the task", "mode": "fire" }),
        )
        .await
        .expect("send");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while mock.chat_call_count() < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first LLM call must start within 5s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await; // hygiene-allow: poll-step (deadline-bounded LLM-call poll)
    }

    // When the task goes silent past the threshold, the REAL driver loop
    // (running at 50ms) parks the warning on its own tick — no manual
    // `drive_silence_tick` here: its dedup state is independent of the
    // loop's, so a manual pass alongside the real one would double-park
    // (exactly the duplicate the count assertion below must not allow)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = running_process_task(&session, &session_id);

    // Then EXACTLY ONE warning is parked in main's steer buffer by the
    // real loop's busy-path park (poll for the park, then pin the count:
    // the episode dedup must keep the 50ms re-scans at one entry —
    // `.any()` would pass with two)
    let park_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let buffer = loop {
        let buffer = session_steer_buffer(&server, &session_id).await;
        let count = {
            let buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
            buf.items_text()
                .iter()
                .filter(|text| text.contains("<background-task-silence-warning"))
                .count()
        };
        if count >= 1 {
            break buffer;
        }
        assert!(
            tokio::time::Instant::now() < park_deadline,
            "the busy-path park must land within 5s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // hygiene-allow: poll-step (deadline-bounded park poll)
    };
    // Allow a grace window for would-be duplicates to land, then pin the count
    tokio::time::sleep(Duration::from_millis(300)).await; // hygiene-allow: wall-clock duplicate-detection window (the loop re-scans at 50ms; a duplicate would land within a few ticks)
    let parked = {
        let buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.items_text()
    };
    let parked_warnings = parked
        .iter()
        .filter(|text| {
            text.contains("<background-task-silence-warning") && text.contains(&task_id)
        })
        .count();
    assert_eq!(
        parked_warnings, 1,
        "exactly one parked warning (the episode dedup must hold across the loop's re-scans): {parked:?}"
    );
    assert_eq!(
        mock.chat_call_count(),
        1,
        "no second turn may launch while main is busy"
    );

    // And when the gate opens, the warning rides the SAME turn's next
    // LLM request (the before_step flush)
    release.send(()).expect("the gate receiver is still alive");
    poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains("<background-task-silence-warning"),
        "the parked warning must reach the next LLM request: {all}"
    );
    assert!(
        all.contains(&task_id),
        "the warning must name the task: {all}"
    );
}

/// Given a due silence warning while main is IDLE, when the scan ticks,
/// then the warning injects a NEW engine turn directly — the idle path
/// (its own turn.prompt, the warning as the user prompt, the
/// system_trigger origin on the durable record).
///
/// Runs the REAL spawned driver loop (no manual tick — the episode state
/// lives inside the loop). The session is materialized by the warning
/// turn itself: the injected turn records its ops through the session's
/// wire recorder, which materializes the lazy session's deferred log.
#[tokio::test]
async fn should_inject_the_silence_warning_as_a_new_turn_when_idle() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("warning ack");
    let (server, _tmp) = bh8_server(Arc::clone(&mock), BH8_TOML);
    let created = server
        .call_method("session.create", json!({ "title": "idle silence" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Given an idle session (the materializing turn has already settled,
    // so the wire log exists on disk) and a silent task past the threshold
    converse_once(&server, &session_id).await;
    wait_for_turn_idle(&server, &session_id).await;
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let task_id = running_process_task(&session, &session_id);

    // Then the warning ran as its own turn: the model saw the warning
    // text naming the task, and the durable wire log holds the
    // turn.prompt with the system_trigger origin
    await_chat_message(&mock, "<background-task-silence-warning").await;
    assert!(silence_warning_reached_model(&mock));
    let all = recorded_all(&mock).to_string();
    assert!(
        all.contains(&task_id),
        "the warning must name the task: {all}"
    );

    let manager = server.session_manager();
    let wire_log_path = manager
        .persistence()
        .wire_log_path(&SessionId::new(&session_id));
    // The wire log materializes asynchronously (the recorder's flush is
    // decoupled from the turn's completion): poll for the records
    // instead of requiring them instantly.
    let ops = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(log) = JsonlAppendLog::open(&wire_log_path).await {
                let read = log.read_all().await.unwrap_or_default();
                if read.iter().any(|op| {
                    matches!(op, WireOp::TurnPrompt { origin, .. } if origin["kind"] == "system_trigger")
                }) {
                    break read;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the warning turn's turn.prompt never materialized"
            );
            tokio::time::sleep(Duration::from_millis(20)).await; // hygiene-allow: poll-step (deadline-bounded wire-log poll)
        }
    };
    // And exactly one warning turn ran (no multi-inject)
    let warning_turns = ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                WireOp::TurnPrompt { origin, .. } if origin["kind"] == "system_trigger"
            )
        })
        .count();
    assert_eq!(warning_turns, 1, "one warning turn, no multi-inject");
}
