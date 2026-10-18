//! `SelectToolsTool` — progressive tool disclosure (the `select_tools` tool).
//!
//! Mirrors the TypeScript `SelectToolsTool`: lets the model load deferred
//! tools by exact name. Delegates loading to [`ToolSelectService`] (the
//! `toolSelect` domain). Tools that were already loaded or are unknown are
//! reported separately so the model can correct its request.
//!
//! `ToolSelectService` methods like `load_tool` require `&mut self`, so the
//! service is shared behind an `Arc<Mutex<ToolSelectService>>` — the only
//! way to provide interior mutability without modifying the domain module.

use std::sync::{Arc, Mutex};

use crate::error::EngineResult;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};
use crate::tool_select::ToolSelectService;

/// `select_tools` — load one or more deferred tools by name.
///
/// The tool holds `Arc<Mutex<ToolSelectService>>` because the backing
/// service's `load_tool` method requires `&mut self`. The `Mutex` is held
/// only for the duration of the load operation, never across an `await`.
pub struct SelectToolsTool {
    /// The session-scoped tool-select service backing this tool.
    tool_select: Arc<Mutex<ToolSelectService>>,
}

impl SelectToolsTool {
    /// Create the tool bound to the session-scoped tool-select service.
    pub fn new(tool_select: Arc<Mutex<ToolSelectService>>) -> Self {
        Self { tool_select }
    }
}

impl Tool for SelectToolsTool {
    fn name(&self) -> &str {
        "select_tools"
    }

