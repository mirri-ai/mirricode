// layer: T1
// Domain group binary for the tools_scope domain — consolidated from per-file
// test binaries per 16-test-execution-strategy.md §4.4 (pure file moves).

mod builtin_tools_ {
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
        workspace_id: None,
        scheduled_task_creator: None,
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
                turn_origin: Default::default(),
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
            "Edit",
            "EnterPlanMode",
            "ExitPlanMode",
            "GetGoal",
            "Glob",
            "Grep",
            "Read",
            "ReadMediaFile",
            "SetGoalBudget",
            "SetTimer",
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
    assert_eq!(guard.builtin_count(), 27);
    // And nothing is visible to the LLM loop until enabled by the host.
    assert!(guard.loop_tools().is_empty());
}

/// A creator double for the CreateScheduledTask tool: accepts every
/// draft and answers the created-task id (ADR-022's approve-means-create).
struct RecordingCreationSink;

impl mirri_core::tools::create_scheduled_task::ScheduledTaskCreator
    for RecordingCreationSink
{
    fn create(
        &self,
        _draft: mirri_core::tools::create_scheduled_task::ScheduledTaskDraft,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>,
    > {
        Box::pin(async { Ok("cron-created-test".to_string()) })
    }
}

/// Given built-in tools registered with a workspace id and a creation
/// sink, when the workspace-creation tool is looked up, then
/// CreateScheduledTask is registered under its own name and none of the
/// retired tool names resolve.
#[test]
fn should_register_create_scheduled_task_when_workspace_and_sink_are_bound() {
    // Given: services carrying the host-bound workspace + sink pair.
    let mut services = fresh_services();
    services.workspace_id = Some("ws-1".to_string());
    services.scheduled_task_creator = Some(Arc::new(RecordingCreationSink));
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);

    // When: resolving the tool by name.
    let guard = tools.read();

    // Then: the new tool is registered and no retired name survives.
    assert!(
        guard.find_tool("CreateScheduledTask").is_some(),
        "CreateScheduledTask must be registered"
    );
    for retired in ["ScheduledTaskPropose", "CronCreate", "CronList", "CronDelete", "CronPropose"] {
        assert!(
            guard.find_tool(retired).is_none(),
            "the retired tool name '{retired}' must not resolve"
        );
    }
}

/// Given an agent created under a whitelist profile (the builtin `agent`
/// set) with a bound workspace and sink, when the tool table is computed,
/// then CreateScheduledTask is visible to the LLM — the whitelist must
/// not filter the workspace-creation tool out of the model's table.
#[test]
fn should_expose_create_scheduled_task_to_the_llm_under_the_agent_profile_whitelist() {
    // Given: services carrying the host-bound workspace + sink pair and
    // the builtin `agent` profile's explicit whitelist applied (the main
    // session's activation path).
    let mut services = fresh_services();
    services.workspace_id = Some("ws-1".to_string());
    services.scheduled_task_creator = Some(Arc::new(RecordingCreationSink));
    let tools = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
    register_builtin_tools(&mut tools.write(), &services);
    let profile_manager = mirri_core::profile::ProfileManager::new();
    let agent_tools = profile_manager
        .get("agent")
        .expect("builtin agent profile")
        .tools
        .expect("agent whitelist");
    tools.write().set_active_tools(agent_tools);

    // When: reading the LLM-visible tool table.
    let guard = tools.read();

    // Then: the workspace-creation tool survives the whitelist.
    assert!(
        guard
            .loop_tools()
            .iter()
            .any(|def| def.name == "CreateScheduledTask"),
        "CreateScheduledTask must be visible to the LLM under the agent profile whitelist"
    );
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
// SetTimer — end-to-end pipeline (C1: the session timer face)
// ===========================================================================

/// Given the registered SetTimer tool, when a delay and message are
/// supplied through the real pipeline, then a one-shot task lands on the
/// session's cron manager and the reply reports the fire instant — the
/// timer is gone after its single fire (no list/delete faces exist).
#[tokio::test]
async fn should_register_a_one_shot_timer_when_using_set_timer_through_pipeline() {
    // Given: built-in tools registered (the session id is baked into
    // SetTimerTool via the services).
    let (tools, services) = fresh_tool_manager_with_services();
    let mut validators = ValidatorCache::new();

    // When: SetTimer schedules a 10-minute reminder.
    let result = call_tool(
        &tools,
        "SetTimer",
        serde_json::json!({
            "delay_minutes": 10,
            "message": "check the CI result"
        }),
        &mut validators,
    )
    .await;
    assert!(!result.is_error, "set failed: {}", output_text(&result));
    let payload: serde_json::Value =
        serde_json::from_str(&output_text(&result)).unwrap();
    let task_id = payload["id"].as_str().unwrap().to_string();
    assert_eq!(payload["delayMinutes"], 10);
    assert_eq!(payload["message"], "check the CI result");
    assert!(payload["firesOnceAt"].is_string());

    // Then: the session's cron manager holds exactly one ONE-SHOT task.
    let tasks = services.cron.list();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task_id);
    assert!(!tasks[0].recurring, "C1: the timer is one-shot");
    assert_eq!(tasks[0].prompt, "check the CI result");
    assert_eq!(tasks[0].session_id, "test-session");

    // And when the fire consumes it (forced due + one check_due pass),
    // the timer is gone — fires once, cleans itself up.
    assert!(services
        .cron
        .set_next_fire_at(&task_id, Some(chrono::Utc::now() - chrono::Duration::seconds(5))));
    let fires = services.cron.check_due().await;
    assert_eq!(fires.len(), 1, "the forced-due timer fires once");
    assert!(
        services.cron.get(&task_id).is_none(),
        "the fired timer removed itself — no cleanup tool needed"
    );
    assert!(services.cron.list().is_empty());
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

}

mod tool_registry_ {
//! Integration tests for the tool registry, execution pipeline, and scheduler.

use std::sync::Arc;

use mirri_core::EngineResult;
use mirri_core::cancel::CancelToken;
use mirri_core::hooks::NoopHooks;
use mirri_core::tool::ToolErrorResult;
use mirri_core::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use mirri_core::tool_registry::{
    PendingToolTask, PreflightResult, ToolManager, ValidatorCache, preflight_tool_call,
    run_tool_batch, schedule_and_execute,
};

// ---------------------------------------------------------------------------
// Mock tools for testing
// ---------------------------------------------------------------------------

/// A simple echo tool that returns its input text.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Echoes the input text"
    }

    fn parameters(&self) -> &serde_json::Value {
        // Use a static for the schema
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let text = input
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: vec![],
                display: ToolInputDisplay::Generic {
                    summary: text.clone(),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| {
                Box::pin(async move { ToolResult::success(format!("Echo: {}", text)) })
            }),
        })
    }
}

/// A read tool that simulates file read access.
struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a file"
    }

    fn parameters(&self) -> &serde_json::Value {
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "line_offset": { "type": "number" },
                    "n_lines": { "type": "number" }
                },
                "required": ["path"]
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let path = input
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();

        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: vec![ToolAccess {
                    kind: AccessKind::File,
                    path: Some(path.clone()),
                    operation: AccessOperation::Read,
                }],
                display: ToolInputDisplay::FileIo {
                    path: path.clone(),
                    operation: "read".to_string(),
                },
                approval_rule: "Read".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| {
                Box::pin(async move { ToolResult::success(format!("Read: {}", path)) })
            }),
        })
    }
}

/// A write tool that simulates file write access.
struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Writes a file"
    }

    fn parameters(&self) -> &serde_json::Value {
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let path = input
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string();

        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: vec![ToolAccess {
                    kind: AccessKind::File,
                    path: Some(path.clone()),
                    operation: AccessOperation::Write,
                }],
                display: ToolInputDisplay::FileIo {
                    path: path.clone(),
                    operation: "write".to_string(),
                },
                approval_rule: "Write".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| {
                Box::pin(async move { ToolResult::success(format!("Wrote: {}", path)) })
            }),
        })
    }
}

/// A tool that always returns an error during resolve_execution.
struct ErrorTool;

impl Tool for ErrorTool {
    fn name(&self) -> &str {
        "ErrorTool"
    }

    fn description(&self) -> &str {
        "Always errors during resolve_execution"
    }

    fn parameters(&self) -> &serde_json::Value {
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object"}))
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Error(ToolErrorResult {
            message: "This tool always errors".to_string(),
        }))
    }
}

// ---------------------------------------------------------------------------
// ToolManager registration tests
// ---------------------------------------------------------------------------

#[test]
fn should_find_builtin_tool_when_registered() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));

    assert!(tm.find_tool("Echo").is_some());
    assert!(tm.find_tool("Nonexistent").is_none());
    assert_eq!(tm.builtin_count(), 1);
}

#[test]
fn should_find_user_tool_when_registered() {
    let mut tm = ToolManager::new();
    tm.register_user_tool(Arc::new(EchoTool));

    assert!(tm.find_tool("Echo").is_some());
    assert_eq!(tm.user_count(), 1);
}

#[test]
fn should_find_mcp_tool_when_registered() {
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("server-1".to_string(), Arc::new(EchoTool));

    assert!(tm.find_tool("Echo").is_some());
    assert_eq!(tm.mcp_count(), 1);
}

#[test]
fn should_prefer_builtin_tool_when_name_registered_in_all_categories() {
    // If the same name is registered in builtin, user, and MCP,
    // builtin should be returned first.
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_user_tool(Arc::new(EchoTool));
    tm.register_mcp_tool("server-1".to_string(), Arc::new(EchoTool));

    // All three are registered, but find_tool returns the first found (builtin)
    assert!(tm.find_tool("Echo").is_some());
    assert_eq!(tm.builtin_count(), 1);
    assert_eq!(tm.user_count(), 1);
    assert_eq!(tm.mcp_count(), 1);
}

#[test]
fn should_return_sorted_names_when_all_tools_registered() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(WriteTool));
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));

    let names = tm.all_tool_names();
    assert_eq!(names, vec!["Echo", "Read", "Write"]);
}

/// Given an agent in the unrestricted default (the
/// `AgentScopeConfig.enabled_tools = None` arm), When
/// `enable_all_registered` fills the active set, Then `enabled_tools()` is
/// sorted by name — it feeds the ToolsDiffInjector announcement text, so an
/// unsorted HashMap iteration order would change the injected message
/// across processes and thrash the provider-side prompt cache.
#[test]
fn should_return_sorted_enabled_tools_when_all_registered_tools_are_enabled() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(WriteTool));
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));

    tm.enable_all_registered();

    assert_eq!(tm.enabled_tools(), &["Echo", "Read", "Write"]);
}

/// Given a registered builtin tool, When `enable_all_registered` runs
/// after the tool was mode-gated away via
/// `set_builtin_tool_enabled(name, false)`, Then the tool stays out of the
/// active set and the LLM-visible table ("all registered" excludes the
/// mode-gated set).
#[test]
fn should_exclude_disabled_builtin_tool_when_enable_all_registered_runs() {
    // Given a tool manager with one mode-gated builtin and one plain builtin
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));
    assert!(tm.set_builtin_tool_enabled("Echo", false));

    // When the unrestricted default fills the active set
    tm.enable_all_registered();

    // Then the gated tool is absent from both the active set and the table
    assert!(!tm.enabled_tools().iter().any(|name| name == "Echo"));
    let names: Vec<String> = tm.loop_tools().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["Read"]);
}

/// Given a builtin tool disabled via `set_builtin_tool_enabled(name,
/// false)`, When the table is inspected and the tool looked up, Then the
/// tool is gone from the LLM-visible table but still resolvable through
/// `find_tool` (the registration survives — only visibility changed).
#[test]
fn should_keep_disabled_builtin_tool_resolvable_when_removed_from_the_table() {
    // Given an enabled builtin tool in the table
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.enable_all_registered();
    assert!(tm.loop_tools().iter().any(|d| d.name == "Echo"));

    // When it is mode-gated away
    assert!(tm.set_builtin_tool_enabled("Echo", false));

    // Then it leaves the table but stays resolvable
    assert!(!tm.loop_tools().iter().any(|d| d.name == "Echo"));
    assert!(tm.find_tool("Echo").is_some());
    assert_eq!(tm.builtin_count(), 1);
}

/// Given a builtin tool disabled via `set_builtin_tool_enabled(name,
/// false)`, When it is re-enabled with
/// `set_builtin_tool_enabled(name, true)`, Then the tool returns to the
/// LLM-visible table.
#[test]
fn should_restore_disabled_builtin_tool_when_re_enabled() {
    // Given a mode-gated builtin tool
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.set_builtin_tool_enabled("Echo", false);
    tm.enable_all_registered();
    assert!(!tm.loop_tools().iter().any(|d| d.name == "Echo"));

    // When the mode gate opens
    assert!(tm.set_builtin_tool_enabled("Echo", true));

    // Then the tool is visible to the LLM again
    assert!(tm.loop_tools().iter().any(|d| d.name == "Echo"));
    assert!(tm.enabled_tools().iter().any(|name| name == "Echo"));
}

/// Given a mode-gated builtin re-enabled after the sorted active set was
/// already built, When the toggle lands, Then `enabled_tools()` stays
/// sorted — the announcement of the re-enabled tools must keep the
/// same deterministic order as the initial announcement.
#[test]
fn should_keep_enabled_tools_sorted_when_a_builtin_tool_is_re_enabled() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(WriteTool));
    tm.register_builtin(Arc::new(ReadTool));
    tm.register_builtin(Arc::new(EchoTool));
    tm.set_builtin_tool_enabled("Write", false);
    tm.enable_all_registered();

    assert!(tm.set_builtin_tool_enabled("Write", true));
    assert_eq!(tm.enabled_tools(), &["Echo", "Read", "Write"]);
}

/// Given a tool name that is not a registered builtin, When
/// `set_builtin_tool_enabled` targets it, Then the call returns `false`
/// and changes nothing.
#[test]
fn should_return_false_when_set_builtin_tool_enabled_targets_unknown_name() {
    // Given a tool manager with one registered builtin
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.enable_all_registered();
    let before: Vec<String> = tm.loop_tools().iter().map(|d| d.name.clone()).collect();

    // When an unknown builtin name is toggled (both directions)
    assert!(!tm.set_builtin_tool_enabled("Nonexistent", false));
    assert!(!tm.set_builtin_tool_enabled("Nonexistent", true));

    // Then nothing changed
    let after: Vec<String> = tm.loop_tools().iter().map(|d| d.name.clone()).collect();
    assert_eq!(before, after);
}

/// Given a builtin tool mode-gated away via
/// `set_builtin_tool_enabled(name, false)`, When an explicit whitelist
/// (set_active_tools) names the tool, Then it still stays out of the
/// LLM-visible table — `builtin_disabled` is authoritative for
/// mode-gated capabilities, so a whitelist cannot widen it.
#[test]
fn should_keep_disabled_builtin_hidden_when_an_explicit_whitelist_names_it() {
    // Given a mode-gated builtin tool
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));
    tm.set_builtin_tool_enabled("Echo", false);

    // When an explicit whitelist names the gated tool
    tm.set_active_tools(vec!["Echo".to_string(), "Read".to_string()]);

    // Then only the non-gated tool is visible
    let names: Vec<String> = tm.loop_tools().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["Read"]);
    // And the tool remains resolvable for direct execution
    assert!(tm.find_tool("Echo").is_some());
}

#[test]
fn should_remove_only_target_server_tools_when_remove_mcp_tools_by_server_called() {
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("server-1".to_string(), Arc::new(EchoTool));
    tm.register_mcp_tool("server-2".to_string(), Arc::new(ReadTool));

    assert_eq!(tm.mcp_count(), 2);

    assert_eq!(tm.remove_mcp_tools_by_server("server-1"), 1);
    assert_eq!(tm.mcp_count(), 1);
    assert!(tm.find_tool("Echo").is_none());
    assert!(tm.find_tool("Read").is_some());
}

#[test]
fn should_return_zero_when_remove_mcp_tools_by_server_has_no_matching_tools() {
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("server-1".to_string(), Arc::new(EchoTool));

    let removed = tm.remove_mcp_tools_by_server("unknown-server");

    assert_eq!(removed, 0);
    assert_eq!(tm.mcp_count(), 1);
}

#[test]
fn should_return_count_of_removed_tools_when_remove_mcp_tools_by_server_matches() {
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("server-1".to_string(), Arc::new(EchoTool));
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    tm.register_mcp_tool("server-2".to_string(), Arc::new(EchoTool));

    let removed = tm.remove_mcp_tools_by_server("server-1");

    assert_eq!(removed, 1);
    assert_eq!(tm.mcp_count(), 1);
}

#[test]
fn should_return_zero_when_remove_mcp_tools_by_server_called_on_empty_table() {
    let mut tm = ToolManager::new();

    let removed = tm.remove_mcp_tools_by_server("server-1");

    assert_eq!(removed, 0);
    assert_eq!(tm.mcp_count(), 0);
}

// ---------------------------------------------------------------------------
// loop_tools tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// User-tool visibility and management (plans/07 T2 — F22)
// ---------------------------------------------------------------------------

/// Given an agent in the unrestricted default (the
/// `AgentScopeConfig.enabled_tools = None` arm — `enable_all_registered`),
/// When a user tool is registered, Then it enters the LLM-visible tool
/// table without any explicit activation (TS `profile.addActiveTool`
/// parity, F22).
#[test]
fn should_make_user_tool_visible_when_registered_under_the_unrestricted_default() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.enable_all_registered();
    tm.register_user_tool(Arc::new(EchoTool));

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Echo", "Read"],
        "a freshly registered user tool must be LLM-visible in the default mode"
    );
}

/// Given an agent whose enabled_tools is an explicit whitelist (the
/// `AgentScopeConfig.enabled_tools = Some(_)` arm), When a user tool is
/// registered, Then it stays out of the LLM-visible table — the user
/// pinned the visible set, and a new registration must not widen it.
#[test]
fn should_keep_user_tool_invisible_when_registered_under_an_explicit_whitelist() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.set_active_tools(vec!["Read".to_string()]);
    tm.register_user_tool(Arc::new(EchoTool));

    assert!(tm.find_tool("Echo").is_some(), "the tool is registered");
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Read"]);
}

/// Given a registered user tool, When it is disabled by name, Then it
/// leaves the LLM-visible table but stays resolvable in the registry; When
/// it is re-enabled, Then its visibility is restored.
#[test]
fn should_toggle_user_tool_visibility_when_set_user_tool_enabled_is_called() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.enable_all_registered();
    tm.register_user_tool(Arc::new(EchoTool));

    assert!(tm.set_user_tool_enabled("Echo", false));
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Read"]);
    assert!(
        tm.find_tool("Echo").is_some(),
        "a disabled user tool stays registered"
    );

    assert!(tm.set_user_tool_enabled("Echo", true));
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Echo", "Read"]);
}

/// Given a tool name that is not registered, When set_user_tool_enabled
/// is called, Then it answers `false` and changes nothing.
#[test]
fn should_return_false_and_change_nothing_when_set_user_tool_enabled_targets_an_unknown_name() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.enable_all_registered();

    assert!(!tm.set_user_tool_enabled("Nonexistent", false));
    assert!(!tm.set_user_tool_enabled("Nonexistent", true));
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Read"]);
}

/// Given one visible and one disabled user tool, When the management read
/// face is queried, Then every registered user tool is listed sorted with
/// its registration payload and current visibility.
#[test]
fn should_list_user_tools_with_visibility_when_user_tools_info_is_called() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.enable_all_registered();
    tm.register_user_tool(Arc::new(EchoTool));
    tm.register_user_tool(Arc::new(WriteTool));
    tm.set_user_tool_enabled("Write", false);

    let infos = tm.user_tools_info();
    let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["Echo", "Write"]);
    assert!(infos[0].enabled, "the untouched tool stays visible");
    assert!(!infos[1].enabled, "the disabled tool reports invisible");
    assert_eq!(infos[0].description, "Echoes the input text");
    assert_eq!(infos[1].parameters["properties"]["path"]["type"], "string");
}

/// Given an explicit-whitelist agent with a registered (invisible) user
/// tool, When the tool is enabled by name, Then it is added to the
/// whitelist and becomes visible — while MCP admission stays
/// whitelist-gated (the F-P11a-2 semantics are untouched).
#[test]
fn should_widen_an_explicit_whitelist_when_a_user_tool_is_enabled() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    tm.set_active_tools(vec!["Read".to_string()]);
    tm.register_user_tool(Arc::new(EchoTool));

    assert!(tm.set_user_tool_enabled("Echo", true));
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Echo", "Read"]);

    // A late MCP registration still cannot leak past the whitelist.
    tm.register_mcp_tool("server-1".to_string(), Arc::new(WriteTool));
    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Echo", "Read"]);
}

#[test]
fn should_return_enabled_tools_sorted_when_active_tools_set() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(WriteTool));
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));
    tm.set_active_tools(vec![
        "Write".to_string(),
        "Read".to_string(),
        "Echo".to_string(),
    ]);

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Echo", "Read", "Write"]);
}

#[test]
fn should_include_mcp_tools_when_disclosure_off() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    // No explicit whitelist: every registered builtin/user tool is enabled
    // (the AgentScopeConfig enabled_tools = None arm), and MCP tools are
    // admitted alongside them.
    tm.enable_all_registered();
    // disclosure is off by default

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    // Should include both Echo (enabled) and Read (MCP), sorted
    assert_eq!(names, vec!["Echo", "Read"]);
    // MCP tool should NOT be deferred when disclosure is off
    let read_def = defs.iter().find(|d| d.name == "Read").unwrap();
    assert!(!read_def.deferred);
}

/// Given an agent whose enabled_tools is an explicit whitelist (the
/// `AgentScopeConfig.enabled_tools = Some(_)` arm — the btw fork's veto
/// shape), When an MCP server registers its tools after the agent was
/// created, Then only whitelisted MCP tool names enter the table — a late
/// registration cannot leak tools past the whitelist (F-P11a-2: the
/// embedded `register_server_tools_to_sessions` and mirri-server's
/// equivalent register into every live agent, including btw forks).
#[test]
fn should_exclude_mcp_tools_from_the_table_when_an_explicit_whitelist_is_set() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.set_active_tools(vec!["Echo".to_string()]);
    // The MCP server completes its connection after the whitelist was set.
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Echo"],
        "the late MCP tool must stay out of the whitelisted table"
    );
}

/// Given an agent whose explicit whitelist is empty (the btw fork's
/// `enabled_tools = Some(vec![])`), When MCP tools register late, Then the
/// LLM-visible tool table stays empty — the side channel keeps answering
/// from context alone no matter how many servers connect afterwards.
#[test]
fn should_keep_the_tool_table_empty_when_mcp_tools_register_into_an_empty_whitelist() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.set_active_tools(Vec::new());
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    tm.register_mcp_tool("server-2".to_string(), Arc::new(WriteTool));

    let defs = tm.loop_tools();
    assert!(
        defs.is_empty(),
        "an empty whitelist must keep the tool table empty: {:?}",
        defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>()
    );
}

/// Given an explicit whitelist that names an MCP tool (a subagent scoped to
/// one server's tool), When that server registers alongside another server
/// the whitelist never mentioned, Then the table carries exactly the
/// whitelisted tools — no more, no less.
#[test]
fn should_admit_only_whitelisted_mcp_tools_when_the_whitelist_names_them() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.set_active_tools(vec!["Echo".to_string(), "Read".to_string()]);
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    tm.register_mcp_tool("server-2".to_string(), Arc::new(WriteTool));

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Echo", "Read"],
        "exactly the whitelisted tools may appear, not {:?}",
        names
    );
}

#[test]
fn should_include_only_loaded_mcp_tools_when_disclosure_on() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    tm.register_mcp_tool("server-2".to_string(), Arc::new(WriteTool));
    // No explicit whitelist: every registered builtin/user tool is enabled,
    // and MCP tools follow the disclosure gate.
    tm.enable_all_registered();
    tm.set_disclosure(true);
    // Only load the Read MCP tool
    tm.load_mcp_tool("Read");

    let defs = tm.loop_tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    // Should include Echo and Read (loaded MCP), sorted.
    // Note: "select_tools" is added to the names list but filtered out because
    // it's not a registered tool — it would appear only if a select_tools tool
    // implementation is registered.
    assert!(names.contains(&"Echo"));
    assert!(names.contains(&"Read"));
    assert!(!names.contains(&"Write")); // not loaded

    // Loaded MCP tool should be deferred
    let read_def = defs.iter().find(|d| d.name == "Read").unwrap();
    assert!(read_def.deferred);

    // Builtin tool should NOT be deferred
    let echo_def = defs.iter().find(|d| d.name == "Echo").unwrap();
    assert!(!echo_def.deferred);
}

#[test]
fn should_return_empty_when_disclosure_on_and_no_mcp_loaded() {
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("server-1".to_string(), Arc::new(ReadTool));
    tm.set_disclosure(true);
    // No MCP tools loaded

    let defs = tm.loop_tools();
    // Should only have select_tools (if registered) — but select_tools is not
    // registered as a tool, so it won't appear in the output
    // The only name in the list would be "select_tools", but find_tool returns None
    assert!(defs.is_empty() || defs.iter().all(|d| d.name != "Read"));
}

#[test]
fn should_dedupe_tool_names_when_duplicate_active_tools_set() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    // Add Echo twice in enabled tools
    tm.set_active_tools(vec!["Echo".to_string(), "Echo".to_string()]);

    let defs = tm.loop_tools();
    let echo_count = defs.iter().filter(|d| d.name == "Echo").count();
    assert_eq!(echo_count, 1);
}

// ---------------------------------------------------------------------------
// Preflight tests
// ---------------------------------------------------------------------------

#[test]
fn should_return_valid_when_args_are_valid() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();

    let result = preflight_tool_call(
        "Echo",
        r#"{"text": "hello"}"#,
        "call-1",
        &tm,
        &mut validators,
    );

    match result {
        PreflightResult::Valid(v) => {
            assert_eq!(v.tool_call_id, "call-1");
            assert_eq!(v.args["text"], "hello");
        }
        PreflightResult::Rejected(_) => panic!("expected valid"),
    }
}

#[test]
fn should_return_rejected_when_tool_not_found() {
    let tm = ToolManager::new();
    let mut validators = ValidatorCache::new();

    let result = preflight_tool_call("Nonexistent", r#"{}"#, "call-1", &tm, &mut validators);

    match result {
        PreflightResult::Rejected(r) => {
            assert_eq!(r.tool_name, "Nonexistent");
            assert_eq!(r.tool_call_id, "call-1");
            assert!(r.error.contains("not found"));
        }
        PreflightResult::Valid(_) => panic!("expected rejected"),
    }
}

#[test]
fn should_return_rejected_when_required_arg_missing() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();

    let result = preflight_tool_call("Echo", r#"{}"#, "call-1", &tm, &mut validators);

    match result {
        PreflightResult::Rejected(r) => {
            assert_eq!(r.tool_name, "Echo");
            assert!(r.error.contains("Invalid args") || r.error.contains("required"));
        }
        PreflightResult::Valid(_) => panic!("expected rejected for missing required field"),
    }
}

#[test]
fn should_parse_to_empty_object_when_json_is_malformed() {
    let mut tm = ToolManager::new();
    // Use ReadTool which doesn't require any fields (path is required but
    // validation should reject it). Actually, let's use a tool with optional
    // fields. ReadTool requires "path", so malformed JSON → {} → missing "path"
    // → rejected. Let me use ErrorTool instead, which has no required fields.
    tm.register_builtin(Arc::new(ErrorTool));
    let mut validators = ValidatorCache::new();

    let result = preflight_tool_call(
        "ErrorTool",
        "not valid json {{{",
        "call-1",
        &tm,
        &mut validators,
    );

    // Should parse to {} and pass validation (ErrorTool has no required fields)
    match result {
        PreflightResult::Valid(v) => {
            assert!(v.args.is_object());
            assert!(v.args.as_object().unwrap().is_empty());
        }
        PreflightResult::Rejected(_) => panic!("expected valid with lenient parse"),
    }
}

#[test]
fn should_convert_arg_aliases_and_coerce_numeric_strings_when_preflight_run() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    let mut validators = ValidatorCache::new();

    // Pass "offset" instead of "line_offset" and "limit" instead of "n_lines"
    // Also pass numeric strings for number fields
    let result = preflight_tool_call(
        "Read",
        r#"{"path": "/test.txt", "offset": "10", "limit": "100"}"#,
        "call-1",
        &tm,
        &mut validators,
    );

    match result {
        PreflightResult::Valid(v) => {
            // Alias remapped
            assert!(v.args.get("line_offset").is_some());
            assert!(v.args.get("n_lines").is_some());
            assert!(v.args.get("offset").is_none());
            assert!(v.args.get("limit").is_none());
            // Numeric string coerced
            assert_eq!(v.args["line_offset"], 10.0);
            assert_eq!(v.args["n_lines"], 100.0);
        }
        PreflightResult::Rejected(r) => panic!("expected valid, got rejected: {}", r.error),
    }
}

