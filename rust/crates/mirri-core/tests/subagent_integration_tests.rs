//! End-to-end tests exercising [`SubagentTask::start`] through a real
//! [`TaskManager`].
//!
//! These tests bridge the subagent lifecycle into session-level task
//! tracking via a [`TaskSink`] adapter that writes into a [`TaskManager`],
//! verifying that completion, panic failure, and cancellation all land in
//! the manager with the expected terminal status.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use mirri_core::cancel::CancelToken;
use mirri_core::hook_engine::{HookDef, HookEventType};
use mirri_core::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, ModelCapability};
use mirri_core::subagent::{
    SessionSubagentHost, SpawnRequest, SubagentCompletion, SubagentHandle, SubagentLlmFactory,
    SubagentTask, TaskSink,
};
use mirri_core::task::{TaskKind, TaskManager, TaskStatus};
use mirri_core::{
    AgentId, AgentProfile, AgentScopeConfig, EngineResult, PermissionMode, ProfileManager,
    ReverseRpcCallback, SessionId, SessionMetadata, SessionScope, SessionScopeConfig, SkillCatalog,
    SubagentHost, TurnFlowConfig,
};
use serde_json::{Value, json};

/// A [`TaskSink`] adapter backed by a [`TaskManager`].
///
/// Mirrors how the host bridges subagent tasks into the session's task
/// tracking: output is appended to the registered task, and settlement is
/// forwarded to the manager's `complete` / `fail` / `stop` methods.
struct TaskManagerSink {
    task_id: String,
    manager: Arc<TaskManager>,
    cancel: CancelToken,
}

impl TaskManagerSink {
    fn new(task_id: String, manager: Arc<TaskManager>, cancel: CancelToken) -> Self {
        Self {
            task_id,
            manager,
            cancel,
        }
    }
}

impl TaskSink for TaskManagerSink {
    fn append_output(&mut self, data: &str) {
        self.manager.append_output(&self.task_id, data);
    }

    fn settle(&mut self, status: TaskStatus, _stop_reason: Option<String>) {
        match status {
            TaskStatus::Completed { exit_code } => self.manager.complete(&self.task_id, exit_code),
            TaskStatus::Failed(message) => self.manager.fail(&self.task_id, message),
            TaskStatus::Stopped => self.manager.stop(&self.task_id),
            // A subagent task never settles as Running; nothing to record.
            TaskStatus::Running => {}
        }
    }

    fn signal(&self) -> &CancelToken {
        &self.cancel
    }
}

/// Create a handle whose completion never resolves, for cancellation tests.
fn never_completing_handle(agent_id: &str) -> SubagentHandle {
    SubagentHandle::new(
        agent_id.to_string(),
        "coder".to_string(),
        None,
        Box::pin(std::future::pending::<Result<SubagentCompletion, _>>()),
    )
}

#[tokio::test]
async fn should_record_output_in_task_manager_when_subagent_completes() {
    // Given a TaskManager with a registered task and a sink adapter writing to it
    let manager = Arc::new(TaskManager::new());
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "subagent task".into(),
        "session-e2e".into(),
    );
    let mut sink = TaskManagerSink::new(task_id.clone(), Arc::clone(&manager), CancelToken::new());
    let handle = SubagentHandle::from_result(
        "agent-e2e".into(),
        "coder".into(),
        None,
        "hello from subagent".into(),
    );
    let mut task = SubagentTask::new(handle, "subagent task".into(), CancelToken::new());

    // When the subagent task is started
    task.start(&mut sink).await;

    // Then the TaskManager records the completed status and the subagent's output
    let recorded = manager
        .get(&task_id)
        .expect("registered task is retrievable");
    assert_eq!(recorded.status, TaskStatus::Completed { exit_code: 0 });
    assert!(
        recorded.output.contains("hello from subagent"),
        "task output should contain the subagent result, got: {}",
        recorded.output
    );
}

