//! Interrupted-task recovery service (spec §3) — the in-process interruption
//! list plus the one-shot startup check that fills it.
//!
//! Two entry families, both owned by [`EmbeddedServer`]:
//!
//! - **`run_startup_recovery_check`** runs once per process, on the first
//!   dispatched RPC (the same lazy seam `migrate_legacy_sessions_once` and
//!   `run_startup_orphan_governance` use — construction may precede the
//!   Tokio runtime, the first `call_method` never does). It scans the
//!   lifecycle sentinel (§1.2), reads the graceful records straight (D-M2),
//!   infers the abnormal exits' interrupted sessions from disk (§1.3),
//!   backfills the synthetic `interrupted` closure (§2.1, ADR-008
//!   read-judge-write idempotence: the tail is re-scanned immediately
//!   before each append), consumes the judged instance directories, and
//!   writes this instance's own launch marker.
//! - **`dispatch_recovery_*`** serve the `recovery.*` RPC surface over the
//!   in-process list. Task 6 lands the full five-method family: `list` /
//!   `dismiss` / `dismiss_all` and the resume pair (E.2's ensure-resident
//!   + recovery-origin fire turn, C.4's bulk arm).
//!
//! The badge read-side override (§2.2's OR-merge) lives in
//! `session_handlers.rs`'s list build, consulting
//! [`RecoveryState::last_turn_reason_override`].

use std::sync::Arc;

use mirri_core::SessionManager;
use mirri_core::session::recovery_scan::{
    append_interrupted_closure, scan_interrupted_closure, scan_open_turn, session_is_archived,
};
use mirri_persistence::lifecycle::{self, AbnormalExit, LaunchRecord, RunningSessionEntry};
use mirri_transport::adapter::EngineError;
use serde::Serialize;
use serde_json::{Value, json};

use super::{EmbeddedServer, invalid_params, not_found};

/// The interruption retention window (spec §3 / ADR-006): an interruption
/// drops out of `recovery.list` once its `interrupted_at` is older than
/// this — judged lazily at read time, never by a background timer.
const RETENTION_WINDOW_MS: u64 = 72 * 60 * 60 * 1000;

/// One interruption entry of the in-process recovery list.
#[derive(Debug, Clone)]
pub(super) struct InterruptionItem {
    /// The interrupted session's id.
    pub session_id: String,
    /// The workspace key (the registry bucket id) the session belongs to.
    pub workspace_key: String,
    /// The session's display title at interruption time.
    pub title: String,
    /// Epoch-ms of the interruption (the dangling prompt's time on the
    /// abnormal path, the exit snapshot's timestamp on the graceful path).
    pub interrupted_at: u64,
    /// `"service_exit"` (abnormal) or `"graceful_exit"`.
    pub cause: String,
    /// A pending background task was inside the dangling turn's bracket —
    /// the resume prompt's check-before-rerun instruction input (E.3).
    pub pending_background_hint: Option<String>,
    /// Whether the user settled the entry (resume or dismiss) — settled
    /// entries leave both the list and the badge override.
    pub settled: bool,
}

/// The in-process recovery list (spec §3's RecoveryService state) plus the
/// test-injection seams.
pub(super) struct RecoveryState {
    /// The interruption list. Read on every list/badge read; written once at
    /// startup inference and on resume/dismiss settling.
    interruptions: std::sync::RwLock<Vec<InterruptionItem>>,
    /// The judge-time clock override (tests move it past the retention
    /// boundary). `None` = the real wall clock.
    #[cfg(test)]
    now_override: std::sync::Mutex<Option<u64>>,
}

// The Task-5 seam is closed: the dispatch faces below are registered in
// the route table (Task 6), so no dead-code allowance is needed here.
impl RecoveryState {
    /// The empty default — every server starts with no known interruptions
    /// until the startup check fills the list.
    pub(super) fn new() -> Self {
        Self {
            interruptions: std::sync::RwLock::new(Vec::new()),
            #[cfg(test)]
            now_override: std::sync::Mutex::new(None),
        }
    }

