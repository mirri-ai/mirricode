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
    },
    // Stops one background task of the session.
    Route {
        method: "task.cancel",
        aliases: &["stopBackground"],
        handler: |server, params| Box::pin(server.dispatch_stop_background(params)),
    },
    // The accumulated output preview of one background task.
    Route {
        method: "task.output",
        aliases: &["getBackgroundOutput"],
        handler: |server, params| Box::pin(server.dispatch_get_background_output(params)),
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
    },
    // B2 gap implementation: the task.cancel sister operation — stop one
    // background task with a reason, returning the stopped task.
    Route {
        method: "task.stop",
        aliases: &["stopTask"],
        handler: |server, params| Box::pin(server.dispatch_stop_task(params)),
    },
    // B7 workspace task overview (P-023 / D-1): the live-session task
    // aggregation over every built workspace runtime — the same AgentTask
    // element shape task.list serves, workspaceId-scoped.
    Route {
        method: "task.listWorkspace",
        aliases: &["listWorkspaceTasks"],
        handler: |server, params| Box::pin(server.dispatch_list_workspace_tasks(params)),
    },
    // The session's cron tasks.
    Route {
        method: "cron.list",
        aliases: &["getCronTasks"],
        handler: |server, params| Box::pin(server.dispatch_get_cron_tasks(params)),
    },
    // B6 cron management write face — remove one cron task by id (the
    // durable cron.delete wire op, write-before-mutate; unknown ids
    // answer not_found).
    Route {
        method: "cron.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_delete_cron_task(params)),
    },
    // B6 cron management run-now face — fire one task immediately through
    // the driver's cron_job injection path, schedule untouched (D-2);
    // fire-and-forget ack.
    Route {
        method: "cron.trigger",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_trigger_cron_task(params)),
    },
];