// ---------------------------------------------------------------------------
// Scheduler tests (integration)
// ---------------------------------------------------------------------------

fn read_access(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Read,
    }
}

fn write_access(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Write,
    }
}

fn all_access() -> ToolAccess {
    ToolAccess {
        kind: AccessKind::All,
        path: None,
        operation: AccessOperation::ReadWrite,
    }
}

fn make_simple_task(
    tool_call_id: &str,
    accesses: Vec<ToolAccess>,
    result_text: &str,
) -> PendingToolTask {
    let result_text = result_text.to_string();
    let ctx = mirri_core::tool::ExecutableToolContext {
        approval_metadata: None,
        wait_registry: None,
        turn_id: "test-turn".to_string(),
        tool_call_id: tool_call_id.to_string(),
        cancel: CancelToken::new(),
        progress: Default::default(),
        turn_origin: Default::default(),
    };
    PendingToolTask {
        tool_call_id: tool_call_id.to_string(),
        tool_name: "SimpleTool".to_string(),
        args: serde_json::json!({}),
        accesses,
        execute: Box::new(move |_ctx| Box::pin(async move { ToolResult::success(result_text) })),
        ctx,
        stop_batch_after_this: false,
    }
}

#[tokio::test]
async fn should_execute_concurrently_when_multiple_reads_to_same_path() {
    let cancel = CancelToken::new();
    let tasks = vec![
        make_simple_task("r1", vec![read_access("/a")], "result-1"),
        make_simple_task("r2", vec![read_access("/a")], "result-2"),
        make_simple_task("r3", vec![read_access("/a")], "result-3"),
    ];

    let results = schedule_and_execute(tasks, &cancel).await;

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, "r1");
    assert_eq!(results[1].0, "r2");
    assert_eq!(results[2].0, "r3");
}

#[tokio::test]
async fn should_serialize_execution_when_write_conflict_on_same_path() {
    let cancel = CancelToken::new();
    let tasks = vec![
        make_simple_task("w1", vec![write_access("/same")], "result-1"),
        make_simple_task("w2", vec![write_access("/same")], "result-2"),
    ];

    let results = schedule_and_execute(tasks, &cancel).await;

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "w1");
    assert_eq!(results[1].0, "w2");
}

#[tokio::test]
async fn should_serialize_execution_when_all_access_conflicts_with_reads() {
    let cancel = CancelToken::new();
    let tasks = vec![
        make_simple_task("a1", vec![all_access()], "result-1"),
        make_simple_task("r1", vec![read_access("/a")], "result-2"),
        make_simple_task("r2", vec![read_access("/b")], "result-3"),
    ];

    let results = schedule_and_execute(tasks, &cancel).await;

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, "a1");
    assert_eq!(results[1].0, "r1");
    assert_eq!(results[2].0, "r2");
}

#[tokio::test]
async fn should_return_empty_results_when_no_tasks_scheduled() {
    let cancel = CancelToken::new();
    let results = schedule_and_execute(Vec::new(), &cancel).await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn should_execute_and_return_result_when_single_task_scheduled() {
    let cancel = CancelToken::new();
    let tasks = vec![make_simple_task("s1", vec![], "only-result")];
    let results = schedule_and_execute(tasks, &cancel).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "s1");
    match &results[0].3.output {
        ToolOutput::Text(s) => assert_eq!(s, "only-result"),
        _ => panic!("expected text output"),
    }
}

// ---------------------------------------------------------------------------
// Execution pipeline tests (end-to-end)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_execute_and_return_output_when_single_tool_call_processed() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![(
        "Echo".to_string(),
        r#"{"text":"hello"}"#.to_string(),
        "call-1".to_string(),
    )];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "call-1");
    assert!(!results[0].1.is_error);
    match &results[0].1.output {
        ToolOutput::Text(s) => assert_eq!(s, "Echo: hello"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_execute_all_and_return_outputs_when_multiple_tool_calls_processed() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ReadTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![
        (
            "Echo".to_string(),
            r#"{"text":"first"}"#.to_string(),
            "call-1".to_string(),
        ),
        (
            "Read".to_string(),
            r#"{"path":"/test.txt"}"#.to_string(),
            "call-2".to_string(),
        ),
    ];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 2);

    // Find results by tool_call_id (order may vary based on scheduling)
    let echo_result = results
        .iter()
        .find(|(id, _)| id == "call-1")
        .expect("echo result");
    assert!(!echo_result.1.is_error);
    match &echo_result.1.output {
        ToolOutput::Text(s) => assert_eq!(s, "Echo: first"),
        _ => panic!("expected text output"),
    }

    let read_result = results
        .iter()
        .find(|(id, _)| id == "call-2")
        .expect("read result");
    assert!(!read_result.1.is_error);
    match &read_result.1.output {
        ToolOutput::Text(s) => assert_eq!(s, "Read: /test.txt"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_return_error_when_tool_not_found_in_pipeline() {
    let tm = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![(
        "Nonexistent".to_string(),
        "{}".to_string(),
        "call-1".to_string(),
    )];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_error);
    match &results[0].1.output {
        ToolOutput::Text(s) => assert!(s.contains("not found")),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_return_error_when_args_are_invalid_in_pipeline() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![("Echo".to_string(), "{}".to_string(), "call-1".to_string())];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_error);
}

#[tokio::test]
async fn should_return_synthetic_error_when_tool_resolution_errors() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ErrorTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![(
        "ErrorTool".to_string(),
        "{}".to_string(),
        "call-1".to_string(),
    )];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_error);
    match &results[0].1.output {
        ToolOutput::Text(s) => assert_eq!(s, "This tool always errors"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_convert_arg_aliases_and_succeed_when_pipeline_processes_converted_args() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(ReadTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    // Use aliases and numeric strings
    let tool_calls = vec![(
        "Read".to_string(),
        r#"{"path":"/test.txt","offset":"10","limit":"50"}"#.to_string(),
        "call-1".to_string(),
    )];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(!results[0].1.is_error);
    match &results[0].1.output {
        ToolOutput::Text(s) => assert_eq!(s, "Read: /test.txt"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_return_cancelled_error_when_cancelled_before_execution() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    // Cancel before running
    cancel.cancel();

    let tool_calls = vec![(
        "Echo".to_string(),
        r#"{"text":"hello"}"#.to_string(),
        "call-1".to_string(),
    )];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_error);
    match &results[0].1.output {
        ToolOutput::Text(s) => assert!(s.contains("cancelled")),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn should_return_mixed_results_when_batch_contains_valid_and_invalid_calls() {
    let mut tm = ToolManager::new();
    tm.register_builtin(Arc::new(EchoTool));
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = NoopHooks;

    let tool_calls = vec![
        (
            "Echo".to_string(),
            r#"{"text":"hello"}"#.to_string(),
            "call-1".to_string(),
        ),
        (
            "Nonexistent".to_string(),
            "{}".to_string(),
            "call-2".to_string(),
        ),
        ("Echo".to_string(), "{}".to_string(), "call-3".to_string()), // missing required
    ];

    let results = run_tool_batch(
        &tool_calls,
        &tm,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 3);

    // call-1: valid
    let r1 = results.iter().find(|(id, _)| id == "call-1").unwrap();
    assert!(!r1.1.is_error);

    // call-2: not found
    let r2 = results.iter().find(|(id, _)| id == "call-2").unwrap();
    assert!(r2.1.is_error);

    // call-3: invalid args
    let r3 = results.iter().find(|(id, _)| id == "call-3").unwrap();
    assert!(r3.1.is_error);
}

// -----------------------------------------------------------------------
// loadable_dynamic_tool_names (tools_added announcement scope, ADR-3-R2)
// -----------------------------------------------------------------------

/// A minimal MCP-shaped tool whose name follows the `mcp__<server>__<tool>`
/// wire convention.
struct ProbeMcpTool;

impl Tool for ProbeMcpTool {
    fn name(&self) -> &str {
        "mcp__srv__probe"
    }

    fn description(&self) -> &str {
        "A probe tool registered as an MCP tool"
    }

    fn parameters(&self) -> &serde_json::Value {
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({ "type": "object", "properties": {} }))
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Error(ToolErrorResult {
            message: "probe tool never executes in these tests".to_string(),
        }))
    }
}

/// Another minimal MCP tool on a second server, for whitelist filtering.
struct OtherServerTool;

impl Tool for OtherServerTool {
    fn name(&self) -> &str {
        "mcp__other__tool"
    }

    fn description(&self) -> &str {
        "A tool on another MCP server"
    }

    fn parameters(&self) -> &serde_json::Value {
        use std::sync::OnceLock;
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({ "type": "object", "properties": {} }))
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Error(ToolErrorResult {
            message: "probe tool never executes in these tests".to_string(),
        }))
    }
}

#[test]
fn should_return_only_mcp_tool_names_when_computing_loadable_dynamic_tools() {
    // Given a manager with two MCP tools, all admitted by the
    // non-explicit default (`enable_all_registered`)
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("srv".to_string(), Arc::new(ProbeMcpTool));
    tm.register_mcp_tool("other".to_string(), Arc::new(OtherServerTool));
    tm.enable_all_registered();

    // When computing the loadable dynamic names
    let names = tm.loadable_dynamic_tool_names();

    // Then only the MCP tool names appear — builtins and user tools are
    // static-table members, never announcement material
    assert_eq!(
        names,
        vec![
            "mcp__other__tool".to_string(),
            "mcp__srv__probe".to_string()
        ]
    );
}

#[test]
fn should_filter_loadable_dynamic_tools_by_explicit_whitelist() {
    // Given a manager with an explicit whitelist naming one server's tool
    // exactly and the other server by the glob form
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("srv".to_string(), Arc::new(ProbeMcpTool));
    tm.register_mcp_tool("other".to_string(), Arc::new(OtherServerTool));
    tm.set_active_tools(vec![
        "mcp__srv__probe".to_string(),
        "mcp__other__*".to_string(),
    ]);

    // When computing the loadable dynamic names
    let names = tm.loadable_dynamic_tool_names();

    // Then both pass admission (exact name and glob entry) — mirroring
    // the table's own whitelist semantics
    assert_eq!(
        names,
        vec![
            "mcp__other__tool".to_string(),
            "mcp__srv__probe".to_string()
        ]
    );
}

#[test]
fn should_return_no_loadable_dynamic_tools_when_no_mcp_tools_are_registered() {
    // Given a manager with only builtin/user tools registered
    let mut tm = ToolManager::new();
    tm.enable_all_registered();

    // When computing the loadable dynamic names
    // Then the set is empty — nothing to announce
    assert!(tm.loadable_dynamic_tool_names().is_empty());
}

#[test]
fn should_include_unloaded_mcp_tools_when_computing_loadable_dynamic_tools() {
    // Given a disclosure-mode manager whose MCP tool has not been loaded
    // via select_tools (the whole point of the announcement: names first,
    // definitions on demand)
    let mut tm = ToolManager::new();
    tm.register_mcp_tool("srv".to_string(), Arc::new(ProbeMcpTool));
    tm.enable_all_registered();
    tm.set_disclosure(true);

    // When computing the loadable dynamic names
    let names = tm.loadable_dynamic_tool_names();

    // Then the not-yet-loaded tool is loadable material
    assert_eq!(names, vec!["mcp__srv__probe".to_string()]);
}

}

mod integration_ {
//! Cross-module integration tests (Task 23).
//!
//! These tests verify that all Phase 2 engine modules work together correctly.
//! They exercise the full pipeline: scope hierarchy → TurnFlow → loop engine →
//! tool registry → permission gate → context memory → compaction.
//!
//! Test naming follows the Given-When-Then convention.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use futures::stream;
use serde_json::Value;

use mirri_core::cancel::CancelToken;
use mirri_core::compaction::{
    CompactionRoundInput, CompactionStrategy, FullCompaction, compaction_round,
};
use mirri_core::context_memory::{ContextMemory, LoopEvent};
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::hook_engine::{HookDef, HookEngine, HookEventType};
use mirri_core::hooks::*;
use mirri_core::llm::*;
use mirri_core::loop_engine::*;
use mirri_core::message::{ContentPart, Message, Role};
use mirri_core::origin::PromptOrigin;
use mirri_core::permission::{
    PermissionManager, PermissionMode, PermissionPolicyContext, PermissionPolicyResult,
    PermissionRule, PermissionServices, RuleDecision, RuleScope,
};
use mirri_core::tool::*;
use mirri_core::tool_registry::{ToolManager, ValidatorCache, run_tool_batch};
use mirri_core::turn_flow::*;

// ===========================================================================
// Mock LLM (copied from loop_engine_tests pattern)
// ===========================================================================

#[derive(Clone)]
struct MockResponse {
    text: String,
    think_text: String,
    tool_calls: Vec<LlmToolCall>,
    stop_reason: String,
    usage: LlmUsage,
    error: Option<String>,
}

impl MockResponse {
    fn end_turn(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
        }
    }

    fn tool_use(calls: Vec<LlmToolCall>) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: calls,
            stop_reason: "tool_calls".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
        }
    }

    fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        self
    }

    #[allow(dead_code)]
    fn error(message: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: Some(message.to_string()),
        }
    }
}

struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
        }
    }

    #[allow(dead_code)]
    fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl Llm for MockLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn capability(&self) -> &ModelCapability {
        static CAP: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAP
    }

    fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
        self.retryable
    }

    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(EngineError::ProviderError(
                "No more mock responses".to_string(),
            ));
        }
        let resp = queue.remove(0);
        drop(queue);

        let events = build_events(&resp);
        let stream = stream::iter(events);
        Ok(Box::pin(stream))
    }
}

fn build_events(resp: &MockResponse) -> Vec<LlmEvent> {
    let mut events = Vec::new();

    if let Some(ref err) = resp.error {
        events.push(LlmEvent::Error {
            message: err.clone(),
        });
        return events;
    }

    if !resp.text.is_empty() {
        events.push(LlmEvent::TextDelta {
            text: resp.text.clone(),
        });
        events.push(LlmEvent::TextPart {
            text: resp.text.clone(),
        });
    }

    if !resp.think_text.is_empty() {
        events.push(LlmEvent::ThinkPart {
            text: resp.think_text.clone(),
        });
    }

    for tc in &resp.tool_calls {
        events.push(LlmEvent::ToolCall {
            tool: tc.name.clone(),
            args: tc.args.clone(),
            call_id: tc.id.clone(),
        });
    }

    events.push(LlmEvent::Usage {
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
        cache_read_tokens: resp.usage.cache_read_tokens,
        cache_creation_tokens: resp.usage.cache_creation_tokens,
    });

    events.push(LlmEvent::Done {
        stop_reason: resp.stop_reason.clone(),
    });

    events
}

// ===========================================================================
// Mock Tools
// ===========================================================================

/// A simple echo tool for testing.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        })
    }

    fn resolve_execution(&self, input: Value) -> EngineResult<ToolExecution> {
        let msg = input
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message")
            .to_string();
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: format!("Echo: {}", msg),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| Box::pin(async move { ToolResult::success(msg) })),
        })
    }
}

/// A tool that always returns an error.
struct ErrorTool;

impl Tool for ErrorTool {
    fn name(&self) -> &str {
        "ErrorTool"
    }

    fn description(&self) -> &str {
        "Always returns an error"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "ErrorTool".to_string(),
                },
                approval_rule: "ErrorTool".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| Box::pin(async { ToolResult::error("Something went wrong") })),
        })
    }
}

// ===========================================================================
// Mock Hooks
// ===========================================================================

#[derive(Clone, Default)]
struct TestHooks {
    before_step_block: bool,
    after_step_stop_turn: bool,
    should_continue: bool,
}

impl LoopHooks for TestHooks {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let block = self.before_step_block;
        Box::pin(async move {
            Ok(BeforeStepResult {
                block,
                block_reason: None,
            })
        })
    }

    fn after_step(&self, _ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let stop = self.after_step_stop_turn;
        Box::pin(async move { Ok(AfterStepResult { stop_turn: stop }) })
    }

    fn should_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        let cont = self.should_continue;
        Box::pin(async move {
            Ok(ContinueAfterStopResult {
                continue_turn: cont,
            })
        })
    }
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn make_message_builders() -> MessageBuilders {
    MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

fn make_tool_manager_with_echo() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm
}

fn make_tool_call(id: &str, name: &str, args: Value) -> LlmToolCall {
    LlmToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

fn make_tool_manager_with_echo_and_error() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string(), "ErrorTool".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ErrorTool));
    tm
}

// ===========================================================================
// 1. Context Memory + Tool Registry Integration
// ===========================================================================

#[tokio::test]
async fn should_record_loop_events_when_appending_tool_results() {
    // Given: a ToolManager with an Echo tool registered and a ContextMemory.
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let mut memory = ContextMemory::new();

    // Open an assistant step and record a tool call.
    memory.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    memory.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Echo".to_string(),
        args: serde_json::json!({"message": "hello"}),
    });

    // When: execute the tool call and record the result into memory.
    let results = run_tool_batch(
        &[(
            "Echo".to_string(),
            r#"{"message":"hello"}"#.to_string(),
            "call-1".to_string(),
        )],
        &tools,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &NoopHooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    let (call_id, result) = &results[0];
    assert_eq!(call_id, "call-1");
    assert!(!result.is_error);

    // Record the tool result into ContextMemory.
    memory.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call-1".to_string(),
        output: result.output.clone(),
        is_error: result.is_error,
        note: result.note.clone(),
        entry_seq: None,
    });

    // Close the step.
    memory.append_loop_event(LoopEvent::StepEnd {
        step_id: "step-1".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });

    // Then: ContextMemory history has the assistant tool call and the tool result.
    let history = memory.history().to_vec();
    assert_eq!(history.len(), 2, "expected assistant + tool message");

    // First message: assistant with tool_calls.
    assert_eq!(history[0].message.role, Role::Assistant);
    assert_eq!(history[0].message.tool_calls.len(), 1);
    assert_eq!(history[0].message.tool_calls[0].function.name, "Echo");

    // Second message: tool result for call-1.
    assert_eq!(history[1].message.role, Role::Tool);
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-1"));
    assert!(history[1].message.text_content().contains("hello"));
}

// ===========================================================================
// 2. Context Memory + Permission Gate Integration
// ===========================================================================

#[test]
fn should_approve_tool_when_permission_manager_in_yolo_mode() {
    // Given: a PermissionManager in Yolo mode with no rules.
    let manager = PermissionManager::new(PermissionMode::Yolo, Vec::new());
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Yolo,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: evaluate the policy chain.
    let result = manager.evaluate(&ctx);

    // Then: Yolo mode approves everything (no deny rule).
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "Yolo mode should approve, got {:?}",
        result
    );
}

#[test]
fn should_deny_tool_when_deny_rule_matches() {
    // Given: a PermissionManager in Auto mode with a deny rule for "Bash".
    let rules = vec![PermissionRule {
        decision: RuleDecision::Deny,
        scope: RuleScope::User,
        pattern: "Bash".to_string(),
        reason: None,
    }];
    let manager = PermissionManager::new(PermissionMode::Auto, rules);
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Auto,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: evaluate the policy chain.
    let result = manager.evaluate(&ctx);

    // Then: deny rule beats auto-approve.
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should block, got {:?}",
        result
    );
}

#[test]
fn should_approve_when_before_tool_call_in_headless_mode() {
    // Given: a PermissionManager in Manual mode (default), no rules, headless.
    let mut manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    manager.set_headless(true);
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Read",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Manual,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: call before_tool_call (headless mode — Ask defaults to Approved).
    let result = manager.before_tool_call(&ctx);

    // Then: in headless mode, Ask falls back to Approved.
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "headless Ask should translate to Approved, got {:?}",
        result
    );
}

#[test]
fn should_block_when_before_tool_call_in_non_headless_mode() {
    // Given: a PermissionManager in Manual mode (default), no rules, NOT headless.
    // "Read" is in the default-approve list, so we use a tool that will hit
    // FallbackAskPolicy.
    let manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "SomeRandomTool",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Manual,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: call before_tool_call (non-headless — Ask should be Blocked).
    let result = manager.before_tool_call(&ctx);

    // Then: in non-headless mode, Ask should be Blocked.
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "non-headless Ask should translate to Blocked, got {:?}",
        result
    );
}

/// A unique marker file path for observing that a hook command ran.
fn permission_result_marker(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mirri_{label}_{}_{nanos}.marker",
        std::process::id()
    ))
}

/// Poll briefly for the marker file. `PermissionManager::before_tool_call`
/// fires the `PermissionResult` hook fire-and-forget on a worker thread, so
/// the write may land slightly after the call returns.
fn wait_for_marker(marker: &std::path::Path) -> bool {
    for _ in 0..50 {
        if marker.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn should_fire_permission_result_when_before_tool_call_approves_in_yolo_mode() {
    // Given: a PermissionManager wired with services carrying a hook engine
    // that registers a PermissionResult hook touching a marker file.
    let marker = permission_result_marker("pr_approved");
    let engine = std::sync::Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PermissionResult,
        format!("touch {}", marker.display()),
    ));
    let manager = PermissionManager::with_services(
        PermissionMode::Yolo,
        Vec::new(),
        PermissionServices {
            hook_engine: Some(std::sync::Arc::clone(&engine)),
            ..PermissionServices::default()
        },
    );
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();
    let ctx = PermissionPolicyContext {
        tool_name: "Read",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Yolo,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: before_tool_call approves the tool call (Yolo auto-approve).
    let result = manager.before_tool_call(&ctx);

    // Then: the tool is approved and the PermissionResult hook fires with the
    // approval outcome.
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "Yolo mode should approve, got {:?}",
        result
    );
    assert!(
        wait_for_marker(&marker),
        "PermissionResult hook should fire after an approved decision"
    );
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn should_fire_permission_result_when_before_tool_call_rejects_with_deny_rule() {
    // Given: a PermissionManager in Auto mode with a deny rule for "Bash" and
    // a hook engine registering a PermissionResult hook touching a marker.
    let marker = permission_result_marker("pr_rejected");
    let engine = std::sync::Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PermissionResult,
        format!("touch {}", marker.display()),
    ));
    let rules = vec![PermissionRule {
        decision: RuleDecision::Deny,
        scope: RuleScope::User,
        pattern: "Bash".to_string(),
        reason: None,
    }];
    let manager = PermissionManager::with_services(
        PermissionMode::Auto,
        rules,
        PermissionServices {
            hook_engine: Some(std::sync::Arc::clone(&engine)),
            ..PermissionServices::default()
        },
    );
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();
    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-2",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Auto,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };

    // When: before_tool_call rejects the tool call via the deny rule.
    let result = manager.before_tool_call(&ctx);

    // Then: the tool is blocked and the PermissionResult hook fires with the
    // rejection outcome.
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "deny rule should block, got {:?}",
        result
    );
    assert!(
        wait_for_marker(&marker),
        "PermissionResult hook should fire after a rejected decision"
    );
    let _ = std::fs::remove_file(&marker);
}

// ===========================================================================
// 3. Tool Registry + Permission + Execution Pipeline Integration
// ===========================================================================

/// Hooks that deny a specific tool name.
struct DenyToolHooks {
    denied_tool: String,
}

impl DenyToolHooks {
    fn new(denied_tool: &str) -> Self {
        Self {
            denied_tool: denied_tool.to_string(),
        }
    }
}

impl LoopHooks for DenyToolHooks {
    fn authorize_tool_execution(
        &self,
        ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        let denied = self.denied_tool.clone();
        let tool_name = ctx.tool_name.clone();
        Box::pin(async move {
            if tool_name == denied {
                Ok(AuthorizeToolResult::Blocked {
                    reason: format!("Tool {} is denied by policy", denied),
                })
            } else {
                Ok(AuthorizeToolResult::Approved { metadata: None })
            }
        })
    }
}

#[tokio::test]
async fn should_block_denied_and_execute_allowed_when_mixed_tool_batch_with_permission_hooks() {
    // Given: ToolManager with Echo and ErrorTool, hooks that deny ErrorTool.
    let tools = make_tool_manager_with_echo_and_error();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = DenyToolHooks::new("ErrorTool");

    // When: execute a batch with both an allowed and a denied tool.
    let results = run_tool_batch(
        &[
            (
                "Echo".to_string(),
                r#"{"message":"allowed"}"#.to_string(),
                "call-allowed".to_string(),
            ),
            (
                "ErrorTool".to_string(),
                "{}".to_string(),
                "call-denied".to_string(),
            ),
        ],
        &tools,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    // Then: both results returned — allowed executed, denied blocked.
    assert_eq!(results.len(), 2);

    let echo_result = results
        .iter()
        .find(|(id, _)| id == "call-allowed")
        .expect("echo result should exist");
    assert!(!echo_result.1.is_error, "Echo should succeed");
    assert!(
        echo_result
            .1
            .output
            .as_text()
            .unwrap_or_default()
            .contains("allowed"),
        "Echo output should contain the message"
    );

    let denied_result = results
        .iter()
        .find(|(id, _)| id == "call-denied")
        .expect("denied result should exist");
    assert!(
        denied_result.1.is_error,
        "denied tool should return error result"
    );
    assert!(
        denied_result
            .1
            .output
            .as_text()
            .unwrap_or_default()
            .contains("denied"),
        "denied result should mention denial, got: {:?}",
        denied_result.1.output
    );
}

// ===========================================================================
// 4. Loop Engine + Context Memory Integration
// ===========================================================================

#[tokio::test]
async fn should_update_history_correctly_when_executing_loop_step() {
    // Given: mock LLM that returns a tool call then a final response,
    // a ToolManager with Echo, and an empty ContextMemory.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "world"}),
        )]),
        MockResponse::end_turn("All done!"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When: run a full turn (user message → tool call → tool result → final response).
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then: turn completed with 2 steps.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert_eq!(result.usage.input_tokens, 200);
    assert_eq!(result.usage.output_tokens, 100);

    // ContextMemory history should contain:
    // 1. User message (from message builder)
    // 2. Assistant message with tool_call
    // 3. Tool result message
    // 4. Assistant message with final text
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // The message builders always produce a user "Hello" message, so we expect:
    // [user, assistant(tool_call), tool(result), assistant(text)]
    assert!(
        history.len() >= 3,
        "expected at least 3 messages, got {}",
        history.len()
    );

    // Find the tool result message.
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "expected 1 tool result message");
    assert_eq!(tool_msgs[0].message.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        tool_msgs[0].message.text_content().contains("world"),
        "tool result should contain echoed message"
    );

    // Find assistant messages with tool calls.
    let assistant_with_tools: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Assistant && !m.message.tool_calls.is_empty())
        .collect();
    assert_eq!(assistant_with_tools.len(), 1);
    assert_eq!(
        assistant_with_tools[0].message.tool_calls[0].function.name,
        "Echo"
    );
}

