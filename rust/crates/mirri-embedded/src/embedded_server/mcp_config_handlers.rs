//! MCP config-domain dispatch handlers — the P1 configuration read-write
//! surface (`mcp.config.list` / `mcp.config.set` / `mcp.config.remove` /
//! `mcp.config.setToolFilter`, `docs/specs/mcp-architecture.md` §4.1).
//!
//! Thin adapters over `mirri_mcp`'s raw config surface: reads present the
//! stored text (`${VAR}` references and the stored field forms untouched —
//! expansion only happens at connect/test time, adjudication A5; the one
//! presentation transform infers the flat `transport` tag a stored entry
//! omits, exactly the TS `config-schema.ts` preprocess), writes are atomic
//! (temp file + rename), and the three-layer merge (user → project root →
//! project local) replaces per-server entries exactly as
//! `load_mcp_config` does.
//!
//! Scope rules (adjudications A1/A2):
//! - `user` reads/writes the global layer (`<base>/.mirri-code/mcp.json`).
//! - `workspace` writes the project-local layer
//!   (`<cwd>/.mirri-code/mcp.json`) only — the project root's `.mcp.json`
//!   is a read-only git-shared asset (shown in views, never machine-written).
//! - an absent scope lists the merged three-layer view, `user` the user
//!   file as stored, `workspace` the project-root + project layers merged.
//! - plugin-bundled servers (`plugin-<id>:<name>` runtime names) are
//!   read-only here: `set`/`remove` reject them (A1), and their tool
//!   filters write the plugin user state and reconnect the live
//!   connection (the plugin-status batch 3c surface).
//!
//! The user layer resolves through the base the bootstrap's MCP manager
//! load resolved (`EngineParts::mcp_home_base` — the explicit base, else
//! injected home, then `$HOME`, then the resolved Mirri home), so the
//! config surface reads and writes the very file the running manager
//! loaded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_transport::adapter::EngineError;
use serde_json::{Value, json};

use mirri_mcp::McpServerConfig;
use mirri_mcp::config::{
    McpToolFilterPatch, global_config_path, layer_revision, local_config_path,
    project_root_config_path, read_raw_config_file, remove_server_entry, set_server_tool_filter,
    upsert_filter_override_entry, upsert_server_entry,
};

use super::responses::{McpConfigTestResult, ack, to_result};
use super::{
    EmbeddedServer, invalid_params, normalize_params, not_found_domain, plugin_error_to_engine,
};

/// How long a reconcile's connection teardown waits for in-flight tool
/// calls before forcing the disconnect (spec §4.3 / R2: let calls finish;
/// after the window, proceed and the in-flight call fails with an error
/// naming the server).
const RECONCILE_INFLIGHT_DRAIN: std::time::Duration = std::time::Duration::from_secs(10);

impl EmbeddedServer {
    /// `mcp.config.list(scope?)` — the layered config view: no scope = the
    /// merged three-layer view (each entry annotated with its source layer,
    /// later layers replacing same-name entries entirely); `user` = the user
    /// file as stored; `workspace` = the project-root + project layers
    /// merged. Each entry carries the stored config verbatim (A5) plus the
    /// derived effective state — for the merged views the A7 family merge
    /// over the view's layers (the connect set's truth: inherited and
    /// overridden filter families show), for the single-layer user view the
    /// stored entry's own derivation.
    pub(super) async fn dispatch_mcp_config_list(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params = normalize_params(params);
        let scope = optional_scope_arg(&params)?;
        let (global_path, root_path, local_path) =
            self.mcp_config_layer_paths(&self.default_layer_root());

        // The A7 family merge over the view's layers — the effective state
        // the pools connect with (the same single source the reconcile
        // desired set derives from), so an inherited or overridden filter
        // family shows in the entry's effective view instead of the top
        // layer's stored fields alone. The read-modify-write flows (the
        // web's per-tool toggle reads `effective` and writes the complete
        // new filter state) round-trip the merged truth this way instead of
        // silently dropping a shadowed layer's filter. The single-layer
        // `user` view keeps the stored derivation — no cross-layer merge
        // applies to it.
        let merged_effective: HashMap<String, mirri_mcp::McpServerConfig> = match scope {
            Some(McpConfigScope::User) => HashMap::new(),
            view => {
                let read = |path: Option<&std::path::Path>,
                            stdio_cwd_default: mirri_mcp::config::StdioCwdDefault|
                 -> Option<(
                    HashMap<String, mirri_mcp::McpServerConfig>,
                    HashMap<String, Value>,
                )> {
                    let path = path?;
                    if path.is_file() {
                        mirri_mcp::config::load_config_file_with_raw_for_layer(
                            path,
                            stdio_cwd_default,
                        )
                        .ok()
                    } else {
                        Some((HashMap::new(), HashMap::new()))
                    }
                };
                let user = if view.is_none() {
                    read(
                        global_path.as_deref(),
                        mirri_mcp::config::StdioCwdDefault::Inherit,
                    )
                } else {
                    Some((HashMap::new(), HashMap::new()))
                };
                match (
                    user,
                    read(
                        Some(&root_path),
                        mirri_mcp::config::StdioCwdDefault::ConfigDir,
                    ),
                    read(
                        local_path.as_deref(),
                        mirri_mcp::config::StdioCwdDefault::ConfigDir,
                    ),
                ) {
                    (Some((user, user_raw)), Some((root, root_raw)), Some((local, local_raw))) => {
                        let layered = mirri_mcp::config::LayeredMcpConfig {
                            user,
                            user_raw,
                            project_root: root,
                            project_root_raw: root_raw,
                            project: local,
                            project_raw: local_raw,
                        };
                        if view.is_none() {
                            mirri_mcp::config::merge_layers(&layered)
                        } else {
                            layered.project_merged()
                        }
                    }
                    // A layer too broken for the typed load: the raw view
                    // still lists its entries — keep the per-entry
                    // derivation rather than failing the whole list.
                    _ => HashMap::new(),
                }
            }
        };

        // Layer order: user → project root → project local; a later layer
        // replaces an earlier same-name entry at its first-appearance
        // position (the `load_mcp_config` per-server merge).
        let layers: Vec<(Option<PathBuf>, &'static str)> = match scope {
            Some(McpConfigScope::User) => vec![(global_path, "user")],
            Some(McpConfigScope::Workspace) => {
                vec![(Some(root_path), "projectRoot"), (local_path, "project")]
            }
            None => vec![
                (global_path, "user"),
                (Some(root_path), "projectRoot"),
                (local_path, "project"),
            ],
        };

        let mut entries: Vec<Value> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();

        for (path, source) in layers {
            let Some(path) = path else { continue };
            let root = read_raw_config_file(&path).map_err(mcp_config_error)?;
            // F-3: the revision of the file defining this layer's entries —
            // the optimistic-lock token write calls side-carry. Computed
            // once per layer: the hash covers the whole file, so per-entry
            // recomputation was pure waste.
            let revision = layer_revision(&path);
            let Some(servers) = root.get("mcpServers").and_then(Value::as_object) else {
                continue;
            };
            for (name, raw) in servers {
                let entry = json!({
                    "name": name,
                    "source": source,
                    "revision": &revision,
                    "config": present_config_entry(raw),
                    "effective": match merged_effective.get(name) {
                        Some(merged) => effective_view_from_config(merged),
                        None => effective_config_view(raw),
                    },
                });
                match index.get(name) {
                    Some(&slot) => entries[slot] = entry,
                    None => {
                        index.insert(name.clone(), entries.len());
                        entries.push(entry);
                    }
                }
            }
        }

        // The scope-less merged view appends the plugin-contributed servers
        // under their runtime-name slots (the `plugin-` prefix is reserved,
        // so no config-file entry can collide, A1). The entry carries the
        // manifest name plus the per-server user state — per-server-disabled
        // servers still list so the view can re-enable them.
        if scope.is_none()
            && let Some(plugin_manager) = &self.plugin_manager
        {
            for plugin_entry in plugin_manager.mcp_server_entries() {
                let runtime_name = mirri_plugin_host::plugin_mcp_runtime_name(
                    &plugin_entry.plugin_id,
                    &plugin_entry.server_name,
                );
                let mut effective = effective_config_view(&plugin_entry.config);
                if let Some(state) = &plugin_entry.state {
                    // The record is the complete user state: a recorded
                    // field overrides the manifest-derived view, a
                    // recorded-absent field clears it (the
                    // `apply_mcp_server_state` parity).
                    effective["enabled"] = json!(state.enabled);
                    match &state.enabled_tools {
                        Some(tools) => effective["enabledTools"] = json!(tools),
                        None => {
                            if let Some(obj) = effective.as_object_mut() {
                                obj.remove("enabledTools");
                            }
                        }
                    }
                    match &state.disabled_tools {
                        Some(tools) => effective["disabledTools"] = json!(tools),
                        None => {
                            if let Some(obj) = effective.as_object_mut() {
                                obj.remove("disabledTools");
                            }
                        }
                    }
                }
                entries.push(json!({
                    "name": plugin_entry.server_name,
                    "runtimeName": runtime_name,
                    "source": "plugin",
                    // No config-layer file backs a plugin-bundled entry
                    // (its connection definition is plugin-owned, A1),
                    // so it has no writable-layer revision to lock on.
                    "revision": "absent",
                    "config": present_config_entry(&plugin_entry.config),
                    "effective": effective,
                }));
            }
        }
        Ok(Value::Array(entries))
    }

    /// `mcp.config.set(scope, name, config, expectedRevision?)` — validate
    /// and atomically upsert one server entry: `user` targets the global
    /// file, `workspace` the project layer only (A2). The stored value is
    /// the caller's raw config, `${VAR}` references included. An
    /// `expectedRevision` different from the target layer file's current
    /// revision answers the `mcp.config.conflict` domain error before any
    /// write (F-3); without one the write is last-writer-wins (N5).
    pub(super) async fn dispatch_mcp_config_set(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (scope, name, config, expected_revision) = set_args(&params)?;
        reject_plugin_server(&name, "mcp.config.set")?;
        let path = self.writable_scope_path(scope)?;
        check_config_revision(&path, expected_revision.as_deref())?;
        upsert_server_entry(&path, &name, config).map_err(mcp_config_error)?;
        // The write landed: reconcile the running pool behind the response
        // (spec §4.3) — the ack never waits on a connection (lazy-connect
        // semantics preserved).
        self.spawn_mcp_pool_reconcile();
        ack()
    }

