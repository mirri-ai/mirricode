//! End-to-end tests for the built-in tools.
//!
//! These tests drive the real tool pipeline — `preflight_tool_call` (schema
//! validation) → `prepare_tool_call` (resolve_execution + NoopHooks) →
//! execute — against real files, processes, and session services.
//!
//! Test naming follows the Given-When-Then convention.

use std::sync::Arc;

use mirri_collaboration::CollaborationManager;
use mirri_core::cancel::CancelToken;
use mirri_core::hooks::NoopHooks;
use mirri_core::tool::{ExecutableToolContext, ToolOutput, ToolResult};
use mirri_core::tool_registry::{
    PreflightResult, ToolManager, ValidatorCache, preflight_tool_call, prepare_tool_call,
};
use mirri_core::tools::{BuiltinToolServices, WorkspacePaths, register_builtin_tools};
use mirri_core::{
    CronManager, DisclosureState, GoalManager, PlanManager, SessionWireLog, SkillCatalog, TaskKind,
    TaskManager, TodoList, TodoStatus, ToolSelectService,
};

/// Build the standard built-in tool services (fresh session-scoped state).
fn fresh_services() -> BuiltinToolServices {
    BuiltinToolServices {
        todo_list: Arc::new(TodoList::new()),
        tasks: Arc::new(TaskManager::new()),
        goal: Arc::new(GoalManager::new()),
        plan: Arc::new(PlanManager::new()),
        cron: Arc::new(CronManager::new()),
        collaboration: Arc::new(CollaborationManager::new()),
        skill_catalog: Arc::new(SkillCatalog::new()),
        tool_select: Arc::new(std::sync::Mutex::new(ToolSelectService::new())),
        reverse_rpc_slot: None,
        interaction_kernel: None,
        subagent_host: None,
        web_search_provider: None,
        fetch_url_provider: None,
        session_id: "test-session".to_string(),
        agent_id: "agent-main".to_string(),
        workspace_paths: None,
        wire_recorder: SessionWireLog::default(),
        event_bus: None,
        io: mirri_core::tools::ToolIo::default(),
        background_gate: Arc::new(|| true),
        subagent_timeout: None,
        max_swarm_items: None,
        question_channel_wait: std::time::Duration::from_secs(300),
    }
}

/// A tool manager with every built-in tool registered over fresh
/// session-scoped services, wrapped the same way
/// [`AgentScope`](mirri_core::AgentScope) wraps it (`Arc<RwLock<..>>`).
/// Returns the services alongside so tests can inspect the state the tools
/// mutate.
fn fresh_tool_manager_with_services() -> (Arc<parking_lot::RwLock<ToolManager>>, BuiltinToolServices)
{
    let services = fresh_services();
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    (tools, services)
}

/// A tool manager with every built-in tool registered (services discarded).
fn fresh_tool_manager() -> Arc<parking_lot::RwLock<ToolManager>> {
    fresh_tool_manager_with_services().0
}

/// Run one tool call through preflight → prepare → execute.
///
/// The tool manager is only consulted inside the synchronous preflight; the
/// read guard is released before any `.await`, so no lock is held across an
/// await point. Schema-rejected calls panic (tests use [`preflight_result`]
/// for those).
async fn call_tool(
    tools: &Arc<parking_lot::RwLock<ToolManager>>,
    name: &str,
    args: serde_json::Value,
    validators: &mut ValidatorCache,
) -> ToolResult {
    let valid = {
        let guard = tools.read();
        match preflight_tool_call(name, &args.to_string(), "call-1", &guard, validators) {
            PreflightResult::Valid(valid) => valid,
            PreflightResult::Rejected(rejected) => {
                panic!("preflight rejected: {}", rejected.error);
            }
        }
    };
    let prepared = prepare_tool_call(valid, "turn-1", 0, &CancelToken::default(), &NoopHooks)
        .await
        .expect("prepare should not fail");
    match prepared {
        mirri_core::PrepareResult::Ready(prepared) => {
            let ctx = ExecutableToolContext {
                approval_metadata: None,
                wait_registry: None,
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                cancel: CancelToken::default(),
                progress: Default::default(),
            };
            (prepared.execute)(ctx).await
        }
        mirri_core::PrepareResult::Blocked(blocked) => {
            ToolResult::error(format!("blocked: {}", blocked.reason))
        }
        mirri_core::PrepareResult::Synthetic(synthetic) => synthetic.result,
    }
}

/// Run preflight only, returning the preflight result for rejection checks.
fn preflight_result(
    tools: &Arc<parking_lot::RwLock<ToolManager>>,
    name: &str,
    args: serde_json::Value,
    validators: &mut ValidatorCache,
) -> PreflightResult {
    let guard = tools.read();
    preflight_tool_call(name, &args.to_string(), "call-1", &guard, validators)
}

fn output_text(result: &ToolResult) -> String {
    match &result.output {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::Parts(_) => panic!("expected text output"),
    }
}

// ===========================================================================
// Registration
// ===========================================================================

