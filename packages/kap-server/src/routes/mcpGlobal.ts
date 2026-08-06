/**
 * `/mcp/global` REST routes — global MCP server config management (v2 port).
 *
 * Mirrors the v1 server's wire contract
 * (`packages/server/src/routes/tools.ts` lines 323-642):
 *
 *   GET    /mcp/global/servers                          data: {servers: McpServer[]}
 *   GET    /mcp/global/tools                            data: {tools: ToolDescriptor[]}
 *   POST   /mcp/global/servers                          data: {reloading: true}
 *   PUT    /mcp/global/servers/{name}                   data: {reloading: true}
 *   DELETE /mcp/global/servers/{name}                   data: {reloading: true}
 *   POST   /mcp/global/servers/{name}:enable|disable|connect
 *   POST   /mcp/global/servers:reload                   data: {reloading: true}
 *   GET    /mcp/global/toggle-state                     data: {disabled_servers, disabled_tools}
 *   POST   /mcp/global/servers/{serverName}/tools/{tail}:enable|disable
 *   POST   /mcp/global/servers-test                     data: TestConnectMcpServerResponse
 *
 * **v2 MCP model** (differs from v1):
 * - v1 manages MCP servers through `IMcpService` (a global singleton).
 * - v2 loads MCP server config from `mcp.json` files via
 *   `IWorkspaceMcpConfigService`. The user `mcp.json` is at
 *   `join(resolveMirriHome(homeDir), 'mcp.json')`. The config service watches
 *   files for changes — writing the `mcp.json` triggers automatic reload.
 * - `IWorkspaceMcpService` provides `connectionManager()` for runtime state.
 * - Like profiles, the engine doesn't write config files — the route writes
 *   `mcp.json` directly, and the file watcher reloads.
 *
 * **Scope**: these routes are "global" (session-independent), but the config
 * and connection services are Workspace-scoped. The route resolves the first
 * available workspace handler. When no workspace exists, config reads return
 * empty lists and writes still go to the user `mcp.json` file (the file
 * watcher in the next workspace will pick it up).
 *
 * **Error mapping**:
 *   - unknown MCP server       → envelope `code: 40408 mcp.server_not_found`
 *   - malformed `{tail}`       → envelope `code: 40001 validation.failed`
 */

