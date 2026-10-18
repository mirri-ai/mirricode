//! Plan-domain routes — the 4 `plan.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 5, the final batch).
//! Doc comments moved with their arms; handlers are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "plan.enter",
        aliases: &["enterPlan"],
        handler: |server, params| Box::pin(server.dispatch_enter_plan(params)),
    },
    Route {
        method: "plan.cancel",
        aliases: &["cancelPlan"],
        handler: |server, params| Box::pin(server.dispatch_cancel_plan(params)),
    },
    Route {
        method: "plan.clear",
        aliases: &["clearPlan"],
        handler: |server, params| Box::pin(server.dispatch_clear_plan(params)),
    },
    // (renamed from the embedded `plan.get` in M3-c, D4 §2; the old wire
    // name is gone, so it is not kept as an alias — getPlan remains as the
    // pre-existing v1 legacy alias).
    Route {
        method: "plan.status",
        aliases: &["getPlan"],
        handler: |server, params| Box::pin(server.dispatch_get_plan(params)),
    },
];
