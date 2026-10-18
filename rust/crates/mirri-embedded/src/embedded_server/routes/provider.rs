//! Provider-domain routes — the providerDiscovery / providerService
//! surface: the B5-a `providerDiscovery.refreshProviderModels` read, the
//! P6 B4a `providerDiscovery.listCatalog` directory browse (G-8 前半), the
//! P7 B4b `providerDiscovery.importCatalog` directory import (G-8 后半),
//! plus the B5-b `provider.*` CRUD rows (`provider.get` / `provider.list` /
//! `provider.set` / `provider.delete`).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Refresh the engine's remote provider-model knowledge by pulling the
    // models.dev directory (fetch, cache under the home, merge into
    // model.list; a failed fetch degrades to the cache + a
    // config.diagnostics warning, never an RPC error).
    Route {
        method: "providerDiscovery.refreshProviderModels",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_refresh_provider_models(params)),
    },
    // Browse the models.dev directory (read-only in-memory/disk cache,
    // zero network; the empty array is the honest not-yet-refreshed
    // signal — C1/US-5 degradation).
    Route {
        method: "providerDiscovery.listCatalog",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_list_provider_catalog(params)),
    },
    // Import one models.dev directory entry as a configured provider plus
    // its full minimal-alias set in one config write transaction.
    Route {
        method: "providerDiscovery.importCatalog",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_import_provider_catalog(params)),
    },
    // The B5-b providerService CRUD (klient providerConfigSchema records).
    Route {
        method: "provider.get",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_provider_get(params)),
    },
    Route {
        method: "provider.list",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_provider_list()),
    },
    Route {
        method: "provider.set",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_provider_set(params)),
    },
    Route {
        method: "provider.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_provider_delete(params)),
    },
];
