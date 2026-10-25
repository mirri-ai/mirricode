//! Workspace-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B4-b surface
//! (`workspace.*`).
//!
//! Batch W2 replaces the single-bound honest stubs with the real
//! registry semantics (design `08-multi-workspace.md` §4):
//!
//! - `workspace.createOrTouch` — validate the root exists and is a
//!   directory, normalize it **lexically** (never canonicalized — the id
//!   derivation chain's TS parity), persist through the D8 locked
//!   read-merge-write, register + build the workspace's runtime, and return
//!   the `WorkspaceView`. Idempotent: the same root again touches
//!   `last_opened_at` without rebuilding.
//! - `workspace.list` — every registry registration with its live
//!   `session_count` from the index read model, ordered by `last_opened_at`
//!   descending (the in-memory value, so touches are immediately visible).
//! - `workspace.get` / `handlerFor` — resolve by id (root references are
//!   normalized + key-derivated, then resolved); an unknown workspace is
//!   JSON `null` (the klient `maybe` semantics).
//! - `workspace.update` — rename (display name only): written through the
//!   index row and the D8 merged registry write, announced as
//!   `event.workspace.updated` (W6).
//! - `workspace.delete` — the D4 three-choice unregister (`mode: "purge"`
//!   deletes the bucket data + index rows unrecoverably; `mode: "remove"`
//!   preserves every byte for a re-register restore); both modes close the
//!   workspace's live sessions, tombstone every alias member, and announce
//!   `event.workspace.deleted` (W6).
//! - `workspace.setTrust` — the W8 trust management face: `{workspaceId,
//!   trusted}` writes the home-level trust store, then flips the live
//!   gates (runtime verdict, AGENTS.md gates, and a reconcile pass that
//!   connects or disconnects the workspace's project-layer MCP servers
//!   immediately) without rebuilding the runtime or closing sessions.
//!   Every view (`list`/`get`/`createOrTouch`/`update`) carries the live
//!   `trusted` field read from the store.
//!
//! Every mutation emits the matching server-scoped lifecycle event
//! (`event.workspace.created` / `updated` / `deleted`) AFTER the
//! persistence landed (contract 3 — the vocabulary lives in
//! `event_projection.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use mirri_core::{
    CloseReason, ServerEvent, SessionId, WorkspaceSnapshot, encode_work_dir_key, lexically_resolve,
};
use mirri_session_index::{RegistryWriter, WORKSPACE_REGISTRY_FILENAME, WorkspaceRow};
use mirri_transport::adapter::EngineError;

use super::responses::{
    WORKSPACE_SCOPE_KIND, WorkspaceDeletedResult, WorkspaceHandleResult, WorkspaceTrustResult,
    WorkspaceView, to_result,
};
use super::{EmbeddedServer, invalid_params, normalize_params, session_error};

impl EmbeddedServer {
    // --- view / persistence helpers -----------------------------------------

    /// The bound workspace's `WorkspaceView` for one registration — the
    /// shared `createOrTouch` / `list` / `get` result build: camelCase,
    /// RFC3339 string timestamps, `name` defaulting to the root's basename
    /// (TS `name ?? basename(root)` parity), and the `sessionCount` served
    /// from the session index's read model scoped to the workspace key.
    fn workspace_view(
        &self,
        registration: &crate::workspace_registry::WorkspaceRegistration,
    ) -> WorkspaceView {
        let name = registration
            .name
            .clone()
            .or_else(|| basename_of(&registration.root))
            .unwrap_or_default();
        WorkspaceView {
            id: registration.id.clone(),
            root: registration.root.to_string_lossy().to_string(),
            name,
            created_at: registration.created_at.clone(),
            last_opened_at: registration.last_opened_at.clone(),
            session_count: self.workspace_session_count(&registration.id),
            trusted: self.workspace_is_trusted(&registration.id),
        }
    }

