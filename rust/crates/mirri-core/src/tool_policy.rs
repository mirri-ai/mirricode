//! `tool_policy` — agent-scope tool authorization.
//!
//! Ports the TypeScript `toolPolicy` domain. A [`ToolActivationPolicy`] is an
//! allowlist/denylist pair attached to an agent profile. The
//! [`AgentToolPolicyService`] layers the profile policy with session-scoped and
//! global-scoped disable lists to answer the single question every tool call
//! site needs answered: *is this tool permitted to run right now?*
//!
//! Two flavors of the question exist:
//!
//! - [`ToolActivationPolicy::is_active`] — runtime permission (may the tool
//!   execute).
//! - [`ToolActivationPolicy::is_active_for_disclosure`] — disclosure permission
//!   (may the tool be advertised to the model). Deferred tools are permitted to
//!   run but are withheld from disclosure until explicitly loaded.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The origin category of a tool.
///
/// Carried through authorization decisions so policies can apply different
/// rules to first-party builtins, user-defined tools, and MCP-sourced tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolSource {
    /// First-party tool shipped with the engine.
    Builtin,
    /// Tool defined by the user (e.g. a local script registration).
    User,
    /// Tool provided by an MCP server.
    Mcp,
}

impl fmt::Display for ToolSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolSource::Builtin => write!(f, "builtin"),
            ToolSource::User => write!(f, "user"),
            ToolSource::Mcp => write!(f, "mcp"),
        }
    }
}

/// An allowlist/denylist pair controlling which tools are active for a profile.
///
/// - `enabled_tools`, when `Some`, is an explicit allowlist: only tools named
///   in it are considered active (subject to the denylist). When `None`, all
///   tools are candidates (open by default).
/// - `disabled_tools` is a denylist that always wins.
/// - `deferred` lists tools that are permitted to run but withheld from
///   disclosure until loaded (progressive disclosure).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolActivationPolicy {
    /// Optional allowlist. `None` means open (no allowlist gating).
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist — always takes precedence over the allowlist.
    pub disabled_tools: Vec<String>,
    /// Tools withheld from disclosure until explicitly loaded.
    pub deferred: Vec<String>,
}

impl ToolActivationPolicy {
    /// Whether a tool is permitted to run under this policy.
    ///
    /// The `source` is accepted for source-scoped rule parity with the
    /// TypeScript port and is reserved for future per-source rules; the core
    /// decision is driven by the enabled/disabled lists.
    pub fn is_active(&self, name: &str, _source: ToolSource) -> bool {
        if self.list_contains(&self.disabled_tools, name) {
            return false;
        }
        match &self.enabled_tools {
            Some(allowlist) => self.list_contains(allowlist, name),
            None => true,
        }
    }

    /// Whether a tool may be *disclosed* (advertised) to the model.
    ///
    /// Same as [`is_active`](Self::is_active) but additionally excludes
    /// deferred tools — they are not shown until explicitly loaded.
    pub fn is_active_for_disclosure(&self, name: &str, source: ToolSource) -> bool {
        if self.list_contains(&self.deferred, name) {
            return false;
        }
        self.is_active(name, source)
    }

    fn list_contains(&self, list: &[String], name: &str) -> bool {
        list.iter().any(|entry| entry == name)
    }
}

/// Layered tool authorization service for a single agent.
///
/// Combines a profile policy with session-scoped and global-scoped disable
/// lists. The precedence is:
///
/// 1. profile policy (allowlist/denylist/deferred),
/// 2. session-scoped disable list,
/// 3. global-scoped disable list.
///
/// A tool is active only if it passes every layer.
pub struct AgentToolPolicyService {
    profile_policy: ToolActivationPolicy,
    session_disabled: Vec<String>,
    global_disabled: Vec<String>,
}

impl AgentToolPolicyService {
    /// Create a service backed by the given profile policy and empty
    /// session/global disable lists.
    pub fn new(profile_policy: ToolActivationPolicy) -> Self {
        Self {
            profile_policy,
            session_disabled: Vec::new(),
            global_disabled: Vec::new(),
        }
    }

    /// Whether a tool is permitted to run, honoring the profile policy and the
    /// session/global disable lists.
    ///
    /// `source` defaults to [`ToolSource::Builtin`] when `None`.
    pub fn is_tool_active(&self, name: &str, source: Option<ToolSource>) -> bool {
        self.is_tool_active_for_profile(&self.profile_policy, name, source)
    }