#[test]
fn should_have_all_builtin_tools_present_when_checking_registry() {
    // Given: a fresh tool manager with built-in registration.
    let tools = fresh_tool_manager();

    // When: listing registered tools.
    let guard = tools.read();
    let names = guard.all_tool_names();

    // Then: every built-in tool is registered exactly once.
    assert_eq!(
        names,
        [
            "Bash",
            "CollaborationEscalate",
            "CollaborationGetMessages",
            "CollaborationGetState",
            "CollaborationIntervene",
            "CollaborationSendMessage",
            "CollaborationStart",
            "CollaborationTerminate",
            "CreateGoal",
            "CronCreate",
            "CronDelete",
            "CronList",
            "Edit",
            "EnterPlanMode",
            "ExitPlanMode",
            "GetGoal",
            "Glob",
            "Grep",
            "Read",
            "ReadMediaFile",
            "SetGoalBudget",
            "Skill",
            "TaskList",
            "TaskOutput",
            "TaskStop",
            "TodoList",
            "UpdateGoal",
            "Write",
            "select_tools"
        ]
    );
    assert_eq!(guard.builtin_count(), 29);
    // And nothing is visible to the LLM loop until enabled by the host.
    assert!(guard.loop_tools().is_empty());
}

// ===========================================================================
// Bash
// ===========================================================================

#[tokio::test]
async fn should_return_stdout_and_exit_code_when_calling_bash_with_echo_command() {
    // Given: built-in tools registered.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: the Bash tool runs `echo hello`.
    let result = call_tool(
        &tools,
        "Bash",
        serde_json::json!({ "command": "echo hello" }),
        &mut validators,
    )
    .await;

    // Then: the echoed text is returned as plain text (no JSON envelope —
    // ADR-013, TS parity).
    assert!(!result.is_error);
    assert_eq!(output_text(&result), "hello\n");
}

/// Given: built-in tools registered over services whose workspace root is a
/// temporary session workdir. When: the Bash tool runs `pwd` without a `cwd`
/// argument. Then: the command executes in the session workdir, not the
/// engine process's cwd (audit P0-1 — the registration must actually pass
/// the workdir into the tool).
#[tokio::test]
async fn should_run_bash_in_the_workspace_root_when_services_define_a_session_workdir() {
    // Given: a session workdir distinct from the engine process's cwd.
    let tmp = tempfile::tempdir().unwrap();
    let mut services = fresh_services();
    services.workspace_paths = Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf())));
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    let mut validators = ValidatorCache::new();

    // When: the registered Bash tool runs `pwd` with no `cwd` argument.
    let result = call_tool(
        &tools,
        "Bash",
        serde_json::json!({ "command": "pwd" }),
        &mut validators,
    )
    .await;

    // Then: the reported directory is the session workdir. `pwd` reports the
    // canonical path; on macOS the temp dir may live under `/var` while
    // canonicalization resolves it to `/private/var`.
    assert!(!result.is_error);
    let expected = tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| tmp.path().to_path_buf());
    assert_eq!(output_text(&result), format!("{}\n", expected.display()));
}

// ===========================================================================
// Glob / Grep outside-path pass-through (plan 2026-10-10-sandbox-path-permission)
// ===========================================================================

/// Given a registered Glob tool over a session workdir, When the model passes
/// a clean absolute path outside the workspace root, Then the call is no
/// longer refused at the tool layer: the plan is built over the canonical
/// outside path and the search runs — the permission chain (not the tool)
/// decides whether the outside access is authorized (the audit exploit was a
/// subagent Glob enumerating the engine's own storage; it is now gated by
/// that chain).
#[tokio::test]
async fn should_glob_an_outside_directory_when_services_define_a_session_workdir() {
    // Given: a session workdir distinct from the outside dir.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let outside = tmp.path().join("other");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
    let mut services = fresh_services();
    services.workspace_paths = Some(Arc::new(WorkspacePaths::new(root)));
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    let mut validators = ValidatorCache::new();

    // When: the registered Glob tool targets the outside dir.
    let result = call_tool(
        &tools,
        "Glob",
        serde_json::json!({
            "pattern": "**",
            "path": outside.to_string_lossy(),
        }),
        &mut validators,
    )
    .await;

    // Then: the outside directory is enumerated (no tool-layer refusal).
    assert!(!result.is_error, "glob failed: {}", output_text(&result));
    assert!(
        output_text(&result).contains("secret.txt"),
        "the outside directory must be searchable: {}",
        output_text(&result)
    );
}

/// Given a registered Grep tool over a session workdir, When the model passes
/// a clean absolute path outside the workspace root, Then the call is no
/// longer refused at the tool layer: the plan is built over the canonical
/// outside path and the search runs — the permission chain decides.
#[tokio::test]
async fn should_search_an_outside_directory_when_services_define_a_session_workdir() {
    // Given: a session workdir distinct from the outside dir.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let outside = tmp.path().join("other");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "needle\n").unwrap();
    let mut services = fresh_services();
    services.workspace_paths = Some(Arc::new(WorkspacePaths::new(root)));
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    let mut validators = ValidatorCache::new();

    // When: the registered Grep tool targets the outside dir.
    let result = call_tool(
        &tools,
        "Grep",
        serde_json::json!({
            "pattern": "needle",
            "path": outside.to_string_lossy(),
        }),
        &mut validators,
    )
    .await;

    // Then: the outside directory is searched (no tool-layer refusal).
    assert!(!result.is_error, "grep failed: {}", output_text(&result));
    assert!(
        output_text(&result).contains("secret.txt"),
        "the outside directory must be searchable: {}",
        output_text(&result)
    );
}

