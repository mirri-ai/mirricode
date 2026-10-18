//! Plugin manifest parsing and validation.
//!
//! A plugin manifest (`mirri.plugin.json`, `kimi.plugin.json`, or the
//! `.mirri-plugin/plugin.json` / `.kimi-plugin/plugin.json` directory
//! variants) describes the resources a plugin contributes: skills, agents,
//! commands, MCP servers, hooks, and system prompts.
//!
//! Parsing follows the TS v2 benchmark's **tolerant model** (B2,
//! `packages/agent-core-v2/src/app/plugin/manifest.ts`): a readable JSON
//! manifest never fails wholesale. Field problems produce
//! [`PluginDiagnostic`]s, and [`parse_manifest`] returns the survivor
//! manifest (if any) together with the diagnostics. Only a missing
//! manifest, an unreadable file, or invalid JSON leaves `manifest` as
//! `None` (with an `error` diagnostic), mirroring the `ParsedManifestResult`
//! the TS `parseManifest` returns.
//!
//! [`validate_manifest`] checks field constraints on an already-parsed
//! manifest and [`validate_path_in_root`] guards against path traversal.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use mirri_core::hook_engine::HookDef;
use regex::Regex;
use serde::Deserialize;
use serde::Deserializer;
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::commands::{
    CommandEntry, CommandFile, CommandSpec, command_name_from_file, is_md_file_name,
    list_markdown_files_recursive,
};
use crate::error::PluginResult;

/// Maximum allowed byte size of a rendered `systemPrompt` part (inline or
/// file, 32 KiB) — the TS `PLUGIN_SYSTEM_PROMPT_MAX_BYTES` (B6).
pub const SYSTEM_PROMPT_MAX_BYTES: usize = 32 * 1024;

/// Plugin name rule — the TS `PLUGIN_NAME_REGEX` (B3).
const PLUGIN_NAME_RE_STR: &str = r"^[a-z0-9][a-z0-9_-]{0,63}$";

static PLUGIN_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(PLUGIN_NAME_RE_STR).expect("hardcoded name regex is valid")
});

/// Command-name rule for inline command specs: the historical (pre-B3)
/// `[a-zA-Z0-9_-]+` rule, kept for the Rust-only inline `CommandSpec`
/// extension (D-B9).
const COMMAND_NAME_RE_STR: &str = r"^[a-zA-Z0-9_-]+$";

static COMMAND_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(COMMAND_NAME_RE_STR).expect("hardcoded command regex is valid")
});

/// Runtime fields the host recognises but does not support — the TS
/// `UNSUPPORTED_RUNTIME_FIELDS` (B2). Their presence is reported as an
/// `info` diagnostic, never a failure.
pub const UNSUPPORTED_RUNTIME_FIELDS: &[&str] = &[
    "tools",
    "apps",
    "inject",
    "configFile",
    "config_file",
    "bootstrap",
];

/// Diagnostic severity — the TS `PluginDiagnosticSeverity` (B2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginDiagnosticSeverity {
    /// A problem that marks the plugin `state: 'error'` (B2).
    Error,
    /// A degradation that drops one contribution without failing the plugin.
    Warn,
    /// Informational notice (e.g. an unsupported runtime field).
    Info,
}

/// One manifest parse/validation issue — the TS `PluginDiagnostic`
/// (`{ severity, message }`, camelCase serde, B2/K).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDiagnostic {
    /// Severity of the issue.
    pub severity: PluginDiagnosticSeverity,
    /// Human-readable description.
    pub message: String,
}

impl PluginDiagnostic {
    /// An `error`-severity diagnostic.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: PluginDiagnosticSeverity::Error,
            message: message.into(),
        }
    }

    /// A `warn`-severity diagnostic.
    pub fn warn(message: impl Into<String>) -> Self {
        Self {
            severity: PluginDiagnosticSeverity::Warn,
            message: message.into(),
        }
    }

    /// An `info`-severity diagnostic.
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: PluginDiagnosticSeverity::Info,
            message: message.into(),
        }
    }
}

/// Plugin health — the TS `PluginState` (`'ok' | 'error'`, B2/K).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginState {
    /// No `error`-severity diagnostics and a present manifest.
    Ok,
    /// An `error`-severity diagnostic, or no manifest at all.
    Error,
}

/// Manifest location kind — the TS `PluginManifestKind` (L).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PluginManifestKind {
    /// `mirri.plugin.json` / `kimi.plugin.json` at the plugin root.
    #[serde(rename = "kimi-plugin-root")]
    Root,
    /// `.mirri-plugin/plugin.json` / `.kimi-plugin/plugin.json`.
    #[serde(rename = "kimi-plugin-dir")]
    Dir,
}

/// The result of [`parse_manifest`] — the TS `ParsedManifestResult`: the
/// best-effort manifest (absent when the file could not be read/parsed or
/// the name is unusable), the manifest location, and the diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ParsedManifest {
    /// The parsed manifest, when the JSON was readable and the name is
    /// usable. `None` mirrors the TS cases where `ParsedManifestResult`
    /// carries no `manifest` (missing/unreadable/invalid JSON, and
    /// missing/invalid `name`).
    pub manifest: Option<PluginManifest>,
    /// Where the manifest lives (`kimi-plugin-root` or `kimi-plugin-dir`).
    pub kind: Option<PluginManifestKind>,
    /// The resolved manifest file path.
    pub path: Option<PathBuf>,
    /// A shadowed directory candidate (`mirri.plugin.json` +
    /// `.mirri-plugin/plugin.json`, or the kimi pair), if any.
    pub shadowed_path: Option<PathBuf>,
    /// Every issue found while parsing, in encounter order.
    pub diagnostics: Vec<PluginDiagnostic>,
}

impl ParsedManifest {
    /// Whether any diagnostic has `error` severity — the TS summary
    /// `hasErrors` predicate (K).
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == PluginDiagnosticSeverity::Error)
    }

    /// The TS `recordFrom` state derivation: `error` when the manifest is
    /// missing or an error diagnostic is present, `ok` otherwise (B2/K).
    pub fn state(&self) -> PluginState {
        if self.manifest.is_none() || self.has_errors() {
            PluginState::Error
        } else {
            PluginState::Ok
        }
    }
}

/// Authentication scheme marker — the TS `auth: z.literal('oauth')` (B7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerAuth {
    OAuth,
}

/// stdio executor — the TS `executor: z.enum(['local', 'kaos'])` (B7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerExecutor {
    Local,
    Kaos,
}

/// Fields shared by every transport — the TS `McpServerCommonFields` (B7).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerCommonFields {
    /// Whether the server is enabled. `None` means the default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Startup timeout in milliseconds (the TS `McpTimeoutMsSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    /// Per-tool-call timeout in milliseconds (the TS `McpTimeoutMsSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    /// Allowlist of tool names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist of tool names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
    /// OAuth auth marker (`auth: 'oauth'`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<McpServerAuth>,
}

/// stdio transport config — the TS `McpServerStdioConfigSchema` (B7).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStdioConfig {
    /// The command to spawn. A bare PATH command (e.g. `node`, `uvx`) or, after
    /// normalization, an absolute path inside the plugin root for `./`-relative
    /// entries (TS `normalizePluginMcpServer`).
    pub command: String,
    /// Command-line arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Environment overrides (all string values, the TS `StringRecordSchema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// Working directory (after normalization, an absolute path inside the
    /// plugin root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Execution backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<McpServerExecutor>,
    /// Common server options.
    #[serde(flatten)]
    pub common: McpServerCommonFields,
}

