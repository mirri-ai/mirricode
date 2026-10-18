//! Injection system — per-step system reminders injected into context memory.
//!
//! The `Injector` trait allows the host to inspect the current context memory
//! and optionally produce a reminder string. The `InjectionManager` collects
//! multiple injectors and runs them in order, appending each non-`None` result
//! to context memory as a system reminder.

use crate::capability_gap::{
    CapabilityGapDetector, ModelCapabilities, ProfileInfo, extract_attachment_paths,
};
use crate::context_memory::ContextMemory;
use crate::goal::{Goal, GoalBudget, GoalManager, GoalStatus};
use crate::message::{Role, ToolCall};
use crate::origin::PromptOrigin;
use crate::permission::{PermissionManager, PermissionMode};
use crate::plan::PlanManager;
use crate::todo::TodoList;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// A single injector that can produce a per-step reminder.
///
/// The injector receives a read-only reference to context memory and may
/// return `Some(text)` to inject. The text is wrapped in a `<system-reminder>`
/// tag by `ContextMemory::append_system_reminder`.
pub trait Injector: Send + Sync {
    /// Inspect the context memory and optionally return a reminder string.
    fn inject(&self, memory: &ContextMemory) -> Option<String>;
}

/// Manages a collection of injectors.
///
/// Injectors are run in registration order. Each non-`None` result is
/// appended to context memory as a system reminder.
pub struct InjectionManager {
    injectors: Vec<Arc<dyn Injector>>,
}

impl InjectionManager {
    /// Create an empty injection manager.
    pub fn new() -> Self {
        Self {
            injectors: Vec::new(),
        }
    }

    /// Add an injector to the collection.
    pub fn add(&mut self, injector: Box<dyn Injector>) {
        self.injectors.push(Arc::from(injector));
    }

    /// Add a shared injector (for injectors that need cross-turn state
    /// persistence).
    ///
    /// The caller passes an `Arc<dyn Injector>` so the same injector instance
    /// — and its `Mutex`-protected state — survives across turns even though a
    /// fresh `InjectionManager` is built each turn.
    pub fn add_shared(&mut self, injector: Arc<dyn Injector>) {
        self.injectors.push(injector);
    }

    /// Run all injectors and append their results to context memory.
    ///
    /// Each injector that returns `Some(text)` causes a
    /// `memory.append_system_reminder(text)` call. Returns the injected
    /// reminder texts, unwrapped and in injection order, so the caller can
    /// persist them to the wire log (`context.append_message`): the wire
    /// replay must materialize the exact same reminders the live session
    /// appended, or the restored history drifts from the live one (the wire
    /// replay equivalence invariant, ADR-009).
    pub fn inject(&self, memory: &mut ContextMemory) -> Vec<String> {
        let mut injected = Vec::new();
        for injector in &self.injectors {
            if let Some(text) = injector.inject(memory) {
                memory.append_system_reminder(text.clone());
                injected.push(text);
            }
        }
        injected
    }

    /// Number of registered injectors.
    pub fn len(&self) -> usize {
        self.injectors.len()
    }

    /// Whether no injectors are registered.
    pub fn is_empty(&self) -> bool {
        self.injectors.is_empty()
    }
}

impl Default for InjectionManager {
    fn default() -> Self {
        Self::new()
    }
}

mod capability_gap;
mod goal;
mod permission_mode;
mod plan_mode;
mod plugin_session_start;
mod todo_list;
mod tools_diff;

#[cfg(test)]
mod tests;

pub use capability_gap::CapabilityGapInjector;
pub use goal::GoalInjector;
pub use permission_mode::PermissionModeInjector;
pub use plan_mode::PlanModeInjector;
pub use plugin_session_start::PluginSessionStartInjector;
pub use todo_list::TodoListReminderInjector;
pub use tools_diff::ToolsDiffInjector;