#[tokio::test]
async fn should_record_failure_in_task_manager_when_subagent_panics() {
    // Given a TaskManager with a registered task and a subagent whose
    // completion future panics
    let manager = Arc::new(TaskManager::new());
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "panicking subagent".into(),
        "session-e2e".into(),
    );
    let mut sink = TaskManagerSink::new(task_id.clone(), Arc::clone(&manager), CancelToken::new());
    let handle = SubagentHandle::new(
        "agent-panic".into(),
        "coder".into(),
        None,
        Box::pin(async move {
            std::panic::panic_any("subagent engine exploded");
            #[allow(unreachable_code)]
            Ok(SubagentCompletion::from_text("never reached"))
        }),
    );
    let mut task = SubagentTask::new(handle, "panicking subagent".into(), CancelToken::new());

    // When the subagent task is started
    task.start(&mut sink).await;

    // Then the TaskManager records the failed status with the panic message
    let recorded = manager
        .get(&task_id)
        .expect("registered task is retrievable");
    match &recorded.status {
        TaskStatus::Failed(message) => {
            assert!(
                message.contains("subagent engine exploded"),
                "failure message should carry the panic text, got: {message}"
            );
        }
        other => panic!("expected Failed status, got {other:?}"),
    }
}

#[tokio::test]
async fn should_record_subagent_as_stopped_when_sink_cancel_token_is_cancelled() {
    // Given a TaskManager with a registered task, a never-completing
    // subagent, and a sink whose cancel token is already cancelled
    let manager = Arc::new(TaskManager::new());
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "cancellable subagent".into(),
        "session-e2e".into(),
    );
    let sink_cancel = CancelToken::new();
    sink_cancel.cancel();
    let mut sink = TaskManagerSink::new(task_id.clone(), Arc::clone(&manager), sink_cancel);
    let handle = never_completing_handle("agent-cancel-sink");
    let mut task = SubagentTask::new(handle, "cancellable subagent".into(), CancelToken::new());

    // When the subagent task is started
    task.start(&mut sink).await;

    // Then the TaskManager records the stopped status
    let recorded = manager
        .get(&task_id)
        .expect("registered task is retrievable");
    assert_eq!(recorded.status, TaskStatus::Stopped);
}

#[tokio::test]
async fn should_record_subagent_as_stopped_when_task_cancel_token_is_cancelled() {
    // Given a TaskManager with a registered task, a never-completing
    // subagent whose own cancel token is already cancelled, and a sink with
    // a fresh (uncancelled) token
    let manager = Arc::new(TaskManager::new());
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "cancellable subagent".into(),
        "session-e2e".into(),
    );
    let mut sink = TaskManagerSink::new(task_id.clone(), Arc::clone(&manager), CancelToken::new());
    let handle = never_completing_handle("agent-cancel-task");
    let task_cancel = CancelToken::new();
    task_cancel.cancel();
    let mut task = SubagentTask::new(handle, "cancellable subagent".into(), task_cancel);

    // When the subagent task is started
    task.start(&mut sink).await;

    // Then the TaskManager records the stopped status
    let recorded = manager
        .get(&task_id)
        .expect("registered task is retrievable");
    assert_eq!(recorded.status, TaskStatus::Stopped);
}

// ---------------------------------------------------------------------------
// ADR-009 (audit P0-1): the subagent turn must wire the parent's approval
// callback and interaction kernel, so Manual-mode approvals reach the
// client instead of being hard-rejected locally.
// ---------------------------------------------------------------------------

/// A real session scope without a workspace root (file tools pass paths
/// through) — the scaffolding the host-based tests below share.
fn make_session() -> Arc<SessionScope> {
    make_session_with_hooks(Vec::new())
}

/// A real session scope with the given external command hooks registered.
fn make_session_with_hooks(hooks: Vec<HookDef>) -> Arc<SessionScope> {
    Arc::new(SessionScope::new(
        SessionId::new("session-test"),
        SessionMetadata::default(),
        SessionScopeConfig {
            hooks,
            ..SessionScopeConfig::default()
        },
        Arc::new(SkillCatalog::new()),
        None,
        None,
    ))
}

