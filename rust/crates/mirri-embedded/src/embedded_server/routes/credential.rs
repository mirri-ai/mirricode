//! Credential-domain routes — the provider-credential store surface
//! (Task 5): `credential.list` / `credential.set` /
//! `credential.remove` / `credential.status`, the CRUD +
//! health rows registered with the config domain in the authoritative
//! registry. Both faces (ADR-010): the serve wire rides the engine
//! fall-through to these handlers. No plaintext ever crosses the wire
//! (F.1) — the CLI is the only reveal face (SDR-002/ADR-003).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // List the store's entries as {provider_id, type, time_created}
    // records — no key material, ever.
    Route {
        method: "credential.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_list(params)),
        settlement: Route::SETTLES,
    },
    // Create/replace one provider's credential from the tagged
    // CredentialValue union; unknown type tags are invalid params.
    Route {
        method: "credential.set",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_set(params)),
        settlement: Route::SETTLES,
    },
    // Remove one provider's credential (a missing entry is a successful
    // no-op).
    Route {
        method: "credential.remove",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_remove(params)),
        settlement: Route::SETTLES,
    },
    // The health face over the in-memory auth-failure map: normal by
    // default, needs_reconfigure after a recorded failure, cleared by
    // any successful write.
    Route {
        method: "credential.status",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_status(params)),
        settlement: Route::SETTLES,
    },
];
