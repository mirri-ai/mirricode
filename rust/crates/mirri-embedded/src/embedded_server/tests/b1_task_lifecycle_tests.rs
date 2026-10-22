//! B1 T4: task-lifecycle assembly — the per-session timeout sweeper, the
//! `[task]`/`[subagent]` knobs flowing from `config.toml` into the session's
//! [`TaskManager`], and the kill-grace / keep-alive wiring surface.
//!
//! These tests live in their own module (per the B1 plan) so the 800+-test
//! `tests.rs` keeps a single owner: every B1 embedded test lands here.

use std::sync::{Arc, RwLock};

use serde_json::json;

use super::*;

/// A throwaway server with a config parsed from the given TOML and a
/// shortened task-sweep tick — the assembly test bed for the lifecycle
/// driver (the same shape `test_server_with_config_toml` builds, minus the
/// on-disk config file, plus the sweep knob).
fn b1_server_with_config(
    toml_contents: &str,
    sweep_tick: std::time::Duration,
) -> (EmbeddedServer, tempfile::TempDir) {
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
    let server = EmbeddedServer::with_providers(manager, None, None, None, None, config, None)
        .with_task_sweep_tick(sweep_tick);
    (server, tmp)
}

/// Given a session on a server whose task sweeper ticks fast, When a task
/// with an explicit 100ms deadline is registered and never settles, Then
/// the sweeper terminates it — `failed` with the timeout message — within a
/// few ticks (the pre-B1 server had no caller for `terminate_expired`, so a
/// stuck task ran forever).
#[tokio::test]
async fn should_terminate_an_expired_task_when_the_sweeper_ticks() {
    use mirri_core::task::TASK_TIMEOUT_MESSAGE;
    use mirri_core::task::{TaskKind, TaskManager, TaskStatus};

    let (server, _tmp) = b1_server_with_config("", std::time::Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "swept" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let tasks: Arc<TaskManager> = session.tasks();
    let task_id = tasks.register_with_timeout(
        TaskKind::Process {
            command: Some("sleep 600".into()),
            dispatcher: None,
        },
        "stuck".into(),
        session_id.clone(),
        Some(std::time::Duration::from_millis(100)),
    );

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = tasks.get(&task_id)
            && task.status == TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sweeper must terminate the expired task; last status: {:?}",
            tasks.get(&task_id).map(|task| task.status.clone())
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Given a session created on a server with `[task] maxRunningTasks = 1`,
/// When the session's TaskManager takes a second background registration,
/// Then it is rejected — the capacity knob flows from config.toml through
/// the session assembly into the live manager.
#[tokio::test]
async fn should_apply_the_capacity_limit_when_the_task_config_names_one() {
    use mirri_core::task::{TASK_CAPACITY_MESSAGE, TaskKind};

    let (server, _tmp) = b1_server_with_config(
        "[task]\nmaxRunningTasks = 1\n",
        std::time::Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({ "title": "capacity" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let tasks = session.tasks();
    assert!(
        tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None
                },
                "first".into(),
                session_id.clone(),
                None,
            )
            .is_ok()
    );
    assert_eq!(
        tasks.register_background(
            TaskKind::Process {
                command: None,
                dispatcher: None
            },
            "second".into(),
            session_id.clone(),
            None,
        ),
        Err(TASK_CAPACITY_MESSAGE.to_string()),
        "the [task] maxRunningTasks knob must reach the session's TaskManager"
    );
}

/// Given a resumed session, When it comes back on the same server, Then the
/// task sweeper drives it too (a hung task left running by the previous
/// process is re-driven after the resume).
#[tokio::test]
async fn should_drive_the_task_sweeper_again_when_a_session_is_resumed() {
    use mirri_core::task::TASK_TIMEOUT_MESSAGE;
    use mirri_core::task::{TaskKind, TaskManager, TaskStatus};

    let (server, _tmp) = b1_server_with_config("", std::time::Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "resume-swept" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // Converse once to materialize the session (SDR-004), then close it.
    let _ = server
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "hi", "mode": "fire" }),
        )
        .await;
    let _ = server
        .call_method("session.close", json!({ "session_id": session_id }))
        .await;

    let resumed = server
        .call_method("session.resume", json!({ "session_id": session_id }))
        .await
        .expect("resume");
    assert_eq!(resumed["id"], json!(session_id));

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live again");
    let tasks: Arc<TaskManager> = session.tasks();
    let task_id = tasks.register_with_timeout(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "post-resume stuck".into(),
        session_id.clone(),
        Some(std::time::Duration::from_millis(100)),
    );

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = tasks.get(&task_id)
            && task.status == TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed session's sweeper must terminate the expired task"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// A profiles + config test bed: registers a `"coder"` profile (so the
/// `Agent` tool can spawn subagents through the mock LLM), parses the
/// given `[task]`/`[subagent]` TOML, and shortens the sweep tick.
pub(super) fn b1_profiles_server_with_config(
    llm_factory: Arc<dyn LlmFactory>,
    toml_contents: &str,
    sweep_tick: std::time::Duration,
) -> (EmbeddedServer, tempfile::TempDir) {
    use mirri_core::{AgentProfile, PermissionMode, ProfileManager};
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    let profile_manager = Arc::new(ProfileManager::new());
    profile_manager
        .register(AgentProfile {
            name: "coder".to_string(),
            model: "mock-llm".to_string(),
            permission_mode: Some(PermissionMode::Yolo),
            // Default-deny: the coder must explicitly allowlist what it may
            // dispatch (nested AgentSwarm members spawn as coders).
            subagents: Some(vec!["coder".to_string()]),
            ..AgentProfile::default()
        })
        .expect("coder profile registers");
    let mut parsed = ServerConfig::from_toml(toml_contents).expect("test config should parse");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback): seed the test default unless the TOML sets one.
    if parsed.default_model.is_none() {
        parsed.default_model = Some(super::TEST_DEFAULT_MODEL.to_string());
    }
    let config = Arc::new(RwLock::new(parsed));
    let server = EmbeddedServer::with_providers(
        manager,
        Some(llm_factory),
        Some(profile_manager),
        None,
        None,
        config,
        None,
    )
    .with_task_sweep_tick(sweep_tick);
    (server, tmp)
}

/// Wait until the scripted mock has served `calls` chat calls (the turn
/// progresses) or the deadline expires.
pub(super) async fn await_chat_calls(mock: &ScriptedLlm, calls: u64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while mock.chat_call_count() < calls {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scripted flow stalled at {} chat calls",
            mock.chat_call_count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Given a server configured with `[task] maxRunningTasks = 1` and a first
/// background agent that never settles, When the main agent dispatches a
/// second background Agent call, Then the second tool result carries the TS
/// parity capacity message and the task list holds exactly one running
/// background task (S2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_reject_the_second_background_agent_when_max_running_tasks_is_1() {
    let main_llm = Arc::new(ScriptedLlm::new());
    let child_llm = Arc::new(ScriptedLlm::new());
    // 1. Main agent → first background Agent dispatch.
    main_llm.push_tool_use(vec![LlmToolCall {
        id: "call_0".to_string(),
        name: "Agent".to_string(),
        args: json!({
            "prompt": "first",
            "description": "one",
            "run_in_background": true
        }),
    }]);
    // 1b. The in-turn side title call — fires after the first step lands
    // (SDR-016) and consumes one scripted reply.
    main_llm.push_text("background agents");
    // 2. Main agent → second background Agent dispatch (rejected).
    main_llm.push_tool_use(vec![LlmToolCall {
        id: "call_2".to_string(),
        name: "Agent".to_string(),
        args: json!({
            "prompt": "second",
            "description": "two",
            "run_in_background": true
        }),
    }]);
    // 3. Main agent final reply (its input carries the rejected result).
    main_llm.push_text("done");
    // 4. The first subagent's LLM call — gated so it never settles (the
    //    capacity slot stays taken). Separate mock (see
    //    ModelRoutedLlmFactory): the post-T4 scheduler race between the
    //    parent's continuation and the detached child's first call cannot
    //    cross-feed the scripted responses.
    let _hold_first = child_llm.push_gated("first subagent result");

    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ModelRoutedLlmFactory {
            child_model: "mock-llm",
            main: Arc::clone(&main_llm),
            child: Arc::clone(&child_llm),
        }),
        "[task]\nmaxRunningTasks = 1\n",
        std::time::Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({ "permissionMode": "yolo" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "dispatch two background agents",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    // The main agent's final chat call (its third — the side title call
    // takes one in between, and the subagent answers from its own mock)
    // carries the second Agent call's tool result — the capacity
    // rejection.
    await_chat_calls(&main_llm, 4).await;
    let final_messages = main_llm.messages_of_call(3).to_string();
    assert!(
        final_messages.contains("Too many background tasks are already running."),
        "the capacity rejection must reach the model: {final_messages}"
    );

    // The task list holds exactly the one running background task.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let running = session
        .tasks()
        .list()
        .into_iter()
        .filter(|task| matches!(task.status, mirri_core::task::TaskStatus::Running))
        .count();
    assert_eq!(
        running,
        1,
        "only the first background task is running; tasks: {:?}",
        session.tasks().list()
    );
}

/// An LLM factory routing by model id: the spawned subagent (the `"coder"`
/// profile runs on `"mock-llm"`) answers from its own mock while the main
/// agent answers from another. Since B4 T4 the parent's continuation LLM
/// call and the detached child's first call race on the scheduler (the old
/// block_in_place shape issued the child's call synchronously inside the
/// tool call, so the child always came first — that order was the
/// artifact), so a scripted test that must keep both agents' responses
/// distinct gives them SEPARATE queues: the race then cannot cross-feed
/// the scripted responses.
struct ModelRoutedLlmFactory {
    child_model: &'static str,
    main: Arc<ScriptedLlm>,
    child: Arc<ScriptedLlm>,
}

impl LlmFactory for ModelRoutedLlmFactory {
    fn create_llm(&self, config: &mirri_core::AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        if config.model_id == self.child_model {
            Ok(Arc::clone(&self.child) as Arc<dyn Llm>)
        } else {
            Ok(Arc::clone(&self.main) as Arc<dyn Llm>)
        }
    }
}

/// Given a session created on a server with `[subagent] timeoutMs = 300`,
/// When a background subagent hangs (a gated LLM call that never opens),
/// Then the sweeper terminates the tracked task — `failed` with the timeout
/// message — and fires its cancel signal: the runaway agent is actually
/// stopped, not just relabelled (S1).
///
/// Runs on the single-threaded runtime ON PURPOSE: `terminate_expired`
/// fires the signal and flips the status in two synchronous calls, and on
/// a multi-worker runtime the woken runner can be polled between them —
/// landing its own `Stopped` first under the first-terminal-wins rule (see
/// `terminate_expired`'s doc). Single-threaded, the pair is atomic, so the
/// expected `Failed(timeout)` is deterministic.
#[tokio::test]
async fn should_terminate_a_hung_subagent_when_the_subagent_timeout_expires() {
    let main_llm = Arc::new(ScriptedLlm::new());
    let child_llm = Arc::new(ScriptedLlm::new());
    // 1. Main agent → background Agent dispatch.
    main_llm.push_tool_use(vec![LlmToolCall {
        id: "call_0".to_string(),
        name: "Agent".to_string(),
        args: json!({
            "prompt": "hang forever",
            "description": "hung",
            "run_in_background": true
        }),
    }]);
    // 2. Main agent final reply (the turn ends; the background task hangs).
    main_llm.push_text("done");
    // 3. The subagent's LLM call — the gate is NEVER opened: the subagent
    //    hangs past its 300ms deadline.
    let _hold = child_llm.push_gated("never delivered");

    let (server, _tmp) = b1_profiles_server_with_config(
        Arc::new(ModelRoutedLlmFactory {
            child_model: "mock-llm",
            main: Arc::clone(&main_llm),
            child: Arc::clone(&child_llm),
        }),
        "[subagent]\ntimeoutMs = 300\n",
        std::time::Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({ "permissionMode": "yolo" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "run a background agent that hangs",
                "mode": "fire"
            }),
        )
        .await
        .expect("send");

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let tasks = session.tasks();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let hung = tasks
            .list()
            .into_iter()
            .find(|task| matches!(task.kind, mirri_core::task::TaskKind::Agent { .. }));
        if let Some(task) = hung
            && let mirri_core::task::TaskStatus::Failed(reason) = &task.status
            && reason == "task exceeded its configured timeout"
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sweeper must terminate the hung subagent; tasks: {:?}",
            tasks
                .list()
                .into_iter()
                .map(|task| (task.kind, task.status))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Given a session with a running background task, When the session closes,
/// Then the task lands `killed` on the wire face (D-62: Rust realizes the TS
/// `lost` semantics as persisted-killed) and subscribers observe the
/// `event.background.task.terminated` termination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_kill_running_tasks_when_the_session_closes() {
    use mirri_core::task::{TaskKind, TaskStatus};

    let (server, _tmp) = b1_server_with_config("", std::time::Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({ "title": "closing" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let tasks = session.tasks();
    let task_id = tasks.register_with_timeout(
        TaskKind::Process {
            command: Some("sleep 60".into()),
            dispatcher: None,
        },
        "running at close".into(),
        session_id.clone(),
        None,
    );
    let info = tasks
        .get(&task_id)
        .expect("task registered")
        .background_task_info();

    // Hold the task-manager Arc past the close so the post-close state is
    // still observable (the session scope itself is gone).
    let mut events = server.subscribe_events().await.expect("subscribe");
    server
        .call_method("session.close", json!({ "session_id": session_id }))
        .await
        .expect("close");

    assert_eq!(
        tasks.get(&task_id).expect("task survives the close").status,
        TaskStatus::Stopped,
        "the running task must land killed at close (persisted-killed ≡ lost)"
    );
    // The termination event for the pre-registered task is observable on
    // the stream (the started event predates the subscription; only the
    // terminated one is asserted).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let Ok(Some(notification)) = tokio::time::timeout_at(deadline, events.recv()).await else {
            panic!("the terminated event never arrived");
        };
        if notification.method == "event.background.task.terminated"
            && notification.params["info"]["taskId"] == json!(info.task_id)
        {
            assert_eq!(notification.params["info"]["status"], json!("killed"));
            break;
        }
    }
}
