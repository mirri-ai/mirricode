//! TodoList staleness reminders derived from recent history.

use super::*;

// ====================================================================
// TodoListReminderInjector
// ====================================================================

/// Injects a reminder to use TodoList when it hasn't been updated recently.
///
/// Dedup is derived from history: counts assistant turns since the last
/// TodoList tool call and since the last todo-list reminder.
pub struct TodoListReminderInjector {
    todo_list: Arc<TodoList>,
    /// Inject-time gate: is `TodoList` active on the owning agent's tool
    /// face right now? Evaluated on every inject call (US4.2: a mid-session
    /// disable must silence the reminder on the next turn). `None` — the
    /// injector is inert (the wiring site did not supply a tool face).
    tool_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl TodoListReminderInjector {
    /// Create a todo-list reminder injector backed by the given list.
    pub fn new(todo_list: Arc<TodoList>) -> Self {
        Self {
            todo_list,
            tool_gate: None,
        }
    }

    /// Wire the active-tool gate: the reminder only injects while the gate
    /// returns `true` (the `TodoList` tool is on the agent's active tool
    /// face). Evaluated at every inject call, so a mid-session disable
    /// silences the next injection. Without a gate the injector is inert.
    pub fn with_tool_gate(mut self, gate: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.tool_gate = Some(gate);
        self
    }
}

/// Minimum number of assistant turns since the last TodoList tool call before
/// a reminder is injected.
const TURNS_SINCE_WRITE: u32 = 10;
/// Minimum number of assistant turns since the last todo-list reminder before
/// a new one is injected.
const TURNS_BETWEEN_REMINDERS: u32 = 10;

/// Whether a tool call is a *write* to the TodoList tool.
///
/// Only calls whose `arguments` JSON contains a `"todos"` key with an array
/// value count as writes — a query-only TodoList call (no `"todos"`) does not
/// reset the write-turn counter.
fn is_todo_list_write(tool_call: &ToolCall) -> bool {
    if tool_call.function.name != "TodoList" {
        return false;
    }
    // Check if arguments contain a "todos" array.
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tool_call.function.arguments) {
        args.get("todos").is_some_and(|v| v.is_array())
    } else {
        false
    }
}

impl Injector for TodoListReminderInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        // US4.2 inject-time gate: both a closed gate AND an unset gate mean
        // no injection (an agent whose TodoList availability is unknown must
        // not be nagged). Only an open gate proceeds to the threshold logic.
        match &self.tool_gate {
            Some(gate) if !gate() => return None,
            None => return None,
            _ => {}
        }

        let history = memory.history();

        let mut turns_since_last_write: u32 = 0;
        let mut turns_since_last_reminder: u32 = 0;
        let mut found_todo_call = false;
        let mut found_reminder = false;

        for m in history.iter().rev() {
            if m.message.role == Role::Assistant {
                if !found_todo_call {
                    turns_since_last_write += 1;
                }
                if !found_reminder {
                    turns_since_last_reminder += 1;
                }
                // Check for a TodoList *write* call — stop counting write turns.
                if !found_todo_call && m.message.tool_calls.iter().any(is_todo_list_write) {
                    found_todo_call = true;
                }
            }
            if !found_reminder
                && m.origin == Some(PromptOrigin::Injection)
                && m.message
                    .text_content()
                    .contains("The TodoList tool has not been updated recently")
            {
                found_reminder = true;
            }
            if found_todo_call && found_reminder {
                break;
            }
        }

        if turns_since_last_write < TURNS_SINCE_WRITE
            || turns_since_last_reminder < TURNS_BETWEEN_REMINDERS
        {
            return None;
        }

        let todos = self.todo_list.get();

        let mut text = "\
The TodoList tool has not been updated recently. If you are working on tasks that benefit from progress tracking, consider using TodoList to update todo status. Also consider clearing or rewriting the todo list if it has become stale and no longer matches the current work. Only use it if relevant. This is a gentle reminder; ignore it if not applicable. Make sure that you NEVER mention this this reminder to the user."
            .to_string();

        if !todos.is_empty() {
            let rendered = todos
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{}. [{}] {}", i + 1, t.status, t.title))
                .collect::<Vec<_>>()
                .join("\n");
            text.push_str("\n\nCurrent todo list:\n");
            text.push_str(&rendered);
        }

        Some(text)
    }
}