// ===========================================================================
// 5. Loop Engine + Compaction Integration
// ===========================================================================

#[tokio::test]
async fn should_compact_memory_when_context_near_token_limit() {
    // Given: a ContextMemory with many messages (approaching token limit)
    // and a mock LLM that returns a compaction summary.
    let llm = MockLlm::new(vec![MockResponse::end_turn(
        "This is a summary of the conversation.",
    )]);
    let cancel = CancelToken::new();
    let tools = ToolManager::new();
    let mut memory = ContextMemory::new();

    // Fill memory with 20 user+assistant pairs to simulate a long conversation.
    // The user bodies are large so the compaction's 20k-token keep-window
    // genuinely trims — a tiny memory cannot pass the shrink check
    // (P8a-3 变小校验) and is rejected by design.
    for i in 0..20 {
        memory.append_user_message(
            vec![ContentPart::Text {
                text: format!("User message number {i}: {}", "x".repeat(16_000)),
            }],
            PromptOrigin::User,
        );
        // Simulate an assistant response by directly appending.
        memory.append_message(mirri_core::context::ContextMessage {
            message: Message {
                role: Role::Assistant,
                name: None,
                content: vec![ContentPart::Text {
                    text: format!("Assistant response number {}", i),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }

    let tokens_before = memory.token_count_with_pending();
    assert!(
        tokens_before > 0,
        "memory should have non-zero token count before compaction"
    );
    let messages_before = memory.history().len();
    assert_eq!(
        messages_before, 40,
        "expected 40 messages before compaction"
    );

    // When: run a compaction round.
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
        todo_list: None,
    })
    .await
    .expect("compaction should succeed");

    // Then: memory is compacted — fewer messages, lower token count.
    let messages_after = memory.history().len();

    assert!(
        messages_after < messages_before,
        "compacted memory should have fewer messages: {} < {}",
        messages_after,
        messages_before
    );
    assert!(
        result.compacted_count < messages_before,
        "compacted_count should cover the compacted prefix, not the retention tail"
    );
    // New placement form (ADR-001/002): the compacted prefix plus the
    // verbatim retention tail must cover the original history.
    assert_eq!(
        result.compacted_count + result.retained_tail_count,
        messages_before,
        "prefix + retention tail must cover the original history"
    );
    assert!(!result.summary.is_empty(), "summary should not be empty");
}

#[tokio::test]
async fn should_trigger_compaction_when_token_count_exceeds_threshold() {
    // Given: a FullCompaction with a known max_context_tokens and a memory
    // whose token count exceeds the trigger ratio.
    let mut compaction = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 10_000,
        trigger_ratio: 0.85,
        ..Default::default()
    });
    compaction.reset_for_turn();

    let mut memory = ContextMemory::new();
    // Fill memory with enough content to exceed 85% of 10,000 tokens.
    // Each message is ~4 tokens, so we need ~2,200 messages to reach ~8,500 tokens.
    // Instead, use a large text message.
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "x".repeat(40_000), // ~10,000 tokens
        }],
        PromptOrigin::User,
    );

    // When: check auto-compaction.
    let should_compact = compaction
        .check_auto_compaction(&memory, false)
        .expect("check should not error");

    // Then: compaction should be triggered.
    assert!(
        should_compact,
        "auto-compaction should trigger when token count exceeds 85% of max"
    );
}

// ===========================================================================
// 6. TurnFlow + Loop Engine + Context Memory Integration
// ===========================================================================

#[tokio::test]
async fn should_flush_steer_and_accumulate_usage_when_running_turn() {
    // Given: TurnFlow, mock LLM that calls a tool then ends, and ContextMemory.
    // The steer() method only buffers when a turn is active — we test the
    // steer-flush path by verifying that after the turn, the steer buffer
    // is empty (flushed during before_step hooks).
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )])
        .with_usage(100, 50),
        MockResponse::end_turn("Done!").with_usage(200, 75),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run one turn (steer buffer starts empty, hooks flush during steps).
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: turn completed, steer buffer empty, usage accumulated across steps.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert!(
        !turn_flow.has_pending_steers(),
        "steer buffer should be empty"
    );
    assert_eq!(
        result.usage.input_tokens, 300,
        "input tokens should accumulate"
    );
    assert_eq!(
        result.usage.output_tokens, 125,
        "output tokens should accumulate"
    );

    // ContextMemory should contain the user message and tool exchange.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let has_user = history.iter().any(|m| m.message.role == Role::User);
    assert!(has_user, "should have at least one user message");
    let has_tool = history.iter().any(|m| m.message.role == Role::Tool);
    assert!(has_tool, "should have a tool result message");
}

// ===========================================================================
// 7. TurnFlow + Tool Call Deduplication Integration
// ===========================================================================

#[tokio::test]
async fn should_detect_duplicates_when_repeated_tool_calls_across_steps() {
    // Given: a TurnFlow and a mock LLM that returns the same Echo tool call
    // for 3 consecutive steps, then ends.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-3",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::end_turn("Done after dedup"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run one turn — the deduplicator should detect the repeated
    // identical tool calls across steps and eventually add reminders.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Keep calling Echo".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: turn completes (either via EndTurn or dedup-forced stop).
    assert_eq!(result.steps, 4, "should execute 4 steps");

    // Verify that tool results exist in history — the cross-step dedup
    // escalation should not prevent results from being recorded.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(
        tool_msgs.len(),
        3,
        "expected 3 tool result messages (one per step)"
    );

    // At least one of the later tool results should contain a dedup reminder
    // (streak ≥ 3 triggers reminder1).
    let has_reminder = tool_msgs.iter().any(|m| {
        m.message.text_content().contains("system-reminder")
            || m.message.text_content().contains("same tool")
            || m.message.text_content().contains("repeatedly")
    });
    assert!(
        has_reminder,
        "expected at least one dedup reminder in tool results after 3 identical calls"
    );
}

#[tokio::test]
async fn should_return_synthetic_result_when_same_step_duplicate_tool_call() {
    // Given: TurnFlow and mock LLM that calls the same tool twice in one step.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![
            make_tool_call(
                "call-1",
                "Echo",
                serde_json::json!({"message": "hello-echo"}),
            ),
            make_tool_call(
                "call-2",
                "Echo",
                serde_json::json!({"message": "hello-echo"}),
            ),
        ]),
        MockResponse::end_turn("Done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run the turn.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: both tool calls produce results, but one is synthetic (empty).
    // The deduplicator returns a synthetic empty result for the second
    // identical call. We verify that at least one result contains the
    // echoed message and at least one does not.
    assert_eq!(result.steps, 2);

    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "expected 2 tool results");

    // One result should contain the echoed "hello-echo" (the real execution),
    // and the other should NOT — it should be the synthetic dedup result
    // (a "result pending" placeholder, which does not contain "hello-echo").
    let texts: Vec<String> = tool_msgs.iter().map(|m| m.message.text_content()).collect();
    let has_real = texts.iter().any(|t| t.contains("hello-echo"));
    let has_synthetic = texts.iter().any(|t| !t.contains("hello-echo"));
    assert!(
        has_real,
        "at least one tool result should contain the echoed message"
    );
    assert!(
        has_synthetic,
        "at least one tool result should be synthetic (no echo)"
    );
}

// ===========================================================================
// 8. Scope Hierarchy + Engine Components Integration
// ===========================================================================

#[tokio::test]
async fn should_share_arc_pointers_when_accessing_agent_components_from_full_scope_tree() {
    // Given: a complete scope tree (App → Workspace → Session → Agent).
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-1"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-1"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-1"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // When: get Arc handles to all engine components.
    let cm1 = agent.context_memory();
    let cm2 = agent.context_memory();
    let tm1 = agent.tool_manager();
    let tm2 = agent.tool_manager();
    let pm1 = agent.permission_manager();
    let pm2 = agent.permission_manager();

    // Then: all Arc handles point to the same instance.
    assert!(
        Arc::ptr_eq(&cm1, &cm2),
        "ContextMemory Arc should be shared"
    );
    assert!(Arc::ptr_eq(&tm1, &tm2), "ToolManager Arc should be shared");
    assert!(
        Arc::ptr_eq(&pm1, &pm2),
        "PermissionManager Arc should be shared"
    );

    // The engine components should be in their initial state.
    {
        let guard = cm1.lock().unwrap();
        assert!(guard.history().is_empty(), "ContextMemory should be empty");
    }
    assert_eq!(
        tm1.read().all_tool_names().len(),
        28,
        "ToolManager should have all built-in tools registered"
    );
    assert_eq!(
        pm1.mode(),
        PermissionMode::default(),
        "PermissionManager should be in default mode"
    );
}

// ===========================================================================
// 9. Scope Hierarchy + Multi-Agent Integration
// ===========================================================================

#[tokio::test]
async fn should_have_independent_context_memories_when_multiple_agents_execute_turns() {
    // Given: a session with two agents, each with its own ContextMemory.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-multi"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-multi"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();

    let agent_a = session
        .create_agent(
            mirri_core::AgentId::new("agent-a"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    let agent_b = session
        .create_agent(
            mirri_core::AgentId::new("agent-b"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // Verify the ContextMemory Arcs are NOT the same.
    let cm_a = agent_a.context_memory();
    let cm_b = agent_b.context_memory();
    assert!(
        !Arc::ptr_eq(&cm_a, &cm_b),
        "different agents should have different ContextMemory instances"
    );

    // When: write to agent A's memory only.
    {
        let mut guard = cm_a.lock().unwrap();
        guard.append_user_message(
            vec![ContentPart::Text {
                text: "Message for agent A".to_string(),
            }],
            PromptOrigin::User,
        );
    }

    // Then: agent B's memory is unaffected.
    {
        let guard_a = cm_a.lock().unwrap();
        let guard_b = cm_b.lock().unwrap();
        assert_eq!(guard_a.history().len(), 1, "agent A should have 1 message");
        assert_eq!(guard_b.history().len(), 0, "agent B should have 0 messages");
    }
}

#[tokio::test]
async fn should_isolate_state_when_multiple_agents_run_concurrent_turns() {
    // Given: a session with 3 agents, each with its own TurnFlow and LLM.
    let app = Arc::new(mirri_core::AppScope::new(
        mirri_core::AppScopeConfig::default(),
    ));
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-concurrent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = Arc::new(
        ws.create_session(
            mirri_core::SessionId::new("sess-concurrent"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap(),
    );

    let mut agents = Vec::new();
    for i in 0..3 {
        let agent = session
            .create_agent(
                mirri_core::AgentId::new(format!("agent-{i}")),
                mirri_core::AgentScopeConfig::default(),
            )
            .await
            .unwrap();
        agents.push(agent);
    }

    let cancel = Arc::new(CancelToken::new());

    // When: spawn concurrent tasks, each running a turn on its own agent.
    let mut handles = Vec::new();
    for (i, agent) in agents.into_iter().enumerate() {
        let llm = MockLlm::new(vec![MockResponse::end_turn(&format!(
            "Response from agent {}",
            i
        ))]);
        let cancel = cancel.clone();
        let memory = agent.context_memory();

        let handle = tokio::spawn(async move {
            let tools = ToolManager::new();
            let mut validators = ValidatorCache::new();
            let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

            turn_flow
                .run_one_turn(
                    vec![ContentPart::Text {
                        text: format!("Hello from agent {}", i),
                    }],
                    PromptOrigin::User,
                    memory.clone(),
                    &tools,
                    Arc::new(FixedLlmResolver::new(Arc::new(llm))),
                    &cancel,
                    &mut validators,
                    None,
                    None,
                )
                .await
                .expect("turn should succeed");

            memory
        });
        handles.push(handle);
    }

    // Then: each agent's memory has exactly 2 messages (user + assistant).
    for (i, handle) in handles.into_iter().enumerate() {
        let memory = handle.await.expect("task should not panic");
        let guard = memory.lock().unwrap();
        let history = guard.history();
        assert_eq!(
            history.len(),
            2,
            "agent {} should have 2 messages, got {}",
            i,
            history.len()
        );
        assert_eq!(history[0].message.role, Role::User);
        assert_eq!(history[1].message.role, Role::Assistant);
    }
}

// ===========================================================================
// 10. Full Pipeline: Scope + TurnFlow + Loop Engine + Tools + Permission + Compaction
// ===========================================================================

#[tokio::test]
async fn should_update_all_state_correctly_when_executing_complete_turn_through_full_pipeline() {
    // Given: complete scope tree → AgentScope with all engine components.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-full"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-full"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();

    let agent_config = mirri_core::AgentScopeConfig {
        permission_mode: PermissionMode::Yolo,
        ..Default::default()
    };
    let agent = session
        .create_agent(mirri_core::AgentId::new("agent-full"), agent_config)
        .await
        .unwrap();

    // The agent's ToolManager is behind Arc (immutable). We create a separate
    // ToolManager with the Echo tool for the turn, simulating how a host would
    // configure tools before launching the agent loop.
    let tools = make_tool_manager_with_echo();

    // Verify permission mode propagated from config.
    assert_eq!(agent.permission_manager().mode(), PermissionMode::Yolo);

    // Mock LLM: first calls Echo, then returns final text.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "pipeline-test"}),
        )])
        .with_usage(150, 80),
        MockResponse::end_turn("Pipeline complete!").with_usage(250, 120),
    ]);

    let memory = agent.context_memory();
    let cancel = CancelToken::new();
    let mut validators = ValidatorCache::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: execute a full turn through the entire pipeline.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Run the full pipeline".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("full pipeline turn should succeed");

    // Then: verify all state is correctly updated.

    // Turn result.
    assert_eq!(result.turn_id, 1);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2, "should execute 2 steps (tool + final)");
    assert_eq!(
        result.usage.input_tokens, 400,
        "input tokens should accumulate"
    );
    assert_eq!(
        result.usage.output_tokens, 200,
        "output tokens should accumulate"
    );
    assert!(!result.blocked_by_prompt_hook);
    assert!(!turn_flow.is_turn_active());

    // ContextMemory history.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // Expected: [user, assistant(tool_call), tool(result), assistant(final_text)]
    assert!(
        history.len() >= 4,
        "expected at least 4 messages in history, got {}",
        history.len()
    );

    // First user message.
    assert_eq!(history[0].message.role, Role::User);
    assert!(
        history[0]
            .message
            .text_content()
            .contains("Run the full pipeline")
    );

    // Assistant with tool call.
    let assistant_with_tool = history
        .iter()
        .find(|m| m.message.role == Role::Assistant && !m.message.tool_calls.is_empty())
        .expect("should find assistant message with tool call");
    assert_eq!(
        assistant_with_tool.message.tool_calls[0].function.name,
        "Echo"
    );

    // Tool result.
    let tool_msg = history
        .iter()
        .find(|m| m.message.role == Role::Tool)
        .expect("should find tool result message");
    assert_eq!(tool_msg.message.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        tool_msg.message.text_content().contains("pipeline-test"),
        "tool result should contain echoed message"
    );

    // Final assistant text.
    let final_assistant = history
        .iter()
        .rfind(|m| m.message.role == Role::Assistant)
        .expect("should find final assistant message");
    assert!(
        final_assistant
            .message
            .text_content()
            .contains("Pipeline complete!"),
        "final assistant message should contain the response text"
    );

    // Token count should be updated.
    let token_count = {
        let guard = memory.lock().unwrap();
        guard.token_count()
    };
    assert_eq!(
        token_count, 370,
        "token count should reflect last step usage (250 + 120)"
    );
}

#[tokio::test]
async fn should_increment_turn_id_when_second_turn_runs() {
    // Given: a full pipeline setup.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-turns"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-turns"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-turns"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![
        MockResponse::end_turn("First response"),
        MockResponse::end_turn("Second response"),
    ]));
    let memory = agent.context_memory();
    let tools = ToolManager::new();
    let cancel = CancelToken::new();
    let mut validators = ValidatorCache::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run two consecutive turns.
    let result1 = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "First".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(llm.clone())),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("first turn should succeed");

    let result2 = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Second".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(llm.clone())),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("second turn should succeed");

    // Then: turn IDs increment and both turns succeed.
    assert_eq!(result1.turn_id, 1);
    assert_eq!(result2.turn_id, 2);
    assert_eq!(result1.steps, 1);
    assert_eq!(result2.steps, 1);

    // Memory should contain messages from both turns.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    assert_eq!(
        history, 4,
        "expected 4 messages (2 user + 2 assistant from two turns)"
    );
}

// ===========================================================================
// Helper trait for ToolOutput
// ===========================================================================

trait ToolOutputExt {
    fn as_text(&self) -> Option<String>;
}

impl ToolOutputExt for ToolOutput {
    fn as_text(&self) -> Option<String> {
        match self {
            ToolOutput::Text(t) => Some(t.clone()),
            ToolOutput::Parts(parts) => {
                let mut text = String::new();
                for part in parts {
                    if let ContentPart::Text { text: t } = part {
                        text.push_str(t);
                    }
                }
                if text.is_empty() { None } else { Some(text) }
            }
        }
    }
}

}

