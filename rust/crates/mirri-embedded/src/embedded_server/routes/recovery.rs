//! Interrupted-task recovery routes — the 5 `recovery.*` arms of the
//! recovery notification card (interrupted-task-recovery spec §3).
//! Rust-first wires: no klient RPC counterpart (the TS surface lands with
//! the web batch), both faces ride the same engine — the embedded route
//! and the serve wire's engine fall-through.

use super::{Route, SettlementClass};

pub(super) const ROUTES: &[Route] = &[
    // The card's pull source: the in-process interruption list (rows
    // carry session/workspace/title, the interruption moment, the cause,
    // and the pending-background hint; expiresAt = the retention
    // boundary).
    //
    // ADR-003 c recovery gate: the rows must be the settled ones, so this
    // route waits for the recovery check (not exempt — the whitelist is
    // the four read-face methods, not the recoverable-session faces).
    Route {
        method: "recovery.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_list(params)),
        settlement: SettlementClass::AwaitRecovery,
    },
    // Resume one interruption (E.2): settle, ensure resident, then the
    // recovery-origin fire turn (E.3's stop-moment text + background-task
    // check instruction). Unknown or settled id answers not_found.
    Route {
        method: "recovery.resume",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_resume(params)),
        settlement: Route::SETTLES,
    },
    // Resume every live interruption (C.4): sequential fire turns, the
    // queued counter a batch-2 placeholder.
    Route {
        method: "recovery.resumeAll",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_resume_all(params)),
        settlement: Route::SETTLES,
    },
    // Dismiss one interruption (E.5): settle without driving a turn.
    Route {
        method: "recovery.dismiss",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_dismiss(params)),
        settlement: Route::SETTLES,
    },
    // Dismiss every interruption (C.4's bulk arm).
    Route {
        method: "recovery.dismissAll",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_recovery_dismiss_all(params)),
        settlement: Route::SETTLES,
    },
];
