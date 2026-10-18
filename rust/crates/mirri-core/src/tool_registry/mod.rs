//! Tool registry — manages builtin, user, and MCP tool registrations.
//!
//! The [`ToolManager`] holds three independent maps:
//! - `builtin_tools` — constructed conditionally (core tools like Read, Write, Bash)
//! - `user_tools` — RPC-backed tools registered by the host
//! - `mcp_tools` — server-qualified tools from MCP servers
//!
//! The tool table is precomputed eagerly on every mutation and cached in
//! [`ToolManager::cached_table`]. [`ToolManager::loop_tools`] returns a clone
//! of this cache, avoiding per-step sort + dedup + filter-map. Every
//! mutating method MUST call [`ToolManager::recompute_table`] to keep the
//! cache consistent.

pub mod arg_converter;
pub mod args_validator;
pub mod execution;
pub mod scheduler;

// Re-export key types and functions for convenience.
pub use args_validator::{ToolArgsValidator, ValidatorCache};
pub use execution::{
    BlockedToolCall, DispatchedToolCall, PreflightResult, PrepareResult, PreparedToolCall,
    RejectedToolCall, SyntheticToolCall, ValidatedToolCall, execute_tool_batch,
    preflight_tool_call, prepare_tool_call, run_tool_batch,
};
pub use scheduler::{PendingToolTask, schedule_and_execute};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::tool::{Tool, ToolDefinition};

/// An MCP tool entry (server-qualified).
///
/// MCP tools are registered with the server name they belong to, and carry a
/// `loaded` flag for progressive disclosure: only loaded MCP tools appear in
/// the tool table when disclosure is active.
#[derive(Clone)]
pub struct McpToolEntry {
    /// The MCP server that provides this tool.
    pub server: String,
    /// The tool implementation.
    pub tool: Arc<dyn Tool>,
    /// Whether this tool has been loaded into context via `select_tools`.
    pub loaded: bool,
}

/// One row of the user-tool management read face (`tool.listUserTools`):
/// the registration payload plus the tool's current visibility to the LLM.
#[derive(Clone, Debug)]
pub struct UserToolInfo {
    /// The registered name.
    pub name: String,
    /// The registered description.
    pub description: String,
    /// The registered JSON-schema parameters (the payload the host
    /// supplied at registration time).
    pub parameters: serde_json::Value,
    /// Whether the name is in the active set (the tool is LLM-visible).
    pub enabled: bool,
}

impl std::fmt::Debug for McpToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolEntry")
            .field("server", &self.server)
            .field("tool_name", &self.tool.name())
            .field("loaded", &self.loaded)
            .finish()
    }
}

/// The tool registry. Manages builtin, user, and MCP tools.
///
/// Tool lookup order is builtin → user → MCP (see [`ToolManager::find_tool`]).
/// The registry is re-read before every step via [`ToolManager::loop_tools`]
/// to produce the tool definition list for the LLM call.
///
/// `Clone` is the turn-entry snapshot: a turn does not hold the registry's
/// read guard for its duration (that guard was the deadlock half — see the
/// registration paths' `try_write` retry); it clones the table once and
/// runs on the frozen view, which is semantically what the guard already
/// provided (writers could not run under a turn-held read guard anyway).
#[derive(Clone)]
pub struct ToolManager {
    builtin_tools: HashMap<String, Arc<dyn Tool>>,
    user_tools: HashMap<String, Arc<dyn Tool>>,
    mcp_tools: HashMap<String, McpToolEntry>,
    /// The set of enabled tool names (from profile/config).
    enabled_tools: Vec<String>,
    /// Whether `enabled_tools` is an explicit whitelist (set via
    /// [`ToolManager::set_active_tools`], i.e. the
    /// `AgentScopeConfig.enabled_tools = Some(_)` arm) rather than the
    /// "all registered builtin/user tools" default filled in by
    /// [`ToolManager::enable_all_registered`]. When `true`,
    /// [`recompute_table`](Self::recompute_table) filters MCP tool names
    /// through the whitelist too, so a server that connects after the
    /// agent was created cannot leak tools past the whitelist (the btw
    /// fork's all-tools veto, F-P11a-2).
    enabled_tools_explicit: bool,
    /// Mode-gated builtin tools hidden from the LLM table (e.g. the
    /// collaboration tools, exposed only after the host enters
    /// collaboration mode). The tools stay registered in `builtin_tools`
    /// and resolvable via [`find_tool`](Self::find_tool) — this set only
    /// keeps them out of [`loop_tools`](Self::loop_tools), and it is
    /// authoritative even under an explicit whitelist (see
    /// [`recompute_table`](Self::recompute_table)).
    builtin_disabled: HashSet<String>,
    /// Whether progressive disclosure is active.
    disclosure_enabled: bool,
    /// The set of loaded MCP tool names (via `select_tools`).
    loaded_mcp_tools: Vec<String>,
    /// Precomputed tool table (rebuilt on every mutation).
    /// Avoids sort + dedup + filter_map on every `loop_tools()` call,
    /// which the design (§2.3, §4.4) identifies as a per-step hot path.
    cached_table: Vec<ToolDefinition>,
}

