//! [`PluginManager`] — orchestrates plugin install, load, enable, disable,
//! and removal.
//!
//! The manager owns a [`PluginStore`] for persistence and an in-memory map
//! of loaded plugins ([`LoadedPlugin`]).  It is the single entry point for
//! all plugin lifecycle operations.
//!
//! # Lifecycle
//!
//! 1. **install** — resolve the source, fetch it (local copies from the
//!    normalized root; github/zip fetch into a temp dir), publish under the
//!    TS managed layout (`plugins/managed/<id>`, staging→swap),
//!    parse manifest, persist record.  Re-installing an id **upgrades** it
//!    (A2): the previous copy is swapped out and deleted after the persist,
//!    and the record keeps `enabled`/`installedAt`/per-server capabilities.
//! 2. **load_all** — read `installed.json` (TS v1 envelope; legacy bare
//!    arrays migrate, A6), parse each manifest, populate the in-memory map.
//! 3. **enable / disable** — toggle the `enabled` flag in the store and
//!    reload the affected plugin (same-value calls are no-ops, C1).
//! 4. **reload** — re-read manifest from disk and update the in-memory map.
//! 5. **remove** — drop the store record and the in-memory entry; the
//!    managed directory is **kept** (the TS semantics, C2).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use chrono::Utc;
use mirri_core::hook_engine::HookDef;
use mirri_core::plugin_runtime::{
    PluginSessionStartSpec, PluginSystemPromptSection,
};
use tracing::{info, warn};

use crate::commands::{
    CommandEntry, CommandRegistry, CommandSpec, ParsedCommandFile, expand_command_arguments,
    load_command_file,
};
use crate::error::{PluginError, PluginResult};
use crate::github::{GithubRef, GithubRefKind, GithubResolver, ReqwestGithubResolver, is_full_sha};
use crate::manifest::{
    McpServerConfig, ParsedManifest, PluginDiagnostic, PluginManifest, PluginManifestKind,
    PluginState, parse_manifest,
};
use crate::source::{GithubInstallCapture, PluginSource, copy_dir_recursive, install_from_source, parse_source};
use crate::store::{InstalledPlugin, PluginStore};

/// A loaded plugin: its best-effort manifest (absent when the manifest is
/// unreadable or unusable — the TS `PluginRecord.manifest`), its store
/// record, and the derived health state (B2/K).
#[derive(Debug, Clone)]
struct LoadedPlugin {
    manifest: Option<PluginManifest>,
    record: InstalledPlugin,
    root: PathBuf,
    state: PluginState,
    diagnostics: Vec<PluginDiagnostic>,
    /// Where the manifest lives — the TS `PluginManifestKind`
    /// (`kimi-plugin-root` | `kimi-plugin-dir`, L/N3), from the parse.
    manifest_kind: Option<PluginManifestKind>,
    /// The resolved manifest file path (L/N3).
    manifest_path: Option<PathBuf>,
    /// A shadowed directory-candidate manifest path, when both a root and a
    /// directory candidate exist (L/N3).
    shadowed_manifest_path: Option<PathBuf>,
}

impl LoadedPlugin {
    /// The TS `recordFrom` conversion: the record's manifest, health state,
    /// and diagnostics flow from the [`ParsedManifest`].
    fn from_parsed(parsed: ParsedManifest, record: InstalledPlugin, root: PathBuf) -> Self {
        let state = parsed.state();
        Self {
            manifest: parsed.manifest,
            record,
            root,
            state,
            diagnostics: parsed.diagnostics,
            manifest_kind: parsed.kind,
            manifest_path: parsed.path,
            shadowed_manifest_path: parsed.shadowed_path,
        }
    }
}

/// Public summary of a plugin, returned by [`PluginManager::list`].
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Plugin identifier.
    pub id: String,
    /// Semantic version (if known).
    pub version: Option<String>,
    /// Human-readable description (if known).
    pub description: Option<String>,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// Where the plugin was installed from.
    pub source: PluginSource,
    /// Absolute path to the plugin directory.
    pub path: PathBuf,
    /// Manifest health (`ok` | `error` — the TS `PluginState`, B2/K).
    pub state: PluginState,
    /// Every manifest parse/validation issue (B2/K).
    pub diagnostics: Vec<PluginDiagnostic>,
}

impl PluginInfo {
    /// Whether any diagnostic has `error` severity — the TS summary
    /// `hasErrors` predicate (K; wired into the RPC summary in batch 2).
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == crate::manifest::PluginDiagnosticSeverity::Error)
    }
}

/// One row of [`PluginManager::check_updates`] — the klient
/// `pluginUpdateStatusSchema` wire shape (`plugins.ts:154-161`):
/// `{ id, source: 'github', current?, latest, displayVersion,
/// updateAvailable }`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginUpdateStatus {
    /// Plugin identifier.
    pub id: String,
    /// Always `"github"` — only github-sourced plugins are checked.
    pub source: &'static str,
    /// The ref the plugin was installed at (omitted for legacy records
    /// without captured metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<GithubRef>,
    /// The ref the repo's "latest" state resolves to.
    pub latest: GithubRef,
    /// Human-readable latest version (the ref value, or the first 12 hex
    /// chars of the latest commit SHA for movable refs).
    pub display_version: String,
    /// Whether the installed copy differs from the latest state.
    pub update_available: bool,
}

/// The GitHub attribution of a summary — the klient `pluginGithubMetadataSchema`
/// (`plugins.ts:111-116`): `{ owner, repo, ref, installedSha? }`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginGithubMetadata {
    /// The repository owner.
    pub owner: String,
    /// The repository name.
    pub repo: String,
    /// The ref the plugin was installed at.
    pub r#ref: GithubRef,
    /// The checked-out commit SHA when it was captured at install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_sha: Option<String>,
}

/// One row of `plugin.list` / the `plugin.install` success — the klient
/// `pluginSummarySchema` (`plugins.ts:118-133`): `{ id, displayName,
/// version?, enabled, state, skillCount, mcpServerCount,
/// enabledMcpServerCount, hookCount, commandCount, hasErrors, source,
/// originalSource?, github? }`, field for field (N1/N2).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSummary {
    /// Plugin identifier.
    pub id: String,
    /// The manifest `interface.displayName`, falling back to the id (the TS
    /// `recordToSummary`).
    pub display_name: String,
    /// Semantic version (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// Manifest health (`ok` | `error`).
    pub state: PluginState,
    /// Number of loadable skill roots — see the D-N2 divergence note.
    pub skill_count: usize,
    /// Number of manifest MCP servers.
    pub mcp_server_count: usize,
    /// Number of manifest MCP servers that are effectively enabled.
    pub enabled_mcp_server_count: usize,
    /// Number of manifest hooks.
    pub hook_count: usize,
    /// Number of manifest command entries.
    pub command_count: usize,
    /// Whether any diagnostic has `error` severity.
    pub has_errors: bool,
    /// The install source kind: `"local-path"` | `"zip-url"` | `"github"`.
    pub source: &'static str,
    /// The source string as spelled at install time (legacy records have
    /// none and omit the field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_source: Option<String>,
    /// GitHub attribution for github-sourced plugins with captured ref
    /// metadata (legacy records degrade to absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<PluginGithubMetadata>,
}

/// One plugin-contributed MCP server in `plugin.info` — the klient
/// `pluginMcpServerInfoSchema` (`plugins.ts:98-109`): `{ name,
/// runtimeName, enabled, transport, command?, args?, cwd?, url?, envKeys?,
/// headerKeys? }`, the TS `pluginMcpServerInfo` producer (N3).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMcpServerInfo {
    /// The manifest server name.
    pub name: String,
    /// The engine's runtime name `plugin-<id>:<name>`.
    pub runtime_name: String,
    /// Whether the server is effectively enabled (user state, else the
    /// manifest default).
    pub enabled: bool,
    /// `"stdio"` | `"http"` | `"sse"`.
    pub transport: &'static str,
    /// stdio: the command to spawn (the normalized absolute path for
    /// `./`-relative entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// stdio: command-line arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// stdio: working directory (normalized for `./`-relative entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// http/sse: the endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// stdio: sorted environment variable names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_keys: Option<Vec<String>>,
    /// http/sse: sorted header names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_keys: Option<Vec<String>>,
}

/// One row of `plugin.info` — the klient `pluginInfoSchema` (`plugins.ts:135-145`):
/// the [`PluginSummary`] super-set plus `{ root, installedAt, updatedAt?,
/// manifestKind?, manifestPath?, manifest?, mcpServers,
/// shadowedManifestPath?, diagnostics }` (N3/L).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfoView {
    /// The summary fields shared with `plugin.list` / `plugin.install`.
    #[serde(flatten)]
    pub summary: PluginSummary,
    /// Absolute path to the plugin directory.
    pub root: String,
    /// When the plugin was first installed (RFC 3339, the TS
    /// `new Date().toISOString()`).
    pub installed_at: String,
    /// When the record was last updated (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Where the manifest lives (`kimi-plugin-root` | `kimi-plugin-dir`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_kind: Option<PluginManifestKind>,
    /// The resolved manifest file path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    /// The parsed manifest in the klient `pluginManifestSchema` wire shape
    /// (commands narrowed to the TS `{path, name}` entries, hooks remapped
    /// to `{event, matcher?, command, timeout?}`), omitted when the
    /// manifest is unusable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<serde_json::Value>,
    /// Every manifest server with its per-server effective state, sorted by
    /// name.
    pub mcp_servers: Vec<PluginMcpServerInfo>,
    /// A shadowed directory-candidate manifest path, when both a root and a
    /// directory candidate exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_manifest_path: Option<String>,
    /// Every manifest parse/validation issue.
    pub diagnostics: Vec<PluginDiagnostic>,
}

/// One plugin-contributed MCP server for the config management view (the
/// `mcp.config.list` plugin entries): the manifest's connection definition
/// plus the user's per-server state (A1). `state: None` means the manifest
/// value applies.
#[derive(Debug, Clone)]
pub struct PluginMcpServerEntry {
    /// The contributing plugin's id.
    pub plugin_id: String,
    /// The server's manifest name.
    pub server_name: String,
    /// The manifest's connection config (raw JSON, as contributed).
    pub config: serde_json::Value,
    /// The user's per-server state, when any override was recorded.
    pub state: Option<crate::store::PluginMcpServerState>,
}

/// One entry of the reload `errors` list — the TS `ReloadSummary` error row
/// (`{ id, message }`, item C3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReloadError {
    /// The plugin id of the record that failed to materialize.
    pub id: String,
    /// The failure message (the TS caught error's message).
    pub message: String,
}

/// The result of [`PluginManager::reload_all`] — the TS `ReloadSummary`
/// (`{ added, removed, errors }`, the `PluginManager.reload` of manager.ts,
/// item C3): `added`/`removed` are the installed.json record id diff against
/// the previously loaded set, and `errors` collects the per-record
/// materialization failures. Error-state plugins (missing/broken manifest)
/// are not failures — they stay in the list with `state: 'error'` and land
/// in neither bucket (B2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadSummary {
    /// Plugin ids present in `installed.json` but not previously loaded.
    pub added: Vec<String>,
    /// Previously loaded plugin ids absent from `installed.json`.
    pub removed: Vec<String>,
    /// Records that failed to materialize (`{id, message}`, the TS
    /// try/catch of `reload()`).
    pub errors: Vec<PluginReloadError>,
}

/// The plugin attribution of a skill root — the TS `SkillPluginContext`
/// (`skillCatalog/types.ts`: `{ id, instructions? }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPluginContext {
    /// The contributing plugin's id.
    pub id: String,
    /// The manifest `skillInstructions` attached to the root (the item-J
    /// field, carried here from parse time).
    pub instructions: Option<String>,
}

/// A plugin-contributed skill root — the TS `SkillRoot` shape
/// (`skillCatalog/types.ts`: `{ path, source: 'extra', plugin? }`; the
/// `source` is always `'extra'` for plugin roots, and `plugin` is always
/// present — the TS `pluginSkillRoots` producer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkillRoot {
    /// The absolute skill directory path (resolved at manifest parse, B8).
    pub path: PathBuf,
    /// Always `"extra"` — the TS `SkillSource` value for plugin roots.
    pub source: &'static str,
    /// The contributing plugin's context.
    pub plugin: SkillPluginContext,
}

/// A plugin-contributed agent root — the TS `PluginAgentRoot`
/// (`plugin/types.ts`: `{ path, source: 'plugin' }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAgentRoot {
    /// The absolute agent directory path (resolved at manifest parse, B8).
    pub path: PathBuf,
    /// Always `"plugin"` — the TS `PluginAgentRoot.source`.
    pub source: &'static str,
}

/// A resolved command contributed by an enabled plugin — the TS
/// `PluginCommandDef` (`plugin/types.ts`: `{ pluginId, name, description,
/// body, path }`, B9/N4). `path` is the markdown file's absolute path for
/// the md model, and empty for inline specs (the historical `plugin.commands`
/// mapping).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandDef {
    /// The contributing plugin's id.
    pub plugin_id: String,
    /// The command name.
    pub name: String,
    /// Short human-readable description (frontmatter `description` or the
    /// first body line; empty for inline specs without one).
    pub description: String,
    /// The prompt body sent to the model (the md body, or the inline spec
    /// template).
    pub body: String,
    /// The markdown file's absolute path; empty for inline specs.
    pub path: String,
}