    /// The workspace's LIVE trust verdict (W8, design 08 §5 D6): the
    /// default workspace (this engine's pin root) is trusted by
    /// construction; every other row answers from the home-level trust
    /// store — a fresh read, never a runtime snapshot, so a
    /// `workspace.setTrust` flip (or another process's store write) is
    /// visible to the very next listing. A no-store engine's non-default
    /// rows answer untrusted (the honest degraded read).
    fn workspace_is_trusted(&self, workspace_id: &str) -> bool {
        if Some(workspace_id) == self.registry.default_workspace_id().as_deref() {
            return true;
        }
        self.trust_store
            .as_ref()
            .is_some_and(|store| store.is_trusted(workspace_id))
    }

    /// The workspace's non-archived session count, from the index's read
    /// model when one is available; `0` (the honest fallback) otherwise.
    /// The count folds the workspace's D3 alias set (ADR D3): a legacy
    /// split bucket (another spelling of the same root) counts with it.
    fn workspace_session_count(&self, workspace_id: &str) -> usize {
        let Some(index) = self.session_index() else {
            return 0;
        };
        let group = self.registry.alias_group_of(workspace_id);
        index.count_active(Some(&group)).unwrap_or(0)
    }

    /// The registry file path — `<home>/workspaces.json`, the cross-engine
    /// source of truth; `None` when no home was injected (the persistence
    /// degrade parity of the session index: registrations still land
    /// in-memory, they just never reach the file).
    fn workspace_registry_path(&self) -> Option<PathBuf> {
        self.home_dir
            .as_deref()
            .map(|home| home.join(WORKSPACE_REGISTRY_FILENAME))
    }

    /// The workspace rows the registry currently holds (one per
    /// registration), converted to the index/registry row shape.
    fn registry_rows(&self) -> Vec<WorkspaceRow> {
        self.registry
            .registrations()
            .iter()
            .map(registration_to_row)
            .collect()
    }

    /// Persist one registration: the session-index row first (db +
    /// legacy write-through), then the D8 locked read-merge-write of the
    /// full registry (local rows ∪ disk rows, tombstone wins — the last
    /// writer is the merge, so a concurrent TS row written between the two
    /// survives). Persistence failures propagate — an RPC must never claim
    /// a registration that did not land.
    fn persist_workspace_registration(
        &self,
        registration: &crate::workspace_registry::WorkspaceRegistration,
    ) -> Result<(), EngineError> {
        if let Some(index) = self.session_index() {
            index
                .upsert_workspace(&registration_to_row(registration))
                .map_err(|error| {
                    EngineError::Internal(format!("indexing the workspace failed: {error}"))
                })?;
        }
        if let Some(registry_path) = self.workspace_registry_path() {
            let local_rows = self.registry_rows();
            RegistryWriter::new(registry_path)
                .write_merged(&local_rows)
                .map_err(|error| {
                    EngineError::Internal(format!("writing workspaces.json failed: {error}"))
                })?;
        }
        Ok(())
    }

    /// [`Self::persist_workspace_registration`] for a **fresh registration**
    /// (the `createOrTouch` new-root arm): the id is reinstated — its
    /// tombstone, if any, is cleared by this write (the D4 "re-register
    /// clears" rule; the TS `createOrTouch` does the same
    /// `deletedIds.delete(ws.id)`), so a remove→re-register cycle restores
    /// the workspace across processes.
    fn persist_workspace_reinstatement(
        &self,
        registration: &crate::workspace_registry::WorkspaceRegistration,
    ) -> Result<(), EngineError> {
        if let Some(index) = self.session_index() {
            index
                .upsert_workspace(&registration_to_row(registration))
                .map_err(|error| {
                    EngineError::Internal(format!("indexing the workspace failed: {error}"))
                })?;
        }
        if let Some(registry_path) = self.workspace_registry_path() {
            let local_rows = self.registry_rows();
            RegistryWriter::new(registry_path)
                .write_merged_reinstated(&local_rows, std::slice::from_ref(&registration.id))
                .map_err(|error| {
                    EngineError::Internal(format!("writing workspaces.json failed: {error}"))
                })?;
        }
        Ok(())
    }

