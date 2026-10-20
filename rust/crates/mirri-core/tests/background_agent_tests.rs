//! 后台任务管理器集成测试 — 覆盖心跳过期、agent 超时终止、输出隔离与截断、
//! 持久化恢复以及重启后 running→stopped 调谐等行为。
//!
//! 这些测试通过 [`TaskManager`] 的公共 API 验证端到端行为, 使用真实文件系统
//! 持久化 (tempfile) 和 wall-clock 时间 (短间隔 + sleep) 来模拟时间流逝。

use std::sync::Arc;
use std::time::Duration;

use mirri_core::task::{
    MAX_TASK_OUTPUT_BYTES, TASK_TIMEOUT_MESSAGE, TaskKind, TaskManager, TaskStatus,
};
use mirri_core::task_persist::TaskPersistence;

// ============================================================
// 心跳与过期检测
// ============================================================

/// 当心跳间隔已过且未刷新时, is_stale 应返回 true。
#[test]
fn should_mark_task_as_stale_when_heartbeat_expires() {
    // 给定: 一个配置了 50ms 心跳间隔的 TaskManager
    let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(50));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "stale-agent".into(),
        "session-1".into(),
    );

    // 刚注册时不应过期
    assert!(!manager.is_stale(&task_id), "新注册的任务不应立即过期");

    // 当: 等待超过心跳间隔
    std::thread::sleep(Duration::from_millis(100));

    // 则: is_stale 返回 true
    assert!(manager.is_stale(&task_id), "心跳过期后任务应被标记为 stale");
}

/// 当心跳间隔内刷新了心跳, is_stale 应返回 false。
#[test]
fn should_keep_task_alive_when_heartbeat_is_refreshed_within_interval() {
    // 给定: 一个配置了 200ms 心跳间隔的 TaskManager
    let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(200));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "alive-agent".into(),
        "session-1".into(),
    );

    // 当: 等待一半间隔后刷新心跳, 再等待一半间隔
    std::thread::sleep(Duration::from_millis(100));
    manager.heartbeat(&task_id);
    std::thread::sleep(Duration::from_millis(100));

    // 则: is_stale 仍返回 false (心跳在过期前被刷新)
    assert!(
        !manager.is_stale(&task_id),
        "心跳在过期前刷新, 任务不应被标记为 stale"
    );
}

/// 当未配置心跳间隔时, is_stale 始终返回 false。
#[test]
fn should_never_report_stale_when_no_heartbeat_interval_is_configured() {
    // 给定: 一个未配置心跳间隔的 TaskManager
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "no-heartbeat".into(),
        "session-1".into(),
    );

    // 当: 等待一段时间
    std::thread::sleep(Duration::from_millis(50));

    // 则: is_stale 返回 false
    assert!(
        !manager.is_stale(&task_id),
        "未配置心跳间隔时, 任务永远不应被标记为 stale"
    );
}

// ============================================================
// Agent 超时终止
// ============================================================

/// 当 agent_timeout 到期时, terminate_expired 应终止任务并返回其 task_id。
#[test]
fn should_terminate_task_when_agent_timeout_is_reached() {
    // 给定: 一个配置了 50ms agent_timeout 的 TaskManager 和一个 running 任务
    let manager = TaskManager::new().with_agent_timeout(Duration::from_millis(50));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "slow-agent".into(),
        "session-1".into(),
    );

    // 当: 等待超过 agent_timeout 后调用 terminate_expired
    std::thread::sleep(Duration::from_millis(100));
    let terminated = manager.terminate_expired();

    // 则: 返回的列表包含该 task_id, 且任务状态变为 Failed
    assert_eq!(terminated, vec![task_id.clone()]);
    let task = manager.get(&task_id).expect("任务应存在");
    assert_eq!(
        task.status,
        TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string())
    );
}

/// 当未配置 agent_timeout 时, terminate_expired 应返回空列表。
#[test]
fn should_not_terminate_when_no_agent_timeout_is_configured() {
    // 给定: 一个未配置 agent_timeout 的 TaskManager 和一个 running 任务
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "no-timeout".into(),
        "session-1".into(),
    );

    // 当: 等待一段时间后调用 terminate_expired
    std::thread::sleep(Duration::from_millis(50));
    let terminated = manager.terminate_expired();

    // 则: 返回空列表, 任务仍为 Running
    assert!(
        terminated.is_empty(),
        "未配置 agent_timeout 时不应终止任何任务"
    );
    let task = manager.get(&task_id).expect("任务应存在");
    assert_eq!(task.status, TaskStatus::Running);
}

