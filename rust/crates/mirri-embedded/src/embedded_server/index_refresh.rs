//! The session-consistency index refresh driver (batch 1, US C1.1): one
//! spawned task per **server** (not per session — the listing is a
//! cross-session view), behind the
//! [`mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG`] experimental flag.
//!
//! Each tick runs the re-runnable index converge
//! ([`reconcile_session_index_via`](super::session_index::reconcile_session_index_via))
//! — the same pass the first `call_method` performs once — so a foreign
//! entry's session mutation on the shared home (a rename's `state.json`
//! rewrite moves the file's mtime) rebuilds the stale row from the fresh
//! disk read, and this engine's next `session.list` answers the new title
//! inside the C1.1 ten-second freshness budget.
//!
//! Concurrency posture (ADR-004, batch 2): each tick's pass takes the
//! `<home>/session-index.reconcile.lock` file lock (the same create_new +
//! stale-takeover + bounded-wait pattern the workspace registry writes
//! use), so two processes ticking simultaneously serialize — one pass
//! runs after the other, never interleaved. A lock held past the wait
//! budget skips that round's converge (the degrade ADR-004 prescribes:
//! the reconcile is a fallback converger, the next tick retries).
//!
//! The task captures only `Arc` clones (the workspace registry, the
//! sessions root), never a borrow of the server, and runs for the process
//! lifetime — the listing freshness must survive every session closing.
//! Failures inside the converge are best-effort by design (each row
//! upsert logs and continues); the driver itself never exits on a
//! failure — the next tick retries.

use std::sync::Arc;

use super::EmbeddedServer;
use super::session_index::reconcile_session_index_via;

impl EmbeddedServer {
    /// Lazily spawn the per-server index refresh driver on the first
    /// `call_method` invocation — the
    /// [`Self::maybe_spawn_models_catalog_preheat`] pattern:
    /// construction may precede the Tokio runtime, while the first RPC
    /// call is guaranteed to run inside one. Guarded by an atomic so
    /// exactly one driver runs per server; gated behind
    /// [`mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG`] (off by default —
    /// a disabled flag spawns nothing, keeping the pre-batch read path
    /// byte-identical).
    pub(super) fn maybe_spawn_index_refresh_driver(&self) {
        use std::sync::atomic::Ordering;

        if !self
            .flag_registry()
            .enabled(mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG)
        {
            return;
        }
        if self.index_refresh_spawned.swap(true, Ordering::SeqCst) {
            return;
        }
        self.spawn_index_refresh_driver();
    }

    /// The driver body: one Tokio task ticking the index converge every
    /// [`Self::with_index_refresh_tick`] interval (the production default
    /// is [`INDEX_REFRESH_TICK_SECS`](super::INDEX_REFRESH_TICK_SECS)).
    ///
    /// The first tick of `tokio::time::interval` completes immediately:
    /// it re-runs the converge the just-finished first `call_method`
    /// already performed (the once-guarded
    /// [`Self::reconcile_session_index_once`]) — an mtime-equal pass that
    /// rebuilds nothing, so the immediate tick is a no-op probe, not a
    /// double converge.
    ///
    /// The sessions root is derived from the registry's default manager's
    /// persistence (`SessionPersistence::sessions_dir` — every registered
    /// workspace's bucket lives under the same root), captured ONCE at
    /// spawn: the root is a constructor-level fact (`with_persistence_dir`
    /// / the composition root), not a per-call one. An engine with no
    /// shared index connection (no injected home) keeps the task but
    /// every tick no-ops inside the shared free function — the disk scan
    /// remains the listing source, the same degrade the in-server
    /// reconcile has.
    pub(super) fn spawn_index_refresh_driver(&self) {
        // The registry seam (W3): the converge reads every registered
        // workspace's bucket through it, never a captured manager.
        let registry = Arc::clone(&self.registry);
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        let tick = self.index_refresh_tick;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                // The shared index connection (the registry's
                // home-installed lazy open) — resolved per tick, not
                // captured, so an engine whose home lands late (a host
                // attaching it after the first RPC) still converges from
                // the next tick on. `None` (no home installed) is the
                // documented no-op degrade.
                if let Some(index) = registry.shared_session_index() {
                    reconcile_session_index_via(&registry, &sessions_root, &index).await;
                }
            }
        });
    }
}
