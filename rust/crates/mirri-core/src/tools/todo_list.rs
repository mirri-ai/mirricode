//! `TodoListTool` — manage the session-scoped todo list.
//!
//! The tool serves both reads and writes (mirroring the TypeScript
//! `TodoListTool` / `todo` domain):
//!
//!   - `resolve_execution({ todos: [...] })` — replace the full list
//!   - `resolve_execution({ todos: [] })`    — clear the list
//!   - `resolve_execution({})`               — query current list (no mutation)

use std::sync::Arc;

use mirri_proto::WireOp;

use crate::error::EngineResult;
use crate::todo::{TodoItem, TodoList, TodoStatus, render_todo_list};
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};

/// Reminder appended to the output when the todo list is written, mirroring
/// the TypeScript `TODO_LIST_WRITE_REMINDER` constant.
const TODO_LIST_WRITE_REMINDER: &str = "Ensure that you continue to use the \
    todo list to track progress. Mark tasks done immediately after finishing \
    them, and keep exactly one task in_progress when work is underway.";

/// `TodoList` — view or update the session todo list.
pub struct TodoListTool {
    /// The session-scoped list this tool reads and replaces.
    todo_list: Arc<TodoList>,
    /// The session's wire-log recorder. When attached, writes persist as
    /// `tools.update_store` (key "todo") ops (write-before-mutate) so a
    /// resumed session replays them back into the todo list.
    wire_recorder: Option<crate::scope::SessionWireLog>,
}

impl TodoListTool {
    /// Create the tool bound to the session-scoped todo list.
    pub fn new(todo_list: Arc<TodoList>) -> Self {
        Self {
            todo_list,
            wire_recorder: None,
        }
    }

    /// Attach the session's wire-log recorder so writes persist as
    /// `tools.update_store` (key "todo") records.
    pub fn with_wire_recorder(mut self, recorder: crate::scope::SessionWireLog) -> Self {
        self.wire_recorder = Some(recorder);
        self
    }
}

impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "TodoList"
    }

    fn description(&self) -> &str {
        "View or update the session todo list. The list is shared across the \
         whole session, so subagents see the same items. Omit the 'todos' \
         argument to read the current list. When writing, always pass the \
         complete list you want to keep; items are replaced wholesale."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {
                                    "type": "string",
                                    "description": "Description of the task."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "done"],
                                    "description": "Task status."
                                }
                            },
                            "required": ["title", "status"]
                        },
                        "description": "The complete list of todo items to keep. Omit to read the current todo list without making changes. Pass an empty array to clear the list."
                    }
                },
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let todo_list = Arc::clone(&self.todo_list);

        // Query mode — no `todos` argument: return the current list without mutation.
        if input.get("todos").is_none() {
            let plan = ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::TodoList,
                approval_rule: "TodoList".to_string(),
                stop_batch_after_this: false,
            };
            return Ok(ToolExecution::Runnable {
                plan,
                execute: Box::new(move |_ctx| {
                    Box::pin(async move { ToolResult::success(render_todo_list(&todo_list.get())) })
                }),
            });
        }

        // Write mode — parse and validate the provided todos.
        let Some(todos) = input.get("todos").and_then(serde_json::Value::as_array) else {
            return Ok(crate::tools::tool_execution_error(
                "TodoList 'todos' must be an array",
            ));
        };

        let mut items = Vec::with_capacity(todos.len());
        for raw_item in todos {
            let (Some(title), Some(status)) = (
                raw_item.get("title").and_then(serde_json::Value::as_str),
                raw_item.get("status").and_then(serde_json::Value::as_str),
            ) else {
                return Ok(crate::tools::tool_execution_error(format!(
                    "TodoList item missing 'title' or 'status': {raw_item}"
                )));
            };
            let status = match status.parse::<TodoStatus>() {
                Ok(status) => status,
                Err(error) => {
                    return Ok(crate::tools::tool_execution_error(format!(
                        "TodoList: {error}"
                    )));
                }
            };
            items.push(TodoItem {
                title: title.to_string(),
                status,
            });
        }

        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::TodoList,
            approval_rule: "TodoList".to_string(),
            stop_batch_after_this: false,
        };

        let wire_recorder = self.wire_recorder.clone();

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    // Write-before-mutate: persist the new list as a
                    // `tools.update_store` op (key "todo") before touching
                    // the in-memory list, so a crash between the two leaves
                    // the wire log ahead of memory (resume treats the wire
                    // log as the source of truth). Query mode never reaches
                    // here.
                    let wire_value: Vec<serde_json::Value> = items
                        .iter()
                        .map(|t| serde_json::json!({ "title": t.title, "status": t.status.as_str() }))
                        .collect();
                    if let Some(recorder) = wire_recorder.as_ref() {
                        recorder
                            .record_op(WireOp::ToolsUpdateStore {
                                key: "todo".to_string(),
                                value: serde_json::Value::Array(wire_value),
                            })
                            .await;
                    }
                    todo_list.set(items);
                    let stored = todo_list.get();
                    let output = if stored.is_empty() {
                        "Todo list cleared.".to_string()
                    } else {
                        format!(
                            "Todo list updated.\n{}\n\n{}",
                            render_todo_list(&stored),
                            TODO_LIST_WRITE_REMINDER
                        )
                    };
                    ToolResult::success(output)
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::TodoStatus;
    use crate::tool::{ToolOutput, ToolResult};

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &TodoListTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    #[tokio::test]
    async fn should_return_rendered_list_and_store_items_when_setting_todos_on_an_empty_list() {
        let list = Arc::new(TodoList::new());
        let tool = TodoListTool::new(Arc::clone(&list));
        let input = serde_json::json!({
            "todos": [
                {"title": "First", "status": "pending"},
                {"title": "Second", "status": "done"}
            ]
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        let rendered = output_text(&result);
        assert!(rendered.contains("[pending] First"), "rendered: {rendered}");
        assert!(rendered.contains("[done] Second"), "rendered: {rendered}");
        // The list is stored on the shared service for other agents to see.
        let stored = list.get();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[1].status, TodoStatus::Done);
    }

    #[tokio::test]
    async fn should_return_execution_error_without_any_write_when_setting_an_invalid_status() {
        let list = Arc::new(TodoList::new());
        list.set(vec![TodoItem {
            title: "Keep me".to_string(),
            status: TodoStatus::Pending,
        }]);
        let tool = TodoListTool::new(Arc::clone(&list));
        let input = serde_json::json!({
            "todos": [{"title": "Bad", "status": "blocked"}]
        });
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };
        assert!(error.message.contains("invalid todo status 'blocked'"));
        // The existing list is untouched.
        assert_eq!(list.get()[0].title, "Keep me");
    }

    #[tokio::test]
    async fn should_clear_the_list_when_setting_empty_todos() {
        let list = Arc::new(TodoList::new());
        list.set(vec![TodoItem {
            title: "Old".to_string(),
            status: TodoStatus::Pending,
        }]);
        let tool = TodoListTool::new(Arc::clone(&list));
        let input = serde_json::json!({ "todos": [] });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "Todo list cleared.");
        assert!(list.get().is_empty());
    }

    #[tokio::test]
    async fn should_return_current_list_when_todos_is_not_provided() {
        let list = Arc::new(TodoList::new());
        list.set(vec![
            TodoItem {
                title: "Read docs".to_string(),
                status: TodoStatus::Done,
            },
            TodoItem {
                title: "Write code".to_string(),
                status: TodoStatus::InProgress,
            },
        ]);
        let tool = TodoListTool::new(Arc::clone(&list));
        // No `todos` key at all → query mode.
        let input = serde_json::json!({});
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        let rendered = output_text(&result);
        assert!(
            rendered.contains("Current todo list:"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("[done] Read docs"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("[in_progress] Write code"),
            "rendered: {rendered}"
        );
        // Query mode must not mutate the list.
        assert_eq!(list.get().len(), 2);
        assert_eq!(list.get()[0].title, "Read docs");
    }

    #[tokio::test]
    async fn should_update_and_return_list_with_reminder_when_todos_is_provided() {
        let list = Arc::new(TodoList::new());
        let tool = TodoListTool::new(Arc::clone(&list));
        let input = serde_json::json!({
            "todos": [
                {"title": "Task A", "status": "pending"},
                {"title": "Task B", "status": "in_progress"}
            ]
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        let rendered = output_text(&result);
        // The output starts with the "updated" banner.
        assert!(
            rendered.starts_with("Todo list updated.\n"),
            "rendered: {rendered}"
        );
        // The rendered list is included.
        assert!(
            rendered.contains("[pending] Task A"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("[in_progress] Task B"),
            "rendered: {rendered}"
        );
        // The list is stored on the shared service.
        let stored = list.get();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].title, "Task A");
        assert_eq!(stored[1].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn should_record_a_wire_op_when_writing_todos() {
        let list = Arc::new(TodoList::new());
        let recorder = crate::scope::SessionWireLog::default();
        // `Default` starts detached (record_op is a no-op); defer so ops buffer.
        recorder.start_deferred().await;
        let tool = TodoListTool::new(Arc::clone(&list)).with_wire_recorder(recorder.clone());
        let input = serde_json::json!({
            "todos": [{"title": "Task A", "status": "pending"}]
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        // The deferred slot buffers the op — read it back.
        let ops = recorder.take_deferred_ops().await;
        assert_eq!(ops.len(), 1, "exactly one wire op per write");
    }

    #[tokio::test]
    async fn should_not_record_a_wire_op_when_querying_todos() {
        let list = Arc::new(TodoList::new());
        let recorder = crate::scope::SessionWireLog::default();
        // `Default` starts detached (record_op is a no-op); defer so ops buffer.
        recorder.start_deferred().await;
        let tool = TodoListTool::new(Arc::clone(&list)).with_wire_recorder(recorder.clone());
        let result = execute_tool(&tool, serde_json::json!({})).await;
        assert!(!result.is_error);
        assert!(
            recorder.take_deferred_ops().await.is_empty(),
            "query records nothing"
        );
    }

    #[tokio::test]
    async fn should_include_write_reminder_when_updating_todos() {
        let list = Arc::new(TodoList::new());
        let tool = TodoListTool::new(Arc::clone(&list));
        let input = serde_json::json!({
            "todos": [{"title": "Do something", "status": "pending"}]
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        let rendered = output_text(&result);
        // The write reminder must be present in the output.
        assert!(
            rendered.contains("Ensure that you continue to use the todo list"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("keep exactly one task in_progress"),
            "rendered: {rendered}"
        );
        // The reminder follows the rendered list, separated by a blank line.
        assert!(
            rendered.contains("\n\nEnsure that you continue"),
            "rendered: {rendered}"
        );
    }
}