    /// Whether a tool may be disclosed to the model, honoring the profile
    /// policy (including its deferred list) and the session/global disable
    /// lists.
    ///
    /// `source` defaults to [`ToolSource::Builtin`] when `None`.
    pub fn is_tool_active_for_disclosure(&self, name: &str, source: Option<ToolSource>) -> bool {
        let source = source.unwrap_or(ToolSource::Builtin);
        if !self.profile_policy.is_active_for_disclosure(name, source) {
            return false;
        }
        if self.is_disabled_at_runtime(name) {
            return false;
        }
        true
    }

    /// Whether a tool is active under an arbitrary profile policy, still
    /// honoring the session/global disable lists.
    ///
    /// Used when evaluating a profile other than the service's own (for
    /// example, a subagent profile).
    pub fn is_tool_active_for_profile(
        &self,
        policy: &ToolActivationPolicy,
        name: &str,
        source: Option<ToolSource>,
    ) -> bool {
        let source = source.unwrap_or(ToolSource::Builtin);
        if !policy.is_active(name, source) {
            return false;
        }
        if self.is_disabled_at_runtime(name) {
            return false;
        }
        true
    }

    /// Replace the session-scoped disable list.
    pub fn set_session_disabled_tools(&mut self, names: Vec<String>) {
        self.session_disabled = names;
    }

    /// Replace the global-scoped disable list.
    pub fn set_global_disabled_tools(&mut self, names: Vec<String>) {
        self.global_disabled = names;
    }

