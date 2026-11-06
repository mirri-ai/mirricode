//! Workspace-domain routes — the seven B4-b/W8 arms: the four EMB backfills
//! of the SRV-shape handlers (`workspace.list` / `createOrTouch` / `update` /
//! `delete`, byte-for-byte wire mirrors), the two gap implementations
//! (`workspace.get` / `handlerFor`, domain-consistent shapes), and the W8
//! trust management face (`workspace.setTrust`).

use super::{Route, SettlementClass};

pub(super) const ROUTES: &[Route] = &[
    // The bound workspace's summary: a bare `[{id, cwd}]` array (SRV wire
    // mirror).
    // ADR-003 c read-face whitelist: the registry's rows plus per-row
    // session counts from the live managers / on-disk buckets — reads
    // only, no task registration or session residency; the census wait
    // stays (ADR-012 c's ledger is unconditional).
    Route {
        method: "workspace.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_list_workspaces(params)),
        settlement: SettlementClass::AwaitCensus,
    },
    // Honest ack of a create/touch: the bound workspace's info with the
    // requested name echoed (SRV wire mirror).
    Route {
        method: "workspace.createOrTouch",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_create_workspace(params)),
        settlement: Route::SETTLES,
    },
    // Honest not-applied ack of a rename (SRV wire mirror).
    Route {
        method: "workspace.update",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_update_workspace(params)),
        settlement: Route::SETTLES,
    },
    // Honest not-applied ack of a delete (SRV wire mirror).
    Route {
        method: "workspace.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_delete_workspace(params)),
        settlement: Route::SETTLES,
    },
    // The bound workspace's WorkspaceView, or null for an unknown id (the
    // klient `maybe` semantics; domain-consistent shape).
    Route {
        method: "workspace.get",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_get_workspace(params)),
        settlement: Route::SETTLES,
    },
    // The bound workspace's scope handle `{id, kind}` (klient
    // `handleWireSchema` loose shape; single-bound semantics).
    Route {
        method: "workspace.handlerFor",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_workspace_handler_for(params)),
        settlement: Route::SETTLES,
    },
    // The W8 trust management face: `{workspaceId | workspace_id, trusted}`
    // → `{trusted}` — the TS v2 REST trust face's shape, driving the
    // home-level trust store and the live gate/linkage flip.
    Route {
        method: "workspace.setTrust",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_set_workspace_trust(params)),
        settlement: Route::SETTLES,
    },
];
