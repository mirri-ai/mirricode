//! Goal-domain routes — the 5 `goal.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 5, the final batch).
//! Handlers are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "goal.create",
        aliases: &["createGoal"],
        handler: |server, params| Box::pin(server.dispatch_create_goal(params)),
    },
    Route {
        method: "goal.get",
        aliases: &["getGoal"],
        handler: |server, params| Box::pin(server.dispatch_get_goal(params)),
    },
    Route {
        method: "goal.pause",
        aliases: &["pauseGoal"],
        handler: |server, params| Box::pin(server.dispatch_pause_goal(params)),
    },
    Route {
        method: "goal.resume",
        aliases: &["resumeGoal"],
        handler: |server, params| Box::pin(server.dispatch_resume_goal(params)),
    },
    Route {
        method: "goal.cancel",
        aliases: &["cancelGoal"],
        handler: |server, params| Box::pin(server.dispatch_cancel_goal(params)),
    },
];