// ============================================================
// 输出访问隔离
// ============================================================

/// 当使用非创建会话访问任务输出时, 应被拒绝 (返回 None)。
#[test]
fn should_restrict_output_access_to_creating_session() {
    // 给定: 会话 A 创建的任务, 并写入了输出
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "session-scoped".into(),
        "session-A".into(),
    );
    manager.append_output(&task_id, "secret output\n");

    // 当: 用会话 B 尝试访问输出
    let result_b = manager.output_for_session(&task_id, "session-B");

    // 则: 返回 None (拒绝访问)
    assert!(result_b.is_none(), "非创建会话不应能访问任务输出");

    // 同时验证创建会话 A 仍可正常访问
    let result_a = manager.output_for_session(&task_id, "session-A");
    assert!(result_a.is_some(), "创建会话应能访问任务输出");
    let output = result_a.unwrap();
    assert!(output.full_output.contains("secret output"));
}

// ============================================================
// 输出截断
// ============================================================

/// 当输出超过上限时, 应被截断并标记为 truncated。
#[test]
fn should_truncate_output_when_exceeding_limit() {
    // 给定: 一个任务
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "chatty-task".into(),
        "session-1".into(),
    );

    // 当: 写入超过 MAX_TASK_OUTPUT_BYTES 的输出
    let head = "H".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);
    let tail = "T".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);
    manager.append_output(&task_id, &head);
    manager.append_output(&task_id, &tail);

    // 则: 输出被截断, 长度不超过上限, 且保留了最近的尾部
    let output = manager.output(&task_id).expect("任务应存在");
    assert!(output.truncated, "超过上限的输出应被标记为 truncated");
    assert!(
        output.full_output.len() <= MAX_TASK_OUTPUT_BYTES,
        "截断后输出不应超过上限"
    );
    assert!(output.full_output.ends_with('T'), "截断应保留最近的输出");
    assert!(
        output.preview.contains("[output truncated]"),
        "预览应包含截断标记"
    );
}

// ============================================================
// 持久化与恢复
// ============================================================

/// 当 TaskManager 配置了持久化时, 注册的任务应被写入磁盘, 并可通过新的
/// TaskManager 恢复。
#[test]
fn should_persist_and_restore_background_task_state() {
    // 给定: 一个带持久化的 TaskManager, 注册并完成任务
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    let persistence = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let manager = TaskManager::with_persistence(persistence);

    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "persisted-task".into(),
        "session-1".into(),
    );
    manager.append_output(&task_id, "some output\n");
    manager.complete(&task_id, 0);

    // 当: 用同一个持久化目录创建新的 TaskManager (模拟重启)
    let persistence2 = Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let restored_manager = TaskManager::with_persistence(persistence2);

    // 则: 任务被恢复, 状态和输出保持一致
    let restored_task = restored_manager
        .get(&task_id)
        .expect("持久化的任务应被恢复");
    assert_eq!(restored_task.id, task_id);
    assert_eq!(restored_task.description, "persisted-task");
    assert_eq!(restored_task.status, TaskStatus::Completed { exit_code: 0 });
    assert!(restored_task.output.contains("some output"));
}

