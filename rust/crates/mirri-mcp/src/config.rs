//! MCP configuration loading and three-layer merging.
//!
//! Configuration is discovered from up to three locations, each optionally
//! present:
//!
//! 1. **Global** — `~/.mirri-code/mcp.json` (user-wide defaults)
//! 2. **Project root** — `<project-root>/.mcp.json` (Claude-compatible format)
//! 3. **Project local** — `<cwd>/.mirri-code/mcp.json` (per-workspace overrides)
//!
//! Later layers override earlier ones on a per-server basis: if the same
//! server name appears in multiple layers, the last layer wins entirely
//! (the entry is replaced, not deep-merged).
//!
//! Environment variables are expanded in all string fields of the config
//! using `${VAR}` or `${env:VAR}` syntax.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{McpError, McpResult};
use crate::transport::McpTransportConfig;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Authentication configuration for an MCP server.
///
/// Tagged by `type` so it deserializes directly from JSON:
///
/// ```json
/// { "type": "bearer", "token": "secret" }
/// { "type": "oauth", "client_id": "abc", "scopes": ["read", "write"] }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpAuthConfig {
    /// Static bearer token.
    #[serde(rename = "bearer")]
    Bearer {
        /// The bearer token value.
        token: String,
    },
    /// OAuth configuration (the actual token acquisition is handled upstream).
    #[serde(rename = "oauth")]
    OAuth {
        /// OAuth client id.
        #[serde(rename = "clientId", default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// Requested OAuth scopes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scopes: Option<Vec<String>>,
    },
}

/// Configuration for a single MCP server.
///
/// Combines the transport configuration (from Task 24) with server-level
/// options such as timeouts, tool filtering, and authentication.
///
/// `PartialEq` is derived so the config hot-reload watcher can detect when
/// an already-connected server's config changed on disk and force a
/// reconnect.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServerConfig {
    /// Transport configuration (stdio / http / sse).
    pub transport: McpTransportConfig,
    /// Whether the server is enabled. Defaults to `true` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Startup timeout in milliseconds. Defaults to 30000.
    #[serde(
        rename = "startupTimeoutMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_timeout_ms: Option<u64>,
    /// Per-tool-call timeout in milliseconds. Defaults to 60000.
    #[serde(
        rename = "toolTimeoutMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_timeout_ms: Option<u64>,
    /// Allowlist of tool names. When present, only these tools are exposed.
    #[serde(
        rename = "enabledTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist of tool names. Tools listed here are hidden.
    #[serde(
        rename = "disabledTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disabled_tools: Option<Vec<String>>,
    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpAuthConfig>,
}

/// Custom deserializer for `McpServerConfig` that normalizes the transport
/// field before delegating to standard deserialization.
///
/// This makes `McpServerConfig` accept all three input forms:
/// 1. TS/Claude-style with no `transport` key (inferred from `command`/`url`)
/// 2. String `transport` like `"stdio"` (converted to object form)
/// 3. Object `transport` like `{"type": "stdio", ...}` (passthrough)
impl<'de> serde::Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize into a generic JSON value so we can normalize.
        let mut value = serde_json::Value::deserialize(deserializer)?;
        normalize_server_entry(&mut value);

        // Now that transport is guaranteed to be an object, deserialize
        // normally. We use `#[serde(flatten)]` via a temporary wrapper to
        // avoid duplicating the struct fields.
        #[derive(Deserialize)]
        struct Wrapper {
            transport: McpTransportConfig,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            enabled: Option<bool>,
            #[serde(
                rename = "startupTimeoutMs",
                default,
                skip_serializing_if = "Option::is_none"
            )]
            startup_timeout_ms: Option<u64>,
            #[serde(
                rename = "toolTimeoutMs",
                default,
                skip_serializing_if = "Option::is_none"
            )]
            tool_timeout_ms: Option<u64>,
            #[serde(
                rename = "enabledTools",
                default,
                skip_serializing_if = "Option::is_none"
            )]
            enabled_tools: Option<Vec<String>>,
            #[serde(
                rename = "disabledTools",
                default,
                skip_serializing_if = "Option::is_none"
            )]
            disabled_tools: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            auth: Option<McpAuthConfig>,
        }

        let w = Wrapper::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(McpServerConfig {
            transport: w.transport,
            enabled: w.enabled,
            startup_timeout_ms: w.startup_timeout_ms,
            tool_timeout_ms: w.tool_timeout_ms,
            enabled_tools: w.enabled_tools,
            disabled_tools: w.disabled_tools,
            auth: w.auth,
        })
    }
}

impl McpServerConfig {
    /// Returns `true` if the server is enabled (defaults to `true`).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Returns the startup timeout as `std::time::Duration`.
    pub fn startup_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.startup_timeout_ms.unwrap_or(30_000))
    }

    /// Returns the per-tool-call timeout as `std::time::Duration`.
    pub fn tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.tool_timeout_ms.unwrap_or(60_000))
    }

    /// Returns `true` if the given tool name should be exposed for this server,
    /// applying the `enabled_tools` / `disabled_tools` filters.
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        if let Some(disabled) = &self.disabled_tools
            && disabled.iter().any(|t| t == tool_name)
        {
            return false;
        }
        if let Some(enabled) = &self.enabled_tools
            && !enabled.iter().any(|t| t == tool_name)
        {
            return false;
        }
        true
    }
}

/// Top-level MCP configuration file.
///
/// Maps server names to their configurations. This is the shape expected in
/// each of the three config layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfigFile {
    /// Server name → server config.
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Load and merge three layers of MCP configuration.
///
/// Layers, in order of increasing priority:
/// 1. `~/.mirri-code/mcp.json` (global)
/// 2. `<project-root>/.mcp.json` (Claude-compatible)
/// 3. `<cwd>/.mirri-code/mcp.json` (project local)
///
/// If a file does not exist it is silently skipped. Environment variable
/// expansion (`${VAR}` and `${env:VAR}`) is applied to all loaded configs.
/// For stdio transports, the `cwd` field (if relative) is resolved relative
/// to the directory containing the config file it came from.
pub fn load_mcp_config(
    home_dir: &Path,
    project_root: &Path,
    cwd: &Path,
) -> McpResult<HashMap<String, McpServerConfig>> {
    // The A7 family-wise merge over the layered load (adjudication
    // ADR-006): the connection family (transport/env/timeout/enabled) keeps
    // the historical whole-entry shadowing — the highest *complete* entry
    // wins — while the filter family overrides only when a layer explicitly
    // carries `enabledTools`/`disabledTools` (inheriting the lower layer
    // otherwise).
    let layered = load_mcp_config_layered(home_dir, project_root, cwd)?;
    Ok(merge_layers(&layered))
}

/// The pool-routing view of the three config layers: each layer's entries
/// with their provenance kept (the flat [`load_mcp_config`] discards it) —
/// `user` feeds the app-level user pool, the project layers feed the
/// workspace pool. A name present in several layers appears in each map:
/// every pool holds its own connection, the view layer shadows the
/// earlier entries (spec §4.2).
///
/// The parallel `*_raw` maps hold the stored JSON of each layer's entries
/// — the only place the A7 merge learns whether an entry explicitly
/// carries a `transport` key (the parsed values normalize every entry to
/// an object transport, so the parsed maps cannot tell a complete entry
/// from a filter-only override).
#[derive(Debug, Default, Clone)]
pub struct LayeredMcpConfig {
    /// Entries from the user layer (`<home>/.mirri-code/mcp.json`).
    pub user: HashMap<String, McpServerConfig>,
    /// Raw stored JSON of the user-layer entries (A7 transport-presence).
    pub user_raw: HashMap<String, Value>,
    /// Entries from the project-root layer (`.mcp.json`, Claude-compatible).
    pub project_root: HashMap<String, McpServerConfig>,
    /// Raw stored JSON of the project-root entries (A7 transport-presence).
    pub project_root_raw: HashMap<String, Value>,
    /// Entries from the project-local layer (`.mirri-code/mcp.json`).
    pub project: HashMap<String, McpServerConfig>,
    /// Raw stored JSON of the project-local entries (A7 transport-presence).
    pub project_raw: HashMap<String, Value>,
}