    fn description(&self) -> &str {
        "Load one or more tools by name so you can call them. All available \
         tool names are listed in the <tools_added>/<tools_removed> \
         announcements in the system context — fold them in order to get the \
         current list. Pass the exact name(s) you need; their full \
         definitions become available immediately, so you can call them \
         directly in your next tool call."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Exact tool names to load, taken from the latest announced tool list."
                    }
                },
                "required": ["names"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(names_arr) = input.get("names").and_then(serde_json::Value::as_array) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "select_tools requires a 'names' array argument".to_string(),
            }));
        };

        if names_arr.is_empty() {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "select_tools 'names' must contain at least one tool name".to_string(),
            }));
        }

        let mut names = Vec::with_capacity(names_arr.len());
        for raw in names_arr {
            let Some(name) = raw.as_str() else {
                return Ok(ToolExecution::Error(ToolErrorResult {
                    message: format!("select_tools: every name must be a string, got {raw}"),
                }));
            };
            names.push(name.to_string());
        }

        let tool_select = Arc::clone(&self.tool_select);
        let display_summary = format!("Loading tools: {}", names.join(", "));
        let execution_plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: display_summary,
            },
            approval_rule: "select_tools".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan: execution_plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    let mut svc = tool_select
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());

                    // If progressive disclosure is not enabled at all, the
                    // tool has nothing to load. Return a clear, actionable
                    // message rather than a generic error.
                    if !svc.is_disclosure_enabled() {
                        return ToolResult::error(
                            "select_tools is not configured for this agent. \
                             Progressive tool disclosure is disabled (all tools \
                             are already available). There is nothing to load.",
                        );
                    }

                    let mut loaded = Vec::new();
                    let mut already_available = Vec::new();
                    let mut unknown = Vec::new();

                    for name in &names {
                        if svc.is_deferred(name) {
                            // `load_tool` moves the tool from the deferred set
                            // to the loaded set.
                            svc.load_tool(name);
                            loaded.push(name.clone());
                        } else if svc.is_loaded(name) {
                            already_available.push(name.clone());
                        } else {
                            unknown.push(name.clone());
                        }
                    }

                    let mut lines = Vec::new();
                    if !loaded.is_empty() {
                        lines.push(format!("Loaded: {}", loaded.join(", ")));
                    }
                    if !already_available.is_empty() {
                        lines.push(format!(
                            "Already available: {}",
                            already_available.join(", ")
                        ));
                    }
                    for name in &unknown {
                        lines.push(format!(
                            "Unknown tool: {name}. Pick from the latest announced tools list."
                        ));
                    }

                    let is_error = loaded.is_empty() && already_available.is_empty();
                    if is_error {
                        ToolResult::error(lines.join("\n"))
                    } else {
                        ToolResult::success(lines.join("\n"))
                    }
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;
    use crate::tool_select::DisclosureState;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &SelectToolsTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    fn make_service() -> Arc<Mutex<ToolSelectService>> {
        let mut svc = ToolSelectService::new();
        svc.set_disclosure(DisclosureState::Deferred);
        svc.register_deferred("expensive_search".to_string());
        svc.register_deferred("heavy_analyzer".to_string());
        Arc::new(Mutex::new(svc))
    }

    #[tokio::test]
    async fn should_load_deferred_tools_when_names_match() {
        let svc = make_service();
        let tool = SelectToolsTool::new(Arc::clone(&svc));

        let result =
            execute_tool(&tool, serde_json::json!({ "names": ["expensive_search"] })).await;

        assert!(!result.is_error, "result: {:?}", result);
        let message = output_text(&result);
        assert!(message.contains("Loaded: expensive_search"));
        // The service now marks it as loaded.
        let guard = svc.lock().unwrap();
        assert!(guard.is_loaded("expensive_search"));
        assert!(!guard.is_deferred("expensive_search"));
    }

    #[tokio::test]
    async fn should_report_already_available_when_tool_was_previously_loaded() {
        let svc = make_service();
        {
            let mut guard = svc.lock().unwrap();
            guard.load_tool("heavy_analyzer");
        }
        let tool = SelectToolsTool::new(Arc::clone(&svc));

        let result = execute_tool(&tool, serde_json::json!({ "names": ["heavy_analyzer"] })).await;

        assert!(!result.is_error);
        let message = output_text(&result);
        assert!(message.contains("Already available: heavy_analyzer"));
    }

    #[tokio::test]
    async fn should_report_unknown_when_tool_is_not_deferred_and_not_loaded() {
        let svc = make_service();
        let tool = SelectToolsTool::new(svc);

        let result = execute_tool(&tool, serde_json::json!({ "names": ["nonexistent"] })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(message.contains("Unknown tool: nonexistent"));
    }

    #[tokio::test]
    async fn should_handle_mixed_names_when_a_single_call_contains_loaded_available_and_unknown_tools()
     {
        let svc = make_service();
        {
            let mut guard = svc.lock().unwrap();
            guard.load_tool("heavy_analyzer");
        }
        let tool = SelectToolsTool::new(svc);

        let result = execute_tool(
            &tool,
            serde_json::json!({
                "names": ["expensive_search", "heavy_analyzer", "unknown_tool"]
            }),
        )
        .await;

        assert!(!result.is_error);
        let message = output_text(&result);
        assert!(message.contains("Loaded: expensive_search"));
        assert!(message.contains("Already available: heavy_analyzer"));
        assert!(message.contains("Unknown tool: unknown_tool"));
    }

    #[tokio::test]
    async fn should_return_error_when_disclosure_not_enabled() {
        let svc = Arc::new(Mutex::new(ToolSelectService::new()));
        let tool = SelectToolsTool::new(svc);

        let result = execute_tool(&tool, serde_json::json!({ "names": ["any_tool"] })).await;

        assert!(result.is_error);
        let message = output_text(&result);
        assert!(message.contains("not configured"));
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_names_argument_is_missing() {
        let svc = make_service();
        let tool = SelectToolsTool::new(svc);

        let execution = tool.resolve_execution(serde_json::json!({})).unwrap();
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution for missing names");
        };
        assert!(error.message.contains("'names'"));
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_names_array_is_empty() {
        let svc = make_service();
        let tool = SelectToolsTool::new(svc);

        let execution = tool
            .resolve_execution(serde_json::json!({ "names": [] }))
            .unwrap();
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution for empty names");
        };
        assert!(error.message.contains("at least one"));
    }
}