    /// The D8 touch debounce arm (W6, design §2.3): `session.create` touches
    /// the workspace's `last_opened_at` **in memory** and marks it pending a
    /// deferred flush — a session creation never writes the registry file
    /// directly. The first touch after an idle window arms one detached
    /// timer that flushes when the window ends; touches inside the window
    /// only update memory. Register/rename/delete flush through their own
    /// immediate writes (which carry the current rows — pending touches are
    /// folded in, and the pending set is drained by
    /// [`Self::drain_pending_registry_touches`]).
    pub(super) fn touch_workspace_last_opened(&self, workspace_id: &str) {
        if !self.registry.touch_opened(workspace_id) {
            return; // an unregistered id is never an error for a touch
        }
        if !self.registry.try_arm_touch_flush() {
            return; // a timer is already armed for this window
        }
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            tokio::time::sleep(crate::workspace_registry::WorkspaceRegistry::TOUCH_DEBOUNCE_WINDOW)
                .await;
            // A flush with an empty pending set is a no-op — the window's
            // touches were already folded into an immediate write.
            registry.flush_pending_registry_touches();
        });
    }

    /// Drain the pending debounce touches without writing (the immediate
    /// write paths call this before their own write, which already carries
    /// every current row — the in-memory touches ride along).
    pub(super) fn drain_pending_registry_touches(&self) {
        let _ = self.registry.take_pending_registry_touches();
    }

    // --- workspace.list -----------------------------------------------------

    /// `workspace.list` — every registry registration as a `WorkspaceView`,
    /// ordered by `last_opened_at` descending (the in-memory value, so a
    /// touch is immediately visible to the next listing).
    pub(super) async fn dispatch_list_workspaces(
        &self,
        _params: Value,
    ) -> Result<Value, EngineError> {
        let mut registrations = self.registry.registrations();
        registrations.sort_by(|a, b| {
            parse_rfc3339(&b.last_opened_at).cmp(&parse_rfc3339(&a.last_opened_at))
        });
        let views: Vec<WorkspaceView> = registrations
            .iter()
            .map(|registration| self.workspace_view(registration))
            .collect();
        to_result(views)
    }

    // --- workspace.createOrTouch --------------------------------------------

    /// `workspace.createOrTouch` — register a workspace root (or touch an
    /// already-registered one): validate the root exists and is a directory,
    /// normalize it lexically (`lexically_resolve` — the id derivation
    /// chain's TS parity, symlinks are never canonicalized), persist, and
    /// register + build. A new workspace returns `WorkspaceCreated`; a touch
    /// of an existing one updates `last_opened_at` (and the name, when one
    /// was requested) without rebuilding and returns `WorkspaceUpdated`.
    pub(super) async fn dispatch_create_workspace(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: CreateWorkspaceParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let raw_root = PathBuf::from(&params.root);
        if !raw_root.is_dir() {
            return Err(invalid_params(format!(
                "workspace root does not exist or is not a directory: {}",
                params.root
            )));
        }
        // Lexical normalization ONLY — canonicalizing here would hash a
        // different string for symlinked roots than the TS side (the id
        // derivation in `workdir_key.rs` documents this in detail).
        let root = lexically_resolve(&raw_root);
        let workspace_id = encode_work_dir_key(&root);
        let now = Utc::now().to_rfc3339();

        let existing = self.registry.registration(&workspace_id);
        let (registration, event) = match existing {
            Some(mut registration) => {
                // Touch: `last_opened_at` advances (the in-memory value the
                // listing sorts on), the name is replaced only when the
                // request carries one, everything else is kept — and the
                // runtime is NOT rebuilt (idempotent).
                registration.last_opened_at = now;
                if let Some(name) = params.name {
                    registration.name = Some(name);
                }
                self.registry.register(registration.clone());
                // An explicit workspace-open RPC writes immediately (design
                // §2.3); any debounced touches are folded into this write.
                self.drain_pending_registry_touches();
                self.persist_workspace_registration(&registration)?;
                (
                    registration.clone(),
                    ServerEvent::WorkspaceUpdated {
                        workspace: registration_to_snapshot(&registration),
                    },
                )
            }
            None => {
                // New registration: the name defaults to the root's basename
                // (TS parity — the TS engine writes the basename as name).
                let name = params
                    .name
                    .or_else(|| basename_of(&root))
                    .or_else(|| Some(root.to_string_lossy().to_string()));
                let registration = crate::workspace_registry::WorkspaceRegistration {
                    id: workspace_id.clone(),
                    root: root.clone(),
                    name,
                    created_at: now.clone(),
                    last_opened_at: now,
                };
                self.registry.register(registration.clone());
                // The reinstating write: a fresh registration clears the id's
                // tombstone (D4 "cleared on re-register") so a removed
                // workspace is fully restored by re-adding its root.
                self.drain_pending_registry_touches();
                self.persist_workspace_reinstatement(&registration)?;
                // Register + build (the explicit-registration path: the
                // workspace is live the moment createOrTouch returns).
                self.registry
                    .register_and_build(registration.clone())
                    .map_err(|error| {
                        EngineError::Internal(format!(
                            "building the workspace runtime failed: {error}"
                        ))
                    })?;
                let event = ServerEvent::WorkspaceCreated {
                    workspace: registration_to_snapshot(&registration),
                };
                (registration, event)
            }
        };

        // Emit AFTER the persistence landed — the event must never announce
        // a registration the file does not carry.
        self.emit_server_event(event);
        to_result(self.workspace_view(&registration))
    }

    // --- workspace.update / workspace.delete (real semantics, W6) ----------

    /// `workspace.update` — rename (display name only, the TS PATCH
    /// `workspaceUpdateSchema` parity): a non-empty name on a REGISTERED
    /// workspace. The rename is written through the index row and the D8
    /// merged registry write, applied to the in-memory registration, and
    /// announced as `event.workspace.updated` after the persistence landed.
    /// An empty name is `invalid_params` (the zod `min(1)` parity — the web
    /// guards blank input client-side and never sends it); an unknown id is
    /// the W4/W5 `workspace {id} does not exist` spelling.
    pub(super) async fn dispatch_update_workspace(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct UpdateWorkspaceParams {
            #[serde(rename = "workspaceId", alias = "workspace_id")]
            workspace_id: String,
            name: String,
        }
        let params: UpdateWorkspaceParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if params.name.is_empty() {
            return Err(invalid_params("workspace name must not be empty"));
        }

        let Some(mut registration) = self.registry.registration(&params.workspace_id) else {
            return Err(invalid_params(format!(
                "workspace {} does not exist",
                params.workspace_id
            )));
        };
        registration.name = Some(params.name);
        self.registry.register(registration.clone());
        // An explicit rename writes immediately (design §2.3); debounced
        // touches are folded into this write.
        self.drain_pending_registry_touches();
        self.persist_workspace_registration(&registration)?;

        // Emit AFTER the persistence landed — the event must never announce
        // a rename the file does not carry.
        self.emit_server_event(ServerEvent::WorkspaceUpdated {
            workspace: registration_to_snapshot(&registration),
        });
        to_result(self.workspace_view(&registration))
    }

    /// `workspace.delete` — the D4 three-choice unregister:
    ///
    /// - `mode: "purge"` — close the workspace's live sessions, delete the
    ///   bucket directory (`<sessions_root>/<bucket_key>`, every alias-group
    ///   member), delete the index rows (sessions + workspaces), tombstone.
    ///   Unrecoverable by design.
    /// - `mode: "remove"` — close the live sessions, keep every byte on
    ///   disk and every index row, tombstone. Re-adding the same root
    ///   re-registers it (the reinstating write clears the tombstone) and
    ///   the sessions come back.
    ///
    /// Both modes emit `event.workspace.deleted` after the persistence
    /// landed. The **default workspace is refused** — it is pinned to this
    /// engine's process root (the serve root / the embedded host's cwd) and
    /// every degrade path addresses its runtime; deleting it would strand
    /// the pinned manager (a default re-election is a W9 candidate).
    pub(super) async fn dispatch_delete_workspace(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DeleteWorkspaceParams {
            #[serde(rename = "workspaceId", alias = "workspace_id")]
            workspace_id: String,
            /// `"purge"` | `"remove"` — required: the destructive path is
            /// never silently defaulted into.
            mode: String,
        }
        let params: DeleteWorkspaceParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let purge = match params.mode.as_str() {
            "purge" => true,
            "remove" => false,
            other => {
                return Err(invalid_params(format!(
                    "workspace.delete mode must be \"purge\" or \"remove\", got {other:?}"
                )));
            }
        };
        let workspace_id = params.workspace_id;
        if Some(workspace_id.as_str()) == self.registry.default_workspace_id().as_deref() {
            return Err(invalid_params(format!(
                "cannot delete the default workspace {workspace_id}: it is pinned to this \
                 engine's process root"
            )));
        }
        let Some(registration) = self.registry.registration(&workspace_id) else {
            return Err(invalid_params(format!(
                "workspace {workspace_id} does not exist"
            )));
        };

        // The alias group (D3) drives every per-bucket action: a legacy
        // split spelling of the same root is deleted/tombstoned with it
        // (the TS `delete()` removes by root key — the same fan-out).
        let alias_group = self.registry.alias_group_of(&workspace_id);

        // An explicit unregister writes immediately; debounced touches are
        // folded into the delete's own writes.
        self.drain_pending_registry_touches();

        // 1. Close the workspace's live sessions (a clean shutdown through
        // the very same path `session.close` takes). Only a BUILT runtime
        // holds live sessions — a never-touched registration has none, and
        // the peek must not build one.
        if let Some(runtime) = self.registry.built_runtime(&workspace_id) {
            let live_ids: Vec<String> = runtime
                .manager
                .list_sessions()
                .await
                .into_iter()
                .map(|info| info.id)
                .collect();
            for session_id in live_ids {
                let sid = SessionId::new(&session_id);
                // The close drops the live scope, so the metadata is captured
                // first: the remove mode refreshes the session's index row so
                // a later re-register lists it (the purge mode deletes the
                // rows anyway).
                let closing_metadata = match runtime.manager.get_session(&sid).await {
                    Some(session) => Some(session.metadata().await),
                    None => None,
                };
                runtime
                    .manager
                    .close_session(&sid, CloseReason::UserInitiated)
                    .await
                    .map_err(session_error)?;
                self.forget_active_session(&session_id);
                self.dispose_activity_trackers(&session_id);
                self.dispose_turn_flows(&session_id);
                self.clear_session_overlay(&session_id).await;
                if !purge && let Some(metadata) = closing_metadata {
                    self.index_upsert_live(&sid, &metadata);
                }
            }
        }

        let mut deleted_sessions = 0usize;
        if purge {
            // Retention attachment reclamation (SDR-004, Task 9): the
            // bucket's blob references come off the shared ledger BEFORE
            // the wholesale directory removal (the decrement extracts
            // them from the record streams, which must still exist).
            // Per session directory of every alias member's bucket.
            if let Some(sessions_root) = self.workspace_sessions_root() {
                for member in &alias_group {
                    let bucket = sessions_root.join(member);
                    let Ok(entries) = std::fs::read_dir(&bucket) else {
                        continue; // no bucket yet — nothing to decrement
                    };
                    for entry in entries.flatten() {
                        if entry.path().is_dir() {
                            self.blob_ledger_decrement_dir(&entry.path()).await;
                        }
                    }
                }
            }
            // 2. Delete the bucket directories (every alias member's
            // `<sessions_root>/<bucket_key>`). Best-effort per member: a
            // missing directory is fine, a failure on one member logs and
            // the delete proceeds (the registry/index steps below are the
            // authoritative removal).
            if let Some(sessions_root) = self.workspace_sessions_root() {
                for member in &alias_group {
                    let bucket = sessions_root.join(member);
                    match std::fs::remove_dir_all(&bucket) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            tracing::warn!(
                                bucket = %bucket.display(),
                                %error,
                                "purging the workspace bucket directory failed"
                            );
                        }
                    }
                }
            }
            // 3. Delete the index rows (sessions + workspaces) for every
            // alias member.
            if let Some(index) = self.session_index() {
                deleted_sessions =
                    index
                        .delete_sessions_for_workspaces(&alias_group)
                        .map_err(|error| {
                            EngineError::Internal(format!(
                                "deleting the workspace's index rows failed: {error}"
                            ))
                        })?;
                for member in &alias_group {
                    index.delete_workspace(member).map_err(|error| {
                        EngineError::Internal(format!(
                            "deleting the workspace index row failed: {error}"
                        ))
                    })?;
                }
            }
        } else {
            // Remove mode: the data is preserved — bucket directories and
            // index rows stay exactly as they are (a re-registration
            // restores the listing).
            deleted_sessions = 0;
        }

        // 4. Drop the in-memory registration + runtime.
        self.registry.remove(&workspace_id);
        // And the workspace's skill-directory watcher with it (plan Task
        // 5: the handle drop signals the watcher task's shutdown channel,
        // so the removal reclaims the task — no orphaned watcher keeps
        // rescanning a removed workspace).
        self.unmount_skill_watcher(&workspace_id);

        // 5. Tombstone every alias member (the D8 lock path) — a concurrent
        // TS writer must not resurrect the row.
        if let Some(registry_path) = self.workspace_registry_path() {
            for member in &alias_group {
                mirri_session_index::add_tombstone(&registry_path, member).map_err(|error| {
                    EngineError::Internal(format!("tombstoning workspace {member} failed: {error}"))
                })?;
            }
        }

        // Emit AFTER the persistence landed — the event must never announce
        // a deletion the registry file does not carry.
        self.emit_server_event(ServerEvent::WorkspaceDeleted {
            workspace_id: workspace_id.clone(),
            root: registration.root.to_string_lossy().into_owned(),
        });
        to_result(WorkspaceDeletedResult {
            ok: true,
            mode: if purge { "purge" } else { "remove" }.to_string(),
            workspace_id,
            deleted_sessions,
        })
    }

    /// The sessions persistence root shared by every assembled runtime
    /// (`<home>/v3_sessions` — `bootstrap`'s `build_workspace_runtime`
    /// roots ALL runtimes at the same home dir, bucketed per workspace).
    /// The default manager's root IS the shared root, so no per-workspace
    /// build is needed. `None` when no home is injected (the
    /// direct-constructor engines: their persistence is workspace-local
    /// and a purge has no shared bucket to remove).
    fn workspace_sessions_root(&self) -> Option<PathBuf> {
        // No injected home (`?` yields None): the bucket derivation assumes
        // the shared home-rooted layout (`<root>/<bucket_key>`); a
        // workspace-local root (the direct-constructor engines) never had
        // one.
        self.home_dir.as_ref()?;
        Some(self.manager.persistence().sessions_dir().to_path_buf())
    }

    // --- workspace.get / workspace.handlerFor -------------------------------

    /// `workspace.get` — resolve a workspace by id to its `WorkspaceView`,
    /// or JSON `null` when the id is unknown (the klient `maybe` semantics:
    /// a miss is not an error).
    pub(super) async fn dispatch_get_workspace(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct GetWorkspaceParams {
            #[serde(rename = "workspaceId", alias = "workspace_id")]
            workspace_id: String,
        }
        let params: GetWorkspaceParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        match self.registry.registration(&params.workspace_id) {
            Some(registration) => to_result(self.workspace_view(&registration)),
            None => to_result(Value::Null),
        }
    }

    /// `workspace.handlerFor` — resolve a workspace reference (a `{root}` or
    /// `{id}` / `{workspaceId}` shape, parsed tolerantly) to the
    /// scope-handle wire `{id, kind}` (the klient `handleWireSchema` loose
    /// shape), or JSON `null` when the reference names no registered
    /// workspace (the klient `maybe` semantics). Root references resolve
    /// through the same lexical normalization + key derivation as
    /// `createOrTouch`.
    pub(super) async fn dispatch_workspace_handler_for(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WorkspaceRefParams {
            // The reference's id spelling (klient sends `workspaceId`; the
            // domain shape also accepts `id`).
            #[serde(default, alias = "workspaceId", alias = "workspace_id")]
            id: Option<String>,
            #[serde(default)]
            root: Option<String>,
        }
        let params: WorkspaceRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if params.id.is_none() && params.root.is_none() {
            return Err(invalid_params(
                "workspace.handlerFor needs a workspace reference: {root} or {id}",
            ));
        }

        let workspace_id = match (&params.id, &params.root) {
            (Some(id), _) => id.clone(),
            (None, Some(root)) => encode_work_dir_key(&lexically_resolve(Path::new(root))),
            (None, None) => unreachable!("validated above"),
        };
        match self.registry.registration(&workspace_id) {
            Some(registration) => to_result(WorkspaceHandleResult {
                id: registration.id,
                kind: WORKSPACE_SCOPE_KIND,
            }),
            None => to_result(Value::Null),
        }
    }

    // --- workspace.setTrust (W8) ---------------------------------------------

    /// `workspace.setTrust` — the trust management face (W8, US4.1/US4.2,
    /// the SDR-007 entry's write side): `{workspaceId | workspace_id,
    /// trusted}` → `{trusted}` (the TS v2 REST trust face's
    /// `workspaceTrustResponseSchema`). The write lands in the home-level
    /// trust store (D6: survives unregister/re-register), then the
    /// in-process gate flips live (the TS v2 same-process hot-reload
    /// parity — [`Self::apply_workspace_trust`]) so the effect is
    /// immediate without a runtime rebuild:
    ///
    /// - `trusted: true` — the workspace's project MCP layers connect
    ///   (the reconcile kernel's next pass reads the root's layers),
    ///   and the AGENTS.md project section rides for sessions/agents
    ///   created from now on (plus a best-effort refresh of every live
    ///   agent's system prompt).
    /// - `trusted: false` — the workspace's project-layer connections
    ///   disconnect NOW (the kernel's converge-to-empty arm), and the
    ///   AGENTS.md project section is excluded from everything that
    ///   loads next (live agents' prompts refresh stripped).
    ///
    /// The **default workspace cannot be untrusted** — it is pinned to
    /// this engine's process root and trusted by construction (the
    /// `workspace.delete` refusal's mirror); trusting it is an idempotent
    /// no-op success. A no-store engine (no injected home) refuses:
    /// trust has nowhere to persist, and the D6 gate is store-driven.
    pub(super) async fn dispatch_set_workspace_trust(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SetWorkspaceTrustParams {
            #[serde(rename = "workspaceId", alias = "workspace_id")]
            workspace_id: String,
            trusted: bool,
        }
        let params: SetWorkspaceTrustParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;

        let Some(registration) = self.registry.registration(&params.workspace_id) else {
            return Err(invalid_params(format!(
                "workspace {} does not exist",
                params.workspace_id
            )));
        };
        let workspace_id = registration.id.clone();
        if Some(workspace_id.as_str()) == self.registry.default_workspace_id().as_deref() {
            if !params.trusted {
                return Err(invalid_params(format!(
                    "cannot untrust the default workspace {workspace_id}: it is pinned to this \
                     engine's process root"
                )));
            }
            // Trusting the default is the state it is already in — an
            // idempotent no-op (the store is not consulted for the pin
            // root, so nothing is written either).
            return to_result(WorkspaceTrustResult { trusted: true });
        }
        let Some(store) = &self.trust_store else {
            return Err(invalid_params(
                "workspace trust cannot be persisted without a Mirri home",
            ));
        };
        if params.trusted == self.workspace_is_trusted(&workspace_id) {
            // Already in the requested state: the live gates agree, the
            // write would be an idempotent no-op — answer without touching
            // the file or running a reconcile.
            return to_result(WorkspaceTrustResult {
                trusted: params.trusted,
            });
        }

        // 1. Persist FIRST — the flip must never be announced for a grant
        // the store does not carry (the same persistence-before-effect
        // rule the lifecycle events follow).
        store
            .set_trusted(&workspace_id, params.trusted)
            .map_err(|error| {
                EngineError::Internal(format!("writing the trust store failed: {error}"))
            })?;

        // 2. Flip every live gate + converge the pools.
        self.apply_workspace_trust(&workspace_id, params.trusted)
            .await;

        to_result(WorkspaceTrustResult {
            trusted: params.trusted,
        })
    }

    /// The trust-effect linkage (W8): flip the workspace's live gates and
    /// converge its pools for the new verdict — the fine-grained refresh
    /// that replaces a runtime rebuild (a rebuild would strand the
    /// workspace's live sessions: the routing seam resolves the active
    /// ids through the registry's built map, whose replacement manager
    /// holds none of them). An **unbuilt** runtime needs nothing here —
    /// the next lazy `ensure_built` consults the (already-updated) store.
    ///
    /// For a built runtime:
    ///
    /// - the runtime's verdict atom flips (the kernel, the shadow view and
    ///   the lazy-connect guard read it live);
    /// - the scope's project-AGENTS.md gate flips (every session created
    ///   from now on carries the new verdict);
    /// - every LIVE session's own gate flips and its agents' system
    ///   prompts re-derive under the new gate (best-effort — agents
    ///   created outside session scope are skipped); sessions keep
    ///   running, nothing is closed;
    /// - one reconcile kernel pass converges the pools: trusted ⇒ the
    ///   root's project layers connect into the runtime's existing pool;
    ///   untrusted ⇒ the pool's non-plugin servers disconnect (the
    ///   converge-to-empty arm, US4.1's 即刻断开) and the user-pool shadow
    ///   view drops the root's names.
    ///
    /// The runtime's `pending_mcp_configs` deliberately stays stale after
    /// the flip: nothing reads it once the one-shot lazy-connect guard is
    /// spent, and the kernel re-derives the desired set from disk.
    async fn apply_workspace_trust(&self, workspace_id: &str, trusted: bool) {
        if let Some(runtime) = self.registry.built_runtime(workspace_id) {
            runtime.set_trusted(trusted);
            runtime.scope.set_load_project_agents_md(trusted);
            // The live-session sweep: flip each session's own gate and
            // re-derive its agents' prompts under it. Best-effort per
            // session (a session vanishing mid-sweep is fine — the sweep
            // only touches what it still finds).
            let live_ids: Vec<String> = runtime
                .manager
                .list_sessions()
                .await
                .into_iter()
                .map(|info| info.id)
                .collect();
            for session_id in live_ids {
                let sid = SessionId::new(&session_id);
                if let Some(session) = runtime.manager.get_session(&sid).await {
                    session.set_load_project_agents_md(trusted);
                    session.refresh_all_agent_system_prompts().await;
                }
            }
        }

        // The single converge pass (the `mcp.sync` kernel): connects on
        // trust, disconnects on untrust, and refreshes the user-pool
        // shadow view either way.
        self.reconcile_mcp_pools().await;
    }
}

