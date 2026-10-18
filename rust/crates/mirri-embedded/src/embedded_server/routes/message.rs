//! Message-domain routes — the 3 `message.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 3). Doc comments moved
//! with their arms; handlers and inline values are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Records a user message and runs one engine turn (wait/fire).
    Route {
        method: "message.send",
        aliases: &["prompt"],
        handler: |server, params| Box::pin(server.dispatch_send_message(params)),
    },
    // Cancels in-flight fire-mode turns of the session.
    Route {
        method: "message.abort",
        aliases: &["cancel"],
        handler: |server, params| Box::pin(server.dispatch_cancel(params)),
    },
    // Steers the agent's in-flight turn flow with new content.
    Route {
        method: "message.steer",
        aliases: &["steer"],
        handler: |server, params| Box::pin(server.dispatch_steer(params)),
    },
];
