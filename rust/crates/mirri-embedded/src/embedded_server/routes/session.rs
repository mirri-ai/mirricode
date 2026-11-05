//! Session-domain routes — the 25 `session.*` arms: the 20 migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 1), the M5-3b
//! `session.get` EMB backfill, the three B1 gap implementations
//! (`countActive` / `createChild` / `registerAgent`), and the B1
//! `session.restore` EMB backfill. Doc comments moved
//! with their arms; handlers and inline values are unchanged.

use super::{Route, SettlementClass, not_implemented};

pub(super) const ROUTES: &[Route] = &[
    // Returns the session's wire view; provisions the default main agent.
    Route {
        method: "session.create",
        aliases: &["createSession"],
        handler: |server, params| Box::pin(server.dispatch_create_session(params)),
        settlement: Route::SETTLES,
    },
    // Returns an array of session summary views. Accepts the optional
    // `includeArchive` param (and its snake_case alias) to keep archived
    // sessions in the listing; the default excludes them (TS parity).
    //
    // ADR-003 c read-face whitelist: the listing skips the reconcile
    // (a row not yet converged is filled in by the idempotent converge —
    // session-consistency ADR-004's ruling reversal) and the recovery
    // check; it still waits for the census, since a concurrent census must
    // never sweep this instance's own tasks (ADR-012 c).
    Route {
        method: "session.list",
        aliases: &["listSessions"],
        handler: |server, params| Box::pin(server.dispatch_list_sessions(params)),
        settlement: SettlementClass::AwaitCensus,
    },
    // Replays a closed session's wire log; returns the wire view.
    Route {
        method: "session.resume",
        aliases: &["resumeSession"],
        handler: |server, params| Box::pin(server.dispatch_resume_session(params)),
        // ADR-006: the resume family's on-demand converge is retired —
        // the one-shot migration seeds rows inside the census settlement.
        // These routes build a runtime / carry a write face, so they keep
        // the full housekeeping class.
        settlement: Route::SETTLES,
    },
    // A live session needs no reload in the embedded engine; a closed
    // one is resumed. Returns the wire view.
    Route {
        method: "session.reload",
        aliases: &["reloadSession"],
        handler: |server, params| Box::pin(server.dispatch_reload_session(params)),
        // ADR-006: the resume family's on-demand converge is retired —
        // the one-shot migration seeds rows inside the census settlement.
        // These routes build a runtime / carry a write face, so they keep
        // the full housekeeping class.
        settlement: Route::SETTLES,
    },
    // Closes the session (user-initiated); returns `{ ok: true }`.
    Route {
        method: "session.close",
        aliases: &["closeSession"],
        handler: |server, params| Box::pin(server.dispatch_close_session(params)),
        settlement: Route::SETTLES,
    },
    // No archive store in the embedded engine; an existing session is
    // closed with its wire log kept on disk. Returns JSON `null` (the
    // klient noResult spelling).
    Route {
        method: "session.archive",
        aliases: &["archiveSession"],
        handler: |server, params| Box::pin(server.dispatch_archive_session(params)),
        settlement: Route::SETTLES,
    },
    // B1 EMB backfill: restore (un-archive) the persisted session — the
    // resume path over wire.jsonl; returns the wire view (the same shape
    // the SRV transport handler serves).
    Route {
        method: "session.restore",
        aliases: &["restoreSession"],
        handler: |server, params| Box::pin(server.dispatch_restore_session(params)),
        // ADR-006: the resume family's on-demand converge is retired —
        // the one-shot migration seeds rows inside the census settlement.
        // These routes build a runtime / carry a write face, so they keep
        // the full housekeeping class.
        settlement: Route::SETTLES,
    },
    // Forks the session's first agent into a new session.
    Route {
        method: "session.fork",
        aliases: &["forkSession"],
        handler: |server, params| Box::pin(server.dispatch_fork_session(params)),
        settlement: Route::SETTLES,
    },
    // Renames the session; returns the updated wire view.
    Route {
        method: "session.rename",
        aliases: &["renameSession"],
        handler: |server, params| Box::pin(server.dispatch_rename_session(params)),
        settlement: Route::SETTLES,
    },
    // SDR-014 recency touch — user-interaction-only: bumps lastUsedAt
    // (the sidebar sort key) without touching updatedAt; returns the
    // updated wire view.
    Route {
        method: "session.touch",
        aliases: &["touchSession"],
        handler: |server, params| Box::pin(server.dispatch_touch_session(params)),
        settlement: Route::SETTLES,
    },
    // SDR-016/017 pin — the pin trio rides the metadata row (ADR-006);
    // a pin is not usage, so the recency keys stay untouched.
    Route {
        method: "session.pin",
        aliases: &["pinSession"],
        handler: |server, params| Box::pin(server.dispatch_pin_session(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "session.unpin",
        aliases: &["unpinSession"],
        handler: |server, params| Box::pin(server.dispatch_unpin_session(params)),
        settlement: Route::SETTLES,
    },
    // Updates tags (and title) of the session; returns the wire view.
    Route {
        method: "session.updateMeta",
        aliases: &["updateSessionMetadata"],
        handler: |server, params| Box::pin(server.dispatch_update_session_metadata(params)),
        settlement: Route::SETTLES,
    },
    // Returns the session's metadata (id, title, agents, tags).
    Route {
        method: "session.getMeta",
        aliases: &["getSessionMetadata"],
        handler: |server, params| Box::pin(server.dispatch_get_session_metadata(params)),
        settlement: Route::SETTLES,
    },
    // The B1 EMB backfill: summary view + agent list (same wire shape the
    // mirri-server transport handler serves).
    Route {
        method: "session.get",
        aliases: &["getSession"],
        handler: |server, params| Box::pin(server.dispatch_get_session(params)),
        // ADR-006: the resume family's on-demand converge is retired —
        // the one-shot migration seeds rows inside the census settlement,
        // so this pure read face joins the audited whitelist.
        settlement: SettlementClass::AwaitCensus,
    },
    // Exports the session's persisted files to a zip archive.
    Route {
        method: "session.export",
        aliases: &["exportSession"],
        handler: |server, params| Box::pin(server.dispatch_export_session(params)),
        settlement: Route::SETTLES,
    },
    // Session-level warnings; the engine records none yet.
    Route {
        method: "session.warnings",
        aliases: &["getSessionWarnings"],
        handler: |server, params| Box::pin(server.dispatch_get_session_warnings(params)),
        settlement: Route::SETTLES,
    },
    // Clears the addressed agent's context memory.
    Route {
        method: "session.clearContext",
        aliases: &["clearContext"],
        handler: |server, params| Box::pin(server.dispatch_clear_context(params)),
        settlement: Route::SETTLES,
    },
    // The addressed agent's full context history (serialized
    // ContextMessages — the load-replay read face, named after the
    // agent.getContext domain it mirrors).
    Route {
        method: "session.getHistory",
        aliases: &["getHistory"],
        handler: |server, params| Box::pin(server.dispatch_get_history(params)),
        settlement: Route::SETTLES,
    },
    // The P10 global literal search read face (title + user/assistant
    // message text, NFKC + case folded, cursor paginated).
    Route {
        method: "session.search",
        aliases: &["searchSessions"],
        handler: |server, params| Box::pin(server.dispatch_search(params)),
        settlement: Route::SETTLES,
    },
    // Undoes the last `count` user-origin messages of the agent.
    Route {
        method: "session.undo",
        aliases: &["undoHistory"],
        handler: |server, params| Box::pin(server.dispatch_undo_history(params)),
        settlement: Route::SETTLES,
    },
    // Manual compaction: fire-and-forget background task.
    Route {
        method: "session.beginCompaction",
        aliases: &["beginCompaction"],
        handler: |server, params| Box::pin(server.dispatch_begin_compaction(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "session.cancelCompaction",
        aliases: &["cancelCompaction"],
        handler: |server, params| Box::pin(server.dispatch_cancel_compaction(params)),
        settlement: Route::SETTLES,
    },
    // Has no subagent-spawn facility to author AGENTS.md; explicit
    // not_implemented instead of the former silent acknowledgement.
    Route {
        method: "session.generateAgentsMd",
        aliases: &["generateAgentsMd"],
        handler: |_server, _params| {
            Box::pin(async {
                Err(not_implemented(
                    "session.generateAgentsMd is not implemented by the embedded engine: it would require spawning a subagent to author AGENTS.md",
                ))
            })
        },
        settlement: Route::SETTLES,
    },
    // Print-mode drain wait (B1 D-59): waits — bounded by
    // `task.printWaitCeilingS` — for the session's background tasks to
    // reach terminal status; returns `{ waited, remaining }`.
    Route {
        method: "session.waitForBackgroundTasksOnPrint",
        aliases: &["waitForBackgroundTasksOnPrint"],
        handler: |server, params| {
            Box::pin(server.dispatch_wait_for_background_tasks_on_print(params))
        },
        settlement: Route::SETTLES,
    },
    // The print-completion decision (B1 D-59): `finish` or `continue`
    // per the resolved print background mode (exit / drain / steer).
    Route {
        method: "session.handlePrintMainTurnCompleted",
        aliases: &["handlePrintMainTurnCompleted"],
        handler: |server, params| {
            Box::pin(server.dispatch_handle_print_main_turn_completed(params))
        },
        settlement: Route::SETTLES,
    },
    // Additional dirs are not supported; reports the workspace root.
    Route {
        method: "session.addAdditionalDir",
        aliases: &["addAdditionalDir"],
        handler: |server, params| Box::pin(server.dispatch_add_additional_dir(params)),
        settlement: Route::SETTLES,
    },
    // B1 gap implementations (klient references): the live-session count,
    // the child-session fork, and the agent registration.
    Route {
        method: "session.countActive",
        aliases: &["countActiveSessions"],
        handler: |server, params| Box::pin(server.dispatch_count_active_sessions(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "session.createChild",
        aliases: &["createChildSession"],
        handler: |server, params| Box::pin(server.dispatch_create_child_session(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "session.registerAgent",
        aliases: &["registerAgent"],
        handler: |server, params| Box::pin(server.dispatch_register_agent(params)),
        settlement: Route::SETTLES,
    },
];
