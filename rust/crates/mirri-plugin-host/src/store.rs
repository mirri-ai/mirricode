//! Persistence of installed-plugin records (`installed.json`).
//!
//! [`PluginStore`] is the single read/write surface for the on-disk plugin
//! registry.  It stores the TS v1 `{version: 1, plugins: [...]}` envelope at
//! `<plugins_dir>/installed.json` (item A6 — a camelCase record per the TS
//! `InstalledRecord` shape, so a TS engine and this crate can share a home
//! directory):
//!
//! ```json
//! {
//!   "version": 1,
//!   "plugins": [
//!     {
//!       "id": "demo-plugin",
//!       "root": "/home/u/.mirri/plugins/managed/demo-plugin",
//!       "source": "local-path",
//!       "enabled": true,
//!       "installedAt": "2026-01-01T00:00:00+00:00",
//!       "updatedAt": "2026-01-01T00:00:00+00:00",
//!       "originalSource": "/path/to/source",
//!       "capabilities": { "mcpServers": { "helper": { "enabled": true } } },
//!       "github": { "owner": "u", "repo": "r", "ref": { "kind": "branch", "value": "main" } }
//!     }
//!   ]
//! }
//! ```
//!
//! **Read compatibility**: the store also loads the pre-A6 bare-array format
//! (an array of snake_case records with `{type, repo, commit, kind}`
//! sources, flat `github_ref`/`installed_sha`/`mcp_servers` fields) and
//! migrates it on the next write.  Matching the TS `readInstalled`, the
//! `version` key is not validated (only the `plugins` array is required), so
//! TS-written files parse regardless of the version field.  All operations
//! are synchronous because the file is small and writes are infrequent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{PluginError, PluginResult};
use crate::github::GithubRef;
use crate::source::PluginSource;

/// Filename for the on-disk plugin registry.
const STORE_FILENAME: &str = "installed.json";

/// User-controlled per-server state for a plugin-contributed MCP server
/// (adjudication A1: the plugin owns the connection definition; the user
/// owns enablement and the tool filters).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginMcpServerState {
    /// Whether the server is enabled. Defaults to `true` when no record
    /// exists (the manifest's own `enabled` applies then).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Allowlist of tool names (`present` replaces the stored one, `None`
    /// leaves the manifest value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// Denylist of tool names (`present` replaces the stored one, `None`
    /// leaves the manifest value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

fn default_enabled() -> bool {
    true
}

/// A single installed-plugin record, persisted to `installed.json`.
///
/// The in-memory struct keeps the historical (snake_case) field names; its
/// serialization is the TS v1 record shape (A6): `{id, root, source,
/// enabled, installedAt, updatedAt?, originalSource?, capabilities?,
/// github?}` — camelCase keys, `source` as the `"local-path" | "zip-url" |
/// "github"` literal, per-server state under `capabilities.mcpServers`, and
/// github attribution under `github: {owner, repo, ref, installedSha?}`.
/// `version` is deliberately not persisted (the TS `InstalledRecord` has no
/// version field — it is derived from the manifest at load time).
///
/// Deserialization accepts both the v1 record and the pre-A6 snake_case
/// record (migration).
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    /// Plugin identifier (same as manifest `name`).
    pub id: String,
    /// Semantic version (from manifest, if present).  In-memory only — the
    /// TS `InstalledRecord` has no version field (A6); the summary derives
    /// it from the manifest.
    pub version: Option<String>,
    /// Where the plugin was installed from.
    pub source: PluginSource,
    /// The source string as the user spelled it at install time (the TS
    /// `InstalledRecord.originalSource`, A1/N1): the trimmed install input.
    /// `None` on legacy records.
    pub original_source: Option<String>,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// When the plugin was first installed.
    pub installed_at: chrono::DateTime<chrono::Utc>,
    /// When the record was last updated (enable/disable/reload/upgrade).
    /// The TS `updatedAt` is optional on disk; a record without it reads as
    /// `installed_at`.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Absolute path to the plugin installation directory (the TS `root`).
    pub path: PathBuf,
    /// The GitHub ref the plugin was installed at (`{kind, value}`), for
    /// github-sourced plugins with captured metadata (the TS
    /// `github.ref`). `None` on legacy records and non-github sources.
    pub github_ref: Option<GithubRef>,
    /// The commit SHA the plugin was installed at (`git rev-parse HEAD`
    /// after clone/checkout), for github-sourced plugins (the TS
    /// `github.installedSha`). `None` on legacy records and non-github
    /// sources.
    pub installed_sha: Option<String>,
    /// Per-server MCP state overrides (A1), keyed by the manifest server
    /// name (the TS `capabilities.mcpServers`). Legacy records load as an
    /// empty map.
    pub mcp_servers: HashMap<String, PluginMcpServerState>,
}