// --- workspace.createOrTouch params ---------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorkspaceParams {
    /// The root to register — required (a missing root is invalid params).
    root: String,
    #[serde(default)]
    name: Option<String>,
}

// --- small helpers ----------------------------------------------------------

/// The root's basename — the TS `name ?? basename(root)` fallback.
fn basename_of(root: &Path) -> Option<String> {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
}

/// Convert a registration to the index/registry row shape (timestamps
/// parsed back to `DateTime` — the registration carries them as RFC 3339
/// strings; an unparseable string falls back to now, the registry's `parseTime`
/// tolerance).
fn registration_to_row(
    registration: &crate::workspace_registry::WorkspaceRegistration,
) -> WorkspaceRow {
    WorkspaceRow {
        id: registration.id.clone(),
        root: Some(registration.root.to_string_lossy().to_string()),
        name: registration.name.clone(),
        created_at: parse_rfc3339(&registration.created_at),
        last_opened_at: parse_rfc3339(&registration.last_opened_at),
        sort_order: 0,
    }
}

/// The server-event snapshot for a registration (the event payload carries
/// the identity snapshot — id/root/name — not the full view).
fn registration_to_snapshot(
    registration: &crate::workspace_registry::WorkspaceRegistration,
) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        id: registration.id.clone(),
        root: registration.root.to_string_lossy().to_string(),
        name: registration.name.clone(),
    }
}

/// Parse an RFC 3339 timestamp with the registry's tolerance: an
/// unparseable string falls back to now.
fn parse_rfc3339(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