    /// The judge clock: the test override when installed, else the real
    /// wall clock (epoch-ms, floored at 0).
    fn now_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(overridden) = *self.now_override.lock().unwrap_or_else(|e| e.into_inner()) {
            return overridden;
        }
        chrono::Utc::now().timestamp_millis().max(0) as u64
    }

    #[cfg(test)]
    /// Move the judge clock (tests age an interruption past the retention
    /// boundary without waiting).
    pub(super) fn set_now_override(&self, now_ms: Option<u64>) {
        *self.now_override.lock().unwrap_or_else(|e| e.into_inner()) = now_ms;
    }

    /// Replace the whole list (the startup inference's single write).
    pub(super) fn set_interruptions(&self, items: Vec<InterruptionItem>) {
        *self
            .interruptions
            .write()
            .unwrap_or_else(|e| e.into_inner()) = items;
    }

    /// The unsettled, unexpired entries (the `recovery.list` body).
    fn live_interruptions(&self) -> Vec<InterruptionItem> {
        let now = self.now_ms();
        self.interruptions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|item| !item.settled && item.interrupted_at + RETENTION_WINDOW_MS > now)
            .cloned()
            .collect()
    }

    /// The badge override for one session row: `Some("interrupted")` while
    /// the in-process list carries a live entry for it, `None` otherwise
    /// (§2.1 interception point ① — the row's own projection stays
    /// untouched; only the live in-process arm of the OR-merge writes).
    ///
    /// The bare list predicate (no liveness query). The resident-session
    /// F-06 settling (a turn newer than the interruption) is applied by
    /// [`EmbeddedServer::settle_superseded_interruptions`] before each
    /// list/badge read, so the two arms never disagree.
    pub(super) fn last_turn_reason_override(&self, session_id: &str) -> Option<String> {
        if self.is_live(session_id) {
            Some("interrupted".to_string())
        } else {
            None
        }
    }

    /// Whether the interruption list holds an unsettled, unexpired entry
    /// for `session_id` (spec §2.2's in-process arm — the read-time
    /// badge override's predicate).
    fn is_live(&self, session_id: &str) -> bool {
        let now = self.now_ms();
        self.interruptions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|item| {
                !item.settled
                    && item.session_id == session_id
                    && item.interrupted_at + RETENTION_WINDOW_MS > now
            })
    }

    /// Settle the entry for `session_id` (dismiss — and resume, once Task 6
    /// lands it). Idempotent: an already-settled or unknown id is a no-op.
    fn settle(&self, session_id: &str) {
        let mut items = self
            .interruptions
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items
            .iter_mut()
            .find(|item| item.session_id == session_id && !item.settled)
        {
            item.settled = true;
        }
    }

    /// Take the live (unsettled, unexpired) entry for `session_id` out and
    /// mark it settled — the resume path's claim (the entry leaves the
    /// list before the turn launches, so a concurrent list read never sees
    /// a settled-but-listed row). `None` when no live entry exists.
    fn unsettle_take(&self, session_id: &str) -> Option<InterruptionItem> {
        let now = self.now_ms();
        let mut items = self
            .interruptions
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let item = items.iter_mut().find(|item| {
            !item.settled
                && item.session_id == session_id
                && item.interrupted_at + RETENTION_WINDOW_MS > now
        })?;
        item.settled = true;
        Some(item.clone())
    }

    /// Restore an `unsettle_take`n item as unsettled (F-05): the resume
    /// path's turn launch failed, so the entry goes back to live — the
    /// badge survives and a later resume/dismiss retries. Idempotent and
    /// honest: the item is pushed back only when no other live entry for
    /// the same session exists (a duplicate could never win a race here —
    /// the take removed the only one).
    fn restore_unsettled(&self, item: InterruptionItem) {
        let mut items = self
            .interruptions
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if !items
            .iter()
            .any(|existing| existing.session_id == item.session_id && !existing.settled)
        {
            items.push(item);
        }
    }

    /// The session ids of every unsettled (and possibly expired — the
    /// caller filters) entry (F-06's candidate set: the resident-session
    /// supersede check walks exactly these).
    fn unsettled_session_ids(&self) -> Vec<String> {
        self.interruptions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|item| !item.settled)
            .map(|item| item.session_id.clone())
            .collect()
    }

    /// Settle every entry (`dismiss_all`).
    fn settle_all(&self) {
        for item in self
            .interruptions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            item.settled = true;
        }
    }
}

