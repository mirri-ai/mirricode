//! Session resume: reconstruct a session's agent state by replaying its
//! persisted `wire.jsonl` (snapshot + incremental ops), restoring agent
//! configuration and session-scoped state.

use super::*;
use mirri_proto::{ReplayLimits, apply_with_limits};

impl SessionManager {
    /// Resume a session from its persisted wire log.
    ///
    /// Reconstructs the session's agent state by replaying `wire.jsonl`,
    /// restoring agent configuration and session-scoped state. The recreated
    /// main agent is built from a default [`AgentScopeConfig`] (with the
    /// permission mode and usage totals replayed from the log); hosts that
    /// need runtime services on the resumed agent (e.g. a reverse-RPC
    /// callback so user tools and approval asks keep working) use
    /// [`resume_session_with_config`](Self::resume_session_with_config).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::ResumeFailed`] if the wire log file is
    /// missing, [`SessionError::StateFile`] if `state.json` cannot be
    /// loaded, or [`SessionError::AlreadyExists`] if the session is still
    /// registered in the workspace.
    pub async fn resume_session(&self, session_id: &SessionId) -> SessionResult<Arc<SessionScope>> {
        self.resume_session_with_config(session_id, AgentScopeConfig::default())
            .await
    }

    /// Resume a session from its persisted wire log, building the recreated
    /// main agent on top of `base_config`.
    ///
    /// Same replay as [`resume_session`](Self::resume_session), but the
    /// replayed permission mode and usage totals are layered onto the given
    /// base config — the host keeps its runtime services (reverse-RPC
    /// callback, providers) on the resumed agent instead of getting the
    /// bare default config. The fold simulation runs under the default
    /// capacity constants; use
    /// [`resume_session_with_limits`](Self::resume_session_with_limits) to
    /// supply the resume-time limits a session with tightened caps ran
    /// with.
    ///
    /// # Errors
    ///
    /// Same error surface as [`resume_session`](Self::resume_session).
    pub async fn resume_session_with_config(
        &self,
        session_id: &SessionId,
        base_config: AgentScopeConfig,
    ) -> SessionResult<Arc<SessionScope>> {
        self.resume_session_with_limits(session_id, base_config, None)
            .await
    }

    /// Resume a session from its persisted wire log with resume-time
    /// capacity limits for the fold simulation.
    ///
    /// Same replay as
    /// [`resume_session_with_config`](Self::resume_session_with_config),
    /// but the wire-log fold models the given [`ReplayLimits`] ceilings
    /// (the `set_limits` values the session's live turns ran with —
    /// `TurnFlow` applies them from its compaction strategy every turn).
    /// `None` keeps the default-constants behavior (10 000 / 1 000). The
    /// limits are constant per session run, so resume-time values
    /// reproduce the historical fold unless the config changed between
    /// runs — that residual is accepted (see [`ReplayLimits`]).
    ///
    /// # Errors
    ///
    /// Same error surface as [`resume_session`](Self::resume_session).
    pub async fn resume_session_with_limits(
        &self,
        session_id: &SessionId,
        base_config: AgentScopeConfig,
        replay_limits: Option<ReplayLimits>,
    ) -> SessionResult<Arc<SessionScope>> {
        self.resume_session_with_scope(
            session_id,
            base_config,
            replay_limits,
            SessionScopeConfig::default(),
        )
        .await
    }

    /// Resume a session with an explicit session-scope config (B1 T4): the
    /// task-lifecycle knobs (`[task]`/`[subagent]`) a resumed session runs
    /// with. Everything else is the same replay as
    /// [`resume_session_with_limits`](Self::resume_session_with_limits) —
    /// this variant exists so the embedded resume assembly can push the same
    /// derived [`SessionScopeConfig`] the create path uses.
    pub async fn resume_session_with_scope(
        &self,
        session_id: &SessionId,
        base_config: AgentScopeConfig,
        replay_limits: Option<ReplayLimits>,
        scope_config: SessionScopeConfig,
    ) -> SessionResult<Arc<SessionScope>> {
        // 1. Check if the session is already live in the workspace — FIRST,
        //    before any persistence read (SDR-004): a lazy-created session
        //    that never conversed has no wire log yet, but a live session
        //    must still answer `AlreadyExists`, never a resume/init failure.
        if self.workspace.get_session(session_id).await.is_some() {
            return Err(SessionError::AlreadyExists(session_id.to_string()));
        }

        // 2. Validate that the wire log exists — it is the source of truth
        //    for the session's agent state.
        let wire_log_path = self.persistence.wire_log_path(session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return Err(SessionError::ResumeFailed(format!(
                "wire log not found: {}",
                wire_log_path.display()
            )));
        }

        // 3. Load persisted metadata.
        let metadata = self.persistence.load_state(session_id).await?;

        // An explicit resume clears any close record from this process —
        // the session is live again.
        self.closed_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id.as_str());

        // 4. Create the session in the workspace with the loaded metadata.
        //    Its background tasks are rehydrated from `<session_dir>/tasks/`
        //    (tasks left running by the previous process are reconciled to
        //    stopped at load).
        let tasks_dir = self.persistence.session_dir(session_id).join("tasks");
        let session = self
            .workspace
            .create_session_with_config(session_id.clone(), metadata, scope_config, Some(tasks_dir))
            .await
            .map_err(|e| SessionError::AlreadyExists(e.to_string()))?;

        // 5-8. Replay the wire log, install the event plumbing, and rebuild
        //      the agent. Every step here is fallible — and the session is
        //      already registered in the workspace map — so a failure must
        //      roll the half-created session back out: a failed resume must
        //      never leave a live-but-empty zombie behind (read faces would
        //      answer hollow data while artifact-dependent faces report
        //      not-found, and every later resume would fail with
        //      AlreadyExists until the process restarted). The on-disk files
        //      stay untouched — the failure is this attempt's, not the
        //      log's death sentence, and a repaired log must resume on the
        //      next attempt.
        if let Err(error) = self
            .land_resume_tail(session_id, &session, base_config, replay_limits)
            .await
        {
            self.rollback_failed_resume(session_id).await;
            return Err(error);
        }

        // Fire the SessionStart external-command hook for the resumed
        // session (TS input shape: `{ source: 'resume' }`). Fire-and-forget.
        let _ = fire_session_start_hook(&session, "resume").await;