impl Serialize for InstalledPlugin {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut record = serde_json::Map::new();
        record.insert("id".to_string(), serde_json::json!(self.id));
        record.insert(
            "root".to_string(),
            serde_json::json!(self.path.display().to_string()),
        );
        record.insert(
            "source".to_string(),
            serde_json::json!(source_literal(&self.source)),
        );
        record.insert("enabled".to_string(), serde_json::json!(self.enabled));
        record.insert(
            "installedAt".to_string(),
            serde_json::json!(self.installed_at.to_rfc3339()),
        );
        record.insert(
            "updatedAt".to_string(),
            serde_json::json!(self.updated_at.to_rfc3339()),
        );
        if let Some(original_source) = &self.original_source {
            record.insert("originalSource".to_string(), serde_json::json!(original_source));
        }
        if !self.mcp_servers.is_empty() {
            record.insert(
                "capabilities".to_string(),
                capabilities_value(&self.mcp_servers),
            );
        }
        if let Some(github) = github_value(&self.source, &self.github_ref, self.installed_sha.as_deref())
        {
            record.insert("github".to_string(), github);
        }
        serde_json::Value::Object(record).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InstalledPlugin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value.as_object().ok_or_else(|| {
            D::Error::custom("installed.json plugin record must be a JSON object")
        })?;
        // The v1 record carries `source` as the `"local-path"|"zip-url"|
        // "github"` string literal; the pre-A6 record carries it as the
        // tagged `{type, ...}` object. Branch on the JSON type.
        let source_is_literal = obj
            .get("source")
            .is_some_and(|source| source.is_string());
        if source_is_literal {
            Self::from_v1_record(obj).map_err(D::Error::custom)
        } else {
            Self::from_legacy_record(obj).map_err(D::Error::custom)
        }
    }
}

impl InstalledPlugin {
    /// Build a record from the TS v1 camelCase shape.
    fn from_v1_record(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, serde_json::Error> {
        let missing = |key: &str| {
            serde_json::Error::custom(format!(
                "installed.json record is missing field \"{key}\""
            ))
        };
        let take_str =
            |key: &str| -> Result<String, serde_json::Error> {
                obj.get(key)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| missing(key))
            };

        let id = take_str("id")?;
        let root = take_str("root")?;
        let source_literal = take_str("source")?;
        let enabled = obj
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| missing("enabled"))?;
        let installed_at = parse_rfc3339(obj.get("installedAt"))
            .ok_or_else(|| missing("installedAt"))?;
        // The TS `updatedAt` is optional; a record without it follows the
        // installed-at timestamp.
        let updated_at = parse_rfc3339(obj.get("updatedAt")).unwrap_or(installed_at);
        let original_source = obj
            .get("originalSource")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let mut mcp_servers = HashMap::new();
        if let Some(servers) = obj
            .get("capabilities")
            .and_then(|c| c.get("mcpServers"))
            .and_then(|v| v.as_object())
        {
            for (name, state) in servers {
                mcp_servers.insert(
                    name.clone(),
                    PluginMcpServerState {
                        enabled: state
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true),
                        enabled_tools: string_array(state.get("enabledTools")),
                        disabled_tools: string_array(state.get("disabledTools")),
                    },
                );
            }
        }

        let source = match source_literal.as_str() {
            // The v1 shape has no local path — `local-path` and `zip-url`
            // records keep only their type (the original spelling lives in
            // `originalSource`).
            "local-path" => PluginSource::Local {
                path: String::new(),
            },
            "zip-url" => PluginSource::Zip {
                url: original_source.clone().unwrap_or_default(),
            },
            "github" => {
                // The v1 shape stores owner/repo split across the `github`
                // metadata; rebuild the `owner/repo` repo spec the record
                // machinery expects, degrading to an empty repo on malformed
                // metadata (the record then fails its update check and is
                // skipped, like the TS `github === undefined` skip).
                let owner = obj
                    .get("github")
                    .and_then(|g| g.get("owner"))
                    .and_then(|v| v.as_str());
                let repo_name = obj
                    .get("github")
                    .and_then(|g| g.get("repo"))
                    .and_then(|v| v.as_str());
                let repo = match (owner, repo_name) {
                    (Some(owner), Some(repo_name)) => format!("{owner}/{repo_name}"),
                    _ => String::new(),
                };
                PluginSource::Github {
                    repo,
                    commit: None,
                    kind: None,
                }
            }
            other => {
                return Err(serde_json::Error::custom(format!(
                    "installed.json record has unknown source literal \"{other}\""
                )))
            }
        };

        let (github_ref, installed_sha) = match obj.get("github") {
            Some(github) => (
                github
                    .get("ref")
                    .and_then(|r| serde_json::from_value(r.clone()).ok()),
                github
                    .get("installedSha")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
            None => (None, None),
        };

        Ok(InstalledPlugin {
            id,
            version: None,
            source,
            original_source,
            enabled,
            installed_at,
            updated_at,
            path: PathBuf::from(root),
            github_ref,
            installed_sha,
            mcp_servers,
        })
    }

    /// Build a record from the pre-A6 snake_case shape (`{type, ...}`
    /// source objects, flat `github_ref`/`installed_sha`/`mcp_servers`).
    fn from_legacy_record(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, serde_json::Error> {
        let legacy = LegacyRecord::deserialize(serde_json::Value::Object(obj.clone()))?;
        Ok(InstalledPlugin {
            id: legacy.id,
            version: legacy.version,
            source: legacy.source,
            original_source: legacy.original_source,
            enabled: legacy.enabled,
            installed_at: legacy.installed_at,
            updated_at: legacy.updated_at.unwrap_or(legacy.installed_at),
            path: legacy.path,
            github_ref: legacy.github_ref,
            installed_sha: legacy.installed_sha,
            mcp_servers: legacy.mcp_servers,
        })
    }
}

