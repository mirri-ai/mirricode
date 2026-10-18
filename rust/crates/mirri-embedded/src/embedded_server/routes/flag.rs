//! Flag-domain routes — the B5-a `flagService` mirror: the five
//! experimental-flag reads over the engine's `FlagRegistry` (klient rich
//! shapes, with the honest degradations documented in the handlers).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Whether a flag id is enabled (unregistered ids are false).
    Route {
        method: "flag.enabled",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_flag_enabled(params)),
    },
    // The registered flags' effective states as a record (id → bool).
    Route {
        method: "flag.snapshot",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_flag_snapshot()),
    },
    // The ids whose effective state is enabled.
    Route {
        method: "flag.enabledIds",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_flag_enabled_ids()),
    },
    // One flag's rich state, or JSON null for an unknown id (klient maybe).
    Route {
        method: "flag.explain",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_flag_explain(params)),
    },
    // Every registered flag's rich state, sorted by id.
    Route {
        method: "flag.explainAll",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_flag_explain_all()),
    },
];