// ===========================================================================
// Write → Read → Edit round trip
// ===========================================================================

#[tokio::test]
async fn should_roundtrip_content_with_line_numbers_when_writing_then_reading_new_file() {
    // Given: built-in tools registered and a workspace directory.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("roundtrip.txt");
    let path_str = path.to_string_lossy().into_owned();

    // When: Write creates the file and Read reads it back.
    let write_result = call_tool(
        &tools,
        "Write",
        serde_json::json!({ "path": path_str, "content": "line one\nline two\n" }),
        &mut validators,
    )
    .await;
    assert!(
        !write_result.is_error,
        "write failed: {}",
        output_text(&write_result)
    );

    let read_result = call_tool(
        &tools,
        "Read",
        serde_json::json!({ "path": path_str }),
        &mut validators,
    )
    .await;

    // Then: the file content is returned with 1-based line numbers.
    assert!(!read_result.is_error);
    assert_eq!(output_text(&read_result), "1\tline one\n2\tline two\n");
}

#[tokio::test]
async fn should_persist_change_when_editing_written_file() {
    // Given: a file containing a repeated word.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("edit_me.txt");
    let path_str = path.to_string_lossy().into_owned();
    std::fs::write(&path, "hello world\n").unwrap();

    // When: Edit replaces 'hello' with 'goodbye'.
    let result = call_tool(
        &tools,
        "Edit",
        serde_json::json!({
            "path": path_str,
            "old_string": "hello",
            "new_string": "goodbye"
        }),
        &mut validators,
    )
    .await;

    // Then: the single occurrence was replaced and persisted.
    assert!(!result.is_error, "edit failed: {}", output_text(&result));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world\n");
}

#[tokio::test]
async fn should_return_synthetic_error_when_editing_ambiguous_string_without_replace_all() {
    // Given: built-in tools registered and a file with three occurrences.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dup.txt");
    let path_str = path.to_string_lossy().into_owned();
    std::fs::write(&path, "a a a\n").unwrap();

    // When: Edit with an old_string that occurs multiple times.
    let result = call_tool(
        &tools,
        "Edit",
        serde_json::json!({
            "path": path_str,
            "old_string": "a",
            "new_string": "b"
        }),
        &mut validators,
    )
    .await;

    // Then: the call is a synthetic error and the file is untouched.
    assert!(result.is_error);
    assert!(output_text(&result).contains("occurs 3 times"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a a a\n");
}

// ===========================================================================
// Glob + Grep
// ===========================================================================

#[tokio::test]
async fn should_return_matching_paths_when_globbing_rust_files() {
    // Given: a directory with .rs and .txt files.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "notes\n").unwrap();

    // When: Glob runs with '*.rs'.
    let result = call_tool(
        &tools,
        "Glob",
        serde_json::json!({ "pattern": "*.rs", "path": tmp.path() }),
        &mut validators,
    )
    .await;

    // Then: only the .rs file is reported.
    assert!(!result.is_error, "glob failed: {}", output_text(&result));
    let text = output_text(&result);
    assert!(text.contains("main.rs"), "expected main.rs in: {text}");
    assert!(
        !text.contains("notes.txt"),
        "notes.txt should not appear: {text}"
    );
}

#[tokio::test]
async fn should_return_matches_with_line_numbers_when_grepping_matching_content() {
    // Given: built-in tools registered and a directory with matching content.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();

    // When: Grep searches for 'beta'.
    let result = call_tool(
        &tools,
        "Grep",
        serde_json::json!({ "pattern": "beta", "path": tmp.path(), "output_mode": "content" }),
        &mut validators,
    )
    .await;

    // Then: the match is reported as a content block — `path:start-end`
    // header plus the numbered match row with its marker.
    assert!(!result.is_error, "grep failed: {}", output_text(&result));
    let text = output_text(&result);
    assert_eq!(text, "a.txt:2-2\n  2│beta ← match");
}

// ===========================================================================
// TodoList — session-shared through the real scope wiring
// ===========================================================================

#[tokio::test]
async fn should_update_session_list_when_calling_todolist_from_agent() {
    // Given: a real scope tree with one agent (built-ins auto-registered).
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-todo"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-todo"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-todo"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    let mut validators = ValidatorCache::new();
    let tools = agent.tool_manager();

    // When: the TodoList tool sets two items through the engine pipeline.
    let result = call_tool(
        &tools,
        "TodoList",
        serde_json::json!({
            "todos": [
                {"title": "Explore", "status": "in_progress"},
                {"title": "Implement", "status": "pending"}
            ]
        }),
        &mut validators,
    )
    .await;

    // Then: the agent's session-scoped list reflects the new items, and a
    // second agent in the same session sees the same list.
    assert!(!result.is_error);
    assert!(output_text(&result).contains("[in_progress] Explore"));
    let stored = agent.todo_list().get();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].title, "Explore");
    assert_eq!(stored[0].status, TodoStatus::InProgress);

    let second_agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-todo-2"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        second_agent.todo_list().get(),
        stored,
        "list is session-shared"
    );
}

// ===========================================================================
// Task* tools against the real TaskManager
// ===========================================================================