    /// Whether a tool is disabled by the session or global runtime lists.
    fn is_disabled_at_runtime(&self, name: &str) -> bool {
        self.session_disabled.iter().any(|entry| entry == name)
            || self.global_disabled.iter().any(|entry| entry == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ToolActivationPolicy {
        ToolActivationPolicy::default()
    }

    #[test]
    fn should_be_active_when_no_lists_configured() {
        // Given a policy with no allowlist and no denylist
        let p = policy();
        // When checking an arbitrary tool
        let active = p.is_active("read", ToolSource::Builtin);
        // Then it is active (open by default)
        assert!(active);
    }

    #[test]
    fn should_be_inactive_when_tool_is_in_disabled_list() {
        // Given a policy that disables "bash"
        let p = ToolActivationPolicy {
            disabled_tools: vec!["bash".to_string()],
            ..policy()
        };
        // When checking "bash"
        let active = p.is_active("bash", ToolSource::Builtin);
        // Then it is inactive
        assert!(!active);
    }

    #[test]
    fn should_be_active_for_other_tools_when_one_is_disabled() {
        // Given a policy that disables "bash"
        let p = ToolActivationPolicy {
            disabled_tools: vec!["bash".to_string()],
            ..policy()
        };
        // When checking "read"
        let active = p.is_active("read", ToolSource::Builtin);
        // Then "read" is still active
        assert!(active);
    }

    #[test]
    fn should_be_inactive_when_tool_not_in_allowlist() {
        // Given a policy with an explicit allowlist of ["read", "write"]
        let p = ToolActivationPolicy {
            enabled_tools: Some(vec!["read".to_string(), "write".to_string()]),
            ..policy()
        };
        // When checking a tool not in the allowlist
        let active = p.is_active("bash", ToolSource::Builtin);
        // Then it is inactive
        assert!(!active);
    }

    #[test]
    fn should_be_active_when_tool_is_in_allowlist() {
        // Given a policy with an explicit allowlist of ["read", "write"]
        let p = ToolActivationPolicy {
            enabled_tools: Some(vec!["read".to_string(), "write".to_string()]),
            ..policy()
        };
        // When checking "read"
        let active = p.is_active("read", ToolSource::Builtin);
        // Then it is active
        assert!(active);
    }

    #[test]
    fn should_be_inactive_when_tool_is_in_both_allowlist_and_disabled_list() {
        // Given a policy where "read" is allowlisted but also disabled
        let p = ToolActivationPolicy {
            enabled_tools: Some(vec!["read".to_string()]),
            disabled_tools: vec!["read".to_string()],
            ..policy()
        };
        // When checking "read"
        let active = p.is_active("read", ToolSource::Builtin);
        // Then the denylist wins and it is inactive
        assert!(!active);
    }

    #[test]
    fn should_be_inactive_for_disclosure_when_tool_is_deferred() {
        // Given a policy where "expensive_tool" is deferred
        let p = ToolActivationPolicy {
            deferred: vec!["expensive_tool".to_string()],
            ..policy()
        };
        // When checking disclosure for the deferred tool
        let disclosed = p.is_active_for_disclosure("expensive_tool", ToolSource::Builtin);
        // Then it is not disclosed
        assert!(!disclosed);
    }

    #[test]
    fn should_still_be_runtime_active_when_tool_is_deferred() {
        // Given a policy where "expensive_tool" is deferred (but not disabled)
        let p = ToolActivationPolicy {
            deferred: vec!["expensive_tool".to_string()],
            ..policy()
        };
        // When checking runtime activation for the deferred tool
        let active = p.is_active("expensive_tool", ToolSource::Builtin);
        // Then it may still run (deferred only suppresses disclosure)
        assert!(active);
    }

    #[test]
    fn should_be_inactive_for_disclosure_when_tool_is_disabled() {
        // Given a policy where "bash" is disabled
        let p = ToolActivationPolicy {
            disabled_tools: vec!["bash".to_string()],
            ..policy()
        };
        // When checking disclosure for "bash"
        let disclosed = p.is_active_for_disclosure("bash", ToolSource::Builtin);
        // Then it is not disclosed
        assert!(!disclosed);
    }

    #[test]
    fn should_be_inactive_when_tool_is_in_session_disabled_list() {
        // Given a service with an open profile policy but a session disable on "bash"
        let mut svc = AgentToolPolicyService::new(policy());
        svc.set_session_disabled_tools(vec!["bash".to_string()]);
        // When checking "bash" through the service
        let active = svc.is_tool_active("bash", None);
        // Then the session disable wins and it is inactive
        assert!(!active);
    }

    #[test]
    fn should_be_inactive_when_tool_is_in_global_disabled_list() {
        // Given a service with a global disable on "grep"
        let mut svc = AgentToolPolicyService::new(policy());
        svc.set_global_disabled_tools(vec!["grep".to_string()]);
        // When checking "grep" through the service
        let active = svc.is_tool_active("grep", None);
        // Then the global disable wins and it is inactive
        assert!(!active);
    }

    #[test]
    fn should_be_active_when_no_layer_disables_the_tool() {
        // Given a service whose profile allows everything and no runtime disables
        let svc = AgentToolPolicyService::new(policy());
        // When checking "read"
        let active = svc.is_tool_active("read", Some(ToolSource::Mcp));
        // Then it is active
        assert!(active);
    }

    #[test]
    fn should_suppress_disclosure_when_profile_defers_the_tool() {
        // Given a service whose profile defers "expensive_tool"
        let svc = AgentToolPolicyService::new(ToolActivationPolicy {
            deferred: vec!["expensive_tool".to_string()],
            ..policy()
        });
        // When checking disclosure for "expensive_tool"
        let disclosed = svc.is_tool_active_for_disclosure("expensive_tool", None);
        // Then it is not disclosed
        assert!(!disclosed);
    }

    #[test]
    fn should_honor_external_profile_policy_when_checking_tool_activation() {
        // Given a service with an open profile and a session disable on "bash"
        let mut svc = AgentToolPolicyService::new(policy());
        svc.set_session_disabled_tools(vec!["bash".to_string()]);
        // And an external profile policy that allowlists only "read"
        let external = ToolActivationPolicy {
            enabled_tools: Some(vec!["read".to_string()]),
            ..policy()
        };
        // When checking "read" against the external profile
        let read_active = svc.is_tool_active_for_profile(&external, "read", None);
        // And checking "bash" (allowlisted? no) against the external profile
        let bash_active = svc.is_tool_active_for_profile(&external, "bash", None);
        // Then "read" is active and "bash" is not (absent from allowlist)
        assert!(read_active);
        assert!(!bash_active);
    }

    #[test]
    fn should_respect_session_disable_when_evaluating_external_profile_policy() {
        // Given a service with a session disable on "write"
        let mut svc = AgentToolPolicyService::new(policy());
        svc.set_session_disabled_tools(vec!["write".to_string()]);
        // And an external profile that allowlists "write"
        let external = ToolActivationPolicy {
            enabled_tools: Some(vec!["write".to_string()]),
            ..policy()
        };
        // When checking "write" against the external profile
        let active = svc.is_tool_active_for_profile(&external, "write", None);
        // Then the session disable still wins
        assert!(!active);
    }

    #[test]
    fn should_display_as_lowercase_tag_when_formatting_tool_source() {
        // Given the three sources
        // When displaying them
        // Then they render as lowercase serde tags
        assert_eq!(ToolSource::Builtin.to_string(), "builtin");
        assert_eq!(ToolSource::User.to_string(), "user");
        assert_eq!(ToolSource::Mcp.to_string(), "mcp");
    }
}
