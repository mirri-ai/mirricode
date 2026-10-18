//! Capability registry and resolver — central mapping from capability tags
//! to tool names.
//!
//! Sources of capability declarations:
//! - Built-in tools declare capabilities via [`Tool::capabilities`](crate::tool::Tool::capabilities).
//! - `integrations.yaml` attaches capabilities to MCP tools by server name.
//!
//! The registry is a pure data structure. The resolver wraps it with
//! filesystem context (`home_dir`, `cwd`) for loading integration configs.
//!
//! Ported from the TypeScript `capability` domain
//! (`agent-core-v2/src/agent/capability/`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config types (mirror integrations.yaml schema)
// ---------------------------------------------------------------------------

/// One integration declaration in `integrations.yaml`.
///
/// The key in the parent [`IntegrationsConfig`] record is matched against an
/// MCP server name (see [`CapabilityRegistry::apply_integrations`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSpec {
    /// Capability tags this integration provides (e.g. `["code.explore"]`).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Tool names this integration's tools claim to be preferred over.
    /// Serialized as `preferOver` in YAML to match the TS schema.
    #[serde(default, rename = "preferOver")]
    pub prefer_over: Vec<String>,
}

/// Root schema for `.mirri-code/integrations.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationsConfig {
    /// Map of server name → integration spec.
    #[serde(default)]
    pub integrations: HashMap<String, IntegrationSpec>,
}

// ---------------------------------------------------------------------------
// Source enum
// ---------------------------------------------------------------------------

/// Whether a tool provider is built-in or sourced from an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A built-in tool shipped with the kernel.
    Builtin,
    /// A tool provided by a connected MCP server.
    Mcp,
}

impl Source {
    /// Returns the string representation matching the TS union
    /// `'builtin' | 'mcp'`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Mcp => "mcp",
        }
    }
}

// ---------------------------------------------------------------------------
// CapabilityProvider
// ---------------------------------------------------------------------------

/// A resolved provider for a capability, annotated with source and
/// `prefer_over` claims.
///
/// Returned by [`CapabilityRegistry::resolve_providers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityProvider {
    /// The tool name providing the capability.
    pub tool_name: String,
    /// Whether the tool is built-in or from an MCP server.
    pub source: Source,
    /// Tool names this provider claims to be preferred over (from
    /// `integrations.yaml` `preferOver` declarations).
    pub prefer_over: Vec<String>,
}

// ---------------------------------------------------------------------------
// CapabilityRegistry
// ---------------------------------------------------------------------------

/// Central mapping from capability tags to tool names.
///
/// Tracks four relationships:
/// - `providers`: capability id → ordered set of tool names providing it.
/// - `by_tool`: tool name → set of capability ids it declares.
/// - `prefer_over`: tool name → ordered set of tool names it declared
///   preference over (from `integrations.yaml`).
/// - `source_by_tool`: tool name → `Builtin` or `Mcp`.
///
/// Mutating methods take `&mut self`; read-only methods take `&self`.
/// Not thread-safe by itself; for shared access, wrap in a `Mutex`.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    /// capability id → ordered set of tool names providing it (insertion order).
    providers: HashMap<String, Vec<String>>,
    /// tool name → set of capability ids it declares.
    by_tool: HashMap<String, HashSet<String>>,
    /// tool name → ordered set of tool names it declared preference over.
    prefer_over: HashMap<String, Vec<String>>,
    /// tool name → source (builtin or mcp).
    source_by_tool: HashMap<String, Source>,
}

/// Insert `item` into `vec` if not already present, preserving insertion order.
fn insert_unique(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|x| x == item) {
        vec.push(item.to_string());
    }
}

