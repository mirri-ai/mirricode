//! Credential-domain routes — the provider-credential store surface
//! (Task 5): `credential.list` / `credential.set` /
//! `credential.remove` / `credential.status`, the embedded-only CRUD +
//! health rows registered with the config domain in the authoritative
//! registry. No plaintext ever crosses the wire (F.1) — the CLI is the
//! only reveal face (SDR-002/ADR-003).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // List the store's entries as {provider_id, type, time_created}
    // records — no key material, ever.
    Route {
        method: "credential.list",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_list(params)),
    },
    // Create/replace one provider's credential from the tagged
    // CredentialValue union; unknown type tags are invalid params.
    Route {
        method: "credential.set",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_set(params)),
    },
    // Remove one provider's credential (a missing entry is a successful
    // no-op).
    Route {
        method: "credential.remove",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_remove(params)),
    },
    // The health face over the in-memory auth-failure map: normal by
    // default, needs_reconfigure after a recorded failure, cleared by
    // any successful write.
    Route {
        method: "credential.status",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_credential_status(params)),
    },
];