    /// `mcp.config.remove(scope, name, expectedRevision?)` — remove one
    /// server entry (idempotent: removing an absent name is a success). An
    /// `expectedRevision` mismatch answers `mcp.config.conflict` before any
    /// write (F-3); without one the remove is last-writer-wins (N5).
    pub(super) async fn dispatch_mcp_config_remove(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (scope, name, expected_revision) = scope_name_args(&params, "mcp.config.remove")?;
        reject_plugin_server(&name, "mcp.config.remove")?;
        let path = self.writable_scope_path(scope)?;
        check_config_revision(&path, expected_revision.as_deref())?;
        remove_server_entry(&path, &name).map_err(mcp_config_error)?;
        self.spawn_mcp_pool_reconcile();
        ack()
    }

    /// `mcp.config.test` — fresh-connect test of one config (spec §4.1, the
    /// management UI's "test connection" backend): either the direct
    /// `config` argument (a not-yet-saved entry, `${VAR}` references
    /// expanded here per A5 — expansion happens at test time only) or
    /// `scope` + `name` (a persisted entry resolved from the addressed
    /// layer with the loader's semantics). The connection runs on a
    /// scratch manager under a `__test_<uuid>` runtime name — no status
    /// listeners, no leak into the running pools — and is removed
    /// afterwards; the config's `startupTimeoutMs` bounds the handshake.
    pub(super) async fn dispatch_mcp_config_test(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params = normalize_params(params);
        let mut config = self.test_config_arg(&params)?;

        // A connectivity test ignores the stored enablement toggle: the
        // manager short-circuits disabled entries to `Disabled` without
        // connecting, which would report a misleading result.
        config.enabled = Some(true);

        // The scratch manager shares the OAuth token store, so a test of an
        // OAuth-configured remote server exercises the real path (a cached
        // token attaches; without one the server's 401 surfaces honestly).
        let scratch = match self.mcp_oauth_token_dir() {
            Some(dir) => mirri_mcp::McpConnectionManager::with_oauth_token_dir(dir),
            None => mirri_mcp::McpConnectionManager::new(),
        };
        let temp_name = format!("__test_{}", uuid::Uuid::new_v4());
        let outcome = match scratch.connect_one(&temp_name, config).await {
            Ok(()) => {
                let tools = scratch.list_all_tools().await;
                let count = tools.iter().filter(|(name, _)| *name == temp_name).count();
                McpConfigTestResult {
                    status: "ok".to_string(),
                    tool_count: count,
                    error: None,
                }
            }
            Err(error) => McpConfigTestResult {
                status: "error".to_string(),
                tool_count: 0,
                error: Some(error.to_string()),
            },
        };
        // Remove (not just drop) so the scratch stdio child is reaped
        // deterministically.
        if let Err(error) = scratch.remove(&temp_name).await {
            tracing::warn!(server = %temp_name, %error, "test connection cleanup failed");
        }
        to_result(outcome)
    }

    /// Resolve the `mcp.config.test` target config: the object spelling
    /// (`{config}` for a direct test, or `{scope, name}` for a persisted
    /// entry) or the positional tuple spellings (`[config]` /
    /// `[scope, name]`). A direct config is expanded here; a persisted
    /// entry loads through the layer-file loader (which expands and
    /// anchors relative stdio `cwd`, exactly as a real connect would).
    fn test_config_arg(&self, params: &Value) -> Result<McpServerConfig, EngineError> {
        let target = parse_test_args(params, "mcp.config.test")?;

        if let Some(raw) = target.direct {
            // A5: expansion happens on the test path — value-level, the
            // same expansion the connect path applies. The raw config
            // deserializes first, then string *values* expand; object keys
            // holding `${...}` are never touched (TS value-level semantics,
            // spec §4.6 environment row) — no serialized-JSON text splicing.
            let config: McpServerConfig = serde_json::from_value(raw)
                .map_err(|e| invalid_params(format!("the test config is invalid: {e}")))?;
            let (expanded, _undefined) = mirri_mcp::config::expand_server_config_values(&config);
            return Ok(expanded);
        }

        // A persisted entry: `user` reads the user layer; `workspace`
        // resolves the project layers merged (project local first, then
        // the read-only project root).
        let scope = target
            .scope
            .ok_or_else(|| invalid_params("scope is required for mcp.config.test"))?;
        let name = target
            .name
            .ok_or_else(|| invalid_params("name is required for mcp.config.test"))?;
        self.load_named_server_config(scope, &name)
    }

    /// `mcp.config.preview` — the resolved-value preview (F-4): the
    /// adjudication A5 **sole sanctioned outlet** for resolved values. An
    /// explicit request answers the target config with every `${VAR}` /
    /// `${env:VAR}` string *value* expanded against the process
    /// environment, plus the names of the variables that stayed undefined
    /// (they expand to the empty string — the compatible semantics). The
    /// target locates exactly like `mcp.config.test`: the direct `config`
    /// argument (a not-yet-saved entry) or `scope` + `name` (a persisted
    /// entry read from the addressed layer as stored).
    ///
    /// The A5 discipline around the one exception: nothing is cached, no
    /// event is emitted, nothing is logged, and no file is written —
    /// outside this response body resolved values appear on no surface
    /// (the stored text keeps its references verbatim).
    pub(super) async fn dispatch_preview_mcp_config(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params = normalize_params(params);
        let target = parse_test_args(&params, "mcp.config.preview")?;
        let raw: Value = match target.direct {
            Some(config) => config,
            None => {
                let scope = target
                    .scope
                    .ok_or_else(|| invalid_params("scope is required for mcp.config.preview"))?;
                let name = target
                    .name
                    .ok_or_else(|| invalid_params("name is required for mcp.config.preview"))?;
                self.load_named_server_config_raw(scope, &name)?
            }
        };

        // The same validation the connect/test path applies: an entry that
        // cannot parse as a server config answers invalid params here,
        // never a half-preview of a broken entry.
        serde_json::from_value::<McpServerConfig>(raw.clone())
            .map_err(|e| invalid_params(format!("the preview config is invalid: {e}")))?;

        // Value-level expansion over the raw stored form (the M2 seam's
        // raw-JSON entry): the wire shape and passthrough keys survive,
        // only string values resolve — then the same presentation the
        // list view applies (the flat `transport` tag inferred when the
        // stored entry omits it).
        let (expanded, undefined_vars) =
            mirri_mcp::config::expand_server_config_json_with_env(&raw, |name| {
                std::env::var(name).ok()
            });
        Ok(json!({
            "config": present_config_entry(&expanded),
            "undefined_vars": undefined_vars,
        }))
    }

    /// A persisted entry by scope + name with the connect-set single
    /// source (the same derivation the pools and the reconcile desired set
    /// use): `workspace` resolves the A7 project merge, `user` the
    /// cross-pool `user_merged` view — a filter-only override entry never
    /// hands its inert placeholder transport to a fresh-connect test, the
    /// test sees exactly the config a connect would see. Raw `${VAR}`
    /// references stay verbatim (A5 — expansion happens at connect/test
    /// time); relative stdio `cwd` paths are anchored against the defining
    /// file's directory, exactly as a real connect would resolve them.
    fn load_named_server_config(
        &self,
        scope: McpConfigScope,
        name: &str,
    ) -> Result<McpServerConfig, EngineError> {
        let workspace_root = self.manager.workspace().cwd().to_path_buf();
        let home_base = self
            .home_dir
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .or_else(|| mirri_core::config::mirri_home_dir().ok());
        let desired = desired_configs_for_scope(scope, home_base.as_deref(), &workspace_root)
            .ok_or_else(|| named_server_not_found(scope, name))?;
        desired
            .get(name)
            .cloned()
            .ok_or_else(|| named_server_not_found(scope, name))
    }

    /// A persisted entry's **raw stored JSON** by scope + name — the same
    /// layer semantics as [`Self::load_named_server_config`], but the
    /// stored text: `${VAR}` references, passthrough keys and the stored
    /// field forms untouched. The preview reads through this so the
    /// resolved-value answer keeps the stored wire shape instead of the
    /// typed round-trip's normalized form. A filter-only override entry is
    /// skipped — it carries no connection family, so the config a connect
    /// would see is the next layer's complete entry.
    fn load_named_server_config_raw(
        &self,
        scope: McpConfigScope,
        name: &str,
    ) -> Result<Value, EngineError> {
        for path in self.scope_layer_paths(scope) {
            if let Ok(root) = read_raw_config_file(&path)
                && let Some(raw) = root.get("mcpServers").and_then(|servers| servers.get(name))
                && raw.is_object()
                && mirri_mcp::config::entry_has_transport(raw)
            {
                return Ok(raw.clone());
            }
        }
        Err(named_server_not_found(scope, name))
    }

    /// The config-layer files a scope reads, in priority order — the
    /// shared location semantics of the test and preview paths (`user` →
    /// the user layer; `workspace` → the project-local layer, then the
    /// read-only project root).
    fn scope_layer_paths(&self, scope: McpConfigScope) -> Vec<PathBuf> {
        let (global_path, root_path, local_path) =
            self.mcp_config_layer_paths(&self.default_layer_root());
        match scope {
            McpConfigScope::User => vec![global_path],
            McpConfigScope::Workspace => vec![local_path, Some(root_path)],
        }
        .into_iter()
        .flatten()
        .collect()
    }