/// http/sse transport config — the TS `McpServerHttpConfigSchema` /
/// `McpServerSseConfigSchema` (B7).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRemoteConfig {
    /// The endpoint URL (a valid http(s) URL).
    pub url: String,
    /// Extra HTTP headers (all string values).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Name of the environment variable holding the bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    /// Common server options.
    #[serde(flatten)]
    pub common: McpServerCommonFields,
}

/// One validated MCP server config — the TS `McpServerConfig`
/// (discriminated union on `transport`, B7). Built by [`parse_mcp_server`],
/// the zod `McpServerConfigSchema` mirror.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum McpServerConfig {
    /// `transport: "stdio"` — spawn a command.
    Stdio(McpStdioConfig),
    /// `transport: "http"` — streamable HTTP transport.
    Http(McpRemoteConfig),
    /// `transport: "sse"` — SSE transport.
    Sse(McpRemoteConfig),
}

impl<'de> serde::Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = JsonValue::deserialize(deserializer)?;
        parse_mcp_server(&value).map_err(serde::de::Error::custom)
    }
}

/// The zod `McpServerConfigSchema` mirror (B7): infer the `transport` tag
/// from `command`/`url` when absent (the TS `inferTransportTag`), then
/// validate against the transport's field requirements (the TS
/// `McpServerConfigDiscriminatedSchema`). Returns a human-readable reason
/// on failure so the caller can emit the TS
/// `Invalid MCP server "<name>": <reason>` warn diagnostic.
pub fn parse_mcp_server(value: &JsonValue) -> Result<McpServerConfig, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "expected an object".to_string())?;
    let transport: &str = match obj.get("transport") {
        Some(JsonValue::String(s)) => s.as_str(),
        Some(_) => return Err("\"transport\" must be a string".to_string()),
        None => {
            if obj.get("command").is_some_and(JsonValue::is_string) {
                "stdio"
            } else if obj.get("url").is_some_and(JsonValue::is_string) {
                "http"
            } else {
                return Err(
                    "expected a \"command\" (stdio) or \"url\" (http) field".to_string(),
                );
            }
        }
    };
    match transport {
        "stdio" => parse_mcp_stdio(obj),
        "http" => parse_mcp_remote(obj, TransportTag::Http),
        "sse" => parse_mcp_remote(obj, TransportTag::Sse),
        other => Err(format!(
            "\"transport\" must be \"stdio\", \"http\", or \"sse\" (got \"{other}\")"
        )),
    }
}

#[derive(Clone, Copy)]
enum TransportTag {
    Http,
    Sse,
}

fn parse_mcp_stdio(obj: &JsonMap<String, JsonValue>) -> Result<McpServerConfig, String> {
    let command = obj
        .get("command")
        .ok_or_else(|| "\"command\" is required".to_string())?
        .as_str()
        .ok_or_else(|| "\"command\" must be a string".to_string())?;
    if command.is_empty() {
        return Err("\"command\" must not be empty".to_string());
    }
    let args = optional_string_array(obj, "args")?;
    let env = optional_string_map(obj, "env")?;
    let cwd = optional_string(obj, "cwd")?;
    let executor = match optional_string(obj, "executor")? {
        Some(value) if value == "local" => Some(McpServerExecutor::Local),
        Some(value) if value == "kaos" => Some(McpServerExecutor::Kaos),
        Some(other) => {
            return Err(format!(
                "\"executor\" must be \"local\" or \"kaos\" (got \"{other}\")"
            ));
        }
        None => None,
    };
    let common = parse_common_fields(obj)?;
    Ok(McpServerConfig::Stdio(McpStdioConfig {
        command: command.to_string(),
        args,
        env,
        cwd,
        executor,
        common,
    }))
}

fn parse_mcp_remote(
    obj: &JsonMap<String, JsonValue>,
    tag: TransportTag,
) -> Result<McpServerConfig, String> {
    let url = obj
        .get("url")
        .ok_or_else(|| "\"url\" is required".to_string())?
        .as_str()
        .ok_or_else(|| "\"url\" must be a string".to_string())?;
    if !is_valid_http_url(url) {
        return Err("\"url\" must be a valid http(s) URL".to_string());
    }
    let headers = optional_string_map(obj, "headers")?;
    let bearer_token_env_var = match optional_string(obj, "bearerTokenEnvVar")? {
        Some(value) if value.is_empty() => {
            return Err("\"bearerTokenEnvVar\" must not be empty".to_string());
        }
        other => other,
    };
    let common = parse_common_fields(obj)?;
    let remote = McpRemoteConfig {
        url: url.to_string(),
        headers,
        bearer_token_env_var,
        common,
    };
    Ok(match tag {
        TransportTag::Http => McpServerConfig::Http(remote),
        TransportTag::Sse => McpServerConfig::Sse(remote),
    })
}

/// The TS `McpServerCommonFields` validation: `enabled` boolean, the two
/// bounded integer timeouts (`McpTimeoutMsSchema`), the tool-filter string
/// arrays, and `auth: 'oauth'` (B7).
fn parse_common_fields(
    obj: &JsonMap<String, JsonValue>,
) -> Result<McpServerCommonFields, String> {
    let enabled = optional_boolean(obj, "enabled")?;
    let startup_timeout_ms = optional_bounded_uint(obj, "startupTimeoutMs")?;
    let tool_timeout_ms = optional_bounded_uint(obj, "toolTimeoutMs")?;
    let enabled_tools = optional_string_array(obj, "enabledTools")?;
    let disabled_tools = optional_string_array(obj, "disabledTools")?;
    let auth = match optional_string(obj, "auth")? {
        Some(value) if value == "oauth" => Some(McpServerAuth::OAuth),
        Some(other) => {
            return Err(format!("\"auth\" must be \"oauth\" (got \"{other}\")"));
        }
        None => None,
    };
    Ok(McpServerCommonFields {
        enabled,
        startup_timeout_ms,
        tool_timeout_ms,
        enabled_tools,
        disabled_tools,
        auth,
    })
}

/// The TS `MAX_MCP_TIMEOUT_MS` (`2_147_483_647`).
const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;

fn optional_boolean(obj: &JsonMap<String, JsonValue>, key: &str) -> Result<Option<bool>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(JsonValue::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("\"{key}\" must be a boolean")),
    }
}

fn optional_string(obj: &JsonMap<String, JsonValue>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(JsonValue::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("\"{key}\" must be a string")),
    }
}

fn optional_string_array(
    obj: &JsonMap<String, JsonValue>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(JsonValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let JsonValue::String(s) = item else {
                    return Err(format!("\"{key}\" must be an array of strings"));
                };
                out.push(s.clone());
            }
            Ok(Some(out))
        }
        Some(_) => Err(format!("\"{key}\" must be an array of strings")),
    }
}

fn optional_string_map(
    obj: &JsonMap<String, JsonValue>,
    key: &str,
) -> Result<Option<HashMap<String, String>>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(JsonValue::Object(map)) => {
            let mut out = HashMap::with_capacity(map.len());
            for (k, v) in map {
                let JsonValue::String(s) = v else {
                    return Err(format!("\"{key}\" must be an object of string values"));
                };
                out.insert(k.clone(), s.clone());
            }
            Ok(Some(out))
        }
        Some(_) => Err(format!("\"{key}\" must be an object of string values")),
    }
}