/// The pre-A6 installed.json record (snake_case fields, tagged source
/// object) — read-only, for migration.
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LegacyRecord {
    id: String,
    #[serde(default)]
    version: Option<String>,
    source: PluginSource,
    #[serde(default)]
    original_source: Option<String>,
    enabled: bool,
    installed_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    path: PathBuf,
    #[serde(default)]
    github_ref: Option<GithubRef>,
    #[serde(default)]
    installed_sha: Option<String>,
    #[serde(default)]
    mcp_servers: HashMap<String, PluginMcpServerState>,
}

/// The TS `PluginSource` literal of a source (the persisted `source` in the
/// v1 record): `"local-path"` | `"zip-url"` | `"github"`.
fn source_literal(source: &PluginSource) -> &'static str {
    match source {
        PluginSource::Local { .. } => "local-path",
        PluginSource::Zip { .. } => "zip-url",
        PluginSource::Github { .. } => "github",
    }
}

/// The v1 `capabilities: {mcpServers: {<name>: {enabled, enabledTools?,
/// disabledTools?}}}` value for a record's per-server state map.
fn capabilities_value(servers: &HashMap<String, PluginMcpServerState>) -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for (name, state) in servers {
        let mut entry = serde_json::Map::new();
        entry.insert("enabled".to_string(), serde_json::json!(state.enabled));
        if let Some(tools) = &state.enabled_tools {
            entry.insert("enabledTools".to_string(), serde_json::json!(tools));
        }
        if let Some(tools) = &state.disabled_tools {
            entry.insert("disabledTools".to_string(), serde_json::json!(tools));
        }
        entries.insert(name.clone(), serde_json::Value::Object(entry));
    }
    serde_json::json!({ "mcpServers": serde_json::Value::Object(entries) })
}

