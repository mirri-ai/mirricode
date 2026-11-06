//! Config-domain routes — the B5-b klient `configService` section methods
//! (`config.getAll` / `config.inspect` / `config.replace` /
//! `config.replaceSections` / `config.reload`), riding the same config.toml
//! write pipeline the Settled `config.get` / `config.set` handlers use.

use super::{Route, SettlementClass};

pub(super) const ROUTES: &[Route] = &[
    // ADR-003 c read-face whitelist: reads the in-memory config snapshot
    // only — no reconcile or recovery ordering applies; the census wait
    // stays (ADR-012 c's ledger is unconditional).
    Route {
        method: "config.getAll",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_config_get_all()),
        settlement: SettlementClass::AwaitCensus,
    },
    Route {
        method: "config.inspect",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_inspect(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "config.reload",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_config_reload()),
        settlement: Route::SETTLES,
    },
    Route {
        method: "config.replace",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_replace(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "config.replaceSections",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_replace_sections(params)),
        settlement: Route::SETTLES,
    },
];