/// `startupTimeoutMs`/`toolTimeoutMs`: positive integers capped at
/// `MAX_MCP_TIMEOUT_MS` (the TS `McpTimeoutMsSchema`, B7).
fn optional_bounded_uint(
    obj: &JsonMap<String, JsonValue>,
    key: &str,
) -> Result<Option<u64>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(JsonValue::Number(n)) => match n.as_u64() {
            Some(v) if (1..=MAX_MCP_TIMEOUT_MS).contains(&v) => Ok(Some(v)),
            _ => Err(format!(
                "\"{key}\" must be an integer between 1 and {MAX_MCP_TIMEOUT_MS}"
            )),
        },
        Some(_) => Err(format!(
            "\"{key}\" must be an integer between 1 and {MAX_MCP_TIMEOUT_MS}"
        )),
    }
}

/// WHATWG `new URL(...)` (+ http(s) protocol) — the zod v4
/// `z.string().url()` semantics behind the TS `McpServerConfigSchema` (B7).
/// A pragmatic check: a `scheme://` split with an http/https scheme and a
/// non-empty, whitespace-free authority.
fn is_valid_http_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    let host = rest.split(['/', '#', '?']).next().unwrap_or("");
    !host.is_empty() && !host.contains(char::is_whitespace)
}

/// The `sessionStart` field: an object selecting a skill to start a session
/// with — the TS `PluginSessionStart` (`{ skill: string }`, B4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginSessionStart {
    /// Skill path/name to start the session with.
    pub skill: String,
}

/// The `interface` field: presentation metadata about the plugin — the TS
/// `PluginInterface` (B5). All fields are optional; an object with none of
/// the recognized fields is treated as absent (TS `readInterface`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInterface {
    /// Display name shown in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// One-line summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    /// Longer description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_description: Option<String>,
    /// The developers' name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_name: Option<String>,
    /// The plugin's website URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
}

/// The `author` field: `{name?, email?}` — the TS `PluginAuthor` (B10). A
/// plain-string author is normalized to `{name}` (TS `readAuthor`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginAuthor {
    /// Author name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Author email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Plugin manifest (`mirri.plugin.json`, `kimi.plugin.json`, or the
/// `.mirri-plugin/plugin.json` / `.kimi-plugin/plugin.json` variants).
///
/// Every field except `name` is optional; a minimal plugin is just a name.
/// Parsed best-effort by [`parse_manifest`]: shape problems degrade to
/// "absent" with a diagnostic (B2) instead of failing the manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginManifest {
    /// Plugin identifier. Must match the TS `PLUGIN_NAME_REGEX`
    /// `^[a-z0-9][a-z0-9_-]{0,63}$` (B3).
    pub name: String,
    /// Semantic version string (e.g. `"1.0.0"`). Not parsed — stored as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Skill directories (B8): `"./"`-relative entries resolved to absolute
    /// realpath'd paths at parse time (the TS `resolveDirListField` values);
    /// an absent field infers the plugin root when `SKILL.md` exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Agent directories (B8): `"./"`-relative entries resolved to absolute
    /// realpath'd paths at parse time; an absent field infers
    /// `<root>/agents` when the directory exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    /// Skill to start a session with, if any (B4).
    #[serde(
        rename = "sessionStart",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_session_start"
    )]
    pub session_start: Option<PluginSessionStart>,
    /// MCP server configurations keyed by server name. Each entry is
    /// validated against the TS `McpServerConfigSchema` (B7): invalid
    /// entries are dropped with a `warn` diagnostic, never fatal.
    #[serde(
        rename = "mcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
    /// External command hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<HookDef>>,
    /// Command entries (B9): TS markdown file entries (`{path, name}`,
    /// absolute resolved paths) and/or Rust-only inline command specs with
    /// prompt templates (D-B9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<CommandEntry>>,
    /// The **rendered** system prompt: the inline `systemPrompt` and the
    /// `systemPromptPath` file content, merged with `"\n\n"` (B6); an
    /// over-limit part is ignored with a `warn` diagnostic. The raw path is
    /// kept in [`Self::system_prompt_path`].
    #[serde(
        rename = "systemPrompt",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_trimmed_string"
    )]
    pub system_prompt: Option<String>,
    /// The raw `systemPromptPath` value (trimmed), as contributed. The
    /// rendered content lives in [`Self::system_prompt`].
    #[serde(
        rename = "systemPromptPath",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_trimmed_string"
    )]
    pub system_prompt_path: Option<String>,
    /// Presentation metadata (B5).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_interface"
    )]
    pub interface: Option<PluginInterface>,
    /// Keywords describing the plugin (B10).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_string_array"
    )]
    pub keywords: Option<Vec<String>>,
    /// Author metadata (B10).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_author"
    )]
    pub author: Option<PluginAuthor>,
    /// Plugin homepage URL (B10).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_trimmed_string"
    )]
    pub homepage: Option<String>,
    /// Plugin license identifier (B10).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_trimmed_string"
    )]
    pub license: Option<String>,
    /// Instructions to attach to the plugin's skill roots (B10; consumed by
    /// a later batch).
    #[serde(
        rename = "skillInstructions",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_trimmed_string"
    )]
    pub skill_instructions: Option<String>,
}

/// Candidate manifest paths, in TS preference order (B1):
/// `mirri.plugin.json` > `.mirri-plugin/plugin.json` > `kimi.plugin.json` >
/// `.kimi-plugin/plugin.json`. The directory candidates are `plugin.json`
/// inside the respective hidden directory.
const MANIFEST_CANDIDATES: &[&str] = &[
    "mirri.plugin.json",
    ".mirri-plugin/plugin.json",
    "kimi.plugin.json",
    ".kimi-plugin/plugin.json",
];