mod permission_ {
//! Integration tests for the permission gate and policy chain.

use std::collections::HashSet;
use std::sync::Arc;

use mirri_core::cancel::CancelToken;
use mirri_core::permission::{
    ParsedPattern, PermissionManager, PermissionMode, PermissionPolicyContext,
    PermissionPolicyResult, PermissionRule, RuleDecision, RuleScope, match_permission_rule,
    parse_pattern,
};
use mirri_core::tool::{AccessKind, AccessOperation, ToolAccess, ToolInputDisplay};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a [`PermissionPolicyContext`] from the given parameters and evaluate
/// the policy chain via a fresh [`PermissionManager`].
///
/// This mirrors how the `TurnFlow` would build the context: it pulls
/// `effective_rules` from the manager but takes `session_approvals` directly
/// (in production these come from `all_session_approvals()`).
fn eval(
    mode: PermissionMode,
    rules: &[PermissionRule],
    session_approvals: &HashSet<String>,
    tool_name: &str,
    display: &ToolInputDisplay,
    accesses: &[ToolAccess],
) -> PermissionPolicyResult {
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let manager = PermissionManager::new(mode, rules.to_vec());
    let effective_rules = manager.effective_rules();
    let ctx = PermissionPolicyContext {
        tool_name,
        tool_call_id: "test-call",
        args: &args,
        approval_rule: "",
        display,
        accesses,
        mode,
        rules: &effective_rules,
        session_approvals,
        cancel: &cancel,
        net_class: None,
        turn_origin: Default::default(),
    };
    manager.evaluate(&ctx)
}

fn empty_approvals() -> HashSet<String> {
    HashSet::new()
}

fn approvals(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn rule(decision: RuleDecision, pattern: &str) -> PermissionRule {
    PermissionRule {
        decision,
        scope: RuleScope::User,
        pattern: pattern.to_string(),
        reason: None,
    }
}

fn file_write_access(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Write,
    }
}

fn generic_display() -> ToolInputDisplay {
    ToolInputDisplay::Generic {
        summary: "test".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 1. Policy chain order: deny before auto-approve
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_deny_rule_matches_in_auto_mode() {
    let rules = [rule(RuleDecision::Deny, "Bash")];
    let result = eval(
        PermissionMode::Auto,
        &rules,
        &empty_approvals(),
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should beat auto-approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 2. Policy chain order: deny before yolo
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_deny_rule_matches_in_yolo_mode() {
    let rules = [rule(RuleDecision::Deny, "Write")];
    let result = eval(
        PermissionMode::Yolo,
        &rules,
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should beat yolo, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 3. Auto mode approve
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_auto_mode_with_no_matching_rules() {
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "auto mode should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 4. Yolo mode approve
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_yolo_mode() {
    let result = eval(
        PermissionMode::Yolo,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "yolo mode should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 5. Manual mode fallback ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_manual_mode_with_no_matching_rules() {
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "manual mode with no matching rules should fall back to ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 6. Default approve tools
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_tool_is_default_approved_in_manual_mode() {
    for tool_name in &["Read", "Grep", "Glob"] {
        let result = eval(
            PermissionMode::Manual,
            &[],
            &empty_approvals(),
            tool_name,
            &generic_display(),
            &[],
        );
        assert!(
            matches!(result, PermissionPolicyResult::Approve { .. }),
            "{} should be default-approved in manual mode, got {:?}",
            tool_name,
            result
        );
    }
}

// ---------------------------------------------------------------------------
// 7. User deny rule
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_user_deny_rule_matches() {
    let rules = [rule(RuleDecision::Deny, "Bash")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should deny, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 8. User allow rule
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_user_allow_rule_matches() {
    let rules = [rule(RuleDecision::Allow, "Read")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Read",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "allow rule should approve, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 9. User ask rule
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_user_ask_rule_matches() {
    let rules = [rule(RuleDecision::Ask, "Write")];
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &empty_approvals(),
        "Write",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "ask rule should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 10. Session approval beats ask rule
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_session_approval_exists_for_ask_rule() {
    let rules = [rule(RuleDecision::Ask, "Bash")];
    let session = approvals(&["Bash"]);
    let result = eval(
        PermissionMode::Manual,
        &rules,
        &session,
        "Bash",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "session approval should beat ask rule, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 11. Sensitive file access → ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_writing_to_sensitive_file() {
    let accesses = [file_write_access(".env")];
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &accesses,
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "writing to .env should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 12. Git control path access → ask
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_writing_to_git_control_path() {
    let accesses = [file_write_access(".git/config")];
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "Write",
        &generic_display(),
        &accesses,
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "writing to .git/config should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 13. Auto mode + AskUserQuestion → deny
// ---------------------------------------------------------------------------

#[test]
fn should_deny_when_ask_user_question_in_auto_mode() {
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "AskUserQuestion",
        &generic_display(),
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "auto mode should deny AskUserQuestion, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 14. ExitPlanMode review → ask (manual mode)
// ---------------------------------------------------------------------------

#[test]
fn should_ask_when_exit_plan_mode_review_in_manual_mode() {
    let display = ToolInputDisplay::PlanReview {
        plan: "plan".to_string(),
        path: None,
        options: Vec::new(),
    };
    let result = eval(
        PermissionMode::Manual,
        &[],
        &empty_approvals(),
        "ExitPlanMode",
        &display,
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Ask { .. }),
        "ExitPlanMode review in manual mode should ask, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 15. ExitPlanMode auto mode → approve (auto-approve runs first)
// ---------------------------------------------------------------------------

#[test]
fn should_approve_when_exit_plan_mode_in_auto_mode() {
    let display = ToolInputDisplay::PlanReview {
        plan: "plan".to_string(),
        path: None,
        options: Vec::new(),
    };
    let result = eval(
        PermissionMode::Auto,
        &[],
        &empty_approvals(),
        "ExitPlanMode",
        &display,
        &[],
    );
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "ExitPlanMode in auto mode should be approved by auto-approve policy, got {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// 16. Parse pattern: Write(/foo/**)
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_and_arg_pattern_when_pattern_has_argument() {
    let p = parse_pattern("Write(/foo/**)").unwrap();
    assert_eq!(p.tool_name, "Write");
    assert_eq!(p.arg_pattern.as_deref(), Some("/foo/**"));
}

// ---------------------------------------------------------------------------
// 17. Parse pattern: bare tool name
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_without_arg_pattern_when_pattern_is_bare() {
    let p = parse_pattern("Bash").unwrap();
    assert_eq!(p.tool_name, "Bash");
    assert_eq!(p.arg_pattern, None);
}

// ---------------------------------------------------------------------------
// 18. Parse pattern: empty parens
// ---------------------------------------------------------------------------

#[test]
fn should_parse_tool_name_without_arg_pattern_when_pattern_has_empty_parens() {
    let p: ParsedPattern = parse_pattern("Tool()").unwrap();
    assert_eq!(p.tool_name, "Tool");
    assert_eq!(p.arg_pattern, None);
}

// ---------------------------------------------------------------------------
// 19. Match rule: tool name glob
// ---------------------------------------------------------------------------

#[test]
fn should_match_when_glob_pattern_matches_tool_name() {
    assert!(match_permission_rule(
        "mcp__github__*",
        "mcp__github__create_pr",
        None
    ));
}

// ---------------------------------------------------------------------------
// 20. Match rule: exact tool name
// ---------------------------------------------------------------------------

#[test]
fn should_match_when_pattern_exactly_matches_tool_name() {
    assert!(match_permission_rule("Read", "Read", None));
}

// ---------------------------------------------------------------------------
// 21. Match rule: no match
// ---------------------------------------------------------------------------

#[test]
fn should_not_match_when_pattern_does_not_match_tool_name() {
    assert!(!match_permission_rule("Write", "Read", None));
}

// ---------------------------------------------------------------------------
// 22. Subagent inheritance: effective_rules includes parent's rules
// ---------------------------------------------------------------------------

#[test]
fn should_inherit_parent_rules_when_subagent_created_with_parent() {
    let parent_rules = [rule(RuleDecision::Allow, "Bash")];
    let parent = PermissionManager::new(PermissionMode::Manual, parent_rules.to_vec());
    let parent_arc = Arc::new(parent);
    let child = PermissionManager::with_parent(PermissionMode::Manual, vec![], parent_arc);
    let effective = child.effective_rules();
    assert!(
        effective
            .iter()
            .any(|r| r.pattern == "Bash" && r.decision == RuleDecision::Allow),
        "child's effective_rules should include parent's Bash allow rule, got {:?}",
        effective
    );
}

// ---------------------------------------------------------------------------
// 23. Session approval inheritance: child sees parent's approvals
// ---------------------------------------------------------------------------

#[test]
fn should_inherit_parent_session_approvals_when_subagent_created_with_parent() {
    let parent = PermissionManager::new(PermissionMode::Manual, vec![]);
    parent.record_approval("Bash".to_string());
    let parent_arc = Arc::new(parent);
    let child = PermissionManager::with_parent(PermissionMode::Manual, vec![], parent_arc);
    let approvals = child.all_session_approvals();
    assert!(
        approvals.contains(&"Bash".to_string()),
        "child's all_session_approvals should include parent's 'Bash' approval, got {:?}",
        approvals
    );
}

// ---------------------------------------------------------------------------
// 24. Runtime rule swap: a swapped-in allow rule approves what used to ask
// ---------------------------------------------------------------------------

/// Given a manual-mode chain with a runtime-swapped allow rule, when a
/// previously-asked tool is called again, then the session-approval
/// history policy is bypassed by the user-configured allow policy.
///
/// (The live-manager transition itself — `evaluate` observing a swapped set
/// on the SAME manager — is pinned by the in-module test
/// `should_evaluate_new_decision_when_set_rules_swaps_rules_at_runtime`
/// in `permission/mod.rs`; this integration test pins the chain-level
/// effect of the extracted rule set.)
#[test]
fn should_approve_when_allow_rule_swapped_in_at_runtime() {
    let manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    let display = generic_display();
    assert!(matches!(
        eval(
            PermissionMode::Manual,
            &[],
            &empty_approvals(),
            "Bash",
            &display,
            &[]
        ),
        PermissionPolicyResult::Ask { .. }
    ));

    manager.set_rules(vec![rule(RuleDecision::Allow, "Bash")]);
    let effective = manager.effective_rules();
    let result = eval(
        PermissionMode::Manual,
        &effective,
        &empty_approvals(),
        "Bash",
        &display,
        &[],
    );
    assert!(matches!(result, PermissionPolicyResult::Approve { .. }));
}

// ---------------------------------------------------------------------------
// unattended ask-deny (stage-1 batch-1, S6 / ADR-002)
// ---------------------------------------------------------------------------

/// Owned inputs + a borrow of them — the S6 ask-deny test seam (the
/// context borrows only within the owning scope).
struct OriginCtxBundle {
    cancel: CancelToken,
    args: serde_json::Value,
    display: ToolInputDisplay,
    approvals: HashSet<String>,
}

impl OriginCtxBundle {
    fn ctx<'a>(
        &'a self,
        tool_name: &'a str,
        turn_origin: mirri_core::permission::TurnOriginContext,
    ) -> PermissionPolicyContext<'a> {
        PermissionPolicyContext {
            tool_name,
            tool_call_id: "test-call",
            args: &self.args,
            approval_rule: "",
            display: &self.display,
            accesses: &[],
            mode: mirri_core::permission::PermissionMode::Manual,
            rules: &[],
            session_approvals: &self.approvals,
            cancel: &self.cancel,
            net_class: None,
            turn_origin,
        }
    }
}

fn origin_bundle() -> OriginCtxBundle {
    OriginCtxBundle {
        cancel: CancelToken::new(),
        args: serde_json::Value::Null,
        display: ToolInputDisplay::Generic {
            summary: "Bash".to_string(),
        },
        approvals: HashSet::new(),
    }
}

#[test]
fn should_block_an_ask_when_the_turn_is_an_unattended_cron_turn_on_the_sync_path() {
    // Given a Bash call (an ask-class tool on the manual chain) inside an
    // unattended cron turn, on the synchronous before_tool_call path
    let manager = PermissionManager::new(
        mirri_core::permission::PermissionMode::Manual,
        Vec::new(),
    );
    let bundle = origin_bundle();
    let ctx = bundle.ctx(
        "Bash",
        mirri_core::permission::TurnOriginContext::Cron {
            workspace: true,
            reject_asks: true,
            readonly: false,
            task_id: "cron-u1".to_string(),
        },
    );

    // When the policy chain is evaluated through before_tool_call
    let result = manager.before_tool_call(&ctx);

    // Then the ask is blocked with the unified reroute receipt — no
    // headless auto-approve, no fallback text.
    match result {
        mirri_core::hooks::AuthorizeToolResult::Blocked { reason } => {
            assert_eq!(reason, mirri_core::permission::CRON_ASK_DENY_MESSAGE);
            assert!(reason.contains("问询即拒"), "the receipt names the posture: {reason}");
            assert!(reason.contains("改"), "the receipt carries a reroute hint: {reason}");
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[test]
fn should_keep_the_existing_behavior_when_the_turn_origin_is_interactive() {
    // Given the same Bash call on an interactive turn (attended)
    let manager = PermissionManager::new(
        mirri_core::permission::PermissionMode::Manual,
        Vec::new(),
    );
    let bundle = origin_bundle();
    let ctx = bundle.ctx("Bash", mirri_core::permission::TurnOriginContext::Interactive);

    // When evaluated through before_tool_call (headless=false → the
    // no-approver fallback)
    let result = manager.before_tool_call(&ctx);

    // Then the legacy translation stands — the unattended gate did not
    // touch the interactive path.
    match result {
        mirri_core::hooks::AuthorizeToolResult::Blocked { reason } => {
            assert_eq!(
                reason, "approval required but no approval mechanism available",
                "the interactive fallback text is unchanged"
            );
        }
        other => panic!("expected Blocked fallback, got {other:?}"),
    }
}

#[test]
fn should_block_an_attended_cron_turn_ask_differently_when_someone_is_present() {
    // Given a session-timer cron fire (workspace=false) — its asks go
    // to the approval face like an interactive turn's would (D.3).
    let manager = PermissionManager::new(
        mirri_core::permission::PermissionMode::Manual,
        Vec::new(),
    );
    let bundle = origin_bundle();
    let ctx = bundle.ctx(
        "Bash",
        mirri_core::permission::TurnOriginContext::Cron {
            workspace: false,
            reject_asks: true,
            readonly: false,
            task_id: "cron-a1".to_string(),
        },
    );

    let result = manager.before_tool_call(&ctx);

    // Then the ask-deny gate does NOT fire — the same interactive
    // fallback applies.
    match result {
        mirri_core::hooks::AuthorizeToolResult::Blocked { reason } => {
            assert_eq!(reason, "approval required but no approval mechanism available");
        }
        other => panic!("expected Blocked fallback, got {other:?}"),
    }
}

#[test]
fn should_share_one_receipt_text_across_both_paths() {
    // The sync path's deny_when_unattended message and the unattended gate
    // use the same constant — one receipt, both translations.
    assert!(
        mirri_core::permission::CRON_ASK_DENY_MESSAGE.contains("问询即拒")
            && mirri_core::permission::CRON_ASK_DENY_MESSAGE.contains("改用"),
        "one receipt, both translations: {}",
        mirri_core::permission::CRON_ASK_DENY_MESSAGE
    );
}

}

mod scope_ {
//! Integration tests for the scope hierarchy (Task 22).
//!
//! These tests verify the explicit nested-struct scope tree that replaces the
//! TS v2 DI container. Tests are organized by scope level and use the
//! Given-When-Then naming convention.

use std::sync::Arc;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, LifecycleScope, ProviderType, ScopeTree,
    SessionId, SessionMetadata, TokenUsage, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};

// ===========================================================================
// LifecycleScope tests
// ===========================================================================

#[test]
fn should_succeed_when_child_scope_is_strictly_higher() {
    assert!(
        LifecycleScope::Workspace
            .must_be_strictly_higher(LifecycleScope::App)
            .is_ok()
    );
    assert!(
        LifecycleScope::Session
            .must_be_strictly_higher(LifecycleScope::Workspace)
            .is_ok()
    );
    assert!(
        LifecycleScope::Agent
            .must_be_strictly_higher(LifecycleScope::Session)
            .is_ok()
    );
}

#[test]
fn should_return_error_when_validating_equal_scopes_as_strictly_higher() {
    let result = LifecycleScope::Session.must_be_strictly_higher(LifecycleScope::Session);
    assert!(result.is_err());
}

#[test]
fn should_return_error_when_child_scope_is_lower_than_parent() {
    let result = LifecycleScope::App.must_be_strictly_higher(LifecycleScope::Session);
    assert!(result.is_err());
}

#[test]
fn should_return_true_when_agent_is_shorter_lived_than_app() {
    assert!(LifecycleScope::Agent.is_shorter_lived_than(LifecycleScope::App));
    assert!(LifecycleScope::Session.is_shorter_lived_than(LifecycleScope::App));
    assert!(!LifecycleScope::App.is_shorter_lived_than(LifecycleScope::Agent));
}

// ===========================================================================
// AppScope tests
// ===========================================================================

#[tokio::test]
async fn should_have_no_workspaces_when_app_scope_is_newly_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let workspaces = app.list_workspaces().await;
    assert!(workspaces.is_empty());
}

#[tokio::test]
async fn should_register_workspace_when_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-1");
    let cwd = std::path::PathBuf::from("/tmp/project");

    let ws = app.create_workspace(id.clone(), cwd.clone()).await.unwrap();
    assert_eq!(ws.id(), &id);
    assert_eq!(ws.cwd(), cwd.as_path());

    let listed = app.list_workspaces().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], id);
}

#[tokio::test]
async fn should_return_same_arc_when_workspace_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-get");
    let created = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let fetched = app.get_workspace(&id).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_workspace_and_empty_list_when_workspace_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-remove");
    let _ = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let removed = app.remove_workspace(&id).await;
    assert!(removed.is_some());

    let listed = app.list_workspaces().await;
    assert!(listed.is_empty());

    let gone = app.get_workspace(&id).await;
    assert!(gone.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-dup");
    let _ = app
        .create_workspace(id.clone(), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let result = app
        .create_workspace(id, std::path::PathBuf::from("/tmp"))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_set_config_when_creating_workspace_with_explicit_config() {
    let app = AppScope::new(AppScopeConfig::default());
    let id = WorkspaceId::new("ws-config");
    let config = WorkspaceScopeConfig {
        trust_level: WorkspaceTrustLevel::Trusted,
        ..Default::default()
    };
    let ws = app
        .create_workspace_with_config(id.clone(), std::path::PathBuf::from("/tmp"), config)
        .await
        .unwrap();
    assert_eq!(ws.config().trust_level, WorkspaceTrustLevel::Trusted);
}

// ===========================================================================
// WorkspaceScope tests
// ===========================================================================

#[tokio::test]
async fn should_maintain_parent_child_relationship_when_session_is_created_in_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-1"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let session_id = SessionId::new("sess-1");
    let metadata = SessionMetadata::default();
    let session = ws
        .create_session(session_id.clone(), metadata)
        .await
        .unwrap();

    assert_eq!(session.id(), &session_id);

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0], session_id);
}

#[tokio::test]
async fn should_return_same_arc_when_session_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-2"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-get");
    let created = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let fetched = ws.get_session(&sid).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_session_and_empty_list_when_session_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-3"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-remove");
    let _ = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let removed = ws.remove_session(&sid).await;
    assert!(removed.is_some());

    assert!(ws.list_sessions().await.is_empty());
    assert!(ws.get_session(&sid).await.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-dup"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();

    let sid = SessionId::new("sess-dup");
    let _ = ws
        .create_session(sid.clone(), SessionMetadata::default())
        .await
        .unwrap();

    let result = ws.create_session(sid, SessionMetadata::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_list_all_sessions_when_multiple_sessions_exist_in_same_workspace() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-multi"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    for i in 0..5 {
        let sid = SessionId::new(format!("sess-{i}"));
        ws.create_session(sid, SessionMetadata::default())
            .await
            .unwrap();
    }

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 5);
}

// ===========================================================================
// SessionScope tests
// ===========================================================================

#[tokio::test]
async fn should_maintain_parent_child_relationship_when_agent_is_created_in_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-a"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-a"), SessionMetadata::default())
        .await
        .unwrap();

    let agent_id = AgentId::new("agent-1");
    let agent = session
        .create_agent(agent_id.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    assert_eq!(agent.id(), &agent_id);

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0], agent_id);
}

#[tokio::test]
async fn should_return_same_arc_when_agent_is_fetched_by_id() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-b"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-b"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-get");
    let created = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let fetched = session.get_agent(&aid).await.unwrap();
    assert!(Arc::ptr_eq(&created, &fetched));
}

#[tokio::test]
async fn should_remove_agent_and_empty_list_when_agent_is_removed() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-c"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-c"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-remove");
    let _ = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let removed = session.remove_agent(&aid).await;
    assert!(removed.is_some());

    assert!(session.list_agents().await.is_empty());
    assert!(session.get_agent(&aid).await.is_none());
}

#[tokio::test]
async fn should_return_error_when_creating_duplicate_agent() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-d"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-d"), SessionMetadata::default())
        .await
        .unwrap();

    let aid = AgentId::new("agent-dup");
    let _ = session
        .create_agent(aid.clone(), AgentScopeConfig::default())
        .await
        .unwrap();

    let result = session.create_agent(aid, AgentScopeConfig::default()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_list_all_agents_when_multiple_agents_exist_in_same_session() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-e"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-e"), SessionMetadata::default())
        .await
        .unwrap();

    for i in 0..5 {
        let aid = AgentId::new(format!("agent-{i}"));
        session
            .create_agent(aid, AgentScopeConfig::default())
            .await
            .unwrap();
    }

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 5);
}

#[tokio::test]
async fn should_return_clone_when_session_metadata_is_read() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-meta"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    let metadata = SessionMetadata {
        title: "My Session".to_string(),
        ..Default::default()
    };
    let session = ws
        .create_session(SessionId::new("sess-meta"), metadata)
        .await
        .unwrap();

    let read = session.metadata().await;
    assert_eq!(read.title, "My Session");
}

#[tokio::test]
async fn should_update_title_and_refresh_updated_at_when_metadata_is_updated() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-update"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-update"), SessionMetadata::default())
        .await
        .unwrap();

    let original = session.metadata().await;

    // Small delay so updated_at is measurably different.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    session
        .update_metadata(|m| {
            m.title = "Updated Title".to_string();
        })
        .await;

    let updated = session.metadata().await;
    assert_eq!(updated.title, "Updated Title");
    assert!(updated.updated_at > original.updated_at);
    assert_eq!(updated.created_at, original.created_at);
}

// ===========================================================================
// AgentScope tests
// ===========================================================================

#[tokio::test]
async fn should_have_all_engine_components_initialized_when_agent_scope_is_newly_created() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-agent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-agent"), SessionMetadata::default())
        .await
        .unwrap();

    let agent = session
        .create_agent(AgentId::new("agent-init"), AgentScopeConfig::default())
        .await
        .unwrap();

    // ContextMemory should be empty initially.
    {
        let cm = agent.context_memory();
        let guard = cm.lock().unwrap();
        assert!(guard.history().is_empty());
        assert_eq!(guard.token_count(), 0);
    }

    // ToolManager should have all built-in tools registered and all
    // enabled by default (no explicit allowlist means enable-all).
    let tm = agent.tool_manager();
    {
        let guard = tm.read();
        assert_eq!(guard.all_tool_names().len(), 28);
        assert!(
            !guard.enabled_tools().is_empty(),
            "default config should enable all registered tools"
        );
        assert!(
            !guard.loop_tools().is_empty(),
            "tools should be visible to the LLM by default"
        );
    }

    // Session-scoped services are accessible and initialized.
    assert!(agent.todo_list().get().is_empty());
    assert!(agent.goal().get().is_none());
    assert!(!agent.plan().is_active());
    assert!(agent.tasks().list().is_empty());

    // PermissionManager should be in default mode.
    let pm = agent.permission_manager();
    assert_eq!(pm.mode(), mirri_core::PermissionMode::default());
}

#[tokio::test]
async fn should_point_to_same_instance_when_getting_arc_handles_from_agent_scope() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-arc"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-arc"), SessionMetadata::default())
        .await
        .unwrap();

    let agent = session
        .create_agent(AgentId::new("agent-arc"), AgentScopeConfig::default())
        .await
        .unwrap();

    let cm1 = agent.context_memory();
    let cm2 = agent.context_memory();
    assert!(Arc::ptr_eq(&cm1, &cm2));

    let tm1 = agent.tool_manager();
    let tm2 = agent.tool_manager();
    assert!(Arc::ptr_eq(&tm1, &tm2));

    let pm1 = agent.permission_manager();
    let pm2 = agent.permission_manager();
    assert!(Arc::ptr_eq(&pm1, &pm2));
}

#[tokio::test]
async fn should_be_accessible_when_agent_config_has_custom_values() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-cfg"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-cfg"), SessionMetadata::default())
        .await
        .unwrap();

    let config = AgentScopeConfig {
        model_id: "gpt-4".to_string(),
        provider_type: ProviderType::OpenAi,
        system_prompt: Some("You are a helpful assistant.".to_string()),
        max_turns: 50,
        permission_mode: mirri_core::PermissionMode::Yolo,
        ..Default::default()
    };
    let agent = session
        .create_agent(AgentId::new("agent-cfg"), config)
        .await
        .unwrap();

    let config = agent.config();
    let cfg = config.read().unwrap_or_else(|e| e.into_inner());
    assert_eq!(cfg.model_id, "gpt-4");
    assert_eq!(cfg.provider_type, ProviderType::OpenAi);
    // The system prompt may have AGENTS.md context appended by create_agent.
    let prompt = cfg.system_prompt.as_ref().unwrap();
    assert!(
        prompt.starts_with("You are a helpful assistant."),
        "prompt should start with the original text: {prompt}"
    );
    assert_eq!(cfg.max_turns, 50);
}

// ===========================================================================
// ScopeTree tests
// ===========================================================================

#[tokio::test]
async fn should_access_all_scope_levels_when_navigating_full_hierarchy() {
    let tree = ScopeTree::new(AppScopeConfig::default());
    let app = tree.app();

    let ws = app
        .create_workspace(
            WorkspaceId::new("tree-ws"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("tree-sess"), SessionMetadata::default())
        .await
        .unwrap();
    let agent = session
        .create_agent(AgentId::new("tree-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    // Verify the full chain is accessible.
    assert_eq!(agent.id().as_str(), "tree-agent");
    assert_eq!(session.id().as_str(), "tree-sess");
    assert_eq!(ws.id().as_str(), "tree-ws");
}

#[tokio::test]
async fn should_share_state_when_both_clone_refs_access_app() {
    let tree = ScopeTree::new(AppScopeConfig::default());
    let tree2 = tree.clone();

    tree.app()
        .create_workspace(
            WorkspaceId::new("shared-ws"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();

    let listed = tree2.app().list_workspaces().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), "shared-ws");
}

// ===========================================================================
// Concurrency tests
// ===========================================================================

#[tokio::test]
async fn should_register_all_workspaces_when_created_concurrently() {
    let app = Arc::new(AppScope::new(AppScopeConfig::default()));
    let mut handles = Vec::new();

    for i in 0..10 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            let id = WorkspaceId::new(format!("concurrent-ws-{i}"));
            app_clone
                .create_workspace(id, std::path::PathBuf::from(format!("/tmp/ws-{i}")))
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let workspaces = app.list_workspaces().await;
    assert_eq!(workspaces.len(), 10);
}

#[tokio::test]
async fn should_register_all_sessions_when_created_concurrently() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-concurrent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let ws = Arc::new(ws);

    let mut handles = Vec::new();
    for i in 0..10 {
        let ws_clone = Arc::clone(&ws);
        handles.push(tokio::spawn(async move {
            let sid = SessionId::new(format!("concurrent-sess-{i}"));
            ws_clone
                .create_session(sid, SessionMetadata::default())
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let sessions = ws.list_sessions().await;
    assert_eq!(sessions.len(), 10);
}

#[tokio::test]
async fn should_register_all_agents_when_created_concurrently() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-concurrent-agent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-concurrent-agent"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();
    let session = Arc::new(session);

    let mut handles = Vec::new();
    for i in 0..10 {
        let session_clone = Arc::clone(&session);
        handles.push(tokio::spawn(async move {
            let aid = AgentId::new(format!("concurrent-agent-{i}"));
            session_clone
                .create_agent(aid, AgentScopeConfig::default())
                .await
                .unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 10);
}

#[tokio::test]
async fn should_not_deadlock_when_concurrent_reads_and_writes_on_workspace_map() {
    let app = Arc::new(AppScope::new(AppScopeConfig::default()));

    // Pre-populate.
    for i in 0..5 {
        app.create_workspace(
            WorkspaceId::new(format!("rw-ws-{i}")),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    }

    let mut handles = Vec::new();

    // Readers.
    for _ in 0..5 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                let _ = app_clone.list_workspaces().await;
            }
        }));
    }

    // Writers.
    for i in 0..5 {
        let app_clone = Arc::clone(&app);
        handles.push(tokio::spawn(async move {
            let id = WorkspaceId::new(format!("rw-ws-new-{i}"));
            let _ = app_clone
                .create_workspace(id, std::path::PathBuf::from("/tmp"))
                .await;
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Should have at least the 5 original + 5 new = 10 workspaces.
    let workspaces = app.list_workspaces().await;
    assert!(workspaces.len() >= 10);
}

#[tokio::test]
async fn should_serialize_correctly_when_concurrent_access_to_shared_agent_context_memory() {
    use mirri_core::{ContentPart, PromptOrigin};

    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(WorkspaceId::new("ws-ctx"), std::path::PathBuf::from("/tmp"))
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-ctx"), SessionMetadata::default())
        .await
        .unwrap();
    let agent = session
        .create_agent(AgentId::new("agent-ctx"), AgentScopeConfig::default())
        .await
        .unwrap();

    let cm = agent.context_memory();

    // Concurrent writes to ContextMemory via std::sync::Mutex.
    let cm_clone = Arc::clone(&cm);
    let writer = tokio::task::spawn_blocking(move || {
        for i in 0..10 {
            let mut guard = cm_clone.lock().unwrap();
            guard.append_user_message(
                vec![ContentPart::Text {
                    text: format!("Message {i}"),
                }],
                PromptOrigin::User,
            );
        }
    });
    writer.await.unwrap();

    let guard = cm.lock().unwrap();
    assert_eq!(guard.history().len(), 10);
}

// ===========================================================================
// SessionScope::runtime_extras tests
// ===========================================================================

/// Given a session holding a main agent and a subagent, each with recorded
/// usage in its own usage service, when the runtime extras are read, then
/// the usage totals sum **every** agent (SDR-002 full aggregation) instead
/// of the first agent only.
#[tokio::test]
async fn should_sum_usage_across_all_agents_when_runtime_extras_called() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-agg"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(SessionId::new("sess-usage-agg"), SessionMetadata::default())
        .await
        .unwrap();

    let main = session
        .create_agent(AgentId::new("main"), AgentScopeConfig::default())
        .await
        .unwrap();
    let sub = session
        .create_agent(AgentId::new("sub-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    main.usage_service().lock().unwrap().record(
        "model-main",
        TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
        },
        None,
    );
    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then usage sums both agents — not the main agent's 100/40/10/5 alone
    assert_eq!(extras.usage.input_tokens, 130);
    assert_eq!(extras.usage.output_tokens, 60);
    assert_eq!(extras.usage.cache_read_tokens, 10);
    assert_eq!(extras.usage.cache_creation_tokens, 5);
}

/// Given a session whose main agent and subagent each recorded usage, when
/// the runtime extras are read, then the cold-start baseline is the MAIN
/// agent's first call (SDR-004) — the subagent's first call never moves it.
#[tokio::test]
async fn should_take_first_call_from_main_agent_when_runtime_extras_called() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-firstcall"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-firstcall"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let main = session
        .create_agent(AgentId::new("main"), AgentScopeConfig::default())
        .await
        .unwrap();
    let sub = session
        .create_agent(AgentId::new("sub-agent"), AgentScopeConfig::default())
        .await
        .unwrap();

    main.usage_service().lock().unwrap().record(
        "model-main",
        TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
        },
        None,
    );
    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then the baseline is the main agent's first call, not the subagent's
    let first = extras.first_call.expect("main agent's first call");
    assert_eq!(first.input_tokens, 100);
    assert_eq!(first.output_tokens, 40);
    assert_eq!(first.cache_read_tokens, 10);
    // And the totals still sum every agent (SDR-002)
    assert_eq!(extras.usage.input_tokens, 130);
}

/// Given a session whose subagent recorded usage while the main agent
/// recorded none, when the runtime extras are read, then `model` and
/// `message_count` still describe the **first** agent — only usage is
/// session-aggregated (SDR-002: the "current turn" view stays main-agent).
#[tokio::test]
async fn should_keep_model_from_first_agent_when_usage_is_summed_across_agents() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-first"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-first"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    let _main = session
        .create_agent(
            AgentId::new("main"),
            AgentScopeConfig {
                model_id: "model-main".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let sub = session
        .create_agent(
            AgentId::new("sub-agent"),
            AgentScopeConfig {
                model_id: "model-sub".to_string(),
                ..AgentScopeConfig::default()
            },
        )
        .await
        .unwrap();

    sub.usage_service().lock().unwrap().record(
        "model-sub",
        TokenUsage {
            input_tokens: 999,
            output_tokens: 999,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        None,
    );

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then model stays the first agent's, and usage still counts the subagent
    assert_eq!(extras.model, "model-main");
    assert_eq!(extras.message_count, 0);
    assert_eq!(extras.usage.input_tokens, 999);
}

/// Given a session with no agents, when the runtime extras are read, then
/// every field answers its neutral value (empty model, zeroed usage).
#[tokio::test]
async fn should_return_neutral_usage_when_session_has_no_agents() {
    let app = AppScope::new(AppScopeConfig::default());
    let ws = app
        .create_workspace(
            WorkspaceId::new("ws-usage-empty"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            SessionId::new("sess-usage-empty"),
            SessionMetadata::default(),
        )
        .await
        .unwrap();

    // When the runtime extras are read
    let extras = session.runtime_extras().await;

    // Then every field is neutral
    assert_eq!(extras.model, "");
    assert_eq!(extras.usage, TokenUsage::default());
    assert_eq!(extras.message_count, 0);
}

}

mod hook_bridge_ {
//! Integration tests for the `ExternalHooksBridge` and lifecycle hooks.
//!
//! These tests verify that:
//! - `ExternalHooksBridge` correctly implements `LoopHooks`.
//! - Each `LoopHooks` callback triggers the corresponding `HookEventType`.
//! - Block outcomes from hooks translate to the correct `LoopHooks` results.
//! - `OrderedHookSlot` executes hooks in priority order.
//! - `SessionLifecycleHooks` fires callbacks for session create / close.
//! - Hook registration and unregistration work correctly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use mirri_core::cancel::CancelToken;
use mirri_core::hook_engine::{
    ExternalHooksBridge, HookDef, HookEngine, HookEventType, OrderedHookSlot, SessionHookSlot,
    SessionLifecycleHooks,
};
use mirri_core::hooks::{
    AuthorizeToolContext, AuthorizeToolResult, BeforeStepContext, ContinueAfterStopContext,
    FinalizeToolContext, LoopHooks, PrepareToolContext, PrepareToolResult,
};
use mirri_core::tool::{ToolInputDisplay, ToolResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a `CancelToken` for tests.
fn test_cancel() -> CancelToken {
    CancelToken::new()
}

/// Create a `ToolInputDisplay` for tests.
fn test_display() -> ToolInputDisplay {
    ToolInputDisplay::Command {
        command: "test-command".to_string(),
    }
}

/// Create an `ExternalHooksBridge` with the given hooks registered on the
/// engine, and an empty base env.
fn make_bridge(hooks: Vec<HookDef>) -> ExternalHooksBridge {
    let engine = Arc::new(HookEngine::new());
    engine.register_all(hooks);
    ExternalHooksBridge::new(engine, HashMap::new())
}

// ---------------------------------------------------------------------------
// ExternalHooksBridge implements LoopHooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_be_usable_as_loop_hooks_trait_object_when_bridge_created() {
    // Verify the bridge can be used as a `dyn LoopHooks` (trait object).
    let bridge = make_bridge(vec![]);
    let _hooks: &dyn LoopHooks = &bridge;
    // If this compiles, the bridge implements LoopHooks.
}

// ---------------------------------------------------------------------------
// before_step → PreLlmRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_before_step_called_with_pre_llm_request_hook() {
    // Write a marker file so we can verify the hook ran.
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_before_step_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo triggered > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PreLlmRequest, command)]);

    let ctx = BeforeStepContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        cancel: test_cancel(),
    };

    let result = bridge.before_step(ctx).await.unwrap();
    assert!(!result.block, "passing hook should not block");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("triggered"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_return_block_when_before_step_called_with_blocking_pre_llm_request_hook() {
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PreLlmRequest, "exit 2")]);

    let ctx = BeforeStepContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        cancel: test_cancel(),
    };

    let result = bridge.before_step(ctx).await.unwrap();
    assert!(result.block, "blocking hook should set block=true");
    // A silent exit 2 carries no stderr, so the merged reason normalises to
    // None — the step loop's fallback text ("Step blocked by before_step
    // hook") supplies the user-facing message downstream.
    assert!(
        result.block_reason.is_none(),
        "silent exit 2 should normalise to an absent reason"
    );
}

// ---------------------------------------------------------------------------
// after_step → PostLlmRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_after_step_called_with_post_llm_request_hook() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_after_step_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo post_llm > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PostLlmRequest, command)]);

    let ctx = mirri_core::hooks::AfterStepContext {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        stop_reason: "end_turn".to_string(),
    };

    let result = bridge.after_step(ctx).await.unwrap();
    assert!(!result.stop_turn, "passing hook should not stop turn");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("post_llm"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// prepare_tool_execution → PreToolUse (fired by TurnFlowHooks, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with a PreToolUse hook registered, When a tool call is
/// prepared, Then the bridge has no opinion — it neither fires the hook nor
/// blocks, because the PreToolUse event is triggered by the primary
/// TurnFlowHooks (which has the context-memory access the additionalContext
/// injection and the rewritten-args channel need).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_pre_tool_use_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_pretool_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PreToolUse,
        format!("echo fired > {:?}", marker_path),
    )]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, PrepareToolResult::Allowed),
        "bridge must preserve the primary's prepare decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the PreToolUse hook"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// authorize_tool_execution → PermissionRequest (fired by TurnFlowHooks at the
