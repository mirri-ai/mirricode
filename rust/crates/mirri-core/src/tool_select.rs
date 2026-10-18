//! `tool_select` — progressive tool disclosure and selection announcements.
//!
//! Ports the TypeScript `toolSelect` domain. Not every tool needs to be
//! advertised to the model on every turn. [`ToolSelectService`] implements a
//! three-mode disclosure policy:
//!
//! - [`DisclosureState::All`] — every tool is disclosed (default).
//! - [`DisclosureState::Deferred`] — deferred tools are withheld until
//!   explicitly loaded via [`ToolSelectService::load_tool`].
//! - [`DisclosureState::Selective`] — only explicitly loaded tools are
//!   disclosed.
//!
//! The service also shapes the tool list presented to the model so recently
//! used tools surface first, and it can emit a reminder naming the deferred
//! tools that remain available to load.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Progressive disclosure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DisclosureState {
    /// Disclose every known tool.
    #[default]
    All,
    /// Disclose all tools except deferred ones (until loaded).
    Deferred,
    /// Disclose only explicitly loaded tools.
    Selective,
}

impl DisclosureState {
    fn label(self) -> &'static str {
        match self {
            DisclosureState::All => "all",
            DisclosureState::Deferred => "deferred",
            DisclosureState::Selective => "selective",
        }
    }
}

impl fmt::Display for DisclosureState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Configuration for the tool selection service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectConfig {
    /// Active disclosure mode.
    pub disclosure: DisclosureState,
    /// Tools withheld from disclosure until loaded.
    pub deferred_tools: Vec<String>,
    /// Tools that have been explicitly loaded and are now disclosable.
    pub loaded_tools: Vec<String>,
}

/// Progressive tool disclosure and selection service.
pub struct ToolSelectService {
    config: ToolSelectConfig,
    /// Tool names in order of use (most recent at the end). Each tool appears
    /// at most once; reusing a tool moves it to the end.
    history: Vec<String>,
}

impl Default for ToolSelectService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSelectService {
    /// Create a service in [`DisclosureState::All`] with empty lists.
    pub fn new() -> Self {
        Self {
            config: ToolSelectConfig::default(),
            history: Vec::new(),
        }
    }

    /// Set the active disclosure mode.
    pub fn set_disclosure(&mut self, state: DisclosureState) {
        self.config.disclosure = state;
    }

    /// Whether any progressive disclosure is in effect.
    ///
    /// Returns `true` when the mode is not [`DisclosureState::All`].
    pub fn is_disclosure_enabled(&self) -> bool {
        self.config.disclosure != DisclosureState::All
    }

    /// Register a tool as deferred (withheld from disclosure until loaded).
    ///
    /// Idempotent — registering the same name twice has no extra effect.
    pub fn register_deferred(&mut self, name: String) {
        if !self.config.deferred_tools.iter().any(|d| d == &name) {
            self.config.deferred_tools.push(name);
        }
    }

    /// Load a deferred tool, moving it from the deferred set to the loaded set.
    ///
    /// Returns `true` if the tool was deferred and is now loaded, `false` if
    /// the tool was not registered as deferred.
    pub fn load_tool(&mut self, name: &str) -> bool {
        if let Some(position) = self.config.deferred_tools.iter().position(|d| d == name) {
            self.config.deferred_tools.remove(position);
            if !self.config.loaded_tools.iter().any(|l| l == name) {
                self.config.loaded_tools.push(name.to_string());
            }
            true
        } else {
            false
        }
    }

    /// Whether a tool is currently deferred (withheld from disclosure).
    pub fn is_deferred(&self, name: &str) -> bool {
        self.config.deferred_tools.iter().any(|d| d == name)
    }

    /// Whether a tool has been explicitly loaded.
    pub fn is_loaded(&self, name: &str) -> bool {
        self.config.loaded_tools.iter().any(|l| l == name)
    }

    /// Record that a tool was used, for history-based shaping.
    ///
    /// Each tool appears at most once in the history; reusing a tool moves it
    /// to the most-recent position.
    pub fn record_usage(&mut self, name: &str) {
        self.history.retain(|used| used != name);
        self.history.push(name.to_string());
    }

