//! Type-safe identifiers for each scope level.
//!
//! Each ID wraps an `Arc<str>` so cloning is cheap and the same ID value can be
//! shared across threads without copying the string data.

use std::fmt;
use std::sync::Arc;

/// Generates a type-safe ID wrapper around `Arc<str>`.
macro_rules! define_scope_id {
    ($name:ident, $doc_name:expr) => {
        #[doc = concat!("Type-safe identifier for a ", $doc_name, ".")]
        ///
        /// Internally backed by `Arc<str>` so clones are cheap.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Create a new ID from any string-like value.
            pub fn new(s: impl Into<String>) -> Self {
                Self(Arc::from(s.into()))
            }

            /// Access the underlying string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self::new(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self::new(s.to_string())
            }
        }

        impl From<&String> for $name {
            fn from(s: &String) -> Self {
                Self::new(s.clone())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

define_scope_id!(WorkspaceId, "workspace");
define_scope_id!(SessionId, "session");
define_scope_id!(AgentId, "agent");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_display_and_compare_by_value_when_creating_workspace_id() {
        let a = WorkspaceId::new("ws-1");
        let b = WorkspaceId::new("ws-1");
        let c = WorkspaceId::new("ws-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.to_string(), "ws-1");
        assert_eq!(a.as_str(), "ws-1");
    }

    #[test]
    fn should_convert_from_string_when_creating_session_id() {
        let s: SessionId = "sess-42".to_string().into();
        assert_eq!(s.as_str(), "sess-42");
    }

    #[test]
    fn should_convert_from_str_when_creating_agent_id() {
        let a: AgentId = "agent-7".into();
        assert_eq!(a.as_str(), "agent-7");
    }

    #[test]
    fn should_produce_equal_clone_when_cloning_workspace_id() {
        let a = WorkspaceId::new("ws-clonable");
        let b = a.clone();
        assert_eq!(a, b);
    }
}
