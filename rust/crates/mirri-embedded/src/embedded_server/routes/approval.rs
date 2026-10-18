//! Approval-domain routes — the B4-a blocking-HITL surface (`approval.*`)
//! plus the B9 user-level permission rule CRUD.
//! `approval.decide` mirrors mirri-server's handler wire-for-wire;
//! `approval.listPending` serves the klient `approvalRequest[]` shape;
//! `approval.listRules` / `addRule` / `removeRule` manage the user-scope
//! rules persisted in the mirri home config.toml.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Resolve a pending tool-approval request through the session's gate
    // (the SRV wire shape: `approved` + optional `reason`, `{ok: true}`).
    Route {
        method: "approval.decide",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_approval_decide(params)),
    },
    // The session's pending approvals (klient `approvalRequest[]`, bare
    // array).
    Route {
        method: "approval.listPending",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_approval_list_pending(params)),
    },
    // B9: the user-level permission rules of the shared config snapshot.
    Route {
        method: "approval.listRules",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_approval_list_rules(params)),
    },
    // B9: append one user-scope rule through the config.toml write pipeline
    // and push the new rule set into every live agent.
    Route {
        method: "approval.addRule",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_approval_add_rule(params)),
    },
    // B9: remove one user-scope rule by {pattern, decision}.
    Route {
        method: "approval.removeRule",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_approval_remove_rule(params)),
    },
];