/// 当从磁盘加载 running 状态的任务时, 应被调谐为 stopped (重启后无活跃进程)。
#[test]
fn should_reconcile_running_tasks_as_stopped_on_load() {
    // 给定: 磁盘上有一个 running 状态的任务
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    let persistence = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));

    let manager = TaskManager::with_persistence(Arc::clone(&persistence));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "running-task".into(),
        "session-1".into(),
    );
    // 任务此时是 Running 状态, 已被持久化到磁盘

    // 当: 用同一持久化目录创建新的 TaskManager (模拟进程重启)
    let persistence2 = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let restarted_manager = TaskManager::with_persistence(persistence2);

    // 则: 任务被恢复, 但状态调谐为 Stopped
    let restored_task = restarted_manager.get(&task_id).expect("任务应被恢复");
    assert_eq!(
        restored_task.status,
        TaskStatus::Stopped,
        "重启后 running 任务应被调谐为 stopped"
    );

    // 磁盘上的状态也应被更新为 Stopped
    let on_disk: mirri_core::task::AgentTask = serde_json::from_str(
        &std::fs::read_to_string(tasks_dir.join(format!("{task_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
}

// ============================================================
// B1 T3: 优雅停止 — 宽限期 + 自然定局优先 + 后台进程真实可杀
// ============================================================

/// 当挂接的信号让任务在宽限窗口内自然定局时, stop_with_grace 应保留
/// 自然终态 — 而不是过早翻转成 killed。
#[tokio::test]
async fn should_keep_the_natural_terminal_status_when_graceful_stop_settles_in_time() {
    use mirri_core::cancel::CancelToken;
    use mirri_core::task::TaskManager;

    let tasks = Arc::new(TaskManager::new());
    let id = tasks.register(
        TaskKind::Agent {
            agent_id: Some("agent-1".into()),
            subagent_type: Some("coder".into()),
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "quick".into(),
        "session-1".into(),
    );
    let signal = CancelToken::new();
    tasks.attach_signal(&id, signal.clone());
    // 模拟子代理观察到取消后 50ms 自然定局。
    let mgr = Arc::clone(&tasks);
    let id_clone = id.clone();
    tokio::spawn(async move {
        signal.cancelled().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        mgr.complete(&id_clone, 0);
    });

    tasks.stop_with_grace(&id, Duration::from_secs(2)).await;

    assert_eq!(
        tasks.get(&id).unwrap().status,
        TaskStatus::Completed { exit_code: 0 },
        "graceful stop must not flip a task that settles naturally in-window"
    );
}

/// 当任务在宽限期内从不定局时, stop_with_grace 应升级为 Stopped
/// (TS forceStop 语义)。挂接一个永不 settle 的信号, 真正走过等待路径。
#[tokio::test]
async fn should_escalate_to_stopped_when_grace_expires_without_a_settlement() {
    use mirri_core::cancel::CancelToken;

    let tasks = TaskManager::new();
    let id = tasks.register(
        TaskKind::Process { command: None, dispatcher: None },
        "stuck".into(),
        "session-1".into(),
    );
    // 挂接信号但 runner 从不定局 —— 等待路径必须走满 grace 后升级。
    tasks.attach_signal(&id, CancelToken::new());
    let started = std::time::Instant::now();
    tasks.stop_with_grace(&id, Duration::from_millis(150)).await;
    assert_eq!(tasks.get(&id).unwrap().status, TaskStatus::Stopped);
    assert!(
        started.elapsed() >= Duration::from_millis(140),
        "the escalation must wait out the grace window before flipping (took {:?})",
        started.elapsed()
    );
}

/// 当 stop_all_with_grace 面对混合任务集时, 已定局者保留原终态、
/// 挂死者升级 Stopped。
#[tokio::test]
async fn should_stop_only_unsettled_tasks_when_stopping_all_with_grace() {
    let tasks = TaskManager::new();
    let settled = tasks.register(
        TaskKind::Process { command: None, dispatcher: None },
        "settled".into(),
        "session-1".into(),
    );
    let stuck = tasks.register(
        TaskKind::Process { command: None, dispatcher: None },
        "stuck".into(),
        "session-1".into(),
    );
    tasks.complete(&settled, 0);

    tasks.stop_all_with_grace(Duration::from_millis(100)).await;

    assert_eq!(
        tasks.get(&settled).unwrap().status,
        TaskStatus::Completed { exit_code: 0 }
    );
    assert_eq!(tasks.get(&stuck).unwrap().status, TaskStatus::Stopped);
}

/// 当真实的后台 `sleep 60` 进程任务被宽限停止时, OS 进程应被真实终止
/// (SIGTERM→SIGKILL 经 mirri-os) — 修复前的代码只翻转状态并留下孤儿进程。
#[cfg(unix)]
#[tokio::test]
async fn should_kill_the_real_process_when_a_background_bash_task_is_stopped() {
    use mirri_core::tools::bash::start_background_command;

    let process = Arc::new(mirri_os::LocalProcess::new());
    let tasks = Arc::new(TaskManager::new());
    let started = start_background_command(
        process.clone(),
        "sleep 60".into(),
        None,
        None,
        false,
        "orphan check".into(),
        Arc::clone(&tasks),
        "session-1".into(),
        "agent-main".into(),
    )
    .await;
    let started_text = match &started.output {
        mirri_core::tool::ToolOutput::Text(text) => text.clone(),
        _ => panic!("expected text output"),
    };
    let task_id = started_text
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .expect("background result carries the task id")
        .to_string();

    let alive = |pid: i32| {
        mirri_os::probe_process(pid)
            .map(|probe| probe.alive)
            .unwrap_or(false)
    };
    // Identify the spawned process through the task's own fingerprint
    // instead of matching its command line over the whole machine: the
    // workspace suite runs every test binary in parallel, so a
    // `pgrep -f "sleep 60"` here also sees the `sleep 60` processes other
    // tests leave running (the orphan-governance keepAlive fixtures), and
    // the post-stop liveness assertion could never hold.
    let pid = {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(fingerprint) = tasks.get(&task_id).and_then(|task| task.fingerprint) {
                break fingerprint.pid;
            }
            if std::time::Instant::now() >= deadline {
                panic!("the background task must record its process fingerprint");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    // The command spawns on a detached task — wait for the process to
    // appear before asserting liveness (start latency, not a semantic
    // race).
    let appeared = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < appeared && !alive(pid) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        alive(pid),
        "the background process should be alive before the stop"
    );

    tasks
        .stop_with_grace(&task_id, Duration::from_secs(2))
        .await;

    // 进程组内 SIGTERM 已送达；宽限升级后必然退出。
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline && alive(pid) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !alive(pid),
        "the stopped background task's process must be dead"
    );
    // 清理兜底：按具体 PID kill，绝不用宽匹配。
    if alive(pid) {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
}

// ============================================================
// B1 T7: 会话关闭 — persisted-killed 语义核验 + keepAliveOnExit
// ============================================================

/// 构建一个带 tasks 持久化根的 SessionManager（tasks 落在
/// `<tmp>/.mirri/v3_sessions/<session>/tasks/`）。
async fn b1_manager() -> (
    mirri_core::SessionManager,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    use mirri_core::scope::{WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel};
    use mirri_core::{SessionManager, WorkspaceScope};

    let tmp = tempfile::tempdir().unwrap();
    let workspace = std::sync::Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig {
            trust_level: WorkspaceTrustLevel::Trusted,
            ..Default::default()
        },
    ));
    let sessions_dir = tmp.path().join(".mirri").join("v3_sessions");
    let manager = SessionManager::with_persistence_dir(
        workspace,
        sessions_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    (manager, tmp, sessions_dir)
}

/// 在持久化根下查找任务的 on-disk JSON 路径。
fn b1_task_json(sessions_dir: &std::path::Path, task_id: &str) -> std::path::PathBuf {
    let pattern = sessions_dir
        .join("**")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let mut results = Vec::new();
    fn walk(dir: &std::path::Path, name: &str, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, name, out);
                } else if path.file_name().map(|n| n == name).unwrap_or(false) {
                    out.push(path);
                }
            }
        }
    }
    walk(
        sessions_dir,
        pattern.file_name().unwrap().to_str().unwrap(),
        &mut results,
    );
    assert_eq!(results.len(), 1, "expected exactly one persisted task file");
    results.remove(0)
}