        Ok(session)
    }

    /// The fallible tail of [`Self::resume_session_with_scope`]: everything
    /// after the workspace registration — the wire-log replay, the event
    /// plumbing, the agent rebuild, the session-scoped state restore, the
    /// subagent rehydration, the append-mode wire log, and the
    /// interruption-reminder reconciliation. The caller rolls the registered
    /// session back out of the live tables when this fails.
    async fn land_resume_tail(
        &self,
        session_id: &SessionId,
        session: &Arc<SessionScope>,
        base_config: AgentScopeConfig,
        replay_limits: Option<ReplayLimits>,
    ) -> SessionResult<()> {
        // Replay the wire log (from the snapshot checkpoint when one
        // exists) and rebuild the main agent's context memory. Any
        // `blobref:` media URLs recorded in the log are rehydrated back
        // to `data:` URIs from the session's blob store (mirroring the
        // TS kernel's post-restore rehydrate pass); blobs that cannot be
        // loaded degrade to a placeholder rather than failing the resume.
        let replay = self
            .replay_agent_state_opt(session_id, true, replay_limits)
            .await?;
        let (state, replayed_op_count) = (replay.state, replay.op_count);
        let blob_service = AgentBlobService::new(self.persistence.session_dir(session_id));
        let memory = reconstruct_context_memory_with_blobs(&state, &blob_service);

        // F2 (ADR-017 "单一折叠结果"): the embedded resume assembly re-parks
        // the session's still-parked steers right after this returns, via
        // [`Self::replayed_pending_steers`] — which used to re-run a SECOND
        // full main-log replay (fold + whole-log parse) just to read
        // `state.steer_pending`. Derive the pairs HERE from the replay we
        // just performed (turn.steer minus turn.steer_flushed, blob-rehydrated
        // through the same session blob store the memory reconstruction used)
        // and cache them on the manager keyed by session id. The embedded
        // face consumes the cache and the second replay (with its full-log
        // parse) disappears. Behavior is identical: the blob-rehydrated
        // `(input, origin)` pairs are exactly what a from-scratch replay
        // would produce — a resume folds the same log with the same limits.
        // The cache is written unconditionally — an empty steer set is the
        // common case, and skipping its write would make every later read
        // fall back to the second full replay the cache exists to avoid.
        self.put_pending_steers(session_id, steer_pending_pairs(&state, &blob_service));

        // Tripwire (duplicate turn ids): the log holds the same `turn.ended`
        // id more than once — an allocator restart re-minted it. Turn-
        // addressable surfaces (fork's `turnId` anchor, undo-by-turn) are
        // ambiguous for exactly these ids; surface the conflict instead of
        // failing the resume.
        for turn_id in &replay.duplicate_turn_ids {
            tracing::warn!(
                session_id = %session_id,
                turn_id,
                occurrences = "multiple",
                "wire log holds duplicate turn ids (a restarted turn allocator \
                 re-minted ids the log already recorded); turn-addressable \
                 surfaces are ambiguous for these ids"
            );
        }

        // Install fresh per-session event plumbing *before* the agent is
        // created: the resumed agent's goal/skill tools are constructed
        // with this bus baked in, so model-initiated mutations announce on
        // the session's wire (`event.goal.updated` / `event.skill.activated`).
        // The resumed announcement is NOT published here — a publish on
        // the fresh bus reaches zero subscribers (relays install after
        // this returns); the caller announces after wiring its relay. The
        // task manager is wired onto the fresh bus for the same reason
        // every other publisher is: buses are never reused.
        let artifacts = self.install_fresh_artifacts(session_id, session);
        // Seed the session's turn-id allocator past the replayed log's
        // maximum (the single seeding point — every flow the session later
        // creates draws from this counter, so a fresh flow's first turn
        // continues the numbering instead of restarting at 1 and re-minting
        // an id the log already holds). Bumped again below for subagent
        // journals that minted higher.
        artifacts.bump_turn_id_floor(replay.max_turn_id);
        session
            .tasks()
            .set_event_bus(Arc::clone(&artifacts.event_bus));
        // The cron manager is wired onto the fresh bus for the same reason
        // (buses are never reused across a resume).
        session
            .cron()
            .set_event_bus(Arc::clone(&artifacts.event_bus));

        // Build the AgentScopeConfig from the replayed state (layered
        // onto the host-supplied base config) so that permission mode and
        // usage totals are restored at agent creation time. Other fields
        // (goal, plan mode, tool activation) are restored after creation
        // via the session-scoped managers. The fresh session bus rides
        // the config so the rebuilt tools announce on it.
        let mut agent_config = build_agent_config_from_state(&state, base_config, session_id)?;
        agent_config.event_bus = Some(Arc::clone(&artifacts.event_bus));
        // ADR-009 (audit P0-1, review N1): the interaction kernel rides the
        // resumed main agent's config from the FRESH gates installed above —
        // the same two-gate source the embedded create path wires. The
        // embedded host's resume base config cannot carry it (it is built
        // before this session exists), the same constraint as the title
        // handle; this is its main-agent creation point. [`restore_subagents`]
        // below inherits it, so a resumed child's approval asks register
        // with the session's gates and stay visible on the
        // `approval.listPending` / `question.listPending` / `interaction.*`
        // surfaces across the restart.
        agent_config.interaction_kernel = Some(Arc::new(crate::InteractionKernel::new(
            Arc::clone(&artifacts.approval_gate),
            Arc::clone(&artifacts.question_gate),
        )));
        let agent = session
            .create_agent(AgentId::new(MAIN_AGENT_ID), agent_config)
            .await
            .map_err(|e| SessionError::ResumeFailed(format!("failed to recreate agent: {e}")))?;
        *agent
            .context_memory()
            .lock()
            .expect("context memory mutex poisoned") = memory;

        // Restore session-scoped state that lives outside the agent config:
        // the active goal (with budget), plan-mode state, and the cron
        // tasks. These services are session-scoped (`Arc<GoalManager>`,
        // `Arc<PlanManager>`, `Arc<CronManager>`) and use interior
        // mutability, so they are restored after the agent is created.
        restore_session_scoped_state(&agent, &state);

        // Restore swarm mode (TS `restoreEnter`): only the state
        // machine is restored — the reminder injection is NOT repeated
        // (the replayed context already carries the reminder when it
        // was delivered live, and the replay of `swarm_mode.exit`
        // already retracted it when it was popped). The trigger is the
        // folded `swarm_mode.enter` payload (snake_case "manual" /
        // "task" / "tool"); a trigger that fails to parse leaves the
        // mode inactive rather than failing the resume.
        if let Some(trigger) = state.swarm_trigger.clone()
            && let Ok(trigger) = serde_json::from_value::<crate::swarm::SwarmModeTrigger>(trigger)
        {
            session.swarm_mode().restore_enter(trigger);
        }

        // B4 T8 (D-55) → ADR-006/ADR-017 (lazy subagent restore): the
        // children are registered in the roster and their journals' turn-id
        // ceiling is scanned, but their `AgentScope`s are NOT recreated here —
        // that fold is deferred to first use via the session's lazy hydration
        // seam (so a session with many registered children resumes fast). The
        // children's journals share the session's turn-id namespace (their
        // flows draw from the same allocator), so their minted maximum raises
        // the floor too — a resumed subagent must not re-mint an id its own
        // journal already recorded.
        let child_max_turn_id =
            restore_subagents(session, &state, &self.persistence, Some(self)).await;
        artifacts.bump_turn_id_floor(child_max_turn_id);

        // ADR-006/ADR-017: install the lazy subagent hydration seam — a
        // registered-but-dormant child's `AgentScope` is reconstructed from
        // its journal the first time it is woken/consumed (the host's adopt/
        // resume faces, the embedded steer re-park). The closure captures only
        // the persistence layer, so the session owns it without a lifetime
        // cycle.
        self.install_subagent_hydrator(session);

        // Re-open the wire log for appending so future turns are
        // recorded. The replay above ran with no writer installed (a fresh
        // scope starts with `wire_log: None`), so the replayed ops were
        // never re-appended to the log they came from. Appending — not
        // truncating — preserves the recorded history.
        let wire_log_path = self.persistence.wire_log_path(session_id);
        let wire_log = JsonlAppendLog::open_for_append(&wire_log_path)
            .await
            .map_err(|e| {
                SessionError::WireLog(format!(
                    "failed to open {} for appending: {e}",
                    wire_log_path.display()
                ))
            })?;
        session.install_wire_log(wire_log).await;

        // Reconcile pending interruption reminders (TS
        // `reconcilePendingReminders`): a crash between the durable
        // `turn.cancel` write and the reminder delivery leaves
        // `interruption_pending` non-empty in the replayed state —
        // deliver the reminder into the reconstructed memory and record
        // the delivery ops on the wire log just re-opened for
        // appending, so the next replay starts from a drained set. The
        // dedup inside the helper keeps a journal that already carries
        // the reminder append from being duplicated.
        if !state.interruption_pending.is_empty() {
            let memory = agent.context_memory();
            crate::interruption_reminder::deliver_pending_interruption_reminders(
                &memory,
                Some(&session.wire_recorder()),
                &state.interruption_pending,
            )
            .await;
        }

        tracing::info!(
            session_id = %session_id,
            wire_log = %wire_log_path.display(),
            replayed_op_count,
            message_count = state.conversation.messages.len(),
            "session resumed; wire log replayed and agent context reconstructed"
        );
        Ok(())
    }

    /// Roll a failed resume back out of the live tables (the counterpart of
    /// [`SessionManager::rollback_failed_fork`], minus the on-disk
    /// deletion): the session leaves the workspace map and any installed
    /// artifacts are drained exactly like a close, so no pending gate future
    /// or bus subscriber of the failed resume is left hanging. The on-disk
    /// session files are untouched — the failure to replay belongs to this
    /// attempt, not to the persisted data, and a repaired log must resume
    /// cleanly on the next try. The closed-id record is deliberately NOT
    /// set: a failed resume is not a user close, and restore-on-demand must
    /// stay free to retry once the underlying condition (e.g. a corrupt wire
    /// log) is fixed.
    async fn rollback_failed_resume(&self, session_id: &SessionId) {
        // 1. Workspace map: the session must stop resolving as live.
        self.workspace.remove_session(session_id).await;
        // 2. Artifacts (bus + gates), if the failure happened after they
        //    were installed — drained so nothing dangles. The map entry is
        //    taken OUT of the lock before any await (the guard must not
        //    live across the async shutdown drain).
        let artifacts = self
            .artifacts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(artifacts) = artifacts {
            SessionShutdown::new(&artifacts).shutdown().await;
        }
        // 3. Discard the F2 pending-steer cache (if the resume populated it
        //    before a later step failed): the session never becomes live, so
        //    its replayed steer pairs must not linger.
        self.pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// Replay a session's wire log into an [`AgentState`].
    ///
    /// First the wire log is migrated in-memory to the current protocol
    /// version (old v1.0-v1.4 records are transformed so they replay instead
    /// of being lost), then the optional on-disk rewrite of the migrated log
    /// is attempted best-effort. The migrated records are folded into an
    /// [`AgentState`], using the `snapshot.json` checkpoint (when present)
    /// as the base state so only the wire ops recorded after its `op_index`
    /// are replayed; without a checkpoint the whole log is replayed from a
    /// fresh state. Returns the reconstructed state, the total number of
    /// ops folded (the value a fresh snapshot should record), the log's
    /// maximum minted turn id (the floor the resumed session's turn-id
    /// allocator must be seeded past — computed over the FULL log, not just
    /// the post-checkpoint tail), and the turn ids the log minted more than
    /// once (the allocator-restart signature the resume warns about).
    ///
    /// The fold models the given [`ReplayLimits`] ceilings — the close-time
    /// snapshot path passes the limits derived from the wired server-config
    /// snapshot, so the checkpoint agrees with a later resume on which
    /// appends the capacity checks rejected; `None` keeps the default
    /// constants (10 000 / 1 000), the historical behavior.
    ///
    /// A snapshot that is inconsistent with the log (corrupt file, or an
    /// `op_index` beyond the end of the log) is ignored with a warning and a
    /// full replay is performed instead. A record that parses as JSON but
    /// not as a wire op fails the replay as corruption, except for the final
    /// record which is dropped with a warning (mirroring the torn-final-line
    /// tolerance of the wire log reader).
    pub(crate) async fn replay_agent_state(
        &self,
        session_id: &SessionId,
        replay_limits: Option<ReplayLimits>,
    ) -> SessionResult<WireLogReplay> {
        self.replay_agent_state_opt(session_id, true, replay_limits)
            .await
    }

    /// Read-only per-agent context histories for a session, without
    /// resuming it and without any on-disk side effect — the scan surface
    /// behind the embedded `session.search` RPC.
    ///
    /// A live session reads every agent's in-memory context; a
    /// persisted-only session replays its `wire.jsonl` **in memory** (the
    /// on-disk migration rewrite of [`Self::replay_agent_state`] is
    /// deliberately skipped — a query must not rewrite the log it reads),
    /// yielding the main agent's history alone (subagent state is not
    /// persisted). `None` means the session is neither live nor replayable
    /// (no wire log / unreadable log) — the caller degrades by skipping the
    /// session's messages, never by failing the whole query.
    ///
    /// Lazy-restore boundary (ADR-006/ADR-017, ROADMAP open item 15): a
    /// resumed session's dormant children — registered in the roster but not
    /// yet hydrated — are NOT scanned here, so `session.search` misses their
    /// journals until the child is woken. The OLD eager restore made every
    /// child searchable at resume time. Accepted gap for now; candidates:
    /// read-only journal replay of roster children at scan time, or
    /// documenting the gap as the contract.
    pub async fn read_only_histories(
        &self,
        session_id: &SessionId,
    ) -> Option<Vec<(String, Vec<ContextMessage>)>> {
        if let Some(session) = self.workspace.get_session(session_id).await {
            let mut out = Vec::new();
            for agent_id in session.list_agents().await {
                let Some(agent) = session.get_agent(&agent_id).await else {
                    continue;
                };
                let history = agent
                    .context_memory()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .history()
                    .to_vec();
                out.push((agent_id.as_str().to_string(), history));
            }
            return Some(out);
        }
        let replay = self
            .replay_agent_state_opt(session_id, false, None)
            .await
            .map_err(|error| {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "read-only history replay failed; the session degrades out of the scan"
                );
                error
            })
            .ok()?;
        let state = replay.state;
        let memory = reconstruct_context_memory(&state);
        Some(vec![(MAIN_AGENT_ID.to_string(), memory.history().to_vec())])
    }

    /// Read-only per-turn usage fold of a session's main-agent wire log —
    /// the read face behind the `session.snapshot` / `message.list`
    /// per-turn usage columns. No resume, no on-disk side effect (mirrors
    /// [`Self::read_only_histories`]). `None` means no readable wire log;
    /// callers degrade to an empty fold, never fail the RPC.
    pub async fn read_only_turn_usages(
        &self,
        session_id: &SessionId,
    ) -> Option<crate::session::turn_usage::TurnUsageFold> {
        let wire_log_path = self.persistence.wire_log_path(session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return None;
        }
        let migrated_records = migrate_records_in_memory(&wire_log_path)
            .await
            .map_err(|error| {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "turn-usage wire log read failed; the read face degrades to empty"
                );
                error
            })
            .ok()?;
        let ops = migrated_records_to_wire_ops(migrated_records, &wire_log_path)
            .map_err(|error| {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "turn-usage wire log is unreadable as ops; the read face degrades to empty"
                );
                error
            })
            .ok()?;
        Some(crate::session::turn_usage::fold_turn_usages(&ops))
    }

    /// Read-only maximum turn id a session's main-agent wire log has minted —
    /// every turn-id-bearing record counts (ended ids, cancel ids, and the
    /// `turn.prompt` ordinals the aborted/dangling namespace occupies; see
    /// [`crate::session::turn_usage::wire_log_max_turn_id`]), not just
    /// `turn.ended` (a dangling bracket's ordinal can exceed the ended
    /// maximum). The resume path no longer reads this — the replay itself
    /// seeds the session's allocator — but the read face stays for
    /// diagnostics and tooling. No on-disk side effect (mirrors
    /// [`Self::read_only_turn_usages`]). `0` means no readable wire log or
    /// no turn records.
    pub async fn read_only_max_turn_id(&self, session_id: &SessionId) -> u64 {
        let wire_log_path = self.persistence.wire_log_path(session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return 0;
        }
        let migrated_records = match migrate_records_in_memory(&wire_log_path).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "max-turn-id wire log read failed; the read face reports 0"
                );
                return 0;
            }
        };
        let ops = match migrated_records_to_wire_ops(migrated_records, &wire_log_path) {
            Ok(ops) => ops,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "max-turn-id wire log is unreadable as ops; the read face reports 0"
                );
                return 0;
            }
        };
        crate::session::turn_usage::wire_log_max_turn_id(&ops)
    }

    /// Read-only start time (epoch-ms) of the session's still-open turn —
    /// the anchor the web's live "Working for X" run header ticks from
    /// (`session.snapshot`'s `current_turn_started_at`). Returns the `time`
    /// of the LAST `turn.prompt` record when no `turn.ended` follows it
    /// (the turn is still open); `None` when every turn is closed, the wire
    /// log is missing/unreadable, or the open turn's record carries no
    /// `time` (a legacy TS log that predates the stamp).
    ///
    /// The per-record `time` field is injected at the persistence layer into
    /// the raw JSONL (see `mirri_persistence::append_log`) and dropped when
    /// the migrated records deserialize into [`WireOp`], so the read walks
    /// the migrated records BEFORE the wire-op conversion — directly on the
    /// JSON values `migrate_records_in_memory` returns — like the other
    /// read-only faces, with no on-disk side effect.
    pub async fn read_only_open_turn_started_at(&self, session_id: &SessionId) -> Option<u64> {
        let wire_log_path = self.persistence.wire_log_path(session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return None;
        }
        let migrated_records = migrate_records_in_memory(&wire_log_path)
            .await
            .map_err(|error| {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "open-turn-start wire log read failed; the snapshot stays without a turn start anchor"
                );
                error
            })
            .ok()?;
        // Walk the raw migrated records (pre-WireOp — the `time` field lives
        // only there): the most recent `turn.prompt` opens the running turn
        // and any `turn.ended` after it closes it. A prompt record whose
        // `time` is absent/unusable yields None, never a wrong anchor.
        let mut started_at: Option<u64> = None;
        for record in &migrated_records {
            match record
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
            {
                "turn.prompt" => {
                    started_at = record.get("time").and_then(serde_json::Value::as_u64);
                }
                "turn.ended" => {
                    started_at = None;
                }
                _ => {}
            }
        }
        started_at
    }

    /// [`Self::replay_agent_state`] with the on-disk migration rewrite made
    /// optional — read-only callers (`read_only_histories`) pass `false` so
    /// a query never writes the log it reads — and with resume-time
    /// capacity limits for the fold simulation: `None` keeps the
    /// default-constants behavior (the read-only surfaces have no turn-flow
    /// strategy in scope; the resume path passes the strategy's caps).
    async fn replay_agent_state_opt(
        &self,
        session_id: &SessionId,
        rewrite_log: bool,
        replay_limits: Option<ReplayLimits>,
    ) -> SessionResult<WireLogReplay> {
        let wire_log_path = self.persistence.wire_log_path(session_id);

        // Migrate the wire log in memory first: records written by older
        // protocols (v1.0-v1.4) are brought up to the current wire shape
        // before replay, so they reduce to the same state a fresh session
        // would have produced instead of being skipped. The optional on-disk
        // rewrite afterwards is best-effort — a failure (e.g. a torn final
        // line that only the in-memory path tolerates) must not block the
        // resume.
        let migrated_records = migrate_records_in_memory(&wire_log_path)
            .await
            .map_err(|e| {
                SessionError::WireLog(format!("failed to read {}: {e}", wire_log_path.display()))
            })?;
        if rewrite_log && let Err(error) = migrate_log_file(&wire_log_path).await {
            tracing::warn!(
                session_id = %session_id,
                wire_log = %wire_log_path.display(),
                %error,
                "best-effort wire log migration rewrite failed; replaying in-memory migrated records"
            );
        }

        // The CPU-bound replay core — JSON parsing, record migration,
        // snapshot deserialization, and the reducer fold — runs on the
        // blocking pool, never on an async worker thread. The production
        // incident (2026-10-12) traced the web UI freeze to exactly this
        // loop starving the executor while parallel tasks ran: the fold
        // holds the worker between await points, so event forwarders,
        // heartbeats, and RPC dispatch cannot be scheduled.
        let session_id_owned = session_id.to_string();
        let snapshot_path = self.persistence.snapshot_path(session_id);
        let replay = tokio::task::spawn_blocking(move || {
            replay_records_blocking(
                &session_id_owned,
                &wire_log_path,
                migrated_records,
                &snapshot_path,
                replay_limits,
            )
        })
        .await
        .map_err(|e| SessionError::ResumeFailed(format!("wire log replay task failed: {e}")))??;
        Ok(replay)
    }

    /// The session's still-parked steers, replayed read-only from the wire
    /// log (T3, the durable-steer ruling).
    ///
    /// Returns the `(input, origin)` pairs of every `turn.steer` record not
    /// yet consumed by a `turn.steer_flushed` marker — the durable steer
    /// buffer a resume restores into the agent's turn flow, so a steer the
    /// user sent before a restart is not silently swallowed. `blobref:`
    /// media URLs are rehydrated from the session's blob store (the same
    /// pass the context reconstruction applies). Read-only: the on-disk
    /// migration rewrite is skipped, and a replay failure degrades to an
    /// empty buffer (a broken log must not fail the resume it already
    /// survived) with a warning.
    pub async fn replayed_pending_steers(
        &self,
        session_id: &SessionId,
    ) -> Vec<(
        Vec<crate::message::ContentPart>,
        crate::origin::PromptOrigin,
    )> {
        // F2: the just-resumed session's pending steers were already derived
        // by the single main-log fold in `land_resume_tail` and cached — take
        // them, eliminating the second full-log replay (and its full-log
        // parse; ADR-017). When the entry is absent (a non-resume caller, or a
        // resume whose state folded no steers) the normal fallback below runs.
        let cached = self
            .pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(steers) = cached {
            return steers;
        }
        let Ok(replay) = self.replay_agent_state_opt(session_id, false, None).await else {
            tracing::warn!(
                session_id = %session_id,
                "pending-steer replay failed; the resumed steer buffer starts empty"
            );
            return Vec::new();
        };
        let state = replay.state;
        if state.steer_pending.is_empty() {
            return Vec::new();
        }
        let blob_service = AgentBlobService::new(self.persistence.session_dir(session_id));
        steer_pending_pairs(&state, &blob_service)
    }

    /// The still-parked steers of a SUBAGENT journal, replayed read-only —
    /// the child-journal counterpart of [`Self::replayed_pending_steers`]
    /// (the task-8 resume restore).
    ///
    /// Notification records routed to a subagent addressee ride its OWN
    /// journal (`agents/<agent_id>/wire.jsonl` — the [`notification_recorder`]
    /// routing contract): the busy-path park records the `turn.steer` there,
    /// so the resume re-park must read the CHILD journal, never the session
    /// journal the main-agent replay reads. The fold mirrors
    /// [`restore_subagents`]: the whole journal is reduced from a fresh state
    /// under the default replay limits (children have no snapshot
    /// checkpoint). Read-only and best-effort — a missing or unreadable
    /// journal degrades to an empty buffer with a warning (a broken child
    /// log must not fail the resume its main journal already survived).
    pub async fn replayed_pending_agent_steers(
        &self,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Vec<(
        Vec<crate::message::ContentPart>,
        crate::origin::PromptOrigin,
    )> {
        // D2 single-scan: the resume's turn-id ceiling scan already derived
        // each child's still-parked steer entries from the same journal read
        // and cached them under (session, agent) — take them, so the
        // per-child read face costs no journal I/O or fold at all. When the
        // entry is absent (a non-resume caller, or the journal was missing
        // at scan time) the normal fallback below runs.
        let cached = self
            .pending_agent_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(session_id.clone(), agent_id.to_string()));
        if let Some(parked) = cached {
            let blob_service = AgentBlobService::new(self.persistence.session_dir(session_id));
            return parked_pairs_from_entries(&parked, &blob_service);
        }
        let journal_path = self.persistence.agent_wire_log_path(session_id, agent_id);
        let records = match migrate_records_in_memory(&journal_path).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    agent_id,
                    journal = %journal_path.display(),
                    %error,
                    "subagent pending-steer replay failed; the resumed steer buffer starts empty"
                );
                return Vec::new();
            }
        };
        let mut state = AgentState::new();
        for record in records {
            match serde_json::from_value::<WireOp>(record) {
                Ok(op) => {
                    apply_with_limits(&mut state, &op, None);
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        agent_id,
                        %error,
                        "skipping an unparsable subagent journal record during pending-steer replay"
                    );
                }
            }
        }
        if state.steer_pending.is_empty() {
            return Vec::new();
        }
        let blob_service = AgentBlobService::new(self.persistence.session_dir(session_id));
        steer_pending_pairs(&state, &blob_service)
    }
}