    /// `mcp.config.setToolFilter(name, filter, scope?, expectedRevision?)` —
    /// the unified write entry for tool-level switches: present fields
    /// replace the stored ones, absent fields clear them (the filter object
    /// is the entry's complete new filter state). Target-layer semantics per
    /// adjudication A2/ADR-006 (ADR-012 2026-10-17 revision):
    ///
    /// - an absent `scope` patches the highest-priority layer defining the
    ///   name among the **writable** ones (project local → user); a name
    ///   defined only in the project root gets a **filter-only override
    ///   entry** in the project local layer (the shared root file itself is
    ///   never machine-written, A2). Note the revision lock for this path
    ///   targets the **project-local** file (the file the patch rewrites) —
    ///   a client locking with the revision `mcp.config.list` reports for
    ///   the root-defined entry (the root file's) will conflict;
    /// - `scope='workspace'` patches the project local layer, or, when the
    ///   name is not defined there, creates a **filter-only override entry**
    ///   (only `enabledTools`/`disabledTools`, no transport — the A7
    ///   connection family stays with the shadowed layer) when the name is
    ///   defined in the project root or the user layer;
    /// - `scope='user'` patches the user layer only;
    /// - the overlap shape (the workspace root is the user layer directory
    ///   — the desktop fallback when `mirri web` spawns with cwd=`/`) has
    ///   no project-local layer at all: any write that would target it
    ///   answers the `mcp.config.no_project_layer` domain error;
    /// - a name no layer defines answers a domain-coded not-found.
    ///
    /// An `expectedRevision` different from the write target layer file's
    /// current revision answers `mcp.config.conflict` before any write
    /// (F-3); without one the patch is last-writer-wins (N5).
    pub(super) async fn dispatch_mcp_config_set_tool_filter(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params = normalize_params(params);
        let (name, patch, scope, expected_revision) = set_tool_filter_args(&params)?;
        if name.starts_with("plugin-") {
            // Plugin-bundled server: the patch writes the plugin user state
            // (A1 — the connection definition stays manifest-owned), then
            // the live connection follows the new effective state.
            //
            // F-3: a plugin server has no config-layer file to lock — its
            // list entry carries `revision: "absent"` — so the only lock
            // token that can legitimately match is the literal "absent";
            // any other `expectedRevision` names a layer that cannot exist
            // and is a client-side error, never a silently ignored lock.
            match expected_revision.as_deref() {
                None | Some("absent") => {}
                Some(_) => {
                    return Err(invalid_params(format!(
                        "'{name}' is a plugin-bundled MCP server with no config-layer file to lock; expectedRevision must be \"absent\" or omitted (mcp.config.setToolFilter, adjudication A1)"
                    )));
                }
            }
            let Some(plugin_manager) = &self.plugin_manager else {
                return Err(not_found_domain(
                    "mcp.server_not_found",
                    format!("MCP server '{name}' is not defined by any installed plugin"),
                ));
            };
            let entry = plugin_manager
                .mcp_server_entries()
                .into_iter()
                .find(|entry| {
                    mirri_plugin_host::plugin_mcp_runtime_name(&entry.plugin_id, &entry.server_name)
                        == name
                })
                .ok_or_else(|| {
                    not_found_domain(
                        "mcp.server_not_found",
                        format!("MCP server '{name}' is not defined by any installed plugin"),
                    )
                })?;
            plugin_manager
                .set_mcp_server_tool_filter(
                    &entry.plugin_id,
                    &entry.server_name,
                    patch.enabled_tools,
                    patch.disabled_tools,
                )
                .map_err(plugin_error_to_engine)?;
            self.sync_plugin_mcp_connection(&name).await;
            return ack();
        }

        let (global_path, root_path, local_path) =
            self.mcp_config_layer_paths(&self.default_layer_root());
        match scope {
            Some(McpConfigScope::User) => {
                let Some(global) = global_path else {
                    return Err(EngineError::Internal(
                        "no home directory is available to resolve the user MCP config layer"
                            .to_string(),
                    ));
                };
                if layer_defines(&global, &name)? {
                    // F-3: the optimistic lock is checked against the
                    // owning layer's file — the one the patch rewrites.
                    check_config_revision(&global, expected_revision.as_deref())?;
                    set_server_tool_filter(&global, &name, &patch).map_err(mcp_config_error)?;
                    // A tool-filter change reconciles without a reconnect
                    // (the diff's RefreshToolFilter op).
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                Err(not_found_domain(
                    "mcp.server_not_found",
                    format!("MCP server '{name}' is not defined in the user configuration layer"),
                ))
            }
            Some(McpConfigScope::Workspace) => {
                let Some(local) = local_path.as_deref() else {
                    // D-3: the overlap shape (the workspace root is the
                    // user layer directory) has no project-local layer — a
                    // workspace-scoped write answers the domain error
                    // instead of silently writing the user file.
                    return Err(not_found_domain(
                        "mcp.config.no_project_layer",
                        "the workspace root overlaps the user layer directory — no project-local layer exists; use the user scope".to_string(),
                    ));
                };
                if layer_defines(local, &name)? {
                    check_config_revision(local, expected_revision.as_deref())?;
                    set_server_tool_filter(local, &name, &patch).map_err(mcp_config_error)?;
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                // Not defined in the project local layer: create a
                // filter-only override entry (no transport — the A7
                // connection family stays with the shadowed layer) when the
                // name is defined in the project root or the user layer.
                let root_defined = layer_defines(&root_path, &name)?;
                let user_defined = match global_path.as_deref() {
                    Some(global) => layer_defines(global, &name)?,
                    None => false,
                };
                if root_defined || user_defined {
                    check_config_revision(local, expected_revision.as_deref())?;
                    upsert_filter_override_entry(local, &name, &patch).map_err(mcp_config_error)?;
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                Err(not_found_domain(
                    "mcp.server_not_found",
                    format!("MCP server '{name}' is not defined in any configuration layer"),
                ))
            }
            None => {
                let local = local_path.as_deref();
                let local_defines = match local {
                    Some(path) => layer_defines(path, &name)?,
                    None => false,
                };
                if local_defines {
                    let local = local.expect("checked above");
                    check_config_revision(local, expected_revision.as_deref())?;
                    set_server_tool_filter(local, &name, &patch).map_err(mcp_config_error)?;
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                if layer_defines(&root_path, &name)? {
                    // The definition layer is the read-only project root
                    // (the git-shared asset is never machine-written, A2) —
                    // the tool toggle writes a filter-only override to the
                    // project-local layer instead (SDR-S005 / PS3.3:
                    // tool-level control must work for every source;
                    // ADR-012 2026-10-17 revision).
                    let Some(local) = local else {
                        // The root-defined name in the overlap shape has no
                        // writable override slot — the message names both
                        // outsides (the user scope answers not-found for a
                        // root-only name; the root file is hand-edited).
                        return Err(not_found_domain(
                            "mcp.config.no_project_layer",
                            format!(
                                "the workspace root overlaps the user layer directory — no project-local layer exists to override '{name}'; edit the project-root .mcp.json directly"
                            ),
                        ));
                    };
                    check_config_revision(local, expected_revision.as_deref())?;
                    upsert_filter_override_entry(local, &name, &patch).map_err(mcp_config_error)?;
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                if let Some(global) = global_path
                    && layer_defines(&global, &name)?
                {
                    check_config_revision(&global, expected_revision.as_deref())?;
                    set_server_tool_filter(&global, &name, &patch).map_err(mcp_config_error)?;
                    self.spawn_mcp_pool_reconcile();
                    return ack();
                }
                Err(not_found_domain(
                    "mcp.server_not_found",
                    format!("MCP server '{name}' is not defined in any configuration layer"),
                ))
            }
        }
    }

    /// `mcp.tools(name)` — the single-server tool listing with per-tool
    /// filter state (ADR-004, the F-5 wire): every tool the connected
    /// server advertises, each annotated with the merged config's
    /// `enabled_tools` / `disabled_tools` verdict at call time — the raw
    /// filter fields only, never the connection's expanded values (A5),
    /// and the predicate is the connection layer's own
    /// [`McpServerConfig::is_tool_allowed`] (one semantics, no second
    /// opinion). A name no pool holds, or one that is not connected,
    /// answers the explicit `connected: false` shape (US1.7 — a named
    /// miss, never an empty-list imposture that would read as "connected
    /// with no tools").
    pub(super) async fn dispatch_list_mcp_tools(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let name = match &params {
            Value::Object(map) => named_string(map, &["name", "serverName"]),
            Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
        .ok_or_else(|| invalid_params("name is required for mcp.tools"))?;

        // The owning pool routes the name (workspace first — same-name
        // workspace entries shadow user entries, spec §4.2).
        let Some(manager) = self.owning_pool_manager(&name).await else {
            return Ok(json!({ "connected": false, "tools": [] }));
        };
        let Some(sums) = super::mcp_tool_summaries(&manager, &name).await else {
            return Ok(json!({ "connected": false, "tools": [] }));
        };
        let filter = self.merged_tool_filter_for_server(&name);
        let tools: Vec<Value> = sums
            .into_iter()
            .map(|(tool, description)| {
                // A name no layer defines is "not disabled": no stored
                // filter means every tool enabled — the connection
                // layer's own verdict on a filter-less config, never a
                // blanket-disabled imposture.
                let enabled = filter
                    .as_ref()
                    .is_none_or(|config| config.is_tool_allowed(&tool));
                let mut entry = json!({ "name": tool, "enabled": enabled });
                // `description` stays optional on the wire: an omitted
                // description never lands as an explicit null.
                if let Some(description) = description {
                    entry["description"] = json!(description);
                }
                entry
            })
            .collect();
        Ok(json!({ "connected": true, "tools": tools }))
    }

    /// The merged-config tool filter for `name` at call time: the entry's
    /// `enabled_tools` / `disabled_tools` as `load_mcp_config` merged the
    /// three layers (later layers replace same-name entries — the loader's
    /// merge). `None` when no layer defines the name (every tool enabled —
    /// the caller treats a missing verdict as "not disabled"). Only the
    /// literal filter fields are ever read from here (A5); plugin-bundled
    /// servers are not config-layer entries, so their filter state rides
    /// the plugin-user-state surface instead.
    fn merged_tool_filter_for_server(&self, name: &str) -> Option<McpServerConfig> {
        let workspace_root = self.manager.workspace().cwd();
        let home_base = self.mcp_user_layer_base()?;
        mirri_mcp::config::load_mcp_config(&home_base, workspace_root, workspace_root)
            .ok()?
            .get(name)
            .cloned()
    }

    /// The base directory whose `/.mirri-code/mcp.json` forms the MCP user
    /// layer: the bootstrap's pool-load base when the server was assembled
    /// ([`crate::bootstrap::EngineParts::mcp_home_base`]), else the same
    /// call-time chain the bootstrap resolves (injected home, then `$HOME`,
    /// then the resolved Mirri home). Keeping the assembled value primary
    /// guarantees the config surface and the running pools address the very
    /// same file even when the two chains would disagree (the `mirri serve`
    /// shape: injected home = `~/.mirri-code`, MCP base = `$HOME`).
    fn mcp_user_layer_base(&self) -> Option<PathBuf> {
        self.mcp_home_base
            .clone()
            .or_else(|| crate::bootstrap::resolve_mcp_home_base(self.home_dir.as_deref()))
    }

    /// Whether the project-local path and the user-layer global path
    /// resolve to the same file — the desktop overlap shape (`mirri web`
    /// spawns with cwd=`/`, the workspace root falls back to `$HOME`, and
    /// `<root>/.mirri-code/mcp.json` IS `~/.mirri-code/mcp.json`).
    /// Canonicalized when possible, literal comparison otherwise (missing
    /// files canonicalize to nothing).
    fn project_local_overlaps_user_layer(local: &Path, global: Option<&Path>) -> bool {
        let Some(global) = global else { return false };
        match (local.canonicalize(), global.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => local == global,
        }
    }

    /// The three config-layer paths for the calling context: (user,
    /// project root, project local). The user layer resolves through the
    /// same base the bootstrap's MCP manager load used (see
    /// [`Self::mcp_user_layer_base`]) so the config surface and the running
    /// manager always address the same file; the project layers anchor on
    /// the given workspace root — W7 per-root fan-out: the reconcile
    /// kernel iterates this over every built workspace's root, while the
    /// `mcp.config.*` RPC surface (which carries no workspace
    /// discriminator) passes the default workspace's root. The project-local
    /// slot is `None` in the overlap shape (the workspace root IS the user
    /// layer directory — the desktop fallback): the overlapping file
    /// belongs to the user layer only and never doubles as a project
    /// layer. Note the overlap test canonicalizes when possible and falls
    /// back to literal comparison — an aliased path to a not-yet-created
    /// file can slip past the literal fallback, but the created file is
    /// the user layer's own path, so subsequent loads detect it.
    fn mcp_config_layer_paths(
        &self,
        workspace_root: &std::path::Path,
    ) -> (Option<PathBuf>, PathBuf, Option<PathBuf>) {
        let global = self
            .mcp_user_layer_base()
            .map(|base| global_config_path(&base));
        let local = local_config_path(workspace_root);
        // The overlap guard (the desktop shape): the project-local slot is
        // the user layer's own file — it must not participate as a project
        // layer (re-labeling user entries as project, or feeding the
        // workspace pool with the user layer's connections).
        let local = if Self::project_local_overlaps_user_layer(&local, global.as_deref()) {
            None
        } else {
            Some(local)
        };
        (global, project_root_config_path(workspace_root), local)
    }

    /// The default workspace's root — the root the `mcp.config.*` RPC
    /// surface anchors on (see [`Self::mcp_config_layer_paths`]).
    fn default_layer_root(&self) -> PathBuf {
        self.manager.workspace().cwd().to_path_buf()
    }

    /// The write target for a scope: `user` → the global file, `workspace` →
    /// the project layer (the project root's Claude-compatible file is
    /// read-only by adjudication A2).
    fn writable_scope_path(&self, scope: McpConfigScope) -> Result<PathBuf, EngineError> {
        match scope {
            McpConfigScope::User => {
                let (global, _, _) = self.mcp_config_layer_paths(&self.default_layer_root());
                global.ok_or_else(|| {
                    EngineError::Internal(
                        "no home directory is available to resolve the user MCP config layer"
                            .to_string(),
                    )
                })
            }
            McpConfigScope::Workspace => {
                let (_, _, local) = self.mcp_config_layer_paths(&self.default_layer_root());
                // D-3: the overlap shape has no project-local layer — a
                // workspace-scoped write (mcp.config.set/remove) answers the
                // domain error instead of silently writing the user file.
                local.ok_or_else(|| not_found_domain(
                    "mcp.config.no_project_layer",
                    "the workspace root overlaps the user layer directory — no project-local layer exists; use the user scope".to_string(),
                ))
            }
        }
    }

    /// `mcp.sync` — the idempotent reconcile entry (F-6, ADR-006): re-run
    /// the single reconcile kernel over both pools and answer the summary
    /// of what it applied. Manual edits to the config layers (an editor
    /// over `~/.mirri-code/mcp.json`, a checked-out `.mcp.json`) converge
    /// through exactly the same kernel the config writes and the file
    /// watcher use — one diff semantics, never a second opinion. A layer
    /// too broken to parse keeps that pool's current connections and the
    /// call still succeeds (the conservative stance); a config the pools
    /// already match answers the all-zero summary.
    pub(super) async fn dispatch_mcp_sync(&self, _params: Value) -> Result<Value, EngineError> {
        let summary = self.reconcile_mcp_pools().await;
        serde_json::to_value(summary).map_err(|error| {
            EngineError::Internal(format!("MCP sync summary serialization failed: {error}"))
        })
    }

    /// The single reconcile kernel entry (F-6): re-derive the desired
    /// state of both pools from the config files and apply the incremental
    /// diff — the user pool, then the workspace pool — holding
    /// [`Self::mcp_reconcile_lock`] for the whole run so concurrent
    /// writers (and a racing `mcp.sync`) apply their pool operations in
    /// order, never interleaved. Answers what the pass applied, by op
    /// kind.
    pub(super) async fn reconcile_mcp_pools(&self) -> McpSyncSummary {
        let kernel = self.mcp_reconcile_kernel();
        // Serialize concurrent writes: pool operations apply one write at
        // a time (each task re-reads the desired state from the files, so
        // the last writer's view converges regardless of lock-acquisition
        // order).
        let _guard = self.mcp_reconcile_lock.lock().await;
        kernel.run().await
    }

    /// Spawn the background pool reconcile for a completed config write
    /// (spec §4.3) — the fire-and-forget form of
    /// [`Self::reconcile_mcp_pools`]: reload both scopes' desired entries,
    /// diff them against the pools' registered configs, and apply the
    /// incremental pool operations. The write already answered its ack;
    /// this runs detached, exactly like the startup lazy connect (no RPC
    /// ever waits on a connection). The untouched scope's diff is the
    /// idempotent steady state, so the write's hot path behaves exactly
    /// as the addressed scope's reconcile did.
    fn spawn_mcp_pool_reconcile(&self) {
        if self.mcp_manager.is_none() && self.user_mcp_pool.is_none() {
            return; // no pool wired (the low-level constructor) — the file write stands
        }
        let kernel = self.mcp_reconcile_kernel();
        let reconcile_lock = std::sync::Arc::clone(&self.mcp_reconcile_lock);
        tokio::spawn(async move {
            let _guard = reconcile_lock.lock().await;
            let summary = kernel.run().await;
            tracing::debug!(
                added = summary.added,
                removed = summary.removed,
                reconnected = summary.reconnected,
                refreshed = summary.refreshed,
                "background MCP pool reconcile complete"
            );
        });
    }

    /// Build the detached reconcile kernel: the pool handles, the live
    /// agents' host, and the layer locations the reload re-reads — owned
    /// `Arc` handles, so both the awaited `mcp.sync` path and the spawned
    /// post-write path run the exact same kernel.
    ///
    /// The listener attach happens here (idempotent — guarded by an atomic
    /// flag): a pool whose startup set was empty has no status listener
    /// yet, and without one a reconciled connect's tools would never reach
    /// the live agents. The user pool's shadow set is the fresh
    /// workspace-layer name view — config layers plus the plugin runtime
    /// names, the same scope the startup connect set covers (T1 review
    /// F-T1-3: the shared set refreshes on every call, not just the
    /// first).
    fn mcp_reconcile_kernel(&self) -> McpReconcileKernel {
        // The listener attach happens here (idempotent — guarded by an atomic
        // flag): a pool whose startup set was empty has no status listener
        // yet, and without one a reconciled connect's tools would never reach
        // the live agents. The user pool's shadow set is the fresh
        // multi-workspace union — every trusted built root's workspace-layer
        // names plus the plugin runtime names (T1 review F-T1-3: the shared
        // set refreshes on every call, not just the first).
        self.ensure_mcp_pool_listeners(&self.workspace_shadow_view());

        McpReconcileKernel {
            registry: Arc::clone(&self.registry),
            user_pool: self.user_mcp_pool.as_ref().map(|pool| pool.manager()),
            pool_events: self.mcp_pool_events.clone(),
            home_base: self.mcp_user_layer_base(),
        }
    }

    /// The user-pool shadow view as a multi-workspace union (W7): every
    /// **trusted** built runtime's workspace-layer server names plus the
    /// plugin runtime names — the names a user-pool event must not register
    /// (spec §4.2: the workspace entry owns the name's tools). Untrusted
    /// roots contribute nothing (their project layers never loaded, D6);
    /// registered-but-unbuilt rows hold no connections and no sessions, so
    /// they cannot shadow anything yet.
    pub(super) fn workspace_shadow_view(&self) -> std::collections::HashSet<String> {
        let home_base = self.mcp_user_layer_base();
        let mut shadowed = std::collections::HashSet::new();
        for runtime in self.registry.built_runtimes() {
            if !runtime.trusted() {
                continue;
            }
            shadowed.extend(
                desired_configs_for_scope(
                    McpConfigScope::Workspace,
                    home_base.as_deref(),
                    runtime.scope.cwd(),
                )
                .unwrap_or_default()
                .keys()
                .cloned(),
            );
        }
        if let Some(plugin_manager) = &self.plugin_manager {
            shadowed.extend(plugin_manager.enabled_mcp_servers().keys().cloned());
        }
        shadowed
    }

    /// The workspace pool of the runtime owning a live session (W7): the
    /// routing seam's resolution, read through to the built runtime's own
    /// pool — a session of a non-default workspace gets ITS workspace's
    /// project-layer tools, never the default's. `None` when no workspace
    /// owns the session (the caller falls back to the default pool, the
    /// historical single-workspace behavior).
    pub(super) fn workspace_pool_for_session(
        &self,
        session_id: &str,
    ) -> Option<Arc<mirri_mcp::McpConnectionManager>> {
        use crate::workspace_registry::ManagerResolution;
        match self.registry.manager_for(session_id) {
            ManagerResolution::Active { workspace_id, .. }
            | ManagerResolution::Restored { workspace_id, .. } => self
                .registry
                .built_runtime(&workspace_id)
                .and_then(|runtime| runtime.mcp_manager.clone()),
            ManagerResolution::NotFound => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pool reconcile (spec §4.3, the F-6 unified kernel)
// ---------------------------------------------------------------------------

/// The `mcp.sync` result summary (F-6): the pool operations the last
/// reconcile pass applied, counted by kind — connects (`added`),
/// disconnects (`removed`, both the unregistering removal and the
/// disabling tear-down), teardown-and-reconnects (`reconnected`), and
/// in-place config refreshes (`refreshed`, the tool-filter family). The
/// all-zero summary is the idempotent steady state: a config the pools
/// already match reconciles as a no-op.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) struct McpSyncSummary {
    added: u64,
    removed: u64,
    reconnected: u64,
    refreshed: u64,
}

/// The detached form of the reconcile kernel: everything a reconcile pass
/// operates on, as owned `Arc` handles a fire-and-forget task can carry —
/// so the awaited `mcp.sync` call and the post-write background spawn run
/// the exact same code (one diff semantics, never a second opinion).
///
/// W7 per-workspace fan-out: the workspace registry drives the pass —
/// every **built** runtime contributes its own root (project layers), its
/// own workspace pool, and its own session manager; the user pool stays
/// process-single and its operations fan to every built runtime's
/// sessions. Untrusted runtimes are skipped entirely (the D6 zero-load
/// red line: their project files were never loaded at build, and the
/// reconcile never re-reads them).
struct McpReconcileKernel {
    /// The workspace registry — the per-workspace fan-out source.
    registry: std::sync::Arc<crate::workspace_registry::WorkspaceRegistry>,
    /// The user pool's connection manager (the user layer's connections —
    /// one per server, shared across every workspace).
    user_pool: Option<std::sync::Arc<mirri_mcp::McpConnectionManager>>,
    /// The app-level pool status broadcast (M7, F-9): every applied pool
    /// operation re-emits its `event.mcp.server.status` payload onto this
    /// channel (ADR-008 — a persisted change always emits), in addition to
    /// the session-level fan-out `publish_pool_status_event` performs.
    pool_events: tokio::sync::broadcast::Sender<std::sync::Arc<serde_json::Value>>,
    /// The home chain the reload re-reads the user layer through.
    home_base: Option<PathBuf>,
}

impl McpReconcileKernel {
    /// Run the full pass — the user pool once (process-single), then every
    /// built workspace's pool against its own root — under the
    /// caller-held reconcile lock, accumulating the summary across all of
    /// them.
    async fn run(self) -> McpSyncSummary {
        let mut summary = McpSyncSummary::default();
        let runtimes = self.registry.built_runtimes();
        let all_managers: Vec<std::sync::Arc<mirri_core::SessionManager>> = runtimes
            .iter()
            .map(|runtime| std::sync::Arc::clone(&runtime.manager))
            .collect();
        // The multi-root shadow view: every trusted root's workspace-layer
        // names (the names a user-pool op must not re-register, spec §4.2).
        // Plugin runtime names are deliberately absent here (unlike
        // `workspace_shadow_view`): a plugin name can never appear in the
        // user layer's config-derived diff (the reserved `plugin-` prefix,
        // adjudication A1), so the check is unreachable for them anyway.
        let mut shadowed = std::collections::HashSet::new();
        for runtime in &runtimes {
            if runtime.trusted() {
                shadowed.extend(
                    desired_configs_for_scope(
                        McpConfigScope::Workspace,
                        self.home_base.as_deref(),
                        runtime.scope.cwd(),
                    )
                    .unwrap_or_default()
                    .keys()
                    .cloned(),
                );
            }
        }
        self.reconcile_user_scope(&runtimes, &all_managers, &shadowed, &mut summary)
            .await;
        for runtime in &runtimes {
            self.reconcile_workspace_scope(runtime, &mut summary).await;
        }
        summary
    }

    /// The user scope's reconcile (the process-single user pool): the
    /// desired set is the A7 cross-pool view over the user layer plus the
    /// project filter-only overrides of EVERY trusted root — the union the
    /// pools connected with — and every operation fans to all built
    /// workspaces' sessions (the pool is shared across them).
    async fn reconcile_user_scope(
        &self,
        runtimes: &[std::sync::Arc<crate::workspace_registry::WorkspaceRuntime>],
        all_managers: &[std::sync::Arc<mirri_core::SessionManager>],
        shadowed: &std::collections::HashSet<String>,
        summary: &mut McpSyncSummary,
    ) {
        let Some(target) = self.user_pool.clone() else {
            return; // no pool wired (the low-level constructor) — the file write stands
        };
        let Some(desired) = self.user_scope_desired(runtimes) else {
            // The conservative stance: a layer too broken to parse keeps
            // the pool's current connections (the sync call itself still
            // succeeds — the workspace scopes may still apply).
            tracing::warn!(
                "MCP config reconcile could not parse the config layers; keeping current connections"
            );
            return;
        };
        let current = target.registered_configs().await;
        // Guardrail: an empty desired set against a pool of live connections
        // whose user-layer file does not exist on disk is almost always a
        // base-resolution bug (the pool loaded a file the reload cannot see)
        // — never an intent a per-server `mcp.config.remove` would express.
        // Keep the connections and surface the divergence; a user who truly
        // emptied the layer edits the file to zero entries, which exists and
        // reconciles normally.
        if desired.is_empty()
            && !current.is_empty()
            && !self
                .home_base
                .as_deref()
                .map(global_config_path)
                .is_some_and(|path| path.exists())
        {
            tracing::error!(
                base = ?self.home_base,
                live_connections = current.len(),
                "MCP user-layer reconcile derived an empty desired set while the pool holds \
                 live connections and the user-layer file is missing; keeping the current \
                 connections (suspected base-resolution divergence)"
            );
            return;
        }
        for op in mirri_mcp::diff_pool_ops(&current, &desired) {
            self.apply_pool_op(
                &target,
                all_managers,
                McpConfigScope::User,
                shadowed,
                op,
                summary,
            )
            .await;
        }
    }

    /// The user scope's desired set as a multi-root union: the user layer's
    /// complete entries with every trusted root's project filter-only
    /// overrides merged over them (`user_merged` over the union — the same
    /// shape a single-root engine derived, extended per root). Roots fold
    /// in `built_runtimes` order (default first, then by id); a name
    /// overridden in several roots takes the last root's entry —
    /// deterministic, and a shape the single-root engine never produced.
    /// Untrusted roots contribute nothing (D6: their layers never loaded).
    fn user_scope_desired(
        &self,
        runtimes: &[std::sync::Arc<crate::workspace_registry::WorkspaceRuntime>],
    ) -> Option<HashMap<String, McpServerConfig>> {
        let home = self.home_base.as_deref()?;
        let (user, user_raw) = layer_file_with_raw(
            &global_config_path(home),
            mirri_mcp::config::StdioCwdDefault::Inherit,
        )?;
        let mut project_root = HashMap::new();
        let mut project_root_raw = HashMap::new();
        let mut project = HashMap::new();
        let mut project_raw = HashMap::new();
        for runtime in runtimes {
            if !runtime.trusted() {
                continue;
            }
            let root = runtime.scope.cwd();
            let (root_parsed, root_raw) = layer_file_with_raw(
                &project_root_config_path(root),
                mirri_mcp::config::StdioCwdDefault::ConfigDir,
            )?;
            project_root.extend(root_parsed);
            project_root_raw.extend(root_raw);
            // The overlap guard: a workspace root that IS the user layer
            // directory contributes no project-local layer (the desktop
            // fallback shape — the file was already read as the user layer).
            if let Some(local) = mirri_mcp::config::local_layer_path_if_distinct(home, root) {
                let (local_parsed, local_raw) =
                    layer_file_with_raw(&local, mirri_mcp::config::StdioCwdDefault::ConfigDir)?;
                project.extend(local_parsed);
                project_raw.extend(local_raw);
            }
        }
        let layered = mirri_mcp::config::LayeredMcpConfig {
            user,
            user_raw,
            project_root,
            project_root_raw,
            project,
            project_raw,
        };
        Some(layered.user_merged())
    }

    /// One workspace's scope reconcile (W7 fan-out): the runtime's own pool
    /// diffed against its own root's project layers, every operation
    /// fanning to that workspace's sessions only (per-workspace isolation,
    /// design 08 §2). An untrusted runtime converges to **empty** (W8's
    /// gate symmetry, US4.1 即刻断开): its desired set is nothing, so the
    /// diff's removes disconnect the project-layer servers a live flip
    /// (or a cross-process store write followed by a reconcile) left
    /// behind — the untrusted root's files are still never re-read (D6),
    /// only its existing connections are dropped.
    async fn reconcile_workspace_scope(
        &self,
        runtime: &crate::workspace_registry::WorkspaceRuntime,
        summary: &mut McpSyncSummary,
    ) {
        let Some(target) = runtime.mcp_manager.clone() else {
            return; // no pool wired for this root — the file write stands
        };
        let mut current = target.registered_configs().await;
        current.retain(|name, _| !name.starts_with("plugin-"));
        // The trust gate (D6): an untrusted root's desired set is EMPTY —
        // zero project-layer connections, converging what is already
        // registered there (the W8 untrust flip's immediate disconnect).
        let desired = if runtime.trusted() {
            match desired_configs_for_scope(
                McpConfigScope::Workspace,
                self.home_base.as_deref(),
                runtime.scope.cwd(),
            ) {
                Some(desired) => desired,
                None => {
                    // The conservative stance: a layer too broken to parse
                    // keeps this pool's current connections (the sync call
                    // itself still succeeds — the other scopes may apply).
                    tracing::warn!(
                        workspace = %runtime.workspace_id,
                        "MCP config reconcile could not parse the config layers; keeping current connections"
                    );
                    return;
                }
            }
        } else {
            std::collections::HashMap::new()
        };
        let managers = [std::sync::Arc::clone(&runtime.manager)];
        let no_shadow = std::collections::HashSet::new();
        for op in mirri_mcp::diff_pool_ops(&current, &desired) {
            self.apply_pool_op(
                &target,
                &managers,
                McpConfigScope::Workspace,
                &no_shadow,
                op,
                summary,
            )
            .await;
        }
    }

    /// Apply one incremental pool operation (the shared body of both
    /// scopes' diffs): add connects, remove disconnects, transport changes
    /// reconnect, tool-filter changes refresh registrations without
    /// touching the connection — counting each applied op into the summary
    /// by kind, and fanning the live agents' tool-table updates to the
    /// addressed scope's session managers (the user pool: every built
    /// workspace; a workspace pool: its own).
    ///
    /// Plugin-bundled servers (reserved `plugin-` runtime names) are never
    /// in the diff — their connection definitions are plugin-owned (A1),
    /// the plugin paths manage them.
    async fn apply_pool_op(
        &self,
        target: &std::sync::Arc<mirri_mcp::McpConnectionManager>,
        session_managers: &[std::sync::Arc<mirri_core::SessionManager>],
        scope: McpConfigScope,
        shadowed: &std::collections::HashSet<String>,
        op: mirri_mcp::PoolOp,
        summary: &mut McpSyncSummary,
    ) {
        match op {
            mirri_mcp::PoolOp::Connect { name, config } => {
                summary.added += 1;
                if scope == McpConfigScope::Workspace {
                    // F-T1-3 (T1 review): a hot-added workspace server
                    // takes over its name from any same-name user-layer
                    // server that already registered tools — drop the old
                    // registrations so the workspace entry's own Connected
                    // event registers the authoritative set (the user
                    // pool's future events are shadow-filtered by the
                    // refreshed shared set).
                    super::remove_server_tools_from_sessions(session_managers, &name).await;
                }
                if let Err(error) = target.connect_one(&name, config).await {
                    tracing::warn!(server = %name, %error, "MCP reconcile connect failed");
                } else {
                    // ADR-008 (M7): the persisted addition re-emits the
                    // status event with the added change kind — the pool
                    // listener's plain `status` event carries the
                    // transition, this one carries the persisted-change
                    // semantics.
                    self.publish_pool_op_event(
                        target,
                        session_managers,
                        &name,
                        scope.pool_label(),
                        "added",
                    )
                    .await;
                }
            }
            mirri_mcp::PoolOp::Disconnect { name, unregister } => {
                summary.removed += 1;
                // The removed event rides the pre-op state: after an
                // unregistering `remove()` the registration (and its
                // transport) is gone, so there is nothing left to name.
                self.publish_pool_op_event(
                    target,
                    session_managers,
                    &name,
                    scope.pool_label(),
                    "removed",
                )
                .await;
                drain_in_flight(target, &name).await;
                let result = if unregister {
                    target.remove(&name).await
                } else {
                    target.disable_server(&name).await
                };
                if let Err(error) = result {
                    tracing::warn!(server = %name, %error, "MCP reconcile disconnect failed");
                }
                if unregister {
                    // `remove()` notifies `Pending`, which the status
                    // listeners deliberately do not treat as a teardown —
                    // the entry is already gone from the config view.
                    // Unregister the live agents' stale bridge tools
                    // explicitly (T1 review F-T1-1): the same call the
                    // Failed/Disabled/NeedsAuth paths make, without which
                    // the LLM keeps a dead `McpBridgeTool` that answers
                    // `Disconnected`.
                    super::remove_server_tools_from_sessions(session_managers, &name).await;
                }
            }
            mirri_mcp::PoolOp::Reconnect { name, config } => {
                summary.reconnected += 1;
                drain_in_flight(target, &name).await;
                if let Err(error) = target.remove(&name).await {
                    tracing::warn!(
                        server = %name,
                        %error,
                        "MCP reconcile teardown before reconnect failed"
                    );
                }
                if let Err(error) = target.connect_one(&name, config).await {
                    tracing::warn!(server = %name, %error, "MCP reconcile reconnect failed");
                } else {
                    self.publish_pool_op_event(
                        target,
                        session_managers,
                        &name,
                        scope.pool_label(),
                        "reconfigured",
                    )
                    .await;
                }
            }
            mirri_mcp::PoolOp::RefreshToolFilter { name, config } => {
                summary.refreshed += 1;
                if let Err(error) = target.update_server_config(&name, config).await {
                    tracing::warn!(
                        server = %name,
                        %error,
                        "MCP reconcile config refresh failed"
                    );
                    return;
                }
                if scope == McpConfigScope::User && shadowed.contains(&name) {
                    // The workspace entry owns this name's tools (spec
                    // §4.2) — not the user layer.
                    return;
                }
                // Refresh the live agents' registrations under the new
                // filter: drop the old set, register the (possibly
                // narrowed) current one — same call order the status
                // listeners use.
                super::remove_server_tools_from_sessions(session_managers, &name).await;
                super::register_server_tools_to_sessions(target, session_managers, &name).await;
                // F-T1-4 (M7): the filter swap is a persisted change with
                // no connection transition, so the pool listeners stay
                // silent — the kernel re-emits the status event itself
                // (ADR-008: every persisted change emits).
                self.publish_pool_op_event(
                    target,
                    session_managers,
                    &name,
                    scope.pool_label(),
                    "field-change",
                )
                .await;
            }
        }
    }

    /// Re-emit one applied pool operation as an `event.mcp.server.status`
    /// payload on both legs (the live sessions' buses and the app-level
    /// broadcast), stamped with the persisted-change `change_kind`. The
    /// server's current state names the status (a refresh keeps the live
    /// state; a connect answers with what it settled into); a name no
    /// longer registered has nothing to report and is skipped.
    async fn publish_pool_op_event(
        &self,
        target: &std::sync::Arc<mirri_mcp::McpConnectionManager>,
        session_managers: &[std::sync::Arc<mirri_core::SessionManager>],
        name: &str,
        pool: &str,
        change_kind: &str,
    ) {
        let Some((_, state)) = target
            .list_connections()
            .await
            .into_iter()
            .find(|(conn_name, _)| conn_name == name)
        else {
            return;
        };
        super::publish_pool_status_event(
            target,
            session_managers,
            &self.pool_events,
            name,
            pool,
            Some(change_kind),
            &state,
        )
        .await;
    }
}

/// The desired pool state for a reconcile: the addressed scope's config
/// entries loaded with the loader's canonical transforms (raw `${VAR}`
/// text, anchored stdio `cwd` — the same shape the startup connect used,
/// so the diff compares like with like). `None` when an existing layer
/// file fails to parse (the reconcile then keeps the current connections —
/// the file watcher's stance) or when the user scope has no home to read.
///
/// The **workspace** scope derives its desired set from the **same A7
/// project merge** the startup connect set uses
/// (`mirri_mcp::config::LayeredMcpConfig::project_merged`, spec §4.2): a
/// local filter-only override (created by the workspace scope of
/// `setToolFilter`) contributes its filter to the same-name complete
/// entry, and a connection-only redefinition inherits the shadowed layer's
/// filter — the desired state is exactly what the pool connected, so the
/// reconcile diff never reverts the filter family on the live table
/// (review round 1, Cases A/B). A filter-only entry with no complete entry
/// in either project layer remains a non-connection and does not surface.
/// The **user** scope derives its desired set from the A7 cross-pool view
/// (`mirri_mcp::config::LayeredMcpConfig::user_merged`): the user layer's
/// complete entries with the project layers' filter-only overrides merged
/// over them — the same shape its startup connect set used, so a
/// workspace-scope `setToolFilter` on a user-defined name applies to the
/// live user pool as a `RefreshToolFilter` instead of surviving only as a
/// stale-on-restart file entry (F-14 residual).
///
/// One layer file: absent → empty; present but unparseable → `None`
/// (abort the reconcile, keep the current connections). Returns the
/// parsed entries plus their raw stored JSON — the A7 merge's
/// transport-presence source. Shared by the single-root desired-state
/// computation and the kernel's multi-root user-scope union.
/// `pub(super)` so the crate-local tests can exercise this computation.
fn layer_file_with_raw(
    path: &std::path::Path,
    stdio_cwd_default: mirri_mcp::config::StdioCwdDefault,
) -> Option<(
    HashMap<String, McpServerConfig>,
    HashMap<String, serde_json::Value>,
)> {
    if path.is_file() {
        mirri_mcp::config::load_config_file_with_raw_for_layer(path, stdio_cwd_default).ok()
    } else {
        Some((HashMap::new(), HashMap::new()))
    }
}

/// `pub(super)` so the crate-local tests can exercise this computation.
pub(super) fn desired_configs_for_scope(
    scope: McpConfigScope,
    home_base: Option<&std::path::Path>,
    workspace_root: &std::path::Path,
) -> Option<HashMap<String, McpServerConfig>> {
    match scope {
        McpConfigScope::User => {
            let (user_parsed, user_raw) = layer_file_with_raw(
                &global_config_path(home_base?),
                mirri_mcp::config::StdioCwdDefault::Inherit,
            )?;
            let (root_parsed, root_raw) = layer_file_with_raw(
                &project_root_config_path(workspace_root),
                mirri_mcp::config::StdioCwdDefault::ConfigDir,
            )?;
            let (local_parsed, local_raw) = match home_base {
                // The overlap guard: no distinct project-local layer (the
                // desktop fallback shape) — an empty layer, semantics
                // identical to "no project file". An unresolvable home
                // (None) is NOT an overlap — the local layer participates.
                Some(home) => {
                    match mirri_mcp::config::local_layer_path_if_distinct(home, workspace_root) {
                        Some(local) => layer_file_with_raw(
                            &local,
                            mirri_mcp::config::StdioCwdDefault::ConfigDir,
                        )?,
                        None => (HashMap::new(), HashMap::new()),
                    }
                }
                None => layer_file_with_raw(
                    &mirri_mcp::config::local_config_path(workspace_root),
                    mirri_mcp::config::StdioCwdDefault::ConfigDir,
                )?,
            };
            // The A7 cross-pool merge (`user_merged`) — the same shape the
            // startup connect set used: the user layer's complete entries
            // with the project layers' filter-only overrides applied, the
            // complete project entries skipped (workspace-owned names).
            let layered = mirri_mcp::config::LayeredMcpConfig {
                user: user_parsed,
                user_raw,
                project_root: root_parsed,
                project_root_raw: root_raw,
                project: local_parsed,
                project_raw: local_raw,
            };
            Some(layered.user_merged())
        }
        McpConfigScope::Workspace => {
            let (root_parsed, root_raw) = layer_file_with_raw(
                &project_root_config_path(workspace_root),
                mirri_mcp::config::StdioCwdDefault::ConfigDir,
            )?;
            let (local_parsed, local_raw) = match home_base {
                Some(home) => {
                    match mirri_mcp::config::local_layer_path_if_distinct(home, workspace_root) {
                        Some(local) => layer_file_with_raw(
                            &local,
                            mirri_mcp::config::StdioCwdDefault::ConfigDir,
                        )?,
                        None => (HashMap::new(), HashMap::new()),
                    }
                }
                None => layer_file_with_raw(
                    &mirri_mcp::config::local_config_path(workspace_root),
                    mirri_mcp::config::StdioCwdDefault::ConfigDir,
                )?,
            };
            // The A7 project merge — the exact view `project_merged`
            // produced the startup connect set (spec §4.2): filter-only
            // overrides contribute their filter to the same-name complete
            // entry, connection-only redefinitions inherit the shadowed
            // layer's filter.
            let layered = mirri_mcp::config::LayeredMcpConfig {
                user: HashMap::new(),
                user_raw: HashMap::new(),
                project_root: root_parsed,
                project_root_raw: root_raw,
                project: local_parsed,
                project_raw: local_raw,
            };
            Some(layered.project_merged())
        }
    }
}

/// Let in-flight tool calls on `name` finish before a teardown; after the
/// drain window, proceed anyway — the in-flight call then fails with an
/// error naming the server (spec §4.3 / R2's force-fail half).
async fn drain_in_flight(manager: &std::sync::Arc<mirri_mcp::McpConnectionManager>, name: &str) {
    if !manager.wait_for_idle(name, RECONCILE_INFLIGHT_DRAIN).await {
        tracing::warn!(
            server = %name,
            "in-flight MCP tool calls did not finish within the drain window; forcing the disconnect"
        );
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// The scope a `mcp.config.*` call addresses (A2: only the user file and
/// the project layer are machine-writable; the project-root layer is a
/// read-only shared asset). `pub(super)` so the crate-local tests can
/// exercise the reconcile's desired-state computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpConfigScope {
    User,
    Workspace,
}

impl McpConfigScope {
    fn parse_str(value: &str) -> Result<Self, EngineError> {
        match value {
            "user" => Ok(Self::User),
            "workspace" => Ok(Self::Workspace),
            other => Err(invalid_params(format!(
                "scope must be 'user' or 'workspace', got '{other}'"
            ))),
        }
    }

    /// The scope's pool label for the M7 `event.mcp.server.status` payload's
    /// `pool` field — which pool the reconciled server belongs to.
    fn pool_label(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
        }
    }
}

/// Parse the list methods' optional `scope` argument (named object with
/// the `configScope` alias tolerated, or the positional tuple spelling).
fn optional_scope_arg(params: &Value) -> Result<Option<McpConfigScope>, EngineError> {
    let raw = match params {
        Value::Array(items) => match items.first() {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        },
        Value::Object(map) => match map.get("scope").or_else(|| map.get("configScope")) {
            Some(Value::String(s)) => Some(s.as_str()),
            None | Some(Value::Null) => None,
            Some(_) => {
                return Err(invalid_params("scope must be 'user' or 'workspace'"));
            }
        },
        _ => None,
    };
    match raw {
        Some(value) => Ok(Some(McpConfigScope::parse_str(value)?)),
        None => Ok(None),
    }
}

/// Parse the `(scope, name, expectedRevision?)` pair/triple (`remove`),
/// accepting the object spelling (`{scope, name|serverName,
/// expectedRevision?}`) and the positional tuple spelling
/// (`[scope, name, expectedRevision?]`).
fn scope_name_args(
    params: &Value,
    method: &str,
) -> Result<(McpConfigScope, String, Option<String>), EngineError> {
    match normalize_params(params.clone()) {
        Value::Array(items) => match items.as_slice() {
            [Value::String(scope), Value::String(name)] => {
                Ok((McpConfigScope::parse_str(scope)?, name.clone(), None))
            }
            [Value::String(scope), Value::String(name), tail @ ..] => Ok((
                McpConfigScope::parse_str(scope)?,
                name.clone(),
                trailing_expected_revision(tail, method)?,
            )),
            _ => Err(invalid_params(format!(
                "{method} takes (scope, name, expectedRevision?)"
            ))),
        },
        Value::Object(map) => {
            let scope = map
                .get("scope")
                .or_else(|| map.get("configScope"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params(format!("scope is required for {method}")))?;
            let name = named_string(&map, &["name", "serverName"])
                .ok_or_else(|| invalid_params(format!("name is required for {method}")))?;
            Ok((
                McpConfigScope::parse_str(scope)?,
                name,
                expected_revision_in(&map)?,
            ))
        }
        _ => Err(invalid_params(format!(
            "{method} takes (scope, name, expectedRevision?)"
        ))),
    }
}

/// Parse the `(scope, name, config, expectedRevision?)` quadruple (`set`),
/// accepting the object spelling (`{scope, name|serverName, config|
/// serverConfig, expectedRevision?}`) and the positional tuple spelling
/// (`[scope, name, config, expectedRevision?]`). The config is stored
/// exactly as provided — validation happens in the write path.
fn set_args(
    params: &Value,
) -> Result<(McpConfigScope, String, Value, Option<String>), EngineError> {
    match normalize_params(params.clone()) {
        Value::Array(items) => match items.as_slice() {
            [Value::String(scope), Value::String(name), config] => Ok((
                McpConfigScope::parse_str(scope)?,
                name.clone(),
                config.clone(),
                None,
            )),
            [Value::String(scope), Value::String(name), config, tail @ ..] => Ok((
                McpConfigScope::parse_str(scope)?,
                name.clone(),
                config.clone(),
                trailing_expected_revision(tail, "mcp.config.set")?,
            )),
            _ => Err(invalid_params(
                "mcp.config.set takes (scope, name, config, expectedRevision?)",
            )),
        },
        Value::Object(map) => {
            let scope = map
                .get("scope")
                .or_else(|| map.get("configScope"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("scope is required for mcp.config.set"))?;
            let name = named_string(&map, &["name", "serverName"])
                .ok_or_else(|| invalid_params("name is required for mcp.config.set"))?;
            let config = map
                .get("config")
                .or_else(|| map.get("serverConfig"))
                .ok_or_else(|| invalid_params("config is required for mcp.config.set"))?;
            if !config.is_object() {
                return Err(invalid_params("config must be an object"));
            }
            Ok((
                McpConfigScope::parse_str(scope)?,
                name,
                config.clone(),
                expected_revision_in(&map)?,
            ))
        }
        _ => Err(invalid_params(
            "mcp.config.set takes (scope, name, config, expectedRevision?)",
        )),
    }
}

/// Parse the `setToolFilter(name, filter, scope?, expectedRevision?)`
/// call: the object spelling (`{name|serverName, filter|toolFilter, scope?,
/// expectedRevision?}`) and the positional tuple spelling. The tuple tail
/// stays backward compatible with the historical
/// `[name, filter, expectedRevision?]` shape (N5 — a trailing addition may
/// not break old callers): one trailing string that is exactly the literal
/// scope value `"user"`/`"workspace"` reads as the scope
/// (`[name, filter, scope]`), any other single trailing string reads as
/// `expectedRevision`, and the full new shape is
/// `[name, filter, scope, expectedRevision]`.
fn set_tool_filter_args(
    params: &Value,
) -> Result<
    (
        String,
        McpToolFilterPatch,
        Option<McpConfigScope>,
        Option<String>,
    ),
    EngineError,
> {
    match params {
        Value::Array(items) => {
            let name = items
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("name is required for mcp.config.setToolFilter"))?
                .to_string();
            let patch = tool_filter_arg(params)?;
            let (scope, expected_revision) = match items.as_slice() {
                [_, _, Value::String(scope), Value::String(revision)] => (
                    Some(McpConfigScope::parse_str(scope)?),
                    Some(revision.clone()),
                ),
                [_, _, Value::Null, Value::String(revision)] => (None, Some(revision.clone())),
                [_, _, Value::String(tail)] if tail == "user" || tail == "workspace" => {
                    (Some(McpConfigScope::parse_str(tail)?), None)
                }
                [_, _, Value::String(revision)] => (None, Some(revision.clone())),
                [_, _, ..] => {
                    return Err(invalid_params(
                        "mcp.config.setToolFilter takes (name, filter, scope?, expectedRevision?)",
                    ));
                }
                _ => (None, None),
            };
            Ok((name, patch, scope, expected_revision))
        }
        Value::Object(map) => {
            let name = named_string(map, &["name", "serverName"])
                .ok_or_else(|| invalid_params("name is required for mcp.config.setToolFilter"))?;
            let patch = tool_filter_arg(params)?;
            Ok((
                name,
                patch,
                optional_scope_arg(params)?,
                expected_revision_in(map)?,
            ))
        }
        _ => Err(invalid_params(
            "mcp.config.setToolFilter takes (name, filter, scope?, expectedRevision?)",
        )),
    }
}

/// Parse the tool-filter patch (`{filter|toolFilter}` with optional
/// `enabledTools`/`disabledTools` string arrays, or the second positional
/// object). The patch is the entry's complete new filter state: present
/// fields replace, absent fields clear.
fn tool_filter_arg(params: &Value) -> Result<McpToolFilterPatch, EngineError> {
    let filter = match params {
        Value::Array(items) => items.get(1),
        Value::Object(map) => map.get("filter").or_else(|| map.get("toolFilter")),
        _ => None,
    };
    let Some(filter) = filter else {
        return Err(invalid_params(
            "filter is required for mcp.config.setToolFilter",
        ));
    };
    let Some(map) = filter.as_object() else {
        return Err(invalid_params(
            "filter must be an object with optional enabledTools/disabledTools arrays",
        ));
    };
    Ok(McpToolFilterPatch {
        enabled_tools: string_list_field(map, "enabledTools")?,
        disabled_tools: string_list_field(map, "disabledTools")?,
    })
}

/// The one trailing positional `expectedRevision` element of a tuple
/// spelling (`[.., expectedRevision]`); more than one extra element is a
/// malformed call.
fn trailing_expected_revision(tail: &[Value], method: &str) -> Result<Option<String>, EngineError> {
    match tail {
        [] => Ok(None),
        [Value::String(revision)] => Ok(Some(revision.clone())),
        _ => Err(invalid_params(format!(
            "{method} takes an optional trailing expectedRevision string"
        ))),
    }
}

/// Read the optional named `expectedRevision` field (`expected_revision`
/// alias tolerated) of the object spelling.
fn expected_revision_in(
    map: &serde_json::Map<String, Value>,
) -> Result<Option<String>, EngineError> {
    match map
        .get("expectedRevision")
        .or_else(|| map.get("expected_revision"))
    {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(revision)) => Ok(Some(revision.clone())),
        Some(_) => Err(invalid_params("expectedRevision must be a string")),
    }
}

/// Whether one config layer file defines `name`. A missing file reads as
/// an empty envelope (`false`); a file that fails to parse is a config
/// error — the management surface never silently skips a broken layer it
/// is about to write.
fn layer_defines(path: &std::path::Path, name: &str) -> Result<bool, EngineError> {
    let root = read_raw_config_file(path).map_err(mcp_config_error)?;
    Ok(root
        .get("mcpServers")
        .and_then(|servers| servers.get(name))
        .is_some())
}

/// The F-3 optimistic-lock check: when the caller side-carried an
/// `expectedRevision`, it must equal the addressed layer file's current
/// content-addressed revision — a mismatch answers the
/// `mcp.config.conflict` domain error (the message carries the current
/// revision so the client can re-list and retry) and the file is left
/// untouched. `None` keeps the historical last-writer-wins behavior (N5).
fn check_config_revision(
    path: &std::path::Path,
    expected: Option<&str>,
) -> Result<(), EngineError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = layer_revision(path);
    if current != expected {
        return Err(config_conflict_error(expected, &current));
    }
    Ok(())
}

/// The F-3 conflict error: `expectedRevision` no longer matches the
/// addressed layer's current revision. The domain code rides the same
/// `data.domainCode` slot the `mcp.server_not_found` family uses.
fn config_conflict_error(expected: &str, current: &str) -> EngineError {
    not_found_domain(
        "mcp.config.conflict",
        format!(
            "MCP config revision conflict: expected {expected}, current {current} — re-list and retry"
        ),
    )
}

/// Read one optional string-array field (`None`/absent → `None`).
fn string_list_field(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<String>>, EngineError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                let Some(value) = item.as_str() else {
                    return Err(invalid_params(format!(
                        "{field} must be an array of strings"
                    )));
                };
                list.push(value.to_string());
            }
            Ok(Some(list))
        }
        Some(_) => Err(invalid_params(format!(
            "{field} must be an array of strings"
        ))),
    }
}

/// The first present string among the given key spellings.
fn named_string(map: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| map.get(*name))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Reject writes to plugin-bundled servers (A1: the connection definition
/// is plugin-owned — only tool filters are user-writable, and those go
/// through the plugin user state, not the config files).
fn reject_plugin_server(name: &str, method: &str) -> Result<(), EngineError> {
    if name.starts_with("plugin-") {
        return Err(invalid_params(format!(
            "'{name}' is a plugin-bundled MCP server; plugin connection definitions are read-only ({method}, adjudication A1)"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

/// Present a stored entry for the wire: TS preprocess parity
/// (`agent-core-v2/mcpCore/config-schema.ts`) — infer the flat `transport`
/// tag when the stored entry omits it (`command` → `stdio`, `url` →
/// `http`); every other byte stays verbatim, `${VAR}` references included
/// (adjudication A5). An entry that already carries a `transport` key is
/// presented untouched.
fn present_config_entry(raw: &Value) -> Value {
    let Some(obj) = raw.as_object() else {
        return raw.clone();
    };
    if obj.contains_key("transport") {
        return raw.clone();
    }
    let inferred = if obj.get("command").is_some_and(Value::is_string) {
        "stdio"
    } else if obj.get("url").is_some_and(Value::is_string) {
        "http"
    } else {
        return raw.clone();
    };
    let mut presented = obj.clone();
    presented.insert("transport".to_string(), json!(inferred));
    Value::Object(presented)
}

/// Derive the effective-state view from a parsed config: the server-level
/// overrides (`enabled` defaults true, the tool filters and timeouts
/// present-only). Shared by the per-entry derivation and the A7 merged
/// view (the same fields either way — the transport family never appears
/// on this surface).
fn effective_view_from_config(config: &McpServerConfig) -> Value {
    let mut effective = json!({ "enabled": config.enabled.unwrap_or(true) });
    if let Some(tools) = &config.enabled_tools {
        effective["enabledTools"] = json!(tools);
    }
    if let Some(tools) = &config.disabled_tools {
        effective["disabledTools"] = json!(tools);
    }
    if let Some(ms) = config.startup_timeout_ms {
        effective["startupTimeoutMs"] = json!(ms);
    }
    if let Some(ms) = config.tool_timeout_ms {
        effective["toolTimeoutMs"] = json!(ms);
    }
    effective
}

/// Derive the effective-state view from a stored entry: the parsed
/// server-level overrides. An entry too broken to parse still lists — the
/// management view must show it so the user can fix it — and every field
/// this view carries is an optional override the broken entry does not
/// store, so the defaults are the honest answer (the connect path reports
/// the transport breakage itself).
fn effective_config_view(raw: &Value) -> Value {
    match serde_json::from_value::<McpServerConfig>(raw.clone()) {
        Ok(config) => effective_view_from_config(&config),
        Err(_) => json!({ "enabled": true }),
    }
}

/// A parsed `mcp.config.test`-shaped target — shared by the test and
/// preview paths, which locate their config the same way: the direct
/// `config` object (a not-yet-saved entry), or the `scope` + `name` pair
/// addressing a persisted one.
struct ConfigTarget {
    scope: Option<McpConfigScope>,
    name: Option<String>,
    direct: Option<Value>,
}

/// Parse the `mcp.config.test`-shaped arguments (the object spelling
/// `{config}` or `{scope, name}`, or the positional tuple spellings
/// `[config]` / `[scope, name]`) into the target the caller then resolves.
fn parse_test_args(params: &Value, method: &str) -> Result<ConfigTarget, EngineError> {
    match params {
        Value::Array(items) => match items.as_slice() {
            [config] if config.is_object() => Ok(ConfigTarget {
                scope: None,
                name: None,
                direct: Some(config.clone()),
            }),
            [Value::String(scope), Value::String(name)] => Ok(ConfigTarget {
                scope: Some(McpConfigScope::parse_str(scope)?),
                name: Some(name.clone()),
                direct: None,
            }),
            _ => Err(invalid_params(format!(
                "{method} takes a config, or a scope and name"
            ))),
        },
        Value::Object(map) => {
            let direct = map.get("config").or_else(|| map.get("serverConfig"));
            match (
                direct,
                map.get("scope").or_else(|| map.get("configScope")),
                map.get("name").or_else(|| map.get("serverName")),
            ) {
                (Some(config), _, _) if config.is_object() => Ok(ConfigTarget {
                    scope: None,
                    name: None,
                    direct: Some(config.clone()),
                }),
                (_, Some(Value::String(scope)), Some(Value::String(name))) => Ok(ConfigTarget {
                    scope: Some(McpConfigScope::parse_str(scope)?),
                    name: Some(name.as_str().to_string()),
                    direct: None,
                }),
                _ => Err(invalid_params(format!(
                    "{method} takes a config, or a scope and name"
                ))),
            }
        }
        _ => Err(invalid_params(format!(
            "{method} takes a config, or a scope and name"
        ))),
    }
}

/// The domain-coded not-found a scope + name lookup answers when no
/// addressed layer defines the name (shared by the test and preview
/// location paths).
fn named_server_not_found(scope: McpConfigScope, name: &str) -> EngineError {
    not_found_domain(
        "mcp.server_not_found",
        format!(
            "MCP server '{name}' is not defined in the {} configuration layer",
            match scope {
                McpConfigScope::User => "user",
                McpConfigScope::Workspace => "workspace",
            }
        ),
    )
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map an [`mirri_mcp::McpError`] from the config surface onto the engine
/// wire: a validation failure (`InvalidInput` — an illegal server config,
/// a filter target missing from the addressed layer) is client-correctable
/// and answers `invalid params`; every file-level failure (unreadable,
/// unparseable, unwritable) is an internal error carrying the raw message.
fn mcp_config_error(error: mirri_mcp::McpError) -> EngineError {
    match error {
        mirri_mcp::McpError::InvalidInput(message) => invalid_params(message),
        other => EngineError::Internal(format!("MCP config error: {other}")),
    }
}
