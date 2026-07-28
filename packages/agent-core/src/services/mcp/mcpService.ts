/**
 * `McpService` — implementation of `IMcpService`.
 */

import { Disposable, InstantiationType, registerSingleton } from '../../di';
import type { McpServer, ToolDescriptor } from '@mirri-ai/protocol';

import { ICoreProcessService } from '../coreProcess/coreProcess';
import type { McpServerConfig } from '../../config';
import { ErrorCodes, isMirriError } from '../../errors';
import {
  IMcpService,
  McpServerNotFoundError,
  toProtocolMcpServer,
} from './mcp';

export class McpService extends Disposable implements IMcpService {
  readonly _serviceBrand: undefined;

  constructor(@ICoreProcessService private readonly core: ICoreProcessService) {
    super();
  }

  async list(): Promise<readonly McpServer[]> {
    // `listMcpServers` is on the SessionAPI surface; we need a session id to
    // dispatch. Pick the most recently created one. If no sessions exist,
    // return an empty list (the MCP registrar may have started up but the
    // RPC plumbing isn't reachable until a session is open).
    const sessionId = await this._anyKnownSessionId();
    if (sessionId === undefined) return [];
    const raw = await this.core.rpc.listMcpServers({ sessionId });
    return raw.map(toProtocolMcpServer);
  }

  async restart(serverId: string): Promise<{ restarting: true }> {
    const sessionId = await this._anyKnownSessionId();
    if (sessionId === undefined) {
      // No session => no MCP registrar reachable => server can't be reached.
      throw new McpServerNotFoundError(serverId);
    }
    // Existence check: the wire id is the agent-core `name`. The reconnect
    // call will reject for unknown names; we pre-check so the route can
    // emit a deterministic 40408 envelope without depending on agent-core
    // error message shape.
    const known = await this.core.rpc.listMcpServers({ sessionId });
    if (!known.some((s) => s.name === serverId)) {
      throw new McpServerNotFoundError(serverId);
    }
    await this.core.rpc.reconnectMcpServer({ sessionId, name: serverId });
    return { restarting: true };
  }

  async enableMcpServer(serverId: string): Promise<void> {
    await this.core.rpc.toggleGlobalMcpServer({ name: serverId, enabled: true });
  }

  async disableMcpServer(serverId: string): Promise<void> {
    await this.core.rpc.toggleGlobalMcpServer({ name: serverId, enabled: false });
  }

  async enableMcpTool(qualifiedName: string): Promise<void> {
    const sessionId = await this._anyKnownSessionId();
    if (sessionId === undefined) {
      throw new McpServerNotFoundError(qualifiedName);
    }
    await this.core.rpc.enableMcpTool({ sessionId, qualifiedName });
  }

  async disableMcpTool(qualifiedName: string): Promise<void> {
    const sessionId = await this._anyKnownSessionId();
    if (sessionId === undefined) {
      throw new McpServerNotFoundError(qualifiedName);
    }
    await this.core.rpc.disableMcpTool({ sessionId, qualifiedName });
  }

  async getToggleState(): Promise<{ disabledServers: readonly string[]; disabledTools: readonly string[] }> {
    const sessionId = await this._anyKnownSessionId();
    if (sessionId === undefined) {
      return { disabledServers: [], disabledTools: [] };
    }
    const result = await this.core.rpc.getMcpToggleState({ sessionId });
    return {
      disabledServers: result.disabledServers,
      disabledTools: result.disabledTools,
    };
  }

  // -------------------------------------------------------------------------
  // Global MCP (session-independent)
  // -------------------------------------------------------------------------

  async listAll(): Promise<{
    servers: readonly McpServer[];
    tools: readonly ToolDescriptor[];
  }> {
    const [servers, tools] = await Promise.all([
      this.listServersImmediate(),
      this.listToolsAfterConnected(),
    ]);
    return { servers, tools };
  }

  async listServersImmediate(): Promise<readonly McpServer[]> {
    const rawServers = await this.core.rpc.listGlobalMcpServers({});
    return rawServers.map(toProtocolMcpServer);
  }

  async listToolsAfterConnected(): Promise<readonly ToolDescriptor[]> {
    const rawTools = await this.core.rpc.listGlobalMcpTools({});
    return rawTools.map((t) => ({
      name: t.name,
      description: t.description,
      input_schema: undefined,
      source: 'mcp' as const,
      mcp_server_id: t.mcpServerId,
    }));
  }

  async reloadAll(): Promise<void> {
    await this.core.rpc.reloadGlobalMcp({});
  }

  async createServer(name: string, config: McpServerConfig): Promise<void> {
    await this.core.rpc.createGlobalMcpServer({ name, config });
  }

  async updateServer(name: string, config: McpServerConfig): Promise<void> {
    await this.core.rpc.updateGlobalMcpServer({ name, config });
  }

  async deleteServer(name: string): Promise<void> {
    await this.core.rpc.deleteGlobalMcpServer({ name });
  }

  async getGlobalToggleState(): Promise<{ disabledServers: readonly string[]; disabledTools: readonly string[] }> {
    const result = await this.core.rpc.getGlobalMcpToggleState({});
    return {
      disabledServers: result.disabledServers,
      disabledTools: result.disabledTools,
    };
  }

  async enableGlobalMcpTool(qualifiedName: string): Promise<void> {
    await this.core.rpc.enableGlobalMcpTool({ qualifiedName });
  }

  async disableGlobalMcpTool(qualifiedName: string): Promise<void> {
    await this.core.rpc.disableGlobalMcpTool({ qualifiedName });
  }

  async testConnect(config: McpServerConfig): Promise<{
    status: 'connected' | 'error';
    toolCount: number;
    error?: string;
    tools: readonly { name: string; description: string }[];
  }> {
    const result = await this.core.rpc.testConnectMcpServer({ config });
    return {
      status: result.status,
      toolCount: result.toolCount,
      ...(result.error !== undefined ? { error: result.error } : {}),
      tools: result.tools,
    };
  }

  async connectServer(name: string): Promise<{ server: McpServer; tools: readonly ToolDescriptor[] }> {
    try {
      const result = await this.core.rpc.connectGlobalMcpServer({ name });
      const server = toProtocolMcpServer(result.server);
      const tools: ToolDescriptor[] = result.tools.map((t) => ({
        name: t.name,
        description: t.description,
        input_schema: undefined,
        source: 'mcp' as const,
        mcp_server_id: name,
      }));
      return { server, tools };
    } catch (error: unknown) {
      if (isMirriError(error) && error.code === ErrorCodes.MCP_SERVER_NOT_FOUND) {
        throw new McpServerNotFoundError(name);
      }
      throw error;
    }
  }

  /**
   * Find a usable session id for dispatching SessionAPI calls. Returns the
   * most recently created session id, or `undefined` when no sessions exist.
   */
  private async _anyKnownSessionId(): Promise<string | undefined> {
    const all = await this.core.rpc.listSessions({});
    if (all.length === 0) return undefined;
    // Sort by createdAt desc — newest sessions are the most likely to have
    // an active MCP RPC binding.
    const sorted = [...all].toSorted((a, b) => b.createdAt - a.createdAt);
    return sorted[0]?.id;
  }
}

// Self-register under the global singleton registry. All ctor deps are
// `@I…`-injected; `staticArguments = []`. `supportsDelayedInstantiation =
// false` preserves current reverse-dispose semantics.
registerSingleton(IMcpService, McpService, InstantiationType.Delayed);