/// `recovery.list`'s row (the klient contract's camelCase shape).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InterruptionRow {
    session_id: String,
    workspace_key: String,
    workspace_name: String,
    title: String,
    interrupted_at: u64,
    cause: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_background_hint: Option<String>,
}

// The Task-5 seam is closed (same as the RecoveryState block above): the
// dispatch faces are registered in the route table.
impl EmbeddedServer {
    /// The one-shot startup check (spec §1.2): sentinel scan → liveness
    /// gate → graceful read / abnormal inference → closure backfill →
    /// consume → own launch marker.
    ///
    /// A no-op without a lifecycle root (tests and the non-serve hosts —
    /// the same `Option` gate Task 4's graceful write uses). The
    /// `recovery_settled` OnceCell guards exactly one run per process;
    /// concurrent first dispatchers await the same settlement (the
    /// `run_startup_orphan_governance` pattern).
    pub(super) async fn run_startup_recovery_check(&self) {
        self.recovery_settled
            .get_or_init(|| async {
                self.run_startup_recovery_check_once().await;
            })
            .await;
    }

    /// The check's body — see [`Self::run_startup_recovery_check`].
    async fn run_startup_recovery_check_once(&self) {
        let Some((root, port, instance)) = self.lifecycle_root.clone() else {
            return;
        };
        let now = self.recovery.now_ms();
        #[cfg(test)]
        let probe_override = self
            .recovery_probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        #[cfg(not(test))]
        let probe_override: Option<Box<dyn Fn(u16, u32) -> bool + Send + Sync>> = None;
        let probe: Box<dyn Fn(u16, u32) -> bool + Send + Sync> = match probe_override {
            Some(probe) => probe,
            // The production liveness gate (spec §1.2): a TCP connect to
            // the recorded port AND the recorded pid both being alive
            // means the instance still owns it. Same-port instances
            // never reach the probe (the sentinel rules them
            // structurally dead). The pid arm is the F-04 assist: a
            // reachable port with a dead recorded pid reads as a dead
            // instance — the port was re-bound by an unrelated process,
            // or the connect is served by a stale listener.
            None => {
                Box::new(|target_port, pid| probe_port_alive(target_port) && probe_pid_alive(pid))
            }
        };
        let verdict = lifecycle::scan_and_judge(&root, port, now, &probe).await;

        let mut items: Vec<InterruptionItem> = Vec::new();
        // The graceful arm (D-M2): the record IS the truth — no disk
        // inference, entries read straight. Every judged directory that
        // carries a readable graceful record contributes its entries.
        for dir in &verdict.consumed {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(record) = lifecycle::read_graceful(&root, name).await {
                for entry in &record.running_sessions {
                    items.push(graceful_entry(entry, &record));
                }
            }
        }

        // The abnormal arm (§1.3): infer from disk across every
        // REGISTERED workspace's persistence bucket (F-02 — only the
        // default workspace is eagerly built at boot, so
        // `built_runtimes` alone would miss interrupted sessions of
        // non-default workspaces; every registered row is built here).
        for abnormal in &verdict.abnormal {
            items.extend(self.infer_abnormal_interruptions(abnormal).await);
        }

        // Settle the in-process list BEFORE the backfill writes, so a
        // concurrent first list read never sees the badge flash off
        // between the inference and the closure landing (§2.2's
        // no-flicker merge order).
        if !items.is_empty() {
            self.recovery.set_interruptions(items);
        }

        // Backfill the synthetic closures (§2.1) — the read-time
        // derivation means the badge survives even when a backfill write
        // fails; a failure logs and moves on (F.4 tolerance).
        for abnormal in &verdict.abnormal {
            self.backfill_abnormal_closures(abnormal).await;
        }

        // Consume the judged directories (the atomic rename that makes a
        // racing double startup single-winner), then write this
        // instance's own launch marker under the SAME instance id the
        // assembly minted (F-01 — the shutdown-side graceful write lands
        // in the same directory).
        if let Err(error) = lifecycle::consume(&root, &verdict.consumed).await {
            tracing::warn!(%error, "failed to consume judged lifecycle directories");
        }
        let launch = LaunchRecord {
            instance: instance.clone(),
            started_at: chrono::Utc::now().timestamp_millis().max(0) as u64,
            port,
            pid: std::process::id(),
        };
        if let Err(error) = lifecycle::write_launch(&root, &launch).await {
            tracing::warn!(%error, %instance, "failed to write the lifecycle launch marker");
        }
    }