impl CapabilityRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    // -----------------------------------------------------------------------
    // Mutating methods (&mut self)
    // -----------------------------------------------------------------------

    /// Register a built-in tool's capability declarations.
    ///
    /// If `capabilities` is empty, this is a no-op — the tool is not added to
    /// any capability's provider list.
    pub fn register_builtin_tool(&mut self, name: &str, capabilities: &[String]) {
        if capabilities.is_empty() {
            return;
        }
        self.add_tool(name, capabilities, Source::Builtin);
    }

    /// Wipe integrations-derived MCP entries.
    ///
    /// Called before re-applying an `integrations.yaml` to ensure stale MCP
    /// tools from a disconnected server are removed. Builtin entries survive.
    pub fn reset_integrations(&mut self) {
        let mcp_tools: Vec<String> = self
            .source_by_tool
            .iter()
            .filter(|(_, source)| **source == Source::Mcp)
            .map(|(name, _)| name.clone())
            .collect();
        for tool in mcp_tools {
            self.forget_tool(&tool);
        }
    }

    /// Apply an `integrations.yaml` config.
    ///
    /// For each declared integration whose name matches an MCP server, attach
    /// the declared capabilities to every tool the server exposes (identified
    /// by qualified name). Non-matching entries are ignored silently — a
    /// server may not be connected yet.
    ///
    /// MCP-sourced entries from a previous call are wiped first via
    /// [`reset_integrations`](Self::reset_integrations).
    pub fn apply_integrations(
        &mut self,
        config: &IntegrationsConfig,
        mcp_tools_by_server: &HashMap<String, Vec<String>>,
    ) {
        self.reset_integrations();
        for (server_name, spec) in &config.integrations {
            let Some(qualified_names) = mcp_tools_by_server.get(server_name) else {
                continue;
            };
            if qualified_names.is_empty() {
                continue;
            }
            let caps = &spec.capabilities;
            let prefer_over = &spec.prefer_over;
            for qualified in qualified_names {
                if !caps.is_empty() {
                    self.add_tool(qualified, caps, Source::Mcp);
                }
                if !prefer_over.is_empty() {
                    let existing = self.prefer_over.entry(qualified.clone()).or_default();
                    for name in prefer_over {
                        insert_unique(existing, name);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Read-only methods (&self)
    // -----------------------------------------------------------------------

    /// Names of tools that provide a capability, in insertion order.
    pub fn providers_of(&self, capability: &str) -> Vec<String> {
        self.providers.get(capability).cloned().unwrap_or_default()
    }

    /// All capabilities known to the registry, sorted alphabetically.
    pub fn capabilities(&self) -> Vec<String> {
        let mut caps: Vec<String> = self.providers.keys().cloned().collect();
        caps.sort();
        caps
    }

    /// Providers for a capability, annotated with source and `prefer_over`
    /// claims.
    ///
    /// Only includes tools whose names appear in `available_tool_names` (so a
    /// disconnected MCP tool does not surface).
    ///
    /// Ordering is driven by explicit `prefer_over` declarations: if provider
    /// A declares `prefer_over: [B]`, A precedes B in the output. Otherwise
    /// the order is the registry's insertion order (stable, no implicit
    /// MCP-first bias).
    pub fn resolve_providers(
        &self,
        capability: &str,
        available_tool_names: &[String],
    ) -> Vec<CapabilityProvider> {
        let available: HashSet<&str> = available_tool_names.iter().map(|s| s.as_str()).collect();

        let raw: Vec<String> = self
            .providers_of(capability)
            .into_iter()
            .filter(|name| available.contains(name.as_str()))
            .collect();

        // Compute a dominance rank for each provider: how many *other* available
        // providers of this capability it is preferred over, directly or
        // transitively. Higher rank → earlier in the output.
        //
        // The TS implementation uses a pairwise comparator with `toSorted`,
        // which only checks direct `preferOver` relationships. That works in
        // V8's TimSort for small arrays but is not guaranteed to produce
        // correct transitive ordering. We compute reachability explicitly to
        // make the ordering deterministic and correct for chains like
        // `C prefers over B prefers over A`.
        let raw_set: HashSet<String> = raw.iter().cloned().collect();
        let compute_rank = |name: &str| -> usize {
            // DFS over the prefer_over graph starting from `name`.
            let mut visited: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = vec![name.to_string()];
            while let Some(current) = stack.pop() {
                if let Some(preferred) = self.prefer_over.get(&current) {
                    for target in preferred {
                        // Only count targets that are also available providers
                        // of this capability.
                        if raw_set.contains(target) && visited.insert(target.clone()) {
                            stack.push(target.clone());
                        }
                    }
                }
            }
            visited.len()
        };

        // Pre-compute ranks and sort by rank descending (higher rank first).
        // Rust's `sort_by` is stable, so ties preserve insertion order.
        let mut ranked: Vec<(String, usize)> = raw
            .into_iter()
            .map(|name| {
                let r = compute_rank(&name);
                (name, r)
            })
            .collect();
        ranked.sort_by_key(|(_, rank)| std::cmp::Reverse(*rank));

        ranked
            .into_iter()
            .map(|(name, _)| {
                let source = *self.source_by_tool.get(&name).unwrap_or(&Source::Builtin);
                let prefer_over = self.prefer_over.get(&name).cloned().unwrap_or_default();
                CapabilityProvider {
                    tool_name: name,
                    source,
                    prefer_over,
                }
            })
            .collect()
    }

    /// Produce a compact hint block for injection into the system prompt.
    ///
    /// Returns an empty string when there is nothing worth saying (e.g. only
    /// builtin providers with no rivals).
    ///
    /// Format is intentionally short — the model reads it once per turn and
    /// just needs the preference order, not prose.
    pub fn build_hint(&self, available_tool_names: &[String]) -> String {
        let mut lines: Vec<String> = Vec::new();
        for capability in self.capabilities() {
            let providers = self.resolve_providers(&capability, available_tool_names);
            if providers.is_empty() {
                continue;
            }
            let primary = &providers[0];
            // Build the fallback list: primary's prefer_over items first, then
            // other providers of the same capability, deduplicated.
            let mut fallback_names: Vec<String> = primary.prefer_over.clone();
            for p in &providers[1..] {
                insert_unique(&mut fallback_names, &p.tool_name);
            }
            let available: HashSet<&str> =
                available_tool_names.iter().map(|s| s.as_str()).collect();
            let fallbacks: Vec<&str> = fallback_names
                .iter()
                .map(|s| s.as_str())
                .filter(|n| available.contains(n) && *n != primary.tool_name)
                .collect();

            let has_explicit_preference = !primary.prefer_over.is_empty();
            if has_explicit_preference || !fallbacks.is_empty() {
                let tail = if fallbacks.is_empty() {
                    String::new()
                } else {
                    format!(" (falls back to: {})", fallbacks.join(", "))
                };
                lines.push(format!(
                    "- For `{}`, prefer `{}`{}.",
                    capability, primary.tool_name, tail
                ));
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        let mut result =
            vec!["Tool preference hints (derived from installed integrations):".to_string()];
        result.extend(lines);
        result.join("\n")
    }

    /// Look up which tool names provide any of the given capabilities.
    ///
    /// Only tools present in `available_tool_names` are included. The result
    /// is deduplicated but otherwise in registry insertion order.
    pub fn tools_for_capabilities(
        &self,
        capabilities: &[String],
        available_tool_names: &[String],
    ) -> Vec<String> {
        let available: HashSet<&str> = available_tool_names.iter().map(|s| s.as_str()).collect();
        let mut result: Vec<String> = Vec::new();
        for cap in capabilities {
            for name in self.providers_of(cap) {
                if available.contains(name.as_str()) {
                    insert_unique(&mut result, &name);
                }
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Add a tool with its capabilities and source to all internal maps.
    fn add_tool(&mut self, name: &str, capabilities: &[String], source: Source) {
        self.source_by_tool.insert(name.to_string(), source);
        let cap_set = self.by_tool.entry(name.to_string()).or_default();
        for cap in capabilities {
            cap_set.insert(cap.clone());
            let provs = self.providers.entry(cap.clone()).or_default();
            insert_unique(provs, name);
        }
    }

    /// Remove a tool from all internal maps.
    ///
    /// If the tool has no `by_tool` entry (no capabilities declared), this is
    /// a no-op. This matches the TS `forgetTool` behavior, which returns early
    /// when `byTool.get(name)` is undefined.
    fn forget_tool(&mut self, name: &str) {
        let Some(caps) = self.by_tool.remove(name) else {
            return;
        };
        for cap in &caps {
            if let Some(provs) = self.providers.get_mut(cap) {
                provs.retain(|n| n != name);
                if provs.is_empty() {
                    self.providers.remove(cap);
                }
            }
        }
        self.prefer_over.remove(name);
        self.source_by_tool.remove(name);
    }
}

// ---------------------------------------------------------------------------
// CapabilityResolver
// ---------------------------------------------------------------------------

/// Wraps [`CapabilityRegistry`] with filesystem context (`home_dir`, `cwd`)
/// for loading integration configs and resolving profile-declared
/// `capabilities_required` into concrete tool names.
#[derive(Debug)]
pub struct CapabilityResolver {
    /// The underlying capability registry.
    pub registry: CapabilityRegistry,
    /// User home directory (for user-global `integrations.yaml`).
    pub home_dir: PathBuf,
    /// Current working directory (for project-local
    /// `.mirri-code/integrations.yaml`).
    pub cwd: PathBuf,
}

impl CapabilityResolver {
    /// Create a new resolver with an empty registry.
    pub fn new(home_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            registry: CapabilityRegistry::new(),
            home_dir: home_dir.into(),
            cwd: cwd.into(),
        }
    }

    /// Register a built-in tool's capabilities into the registry.
    pub fn register_builtin_tool(&mut self, name: &str, capabilities: &[String]) {
        self.registry.register_builtin_tool(name, capabilities);
    }

    /// Apply an `integrations.yaml` config to the registry.
    pub fn apply_integrations(
        &mut self,
        config: &IntegrationsConfig,
        mcp_tools_by_server: &HashMap<String, Vec<String>>,
    ) {
        self.registry
            .apply_integrations(config, mcp_tools_by_server);
    }

    /// Produce a capability-preference hint string for the system prompt.
    /// Empty when there is nothing worth saying.
    pub fn build_hint(&self, available_tool_names: &[String]) -> String {
        self.registry.build_hint(available_tool_names)
    }

    /// Extend a base tool-name list with tool names that satisfy any of the
    /// given capabilities.
    ///
    /// Used by profile binding to auto-add discovered MCP tools when a profile
    /// declares `capabilities_required`. If `capabilities_required` is empty,
    /// `base_names` is returned as-is. If no matching tools are found,
    /// `base_names` is returned as-is.
    pub fn augment_tool_names(
        &self,
        base_names: &[String],
        capabilities_required: &[String],
        available: &[String],
    ) -> Vec<String> {
        if capabilities_required.is_empty() {
            return base_names.to_vec();
        }
        let extras = self
            .registry
            .tools_for_capabilities(capabilities_required, available);
        if extras.is_empty() {
            return base_names.to_vec();
        }
        let mut merged = base_names.to_vec();
        for name in &extras {
            insert_unique(&mut merged, name);
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Create a `Vec<String>` from string literals.
    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Create a `HashMap<String, Vec<String>>` from (key, &[&str]) pairs.
    fn server_map(items: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), strs(v)))
            .collect()
    }

    /// Create an `IntegrationsConfig` from (server, caps, prefer_over) tuples.
    fn config(items: &[(&str, &[&str], &[&str])]) -> IntegrationsConfig {
        IntegrationsConfig {
            integrations: items
                .iter()
                .map(|(server, caps, prefer_over)| {
                    (
                        server.to_string(),
                        IntegrationSpec {
                            capabilities: strs(caps),
                            prefer_over: strs(prefer_over),
                        },
                    )
                })
                .collect(),
        }
    }

    // -----------------------------------------------------------------------
    // register_builtin_tool
    // -----------------------------------------------------------------------

    #[test]
    fn should_register_declared_capabilities_when_registering_a_builtin_tool() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        assert_eq!(r.providers_of("code.explore"), strs(&["Grep"]));
        assert_eq!(r.capabilities(), strs(&["code.explore"]));
    }

    #[test]
    fn should_be_noop_when_registering_with_empty_capabilities() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Read", &[]);

        assert!(r.capabilities().is_empty());
        assert!(r.providers_of("code.explore").is_empty());
    }

    #[test]
    fn should_register_all_providers_when_multiple_tools_declare_the_same_capability() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));
        r.register_builtin_tool("Glob", &strs(&["code.explore"]));

        assert_eq!(r.providers_of("code.explore"), strs(&["Grep", "Glob"]));
    }

    // -----------------------------------------------------------------------
    // capabilities()
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_capabilities_in_sorted_order_when_registered_in_arbitrary_order() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("A", &strs(&["z.cap", "a.cap"]));
        r.register_builtin_tool("B", &strs(&["m.cap"]));

        assert_eq!(r.capabilities(), strs(&["a.cap", "m.cap", "z.cap"]));
    }

    // -----------------------------------------------------------------------
    // apply_integrations
    // -----------------------------------------------------------------------

    #[test]
    fn should_attach_capabilities_to_mcp_tools_when_integrations_config_matches_server() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[(
            "codebase-memory-mcp",
            &["code.explore", "test.navigate"],
            &["Grep"],
        )]);
        let mcp = server_map(&[(
            "codebase-memory-mcp",
            &[
                "mcp__codebase-memory-mcp__search_graph",
                "mcp__codebase-memory-mcp__trace_path",
            ],
        )]);
        r.apply_integrations(&cfg, &mcp);

        assert!(
            r.providers_of("code.explore")
                .contains(&"mcp__codebase-memory-mcp__search_graph".to_string())
        );
        assert!(r.providers_of("code.explore").contains(&"Grep".to_string()));
    }

    #[test]
    fn should_silently_ignore_unknown_server_when_applying_integrations_config() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("unknown", &["code.explore"], &[])]);
        let mcp = HashMap::new();
        r.apply_integrations(&cfg, &mcp);

        assert_eq!(r.providers_of("code.explore"), strs(&["Grep"]));
    }

    #[test]
    fn should_be_noop_when_mcp_tools_by_server_has_empty_array() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &[])]);
        r.apply_integrations(&cfg, &mcp);

        assert_eq!(r.providers_of("code.explore"), strs(&["Grep"]));
    }

    #[test]
    fn should_wipe_only_mcp_entries_when_reset_integrations_is_called() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &["mcp__srv__t"])]);
        r.apply_integrations(&cfg, &mcp);
        assert!(r.providers_of("code.explore").len() > 1);

        r.reset_integrations();
        assert_eq!(r.providers_of("code.explore"), strs(&["Grep"]));
    }

    // -----------------------------------------------------------------------
    // resolve_providers
    // -----------------------------------------------------------------------

    #[test]
    fn should_exclude_unavailable_tools_when_resolving_providers() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &["mcp__srv__search"])]);
        r.apply_integrations(&cfg, &mcp);

        let providers = r.resolve_providers("code.explore", &strs(&["Grep"]));
        assert_eq!(
            providers
                .iter()
                .map(|p| p.tool_name.clone())
                .collect::<Vec<_>>(),
            strs(&["Grep"])
        );
    }

    #[test]
    fn should_not_reorder_mcp_ahead_of_builtins_when_no_prefer_over_is_declared() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &["mcp__srv__search"])]);
        r.apply_integrations(&cfg, &mcp);

        let available = strs(&["Grep", "mcp__srv__search"]);
        let providers = r.resolve_providers("code.explore", &available);
        // Insertion order: Grep registered first, MCP applied second. No
        // prefer_over, so the registry does NOT reshuffle to MCP-first.
        assert_eq!(
            providers
                .iter()
                .map(|p| p.tool_name.clone())
                .collect::<Vec<_>>(),
            strs(&["Grep", "mcp__srv__search"])
        );
    }

    #[test]
    fn should_order_providers_transitively_when_prefer_over_chain_is_declared() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[
            ("srv", &["code.explore"], &["Grep"]),
            ("srv2", &["code.explore"], &["mcp__srv__search"]),
        ]);
        let mcp = server_map(&[
            ("srv", &["mcp__srv__search"]),
            ("srv2", &["mcp__srv2__deep"]),
        ]);
        r.apply_integrations(&cfg, &mcp);

        let available = strs(&["Grep", "mcp__srv__search", "mcp__srv2__deep"]);
        let providers = r.resolve_providers("code.explore", &available);
        // srv2 prefers over srv's tool, srv prefers over Grep → srv2 > srv > Grep
        assert_eq!(providers[0].tool_name, "mcp__srv2__deep");
        assert_eq!(providers[1].tool_name, "mcp__srv__search");
        assert_eq!(providers[2].tool_name, "Grep");
    }

    #[test]
    fn should_return_empty_when_capability_is_unknown() {
        let r = CapabilityRegistry::new();
        assert_eq!(r.resolve_providers("unknown.cap", &strs(&["Grep"])), vec![]);
    }

    #[test]
    fn should_return_empty_when_all_providers_filtered_out() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &["mcp__srv__t"])]);
        r.apply_integrations(&cfg, &mcp);

        assert_eq!(
            r.resolve_providers("code.explore", &strs(&["Read"])),
            vec![]
        );
    }

    #[test]
    fn should_record_builtin_source_when_tool_is_registered_as_builtin() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let providers = r.resolve_providers("code.explore", &strs(&["Grep"]));
        assert_eq!(providers[0].source, Source::Builtin);
    }

    #[test]
    fn should_record_mcp_source_when_tool_comes_from_integrations() {
        let mut r = CapabilityRegistry::new();

        let cfg = config(&[("srv", &["code.explore"], &[])]);
        let mcp = server_map(&[("srv", &["mcp__srv__t"])]);
        r.apply_integrations(&cfg, &mcp);

        let providers = r.resolve_providers("code.explore", &strs(&["mcp__srv__t"]));
        assert_eq!(providers[0].source, Source::Mcp);
    }

    // -----------------------------------------------------------------------
    // build_hint
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_empty_hint_when_only_builtin_providers_exist() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let hint = r.build_hint(&strs(&["Grep"]));
        assert_eq!(hint, "");
    }

    #[test]
    fn should_produce_hint_when_mcp_provider_is_available() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let cfg = config(&[("codebase-memory-mcp", &["code.explore"], &["Grep"])]);
        let mcp = server_map(&[(
            "codebase-memory-mcp",
            &["mcp__codebase-memory-mcp__search_graph"],
        )]);
        r.apply_integrations(&cfg, &mcp);

        let available = strs(&["Grep", "mcp__codebase-memory-mcp__search_graph"]);
        let hint = r.build_hint(&available);
        assert!(hint.contains("code.explore"));
        assert!(hint.contains("mcp__codebase-memory-mcp__search_graph"));
        assert!(hint.contains("Grep"));
    }

    #[test]
    fn should_return_empty_hint_when_registry_is_empty() {
        let r = CapabilityRegistry::new();
        assert_eq!(r.build_hint(&[]), "");
    }

    // -----------------------------------------------------------------------
    // tools_for_capabilities
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_matching_available_tools_when_capability_is_queried() {
        let mut r = CapabilityRegistry::new();
        r.register_builtin_tool("Grep", &strs(&["code.explore"]));
        r.register_builtin_tool("Glob", &strs(&["code.explore"]));

        let available = strs(&["Grep", "Glob", "Read"]);
        let mut result = r.tools_for_capabilities(&strs(&["code.explore"]), &available);
        result.sort();
        assert_eq!(result, strs(&["Glob", "Grep"]));
    }

    #[test]
    fn should_return_empty_when_no_tools_provide_required_capability() {
        let r = CapabilityRegistry::new();
        let result = r.tools_for_capabilities(&strs(&["code.explore"]), &strs(&["Read"]));
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // CapabilityResolver.augment_tool_names
    // -----------------------------------------------------------------------

    #[test]
    fn should_augment_base_names_with_matching_tools_when_capability_is_required() {
        let mut resolver = CapabilityResolver::new("/tmp", "/tmp");
        resolver.register_builtin_tool("Grep", &strs(&["code.explore"]));
        resolver.register_builtin_tool("Read", &strs(&["code.read"]));

        let cfg = config(&[("codegraph", &["code.explore"], &["Grep"])]);
        let mcp = server_map(&[("codegraph", &["mcp__codegraph__search"])]);
        resolver.apply_integrations(&cfg, &mcp);

        let base = strs(&["Read", "Grep"]);
        let required = strs(&["code.explore"]);
        let available = strs(&["Read", "Grep", "mcp__codegraph__search"]);

        let result = resolver.augment_tool_names(&base, &required, &available);
        assert!(result.contains(&"mcp__codegraph__search".to_string()));
        assert!(result.contains(&"Read".to_string()));
        assert!(result.contains(&"Grep".to_string()));
    }

    #[test]
    fn should_not_augment_when_no_capabilities_required() {
        let mut resolver = CapabilityResolver::new("/tmp", "/tmp");
        resolver.register_builtin_tool("Grep", &strs(&["code.explore"]));

        let base = strs(&["Read", "Grep"]);
        let available = strs(&["Read", "Grep"]);

        let result = resolver.augment_tool_names(&base, &[], &available);
        assert_eq!(result, base);
    }

    #[test]
    fn should_return_base_unchanged_when_no_matching_tools_found() {
        let mut resolver = CapabilityResolver::new("/tmp", "/tmp");
        resolver.register_builtin_tool("Read", &strs(&["code.read"]));

        let base = strs(&["Read"]);
        let required = strs(&["code.explore"]);
        let available = strs(&["Read"]);

        let result = resolver.augment_tool_names(&base, &required, &available);
        assert_eq!(result, base);
    }
}