/// The runtime name of a plugin-contributed MCP server — the key the
/// engine's connection layer knows it by (`plugin-<id>:<name>`, the TS
/// engine's convention).
pub fn plugin_mcp_runtime_name(plugin_id: &str, server_name: &str) -> String {
    format!("plugin-{plugin_id}:{server_name}")
}

/// Lowercase a plugin id for lookups, mirroring the TS `normalizePluginId`
/// (B3): callers may address a plugin in any case, while the on-disk record
/// keeps the id captured at install time. Ids are ASCII by the name regex,
/// so `to_lowercase` matches the JS `toLowerCase()` semantics.
pub fn normalize_plugin_id(id: &str) -> String {
    id.to_lowercase()
}

/// Overlay a per-server user state onto a contributed config value: the
/// state's enablement and tool filters override the manifest's.
fn apply_mcp_server_state(
    config: &mut serde_json::Value,
    state: Option<&crate::store::PluginMcpServerState>,
) {
    let Some(state) = state else { return };
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    // The collected view only contains enabled servers, so the effective
    // flag is pinned true (the TS `withMcpServerEnabled` normalization).
    obj.insert("enabled".to_string(), serde_json::Value::Bool(true));
    match &state.enabled_tools {
        Some(tools) => {
            obj.insert("enabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            obj.remove("enabledTools");
        }
    }
    match &state.disabled_tools {
        Some(tools) => {
            obj.insert("disabledTools".to_string(), serde_json::json!(tools));
        }
        None => {
            obj.remove("disabledTools");
        }
    }
}

/// A per-server write must address a server the plugin actually
/// contributes.
fn ensure_server_in_manifest(loaded: &LoadedPlugin, id: &str, server: &str) -> PluginResult<()> {
    let known = loaded
        .manifest
        .as_ref()
        .and_then(|m| m.mcp_servers.as_ref())
        .is_some_and(|servers| servers.contains_key(server));
    if !known {
        return Err(PluginError::McpServerNotFound(format!("{id}:{server}")));
    }
    Ok(())
}

/// The plugin lifecycle manager.
///
/// Owns the plugins directory, the persistent store, and an in-memory cache
/// of loaded manifests.  Thread-safe via `RwLock`.
pub struct PluginManager {
    plugins_dir: PathBuf,
    /// The Mirri home directory — the value injected into decorated hooks'
    /// `MIRRICODE_HOME` env (the TS manager's `kimiHomeDir`, item E). For
    /// the canonical layout (`<home>/plugins`) this is `plugins_dir.parent()`;
    /// [`Self::new_with_home`] allows an explicit value (tests, non-standard
    /// layouts).
    home_dir: PathBuf,
    store: PluginStore,
    loaded: RwLock<HashMap<String, LoadedPlugin>>,
    /// GitHub ref resolver for `check_updates` and install-time metadata
    /// capture. Injectable so tests never touch the network.
    github_resolver: Arc<dyn GithubResolver>,
}

impl PluginManager {
    /// Create a new manager backed by the given plugins directory.
    ///
    /// The directory (and `installed.json` inside it) is created lazily on
    /// first write. The `MIRRICODE_HOME` decoration derives the home as the
    /// plugins directory's parent (the canonical `<home>/plugins` layout).
    pub fn new(plugins_dir: PathBuf) -> Self {
        let home_dir = plugins_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| plugins_dir.clone());
        Self::new_with_home(plugins_dir, home_dir)
    }

    /// Create a manager with an explicit home directory for the decorated
    /// hooks' `MIRRICODE_HOME` env (item E) and an injected GitHub resolver —
    /// the test seam for update checks (the default constructor uses the
    /// real reqwest-backed resolver).
    pub fn new_with_home(plugins_dir: PathBuf, home_dir: PathBuf) -> Self {
        Self::new_with_home_and_github_resolver(
            plugins_dir,
            home_dir,
            Arc::new(ReqwestGithubResolver::new()),
        )
    }

    /// Create a manager with an injected GitHub resolver — the test seam for
    /// update checks (the default constructor uses the real reqwest-backed
    /// resolver).
    pub fn new_with_github_resolver(
        plugins_dir: PathBuf,
        github_resolver: Arc<dyn GithubResolver>,
    ) -> Self {
        let home_dir = plugins_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| plugins_dir.clone());
        Self::new_with_home_and_github_resolver(plugins_dir, home_dir, github_resolver)
    }

    fn new_with_home_and_github_resolver(
        plugins_dir: PathBuf,
        home_dir: PathBuf,
        github_resolver: Arc<dyn GithubResolver>,
    ) -> Self {
        let store = PluginStore::new(&plugins_dir);
        Self {
            plugins_dir,
            home_dir,
            store,
            loaded: RwLock::new(HashMap::new()),
            github_resolver,
        }
    }

    // -----------------------------------------------------------------------
    // Install
    // -----------------------------------------------------------------------

    /// Install a plugin from a source string.
    ///
    /// Parses the source, fetches it (local sources copy directly from the
    /// normalized root; github/zip sources fetch into a temp dir first),
    /// then publishes the plugin under the TS managed layout
    /// `<plugins_dir>/managed/<id>` via the staging→swap dance
    /// (`copyPluginToManagedRoot`), parses the manifest, and persists the
    /// record to `installed.json`.
    ///
    /// **Installing an already-installed id upgrades it** (item A2, the TS
    /// `install`): the new copy replaces the managed directory, the record
    /// keeps its `enabled`/`installedAt`/per-server capabilities,
    /// `updatedAt` refreshes to now, and the previous managed copy is
    /// deleted **after** the persist (best-effort). A failure after the
    /// swap restores the previous copy; a failed restore aggregates both
    /// errors ([`PluginError::InstallRollbackFailed`], the TS
    /// `AggregateError`).
    ///
    /// Mirrors the TS `PluginManager.install` failure semantics (B2/K): the
    /// install only fails when no usable manifest exists (missing file,
    /// invalid JSON, invalid name) — the failure message is the first
    /// `error` diagnostic. A manifest that parses but carries `error`-level
    /// diagnostics installs successfully with `state: 'error'`.
    ///
    /// Returns the plugin id (manifest `name`).
    pub async fn install(&self, source: &str) -> PluginResult<String> {
        let plugin_source = parse_source(source)?;

        let temp_dir = self
            .plugins_dir
            .join(format!("__install_tmp_{}", uuid::Uuid::new_v4()));

        // Fetch step: local sources copy from the normalized source root
        // (item A4 — absolute + realpath + is_dir, the TS
        // `normalizeInstallRoot`); github/zip sources fetch into the temp
        // dir (the TS `downloadZip`/`extractZip` step).
        let (source_root, github_capture) = match &plugin_source {
            PluginSource::Local { path } => {
                let root = normalize_install_root(Path::new(path))?;
                (root, GithubInstallCapture::default())
            }
            _ => {
                std::fs::create_dir_all(&temp_dir)?;
                match install_from_source(
                    &plugin_source,
                    &temp_dir,
                    self.github_resolver.as_ref(),
                )
                .await
                {
                    Ok((root, capture)) => (root, capture),
                    Err(e) => {
                        let _ = std::fs::remove_dir_all(&temp_dir);
                        return Err(e);
                    }
                }
            }
        };

        let install_result = self
            .install_inner(&plugin_source, &source_root, &github_capture, source)
            .await;
        // The TS `finally` cleanup of the fetch temp dir (a local install
        // never created one — the removal is a no-op then).
        let _ = std::fs::remove_dir_all(&temp_dir);
        install_result
    }

    /// Inner install logic — the caller owns the fetch-temp cleanup.
    async fn install_inner(
        &self,
        plugin_source: &PluginSource,
        source_root: &Path,
        github_capture: &GithubInstallCapture,
        source: &str,
    ) -> PluginResult<String> {
        // Parse the manifest to get the plugin name. Like the TS
        // `install`, a missing/unusable manifest fails the install with the
        // first error diagnostic as the message (K).
        let parsed = parse_manifest(source_root);
        let Some(manifest) = parsed.manifest.clone() else {
            let message = parsed
                .diagnostics
                .iter()
                .find(|d| d.severity == crate::manifest::PluginDiagnosticSeverity::Error)
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "no manifest".to_string());
            let detail = match plugin_source {
                PluginSource::Local { path } => format!("at {path}"),
                _ => format!("from {}", source.trim()),
            };
            return Err(PluginError::InstallFailed(format!(
                "Cannot install plugin {detail}: {message}"
            )));
        };
        let id = manifest.name.clone();

        // Publish under the managed layout via the staging→swap dance (the
        // TS `copyPluginToManagedRoot`).
        let managed_copy = copy_plugin_to_managed_root(&self.plugins_dir, &id, source_root)?;
        let normalized_root = managed_copy.root.clone();

        // Re-parse at the managed location for the record — the TS
        // `parseManifest(normalizedRoot)` step: the record keeps the
        // managed-root parse, including `state: 'error'` when the manifest
        // carries error diagnostics (B2).
        let managed_parsed = parse_manifest(&normalized_root);

        // The TS `recordFrom` identity of an upgrade (A2): an existing
        // record keeps `enabled`, `installedAt`, and its per-server
        // capabilities; `updatedAt` refreshes to now. The existing record
        // is read best-effort (the TS reads its in-memory map, which never
        // fails): an unreadable store here degrades to "no existing", and
        // the failure surfaces at the persist below — where the rollback
        // applies.
        let now = Utc::now();
        let existing = self.store.find(&id).unwrap_or(None);
        let record = InstalledPlugin {
            id: id.clone(),
            version: manifest.version.clone(),
            source: plugin_source.clone(),
            // The TS `originalSource`: `resolved.path` for local-path
            // sources (the trimmed path), else the trimmed install input
            // (N1/A1 — the persisted echo of the source string).
            original_source: Some(match plugin_source {
                PluginSource::Local { path } => path.trim().to_string(),
                _ => source.trim().to_string(),
            }),
            enabled: existing.as_ref().map(|r| r.enabled).unwrap_or(true),
            installed_at: existing.as_ref().map(|r| r.installed_at).unwrap_or(now),
            updated_at: now,
            path: normalized_root.clone(),
            github_ref: github_capture.github_ref.clone(),
            installed_sha: github_capture.installed_sha.clone(),
            mcp_servers: existing
                .as_ref()
                .map(|r| r.mcp_servers.clone())
                .unwrap_or_default(),
        };

        // Persist; on failure restore the previous managed copy (the TS
        // `rollbackManagedPluginCopy`). A failed restore aggregates both
        // errors (the TS `AggregateError`).
        if let Err(install_error) = self.store.upsert(record.clone()) {
            return match rollback_managed_plugin_copy(&managed_copy) {
                Ok(()) => Err(install_error),
                Err(rollback_error) => Err(PluginError::InstallRollbackFailed {
                    install: install_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            };
        }

        // The old managed copy is deleted only after the persist succeeded,
        // best-effort — the TS `rm(previousRoot, {force}).catch(() =>
        // undefined)`.
        if let Some(previous_root) = &managed_copy.previous_root {
            let _ = std::fs::remove_dir_all(previous_root);
        }

        let loaded = LoadedPlugin::from_parsed(managed_parsed, record.clone(), normalized_root);
        {
            let mut map = self
                .loaded
                .write()
                .expect("plugin loaded map lock poisoned");
            map.insert(id.clone(), loaded);
        }

        info!(plugin = %id, "plugin installed");
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Load / reload
    // -----------------------------------------------------------------------

    /// Load all installed plugins from the store into memory.
    ///
    /// This reads `installed.json`, parses each plugin's manifest, and
    /// populates the in-memory map.  Plugins whose manifest is missing or
    /// invalid are **kept** with `state: 'error'` (the TS `load`/`materialize`
    /// behavior, B2): an error-state plugin never disappears from the list —
    /// its contributions are simply skipped by the collectors.
    pub fn load_all(&self) -> PluginResult<()> {
        let records = self.store.load()?;
        let mut map = self
            .loaded
            .write()
            .expect("plugin loaded map lock poisoned");
        map.clear();

        for record in records {
            let parsed = parse_manifest(&record.path);
            let state = parsed.state();
            if state == PluginState::Ok {
                info!(plugin = %record.id, "plugin loaded");
            } else {
                warn!(plugin = %record.id, "plugin manifest has errors; keeping the plugin with state=error");
            }
            map.insert(
                record.id.clone(),
                LoadedPlugin::from_parsed(parsed, record.clone(), record.path.clone()),
            );
        }
        Ok(())
    }

    /// Reload a single plugin by re-reading its manifest from disk.
    ///
    /// Returns [`PluginError::NotFound`] if the plugin is not in the store.
    /// A manifest that fails to parse keeps the plugin in the map with
    /// `state: 'error'` (the TS `reload` materialize flow, B2/C3).
    pub fn reload(&self, id: &str) -> PluginResult<()> {
        let record = self
            .store
            .find(id)?
            .ok_or_else(|| PluginError::NotFound(id.to_string()))?;

        let parsed = parse_manifest(&record.path);
        let state = parsed.state();
        if state == PluginState::Error {
            warn!(plugin = %id, "plugin manifest has errors; keeping the plugin with state=error");
        }
        let loaded = LoadedPlugin::from_parsed(parsed, record.clone(), record.path.clone());

        {
            let mut map = self
                .loaded
                .write()
                .expect("plugin loaded map lock poisoned");
            // Normalize the map key (B3): the in-memory index is the
            // lowercase id, while `record.id` keeps the install-time value.
            map.insert(normalize_plugin_id(id), loaded);
        }

        info!(plugin = %id, "plugin reloaded");
        Ok(())
    }

    /// Reload every plugin from the store — the TS `PluginManager.reload`
    /// (manager.ts, item C3): re-read `installed.json`, materialize every
    /// record, and report the id diff plus the materialization failures.
    ///
    /// Semantics, branch for branch:
    ///
    /// - a store read failure (a corrupt `installed.json`) fails the whole
    ///   reload — the TS `readInstalled` throw, surfaced as
    ///   `plugin.load_failed` at the RPC layer;
    /// - a record whose manifest is missing or broken materializes with
    ///   `state: 'error'` and **stays in the list** — it is not an error and
    ///   never counted as removed (B2; the TS `parseManifest` comment);
    /// - a record whose materialization throws (the TS try/catch scope —
    ///   currently unreachable in rust, where every materialize step is
    ///   tolerant) lands in `errors` and is dropped from the list;
    /// - `added`/`removed` are the id diff between the store's records and
    ///   the previously loaded set (TS `reload`'s `prevIds`/`next` loop).
    pub fn reload_all(&self) -> PluginResult<ReloadSummary> {
        let prev_ids: HashSet<String> = {
            let map = self
                .loaded
                .read()
                .expect("plugin loaded map lock poisoned");
            map.keys().cloned().collect()
        };
        let records = self.store.load()?;
        let mut next = HashMap::new();
        let mut errors = Vec::new();
        for record in records {
            let id = record.id.clone();
            match materialize_record(record) {
                Ok(loaded) => {
                    if loaded.state == PluginState::Ok {
                        info!(plugin = %id, "plugin loaded");
                    } else {
                        warn!(plugin = %id, "plugin manifest has errors; keeping the plugin with state=error");
                    }
                    next.insert(id, loaded);
                }
                Err(message) => {
                    warn!(plugin = %id, error = %message, "plugin failed to materialize; reporting in reload errors");
                    errors.push(PluginReloadError { id, message });
                }
            }
        }
        let added: Vec<String> = next
            .keys()
            .filter(|id| !prev_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let removed: Vec<String> = prev_ids
            .iter()
            .filter(|id| !next.contains_key(id.as_str()))
            .cloned()
            .collect();
        {
            let mut map = self
                .loaded
                .write()
                .expect("plugin loaded map lock poisoned");
            *map = next;
        }
        Ok(ReloadSummary {
            added,
            removed,
            errors,
        })
    }

    // -----------------------------------------------------------------------
    // Enable / disable
    // -----------------------------------------------------------------------

    /// Enable a plugin.
    ///
    /// A no-op when the plugin is already enabled: the TS `setEnabled`
    /// early return skips both the store write and the reload — no
    /// `updatedAt` refresh, no redundant I/O (item C1).
    pub fn enable(&self, id: &str) -> PluginResult<()> {
        if !self.store.set_enabled(id, true)? {
            return Ok(());
        }
        self.reload(id)?;
        info!(plugin = %id, "plugin enabled");
        Ok(())
    }

    /// Disable a plugin (same-value calls are no-ops too — see
    /// [`Self::enable`], item C1).
    pub fn disable(&self, id: &str) -> PluginResult<()> {
        if !self.store.set_enabled(id, false)? {
            return Ok(());
        }
        self.reload(id)?;
        info!(plugin = %id, "plugin disabled");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Remove
    // -----------------------------------------------------------------------

    /// Remove a plugin: delete its store record and in-memory entry.
    ///
    /// The managed directory is **kept on disk** — the TS `remove` only
    /// persists the record list (item C2); reloads afterwards report the id
    /// as removed while the directory stays at
    /// `<plugins_dir>/managed/<id>`.
    pub fn remove(&self, id: &str) -> PluginResult<()> {
        // Remove from store (NotFound when the id is unknown).
        self.store.remove(id)?;

        // Remove from the in-memory map (normalized key, like every other
        // lookup — B3).
        {
            let mut map = self
                .loaded
                .write()
                .expect("plugin loaded map lock poisoned");
            map.remove(&normalize_plugin_id(id));
        }

        info!(plugin = %id, "plugin removed");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Query
    // -----------------------------------------------------------------------

    /// List all known plugins with their current status.
    pub fn list(&self) -> Vec<PluginInfo> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        map.values()
            .map(|loaded| PluginInfo {
                id: loaded
                    .manifest
                    .as_ref()
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| loaded.record.id.clone()),
                version: loaded.manifest.as_ref().and_then(|m| m.version.clone()),
                description: loaded.manifest.as_ref().and_then(|m| m.description.clone()),
                enabled: loaded.record.enabled,
                source: loaded.record.source.clone(),
                path: loaded.root.clone(),
                state: loaded.state,
                diagnostics: loaded.diagnostics.clone(),
            })
            .collect()
    }

    /// The `plugin.list` wire rows — the klient `pluginSummarySchema[]`
    /// shape (N1), the TS `PluginManager.summaries` producer: every loaded
    /// plugin (error-state included) as a [`PluginSummary`], sorted by id
    /// (`localeCompare` parity for the ASCII id space).
    pub fn summaries(&self) -> Vec<PluginSummary> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut summaries: Vec<PluginSummary> =
            map.values().map(summary_from_loaded).collect();
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }

    /// The summary of one plugin by id (the `plugin.install` success body —
    /// N2, the TS `summaries().find` equivalent).
    pub fn summary(&self, id: &str) -> Option<PluginSummary> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        map.get(&normalize_plugin_id(id)).map(summary_from_loaded)
    }

    /// The `plugin.info` wire row — the klient `pluginInfoSchema` shape
    /// (N3/L), the TS `PluginManager.info` producer: the summary plus the
    /// install/parse metadata (`root`, `installedAt`, `updatedAt`,
    /// `manifestKind`, `manifestPath`, wire `manifest`, `mcpServers`,
    /// `shadowedManifestPath`, `diagnostics`).
    pub fn info(&self, id: &str) -> Option<PluginInfoView> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        map.get(&normalize_plugin_id(id))
            .map(info_from_loaded)
    }

    /// Collect MCP server configs from all enabled plugins, keyed by the
    /// runtime name `plugin-<id>:<name>` (the TS engine's convention — a
    /// plugin server can never collide with a user-config server). Only
    /// effectively-enabled servers join the set — the TS
    /// `isMcpServerEnabled` (`state?.enabled ?? config.enabled !== false`,
    /// item D1): the recorded per-server user state wins; without one, a
    /// manifest `enabled: false` excludes the server. Tool filters override
    /// the manifest values. Error-state plugins are skipped (B2).
    pub fn enabled_mcp_servers(&self) -> HashMap<String, serde_json::Value> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = HashMap::new();
        let home = self.home_dir.display().to_string();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            if let Some(servers) = loaded.manifest.as_ref().and_then(|m| m.mcp_servers.as_ref()) {
                let root = loaded.root.display().to_string();
                for (name, config) in servers {
                    if !is_mcp_server_effectively_enabled(&loaded.record, name, config) {
                        continue;
                    }
                    let state = loaded.record.mcp_servers.get(name);
                    // The runtime decoration (item A7) — the TS
                    // `withPluginMcpRuntime` applied at the manager's exit:
                    // stdio servers get the cwd default and the runtime env
                    // facts; http/sse pass through.
                    let decorated = with_plugin_mcp_runtime(config.clone(), &root, &home);
                    let mut config = mcp_config_to_value(&decorated);
                    apply_mcp_server_state(&mut config, state);
                    result.insert(plugin_mcp_runtime_name(&loaded.record.id, name), config);
                }
            }
        }
        result
    }

    /// The plugin-contributed MCP servers for the config management view:
    /// every enabled plugin's manifest servers with their user state (A1).
    /// Per-server-disabled servers still list — the view must show them so
    /// the user can re-enable. Error-state plugins are skipped (B2).
    pub fn mcp_server_entries(&self) -> Vec<PluginMcpServerEntry> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut entries = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            if let Some(servers) = loaded.manifest.as_ref().and_then(|m| m.mcp_servers.as_ref()) {
                for (name, config) in servers {
                    entries.push(PluginMcpServerEntry {
                        plugin_id: loaded.record.id.clone(),
                        server_name: name.clone(),
                        config: mcp_config_to_value(config),
                        state: loaded.record.mcp_servers.get(name).cloned(),
                    });
                }
            }
        }
        entries
    }

    /// Enable or disable one of a plugin's contributed MCP servers (A1).
    ///
    /// Returns the server's runtime name. Fails with
    /// [`PluginError::NotFound`] when the plugin is unknown and
    /// [`PluginError::McpServerNotFound`] when its manifest does not
    /// contribute a server by that name.
    pub fn set_mcp_server_enabled(
        &self,
        id: &str,
        server: &str,
        enabled: bool,
    ) -> PluginResult<String> {
        {
            let map = self.loaded.read().expect("plugin loaded map lock poisoned");
            let loaded = map
                .get(&normalize_plugin_id(id))
                .ok_or_else(|| PluginError::NotFound(id.to_string()))?;
            ensure_server_in_manifest(loaded, id, server)?;
        }
        self.store.set_mcp_server_enabled(id, server, enabled)?;
        self.reload(id)?;
        info!(plugin = %id, server = %server, enabled, "plugin MCP server enabled state set");
        Ok(plugin_mcp_runtime_name(id, server))
    }

    /// Apply a tool-filter patch to one of a plugin's contributed MCP
    /// servers (A1; present fields replace, absent fields clear).
    ///
    /// Returns the server's runtime name. Fails the same way
    /// [`PluginManager::set_mcp_server_enabled`] does.
    pub fn set_mcp_server_tool_filter(
        &self,
        id: &str,
        server: &str,
        enabled_tools: Option<Vec<String>>,
        disabled_tools: Option<Vec<String>>,
    ) -> PluginResult<String> {
        {
            let map = self.loaded.read().expect("plugin loaded map lock poisoned");
            let loaded = map
                .get(&normalize_plugin_id(id))
                .ok_or_else(|| PluginError::NotFound(id.to_string()))?;
            ensure_server_in_manifest(loaded, id, server)?;
        }
        self.store
            .set_mcp_server_tool_filter(id, server, enabled_tools, disabled_tools)?;
        self.reload(id)?;
        info!(plugin = %id, server = %server, "plugin MCP server tool filter set");
        Ok(plugin_mcp_runtime_name(id, server))
    }

    /// Collect hooks from all enabled plugins, decorated with the plugin
    /// runtime facts (item E). Error-state plugins are skipped (B2 — the TS
    /// `enabledHooks` `state !== 'ok'` check).
    ///
    /// Each hook mirrors the TS `enabledHooks` decoration: `cwd` is pinned
    /// to the plugin root and `env` carries `MIRRICODE_HOME` (the manager's
    /// home dir) plus `MIRRI_PLUGIN_ROOT` (the plugin root) — the TS
    /// `{...hook, cwd: record.root, env: {...}}` overrides, so a
    /// manifest-declared `cwd`/`env` is replaced by the runtime facts.
    pub fn enabled_hooks(&self) -> Vec<HookDef> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let root = loaded.root.display().to_string();
            let mut env = std::collections::HashMap::new();
            env.insert("MIRRICODE_HOME".to_string(), self.home_dir.display().to_string());
            env.insert("MIRRI_PLUGIN_ROOT".to_string(), root.clone());
            if let Some(hooks) = loaded.manifest.as_ref().and_then(|m| m.hooks.as_ref()) {
                for hook in hooks.iter() {
                    let mut decorated = hook.clone();
                    decorated.cwd = Some(root.clone());
                    decorated.env = Some(env.clone());
                    result.push(decorated);
                }
            }
        }
        result
    }

    /// Collect the rendered system prompts of all enabled plugins, each
    /// attributed to its plugin (item J — the TS `enabledSystemPrompts`
    /// `{pluginId, content}` shape).
    ///
    /// The content is rendered at parse time ([`crate::manifest`]
    /// `readSystemPrompt`): inline and file parts are trimmed (file contents
    /// additionally have a leading UTF-8 BOM stripped), joined with
    /// `"\n\n"`, and each part capped at 32 KiB — an over-limit part is
    /// ignored with a `warn` diagnostic instead of included (B6/B2).
    /// Error-state plugins are skipped (B2 — the TS `state !== 'ok'` check).
    pub fn enabled_system_prompts(&self) -> Vec<PluginSystemPromptSection> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            if let Some(prompt) = &loaded.manifest.as_ref().and_then(|m| m.system_prompt.clone()) {
                result.push(PluginSystemPromptSection {
                    plugin_id: loaded.record.id.clone(),
                    content: prompt.clone(),
                });
            }
        }
        result
    }

    /// Collect the `sessionStart:{skill}` declarations of all enabled
    /// plugins (item I — the TS `enabledSessionStarts` `{pluginId,
    /// skillName}` shape, `pluginId` from the record and `skillName` from
    /// the manifest `sessionStart.skill`).
    ///
    /// A plugin with no `sessionStart` (or no `skill` inside it) is
    /// skipped entirely (the TS `skill === undefined` skip). Error-state
    /// plugins are skipped (B2).
    pub fn enabled_session_starts(&self) -> Vec<PluginSessionStartSpec> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let Some(skill) = loaded
                .manifest
                .as_ref()
                .and_then(|m| m.session_start.as_ref())
                .map(|s| s.skill.clone())
            else {
                continue;
            };
            result.push(PluginSessionStartSpec {
                plugin_id: loaded.record.id.clone(),
                skill_name: skill,
            });
        }
        result
    }

    /// Collect skill roots from all enabled plugins, in the TS `SkillRoot`
    /// shape (`{ path, source: 'extra', plugin: { id, instructions } }` —
    /// the TS `pluginSkillRoots` producer). The paths are the resolved
    /// absolute skill directories of the manifest (B8, realpath'd at parse
    /// time). Error-state plugins are skipped (B2). Sorted by plugin id
    /// then path for deterministic consumption.
    pub fn plugin_skill_roots(&self) -> Vec<PluginSkillRoot> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            for dir in manifest.skills.as_deref().unwrap_or_default() {
                result.push(PluginSkillRoot {
                    path: PathBuf::from(dir),
                    source: "extra",
                    plugin: SkillPluginContext {
                        id: loaded.record.id.clone(),
                        instructions: manifest.skill_instructions.clone(),
                    },
                });
            }
        }
        result.sort_by(|left, right| {
            (&left.plugin.id, &left.path).cmp(&(&right.plugin.id, &right.path))
        });
        result
    }

    /// Collect agent roots from all enabled plugins, in the TS
    /// `PluginAgentRoot` shape (`{ path, source: 'plugin' }` — the TS
    /// `pluginAgentRoots` producer). Error-state plugins are skipped (B2).
    /// Sorted by plugin id then path for deterministic consumption.
    pub fn plugin_agent_roots(&self) -> Vec<PluginAgentRoot> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            for dir in manifest.agents.as_deref().unwrap_or_default() {
                result.push(PluginAgentRoot {
                    path: PathBuf::from(dir),
                    source: "plugin",
                });
            }
        }
        result.sort_by(|left, right| {
            // The TS roots carry no plugin id; sort by path for determinism.
            left.path.cmp(&right.path)
        });
        result
    }

    /// Collect command file paths from all enabled plugins.
    ///
    /// Only the markdown-file [`CommandEntry::File`] entries are collected;
    /// each entry is the absolute resolved path of a markdown file (B9).
    /// Error-state plugins are skipped (B2).
    pub fn plugin_command_files(&self) -> Vec<PathBuf> {
        self.enabled_command_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                CommandEntry::File(file) => Some(file.path),
                CommandEntry::Inline(_) => None,
            })
            .collect()
    }

    /// Collect inline command specs from all enabled plugins, grouped by
    /// plugin name.
    ///
    /// The pairs are sorted by plugin name so that conflict resolution
    /// during a registry reload is deterministic: given two plugins
    /// defining the same command name, the lexically later plugin wins.
    /// Error-state plugins are skipped (B2).
    pub fn enabled_command_specs(&self) -> Vec<(String, Vec<CommandSpec>)> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let mut specs = Vec::new();
            if let Some(commands) = loaded.manifest.as_ref().and_then(|m| m.commands.as_ref()) {
                for entry in commands {
                    if let CommandEntry::Inline(spec) = entry {
                        specs.push(spec.clone());
                    }
                }
            }
            if !specs.is_empty() {
                result.push((loaded.record.id.clone(), specs));
            }
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        result
    }

    /// Build a [`CommandRegistry`] from the inline commands of all enabled
    /// plugins.
    ///
    /// The registry is rebuilt from the current manager state on every
    /// call, so it always reflects the latest installs, enables, disables,
    /// and removes — call it again after any lifecycle change to reload.
    pub fn command_registry(&self) -> CommandRegistry {
        let registry = CommandRegistry::new();
        registry.reload(self.enabled_command_specs());
        registry
    }

    /// Collect every command contributed by enabled plugins in the TS
    /// `PluginCommandDef` shape — the `plugin.commands` RPC source (N4,
    /// the TS `enabledCommands` producer). Markdown-file commands are
    /// loaded and parsed (frontmatter `name`/`description` with the
    /// relative-path-derived fallbacks); inline specs map onto the same
    /// shape with `path: ""`. A file that fails to load (deleted, bad
    /// frontmatter) is dropped silently (the TS `loadPluginCommand`
    /// catch-all). Sorted by plugin id then command name for deterministic
    /// output (the TS order is record-insertion by manifest entry).
    pub fn enabled_commands(&self) -> Vec<PluginCommandDef> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        for loaded in map.values() {
            if !loaded.record.enabled || loaded.state != PluginState::Ok {
                continue;
            }
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            let Some(commands) = manifest.commands.as_ref() else {
                continue;
            };
            for entry in commands {
                match entry {
                    CommandEntry::File(file) => {
                        if let Some(parsed) = load_command_file(&file.path, Some(&file.name)) {
                            result.push(PluginCommandDef {
                                plugin_id: loaded.record.id.clone(),
                                name: parsed.name,
                                description: parsed.description,
                                body: parsed.body,
                                path: file.path.to_string_lossy().into_owned(),
                            });
                        }
                    }
                    CommandEntry::Inline(spec) => result.push(PluginCommandDef {
                        plugin_id: loaded.record.id.clone(),
                        name: spec.name.clone(),
                        description: spec.description.clone().unwrap_or_default(),
                        body: spec.template.clone(),
                        path: String::new(),
                    }),
                }
            }
        }
        result.sort_by(|left, right| {
            (&left.plugin_id, &left.name).cmp(&(&right.plugin_id, &right.name))
        });
        result
    }

    /// Run a command by name with the invocation's argument string —
    /// `plugin.activateCommand`. The TS markdown model wins over the
    /// inline-spec extension (D-B9): the first enabled plugin (by id) whose
    /// md command's **effective name** (frontmatter `name`, falling back to
    /// the relative-path-derived entry name — the same resolution
    /// [`Self::enabled_commands`] surfaces) matches is expanded with the TS
    /// `expandCommandArguments` semantics; otherwise the inline registry
    /// renders the template with minijinja. Unknown commands answer
    /// [`PluginError::CommandNotFound`]; an md file that cannot be loaded
    /// (deleted, bad frontmatter — the TS drop path) is skipped like the
    /// listing skips it.
    pub fn activate_command(&self, name: &str, args: &str) -> PluginResult<String> {
        let md_hit = {
            let map = self.loaded.read().expect("plugin loaded map lock poisoned");
            let mut plugins: Vec<&LoadedPlugin> = map
                .values()
                .filter(|loaded| loaded.record.enabled && loaded.state == PluginState::Ok)
                .collect();
            plugins.sort_by(|left, right| left.record.id.cmp(&right.record.id));
            let mut hit: Option<ParsedCommandFile> = None;
            'outer: for loaded in plugins {
                let Some(commands) = loaded.manifest.as_ref().and_then(|m| m.commands.as_ref())
                else {
                    continue;
                };
                for entry in commands {
                    let CommandEntry::File(file) = entry else {
                        continue;
                    };
                    if let Some(parsed) = load_command_file(&file.path, Some(&file.name))
                        && parsed.name == name
                    {
                        hit = Some(parsed);
                        break 'outer;
                    }
                }
            }
            hit
        };
        if let Some(parsed) = md_hit {
            return Ok(expand_command_arguments(&parsed.body, args));
        }
        self.command_registry().execute(name, args)
    }

    /// The command entries of every enabled plugin with a usable manifest.
    fn enabled_command_entries(&self) -> Vec<CommandEntry> {
        let map = self.loaded.read().expect("plugin loaded map lock poisoned");
        let mut result = Vec::new();
        let mut plugins: Vec<&LoadedPlugin> = map
            .values()
            .filter(|loaded| loaded.record.enabled && loaded.state == PluginState::Ok)
            .collect();
        plugins.sort_by(|left, right| left.record.id.cmp(&right.record.id));
        for loaded in plugins {
            if let Some(commands) = loaded.manifest.as_ref().and_then(|m| m.commands.as_ref()) {
                result.extend(commands.iter().cloned());
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Update checks
    // -----------------------------------------------------------------------

    /// Check every installed github-sourced plugin for updates, mirroring
    /// the TS `PluginManager.checkUpdates` (`agent-core-v2
    /// /app/plugin/manager.ts:230-246, 418-469`).
    ///
    /// Semantics, branch for branch:
    ///
    /// 1. Only github-sourced records are checked; a record whose
    ///    resolution fails (network error, unreachable repo) is **skipped
    ///    with a warning**, never fatal; the surviving results are sorted
    ///    by id.
    /// 2. Pinned tag / SHA — `{current, latest: current, updateAvailable:
    ///    false, displayVersion: current.value}` (no network).
    /// 3. Pinned branch — resolve the branch's latest SHA;
    ///    `updateAvailable = installedSha missing || != latestSha`;
    ///    `displayVersion` is the first 12 chars of the latest SHA.
    /// 4. Unpinned — resolve the latest ref (release tag, else `HEAD`);
    ///    `updateAvailable = ref changed || (branch/tag && installedSha
    ///    != latestSha)`.
    ///
    /// **Legacy records** (installed before `github_ref`/`installed_sha`
    /// existed, so both are `None`):
    ///
    /// - `commit: Some` → treated as pinned: `updateAvailable: false`,
    ///   `current = latest`, with the kind derived the install-time way
    ///   (40-hex → `sha`, else an Atom-feed probe: `branch` when the ref
    ///   resolves, `tag` otherwise).
    /// - `commit: None` → treated as a moving ref with a missing
    ///   `installedSha`: `updateAvailable: true` (the TS
    ///   `installedSha === undefined ||` semantics), `current` omitted,
    ///   `latest` the freshly resolved ref.
    pub async fn check_updates(&self) -> Vec<PluginUpdateStatus> {
        let records = match self.store.load() {
            Ok(records) => records,
            Err(e) => {
                warn!(error = %e, "failed to load plugin records for update check");
                return Vec::new();
            }
        };

        let mut results = Vec::new();
        for record in records {
            let PluginSource::Github { repo, .. } = &record.source else {
                continue;
            };
            match self.check_github_update(&record, repo).await {
                Ok(status) => results.push(status),
                Err(e) => {
                    // TS semantics: a per-plugin failure silently drops the
                    // record from the result.
                    warn!(plugin = %record.id, error = %e, "update check failed, skipping");
                }
            }
        }
        results.sort_by(|a, b| a.id.cmp(&b.id));
        results
    }

    /// The per-record update check — the TS `checkGithubUpdate` mirror.
    /// Errors skip the record (see [`Self::check_updates`]).
    async fn check_github_update(
        &self,
        record: &InstalledPlugin,
        repo: &str,
    ) -> Result<PluginUpdateStatus, String> {
        // Legacy records (installed before `github_ref`/`installed_sha`
        // were captured) never reach the four-branch logic — the mini-design
        // ruling fixes their answers up front.
        if record.github_ref.is_none() {
            let commit = match &record.source {
                PluginSource::Github { commit, .. } => commit.as_deref(),
                _ => None,
            };
            return self.check_legacy_update(record, repo, commit).await;
        }

        let installed_sha = record.installed_sha.as_deref();
        let current = record.github_ref.clone().expect("checked above");

        // The pin is where the install's explicit ref lives. For records
        // this crate writes it matches the source's `commit`/`kind`; for
        // TS-written records (which persist no commit in the file) it is
        // re-derived from `originalSource` with the captured `github.ref`
        // as fallback — the TS `explicitGithubRef` (item A6 read interop).
        let pinned = explicit_github_ref(record);

        match pinned {
            Some(pinned) => match pinned.kind {
                // Branch 2: pinned tag / SHA — pinned, no network.
                GithubRefKind::Tag | GithubRefKind::Sha => Ok(PluginUpdateStatus {
                    id: record.id.clone(),
                    source: "github",
                    latest: current.clone(),
                    display_version: current.value.clone(),
                    update_available: false,
                    current: Some(current),
                }),
                // Branch 3: pinned branch — compare against the latest SHA.
                GithubRefKind::Branch => {
                    let latest_sha = self
                        .github_resolver
                        .resolve_commit_sha(repo, &pinned.value)
                        .await?;
                    let update_available = match installed_sha {
                        Some(sha) => sha != latest_sha,
                        None => true,
                    };
                    Ok(PluginUpdateStatus {
                        id: record.id.clone(),
                        source: "github",
                        latest: current.clone(),
                        display_version: latest_sha.chars().take(12).collect(),
                        update_available,
                        current: Some(current),
                    })
                }
            },
            // Branch 4: unpinned — resolve the latest ref and compare.
            None => {
                let (latest, display_version) =
                    self.github_resolver.resolve_latest_ref(repo).await?;
                let update_available = {
                    let ref_changed = current.kind != latest.kind || current.value != latest.value;
                    if ref_changed {
                        true
                    } else if matches!(latest.kind, GithubRefKind::Branch | GithubRefKind::Tag) {
                        let latest_sha = self
                            .github_resolver
                            .resolve_commit_sha(repo, &latest.value)
                            .await?;
                        match installed_sha {
                            Some(sha) => sha != latest_sha,
                            None => true,
                        }
                    } else {
                        false
                    }
                };
                Ok(PluginUpdateStatus {
                    id: record.id.clone(),
                    source: "github",
                    display_version,
                    update_available,
                    latest,
                    current: Some(current),
                })
            }
        }
    }

    /// The legacy-record update check — records installed before
    /// `github_ref`/`installed_sha` were captured.
    ///
    /// - `commit: Some` → treated as pinned: `updateAvailable: false`,
    ///   `current = latest`, with the kind derived the install-time way
    ///   (40-hex → `sha`, else an Atom-feed probe: `branch` when the ref
    ///   resolves, `tag` otherwise).
    /// - `commit: None` → treated as a moving ref with a missing
    ///   `installedSha`: `updateAvailable: true` (the TS
    ///   `installedSha === undefined ||` semantics), `current` omitted,
    ///   `latest` the freshly resolved ref.
    async fn check_legacy_update(
        &self,
        record: &InstalledPlugin,
        repo: &str,
        commit: Option<&str>,
    ) -> Result<PluginUpdateStatus, String> {
        let Some(ref_name) = commit else {
            let (latest, display_version) = self.github_resolver.resolve_latest_ref(repo).await?;
            return Ok(PluginUpdateStatus {
                id: record.id.clone(),
                source: "github",
                display_version,
                update_available: true,
                latest,
                current: None,
            });
        };

        let kind = if is_full_sha(ref_name) {
            GithubRefKind::Sha
        } else {
            match self
                .github_resolver
                .resolve_commit_sha(repo, ref_name)
                .await
            {
                Ok(_) => GithubRefKind::Branch,
                Err(_) => GithubRefKind::Tag,
            }
        };
        let current = GithubRef {
            kind,
            value: ref_name.to_string(),
        };
        Ok(PluginUpdateStatus {
            id: record.id.clone(),
            source: "github",
            display_version: current.value.clone(),
            update_available: false,
            latest: current.clone(),
            current: Some(current),
        })
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------
}

/// The TS `explicitGithubRef` mirror (manager.ts): the pinned ref of a
/// github-sourced record — the explicit ref spelled at install time. The TS
/// re-parses the persisted `originalSource` (`source.kind === 'github' ?
/// source.ref : fallback`); records whose original source is a `github.com`
/// URL carry the kind in the URL grammar. When the source re-parse yields no
/// explicit ref, the captured `github.ref` is the fallback — a sha or a
/// non-HEAD branch pins, anything else (tag, `HEAD`) means the record
/// follows the unpinned branch.
///
/// For this crate's rust-only `github:repo[@ref]` syntax (unknown to the TS
/// grammar) the captured install-time metadata rules instead (D-M1): a
/// 40-hex ref is an unambiguous sha pin; any other ref falls back to the
/// captured metadata. Returns `None` for unpinned records.
fn explicit_github_ref(record: &InstalledPlugin) -> Option<GithubRef> {
    let fallback = match &record.github_ref {
        Some(r) if r.kind == GithubRefKind::Sha
            || (r.kind == GithubRefKind::Branch && r.value != "HEAD") =>
        {
            Some(r.clone())
        }
        _ => None,
    };
    let Some(original) = &record.original_source else {
        return fallback;
    };
    match parse_source(original) {
        Ok(PluginSource::Github {
            commit: Some(ref_name),
            kind: Some(kind),
            ..
        }) => Some(GithubRef {
            kind,
            value: ref_name,
        }),
        Ok(PluginSource::Github {
            commit: Some(ref_name),
            kind: None,
            ..
        }) => {
            if is_full_sha(&ref_name) {
                Some(GithubRef::sha(ref_name.to_lowercase()))
            } else {
                // rust-only `github:repo@ref` syntax: the `@ref` is an
                // explicit pin; the kind comes from the install-time
                // capture (D-M1 — a branch probe classified it then), with a
                // non-SHA ref defaulting to a branch pin (the TS
                // `/tree/<ref>` classification) when no capture exists.
                Some(
                    record
                        .github_ref
                        .clone()
                        .unwrap_or_else(|| GithubRef::branch(ref_name)),
                )
            }
        }
        _ => fallback,
    }
}

/// The TS `normalizeInstallRoot` (manager.ts): trim the path, require an
/// absolute path, resolve symlinks with `realpath` (canonicalize), and
/// require a directory. The error messages mirror the TS text (item A4).
fn normalize_install_root(root: &Path) -> PluginResult<PathBuf> {
    let trimmed = root.to_string_lossy().trim().to_string();
    if !Path::new(&trimmed).is_absolute() {
        return Err(PluginError::InstallFailed(format!(
            "Plugin root must be an absolute path (got \"{trimmed}\")"
        )));
    }
    let resolved = std::fs::canonicalize(&trimmed).map_err(|_| {
        PluginError::InstallFailed(format!("Plugin root does not exist: {trimmed}"))
    })?;
    if !resolved.is_dir() {
        return Err(PluginError::InstallFailed(format!(
            "Plugin root is not a directory: {trimmed}"
        )));
    }
    Ok(resolved)
}

/// A published managed copy: the plugin's root and the previous root it
/// replaced (absent on a first install).
struct ManagedCopy {
    /// The published (realpath'd) managed root.
    root: PathBuf,
    /// The previous managed root, moved aside before publishing.
    previous_root: Option<PathBuf>,
}

/// The TS `copyPluginToManagedRoot` (manager.ts): copy `source_root` into a
/// fresh staging directory under `<plugins_dir>/managed/`, move the current
/// managed root aside, and rename the staging directory into place (item
/// A2's swap). On failure the copy cleans up after itself — the alive copy
/// (staging, or the published managed root) is removed and the
/// moved-aside root is renamed back — and returns the error.
fn copy_plugin_to_managed_root(
    plugins_dir: &Path,
    id: &str,
    source_root: &Path,
) -> PluginResult<ManagedCopy> {
    let managed_root = plugins_dir.join("managed").join(id);
    let managed_dir = managed_root
        .parent()
        .expect("managed root always has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&managed_dir)?;
    let staging_root = mkdtemp(&managed_dir, &format!("{id}-"))?;
    let previous_root = PathBuf::from(format!("{}-previous", staging_root.display()));

    let mut moved_previous = false;
    let mut published = false;
    let result = (|| -> PluginResult<()> {
        copy_dir_recursive(source_root, &staging_root)?;
        match std::fs::rename(&managed_root, &previous_root) {
            Ok(()) => moved_previous = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::rename(&staging_root, &managed_root)?;
        published = true;
        Ok(())
    })();

    if let Err(error) = result {
        // The `copyPluginToManagedRoot` internal catch: remove the alive
        // copy and, when a previous root was moved aside, restore it.
        let _ = std::fs::remove_dir_all(if published { &managed_root } else { &staging_root });
        if moved_previous {
            let _ = std::fs::rename(&previous_root, &managed_root);
        }
        return Err(error);
    }

    // The TS `root: await realpath(managedRoot)`.
    let root = std::fs::canonicalize(&managed_root)?;
    Ok(ManagedCopy {
        root,
        previous_root: moved_previous.then_some(previous_root),
    })
}

/// The TS `rollbackManagedPluginCopy` (manager.ts): remove the published
/// root and rename the previous root back into place.
fn rollback_managed_plugin_copy(copy: &ManagedCopy) -> PluginResult<()> {
    std::fs::remove_dir_all(&copy.root)?;
    if let Some(previous_root) = &copy.previous_root {
        std::fs::rename(previous_root, &copy.root)?;
    }
    Ok(())
}

/// Create a unique empty directory under `parent` with `prefix` — the TS
/// `mkdtemp` equivalent (no runtime tempfile dependency).
fn mkdtemp(parent: &Path, prefix: &str) -> PluginResult<PathBuf> {
    for _ in 0..100 {
        let candidate = parent.join(format!("{prefix}{}", uuid::Uuid::new_v4()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(PluginError::InstallFailed(format!(
        "could not create a staging directory under {}",
        parent.display()
    )))
}

/// Serialize a validated MCP server config back to its wire shape
/// (`{"transport": "stdio", ...}`) — the external shape of
/// [`PluginManager::enabled_mcp_servers`] / [`PluginManager::mcp_server_entries`]
/// stays `serde_json::Value` while the manifest parse front-end validates
/// every entry (B7).
fn mcp_config_to_value(config: &McpServerConfig) -> serde_json::Value {
    serde_json::to_value(config).unwrap_or(serde_json::Value::Null)
}

/// The TS `withPluginMcpRuntime` decoration (manager.ts, item A7) applied
/// at [`PluginManager::enabled_mcp_servers`]'s exit: stdio servers default
/// their `cwd` to the plugin root and merge `MIRRICODE_HOME` (the
/// manager's home dir, same value as the item E hook decoration) plus
/// `MIRRI_PLUGIN_ROOT` into their env — the merged facts win over
/// manifest-declared same-name keys, exactly like the TS
/// `{ ...config.env, MIRRICODE_HOME, MIRRI_PLUGIN_ROOT }` spread; other
/// manifest env pulls through. http/sse servers pass through untouched
/// (the TS `transport === 'http' || transport === 'sse'` early return).
/// The TS `node` command-rewrite branches (electron / native fallback
/// subcommand) are N/A for the rust host — a native binary cannot run node
/// scripts by itself (decision D-A7).
fn with_plugin_mcp_runtime(
    config: McpServerConfig,
    plugin_root: &str,
    home_dir: &str,
) -> McpServerConfig {
    let McpServerConfig::Stdio(mut stdio) = config else {
        return config;
    };
    if stdio.cwd.is_none() {
        stdio.cwd = Some(plugin_root.to_string());
    }
    let mut env = stdio.env.take().unwrap_or_default();
    env.insert("MIRRICODE_HOME".to_string(), home_dir.to_string());
    env.insert("MIRRI_PLUGIN_ROOT".to_string(), plugin_root.to_string());
    stdio.env = Some(env);
    McpServerConfig::Stdio(stdio)
}

/// The klient `pluginSourceSchema` kind of a [`PluginSource`] — the
/// `pluginSummarySchema.source` field (N1): `"local-path"` | `"zip-url"` |
/// `"github"`.
fn plugin_source_kind(source: &PluginSource) -> &'static str {
    match source {
        PluginSource::Local { .. } => "local-path",
        PluginSource::Zip { .. } => "zip-url",
        PluginSource::Github { .. } => "github",
    }
}

/// The summary's `github` attribution — the TS `recordToSummary`
/// passthrough of the install-captured metadata (N1): `{owner, repo, ref,
/// installedSha?}` from the store record, degraded to `None` when no ref
/// was captured (legacy / non-github records then omit the field, matching
/// the TS `github: record.github` which is `undefined` for such installs).
fn github_metadata(record: &InstalledPlugin) -> Option<PluginGithubMetadata> {
    let PluginSource::Github { repo, .. } = &record.source else {
        return None;
    };
    // `github:` sources and installed.json records carry `owner/repo` in
    // one string; the URL grammar stores it the same way (A1).
    let (owner, repo_name) = repo.split_once('/')?;
    let r#ref = record.github_ref.clone()?;
    Some(PluginGithubMetadata {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        r#ref,
        installed_sha: record.installed_sha.clone(),
    })
}

/// The TS `materialize` step of `reload()` (item C3): parse the record's
/// manifest and build the loaded plugin. Kept as a fallible seam — the rust
/// steps are all tolerant today (`parse_manifest` returns diagnostics instead
/// of throwing, and skill counting is a guarded walk), so a failure here is
/// currently unreachable, exactly like the TS try/catch scope around
/// `materialize` — but any future per-record failure lands in the reload
/// `errors` list instead of silently dropping the record.
fn materialize_record(record: InstalledPlugin) -> Result<LoadedPlugin, String> {
    let parsed = parse_manifest(&record.path);
    Ok(LoadedPlugin::from_parsed(parsed, record.clone(), record.path.clone()))
}

/// Whether a manifest MCP server is effectively enabled — the TS
/// `isMcpServerEnabled` (`state?.enabled ?? config.enabled !== false`,
/// manager.ts): the recorded per-server user state wins; without a record
/// the manifest's own `enabled` applies (absent means enabled).
fn is_mcp_server_effectively_enabled(
    record: &InstalledPlugin,
    name: &str,
    config: &McpServerConfig,
) -> bool {
    if let Some(state) = record.mcp_servers.get(name) {
        return state.enabled;
    }
    let manifest_enabled = match config {
        McpServerConfig::Stdio(c) => c.common.enabled,
        McpServerConfig::Http(c) => c.common.enabled,
        McpServerConfig::Sse(c) => c.common.enabled,
    };
    manifest_enabled != Some(false)
}

/// The `plugin.info` `mcpServers` list — the TS `pluginMcpServersInfo`
/// producer (N3): every manifest server as [`PluginMcpServerInfo`], sorted
/// by name. stdio servers carry `command`/`args`/`cwd`/`envKeys`; http/sse
/// servers carry `url`/`headerKeys` (sorted key names).
fn mcp_servers_info(loaded: &LoadedPlugin) -> Vec<PluginMcpServerInfo> {
    let Some(servers) = loaded.manifest.as_ref().and_then(|m| m.mcp_servers.as_ref()) else {
        return Vec::new();
    };
    let mut rows: Vec<PluginMcpServerInfo> = servers
        .iter()
        .map(|(name, config)| {
            let runtime_name = plugin_mcp_runtime_name(&loaded.record.id, name);
            let enabled = is_mcp_server_effectively_enabled(&loaded.record, name, config);
            match config {
                McpServerConfig::Stdio(c) => PluginMcpServerInfo {
                    name: name.clone(),
                    runtime_name,
                    enabled,
                    transport: "stdio",
                    command: Some(c.command.clone()),
                    args: c.args.clone(),
                    cwd: c.cwd.clone(),
                    url: None,
                    env_keys: c.env.as_ref().map(|env| {
                        let mut keys: Vec<String> = env.keys().cloned().collect();
                        keys.sort();
                        keys
                    }),
                    header_keys: None,
                },
                McpServerConfig::Http(c) => PluginMcpServerInfo {
                    name: name.clone(),
                    runtime_name,
                    enabled,
                    transport: "http",
                    command: None,
                    args: None,
                    cwd: None,
                    url: Some(c.url.clone()),
                    env_keys: None,
                    header_keys: c.headers.as_ref().map(|headers| {
                        let mut keys: Vec<String> = headers.keys().cloned().collect();
                        keys.sort();
                        keys
                    }),
                },
                McpServerConfig::Sse(c) => PluginMcpServerInfo {
                    name: name.clone(),
                    runtime_name,
                    enabled,
                    transport: "sse",
                    command: None,
                    args: None,
                    cwd: None,
                    url: Some(c.url.clone()),
                    env_keys: None,
                    header_keys: c.headers.as_ref().map(|headers| {
                        let mut keys: Vec<String> = headers.keys().cloned().collect();
                        keys.sort();
                        keys
                    }),
                },
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Serialize a parsed manifest into the klient `pluginManifestSchema` wire
/// shape (N3) — the TS `PluginManifest` JSON, field for field:
///
/// - the top-level fields serialize through the struct's camelCase serde
///   (`mcpServers`, `sessionStart`, `systemPrompt`, `skillInstructions`,
///   `interface`, `author`, …);
/// - `commands` is narrowed to the TS `{path, name}` entries — the
///   Rust-only inline specs (D-B9) are dropped from the wire so klient
///   parsing never trips on the unknown shape;
/// - `hooks` is remapped from the engine `HookDef` to the TS `{event,
///   matcher?, command, timeout?}` item (pattern→matcher, timeout_ms→
///   seconds), dropping rust-only event kinds the TS enum does not know.
///
/// Returns `None` when the manifest cannot be serialized at all.
fn plugin_manifest_wire(manifest: &PluginManifest) -> Option<serde_json::Value> {
    let mut wire = serde_json::to_value(manifest).ok()?;
    let object = wire.as_object_mut()?;

    if let Some(commands) = object.get_mut("commands").and_then(|v| v.as_array_mut()) {
        commands.retain(|entry| entry.get("path").is_some() && entry.get("name").is_some());
    }

    if let Some(hooks) = object.get_mut("hooks").and_then(|v| v.as_array_mut()) {
        *hooks = hooks
            .iter()
            .filter_map(|hook| {
                let event = hook.get("event")?.as_str()?;
                if !TS_HOOK_EVENTS.contains(&event) {
                    return None;
                }
                let mut item = serde_json::Map::new();
                item.insert("event".to_string(), serde_json::Value::String(event.to_string()));
                if let Some(matcher) = hook.get("pattern").and_then(|v| v.as_str()) {
                    item.insert("matcher".to_string(), serde_json::json!(matcher));
                }
                item.insert(
                    "command".to_string(),
                    hook.get("command")?.clone(),
                );
                if let Some(timeout_ms) = hook.get("timeout_ms").and_then(|v| v.as_u64())
                    && timeout_ms > 0
                {
                    // TS `hookDefSchema` bounds the timeout to 1..=600
                    // seconds — clamp the ms value into that window.
                    item.insert(
                        "timeout".to_string(),
                        serde_json::json!((timeout_ms / 1000).clamp(1, 600)),
                    );
                }
                Some(serde_json::Value::Object(item))
            })
            .collect();
    }

    Some(wire)
}

/// The TS `hookDefSchema` event names (`plugins.ts:40-58`) — the rust
/// HookEngine's extra event kinds (`PreLlmRequest`, `PostLlmRequest`)
/// are engine-internal and filtered off the wire.
const TS_HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionResult",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Notification",
];

/// One [`LoadedPlugin`] row of `plugin.list` / `plugin.install` — the TS
/// `recordToSummary` mirror (N1/N2).
fn summary_from_loaded(loaded: &LoadedPlugin) -> PluginSummary {
    let id = loaded
        .manifest
        .as_ref()
        .map(|m| m.name.clone())
        .unwrap_or_else(|| loaded.record.id.clone());
    let manifest = loaded.manifest.as_ref();
    let mcp_server_count = manifest
        .and_then(|m| m.mcp_servers.as_ref())
        .map_or(0, |servers| servers.len());
    let enabled_mcp_server_count = manifest
        .and_then(|m| m.mcp_servers.as_ref())
        .map_or(0, |servers| {
            servers
                .iter()
                .filter(|(name, config)| {
                    is_mcp_server_effectively_enabled(&loaded.record, name, config)
                })
                .count()
        });
    let skill_count = discovered_skill_count(&id, manifest);
    PluginSummary {
        display_name: manifest
            .and_then(|m| m.interface.as_ref())
            .and_then(|i| i.display_name.clone())
            .unwrap_or_else(|| id.clone()),
        id,
        version: manifest.and_then(|m| m.version.clone()),
        enabled: loaded.record.enabled,
        state: loaded.state,
        skill_count,
        mcp_server_count,
        enabled_mcp_server_count,
        hook_count: manifest.and_then(|m| m.hooks.as_ref()).map_or(0, |h| h.len()),
        command_count: manifest.and_then(|m| m.commands.as_ref()).map_or(0, |c| c.len()),
        has_errors: loaded.diagnostics.iter().any(|d| {
            d.severity == crate::manifest::PluginDiagnosticSeverity::Error
        }),
        source: plugin_source_kind(&loaded.record.source),
        original_source: loaded.record.original_source.clone(),
        github: github_metadata(&loaded.record),
    }
}

/// The summary's `skillCount` — the real **discovery** count (D-N2
/// upgrade), the rust mirror of the TS `countDiscoveredPluginSkills`
/// (`manager.ts:798-812`). Walks every manifest skill root and counts the
/// skill names the catalog would index — directories holding a `SKILL.md`,
/// recursively, deduped by name within the plugin (the rust catalog's name
/// key; TS dedups by `${pluginId}\0${name}`). A declared root that does
/// not exist or holds no `SKILL.md` contributes nothing, so the count is
/// the number of usable plugin skills, never the manifest entry count.
///
/// Known divergence: TS additionally registers root-level `*.md` files as
/// skills for plugin roots; the rust catalog has no such channel, so they
/// are not counted either (the count mirrors what the engine indexes).
fn discovered_skill_count(
    plugin_id: &str,
    manifest: Option<&crate::manifest::PluginManifest>,
) -> usize {
    let mut names = HashSet::new();
    for dir in manifest
        .and_then(|m| m.skills.as_deref())
        .unwrap_or_default()
    {
        collect_skill_names(Path::new(dir), &mut names);
    }
    // The TS discovery key carries the plugin id; rust's catalog is
    // workspace-wide keyed by name, and summary counts are per plugin, so
    // the id is part of the contract but not of the dedup set.
    let _ = plugin_id;
    names.len()
}

/// Recursively collect the skill names under one root: every directory
/// (the root itself included) that contains a `SKILL.md` contributes its
/// basename. Unreadable or missing directories contribute nothing — the
/// same tolerance the discovery walk applies.
fn collect_skill_names(dir: &Path, out: &mut HashSet<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut subdirs = Vec::new();
    let mut has_skill_md = false;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            subdirs.push(entry_path);
        } else if entry.file_name().to_str() == Some(mirri_core::skill::SKILL_MD_FILE) {
            has_skill_md = true;
        }
    }
    if has_skill_md
        && let Some(name) = dir.file_name().and_then(|name| name.to_str())
    {
        out.insert(name.to_string());
    }
    for subdir in subdirs {
        collect_skill_names(&subdir, out);
    }
}

/// One [`LoadedPlugin`] row of `plugin.info` — the TS `recordToInfo`
/// mirror (N3/L).
fn info_from_loaded(loaded: &LoadedPlugin) -> PluginInfoView {
    PluginInfoView {
        summary: summary_from_loaded(loaded),
        root: loaded.root.display().to_string(),
        installed_at: loaded.record.installed_at.to_rfc3339(),
        updated_at: Some(loaded.record.updated_at.to_rfc3339()),
        manifest_kind: loaded.manifest_kind,
        manifest_path: loaded
            .manifest_path
            .as_ref()
            .map(|p| p.display().to_string()),
        manifest: loaded
            .manifest
            .as_ref()
            .and_then(plugin_manifest_wire),
        mcp_servers: mcp_servers_info(loaded),
        shadowed_manifest_path: loaded
            .shadowed_manifest_path
            .as_ref()
            .map(|p| p.display().to_string()),
        diagnostics: loaded.diagnostics.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::ResolverFuture;

    /// A stub GitHub resolver — canned answers per repo, so the update-check
    /// tests never touch the network.
    #[derive(Default)]
    struct StubResolver {
        /// repo → SHA returned for every `resolve_commit_sha` call.
        commit_shas: std::collections::HashMap<String, String>,
        /// repo → (ref, display) returned by `resolve_latest_ref`.
        latest_refs: std::collections::HashMap<String, (GithubRef, String)>,
        /// repos whose resolution always fails.
        failing: std::collections::HashSet<String>,
    }

    impl StubResolver {
        fn with_commit_sha(mut self, repo: &str, sha: &str) -> Self {
            self.commit_shas.insert(repo.to_string(), sha.to_string());
            self
        }

        fn with_latest_tag(mut self, repo: &str, tag: &str) -> Self {
            self.latest_refs
                .insert(repo.to_string(), (GithubRef::tag(tag), tag.to_string()));
            self
        }

        fn with_latest_head(mut self, repo: &str) -> Self {
            self.latest_refs.insert(
                repo.to_string(),
                (GithubRef::branch("HEAD"), "HEAD".to_string()),
            );
            self
        }

        fn failing(mut self, repo: &str) -> Self {
            self.failing.insert(repo.to_string());
            self
        }
    }

    impl GithubResolver for StubResolver {
        fn resolve_commit_sha<'a>(
            &'a self,
            repo: &'a str,
            _ref_name: &'a str,
        ) -> ResolverFuture<'a, String> {
            Box::pin(async move {
                if self.failing.contains(repo) {
                    return Err(format!("stub: {repo} unreachable"));
                }
                self.commit_shas
                    .get(repo)
                    .cloned()
                    .ok_or_else(|| format!("stub: no SHA configured for {repo}"))
            })
        }

        fn resolve_latest_ref<'a>(
            &'a self,
            repo: &'a str,
        ) -> ResolverFuture<'a, (GithubRef, String)> {
            Box::pin(async move {
                if self.failing.contains(repo) {
                    return Err(format!("stub: {repo} unreachable"));
                }
                self.latest_refs
                    .get(repo)
                    .cloned()
                    .ok_or_else(|| format!("stub: no latest ref configured for {repo}"))
            })
        }
    }

    /// A store-backed manager with hand-seeded github records — the
    /// update-check tests never install from the network. Records seed
    /// through the store (which writes the v1 envelope), so they exercise
    /// the post-A6 file round trip.
    fn manager_with_records(
        resolver: Arc<dyn GithubResolver>,
        records: Vec<InstalledPlugin>,
    ) -> (tempfile::TempDir, PluginManager) {
        let dir = tempfile::tempdir().expect("failed to create plugins dir");
        let manager = PluginManager::new_with_github_resolver(dir.path().to_path_buf(), resolver);
        for record in records {
            manager
                .store
                .add(record)
                .expect("seed record should persist");
        }
        (dir, manager)
    }

    /// A store-backed manager seeded with a **legacy** bare-array
    /// `installed.json` (pre-A6: snake_case records, tagged source objects)
    /// — the legacy update-check records (`github_ref` absent) only exist in
    /// that format; a v1 round trip would drop their `commit` pin (the TS
    /// format has no source-commit field, item A6).
    fn manager_with_legacy_records(
        resolver: Arc<dyn GithubResolver>,
        records: &[InstalledPlugin],
    ) -> (tempfile::TempDir, PluginManager) {
        let dir = tempfile::tempdir().expect("failed to create plugins dir");
        let legacy: Vec<serde_json::Value> = records.iter().map(legacy_record_json).collect();
        std::fs::write(
            dir.path().join("installed.json"),
            serde_json::to_string_pretty(&legacy).expect("legacy json"),
        )
        .expect("write legacy store");
        let manager = PluginManager::new_with_github_resolver(dir.path().to_path_buf(), resolver);
        (dir, manager)
    }

    /// Serialize a record into the pre-A6 installed.json row shape (the
    /// tagged `{type, ...}` source object, flat `github_ref`/`installed_sha`
    /// fields) for the legacy seeding helper.
    fn legacy_record_json(record: &InstalledPlugin) -> serde_json::Value {
        let source = match &record.source {
            PluginSource::Github { repo, commit, kind } => serde_json::json!({
                "type": "github",
                "repo": repo,
                "commit": commit,
                "kind": kind,
            }),
            other => serde_json::to_value(other).expect("source json"),
        };
        serde_json::json!({
            "id": record.id,
            "version": record.version,
            "source": source,
            "original_source": record.original_source,
            "enabled": record.enabled,
            "installed_at": record.installed_at.to_rfc3339(),
            "updated_at": record.updated_at.to_rfc3339(),
            "path": record.path,
            "github_ref": record.github_ref,
            "installed_sha": record.installed_sha,
        })
    }

    fn github_record(
        id: &str,
        repo: &str,
        commit: Option<&str>,
        github_ref: Option<GithubRef>,
        installed_sha: Option<&str>,
    ) -> InstalledPlugin {
        let now = Utc::now();
        InstalledPlugin {
            id: id.to_string(),
            version: Some("1.0.0".to_string()),
            source: PluginSource::Github {
                repo: repo.to_string(),
                commit: commit.map(str::to_string),
                kind: None,
            },
            // The realistic rust install echo: `github:repo[@ref]`.
            original_source: Some(match commit {
                Some(ref_name) => format!("github:{repo}@{ref_name}"),
                None => format!("github:{repo}"),
            }),
            enabled: true,
            installed_at: now,
            updated_at: now,
            path: PathBuf::from("/plugins").join(id),
            github_ref,
            installed_sha: installed_sha.map(str::to_string),
            mcp_servers: std::collections::HashMap::new(),
        }
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[tokio::test]
    async fn should_return_empty_list_when_no_plugins_are_installed() {
        let (_dir, manager) = manager_with_records(Arc::new(StubResolver::default()), Vec::new());
        assert!(manager.check_updates().await.is_empty());
    }

    #[tokio::test]
    async fn should_skip_update_check_when_plugin_is_not_github_sourced() {
        let now = Utc::now();
        let local_record = InstalledPlugin {
            id: "local-plugin".to_string(),
            version: None,
            source: PluginSource::Local {
                path: "/source".to_string(),
            },
            original_source: Some("/source".to_string()),
            enabled: true,
            installed_at: now,
            updated_at: now,
            path: PathBuf::from("/plugins/local-plugin"),
            github_ref: None,
            installed_sha: None,
            mcp_servers: std::collections::HashMap::new(),
        };
        let (_dir, manager) =
            manager_with_records(Arc::new(StubResolver::default()), vec![local_record]);
        assert!(manager.check_updates().await.is_empty());
    }

    #[tokio::test]
    async fn should_report_no_update_when_plugin_is_pinned_to_a_sha() {
        // No stub answers configured: a pinned SHA must resolve nothing.
        let (_dir, manager) = manager_with_records(
            Arc::new(StubResolver::default()),
            vec![github_record(
                "pinned",
                "user/repo",
                Some(SHA),
                Some(GithubRef::sha(SHA)),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.id, "pinned");
        assert_eq!(status.source, "github");
        assert_eq!(status.current, Some(GithubRef::sha(SHA)));
        assert_eq!(status.latest, GithubRef::sha(SHA));
        assert_eq!(status.display_version, SHA);
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_report_no_update_when_plugin_is_pinned_to_a_tag() {
        let (_dir, manager) = manager_with_records(
            Arc::new(StubResolver::default()),
            vec![github_record(
                "tagged",
                "user/repo",
                Some("v1.0.0"),
                Some(GithubRef::tag("v1.0.0")),
                None,
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.latest, GithubRef::tag("v1.0.0"));
        assert_eq!(status.display_version, "v1.0.0");
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_report_no_update_when_pinned_branch_sha_matches_latest() {
        let resolver = StubResolver::default().with_commit_sha("user/repo", SHA);
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "branchy",
                "user/repo",
                Some("main"),
                Some(GithubRef::branch("main")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.latest, GithubRef::branch("main"));
        assert_eq!(status.display_version, &SHA[..12]);
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_report_update_when_pinned_branch_sha_differs_from_latest() {
        let resolver = StubResolver::default()
            .with_commit_sha("user/repo", "fedcba9876543210fedcba9876543210fedcba98");
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "branchy",
                "user/repo",
                Some("main"),
                Some(GithubRef::branch("main")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].update_available);
    }

    #[tokio::test]
    async fn should_report_update_when_pinned_branch_sha_is_missing() {
        let resolver = StubResolver::default().with_commit_sha("user/repo", SHA);
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "branchy",
                "user/repo",
                Some("main"),
                Some(GithubRef::branch("main")),
                None,
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].update_available);
    }

    #[tokio::test]
    async fn should_report_no_update_when_unpinned_latest_tag_matches_and_sha_matches() {
        let resolver = StubResolver::default()
            .with_latest_tag("user/repo", "v2.0.0")
            .with_commit_sha("user/repo", SHA);
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "taggy",
                "user/repo",
                None,
                Some(GithubRef::tag("v2.0.0")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.latest, GithubRef::tag("v2.0.0"));
        assert_eq!(status.display_version, "v2.0.0");
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_report_update_when_unpinned_latest_tag_changed() {
        // Installed at v1.0.0; the repo's latest release moved to v2.0.0.
        let resolver = StubResolver::default().with_latest_tag("user/repo", "v2.0.0");
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "taggy",
                "user/repo",
                None,
                Some(GithubRef::tag("v1.0.0")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.latest, GithubRef::tag("v2.0.0"));
        assert_eq!(status.display_version, "v2.0.0");
        assert!(status.update_available);
    }

    #[tokio::test]
    async fn should_report_update_when_unpinned_head_sha_differs() {
        // HEAD fallback arm: the installed ref is branch HEAD, the latest
        // stays HEAD, and the resolved SHA has moved on.
        let resolver = StubResolver::default()
            .with_latest_head("user/repo")
            .with_commit_sha("user/repo", "fedcba9876543210fedcba9876543210fedcba98");
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "heady",
                "user/repo",
                None,
                Some(GithubRef::branch("HEAD")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.latest, GithubRef::branch("HEAD"));
        assert_eq!(status.display_version, "HEAD");
        assert!(status.update_available);
    }

    #[tokio::test]
    async fn should_report_no_update_when_unpinned_head_sha_matches() {
        let resolver = StubResolver::default()
            .with_latest_head("user/repo")
            .with_commit_sha("user/repo", SHA);
        let (_dir, manager) = manager_with_records(
            Arc::new(resolver),
            vec![github_record(
                "heady",
                "user/repo",
                None,
                Some(GithubRef::branch("HEAD")),
                Some(SHA),
            )],
        );

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].update_available);
    }

    #[tokio::test]
    async fn should_report_no_update_when_legacy_record_has_pinned_commit() {
        // Legacy record: no captured metadata, but an explicit commit —
        // treated as pinned, updateAvailable false.
        let resolver = StubResolver::default().with_commit_sha("user/repo", SHA);
        let record = github_record("legacy-pin", "user/repo", Some("main"), None, None);
        let (_dir, manager) = manager_with_legacy_records(Arc::new(resolver), &[record]);

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.current, Some(GithubRef::branch("main")));
        assert_eq!(status.latest, GithubRef::branch("main"));
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_treat_legacy_record_commit_as_sha_when_it_is_40_hex() {
        let record = github_record("legacy-sha", "user/repo", Some(SHA), None, None);
        let (_dir, manager) = manager_with_legacy_records(Arc::new(StubResolver::default()), &[record]);

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.current, Some(GithubRef::sha(SHA.to_lowercase())));
        assert_eq!(status.latest, GithubRef::sha(SHA.to_lowercase()));
        assert!(!status.update_available);
    }

    #[tokio::test]
    async fn should_report_update_when_legacy_record_has_no_commit() {
        // Legacy unpinned record: moving ref with a missing installedSha —
        // always an update.
        let resolver = StubResolver::default().with_latest_tag("user/repo", "v2.0.0");
        let record = github_record("legacy-move", "user/repo", None, None, None);
        let (_dir, manager) = manager_with_legacy_records(Arc::new(resolver), &[record]);

        let statuses = manager.check_updates().await;
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.current, None);
        assert_eq!(status.latest, GithubRef::tag("v2.0.0"));
        assert!(status.update_available);
    }

    #[tokio::test]
    async fn should_skip_failing_plugins_and_sort_results_by_id() {
        let resolver = StubResolver::default()
            .with_latest_head("zeta/repo")
            .with_commit_sha("zeta/repo", SHA)
            .failing("alpha/repo")
            .with_commit_sha("mid/repo", SHA);
        let records = vec![
            github_record(
                "zeta-plugin",
                "zeta/repo",
                None,
                Some(GithubRef::branch("HEAD")),
                Some(SHA),
            ),
            // This one fails resolution and must be skipped entirely.
            github_record(
                "alpha-plugin",
                "alpha/repo",
                None,
                Some(GithubRef::branch("HEAD")),
                Some(SHA),
            ),
            github_record(
                "mid-plugin",
                "mid/repo",
                Some("main"),
                Some(GithubRef::branch("main")),
                Some(SHA),
            ),
        ];
        let (_dir, manager) = manager_with_records(Arc::new(resolver), records);

        let statuses = manager.check_updates().await;
        let ids: Vec<&str> = statuses.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["mid-plugin", "zeta-plugin"]);
    }

    /// Helper: create a temp plugins dir and a source plugin directory with
    /// a valid manifest.
    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
        let source_dir = tempfile::tempdir().expect("failed to create source dir");

        let manifest = serde_json::json!({
            "name": "test-plugin",
            "version": "1.0.0",
            "description": "A test plugin",
            "skills": ["./skills"],
            "agents": ["./agents"],
            "commands": ["./commands"],
            "systemPrompt": "You are a test assistant.",
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Create the referenced directories.
        std::fs::create_dir_all(source_dir.path().join("skills")).unwrap();
        std::fs::create_dir_all(source_dir.path().join("agents")).unwrap();
        std::fs::create_dir_all(source_dir.path().join("commands")).unwrap();
        std::fs::write(
            source_dir.path().join("commands").join("hello.md"),
            "# Hello command",
        )
        .unwrap();

        (plugins_dir, source_dir)
    }

    #[tokio::test]
    async fn should_install_plugin_when_source_is_local() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        let id = manager
            .install(&source)
            .await
            .expect("install should succeed");
        assert_eq!(id, "test-plugin");

        // The plugin lives under the TS managed layout
        // `<plugins>/managed/<id>` (A2).
        let managed_root = plugins_dir.path().join("managed").join("test-plugin");
        assert!(managed_root.is_dir());
        assert!(managed_root.join("mirri.plugin.json").is_file());
    }

    /// Given an installed plugin, when the same id is installed again from
    /// an updated source, then the install **upgrades** it: no
    /// AlreadyInstalled error, the managed copy is swapped, and the old
    /// directory is gone (item A2 — the TS `install`).
    #[tokio::test]
    async fn should_upgrade_plugin_when_installed_again() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("first install");

        // Brand the source with a new marker file and reinstall.
        std::fs::write(source_dir.path().join("marker-v2"), "v2").unwrap();
        let id = manager
            .install(&source)
            .await
            .expect("reinstall should upgrade, not fail");
        assert_eq!(id, "test-plugin");

        let managed_root = plugins_dir.path().join("managed").join("test-plugin");
        assert!(managed_root.join("marker-v2").is_file());
        // The previous managed copy was swapped out; no stray staging or
        // previous dirs remain under managed/.
        let managed_dir = plugins_dir.path().join("managed");
        let leftover: Vec<String> = std::fs::read_dir(&managed_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != "test-plugin")
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftover.is_empty(),
            "staging/previous dirs must be cleaned up: {leftover:?}"
        );
    }

    #[tokio::test]
    async fn should_load_all_plugins_when_manager_is_restarted() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        // Create a fresh manager (simulating a restart) and load.
        let manager2 = PluginManager::new(plugins_dir.path().to_path_buf());
        manager2.load_all().expect("load_all should succeed");

        let list = manager2.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "test-plugin");
        assert!(list[0].enabled);
    }

    #[tokio::test]
    async fn should_toggle_enabled_state_when_enable_and_disable_are_called() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        manager.disable("test-plugin").expect("disable");
        let list = manager.list();
        assert!(!list[0].enabled);

        manager.enable("test-plugin").expect("enable");
        let list = manager.list();
        assert!(list[0].enabled);
    }

    #[tokio::test]
    async fn should_remove_plugin_record_but_keep_directory_when_plugin_is_removed() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        manager.remove("test-plugin").expect("remove");

        // The managed directory stays on disk (the TS remove, item C2)…
        assert!(plugins_dir.path().join("managed").join("test-plugin").is_dir());

        // …while the record is gone.
        let list = manager.list();
        assert!(list.is_empty());
        assert!(manager.store.find("test-plugin").expect("find").is_none());
    }

    #[tokio::test]
    async fn should_collect_mcp_servers_when_plugin_is_enabled() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        // Write a manifest with MCP servers.
        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": {
                    "command": "node",
                    "args": ["server.js"]
                }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let servers = manager.enabled_mcp_servers();
        assert!(servers.contains_key("plugin-mcp-plugin:my-server"));
    }

    #[tokio::test]
    async fn should_exclude_server_when_its_per_server_state_disables_it() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": { "command": "node", "args": ["server.js"] }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");
        manager
            .set_mcp_server_enabled("mcp-plugin", "my-server", false)
            .expect("set_mcp_server_enabled should succeed");

        assert!(manager.enabled_mcp_servers().is_empty());
        // The disabled server still shows in the entries view so it can be
        // re-enabled.
        let entries = manager.mcp_server_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].server_name, "my-server");
        assert!(entries[0].state.as_ref().is_some_and(|s| !s.enabled));
    }

    #[tokio::test]
    async fn should_apply_tool_filters_when_collecting_enabled_servers() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": { "command": "node", "args": ["server.js"] }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");
        manager
            .set_mcp_server_tool_filter(
                "mcp-plugin",
                "my-server",
                Some(vec!["tool-a".to_string()]),
                None,
            )
            .expect("set_mcp_server_tool_filter should succeed");

        let servers = manager.enabled_mcp_servers();
        let config = &servers["plugin-mcp-plugin:my-server"];
        assert_eq!(config["enabled"], serde_json::json!(true));
        assert_eq!(config["enabledTools"], serde_json::json!(["tool-a"]));
    }

    /// Given an installed plugin contributing a stdio MCP server, when the
    /// enabled server set is collected, then the server carries the runtime
    /// decoration (item A7 — the TS `withPluginMcpRuntime`): `cwd` defaults
    /// to the plugin root, `env` merges `MIRRICODE_HOME` (the manager's home
    /// dir) and `MIRRI_PLUGIN_ROOT` over the manifest-declared env, and the
    /// command stays untouched (the TS `node`-rewrite branches are N/A for
    /// the native rust host, decision D-A7).
    #[tokio::test]
    async fn should_decorate_enabled_stdio_mcp_servers_with_runtime_cwd_and_env_when_collected() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": {
                    "command": "node",
                    "args": ["server.js"],
                    "env": { "MY_VAR": "mine", "MIRRICODE_HOME": "manifest-home" }
                }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let servers = manager.enabled_mcp_servers();
        let config = &servers["plugin-mcp-plugin:my-server"];
        let expected_root = std::fs::canonicalize(plugins_dir.path().join("managed").join("mcp-plugin"))
            .expect("canonicalize the managed root")
            .display()
            .to_string();
        let expected_home = plugins_dir
            .path()
            .parent()
            .expect("plugins dir under a home")
            .display()
            .to_string();
        assert_eq!(
            config["command"],
            serde_json::json!("node"),
            "the native rust host performs no node command rewrite"
        );
        assert_eq!(
            config["cwd"],
            serde_json::json!(expected_root),
            "cwd defaults to the plugin root"
        );
        let env = config["env"].as_object().expect("runtime env injected");
        assert_eq!(
            env["MIRRICODE_HOME"],
            serde_json::json!(expected_home),
            "MIRRICODE_HOME is the manager's home dir"
        );
        assert_eq!(
            env["MIRRI_PLUGIN_ROOT"],
            serde_json::json!(expected_root),
            "MIRRI_PLUGIN_ROOT is the plugin root"
        );
        assert_eq!(env["MY_VAR"], serde_json::json!("mine"), "manifest env is preserved");
        assert_ne!(
            env["MIRRICODE_HOME"],
            serde_json::json!("manifest-home"),
            "the runtime facts win over a manifest-declared same-name env key"
        );
    }

    /// Given an installed plugin contributing an http MCP server, when the
    /// enabled server set is collected, then the server passes through
    /// undecorated (the TS `withPluginMcpRuntime` http/sse early return,
    /// item A7).
    #[tokio::test]
    async fn should_leave_remote_plugin_mcp_servers_undecorated_when_collected() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "remote": { "transport": "http", "url": "https://example.test/mcp" }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let servers = manager.enabled_mcp_servers();
        let config = &servers["plugin-mcp-plugin:remote"];
        assert_eq!(config["url"], serde_json::json!("https://example.test/mcp"));
        assert!(
            config.get("cwd").is_none(),
            "no cwd decoration for remote servers"
        );
        assert!(
            config.get("env").is_none(),
            "no env decoration for remote servers"
        );
    }

    /// Given an installed plugin contributing a stdio MCP server, when the
    /// config-management entries are collected, then they carry the raw
    /// manifest config — the item A7 decoration stays on the runtime set
    /// (`enabled_mcp_servers`) and never leaks into the management view
    /// (the TS `mcpServerEntries` visibility, item A7).
    #[tokio::test]
    async fn should_keep_management_entries_undecorated_when_collected() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": { "command": "node", "args": ["server.js"] }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let entries = manager.mcp_server_entries();
        assert_eq!(entries.len(), 1);
        let config = &entries[0].config;
        assert_eq!(config["command"], serde_json::json!("node"));
        assert!(
            config.get("cwd").is_none(),
            "the management view carries no cwd decoration"
        );
        assert!(
            config.get("env").is_none(),
            "the management view carries no env decoration"
        );
    }

    #[tokio::test]
    async fn should_reject_per_server_writes_when_the_server_is_not_in_the_manifest() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let manifest = serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "my-server": { "command": "node", "args": ["server.js"] }
            }
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let result = manager.set_mcp_server_enabled("mcp-plugin", "ghost", true);
        assert!(matches!(result, Err(PluginError::McpServerNotFound(_))));

        let result = manager.set_mcp_server_tool_filter("mcp-plugin", "ghost", None, Some(vec![]));
        assert!(matches!(result, Err(PluginError::McpServerNotFound(_))));

        let result = manager.set_mcp_server_enabled("ghost-plugin", "my-server", true);
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_exclude_plugin_from_collections_when_plugin_is_disabled() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");
        manager.disable("test-plugin").expect("disable");

        assert!(manager.enabled_mcp_servers().is_empty());
        assert!(manager.enabled_hooks().is_empty());
        assert!(manager.enabled_system_prompts().is_empty());
        assert!(manager.plugin_skill_roots().is_empty());
        assert!(manager.plugin_agent_roots().is_empty());
    }

    #[tokio::test]
    async fn should_collect_system_prompts_when_plugin_is_enabled() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let prompts = manager.enabled_system_prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].plugin_id, "test-plugin");
        assert_eq!(prompts[0].content, "You are a test assistant.");
    }

    #[tokio::test]
    async fn should_read_system_prompt_from_file_when_system_prompt_path_is_set() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        // Overwrite manifest to use systemPromptPath instead (the TS
        // `resolvePluginPathField` requires the "./" prefix, B6/B2).
        let manifest = serde_json::json!({
            "name": "test-plugin",
            "systemPromptPath": "./prompts/system.txt"
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
        std::fs::write(
            source_dir.path().join("prompts").join("system.txt"),
            "File-based system prompt.",
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let prompts = manager.enabled_system_prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].plugin_id, "test-plugin");
        assert_eq!(prompts[0].content, "File-based system prompt.");
    }

    #[tokio::test]
    async fn should_collect_skill_and_agent_roots_when_plugin_is_enabled() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        // Skill roots carry the TS `SkillRoot` attribution (`source: 'extra'`,
        // `plugin: { id, instructions }`); agent roots the TS
        // `PluginAgentRoot` (`source: 'plugin'`).
        let skills = manager.plugin_skill_roots();
        assert_eq!(skills.len(), 1);
        assert!(skills[0].path.ends_with("skills"));
        assert_eq!(skills[0].source, "extra");
        assert_eq!(skills[0].plugin.id, "test-plugin");
        assert_eq!(skills[0].plugin.instructions, None);

        let agents = manager.plugin_agent_roots();
        assert_eq!(agents.len(), 1);
        assert!(agents[0].path.ends_with("agents"));
        assert_eq!(agents[0].source, "plugin");
    }

    #[tokio::test]
    async fn should_load_md_commands_when_enabled_commands_are_collected() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        // Give the md command a frontmatter so the parse is observable.
        std::fs::write(
            source_dir.path().join("commands").join("hello.md"),
            "---\nname: greet\ndescription: Greets the user\n---\n\nHello, world!",
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let commands = manager.enabled_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].plugin_id, "test-plugin");
        assert_eq!(commands[0].name, "greet");
        assert_eq!(commands[0].description, "Greets the user");
        assert_eq!(commands[0].body, "Hello, world!");
        assert!(commands[0].path.ends_with("commands/hello.md"));
    }

    #[tokio::test]
    async fn should_activate_md_command_when_args_are_expanded() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        std::fs::write(
            source_dir.path().join("commands").join("hello.md"),
            "Summarize $ARGUMENTS.",
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        // Name falls back to the entry name ("hello"); `$ARGUMENTS` is
        // replaced (the TS `expandCommandArguments`).
        let output = manager
            .activate_command("hello", "src/main.rs")
            .expect("md command should activate");
        assert_eq!(output, "Summarize src/main.rs.");

        // A template without the placeholder appends `ARGUMENTS:` instead.
        std::fs::write(
            plugins_dir
                .path()
                .join("managed")
                .join("test-plugin")
                .join("commands")
                .join("hello.md"),
            "Just summarize.",
        )
        .unwrap();
        manager.reload("test-plugin").expect("reload");
        let output = manager
            .activate_command("hello", "src/main.rs")
            .expect("md command should activate");
        assert_eq!(output, "Just summarize.\n\nARGUMENTS: src/main.rs");
    }

    #[tokio::test]
    async fn should_activate_inline_spec_when_name_matches_an_inline_command() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        // Replace the fixture manifest with one carrying an inline spec.
        let manifest = serde_json::json!({
            "name": "test-plugin",
            "commands": [
                {
                    "name": "explain",
                    "description": "Explain code",
                    "template": "Analyze {{ file_path }}"
                }
            ]
        });
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let output = manager
            .activate_command("explain", "")
            .expect("inline command should activate");
        assert_eq!(output, "Analyze ");
    }

    #[tokio::test]
    async fn should_answer_command_not_found_when_activating_unknown_command() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        let result = manager.activate_command("ghost", "");
        assert!(matches!(result, Err(PluginError::CommandNotFound(_))));
    }

    #[tokio::test]
    async fn should_reread_manifest_when_plugin_is_reloaded() {
        let (plugins_dir, source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let source = format!("local:{}", source_dir.path().display());
        manager.install(&source).await.expect("install");

        // Modify the manifest on disk.
        let manifest = serde_json::json!({
            "name": "test-plugin",
            "description": "Updated description"
        });
        std::fs::write(
            plugins_dir
                .path()
                .join("managed")
                .join("test-plugin")
                .join("mirri.plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        manager.reload("test-plugin").expect("reload");

        let list = manager.list();
        assert_eq!(list[0].description.as_deref(), Some("Updated description"));
    }

    #[tokio::test]
    async fn should_return_not_found_when_removing_nonexistent_plugin() {
        let (plugins_dir, _source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let result = manager.remove("ghost");
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_return_not_found_when_enabling_nonexistent_plugin() {
        let (plugins_dir, _source_dir) = setup();
        let manager = PluginManager::new(plugins_dir.path().to_path_buf());

        let result = manager.enable("ghost");
        assert!(matches!(result, Err(PluginError::NotFound(_))));
    }
}