/// 当会话关闭 (keep_alive_on_exit = false) 时, 运行中的任务应落 Stopped
/// 并已持久化; 从同一持久化根重新加载不再二次调谐 (无复活), 且 close 后
/// 迟到的自然定局不改变 Stopped —— Rust 以「同步落盘 Stopped」实现 TS 的
/// lost 语义 (D-62 裁决: 不新增 lost wire 状态)。
#[tokio::test]
async fn should_persist_a_stopped_task_and_not_re_reconcile_it_when_a_session_closes() {
    use mirri_core::CloseReason;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, sessions_dir) = b1_manager().await;
    let metadata = mirri_core::scope::SessionMetadata::default();
    let session = manager
        .create_session(
            metadata,
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: false,
                    kill_grace: Duration::from_millis(200),
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let task_id = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "converged".into(),
        session_id.to_string(),
    );

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    // 断言 1: 内存态 Stopped, 磁盘 JSON status == stopped。
    assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    let on_disk: mirri_core::task::AgentTask = serde_json::from_str(
        &std::fs::read_to_string(b1_task_json(&sessions_dir, &task_id)).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
    let ended_at = on_disk.ended_at;

    // 断言 2: 同根重载 → 仍 Stopped, ended_at 不变 (重启不再触碰)。
    let tasks_dir = b1_task_json(&sessions_dir, &task_id)
        .parent()
        .unwrap()
        .to_path_buf();
    let reloaded = TaskManager::with_persistence(std::sync::Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    )));
    let reloaded_task = reloaded.get(&task_id).unwrap();
    assert_eq!(reloaded_task.status, TaskStatus::Stopped);
    assert_eq!(reloaded_task.ended_at, ended_at);

    // 断言 3: close 后迟到的自然定局不改变 Stopped (first-terminal-wins 跨
    // close 存活)。
    tasks.complete(&task_id, 0);
    assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    drop(tmp);
}