#[tokio::test]
async fn should_list_stop_and_output_work_when_using_task_tools_on_registered_task() {
    // Given: built-in tools and a task registered directly on the session
    // task manager (the host's background-execution path).
    let services = fresh_services();
    let task_id = services.tasks.register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "slow build".to_string(),
        "session-1".to_string(),
    );
    services
        .tasks
        .append_output(&task_id, "compiling...\ndone\n");
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    let mut validators = ValidatorCache::new();

    // When: TaskList lists tasks, TaskOutput reads one, TaskStop stops it.
    let list_result = call_tool(&tools, "TaskList", serde_json::json!({}), &mut validators).await;
    assert!(!list_result.is_error);
    let list_text = output_text(&list_result);
    assert!(
        list_text.contains(&task_id),
        "expected task_id in: {list_text}"
    );
    assert!(
        list_text.contains("slow build"),
        "expected description in: {list_text}"
    );

    let output_result = call_tool(
        &tools,
        "TaskOutput",
        serde_json::json!({ "task_id": task_id }),
        &mut validators,
    )
    .await;
    assert!(!output_result.is_error);
    let output_text_val = output_text(&output_result);
    assert!(
        output_text_val.contains("compiling...\ndone\n"),
        "expected output content in: {output_text_val}"
    );

    let stop_result = call_tool(
        &tools,
        "TaskStop",
        serde_json::json!({ "task_id": task_id }),
        &mut validators,
    )
    .await;
    assert!(!stop_result.is_error);

    // Then: the task is marked stopped in the shared manager.
    assert_eq!(
        services.tasks.get(&task_id).unwrap().status,
        mirri_core::task::TaskStatus::Stopped
    );
}

#[tokio::test]
async fn should_return_error_result_when_stopping_unknown_task() {
    // Given: built-in tools registered with no tasks.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: TaskStop is called for an unknown task id.
    let result = call_tool(
        &tools,
        "TaskStop",
        serde_json::json!({ "task_id": "task-ghost" }),
        &mut validators,
    )
    .await;

    // Then: an error result explains the task was not found.
    assert!(result.is_error);
    assert!(output_text(&result).contains("not found"));
}

// ===========================================================================
// Schema validation at the preflight layer
// ===========================================================================

#[test]
fn should_reject_call_when_preflighting_missing_required_arg() {
    // Given: built-in tools registered.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: Bash is called without 'command'.
    let preflight = preflight_result(&tools, "Bash", serde_json::json!({}), &mut validators);

    // Then: preflight rejects the call with a validation message.
    let rejected = match preflight {
        PreflightResult::Rejected(rejected) => rejected,
        PreflightResult::Valid(_) => panic!("expected rejection"),
    };
    // The exact phrasing of a missing-required-property error is
    // jsonschema-version dependent ("must have required property 'x'" vs
    // "\"x\" is a required property"); assert on the stable parts.
    assert!(
        rejected.error.contains("required property"),
        "unexpected rejection message: {}",
        rejected.error
    );
}

#[test]
fn should_reject_call_when_preflighting_unknown_tool_name() {
    // Given: built-in tools registered.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: an unregistered tool name is called.
    let preflight = preflight_result(
        &tools,
        "DoesNotExist",
        serde_json::json!({}),
        &mut validators,
    );

    // Then: preflight rejects with a not-found message.
    let rejected = match preflight {
        PreflightResult::Rejected(rejected) => rejected,
        PreflightResult::Valid(_) => panic!("expected rejection"),
    };
    assert!(rejected.error.contains("not found"));
}

// ===========================================================================
// Goal + Plan services reachable from the agent scope
// ===========================================================================

#[tokio::test]
async fn should_share_state_when_using_goal_and_plan_services_from_session_agent() {
    // Given: a real scope tree with a session (services created by the scope).
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-meta"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-meta"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-meta"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // When: the goal and plan services are used through the session handles.
    agent
        .goal()
        .create(
            "Ship the tools".to_string(),
            Some("All tests pass".to_string()),
        )
        .unwrap();
    agent.plan().enter("Step 1: build".to_string()).unwrap();

    // Then: session and agent handles observe the same state.
    assert_eq!(session.goal().get().unwrap().objective, "Ship the tools");
    assert!(session.plan().is_active());
    assert_eq!(session.plan().get_plan().as_deref(), Some("Step 1: build"));
    assert_eq!(
        session.tasks().list().len(),
        0,
        "fresh session has no background tasks"
    );
}

// ===========================================================================
// Scope service sharing (agent ← session)
// ===========================================================================

#[tokio::test]
async fn should_hold_session_instances_when_agent_checks_service_arcs() {
    // Given: a session with one agent.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-arc2"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-arc2"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-arc2"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // When: comparing Arc identities between the session and the agent.
    // Then: both hold the same service instances.
    assert!(Arc::ptr_eq(&session.todo_list(), &agent.todo_list()));
    assert!(Arc::ptr_eq(&session.tasks(), &agent.tasks()));
    assert!(Arc::ptr_eq(&session.cron(), &agent.cron()));
    assert!(Arc::ptr_eq(&session.goal(), &agent.goal()));
    assert!(Arc::ptr_eq(&session.plan(), &agent.plan()));
}

// ===========================================================================
// Goal tools — end-to-end pipeline
// ===========================================================================

