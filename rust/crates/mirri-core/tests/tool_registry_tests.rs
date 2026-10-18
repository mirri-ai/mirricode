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
        wait_registry: None,
        turn_id: "test-turn".to_string(),
        tool_call_id: tool_call_id.to_string(),
        cancel: CancelToken::new(),
        progress: Default::default(),
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