impl ToolManager {
    /// Create an empty tool manager.
    pub fn new() -> Self {
        Self {
            builtin_tools: HashMap::new(),
            user_tools: HashMap::new(),
            mcp_tools: HashMap::new(),
            enabled_tools: Vec::new(),
            enabled_tools_explicit: false,
            builtin_disabled: HashSet::new(),
            disclosure_enabled: false,
            loaded_mcp_tools: Vec::new(),
            cached_table: Vec::new(),
        }
    }

    /// Recompute the cached tool table from current state.
    ///
    /// Called eagerly at the end of every mutating setter so that
    /// [`loop_tools`](Self::loop_tools) is a cheap clone instead of a
    /// full sort + dedup + filter_map on every step.
    fn recompute_table(&mut self) {
        let mut names: Vec<String> = self.enabled_tools.clone();
        // Mode-gated builtin tools stay hidden even when the active set
        // names them (including an explicit whitelist): `builtin_disabled`
        // is authoritative for the LLM-visible table, so only the host's
        // mode toggle (`set_builtin_tool_enabled`) can re-expose them.
        names.retain(|name| !self.builtin_disabled.contains(name));
        let mcp_names: Vec<String> = if self.disclosure_enabled {
            self.mcp_tools
                .iter()
                .filter(|(_, entry)| entry.loaded)
                .map(|(name, _)| name.clone())
                .collect()
        } else {
            self.mcp_tools.keys().cloned().collect()
        };
        // An explicit whitelist is authoritative for MCP tools as well:
        // only whitelisted server tool names enter the table, so a server
        // that connects after the agent was created cannot widen a
        // restricted tool set (the btw fork's `enabled_tools = Some(vec![])`
        // must stay tool-less). Entries admit by exact name or by the
        // `mcp__*` / `mcp__<server>__*` glob forms the profile wire uses
        // (TS v2 parity). Without a whitelist (the
        // `enable_all_registered` default), MCP tools are admitted
        // alongside the registered builtin/user tools as before.
        if self.enabled_tools_explicit {
            names.extend(mcp_names.into_iter().filter(|name| {
                self.enabled_tools
                    .iter()
                    .any(|enabled| whitelist_entry_matches(enabled, name))
            }));
        } else {
            names.extend(mcp_names);
        }
        if self.disclosure_enabled {
            names.push("select_tools".to_string());
        }
        names.sort();
        names.dedup();
        self.cached_table = names
            .iter()
            .filter_map(|name| {
                self.find_tool(name).map(|tool| {
                    let mut def = ToolDefinition::from(tool.as_ref());
                    if self.disclosure_enabled && self.mcp_tools.contains_key(name) {
                        def.deferred = true;
                    }
                    def
                })
            })
            .collect();
    }

    /// Register a builtin tool.
    pub fn register_builtin(&mut self, tool: Arc<dyn Tool>) {
        self.builtin_tools.insert(tool.name().to_string(), tool);
        self.recompute_table();
    }