#[tokio::test]
async fn should_create_and_get_goal_when_using_goal_tools_through_pipeline() {
    // Given: built-in tools registered with a fresh goal manager.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: CreateGoal records the objective, then GetGoal reads it back.
    let create_result = call_tool(
        &tools,
        "CreateGoal",
        serde_json::json!({
            "objective": "Ship the e2e tool tests",
            "completion_criterion": "All pipeline tests pass"
        }),
        &mut validators,
    )
    .await;
    assert!(
        !create_result.is_error,
        "create failed: {}",
        output_text(&create_result)
    );

    let get_result = call_tool(&tools, "GetGoal", serde_json::json!({}), &mut validators).await;

    // Then: GetGoal returns the stored goal with its default active status.
    assert!(!get_result.is_error);
    let text = output_text(&get_result);
    assert!(
        text.contains("Objective: Ship the e2e tool tests"),
        "{text}"
    );
    assert!(text.contains("Status: active"), "{text}");
    assert!(
        text.contains("Completion criterion: All pipeline tests pass"),
        "{text}"
    );
}

#[tokio::test]
async fn should_update_goal_status_when_using_update_goal_through_pipeline() {
    // Given: built-in tools registered and a goal created through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let create_result = call_tool(
        &tools,
        "CreateGoal",
        serde_json::json!({ "objective": "Ship the scheduler" }),
        &mut validators,
    )
    .await;
    assert!(
        !create_result.is_error,
        "create failed: {}",
        output_text(&create_result)
    );

    // When: UpdateGoal transitions the goal from its default 'active' status
    // to 'complete' — a genuine lifecycle transition (terminal states are
    // only reachable from an active goal; the model cannot supply 'paused').
    let update_result = call_tool(
        &tools,
        "UpdateGoal",
        serde_json::json!({ "status": "complete" }),
        &mut validators,
    )
    .await;
    let update_text = output_text(&update_result);
    assert!(!update_result.is_error, "update failed: {update_text}");
    assert!(
        update_text.contains("complete"),
        "update result should report the new status: {update_text}"
    );

    // Then: GetGoal reports the updated status.
    let get_result = call_tool(&tools, "GetGoal", serde_json::json!({}), &mut validators).await;
    assert!(!get_result.is_error);
    let text = output_text(&get_result);
    assert!(text.contains("Status: complete"), "{text}");
}

#[tokio::test]
async fn should_set_budget_when_using_set_goal_budget_through_pipeline() {
    // Given: built-in tools registered and a goal created through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let create_result = call_tool(
        &tools,
        "CreateGoal",
        serde_json::json!({ "objective": "Bounded work" }),
        &mut validators,
    )
    .await;
    assert!(
        !create_result.is_error,
        "create failed: {}",
        output_text(&create_result)
    );

    // When: SetGoalBudget attaches a 10-turn limit to the goal.
    let budget_result = call_tool(
        &tools,
        "SetGoalBudget",
        serde_json::json!({ "turns": 10 }),
        &mut validators,
    )
    .await;
    assert!(
        !budget_result.is_error,
        "set budget failed: {}",
        output_text(&budget_result)
    );

    // Then: GetGoal reports the new turn budget.
    let get_result = call_tool(&tools, "GetGoal", serde_json::json!({}), &mut validators).await;
    assert!(!get_result.is_error);
    let text = output_text(&get_result);
    assert!(text.contains("Budget: turns=10"), "{text}");
}

// ===========================================================================
// Plan tools — end-to-end pipeline
// ===========================================================================

#[tokio::test]
async fn should_enter_and_exit_plan_mode_when_using_plan_tools_through_pipeline() {
    // Given: built-in tools registered over a fresh plan manager that the test
    // can inspect through the session scope.
    let (tools, services) = fresh_tool_manager_with_services();
    let mut validators = ValidatorCache::new();

    // When: EnterPlanMode is called through the pipeline.
    let enter_result = call_tool(
        &tools,
        "EnterPlanMode",
        serde_json::json!({}),
        &mut validators,
    )
    .await;

    // Then: plan mode is active in the session scope.
    assert!(
        !enter_result.is_error,
        "enter failed: {}",
        output_text(&enter_result)
    );
    assert!(
        services.plan.is_active(),
        "plan mode should be active after enter"
    );

    // When: the model submits the plan through ExitPlanMode's `plan`
    // parameter — the real channel (there is no host-side plan write).
    let exit_result = call_tool(
        &tools,
        "ExitPlanMode",
        serde_json::json!({ "plan": "Step 1: refactor\nStep 2: verify" }),
        &mut validators,
    )
    .await;

    // Then: plan mode is deactivated and the approved plan is returned.
    assert!(
        !exit_result.is_error,
        "exit failed: {}",
        output_text(&exit_result)
    );
    assert!(
        !services.plan.is_active(),
        "plan mode should be inactive after exit"
    );
    assert_eq!(
        services.plan.get_plan().as_deref(),
        Some("Step 1: refactor\nStep 2: verify"),
        "the submitted plan text should be retained after exit"
    );
    let text = output_text(&exit_result);
    assert!(text.contains("## Approved Plan:"), "{text}");
    assert!(text.contains("Step 1: refactor"), "{text}");
}

