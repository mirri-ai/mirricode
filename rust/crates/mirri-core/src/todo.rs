//! Session-scoped todo list service.
//!
//! Mirrors the TypeScript `todo` domain (`ISessionTodoService`): a shared,
//! session-wide list of `TodoItem`s that the `TodoList` tool reads and
//! replaces. The list is session-scoped so subagents within one session share
//! the same view.

use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The status of a todo item.
///
/// Serialized to the wire as `"pending"`, `"in_progress"`, or `"done"`
/// (snake_case), matching the TypeScript `TodoStatus` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// The item is waiting to be started.
    Pending,
    /// Work on the item is underway.
    InProgress,
    /// The item is finished.
    Done,
}

impl TodoStatus {
    /// The canonical wire representation (`"pending"` / `"in_progress"` /
    /// `"done"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TodoStatus {
    type Err = String;

    /// Parse a status from its wire representation.
    ///
    /// Accepts the canonical lowercase forms (`pending`, `in_progress`,
    /// `done`) and any case variant of them (`Pending`, `IN_PROGRESS`).
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "done" => Ok(Self::Done),
            other => Err(format!(
                "invalid todo status '{other}' (expected 'pending', 'in_progress', or 'done')"
            )),
        }
    }
}

/// A single todo list entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Human-readable description of the task.
    pub title: String,
    /// Current status of the task.
    pub status: TodoStatus,
}

/// The session-scoped todo list.
///
/// `set` replaces the whole list (the `TodoList` tool always writes the full
/// list the model wants to keep), while `get`/`clear` provide read and reset
/// access. All methods take `&self`; the list is guarded by an internal
/// `Mutex` so a single `Arc<TodoList>` can be shared across agents and tasks.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoList {
    /// Create an empty todo list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire list with `items`.
    pub fn set(&self, items: Vec<TodoItem>) {
        let mut guard = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = items;
    }

    /// Snapshot of the current items, in list order.
    pub fn get(&self) -> Vec<TodoItem> {
        let guard = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }

    /// Remove every item from the list.
    pub fn clear(&self) {
        let mut guard = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clear();
    }
}

/// Render a todo list for model consumption, mirroring the TypeScript
/// `renderTodoList` helper:
///
/// ```text
/// Current todo list:
///   [pending] title
///   [done] title
/// ```
pub fn render_todo_list(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Todo list is empty.".to_string();
    }
    let mut lines = vec!["Current todo list:".to_string()];
    for item in items {
        lines.push(format!("  [{}] {}", item.status, item.title));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_empty_when_rendering_empty_list() {
        let rendered = render_todo_list(&[]);
        assert_eq!(rendered, "Todo list is empty.");
    }

    #[test]
    fn should_list_each_item_with_status_when_rendering_populated_list() {
        let items = vec![
            TodoItem {
                title: "Write tests".to_string(),
                status: TodoStatus::Pending,
            },
            TodoItem {
                title: "Run clippy".to_string(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                title: "Commit".to_string(),
                status: TodoStatus::Done,
            },
        ];
        let rendered = render_todo_list(&items);
        assert_eq!(
            rendered,
            "Current todo list:\n  [pending] Write tests\n  [in_progress] Run clippy\n  [done] Commit"
        );
    }

    #[test]
    fn should_return_canonical_status_when_parsing_status_variants() {
        assert_eq!("pending".parse::<TodoStatus>(), Ok(TodoStatus::Pending));
        assert_eq!(
            "in_progress".parse::<TodoStatus>(),
            Ok(TodoStatus::InProgress)
        );
        assert_eq!("Done".parse::<TodoStatus>(), Ok(TodoStatus::Done));
        assert_eq!(
            "IN_PROGRESS".parse::<TodoStatus>(),
            Ok(TodoStatus::InProgress)
        );
    }

    #[test]
    fn should_return_error_when_parsing_unknown_status() {
        let error = "blocked".parse::<TodoStatus>().unwrap_err();
        assert!(error.contains("blocked"));
    }

    #[test]
    fn should_use_wire_format_when_serializing_status() {
        assert_eq!(
            serde_json::to_string(&TodoStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
    }

    #[test]
    fn should_be_empty_when_getting_a_new_list() {
        let list = TodoList::new();
        assert!(list.get().is_empty());
    }

    #[test]
    fn should_return_items_in_order_when_getting_after_set() {
        let list = TodoList::new();
        let items = vec![
            TodoItem {
                title: "First".to_string(),
                status: TodoStatus::Pending,
            },
            TodoItem {
                title: "Second".to_string(),
                status: TodoStatus::Done,
            },
        ];
        list.set(items.clone());
        assert_eq!(list.get(), items);
    }

    #[test]
    fn should_restore_empty_list_when_clearing_after_set() {
        let list = TodoList::new();
        list.set(vec![TodoItem {
            title: "Only".to_string(),
            status: TodoStatus::Pending,
        }]);
        list.clear();
        assert!(list.get().is_empty());
    }

    #[test]
    fn should_expose_latest_state_to_all_handles_when_setting_shared_list() {
        let list = std::sync::Arc::new(TodoList::new());
        let other = std::sync::Arc::clone(&list);
        other.set(vec![TodoItem {
            title: "Shared".to_string(),
            status: TodoStatus::InProgress,
        }]);
        assert_eq!(list.get().len(), 1);
        assert_eq!(list.get()[0].title, "Shared");
    }
}