impl LayeredMcpConfig {
    /// The project layers merged — the workspace pool's connect set. The
    /// A7 family-wise merge routes the connection family to the highest
    /// complete entry and the filter family to the last explicit carrier;
    /// a name whose only project-layer entries are filter-only overrides
    /// (no complete entry in either project layer) is not a connection and
    /// does not surface.
    pub fn project_merged(&self) -> HashMap<String, McpServerConfig> {
        let mut merged = HashMap::new();
        let mut pending = HashMap::new();
        merge_layer_into(
            &mut merged,
            &mut pending,
            &self.project_root,
            &self.project_root_raw,
        );
        merge_layer_into(&mut merged, &mut pending, &self.project, &self.project_raw);
        merged
    }

    /// The user pool's connect set — the user layer's complete entries with
    /// the project layers' **filter-only overrides** merged over them. A7's
    /// filter family crosses pools: a workspace-scope `setToolFilter` on a
    /// user-defined name stores a filter-only entry in the project-local
    /// layer, and that filter binds to the user pool's connection (the
    /// connection family never moves — a *complete* project entry of the
    /// same name is workspace-owned for connection and tool registration,
    /// spec §4.2, so the user pool keeps its own layer's definition).
    pub fn user_merged(&self) -> HashMap<String, McpServerConfig> {
        let mut merged = HashMap::new();
        let mut pending = HashMap::new();
        merge_layer_into(&mut merged, &mut pending, &self.user, &self.user_raw);
        merge_filter_only_into(&mut merged, &self.project_root, &self.project_root_raw);
        merge_filter_only_into(&mut merged, &self.project, &self.project_raw);
        merged
    }

    /// Whether every layer is empty.
    pub fn is_empty(&self) -> bool {
        self.user.is_empty() && self.project_root.is_empty() && self.project.is_empty()
    }
}

/// Load only the **user layer** (`<home>/.mirri-code/mcp.json`) into a
/// [`LayeredMcpConfig`] — the untrusted-workspace view (design
/// `08-multi-workspace.md` §5 D6, W7): a workspace whose project layers the
/// trust gate excludes discovers the user layer only, and its project
/// layer files are never read. Expansion and relative-`cwd` resolution
/// apply exactly as in [`load_mcp_config_layered`].
pub fn load_mcp_config_user_layered(home_dir: &Path) -> McpResult<LayeredMcpConfig> {
    let mut layered = LayeredMcpConfig::default();
    let global_path = global_config_path(home_dir);
    if global_path.is_file() {
        let (parsed, raw) = load_config_file_with_raw(&global_path)?;
        layered.user = parsed;
        layered.user_raw = raw;
    }
    Ok(layered)
}

/// Load the three config layers keeping each pool's entries separate (see
/// [`LayeredMcpConfig`]). Expansion (`${VAR}` / `${env:VAR}`) and relative
/// stdio `cwd` resolution apply exactly as in [`load_mcp_config`].
pub fn load_mcp_config_layered(
    home_dir: &Path,
    project_root: &Path,
    cwd: &Path,
) -> McpResult<LayeredMcpConfig> {
    let mut layered = load_mcp_config_user_layered(home_dir)?;

    let project_root_path = project_root_config_path(project_root);
    if project_root_path.is_file() {
        let (parsed, raw) =
            load_config_file_with_raw_for_layer(&project_root_path, StdioCwdDefault::ConfigDir)?;
        layered.project_root = parsed;
        layered.project_root_raw = raw;
    }

    let local_path = local_layer_path_if_distinct(home_dir, cwd);
    if let Some(local_path) = &local_path
        && local_path.is_file()
    {
        let (parsed, raw) =
            load_config_file_with_raw_for_layer(local_path, StdioCwdDefault::ConfigDir)?;
        layered.project = parsed;
        layered.project_raw = raw;
    }

    Ok(layered)
}

/// Read a single config file and parse it, keeping `${VAR}` references
/// verbatim (adjudication A5: the stored and in-memory config stays raw —
/// expansion happens at connect/test time via
/// [`expand_server_config_values`]). Relative stdio `cwd` paths are still
/// resolved against the config file's directory at load time. Public so
/// the engine's connection-test path can resolve a persisted entry with
/// exactly the loader's semantics.
pub fn load_config_file(path: &Path) -> McpResult<HashMap<String, McpServerConfig>> {
    let (parsed, _raw) = load_config_file_with_raw(path)?;
    Ok(parsed)
}

/// [`load_config_file`] plus the raw stored JSON of each entry — the A7
/// merge's transport-presence source ([`entry_has_transport`]). The raw
/// map is what `upsert_filter_override_entry` writes: filter-only entries
/// carry no `transport` key, and the parsed maps cannot recover that fact
/// after normalization, so the layered load keeps both.
pub fn load_config_file_with_raw(
    path: &Path,
) -> McpResult<(HashMap<String, McpServerConfig>, HashMap<String, Value>)> {
    load_config_file_with_raw_impl(path, StdioCwdDefault::Inherit)
}

/// The shared body of [`load_config_file_with_raw`] and
/// [`load_config_file_with_raw_for_layer`].
fn load_config_file_with_raw_impl(
    path: &Path,
    stdio_cwd_default: StdioCwdDefault,
) -> McpResult<(HashMap<String, McpServerConfig>, HashMap<String, Value>)> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        McpError::Protocol(format!("failed to read MCP config {}: {e}", path.display()))
    })?;

    let value: Value = serde_json::from_str(&raw).map_err(|e| {
        McpError::Protocol(format!(
            "failed to parse MCP config {}: {e}",
            path.display()
        ))
    })?;
    let file: McpConfigFile = serde_json::from_value(value.clone()).map_err(|e| {
        McpError::Protocol(format!(
            "failed to parse MCP config {}: {e}",
            path.display()
        ))
    })?;
    let raw_servers = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .iter()
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect()
        })
        .unwrap_or_default();

    // Resolve relative stdio cwd against the config file's directory.
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut servers = file.mcp_servers;
    for cfg in servers.values_mut() {
        resolve_stdio_cwd(cfg, config_dir, stdio_cwd_default);
    }

    Ok((servers, raw_servers))
}

/// How a config file's stdio entries resolve an **omitted** `cwd` (see
/// [`load_config_file_with_raw_for_layer`]).
///
/// The split is layer-shaped by design: a server declared inside a
/// workspace must run in that workspace whatever directory the server
/// process itself was started from, while the user layer's connections are
/// shared across workspaces (one process, one cwd) — there an omitted
/// `cwd` keeps the child inheriting the server's working directory, and
/// pinning one stays the operator's explicit choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdioCwdDefault {
    /// The user layer's behavior: an omitted `cwd` stays `None` — the
    /// child inherits the server process's working directory.
    Inherit,
    /// The project layers' behavior: an omitted `cwd` defaults to the
    /// config file's directory (the workspace root that owns the layer).
    ConfigDir,
}

/// [`load_config_file_with_raw`] with an explicit omitted-`cwd` policy for
/// the layer the file belongs to (see [`StdioCwdDefault`]). The layered
/// loader passes [`StdioCwdDefault::ConfigDir`] for the project layers and
/// [`StdioCwdDefault::Inherit`] for the user layer; a bare
/// [`load_config_file_with_raw`] call resolves a file with the legacy
/// inherit semantics.
pub fn load_config_file_with_raw_for_layer(
    path: &Path,
    stdio_cwd_default: StdioCwdDefault,
) -> McpResult<(HashMap<String, McpServerConfig>, HashMap<String, Value>)> {
    load_config_file_with_raw_impl(path, stdio_cwd_default)
}