    /// Filter the full tool list down to what should be disclosed.
    ///
    /// - [`DisclosureState::All`]: every tool is returned.
    /// - [`DisclosureState::Deferred`]: non-deferred tools plus loaded tools.
    /// - [`DisclosureState::Selective`]: only loaded tools.
    pub fn filter_tools(&self, all_tools: &[String]) -> Vec<String> {
        match self.config.disclosure {
            DisclosureState::All => all_tools.to_vec(),
            DisclosureState::Deferred => all_tools
                .iter()
                .filter(|name| !self.is_deferred(name) || self.is_loaded(name))
                .cloned()
                .collect(),
            DisclosureState::Selective => all_tools
                .iter()
                .filter(|name| self.is_loaded(name))
                .cloned()
                .collect(),
        }
    }

    /// Build a reminder naming the deferred tools that can still be loaded.
    ///
    /// Returns `None` when no deferred tools remain available.
    pub fn build_announcement(&self) -> Option<String> {
        let mut available: Vec<&str> = self
            .config
            .deferred_tools
            .iter()
            .filter(|name| {
                !self
                    .config
                    .loaded_tools
                    .iter()
                    .any(|loaded| loaded == *name)
            })
            .map(String::as_str)
            .collect();
        if available.is_empty() {
            return None;
        }
        available.sort();
        Some(format!(
            "Additional tools available via select_tools: {}",
            available.join(", ")
        ))
    }