/// 当 keep_alive_on_exit = true 时, 会话关闭应跳过任务收敛: 任务保持
/// Running, 随后仍可自然定局; 仍 running 的任务在下次重载时被调谐为
/// Stopped (TS lost 的重启等价物)。
#[tokio::test]
async fn should_skip_task_convergence_when_keep_alive_on_exit_is_enabled() {
    use mirri_core::CloseReason;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, sessions_dir) = b1_manager().await;
    let session = manager
        .create_session(
            mirri_core::scope::SessionMetadata::default(),
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: true,
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let settleable = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "keeps running".into(),
        session_id.to_string(),
    );
    let stuck = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "stays running".into(),
        session_id.to_string(),
    );

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    // 收敛被跳过: 两个任务仍 Running。
    assert_eq!(tasks.get(&settleable).unwrap().status, TaskStatus::Running);
    assert_eq!(tasks.get(&stuck).unwrap().status, TaskStatus::Running);

    // keepAlive 关闭的豁免落盘 (SDR-013 d): 两个幸存的 Running 行都在
    // 内存中带 lifetime_exempt — 常驻寿命清扫器不得在会话 (及其 driver)
    // 消失后按年龄上限清杀它们。
    assert!(
        tasks.get(&settleable).unwrap().lifetime_exempt,
        "the surviving row must carry the lifetime exemption in memory"
    );
    assert!(
        tasks.get(&stuck).unwrap().lifetime_exempt,
        "the surviving row must carry the lifetime exemption in memory"
    );
    // ...且豁免已持久化到磁盘档案 — 清扫器跨重启判定的是档案, 不是内存。
    let settled_json =
        std::fs::read_to_string(b1_task_json(&sessions_dir, &stuck)).unwrap();
    let archived: serde_json::Value = serde_json::from_str(&settled_json).unwrap();
    assert_eq!(
        archived["lifetime_exempt"], serde_json::json!(true),
        "the exemption must be persisted in the archive the sweeper walks"
    );

    // 随后的自然定局仍然生效 (TaskManager Arc 被脱离会话的 runner 持有)。
    tasks.complete(&settleable, 0);
    assert_eq!(
        tasks.get(&settleable).unwrap().status,
        TaskStatus::Completed { exit_code: 0 }
    );

    // 重启语义: 仍 running 的任务重载时被调谐为 Stopped (TS lost 等价)。
    let tasks_dir = b1_task_json(&sessions_dir, &stuck)
        .parent()
        .unwrap()
        .to_path_buf();
    let reloaded = TaskManager::with_persistence(std::sync::Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    )));
    assert_eq!(reloaded.get(&stuck).unwrap().status, TaskStatus::Stopped);
    drop(tmp);
}

/// 当任务在关闭宽限窗口内自然定局时 (D-63 × close 路径集成), 其自然终态
/// 应被保留而非翻转成 killed。
#[tokio::test]
async fn should_keep_a_natural_settlement_when_the_session_close_grace_window_settles() {
    use mirri_core::CloseReason;
    use mirri_core::cancel::CancelToken;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, _sessions_dir) = b1_manager().await;
    let session = manager
        .create_session(
            mirri_core::scope::SessionMetadata::default(),
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: false,
                    kill_grace: Duration::from_secs(2),
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let task_id = tasks.register(
        TaskKind::Agent {
            agent_id: Some("agent-quick".into()),
            subagent_type: Some("coder".into()),
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "settles in window".into(),
        session_id.to_string(),
    );
    let signal = CancelToken::new();
    tasks.attach_signal(&task_id, signal.clone());
    // 模拟 runner 观察取消后 50ms 自然定局。
    let runner_tasks = tasks.clone();
    let runner_id = task_id.clone();
    tokio::spawn(async move {
        signal.cancelled().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        runner_tasks.complete(&runner_id, 0);
    });

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    assert_eq!(
        tasks.get(&task_id).unwrap().status,
        TaskStatus::Completed { exit_code: 0 },
        "a task that settles inside the close grace window keeps its natural status"
    );
    drop(tmp);
}
