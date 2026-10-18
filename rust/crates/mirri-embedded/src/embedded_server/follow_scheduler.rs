//! The session-consistency foreground follow driver (batch 1, spec §5):
//! one spawned task per resumed live session, behind the
//! [`mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG`] experimental flag.
//!
//! Each tick re-resolves the session through the workspace registry (the
//! cron/task driver posture — no manager is captured), and when the
//! journal diverged from the session's known-size anchor, absorbs the
//! foreign appends incrementally via
//! [`SessionManager::absorb_session_foreign_updates`]. Three hard rules
//! from the Task 4 review are load-bearing here:
//!
//! 1. **Invariant 3 (ADR-005) is a hard precondition**: the anchor counts
//!    only this process's OWN bytes. Absorbing while a local turn is
//!    in flight would interleave a foreign write between two of our own
//!    writes — a torn line plus the anchor folding foreign bytes. The
//!    guard is the engine-wide `in_flight_turns` registry (NOT the
//!    in-flight shadow, which a foreign `TurnCompleted` can falsely
//!    clear): a session with ANY in-flight turn is skipped this tick,
//!    never absorbed.
//! 2. **`Truncated` is a stop signal**: `absorb_session_foreign_updates`
//!    handles an externally truncated journal by zeroing the anchor.
//!    Draining again would re-read the whole rebuilt journal from offset
//!    0 and fold the rewritten history onto the stale in-memory one, so
//!    the driver exits on the truncation flag and waits for the session
//!    to be fully resumed again.
//! 3. The driver holds no registry entry (same as the cron driver): it
//!    exits by itself when the session disappears or is truncated.
//!
//! The task handle is tracked in [`EmbeddedServer`]'s follow-drivers map
//! (the relays map's shape): a re-resume aborts the previous driver before
//! spawning the next one, so a close→resume race never stacks two drivers
//! on one session.

use std::sync::Arc;

use super::EmbeddedServer;
use super::session_routing::{resolve_manager_via, resolve_session_via};
use crate::workspace_registry::WorkspaceRegistry;

impl EmbeddedServer {
    /// Start the foreground follow driver for one live session. The driver
    /// ticks every [`Self::with_follow_tick`] interval and, when the
    /// session's journal diverged from the anchor, absorbs the foreign
    /// appends — but ONLY while this process has no in-flight turn for the
    /// session (ADR-005 invariant 3). Exits when the session disappears
    /// (close/archive) or the journal was externally truncated.
    ///
    /// Gated behind `mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG` (off by
    /// default): a disabled flag returns without spawning anything.
    ///
    /// A still-running driver for the SAME session is aborted first (the
    /// relay drain's pattern, adapted): the close→resume race can leave
    /// the old driver's next tick resolving the just-resumed session as
    /// live — it would never exit on its own — so the spawn path aborts
    /// it instead of awaiting it. An abort mid-absorb is safe: the anchor
    /// advances only at a drain's end, so the aborted pass folds nothing
    /// permanently and the successor re-absorbs from the same anchor.
    pub(super) fn spawn_follow_driver(&self, session_id: &str) {
        if !self
            .flag_registry()
            .enabled(mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG)
        {
            return;
        }
        // Abort the previous driver, if any (see the method doc: abort,
        // not await — the close→resume race would leave a live-resolution
        // path for the old driver and an await would hang the successor).
        if let Some(prev) = self
            .follow_drivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
        {
            prev.abort();
        }
        // The routing seam (W3): hold the workspace registry, not one
        // manager — every tick re-resolves the session through it.
        let registry: Arc<WorkspaceRegistry> = Arc::clone(&self.registry);
        // Invariant 3's guard: the engine-wide in-flight turn registry.
        // Deliberately NOT the session's in-flight shadow — a foreign
        // TurnCompleted clears the shadow, which would fool this check
        // while a local turn still runs.
        let in_flight_turns = Arc::clone(&self.in_flight_turns);
        let tick = self.follow_tick;
        let sid = session_id.to_string();
        // The key prefix this session's in-flight turns live under
        // (`"{session_id}:{agent_id}:{turn_id}"`, see the KEY FORMAT
        // CONTRACT at `EmbeddedServer::in_flight_turns` — the session id is
        // a UUID so `"{session_id}:"` matches exactly this session's keys)
        // — built once, not once per probed key.
        let in_flight_prefix = format!("{sid}:");
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            // The first tick completes immediately — an anchor-fresh
            // session is Fresh there, so no work is done before the first
            // real interval elapses.
            loop {
                interval.tick().await;

                // An in-flight local turn means this process is writing
                // the journal RIGHT NOW: absorbing a foreign append
                // interleaved with our own writes would tear a line and
                // fold foreign bytes into the anchor (ADR-005 invariant
                // 3). Skip this tick — the next idle tick absorbs.
                //
                // TOCTOU window: the check-then-act is inherently racy — a
                // turn may register AFTER this guard passes and before its
                // first wire op lands on disk, so a drain can still start
                // while a local turn is about to write. The damage is
                // bounded by the design: a torn line merely suspends the
                // incremental absorb (no error, no exit, no data loss) and
                // the known-size anchor is a floor, never an exact offset,
                // so a foreign byte folded into our own count self-heals on
                // the next diverging stat. Batch 2's write-side commit
                // fence (single-writer serialization) closes this window
                // completely.
                if in_flight_turns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .any(|key| key.starts_with(&in_flight_prefix))
                {
                    continue;
                }

                // Re-resolve through the registry each tick (the cron
                // driver's posture): a closed/archived session is the
                // driver's exit.
                let Ok(session) = resolve_session_via(&registry, &sid).await else {
                    tracing::debug!(
                        session = %sid,
                        "session consistency: follow driver exiting (session gone)"
                    );
                    break;
                };

                // Truncated is a STOP signal, not a data point: the
                // anchor was zeroed by the truncation handling, so
                // continuing to drain would re-read the whole rebuilt
                // journal from offset 0. Exit and wait for a full resume
                // (which re-establishes the anchor and spawns a fresh
                // driver).
                if session.wire_log_truncated() {
                    tracing::warn!(
                        session = %sid,
                        "session consistency: journal truncated externally; follow driver exiting \
                         (a full session.resume re-establishes the anchor)"
                    );
                    break;
                }

                // The owning manager is resolved the same way the session
                // was — the same registry seam, never a captured manager.
                let Ok((manager, _workspace_id)) = resolve_manager_via(&registry, &sid) else {
                    tracing::debug!(
                        session = %sid,
                        "session consistency: follow driver exiting (owning manager unresolvable)"
                    );
                    break;
                };

                if let Err(error) = manager.absorb_session_foreign_updates(&session).await {
                    tracing::warn!(
                        session = %sid,
                        %error,
                        "session consistency: absorb failed; retrying next tick"
                    );
                }
            }
        });
        // Track the handle so a later resume can abort this driver before
        // spawning its successor (and tests can observe the exit).
        self.follow_drivers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), handle);
    }
}