/// Whether a raw stored entry defines a connection — the A7 complete-entry
/// test. Answers `true` for an explicit `transport` key (object or string
/// form) and for the Claude-form entries the loader infers a transport
/// from (`command` → stdio, `url` → http); a filter-only override
/// ([`upsert_filter_override_entry`]) stores only the filter fields and
/// answers `false` — after normalization both shapes parse into
/// `McpServerConfig`, so the family merge decides through the raw form.
pub fn entry_has_transport(raw: &Value) -> bool {
    raw.get("transport").is_some() || raw.get("command").is_some() || raw.get("url").is_some()
}

/// The A7 family-wise merge of the three config layers (adjudication
/// ADR-006) over [`LayeredMcpConfig`], from low to high priority:
///
/// - **Connection family** (transport/env/timeouts/enabled): the highest
///   *complete* entry (one whose raw JSON defines a connection —
///   [`entry_has_transport`]) wins entirely — the historical whole-entry
///   shadowing. A filter-only override entry (no transport) never shadows
///   the connection family.
/// - **Filter family** (`enabledTools`/`disabledTools`): the last layer
///   *explicitly carrying* a filter field wins; a complete entry or
///   override without filter fields inherits the already-merged lower
///   value instead of clearing it.
///
/// A name whose only entries are filter-only overrides (no complete entry
/// in any layer) has nothing to bind the filter to and does not surface —
/// `load_mcp_config` only ever produces connectable entries.
pub fn merge_layers(layered: &LayeredMcpConfig) -> HashMap<String, McpServerConfig> {
    let mut merged = HashMap::new();
    let mut pending = HashMap::new();
    merge_layer_into(&mut merged, &mut pending, &layered.user, &layered.user_raw);
    merge_layer_into(
        &mut merged,
        &mut pending,
        &layered.project_root,
        &layered.project_root_raw,
    );
    merge_layer_into(
        &mut merged,
        &mut pending,
        &layered.project,
        &layered.project_raw,
    );
    merged
}

/// One layer's contribution to the A7 merge: complete entries shadow the
/// connection family (inheriting the filter family when they do not carry
/// it), filter-only overrides replace only the filter family. A filter-only
/// entry that arrives before any complete entry of its name is held in
/// `pending` and binds to the first later complete entry (unless that entry
/// explicitly carries its own filter — the same explicit-only rule the
/// shadowing branch applies), so the entry's layer order never decides
/// whether its filter survives the merge.
fn merge_layer_into(
    merged: &mut HashMap<String, McpServerConfig>,
    pending: &mut HashMap<String, McpServerConfig>,
    layer: &HashMap<String, McpServerConfig>,
    layer_raw: &HashMap<String, Value>,
) {
    for (name, entry) in layer {
        let has_transport = layer_raw.get(name).is_some_and(entry_has_transport);
        match merged.get_mut(name) {
            Some(existing) if has_transport => {
                // Complete entry: the connection family shadows the lower
                // entry entirely; the filter family inherits the lower
                // value unless this entry explicitly carries it (A7:
                // explicit-only override).
                let mut next = entry.clone();
                if next.enabled_tools.is_none() && next.disabled_tools.is_none() {
                    next.enabled_tools = existing.enabled_tools.clone();
                    next.disabled_tools = existing.disabled_tools.clone();
                }
                merged.insert(name.clone(), next);
            }
            Some(existing) => {
                // Filter-only override (no transport): replaces only the
                // filter family; the connection family stays.
                existing.enabled_tools = entry.enabled_tools.clone();
                existing.disabled_tools = entry.disabled_tools.clone();
            }
            None => {
                if has_transport {
                    let mut next = entry.clone();
                    // A filter-only entry held from an earlier (lower)
                    // layer binds to this first complete entry of the name:
                    // the complete entry inherits the filter family unless
                    // it explicitly carries its own.
                    if next.enabled_tools.is_none()
                        && next.disabled_tools.is_none()
                        && let Some(filter) = pending.remove(name)
                    {
                        next.enabled_tools = filter.enabled_tools;
                        next.disabled_tools = filter.disabled_tools;
                    }
                    merged.insert(name.clone(), next);
                } else {
                    // Filter-only with nothing to bind to yet: hold it for
                    // the first later complete entry of the name (a later
                    // filter-only carrier overwrites — the last explicit
                    // carrier wins).
                    pending.insert(name.clone(), entry.clone());
                }
            }
        }
    }
}

/// One project layer's **filter-only** entries merged over `merged` — the
/// complete entries are skipped (the workspace pool owns those names' live
/// connections, spec §4.2), so an override reaches the user pool's entry
/// without ever redefining its transport.
fn merge_filter_only_into(
    merged: &mut HashMap<String, McpServerConfig>,
    layer: &HashMap<String, McpServerConfig>,
    layer_raw: &HashMap<String, Value>,
) {
    let overrides: HashMap<String, McpServerConfig> = layer
        .iter()
        .filter(|(name, _)| {
            layer_raw
                .get(*name)
                .is_some_and(|raw| !entry_has_transport(raw))
        })
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect();
    // The pending map is a throwaway by construction: only filter-only
    // entries flow through here (complete entries are pre-filtered out),
    // so a name with no user-layer binding has no later complete entry to
    // bind to — it stays a non-connection, exactly as before.
    let mut pending = HashMap::new();
    merge_layer_into(merged, &mut pending, &overrides, layer_raw);
}