/// Convert a replayed state's still-parked steers into the `(input, origin)`
/// pairs the resume re-park pushes into the agent's steer buffer — the
/// shared conversion of [`SessionManager::replayed_pending_steers`] and
/// [`SessionManager::replayed_pending_agent_steers`].
///
/// A record that does not parse is skipped with a warning (the raw record
/// stays in the wire log) — one bad steer must not fail the resume or drop
/// the steers behind it. `blobref:` media URLs are rehydrated from the
/// session's blob store.
fn steer_pending_pairs(
    state: &AgentState,
    blob_service: &AgentBlobService,
) -> Vec<(
    Vec<crate::message::ContentPart>,
    crate::origin::PromptOrigin,
)> {
    parked_pairs_from_entries(&state.steer_pending, blob_service)
}

/// Convert raw parked-steer entries (`{"input": .., "origin": ..}` maps —
/// the reducer's `steer_pending` record shape, also what the D2 single-scan
/// caches per child) into the blob-rehydrated `(input, origin)` pairs.
fn parked_pairs_from_entries(
    entries: &[serde_json::Value],
    blob_service: &AgentBlobService,
) -> Vec<(
    Vec<crate::message::ContentPart>,
    crate::origin::PromptOrigin,
)> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        // A record that does not parse is skipped with a warning (the
        // raw record stays in the wire log) — one bad steer must not
        // fail the resume or drop the steers behind it.
        let Some(input) = entry.get("input") else {
            tracing::warn!("skipping pending steer record without input");
            continue;
        };
        let parts = match serde_json::from_value::<Vec<crate::message::ContentPart>>(input.clone())
        {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "skipping pending steer record that does not parse");
                continue;
            }
        };
        let origin = entry
            .get("origin")
            .and_then(|origin| {
                serde_json::from_value::<crate::origin::PromptOrigin>(origin.clone()).ok()
            })
            .unwrap_or(crate::origin::PromptOrigin::User);
        out.push((blob_service.load_parts(&parts), origin));
    }
    out
}