/// A profile manager with a resolvable `coder` profile on the scripted
/// model, so the host's child LLM factory receives the script. The
/// essential profile is overridden to allowlist `coder` — default-deny
/// would refuse the spawn (the user-configured `subagents` face).
fn make_profile_manager() -> Arc<ProfileManager> {
    let manager = Arc::new(ProfileManager::new());
    manager
        .register(AgentProfile {
            name: "coder".to_string(),
            model: "scripted-model".to_string(),
            ..AgentProfile::default()
        })
        .unwrap();
    manager
        .register(AgentProfile {
            name: "agent".to_string(),
            subagents: Some(vec!["coder".to_string()]),
            ..AgentProfile::default()
        })
        .unwrap();
    manager
}

/// An LLM that walks a scripted queue of responses (each chat call pops the
/// next script entry, consumed back-to-front — the FIRST response is the
/// LAST element, matching the host's embedded-test helper).
struct ScriptedLlm {
    script: Mutex<Vec<Vec<LlmEvent>>>,
    capability: ModelCapability,
}

impl Llm for ScriptedLlm {
    fn system_prompt(&self) -> &str {
        ""
    }
    fn model_name(&self) -> &str {
        "test-model"
    }
    fn capability(&self) -> &ModelCapability {
        &self.capability
    }
    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let events = self
            .script
            .lock()
            .unwrap()
            .pop()
            .expect("the scripted LLM ran out of responses");
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// A factory that hands the child model a [`ScriptedLlm`] walking `script`
/// and no other model a turn (an unexpected second model panics on an empty
/// script).
struct ScriptedLlmFactory {
    script: Vec<Vec<LlmEvent>>,
    dispatching_model: &'static str,
}

impl SubagentLlmFactory for ScriptedLlmFactory {
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        if config.model_id == self.dispatching_model {
            Ok(Arc::new(ScriptedLlm {
                script: Mutex::new(self.script.clone()),
                capability: ModelCapability::default(),
            }))
        } else {
            Err(format!(
                "unexpected model in scripted subagent test: {}",
                config.model_id
            ))
        }
    }
}

/// A single scripted response: one assistant text part, done.
fn text_response(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextPart {
            text: text.to_string(),
        },
        LlmEvent::Done {
            stop_reason: "completed".to_string(),
        },
    ]
}

/// A single scripted response: one Write tool call, done (tool-use stop so
/// the loop executes it).
fn write_tool_response(path: &std::path::Path) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolCall {
            tool: "Write".to_string(),
            args: json!({
                "path": path.to_string_lossy(),
                "content": "hello from the subagent"
            }),
            call_id: "call-write-1".to_string(),
        },
        LlmEvent::Done {
            stop_reason: "tool_calls".to_string(),
        },
    ]
}

/// A reverse-RPC callback that records every `approval.ask` payload and
/// answers `approved` — the client contract the production transport
/// implements (hooks.rs: the TS `ApprovalResponse` shape).
struct RecordingApprovalCallback {
    calls: Mutex<Vec<(String, Value)>>,
}

impl RecordingApprovalCallback {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ReverseRpcCallback for RecordingApprovalCallback {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        Ok(json!({ "decision": "approved" }))
    }
}

/// Spawn a `coder` subagent through the given host.
fn spawn_with_host(host: &SessionSubagentHost, cancel: CancelToken) -> SubagentHandle {
    host.spawn(SpawnRequest {
        profile_name: "coder".to_string(),
        prompt: "write the file".to_string(),
        description: "test spawn".to_string(),
        model: None,
        cancel,
        run_in_background: false,
        parent_tool_call_id: None,
        swarm_index: None,
        swarm_item: None,
        suppress_rate_limit_failure_event: false,
    })
    .unwrap()
}