    /// Register a user tool (RPC-backed).
    ///
    /// TS `profile.addActiveTool` parity: under the unrestricted default
    /// ([`enable_all_registered`](Self::enable_all_registered)) the new
    /// name enters the active set immediately, so the tool is visible to
    /// the LLM without an explicit opt-in (F22). Under an explicit
    /// whitelist the name stays out — the user pinned the visible set and
    /// a late registration must not widen it.
    pub fn register_user_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.user_tools.insert(name.clone(), tool);
        if !self.enabled_tools_explicit && !self.enabled_tools.contains(&name) {
            self.enabled_tools.push(name);
        }
        self.recompute_table();
    }

    /// Enable or disable a registered user tool by name (the management
    /// write face behind `tool.setUserToolEnabled`).
    ///
    /// Enabling adds the name to the active set (a no-op in the
    /// unrestricted default, an explicit widening under a whitelist);
    /// disabling removes it — the tool stays registered in `user_tools`
    /// and resolvable via [`find_tool`](Self::find_tool), it just leaves
    /// the LLM-visible table. Returns whether the tool is registered; an
    /// unknown name changes nothing.
    pub fn set_user_tool_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if !self.user_tools.contains_key(name) {
            return false;
        }
        if enabled {
            if !self.enabled_tools.iter().any(|n| n == name) {
                self.enabled_tools.push(name.to_string());
            }
        } else {
            self.enabled_tools.retain(|n| n != name);
        }
        self.recompute_table();
        true
    }

    /// Enable or disable a registered builtin tool by name (the write face
    /// behind mode-gated capabilities like `collaboration.enter` /
    /// `collaboration.exit`).
    ///
    /// Mirrors [`set_user_tool_enabled`](Self::set_user_tool_enabled):
    /// disabling adds the name to [`builtin_disabled`] and removes it from
    /// the active set; enabling removes it from [`builtin_disabled`] and
    /// adds it to the active set. Either way the tool stays registered in
    /// `builtin_tools` and resolvable via [`find_tool`](Self::find_tool).
    /// Returns whether the tool is registered; an unknown name changes
    /// nothing.
    pub fn set_builtin_tool_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if !self.builtin_tools.contains_key(name) {
            return false;
        }
        if enabled {
            self.builtin_disabled.remove(name);
            if !self.enabled_tools.iter().any(|n| n == name) {
                self.enabled_tools.push(name.to_string());
                // Keep the active set sorted: `enabled_tools()` feeds the
                // ToolsDiffInjector announcement text, and a mid-session
                // toggle would otherwise announce the re-enabled tools in
                // insertion order while the initial announcement is sorted.
                self.enabled_tools.sort();
            }
        } else {
            self.builtin_disabled.insert(name.to_string());
            self.enabled_tools.retain(|n| n != name);
        }
        self.recompute_table();
        true
    }

    /// Remove a user-registered tool by name. Returns `true` if removed.
    pub fn remove_user_tool(&mut self, name: &str) -> bool {
        if self.user_tools.remove(name).is_some() {
            // Also remove from enabled_tools if present.
            self.enabled_tools.retain(|n| n != name);
            self.recompute_table();
            true
        } else {
            false
        }
    }

    /// Register an MCP tool.
    pub fn register_mcp_tool(&mut self, server: String, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.mcp_tools.insert(
            name,
            McpToolEntry {
                server,
                tool,
                loaded: false,
            },
        );
        self.recompute_table();
    }

    /// Remove all MCP tools belonging to the given server.
    ///
    /// Returns the number of tool entries that were actually removed.
    pub fn remove_mcp_tools_by_server(&mut self, server: &str) -> usize {
        let before = self.mcp_tools.len();
        self.mcp_tools.retain(|_, entry| entry.server != server);
        let removed = before - self.mcp_tools.len();
        if removed > 0 {
            self.recompute_table();
        }
        removed
    }

    /// Set the enabled tools list (from profile/config).
    ///
    /// Marks the list as an explicit whitelist: MCP tool names not in the
    /// list are kept out of the tool table (see
    /// [`recompute_table`](Self::recompute_table)).
    pub fn set_active_tools(&mut self, tools: Vec<String>) {
        self.enabled_tools = tools;
        self.enabled_tools_explicit = true;
        self.recompute_table();
    }

    /// Enable all registered builtin and user tools.
    ///
    /// Called when no explicit `enabled_tools` list is provided (i.e. the
    /// profile/config sets it to `None`), meaning "all registered tools are
    /// enabled". MCP tools are not included here because they are handled
    /// separately by [`recompute_table`](Self::recompute_table) — the
    /// whitelist flag stays off so late MCP registrations are admitted.
    pub fn enable_all_registered(&mut self) {
        // Mode-gated builtins (builtin_disabled) stay out of the active
        // set: "all registered tools" means all tools the host has not
        // explicitly mode-gated away.
        let mut names: Vec<String> = self
            .builtin_tools
            .keys()
            .filter(|name| !self.builtin_disabled.contains(*name))
            .cloned()
            .collect();
        names.extend(self.user_tools.keys().cloned());
        // Deterministic order: `enabled_tools()` feeds the ToolsDiffInjector
        // announcement text, so an unsorted HashMap iteration order would
        // change the injected message across processes and thrash the
        // provider-side prompt cache.
        names.sort();
        self.enabled_tools = names;
        self.enabled_tools_explicit = false;
        self.recompute_table();
    }

    /// Whether `name` is an actively enabled tool.
    ///
    /// Both activation paths — the explicit whitelist
    /// ([`set_active_tools`](Self::set_active_tools)) and the full set
    /// ([`enable_all_registered`](Self::enable_all_registered)) — populate
    /// `enabled_tools`, so `contains` is the activation verdict. The Agent
    /// tool's background gate (B1 D-50) reads this to decide whether
    /// `run_in_background` is available (TS parity: background dispatch
    /// requires TaskList + TaskOutput + TaskStop to all be active).
    pub fn is_tool_active(&self, name: &str) -> bool {
        self.enabled_tools.iter().any(|enabled| enabled == name)
    }

    /// Set disclosure mode.
    pub fn set_disclosure(&mut self, enabled: bool) {
        self.disclosure_enabled = enabled;
        self.recompute_table();
    }

    /// Mark an MCP tool as loaded (via `select_tools`).
    pub fn load_mcp_tool(&mut self, name: &str) {
        if let Some(entry) = self.mcp_tools.get_mut(name) {
            entry.loaded = true;
        }
        if !self.loaded_mcp_tools.contains(&name.to_string()) {
            self.loaded_mcp_tools.push(name.to_string());
        }
        self.recompute_table();
    }

    /// Find a tool by name (builtin → user → MCP).
    pub fn find_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.builtin_tools.get(name) {
            return Some(Arc::clone(tool));
        }
        if let Some(tool) = self.user_tools.get(name) {
            return Some(Arc::clone(tool));
        }
        if let Some(entry) = self.mcp_tools.get(name) {
            return Some(Arc::clone(&entry.tool));
        }
        None
    }

    /// Get the tool definitions for the LLM call.
    ///
    /// Returns a clone of the precomputed table. The table is rebuilt eagerly
    /// on every mutation (register/unregister/set_active_tools/set_disclosure/
    /// load_mcp_tool), so this is O(n) clone with no sorting or filtering.
    ///
    /// See [`ToolManager::recompute_table`] for the build logic.
    pub fn loop_tools(&self) -> Vec<ToolDefinition> {
        self.cached_table.clone()
    }

    /// List all registered tool names (for debugging/inspection).
    pub fn all_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.builtin_tools.keys().cloned().collect();
        names.extend(self.user_tools.keys().cloned());
        names.extend(self.mcp_tools.keys().cloned());
        names.sort();
        names
    }

    /// The user-tool management read face (`tool.listUserTools`): every
    /// registered user tool with its registration payload and current
    /// visibility to the LLM (whether the name is in the active set).
    /// Sorted by name for determinism, like the visible table itself.
    pub fn user_tools_info(&self) -> Vec<UserToolInfo> {
        let mut infos: Vec<UserToolInfo> = self
            .user_tools
            .iter()
            .map(|(name, tool)| UserToolInfo {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
                enabled: self.enabled_tools.iter().any(|n| n == name),
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Every registered user tool's Arc, for the subagent inheritance face
    /// (B4 T5, D-47 — TS `inheritUserTools`): the caller re-registers these
    /// handles on the child's ToolManager before the child's turn starts.
    pub fn user_tools_snapshot(&self) -> Vec<Arc<dyn crate::tool::Tool>> {
        self.user_tools.values().cloned().collect()
    }

    /// Every registered MCP tool's Arc, for the lazy subagent hydration face
    /// (ADR-006/ADR-017): MCP tools reach agents only through the engine's
    /// live fan-out sites, which iterate LIVE agents — a dormant child
    /// misses every registration that lands between the resume and its
    /// hydration. The hydrator re-registers the parent's CURRENT set (the
    /// entry's `server` tag re-keys the same qualified name
    /// [`register_mcp_tool`](Self::register_mcp_tool) uses), so the woken
    /// child sees the same effective MCP surface a live spawn holds.
    pub fn mcp_tools_snapshot(&self) -> Vec<(String, Arc<dyn crate::tool::Tool>)> {
        self.mcp_tools
            .iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(&entry.tool)))
            .collect()
    }

    /// Get the number of builtin tools.
    pub fn builtin_count(&self) -> usize {
        self.builtin_tools.len()
    }

    /// Get the number of user tools.
    pub fn user_count(&self) -> usize {
        self.user_tools.len()
    }

    /// Get the number of MCP tools.
    pub fn mcp_count(&self) -> usize {
        self.mcp_tools.len()
    }

    /// Get a reference to the enabled tools list.
    pub fn enabled_tools(&self) -> &[String] {
        &self.enabled_tools
    }

    /// Whether progressive disclosure is active.
    pub fn disclosure_enabled(&self) -> bool {
        self.disclosure_enabled
    }

    /// The loadable dynamic (MCP) tool names — the `<tools_added>`
    /// announcement scope (TS `loadableDynamicToolNames` parity, ADR-3-R2):
    /// enabled MCP tools whether loaded or not, whitelist-admitted,
    /// sorted. Only meaningful in disclosure mode — otherwise every MCP
    /// tool is a static-table member with its full definition in each
    /// request, and nothing is announced at all.
    pub fn loadable_dynamic_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.mcp_tools.keys().cloned().collect();
        if self.enabled_tools_explicit {
            names.retain(|name| {
                self.enabled_tools
                    .iter()
                    .any(|enabled| whitelist_entry_matches(enabled, name))
            });
        }
        names.sort();
        names
    }
}