/// The result of replaying a session's wire log: the folded
/// [`AgentState`], the total op count (the value a fresh snapshot should
/// record), the log's maximum minted turn id, and the turn ids the log
/// minted more than once.
pub(crate) struct WireLogReplay {
    pub state: AgentState,
    pub op_count: usize,
    /// The floor the resumed session's turn-id allocator must be seeded
    /// past (see [`crate::session::turn_usage::wire_log_max_turn_id`]).
    pub max_turn_id: u64,
    /// Duplicate `turn.ended` ids — the allocator-restart signature.
    pub duplicate_turn_ids: Vec<u64>,
}

/// The CPU-bound core of a wire-log replay: record parsing, snapshot
/// deserialization, and the reducer fold. Runs on the blocking pool
/// (never an async worker) — see the call site in
/// [`SessionManager::replay_agent_state_opt`].
fn replay_records_blocking(
    session_id: &str,
    wire_log_path: &Path,
    migrated_records: Vec<serde_json::Value>,
    snapshot_path: &Path,
    replay_limits: Option<ReplayLimits>,
) -> SessionResult<WireLogReplay> {
    // Convert the migrated records to wire ops. A record that parses as
    // JSON but not as a wire op is corruption — except for the final
    // record, which is dropped with a warning, mirroring the
    // torn-final-line tolerance the file reader applies.
    let records = migrated_records_to_wire_ops(migrated_records, wire_log_path)?;

    // The turn-id floor over the FULL log (not just the post-checkpoint
    // tail the fold below consumes): the resumed session's allocator
    // must never re-mint an id ANY bracket of the log occupies — ended,
    // aborted, or cancelled (see [`turn_usage::wire_log_max_turn_id`]).
    // Duplicate ended ids are the allocator-restart signature the
    // resume path warns about (layer-3 tripwire).
    let max_turn_id = crate::session::turn_usage::wire_log_max_turn_id(&records);
    let duplicate_turn_ids = crate::session::turn_usage::duplicate_turn_ids(&records);

    let snapshot = match SnapshotStore::read_blocking(snapshot_path) {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                snapshot = %snapshot_path.display(),
                error = %e,
                "ignoring unreadable snapshot; replaying wire log from scratch"
            );
            None
        }
    };

    // Base state + how many ops it already covers. A snapshot whose
    // op_index exceeds the log's record count is stale (the log was
    // truncated or replaced after the checkpoint) — drop it.
    let (mut state, base_op_index) = match snapshot {
        Some((snapshot_state, snapshot_op_index))
            if (snapshot_op_index as usize) <= records.len() =>
        {
            tracing::info!(
                session_id = %session_id,
                snapshot_op_index,
                total_ops = records.len(),
                "using snapshot as replay base state"
            );
            (snapshot_state, snapshot_op_index as usize)
        }
        Some((_stale_state, snapshot_op_index)) => {
            tracing::warn!(
                session_id = %session_id,
                snapshot_op_index,
                total_ops = records.len(),
                "snapshot op_index is ahead of the wire log; replaying from scratch"
            );
            (AgentState::default(), 0)
        }
        None => (AgentState::default(), 0),
    };

    // Fold the ops after the checkpoint through the reducer, under the
    // resume-time capacity limits when the caller supplied them (the
    // compaction-apply arm's slot simulation then models the ceilings
    // the session's live turns ran with). Ops fold in place on `state`,
    // so replay stays deterministic — the same log always yields the
    // same state.
    let mut replayed_ops = 0usize;
    for op in records.into_iter().skip(base_op_index) {
        apply_with_limits(&mut state, &op, replay_limits);
        replayed_ops += 1;
    }

    let total_op_count = base_op_index + replayed_ops;
    tracing::info!(
        session_id = %session_id,
        base_op_index,
        replayed_ops,
        total_op_count,
        "wire log replayed"
    );
    Ok(WireLogReplay {
        state,
        op_count: total_op_count,
        max_turn_id,
        duplicate_turn_ids,
    })
}

/// Convert migrated wire-log records (JSON values) into [`WireOp`]s for
/// replay.
///
/// A record that parses as JSON but not as a wire op is corruption and
/// fails the replay — except for the final record, which is dropped with a
/// warning. This mirrors the torn-final-line tolerance of the file reader:
/// a crash can only truncate the *last* record of a log, never a middle
/// one.
pub(crate) fn migrated_records_to_wire_ops(
    records: Vec<serde_json::Value>,
    wire_log_path: &Path,
) -> SessionResult<Vec<WireOp>> {
    let mut ops = Vec::with_capacity(records.len());
    let record_count = records.len();
    for (line_index, record) in records.into_iter().enumerate() {
        match serde_json::from_value::<WireOp>(record) {
            Ok(op) => ops.push(op),
            Err(error) if line_index + 1 == record_count => {
                tracing::warn!(
                    wire_log = %wire_log_path.display(),
                    line_number = line_index + 1,
                    %error,
                    "dropping final wire record that does not parse as a wire op"
                );
            }
            Err(error) => {
                return Err(SessionError::WireLog(format!(
                    "corrupted wire record at line {} of {}: {error}",
                    line_index + 1,
                    wire_log_path.display()
                )));
            }
        }
    }
    Ok(ops)
}

/// Fire the `SessionStart` external-command hook on the session's hook engine.
///
/// TS input shape: `{ source }` where `source` is `'startup' | 'resume'`
/// (forks are excluded by the caller). Fire-and-forget: a block decision is
/// recorded in the returned result but never prevents session creation.
pub(crate) async fn fire_session_start_hook(
    session: &Arc<SessionScope>,
    source: &str,
) -> SessionResult<()> {
    let input = serde_json::json!({ "source": source });
    let _ = session
        .hook_engine()
        .trigger(HookEventType::SessionStart, input, &HashMap::new())
        .await;
    Ok(())
}