    /// Order a set of tools by recency of use, then alphabetically.
    ///
    /// Recently used tools come first (most recent first); the remaining tools
    /// follow in alphabetical order.
    pub fn shape_history(&self, available: &[String]) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        // Most recently used first: walk history in reverse.
        for used in self.history.iter().rev() {
            if available.iter().any(|name| name == used)
                && !result.iter().any(|already| already == used)
            {
                result.push(used.clone());
            }
        }
        // Remaining tools, alphabetical.
        let mut rest: Vec<&String> = available
            .iter()
            .filter(|name| !result.iter().any(|already| already == *name))
            .collect();
        rest.sort();
        for name in rest {
            result.push(name.clone());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_default_to_all_disclosure_when_created_with_default_config() {
        // Given a fresh service
        let svc = ToolSelectService::new();
        // When inspecting its state
        // Then disclosure is All and disclosure is not "enabled"
        assert!(!svc.is_disclosure_enabled());
        assert!(!svc.is_deferred("anything"));
    }

    #[test]
    fn should_enable_disclosure_when_state_is_not_all() {
        // Given a service switched to Deferred
        let mut svc = ToolSelectService::new();
        svc.set_disclosure(DisclosureState::Deferred);
        // When checking whether disclosure is enabled
        let enabled = svc.is_disclosure_enabled();
        // Then it is enabled
        assert!(enabled);
    }

    #[test]
    fn should_return_all_tools_when_disclosure_is_all() {
        // Given a service in All mode with a deferred tool
        let mut svc = ToolSelectService::new();
        svc.register_deferred("expensive".to_string());
        // When filtering the full tool list
        let filtered = svc.filter_tools(&[
            "read".to_string(),
            "expensive".to_string(),
            "write".to_string(),
        ]);
        // Then every tool is returned regardless of deferred status
        assert_eq!(filtered, vec!["read", "expensive", "write"]);
    }

    #[test]
    fn should_withhold_deferred_tools_when_disclosure_is_deferred() {
        // Given a service in Deferred mode with "expensive" deferred
        let mut svc = ToolSelectService::new();
        svc.set_disclosure(DisclosureState::Deferred);
        svc.register_deferred("expensive".to_string());
        // When filtering the full tool list
        let filtered = svc.filter_tools(&[
            "read".to_string(),
            "expensive".to_string(),
            "write".to_string(),
        ]);
        // Then "expensive" is withheld but the others are returned
        assert_eq!(filtered, vec!["read", "write"]);
    }

    #[test]
    fn should_disclose_loaded_deferred_tools_when_disclosure_is_deferred() {
        // Given a service in Deferred mode where "expensive" is deferred then loaded
        let mut svc = ToolSelectService::new();
        svc.set_disclosure(DisclosureState::Deferred);
        svc.register_deferred("expensive".to_string());
        assert!(svc.load_tool("expensive"));
        // When filtering the full tool list
        let filtered = svc.filter_tools(&["read".to_string(), "expensive".to_string()]);
        // Then "expensive" is disclosed again because it was loaded
        assert_eq!(filtered, vec!["read", "expensive"]);
    }

    #[test]
    fn should_disclose_only_loaded_tools_when_disclosure_is_selective() {
        // Given a service in Selective mode with "read" loaded and "write" not
        let mut svc = ToolSelectService::new();
        svc.set_disclosure(DisclosureState::Selective);
        svc.register_deferred("read".to_string());
        svc.register_deferred("write".to_string());
        assert!(svc.load_tool("read"));
        // When filtering the full tool list
        let filtered = svc.filter_tools(&["read".to_string(), "write".to_string()]);
        // Then only "read" is disclosed
        assert_eq!(filtered, vec!["read"]);
    }

    #[test]
    fn should_return_false_when_loading_a_tool_that_is_not_deferred() {
        // Given a service with no deferred tools
        let mut svc = ToolSelectService::new();
        // When loading a tool that was never deferred
        let loaded = svc.load_tool("unknown");
        // Then it returns false and nothing is loaded
        assert!(!loaded);
        assert!(!svc.is_loaded("unknown"));
    }

    #[test]
    fn should_move_tool_out_of_deferred_set_when_loading_a_deferred_tool() {
        // Given a service with "expensive" deferred
        let mut svc = ToolSelectService::new();
        svc.register_deferred("expensive".to_string());
        // When loading "expensive"
        let loaded = svc.load_tool("expensive");
        // Then it is no longer deferred but is loaded
        assert!(loaded);
        assert!(!svc.is_deferred("expensive"));
        assert!(svc.is_loaded("expensive"));
    }

    #[test]
    fn should_register_deferred_tool_only_once_when_registered_twice() {
        // Given a service
        let mut svc = ToolSelectService::new();
        // When registering the same deferred tool twice
        svc.register_deferred("expensive".to_string());
        svc.register_deferred("expensive".to_string());
        // Then it is deferred exactly once
        assert_eq!(svc.config.deferred_tools.len(), 1);
    }

    #[test]
    fn should_name_available_deferred_tools_when_building_announcement() {
        // Given a service with two deferred tools, one of which is loaded
        let mut svc = ToolSelectService::new();
        svc.register_deferred("alpha".to_string());
        svc.register_deferred("beta".to_string());
        svc.register_deferred("gamma".to_string());
        svc.load_tool("beta");
        // When building the announcement
        let announcement = svc.build_announcement();
        // Then it names the remaining available deferred tools (sorted)
        assert_eq!(
            announcement,
            Some("Additional tools available via select_tools: alpha, gamma".to_string())
        );
    }

    #[test]
    fn should_return_none_when_no_deferred_tools_remain() {
        // Given a service with no deferred tools
        let svc = ToolSelectService::new();
        // When building the announcement
        let announcement = svc.build_announcement();
        // Then there is nothing to announce
        assert_eq!(announcement, None);
    }

    #[test]
    fn should_put_recently_used_tools_first_when_shaping_history() {
        // Given a service that has used "write" then "read" (most recent last)
        let mut svc = ToolSelectService::new();
        svc.record_usage("write");
        svc.record_usage("read");
        // When shaping the available tool list
        let ordered =
            svc.shape_history(&["bash".to_string(), "read".to_string(), "write".to_string()]);
        // Then "read" (most recent) comes first, then "write", then "bash" (alpha)
        assert_eq!(ordered, vec!["read", "write", "bash"]);
    }

    #[test]
    fn should_order_unused_tools_alphabetically_when_shaping_history() {
        // Given a service with no usage history
        let svc = ToolSelectService::new();
        // When shaping an available list
        let ordered = svc.shape_history(&[
            "zebra".to_string(),
            "alpha".to_string(),
            "mango".to_string(),
        ]);
        // Then the result is fully alphabetical
        assert_eq!(ordered, vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn should_move_reused_tool_to_most_recent_when_used_again() {
        // Given a service that used "read" then "write"
        let mut svc = ToolSelectService::new();
        svc.record_usage("read");
        svc.record_usage("write");
        // When "read" is used again
        svc.record_usage("read");
        // Then shaping puts "read" first (most recent)
        let ordered = svc.shape_history(&["read".to_string(), "write".to_string()]);
        assert_eq!(ordered, vec!["read", "write"]);
        // And "read" appears only once in history
        assert_eq!(
            svc.history.iter().filter(|h| h.as_str() == "read").count(),
            1
        );
    }

    #[test]
    fn should_display_as_lowercase_tag_when_formatting_disclosure_state() {
        // Given the three states
        // When displaying them
        // Then they render as lowercase serde tags
        assert_eq!(DisclosureState::All.to_string(), "all");
        assert_eq!(DisclosureState::Deferred.to_string(), "deferred");
        assert_eq!(DisclosureState::Selective.to_string(), "selective");
    }
}