/// Given a parent host config carrying a recording reverse-RPC callback
/// under Manual permission mode, when a subagent's turn calls Write,
/// then the approval.ask reaches the callback (not a local hard rejection)
/// and the approved Write actually executes.
#[tokio::test(flavor = "multi_thread")]
async fn should_route_subagent_approval_ask_to_parent_callback() {
    // Given a session whose host inherits a recording approval callback and
    // Manual permission mode (the session default), and a subagent whose
    // scripted turn calls Write
    let tmp = tempfile::tempdir().unwrap();
    let target_path = tmp.path().join("hello.txt");
    let session = make_session();
    let callback = Arc::new(RecordingApprovalCallback::new());
    let host = SessionSubagentHost::new(
        Arc::downgrade(&session),
        make_profile_manager(),
        AgentId::new("main"),
        AgentScopeConfig {
            permission_mode: PermissionMode::Manual,
            reverse_rpc_slot: Some(mirri_core::reverse_rpc::ReverseRpcSlot::new(RwLock::new(
                Some(Arc::clone(&callback) as Arc<dyn ReverseRpcCallback>),
            ))),
            ..AgentScopeConfig::default()
        },
        Arc::new(ScriptedLlmFactory {
            script: vec![
                text_response("write done"),
                write_tool_response(&target_path),
            ],
            dispatching_model: "scripted-model",
        }),
        TurnFlowConfig::default(),
        None,
        None,
    );

    // When the subagent's turn runs (the Write call requires approval)
    let mut handle = spawn_with_host(&host, CancelToken::new());
    let completion = tokio::time::timeout(Duration::from_secs(10), &mut handle.completion)
        .await
        .expect("subagent turn completes without deadlock")
        .expect("subagent turn succeeds");

    // Then approval.ask reached the parent's callback, named Write ...
    let calls = callback.calls.lock().unwrap();
    let ask = calls
        .iter()
        .find(|(method, _)| method == "approval.ask")
        .unwrap_or_else(|| panic!("approval.ask must reach the parent callback, got: {calls:?}"));
    assert_eq!(ask.1["tool_name"], "Write");
    // ... and the approved Write actually executed.
    assert_eq!(
        std::fs::read_to_string(&target_path).expect("target file written"),
        "hello from the subagent"
    );
    assert_eq!(completion.result, "write done");
}

// ---------------------------------------------------------------------------
// ADR-009 (audit P0-1, serve deployment): a child spawned while the parent
// agent is LIVE in the session must inherit the parent's LIVE reverse-RPC
// callback — the serve transport installs it into the session artifacts at
// subscribe, strictly later than the host's create-time snapshot — not the
// stale snapshot value (which may be None, hard-rejecting the child's Write
// locally).
// ---------------------------------------------------------------------------