// permission ask, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with PermissionRequest hooks registered, When a tool call
/// is authorized, Then the bridge has no opinion — it neither fires the hooks
/// nor denies, because the PermissionRequest event is an observation fired by
/// the primary TurnFlowHooks at the actual permission ask (TS parity:
/// `permission.approval.requested` — never a gate).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_permission_request_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_perm_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![
        HookDef::new(
            HookEventType::PermissionRequest,
            format!("echo fired > {:?}", marker_path),
        ),
        HookDef::new(
            HookEventType::PermissionRequest,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#,
        ),
    ]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let approval_rule = "auto";
    let display = test_display();
    let ctx = AuthorizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule,
        display: &display,
        cancel: test_cancel(),
    };

    let result = bridge.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "bridge must preserve the primary's authorize decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the PermissionRequest hooks"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// finalize_tool_result → PostToolUse (+ PostToolUseFailure on error)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_finalize_tool_result_called_with_post_tool_use_hook() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_finalize_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo post_tool > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PostToolUse, command)]);

    let mut tool_result = ToolResult::success("file content");
    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let result = bridge.finalize_tool_result(ctx).await.unwrap();
    assert!(!result.stop_turn, "passing hook should not stop turn");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("post_tool"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_trigger_post_tool_use_failure_hook_when_finalize_called_with_error_tool_result() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_failure_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo failure_fired > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUseFailure,
        command,
    )]);

    let mut tool_result = ToolResult::error("command not found");
    let args = serde_json::json!({"command": "ls"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("failure_fired"),
        "PostToolUseFailure hook should have been triggered for error results"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_not_trigger_post_tool_use_failure_hook_when_finalize_called_with_success_tool_result()
 {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_no_failure_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo should_not_fire > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUseFailure,
        command,
    )]);

    let mut tool_result = ToolResult::success("ok");
    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    assert!(
        !marker_path.exists(),
        "PostToolUseFailure should NOT fire for successful tool results"
    );
}

/// Given a successful tool result with an oversized output, When the result
/// finalizes, Then the PostToolUse payload's `tool_output` is truncated to
/// 2000 characters (TS parity: `output.slice(0, 2000)`).
#[tokio::test]
async fn should_truncate_tool_output_to_2000_chars_when_finalize_called_with_long_output() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_truncate_{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUse,
        format!("cat > {:?}", marker_path),
    )]);

    let long_output = "x".repeat(3000);
    let mut tool_result = ToolResult::success(long_output);
    let args = serde_json::json!({"command": "ls"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value = serde_json::from_str(&content).expect("input is JSON");
    let tool_output = input["tool_output"].as_str().expect("tool_output present");
    assert_eq!(tool_output.chars().count(), 2000, "truncated to 2000 chars");
    assert!(
        tool_output.chars().all(|c| c == 'x'),
        "the truncation keeps the output prefix"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// should_continue_after_stop → Stop (fired by TurnFlowHooks, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with a Stop hook registered, When the stop boundary is
/// reached, Then the bridge has no opinion — it neither fires the hook nor
/// blocks the primary's decision, because the Stop event is triggered by the
/// primary TurnFlowHooks (which has the context-memory access the forced
/// continuation needs).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_stop_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_stop_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::Stop,
        format!("echo fired > {:?}", marker_path),
    )]);

    let ctx = ContinueAfterStopContext {
        turn_id: "turn-1".to_string(),
        stop_reason: "end_turn".to_string(),
        cancel: test_cancel(),
    };

    let result = bridge.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "bridge must preserve the primary's stop decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the Stop hook"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// OrderedHookSlot — priority-ordered execution
// ---------------------------------------------------------------------------

#[test]
fn should_execute_in_ascending_priority_order_when_ordered_hooks_executed() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let order = Arc::new(StdMutex::new(Vec::<String>::new()));

    let o1 = Arc::clone(&order);
    slot.register(
        "after".to_string(),
        10,
        Box::new(move |_| {
            o1.lock().unwrap().push("after".to_string());
        }),
    );

    let o2 = Arc::clone(&order);
    slot.register(
        "before".to_string(),
        -10,
        Box::new(move |_| {
            o2.lock().unwrap().push("before".to_string());
        }),
    );

    let o3 = Arc::clone(&order);
    slot.register(
        "default".to_string(),
        0,
        Box::new(move |_| {
            o3.lock().unwrap().push("default".to_string());
        }),
    );

    slot.execute("session-1");

    let recorded = order.lock().unwrap();
    assert_eq!(
        *recorded,
        vec!["before", "default", "after"],
        "hooks should execute in ascending priority order"
    );
}

#[test]
fn should_remove_hook_from_execution_when_unregister_called_on_ordered_hooks() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let count = Arc::new(StdMutex::new(0u32));

    let c1 = Arc::clone(&count);
    slot.register(
        "h1".to_string(),
        0,
        Box::new(move |_| {
            *c1.lock().unwrap() += 1;
        }),
    );

    let c2 = Arc::clone(&count);
    slot.register(
        "h2".to_string(),
        0,
        Box::new(move |_| {
            *c2.lock().unwrap() += 1;
        }),
    );

    slot.execute("s1");
    assert_eq!(*count.lock().unwrap(), 2, "both hooks should fire");

    slot.unregister("h1");
    slot.execute("s2");
    assert_eq!(
        *count.lock().unwrap(),
        3,
        "only one hook should fire after unregister"
    );
}

#[test]
fn should_replace_existing_hook_when_duplicate_id_registered() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let value = Arc::new(StdMutex::new(0u32));

    let v1 = Arc::clone(&value);
    slot.register(
        "hook".to_string(),
        0,
        Box::new(move |_| {
            *v1.lock().unwrap() = 1;
        }),
    );

    let v2 = Arc::clone(&value);
    slot.register(
        "hook".to_string(),
        0,
        Box::new(move |_| {
            *v2.lock().unwrap() = 2;
        }),
    );

    assert_eq!(slot.len(), 1, "duplicate id should replace, not add");

    slot.execute("s1");
    assert_eq!(
        *value.lock().unwrap(),
        2,
        "replaced hook should be the one that executes"
    );
}

// ---------------------------------------------------------------------------
// SessionLifecycleHooks — fire events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_run_callback_with_session_id_when_session_created_hook_fired() {
    let hooks = SessionLifecycleHooks::new();
    let received = Arc::new(StdMutex::new(String::new()));

    let r = Arc::clone(&received);
    hooks
        .on_session_created(
            "log-create".to_string(),
            0,
            Box::new(move |sid| {
                *r.lock().unwrap() = sid.to_string();
            }),
        )
        .await;

    hooks.fire_session_created("session-abc").await;

    assert_eq!(
        *received.lock().unwrap(),
        "session-abc",
        "callback should receive the session ID"
    );
}

#[tokio::test]
async fn should_run_callback_when_session_closing_hook_fired() {
    let hooks = SessionLifecycleHooks::new();
    let fired = Arc::new(StdMutex::new(false));

    let f = Arc::clone(&fired);
    hooks
        .on_session_closing(
            "cleanup".to_string(),
            0,
            Box::new(move |_| {
                *f.lock().unwrap() = true;
            }),
        )
        .await;

    hooks.fire_session_closing("session-xyz").await;

    assert!(*fired.lock().unwrap(), "closing callback should have fired");
}

#[tokio::test]
async fn should_fire_in_priority_order_when_multiple_session_lifecycle_hooks_registered() {
    let hooks = SessionLifecycleHooks::new();
    let order = Arc::new(StdMutex::new(Vec::<String>::new()));

    let o1 = Arc::clone(&order);
    hooks
        .on_session_created(
            "after".to_string(),
            10,
            Box::new(move |_| {
                o1.lock().unwrap().push("after".to_string());
            }),
        )
        .await;

    let o2 = Arc::clone(&order);
    hooks
        .on_session_created(
            "before".to_string(),
            -10,
            Box::new(move |_| {
                o2.lock().unwrap().push("before".to_string());
            }),
        )
        .await;

    let o3 = Arc::clone(&order);
    hooks
        .on_session_created(
            "default".to_string(),
            0,
            Box::new(move |_| {
                o3.lock().unwrap().push("default".to_string());
            }),
        )
        .await;

    hooks.fire_session_created("session-1").await;

    let recorded = order.lock().unwrap();
    assert_eq!(
        *recorded,
        vec!["before", "default", "after"],
        "session-created hooks should fire in priority order"
    );
}

#[tokio::test]
async fn should_not_fire_hook_when_unregistered_from_session_lifecycle() {
    let hooks = SessionLifecycleHooks::new();
    let count = Arc::new(StdMutex::new(0u32));

    let c = Arc::clone(&count);
    hooks
        .on_session_created(
            "counter".to_string(),
            0,
            Box::new(move |_| {
                *c.lock().unwrap() += 1;
            }),
        )
        .await;

    hooks.fire_session_created("s1").await;
    assert_eq!(*count.lock().unwrap(), 1);

    // Unregister.
    {
        let mut slot = hooks.on_did_create_session.lock().await;
        slot.unregister("counter");
    }

    hooks.fire_session_created("s2").await;
    assert_eq!(
        *count.lock().unwrap(),
        1,
        "hook should not fire after unregistration"
    );
}

// ---------------------------------------------------------------------------
// Bridge with base_env
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_make_env_available_to_hook_when_bridge_triggered_with_base_env() {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        "echo $BRIDGE_TEST_VAR",
    ));

    let mut base_env = HashMap::new();
    base_env.insert(
        "BRIDGE_TEST_VAR".to_string(),
        "bridge-env-value".to_string(),
    );

    let bridge = ExternalHooksBridge::new(engine, base_env);

    let args = serde_json::json!({});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(matches!(result, PrepareToolResult::Allowed));
}

// ---------------------------------------------------------------------------
// Bridge with no hooks — no-op behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_allowed_when_prepare_tool_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, PrepareToolResult::Allowed),
        "no hooks should return Allowed"
    );
}

#[tokio::test]
async fn should_return_approved_when_authorize_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let approval_rule = "auto";
    let display = test_display();
    let ctx = AuthorizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule,
        display: &display,
        cancel: test_cancel(),
    };

    let result = bridge.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "no hooks should return Approved"
    );
}

#[tokio::test]
async fn should_return_true_when_should_continue_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let ctx = ContinueAfterStopContext {
        turn_id: "turn-1".to_string(),
        stop_reason: "end_turn".to_string(),
        cancel: test_cancel(),
    };

    let result = bridge.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "no hooks should return continue_turn=true"
    );
}

}

mod hook_engine_ {
//! Integration tests for the external command hook engine.
//!
//! These tests exercise the full pipeline: registration, matching, command
//! execution, structured output parsing, parallel trigger, and clear.

use std::collections::HashMap;

use mirri_core::hook_engine::{
    HookBlockSource, HookDef, HookEngine, HookEventType, HookOutcome, matches, run_hook,
};

/// The real local process provider — these tests exercise the hook engine
/// against actual shell commands (the fake-provider coverage lives in
/// `hook_process_provider_tests.rs`).
fn local_process() -> mirri_os::LocalProcess {
    mirri_os::LocalProcess::new()
}

// ---------------------------------------------------------------------------
// Registration & listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_vec_when_engine_is_empty() {
    let engine = HookEngine::new();
    let hooks = engine.list();
    assert!(hooks.is_empty(), "a fresh engine should have no hooks");
}

#[tokio::test]
async fn should_return_all_hooks_when_hooks_are_registered() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    engine.register(HookDef::new(HookEventType::SessionStart, "echo c"));

    let hooks = engine.list();
    assert_eq!(hooks.len(), 3, "all three hooks should be listed");
}

#[tokio::test]
async fn should_return_only_matching_hooks_when_listed_by_event() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo c"));

    let pre_hooks = engine.list_by_event(HookEventType::PreToolUse);
    assert_eq!(
        pre_hooks.len(),
        2,
        "two hooks should match PreToolUse event"
    );

    let post_hooks = engine.list_by_event(HookEventType::PostToolUse);
    assert_eq!(post_hooks.len(), 1, "one hook should match PostToolUse");

    let stop_hooks = engine.list_by_event(HookEventType::Stop);
    assert!(stop_hooks.is_empty(), "no hooks for Stop event");
}

// ---------------------------------------------------------------------------
// Matcher
// ---------------------------------------------------------------------------

#[test]
fn should_match_any_input_when_pattern_is_none() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(&None, &input, &HookEventType::PreToolUse));
}

#[test]
fn should_match_any_input_when_pattern_is_empty() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(
        &Some(String::new()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_match_when_regex_pattern_matches_tool_name() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(
        &Some("^Re.*".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_not_match_when_regex_pattern_does_not_match_tool_name() {
    let input = serde_json::json!({"tool_name": "Write"});
    assert!(!matches(
        &Some("^Read$".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_not_match_when_regex_pattern_is_invalid() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(!matches(
        &Some("[invalid".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_match_against_json_string_when_event_is_non_tool_event() {
    let input = serde_json::json!({"session_id": "abc-123"});
    assert!(matches(
        &Some("abc-123".to_string()),
        &input,
        &HookEventType::SessionStart
    ));
}

// ---------------------------------------------------------------------------
// run_hook — basic command execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_pass_with_stdout_when_echo_command_runs() {
    let hook = HookDef::new(HookEventType::PreToolUse, "echo hello-world");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("hello-world"),
                "stdout should contain echo output"
            );
            assert_eq!(result.exit_code, Some(0));
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_when_command_exits_zero() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 0");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "exit 0 should Pass"
    );
}

#[tokio::test]
async fn should_block_with_exit_code_2_when_command_exits_two() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 2");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Block {
            source,
            reason,
            message,
            ..
        } => {
            assert!(
                matches!(source, HookBlockSource::ExitCode2),
                "exit 2 → ExitCode2 source"
            );
            // A silent exit 2 carries no stderr, so both the reason and the
            // display message are empty (the TS resultFromExitCode shape);
            // the default "Blocked by <event> hook" text surfaces in the
            // render layer, not here.
            assert!(reason.is_empty(), "silent exit 2 reason should be empty");
            assert_eq!(message.as_deref(), Some(""));
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_not_block_when_command_exits_one() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 1");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "exit 1 (non-zero, non-2) should Pass, not Block"
    );
}

// ---------------------------------------------------------------------------
// run_hook — timeout handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_error_when_command_exceeds_timeout() {
    // `sleep 5` with a 200ms timeout should time out.
    let mut hook = HookDef::new(HookEventType::PreToolUse, "sleep 5");
    hook.timeout_ms = 200;

    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Error(msg) => {
            assert!(
                msg.contains("timed out"),
                "error message should mention timeout: {msg}"
            );
        }
        other => panic!("expected Error (timeout), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — structured output parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_block_structured_when_hook_outputs_deny_decision() {
    let command = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
    let hook = HookDef::new(HookEventType::PermissionRequest, command);
    let input = serde_json::json!({"tool_name": "Write"});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Block { source, .. } => {
            assert!(
                matches!(source, HookBlockSource::StructuredOutput),
                "permissionDecision=deny → StructuredOutput source"
            );
        }
        other => panic!("expected Block (structured), got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_when_hook_outputs_additional_context() {
    let command = r#"echo '{"additionalContext":"extra-info"}'"#;
    let hook = HookDef::new(HookEventType::PreToolUse, command);
    let input = serde_json::json!({});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("additionalContext"),
                "stdout should contain the structured JSON"
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — environment variables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_make_env_available_to_command_when_extra_env_provided() {
    // Print the env var so we can verify it was passed through.
    let hook = HookDef::new(HookEventType::PreToolUse, "echo $MY_HOOK_VAR");
    let input = serde_json::json!({});
    let mut extra_env = HashMap::new();
    extra_env.insert("MY_HOOK_VAR".to_string(), "hook-value-42".to_string());

    let outcome = run_hook(&local_process(), &hook, &input, &extra_env).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("hook-value-42"),
                "stdout should contain the env var value: {}",
                result.stdout
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

#[tokio::test]
async fn should_make_hook_env_available_when_hook_defines_env() {
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo $HOOK_DEFINED_VAR");
    hook.env = Some({
        let mut m = HashMap::new();
        m.insert("HOOK_DEFINED_VAR".to_string(), "from-hook-def".to_string());
        m
    });

    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("from-hook-def"),
                "stdout should contain the hook-defined env var"
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — stdin piping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_receive_json_on_stdin_when_json_input_provided() {
    // `cat` echoes stdin back to stdout.
    let hook = HookDef::new(HookEventType::PreToolUse, "cat");
    let input = serde_json::json!({"tool_name": "Read", "step": 42});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("tool_name"),
                "stdout should contain the piped JSON: {}",
                result.stdout
            );
            assert!(result.stdout.contains("Read"));
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// trigger — parallel execution & merging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_execute_all_hooks_when_multiple_hooks_match() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo hook-a"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo hook-b"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked, "neither hook should block");
    assert_eq!(
        result.results.len(),
        2,
        "both matching hooks should have results"
    );
}

#[tokio::test]
async fn should_skip_hook_when_pattern_does_not_match() {
    let engine = HookEngine::new();
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo should-not-run");
    hook.pattern = Some("^Write$".to_string());
    engine.register(hook);

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(
        result.results.is_empty(),
        "non-matching pattern should not execute the hook"
    );
}

#[tokio::test]
async fn should_execute_hook_when_pattern_matches() {
    let engine = HookEngine::new();
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo matching");
    hook.pattern = Some("^Read$".to_string());
    engine.register(hook);

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert_eq!(result.results.len(), 1, "matching hook should execute");
}

#[tokio::test]
async fn should_block_entire_result_when_one_hook_blocks() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo pass-me"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "exit 2"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(result.blocked, "one blocking hook should block the trigger");
    // The silent exit-2 hook carries no stderr, so the reason normalises to
    // None (consumer fallbacks supply the user-facing text downstream).
    assert!(
        result.block_reason.is_none(),
        "silent exit 2 normalises to an absent reason"
    );
}

#[tokio::test]
async fn should_not_block_when_all_hooks_pass() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo b"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked, "all-passing hooks should not block");
}

#[tokio::test]
async fn should_merge_contexts_when_hooks_provide_additional_context() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"additionalContext":"context-alpha"}'"#,
    ));
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"additionalContext":"context-beta"}'"#,
    ));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(
        result.additional_context.contains("context-alpha"),
        "merged context should contain first hook's context"
    );
    assert!(
        result.additional_context.contains("context-beta"),
        "merged context should contain second hook's context"
    );
}

#[tokio::test]
async fn should_propagate_input_when_hook_provides_updated_input() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"updatedInput":{"tool_name":"Read","modified":true}}'"#,
    ));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    let updated = result
        .updated_input
        .as_ref()
        .expect("updated_input should be set");
    assert_eq!(
        updated.get("modified").and_then(|v| v.as_bool()),
        Some(true),
        "updated_input should contain the modified field"
    );
}

#[tokio::test]
async fn should_return_empty_result_when_no_hooks_match() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo nope"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked);
    assert!(result.results.is_empty());
    assert!(result.additional_context.is_empty());
    assert!(result.updated_input.is_none());
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

/// Given a hook with a `cwd`, When the hook runs, Then the command executes
/// in that working directory (TS parity: the `cwd` field on `HookDef`).
#[tokio::test]
async fn should_execute_hook_in_its_working_directory_when_cwd_is_set() {
    let dir = tempfile::tempdir().expect("tempdir must be creatable");
    let mut hook = HookDef::new(
        HookEventType::PreToolUse,
        format!("pwd > {:?}", dir.path().join("pwd.txt")),
    );
    hook.cwd = Some(dir.path().to_string_lossy().to_string());

    let outcome = run_hook(
        &local_process(),
        &hook,
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;
    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "the hook should pass, got {outcome:?}"
    );

    let content = std::fs::read_to_string(dir.path().join("pwd.txt"))
        .expect("the marker should be written inside the hook's cwd");
    let canonical = |p: &str| std::path::Path::new(p).canonicalize().ok();
    assert_eq!(
        canonical(content.trim()),
        canonical(&dir.path().to_string_lossy()),
        "pwd must resolve to the hook's working directory, got {content}"
    );
}

#[tokio::test]
async fn should_remove_all_hooks_when_cleared() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    assert_eq!(engine.list().len(), 2);

    engine.clear();
    assert!(engine.list().is_empty(), "clear should remove all hooks");
    assert!(
        engine.list_by_event(HookEventType::PreToolUse).is_empty(),
        "clear should remove event-indexed hooks too"
    );

    // After clear, trigger should produce an empty result.
    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;
    assert!(
        result.results.is_empty(),
        "trigger after clear should run nothing"
    );
}

// ---------------------------------------------------------------------------
// Structured deny via trigger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_block_structured_when_trigger_runs_deny_hook() {
    let engine = HookEngine::new();
    let command = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
    engine.register(HookDef::new(HookEventType::PermissionRequest, command));

    let input = serde_json::json!({"tool_name": "Write"});
    let result = engine
        .trigger(HookEventType::PermissionRequest, input, &HashMap::new())
        .await;

    assert!(result.blocked, "structured deny should block");

    // Verify the block source is StructuredOutput (not ExitCode2).
    let block_outcome = result
        .results
        .iter()
        .find(|(_, o)| matches!(o, HookOutcome::Block { .. }))
        .map(|(_, o)| o)
        .expect("should have a blocking outcome");

    match block_outcome {
        HookOutcome::Block { source, .. } => {
            assert!(
                matches!(source, HookBlockSource::StructuredOutput),
                "block should come from structured output"
            );
        }
        _ => panic!("expected Block outcome"),
    }
}

// ---------------------------------------------------------------------------
// bridge — PermissionResult
// ---------------------------------------------------------------------------

}

mod hook_process_provider_ {
//! T-13c3-2 / T-13c3-3 — hook runner behaviour through a fake
//! [`DynOsProcess`] provider.
//!
//! These tests prove the hook runner is genuinely provider-injected: the
//! exit-code semantic table (0 → Pass, 2 → Block, other → Pass), the env
//! merge, the stdin JSON piping, and the timeout kill branch are all
//! exercised without any real child process (zero-IO T13c-3).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use mirri_core::hook_engine::{HookBlockSource, HookDef, HookEventType, HookOutcome, run_hook};
use mirri_os::{
    DynChildProcess, DynChildStdin, DynChildStream, DynOsProcess, DynProcessFuture, ExecOptions,
};
use tokio::io::AsyncWrite;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// An async writer that records everything written into a shared buffer
/// (the fake child's stdin pipe).
#[derive(Clone)]
struct StdinRecorder {
    buffer: Arc<StdMutex<Vec<u8>>>,
}

impl StdinRecorder {
    fn new() -> Self {
        Self {
            buffer: Arc::new(StdMutex::new(Vec::new())),
        }
    }
}

impl AsyncWrite for StdinRecorder {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A scripted child double: fixed stdout/stderr, a recorded exit code, and
/// a flag that flips when `kill` is invoked.
struct ScriptedHookChild {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdin: Option<StdinRecorder>,
    killed: Arc<AtomicBool>,
}

impl ScriptedHookChild {
    fn new(exit_code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            exit_code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdin: Some(StdinRecorder::new()),
            killed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle onto the stdin recorder (valid before the runner takes it).
    fn stdin_recorder(&self) -> StdinRecorder {
        self.stdin.clone().expect("stdin not yet taken")
    }
}

impl DynChildProcess for ScriptedHookChild {
    fn take_stdin_writer(&mut self) -> Option<DynChildStdin> {
        self.stdin
            .take()
            .map(|recorder| Box::new(recorder) as DynChildStdin)
    }

    fn take_stdout(&mut self) -> Option<DynChildStream> {
        Some(Box::new(std::io::Cursor::new(std::mem::take(
            &mut self.stdout,
        ))))
    }

