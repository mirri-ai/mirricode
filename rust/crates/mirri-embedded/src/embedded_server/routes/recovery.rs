//! Interrupted-task recovery routes — the 5 `recovery.*` arms of the
//! recovery notification card (interrupted-task-recovery spec §3).
//! Rust-first wires: no klient RPC counterpart (the TS surface lands with
//! the web batch), both faces ride the same engine — the embedded route
//! and the serve wire's engine fall-through.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // The card's pull source: the in-process interruption list (rows
    // carry session/workspace/title, the interruption moment, the cause,
    // and the pending-background hint; expiresAt = the retention
    // boundary).
    Route {
        method: "recovery.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_list(params)),
    },
    // Resume one interruption (E.2): settle, ensure resident, then the
    // recovery-origin fire turn (E.3's stop-moment text + background-task
    // check instruction). Unknown or settled id answers not_found.
    Route {
        method: "recovery.resume",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_resume(params)),
    },
    // Resume every live interruption (C.4): sequential fire turns, the
    // queued counter a batch-2 placeholder.
    Route {
        method: "recovery.resumeAll",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_resume_all(params)),
    },
    // Dismiss one interruption (E.5): settle without driving a turn.
    Route {
        method: "recovery.dismiss",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_dismiss(params)),
    },
    // Dismiss every interruption (C.4's bulk arm).
    Route {
        method: "recovery.dismissAll",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_dismiss_all(params)),
    },
];
