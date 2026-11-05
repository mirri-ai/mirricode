//! Task/cron routes — the 4 `task.*` arms plus the single `cron.list` arm
//! migrated from `EngineAdapter::call_method`'s match (M3 batch 5, the
//! final batch). Doc comments moved with their arms; handlers and inline
//! values are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // The session's background tasks.
    Route {
        method: "task.list",
        aliases: &["getBackground"],
        handler: |server, params| Box::pin(server.dispatch_get_background(params)),
        settlement: Route::SETTLES,
    },
    // Stops one background task of the session.
    Route {
        method: "task.cancel",
        aliases: &["stopBackground"],
        handler: |server, params| Box::pin(server.dispatch_stop_background(params)),
        settlement: Route::SETTLES,
    },
    // The accumulated output preview of one background task.
    Route {
        method: "task.output",
        aliases: &["getBackgroundOutput"],
        handler: |server, params| Box::pin(server.dispatch_get_background_output(params)),
        settlement: Route::SETTLES,
    },
    // Converts a running foreground `!` shell command into a background
    // task (the TS taskService.detach semantics, realized over the shell
    // runner's shared conversion point — the process keeps running under
    // the task id the shell.started event carried). B7 added the
    // toolCallId face: a foreground model tool call (the Bash tool's
    // tracked foreground registration) converts the same way.
    Route {
        method: "task.detach",
        aliases: &["detachBackground"],
        handler: |server, params| Box::pin(server.dispatch_detach_task(params)),
        settlement: Route::SETTLES,
    },
    // B2 gap implementation: the task.cancel sister operation — stop one
    // background task with a reason, returning the stopped task.
    Route {
        method: "task.stop",
        aliases: &["stopTask"],
        handler: |server, params| Box::pin(server.dispatch_stop_task(params)),
        settlement: Route::SETTLES,
    },
    // B7 workspace task overview (P-023 / D-1): the live-session task
    // aggregation over every built workspace runtime — the same AgentTask
    // element shape task.list serves, workspaceId-scoped.
    Route {
        method: "task.listWorkspace",
        aliases: &["listWorkspaceTasks"],
        handler: |server, params| Box::pin(server.dispatch_list_workspace_tasks(params)),
        settlement: Route::SETTLES,
    },
    // The session's cron tasks.
    Route {
        method: "cron.list",
        aliases: &["getCronTasks"],
        handler: |server, params| Box::pin(server.dispatch_get_cron_tasks(params)),
        settlement: Route::SETTLES,
    },
    // The user-creation face (E.1 / A.1, stage-1 batch-3): the management
    // panel's direct create — origin `User`, any fire budget (the
    // model-side gate hangs on the tool face only), the structural
    // validations intact.
    Route {
        method: "cron.add",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_add_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // The cross-session aggregated listing (E.5 / V8a): every existing
    // session's tasks in one view, grouped by session, with honest open
    // flags — visibility is global, delivery stays session-scoped.
    Route {
        method: "cron.listAll",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_list_all_cron_tasks(params)),
        settlement: Route::SETTLES,
    },
    // B6 cron management write face — remove one cron task by id (the
    // durable cron.delete wire op, write-before-mutate; unknown ids
    // answer not_found).
    Route {
        method: "cron.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_delete_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // B6 cron management run-now face — fire one task immediately through
    // the driver's cron_job injection path, schedule untouched (D-2);
    // fire-and-forget ack.
    Route {
        method: "cron.trigger",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_trigger_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // Stage-2 4b: one workspace task's run records (C.2) — the panel's
    // consumption face.
    Route {
        method: "cron.runs.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_list_cron_runs(params)),
        settlement: Route::SETTLES,
    },
    // Stage-2 4b: the explicit-open path (B.2/C.4, ADR-008) — opening a
    // run record materializes the execution session into the index.
    Route {
        method: "cron.run.open",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_open_cron_run(params)),
        settlement: Route::SETTLES,
    },
    // Batch 2 (A9): edit one workspace task in place — the same task id
    // and history, new config; the workspace is not editable.
    Route {
        method: "cron.update",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_update_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // Batch 2 (A9): pause one workspace task (it stops firing and the
    // paused window leaves no trace in the history).
    Route {
        method: "cron.pause",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_pause_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // Batch 2 (A9): resume a paused task from the next FUTURE occurrence
    // (no catch-up replay).
    Route {
        method: "cron.resume",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_resume_cron_task(params)),
        settlement: Route::SETTLES,
    },
    // Form batch 6 (ADR-017): the read-only preview face — the next N
    // occurrences of a schedule (expression or interval), the same
    // kernel check_due plans with; no task side effects, not gated on
    // MIRRICODE_DISABLE_CRON (a display projection).
    Route {
        method: "cron.preview",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_preview_cron(params)),
        settlement: Route::SETTLES,
    },
];
