/**
 * `/tools` + `/mcp/servers*` REST routes.
 *
 * 3 endpoints (REST.md §3.8):
 *
 *   GET  /tools                                  query: {session_id?}    data: {tools: ToolDescriptor[]}
 *   GET  /mcp/servers                            -                       data: {servers: McpServer[]}
 *   POST /mcp/servers/{mcp_server_id}:restart    body: empty             data: {restarting: true}
 *
 * **Error mapping**:
 *   - `McpServerNotFoundError` → envelope `code: 40408 mcp.server_not_found`.
 *   - Other errors → 50001 via the global `installErrorHandler`.
 *
 * **Action suffix**: the `:restart` POST endpoint uses the shared
 * `parseActionSuffix` helper.
 *
 * **Anti-corruption**: route resolves `IToolService` / `IMcpService` via the
 * accessor; no SDK imports.
 */

import {
  ErrorCode,
  createMcpServerBodySchema,
  listAllToolsResponseSchema,
  listGlobalMcpServersResponseSchema,
  listGlobalMcpToolsResponseSchema,
  listMcpServersResponseSchema,
  listToolsQuerySchema,
  listToolsResponseSchema,
  mcpToggleStateResponseSchema,
  reloadMcpServersResultSchema,
  restartMcpServerResultSchema,
  toggleMcpServerResponseSchema,
  updateMcpServerBodySchema,
} from '@mirri-ai/protocol';
import { IMcpService, IToolService, McpServerNotFoundError, type IInstantiationService } from '@mirri-ai/agent-core';


import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { parseActionSuffix } from './action-suffix';