impl Default for ToolManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether one explicit-whitelist entry admits `tool_name`.
///
/// Mirrors the TS v2 activation policy (`agent-core-v2`'s `isToolActive`):
/// an exact entry matches any tool by name; a trailing-`*` glob entry admits
/// only names its prefix covers. Glob patterns are `mcp__`-anchored by
/// construction — `mcp__*` (every server) and `mcp__<server>__*` (one
/// server) are the only pattern families the profile wire uses — so a
/// pattern can never widen a builtin tool's visibility.
fn whitelist_entry_matches(entry: &str, tool_name: &str) -> bool {
    if entry == tool_name {
        return true;
    }
    let Some(prefix) = entry.strip_suffix('*') else {
        return false;
    };
    prefix.starts_with("mcp__") && tool_name.starts_with(prefix)
}

impl std::fmt::Debug for ToolManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolManager")
            .field("builtin_count", &self.builtin_tools.len())
            .field("user_count", &self.user_tools.len())
            .field("mcp_count", &self.mcp_tools.len())
            .field("enabled_tools", &self.enabled_tools)
            .field("disclosure_enabled", &self.disclosure_enabled)
            .field("loaded_mcp_tools", &self.loaded_mcp_tools)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::error::EngineResult;
    use crate::tool::ToolExecution;

    /// A minimal named tool for whitelist tests — registered only, never
    /// executed.
    struct StubTool {
        name: String,
        parameters: serde_json::Value,
    }

    impl StubTool {
        #[allow(clippy::new_ret_no_self)] // a test helper handing out trait objects
        fn new(name: &str) -> Arc<dyn Tool> {
            Arc::new(Self {
                name: name.to_string(),
                parameters: serde_json::json!({}),
            })
        }
    }

    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "stub tool for whitelist tests"
        }

        fn parameters(&self) -> &serde_json::Value {
            &self.parameters
        }

        fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
            unreachable!("the whitelist tests never execute the stub tool")
        }
    }

    fn loop_names(manager: &ToolManager) -> Vec<String> {
        manager
            .loop_tools()
            .into_iter()
            .map(|def| def.name)
            .collect()
    }

    fn manager_with_tools() -> ToolManager {
        let mut manager = ToolManager::new();
        manager.register_builtin(StubTool::new("Read"));
        manager.register_mcp_tool(
            "github".to_string(),
            StubTool::new("mcp__github__get_issue"),
        );
        manager.register_mcp_tool("slack".to_string(), StubTool::new("mcp__slack__post"));
        manager
    }

    /// Given an explicit whitelist carrying the every-server MCP glob,
    /// When MCP tools from several servers are registered,
    /// Then every server's tools are admitted alongside the exact names.
    #[test]
    fn should_admit_every_mcp_tool_when_the_whitelist_carries_the_mcp_glob() {
        let mut manager = manager_with_tools();
        manager.set_active_tools(vec!["Read".to_string(), "mcp__*".to_string()]);

        let names = loop_names(&manager);

        assert!(names.contains(&"Read".to_string()));
        assert!(names.contains(&"mcp__github__get_issue".to_string()));
        assert!(names.contains(&"mcp__slack__post".to_string()));
    }

    /// Given an explicit whitelist naming one server's glob,
    /// When MCP tools from several servers are registered,
    /// Then only the named server's tools are admitted.
    #[test]
    fn should_admit_only_the_named_server_tools_when_the_whitelist_scopes_one_server() {
        let mut manager = manager_with_tools();
        manager.set_active_tools(vec!["Read".to_string(), "mcp__github__*".to_string()]);

        let names = loop_names(&manager);

        assert!(names.contains(&"mcp__github__get_issue".to_string()));
        assert!(!names.contains(&"mcp__slack__post".to_string()));
    }

    /// Given an explicit whitelist with no MCP entry at all,
    /// When MCP tools are registered,
    /// Then they stay out of the tool table (a late server connect cannot
    /// widen a restricted set).
    #[test]
    fn should_exclude_mcp_tools_when_the_whitelist_has_no_mcp_entry() {
        let mut manager = manager_with_tools();
        manager.set_active_tools(vec!["Read".to_string()]);

        let names = loop_names(&manager);

        assert!(names.contains(&"Read".to_string()));
        assert!(!names.contains(&"mcp__github__get_issue".to_string()));
        assert!(!names.contains(&"mcp__slack__post".to_string()));
    }

    /// Given an explicit whitelist naming one MCP tool by its full name,
    /// When that tool and others are registered,
    /// Then exactly the named tool is admitted (the exact form still works).
    #[test]
    fn should_admit_an_exact_mcp_tool_entry_by_full_name() {
        let mut manager = manager_with_tools();
        manager.set_active_tools(vec!["mcp__github__get_issue".to_string()]);

        let names = loop_names(&manager);

        assert!(names.contains(&"mcp__github__get_issue".to_string()));
        assert!(!names.contains(&"mcp__slack__post".to_string()));
    }

    /// Given a mode-gated builtin registered disabled and an explicit
    /// whitelist that does not name it,
    /// When the host toggles the mode on,
    /// Then the tool enters the table — the host's mode gate is authoritative
    /// and the profile whitelist does not fight it.
    #[test]
    fn should_expose_a_mode_gated_tool_when_the_host_enables_it_under_an_explicit_whitelist() {
        let mut manager = ToolManager::new();
        manager.register_builtin(StubTool::new("CollaborationStart"));
        manager.set_builtin_tool_enabled("CollaborationStart", false);
        manager.set_active_tools(vec!["Read".to_string()]);
        assert!(
            !loop_names(&manager).contains(&"CollaborationStart".to_string()),
            "the mode-gated tool stays hidden while its mode is off"
        );

        manager.set_builtin_tool_enabled("CollaborationStart", true);

        assert!(
            loop_names(&manager).contains(&"CollaborationStart".to_string()),
            "the host's mode toggle widens the table past the whitelist"
        );
    }

    /// Given a glob whitelist entry, When matching non-MCP tool names,
    /// Then it never admits them (patterns are MCP-anchored, builtin
    /// visibility stays exact-name only).
    #[test]
    fn should_never_admit_a_builtin_tool_through_a_glob_pattern() {
        let mut manager = manager_with_tools();
        // A glob entry that would prefix-match nothing; every builtin stays
        // visible only through exact entries.
        manager.set_active_tools(vec!["Rea*".to_string()]);

        assert!(!loop_names(&manager).contains(&"Read".to_string()));
    }
}