// ===========================================================================
// Cron tools — end-to-end pipeline
// ===========================================================================

#[tokio::test]
async fn should_create_list_and_delete_cron_when_using_cron_tools_through_pipeline() {
    // Given: built-in tools registered (the session id is baked into
    // CronCreateTool via the services).
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: CronCreate schedules a task, CronList lists it, CronDelete removes it.
    let create_result = call_tool(
        &tools,
        "CronCreate",
        serde_json::json!({
            "cron": "0 9 * * *",
            "prompt": "morning briefing"
        }),
        &mut validators,
    )
    .await;
    assert!(
        !create_result.is_error,
        "create failed: {}",
        output_text(&create_result)
    );
    let create_payload: serde_json::Value =
        serde_json::from_str(&output_text(&create_result)).unwrap();
    let task_id = create_payload["id"].as_str().unwrap().to_string();

    let list_result = call_tool(&tools, "CronList", serde_json::json!({}), &mut validators).await;
    assert!(!list_result.is_error);
    let list_payload: serde_json::Value = serde_json::from_str(&output_text(&list_result)).unwrap();
    let tasks = list_payload["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], task_id);
    assert_eq!(tasks[0]["cron"], "0 9 * * *");
    assert_eq!(tasks[0]["prompt"], "morning briefing");

    let delete_result = call_tool(
        &tools,
        "CronDelete",
        serde_json::json!({ "id": task_id }),
        &mut validators,
    )
    .await;
    assert!(
        !delete_result.is_error,
        "delete failed: {}",
        output_text(&delete_result)
    );

    // Then: the deleted task no longer appears in CronList.
    let list_after = call_tool(&tools, "CronList", serde_json::json!({}), &mut validators).await;
    assert!(!list_after.is_error);
    let after_payload: serde_json::Value = serde_json::from_str(&output_text(&list_after)).unwrap();
    assert_eq!(after_payload["tasks"].as_array().unwrap().len(), 0);
}

// ===========================================================================
// Collaboration tools — end-to-end pipeline
// ===========================================================================

/// A valid collaboration mode (roles + rules) for `CollaborationStartTool`:
/// a coordinator/executor/reviewer review team with sequential communication,
/// human intervention, and human arbitration.
fn collaboration_test_mode() -> serde_json::Value {
    serde_json::json!({
        "id": "review-team",
        "name": "Review Team",
        "description": "Coordinator, executor and reviewer working on one task",
        "roles": [
            {
                "name": "coordinator",
                "description": "Splits the work and assigns tasks",
                "system_prompt": "You are the coordinator.",
                "capabilities": ["read", "plan"],
                "max_agents": 1
            },
            {
                "name": "executor",
                "description": "Executes assigned tasks",
                "system_prompt": "You are the executor.",
                "capabilities": ["read", "write", "run_tests"],
                "max_agents": 1
            },
            {
                "name": "reviewer",
                "description": "Reviews the executor's work",
                "system_prompt": "You are the reviewer.",
                "capabilities": ["read", "run_tests"],
                "max_agents": 1
            }
        ],
        "rules": {
            "communication_pattern": "sequential",
            "intervention_policy": "human",
            "conflict_resolution": "human_arbitration"
        }
    })
}

/// Start a collaboration session through the `CollaborationStart` tool and
/// return the created session id.
async fn start_collaboration_session(
    tools: &Arc<parking_lot::RwLock<ToolManager>>,
    validators: &mut ValidatorCache,
) -> String {
    let result = call_tool(
        tools,
        "CollaborationStart",
        serde_json::json!({
            "mode": collaboration_test_mode(),
            "task": "Refactor the auth module"
        }),
        validators,
    )
    .await;
    assert!(!result.is_error, "start failed: {}", output_text(&result));
    let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
    payload["session_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn should_start_and_terminate_collaboration_session_when_using_collaboration_tools() {
    // Given: built-in tools registered with a fresh collaboration manager.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: CollaborationStart creates a session and CollaborationTerminate ends it.
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    let terminate_result = call_tool(
        &tools,
        "CollaborationTerminate",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;

    // Then: termination succeeds and the session is marked failed('terminated').
    assert!(
        !terminate_result.is_error,
        "terminate failed: {}",
        output_text(&terminate_result)
    );
    let state_result = call_tool(
        &tools,
        "CollaborationGetState",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;
    assert!(!state_result.is_error);
    let state: serde_json::Value = serde_json::from_str(&output_text(&state_result)).unwrap();
    assert_eq!(state["failed"], "terminated");
}

#[tokio::test]
async fn should_send_and_get_messages_when_using_collaboration_tools() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: CollaborationSendMessage sends a task assignment from the
    // coordinator to the executor, then CollaborationGetMessages reads the log.
    let send_result = call_tool(
        &tools,
        "CollaborationSendMessage",
        serde_json::json!({
            "session_id": session_id,
            "message": {
                "type": "task_assignment",
                "from": "coordinator",
                "to": "executor",
                "task": "Refactor auth",
                "context": {}
            }
        }),
        &mut validators,
    )
    .await;
    assert!(
        !send_result.is_error,
        "send failed: {}",
        output_text(&send_result)
    );

    let get_result = call_tool(
        &tools,
        "CollaborationGetMessages",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;

    // Then: the message appears in the log with its payload.
    assert!(!get_result.is_error);
    let messages: serde_json::Value = serde_json::from_str(&output_text(&get_result)).unwrap();
    let first = &messages.as_array().unwrap()[0];
    assert_eq!(first["type"], "task_assignment");
    assert_eq!(first["from"], "coordinator");
    assert_eq!(first["to"], "executor");
    assert_eq!(first["task"], "Refactor auth");
}

#[tokio::test]
async fn should_get_state_when_using_collaboration_get_state_tool() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: CollaborationGetState queries the session.
    let result = call_tool(
        &tools,
        "CollaborationGetState",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;

    // Then: the session info reports a fresh in_progress state at round 1.
    assert!(
        !result.is_error,
        "get state failed: {}",
        output_text(&result)
    );
    let state: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
    assert_eq!(state["in_progress"]["round"], 1);
}

#[tokio::test]
async fn should_intervene_when_using_collaboration_intervene_tool() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: CollaborationIntervene injects a directive for the executor.
    let result = call_tool(
        &tools,
        "CollaborationIntervene",
        serde_json::json!({
            "session_id": session_id,
            "target": "executor",
            "directive": "Do not modify auth.rs directly"
        }),
        &mut validators,
    )
    .await;

    // Then: the directive is delivered and recorded in the message log,
    // without pausing the session.
    assert!(
        !result.is_error,
        "intervene failed: {}",
        output_text(&result)
    );
    let messages_result = call_tool(
        &tools,
        "CollaborationGetMessages",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;
    let messages: serde_json::Value = serde_json::from_str(&output_text(&messages_result)).unwrap();
    let last = messages.as_array().unwrap().last().unwrap();
    assert_eq!(last["type"], "intervention");
    assert_eq!(last["to"], "executor");

    let state_result = call_tool(
        &tools,
        "CollaborationGetState",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;
    let state: serde_json::Value = serde_json::from_str(&output_text(&state_result)).unwrap();
    assert!(
        state.get("in_progress").is_some(),
        "session stays in progress"
    );
}

#[tokio::test]
async fn should_escalate_when_using_collaboration_escalate_tool() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: CollaborationEscalate raises a conflict between two parties.
    let result = call_tool(
        &tools,
        "CollaborationEscalate",
        serde_json::json!({
            "session_id": session_id,
            "from": "reviewer",
            "description": ">= vs > boundary case",
            "parties": ["reviewer", "executor"]
        }),
        &mut validators,
    )
    .await;

    // Then: escalation succeeds and the session pauses for arbitration.
    assert!(
        !result.is_error,
        "escalate failed: {}",
        output_text(&result)
    );
    let state_result = call_tool(
        &tools,
        "CollaborationGetState",
        serde_json::json!({ "session_id": session_id }),
        &mut validators,
    )
    .await;
    let state: serde_json::Value = serde_json::from_str(&output_text(&state_result)).unwrap();
    assert_eq!(state, serde_json::json!("paused"));
}

/// Given the unrestricted default (no explicit allowlist), When all
/// registered builtin tools are enabled, Then the 7 collaboration tools
/// stay out of the LLM-visible table (mode-gated capabilities — only
/// `collaboration.enter` re-exposes them) while remaining registered and
/// resolvable, and `set_builtin_tool_enabled` restores their visibility.
#[test]
fn should_hide_collaboration_tools_when_all_registered_tools_are_enabled() {
    // Given: a fresh tool manager with built-ins registered and the
    // unrestricted default enable path run (mirroring AgentScope).
    let tools = fresh_tool_manager();
    {
        let mut guard = tools.write();
        guard.enable_all_registered();
    }

    let guard = tools.read();

    // Then: no collaboration tool is visible to the LLM.
    for name in mirri_core::tools::COLLABORATION_TOOL_NAMES {
        assert!(
            !guard.loop_tools().iter().any(|d| d.name == name),
            "{name} must be hidden from the LLM table by default"
        );
        // And: the tool is still registered and resolvable.
        assert!(
            guard.find_tool(name).is_some(),
            "{name} must stay registered for direct execution"
        );
    }

    drop(guard);
    {
        let mut guard = tools.write();
        for name in mirri_core::tools::COLLABORATION_TOOL_NAMES {
            guard.set_builtin_tool_enabled(name, true);
        }
    }
    let guard = tools.read();
    for name in mirri_core::tools::COLLABORATION_TOOL_NAMES {
        assert!(
            guard.loop_tools().iter().any(|d| d.name == name),
            "{name} must be visible after the mode gate opens"
        );
    }
}

/// Given a started collaboration session, When CollaborationSendMessage is
/// called with an unknown message `type`, Then the error text lists every
/// valid type so the LLM can correct the call in one step.
#[tokio::test]
async fn should_list_valid_message_types_when_send_message_gets_unknown_type() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: a message with an unknown type tag is sent.
    let result = call_tool(
        &tools,
        "CollaborationSendMessage",
        serde_json::json!({
            "session_id": session_id,
            "message": {
                "type": "handshake",
                "from": "coordinator",
                "to": "executor"
            }
        }),
        &mut validators,
    )
    .await;

    // Then: the error names the rejected type and lists all valid types.
    assert!(result.is_error);
    let text = output_text(&result);
    assert!(text.contains("unknown message type 'handshake'"), "{text}");
    for valid_type in [
        "task_assignment",
        "task_result",
        "question",
        "answer",
        "conflict_escalation",
        "intervention",
        "status_update",
    ] {
        assert!(
            text.contains(valid_type),
            "missing type {valid_type}: {text}"
        );
    }
}

/// Given a started collaboration session, When CollaborationSendMessage is
/// called without a message `type`, Then the error text lists every valid
/// type.
#[tokio::test]
async fn should_list_valid_message_types_when_send_message_gets_missing_type() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: a message without a type tag is sent.
    let result = call_tool(
        &tools,
        "CollaborationSendMessage",
        serde_json::json!({
            "session_id": session_id,
            "message": { "from": "coordinator", "to": "executor" }
        }),
        &mut validators,
    )
    .await;

    // Then: the error states the type is missing and lists all valid types.
    assert!(result.is_error);
    let text = output_text(&result);
    assert!(text.contains("unknown message type"), "{text}");
    assert!(text.contains("task_assignment"), "{text}");
    assert!(text.contains("status_update"), "{text}");
}

/// Given a started collaboration session, When a message names a sender
/// that is not one of the session's agents, Then the error lists the
/// available agent (role) names.
#[tokio::test]
async fn should_list_available_agents_when_send_message_gets_unknown_sender() {
    // Given: a collaboration session started through the pipeline.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();
    let session_id = start_collaboration_session(&tools, &mut validators).await;

    // When: a task assignment from a non-member agent is sent.
    let result = call_tool(
        &tools,
        "CollaborationSendMessage",
        serde_json::json!({
            "session_id": session_id,
            "message": {
                "type": "task_assignment",
                "from": "ghost",
                "to": "executor",
                "task": "Refactor auth",
                "context": {}
            }
        }),
        &mut validators,
    )
    .await;

    // Then: the error rejects 'ghost' and lists the session's agents.
    assert!(result.is_error);
    let text = output_text(&result);
    assert!(text.contains("'ghost'"), "{text}");
    assert!(
        text.contains("available agents: coordinator, executor, reviewer"),
        "{text}"
    );
}

/// Given a collaboration mode with three roles, When CollaborationStart
/// creates a session, Then the success payload lists the agent (role)
/// names alongside the session id.
#[tokio::test]
async fn should_return_agent_names_when_collaboration_start_succeeds() {
    // Given: built-in tools registered with a fresh collaboration manager.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: CollaborationStart creates a session.
    let result = call_tool(
        &tools,
        "CollaborationStart",
        serde_json::json!({
            "mode": collaboration_test_mode(),
            "task": "Refactor the auth module"
        }),
        &mut validators,
    )
    .await;

    // Then: the payload carries the session id and the role-name list.
    assert!(!result.is_error, "start failed: {}", output_text(&result));
    let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
    assert!(payload["session_id"].as_str().is_some());
    assert_eq!(
        payload["agents"],
        serde_json::json!(["coordinator", "executor", "reviewer"])
    );
}

// ===========================================================================
// Skill + select_tools — end-to-end pipeline
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_skill_tool_called_with_unknown_skill() {
    // Given: built-in tools registered with an empty skill catalog.
    let tools = fresh_tool_manager();
    let mut validators = ValidatorCache::new();

    // When: the Skill tool is called with a name that is not in the catalog.
    let result = call_tool(
        &tools,
        "Skill",
        serde_json::json!({ "skill": "no-such-skill" }),
        &mut validators,
    )
    .await;

    // Then: an error result tells the model the skill is not listed.
    assert!(result.is_error);
    let text = output_text(&result);
    assert!(text.contains("no-such-skill"), "{text}");
    assert!(text.contains("not found"), "{text}");
}

#[tokio::test]
async fn should_return_available_tools_when_using_select_tools_tool() {
    // Given: built-in tools registered over a tool-select service with
    // progressive disclosure enabled in deferred mode.
    let (tools, services) = fresh_tool_manager_with_services();
    {
        let mut tool_select = services
            .tool_select
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tool_select.set_disclosure(DisclosureState::Deferred);
        tool_select.register_deferred("expensive_search".to_string());
        tool_select.register_deferred("heavy_analyzer".to_string());
    }
    let mut validators = ValidatorCache::new();

    // When: select_tools loads a deferred tool by name, then loads it again.
    let result = call_tool(
        &tools,
        "select_tools",
        serde_json::json!({ "names": ["expensive_search"] }),
        &mut validators,
    )
    .await;

    // Then: the tool is loaded and its definition becomes available.
    assert!(
        !result.is_error,
        "select_tools failed: {}",
        output_text(&result)
    );
    assert!(output_text(&result).contains("Loaded: expensive_search"));
    {
        let tool_select = services
            .tool_select
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(tool_select.is_loaded("expensive_search"));
        assert!(!tool_select.is_deferred("expensive_search"));
    }

    // When: select_tools is called again with the same tool name.
    let second_result = call_tool(
        &tools,
        "select_tools",
        serde_json::json!({ "names": ["expensive_search"] }),
        &mut validators,
    )
    .await;

    // Then: the tool is reported as already available.
    assert!(!second_result.is_error);
    assert!(output_text(&second_result).contains("Already available: expensive_search"));
}