/// Fire the `SessionEnd` external-command hook on the session's hook engine.
///
/// TS input shape: `{ reason }`. The Rust extension carries the close reason
/// as a string (`"exit"`, `"error"`, or `"user"`). Fire-and-forget: a block
/// decision never prevents the close.
pub(crate) async fn fire_session_end_hook(
    session: &Arc<SessionScope>,
    reason: &CloseReason,
) -> SessionResult<()> {
    let reason_str = match reason {
        CloseReason::Exit => "exit",
        CloseReason::Error(_) => "error",
        CloseReason::UserInitiated => "user",
        CloseReason::UserClosed => "archive",
    };
    let input = serde_json::json!({ "reason": reason_str });
    let _ = session
        .hook_engine()
        .trigger(HookEventType::SessionEnd, input, &HashMap::new())
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// State restoration helpers
// ---------------------------------------------------------------------------

/// Build an [`AgentScopeConfig`] from a replayed [`AgentState`], restoring
/// the permission mode and accumulated usage totals on top of the host's
/// `base` config.
///
/// The model falls back to the base config's `model_id` when the replayed
/// state carries no model record — the embedded host seeds that field with
/// the config snapshot's `default_model`, so the no-record fallback chain
/// equals the fresh-create chain (spec 11 R9). When both are unset the
/// resume fails loudly with an actionable error — never a silent fallback
/// to a built-in model (spec 11 R4).
///
/// Fields that are restored via session-scoped managers (goal, plan mode,
/// tool activation, session approvals) are handled separately by
/// [`restore_session_scoped_state`] because those services use interior
/// mutability and are shared across all agents in the session.
fn build_agent_config_from_state(
    state: &AgentState,
    base: AgentScopeConfig,
    session_id: &SessionId,
) -> SessionResult<AgentScopeConfig> {
    // Restore permission mode. The proto stores the mode as a lowercase
    // string ("manual", "auto", "yolo"); PermissionMode deserializes from
    // the same lowercase strings.
    let permission_mode = parse_permission_mode(&state.permission.mode);

    // Restore accumulated usage totals so the agent's usage service starts
    // with the tokens already consumed before the restart.
    let initial_usage = Some(UsageInitialState {
        total_input_tokens: state.usage.total_input_tokens,
        total_output_tokens: state.usage.total_output_tokens,
        total_cache_read_tokens: state.usage.total_cache_read_tokens,
        total_cache_creation_tokens: state.usage.total_cache_creation_tokens,
        first_call: state
            .usage
            .first_call
            .map(|first| crate::usage::TokenUsage {
                input_tokens: first.input_tokens,
                output_tokens: first.output_tokens,
                cache_read_tokens: first.cache_read_tokens,
                cache_creation_tokens: first.cache_creation_tokens,
            }),
    });

    // Re-apply the recorded model alias so a resumed session keeps its
    // model; with no record and no configured default the resume fails
    // with the same actionable guidance as a fresh create.
    let model_id = state
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .or_else(|| {
            let base_model = base.model_id.clone();
            (!base_model.is_empty()).then_some(base_model)
        })
        .ok_or_else(|| {
            SessionError::ResumeFailed(format!(
                "no model configured for session '{}': the wire log carries no model record \
                 and no default model is set; set default_model in config.toml or create the \
                 session with an explicit model",
                session_id.as_str()
            ))
        })?;

    Ok(AgentScopeConfig {
        permission_mode,
        initial_usage,
        model_id,
        ..base
    })
}

/// Restore session-scoped state (goal + budget, plan mode, session
/// approvals, tool activation, user-tool registrations, cron tasks) onto
/// the freshly created agent's shared managers.
///
/// This is called after [`SessionScope::create_agent`] so the session-scoped
/// `GoalManager`, `PlanManager`, and `CronManager` (shared `Arc`s) already
/// exist. Each manager uses interior mutability (`Mutex`), so restoration is
/// done through `&self` methods.
///
/// # Idempotency
///
/// This function is **NOT idempotent**. It assumes a freshly created agent
/// scope whose `GoalManager` has no active goal, whose `PlanManager` is not
/// in plan mode, and whose `CronManager` is empty. Calling it twice on the
/// same agent would fail silently: the second `goal_manager.create()` would
/// be rejected ("a goal already exists"), the second `plan_manager.enter()`
/// would be rejected ("plan mode already active"), and the second cron
/// restore would re-insert the same tasks over the freshly restored ones,
/// leaving stale state from the first call in place. Always call it exactly
/// once per agent creation, immediately after `create_agent`.
fn restore_session_scoped_state(agent: &Arc<AgentScope>, state: &AgentState) {
    let goal_manager = agent.goal();
    let plan_manager = agent.plan();

    // Restore the active goal and its budget. The proto's GoalState carries
    // the objective, optional completion criterion, status, usage, and budget
    // limits; we map these to GoalManager's create + set_budget API.
    if let Some(goal_state) = &state.goal
        && goal_manager
            .create(
                goal_state.objective.clone(),
                goal_state.completion_criterion.clone(),
            )
            .is_ok()
    {
        // Restore the goal status if it is not the default "active".
        let status = parse_goal_status(&goal_state.status);
        if status != GoalStatus::Active {
            goal_manager.update(status);
        }

        // Restore the budget from either the goal's budget_limits JSON
        // or the session-level BudgetState.
        if let Some(budget) = parse_goal_budget(goal_state, &state.budget) {
            goal_manager.set_budget(budget);
        }

        // Restore the accumulated usage totals replayed from the
        // usage-only `goal.update` ops (F11): a 40-turn budget that had
        // consumed 30 turns before the restart continues from 30, not
        // from zero.
        goal_manager.restore_usage(
            goal_state.turns_used.unwrap_or(0),
            goal_state.tokens_used.unwrap_or(0),
        );
    }

    // Restore plan-mode state with the persisted plan text (F12): the
    // replayed `planText` (enter's versioned field, superseded by any
    // `plan.revision` that carried one) is the plan content — the legacy
    // fallback (pre-T6 logs, or enters that never recorded text) keeps the
    // old behavior of using the plan id as the text.
    if let Some(plan_state) = &state.plan_mode {
        // enter() returns an error if plan mode is already active, but the
        // plan manager is freshly created so this always succeeds.
        let text = state
            .plan_text
            .clone()
            .unwrap_or_else(|| plan_state.plan_id.clone());
        let _ = plan_manager.enter(text);
        plan_manager.set_plan_id(plan_state.plan_id.clone());
    } else if let Some(text) = state.plan_text.clone().filter(|text| !text.is_empty()) {
        // The session was closed after an approved plan exited — plan mode
        // stays inactive, but the plan text stays readable (the resume
        // summary's plan content and `plan.status`'s reference surface).
        plan_manager.restore_plan_text(text);
    }

    // Restore the session approvals derived from
    // `permission.record_approval_result` records (TS parity: the v2
    // kernel's `sessionApprovalRulePatterns`). The rules cache is not
    // touched — session approvals are a separate set consulted by the
    // `SessionApprovalHistoryPolicy`, and the replayed set makes the same
    // tool pattern skip the approval ask it skipped before the restart.
    for pattern in &state.permission.session_approval_patterns {
        agent.permission_manager().record_approval(pattern.clone());
    }

    // Restore the tool-activation state replayed from the tools/profile ops:
    // the restricted active tool set (tools.set_active_tools / the
    // profile.bind seeded set) is applied to the agent's tool manager, so
    // the LLM-visible tool list after the restart matches the pre-restart
    // one. `None` means the unrestricted default the freshly created agent
    // already has, so nothing is applied.

    if let Some(active) = &state.tools.active {
        agent
            .tool_manager()
            .write()
            .set_active_tools(active.clone());
    }

    // Restore the user-tool registrations replayed from
    // `tools.register_user_tool` (minus `tools.unregister_user_tool`). The
    // registrations only replay when the agent carries a reverse-RPC
    // callback (the resumed session's host-supplied base config); without
    // one there is nothing executable to rebuild — the raw records stay in
    // the wire log and a host re-registers its own tools after the resume
    // (mirroring how permission rules are re-supplied). A record that does
    // not parse is skipped with a warning rather than failing the resume.
    restore_user_tool_registrations(agent, &state.tools.user_registrations);

    // Restore the per-user-tool visibility overrides recorded via
    // `tools.set_user_tool_enabled` (enabled=false): the replayed
    // registrations are visible by default in the unrestricted mode, so
    // each disabled name is removed from the recreated agent's active set
    // — the LLM-visible tool list after the restart matches the
    // pre-restart one. (The enabled=true arm needs no restore work: in
    // the unrestricted default the registration is already visible, and
    // under an explicit whitelist the reducer folded the name into the
    // replayed active set.)
    if !state.tools.disabled_user_tools.is_empty() {
        let tool_manager = agent.tool_manager();
        let mut tool_manager = tool_manager.write();
        for name in &state.tools.disabled_user_tools {
            tool_manager.set_user_tool_enabled(name, false);
        }
    }

    // Restore the cron tasks recorded via cron.add (minus those removed via
    // cron.delete). Each record is parsed individually: a task that does not
    // deserialize is skipped with a warning (it stays in the wire log) so one
    // bad record never fails the whole resume.
    restore_cron_tasks(&agent.cron(), &state.cron_tasks);
}

/// Rebuild a session's user-tool registration set from the replayed state.
///
/// Each replayed registration is re-registered into the agent's tool
/// manager as an [`RpcTool`](crate::RpcTool) wired to the agent's
/// reverse-RPC callback (present only when the resume was started with a
/// host-supplied base config carrying one). A record without a name, or
/// one whose parameters are not an object, is skipped with a warning — the
/// raw record stays in the wire log, but a bad record must not take the
/// whole resume down.
/// Resolve the model a restored subagent is rehydrated under (spec 11
/// A14/R5): the registration's pinned value when it carries one (the
/// spawn-time pin survives the restart); otherwise the parent session's
/// CURRENT model — the main agent's live config, which the restore flow
/// resolved from the replayed log (the last switch record, else the
/// configured default) before this step ran. `None` when neither is
/// available, and the child cannot be rehydrated (A6's restore face — the
/// caller logs and skips rather than rehydrate on an empty model).
async fn restore_subagent_model(
    session: &SessionScope,
    registration: &mirri_proto::SubagentRegistration,
) -> Option<String> {
    if let Some(model) = registration.model.clone().filter(|model| !model.is_empty()) {
        return Some(model);
    }
    match session.get_agent(&AgentId::new(MAIN_AGENT_ID)).await {
        Some(main) => {
            let config_arc = main.config();
            let guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
            (!guard.model_id.is_empty()).then(|| guard.model_id.clone())
        }
        None => None,
    }
}

/// Lazy subagent restore (ADR-006/ADR-017 "子代理懒重放"): seed the session's
/// live identity registry from the replayed `subagent.registered` entries and
/// scan each child journal ONLY for its turn-id ceiling — the single resume
/// cost that must stay eager (a restarted allocator must not re-mint an id a
/// child journal already occupies). The per-child full fold, context-memory
/// reconstruction, and `AgentScope` creation are DEFERRED to first use via the
/// session's lazy hydration seam ([`SessionManager::install_subagent_hydrator`]),
/// so a resumed session with many registered children no longer folds all their
/// journals up front.
///
/// Best-effort — a child whose journal is missing or unreadable logs a warning
/// and contributes no ceiling (the registry entry stays; the main agent's
/// history is unaffected).
///
/// Returns the highest turn id any child journal minted.
async fn restore_subagents(
    session: &Arc<SessionScope>,
    state: &AgentState,
    persistence: &SessionPersistence,
    steer_cache: Option<&SessionManager>,
) -> u64 {
    // Seed the live identity registry first (review SF-3): the replayed
    // parent links must be queryable even for children whose journals turn
    // out to be missing below — and the roster is what the lazy hydrator
    // resolves a registered-but-dormant child against.
    session
        .seed_subagent_registry(state.subagents.clone())
        .await;
    // SDR-004 d: the session-level `permission.set_mode` op restores onto
    // the main agent's config only (build_agent_config_from_state); the
    // children rehydrated LAZILY below come back at the config default.
    // Re-broadcast the replayed session mode so the resumed session keeps
    // its pre-restart posture on the main agent and on every child the
    // hydrator later creates.
    session
        .set_permission_mode(parse_permission_mode(&state.permission.mode))
        .await;
    // Turn-id ceiling: scan every child journal for its maximum minted turn
    // id WITHOUT folding it into an AgentState — the only subagent journal
    // work the resume keeps. Materializing each journal's ops just to scan
    // turn ids is light post-F1 (no per-op deep clones).
    //
    // D2 single-scan: the same pass derives each child's still-parked steer
    // entries (`turn.steer` push, `turn.steer_flushed` FIFO drain — the
    // reducer's exact `steer_pending` semantics) and caches them under
    // (session, agent) for the embedded resume assembly's per-child read
    // face, so an N-child resume parses each journal once instead of twice.
    let mut max_turn_id = 0u64;
    for registration in &state.subagents {
        let journal_path = persistence.agent_wire_log_path(session.id(), &registration.agent_id);
        if !journal_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            tracing::warn!(
                session_id = %session.id(),
                agent_id = %registration.agent_id,
                journal = %journal_path.display(),
                "subagent journal missing for turn-id scan; the child stays dormant"
            );
            continue;
        }
        let records = match migrate_records_in_memory(&journal_path).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id(),
                    agent_id = %registration.agent_id,
                    %error,
                    "subagent journal unreadable for turn-id scan; the child stays dormant"
                );
                continue;
            }
        };
        let ops = match migrated_records_to_wire_ops(records, &journal_path) {
            Ok(ops) => ops,
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id(),
                    agent_id = %registration.agent_id,
                    %error,
                    "subagent journal unreadable as ops for the turn-id scan; the child stays dormant"
                );
                continue;
            }
        };
        max_turn_id = max_turn_id.max(crate::session::turn_usage::wire_log_max_turn_id(&ops));
        // D2 single-scan: derive the still-parked steer entries in the same
        // pass — `turn.steer` pushes, `turn.steer_flushed` FIFO drains (the
        // reducer's exact `steer_pending` semantics, replayed without an
        // AgentState). Cached under (session, agent) for the embedded
        // assembly's per-child read face, so the journal is parsed once.
        if let Some(manager) = steer_cache {
            let mut parked: Vec<serde_json::Value> = Vec::new();
            for op in &ops {
                match op {
                    WireOp::TurnSteer { input, origin } => {
                        parked.push(serde_json::json!({
                            "input": input,
                            "origin": origin,
                        }));
                    }
                    WireOp::TurnSteerFlushed { count } => {
                        // FIFO consumption, clamped so a torn log can never
                        // underflow (the reducer's drain guard).
                        let take = (*count as usize).min(parked.len());
                        parked.drain(0..take);
                    }
                    _ => {}
                }
            }
            manager
                .pending_agent_steers
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (session.id().clone(), registration.agent_id.clone()),
                    parked,
                );
        }
    }
    max_turn_id
}

