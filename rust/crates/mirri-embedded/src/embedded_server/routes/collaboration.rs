//! Collaboration-domain routes — the `collaboration.*` mode gate for the
//! `Collaboration*` builtin tools. Mirrors the swarm routes: the host
//! enters/exits collaboration mode per session, and the handlers toggle
//! the session's collaboration-mode flag plus the tool set's visibility.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "collaboration.enter",
        aliases: &["enterCollaboration"],
        handler: |server, params| Box::pin(server.dispatch_enter_collaboration(params)),
    },
    Route {
        method: "collaboration.exit",
        aliases: &["exitCollaboration"],
        handler: |server, params| Box::pin(server.dispatch_exit_collaboration(params)),
    },
];
