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
///
/// The undo checkpoint stack (TS parity: the `Checkpointed` model's
/// `checkpoints`, `conversationTime.ts`) lives under the same mutex: each
/// real-user-input undo anchor snapshots the current list, and an undo pops
/// the stack and restores the snapshot at the popped depth.
#[derive(Debug, Default)]
pub struct TodoList {
    items: Mutex<Vec<TodoItem>>,
    checkpoints: Mutex<Vec<Vec<TodoItem>>>,
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

    /// Push the current list onto the undo checkpoint stack (the memory-side
    /// mirror of the reducer's `context.append_message` checkpoint rule: a
    /// real-user-input undo anchor snapshots the todo list).
    pub fn push_checkpoint(&self) {
        let guard = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stack = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stack.push(guard.clone());
    }

    /// Push a pre-parsed snapshot onto the undo checkpoint stack — the
    /// resume-restore face of [`Self::push_checkpoint`] (the folded wire
    /// state already carries the parsed checkpoint values; the current
    /// in-memory list may not match them).
    pub fn push_items_checkpoint(&self, items: Vec<TodoItem>) {
        let mut stack = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stack.push(items);
    }

    /// Pop `n` checkpoints, restoring the list from the checkpoint at depth
    /// `n` (the memory-side mirror of the reducer's `context.undo` rule).
    /// Returns how many depths were actually applied — `0` when `n` exceeds
    /// the stack, in which case the current list is left unchanged (TS
    /// parity: an undo count exceeding the checkpoints is a no-op).
    pub fn pop_checkpoint(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let mut stack = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if n > stack.len() {
            return 0;
        }
        let idx = stack.len() - n;
        let restored = stack[idx].clone();
        stack.truncate(idx);
        // Lock ordering: drop the checkpoints guard BEFORE taking the items
        // guard. `push_checkpoint` acquires items → checkpoints; holding
        // both here in the reverse order would deadlock a pop racing a push.
        drop(stack);
        let mut guard = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = restored;
        n
    }

    /// Clear the undo checkpoint stack without touching the current list
    /// (the memory-side mirror of the fold's clearing rule: the reducer
    /// drops the checkpoints on `context.apply_compaction` and
    /// `context.clear` — pre-compaction/pre-clear anchor positions are no
    /// longer reachable by undo; `full_compaction.complete` never clears).
    pub fn clear_checkpoints(&self) {
        let mut stack = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stack.clear();
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

    #[test]
    fn should_restore_the_checkpointed_list_when_popping_after_a_write() {
        // Given a set list with one checkpoint pushed, then a new list set
        let list = TodoList::new();
        let original = vec![TodoItem {
            title: "Original".to_string(),
            status: TodoStatus::Pending,
        }];
        list.set(original.clone());
        list.push_checkpoint();
        list.set(vec![TodoItem {
            title: "New".to_string(),
            status: TodoStatus::InProgress,
        }]);

        // When one checkpoint is popped
        let applied = list.pop_checkpoint(1);

        // Then the original list is back and one depth was applied
        assert_eq!(applied, 1);
        assert_eq!(list.get(), original);
    }

    #[test]
    fn should_return_zero_when_popping_more_than_pushed() {
        // Given a list with no checkpoints
        let list = TodoList::new();
        list.set(vec![TodoItem {
            title: "Keep".to_string(),
            status: TodoStatus::Pending,
        }]);

        // When popping more than exists
        let applied = list.pop_checkpoint(3);

        // Then nothing was applied and the list is unchanged
        assert_eq!(applied, 0);
        assert_eq!(
            list.get(),
            vec![TodoItem {
                title: "Keep".to_string(),
                status: TodoStatus::Pending,
            }]
        );
    }

    #[test]
    fn should_restore_the_deepest_checkpoint_when_popping_multiple() {
        // Given two checkpoints with distinct lists
        let list = TodoList::new();
        let first = vec![TodoItem {
            title: "First".to_string(),
            status: TodoStatus::Pending,
        }];
        list.set(first.clone());
        list.push_checkpoint();
        list.set(vec![TodoItem {
            title: "Second".to_string(),
            status: TodoStatus::InProgress,
        }]);
        list.push_checkpoint();
        list.set(vec![TodoItem {
            title: "Third".to_string(),
            status: TodoStatus::Done,
        }]);

        // When two checkpoints are popped at once
        let applied = list.pop_checkpoint(2);

        // Then the deepest (first) list is restored and the stack is empty
        assert_eq!(applied, 2);
        assert_eq!(list.get(), first);
        assert_eq!(list.pop_checkpoint(1), 0);
    }

    #[test]
    fn should_discard_checkpoints_when_cleared() {
        // Given a list with a checkpoint
        let list = TodoList::new();
        list.set(vec![TodoItem {
            title: "A".to_string(),
            status: TodoStatus::Pending,
        }]);
        list.push_checkpoint();

        // When the checkpoints are cleared
        list.clear_checkpoints();

        // Then a pop applies nothing (the stack is gone) while the current
        // list is untouched
        assert_eq!(list.pop_checkpoint(1), 0);
        assert_eq!(
            list.get(),
            vec![TodoItem {
                title: "A".to_string(),
                status: TodoStatus::Pending,
            }]
        );
    }
}