interface ToolsRouteHost {
  get(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> } | undefined,
    handler: (
      req: { id: string; query: unknown; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  post(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; body: unknown; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  put(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; body: unknown; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  delete(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> } | undefined,
    handler: (
      req: { id: string; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
}

export function registerToolsRoutes(
  app: ToolsRouteHost,
  ix: IInstantiationService,
): void {
  // GET /tools ----------------------------------------------------------
  const listToolsRoute = defineRoute(
    {
      method: 'GET',
      path: '/tools',
      querystring: listToolsQuerySchema,
      success: { data: listToolsResponseSchema },
      description: 'List available tools',
      tags: ['tools'],
    },
    async (req, reply) => {
      try {
        const tools = await ix.invokeFunction((a) =>
          a.get(IToolService).list(req.query.session_id),
        );
        reply.send(okEnvelope({ tools }, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.get(
    listToolsRoute.path,
    listToolsRoute.options,
    listToolsRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // GET /tools/all -------------------------------------------------------
  const listAllToolsRoute = defineRoute(
    {
      method: 'GET',
      path: '/tools/all',
      success: { data: listAllToolsResponseSchema },
      description: 'List all builtin tool descriptors (session-independent)',
      tags: ['tools'],
      operationId: 'listAllTools',
    },
    async (req, reply) => {
      const [builtinTools, mcpResult] = await Promise.all([
        ix.invokeFunction((a) => a.get(IToolService).listAll()),
        ix
          .invokeFunction((a) => a.get(IMcpService).listAll())
          .catch(() => ({ servers: [], tools: [] as never[] })),
      ]);
      reply.send(
        okEnvelope({ tools: [...builtinTools, ...mcpResult.tools] }, req.id),
      );
    },
  );
  app.get(
    listAllToolsRoute.path,
    listAllToolsRoute.options,
    listAllToolsRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // GET /mcp/servers ----------------------------------------------------
  const listMcpServersRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/servers',
      success: { data: listMcpServersResponseSchema },
      description: 'List configured MCP servers',
      tags: ['tools'],
    },
    async (req, reply) => {
      try {
        const servers = await ix.invokeFunction((a) => a.get(IMcpService).list());
        reply.send(okEnvelope({ servers }, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.get(
    listMcpServersRoute.path,
    listMcpServersRoute.options,
    listMcpServersRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // POST /mcp/servers/{mcp_server_id}:{restart|enable|disable} ----------
  const restartMcpServerRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/servers/{tail}',
      success: { data: restartMcpServerResultSchema },
      errors: {
        [ErrorCode.MCP_SERVER_NOT_FOUND]: {},
      },
      description: 'Restart, enable, or disable an MCP server by ID',
      tags: ['tools'],
      operationId: 'restartMcpServer',
    },
    async (req, reply) => {
      try {
        const { tail } = req.params as { tail: string };
        const parsed = parseActionSuffix({
          tail,
          allowedActions: ['restart', 'enable', 'disable'] as const,
          resourceLabel: 'mcp_server',
        });
        if (parsed.kind === 'invalid') {
          reply.send(
            errEnvelope(ErrorCode.VALIDATION_FAILED, parsed.reason, req.id),
          );
          return;
        }
        if (parsed.kind === 'bare') {
          reply.send(
            errEnvelope(
              ErrorCode.VALIDATION_FAILED,
              `unsupported action: ${tail}`,
              req.id,
            ),
          );
          return;
        }
        if (parsed.action === 'restart') {
          const result = await ix.invokeFunction((a) =>
            a.get(IMcpService).restart(parsed.id),
          );
          reply.send(okEnvelope(result, req.id));
          return;
        }
        if (parsed.action === 'enable') {
          await ix.invokeFunction((a) =>
            a.get(IMcpService).enableMcpServer(parsed.id),
          );
          reply.send(okEnvelope({ ok: true as const }, req.id));
          return;
        }
        if (parsed.action === 'disable') {
          await ix.invokeFunction((a) =>
            a.get(IMcpService).disableMcpServer(parsed.id),
          );
          reply.send(okEnvelope({ ok: true as const }, req.id));
          return;
        }
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.post(
    restartMcpServerRoute.path,
    restartMcpServerRoute.options,
    restartMcpServerRoute.handler as Parameters<ToolsRouteHost['post']>[2],
  );

  // GET /mcp/toggle-state ------------------------------------------------
  const mcpToggleStateRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/toggle-state',
      success: { data: mcpToggleStateResponseSchema },
      description: 'Get runtime disabled state of MCP servers and tools',
      tags: ['tools'],
      operationId: 'getMcpToggleState',
    },
    async (req, reply) => {
      const result = await ix.invokeFunction((a) => a.get(IMcpService).getToggleState());
      reply.send(
        okEnvelope(
          {
            disabled_servers: result.disabledServers,
            disabled_tools: result.disabledTools,
          },
          req.id,
        ),
      );
    },
  );
  app.get(
    mcpToggleStateRoute.path,
    mcpToggleStateRoute.options,
    mcpToggleStateRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // POST /mcp/tools/{tail}:{enable|disable} ------------------------------
  const toggleMcpToolRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/tools/{tail}',
      success: { data: toggleMcpServerResponseSchema },
      errors: {
        [ErrorCode.MCP_SERVER_NOT_FOUND]: {},
      },
      description: 'Enable or disable a specific MCP tool by qualified name',
      tags: ['tools'],
      operationId: 'toggleMcpTool',
    },
    async (req, reply) => {
      try {
        const { tail } = req.params as { tail: string };
        const parsed = parseActionSuffix({
          tail,
          allowedActions: ['enable', 'disable'] as const,
          resourceLabel: 'mcp_tool',
        });
        if (parsed.kind === 'invalid') {
          reply.send(
            errEnvelope(ErrorCode.VALIDATION_FAILED, parsed.reason, req.id),
          );
          return;
        }
        if (parsed.kind === 'bare') {
          reply.send(
            errEnvelope(
              ErrorCode.VALIDATION_FAILED,
              `unsupported action: ${tail}`,
              req.id,
            ),
          );
          return;
        }
        if (parsed.action === 'enable') {
          await ix.invokeFunction((a) =>
            a.get(IMcpService).enableMcpTool(parsed.id),
          );
        } else {
          await ix.invokeFunction((a) =>
            a.get(IMcpService).disableMcpTool(parsed.id),
          );
        }
        reply.send(okEnvelope({ ok: true as const }, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.post(
    toggleMcpToolRoute.path,
    toggleMcpToolRoute.options,
    toggleMcpToolRoute.handler as Parameters<ToolsRouteHost['post']>[2],
  );

  // ---------------------------------------------------------------------
  // Global MCP routes (session-independent) — Settings MCP panel
  // ---------------------------------------------------------------------

  // GET /mcp/global/servers ----------------------------------------------
  const listGlobalMcpServersRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/global/servers',
      success: { data: listGlobalMcpServersResponseSchema },
      description: 'List global MCP servers (session-independent)',
      tags: ['tools'],
      operationId: 'listGlobalMcpServers',
    },
    async (req, reply) => {
      const { servers } = await ix.invokeFunction((a) => a.get(IMcpService).listAll());
      reply.send(okEnvelope({ servers }, req.id));
    },
  );
  app.get(
    listGlobalMcpServersRoute.path,
    listGlobalMcpServersRoute.options,
    listGlobalMcpServersRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // GET /mcp/global/tools ------------------------------------------------
  const listGlobalMcpToolsRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/global/tools',
      success: { data: listGlobalMcpToolsResponseSchema },
      description: 'List tools from global MCP servers',
      tags: ['tools'],
      operationId: 'listGlobalMcpTools',
    },
    async (req, reply) => {
      const { tools } = await ix.invokeFunction((a) => a.get(IMcpService).listAll());
      reply.send(okEnvelope({ tools }, req.id));
    },
  );
  app.get(
    listGlobalMcpToolsRoute.path,
    listGlobalMcpToolsRoute.options,
    listGlobalMcpToolsRoute.handler as Parameters<ToolsRouteHost['get']>[2],
  );

  // POST /mcp/global/servers ---------------------------------------------
  const createMcpServerRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers',
      body: createMcpServerBodySchema,
      success: { data: reloadMcpServersResultSchema },
      description: 'Create a global MCP server entry',
      tags: ['tools'],
      operationId: 'createMcpServer',
    },
    async (req, reply) => {
      const body = req.body as { name: string; config: Record<string, unknown> };
      await ix.invokeFunction((a) =>
        a.get(IMcpService).createServer(body.name, body.config as never),
      );
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.post(
    createMcpServerRoute.path,
    createMcpServerRoute.options,
    createMcpServerRoute.handler as Parameters<ToolsRouteHost['post']>[2],
  );

  // PUT /mcp/global/servers/{name} ---------------------------------------
  const updateMcpServerRoute = defineRoute(
    {
      method: 'PUT',
      path: '/mcp/global/servers/{name}',
      body: updateMcpServerBodySchema,
      success: { data: reloadMcpServersResultSchema },
      description: 'Update a global MCP server entry',
      tags: ['tools'],
      operationId: 'updateMcpServer',
    },
    async (req, reply) => {
      const { name } = req.params as { name: string };
      const body = req.body as { config: Record<string, unknown> };
      await ix.invokeFunction((a) =>
        a.get(IMcpService).updateServer(name, body.config as never),
      );
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.put(
    updateMcpServerRoute.path,
    updateMcpServerRoute.options,
    updateMcpServerRoute.handler as Parameters<ToolsRouteHost['put']>[2],
  );

  // DELETE /mcp/global/servers/{name} ------------------------------------
  const deleteMcpServerRoute = defineRoute(
    {
      method: 'DELETE',
      path: '/mcp/global/servers/{name}',
      success: { data: reloadMcpServersResultSchema },
      description: 'Delete a global MCP server entry',
      tags: ['tools'],
      operationId: 'deleteMcpServer',
    },
    async (req, reply) => {
      const { name } = req.params as { name: string };
      await ix.invokeFunction((a) => a.get(IMcpService).deleteServer(name));
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.delete(
    deleteMcpServerRoute.path,
    deleteMcpServerRoute.options,
    deleteMcpServerRoute.handler as Parameters<ToolsRouteHost['delete']>[2],
  );

  // POST /mcp/global/servers:reload --------------------------------------
  const reloadMcpServersRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers:reload',
      success: { data: reloadMcpServersResultSchema },
      description: 'Reload all global MCP servers',
      tags: ['tools'],
      operationId: 'reloadMcpServers',
    },
    async (req, reply) => {
      await ix.invokeFunction((a) => a.get(IMcpService).reloadAll());
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.post(
    reloadMcpServersRoute.path,
    reloadMcpServersRoute.options,
    reloadMcpServersRoute.handler as Parameters<ToolsRouteHost['post']>[2],
  );
}

/**
 * Map a thrown error to the right envelope. See module header for the table.
 */
function sendMappedError(
  reply: { send(payload: unknown): unknown },
  requestId: string,
  err: unknown,
): void {
  if (err instanceof McpServerNotFoundError) {
    reply.send(errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, err.message, requestId));
    return;
  }
  throw err;
}