    fn take_stderr(&mut self) -> Option<DynChildStream> {
        Some(Box::new(std::io::Cursor::new(std::mem::take(
            &mut self.stderr,
        ))))
    }

    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>> {
        let code = self.exit_code;
        Box::pin(async move {
            // Yield once so the runner's concurrent IO tasks get a turn
            // without burning wall-clock time.
            tokio::task::yield_now().await;
            Ok(code)
        })
    }

    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>> {
        self.killed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A slow child double: `wait` never resolves until `kill` flips the flag —
/// the shape a child sleeping past the hook timeout has from the runner's
/// perspective.
struct SlowHookChild {
    killed: Arc<AtomicBool>,
    killed_handle: Arc<AtomicBool>,
}

impl SlowHookChild {
    fn new() -> Self {
        let killed = Arc::new(AtomicBool::new(false));
        Self {
            killed_handle: Arc::clone(&killed),
            killed,
        }
    }
}

impl DynChildProcess for SlowHookChild {
    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>> {
        let killed = Arc::clone(&self.killed_handle);
        Box::pin(async move {
            while !killed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // A killed child reports signal termination — the erased trait's
            // convention for "no meaningful exit code".
            Ok(-1)
        })
    }

    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>> {
        self.killed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A recorded exec invocation: (program, args, merged env).
type RecordedExec = (String, Vec<String>, HashMap<String, String>);

/// A scripted process provider double: hands out one pre-built child per
/// `exec` and records every exec invocation.
struct ScriptedHookProcess {
    children: StdMutex<Vec<Box<dyn DynChildProcess>>>,
    execs: StdMutex<Vec<RecordedExec>>,
}

impl ScriptedHookProcess {
    fn with_child(child: Box<dyn DynChildProcess>) -> Self {
        Self {
            children: StdMutex::new(vec![child]),
            execs: StdMutex::new(Vec::new()),
        }
    }

    fn recorded_execs(&self) -> Vec<RecordedExec> {
        self.execs.lock().unwrap().clone()
    }
}

impl DynOsProcess for ScriptedHookProcess {
    fn exec<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>> {
        Box::pin(async move {
            self.execs.lock().unwrap().push((
                cmd.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
                opts.env.clone(),
            ));
            self.children
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow::anyhow!("no scripted child available"))
        })
    }
}

fn hook_with(command: &str) -> HookDef {
    HookDef {
        event: HookEventType::PreToolUse,
        command: command.to_string(),
        pattern: None,
        timeout_ms: 5_000,
        env: None,
        cwd: None,
        fail_closed: false,
    }
}

// ---------------------------------------------------------------------------
// T-13c3-2 — exit-code semantic table through a fake provider
// ---------------------------------------------------------------------------

/// Given a fake child exiting 2 with a stderr explanation, when the hook
/// runs, then the outcome is a Block carrying stderr as both the reason and
/// the display message (exit-code semantic table unchanged, T-13c3-2).
#[tokio::test]
async fn should_block_with_stderr_reason_when_fake_child_exits_two() {
    // Given a fake provider whose child exits 2 with stderr
    let child = ScriptedHookChild::new(2, "", "blocked by policy");
    let stdin_probe = child.stdin_recorder();
    let process = ScriptedHookProcess::with_child(Box::new(child));

    let mut hook = hook_with("some-hook-command");
    hook.env = Some(HashMap::from([(
        "HOOK_VAR".to_string(),
        "from-hook".to_string(),
    )]));

    // When the hook runs with extra env layered on top
    let extra = HashMap::from([
        ("EXTRA_VAR".to_string(), "from-extra".to_string()),
        ("HOOK_VAR".to_string(), "extra-wins".to_string()),
    ]);
    let outcome = run_hook(&process, &hook, &serde_json::json!({"k": "v"}), &extra).await;

    // Then the outcome blocks per the exit-2 row of the semantic table
    match outcome {
        HookOutcome::Block {
            reason,
            message,
            source,
            ..
        } => {
            assert!(
                matches!(source, HookBlockSource::ExitCode2),
                "exit 2 → ExitCode2 source"
            );
            assert_eq!(reason, "blocked by policy");
            assert_eq!(message.as_deref(), Some("blocked by policy"));
        }
        other => panic!("expected Block, got {other:?}"),
    }

    // And the exec reached the provider with the shell command and the
    // merged env (process env < hook env < extra env — extra wins).
    let execs = process.recorded_execs();
    assert_eq!(execs.len(), 1, "exactly one exec should be recorded");
    assert_eq!(execs[0].0, "/bin/sh", "hook commands run via /bin/sh");
    assert_eq!(
        execs[0].1,
        vec!["-c".to_string(), "some-hook-command".to_string()],
        "the hook command is the -c payload"
    );
    assert_eq!(
        execs[0].2.get("HOOK_VAR").map(String::as_str),
        Some("extra-wins"),
        "extra_env should override hook env"
    );
    assert_eq!(
        execs[0].2.get("EXTRA_VAR").map(String::as_str),
        Some("from-extra"),
        "extra_env should pass through"
    );

    // And the JSON input was piped to the child's stdin.
    let written = stdin_probe.buffer.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&written);
    assert!(
        text.contains("\"k\"") && text.contains("\"v\""),
        "stdin should carry the serialized JSON input, got {text:?}"
    );
}

/// Given fake children exiting 0 and 1, when the hooks run, then both
/// outcomes are non-blocking Passes carrying the captured stdout (the
/// non-2 rows of the exit-code semantic table, T-13c3-2).
#[tokio::test]
async fn should_pass_when_fake_child_exits_zero_or_other_non_two() {
    for (code, expect_stdout) in [(0, true), (1, false)] {
        // Given a fake provider whose child exits `code`
        let child = ScriptedHookChild::new(code, "captured output", "");
        let process = ScriptedHookProcess::with_child(Box::new(child));

        // When the hook runs
        let outcome = run_hook(
            &process,
            &hook_with("some-hook-command"),
            &serde_json::json!({}),
            &HashMap::new(),
        )
        .await;

        // Then the outcome is a Pass whose result carries the captured
        // stdout and the exit code
        match outcome {
            HookOutcome::Pass(result) => {
                assert_eq!(result.exit_code, Some(code), "exit code {code} passes");
                if expect_stdout {
                    assert_eq!(
                        result.stdout, "captured output",
                        "stdout capture should survive the provider hop"
                    );
                }
            }
            other => panic!("expected Pass for exit {code}, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// T-13c3-3 — timeout kill branch through a fake slow child
// ---------------------------------------------------------------------------

/// Given a fake child whose wait never resolves until killed, when the
/// hook's timeout elapses, then the child is killed and the failure follows
/// the fail-closed rule (timeout branch unchanged, T-13c3-3).
#[tokio::test]
async fn should_kill_the_slow_child_when_hook_times_out() {
    // Given a slow fake child and a fail-open hook with a 100ms timeout
    let child = SlowHookChild::new();
    let killed_after_run = child.killed_handle.clone();
    let process = ScriptedHookProcess::with_child(Box::new(child));
    let mut hook = hook_with("sleep forever");
    hook.timeout_ms = 100;

    // When the hook runs against the never-exiting child
    let started = std::time::Instant::now();
    let outcome = run_hook(&process, &hook, &serde_json::json!({}), &HashMap::new()).await;

    // Then the outcome is the timeout error…
    match outcome {
        HookOutcome::Error(message) => assert!(
            message.contains("timed out"),
            "expected a timeout message, got {message}"
        ),
        other => panic!("expected Error (timeout), got {other:?}"),
    }
    // …the child was killed (the M13c3-1 assertion: no kill → red)…
    assert!(
        killed_after_run.load(Ordering::SeqCst),
        "the timed-out child must be killed"
    );
    // …and the runner returned instead of hanging on the child's wait.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the timeout branch must reap the child, took {:?}",
        started.elapsed()
    );
}

/// Given the same slow child but a fail-closed hook, when the timeout
/// elapses, then the execution failure blocks (the fail-closed row of the
/// timeout branch, T-13c3-3).
#[tokio::test]
async fn should_block_with_execution_failure_when_slow_hook_times_out_fail_closed() {
    // Given a slow fake child and a fail-closed hook with a 100ms timeout
    let child = SlowHookChild::new();
    let process = ScriptedHookProcess::with_child(Box::new(child));
    let mut hook = hook_with("sleep forever");
    hook.timeout_ms = 100;
    hook.fail_closed = true;

    // When the hook runs
    let outcome = run_hook(&process, &hook, &serde_json::json!({}), &HashMap::new()).await;

    // Then the timeout blocks with the ExecutionFailure source
    match outcome {
        HookOutcome::Block { reason, source, .. } => {
            assert!(
                matches!(source, HookBlockSource::ExecutionFailure),
                "timeout + fail_closed → ExecutionFailure"
            );
            assert!(reason.contains("timed out"), "got {reason}");
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// spawn-failure branch
// ---------------------------------------------------------------------------

/// Given a provider with no scripted child, when the hook runs, then the
/// spawn failure follows the fail-closed rule with the original message
/// prefix (spawn-failure branch unchanged).
#[tokio::test]
async fn should_report_execution_failure_when_provider_cannot_spawn() {
    // Given a provider that has no child to hand out
    let process = ScriptedHookProcess::with_child(Box::new(ScriptedHookChild::new(0, "", "")));
    // Drain the single child so the next exec fails.
    let _ = run_hook(
        &process,
        &hook_with("first"),
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;

    // When the next hook runs against the exhausted provider
    let outcome = run_hook(
        &process,
        &hook_with("second"),
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;

    // Then the failure is recorded with the spawn-failure message
    match outcome {
        HookOutcome::Error(message) => assert!(
            message.contains("Failed to spawn hook command"),
            "got {message}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Given a hook engine constructed with a fake process provider, when
/// `trigger` runs a matching hook whose fake child exits 2, then the merged
/// trigger result is blocked — proving the provider is wired through the
/// engine's construction path (the same path `SessionScope` uses).
#[tokio::test]
async fn should_block_trigger_when_engine_injected_with_fake_process_exiting_two() {
    // Given an engine whose injected provider's child exits 2 with stderr
    let process = ScriptedHookProcess::with_child(Box::new(ScriptedHookChild::new(
        2,
        "",
        "denied by fake provider",
    )));
    let engine = mirri_core::hook_engine::HookEngine::with_process(Arc::new(process));
    engine.register(HookDef::new(HookEventType::PreToolUse, "scripted-command"));

    // When the trigger fires
    let result = engine
        .trigger(
            HookEventType::PreToolUse,
            serde_json::json!({"tool_name": "Read"}),
            &HashMap::new(),
        )
        .await;

    // Then the merged result is blocked with the exit-2 reason
    assert!(
        result.blocked,
        "the fake exit-2 hook should block the trigger"
    );
    assert_eq!(
        result.block_reason.as_deref(),
        Some("denied by fake provider"),
        "the block reason should carry the fake child's stderr"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

}

mod completion_budget_ {
//! Tests for the completion budget (`calculate_max_tokens`) and system prompt
//! placeholder replacement (`replace_placeholders`).
//!
//! - Completion budget: derives the max output tokens for a request from the
//!   current context size and the model's context-window limit, reserving a
//!   fixed safety margin for the response.
//! - Prompt placeholders: substitutes `{{workspace}}` / `{{platform}}` /
//!   `{{date}}` in system prompt templates without touching undefined tokens.

use mirri_core::context_size::calculate_max_tokens;
use mirri_core::prompt::{PlaceholderContext, replace_placeholders};

/// The response safety margin reserved by `calculate_max_tokens` (1024 tokens).
const SAFETY_MARGIN: usize = 1024;

// ---------------------------------------------------------------------------
// Completion budget
// ---------------------------------------------------------------------------

#[test]
fn should_calculate_max_tokens_as_model_limit_minus_context() {
    // Given a model with a 100k context window and a 20k-token context
    let context_tokens = 20_000;
    let model_limit = 100_000;

    // When computing the completion budget
    let max_tokens = calculate_max_tokens(context_tokens, model_limit);

    // Then the result is the leftover window minus the safety margin
    assert_eq!(max_tokens, model_limit - context_tokens - SAFETY_MARGIN);
}

#[test]
fn should_not_exceed_model_max_output_tokens() {
    // Given a model with a 200k context window
    let model_limit = 200_000;

    // When computing budgets for a range of context sizes
    for context_tokens in [0usize, 1, 1024, 50_000, 150_000, 199_000] {
        let max_tokens = calculate_max_tokens(context_tokens, model_limit);

        // Then the budget never exceeds the model's total limit
        assert!(max_tokens <= model_limit, "{max_tokens} > {model_limit}");

        // And never exceeds the remaining window (context + completion fits)
        assert!(
            max_tokens <= model_limit.saturating_sub(context_tokens),
            "context + completion would exceed the window"
        );
    }
}

#[test]
fn should_return_zero_when_context_exceeds_model_limit() {
    // Given a model with a 100k context window
    let model_limit = 100_000;

    // When the context is at or beyond the limit
    let at_limit = calculate_max_tokens(model_limit, model_limit);
    let beyond_limit = calculate_max_tokens(model_limit + 1, model_limit);
    let far_beyond = calculate_max_tokens(model_limit * 2, model_limit);

    // Then no completion tokens are granted
    assert_eq!(at_limit, 0);
    assert_eq!(beyond_limit, 0);
    assert_eq!(far_beyond, 0);
}

#[test]
fn should_return_zero_when_context_leaves_less_than_safety_margin() {
    // Given a model with a 100k context window
    let model_limit = 100_000;

    // When the leftover window is smaller than the safety margin
    let max_tokens = calculate_max_tokens(model_limit - SAFETY_MARGIN + 1, model_limit);

    // Then the budget saturates to zero rather than going negative
    assert_eq!(max_tokens, 0);
}

#[test]
fn should_reserve_safety_margin_for_response() {
    // Given a model with a 200k window and a 30k-token context
    let context_tokens = 30_000;
    let model_limit = 200_000;

    // When computing the completion budget
    let max_tokens = calculate_max_tokens(context_tokens, model_limit);

    // Then the safety margin is reserved: context + margin + budget == limit
    assert_eq!(
        context_tokens + SAFETY_MARGIN + max_tokens,
        model_limit,
        "the full exchange (context + completion) should fit in the window"
    );
}

// ---------------------------------------------------------------------------
// Prompt placeholder replacement
// ---------------------------------------------------------------------------

/// A fully-populated placeholder context for system prompt tests.
fn populated_context() -> PlaceholderContext {
    PlaceholderContext {
        workspace: Some("/home/dev/project".to_string()),
        platform: Some("linux".to_string()),
        date: Some("2025-03-15".to_string()),
    }
}

#[test]
fn should_replace_workspace_placeholder_in_system_prompt() {
    // Given a template with a workspace placeholder and a populated context
    let template = "Work in {{workspace}} and stay there.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the workspace value is substituted
    assert_eq!(rendered, "Work in /home/dev/project and stay there.");
}

#[test]
fn should_replace_platform_placeholder_in_system_prompt() {
    // Given a template with a platform placeholder and a populated context
    let template = "You are running on {{platform}}.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the platform value is substituted
    assert_eq!(rendered, "You are running on linux.");
}

#[test]
fn should_replace_date_placeholder_in_system_prompt() {
    // Given a template with a date placeholder and a populated context
    let template = "Today is {{date}}.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the date value is substituted
    assert_eq!(rendered, "Today is 2025-03-15.");
}

#[test]
fn should_replace_all_placeholders_when_context_is_populated() {
    // Given a template using every supported placeholder
    let template = "[{{date}}] {{platform}} agent in {{workspace}}";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then each placeholder is substituted in a single pass
    assert_eq!(rendered, "[2025-03-15] linux agent in /home/dev/project");
}

#[test]
fn should_leave_undefined_placeholders_unchanged() {
    // Given a template referencing placeholders with no context values
    let template = "Run in {{workspace}} on {{platform}} at {{date}}.";

    // When replacing with an empty context
    let rendered = replace_placeholders(template, &PlaceholderContext::default());

    // Then every token stays verbatim
    assert_eq!(rendered, template);
}

#[test]
fn should_leave_unknown_placeholders_unchanged() {
    // Given a template with an unsupported token alongside known ones
    let template = "{{workspace}} via {{unknown}}";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the known token is replaced and the unknown token stays verbatim
    assert_eq!(rendered, "/home/dev/project via {{unknown}}");
}

#[test]
fn should_not_recursively_expand_nested_placeholders() {
    // Given a context whose value contains another placeholder token
    let ctx = PlaceholderContext {
        workspace: Some("{{platform}}".to_string()),
        platform: Some("linux".to_string()),
        ..Default::default()
    };

    // When replacing placeholders in a template using that value
    let rendered = replace_placeholders("echo {{workspace}}", &ctx);

    // Then the substituted value is emitted as-is, without re-expansion
    assert_eq!(rendered, "echo {{platform}}");
}

#[test]
fn should_not_recursively_expand_self_referential_placeholder() {
    // Given a context whose value references its own placeholder
    let ctx = PlaceholderContext {
        workspace: Some("{{workspace}}".to_string()),
        ..Default::default()
    };

    // When replacing placeholders
    let rendered = replace_placeholders("cd {{workspace}}", &ctx);

    // Then the value is emitted verbatim (no infinite recursion)
    assert_eq!(rendered, "cd {{workspace}}");
}

#[test]
fn should_keep_unclosed_placeholder_verbatim() {
    // Given a template with an unclosed `{{` token
    let template = "prefix {{workspace and trailing";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the unclosed token is untouched
    assert_eq!(rendered, template);
}

}

mod context_memory_ {
//! Integration tests for the `context_memory` module.
//!
//! These verify the load-bearing invariants of `ContextMemory`, the projection
//! pipeline, token estimation, compaction, and undo.

use mirri_core::context::{ContextMessage, ProjectOptions};
use mirri_core::context_memory::select_compaction_user_messages;
use mirri_core::context_memory::token_estimator::{
    MEDIA_TOKEN_ESTIMATE, estimate_message_tokens, estimate_tokens, truncate_text_to_tokens,
    truncate_text_to_tokens_from_end,
};
use mirri_core::message::{ContentPart, ImageUrlData, Message, Role, ToolCall, ToolCallFunction};
use mirri_core::origin::PromptOrigin;
use mirri_core::tool::ToolOutput;
use mirri_core::{CompactionInput, ContextMemory, LoopEvent};

use serde_json::json;

// ---------------------------------------------------------------------------
// Helper builders
// ---------------------------------------------------------------------------

fn user_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::user(text),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn assistant_text_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::assistant(text),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn assistant_tool_call_msg(tool_call_id: &str, tool_name: &str) -> ContextMessage {
    ContextMessage {
        message: Message {
            role: Role::Assistant,
            name: None,
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: tool_call_id.to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: tool_name.to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            tool_call_id: None,
            partial: false,
        },
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn tool_result_msg(tool_call_id: &str, output: &str) -> ContextMessage {
    ContextMessage {
        message: Message::tool_result(tool_call_id, output),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn tool_result_error_msg(tool_call_id: &str, output: &str) -> ContextMessage {
    let mut msg = tool_result_msg(tool_call_id, output);
    msg.is_error = true;
    msg
}

// ---------------------------------------------------------------------------
// 1. Basic append and history
// ---------------------------------------------------------------------------

#[test]
fn should_store_user_message_with_correct_role_and_origin_when_appended() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello world".to_string(),
        }],
        PromptOrigin::User,
    );

    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "hello world".to_string()
        }
    );
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::User));
}

// ---------------------------------------------------------------------------
// 2. Deferred messages during open exchange
// ---------------------------------------------------------------------------

#[test]
fn should_defer_user_messages_when_tool_exchange_is_open() {
    let mut mem = ContextMemory::new();

    // Start a step and make a tool call → opens an exchange.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());

    // Append a user message → should be deferred, not in history.
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "deferred user msg".to_string(),
        }],
        PromptOrigin::User,
    );
    // History should have: [assistant(partial, tool_call)] but NOT the user msg.
    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.role, Role::Assistant);

    // Provide the tool result → closes the exchange, flushes deferred.
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_1".to_string(),
        output: ToolOutput::text("file contents"),
        is_error: false,
        note: None,
        entry_seq: None,
    });

    assert!(!mem.has_open_tool_exchange());
    // History should now be: [assistant, tool_result, user_msg]
    assert_eq!(mem.history().len(), 3);
    assert_eq!(mem.history()[0].message.role, Role::Assistant);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(mem.history()[2].message.role, Role::User);
    assert_eq!(
        mem.history()[2].message.content[0],
        ContentPart::Text {
            text: "deferred user msg".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// 3. Multiple tool calls in one step
// ---------------------------------------------------------------------------

#[test]
fn should_keep_exchange_open_when_multiple_tool_calls_pending() {
    let mut mem = ContextMemory::new();

    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_a".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_b".to_string(),
        tool_name: "write_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());
    assert_eq!(mem.history().len(), 1);
    assert_eq!(mem.history()[0].message.tool_calls.len(), 2);

    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_a".to_string(),
        output: ToolOutput::text("result_a"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    // Still open — call_b pending.
    assert!(mem.has_open_tool_exchange());

    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call_b".to_string(),
        output: ToolOutput::text("result_b"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    assert!(!mem.has_open_tool_exchange());

    // History: [assistant(2 tool_calls), tool_result_a, tool_result_b]
    assert_eq!(mem.history().len(), 3);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(
        mem.history()[1].message.tool_call_id,
        Some("call_a".to_string())
    );
    assert_eq!(mem.history()[2].message.role, Role::Tool);
    assert_eq!(
        mem.history()[2].message.tool_call_id,
        Some("call_b".to_string())
    );
}

// ---------------------------------------------------------------------------
// 4. Close pending tool results (synthesize error)
// ---------------------------------------------------------------------------

#[test]
fn should_synthesize_error_results_when_closing_pending_tool_results() {
    let mut mem = ContextMemory::new();

    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step1".to_string(),
        tool_call_id: "call_x".to_string(),
        tool_name: "read_file".to_string(),
        args: json!({}),
    });

    assert!(mem.has_open_tool_exchange());

    mem.close_pending_tool_results();

    assert!(!mem.has_open_tool_exchange());
    // History: [assistant, synthetic_tool_result]
    assert_eq!(mem.history().len(), 2);
    assert_eq!(mem.history()[1].message.role, Role::Tool);
    assert_eq!(
        mem.history()[1].message.tool_call_id,
        Some("call_x".to_string())
    );
    assert!(mem.history()[1].is_error);
}

// ---------------------------------------------------------------------------
// 5. Projection: normal (strips metadata)
// ---------------------------------------------------------------------------

#[test]
fn should_strip_metadata_and_produce_correct_message_sequence_when_projecting() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "s1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "s1".to_string(),
        text: "I will use a tool".to_string(),
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "s1".to_string(),
        tool_call_id: "tc1".to_string(),
        tool_name: "read".to_string(),
        args: json!({}),
    });
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "tc1".to_string(),
        output: ToolOutput::text("output data"),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::StepEnd {
        step_id: "s1".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });

    let messages = mem.messages();
    // Should be: [user, assistant(text + tool_call), tool_result]
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[2].role, Role::Tool);

    // Verify metadata is stripped: messages are plain Message, no origin field.
    // (We can't check origin directly on Message, but we verify the content is
    // wire-valid.)
    assert!(!messages[0].content.is_empty());
}

// ---------------------------------------------------------------------------
// 6. Projection: orphan tool result dropped
// ---------------------------------------------------------------------------

#[test]
fn should_drop_orphan_tool_results_when_projecting() {
    // Build a history with an orphan tool result (no matching tool call).
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hi".to_string(),
        }],
        PromptOrigin::User,
    );
    // Orphan tool result — no assistant with tool_call "orphan_id" exists.
    mem.append_message(tool_result_msg("orphan_id", "orphan output"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // The orphan should be dropped; only the user message survives.
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, Role::User);
}

// ---------------------------------------------------------------------------
// 7. Projection: adjacency repair (tool result moved adjacent to call)
// ---------------------------------------------------------------------------

#[test]
fn should_move_tool_result_adjacent_to_call_when_projecting() {
    // Build: [user, assistant(tool_call), user, tool_result]
    // The tool result is not adjacent to its call — it should be moved up.
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_adj", "read"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "injected".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(tool_result_msg("tc_adj", "result"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // After repair, the tool result should be adjacent to the assistant.
    // Expected order: [user, user(merged), assistant, tool_result, user]
    // or similar. The key check: tool_result is right after the assistant.
    let assistant_idx = result
        .iter()
        .position(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("assistant with tool_call must exist");
    assert_eq!(result[assistant_idx + 1].role, Role::Tool);
    assert_eq!(
        result[assistant_idx + 1].tool_call_id,
        Some("tc_adj".to_string())
    );
}

// ---------------------------------------------------------------------------
// 8. Projection: synthesize missing (trailing)
// ---------------------------------------------------------------------------

#[test]
fn should_synthesize_missing_trailing_tool_results_when_projecting() {
    // Build: [user, assistant(tool_call)] with no tool result — trailing open.
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_miss", "read"));

    // With synthesize_missing=true, a synthetic result should be added.
    let options = ProjectOptions {
        synthesize_missing: true,
        drop_orphan_results: true,
        ..Default::default()
    };
    let result = mem.project(&options).unwrap();

    // Expected: [user, assistant, synthetic_tool_result]
    assert_eq!(result.len(), 3);
    assert_eq!(result[2].role, Role::Tool);
    assert_eq!(result[2].tool_call_id, Some("tc_miss".to_string()));
}

// ---------------------------------------------------------------------------
// 9. Token estimation
// ---------------------------------------------------------------------------

#[test]
fn should_estimate_tokens_correctly_when_message_contains_ascii_cjk_or_media() {
    // ASCII: 8 chars / 4 = 2 tokens
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    // CJK: 3 chars = 3 tokens
    assert_eq!(estimate_tokens("你好吗"), 3);

    // Message with image: flat 2000 tokens
    let img_msg = ContextMessage {
        message: Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "http://example.com/img.png".to_string(),
                    detail: None,
                    id: None,
                },
                detail: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        },
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    };
    assert_eq!(estimate_message_tokens(&img_msg), MEDIA_TOKEN_ESTIMATE);
}

// ---------------------------------------------------------------------------
// 10. Truncate to tokens (beginning and end)
// ---------------------------------------------------------------------------

#[test]
fn should_truncate_text_from_beginning_and_end_when_truncating_to_token_limit() {
    // 8 ASCII chars = 2 tokens. Truncate to 1 token → first 4 chars.
    let text = "abcdefgh";
    let from_begin = truncate_text_to_tokens(text, 1);
    assert_eq!(from_begin, "abcd");

    // From end: last 4 chars.
    let from_end = truncate_text_to_tokens_from_end(text, 1);
    assert_eq!(from_end, "efgh");

    // CJK: 3 chars = 3 tokens. Truncate to 1 → first 1 char.
    let cjk = "你好吗";
    let cjk_begin = truncate_text_to_tokens(cjk, 1);
    assert_eq!(cjk_begin, "你");
    let cjk_end = truncate_text_to_tokens_from_end(cjk, 1);
    assert_eq!(cjk_end, "吗");
}

// ---------------------------------------------------------------------------
// 11. Compaction user message selection
// ---------------------------------------------------------------------------

#[test]
fn should_keep_all_user_messages_when_compaction_budget_is_sufficient() {
    let mut history = Vec::new();

    // 5 user messages with different origins.
    for i in 0..5 {
        history.push(user_msg(&format!("user message {i}")));
    }
    // An injection (should be excluded).
    history.push(ContextMessage {
        message: Message::user("injection text"),
        origin: Some(PromptOrigin::Injection),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    // A compaction summary (should be excluded).
    history.push(ContextMessage {
        message: Message::user("summary"),
        origin: Some(PromptOrigin::CompactionSummary),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });

    let selection = select_compaction_user_messages(&history, 20_000);

    // All 5 real user messages should be kept, no elision (small enough).
    assert!(!selection.elided);
    assert_eq!(selection.kept.len(), 5);
    assert_eq!(selection.omitted_tokens, 0);
}

#[test]
fn should_elide_messages_when_compaction_budget_exceeded() {
    let mut history = Vec::new();

    // Create a large set of user messages that exceed the 20k token budget.
    // Each message ~100 ASCII chars = 25 tokens. Need >800 messages to exceed.
    // Instead, use fewer messages with CJK (1 token per char) to exceed faster.
    // 500 CJK chars per message × 50 messages = 25000 tokens.
    let big_text: String = "你".repeat(500);
    for _ in 0..50 {
        history.push(user_msg(&big_text));
    }

    let selection = select_compaction_user_messages(&history, 20_000);

    assert!(selection.elided);
    assert!(selection.omitted_tokens > 0);
    // Should have both head and tail messages.
    assert!(!selection.kept.is_empty());
}

// ---------------------------------------------------------------------------
// 12. Apply compaction
// ---------------------------------------------------------------------------

#[test]
fn should_rewrite_history_with_summary_and_kept_messages_when_compaction_applied() {
    let mut mem = ContextMemory::new();

    // Build a history with user + assistant + tool messages.
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "do something".to_string(),
        }],
        PromptOrigin::User,
    );
    // Large assistant/tool bodies so the compaction rewrite genuinely
    // shrinks the history — a tiny memory cannot pass the shrink check
    // (P8a-3 变小校验) and is rejected by design.
    mem.append_message(assistant_text_msg(&format!(
        "I will help: {}",
        "x".repeat(8_000)
    )));
    mem.append_message(assistant_tool_call_msg("tc_c1", "read"));
    mem.append_message(tool_result_msg(
        "tc_c1",
        &format!("data: {}", "y".repeat(8_000)),
    ));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "thanks".to_string(),
        }],
        PromptOrigin::User,
    );

    let input = CompactionInput {
        summary: "Conversation summary".to_string(),
        context_summary: "Context summary text".to_string(),
        compacted_count: 4,
        tokens_before: 1000,
        dropped_count: 0,
        // Retention tail = the newest user message; the prefix ends right
        // after the closed tool exchange (a legal cut).
        retention_tail_start: Some(4),
    };

    let outcome = mem.apply_compaction(input);

    // New history = [head essence user][summary (ADR-002)][retention tail]
    assert!(outcome.applied);
    assert_eq!(mem.history().len(), 3);
    // First should be the kept head user message (verbatim).
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::User));
    // The summary (CompactionSummary origin) sits BEFORE the tail.
    assert_eq!(mem.history()[1].message.role, Role::User);
    assert_eq!(
        mem.history()[1].origin,
        Some(PromptOrigin::CompactionSummary)
    );
    // The retention tail is kept verbatim.
    assert_eq!(mem.history()[2].message.role, Role::User);
    assert_eq!(mem.history()[2].message.text_content(), "thanks");

    // Token count should be re-estimated.
    assert!(mem.token_count() > 0);
    // Open exchange state should be cleared.
    assert!(!mem.has_open_tool_exchange());
}

// ---------------------------------------------------------------------------
// 13. Undo
// ---------------------------------------------------------------------------

#[test]
fn should_remove_last_user_and_trailing_messages_when_undo_called() {
    let mut mem = ContextMemory::new();

    mem.append_user_message(
        vec![ContentPart::Text {
            text: "first prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("first response"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "second prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("second response"));

    assert_eq!(mem.history().len(), 4);

    mem.undo().unwrap();

    // Undo should remove "second prompt" and "second response".
    assert_eq!(mem.history().len(), 2);
    assert_eq!(mem.history()[0].message.role, Role::User);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "first prompt".to_string()
        }
    );
    assert_eq!(mem.history()[1].message.role, Role::Assistant);
}