/// Lazily hydrate ONE registered subagent's `AgentScope` from its journal —
/// the ADR-006/ADR-017 deferred-restore face, invoked by the session's
/// hydrator when the child is first woken/consumed.
///
/// Mirrors the eager per-child fold: migrate the journal, fold it into a fresh
/// state under the default replay limits, reconstruct the context memory, and
/// `create_agent` with the ADR-009 approval-channel inheritance and the
/// spec-11 A6 model guard.
pub(crate) async fn hydrate_subagent_from_journal(
    session: &SessionScope,
    persistence: &SessionPersistence,
    agent_id: &str,
) -> SessionResult<()> {
    // Idempotent: a child already live (hydrated by an earlier trigger, or
    // the directly-constructed eager path) has nothing to redo.
    if session.get_agent(&AgentId::new(agent_id)).await.is_some() {
        return Ok(());
    }
    let registration = session
        .subagent_registration(agent_id)
        .await
        .ok_or_else(|| {
            SessionError::ResumeFailed(format!("cannot hydrate unregistered subagent: {agent_id}"))
        })?;
    let journal_path = persistence.agent_wire_log_path(session.id(), agent_id);
    if !journal_path.exists() {
        // zero-io-allow: baseline Path IO, see F-T13-2
        return Err(SessionError::ResumeFailed(format!(
            "subagent journal missing for lazy hydration: {}",
            journal_path.display()
        )));
    }
    let records = migrate_records_in_memory(&journal_path)
        .await
        .map_err(|e| {
            SessionError::WireLog(format!("failed to read {}: {e}", journal_path.display()))
        })?;
    let mut child_state = AgentState::new();
    for record in records {
        match serde_json::from_value::<WireOp>(record) {
            Ok(op) => {
                let _ = apply_with_limits(&mut child_state, &op, None);
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id(),
                    agent_id,
                    %error,
                    "skipping an unparsable subagent journal record during lazy hydration"
                );
            }
        }
    }
    let memory = reconstruct_context_memory(&child_state);
    // ADR-009 (audit P0-1): the hydrated child config inherits the parent's
    // approval channel — the same two services the live spawn face inherits —
    // so a lazily-restored child keeps its approval callback and interaction
    // kernel across a restart. The main agent is the carrier; when it is
    // absent (a directly-constructed session) the fields degrade to the
    // defaults, matching the pre-ADR-009 config shape.
    let (reverse_rpc_slot, interaction_kernel) =
        match session.get_agent(&AgentId::new(MAIN_AGENT_ID)).await {
            Some(main) => {
                let config_arc = main.config();
                let config_guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
                (
                    config_guard.reverse_rpc_slot.clone(),
                    config_guard.interaction_kernel.clone(),
                )
            }
            None => (None, None),
        };
    let model_id = match restore_subagent_model(session, &registration).await {
        Some(model_id) => model_id,
        None => {
            // spec 11 A6, restore face: the registration carries no pinned
            // model and the parent session has no current model either —
            // hydrating the child with an empty model would silently
            // misroute it, so the child is refused with the actionable
            // warning instead.
            tracing::warn!(
                session_id = %session.id(),
                agent_id,
                profile_name = %registration.profile_name,
                "subagent registration carries no model and the session's main agent has no \
                 current model; the child is not hydrated (spec 11 A6)"
            );
            return Err(SessionError::ResumeFailed(format!(
                "no model resolvable for subagent '{agent_id}'"
            )));
        }
    };
    let config = AgentScopeConfig {
        model_id,
        profile_name: Some(registration.profile_name.clone()),
        thinking_effort: registration.thinking_effort.as_deref().and_then(|effort| {
            serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
        }),
        reverse_rpc_slot,
        interaction_kernel,
        ..AgentScopeConfig::default()
    };
    let child = match session.create_agent(AgentId::new(agent_id), config).await {
        Ok(child) => child,
        Err(error) => {
            // Concurrent double-hydration convergence (review F4-2): two
            // triggers can both pass the `get_agent` idempotence check
            // above and race into `create_agent`. The loser's
            // "agent already exists" failure does NOT mean the child is
            // unrestorable — the winner hydrated it — so re-verify and
            // converge to success instead of surfacing a false failure
            // (which would send callers like the task driver's dispatcher
            // resolution down the main-agent fallback).
            if session.get_agent(&AgentId::new(agent_id)).await.is_some() {
                tracing::debug!(
                    session_id = %session.id(),
                    agent_id,
                    "concurrent hydration race: the child was hydrated by another trigger"
                );
                return Ok(());
            }
            return Err(SessionError::ResumeFailed(format!(
                "failed to create subagent agent: {error}"
            )));
        }
    };
    *child
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = memory;
    // SDR-004 d (the eager path's re-broadcast, applied lazily): the child is
    // created AFTER the resume's `set_permission_mode` broadcast, so it comes
    // back at the agent-config default. Stamp it with the session's current
    // posture — sourced live from the main agent (the carrier of the replayed
    // session mode) rather than over-broadcasting to every agent again.
    if let Some(main) = session.get_agent(&AgentId::new(MAIN_AGENT_ID)).await {
        let main_mode = main
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .permission_mode;
        child.set_permission_mode(main_mode);
    }
    // MCP tool seeding (review F4-1): MCP tools reach agents only through
    // the engine's live fan-out sites, which iterate LIVE agents — a dormant
    // child missed every server add/remove/enable that landed between the
    // resume and this hydration. The main agent's tool table is the fan-outs'
    // continuously-maintained convergence point (the resume path seeds it,
    // every pool transition updates it), so re-registering its CURRENT MCP
    // set onto the child restores the same effective surface the eager
    // restore held (the live-spawn face's D-47 user-tool inheritance, applied
    // to the MCP table at hydration). A server removed after the resume is
    // correctly absent — the snapshot is taken now, not at resume time.
    if let Some(main) = session.get_agent(&AgentId::new(MAIN_AGENT_ID)).await {
        let main_tools = main.tool_manager();
        let mcp_tools = main_tools.read().mcp_tools_snapshot();
        if !mcp_tools.is_empty() {
            let child_tools = child.tool_manager();
            let mut child_tools = child_tools.write();
            for (name, tool) in mcp_tools {
                // The qualified name is the entry key; the server tag rides
                // the registration (removal-by-server still works).
                let server = name
                    .strip_prefix("mcp__")
                    .and_then(|rest| rest.split_once("__"))
                    .map(|(server, _)| server.to_string())
                    .unwrap_or_else(|| name.clone());
                child_tools.register_mcp_tool(server, tool);
            }
        }
    }
    Ok(())
}

fn restore_user_tool_registrations(agent: &Arc<AgentScope>, registrations: &[serde_json::Value]) {
    if registrations.is_empty() {
        return;
    }
    // The session's slot handle — the rebuilt RpcTool resolves the CURRENT
    // callback at call time (the same late-subscribe semantics as
    // AskUserQuestion), so the replay only needs the slot to exist: the host
    // seeds the engine-level callback into it right after the resume lands,
    // and a serve transport's later subscribe overwrites the same handle.
    let Some(callback_slot) = agent
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .reverse_rpc_slot
        .clone()
    else {
        tracing::debug!(
            registrations = registrations.len(),
            "skipping user-tool registration replay: the resumed agent has no reverse-RPC slot \
             (the host re-supplies its own tools after the resume)"
        );
        return;
    };
    let tool_manager = agent.tool_manager();
    let mut tool_manager = tool_manager.write();
    for registration in registrations {
        let name = registration.get("name").and_then(|v| v.as_str());
        let Some(name) = name else {
            tracing::warn!(
                "skipping user-tool registration record without a name; the raw record stays in the wire log"
            );
            continue;
        };
        let description = registration
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let parameters = registration
            .get("parameters")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        tool_manager.register_user_tool(Arc::new(crate::RpcTool::new(
            name.to_string(),
            description.to_string(),
            parameters,
            Arc::clone(&callback_slot),
        )));
    }
}