/// The v1 `github: {owner, repo, ref, installedSha?}` attribution of a
/// github-sourced record with captured ref metadata; `None` for non-github
/// sources and legacy records without a captured ref (the TS
/// `InstalledRecord.github` is absent then too).
fn github_value(
    source: &PluginSource,
    github_ref: &Option<GithubRef>,
    installed_sha: Option<&str>,
) -> Option<serde_json::Value> {
    let PluginSource::Github { repo, .. } = source else {
        return None;
    };
    // `github:` sources and installed.json records carry `owner/repo` in
    // one string; the URL grammar stores it the same way (A1).
    let (owner, repo_name) = repo.split_once('/')?;
    let r#ref = github_ref.as_ref()?;
    let mut metadata = serde_json::Map::new();
    metadata.insert("owner".to_string(), serde_json::json!(owner));
    metadata.insert("repo".to_string(), serde_json::json!(repo_name));
    metadata.insert(
        "ref".to_string(),
        serde_json::to_value(r#ref).ok()?,
    );
    if let Some(sha) = installed_sha {
        metadata.insert("installedSha".to_string(), serde_json::json!(sha));
    }
    Some(serde_json::Value::Object(metadata))
}

/// Parse an RFC3339 timestamp value (`None` when absent or malformed).
fn parse_rfc3339(value: Option<&serde_json::Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Read an optional array-of-strings field (absent → `None`, present →
/// the strings).
fn string_array(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    value.map(|arr| {
        arr.as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// On-disk store for installed-plugin records.
///
/// Wraps the `installed.json` file inside the plugins directory.  The file
/// is read on every operation to avoid stale data when multiple processes
/// might access the same directory.
pub struct PluginStore {
    store_path: PathBuf,
}

impl PluginStore {
    /// Create a store backed by `<plugins_dir>/installed.json`.
    pub fn new(plugins_dir: &Path) -> Self {
        Self {
            store_path: plugins_dir.join(STORE_FILENAME),
        }
    }

    /// Load all installed-plugin records.
    ///
    /// Returns an empty `Vec` if the store file does not exist yet. Accepts
    /// both the TS v1 `{version, plugins:[...]}` envelope and the pre-A6
    /// bare array (item A6 migration); anything else fails with the TS
    /// `readInstalled` error shape. Like the TS `readInstalled`, the
    /// `version` key itself is not validated — only the `plugins` array is
    /// required.
    pub fn load(&self) -> PluginResult<Vec<InstalledPlugin>> {
        if !self.store_path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.store_path)?;
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        let plugins_array = if let Some(array) = value.as_array() {
            array.clone()
        } else if let Some(plugins) = value.get("plugins").and_then(|v| v.as_array()) {
            plugins.clone()
        } else {
            return Err(PluginError::Json(serde_json::Error::custom(
                "installed.json is not a valid InstalledFile object",
            )));
        };
        let plugins: Vec<InstalledPlugin> =
            serde_json::from_value(serde_json::Value::Array(plugins_array))?;
        Ok(plugins)
    }

    /// Persist the full list of installed plugins, replacing the file
    /// atomically (the shared persistence primitive: unique temp file,
    /// fsync, rename), so a crash mid-write can never leave a torn
    /// `installed.json` behind. Always writes the TS v1
    /// `{version: 1, plugins: [...]}` envelope (A6).
    pub fn save(&self, plugins: &[InstalledPlugin]) -> PluginResult<()> {
        let file = serde_json::json!({
            "version": 1,
            "plugins": plugins,
        });
        let json = serde_json::to_string_pretty(&file)?;
        mirri_persistence::write_file_atomically(&self.store_path, json.as_bytes())?;
        Ok(())
    }

    /// Add a new plugin record.
    ///
    /// Returns [`PluginError::AlreadyInstalled`] if a plugin with the same
    /// id (case-insensitively, B3) already exists. Install upgrades use
    /// [`Self::upsert`] instead (A2).
    pub fn add(&self, plugin: InstalledPlugin) -> PluginResult<()> {
        let mut plugins = self.load()?;
        if plugins.iter().any(|p| p.id.eq_ignore_ascii_case(&plugin.id)) {
            return Err(PluginError::AlreadyInstalled(plugin.id.clone()));
        }
        plugins.push(plugin);
        self.save(&plugins)
    }

    /// Insert or replace a plugin record by id (case-insensitively, B3) —
    /// the TS `persist` model where every write replaces the record for the
    /// id (item A2: re-installing a plugin upgrades its record instead of
    /// failing with [`PluginError::AlreadyInstalled`]).
    pub fn upsert(&self, plugin: InstalledPlugin) -> PluginResult<()> {
        let mut plugins = self.load()?;
        if let Some(existing) = plugins
            .iter_mut()
            .find(|p| p.id.eq_ignore_ascii_case(&plugin.id))
        {
            *existing = plugin;
        } else {
            plugins.push(plugin);
        }
        self.save(&plugins)
    }

    /// Remove a plugin record by id.
    ///
    /// The on-disk managed directory is **not** touched — the TS `remove`
    /// only persists the record list (item C2).
    ///
    /// Returns [`PluginError::NotFound`] if no plugin with the given id
    /// exists.
    pub fn remove(&self, id: &str) -> PluginResult<()> {
        let mut plugins = self.load()?;
        let original_len = plugins.len();
        plugins.retain(|p| !p.id.eq_ignore_ascii_case(id));
        if plugins.len() == original_len {
            return Err(PluginError::NotFound(id.to_string()));
        }
        self.save(&plugins)
    }

    /// Enable or disable a plugin by id.
    ///
    /// Returns whether the value changed. A no-op write (the record already
    /// carries the target value) returns `Ok(false)` **without touching the
    /// file or the `updated_at` timestamp** — the TS `setEnabled` early
    /// return (manager.ts, C1).
    /// Returns [`PluginError::NotFound`] if the id is not present.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> PluginResult<bool> {
        let mut plugins = self.load()?;
        let plugin = plugins
            .iter_mut()
            .find(|p| p.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| PluginError::NotFound(id.to_string()))?;
        if plugin.enabled == enabled {
            return Ok(false);
        }
        plugin.enabled = enabled;
        plugin.updated_at = Utc::now();
        self.save(&plugins)?;
        Ok(true)
    }

    /// Enable or disable one of a plugin's contributed MCP servers (A1).
    ///
    /// Returns [`PluginError::NotFound`] if the plugin id is unknown.
    pub fn set_mcp_server_enabled(
        &self,
        id: &str,
        server: &str,
        enabled: bool,
    ) -> PluginResult<()> {
        let mut plugins = self.load()?;
        let plugin = plugins
            .iter_mut()
            .find(|p| p.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| PluginError::NotFound(id.to_string()))?;
        let entry = plugin.mcp_servers.entry(server.to_string()).or_default();
        entry.enabled = enabled;
        plugin.updated_at = Utc::now();
        self.save(&plugins)
    }

    /// Apply a tool-filter patch to one of a plugin's contributed MCP
    /// servers (the record is the server's complete user state: present
    /// fields replace, absent fields clear — an explicit empty state is
    /// distinct from "no record", where the manifest value applies).
    /// Returns whether the record changed.
    ///
    /// Returns [`PluginError::NotFound`] if the plugin id is unknown.
    pub fn set_mcp_server_tool_filter(
        &self,
        id: &str,
        server: &str,
        enabled_tools: Option<Vec<String>>,
        disabled_tools: Option<Vec<String>>,
    ) -> PluginResult<bool> {
        let mut plugins = self.load()?;
        let plugin = plugins
            .iter_mut()
            .find(|p| p.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| PluginError::NotFound(id.to_string()))?;

        let mut next = plugin
            .mcp_servers
            .get(server)
            .cloned()
            .unwrap_or(PluginMcpServerState {
                enabled: true,
                enabled_tools: None,
                disabled_tools: None,
            });
        next.enabled_tools = enabled_tools;
        next.disabled_tools = disabled_tools;

        if plugin.mcp_servers.get(server) == Some(&next) {
            return Ok(false);
        }
        plugin.mcp_servers.insert(server.to_string(), next);
        plugin.updated_at = Utc::now();
        self.save(&plugins)?;
        Ok(true)
    }

    /// Find a plugin record by id.
    ///
    /// The lookup is case-insensitive (the TS `normalizePluginId` semantics,
    /// B3); the record itself keeps the id captured at install time.
    ///
    /// Returns `None` if not found.  Convenience wrapper around [`load`].
    pub fn find(&self, id: &str) -> PluginResult<Option<InstalledPlugin>> {
        let plugins = self.load()?;
        Ok(plugins
            .into_iter()
            .find(|p| p.id.eq_ignore_ascii_case(id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, PluginStore) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = PluginStore::new(dir.path());
        (dir, store)
    }

    fn sample_plugin(id: &str) -> InstalledPlugin {
        InstalledPlugin {
            id: id.to_string(),
            version: Some("1.0.0".to_string()),
            source: PluginSource::Local {
                path: "/source".to_string(),
            },
            original_source: None,
            enabled: true,
            installed_at: Utc::now(),
            updated_at: Utc::now(),
            path: PathBuf::from("/plugins").join(id),
            github_ref: None,
            installed_sha: None,
            mcp_servers: HashMap::new(),
        }
    }

    /// Given a store with a record, when it is saved and the file is
    /// inspected, then the TS v1 envelope and camelCase record shape are
    /// written (item A6): `{version: 1, plugins: [...]}` with `root`,
    /// `installedAt`, `source` literal, and `capabilities.mcpServers`.
    #[test]
    fn should_write_the_ts_v1_envelope_when_saving() {
        let (dir, store) = temp_store();
        store.add(sample_plugin("alpha")).expect("add should succeed");

        let raw = std::fs::read_to_string(dir.path().join(STORE_FILENAME)).expect("read store");
        let file: serde_json::Value = serde_json::from_str(&raw).expect("parse store");
        assert_eq!(file["version"], serde_json::json!(1));
        let record = &file["plugins"][0];
        assert_eq!(record["id"], serde_json::json!("alpha"));
        // Derive the expected root from the PathBuf: the envelope carries
        // native separators (`/plugins\alpha` on Windows).
        let expected_root = PathBuf::from("/plugins").join("alpha");
        assert_eq!(
            record["root"],
            serde_json::json!(expected_root.display().to_string())
        );
        assert_eq!(record["source"], serde_json::json!("local-path"));
        assert_eq!(record["enabled"], serde_json::json!(true));
        assert!(record["installedAt"].is_string());
        assert!(record["updatedAt"].is_string());
        // The TS `InstalledRecord` has no version field (A6).
        assert!(record.get("version").is_none());
        assert!(record.get("path").is_none());
        assert!(record.get("github_ref").is_none());
    }

    /// Given a store with per-server state and github metadata, when it is
    /// saved, then `capabilities.mcpServers` and the `github` attribution
    /// serialize in the TS v1 camelCase shape.
    #[test]
    fn should_write_capabilities_and_github_in_ts_v1_shape_when_saving() {
        let (dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        store
            .set_mcp_server_enabled("alpha", "helper", false)
            .expect("set state");
        store
            .set_mcp_server_tool_filter(
                "alpha",
                "helper",
                Some(vec!["t1".to_string()]),
                Some(vec!["t2".to_string()]),
            )
            .expect("set filter");

        let record = store.find("alpha").expect("find").expect("record");
        let mut record = record;
        record.source = PluginSource::Github {
            repo: "mirricode/demo".to_string(),
            commit: Some("main".to_string()),
            kind: Some(crate::github::GithubRefKind::Branch),
        };
        record.github_ref = Some(GithubRef::branch("main"));
        record.installed_sha = Some("0123456789abcdef0123456789abcdef01234567".to_string());
        store.upsert(record).expect("upsert");

        let raw = std::fs::read_to_string(dir.path().join(STORE_FILENAME)).expect("read store");
        let file: serde_json::Value = serde_json::from_str(&raw).expect("parse store");
        let record = &file["plugins"][0];
        assert_eq!(record["source"], serde_json::json!("github"));
        let state = &record["capabilities"]["mcpServers"]["helper"];
        assert_eq!(state["enabled"], serde_json::json!(false));
        assert_eq!(state["enabledTools"], serde_json::json!(["t1"]));
        assert_eq!(state["disabledTools"], serde_json::json!(["t2"]));
        let github = &record["github"];
        assert_eq!(github["owner"], serde_json::json!("mirricode"));
        assert_eq!(github["repo"], serde_json::json!("demo"));
        assert_eq!(github["ref"], serde_json::json!({"kind": "branch", "value": "main"}));
        assert_eq!(
            github["installedSha"],
            serde_json::json!("0123456789abcdef0123456789abcdef01234567")
        );
    }

    /// Given a TS-written v1 file (camelCase, source literal, capabilities
    /// wrapper, github metadata, no updatedAt), when the store loads it,
    /// then every field maps onto the record — the bidirectional interop
    /// requirement of item A6.
    #[test]
    fn should_load_a_ts_written_v1_file_when_reading_the_store() {
        let (_dir, store) = temp_store();
        let ts_file = serde_json::json!({
            "version": 1,
            "plugins": [
                {
                    "id": "ts-plugin",
                    "root": "/home/u/.mirri/plugins/managed/ts-plugin",
                    "source": "github",
                    "enabled": false,
                    "installedAt": "2026-06-01T00:00:00Z",
                    "originalSource": "https://github.com/mirricode/demo-plugin/tree/main",
                    "capabilities": {
                        "mcpServers": {
                            "helper": { "enabled": false, "enabledTools": ["t1"], "disabledTools": ["t2"] }
                        }
                    },
                    "github": {
                        "owner": "mirricode",
                        "repo": "demo-plugin",
                        "ref": { "kind": "branch", "value": "main" },
                        "installedSha": "0123456789abcdef0123456789abcdef01234567"
                    }
                }
            ]
        });
        std::fs::write(
            store_path(&store).join(STORE_FILENAME),
            serde_json::to_string_pretty(&ts_file).unwrap(),
        )
        .expect("write ts file");

        let plugins = store.load().expect("ts v1 file should load");
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(plugin.id, "ts-plugin");
        assert_eq!(plugin.path, PathBuf::from("/home/u/.mirri/plugins/managed/ts-plugin"));
        assert_eq!(
            plugin.source,
            PluginSource::Github {
                repo: "mirricode/demo-plugin".to_string(),
                commit: None,
                kind: None,
            }
        );
        assert!(!plugin.enabled);
        assert_eq!(plugin.original_source.as_deref(), Some("https://github.com/mirricode/demo-plugin/tree/main"));
        // No updatedAt in the TS file → the record follows installedAt.
        assert_eq!(plugin.updated_at, plugin.installed_at);
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: false,
                enabled_tools: Some(vec!["t1".to_string()]),
                disabled_tools: Some(vec!["t2".to_string()]),
            })
        );
        assert_eq!(plugin.github_ref, Some(GithubRef::branch("main")));
        assert_eq!(
            plugin.installed_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        // The file round-trips: saving rewrites the same v1 envelope.
        store.save(&plugins).expect("save round-trip");
        let round_trip = store.load().expect("reload");
        assert_eq!(round_trip.len(), 1);
        assert_eq!(round_trip[0].id, "ts-plugin");
    }

    /// Given a pre-A6 bare-array file (snake_case, tagged source objects),
    /// when the store loads it, then the records migrate (item A6 read
    /// compatibility) with the old flat `github_ref`/`installed_sha`/
    /// `mcp_servers` fields mapped onto the record.
    #[test]
    fn should_migrate_the_legacy_bare_array_format_when_reading_the_store() {
        let (_dir, store) = temp_store();
        let legacy = r#"[
            {
                "id": "old-plugin",
                "version": "1.0.0",
                "source": {"type": "github", "repo": "user/repo", "commit": null},
                "enabled": true,
                "installed_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z",
                "path": "/plugins/old-plugin",
                "github_ref": {"kind": "branch", "value": "main"},
                "installed_sha": "0123456789abcdef0123456789abcdef01234567",
                "mcp_servers": {
                    "helper": {"enabled": false, "enabled_tools": ["t1"]}
                }
            }
        ]"#;
        std::fs::write(store_path(&store).join(STORE_FILENAME), legacy).expect("write legacy");

        let plugins = store.load().expect("legacy bare array should load");
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(plugin.id, "old-plugin");
        assert_eq!(plugin.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            plugin.source,
            PluginSource::Github {
                repo: "user/repo".to_string(),
                commit: None,
                kind: None,
            }
        );
        assert_eq!(plugin.github_ref, Some(GithubRef::branch("main")));
        assert_eq!(
            plugin.installed_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: false,
                enabled_tools: Some(vec!["t1".to_string()]),
                disabled_tools: None,
            })
        );

        // The next save migrates the file to the v1 envelope.
        store.save(&plugins).expect("save migrates");
        let raw = std::fs::read_to_string(store_path(&store).join(STORE_FILENAME)).expect("read");
        let file: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        assert_eq!(file["version"], serde_json::json!(1));
        assert_eq!(file["plugins"][0]["id"], serde_json::json!("old-plugin"));
        assert_eq!(file["plugins"][0]["root"], serde_json::json!("/plugins/old-plugin"));
        assert_eq!(file["plugins"][0]["github"]["owner"], serde_json::json!("user"));
        assert_eq!(file["plugins"][0]["capabilities"]["mcpServers"]["helper"]["enabledTools"], serde_json::json!(["t1"]));
    }

    /// Given a v1 file whose records load, when the store is asked to
    /// upsert an existing id, then the record is replaced (item A2) — and a
    /// fresh id is appended.
    #[test]
    fn should_replace_record_when_upserting_an_existing_id() {
        let (_dir, store) = temp_store();
        store.add(sample_plugin("alpha")).expect("add");
        store.upsert(sample_plugin("alpha")).expect("upsert");
        let plugins = store.load().expect("load");
        assert_eq!(plugins.len(), 1, "upsert replaces, never duplicates");

        store.upsert(sample_plugin("beta")).expect("upsert new");
        let plugins = store.load().expect("load");
        assert_eq!(plugins.len(), 2);

        // upsert is case-insensitive like every other lookup (B3): the
        // existing record is replaced with the new payload (its id wins,
        // like the TS `persist` after an install).
        store.upsert(sample_plugin("ALPHA")).expect("upsert case");
        let plugins = store.load().expect("load");
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].id, "ALPHA");
    }

    /// Given an installed plugin, when set_enabled is called with the value
    /// it already has, then the call is a no-op: `Ok(false)`, no file write,
    /// no `updated_at` bump (the TS `setEnabled` early return — item C1).
    #[test]
    fn should_not_rewrite_when_set_enabled_value_is_unchanged() {
        let (_dir, store) = temp_store();
        store.add(sample_plugin("alpha")).expect("add");
        let before = store.find("alpha").expect("find").expect("record");
        let before_raw = std::fs::read_to_string(store_path(&store).join(STORE_FILENAME)).expect("read");

        let changed = store.set_enabled("alpha", true).expect("no-op set");
        assert!(!changed, "same value must report no change");

        let after = store.find("alpha").expect("find").expect("record");
        assert_eq!(after.updated_at, before.updated_at, "updated_at must not move");
        let after_raw = std::fs::read_to_string(store_path(&store).join(STORE_FILENAME)).expect("read");
        assert_eq!(before_raw, after_raw, "the store file must not be rewritten");

        // A real toggle still reports a change.
        let changed = store.set_enabled("alpha", false).expect("toggle");
        assert!(changed);
    }

    #[test]
    fn should_load_legacy_record_when_github_metadata_fields_are_absent() {
        let (_dir, store) = temp_store();
        // A pre-checkUpdates installed.json: no github_ref / installed_sha.
        let legacy = r#"[{
            "id": "old-plugin",
            "source": {"type": "github", "repo": "user/repo", "commit": null},
            "enabled": true,
            "installed_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "path": "/plugins/old-plugin"
        }]"#;
        std::fs::write(store_path(&store).join(STORE_FILENAME), legacy).unwrap();
        let plugins = store.load().expect("legacy store should load");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "old-plugin");
        assert_eq!(plugins[0].github_ref, None);
        assert_eq!(plugins[0].installed_sha, None);
    }

    /// The store's backing file path (the temp dir + the store filename).
    fn store_path(store: &PluginStore) -> std::path::PathBuf {
        store
            .store_path
            .parent()
            .expect("store path always has a parent")
            .to_path_buf()
    }

    #[test]
    fn should_return_empty_list_when_no_store_file_exists() {
        let (_dir, store) = temp_store();
        let plugins = store.load().expect("load should succeed");
        assert!(plugins.is_empty());
    }

    #[test]
    fn should_persist_plugin_when_adding_then_loading() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        let plugins = store.load().expect("load should succeed");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "alpha");
    }

    #[test]
    fn should_reject_add_when_plugin_already_exists() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        let result = store.add(sample_plugin("alpha"));
        assert!(matches!(result, Err(PluginError::AlreadyInstalled(_))));
    }

    /// Given a bare-array legacy file or an empty v1 envelope, when the
    /// store loads it, then both parse to the plugin list; a JSON document
    /// that is neither (e.g. a bare object without `plugins`) fails with the
    /// TS `readInstalled` error shape.
    #[test]
    fn should_reject_a_non_installed_file_document_when_loading() {
        let (_dir, store) = temp_store();
        std::fs::write(store_path(&store).join(STORE_FILENAME), r#"{"version": 1}"#).unwrap();
        let result = store.load();
        let Err(PluginError::Json(e)) = result else {
            panic!("expected a JSON error, got {result:?}");
        };
        assert!(
            e.to_string().contains("not a valid InstalledFile"),
            "unexpected message: {e}"
        );

        // An empty v1 envelope parses to an empty list.
        std::fs::write(
            store_path(&store).join(STORE_FILENAME),
            r#"{"version": 1, "plugins": []}"#,
        )
        .unwrap();
        assert!(store.load().expect("empty envelope").is_empty());
    }

    #[test]
    fn should_remove_plugin_when_it_exists() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        store.remove("alpha").expect("remove should succeed");
        let plugins = store.load().expect("load should succeed");
        assert!(plugins.is_empty());
    }

    #[test]
    fn should_reject_remove_when_plugin_does_not_exist() {
        let (_dir, store) = temp_store();
        let result = store.remove("ghost");
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }

    #[test]
    fn should_toggle_enabled_flag_when_setting_enabled() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        store
            .set_enabled("alpha", false)
            .expect("set_enabled should succeed");
        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert!(!plugin.enabled);

        store
            .set_enabled("alpha", true)
            .expect("set_enabled should succeed");
        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert!(plugin.enabled);
    }

    #[test]
    fn should_reject_set_enabled_when_plugin_does_not_exist() {
        let (_dir, store) = temp_store();
        let result = store.set_enabled("ghost", true);
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }

    #[test]
    fn should_load_legacy_record_when_the_mcp_server_map_is_absent() {
        let (_dir, store) = temp_store();
        let legacy = r#"[{
            "id": "old-plugin",
            "source": {"type": "github", "repo": "user/repo", "commit": null},
            "enabled": true,
            "installed_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "path": "/plugins/old-plugin"
        }]"#;
        std::fs::write(store_path(&store).join(STORE_FILENAME), legacy).unwrap();
        let plugins = store.load().expect("legacy store should load");
        assert_eq!(plugins[0].mcp_servers, HashMap::new());
    }

    /// Given an installed plugin, when one of its MCP servers is disabled,
    /// then the per-server record persists and reloads.
    #[test]
    fn should_persist_per_server_state_when_setting_mcp_server_enabled() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");

        store
            .set_mcp_server_enabled("alpha", "helper", false)
            .expect("set_mcp_server_enabled should succeed");

        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: false,
                enabled_tools: None,
                disabled_tools: None,
            })
        );
    }

    /// Given a plugin without a per-server record, when a tool-filter patch
    /// carries only absent fields, then the explicit empty record is still
    /// written (an explicit "no filters" differs from "no record", where the
    /// manifest value applies).
    #[test]
    fn should_persist_the_explicit_empty_state_when_the_filter_patch_is_all_absent() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");

        let changed = store
            .set_mcp_server_tool_filter("alpha", "helper", None, None)
            .expect("set_mcp_server_tool_filter should succeed");

        assert!(changed);
        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: true,
                enabled_tools: None,
                disabled_tools: None,
            })
        );

        // A repeat patch with the same shape changes nothing.
        let changed = store
            .set_mcp_server_tool_filter("alpha", "helper", None, None)
            .expect("repeat patch should succeed");
        assert!(!changed);
    }

    /// Given a per-server tool filter, when a patch clears both fields,
    /// then the cleared record persists — the manifest filter no longer
    /// applies (the record is the complete user state).
    #[test]
    fn should_keep_the_cleared_record_when_the_patch_clears_every_field() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        store
            .set_mcp_server_tool_filter("alpha", "helper", Some(vec!["t1".into()]), None)
            .expect("first patch should succeed");

        let changed = store
            .set_mcp_server_tool_filter("alpha", "helper", None, None)
            .expect("second patch should succeed");

        assert!(changed);
        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: true,
                enabled_tools: None,
                disabled_tools: None,
            })
        );
    }

    /// Given a present filter field, when the patch is applied, then it
    /// replaces the stored one while the other field is left untouched.
    #[test]
    fn should_replace_present_fields_and_clear_absent_ones_in_the_filter_patch() {
        let (_dir, store) = temp_store();
        store
            .add(sample_plugin("alpha"))
            .expect("add should succeed");
        store
            .set_mcp_server_tool_filter(
                "alpha",
                "helper",
                Some(vec!["t1".into(), "t2".into()]),
                Some(vec!["t3".into()]),
            )
            .expect("first patch should succeed");

        store
            .set_mcp_server_tool_filter("alpha", "helper", Some(vec!["t1".into()]), None)
            .expect("second patch should succeed");

        let plugin = store.find("alpha").expect("find should succeed").unwrap();
        assert_eq!(
            plugin.mcp_servers.get("helper"),
            Some(&PluginMcpServerState {
                enabled: true,
                enabled_tools: Some(vec!["t1".to_string()]),
                disabled_tools: None,
            })
        );
    }

    #[test]
    fn should_reject_per_server_writes_when_plugin_does_not_exist() {
        let (_dir, store) = temp_store();
        assert!(matches!(
            store.set_mcp_server_enabled("ghost", "helper", true),
            Err(PluginError::NotFound(_))
        ));
        assert!(matches!(
            store.set_mcp_server_tool_filter("ghost", "helper", Some(vec!["t".into()]), None),
            Err(PluginError::NotFound(_))
        ));
    }
}