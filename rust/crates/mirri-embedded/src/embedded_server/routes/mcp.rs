//! MCP-domain routes — the 26 `mcp.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 2) plus the P1
//! `mcp.config.*` surface (batch 3) and the P2 pool surface (batch 4). Doc comments moved
//! with their arms; handlers and inline values are unchanged.

use super::{Route, not_implemented};

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "mcp.list",
        aliases: &["listMcpServers"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_list(false)),
    },
    Route {
        method: "mcp.startupMetrics",
        aliases: &["getMcpStartupMetrics"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_startup_metrics()),
    },
    Route {
        method: "mcp.reconnect",
        aliases: &["reconnectMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_reconnect(params)),
    },
    // The P1 configuration read-write surface (mcp-architecture §4.1): the
    // layered config view and the atomic per-entry writes.
    Route {
        method: "mcp.config.list",
        aliases: &["listMcpConfig"],
        handler: |server, params| Box::pin(server.dispatch_mcp_config_list(params)),
    },
    Route {
        method: "mcp.config.set",
        aliases: &["setMcpConfig"],
        handler: |server, params| Box::pin(server.dispatch_mcp_config_set(params)),
    },
    Route {
        method: "mcp.config.remove",
        aliases: &["removeMcpConfig"],
        handler: |server, params| Box::pin(server.dispatch_mcp_config_remove(params)),
    },
    Route {
        method: "mcp.config.setToolFilter",
        aliases: &["setMcpToolFilter"],
        handler: |server, params| Box::pin(server.dispatch_mcp_config_set_tool_filter(params)),
    },
    // The P2 pool surface (mcp-architecture §4.4): the fresh-connect config
    // test and the no-workspace global status view.
    Route {
        method: "mcp.config.test",
        aliases: &["testMcpConfig"],
        handler: |server, params| Box::pin(server.dispatch_mcp_config_test(params)),
    },
    // The F-4 resolved-value preview: the adjudication A5 sole sanctioned
    // outlet for environment-expanded values (an explicit request only).
    Route {
        method: "mcp.config.preview",
        aliases: &["previewMcpConfig"],
        handler: |server, params| Box::pin(server.dispatch_preview_mcp_config(params)),
    },
    Route {
        method: "mcp.status",
        aliases: &["getMcpStatus"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_status()),
    },
    // The M1 tool view: the single-server tool listing with per-tool
    // filter state (ADR-004).
    Route {
        method: "mcp.tools",
        aliases: &["listMcpTools"],
        handler: |server, params| Box::pin(server.dispatch_list_mcp_tools(params)),
    },
    // The F-6 idempotent reconcile entry: the single kernel both the
    // config writes and the file watcher already use, exposed as the
    // manual/external-edit convergence call.
    Route {
        method: "mcp.sync",
        aliases: &["syncMcpServers"],
        handler: |server, params| Box::pin(server.dispatch_mcp_sync(params)),
    },
    Route {
        method: "mcp.enable",
        aliases: &["enableMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_enable(params)),
    },
    Route {
        method: "mcp.disable",
        aliases: &["disableMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_disable(params)),
    },
    // Retired in the v2 contract (v1-era session-level toggles; v2
    // deleted them): explicit not_implemented instead of the former
    // silent acknowledgement. `dispatch_mcp_enable_tool` still serves
    // the out-of-scope `mcp.global.enableTool` route.
    Route {
        method: "mcp.enableTool",
        aliases: &["enableMcpTool"],
        handler: |_server, _params| {
            Box::pin(async {
                Err(not_implemented(
                    "mcp.enableTool is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
                ))
            })
        },
    },
    Route {
        method: "mcp.disableTool",
        aliases: &["disableMcpTool"],
        handler: |_server, _params| {
            Box::pin(async {
                Err(not_implemented(
                    "mcp.disableTool is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
                ))
            })
        },
    },
    Route {
        method: "mcp.toggleState",
        aliases: &["getMcpToggleState"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_toggle_state()),
    },
    // Global MCP (host-level).
    Route {
        method: "mcp.global.list",
        aliases: &["listGlobalMcpServers"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_list(false)),
    },
    Route {
        method: "mcp.global.tools",
        aliases: &["listGlobalMcpTools"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_tools()),
    },
    Route {
        method: "mcp.global.reload",
        aliases: &["reloadGlobalMcp"],
        handler: |server, params| Box::pin(server.dispatch_mcp_reload(params)),
    },
    Route {
        method: "mcp.global.create",
        aliases: &["createGlobalMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_connect(params)),
    },
    // Retired in the v2 contract (v1-era global MCP surface; v2 has
    // no global MCP RPC): explicit not_implemented instead of the
    // former silent acknowledgement.
    Route {
        method: "mcp.global.update",
        aliases: &["updateGlobalMcpServer"],
        handler: |_server, _params| {
            Box::pin(async {
                Err(not_implemented(
                    "mcp.global.update is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
                ))
            })
        },
    },
    Route {
        method: "mcp.global.delete",
        aliases: &["deleteGlobalMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_disconnect(params)),
    },
    Route {
        method: "mcp.global.toggle",
        aliases: &["toggleGlobalMcpServer"],
        handler: |_server, _params| {
            Box::pin(async {
                Err(not_implemented(
                    "mcp.global.toggle is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
                ))
            })
        },
    },
    Route {
        method: "mcp.global.toggleState",
        aliases: &["getGlobalMcpToggleState"],
        handler: |server, _params| Box::pin(server.dispatch_mcp_toggle_state()),
    },
    Route {
        method: "mcp.global.enableTool",
        aliases: &["enableGlobalMcpTool"],
        handler: |server, params| Box::pin(server.dispatch_mcp_enable_tool(params)),
    },
    Route {
        method: "mcp.global.disableTool",
        aliases: &["disableGlobalMcpTool"],
        handler: |server, params| Box::pin(server.dispatch_mcp_disable_tool(params)),
    },
    Route {
        method: "mcp.global.testConnect",
        aliases: &["testConnectMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_test_connect(params)),
    },
    Route {
        method: "mcp.global.connect",
        aliases: &["connectGlobalMcpServer"],
        handler: |server, params| Box::pin(server.dispatch_mcp_connect(params)),
    },
];
