//! Lifecycle scope ordering — defines the strict nesting hierarchy.
//!
//! The four scope levels form a total order: `App < Workspace < Session <
//! Agent`. A child scope must be strictly higher than its parent, and a
//! higher-numbered scope is shorter-lived than a lower-numbered one.

use std::fmt;

use crate::error::{EngineError, EngineResult};

/// The four scope levels, ordered from longest-lived to shortest-lived.
///
/// The numeric discriminant encodes the total order: a higher value means a
/// shorter-lived scope nested deeper in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum LifecycleScope {
    /// Root scope — lives for the entire application lifetime.
    App = 0,
    /// A workspace — lives until explicitly removed or the app shuts down.
    Workspace = 1,
    /// A conversation session within a workspace.
    Session = 2,
    /// An individual agent loop within a session.
    Agent = 3,
}

impl fmt::Display for LifecycleScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::App => write!(f, "app"),
            Self::Workspace => write!(f, "workspace"),
            Self::Session => write!(f, "session"),
            Self::Agent => write!(f, "agent"),
        }
    }
}

impl LifecycleScope {
    /// Verify that `self` (the child) is strictly higher in the hierarchy than
    /// `parent`.
    ///
    /// Returns `Err` if the child is not strictly deeper than the parent —
    /// for example, trying to nest an `App` inside a `Workspace`, or a
    /// `Session` inside another `Session`.
    pub fn must_be_strictly_higher(self, parent: LifecycleScope) -> EngineResult<()> {
        if self > parent {
            Ok(())
        } else {
            Err(EngineError::RequestInvalid(format!(
                "lifecycle scope '{}' must be strictly higher than parent '{}'",
                self, parent
            )))
        }
    }

    /// Whether `self` is shorter-lived than `other`.
    ///
    /// A scope is shorter-lived than any scope with a lower numeric level.
    /// For example, `Agent.is_shorter_lived_than(App)` is `true`.
    pub fn is_shorter_lived_than(self, other: LifecycleScope) -> bool {
        self > other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_order_scopes_strictly_when_comparing_lifecycle_scopes() {
        assert!(LifecycleScope::App < LifecycleScope::Workspace);
        assert!(LifecycleScope::Workspace < LifecycleScope::Session);
        assert!(LifecycleScope::Session < LifecycleScope::Agent);
    }

    #[test]
    fn should_succeed_when_child_scope_is_strictly_higher_than_parent() {
        assert!(
            LifecycleScope::Workspace
                .must_be_strictly_higher(LifecycleScope::App)
                .is_ok()
        );
        assert!(
            LifecycleScope::Session
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
    fn should_error_when_child_scope_is_not_strictly_higher_than_parent() {
        assert!(
            LifecycleScope::App
                .must_be_strictly_higher(LifecycleScope::App)
                .is_err()
        );
        assert!(
            LifecycleScope::App
                .must_be_strictly_higher(LifecycleScope::Workspace)
                .is_err()
        );
        assert!(
            LifecycleScope::Workspace
                .must_be_strictly_higher(LifecycleScope::Workspace)
                .is_err()
        );
        assert!(
            LifecycleScope::Workspace
                .must_be_strictly_higher(LifecycleScope::Session)
                .is_err()
        );
    }

    #[test]
    fn should_be_shorter_lived_when_scope_level_is_higher() {
        assert!(LifecycleScope::Agent.is_shorter_lived_than(LifecycleScope::Session));
        assert!(LifecycleScope::Agent.is_shorter_lived_than(LifecycleScope::App));
        assert!(!LifecycleScope::App.is_shorter_lived_than(LifecycleScope::Agent));
        assert!(!LifecycleScope::Session.is_shorter_lived_than(LifecycleScope::Session));
    }
}
