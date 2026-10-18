//! Model-domain routes — the B5-b klient `modelService` record methods
//! (`model.get` / `model.set` / `model.delete`) and the `modelResolver`
//! catalog methods (`model.getProvider` / `model.setDefaultModel`), next to
//! the Settled `model.list` / `model.listProviders` reads.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "model.get",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_get(params)),
    },
    Route {
        method: "model.set",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_set(params)),
    },
    Route {
        method: "model.delete",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_delete(params)),
    },
    Route {
        method: "model.generate",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_generate(params)),
    },
    Route {
        method: "model.getProvider",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_get_provider(params)),
    },
    Route {
        method: "model.setDefaultModel",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_model_set_default_model(params)),
    },
];