/// Given a host whose create-time config snapshot carries NO reverse-RPC
/// callback while its parent agent is registered in the session with a LIVE
/// callback, when a subagent's turn calls Write, then the approval.ask
/// reaches the parent's LIVE callback (not a local hard rejection), the
/// approved Write executes, and the child's own config carries the same
/// callback Arc the live parent holds (so a grandchild inherits it too).
#[tokio::test(flavor = "multi_thread")]
async fn should_inherit_live_parent_callback_over_the_stale_snapshot_when_spawning() {
    // Given a session whose parent agent "main" is registered with a
    // recording approval callback in its LIVE config (the serve transport's
    // subscribe-time stamp), while the host's create-time snapshot carries
    // no callback at all (the snapshot predates the subscribe)
    let tmp = tempfile::tempdir().unwrap();
    let target_path = tmp.path().join("hello.txt");
    let session = make_session();
    let callback = Arc::new(RecordingApprovalCallback::new());
    session
        .create_agent(
            AgentId::new("main"),
            AgentScopeConfig {
                permission_mode: PermissionMode::Manual,
                reverse_rpc_slot: Some(Arc::new(std::sync::RwLock::new(Some(
                    Arc::clone(&callback) as Arc<dyn ReverseRpcCallback>,
                )))),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let host = SessionSubagentHost::new(
        Arc::downgrade(&session),
        make_profile_manager(),
        AgentId::new("main"),
        AgentScopeConfig {
            permission_mode: PermissionMode::Manual,
            reverse_rpc_slot: None,
            ..AgentScopeConfig::default()
        },
        Arc::new(ScriptedLlmFactory {
            script: vec![
                text_response("write done"),
                write_tool_response(&target_path),
            ],
            dispatching_model: "scripted-model",
        }),
        TurnFlowConfig::default(),
        None,
        None,
    );

    // When the subagent's turn runs (the Write call requires approval)
    let mut handle = spawn_with_host(&host, CancelToken::new());
    let completion = tokio::time::timeout(Duration::from_secs(10), &mut handle.completion)
        .await
        .expect("subagent turn completes without deadlock")
        .expect("subagent turn succeeds");

    // Then approval.ask reached the parent's LIVE callback, named Write ...
    // (snapshot the calls so the mutex guard ends before any await below)
    let calls: Vec<(String, Value)> = callback.calls.lock().unwrap().clone();
    let ask = calls
        .iter()
        .find(|(method, _)| method == "approval.ask")
        .unwrap_or_else(|| panic!("approval.ask must reach the parent callback, got: {calls:?}"));
    assert_eq!(ask.1["tool_name"], "Write");
    // ... the approved Write actually executed ...
    assert_eq!(
        std::fs::read_to_string(&target_path).expect("target file written"),
        "hello from the subagent"
    );
    assert_eq!(completion.result, "write done");
    // ... and the child's own config carries the SAME callback Arc as the
    // live parent (a later grandchild spawn inherits it the same way).
    let child = session
        .get_agent(&AgentId::new(&handle.agent_id))
        .await
        .expect("child agent is registered in the session");
    let child_callback = child
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .reverse_rpc_slot
        .as_ref()
        .and_then(|slot| slot.read().unwrap_or_else(|e| e.into_inner()).clone());
    assert!(
        child_callback
            .map(|c| Arc::ptr_eq(&c, &(Arc::clone(&callback) as Arc<dyn ReverseRpcCallback>)))
            .unwrap_or(false),
        "the child config must inherit the parent's live callback Arc"
    );
}

// ---------------------------------------------------------------------------
// spec 11 R5/A3 (defect B/C): the three-tier chain must be reachable — Tier
// 3 (inherit the dispatcher's current model) reads the LIVE parent config,
// and the profile default (Tier 2) wins over the parent's model.
// ---------------------------------------------------------------------------

/// Spawn the given profile through the host — the three-tier tests' spawn
/// face (the host's `spawn_with_host` is pinned to `coder`).
fn spawn_with_profile(
    host: &SessionSubagentHost,
    profile_name: &str,
    cancel: CancelToken,
) -> SubagentHandle {
    host.spawn(SpawnRequest {
        profile_name: profile_name.to_string(),
        prompt: "write the file".to_string(),
        description: "test spawn".to_string(),
        model: None,
        cancel,
        run_in_background: false,
        parent_tool_call_id: None,
        swarm_index: None,
        swarm_item: None,
        suppress_rate_limit_failure_event: false,
    })
    .unwrap()
}

/// A fixed catalog for the three-nothing spawn-error integration test.
struct StaticModelCatalog {
    entries: Vec<mirri_core::ModelCatalogEntry>,
}

impl mirri_core::ModelCatalog for StaticModelCatalog {
    fn list_models(&self) -> Vec<mirri_core::ModelCatalogEntry> {
        self.entries.clone()
    }
    fn get_by_id(&self, id: &str) -> Option<mirri_core::ModelCatalogEntry> {
        self.entries.iter().find(|e| e.id == id).cloned()
    }
}

/// Given a parent agent that switches models AFTER the host was built, when
/// a child is spawned without a model and its profile has no default, then
/// the child runs on the parent's LIVE (new) model — defect B's direct
/// end-to-end assertion (spec 11 A3).
#[tokio::test(flavor = "multi_thread")]
async fn should_inherit_live_parent_model_when_spawned_without_model() {
    // Given a session whose parent agent "main" starts on "model-a", a
    // profile with no default model, and a host built over the create-time
    // snapshot (still "model-a") but bound to the parent's LIVE config
    // handle
    let session = make_session();
    session
        .create_agent(
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-a".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .expect("main agent");
    let main = session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent registered");
    let manager = Arc::new(ProfileManager::new());
    manager
        .register(AgentProfile {
            name: "coder-nomodel".to_string(),
            ..AgentProfile::default()
        })
        .unwrap();
    // Default-deny: allowlist the test profile on the essential main agent.
    manager
        .register(AgentProfile {
            name: "agent".to_string(),
            subagents: Some(vec!["coder-nomodel".to_string()]),
            ..AgentProfile::default()
        })
        .unwrap();
    let host = SessionSubagentHost::new(
        Arc::downgrade(&session),
        manager,
        AgentId::new("main"),
        AgentScopeConfig {
            model_id: "model-a".to_string(),
            ..AgentScopeConfig::default()
        },
        Arc::new(ScriptedLlmFactory {
            script: vec![text_response("done in the live parent model")],
            dispatching_model: "model-b",
        }),
        TurnFlowConfig::default(),
        None,
        None,
    );
    host.set_parent_config_shared(main.config());

    // When the parent switches to "model-b" and a child is spawned without
    // a model (the stale snapshot would have handed the child "model-a")
    main.set_model("model-b".to_string());
    let mut handle = spawn_with_profile(&host, "coder-nomodel", CancelToken::new());
    let completion = tokio::time::timeout(Duration::from_secs(10), &mut handle.completion)
        .await
        .expect("subagent turn completes without deadlock")
        .expect("subagent turn succeeds");

    // Then the child's LLM was built on "model-b" (the scripted factory only
    // accepts its dispatching model) and the child's config carries it
    assert_eq!(completion.result, "done in the live parent model");
    let child = session
        .get_agent(&AgentId::new(&handle.agent_id))
        .await
        .expect("child agent registered");
    assert_eq!(
        child
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "model-b",
        "the child config must carry the parent's live model"
    );
}

/// Given a profile that declares a default model, when a child is spawned
/// without a model while the parent carries a different live model, then the
/// profile's default wins over the parent's model — defect C's direct
/// end-to-end assertion (spec 11 A5).
#[tokio::test(flavor = "multi_thread")]
async fn should_use_profile_default_model_over_parent_model() {
    // Given a session whose parent agent runs "model-a", a host bound to the
    // parent's LIVE config, and a profile declaring a default model
    let session = make_session();
    session
        .create_agent(
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-a".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .expect("main agent");
    let main = session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent registered");
    let manager = Arc::new(ProfileManager::new());
    manager
        .register(AgentProfile {
            name: "coder-default-model".to_string(),
            default_model: Some("profile-default-model".to_string()),
            ..AgentProfile::default()
        })
        .unwrap();
    // Default-deny: allowlist the test profile on the essential main agent.
    manager
        .register(AgentProfile {
            name: "agent".to_string(),
            subagents: Some(vec!["coder-default-model".to_string()]),
            ..AgentProfile::default()
        })
        .unwrap();
    let host = SessionSubagentHost::new(
        Arc::downgrade(&session),
        manager,
        AgentId::new("main"),
        AgentScopeConfig {
            model_id: "model-a".to_string(),
            ..AgentScopeConfig::default()
        },
        Arc::new(ScriptedLlmFactory {
            script: vec![text_response("done on the profile default model")],
            dispatching_model: "profile-default-model",
        }),
        TurnFlowConfig::default(),
        None,
        None,
    );
    host.set_parent_config_shared(main.config());

    // When the child is spawned without a model
    let mut handle = spawn_with_profile(&host, "coder-default-model", CancelToken::new());
    let completion = tokio::time::timeout(Duration::from_secs(10), &mut handle.completion)
        .await
        .expect("subagent turn completes without deadlock")
        .expect("subagent turn succeeds");

    // Then the profile's default model reached the child — the parent's live
    // model did not override it
    assert_eq!(completion.result, "done on the profile default model");
    let child = session
        .get_agent(&AgentId::new(&handle.agent_id))
        .await
        .expect("child agent registered");
    assert_eq!(
        child
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "profile-default-model",
        "the child config must carry the profile's default model"
    );
}

/// Given a host whose parent live config has no current model, a profile
/// with no default, and no explicit model, when a child is spawned, then the
/// dispatch is refused with the available-model listing (spec 11 A6).
#[test]
fn should_error_with_available_models_when_all_three_tiers_empty() {
    // Given a host (refusal is synchronous, so no session) whose parent live
    // handle carries an empty model and whose catalog exposes two aliases
    let shared = Arc::new(std::sync::RwLock::new(AgentScopeConfig::default()));
    let manager = Arc::new(ProfileManager::new());
    manager
        .register(AgentProfile {
            name: "coder-nomodel".to_string(),
            ..AgentProfile::default()
        })
        .unwrap();
    // Default-deny: allowlist the test profile on the essential main agent.
    manager
        .register(AgentProfile {
            name: "agent".to_string(),
            subagents: Some(vec!["coder-nomodel".to_string()]),
            ..AgentProfile::default()
        })
        .unwrap();
    let host = SessionSubagentHost::new(
        std::sync::Weak::new(),
        manager,
        AgentId::new("main"),
        AgentScopeConfig::default(),
        Arc::new(ScriptedLlmFactory {
            script: vec![text_response("should never run")],
            dispatching_model: "unused",
        }),
        TurnFlowConfig::default(),
        Some(Arc::new(StaticModelCatalog {
            entries: vec![
                mirri_core::ModelCatalogEntry {
                    id: "sonnet".to_string(),
                    model: "claude-sonnet-4".to_string(),
                    provider: "test".to_string(),
                    description: None,
                    max_context_size: None,
                    capabilities: Vec::new(),
                    default_effort: None,
                },
                mirri_core::ModelCatalogEntry {
                    id: "opus".to_string(),
                    model: "claude-opus-4".to_string(),
                    provider: "test".to_string(),
                    description: None,
                    max_context_size: None,
                    capabilities: Vec::new(),
                    default_effort: None,
                },
            ],
        })),
        None,
    );
    host.set_parent_config_shared(shared);

    // When the child is spawned without a model
    let error = match host.spawn(SpawnRequest {
        profile_name: "coder-nomodel".to_string(),
        prompt: "write the file".to_string(),
        description: "test spawn".to_string(),
        model: None,
        cancel: CancelToken::new(),
        run_in_background: false,
        parent_tool_call_id: None,
        swarm_index: None,
        swarm_item: None,
        suppress_rate_limit_failure_event: false,
    }) {
        Err(error) => error,
        Ok(_) => panic!("the three-nothing dispatch must be refused"),
    };

    // Then the error carries the available-model listing
    let message = error.to_string();
    assert!(
        message.contains("no model resolved for subagent 'coder-nomodel'"),
        "should name the profile in the error: {message}"
    );
    assert!(
        message.contains("sonnet"),
        "should list valid model aliases: {message}"
    );
    assert!(
        message.contains("opus"),
        "should list valid model aliases: {message}"
    );
}

// ---------------------------------------------------------------------------
// ADR-010 (audit P2-2): a subagent task must carry the turn's terminal stop
// reason — a turn that ends blocked by a hook is a FAILURE, not a success.
// ---------------------------------------------------------------------------

/// Given a subagent turn that ends blocked by a before-step hook, when the
/// task settles, then its status is failed with the reason named.
#[tokio::test(flavor = "multi_thread")]
async fn should_settle_failed_when_subagent_turn_blocked_by_hook() {
    // Given a session whose PreLlmRequest hook always blocks (exit code 2 —
    // the hook engine's Block signal), a subagent to run, and a TaskManager
    // tracking it as a task
    let session =
        make_session_with_hooks(vec![HookDef::new(HookEventType::PreLlmRequest, "exit 2")]);
    let host = SessionSubagentHost::new(
        Arc::downgrade(&session),
        make_profile_manager(),
        AgentId::new("main"),
        AgentScopeConfig::default(),
        Arc::new(ScriptedLlmFactory {
            // The turn is blocked before the first LLM call, so this script
            // is never consumed; it only guards against a non-blocking
            // regression silently completing the turn.
            script: vec![text_response("should never run")],
            dispatching_model: "scripted-model",
        }),
        TurnFlowConfig::default(),
        None,
        None,
    );
    let manager = Arc::new(TaskManager::new());
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "blocked subagent".into(),
        "session-e2e".into(),
    );
    let mut sink = TaskManagerSink::new(task_id.clone(), Arc::clone(&manager), CancelToken::new());
    let handle = spawn_with_host(&host, CancelToken::new());
    let mut task = SubagentTask::new(handle, "blocked subagent".into(), CancelToken::new());

    // When the subagent task settles
    task.start(&mut sink).await;

    // Then the TaskManager records the failed status with the reason named
    let recorded = manager
        .get(&task_id)
        .expect("registered task is retrievable");
    match &recorded.status {
        TaskStatus::Failed(message) => {
            assert!(
                message.contains("blocked"),
                "failure message should name the hook block, got: {message}"
            );
        }
        other => panic!("expected Failed status, got {other:?}"),
    }
}
