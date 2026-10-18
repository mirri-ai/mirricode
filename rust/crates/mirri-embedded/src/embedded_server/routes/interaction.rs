//! Interaction-domain routes — the B4-a blocking-HITL kernel surface
//! (`interaction.*`): the merged pending list, the by-id respond routing,
//! and the recently-resolved memory, all in the klient shapes.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // The session's merged pending interactions (klient envelope with
    // `kind` / `payload` / `origin` / `createdAt`).
    Route {
        method: "interaction.listPending",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_interaction_list_pending(params)),
    },
    // Respond to one pending interaction by id (approval → decision,
    // question → answer; JSON null result).
    Route {
        method: "interaction.respond",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_interaction_respond(params)),
    },
    // Whether the request id was resolved recently (plain boolean).
    Route {
        method: "interaction.isRecentlyResolved",
        aliases: &[],
        handler: |server, params| {
            Box::pin(server.dispatch_interaction_is_recently_resolved(params))
        },
    },
];