    /// The registered workspaces' (workspace key, manager) pairs, building
    /// each runtime on demand (F-02): the inference and backfill must see
    /// EVERY registered workspace's persisted sessions, not just the ones
    /// already built at boot (only the default workspace is eagerly
    /// built). A build failure logs and skips that workspace — a workspace
    /// whose runtime cannot be built contributes no sessions this boot
    /// (the same best-effort tolerance the other disk arms carry).
    fn registered_workspace_managers(&self) -> Vec<(String, Arc<SessionManager>)> {
        let mut pairs = Vec::new();
        for registration in self.registry.registrations() {
            match self.registry.ensure_built(&registration.id) {
                Some(Ok(runtime)) => {
                    pairs.push((runtime.workspace_id.clone(), Arc::clone(&runtime.manager)));
                }
                Some(Err(error)) => {
                    tracing::warn!(
                        workspace_id = %registration.id,
                        %error,
                        "failed to build the workspace runtime for interruption inference"
                    );
                }
                None => {}
            }
        }
        pairs
    }

    /// The abnormal arm's disk inference (spec §1.3): every session of
    /// every REGISTERED workspace (built on demand — F-02) whose wire log
    /// ends in a turn left open by the dead instance. Read-only — the
    /// backfill writes happen afterwards, keyed by the same scan results
    /// re-taken there (ADR-008).
    async fn infer_abnormal_interruptions(&self, abnormal: &AbnormalExit) -> Vec<InterruptionItem> {
        let mut items = Vec::new();
        for (workspace_key, manager) in self.registered_workspace_managers() {
            for info in manager.list_all_sessions(true).await {
                let session_dir = manager
                    .persistence()
                    .session_dir(&mirri_core::SessionId::new(&info.id));
                if session_is_archived(&session_dir).await {
                    continue;
                }
                let wire_log_path = session_dir.join("wire.jsonl");
                // The primary arm: a dangling prompt (§1.3's original
                // inference).
                if let Some(scan) = scan_open_turn(&wire_log_path)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            session_id = %info.id,
                            "failed to scan the wire log tail for interruption inference"
                        );
                        None
                    })
                {
                    // V2's latter half: only prompts that landed AFTER the
                    // dead instance's launch belong to its lifecycle — an
                    // older dangling prompt is stale history, not this
                    // crash.
                    if scan.prompt_time_ms <= abnormal.started_at {
                        continue;
                    }
                    // D-M6: a pending user question/approval inside the
                    // dangling bracket means the turn was waiting on the
                    // USER, not running — not an interruption. The wire
                    // vocabulary carries
                    // `interaction.request`/`interaction.resolved`
                    // (journal-routed but not yet written by any
                    // production ask path today — the gates live in memory
                    // only), so the predicate reads the record pair: an
                    // unresolved request after the prompt skips the
                    // session. Dormant until asks become journaled; see
                    // the module doc (requirements B.4).
                    if has_unresolved_interaction(&session_dir).await {
                        continue;
                    }
                    items.push(InterruptionItem {
                        session_id: info.id.clone(),
                        workspace_key: workspace_key.clone(),
                        title: if info.title.is_empty() {
                            info.id.clone()
                        } else {
                            info.title.clone()
                        },
                        interrupted_at: scan.prompt_time_ms,
                        cause: "service_exit".to_string(),
                        pending_background_hint: scan.pending_background.then(|| {
                            "a background task started inside the interrupted turn may have \
                             finished on its own"
                                .to_string()
                        }),
                        settled: false,
                    });
                    continue;
                }
                // The persistent arm (F-03, spec §2.1 “徽标数据天然跨重
                // 启”): a PREVIOUS boot of this same lifecycle already
                // backfilled the interrupted closure, and the server died
                // again before the user acted — the tail now ends in the
                // closure with no later prompt, so the open-turn arm sees
                // nothing. Re-derive the interruption from the closure
                // itself (ADR-008 read-time derivation: the closure is an
                // append-only fact; the 72h retention and any later
                // prompt are judged here at read time).
                if let Some(closure) = scan_interrupted_closure(&wire_log_path)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            session_id = %info.id,
                            "failed to scan the wire log tail for interrupted closures"
                        );
                        None
                    })
                {
                    // The same lifecycle-window rule: only closures whose
                    // turn was running inside THIS dead instance's window.
                    // (The prompt that opened the closed turn is not
                    // re-derived here — the closure's own end time is the
                    // interruption moment, and a closure from before the
                    // instance's launch is an older crash's already-judged
                    // history, consumed with its own instance directory.)
                    if closure.time_ms > abnormal.started_at {
                        items.push(InterruptionItem {
                            session_id: info.id.clone(),
                            workspace_key: workspace_key.clone(),
                            title: if info.title.is_empty() {
                                info.id.clone()
                            } else {
                                info.title.clone()
                            },
                            interrupted_at: closure.time_ms,
                            // The closure's own recorded cause when it
                            // carries one (the backfill stamps
                            // "service_exit"), else the abnormal arm's.
                            cause: closure
                                .cause
                                .clone()
                                .unwrap_or_else(|| "service_exit".to_string()),
                            // The background hint cannot be re-derived
                            // from the closure record itself (the
                            // dangling bracket is closed); the resume
                            // prompt's check-before-rerun instruction only
                            // rides the first-boot derivation.
                            pending_background_hint: None,
                            settled: false,
                        });
                    }
                }
            }
        }
        items
    }

    /// Backfill the synthetic `interrupted` closures for the dead
    /// instance's inferred sessions (§2.1). ADR-008's read-judge-write
    /// idempotence: the tail is re-scanned immediately before each append
    /// — if another writer (a racing instance, or the turn genuinely
    /// ending) already closed the bracket, this one is skipped.
    async fn backfill_abnormal_closures(&self, abnormal: &AbnormalExit) {
        for (_workspace_key, manager) in self.registered_workspace_managers() {
            for info in manager.list_all_sessions(true).await {
                let session_dir = manager
                    .persistence()
                    .session_dir(&mirri_core::SessionId::new(&info.id));
                if session_is_archived(&session_dir).await {
                    continue;
                }
                // The .gz guard (retention domain, ADR-007): a compressed
                // session is an at-rest archive — the backfill must not
                // rewrite it, and the interruption still rides the
                // in-process list (read-time derivation needs no write).
                if matches!(
                    mirri_persistence::probe_compressed(&session_dir),
                    mirri_persistence::compress::CompressedShape::Compressed
                ) {
                    tracing::warn!(
                        session_id = %info.id,
                        "skipping interrupted-closure backfill: the session is compressed"
                    );
                    continue;
                }
                let wire_log_path = session_dir.join("wire.jsonl");
                let Some(scan) = scan_open_turn(&wire_log_path).await.unwrap_or(None) else {
                    continue;
                };
                if scan.prompt_time_ms <= abnormal.started_at {
                    continue;
                }
                // Resident sessions write through their own recorder (the
                // session's live writer, never a second file handle racing
                // it); non-resident ones append to the journal directly.
                if let Ok(session) = self.resolve_session(&info.id).await {
                    session
                        .record_op(mirri_proto::WireOp::TurnEnded {
                            turn_id: scan.prompt_ordinal,
                            reason: "interrupted".to_string(),
                            error: Some(serde_json::json!({
                                "cause": "service_exit"
                            })),
                            duration_ms: None,
                        })
                        .await;
                } else if let Err(error) =
                    append_interrupted_closure(&wire_log_path, scan.prompt_ordinal, "service_exit")
                        .await
                {
                    tracing::warn!(
                        %error,
                        session_id = %info.id,
                        "failed to backfill the interrupted closure"
                    );
                }
            }
        }
    }

    /// F-06: settle entries whose resident session has driven a turn
    /// NEWER than the interruption — the user opened the session and sent
    /// a message directly (not through `recovery.resume`), so the badge
    /// would otherwise linger forever.
    ///
    /// The minimal honest check: right after the startup inference a
    /// session's activity shadow carries `last_turn_reason == None` (the
    /// backfill appends to the journal, never through the live event
    /// fold). Once the session is resident and `last_turn_reason` is
    /// `Some(_)` — any ended newer turn, whatever its outcome — the
    /// interruption has been superseded by real user-visible progress and
    /// the entry settles. Non-resident sessions and sessions whose shadow
    /// still reports `None` keep the badge (nothing newer has landed).
    pub(super) async fn settle_superseded_interruptions(&self) {
        let candidates: Vec<String> = self.recovery.unsettled_session_ids();
        for session_id in candidates {
            let superseded = self
                .resolve_session_artifacts(&session_id)
                .map(|artifacts| {
                    artifacts
                        .activity_shadow
                        .snapshot_value()
                        .last_turn_reason
                        .is_some()
                })
                .unwrap_or(false);
            if superseded {
                self.recovery.settle(&session_id);
            }
        }
    }

    /// `recovery.list` — the notification card's pull source (spec §3).
    pub(super) async fn dispatch_recovery_list(&self, params: Value) -> Result<Value, EngineError> {
        if !params.is_null() && !params.as_object().is_some_and(|object| object.is_empty()) {
            return Err(invalid_params("recovery.list takes no params"));
        }
        // F-06: a resident session that drove a newer turn settles its
        // entry before the list is read (the manual-continuation badge
        // leak).
        self.settle_superseded_interruptions().await;
        let interruptions: Vec<InterruptionRow> = self
            .recovery
            .live_interruptions()
            .into_iter()
            .map(|item| InterruptionRow {
                workspace_name: self.workspace_display_name(&item.workspace_key),
                session_id: item.session_id,
                workspace_key: item.workspace_key,
                title: item.title,
                interrupted_at: item.interrupted_at,
                cause: item.cause,
                pending_background_hint: item.pending_background_hint,
            })
            .collect();
        // F-08: the honest expiry boundary is the EARLIEST per-item one —
        // `min(interrupted_at) + 72h` over the listed rows (each row's own
        // retention anchor). An empty list carries no boundary at all, so
        // the field is omitted (the client renders "nothing pending",
        // not "expires now").
        let expires_at = interruptions
            .iter()
            .map(|row| row.interrupted_at)
            .min()
            .map(|earliest| earliest + RETENTION_WINDOW_MS);
        let result = RecoveryListResult {
            interruptions,
            expires_at,
        };
        serde_json::to_value(result)
            .map_err(|error| EngineError::Internal(format!("recovery list failed: {error}")))
    }

    /// `recovery.dismiss` — settle one entry (E.5). Idempotent: an unknown
    /// or already-settled id is a plain ok.
    pub(super) async fn dispatch_recovery_dismiss(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| params.get("sessionId").and_then(Value::as_str))
            .ok_or_else(|| invalid_params("recovery.dismiss requires a session_id"))?
            .to_string();
        self.recovery.settle(&session_id);
        Ok(serde_json::json!({ "ok": true }))
    }

    /// `recovery.dismiss_all` — settle every entry (C.4).
    pub(super) async fn dispatch_recovery_dismiss_all(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        if !params.is_null() && !params.as_object().is_some_and(|object| object.is_empty()) {
            return Err(invalid_params("recovery.dismiss_all takes no params"));
        }
        self.recovery.settle_all();
        Ok(serde_json::json!({ "ok": true }))
    }

    /// `recovery.resume` — E.2's resume semantics: settle the entry, make
    /// the session resident (a persisted non-resident session is resumed
    /// first), then launch the recovery turn as a fire-mode
    /// `SystemTrigger` prompt whose origin metadata carries the recovery
    /// context (E.3) and whose text tells the model where the turn stopped
    /// (plus the pending-background check-before-rerun instruction when the
    /// interruption carried one).
    ///
    /// Settle ordering (F-05): the entry is taken out of the list first
    /// (no concurrent list read sees a settled-but-listed row), but if the
    /// turn launch FAILS the item is restored unsettled — the badge stays
    /// live and a later resume/dismiss retries, instead of the
    /// interruption silently vanishing with no turn running.
    ///
    /// An unknown or settled id answers `not_found` — the same
    /// not-found mapping the session lifecycle faces use (an unknown id
    /// is not an interruption; re-resuming a settled one re-drives a turn
    /// the user already settled, so it is refused symmetrically).
    pub(super) async fn dispatch_recovery_resume(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| params.get("sessionId").and_then(Value::as_str))
            .ok_or_else(|| invalid_params("recovery.resume requires a session_id"))?
            .to_string();
        let item = self
            .recovery
            .unsettle_take(&session_id)
            .ok_or_else(|| not_found(format!("session not interrupted: {session_id}")))?;

        match self.launch_recovery_turn(&session_id, &item).await {
            Ok(result) => Ok(result),
            // F-05: the launch failed — restore the entry unsettled so
            // the badge survives and the user can retry.
            Err(error) => {
                self.recovery.restore_unsettled(item);
                Err(error)
            }
        }
    }

    /// The resume's turn launch (E.2/E.3's shared body): ensure the session
    /// is resident, then fire the recovery-origin prompt. Split out of
    /// [`Self::dispatch_recovery_resume`] so the settle-restore wrapper
    /// (F-05) wraps exactly this.
    async fn launch_recovery_turn(
        &self,
        session_id: &str,
        item: &InterruptionItem,
    ) -> Result<Value, EngineError> {
        // Ensure resident: a non-resident session is resumed through the
        // session lifecycle first (the same `session.resume` the client
        // would call), then resolves.
        if self.resolve_session(session_id).await.is_err() {
            self.dispatch_resume_session(json!({ "session_id": session_id }))
                .await?;
            self.resolve_session(session_id)
                .await
                .map_err(|_| not_found(format!("session not found: {session_id}")))?;
        }

        // The resume prompt (E.3's model-side instruction): where the turn
        // stopped, plus the check-before-rerun instruction when the
        // interruption carried a pending background task.
        let mut text = format!(
            "Resume the interrupted task from where it stopped. The service \
             exited at {} while this turn was running.",
            item.interrupted_at
        );
        if let Some(hint) = &item.pending_background_hint {
            text.push_str(&format!(
                " Before re-running anything, check whether this background \
                 task already finished and inspect its result: {hint}"
            ));
        }
        self.send_message_with_origin(
            json!({ "session_id": session_id, "text": text, "mode": "fire" }),
            mirri_core::PromptOrigin::SystemTrigger,
            json!({
                "kind": "recovery",
                "cause": item.cause,
                "interruptedAt": item.interrupted_at,
            }),
            Some("Resume interrupted task".to_string()),
        )
        .await
    }

    /// `recovery.resume_all` — C.4's bulk arm: every live (unsettled,
    /// unexpired) entry is resumed sequentially (fire-mode turns queue
    /// naturally behind the per-session single-active-turn fence). The
    /// `queued` field is a batch-2 placeholder — it stays 0 until the
    /// `resumeConcurrency` configuration lands with the web batch; the
    /// fire-mode queue is today's concurrency backstop. A resume that
    /// FAILS is reported in `failed` (and its entry restored unsettled —
    /// F-05's per-item semantics) instead of being swallowed.
    pub(super) async fn dispatch_recovery_resume_all(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        if !params.is_null() && !params.as_object().is_some_and(|object| object.is_empty()) {
            return Err(invalid_params("recovery.resume_all takes no params"));
        }
        let live = self.recovery.live_interruptions();
        let mut accepted = 0u64;
        let mut failed: Vec<Value> = Vec::new();
        for item in live {
            match self
                .dispatch_recovery_resume(json!({ "session_id": item.session_id }))
                .await
            {
                Ok(_) => accepted += 1,
                Err(error) => failed.push(json!({
                    "sessionId": item.session_id,
                    "error": error.to_string(),
                })),
            }
        }
        Ok(serde_json::json!({
            "accepted": accepted,
            "queued": 0,
            "failed": failed,
        }))
    }

    /// The workspace's display name: the registry's registered name when
    /// one exists, else the key itself (a synthesized or unregistered
    /// workspace has nothing better to show).
    fn workspace_display_name(&self, workspace_key: &str) -> String {
        self.registry
            .registration(workspace_key)
            .and_then(|registration| registration.name)
            .unwrap_or_else(|| workspace_key.to_string())
    }
}