/// If the transport is stdio and `cwd` is relative, join it with `base_dir`.
/// Resolve a stdio entry's `cwd` against `base_dir` (the config file's
/// directory): an absolute path is kept verbatim, a relative one joins
/// `base_dir`, and an omitted one follows [`StdioCwdDefault`] — the
/// layer-shaped policy for the omitted case (see the enum's docs).
fn resolve_stdio_cwd(config: &mut McpServerConfig, base_dir: &Path, default: StdioCwdDefault) {
    if let McpTransportConfig::Stdio { cwd, .. } = &mut config.transport {
        match cwd.take() {
            Some(cwd_path) if cwd_path.is_absolute() => *cwd = Some(cwd_path),
            Some(cwd_path) => *cwd = Some(base_dir.join(cwd_path)),
            None => {
                if default == StdioCwdDefault::ConfigDir {
                    *cwd = Some(base_dir.to_path_buf());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TS/Claude-compatible transport normalization
// ---------------------------------------------------------------------------

/// Normalize a single MCP server entry to the internal transport-object form.
///
/// The TS implementation (and Claude's `.mcp.json` spec) accepts entries like:
///
/// ```json
/// { "command": "npx", "args": ["..."] }          // stdio, inferred
/// { "transport": "stdio", "command": "npx" }      // stdio, string tag
/// { "url": "https://..." }                         // http, inferred
/// { "transport": { "type": "stdio", ... } }        // already object form
/// ```
///
/// Rust's `McpTransportConfig` expects a nested object with a `type` tag.
/// This function rewrites the raw JSON so the rest of deserialization works
/// regardless of which external format was used.
///
/// Precedence when `transport` is absent: `command` → stdio, `url` → http.
/// If both `command` and `url` are present, `command` wins (stdio).
fn normalize_server_entry(entry: &mut serde_json::Value) {
    let obj = match entry.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // If transport is already an object, nothing to do.
    if obj.get("transport").map(|v| v.is_object()).unwrap_or(false) {
        return;
    }

    // Determine the transport type.
    let transport_type = if let Some(t) = obj.get("transport").and_then(|v| v.as_str()) {
        // transport is a string like "stdio" / "http" / "sse".
        t.to_string()
    } else if obj.contains_key("command") {
        // No transport field — infer stdio from command presence.
        "stdio".to_string()
    } else if obj.contains_key("url") {
        // No transport field — infer http from url presence.
        "http".to_string()
    } else if obj.contains_key("enabledTools") || obj.contains_key("disabledTools") {
        // A7 filter-override entry (ADR-006, `upsert_filter_override_entry`):
        // carries only the filter family — no transport, and nothing to
        // infer one from. Synthesize an inert placeholder transport so the
        // entry deserializes; the family merge decides via the raw
        // transport-presence check (`entry_has_transport`), never through
        // this parsed value. Entries with neither a transport nor filter
        // fields stay malformed and let serde produce the error below.
        obj.insert(
            "transport".to_string(),
            serde_json::json!({ "type": "stdio", "command": "" }),
        );
        return;
    } else {
        // Cannot infer — let serde produce the error.
        return;
    };

    // Build the transport object, moving transport-specific fields into it.
    let mut transport_obj = serde_json::Map::new();
    transport_obj.insert(
        "type".to_string(),
        serde_json::Value::String(transport_type),
    );

    // Fields that belong to the transport, not the server-level config.
    const TRANSPORT_FIELDS: &[&str] = &["command", "args", "env", "cwd", "url", "headers", "token"];
    for field in TRANSPORT_FIELDS {
        if let Some(val) = obj.remove(*field) {
            transport_obj.insert(field.to_string(), val);
        }
    }

    obj.insert(
        "transport".to_string(),
        serde_json::Value::Object(transport_obj),
    );
}

// ---------------------------------------------------------------------------
// Environment variable expansion
// ---------------------------------------------------------------------------

/// Expand `${VAR}` and `${env:VAR}` references in a string.
///
/// Unknown variables are replaced with an empty string (matching the
/// behavior of most shell expansion in config tools). String-level
/// helper for individual string fields only — configs never round-trip
/// through it: the config surface expands at the value level via
/// [`expand_server_config_values`] / [`expand_server_config_values_with_env`],
/// so object keys holding `${...}` stay verbatim.
fn expand_env_vars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if ch == '$' && chars.peek().map(|(_, c)| *c) == Some('{') {
            // Consume the '{'.
            chars.next();

            // Collect until closing '}'.
            let mut var_name = String::new();
            let mut found_close = false;
            for (_, inner_ch) in chars.by_ref() {
                if inner_ch == '}' {
                    found_close = true;
                    break;
                }
                var_name.push(inner_ch);
            }

            if !found_close {
                // No closing brace — emit literally.
                result.push_str("${");
                result.push_str(&var_name);
                continue;
            }

            // Strip the `env:` prefix if present.
            let var_name = if let Some(rest) = var_name.strip_prefix("env:") {
                rest.to_string()
            } else {
                var_name
            };

            let value = std::env::var(&var_name).unwrap_or_default();
            result.push_str(&value);
        } else {
            result.push(ch);
        }
    }

    result
}

/// Expand env vars in a single string value (public for testing).
pub fn expand_env_string(input: &str) -> String {
    expand_env_vars(input)
}

/// Expand `${VAR}` / `${env:VAR}` in a string against the injected env
/// lookup, collecting the names of variables that were undefined (expanded
/// to the empty string) so callers can attach them to diagnostics. The
/// testable core of the value-level expansion.
fn expand_env_vars_collecting_with(
    input: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    undefined: &mut Vec<String>,
) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if ch == '$' && chars.peek().map(|(_, c)| *c) == Some('{') {
            chars.next();

            let mut var_name = String::new();
            let mut found_close = false;
            for (_, inner_ch) in chars.by_ref() {
                if inner_ch == '}' {
                    found_close = true;
                    break;
                }
                var_name.push(inner_ch);
            }

            if !found_close {
                result.push_str("${");
                result.push_str(&var_name);
                continue;
            }

            let var_name = if let Some(rest) = var_name.strip_prefix("env:") {
                rest.to_string()
            } else {
                var_name
            };

            match lookup(&var_name) {
                Some(value) => result.push_str(&value),
                None => {
                    undefined.push(var_name);
                }
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Value-level expansion of one server config (adjudication A5): every
/// string *value* — transport command/args/env/cwd, url/headers/token, auth
/// and tool filters — is expanded against the process environment; object
/// keys, numbers and booleans are untouched; a single left-to-right pass
/// (an interpolated value that itself contains `${...}` is not re-expanded).
/// Undefined variables expand to the empty string and their names are
/// collected so callers can attach them to connect/test diagnostics.
///
/// This is the rust mirror of the TypeScript `expandEnvVars` value-level
/// semantics (`packages/agent-core-v2/.../env-expand.ts`); the stored and
/// wire-presented config stays raw — expansion happens at connect time only.
pub fn expand_server_config_values(config: &McpServerConfig) -> (McpServerConfig, Vec<String>) {
    expand_server_config_values_with_env(config, |name| std::env::var(name).ok())
}

/// Testable core of [`expand_server_config_values`]: the env lookup is
/// injected instead of read from the process. The single value-level
/// expansion entry — the connect layer, the `mcp.config.test` direct-config
/// path and sibling tasks (M3/M5) all expand through this predicate, so a
/// key holding `${...}` is never touched anywhere.
pub fn expand_server_config_values_with_env(
    config: &McpServerConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> (McpServerConfig, Vec<String>) {
    let mut value = serde_json::to_value(config).expect("server config always serializes to JSON");
    let mut undefined = Vec::new();
    expand_json_values_with_env(&mut value, &lookup, &mut undefined);
    let expanded: McpServerConfig =
        serde_json::from_value(value).expect("expanded server config always deserializes");
    (expanded, undefined)
}

/// The raw-entry form of [`expand_server_config_values_with_env`]: the
/// same single-pass value-level walk applied to a **stored entry's raw
/// JSON** instead of the typed config, so the wire form — the flat
/// TS/Claude shape with its passthrough keys — survives intact and only
/// string *values* expand. The `mcp.config.preview` surface (F-4, the A5
/// sole sanctioned outlet) expands through this entry: a typed round-trip
/// would normalize the stored field forms away before the user ever sees
/// the resolved values.
pub fn expand_server_config_json_with_env(
    raw: &Value,
    lookup: impl Fn(&str) -> Option<String>,
) -> (Value, Vec<String>) {
    let mut value = raw.clone();
    let mut undefined = Vec::new();
    expand_json_values_with_env(&mut value, &lookup, &mut undefined);
    (value, undefined)
}

/// Recursively expand string values in a JSON structure against the
/// injected env lookup: object *values* and array elements are walked,
/// object keys are left literal, primitives pass through.
fn expand_json_values_with_env(
    value: &mut Value,
    lookup: &dyn Fn(&str) -> Option<String>,
    undefined: &mut Vec<String>,
) {
    match value {
        Value::String(text) => {
            *text = expand_env_vars_collecting_with(text, lookup, undefined);
        }
        Value::Array(items) => {
            for item in items {
                expand_json_values_with_env(item, lookup, undefined);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                expand_json_values_with_env(item, lookup, undefined);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Path helpers (public for testing)
// ---------------------------------------------------------------------------

/// Return the path to the global MCP config file.
pub fn global_config_path(home_dir: &Path) -> PathBuf {
    home_dir.join(".mirri-code").join("mcp.json")
}

/// Return the path to the project-root MCP config file (Claude-compatible).
pub fn project_root_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".mcp.json")
}

/// Return the path to the project-local MCP config file.
pub fn local_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".mirri-code").join("mcp.json")
}

/// The project-local config path, unless it is the very same file as the
/// user layer's global config (the desktop overlap shape: `mirri web`
/// spawns with cwd=`/`, the workspace root falls back to `$HOME`, and
/// `<root>/.mirri-code/mcp.json` IS `~/.mirri-code/mcp.json`). The
/// overlapping file belongs to the user layer only — it must never double
/// as the project-local layer (re-labeling user entries as project, or
/// feeding the workspace pool with the user layer's connections).
pub fn local_layer_path_if_distinct(home_dir: &Path, cwd: &Path) -> Option<PathBuf> {
    let local = local_config_path(cwd);
    let global = global_config_path(home_dir);
    let overlaps = match (local.canonicalize(), global.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => local == global,
    };
    if overlaps { None } else { Some(local) }
}

// ---------------------------------------------------------------------------
// Management write surface (the `mcp.config.*` wire)
// ---------------------------------------------------------------------------

/// Content-addressed revision of one config layer file (FNV-1a 64 over the
/// raw file bytes, hex) — the version source for the `mcp.config.*`
/// optimistic lock (F-3, ADR-005/ADR-008). A missing file answers
/// `"absent"`. Content-addressed on purpose: mtime would churn the
/// revision on any touch even when the content is unchanged.
pub fn layer_revision(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hash: u64 = 0xcbf29ce484222325;
            for b in &bytes {
                hash ^= *b as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
            format!("{hash:016x}")
        }
        Err(_) => "absent".to_string(),
    }
}

/// The tool-filter patch `mcp.config.setToolFilter` applies to one entry:
/// a present field replaces the stored one, an absent field clears it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct McpToolFilterPatch {
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
}

/// Read one config layer as stored: the raw JSON with `${VAR}` references
/// and external field forms untouched — the management view presents the
/// stored text (expansion only happens at connect/test time, adjudication
/// A5). A missing file reads as an empty `mcpServers` envelope.
pub fn read_raw_config_file(path: &Path) -> McpResult<serde_json::Value> {
    if !path.is_file() {
        return Ok(serde_json::json!({ "mcpServers": {} }));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| {
        McpError::Config(format!("failed to read MCP config {}: {e}", path.display()))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        McpError::Config(format!(
            "failed to parse MCP config {}: {e}",
            path.display()
        ))
    })
}

/// Validate a wire-provided server config (any external transport form) and
/// upsert it as `mcpServers.<name>` unchanged — the stored form is the
/// caller's raw value, `${VAR}` references included.
///
/// Writes are atomic (temp file + rename, the token-store pattern). Note:
/// entries serialize with sorted keys (serde_json's default map); files
/// keep their full field set because only the addressed entry is touched.
pub fn upsert_server_entry(path: &Path, name: &str, config: serde_json::Value) -> McpResult<()> {
    validate_server_config(name, &config)?;
    let mut root = load_writable_root(path)?;
    root["mcpServers"][name] = config;
    persist_config_file(path, &root)
}

/// Remove `mcpServers.<name>`. Removing an absent name (or a missing file)
/// is an idempotent success; returns whether an entry was removed.
pub fn remove_server_entry(path: &Path, name: &str) -> McpResult<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut root = load_writable_root(path)?;
    let removed = root["mcpServers"]
        .as_object_mut()
        .map(|servers| servers.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        persist_config_file(path, &root)?;
    }
    Ok(removed)
}

/// Apply a tool-filter patch to `mcpServers.<name>` in place (present
/// fields replace, absent fields clear). Fails naming the server when the
/// entry is absent — the caller resolves the owning layer first. Returns
/// whether the file changed.
pub fn set_server_tool_filter(
    path: &Path,
    name: &str,
    patch: &McpToolFilterPatch,
) -> McpResult<bool> {
    let mut root = load_writable_root(path)?;
    let Some(entry) = root["mcpServers"]
        .as_object_mut()
        .and_then(|servers| servers.get_mut(name))
        .and_then(|entry| entry.as_object_mut())
    else {
        return Err(McpError::InvalidInput(format!(
            "MCP server '{name}' not found in {}",
            path.display()
        )));
    };
    let before = serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null);
    match &patch.enabled_tools {
        Some(tools) => {
            entry.insert("enabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            entry.remove("enabledTools");
        }
    }
    match &patch.disabled_tools {
        Some(tools) => {
            entry.insert("disabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            entry.remove("disabledTools");
        }
    }
    let changed = before != serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null);
    if changed {
        persist_config_file(path, &root)?;
    }
    Ok(changed)
}

/// Upsert a **filter-only override entry** at `mcpServers.<name>` — the
/// stored object carries only `enabledTools`/`disabledTools` (present
/// fields replace, absent fields clear), never a transport key (ADR-006
/// A7: the entry contributes only the filter family to the merged view;
/// the connection definition stays in the shadowed layer). The entry is
/// created when absent, all other entries of the file are preserved, and
/// the file/envelope are created when missing (the same atomic persist as
/// [`upsert_server_entry`]).
pub fn upsert_filter_override_entry(
    path: &Path,
    name: &str,
    patch: &McpToolFilterPatch,
) -> McpResult<()> {
    let mut root = load_writable_root(path)?;
    let Some(servers) = root["mcpServers"].as_object_mut() else {
        return Err(McpError::Config(format!(
            "MCP config {} has a non-object mcpServers envelope",
            path.display()
        )));
    };
    let entry = servers
        .entry(name.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(entry) = entry.as_object_mut() else {
        return Err(McpError::InvalidInput(format!(
            "MCP server '{name}' is not an object — cannot attach a filter override in {}",
            path.display()
        )));
    };
    match &patch.enabled_tools {
        Some(tools) => {
            entry.insert("enabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            entry.remove("enabledTools");
        }
    }
    match &patch.disabled_tools {
        Some(tools) => {
            entry.insert("disabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            entry.remove("disabledTools");
        }
    }
    persist_config_file(path, &root)
}

/// Deserialize (with external-form normalization) to prove the value is a
/// legal server config; the original raw value is what gets stored.
fn validate_server_config(name: &str, config: &serde_json::Value) -> McpResult<()> {
    let mut normalized = config.clone();
    normalize_server_entry(&mut normalized);
    serde_json::from_value::<McpServerConfig>(normalized).map_err(|e| {
        McpError::InvalidInput(format!("invalid MCP server config for '{name}': {e}"))
    })?;
    Ok(())
}

/// Read a config file for writing, guaranteeing a JSON object root with an
/// object `mcpServers` envelope (creating the envelope when absent).
fn load_writable_root(path: &Path) -> McpResult<serde_json::Value> {
    let mut root = read_raw_config_file(path)?;
    let Some(map) = root.as_object_mut() else {
        return Err(McpError::Config(format!(
            "MCP config {} is not a JSON object",
            path.display()
        )));
    };
    let servers = map
        .entry("mcpServers")
        .and_modify(|value| {
            if !value.is_object() {
                *value = serde_json::json!({});
            }
        })
        .or_insert_with(|| serde_json::json!({}));
    debug_assert!(
        servers.is_object(),
        "the envelope is normalized to an object"
    );
    Ok(root)
}

/// Atomically persist the config file via the shared persistence primitive
/// (unique temp file, fsync, rename, best-effort directory fsync) with a
/// pretty layout and trailing newline.
fn persist_config_file(path: &Path, root: &serde_json::Value) -> McpResult<()> {
    let mut raw = serde_json::to_string_pretty(root)
        .map_err(|e| McpError::Config(format!("failed to serialize MCP config: {e}")))?;
    raw.push('\n');
    mirri_persistence::write_file_atomically(path, raw.as_bytes()).map_err(|e| {
        McpError::Config(format!(
            "failed to persist MCP config {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_expand_variable_when_var_syntax_is_used() {
        // SAFETY: This is a test — setting an env var is safe in practice.
        unsafe { std::env::set_var("MCP_TEST_VAR", "hello") };
        assert_eq!(expand_env_string("${MCP_TEST_VAR}"), "hello");
    }

    #[test]
    fn should_expand_variable_when_env_prefix_syntax_is_used() {
        // SAFETY: Test-only env var.
        unsafe { std::env::set_var("MCP_TEST_VAR2", "world") };
        assert_eq!(expand_env_string("${env:MCP_TEST_VAR2}"), "world");
    }

    #[test]
    fn should_return_empty_string_when_variable_is_unknown() {
        assert_eq!(expand_env_string("${DEFINITELY_NOT_SET_VAR_xyz}"), "");
    }

    #[test]
    fn should_return_unchanged_text_when_no_variables_present() {
        assert_eq!(expand_env_string("plain text"), "plain text");
    }

    #[test]
    fn should_expand_variables_when_mixed_with_plain_text() {
        // SAFETY: Test-only env var.
        unsafe { std::env::set_var("MCP_TEST_VAR3", "value") };
        assert_eq!(
            expand_env_string("prefix-${MCP_TEST_VAR3}-suffix"),
            "prefix-value-suffix"
        );
    }

    #[test]
    fn should_emit_literal_when_brace_is_unclosed() {
        assert_eq!(expand_env_string("${UNCLOSED"), "${UNCLOSED");
    }

    #[test]
    fn should_use_default_values_when_optional_fields_are_none() {
        let cfg = McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: None,
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: None,
            auth: None,
        };
        assert!(cfg.is_enabled());
        assert_eq!(
            cfg.startup_timeout(),
            std::time::Duration::from_millis(30_000)
        );
        assert_eq!(cfg.tool_timeout(), std::time::Duration::from_millis(60_000));
        assert!(cfg.is_tool_allowed("any_tool"));
    }

    #[test]
    fn should_filter_tools_when_enabled_or_disabled_lists_are_set() {
        let cfg = McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: Some(false),
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            enabled_tools: Some(vec!["allowed".to_string()]),
            disabled_tools: Some(vec!["blocked".to_string()]),
            auth: None,
        };
        assert!(!cfg.is_enabled());
        assert!(cfg.is_tool_allowed("allowed"));
        assert!(!cfg.is_tool_allowed("blocked"));
        assert!(!cfg.is_tool_allowed("other"));
    }

    // -----------------------------------------------------------------------
    // TS/Claude-compatible format normalization tests
    // -----------------------------------------------------------------------

    #[test]
    fn should_infer_stdio_transport_when_command_present_and_transport_missing() {
        let json = r#"{"mcpServers": {"test": {"command": "npx", "args": ["-y", "server"]}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "server"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_infer_http_transport_when_url_present_and_transport_missing() {
        let json = r#"{"mcpServers": {"test": {"url": "https://example.com/mcp"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Http { url, .. } => {
                assert_eq!(url, "https://example.com/mcp");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn should_accept_string_transport_stdio() {
        let json = r#"{"mcpServers": {"test": {"transport": "stdio", "command": "node", "args": ["server.js"]}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "node");
                assert_eq!(args, &["server.js"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_accept_string_transport_http() {
        let json =
            r#"{"mcpServers": {"test": {"transport": "http", "url": "https://example.com/mcp"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Http { url, .. } => {
                assert_eq!(url, "https://example.com/mcp");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn should_accept_object_transport_without_modification() {
        let json = r#"{"mcpServers": {"test": {"transport": {"type": "stdio", "command": "echo", "args": []}}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "echo");
                assert!(args.is_empty());
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_default_args_to_empty_vec_when_missing() {
        let json = r#"{"mcpServers": {"test": {"command": "echo"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { command, args, .. } => {
                assert_eq!(command, "echo");
                assert!(args.is_empty());
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_default_mcp_servers_to_empty_when_missing() {
        let json = r#"{}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        assert!(file.mcp_servers.is_empty());
    }

    #[test]
    fn should_preserve_server_level_fields_when_normalizing_transport() {
        let json = r#"{"mcpServers": {"test": {"command": "npx", "enabled": false, "startupTimeoutMs": 5000, "enabledTools": ["tool_a"]}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.startup_timeout_ms, Some(5000));
        assert_eq!(
            cfg.enabled_tools.as_deref(),
            Some(&["tool_a".to_string()][..])
        );
    }

    #[test]
    fn should_move_env_into_transport_when_inferring_stdio() {
        let json = r#"{"mcpServers": {"test": {"command": "npx", "env": {"FOO": "bar"}}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { env, .. } => {
                assert_eq!(env.as_ref().unwrap().get("FOO"), Some(&"bar".to_string()));
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_move_cwd_into_transport_when_inferring_stdio() {
        let json = r#"{"mcpServers": {"test": {"command": "npx", "cwd": "/tmp/work"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { cwd, .. } => {
                assert_eq!(
                    cwd.as_ref().unwrap(),
                    &std::path::PathBuf::from("/tmp/work")
                );
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn should_move_headers_and_token_into_transport_when_inferring_http() {
        let json = r#"{"mcpServers": {"test": {"url": "https://api.example.com", "headers": {"X-Custom": "val"}, "token": "secret123"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Http {
                url,
                headers,
                token,
            } => {
                assert_eq!(url, "https://api.example.com");
                assert_eq!(
                    headers.as_ref().unwrap().get("X-Custom"),
                    Some(&"val".to_string())
                );
                assert_eq!(token.as_deref(), Some("secret123"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn should_accept_string_transport_sse() {
        let json = r#"{"mcpServers": {"test": {"transport": "sse", "url": "https://events.example.com"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Sse { url, .. } => {
                assert_eq!(url, "https://events.example.com");
            }
            other => panic!("expected Sse, got {other:?}"),
        }
    }

    #[test]
    fn should_prefer_command_over_url_when_both_present() {
        let json = r#"{"mcpServers": {"test": {"command": "echo", "url": "https://example.com"}}}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        let cfg = &file.mcp_servers["test"];
        match &cfg.transport {
            McpTransportConfig::Stdio { command, .. } => {
                assert_eq!(command, "echo");
            }
            other => panic!("expected Stdio (command wins over url), got {other:?}"),
        }
    }

    #[test]
    fn should_fail_when_server_entry_has_no_transport_command_or_url() {
        let json = r#"{"mcpServers": {"test": {"enabled": true}}}"#;
        let result: Result<McpConfigFile, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn should_fail_when_string_transport_is_unknown() {
        let json = r#"{"mcpServers": {"test": {"transport": "websocket", "url": "ws://x"}}}"#;
        let result: Result<McpConfigFile, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn should_fail_when_stdio_transport_lacks_command() {
        let json = r#"{"mcpServers": {"test": {"transport": "stdio"}}}"#;
        let result: Result<McpConfigFile, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn should_round_trip_through_serialize_and_deserialize() {
        let original = McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "server".to_string()],
                env: Some(HashMap::from([("KEY".to_string(), "val".to_string())])),
                cwd: None,
            },
            enabled: Some(true),
            startup_timeout_ms: Some(45000),
            tool_timeout_ms: None,
            enabled_tools: Some(vec!["tool1".to_string()]),
            disabled_tools: None,
            auth: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn should_parse_multiple_servers_with_mixed_formats() {
        let json = r#"{"mcpServers": {
            "svc1": {"command": "node", "args": ["a.js"]},
            "svc2": {"transport": "http", "url": "https://api.example.com"},
            "svc3": {"transport": {"type": "stdio", "command": "echo", "args": []}}
        }}"#;
        let file: McpConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.mcp_servers.len(), 3);
        assert!(file.mcp_servers.contains_key("svc1"));
        assert!(file.mcp_servers.contains_key("svc2"));
        assert!(file.mcp_servers.contains_key("svc3"));
    }

    // -----------------------------------------------------------------------
    // Layered (pool-routing) config load
    // -----------------------------------------------------------------------

    fn write_config_file(path: &std::path::Path, contents: &str) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn stdio_command(config: &McpServerConfig) -> &str {
        match &config.transport {
            McpTransportConfig::Stdio { command, .. } => command,
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    /// Given configs in all three layers, when the layered load runs, then
    /// each pool's entries keep their provenance — a project-layer name
    /// shadows the user entry in the flat view but both pools still hold
    /// their connection, and the project-local layer replaces the
    /// project-root entry of the same name.
    #[test]
    fn should_route_layers_into_user_and_project_pools_when_loading_layered() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"user-only":{"transport":"stdio","command":"u1"},"shadowed":{"transport":"stdio","command":"user-shadowed"}}}"#,
        );
        write_config_file(
            &root.path().join(".mcp.json"),
            r#"{"mcpServers":{"root-only":{"transport":"stdio","command":"r1"},"shadowed":{"transport":"stdio","command":"root-shadowed"}}}"#,
        );
        write_config_file(
            &cwd.join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"local-only":{"transport":"stdio","command":"l1"},"shadowed":{"transport":"stdio","command":"local-shadowed"}}}"#,
        );

        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();

        assert_eq!(layered.user.len(), 2);
        assert!(layered.user.contains_key("user-only"));
        assert!(layered.user.contains_key("shadowed"));

        assert_eq!(layered.project_root.len(), 2);
        assert!(layered.project_root.contains_key("root-only"));
        assert_eq!(
            stdio_command(&layered.project_root["shadowed"]),
            "root-shadowed"
        );

        assert_eq!(layered.project.len(), 2);
        assert!(layered.project.contains_key("local-only"));
        assert_eq!(
            stdio_command(&layered.project["shadowed"]),
            "local-shadowed"
        );

        // The workspace pool's connect set: project-local replaces the
        // project-root entry of the same name.
        let merged = layered.project_merged();
        assert_eq!(merged.len(), 3);
        assert_eq!(stdio_command(&merged["shadowed"]), "local-shadowed");
    }

    /// Given a cwd whose `.mirri-code/mcp.json` IS the home's user-layer
    /// file (the desktop overlap shape: `mirri web` spawns with cwd=`/`,
    /// the workspace root falls back to `$HOME`), when the layered load
    /// runs, then the project layer stays empty — the user file never
    /// doubles as the project-local layer.
    #[test]
    fn should_skip_project_local_when_it_is_the_user_layer_file() {
        let home = tempfile::tempdir().unwrap();
        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"one":{"transport":"stdio","command":"u"}}}"#,
        );

        let layered = load_mcp_config_layered(home.path(), home.path(), home.path()).unwrap();

        assert!(
            layered.project.is_empty(),
            "the user file must not load as the project layer"
        );
        assert_eq!(layered.user.len(), 1, "the user layer still loads it once");
        assert!(local_layer_path_if_distinct(home.path(), home.path()).is_none());
    }

    /// Given a normal project cwd distinct from home, when the layered
    /// load runs, then the project-local layer participates (unchanged).
    #[test]
    fn should_load_project_local_when_distinct_from_user_layer() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_config_file(
            &project.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"p":{"transport":"stdio","command":"p"}}}"#,
        );

        let layered = load_mcp_config_layered(home.path(), project.path(), project.path()).unwrap();

        assert_eq!(layered.project.len(), 1);
        assert!(local_layer_path_if_distinct(home.path(), project.path()).is_some());
    }

    /// Given a user-layer complete entry with a project-local filter-only
    /// override of the same name, when the user pool's connect set is
    /// computed, then the connection family stays the user layer's while
    /// the override's filter replaces the filter family — and a complete
    /// project entry of a user name never redefines the user pool's
    /// connection (that name is workspace-owned, spec §4.2).
    #[test]
    fn should_bind_the_project_filter_override_to_the_user_pool_connect_set() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{
                "db": {"transport":"stdio","command":"user-cmd"},
                "svc": {"transport":"stdio","command":"user-svc","enabledTools":["svc.a"]}
            }}"#,
        );
        write_config_file(
            &cwd.join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{
                "db": {"disabledTools":["db.query"]},
                "svc": {"transport":"stdio","command":"local-svc"},
                "local-only": {"transport":"stdio","command":"l1"}
            }}"#,
        );

        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let user_pool = layered.user_merged();

        // The filter-only override binds to the user connection: the
        // transport stays the user layer's, the filter comes from the local
        // override.
        assert_eq!(stdio_command(&user_pool["db"]), "user-cmd");
        assert_eq!(
            user_pool["db"].disabled_tools,
            Some(vec!["db.query".to_string()])
        );
        // A complete local entry of a user name never redefines the user
        // pool's connection nor contributes its filter — the name is
        // workspace-owned (spec §4.2), the user pool keeps its own layer's
        // definition.
        assert_eq!(stdio_command(&user_pool["svc"]), "user-svc");
        assert_eq!(
            user_pool["svc"].enabled_tools,
            Some(vec!["svc.a".to_string()])
        );
        // A project-only complete entry belongs to the workspace pool
        // alone.
        assert!(!user_pool.contains_key("local-only"));
    }

    /// Given a filter-only entry in a lower layer with the complete entry
    /// of the same name in a higher layer, when the A7 merge runs, then the
    /// complete entry inherits the held filter (the family merge no longer
    /// depends on layer order) — unless it explicitly carries its own
    /// filter, and the last explicit filter carrier wins across the held
    /// chain. A filter-only name with no complete entry anywhere still
    /// does not surface.
    #[test]
    fn should_bind_a_held_lower_layer_filter_to_the_later_complete_entry() {
        // Case 1: user filter-only + project-root complete (no filter of
        // its own) — the root entry inherits the user layer's filter.
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"db": {"disabledTools":["db.query"]}}}"#,
        );
        write_config_file(
            &root.path().join(".mcp.json"),
            r#"{"mcpServers":{"db": {"transport":"stdio","command":"root-cmd"}}}"#,
        );

        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let merged = merge_layers(&layered);
        assert_eq!(stdio_command(&merged["db"]), "root-cmd");
        assert_eq!(
            merged["db"].disabled_tools,
            Some(vec!["db.query".to_string()]),
            "the later complete entry inherits the held lower-layer filter"
        );

        // Case 2: the later complete entry explicitly carries its own
        // filter — the explicit later carrier wins, the held one is
        // discarded.
        write_config_file(
            &root.path().join(".mcp.json"),
            r#"{"mcpServers":{"db": {"transport":"stdio","command":"root-cmd","enabledTools":["db.get"]}}}"#,
        );
        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let merged = merge_layers(&layered);
        assert_eq!(merged["db"].enabled_tools, Some(vec!["db.get".to_string()]));
        assert!(
            merged["db"].disabled_tools.is_none(),
            "the held filter does not leak into an explicit later carrier"
        );

        // Case 3: two held filter-only carriers (user, then root) before
        // the local complete entry — the last explicit carrier (root)
        // binds.
        write_config_file(
            &root.path().join(".mcp.json"),
            r#"{"mcpServers":{"db": {"enabledTools":["root.filter"]}}}"#,
        );
        write_config_file(
            &cwd.join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"db": {"transport":"stdio","command":"local-cmd"}}}"#,
        );
        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let merged = merge_layers(&layered);
        assert_eq!(stdio_command(&merged["db"]), "local-cmd");
        assert_eq!(
            merged["db"].enabled_tools,
            Some(vec!["root.filter".to_string()]),
            "the last explicit filter carrier before the complete entry wins"
        );

        // Case 4: the same held-binding shape through the project merge —
        // a project-root filter-only entry binds to the local complete
        // entry (the workspace pool's connect set).
        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let project = layered.project_merged();
        assert_eq!(stdio_command(&project["db"]), "local-cmd");
        assert_eq!(
            project["db"].enabled_tools,
            Some(vec!["root.filter".to_string()]),
            "the workspace connect set inherits the held root filter"
        );

        // Case 5: a filter-only name with no complete entry anywhere
        // stays a non-connection.
        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"ghost": {"disabledTools":["ghost.all"]}}}"#,
        );
        let layered = load_mcp_config_layered(home.path(), root.path(), &cwd).unwrap();
        let merged = merge_layers(&layered);
        assert!(
            !merged.contains_key("ghost"),
            "a filter-only name with no complete entry does not surface"
        );
    }

    /// Given no config files anywhere, when the layered load runs, then
    /// both pools come back empty (no error for absent layers).
    #[test]
    fn should_return_empty_pools_when_no_config_files_exist() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();

        let layered = load_mcp_config_layered(home.path(), root.path(), root.path()).unwrap();

        assert!(layered.user.is_empty());
        assert!(layered.project.is_empty());
    }

    /// Given the layered load, when the flat view is requested, then
    /// project entries replace same-name user entries — the historical
    /// `load_mcp_config` per-server merge, preserved over the refactor.
    #[test]
    fn should_keep_the_flat_merge_when_loading_config() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        write_config_file(
            &home.path().join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"user-only":{"transport":"stdio","command":"u1"},"shadowed":{"transport":"stdio","command":"user-shadowed"}}}"#,
        );
        write_config_file(
            &cwd.join(".mirri-code").join("mcp.json"),
            r#"{"mcpServers":{"shadowed":{"transport":"stdio","command":"local-shadowed"}}}"#,
        );

        let merged = load_mcp_config(home.path(), root.path(), &cwd).unwrap();

        assert_eq!(merged.len(), 2);
        assert_eq!(stdio_command(&merged["user-only"]), "u1");
        assert_eq!(stdio_command(&merged["shadowed"]), "local-shadowed");
    }

    // -----------------------------------------------------------------------
    // Management write surface (the `mcp.config.*` wire)
    // -----------------------------------------------------------------------

    /// Given an existing config with one server, when a new entry is
    /// upserted, then the file gains the entry and the existing server —
    /// including its `${VAR}` reference — survives verbatim.
    #[test]
    fn should_add_entries_and_preserve_existing_ones_when_upserting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"existing":{"transport":"stdio","command":"node","args":["${HOME}/bin/server"]}}}"#,
        )
        .unwrap();

        upsert_server_entry(
            &path,
            "fresh",
            serde_json::json!({"transport":"stdio","command":"npx","args":["-y","server"]}),
        )
        .unwrap();

        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            file["mcpServers"]["existing"]["args"][0], "${HOME}/bin/server",
            "the existing entry keeps its ${{VAR}} reference verbatim"
        );
        assert_eq!(file["mcpServers"]["fresh"]["command"], "npx");
    }

    /// Given no config file at all, when an entry is upserted, then the
    /// file is created with the `mcpServers` envelope (parent directories
    /// included).
    #[test]
    fn should_create_the_file_with_the_envelope_when_it_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("mcp.json");

        upsert_server_entry(
            &path,
            "fresh",
            serde_json::json!({"transport":"http","url":"https://example.test/mcp"}),
        )
        .unwrap();

        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            file["mcpServers"]["fresh"]["url"],
            "https://example.test/mcp"
        );
    }

    /// Given an invalid server config (a stdio transport without a command),
    /// when upserting, then the write is rejected naming the server and the
    /// file is left untouched.
    #[test]
    fn should_reject_the_write_and_leave_the_file_untouched_when_the_config_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        let original = r#"{"mcpServers":{"existing":{"transport":"stdio","command":"node"}}}"#;
        std::fs::write(&path, original).unwrap();

        let error = upsert_server_entry(&path, "broken", serde_json::json!({"transport":"stdio"}))
            .unwrap_err();

        assert!(
            error.to_string().contains("broken"),
            "error names the server: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the file is untouched after a rejected write"
        );
    }

    /// Given a config entry, when it is removed, then the key disappears;
    /// removing an absent name (or a missing file) stays an idempotent
    /// success.
    #[test]
    fn should_remove_entries_and_stay_idempotent_when_the_name_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"one":{"transport":"stdio","command":"a"},"two":{"transport":"stdio","command":"b"}}}"#,
        )
        .unwrap();

        assert!(remove_server_entry(&path, "one").unwrap());
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(file["mcpServers"].is_object());
        assert!(file["mcpServers"].get("one").is_none());
        assert!(file["mcpServers"]["two"].is_object());

        assert!(!remove_server_entry(&path, "one").unwrap());
        assert!(!remove_server_entry(&path, "never-there").unwrap());
        assert!(!remove_server_entry(&tmp.path().join("absent.json"), "any").unwrap());
    }

    /// Given an entry with existing filter fields, when a tool-filter patch
    /// is applied, then present fields replace the stored ones, absent
    /// fields are cleared, and the entry's other fields stay untouched.
    #[test]
    fn should_replace_present_fields_and_clear_absent_ones_when_patching_tool_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"srv":{"transport":"stdio","command":"node","enabledTools":["old"],"disabledTools":["gone"]}}}"#,
        )
        .unwrap();

        let changed = set_server_tool_filter(
            &path,
            "srv",
            &McpToolFilterPatch {
                enabled_tools: Some(vec!["a".to_string(), "b".to_string()]),
                disabled_tools: None,
            },
        )
        .unwrap();

        assert!(changed, "the filter patch changed the file");
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            file["mcpServers"]["srv"]["enabledTools"],
            serde_json::json!(["a", "b"])
        );
        assert!(
            file["mcpServers"]["srv"].get("disabledTools").is_none(),
            "an absent patch field clears the stored one"
        );
        assert_eq!(file["mcpServers"]["srv"]["command"], "node");
    }

    /// Given no entry for the patched name, when a tool filter is set, then
    /// the write fails naming the server (the caller resolves the owning
    /// layer before patching).
    #[test]
    fn should_fail_naming_the_server_when_the_patched_entry_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");

        let error =
            set_server_tool_filter(&path, "missing", &McpToolFilterPatch::default()).unwrap_err();

        assert!(
            error.to_string().contains("missing"),
            "error names the server: {error}"
        );
    }

    /// Given a config file, when it is read raw, then `${VAR}` references
    /// and the external transport forms survive — no expansion, no
    /// normalization (the wire presents the stored text, adjudication A5);
    /// a missing file reads as an empty envelope.
    #[test]
    fn should_preserve_references_and_forms_when_reading_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        let stored = r#"{"mcpServers":{"srv":{"command":"node","args":["${HOME}/x"]}}}"#;
        std::fs::write(&path, stored).unwrap();

        let raw = read_raw_config_file(&path).unwrap();
        assert_eq!(
            raw["mcpServers"]["srv"]["args"][0], "${HOME}/x",
            "the raw read never expands references"
        );
        assert!(
            raw["mcpServers"]["srv"].get("transport").is_none(),
            "the raw read never normalizes transport forms"
        );

        let missing = read_raw_config_file(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(missing["mcpServers"], serde_json::json!({}));
    }

    // -----------------------------------------------------------------------
    // Layer revision (the `mcp.config.*` optimistic lock, F-3)
    // -----------------------------------------------------------------------

    /// Given a config file with known bytes, when its layer revision is
    /// computed, then identical content hashes identically, changed content
    /// flips the hash, and a missing file answers "absent".
    #[test]
    fn should_derive_a_content_addressed_hash_when_layer_revision_is_computed() {
        // Given a file with a fixed content
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();

        // When the revision is computed twice over the same bytes
        let first = layer_revision(&path);
        let second = layer_revision(&path);

        // Then the revision is a stable content hash (16 hex chars) and a
        // content change flips it; a missing file answers "absent"
        assert_eq!(first, second, "identical bytes hash identically");
        assert_eq!(first.len(), 16, "a u64 in hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        std::fs::write(
            &path,
            r#"{"mcpServers":{"a":{"transport":"stdio","command":"x"}}}"#,
        )
        .unwrap();
        assert_ne!(
            first,
            layer_revision(&path),
            "content change flips the revision"
        );

        assert_eq!(layer_revision(&tmp.path().join("absent.json")), "absent");
    }
}