// ---------------------------------------------------------------------------
// Extra: system reminder wrapping
// ---------------------------------------------------------------------------

#[test]
fn should_wrap_reminder_in_system_tag_when_appending_system_reminder() {
    let mut mem = ContextMemory::new();
    mem.append_system_reminder("reminder text".to_string());

    assert_eq!(mem.history().len(), 1);
    if let ContentPart::Text { text } = &mem.history()[0].message.content[0] {
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("reminder text"));
        assert!(text.contains("</system-reminder>"));
    } else {
        panic!("expected text content part");
    }
    assert_eq!(mem.history()[0].origin, Some(PromptOrigin::Injection));
}

// ---------------------------------------------------------------------------
// Extra: tool result error rendering in projection
// ---------------------------------------------------------------------------

#[test]
fn should_render_error_prefix_when_projecting_tool_result_error() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_tool_call_msg("tc_err", "read"));
    mem.append_message(tool_result_error_msg("tc_err", "tool failed output"));

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // Find the tool result.
    let tool_msg = result
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("tool result must exist");

    if let ContentPart::Text { text } = &tool_msg.content[0] {
        assert!(text.contains("<system>ERROR: Tool execution failed.</system>"));
        assert!(text.contains("tool failed output"));
    } else {
        panic!("expected text content part");
    }
}

// ---------------------------------------------------------------------------
// Extra: projection drops partial messages
// ---------------------------------------------------------------------------

#[test]
fn should_drop_partial_messages_when_projecting() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "prompt".to_string(),
        }],
        PromptOrigin::User,
    );
    // Push a partial assistant message (via StepBegin without StepEnd).
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "partial_step".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "partial_step".to_string(),
        text: "partial content".to_string(),
    });

    let options = ProjectOptions::normal();
    let result = mem.project(&options).unwrap();

    // The partial assistant should be dropped; only user remains.
    // (Partial messages are dropped in prepare_message_for_projection.)
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, Role::User);
}

// ---------------------------------------------------------------------------
// Extra: undo with count > 1
// ---------------------------------------------------------------------------

#[test]
fn should_remove_multiple_pairs_when_undo_count_called() {
    let mut mem = ContextMemory::new();

    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p1".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a1"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p2".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a2"));
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p3".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(assistant_text_msg("a3"));

    assert_eq!(mem.history().len(), 6);

    mem.undo_count(2).unwrap();

    // Should remove p3+a3+p2+a2, leaving p1+a1.
    assert_eq!(mem.history().len(), 2);
    assert_eq!(
        mem.history()[0].message.content[0],
        ContentPart::Text {
            text: "p1".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// Extra: undo at compaction boundary fails gracefully
// ---------------------------------------------------------------------------

#[test]
fn should_fail_undo_when_crossing_compaction_boundary() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p1".to_string(),
        }],
        PromptOrigin::User,
    );
    // Insert a compaction summary.
    mem.append_message(ContextMessage {
        message: Message::user("summary"),
        origin: Some(PromptOrigin::CompactionSummary),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "p2".to_string(),
        }],
        PromptOrigin::User,
    );

    // Undo should only remove p2 (can't cross compaction boundary).
    mem.undo().unwrap();
    assert_eq!(mem.history().len(), 2);

    // Second undo should fail (would cross compaction boundary).
    let result = mem.undo();
    assert!(result.is_err());
}

}

mod subagent_integration_ {
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
        turn_origin: Default::default(),
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
        turn_origin: Default::default(),
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
        turn_origin: Default::default(),
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

}

mod agent_list_cross_engine_ {
//! Cross-engine list-projection consistency acceptance (R4-6 / US-C1):
//! the rust agent-profile discovery + merge + wire-row projection must
//! agree with the TS `GET /api/v1/agents` catalog projection on the same
//! corpus, row by row.
//!
//! The corpus lives in a single shared location —
//! `packages/agent-profile/test/fixtures/cross-engine-list/` — and is
//! consumed by BOTH engines' test suites from there (no copies): this
//! test and the TS `agents-cross-engine-list.test.ts` compare against the
//! same `expected-list.json` (regenerated from the TS side via
//! `UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1`, the reference implementation).
//!
//! Root mounting (see the fixture README for the full contract): the
//! corpus `project-agents/` directory is mounted as a `Project`-tagged
//! root FIRST (mirroring the TS catalog's `workspace 30 > user 10` source
//! priority) and `user-agents/` as the `User`-tagged root second — the
//! same layout the TS suite mounts as the registered workspace's
//! `.mirri-code/agents` and the `<home>/agents` user root. The
//! `legacy-profiles/` directory is rust-only (the TS engine cannot read
//! `.toml`/`.json` agent files — the documented exception) and is
//! asserted separately, never through the shared expectation.
//!
//! Reading fixture files here is test-only IO: the zero-IO gate scans the
//! production `src` tree only, and this file lives in `tests/`.
//!
//! Normalization convention (mirrors the TS side exactly — see the fixture
//! README for the full contract):
//! - rows sorted by name; every projected field comes from the shared
//!   wire-row base projection
//!   ([`ProfileManager::profile_wire_row_base`] — the same function the
//!   embedded server's REST row composes, so this test asserts the real
//!   projection, not a test-side copy);
//! - `suppressed_candidates` emitted only when non-empty, `{source,
//!   priority, reason}` with the reason spelling `priority`/`disabled`;
//! - `skipped` entries carry the file **basename** and a `<load-failure>`
//!   placeholder reason — the two engines word their parse-error messages
//!   differently by design (the TS text also embeds the TS-side mount
//!   path), so only the presence and identity of the skipped file is
//!   asserted (the underlying reason is asserted non-empty locally).

use std::fs;
use std::path::PathBuf;

use mirri_core::ProfileManager;
use mirri_core::profile::{
    ProfileDiscoveryResult, ProfileSource, ProfileSourceRoot, SkippedProfileFile,
    SuppressedProfileCandidate, SuppressedReason, discover_profiles,
};
use mirri_os::LocalSyncFs;
use serde_json::{Value, json};

/// Root of the shared cross-engine list fixture directory, relative to
/// this crate's manifest (`rust/crates/mirri-core/tests/fixtures/agent-profile/
/// `packages/…`).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent-profile/cross-engine-list")
}

/// The shared root mounting: project root first (priority 30), user root
/// second (priority 10) — the same relative order the TS catalog's source
/// priorities produce, so a same-name project file wins its merge.
fn shared_roots() -> Vec<ProfileSourceRoot> {
    vec![
        ProfileSourceRoot {
            path: fixture_root().join("project-agents"),
            source: ProfileSource::Project,
        },
        ProfileSourceRoot {
            path: fixture_root().join("user-agents"),
            source: ProfileSource::User,
        },
    ]
}

/// Discover the roots and register the winners into a fresh manager —
/// the boot bookkeeping [`ProfileManager::load_from_roots`] performs
/// (registration + provenance + the suppressed-candidates projection),
/// with tagged roots that the user-level-only loader cannot express.
/// Returns the manager plus the raw discovery result (whose skip channel
/// is the data `load_from_roots` would surface).
fn boot_manager(roots: &[ProfileSourceRoot]) -> (ProfileManager, ProfileDiscoveryResult) {
    let result = discover_profiles(roots, &LocalSyncFs).expect("discovery should succeed");
    let manager = ProfileManager::new();
    for discovered in &result.profiles {
        manager
            .register_discovered(discovered)
            .expect("registration should succeed");
    }
    manager.set_suppressed_candidates(result.suppressed.clone());
    (manager, result)
}

/// Normalize the skip channel onto the shared projection shape: basename
/// plus the `<load-failure>` placeholder reason (the engines word their
/// parse errors differently; the reason text itself is asserted non-empty
/// separately).
fn normalize_skipped(skipped: &[SkippedProfileFile]) -> Vec<Value> {
    let mut entries: Vec<(String, Value)> = skipped
        .iter()
        .map(|file| {
            let base = file
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.display().to_string());
            let reason = if file.reason.is_empty() {
                "<empty-reason>"
            } else {
                "<load-failure>"
            };
            (base.clone(), json!({ "path": base, "reason": reason }))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(_, value)| value).collect()
}

#[test]
fn cross_engine_list_projection_matches_shared_expected_list() {
    let (manager, result) = boot_manager(&shared_roots());

    let profiles: Vec<Value> = manager
        .list_all()
        .iter()
        .map(|name| {
            manager
                .profile_wire_row_base(name)
                .expect("every listed name is registered and projects a row")
        })
        .collect();
    let actual = json!({
        "profiles": profiles,
        "skipped": normalize_skipped(&result.skipped),
    });

    let expected_text = fs::read_to_string(fixture_root().join("expected-list.json"))
        .expect("expected-list.json must exist (regenerate from the TS side)");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");

    // Compare row-by-row so a mismatch names the offending profile.
    let expected_rows = expected["profiles"]
        .as_array()
        .expect("expected profiles must be an array");
    let actual_rows = actual["profiles"]
        .as_array()
        .expect("actual profiles must be an array");
    assert_eq!(
        actual_rows.len(),
        expected_rows.len(),
        "rust list row count differs from the shared expected-list.json"
    );
    for (index, expected_row) in expected_rows.iter().enumerate() {
        assert_eq!(
            &actual_rows[index], expected_row,
            "rust row {} differs from the shared expected-list.json \
             (regenerate from the TS side with UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1 \
             if the change is intentional — the rust side must then still pass unedited)",
            expected_row["name"],
        );
    }
    assert_eq!(
        actual["skipped"], expected["skipped"],
        "rust skip channel differs from the shared expected-list.json"
    );
}

/// The rust-only legacy root (documented exception): `.toml` / `.json`
/// agent files load through the legacy `profiles/` root — a surface the
/// TS engine does not have — and a broken legacy `extends` covering a
/// built-in name must leave the built-in row clean (the Task 27 review
/// M1 regression, at integration level).
#[test]
fn legacy_root_loads_rust_only_formats_and_keeps_the_builtin_row_clean() {
    let mut roots = shared_roots();
    roots.push(ProfileSourceRoot {
        path: fixture_root().join("legacy-profiles"),
        source: ProfileSource::User,
    });
    let (manager, result) = boot_manager(&roots);

    // The legacy formats register as user-level profiles through the
    // legacy root (lowest priority — after the user agents root, the real
    // boot chain's order).
    for name in ["veteran", "archivist"] {
        let provenance = manager
            .provenance(name)
            .unwrap_or_else(|| panic!("legacy profile {name} must be registered"));
        assert_eq!(provenance.source, ProfileSource::User);
        assert!(manager.suppressed_candidates(name).is_empty());
        assert!(manager.get(name).is_some());
    }

    // The broken-extends `.toml` covering the built-in `coder` name: the
    // file is a legitimate load failure, so the built-in row stays clean
    // (builtin provenance, NO suppressed candidates — no self-referential
    // phantom builtin loser) and the file lands in the skip channel with
    // a non-empty reason.
    assert_eq!(
        manager.provenance("coder").map(|p| p.source),
        Some(ProfileSource::Builtin)
    );
    assert!(manager.suppressed_candidates("coder").is_empty());
    let broken = result
        .skipped
        .iter()
        .find(|file| file.path.ends_with("coder.toml"))
        .expect("the broken legacy coder override must be skipped");
    assert!(!broken.reason.is_empty());

    // The broken file must not disturb the shared corpus's own rows.
    for name in [
        "agent", "coder", "explore", "guarded", "mystery", "plan", "plain", "shared", "sleeper",
    ] {
        assert!(
            manager.get(name).is_some(),
            "profile {name} must stay registered with the legacy root mounted"
        );
    }
}

/// The same shared corpus mounted OUT OF priority order (the user root
/// first, the project root second) must produce the identical projection —
/// the same-name winner is decided by source priority inside discovery
/// (T19 m1), never by the caller's root order, so the disordered mounting
/// is equivalent to the canonical one by construction.
#[test]
fn cross_engine_list_projection_is_root_order_independent() {
    let mut roots = shared_roots();
    roots.reverse();
    let (manager, result) = boot_manager(&roots);

    // The priority-decided winners: the project file wins `shared`, the
    // user file wins `guarded` (its project candidate is disabled), and the
    // user file covers the essential — exactly as in the canonical order.
    assert_eq!(
        manager.provenance("shared").map(|p| p.source),
        Some(ProfileSource::Project)
    );
    assert_eq!(
        manager.provenance("guarded").map(|p| p.source),
        Some(ProfileSource::User)
    );
    assert_eq!(
        manager.suppressed_candidates("shared"),
        vec![SuppressedProfileCandidate {
            source: ProfileSource::User,
            reason: SuppressedReason::Priority,
        }]
    );

    // And the full row-by-row projection still matches the shared
    // expected-list.json (the same acceptance as the canonical mounting).
    let profiles: Vec<Value> = manager
        .list_all()
        .iter()
        .map(|name| {
            manager
                .profile_wire_row_base(name)
                .expect("every listed name is registered and projects a row")
        })
        .collect();
    let actual = json!({
        "profiles": profiles,
        "skipped": normalize_skipped(&result.skipped),
    });

    let expected_text = fs::read_to_string(fixture_root().join("expected-list.json"))
        .expect("expected-list.json must exist (regenerate from the TS side)");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");
    assert_eq!(
        actual, expected,
        "the disordered mounting must produce the same projection as the canonical one"
    );
}

}

mod agent_md_cross_engine_ {
//! Cross-engine consistency acceptance (R2-4): the rust `.md` agent profile
//! parser must produce field-by-field identical results to the TS
//! `parseAgentFileText` on the shared fixture corpus.
//!
//! The corpus lives in a single shared location —
//! `packages/agent-profile/test/fixtures/cross-engine/corpus/` — and is
//! consumed by BOTH engines' test suites from there (no copies): this test
//! and the TS `cross-engine-fixture.test.ts` compare against the same
//! `expected-outputs.json` (regenerated from the TS side via
//! `UPDATE_CROSS_ENGINE_SNAPSHOT=1 npx vitest run`).
//!
//! Reading fixture files here is test-only IO: the zero-IO gate scans the
//! production `src` tree only, and this file lives in `tests/`.
//!
//! Normalization convention (mirrors the TS side exactly — see the fixture
//! README for the full contract):
//! - success: `{ ok: true, def: { … } }` with the eleven `AgentFileDef`
//!   payload fields, keys in alphabetical order, absent/`None` optional
//!   fields as explicit `null` (never omitted);
//! - failure: `{ ok: false, error: "<reason>" }` — the full
//!   `AgentFileParseError` message (which embeds the synthetic path);
//! - `source` is excluded (both sides parse without a discovery source);
//! - each fixture is parsed with the synthetic path `agents/<file-name>`.

use std::fs;
use std::path::PathBuf;

use mirri_core::profile::agent_md::{AgentFileDef, parse_agent_file_text};
use serde_json::{Map, Value, json};

/// Root of the shared cross-engine fixture directory, relative to this
/// crate itself (`rust/crates/mirri-core/tests/fixtures/agent-profile/…`).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent-profile/cross-engine")
}

/// Normalizes a parsed def: alphabetical keys, `None` → explicit `null`.
fn normalize_def(def: &AgentFileDef) -> Value {
    json!({
        "ok": true,
        "def": {
            "capabilitiesRequired": def.capabilities_required,
            "defaultModel": def.default_model,
            "description": def.description,
            "disallowedTools": def.disallowed_tools,
            "enabled": def.enabled,
            "filePath": def.file_path,
            "name": def.name,
            "prompt": def.prompt,
            "subagents": def.subagents,
            "tools": def.tools,
            "whenToUse": def.when_to_use,
        }
    })
}

#[test]
fn cross_engine_fixture_corpus_matches_shared_expected_outputs() {
    let root = fixture_root();
    let corpus_dir = root.join("corpus");

    // Sorted fixture file names — the same enumeration the TS side does.
    let mut names: Vec<String> = fs::read_dir(&corpus_dir)
        .expect("corpus directory must exist (run from a full checkout)")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect();
    names.sort();

    let mut actual = Map::new();
    for name in &names {
        let text = fs::read_to_string(corpus_dir.join(name))
            .unwrap_or_else(|error| panic!("failed to read fixture {name}: {error}"));
        let path = format!("agents/{name}");
        let value = match parse_agent_file_text(&path, &text) {
            Ok(def) => normalize_def(&def),
            Err(error) => json!({ "ok": false, "error": error.reason }),
        };
        actual.insert(name.clone(), value);
    }

    let expected_text =
        fs::read_to_string(root.join("expected-outputs.json")).expect("expected-outputs.json");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");
    let expected = expected
        .as_object()
        .expect("expected-outputs.json must be a mapping of fixture → output");

    // Compare fixture-by-fixture so a mismatch names the offending file.
    let mut expected_names: Vec<&String> = expected.keys().collect();
    expected_names.sort();
    let actual_names: Vec<&String> = actual.keys().collect();
    assert_eq!(
        actual_names, expected_names,
        "corpus files covered by expected-outputs.json must match"
    );
    for (name, expected_value) in expected {
        let actual_value = actual
            .get(name)
            .expect("every expected fixture must have been parsed");
        assert_eq!(
            actual_value, expected_value,
            "fixture {name}: rust output differs from the shared expected-outputs.json \
             (regenerate from the TS side with UPDATE_CROSS_ENGINE_SNAPSHOT=1 if the \
             change is intentional — the rust side must then still pass unedited)"
        );
    }
}

/// Base-prompt template mirror guard: the rust `system_prompt.md` and the TS
/// `system.md` are documented mirrors (placeholder spelling aside, content
/// must not drift — both engines emit the template through the
/// `base_prompt_default_template` wire field, so drift is a cross-engine
/// inconsistency, not a cosmetic one). This companion to the TS-side guard
/// (`packages/agent-core-v2/test/lint/base-prompt-mirror.test.ts`) runs in
/// the rust CI suite, which the TS package's does not — keeping the guard
/// effective regardless of which side drifts.
#[test]
fn system_prompt_template_matches_ts_mirror() {
    let rust_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scope/system_prompt.md");
    let ts_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent-profile/system-mirror.md");

    let rust_text = fs::read_to_string(&rust_path).expect("read rust system_prompt.md");
    let ts_text = fs::read_to_string(&ts_path).expect("read TS system.md mirror");

    // Normalize the placeholder spelling — the one sanctioned difference
    // between the two files: rust `{{var}}` → TS `${var}`. Names are `\w+`.
    let rust_chars: Vec<char> = rust_text.chars().collect();
    let mut normalized = String::with_capacity(rust_text.len());
    let mut i = 0;
    while i < rust_chars.len() {
        if rust_chars[i] == '{'
            && rust_chars.get(i + 1) == Some(&'{')
            && rust_chars
                .get(i + 2)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            normalized.push('$');
            normalized.push('{');
            i += 2;
            while i < rust_chars.len()
                && (rust_chars[i].is_ascii_alphanumeric() || rust_chars[i] == '_')
            {
                normalized.push(rust_chars[i]);
                i += 1;
            }
            if rust_chars.get(i) == Some(&'}') && rust_chars.get(i + 1) == Some(&'}') {
                normalized.push('}');
                i += 2;
            }
        } else {
            normalized.push(rust_chars[i]);
            i += 1;
        }
    }

    assert_eq!(
        normalized, ts_text,
        "the base prompt template mirrors drifted — whichever file you edited, \
         mirror the change in the other (placeholder spelling: rust {{var}}, TS ${{var}})"
    );
}

}

mod core_types_ {
//! Tests for the core traits and types of `mirri-core`.
//!
//! These verify the load-bearing invariants that later Phase 2 modules rely on:
//! - `Message` / `ContextMessage` serialize to the TS wire format.
//! - `ToolAccess::conflicts` matches the TypeScript tool-access rules.
//! - `PromptOrigin::is_real_user_input` matches the compaction keep-set.
//! - `CancelToken` cancellation propagates to children.
//! - `StopReason::is_loop_continuing` is true only for `ToolUse`.

use mirri_core::{
    AccessKind, AccessOperation, CancelToken, ContextMessage, Message, PromptOrigin, Role,
    StopReason, ToolAccess,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Message serialization
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_role_as_lowercase_when_user_message_created() {
    let msg = Message::user("hello");
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hello");
    // tool_calls default empty -> skipped
    assert!(v.get("tool_calls").is_none() || v["tool_calls"].as_array().unwrap().is_empty());
    assert!(v.get("tool_call_id").is_none());
    assert!(v.get("partial").is_none());
}

#[test]
fn should_serialize_tool_calls_correctly_when_assistant_message_has_tool_calls() {
    let mut msg = Message::assistant("thinking...");
    msg.tool_calls.push(mirri_core::ToolCall {
        id: "call_abc".into(),
        call_type: "function".into(),
        function: mirri_core::ToolCallFunction {
            name: "Read".into(),
            arguments: r#"{"path":"/tmp/x"}"#.into(),
        },
    });
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "assistant");
    let tc = &v["tool_calls"][0];
    assert_eq!(tc["id"], "call_abc");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "Read");
    assert_eq!(tc["function"]["arguments"], r#"{"path":"/tmp/x"}"#);
}

#[test]
fn should_serialize_tool_call_id_when_tool_result_message_created() {
    let msg = Message::tool_result("call_abc", "file contents");
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "tool");
    assert_eq!(v["tool_call_id"], "call_abc");
    assert_eq!(v["content"][0]["text"], "file contents");
}

#[test]
fn should_preserve_fields_when_message_round_trips_through_json() {
    let msg = Message::assistant("hi");
    let json = serde_json::to_string(&msg).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role, Role::Assistant);
    assert_eq!(back.text_content(), "hi");
}

#[test]
fn should_join_text_parts_when_text_content_called() {
    let mut msg = Message::user("first");
    msg.content.push(mirri_core::ContentPart::Text {
        text: "second".into(),
    });
    msg.content.push(mirri_core::ContentPart::Think {
        text: "ignored".into(),
    });
    assert_eq!(msg.text_content(), "first\nsecond");
}

// ---------------------------------------------------------------------------
// ContextMessage serialization (flatten + origin)
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_origin_as_kind_tag_when_context_message_serialized() {
    let msg = Message::user("do something");
    let cm = ContextMessage::from_message(msg).with_origin(PromptOrigin::User);
    let v: serde_json::Value = serde_json::to_value(&cm).unwrap();

    // Flattened message fields are top-level siblings.
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"][0]["text"], "do something");
    // origin is a tagged enum: {"kind": "user"}
    assert_eq!(v["origin"], json!({"kind": "user"}));
    // is_error / note default -> skipped
    assert!(v.get("is_error").is_none());
    assert!(v.get("note").is_none());
}

#[test]
fn should_preserve_origin_when_context_message_round_trips_through_json() {
    let msg = Message::assistant("result");
    let cm = ContextMessage::from_message(msg).with_origin(PromptOrigin::CompactionSummary);
    let json = serde_json::to_string(&cm).unwrap();
    let back: ContextMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.message.role, Role::Assistant);
    assert_eq!(back.origin, Some(PromptOrigin::CompactionSummary));
    assert!(!back.is_error);
}

#[test]
fn should_omit_origin_field_when_context_message_has_no_origin() {
    let cm = ContextMessage::from_message(Message::user("hi"));
    let v: serde_json::Value = serde_json::to_value(&cm).unwrap();
    assert!(v.get("origin").is_none());
}

// ---------------------------------------------------------------------------
// ToolAccess::conflicts
// ---------------------------------------------------------------------------

fn file_read(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Read,
    }
}

fn file_write(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Write,
    }
}