/// Parse the plugin manifest from a plugin root directory, TS-style (B2).
///
/// Tries, in TS order (B1):
/// 1. `<plugin_root>/mirri.plugin.json`
/// 2. `<plugin_root>/.mirri-plugin/plugin.json`
/// 3. `<plugin_root>/kimi.plugin.json`
/// 4. `<plugin_root>/.kimi-plugin/plugin.json`
///
/// The return is always a [`ParsedManifest`], never an error: field-level
/// problems surface as diagnostics, and only a missing manifest, an
/// unreadable file, or invalid JSON leaves `manifest` as `None` — the TS
/// `parseManifest` contract.
pub fn parse_manifest(plugin_root: &Path) -> ParsedManifest {
    // The TS caller passes a realpath'd plugin root into `parseManifest`
    // (install-time normalization). Canonicalize the root once here so
    // every `./`-relative resolution and containment check share one base
    // (e.g. macOS `/var` → `/private/var`); a failed canonicalization
    // (missing root) falls back to the given path.
    let plugin_root =
        std::fs::canonicalize(plugin_root).unwrap_or_else(|_| plugin_root.to_path_buf());

    let mirr_root_path = plugin_root.join("mirri.plugin.json");
    let mirr_dir_path = plugin_root.join(".mirri-plugin/plugin.json");
    let kimi_root_path = plugin_root.join("kimi.plugin.json");
    let kimi_dir_path = plugin_root.join(".kimi-plugin/plugin.json");

    let mirr_root_exists = mirr_root_path.is_file();
    let mirr_dir_exists = mirr_dir_path.is_file();
    let kimi_root_exists = kimi_root_path.is_file();
    let kimi_dir_exists = kimi_dir_path.is_file();

    if !mirr_root_exists && !mirr_dir_exists && !kimi_root_exists && !kimi_dir_exists {
        return ParsedManifest {
            diagnostics: vec![PluginDiagnostic::error(
                "No manifest at mirri.plugin.json, .mirri-plugin/plugin.json, kimi.plugin.json, or .kimi-plugin/plugin.json",
            )],
            ..ParsedManifest::default()
        };
    }

    let manifest_path = if mirr_root_exists {
        mirr_root_path
    } else if mirr_dir_exists {
        mirr_dir_path.clone()
    } else if kimi_root_exists {
        kimi_root_path
    } else {
        kimi_dir_path.clone()
    };
    let kind = if manifest_path == mirr_dir_path || manifest_path == kimi_dir_path {
        PluginManifestKind::Dir
    } else {
        PluginManifestKind::Root
    };
    let shadowed_path = if mirr_root_exists && mirr_dir_exists {
        Some(mirr_dir_path)
    } else if kimi_root_exists && kimi_dir_exists {
        Some(kimi_dir_path)
    } else {
        None
    };

    let rel = manifest_path
        .strip_prefix(&plugin_root)
        .unwrap_or(&manifest_path)
        .to_path_buf();
    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(error) => {
            return ParsedManifest {
                kind: Some(kind),
                path: Some(manifest_path),
                shadowed_path,
                diagnostics: vec![PluginDiagnostic::error(format!(
                    "Failed to parse {}: {error}",
                    rel.display()
                ))],
                ..ParsedManifest::default()
            };
        }
    };
    let value: JsonValue = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            return ParsedManifest {
                kind: Some(kind),
                path: Some(manifest_path),
                shadowed_path,
                diagnostics: vec![PluginDiagnostic::error(format!(
                    "Failed to parse {}: {error}",
                    rel.display()
                ))],
                ..ParsedManifest::default()
            };
        }
    };
    let JsonValue::Object(map) = value else {
        return ParsedManifest {
            kind: Some(kind),
            path: Some(manifest_path),
            shadowed_path,
            diagnostics: vec![PluginDiagnostic::error("manifest must be a JSON object")],
            ..ParsedManifest::default()
        };
    };

    let mut diagnostics: Vec<PluginDiagnostic> = Vec::new();

    // `name` — the TS `parseManifest` name handling (B2/B3): a missing or
    // invalid name is an `error` diagnostic with no survivor manifest.
    let name = match map.get("name") {
        Some(JsonValue::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    if name.is_empty() {
        diagnostics.push(PluginDiagnostic::error("\"name\" is required"));
        return ParsedManifest {
            manifest: None,
            kind: Some(kind),
            path: Some(manifest_path),
            shadowed_path,
            diagnostics,
        };
    }
    if !PLUGIN_NAME_RE.is_match(&name) {
        diagnostics.push(PluginDiagnostic::error(format!(
            "\"name\" must match {PLUGIN_NAME_RE_STR} (got \"{name}\")"
        )));
        return ParsedManifest {
            manifest: None,
            kind: Some(kind),
            path: Some(manifest_path),
            shadowed_path,
            diagnostics,
        };
    }

    let manifest = PluginManifest {
        name: name.clone(),
        version: trimmed_field(&map, "version"),
        description: trimmed_field(&map, "description"),
        skills: read_skill_roots(&plugin_root, &map, &mut diagnostics),
        agents: read_agent_roots(&plugin_root, &map, &mut diagnostics),
        session_start: read_session_start(map.get("sessionStart"), &mut diagnostics),
        mcp_servers: read_mcp_servers(&plugin_root, map.get("mcpServers"), &mut diagnostics),
        hooks: read_hooks(map.get("hooks"), &mut diagnostics),
        commands: read_commands(&plugin_root, map.get("commands"), &mut diagnostics),
        system_prompt: read_system_prompt(&plugin_root, &map, &mut diagnostics),
        system_prompt_path: trimmed_field(&map, "systemPromptPath"),
        interface: read_interface(map.get("interface")),
        keywords: string_array_field(map.get("keywords")),
        author: read_author(map.get("author")),
        homepage: trimmed_field(&map, "homepage"),
        license: trimmed_field(&map, "license"),
        skill_instructions: trimmed_field(&map, "skillInstructions"),
    };
    record_unsupported_runtime_fields(&map, &mut diagnostics);

    ParsedManifest {
        manifest: Some(manifest),
        kind: Some(kind),
        path: Some(manifest_path),
        shadowed_path,
        diagnostics,
    }
}

/// Locate the manifest file inside `plugin_root`.
///
/// Returns the first existing candidate path (TS priority order, B1), or an
/// error if none of the four candidates exist.
pub fn find_manifest_path(plugin_root: &Path) -> PluginResult<PathBuf> {
    for name in MANIFEST_CANDIDATES {
        let candidate = plugin_root.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(crate::error::PluginError::InvalidManifest(format!(
        "no manifest file found in plugin root: {}",
        plugin_root.display()
    )))
}

/// Validate manifest field constraints.
///
/// Checks:
/// - `name` matches the TS `PLUGIN_NAME_REGEX` (B3).
/// - Inline command specs follow the historical command-name rule (D-B9).
///
/// `systemPrompt`/`systemPromptPath` coexistence and size are no longer
/// hard errors: rendering is lenient (B6), and `interface` is an object
/// with no validation (B5).
pub fn validate_manifest(manifest: &PluginManifest) -> PluginResult<()> {
    // Name format (TS `PLUGIN_NAME_REGEX`, B3).
    if manifest.name.is_empty() {
        return Err(crate::error::PluginError::InvalidManifest(
            "plugin name must not be empty".to_string(),
        ));
    }
    if !PLUGIN_NAME_RE.is_match(&manifest.name) {
        return Err(crate::error::PluginError::InvalidManifest(format!(
            "\"name\" must match {} (got \"{}\")",
            PLUGIN_NAME_RE_STR, manifest.name
        )));
    }

    // Inline command specs: name and template are required, and the name
    // must follow the historical command character rules.
    if let Some(commands) = &manifest.commands {
        for entry in commands {
            let CommandEntry::Inline(spec) = entry else {
                continue;
            };
            if spec.name.is_empty() {
                return Err(crate::error::PluginError::InvalidManifest(
                    "command name must not be empty".to_string(),
                ));
            }
            if !COMMAND_NAME_RE.is_match(&spec.name) {
                return Err(crate::error::PluginError::InvalidManifest(format!(
                    "command name '{}' contains invalid characters (allowed: a-z A-Z 0-9 _ -)",
                    spec.name
                )));
            }
            if spec.template.is_empty() {
                return Err(crate::error::PluginError::InvalidManifest(format!(
                    "command '{}' template must not be empty",
                    spec.name
                )));
            }
        }
    }

    Ok(())
}

/// Ensure that `path` (resolved against `root`) does not escape `root`.
///
/// This prevents path-traversal attacks via manifest fields that accept
/// relative paths (skills, agents, commands, systemPromptPath, sessionStart).
///
/// The check normalises the joined path and verifies that it starts with
/// the canonical root. Symlinks are not followed.
pub fn validate_path_in_root(path: &Path, root: &Path) -> PluginResult<()> {
    // Join the path to root (handles both absolute and relative inputs).
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    // Normalise by stripping `.` and `..` components without touching the
    // filesystem (so non-existent paths can still be validated).
    let normalised = normalise_path(&joined);
    let normalised_root = normalise_path(root);

    if normalised.starts_with(&normalised_root) {
        Ok(())
    } else {
        Err(crate::error::PluginError::PathOutsideRoot(format!(
            "path '{}' resolves outside plugin root '{}'",
            path.display(),
            root.display()
        )))
    }
}

/// Lexically normalise a path by removing `.` components and collapsing `..`.
///
/// This mirrors `std::path::Path::canonicalize` but does not touch the
/// filesystem, so it works for paths that do not yet exist.
fn normalise_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => { /* skip `.` */ }
            Component::ParentDir => {
                // Pop the last normal component if possible.
                // Never pop past a root prefix.
                match components.last() {
                    Some(Component::Normal(_)) => {
                        components.pop();
                    }
                    _ => {
                        // Root or empty — keep `..` so it stays visible.
                        components.push(component);
                    }
                }
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
}

// ---------------------------------------------------------------------------
// Field readers — the TS `read*` semantics (B2 diagnostics channel)
// ---------------------------------------------------------------------------

/// The TS `readSessionStart` (B4): an object whose `skill` is a non-blank
/// string; anything else is ignored with a `warn` diagnostic.
fn read_session_start(
    raw: Option<&JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<PluginSessionStart> {
    let Some(JsonValue::Object(map)) = raw else {
        if raw.is_some() {
            diagnostics.push(PluginDiagnostic::warn("\"sessionStart\" must be an object"));
        }
        return None;
    };
    let Some(skill) = trimmed_field(map, "skill") else {
        diagnostics.push(PluginDiagnostic::warn(
            "\"sessionStart.skill\" is required when sessionStart is present",
        ));
        return None;
    };
    Some(PluginSessionStart { skill })
}

/// The TS `readSystemPrompt` (B6): inline and file parts may coexist, each
/// is capped at 32 KiB (over-limit is a `warn` + ignored), and the parts are
/// joined with `"\n\n"`.
fn read_system_prompt(
    plugin_root: &Path,
    map: &JsonMap<String, JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if map
        .get("systemPrompt")
        .is_some_and(|value| !value.is_string())
    {
        diagnostics.push(PluginDiagnostic::warn("\"systemPrompt\" must be a string"));
    }
    if let Some(inline) = trimmed_field(map, "systemPrompt") {
        let bytes = inline.len();
        if bytes > SYSTEM_PROMPT_MAX_BYTES {
            diagnostics.push(PluginDiagnostic::warn(format!(
                "\"systemPrompt\" is {bytes} bytes, exceeding the {} KB limit; the field is ignored",
                SYSTEM_PROMPT_MAX_BYTES / 1024
            )));
        } else {
            parts.push(inline);
        }
    }

    if let Some(path_value) = map.get("systemPromptPath") {
        if !path_value.is_string() {
            diagnostics
                .push(PluginDiagnostic::warn("\"systemPromptPath\" must be a string"));
        } else {
            let trimmed = path_value.as_str().expect("checked above").trim();
            if trimmed.is_empty() {
                diagnostics
                    .push(PluginDiagnostic::warn("\"systemPromptPath\" must not be blank"));
            } else if let Some(resolved) =
                resolve_plugin_path_field(plugin_root, "systemPromptPath", trimmed, diagnostics)
            {
                match std::fs::metadata(&resolved) {
                    Ok(meta) if !meta.is_file() => {
                        diagnostics.push(PluginDiagnostic::warn(format!(
                            "\"systemPromptPath\" is not a file ({path_value})"
                        )));
                    }
                    Ok(meta) if meta.len() as usize > SYSTEM_PROMPT_MAX_BYTES => {
                        diagnostics.push(PluginDiagnostic::warn(format!(
                            "\"systemPromptPath\" is {} bytes, exceeding the {} KB limit; the file is ignored ({path_value})",
                            meta.len(),
                            SYSTEM_PROMPT_MAX_BYTES / 1024
                        )));
                    }
                    Ok(_) => match std::fs::read_to_string(&resolved) {
                        Ok(content) => {
                            let content = normalize_prompt_content(&content);
                            if !content.is_empty() {
                                parts.push(content);
                            }
                        }
                        Err(error) => diagnostics.push(PluginDiagnostic::warn(format!(
                            "Failed to read \"systemPromptPath\" ({path_value}): {error}"
                        ))),
                    },
                    Err(_) => diagnostics.push(PluginDiagnostic::warn(format!(
                        "\"systemPromptPath\" is not a file ({path_value})"
                    ))),
                }
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Strip a leading UTF-8 BOM and trim whitespace, mirroring the TS
/// `readSystemPrompt` file normalization
/// (`content.replace(/^\uFEFF/, '').trim()`).
fn normalize_prompt_content(content: &str) -> String {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .trim()
        .to_string()
}

/// The TS `readMcpServers` (B7): a non-object is a `warn`; each entry is
/// validated against the zod `McpServerConfigSchema` (bad entries are
/// `warn` + dropped), then normalized (`./` command/cwd resolution). An
/// empty result collapses to `None`.
fn read_mcp_servers(
    plugin_root: &Path,
    raw: Option<&JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<HashMap<String, McpServerConfig>> {
    let raw = raw?;
    let JsonValue::Object(map) = raw else {
        diagnostics.push(PluginDiagnostic::warn("\"mcpServers\" must be an object"));
        return None;
    };

    let mut out: HashMap<String, McpServerConfig> = HashMap::new();
    for (name, value) in map {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            diagnostics.push(PluginDiagnostic::warn(
                "\"mcpServers\" entries must have a non-empty name",
            ));
            continue;
        }
        let config = match parse_mcp_server(value) {
            Ok(config) => config,
            Err(reason) => {
                diagnostics.push(PluginDiagnostic::warn(format!(
                    "Invalid MCP server \"{trimmed_name}\": {reason}"
                )));
                continue;
            }
        };
        if let Some(normalized) =
            normalize_plugin_mcp_server(plugin_root, trimmed_name, config, diagnostics)
        {
            out.insert(trimmed_name.to_string(), normalized);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The TS `normalizePluginMcpServer` (B7): for stdio transports, a
/// `./`-prefixed `command` resolves to an absolute path inside the plugin
/// root; any other path with a slash is rejected; `cwd` uses the same
/// `./`-relative resolution. HTTP/SSE configs pass through untouched.
fn normalize_plugin_mcp_server(
    plugin_root: &Path,
    name: &str,
    config: McpServerConfig,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<McpServerConfig> {
    let McpServerConfig::Stdio(mut stdio) = config else {
        return Some(config);
    };

    if stdio.command.starts_with("./") {
        let resolved = resolve_plugin_path_field(
            plugin_root,
            &format!("mcpServers.{name}.command"),
            &stdio.command,
            diagnostics,
        )?;
        stdio.command = resolved;
    } else if stdio.command.contains('/') || Path::new(&stdio.command).is_absolute() {
        diagnostics.push(PluginDiagnostic::warn(format!(
            "\"mcpServers.{name}.command\" must be a PATH command or start with \"./\""
        )));
        return None;
    }

    if let Some(cwd) = stdio.cwd.clone() {
        let resolved = resolve_plugin_path_field(
            plugin_root,
            &format!("mcpServers.{name}.cwd"),
            &cwd,
            diagnostics,
        )?;
        stdio.cwd = Some(resolved);
    }

    Some(McpServerConfig::Stdio(stdio))
}

/// The TS `resolvePluginPathField`: a `./`-prefixed path resolves to an
/// absolute (realpath'd where possible) path inside the plugin root. Any
/// other form, or an outside-root resolution, is a `warn` + `None`.
fn resolve_plugin_path_field(
    plugin_root: &Path,
    field: &str,
    value: &str,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<String> {
    if !value.starts_with("./") {
        diagnostics.push(PluginDiagnostic::warn(format!(
            "\"{field}\" path must start with \"./\" (got \"{value}\")"
        )));
        return None;
    }
    let Some(real) = resolve_within_root(plugin_root, value) else {
        diagnostics.push(PluginDiagnostic::warn(format!(
            "\"{field}\" path resolves outside the plugin ({value})"
        )));
        return None;
    };
    Some(real.to_string_lossy().into_owned())
}

/// The TS `path.resolve(pluginRoot, value)` + `realpath` + `isWithin`
/// pipeline (with a lexical fallback when realpath fails): the joined
/// absolute path is normalized (`.`/`..` collapsed, like `path.resolve`),
/// realpath'd where possible, and checked against the canonical plugin
/// root. Returns the resolved path when it is inside the root.
fn resolve_within_root(plugin_root: &Path, value: &str) -> Option<PathBuf> {
    let absolute = normalise_path(&plugin_root.join(value));
    let real = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    let root_real =
        std::fs::canonicalize(plugin_root).unwrap_or_else(|_| plugin_root.to_path_buf());
    is_within(&real, &root_real).then_some(real)
}

/// The TS `isWithin`: `child` equals `parent` or is lexically below it.
fn is_within(child: &Path, parent: &Path) -> bool {
    if child == parent {
        return true;
    }
    match child.strip_prefix(parent) {
        Ok(rel) => rel.components().next() != Some(Component::ParentDir) && !rel.as_os_str().is_empty(),
        Err(_) => false,
    }
}

/// The TS `readHooks`: an array of hook defs; entries that fail validation
/// are dropped with a `warn` diagnostic (per-index).
fn read_hooks(
    raw: Option<&JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<Vec<HookDef>> {
    let raw = raw?;
    let JsonValue::Array(items) = raw else {
        diagnostics.push(PluginDiagnostic::warn("\"hooks\" must be an array"));
        return None;
    };
    let mut out: Vec<HookDef> = Vec::new();
    for (index, entry) in items.iter().enumerate() {
        match serde_json::from_value::<HookDef>(entry.clone()) {
            Ok(hook) => out.push(hook),
            Err(error) => diagnostics.push(PluginDiagnostic::warn(format!(
                "Invalid hook at index {index}: {error}"
            ))),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The TS `resolveDirListField` mirror (B8) for `skills`/`agents`: the field
/// accepts a string or a string[], each entry must start with `"./"`,
/// resolve (realpath'd) inside the plugin root, and name an existing
/// directory. Violations are `error` diagnostics with the entry dropped —
/// except a missing directory, which is a `warn` — exactly the TS
/// severities. Returns the **resolved absolute paths** (the values the TS
/// stores in `manifest.skills` / `manifest.agents`).
fn resolve_dir_list_field(
    plugin_root: &Path,
    field: &str,
    raw: Option<&JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let entries: Vec<&str> = match raw {
        JsonValue::String(s) => vec![s.as_str()],
        JsonValue::Array(items) if items.iter().all(|item| item.is_string()) => items
            .iter()
            .map(|item| item.as_str().expect("checked above"))
            .collect(),
        _ => {
            diagnostics.push(PluginDiagnostic::error(format!(
                "\"{field}\" must be a string or string[]"
            )));
            return Vec::new();
        }
    };

    let mut resolved: Vec<String> = Vec::new();
    for entry in entries {
        if !entry.starts_with("./") {
            diagnostics.push(PluginDiagnostic::error(format!(
                "\"{field}\" path must start with \"./\" (got \"{entry}\")"
            )));
            continue;
        }
        let Some(real) = resolve_within_root(plugin_root, entry) else {
            diagnostics.push(PluginDiagnostic::error(format!(
                "\"{field}\" path resolves outside the plugin ({entry})"
            )));
            continue;
        };
        if !real.is_dir() {
            diagnostics.push(PluginDiagnostic::warn(format!(
                "\"{field}\" path is not a directory ({entry})"
            )));
            continue;
        }
        resolved.push(real.to_string_lossy().into_owned());
    }
    resolved
}

/// The `skills` field with the TS default inference (B8): an absent field
/// with a `SKILL.md` file at the plugin root yields `[pluginRoot]` — the
/// inference produces no diagnostics.
fn read_skill_roots(
    plugin_root: &Path,
    map: &JsonMap<String, JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<Vec<String>> {
    let mut skills = resolve_dir_list_field(plugin_root, "skills", map.get("skills"), diagnostics);
    if !map.contains_key("skills") && plugin_root.join("SKILL.md").is_file() {
        // The TS caller's plugin root is realpath'd (install-time
        // normalization); canonicalize the managed root here so the
        // inferred root stays consistent with the resolved entries (e.g.
        // macOS `/var` → `/private/var`).
        let root_real =
            std::fs::canonicalize(plugin_root).unwrap_or_else(|_| plugin_root.to_path_buf());
        skills = vec![root_real.to_string_lossy().into_owned()];
    }
    (!skills.is_empty()).then_some(skills)
}

/// The `agents` field with the TS default inference (B8): an absent field
/// with an `agents/` directory at the plugin root yields `[pluginRoot/agents]`
/// — the inference produces no diagnostics.
fn read_agent_roots(
    plugin_root: &Path,
    map: &JsonMap<String, JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<Vec<String>> {
    let mut agents = resolve_dir_list_field(plugin_root, "agents", map.get("agents"), diagnostics);
    if !map.contains_key("agents") && plugin_root.join("agents").is_dir() {
        let root_real =
            std::fs::canonicalize(plugin_root).unwrap_or_else(|_| plugin_root.to_path_buf());
        agents = vec![root_real.join("agents").to_string_lossy().into_owned()];
    }
    (!agents.is_empty()).then_some(agents)
}

/// The TS `readCommands` (string or string[], B9), extended with the
/// Rust-only inline `CommandSpec` entries (D-B9): a pure string field
/// collects markdown files and directories (recursive `.md` collection,
/// relative-path naming, name-sorted); an array mixing in objects activates
/// the inline-spec extension. Wrong shapes and invalid entries are `warn` +
/// dropped, never fatal.
fn read_commands(
    plugin_root: &Path,
    raw: Option<&JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Option<Vec<CommandEntry>> {
    let raw = raw?;
    let entries: Vec<&JsonValue> = match raw {
        JsonValue::String(_) => vec![raw],
        JsonValue::Array(items) => items.iter().collect(),
        _ => {
            diagnostics.push(PluginDiagnostic::warn(
                "\"commands\" must be a string or string[]",
            ));
            return None;
        }
    };

    // A pure string/string[] field follows the TS path exactly; an array
    // mixing in objects additionally accepts inline specs (D-B9).
    let all_strings = entries.iter().all(|entry| entry.is_string());
    let mut md_files: Vec<CommandFile> = Vec::new();
    let mut inline: Vec<CommandEntry> = Vec::new();

    for entry in entries {
        match entry {
            JsonValue::String(path) => {
                let Some(resolved) =
                    resolve_plugin_path_field(plugin_root, "commands", path, diagnostics)
                else {
                    continue;
                };
                let resolved = PathBuf::from(resolved);
                if resolved.is_dir() {
                    md_files.extend(list_markdown_files_recursive(&resolved));
                } else if resolved.is_file() && is_md_file_name(&resolved) {
                    md_files.push(CommandFile {
                        path: resolved.clone(),
                        name: command_name_from_file(
                            &resolved,
                            resolved.parent().unwrap_or(plugin_root),
                        ),
                    });
                } else {
                    diagnostics.push(PluginDiagnostic::warn(format!(
                        "\"commands\" entry must be a directory or .md file ({path})"
                    )));
                }
            }
            JsonValue::Object(_) if !all_strings => {
                match serde_json::from_value::<CommandSpec>(entry.clone()) {
                    Ok(spec) => {
                        if spec.name.is_empty() {
                            diagnostics.push(PluginDiagnostic::warn(
                                "command name must not be empty; dropping the entry",
                            ));
                            continue;
                        }
                        if !COMMAND_NAME_RE.is_match(&spec.name) {
                            diagnostics.push(PluginDiagnostic::warn(format!(
                                "command name '{}' contains invalid characters (allowed: a-z A-Z 0-9 _ -); dropping the entry",
                                spec.name
                            )));
                            continue;
                        }
                        if spec.template.is_empty() {
                            diagnostics.push(PluginDiagnostic::warn(format!(
                                "command '{}' template must not be empty; dropping the entry",
                                spec.name
                            )));
                            continue;
                        }
                        inline.push(CommandEntry::Inline(spec));
                    }
                    Err(error) => diagnostics.push(PluginDiagnostic::warn(format!(
                        "invalid command entry: {error}"
                    ))),
                }
            }
            _ => {
                diagnostics.push(PluginDiagnostic::warn(
                    "\"commands\" entries must be strings or inline command specs",
                ));
            }
        }
    }

    // The TS readCommands sorts the collected md entries by name.
    md_files.sort_by(|left, right| left.name.cmp(&right.name));
    if md_files.is_empty() && inline.is_empty() {
        return None;
    }
    let mut out: Vec<CommandEntry> = md_files.into_iter().map(CommandEntry::File).collect();
    out.extend(inline);
    Some(out)
}

/// The TS `readInterface` (B5): an object whose recognized fields are kept;
/// anything else — including an object with no recognized fields — is
/// silently `None` (the TS reader emits no diagnostics for `interface`).
fn read_interface(raw: Option<&JsonValue>) -> Option<PluginInterface> {
    let Some(JsonValue::Object(map)) = raw else {
        return None;
    };
    let interface = PluginInterface {
        display_name: trimmed_field(map, "displayName"),
        short_description: trimmed_field(map, "shortDescription"),
        long_description: trimmed_field(map, "longDescription"),
        developer_name: trimmed_field(map, "developerName"),
        website_url: trimmed_field(map, "websiteURL"),
    };
    let has_any = interface.display_name.is_some()
        || interface.short_description.is_some()
        || interface.long_description.is_some()
        || interface.developer_name.is_some()
        || interface.website_url.is_some();
    has_any.then_some(interface)
}

/// The TS `readAuthor` (B10): a string becomes `{name}`, an object
/// contributes `name`/`email`, and anything else is `None`.
fn read_author(raw: Option<&JsonValue>) -> Option<PluginAuthor> {
    match raw {
        Some(JsonValue::String(s)) => {
            let name = s.trim();
            (!name.is_empty()).then(|| PluginAuthor {
                name: Some(name.to_string()),
                email: None,
            })
        }
        Some(JsonValue::Object(map)) => {
            let name = trimmed_field(map, "name");
            let email = trimmed_field(map, "email");
            (name.is_some() || email.is_some()).then_some(PluginAuthor { name, email })
        }
        _ => None,
    }
}

/// The TS `recordUnsupportedRuntimeFields` (B2): every present
/// `UNSUPPORTED_RUNTIME_FIELDS` key gets an `info` diagnostic.
fn record_unsupported_runtime_fields(
    map: &JsonMap<String, JsonValue>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) {
    for field in UNSUPPORTED_RUNTIME_FIELDS {
        if map.contains_key(*field) {
            diagnostics.push(PluginDiagnostic::info(format!(
                "\"{field}\" is present but not supported by Kimi plugins"
            )));
        }
    }
}

/// A trimmed, non-blank string field inside a JSON object (TS `stringField`).
fn trimmed_field(map: &JsonMap<String, JsonValue>, key: &str) -> Option<String> {
    let JsonValue::String(s) = map.get(key)? else {
        return None;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A string or string[] field (TS `stringArrayField`); any other shape is
/// `None` (skills/agents entry-resolution diagnostics arrive with B8).
fn string_array_field(raw: Option<&JsonValue>) -> Option<Vec<String>> {
    let Some(JsonValue::Array(items)) = raw else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let JsonValue::String(s) = item else {
            return None;
        };
        out.push(s.clone());
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Lenient field deserialization for the derive path (direct
// `serde_json::from_value::<PluginManifest>` use, e.g. validate tests)
// ---------------------------------------------------------------------------

/// Deserialize a trimmed, non-blank string field (TS `stringField`): any
/// non-string value, or a string that is blank after trimming, yields
/// `None` instead of failing the manifest.
fn de_trimmed_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::String(s)) = value else {
        return Ok(None);
    };
    let trimmed = s.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    })
}

/// Deserialize `keywords` (TS `stringArrayField`): an array of strings kept
/// verbatim; anything else — including an array with a non-string entry —
/// yields `None`.
fn de_string_array<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Array(items)) = value else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let serde_json::Value::String(s) = item else {
            return Ok(None);
        };
        out.push(s);
    }
    Ok(Some(out))
}

/// Deserialize `author` (TS `readAuthor`): a string becomes `{name}`, an
/// object contributes `name`/`email`, and anything else is `None`.
fn de_author<'de, D>(deserializer: D) -> Result<Option<PluginAuthor>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::String(s) => {
            let name = s.trim();
            Ok(if name.is_empty() {
                None
            } else {
                Some(PluginAuthor {
                    name: Some(name.to_string()),
                    email: None,
                })
            })
        }
        serde_json::Value::Object(map) => {
            let name = trimmed_field(&map, "name");
            let email = trimmed_field(&map, "email");
            Ok(if name.is_none() && email.is_none() {
                None
            } else {
                Some(PluginAuthor { name, email })
            })
        }
        _ => Ok(None),
    }
}

/// Deserialize `sessionStart` (TS `readSessionStart`, B4): an object whose
/// `skill` is a non-blank string; anything else is ignored rather than
/// failing the manifest.
fn de_session_start<'de, D>(deserializer: D) -> Result<Option<PluginSessionStart>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Object(map)) = value else {
        return Ok(None);
    };
    let Some(skill) = trimmed_field(&map, "skill") else {
        return Ok(None);
    };
    Ok(Some(PluginSessionStart { skill }))
}

/// Deserialize `interface` (TS `readInterface`, B5): an object whose
/// recognized fields are kept; anything else — including an object with no
/// recognized fields — is `None`.
fn de_interface<'de, D>(deserializer: D) -> Result<Option<PluginInterface>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(serde_json::Value::Object(map)) = value else {
        return Ok(None);
    };
    let interface = PluginInterface {
        display_name: trimmed_field(&map, "displayName"),
        short_description: trimmed_field(&map, "shortDescription"),
        long_description: trimmed_field(&map, "longDescription"),
        developer_name: trimmed_field(&map, "developerName"),
        website_url: trimmed_field(&map, "websiteURL"),
    };
    let has_any = interface.display_name.is_some()
        || interface.short_description.is_some()
        || interface.long_description.is_some()
        || interface.developer_name.is_some()
        || interface.website_url.is_some();
    Ok(has_any.then_some(interface))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_manifest_when_name_is_valid() {
        let manifest = PluginManifest {
            name: "my-plugin_1".to_string(),
            version: None,
            description: None,
            skills: None,
            agents: None,
            session_start: None,
            mcp_servers: None,
            hooks: None,
            commands: None,
            system_prompt: None,
            system_prompt_path: None,
            interface: None,
            keywords: None,
            author: None,
            homepage: None,
            license: None,
            skill_instructions: None,
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn should_serialize_mcp_config_with_transport_tag() {
        // The B7 typed config must serialize back to the same wire shape
        // the manager hands to the MCP layer (`{"transport": "stdio", ...}`).
        let config = McpServerConfig::Stdio(McpStdioConfig {
            command: "node".to_string(),
            args: None,
            env: Some(HashMap::from([("KEY".to_string(), "value".to_string())])),
            cwd: None,
            executor: None,
            common: McpServerCommonFields {
                enabled: Some(true),
                startup_timeout_ms: Some(5000),
                tool_timeout_ms: Some(30000),
                enabled_tools: Some(vec!["tool-a".to_string()]),
                disabled_tools: None,
                auth: None,
            },
        });
        let value = serde_json::to_value(&config).expect("config serializes");
        let obj = value.as_object().expect("config is an object");
        assert_eq!(obj["transport"], serde_json::json!("stdio"));
        assert_eq!(obj["command"], serde_json::json!("node"));
        assert_eq!(obj["startupTimeoutMs"], serde_json::json!(5000));
        assert_eq!(obj["toolTimeoutMs"], serde_json::json!(30000));
        assert_eq!(obj["enabledTools"], serde_json::json!(["tool-a"]));
        assert_eq!(obj["env"], serde_json::json!({"KEY": "value"}));
    }

    #[test]
    fn should_parse_mcp_server_when_command_field_is_present_without_transport() {
        // The TS `inferTransportTag`: a `command` implies stdio.
        let value = serde_json::json!({ "command": "node", "args": ["server.js"] });
        let config = parse_mcp_server(&value).expect("parse should succeed");
        let McpServerConfig::Stdio(stdio) = &config else {
            panic!("expected stdio transport");
        };
        assert_eq!(stdio.command, "node");
        assert_eq!(stdio.args.as_deref(), Some(&["server.js".to_string()][..]));
    }

    #[test]
    fn should_reject_mcp_server_when_command_is_empty() {
        let value = serde_json::json!({ "command": "" });
        let error = parse_mcp_server(&value).expect_err("empty command must fail");
        assert!(error.contains("\"command\" must not be empty"));
    }

    #[test]
    fn should_reject_mcp_server_when_transport_is_unknown() {
        let value = serde_json::json!({ "transport": "ws", "url": "https://x.test" });
        let error = parse_mcp_server(&value).expect_err("unknown transport must fail");
        assert!(error.contains("\"transport\" must be"));
    }

    #[test]
    fn should_reject_mcp_server_when_url_is_not_http() {
        let value = serde_json::json!({ "transport": "http", "url": "ftp://x.test" });
        let error = parse_mcp_server(&value).expect_err("ftp url must fail");
        assert!(error.contains("\"url\" must be a valid http(s) URL"));
    }

    #[test]
    fn should_reject_mcp_server_when_url_is_malformed() {
        for url in ["not a url", "http://", "https://has space.test", "//x.test"] {
            let value = serde_json::json!({ "transport": "sse", "url": url });
            assert!(
                parse_mcp_server(&value).is_err(),
                "url {url:?} should be rejected"
            );
        }
    }

    #[test]
    fn should_accept_mcp_server_when_url_is_http() {
        let value = serde_json::json!({ "transport": "http", "url": "https://mcp.test/stream" });
        let config = parse_mcp_server(&value).expect("parse should succeed");
        let McpServerConfig::Http(remote) = &config else {
            panic!("expected http transport");
        };
        assert_eq!(remote.url, "https://mcp.test/stream");
    }

    #[test]
    fn should_reject_mcp_server_when_timeout_is_zero_or_non_integer() {
        for timeout in [serde_json::json!(0), serde_json::json!(1.5), serde_json::json!(-1)] {
            let value =
                serde_json::json!({ "command": "node", "startupTimeoutMs": timeout });
            assert!(
                parse_mcp_server(&value).is_err(),
                "timeout {timeout} should be rejected"
            );
        }
    }

    #[test]
    fn should_reject_mcp_server_when_env_value_is_not_a_string() {
        let value = serde_json::json!({ "command": "node", "env": { "KEY": 42 } });
        assert!(parse_mcp_server(&value).is_err());
    }

    #[test]
    fn should_reject_manifest_when_name_is_empty() {
        let manifest = PluginManifest {
            name: "".to_string(),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn should_reject_manifest_when_name_contains_spaces() {
        let manifest = PluginManifest {
            name: "my plugin".to_string(),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn should_reject_manifest_when_name_contains_uppercase_letters() {
        let manifest = PluginManifest {
            name: "MyPlugin".to_string(),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn should_accept_manifest_when_name_is_exactly_64_characters() {
        let manifest = PluginManifest {
            name: format!("a{}", "a".repeat(63)),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn should_reject_manifest_when_name_exceeds_64_characters() {
        let manifest = PluginManifest {
            name: format!("a{}", "a".repeat(64)),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn should_accept_manifest_when_both_system_prompt_fields_are_set() {
        let manifest = PluginManifest {
            system_prompt: Some("hello".to_string()),
            system_prompt_path: Some("prompt.txt".to_string()),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn should_accept_manifest_when_system_prompt_exceeds_max_size() {
        // Over-limit is a render-time degrade (B6), not a parse error.
        let manifest = PluginManifest {
            system_prompt: Some("x".repeat(SYSTEM_PROMPT_MAX_BYTES + 1)),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn should_accept_manifest_when_interface_is_an_object() {
        let manifest = PluginManifest {
            interface: Some(PluginInterface {
                display_name: Some("My Plugin".to_string()),
                ..PluginInterface::default()
            }),
            ..blank_manifest()
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    fn blank_manifest() -> PluginManifest {
        PluginManifest {
            name: "test".to_string(),
            version: None,
            description: None,
            skills: None,
            agents: None,
            session_start: None,
            mcp_servers: None,
            hooks: None,
            commands: None,
            system_prompt: None,
            system_prompt_path: None,
            interface: None,
            keywords: None,
            author: None,
            homepage: None,
            license: None,
            skill_instructions: None,
        }
    }

    #[test]
    fn should_accept_path_when_it_is_inside_root() {
        let root = Path::new("/plugins/my-plugin");
        assert!(validate_path_in_root(Path::new("skills/foo"), root).is_ok());
        assert!(validate_path_in_root(Path::new("agents/bar"), root).is_ok());
    }

    #[test]
    fn should_reject_path_when_it_traverses_outside_root() {
        let root = Path::new("/plugins/my-plugin");
        assert!(validate_path_in_root(Path::new("../../etc/passwd"), root).is_err());
        assert!(validate_path_in_root(Path::new("skills/../../etc"), root).is_err());
    }

    #[test]
    fn should_reject_path_when_absolute_path_is_outside_root() {
        let root = Path::new("/plugins/my-plugin");
        assert!(validate_path_in_root(Path::new("/etc/passwd"), root).is_err());
    }
}