/// Rebuild a session's cron task table from the replayed state.
///
/// The state stores the raw `cron.add` task JSON (the F16 wire DTO —
/// camelCase, epoch millis — or, on logs the migration never rewrote, the
/// legacy snake_case serde shape, with `lastFiredAt` stamped in place by
/// `cron.cursor` ops); each record is parsed individually through the
/// shape-tolerant [`cron_task_from_wire`] and handed to
/// [`CronManager::restore_tasks`], which recomputes `next_fire_at` from now
/// (never trusting the persisted absolute timestamp) and keeps already-fired
/// one-shot tasks listed but dormant. An unparseable record is skipped with
/// a warning — the raw record stays in the wire log, but a bad record must
/// not take the whole resume down.
fn restore_cron_tasks(cron: &Arc<CronManager>, raw_tasks: &[serde_json::Value]) {
    let mut tasks = Vec::with_capacity(raw_tasks.len());
    for raw in raw_tasks {
        match crate::cron::cron_task_from_wire(raw) {
            Some(task) => tasks.push(task),
            None => {
                tracing::warn!(
                    raw = %raw,
                    "skipping cron task record that does not parse; the raw record stays in the wire log"
                );
            }
        }
    }
    if tasks.is_empty() {
        return;
    }
    cron.restore_tasks(tasks);
}

/// Parse a permission mode string from the proto state into a
/// [`PermissionMode`].
///
/// Falls back to [`PermissionMode::default()`] (Manual) when the string is
/// not recognised, matching the resilient approach used throughout replay.
pub(crate) fn parse_permission_mode(mode: &str) -> PermissionMode {
    match mode {
        "manual" => PermissionMode::Manual,
        "auto" => PermissionMode::Auto,
        "yolo" => PermissionMode::Yolo,
        _ => {
            tracing::warn!(mode = %mode, "unrecognised permission mode in replayed state; defaulting to manual");
            PermissionMode::default()
        }
    }
}

/// Parse a goal status string from the proto state into a [`GoalStatus`].
///
/// Falls back to [`GoalStatus::Active`] when the string is not recognised.
pub(crate) fn parse_goal_status(status: &str) -> GoalStatus {
    match status {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "completed" => GoalStatus::Complete,
        "blocked" => GoalStatus::Blocked,
        _ => {
            tracing::warn!(
                status = %status,
                "unrecognised goal status in replayed state; defaulting to active"
            );
            GoalStatus::Active
        }
    }
}

