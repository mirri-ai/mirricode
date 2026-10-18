//! Swarm-domain routes — the 3 `swarm.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 5, the final batch).
//! Doc comments moved with their arms; handlers are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "swarm.enter",
        aliases: &["enterSwarm"],
        handler: |server, params| Box::pin(server.dispatch_enter_swarm(params)),
    },
    Route {
        method: "swarm.exit",
        aliases: &["exitSwarm"],
        handler: |server, params| Box::pin(server.dispatch_exit_swarm(params)),
    },
    // Returns a plain boolean (the TS `getSwarmMode` wire type).
    Route {
        method: "swarm.get",
        aliases: &["getSwarmMode"],
        handler: |server, params| Box::pin(server.dispatch_get_swarm_mode(params)),
    },
];
