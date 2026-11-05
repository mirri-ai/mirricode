//! Plugin-domain routes — the 10 `plugin.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 4, plus the Task-7
//! `plugin.checkUpdates` gap implementation). Doc comments moved
//! with their arms; handlers and inline values are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "plugin.list",
        aliases: &["listPlugins"],
        handler: |server, params| Box::pin(server.dispatch_plugin_list(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.install",
        aliases: &["installPlugin"],
        handler: |server, params| Box::pin(server.dispatch_plugin_install(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.setEnabled",
        aliases: &["setPluginEnabled"],
        handler: |server, params| Box::pin(server.dispatch_plugin_set_enabled(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.setMcpServerEnabled",
        aliases: &["setPluginMcpServerEnabled"],
        handler: |server, params| Box::pin(server.dispatch_plugin_set_mcp_server_enabled(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.remove",
        aliases: &["removePlugin"],
        handler: |server, params| Box::pin(server.dispatch_plugin_remove(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.reload",
        aliases: &["reloadPlugins"],
        handler: |server, params| Box::pin(server.dispatch_plugin_reload(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.info",
        aliases: &["getPluginInfo"],
        handler: |server, params| Box::pin(server.dispatch_plugin_info(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.commands",
        aliases: &["listPluginCommands"],
        handler: |server, _params| Box::pin(server.dispatch_plugin_commands()),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.activateCommand",
        aliases: &["activatePluginCommand"],
        handler: |server, params| Box::pin(server.dispatch_plugin_activate_command(params)),
        settlement: Route::SETTLES,
    },
    Route {
        method: "plugin.checkUpdates",
        aliases: &["checkUpdates"],
        handler: |server, _params| Box::pin(server.dispatch_plugin_check_updates()),
        settlement: Route::SETTLES,
    },
];
