//! Config-domain routes — the B5-b klient `configService` section methods
//! (`config.getAll` / `config.inspect` / `config.replace` /
//! `config.replaceSections` / `config.reload`), riding the same config.toml
//! write pipeline the Settled `config.get` / `config.set` handlers use.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "config.getAll",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_config_get_all()),
    },
    Route {
        method: "config.inspect",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_inspect(params)),
    },
    Route {
        method: "config.reload",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_config_reload()),
    },
    Route {
        method: "config.replace",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_replace(params)),
    },
    Route {
        method: "config.replaceSections",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_config_replace_sections(params)),
    },
];