#[test]
fn should_not_conflict_when_concurrent_reads_on_same_path() {
    let a = [file_read("/a/b")];
    let b = [file_read("/a/b")];
    assert!(!ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_when_write_and_read_on_same_path() {
    let a = [file_write("/a/b")];
    let b = [file_read("/a/b")];
    assert!(ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_when_writes_on_overlapping_paths() {
    let a = [file_write("/a/b")];
    let b = [file_write("/a/b/c")];
    assert!(ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_not_conflict_when_writes_on_disjoint_paths() {
    let a = [file_write("/a/b")];
    let b = [file_write("/x/y")];
    assert!(!ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_with_everything_when_access_kind_is_all() {
    let all_access = [ToolAccess {
        kind: AccessKind::All,
        path: None,
        operation: AccessOperation::ReadWrite,
    }];
    let file_read_access = [file_read("/anything")];
    assert!(ToolAccess::conflicts(&all_access, &file_read_access));
}

#[test]
fn should_not_conflict_when_access_kinds_are_file_and_search() {
    let file = [file_read("/a/b")];
    let search = [ToolAccess {
        kind: AccessKind::Search,
        path: Some("/a/b".into()),
        operation: AccessOperation::Read,
    }];
    assert!(!ToolAccess::conflicts(&file, &search));
}

// ---------------------------------------------------------------------------
// PromptOrigin::is_real_user_input
// ---------------------------------------------------------------------------

#[test]
fn should_return_true_when_prompt_origin_is_real_user_input() {
    assert!(PromptOrigin::User.is_real_user_input());
    assert!(
        PromptOrigin::SkillActivation {
            activation_id: None,
            skill_name: None,
            trigger: None,
            skill_args: None,
            skill_path: None,
            skill_source: None,
        }
        .is_real_user_input()
    );
    assert!(PromptOrigin::PluginCommand.is_real_user_input());
}

#[test]
fn should_return_false_when_prompt_origin_is_not_real_user_input() {
    assert!(!PromptOrigin::Injection.is_real_user_input());
    assert!(!PromptOrigin::CompactionSummary.is_real_user_input());
    assert!(!PromptOrigin::SystemTrigger.is_real_user_input());
    assert!(
        !PromptOrigin::BackgroundTask {
            task_id: "t".to_string(),
            status: "completed".to_string(),
            notification_id: "task:t:completed".to_string(),
        }
        .is_real_user_input()
    );
    assert!(!PromptOrigin::ShellCommand.is_real_user_input());
}

#[test]
fn should_serialize_as_kind_tag_when_prompt_origin_serialized() {
    let v = serde_json::to_value(PromptOrigin::User).unwrap();
    assert_eq!(v, json!({"kind": "user"}));
    let v = serde_json::to_value(PromptOrigin::CompactionSummary).unwrap();
    assert_eq!(v, json!({"kind": "compaction_summary"}));
}

// ---------------------------------------------------------------------------
// CancelToken
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_propagate_cancellation_to_children_when_parent_cancelled() {
    let parent = CancelToken::new();
    let child = parent.child();
    let grandchild = child.child();

    assert!(!parent.is_cancelled());
    assert!(!child.is_cancelled());
    assert!(!grandchild.is_cancelled());

    parent.cancel();

    assert!(parent.is_cancelled());
    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
}

#[tokio::test]
async fn should_not_propagate_cancellation_to_parent_when_child_cancelled() {
    let parent = CancelToken::new();
    let child = parent.child();

    child.cancel();

    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
}

#[test]
fn should_return_aborted_error_when_check_called_after_cancel() {
    let token = CancelToken::new();
    assert!(token.check().is_ok());
    token.cancel();
    let err = token.check().unwrap_err();
    assert!(err.is_abort());
}

#[tokio::test]
async fn should_complete_immediately_when_cancelled_called_after_token_cancelled() {
    let token = CancelToken::new();
    let child = token.child();
    token.cancel();
    // Should return immediately since already cancelled.
    child.cancelled().await;
}

// ---------------------------------------------------------------------------
// StopReason
// ---------------------------------------------------------------------------

#[test]
fn should_return_true_only_for_tool_use_when_is_loop_continuing_called() {
    assert!(StopReason::ToolUse.is_loop_continuing());
    assert!(!StopReason::EndTurn.is_loop_continuing());
    assert!(!StopReason::MaxTokens.is_loop_continuing());
    assert!(!StopReason::Filtered.is_loop_continuing());
    assert!(!StopReason::Paused.is_loop_continuing());
    assert!(!StopReason::Unknown.is_loop_continuing());
}

// ---------------------------------------------------------------------------
// EngineError / ErrorCode
// ---------------------------------------------------------------------------

#[test]
fn should_map_to_correct_error_code_when_engine_error_converted() {
    use mirri_core::{EngineError, ErrorCode};

    assert_eq!(
        EngineError::MaxStepsExceeded("x".into()).code(),
        ErrorCode::LoopMaxStepsExceeded
    );
    assert_eq!(
        EngineError::ContextOverflow { attempts: 3 }.code(),
        ErrorCode::ContextOverflow
    );
    assert_eq!(
        EngineError::UserCancelled.code(),
        ErrorCode::AbortUserCancelled
    );
    assert_eq!(
        EngineError::Aborted("boom".into()).code(),
        ErrorCode::AbortCancelled
    );
    // Internal errors must map to their own code, never AbortCancelled.
    assert_eq!(
        EngineError::Internal("boom".into()).code(),
        ErrorCode::Internal
    );
}

#[test]
fn should_serialize_error_code_to_stable_wire_string_when_converted() {
    use mirri_core::ErrorCode;

    assert_eq!(ErrorCode::Internal.as_str(), "internal");
    assert_eq!(ErrorCode::ProviderApiError.as_str(), "provider.api_error");
    assert_eq!(ErrorCode::AbortCancelled.as_str(), "abort.cancelled");
    assert_eq!(
        ErrorCode::ToolExecutionFailed.as_str(),
        "tool.execution_failed"
    );
    // Dot-notation for session/agent domain codes.
    assert_eq!(ErrorCode::SessionNotFound.as_str(), "session.not_found");
    assert_eq!(ErrorCode::AgentNotFound.as_str(), "agent.not_found");
    assert_eq!(
        ErrorCode::LoopMaxStepsExceeded.as_str(),
        "loop.max_steps_exceeded"
    );
    assert_eq!(ErrorCode::ContextOverflow.as_str(), "context.overflow");
}

#[test]
fn should_classify_abort_correctly_when_engine_error_checked() {
    use mirri_core::EngineError;

    assert!(EngineError::UserCancelled.is_abort());
    assert!(EngineError::Aborted("x".into()).is_abort());
    assert!(!EngineError::MaxStepsExceeded("x".into()).is_abort());

    assert!(EngineError::UserCancelled.is_user_cancellation());
    assert!(!EngineError::Aborted("x".into()).is_user_cancellation());
}

#[test]
fn should_preserve_source_chain_when_io_error_converted_to_engine_error() {
    use std::error::Error;

    use mirri_core::{EngineError, ErrorCode};

    // Given an io::Error propagated via the #[from] conversion
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such session file");
    let engine_err = EngineError::from(io_err);

    // Then the original error is retained as the source, and the error code
    // keeps the ToolExecutionFailed mapping of the old manual From impl.
    assert!(matches!(engine_err, EngineError::Io(_)));
    assert_eq!(engine_err.code(), ErrorCode::ToolExecutionFailed);
    let source = engine_err.source().expect("io::Error preserved as source");
    assert!(source.to_string().contains("no such session file"));
}

// ---------------------------------------------------------------------------
// ProjectOptions presets
// ---------------------------------------------------------------------------

#[test]
fn should_only_drop_orphans_when_normal_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::normal();
    assert!(opts.drop_orphan_results);
    assert!(!opts.synthesize_missing);
    assert!(!opts.drop_leading_non_user);
    assert!(!opts.merge_consecutive_assistants);
    assert!(!opts.dedupe_duplicate_tool_calls);
}

#[test]
fn should_enable_all_repairs_when_strict_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::strict();
    assert!(opts.drop_orphan_results);
    assert!(opts.drop_leading_non_user);
    assert!(opts.merge_consecutive_assistants);
    assert!(opts.dedupe_duplicate_tool_calls);
    assert!(!opts.synthesize_missing);
}

#[test]
fn should_synthesize_missing_when_compaction_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::compaction();
    assert!(opts.synthesize_missing);
    assert!(opts.drop_orphan_results);
}

}

mod error_serialization_ {
//! Error serialization tests (G-18/G-20 restore).
//!
//! `EngineError` and `SessionError` do not derive `Serialize`/`Deserialize`
//! directly — their wire representation is produced by the RPC layer
//! (`mirri_server::rpc::ApiErrorResponse`, built from the JSON-RPC code, the
//! `Display` message, and the stable `ErrorCode::as_str()` domain code).
//!
//! `mirri-core` integration tests cannot depend on `mirri-server` (the server
//! depends on the core), so these tests mirror the envelope construction
//! exactly as `rpc.rs` does for `RpcError::Engine` / `RpcError::Session` and
//! verify that every error variant survives a JSON round-trip with its code,
//! message, and domain code intact.

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};

use mirri_core::{EngineError, ErrorCode, SessionError};
use serde_json::{Value, json};

/// Local mirror of `mirri_server::rpc::ApiErrorResponse` — the wire envelope
/// clients receive. Built the same way `ApiErrorResponse::error` builds it for
/// engine/session errors.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct WireErrorEnvelope {
    code: i32,
    msg: String,
    /// Domain error code (e.g. `"provider.api_error"`, `"session.not_found"`);
    /// serialized only when present, like the real envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    domain_code: Option<String>,
    data: Value,
    request_id: String,
}

impl WireErrorEnvelope {
    /// Mirrors `ApiErrorResponse::error(&RpcError::Engine(e), request_id)`.
    fn from_engine(error: &EngineError, request_id: &str) -> Self {
        Self {
            code: -32603, // RpcError::Engine json_rpc_code
            msg: error.to_string(),
            domain_code: Some(error.code().as_str().to_string()),
            data: Value::Null,
            request_id: request_id.to_string(),
        }
    }

    /// Mirrors `ApiErrorResponse::error(&RpcError::Session(e), request_id)`.
    fn from_session(error: &SessionError, request_id: &str) -> Self {
        let code = match error {
            // Session "not found" variants preserve the -32001 NotFound code.
            SessionError::NotFound(_) | SessionError::AgentNotFound(_) => -32001,
            _ => -32603,
        };
        Self {
            code,
            msg: error.to_string(),
            domain_code: Some(error.code().as_str().to_string()),
            data: Value::Null,
            request_id: request_id.to_string(),
        }
    }
}

/// Every `EngineError` variant, so the round-trip test covers all of them.
fn all_engine_error_variants() -> Vec<EngineError> {
    vec![
        EngineError::MaxStepsExceeded("loop hit 10 steps".to_string()),
        EngineError::ContextOverflow { attempts: 3 },
        EngineError::CompactionFailed("token budget exceeded".to_string()),
        EngineError::CompactionUnable("history not compactable".to_string()),
        EngineError::RequestInvalid("missing session_id".to_string()),
        EngineError::PermissionDenied("read blocked by policy".to_string()),
        EngineError::ToolExecutionFailed("bash exited with status 2".to_string()),
        EngineError::ProviderError("upstream 503".to_string()),
        EngineError::UserCancelled,
        EngineError::Aborted("user pressed stop".to_string()),
        EngineError::Internal("worker panicked".to_string()),
        EngineError::HookBlocked("hook denied the step".to_string()),
        EngineError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "profile.toml missing",
        )),
        EngineError::Json(serde_json::from_str::<Value>("{").unwrap_err()),
    ]
}

/// Every `SessionError` variant, so the round-trip test covers all of them.
fn all_session_error_variants() -> Vec<SessionError> {
    vec![
        SessionError::NotFound("sess-1".to_string()),
        SessionError::AlreadyExists("sess-1".to_string()),
        SessionError::AgentNotFound("agent-1".to_string()),
        SessionError::AgentAlreadyExists("agent-1".to_string()),
        SessionError::WireLog("append failed".to_string()),
        SessionError::StateFile("corrupt state.json".to_string()),
        SessionError::Snapshot("snapshot write failed".to_string()),
        SessionError::ResumeFailed("wire log replay failed".to_string()),
        SessionError::AlreadyClosed("sess-1".to_string()),
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )),
        SessionError::Json(serde_json::from_str::<Value>("]").unwrap_err()),
    ]
}

/// should serialize every EngineError variant through the RPC envelope and
/// deserialize it back with code, message, and domain code intact
#[test]
fn should_roundtrip_all_engine_error_variants_through_serde() {
    for error in all_engine_error_variants() {
        let envelope = WireErrorEnvelope::from_engine(&error, "req-1");

        let json = serde_json::to_string(&envelope).expect("envelope should serialize");
        let decoded: WireErrorEnvelope =
            serde_json::from_str(&json).expect("envelope should deserialize");

        assert_eq!(decoded, envelope, "round-trip should preserve the envelope");
        assert_eq!(
            decoded.domain_code.as_deref(),
            Some(error.code().as_str()),
            "domain code should match the variant's ErrorCode"
        );
        assert!(!decoded.msg.is_empty(), "message should never be empty");
    }
}

/// should serialize every SessionError variant through the RPC envelope and
/// deserialize it back with code, message, and domain code intact
#[test]
fn should_roundtrip_all_session_error_variants_through_serde() {
    for error in all_session_error_variants() {
        let envelope = WireErrorEnvelope::from_session(&error, "req-1");

        let json = serde_json::to_string(&envelope).expect("envelope should serialize");
        let decoded: WireErrorEnvelope =
            serde_json::from_str(&json).expect("envelope should deserialize");

        assert_eq!(decoded, envelope, "round-trip should preserve the envelope");
        assert_eq!(
            decoded.domain_code.as_deref(),
            Some(error.code().as_str()),
            "domain code should match the variant's ErrorCode"
        );
        assert!(!decoded.msg.is_empty(), "message should never be empty");
    }
}

/// should preserve the source chain of wrapped errors when serializing
#[test]
fn should_preserve_error_chain_when_serializing() {
    // EngineError::Io carries its source via #[from]; source() must expose
    // the underlying io::Error so operators can see the full chain.
    let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "profile.toml missing");
    let engine = EngineError::Io(io_error);

    assert!(
        engine.source().is_some(),
        "Io variant should expose its source chain"
    );
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = engine.source();
    while let Some(source) = cursor {
        chain.push(source.to_string());
        cursor = source.source();
    }
    assert!(
        chain
            .iter()
            .any(|link| link.contains("profile.toml missing")),
        "chain should include the io error: {chain:?}"
    );

    // The envelope keeps the outer context visible in the wire message while
    // the chain stays intact on the error object.
    let envelope = WireErrorEnvelope::from_engine(&engine, "req-1");
    assert!(envelope.msg.contains("I/O error"));
    assert_eq!(
        envelope.domain_code.as_deref(),
        Some("tool.execution_failed")
    );
}

/// should wrap unexpected panics with context into an EngineError::Internal
#[test]
fn should_wrap_unexpected_errors_with_context() {
    // Simulate catching a panicking worker (e.g. a hook or tool thread) and
    // converting the unwind payload into an EngineError carrying context.
    let result = catch_unwind(AssertUnwindSafe(|| {
        panic!("boom in worker");
    }));

    let payload = match result {
        Ok(()) => panic!("expected the closure to panic"),
        Err(payload) => payload,
    };
    let message = payload.downcast_ref::<&str>().copied().unwrap_or("unknown");
    let error = EngineError::Internal(format!("worker panicked: {message}"));

    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.code().as_str(), "internal");
    assert!(
        error
            .to_string()
            .contains("worker panicked: boom in worker"),
        "context should be preserved: {error}"
    );
}

/// should include the JSON-RPC code, message, and domain code fields in the
/// serialized JSON envelope
#[test]
fn should_include_error_code_and_message_in_json() {
    let error = EngineError::ProviderError("upstream 503".to_string());
    let envelope = WireErrorEnvelope::from_engine(&error, "req-abc");

    let value = serde_json::to_value(&envelope).expect("envelope should serialize");
    assert_eq!(value["code"], json!(-32603));
    assert_eq!(value["msg"], json!("Provider error: upstream 503"));
    assert_eq!(value["domain_code"], json!("provider.api_error"));
    assert_eq!(value["request_id"], json!("req-abc"));
    assert_eq!(value["data"], Value::Null);

    // A non-found session keeps its -32001 code, like the real RPC layer.
    let session = SessionError::NotFound("sess-404".to_string());
    let session_value =
        serde_json::to_value(WireErrorEnvelope::from_session(&session, "req-abc")).unwrap();
    assert_eq!(session_value["code"], json!(-32001));
    assert_eq!(session_value["domain_code"], json!("session.not_found"));
}

/// should preserve structured details (e.g. ContextOverflow.attempts) through
/// serialization
#[test]
fn should_preserve_error_details_when_serializing() {
    let error = EngineError::ContextOverflow { attempts: 7 };

    // The Display message embeds the structured detail...
    let msg = error.to_string();
    assert!(msg.contains("7"), "attempts should be visible: {msg}");

    // ...and the detail survives the wire round-trip.
    let envelope = WireErrorEnvelope::from_engine(&error, "req-1");
    let json = serde_json::to_string(&envelope).unwrap();
    let decoded: WireErrorEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.msg, "Context overflow after 7 compaction attempts");
    assert_eq!(decoded.domain_code.as_deref(), Some("context.overflow"));
    assert_eq!(decoded.code, -32603);
}

}

mod log_and_rpc_isolation_ {
//! G-30: log redaction, tool exchange fallback recovery, and RPC method
//! isolation tests.
//!
//! - **Part 1** exercises the public `mirri_core::redact` API: structured
//!   JSON redaction and raw-string scrubbing for secrets that must never
//!   reach logs or CLI output.
//! - **Part 2** exercises `ContextMemory` tool-exchange semantics: an open
//!   tool exchange defers appends, a failed exchange must close and flush,
//!   and when normal invocation is unavailable the engine falls back to
//!   recording unexecuted calls directly with synthetic error results.
//! - **Part 3** covers `session.create` ("create-rpc") / `plugin.*`
//!   ("plugins-rpc") method isolation. Those handlers live in the host
//!   layer (`mirri-embedded` / `mirri-server`), not in `mirri-core`, so
//!   those tests are `#[ignore]`d placeholders with TODOs (see below).

use mirri_core::loop_engine::tool_call_batch::record_unexecuted_tool_calls;
use mirri_core::redact::{redact_json, redact_string};
use mirri_core::tool::ToolOutput;
use mirri_core::{ContentPart, ContextMemory, LlmToolCall, LoopEvent, PromptOrigin, Role};
use serde_json::json;

/// Build a single text content part, the common helper for user messages.
fn user_text(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text {
        text: text.to_string(),
    }]
}

/// Extract the tool call id from a `ToolCall` / `ToolResult` event.
fn event_tool_call_id(event: &LoopEvent) -> &str {
    match event {
        LoopEvent::ToolCall { tool_call_id, .. } | LoopEvent::ToolResult { tool_call_id, .. } => {
            tool_call_id.as_str()
        }
        _ => panic!("expected a tool call/result event, got: {event:?}"),
    }
}

// ---------------------------------------------------------------------------
// Part 1: log redaction
// ---------------------------------------------------------------------------

#[test]
fn should_redact_sensitive_data_in_log_output() {
    let input = json!({
        "provider": "openai",
        "model": "gpt-4o",
        "apiKey": "sk-proj-live-secret",
        "accessToken": "at_1234",
        "refreshToken": "rt_5678",
        "password": "hunter2",
        "clientSecret": "cs_9012",
    });

    let output = redact_json(&input);

    // Values under sensitive keys are replaced in place with the marker.
    assert_eq!(output["apiKey"], "[REDACTED]");
    assert_eq!(output["accessToken"], "[REDACTED]");
    assert_eq!(output["refreshToken"], "[REDACTED]");
    assert_eq!(output["password"], "[REDACTED]");
    assert_eq!(output["clientSecret"], "[REDACTED]");

    // Non-sensitive keys are untouched.
    assert_eq!(output["provider"], "openai");
    assert_eq!(output["model"], "gpt-4o");

    // The serialized log line must not leak any raw secret anywhere.
    let serialized = output.to_string();
    for secret in [
        "sk-proj-live-secret",
        "at_1234",
        "rt_5678",
        "hunter2",
        "cs_9012",
    ] {
        assert!(
            !serialized.contains(secret),
            "secret leaked into log output: {secret}"
        );
    }
}

#[test]
fn should_preserve_non_sensitive_data_in_log_output() {
    let input = json!({
        "provider": "anthropic",
        "model": "claude-sonnet-4",
        "requestId": "req-123",
        "maxTokens": 8192,
        "temperature": 0.7,
        "message": "the access token will rotate soon",
        "url": "https://api.example.com/v1/messages",
        "headers": { "content-type": "application/json" },
        "flags": [true, false],
        "nested": { "enabled": true, "count": 3 },
    });

    let output = redact_json(&input);

    // Exact-match key normalization: "maxTokens" must NOT be redacted even
    // though it contains the substring "token".
    assert_eq!(output["provider"], "anthropic");
    assert_eq!(output["model"], "claude-sonnet-4");
    assert_eq!(output["requestId"], "req-123");
    assert_eq!(output["maxTokens"], 8192);
    assert_eq!(output["temperature"], 0.7);
    assert_eq!(output["message"], "the access token will rotate soon");
    assert_eq!(output["url"], "https://api.example.com/v1/messages");
    assert_eq!(
        output["headers"],
        json!({ "content-type": "application/json" })
    );
    assert_eq!(output["flags"], json!([true, false]));
    assert_eq!(output["nested"], json!({ "enabled": true, "count": 3 }));
}

#[test]
fn should_redact_api_key_in_url_query_params() {
    // A URL with an `api_key` query parameter leaked into a raw log line.
    let url = "https://api.example.com/v1/models?api_key=sk-abcdefGHIJKLMNOP1234567&limit=5";
    let redacted = redact_string(url);
    assert!(
        redacted.contains("[REDACTED]"),
        "api_key value must be redacted"
    );
    assert!(
        !redacted.contains("sk-abcdefGHIJKLMNOP1234567"),
        "secret must not survive in the log line: {redacted}"
    );

    // Structured log output: secrets embedded in string values (including
    // URLs) are scrubbed too.
    let output = redact_json(&json!({
        "url": url,
        "level": "info",
    }));
    let url_out = output["url"].to_string();
    assert!(
        url_out.contains("[REDACTED]"),
        "url query param must be redacted: {url_out}"
    );
    assert!(!url_out.contains("sk-abcdefGHIJKLMNOP1234567"));
    assert_eq!(output["level"], "info");
}

#[test]
fn should_redact_authorization_header() {
    // Structured log output: an authorization header value is replaced in
    // place, while sibling headers survive.
    let output = redact_json(&json!({
        "headers": {
            "authorization": "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.signature",
            "content-type": "application/json",
        },
        "requestId": "req-123",
    }));
    assert_eq!(output["headers"]["authorization"], "[REDACTED]");
    assert_eq!(output["headers"]["content-type"], "application/json");
    assert_eq!(output["requestId"], "req-123");

    // Free-form log lines: `authorization: bearer <token>` is scrubbed.
    let line = "POST /v1/messages authorization: bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.sig status=200";
    let redacted = redact_string(line);
    assert!(
        redacted.contains("[REDACTED]"),
        "authorization header must be redacted: {redacted}"
    );
    assert!(
        !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.sig"),
        "bearer token must not survive in the log line: {redacted}"
    );
}

// ---------------------------------------------------------------------------
// Part 2: tool exchange fallback
// ---------------------------------------------------------------------------

#[test]
fn should_recover_when_tool_exchange_fails() {
    let mut mem = ContextMemory::new();

    // The assistant opens a tool exchange by emitting a tool call.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: json!({ "command": "ls" }),
    });
    assert!(mem.has_open_tool_exchange());

    // While the exchange is open, user appends are deferred, not stored.
    mem.append_user_message(user_text("please continue"), PromptOrigin::User);
    assert_eq!(
        mem.history().len(),
        1,
        "only the assistant step is stored while the exchange is open"
    );

    // The tool fails: its result arrives flagged as an error.
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call-1".to_string(),
        output: ToolOutput::text("command failed: exit code 1"),
        is_error: true,
        note: None,
        entry_seq: None,
    });

    // Recovery: the exchange is closed, the error result is recorded, and
    // the deferred user message is flushed into history.
    assert!(
        !mem.has_open_tool_exchange(),
        "a failed exchange must not stay open"
    );
    let history = mem.history();
    assert_eq!(
        history.len(),
        3,
        "assistant step + failing tool result + flushed user message"
    );
    assert_eq!(history[1].message.role, Role::Tool);
    assert!(
        history[1].is_error,
        "the tool failure must be recorded as an error"
    );
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(history[2].message.role, Role::User);
    assert_eq!(history[2].message.content, user_text("please continue"));

    // The memory is fully usable again: new appends go straight to history.
    mem.append_user_message(user_text("after recovery"), PromptOrigin::User);
    assert_eq!(mem.history().len(), 4);
    assert!(!mem.has_open_tool_exchange());

    // Fallback variant: the result never arrives (abandoned exchange). The
    // engine falls back to synthesizing an error result for the abandoned
    // call so the memory does not stay stuck buffering.
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-2".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-2".to_string(),
        tool_call_id: "call-2".to_string(),
        tool_name: "Read".to_string(),
        args: json!({ "path": "src/main.rs" }),
    });
    mem.append_user_message(
        user_text("deferred while exchange abandoned"),
        PromptOrigin::User,
    );
    assert!(mem.has_open_tool_exchange());

    mem.close_pending_tool_results();

    assert!(
        !mem.has_open_tool_exchange(),
        "an interrupted exchange must not stay open"
    );
    let history = mem.history();
    assert_eq!(
        history.len(),
        7,
        "scenario 1 (4) + step-2 assistant + synthetic error + flushed user"
    );
    assert_eq!(history[5].message.role, Role::Tool);
    assert!(
        history[5].is_error,
        "the synthesized result must be marked as an error"
    );
    assert_eq!(history[5].message.tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(history[6].message.role, Role::User);
    assert_eq!(
        history[6].message.content,
        user_text("deferred while exchange abandoned")
    );
}

#[test]
fn should_fall_back_to_direct_invocation_when_exchange_unavailable() {
    let mut mem = ContextMemory::new();
    mem.append_user_message(user_text("run the task"), PromptOrigin::User);
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });

    // The model stopped before tool execution could start, so normal
    // invocation is unavailable. The engine falls back to recording each
    // unexecuted call directly, paired with a synthetic error result, in
    // provider order.
    let calls = vec![
        LlmToolCall {
            id: "call-1".to_string(),
            name: "Bash".to_string(),
            args: json!({ "command": "ls" }),
        },
        LlmToolCall {
            id: "call-2".to_string(),
            name: "Read".to_string(),
            args: json!({ "path": "x" }),
        },
    ];
    let recorded = record_unexecuted_tool_calls(&calls, "step-1", &mut mem);

    // Events are persisted in call order: call, error result, call, error
    // result — each call is opened and immediately closed with an error.
    assert_eq!(recorded.len(), 4, "two calls + two synthetic error results");
    assert!(matches!(&recorded[0], LoopEvent::ToolCall { .. }));
    assert!(matches!(
        &recorded[1],
        LoopEvent::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(&recorded[2], LoopEvent::ToolCall { .. }));
    assert!(matches!(
        &recorded[3],
        LoopEvent::ToolResult { is_error: true, .. }
    ));
    assert_eq!(event_tool_call_id(&recorded[0]), "call-1");
    assert_eq!(event_tool_call_id(&recorded[1]), "call-1");
    assert_eq!(event_tool_call_id(&recorded[2]), "call-2");
    assert_eq!(event_tool_call_id(&recorded[3]), "call-2");

    // Each call was closed with its synthetic error result, so no exchange
    // is left open — the memory never gets stuck buffering.
    assert!(!mem.has_open_tool_exchange());

    // Both synthetic failures are recorded in history (with the synthetic
    // tool calls attached to the assistant step).
    let history = mem.history();
    assert_eq!(
        history.len(),
        4,
        "user + assistant (with 2 tool calls) + 2 error results"
    );
    assert_eq!(history[1].message.role, Role::Assistant);
    assert_eq!(history[1].message.tool_calls.len(), 2);
    assert_eq!(history[2].message.role, Role::Tool);
    assert!(history[2].is_error);
    assert_eq!(history[2].message.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(history[3].message.role, Role::Tool);
    assert!(history[3].is_error);
    assert_eq!(history[3].message.tool_call_id.as_deref(), Some("call-2"));

    // Because no exchange is open, subsequent appends are not deferred.
    mem.append_user_message(user_text("after fallback"), PromptOrigin::User);
    assert_eq!(mem.history().len(), 5);
    assert!(!mem.has_open_tool_exchange());
}

// ---------------------------------------------------------------------------
// Part 3: RPC method isolation
//
// The `session.create` ("create-rpc") and `plugin.*`/`plugins.*`
// ("plugins-rpc") methods are dispatched by the HOST layer, not by
// `mirri-core`:
//
// - `mirri-embedded`: `embedded_server/mod.rs` — `dispatch_create_session`
//   (`session.create` / `createSession`) and the `plugin.list` /
//   `plugin.install` / `plugin.reload` handlers.
// - `mirri-server`: `ipc.rs` / `ws_rpc.rs` — `session.create` JSON-RPC.
//
// `mirri-core` only knows the reverse direction
// (`reverse_rpc::ReverseRpcCallback`, engine → client), so there is no
// in-core dispatch table to test here. The isolation property these tests
// will assert once a host-level harness exists:
//
//   1. A `session.create` call resolves independently: its params/return
//      value never bleed into a plugins-rpc call's params/return, and vice
//      versa.
//   2. A failing `session.create` (invalid params, engine error) does not
//      poison subsequent `plugin.*` calls, and a failing `plugin.*` call
//      does not poison subsequent `session.create` calls — each method is
//      dispatched through its own handler with per-call state.
//
// TODO(G-30): move these tests into the crate that owns the dispatch
// (mirri-embedded / mirri-server integration tests) when the harness lands.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "create-rpc dispatch lives in mirri-embedded/mirri-server, not mirri-core (G-30 TODO)"]
fn should_handle_create_rpc_independently() {
    // TODO(G-30, Part 3): exercise `session.create` through the host RPC
    // entry point found in `mirri-embedded/src/embedded_server/mod.rs`
    // (`dispatch_create_session`) and `mirri-server/src/ipc.rs`. Assert:
    //   1. `session.create` with valid params provisions a session and
    //      returns it without mutating any plugin state.
    //   2. An invalid `session.create` (missing/empty `title`) returns an
    //      error frame, and a follow-up valid create still succeeds.
    //   3. Interleaved `session.create` and `plugin.*` calls resolve with
    //      their own params/return values (no cross-talk).
    panic!("G-30 placeholder: create-rpc isolation test belongs in mirri-embedded/mirri-server");
}

#[test]
#[ignore = "plugins-rpc dispatch lives in mirri-embedded/mirri-server, not mirri-core (G-30 TODO)"]
fn should_handle_plugins_rpc_independently() {
    // TODO(G-30, Part 3): exercise `plugin.list` / `plugin.install` /
    // `plugin.reload` through the host RPC entry point found in
    // `mirri-embedded/src/embedded_server/mod.rs` (and any server-side
    // plugin dispatch in `mirri-server`). Assert:
    //   1. A failing plugins-rpc call (e.g. `plugin.install` with a missing
    //      `source`) returns an error frame but leaves `session.create`
    //      fully functional.
    //   2. Listing/installing plugins does not affect session creation
    //      state and vice versa (each handler owns its state).
    //   3. Concurrent or interleaved create + plugin calls never observe
    //      each other's params or return values.
    panic!("G-30 placeholder: plugins-rpc isolation test belongs in mirri-embedded/mirri-server");
}

}