import {
  IBootstrapService,
  IHostFileSystem,
  IWorkspaceLifecycleService,
  IWorkspaceMcpConfigService,
  IWorkspaceMcpService,
  IWorkspaceService,
  resolveMirriHome,
  type McpServerConfig,
  type Scope,
} from '@mirri-ai/agent-core-v2';
import {
  createMcpServerBodySchema,
  listGlobalMcpServersResponseSchema,
  listGlobalMcpToolsResponseSchema,
  mcpToggleStateResponseSchema,
  reloadMcpServersResultSchema,
  testConnectMcpServerBodySchema,
  testConnectMcpServerResponseSchema,
  toggleMcpServerResponseSchema,
  updateMcpServerBodySchema,
} from '@mirri-ai/protocol';
import { mkdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { z } from 'zod';

import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { ErrorCode } from '../protocol/error-codes';
import type { McpServer, ToolDescriptor } from '../protocol/tool';
import { parseActionSuffix } from './action-suffix';

// ---------------------------------------------------------------------------
// Route host interface
// ---------------------------------------------------------------------------

interface McpGlobalRouteHost {
  get(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; params: unknown },
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
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
}

// ---------------------------------------------------------------------------
// Helpers: resolve workspace handle for workspace-scoped services
// ---------------------------------------------------------------------------

async function resolveWorkspaceHandle(
  core: Scope,
): Promise<import('@mirri-ai/agent-core-v2').IWorkspaceScopeHandle | undefined> {
  const lifecycle = core.accessor.get(IWorkspaceLifecycleService);
  const handlers = lifecycle.handlers.list();
  if (handlers.length > 0) return handlers[0];

  const workspaces = await core.accessor.get(IWorkspaceService).list();
  if (workspaces.length === 0) return undefined;

  const ws = workspaces[0]!;
  return lifecycle.handlerFor({ workspaceId: ws.id, root: ws.root });
}

// ---------------------------------------------------------------------------
// Helpers: mcp.json file path and I/O
// ---------------------------------------------------------------------------

function userMcpJsonPath(homeDir: string): string {
  return join(resolveMirriHome(homeDir), 'mcp.json');
}

async function readMcpJson(
  fs: IHostFileSystem,
  filePath: string,
): Promise<Record<string, McpServerConfig>> {
  try {
    const text = await fs.readText(filePath);
    if (text.trim().length === 0) return {};
    const data = JSON.parse(text);
    return data['mcpServers'] ?? {};
  } catch {
    return {};
  }
}

async function writeMcpJson(
  fs: IHostFileSystem,
  filePath: string,
  servers: Record<string, unknown>,
): Promise<void> {
  await mkdir(dirname(filePath), { recursive: true });
  const content = JSON.stringify({ mcpServers: servers }, null, 2) + '\n';
  await fs.writeText(filePath, content);
}

// ---------------------------------------------------------------------------
// Projection: v2 McpServerEntry + McpServerConfig → wire McpServer
// ---------------------------------------------------------------------------

function mapMcpStatus(
  status: 'pending' | 'connected' | 'failed' | 'disabled' | 'needs-auth',
): McpServer['status'] {
  switch (status) {
    case 'pending':
      return 'connecting';
    case 'connected':
      return 'connected';
    case 'disabled':
      return 'disconnected';
    case 'failed':
    case 'needs-auth':
      return 'error';
  }
}

interface McpServerEntryLike {
  readonly name: string;
  readonly transport: McpServerConfig['transport'];
  readonly status: 'pending' | 'connected' | 'failed' | 'disabled' | 'needs-auth';
  readonly toolCount: number;
  readonly error?: string;
}

function toWireMcpServer(entry: McpServerEntryLike): McpServer {
  const base: McpServer = {
    id: entry.name,
    name: entry.name,
    transport: entry.transport,
    status: mapMcpStatus(entry.status),
    tool_count: entry.toolCount,
  };
  if (entry.error !== undefined && entry.error.length > 0) {
    return { ...base, last_error: entry.error };
  }
  return base;
}

// ---------------------------------------------------------------------------
// Param schemas
// ---------------------------------------------------------------------------

const nameParamSchema = z.object({
  name: z.string().min(1),
});

const serverToolTailSchema = z.object({
  serverName: z.string().min(1),
  tail: z.string().min(1),
});

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

export function registerMcpGlobalRoutes(app: McpGlobalRouteHost, core: Scope): void {
  // GET /mcp/global/servers ----------------------------------------------
  const listServersRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/global/servers',
      success: { data: listGlobalMcpServersResponseSchema },
      description: 'List global MCP servers (session-independent)',
      tags: ['tools'],
      operationId: 'listGlobalMcpServers',
    },
    async (req, reply) => {
      const handle = await resolveWorkspaceHandle(core);
      if (handle === undefined) {
        reply.send(okEnvelope({ servers: [] }, req.id));
        return;
      }
      const configService = handle.accessor.get(IWorkspaceMcpConfigService);
      await configService.ready;
      const configs = configService.servers();
      const mcpService = handle.accessor.get(IWorkspaceMcpService);
      const cm = mcpService.connectionManager();
      const runtimeEntries = new Map<string, McpServerEntryLike>();
      for (const entry of cm.list()) {
        runtimeEntries.set(entry.name, entry);
      }

      // Merge config + runtime status
      const servers: McpServer[] = [];
      for (const [name, config] of Object.entries(configs)) {
        const runtime = runtimeEntries.get(name);
        if (runtime !== undefined) {
          servers.push(toWireMcpServer(runtime));
        } else {
          // Config exists but no runtime entry (not connected yet)
          servers.push({
            id: name,
            name,
            transport: config.transport,
            status: config.enabled === false ? 'disconnected' : 'connecting',
            tool_count: 0,
          });
        }
      }
      reply.send(okEnvelope({ servers }, req.id));
    },
  );
  app.get(
    listServersRoute.path,
    listServersRoute.options,
    listServersRoute.handler as Parameters<McpGlobalRouteHost['get']>[2],
  );

  // GET /mcp/global/tools ------------------------------------------------
  const listToolsRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/global/tools',
      success: { data: listGlobalMcpToolsResponseSchema },
      description: 'List tools from global MCP servers',
      tags: ['tools'],
      operationId: 'listGlobalMcpTools',
    },
    async (req, reply) => {
      const handle = await resolveWorkspaceHandle(core);
      if (handle === undefined) {
        reply.send(okEnvelope({ tools: [] }, req.id));
        return;
      }
      const mcpService = handle.accessor.get(IWorkspaceMcpService);
      const cm = mcpService.connectionManager();
      const tools: ToolDescriptor[] = [];
      for (const entry of cm.list()) {
        if (entry.status !== 'connected') continue;
        const resolved = cm.resolved(entry.name);
        if (resolved === undefined) continue;
        for (const tool of resolved.tools) {
          tools.push({
            name: tool.name,
            description: tool.description,
            input_schema: null,
            source: 'mcp',
            mcp_server_id: entry.name,
          });
        }
      }
      reply.send(okEnvelope({ tools }, req.id));
    },
  );
  app.get(
    listToolsRoute.path,
    listToolsRoute.options,
    listToolsRoute.handler as Parameters<McpGlobalRouteHost['get']>[2],
  );

  // POST /mcp/global/servers ---------------------------------------------
  const createServerRoute = defineRoute(
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
      const body = req.body;
      const serverName = body['name'] as string;
      const config = body['config'] as McpServerConfig;
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);
      servers[serverName] = config;
      await writeMcpJson(fs, filePath, servers);
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.post(
    createServerRoute.path,
    createServerRoute.options,
    createServerRoute.handler as Parameters<McpGlobalRouteHost['post']>[2],
  );

  // PUT /mcp/global/servers/{name} ---------------------------------------
  const updateServerRoute = defineRoute(
    {
      method: 'PUT',
      path: '/mcp/global/servers/{name}',
      body: updateMcpServerBodySchema,
      params: nameParamSchema,
      success: { data: reloadMcpServersResultSchema },
      description: 'Update a global MCP server entry',
      tags: ['tools'],
      operationId: 'updateMcpServer',
    },
    async (req, reply) => {
      const { name } = req.params;
      const body = req.body;
      const config = body['config'] as McpServerConfig;
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);
      if (!(name in servers)) {
        reply.send(
          errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${name}" does not exist`, req.id),
        );
        return;
      }
      servers[name] = config;
      await writeMcpJson(fs, filePath, servers);
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.put(
    updateServerRoute.path,
    updateServerRoute.options,
    updateServerRoute.handler as Parameters<McpGlobalRouteHost['put']>[2],
  );

  // DELETE /mcp/global/servers/{name} ------------------------------------
  const deleteServerRoute = defineRoute(
    {
      method: 'DELETE',
      path: '/mcp/global/servers/{name}',
      params: nameParamSchema,
      success: { data: reloadMcpServersResultSchema },
      description: 'Delete a global MCP server entry',
      tags: ['tools'],
      operationId: 'deleteMcpServer',
    },
    async (req, reply) => {
      const { name } = req.params;
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);
      if (!(name in servers)) {
        reply.send(
          errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${name}" does not exist`, req.id),
        );
        return;
      }
      delete servers[name];
      await writeMcpJson(fs, filePath, servers);
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.delete(
    deleteServerRoute.path,
    deleteServerRoute.options,
    deleteServerRoute.handler as Parameters<McpGlobalRouteHost['delete']>[2],
  );

  // POST /mcp/global/servers/{name}:enable|disable|connect ---------------
  const toggleServerRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers/{tail}',
      success: { data: reloadMcpServersResultSchema },
      errors: {
        [ErrorCode.MCP_SERVER_NOT_FOUND]: {},
      },
      description: 'Enable, disable, or connect a global MCP server by name',
      tags: ['tools'],
      operationId: 'toggleGlobalMcpServer',
    },
    async (req, reply) => {
      const { tail } = req.params as { tail: string };
      const parsed = parseActionSuffix({
        tail,
        allowedActions: ['enable', 'disable', 'connect'] as const,
        resourceLabel: 'mcp_server',
      });
      if (parsed.kind === 'invalid' || parsed.kind === 'bare') {
        reply.send(
          errEnvelope(
            ErrorCode.VALIDATION_FAILED,
            parsed.kind === 'invalid'
              ? parsed.reason
              : `unsupported action: ${tail}`,
            req.id,
          ),
        );
        return;
      }

      const name = parsed.id;

      if (parsed.action === 'connect') {
        // Connect: trigger connection manager to connect
        const handle = await resolveWorkspaceHandle(core);
        if (handle === undefined) {
          reply.send(
            errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${name}" does not exist`, req.id),
          );
          return;
        }
        const configService = handle.accessor.get(IWorkspaceMcpConfigService);
        await configService.ready;
        const configs = configService.servers();
        const config = configs[name];
        if (config === undefined) {
          reply.send(
            errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${name}" does not exist`, req.id),
          );
          return;
        }
        const mcpService = handle.accessor.get(IWorkspaceMcpService);
        const cm = mcpService.connectionManager();
        try {
          await cm.connect(name, config);
          const entry = cm.get(name);
          const resolved = cm.resolved(name);
          const serverWire = entry !== undefined ? toWireMcpServer(entry) : {
            id: name, name, transport: config.transport, status: 'connecting' as const, tool_count: 0,
          };
          const tools: ToolDescriptor[] = [];
          if (resolved !== undefined) {
            for (const tool of resolved.tools) {
              tools.push({
                name: tool.name,
                description: tool.description,
                input_schema: null,
                source: 'mcp',
                mcp_server_id: name,
              });
            }
          }
          reply.send(okEnvelope({ server: serverWire, tools }, req.id));
        } catch (error) {
          const msg = error instanceof Error ? error.message : String(error);
          reply.send(errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, msg, req.id));
        }
        return;
      }

      // enable / disable: modify the config in mcp.json
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);
      if (!(name in servers)) {
        reply.send(
          errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${name}" does not exist`, req.id),
        );
        return;
      }
      const existing = { ...servers[name] } as Record<string, unknown>;
      if (parsed.action === 'enable') {
        delete existing['enabled'];
      } else {
        existing['enabled'] = false;
      }
      servers[name] = existing as McpServerConfig;
      await writeMcpJson(fs, filePath, servers);
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.post(
    toggleServerRoute.path,
    toggleServerRoute.options,
    toggleServerRoute.handler as Parameters<McpGlobalRouteHost['post']>[2],
  );

  // POST /mcp/global/servers:reload --------------------------------------
  const reloadServersRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers:reload',
      success: { data: reloadMcpServersResultSchema },
      description: 'Reload all global MCP servers',
      tags: ['tools'],
      operationId: 'reloadMcpServers',
    },
    async (req, reply) => {
      const handle = await resolveWorkspaceHandle(core);
      if (handle === undefined) {
        reply.send(okEnvelope({ reloading: true }, req.id));
        return;
      }
      const mcpService = handle.accessor.get(IWorkspaceMcpService);
      const configService = handle.accessor.get(IWorkspaceMcpConfigService);
      await configService.ready;
      const configs = configService.servers();
      const cm = mcpService.connectionManager();
      await cm.connectAll(configs);
      reply.send(okEnvelope({ reloading: true }, req.id));
    },
  );
  app.post(
    reloadServersRoute.path,
    reloadServersRoute.options,
    reloadServersRoute.handler as Parameters<McpGlobalRouteHost['post']>[2],
  );

  // GET /mcp/global/toggle-state -----------------------------------------
  const toggleStateRoute = defineRoute(
    {
      method: 'GET',
      path: '/mcp/global/toggle-state',
      success: { data: mcpToggleStateResponseSchema },
      description: 'Get global runtime disabled state of MCP servers and tools',
      tags: ['tools'],
      operationId: 'getGlobalMcpToggleState',
    },
    async (req, reply) => {
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);

      const disabledServers: string[] = [];
      const disabledTools: string[] = [];
      for (const [name, config] of Object.entries(servers)) {
        if (config.enabled === false) {
          disabledServers.push(name);
        }
        if (config.disabledTools && config.disabledTools.length > 0) {
          // Qualified as mcp__{server}__{tool} — matches the v1 wire contract
          // (core-impl.ts getGlobalMcpToggleState) and the Web UI's expectation.
          for (const toolName of config.disabledTools) {
            disabledTools.push(`mcp__${name}__${toolName}`);
          }
        }
      }
      reply.send(okEnvelope({ disabled_servers: disabledServers, disabled_tools: disabledTools }, req.id));
    },
  );
  app.get(
    toggleStateRoute.path,
    toggleStateRoute.options,
    toggleStateRoute.handler as Parameters<McpGlobalRouteHost['get']>[2],
  );

  // POST /mcp/global/servers/{serverName}/tools/{tail}:enable|disable -----
  const toggleToolRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers/{serverName}/tools/{tail}',
      params: serverToolTailSchema,
      success: { data: toggleMcpServerResponseSchema },
      errors: {
        [ErrorCode.MCP_SERVER_NOT_FOUND]: {},
      },
      description: 'Enable or disable a specific global MCP tool by server name and tool name',
      tags: ['tools'],
      operationId: 'toggleGlobalMcpTool',
    },
    async (req, reply) => {
      const { serverName, tail } = req.params;
      const parsed = parseActionSuffix({
        tail,
        allowedActions: ['enable', 'disable'] as const,
        resourceLabel: 'mcp_tool',
      });
      if (parsed.kind === 'invalid') {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, parsed.reason, req.id));
        return;
      }
      if (parsed.kind === 'bare') {
        reply.send(
          errEnvelope(ErrorCode.VALIDATION_FAILED, `unsupported action: ${tail}`, req.id),
        );
        return;
      }
      const toolName = parsed.id;

      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = userMcpJsonPath(bootstrap.homeDir);
      const servers = await readMcpJson(fs, filePath);
      if (!(serverName in servers)) {
        reply.send(
          errEnvelope(ErrorCode.MCP_SERVER_NOT_FOUND, `MCP server "${serverName}" does not exist`, req.id),
        );
        return;
      }

      const config = { ...servers[serverName] } as Record<string, unknown>;
      const disabledTools = new Set(
        (config['disabledTools'] as string[] | undefined) ?? [],
      );
      const enabledTools = new Set(
        (config['enabledTools'] as string[] | undefined) ?? [],
      );

      if (parsed.action === 'enable') {
        disabledTools.delete(toolName);
        enabledTools.add(toolName);
      } else {
        enabledTools.delete(toolName);
        disabledTools.add(toolName);
      }

      if (disabledTools.size > 0) {
        config['disabledTools'] = [...disabledTools];
      } else {
        delete config['disabledTools'];
      }
      if (enabledTools.size > 0) {
        config['enabledTools'] = [...enabledTools];
      } else {
        delete config['enabledTools'];
      }

      servers[serverName] = config as McpServerConfig;
      await writeMcpJson(fs, filePath, servers);
      reply.send(okEnvelope({ ok: true as const }, req.id));
    },
  );
  app.post(
    toggleToolRoute.path,
    toggleToolRoute.options,
    toggleToolRoute.handler as Parameters<McpGlobalRouteHost['post']>[2],
  );

  // POST /mcp/global/servers-test ----------------------------------------
  const testConnectRoute = defineRoute(
    {
      method: 'POST',
      path: '/mcp/global/servers-test',
      body: testConnectMcpServerBodySchema,
      success: { data: testConnectMcpServerResponseSchema },
      description: 'Test-connect to an MCP server config without persisting',
      tags: ['tools'],
      operationId: 'testConnectMcpServer',
    },
    async (req, reply) => {
      const body = req.body;
      const config = body['config'] as McpServerConfig;
      const handle = await resolveWorkspaceHandle(core);
      if (handle === undefined) {
        reply.send(
          okEnvelope(
            { status: 'error' as const, tool_count: 0, error: 'No workspace available', tools: [] },
            req.id,
          ),
        );
        return;
      }
      const mcpService = handle.accessor.get(IWorkspaceMcpService);
      const cm = mcpService.connectionManager();
      const testName = `__test_${Date.now()}`;
      try {
        await cm.connect(testName, { ...config, enabled: true });
        const resolved = cm.resolved(testName);
        const tools = resolved !== undefined
          ? resolved.tools.map((t) => ({ name: t.name, description: t.description }))
          : [];
        const toolCount = resolved !== undefined ? resolved.tools.length : 0;
        reply.send(
          okEnvelope({ status: 'connected' as const, tool_count: toolCount, tools }, req.id),
        );
      } catch (error) {
        const msg = error instanceof Error ? error.message : String(error);
        reply.send(
          okEnvelope({ status: 'error' as const, tool_count: 0, error: msg, tools: [] }, req.id),
        );
      } finally {
        try {
          await cm.remove(testName);
        } catch {
          // cleanup best effort
        }
      }
    },
  );
  app.post(
    testConnectRoute.path,
    testConnectRoute.options,
    testConnectRoute.handler as Parameters<McpGlobalRouteHost['post']>[2],
  );
}