/// Derive a [`GoalBudget`] from the goal state's budget_limits JSON and the
/// session-level [`mirri_proto::BudgetState`].
///
/// The proto's `GoalState.budget_limits` is a free-form JSON value; the
/// session-level `BudgetState` carries structured `turns_limit` and
/// `tokens_limit`. We prefer the structured session-level limits and fall
/// back to parsing the JSON value when the session-level limits are absent.
pub(crate) fn parse_goal_budget(
    goal_state: &mirri_proto::GoalState,
    session_budget: &mirri_proto::BudgetState,
) -> Option<GoalBudget> {
    // Prefer the session-level budget limits when present.
    if session_budget.turns_limit.is_some() || session_budget.tokens_limit.is_some() {
        return Some(GoalBudget {
            turns: session_budget.turns_limit.map(|v| v as u64),
            tokens: session_budget.tokens_limit,
            wall_clock_ms: None,
        });
    }

    // Fall back to parsing the goal-level budget_limits JSON. The TS kernel
    // writes this as { "turnBudget": N, "tokenBudget": N, "wallClockBudgetMs": N }
    // while the Rust kernel uses the shorter { "turns": N, "tokens": N,
    // "wallClockMs": N }. We accept both forms for cross-kernel compatibility.
    if let Some(json) = &goal_state.budget_limits {
        let turns = json
            .get("turns")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| json.get("turnBudget").and_then(serde_json::Value::as_u64));
        let tokens = json
            .get("tokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| json.get("tokenBudget").and_then(serde_json::Value::as_u64));
        let wall_clock_ms = json
            .get("wallClockMs")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                json.get("wall_clock_ms")
                    .and_then(serde_json::Value::as_u64)
            })
            .or_else(|| {
                json.get("wallClockBudgetMs")
                    .and_then(serde_json::Value::as_u64)
            });

        if turns.is_some() || tokens.is_some() || wall_clock_ms.is_some() {
            return Some(GoalBudget {
                turns,
                tokens,
                wall_clock_ms,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reverse_rpc::ReverseRpcCallback;
    use mirri_proto::SubagentRegistration;

    /// A reverse-RPC callback that records `approval.ask` payloads and
    /// answers `approved` — the minimal production-contract stand-in.
    #[derive(Default)]
    struct ApprovingCallback {
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait::async_trait]
    impl ReverseRpcCallback for ApprovingCallback {
        async fn call(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            Ok(serde_json::json!({ "decision": "approved" }))
        }
    }

    /// Given a restored session whose main agent carries a reverse-RPC
    /// callback and an interaction kernel, when the subagent registry is
    /// restored, then the rebuilt child config inherits both services from
    /// the main agent (ADR-009 b — the restart must not drop the child's
    /// approval channel).
    #[tokio::test]
    async fn should_inherit_approval_callback_and_interaction_kernel_when_restoring_subagent_config()
     {
        // Given a session whose recreated main agent carries the approval
        // callback and interaction kernel, a registered subagent, and an
        // (empty) child journal on disk
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        let callback = Arc::new(ApprovingCallback::default());
        let kernel = Arc::new(crate::InteractionKernel::new(
            Arc::new(crate::ApprovalGate::new(false)),
            Arc::new(crate::QuestionGate::new()),
        ));
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    reverse_rpc_slot: Some(Arc::new(std::sync::RwLock::new(Some(Arc::clone(
                        &callback,
                    )
                        as Arc<dyn ReverseRpcCallback>)))),
                    interaction_kernel: Some(Arc::clone(&kernel)),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-restored-child";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });

        // When the subagent roster is restored (its agent stays dormant), then
        // the child hydrates on demand from its journal
        restore_subagents(&session, &state, &persistence, None).await;
        hydrate_subagent_from_journal(&session, &persistence, child_id)
            .await
            .expect("the dormant child hydrates on demand");

        // Then the rebuilt child config carries both services, and they are
        // the SAME arcs the main agent carries (the approval channel stays
        // wired across the restart).
        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child rehydrated");
        let config_arc = child.config();
        let guard = config_arc.read().unwrap_or_else(|e| e.into_inner());
        let child_callback = guard
            .reverse_rpc_slot
            .as_ref()
            .and_then(|slot| slot.read().unwrap_or_else(|e| e.into_inner()).clone())
            .expect("child must inherit the reverse-RPC callback");
        assert_eq!(
            Arc::as_ptr(&child_callback),
            Arc::as_ptr(&callback),
            "the child must carry the main agent's callback"
        );
        let child_kernel = guard
            .interaction_kernel
            .as_ref()
            .expect("child must inherit the interaction kernel");
        assert_eq!(
            Arc::as_ptr(child_kernel),
            Arc::as_ptr(&kernel),
            "the child must carry the main agent's interaction kernel"
        );
    }

    /// Given a restored session whose main agent carries a current model and
    /// a registered child whose registration carries NO pinned model, When
    /// the subagent is restored, Then the child is rehydrated on the
    /// parent's current model (spec 11 A14/R5 — a model-less registration
    /// inherits the dispatcher's current model instead of unwrapping to an
    /// empty string).
    #[tokio::test]
    async fn should_inherit_parent_current_model_when_registration_carries_no_model() {
        // Given a session whose main agent carries "parent-model", a
        // registered subagent with no pinned model, and an (empty) child
        // journal on disk
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-inherit"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    model_id: "parent-model".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-inherited-restore";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });

        // When the subagent roster is restored (lazy: only the registry is
        // seeded and the journal turn-id ceiling scanned), then its agent is
        // hydrated on demand from the journal
        restore_subagents(&session, &state, &persistence, None).await;
        // The child is a dormant roster entry — NOT yet an agent scope (the
        // ADR-006/ADR-017 laziness contract).
        assert!(
            session.get_agent(&AgentId::new(child_id)).await.is_none(),
            "the child must stay dormant until it is woken"
        );
        hydrate_subagent_from_journal(&session, &persistence, child_id)
            .await
            .expect("the dormant child hydrates on demand");

        // Then the rebuilt child carries the parent's current model — not an
        // empty string
        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child rehydrated");
        assert_eq!(
            child
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id,
            "parent-model"
        );
    }

    /// Given a restored session whose main agent has NO current model and a
    /// registered child whose registration carries no pinned model, When the
    /// subagent is restored, Then the child is not rehydrated (best-effort
    /// skip with a warning — spec 11 A6's restore face, never an empty
    /// model).
    #[tokio::test]
    async fn should_skip_child_when_registration_and_parent_have_no_model() {
        // Given a session whose main agent has no model, a registered
        // subagent with no pinned model, and an (empty) child journal on
        // disk
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-skip"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        session
            .create_agent(AgentId::new(MAIN_AGENT_ID), AgentScopeConfig::default())
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-skipped-restore";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });

        // When the subagent is restored
        restore_subagents(&session, &state, &persistence, None).await;

        // Then the child is NOT rehydrated (no empty-model agent is minted)
        assert!(
            session.get_agent(&AgentId::new(child_id)).await.is_none(),
            "a child with no resolvable model must not be rehydrated"
        );
    }

    /// Given a session whose replayed state says the live mode was auto,
    /// with the main agent recreated at that mode and a registered child,
    /// When the subagent is restored, Then the child also adjudicates at
    /// the restored mode instead of the config default (SDR-004 d).
    #[tokio::test]
    async fn should_reapply_restored_permission_mode_to_every_agent_when_subagents_are_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-mode"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        // The replay path (build_agent_config_from_state) restores the main
        // agent at the replayed mode; the child comes back at the default.
        // The main agent also carries a model — without one the child is
        // deliberately skipped rather than rehydrated (spec 11 A6).
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    permission_mode: PermissionMode::Auto,
                    model_id: "test-model".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-restored-child";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: None,
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });
        state.permission.mode = "auto".to_string();

        // When the subagent roster is restored (its agent stays dormant), then
        // it hydrates on demand
        restore_subagents(&session, &state, &persistence, None).await;
        hydrate_subagent_from_journal(&session, &persistence, child_id)
            .await
            .expect("the dormant child hydrates on demand");

        // Then both agents adjudicate at the restored mode
        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child rehydrated");
        assert_eq!(child.permission_manager().mode(), PermissionMode::Auto);
        let main = session
            .get_agent(&AgentId::new(MAIN_AGENT_ID))
            .await
            .expect("main agent");
        assert_eq!(main.permission_manager().mode(), PermissionMode::Auto);
    }

    /// Given a resumed session whose child journal mints a HIGH turn id,
    /// When the subagents are restored (their journals scanned for the
    /// turn-id ceiling only — no per-child fold, ADR-006/ADR-017), Then the
    /// returned ceiling reflects that id, so the session's allocator is
    /// seeded past it and never re-mints an id the child journal occupies —
    /// even though the child's agent stays dormant.
    #[tokio::test]
    async fn should_return_the_turn_id_ceiling_from_the_child_journal_scan_without_hydrating() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-ceiling"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    model_id: "test-model".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let serialized = WireOp::TurnEnded {
            turn_id: 27,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
        };
        let child_id = "agent-ceiling-1";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(
            &journal,
            serde_json::to_string(&WireOp::Metadata {
                protocol_version: None,
                created_at: None,
            })
            .unwrap()
                + "\n"
                + &serde_json::to_string(&serialized).unwrap()
                + "\n",
        )
        .unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });

        // When the subagents are restored (scan-only ceiling)
        let ceiling = restore_subagents(&session, &state, &persistence, None).await;

        // Then the ceiling reflects the child journal's high turn id, and the
        // child stays dormant (no per-child fold, no agent created).
        assert_eq!(
            ceiling, 27,
            "the child journal's max turn id raises the ceiling"
        );
        assert!(
            session.get_agent(&AgentId::new(child_id)).await.is_none(),
            "the scan must NOT create the child's agent (lazy restore)"
        );
        assert!(
            session.subagent_registration(child_id).await.is_some(),
            "the scan must keep the child in the roster"
        );
    }

    /// A minimal tool stand-in for the MCP-seeding tests — registered under an
    /// `mcp__<server>__<tool>` qualified name, never executed.
    struct StubMcpTool {
        name: String,
    }

    impl crate::Tool for StubMcpTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "stub MCP tool for hydration tests"
        }
        fn parameters(&self) -> &serde_json::Value {
            static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({}))
        }
        fn resolve_execution(
            &self,
            _input: serde_json::Value,
        ) -> crate::EngineResult<crate::ToolExecution> {
            unreachable!("the hydration tests never execute the stub MCP tool")
        }
    }

    /// Given a resumed session whose main agent carries MCP tools registered
    /// by the engine AFTER the resume (the fan-out the dormant child missed),
    /// When the child is hydrated, Then it receives the session's CURRENT MCP
    /// server set — the same effective surface the eager restore held and a
    /// live spawn holds.
    #[tokio::test]
    async fn should_seed_the_main_agents_current_mcp_tools_when_hydrating_a_child() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-mcp"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    model_id: "test-model".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        // The main agent's MCP table holds the engine's current set — here
        // two servers' tools, the shape a post-resume fan-out registers.
        {
            let tools = session
                .get_agent(&AgentId::new(MAIN_AGENT_ID))
                .await
                .unwrap()
                .tool_manager();
            let mut tools = tools.write();
            tools.register_mcp_tool(
                "linear".to_string(),
                Arc::new(StubMcpTool {
                    name: "mcp__linear__create_issue".to_string(),
                }),
            );
            tools.register_mcp_tool(
                "github".to_string(),
                Arc::new(StubMcpTool {
                    name: "mcp__github__list_prs".to_string(),
                }),
            );
        }
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-mcp-restore";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });

        // When the roster is restored and the child hydrated
        restore_subagents(&session, &state, &persistence, None).await;
        hydrate_subagent_from_journal(&session, &persistence, child_id)
            .await
            .expect("the dormant child hydrates on demand");

        // Then the child's tool table resolves both MCP tools the main agent
        // held at hydration time
        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child rehydrated");
        let tools = child.tool_manager();
        let tools = tools.read();
        assert!(
            tools.find_tool("mcp__linear__create_issue").is_some(),
            "the child must receive the linear MCP tool added after the resume"
        );
        assert!(
            tools.find_tool("mcp__github__list_prs").is_some(),
            "the child must receive the github MCP tool added after the resume"
        );
        assert_eq!(
            tools.mcp_count(),
            2,
            "the child's MCP table mirrors the main agent's current set"
        );
    }

    /// Given a child that was ALREADY created (another concurrent hydration
    /// trigger won the create race after this call's idempotence check
    /// passed), When the hydration runs again, Then it converges to success —
    /// the losing trigger must not report a false failure that pushes callers
    /// (e.g. the task driver's dispatcher resolution) onto the main-agent
    /// fallback.
    #[tokio::test]
    async fn should_converge_to_success_when_a_concurrent_hydration_already_created_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Arc::new(crate::scope::SessionScope::new(
            crate::SessionId::new("session-restore-race"),
            crate::SessionMetadata::default(),
            crate::SessionScopeConfig::default(),
            Arc::new(crate::SkillCatalog::new()),
            None,
            None,
        ));
        session
            .create_agent(
                AgentId::new(MAIN_AGENT_ID),
                AgentScopeConfig {
                    model_id: "test-model".to_string(),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("main agent");
        let persistence = SessionPersistence::new(
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        );
        let child_id = "agent-race-restore";
        let journal = persistence.agent_wire_log_path(session.id(), child_id);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "").unwrap();
        let mut state = mirri_proto::AgentState::new();
        state.subagents.push(SubagentRegistration {
            agent_id: child_id.to_string(),
            profile_name: "coder".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: None,
            parent_agent_id: Some(MAIN_AGENT_ID.to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        });
        restore_subagents(&session, &state, &persistence, None).await;

        // The "other trigger" creates the child between this hydration's
        // idempotence check and its create_agent — modeled by pre-creating
        // the agent with the same registration-derived config the hydrator
        // builds (the exact interleaving of two concurrent
        // hydrate_subagent_from_journal calls).
        session
            .create_agent(
                AgentId::new(child_id),
                AgentScopeConfig {
                    model_id: "test-model".to_string(),
                    profile_name: Some("coder".to_string()),
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("the concurrent trigger's create");

        // When the losing hydration runs, Then it returns Ok — the race
        // converges to success instead of surfacing "agent already exists"
        let outcome = hydrate_subagent_from_journal(&session, &persistence, child_id).await;
        assert!(
            outcome.is_ok(),
            "a lost create race must converge to success, got {outcome:?}"
        );
        assert!(
            session.get_agent(&AgentId::new(child_id)).await.is_some(),
            "the child stays live either way"
        );
    }

    // -----------------------------------------------------------------------
    // read_only_open_turn_started_at
    // -----------------------------------------------------------------------

    /// A manager whose persistence dir lives in a fresh tempdir, with the
    /// session's wire-log directory already created for a handwritten log.
    fn manager_with_session_dir() -> (tempfile::TempDir, Arc<SessionManager>, SessionId) {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Arc::new(crate::scope::WorkspaceScope::new(
            crate::WorkspaceId::new("open-turn-test"),
            tmp.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let session_id = SessionId::new("open-turn-session");
        let wire_log_path = manager.persistence().wire_log_path(&session_id);
        std::fs::create_dir_all(wire_log_path.parent().unwrap()).unwrap();
        (tmp, manager, session_id)
    }

    /// The wire-log JSONL shared by the open-turn tests: the `time` field is
    /// hand-written because `JsonlAppendLog::append` stamps it itself — these
    /// cases pin the exact record the persistence layer would have written.
    fn write_wire_log(manager: &SessionManager, session_id: &SessionId, lines: &[&str]) {
        let wire_log_path = manager.persistence().wire_log_path(session_id);
        std::fs::write(&wire_log_path, lines.join("\n") + "\n").unwrap();
    }

    /// Given a session whose wire log opens a turn (`turn.prompt`) with a
    /// stamped `time` and no `turn.ended` after it, When the open-turn start
    /// is read, Then the LAST prompt's time is returned — the anchor the
    /// snapshot stamps for the web's "Working for X" header.
    #[tokio::test]
    async fn should_return_the_open_turn_start_time_when_no_turn_ended_follows_the_last_prompt() {
        // Given an open turn: a completed turn first, then a fresh prompt
        // with a newer stamped time and no closing turn.ended
        let (_tmp, manager, session_id) = manager_with_session_dir();
        write_wire_log(
            &manager,
            &session_id,
            &[
                r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"},"time":1000}"#,
                r#"{"type":"context.append_message","message":{"role":"assistant"},"time":2000}"#,
                r#"{"type":"turn.ended","turnId":1,"reason":"completed","time":3000}"#,
                r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"},"time":4000}"#,
            ],
        );

        // When the open turn's start time is read
        let started_at = manager.read_only_open_turn_started_at(&session_id).await;

        // Then the second (still-open) turn's prompt time is the anchor
        assert_eq!(started_at, Some(4000));
    }

    /// Given a session whose wire log closes every turn it opens, When the
    /// open-turn start is read, Then None is returned — a closed session has
    /// no running turn to tick.
    #[tokio::test]
    async fn should_return_none_when_every_turn_has_closed() {
        // Given a log where both turn.prompt records have a matching
        // turn.ended after them
        let (_tmp, manager, session_id) = manager_with_session_dir();
        write_wire_log(
            &manager,
            &session_id,
            &[
                r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"},"time":1000}"#,
                r#"{"type":"turn.ended","turnId":1,"reason":"completed","time":2000}"#,
                r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"},"time":3000}"#,
                r#"{"type":"turn.ended","turnId":2,"reason":"completed","time":4000}"#,
            ],
        );

        // When the open turn's start time is read
        let started_at = manager.read_only_open_turn_started_at(&session_id).await;

        // Then there is no open turn to anchor
        assert_eq!(started_at, None);
    }

    /// Given a session whose open turn's prompt record carries no `time`
    /// (a legacy TS log that predates the persistence-layer stamp), When
    /// the open-turn start is read, Then None is returned — a fabricated
    /// anchor would show a wrong elapsed duration.
    #[tokio::test]
    async fn should_return_none_when_the_open_prompt_record_carries_no_time() {
        // Given an open turn whose turn.prompt record has no time field
        let (_tmp, manager, session_id) = manager_with_session_dir();
        write_wire_log(
            &manager,
            &session_id,
            &[r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"}}"#],
        );

        // When the open turn's start time is read
        let started_at = manager.read_only_open_turn_started_at(&session_id).await;

        // Then the read degrades to no anchor instead of guessing
        assert_eq!(started_at, None);
    }

    /// Given a session with no wire log at all, When the open-turn start is
    /// read, Then None is returned (the caller never fails on the miss).
    #[tokio::test]
    async fn should_return_none_when_the_wire_log_is_missing() {
        // Given a manager whose session has no wire log yet
        let (_tmp, manager, session_id) = manager_with_session_dir();

        // When the open turn's start time is read
        let started_at = manager.read_only_open_turn_started_at(&session_id).await;

        // Then the read is an honest None
        assert_eq!(started_at, None);
    }
}