/// `recovery.list`'s envelope (spec §3's wire shape, camelCase-serialized).
/// `expires_at` is the earliest per-item retention boundary (F-08) —
/// omitted when the list is empty (no item, no boundary).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryListResult {
    interruptions: Vec<InterruptionRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

/// The graceful record's entry view (D-M2's direct read): the record
/// already carries everything the list row needs.
fn graceful_entry(
    entry: &RunningSessionEntry,
    record: &lifecycle::GracefulRecord,
) -> InterruptionItem {
    InterruptionItem {
        session_id: entry.session_id.clone(),
        workspace_key: entry.workspace_key.clone(),
        title: if entry.title.is_empty() {
            entry.session_id.clone()
        } else {
            entry.title.clone()
        },
        // The entry's own timestamp first (§1.3: the record-entry
        // moment), falling back to the exit moment for legacy rows.
        interrupted_at: if entry.interrupted_at > 0 {
            entry.interrupted_at
        } else {
            record.exited_at
        },
        cause: "graceful_exit".to_string(),
        // The graceful snapshot cannot see inside the wire log's turn
        // bracket — the background hint is the abnormal arm's derivation.
        pending_background_hint: None,
        settled: false,
    }
}

/// Whether the session's wire log carries an unresolved `interaction.request`
/// after the last prompt — the D-M6 wait-on-user exclusion's disk predicate.
///
/// The `interaction.request` / `interaction.resolved` pair is part of the
/// wire vocabulary and journal routing, but no production ask path writes
/// them today (asks live in the in-memory interaction kernel only), so on
/// current logs this is always `false`. The predicate is kept (and tested
/// with hand-written records) so the exclusion turns on the moment asks
/// become journaled — dropping it would silently regress requirements B.4
/// the day that lands.
async fn has_unresolved_interaction(session_dir: &std::path::Path) -> bool {
    use mirri_persistence::migration::migrate_records_in_memory;

    let wire_log_path = session_dir.join("wire.jsonl");
    let Ok(records) = migrate_records_in_memory(&wire_log_path).await else {
        return false;
    };
    let mut open: Vec<String> = Vec::new();
    for record in &records {
        match record.get("type").and_then(Value::as_str) {
            Some("interaction.request") => {
                if let Some(id) = record.get("id").and_then(Value::as_str) {
                    open.push(id.to_string());
                }
            }
            Some("interaction.resolved") => {
                if let Some(id) = record.get("id").and_then(Value::as_str) {
                    open.retain(|open_id| open_id != id);
                }
            }
            _ => {}
        }
    }
    !open.is_empty()
}

/// The production liveness probe: a TCP connect against `127.0.0.1:port`
/// with a short timeout. A successful connect means some instance still
/// owns the port (alive); anything else — refused, timed out — reads as
/// dead. Synchronous by the sentinel's contract (`&dyn Fn`); the connect
/// deadline bounds the block.
fn probe_port_alive(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// The pid arm of the production liveness gate (spec §1.2 / F-04): the
/// recorded OS process id must still be alive. Uses the platform probe
/// `mirri-os` already ships (`/proc` on Linux, `libproc` on macOS — a
/// zombie counts as dead). On non-Unix platforms the probe is
/// unavailable by construction, so the arm degrades to "cannot rule
/// dead" (`true`) and the TCP connect alone decides — the documented
/// fallback for the one non-Unix surface the engine builds for.
fn probe_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match mirri_os::probe_process(pid as i32) {
            Ok(probe) => probe.alive,
            // A probe ERROR is not evidence of death (the caller reads
            // it as "cannot determine") — trust the TCP arm alone.
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}
