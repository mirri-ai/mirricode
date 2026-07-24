/**
 * `ToolService` + `McpService` (Chain 7 / P1.7, W9.1) unit tests.
 *
 * Hermetic: mocks `ICoreProcessService` with an in-memory `rpc` proxy. Exercises:
 *   - tool source mapping: 'builtin' / 'user'→'skill' / 'mcp' + mcp_server_id parse
 *   - mcp server status mapping (all 5 agent-core literals → 4 wire literals)
 *   - transport pass-through
 *   - last_error surfaced via `error?`
 *   - McpServerNotFoundError raised for unknown server id
 *   - empty-session-list behavior (returns [] / throws not found)
 */

import { describe, expect, it } from 'vitest';

import type {
  CoreRPC,
  EmptyPayload,
  McpServerInfo,
  ReconnectMcpServerPayload,
  SessionSummary,
  GlobalMcpToolInfo,
  CreateGlobalMcpServerPayload,
  UpdateGlobalMcpServerPayload,
  DeleteGlobalMcpServerPayload,
} from '../../src';

import {
  type ICoreProcessService,
  McpServerNotFoundError,
  McpService,
  ToolService,
  toProtocolMcpServer,
  toProtocolTool,
} from '../../src/services';
import type { AgentCoreToolInfoLike } from '../../src/services';

interface FakeBridgeState {
  sessions: SessionSummary[];
  tools: AgentCoreToolInfoLike[];
  mcpServers: McpServerInfo[];
  reconnectCalls: ReconnectMcpServerPayload[];
  globalMcpServers: McpServerInfo[];
  globalMcpTools: GlobalMcpToolInfo[];
  globalMcpReloadCalls: number;
  globalMcpCreateCalls: CreateGlobalMcpServerPayload[];
  globalMcpUpdateCalls: UpdateGlobalMcpServerPayload[];
  globalMcpDeleteCalls: DeleteGlobalMcpServerPayload[];
}

function makeFakeBridge(state: FakeBridgeState): ICoreProcessService {
  const rpc: Partial<CoreRPC> = {
    listSessions: async () => state.sessions,
    getTools: async (_p: unknown) => state.tools as unknown as readonly never[],
    listMcpServers: async (_p: EmptyPayload & { sessionId: string }) => state.mcpServers,
    reconnectMcpServer: async (
      p: ReconnectMcpServerPayload & { sessionId: string },
    ) => {
      state.reconnectCalls.push(p);
    },
    listGlobalMcpServers: async () => state.globalMcpServers,
    listGlobalMcpTools: async () => state.globalMcpTools,
    reloadGlobalMcp: async () => { state.globalMcpReloadCalls++; },
    createGlobalMcpServer: async (p: CreateGlobalMcpServerPayload) => { state.globalMcpCreateCalls.push(p); },
    updateGlobalMcpServer: async (p: UpdateGlobalMcpServerPayload) => { state.globalMcpUpdateCalls.push(p); },
    deleteGlobalMcpServer: async (p: DeleteGlobalMcpServerPayload) => { state.globalMcpDeleteCalls.push(p); },
  };
  return {
    rpc: rpc as CoreRPC,
    ready: async () => undefined,
    dispose: () => undefined,
    _serviceBrand: undefined,
  };
}

function fakeSession(id: string, createdAt: number): SessionSummary {
  return {
    id,
    workDir: '/tmp/wd',
    sessionDir: `/tmp/sd-${id}`,
    createdAt,
    updatedAt: createdAt,
  };
}

function freshState(): FakeBridgeState {
  return {
    sessions: [],
    tools: [],
    mcpServers: [],
    reconnectCalls: [],
    globalMcpServers: [],
    globalMcpTools: [],
    globalMcpReloadCalls: 0,
    globalMcpCreateCalls: [],
    globalMcpUpdateCalls: [],
    globalMcpDeleteCalls: [],
  };
}

// --- Adapter tests ----------------------------------------------------------

describe('toProtocolTool adapter', () => {
  it("maps builtin source as-is and emits input_schema = null", () => {
    const out = toProtocolTool({ name: 'Bash', description: 'd', source: 'builtin' });
    expect(out.source).toBe('builtin');
    expect(out.input_schema).toBeNull();
    expect(out.mcp_server_id).toBeUndefined();
  });

  it("maps agent-core 'user' source to wire 'skill'", () => {
    const out = toProtocolTool({ name: 'mySkill', description: 'd', source: 'user' });
    expect(out.source).toBe('skill');
  });

  it("parses mcp_server_id from qualified mcp tool name 'mcp:lark:search'", () => {
    const out = toProtocolTool({
      name: 'mcp:lark:search',
      description: 'd',
      source: 'mcp',
    });
    expect(out.source).toBe('mcp');
    expect(out.mcp_server_id).toBe('lark');
  });

  it('omits mcp_server_id when the mcp tool name lacks the conventional prefix', () => {
    const out = toProtocolTool({
      name: 'oddly_named',
      description: 'd',
      source: 'mcp',
    });
    expect(out.mcp_server_id).toBeUndefined();
  });
});

describe('toProtocolMcpServer adapter', () => {
  function base(
    overrides: Partial<McpServerInfo> & Pick<McpServerInfo, 'status'>,
  ): McpServerInfo {
    return {
      name: 'lark',
      transport: 'stdio',
      toolCount: 3,
      ...overrides,
    } as McpServerInfo;
  }

  it("maps 'pending' → 'connecting'", () => {
    expect(toProtocolMcpServer(base({ status: 'pending' })).status).toBe('connecting');
  });
  it("passes 'connected' through", () => {
    expect(toProtocolMcpServer(base({ status: 'connected' })).status).toBe('connected');
  });
  it("maps 'failed' → 'error'", () => {
    expect(toProtocolMcpServer(base({ status: 'failed' })).status).toBe('error');
  });
  it("maps 'disabled' → 'disconnected'", () => {
    expect(toProtocolMcpServer(base({ status: 'disabled' })).status).toBe('disconnected');
  });
  it("maps 'needs-auth' → 'error' and surfaces last_error from error?", () => {
    const out = toProtocolMcpServer(
      base({ status: 'needs-auth', error: 'visit https://auth' }),
    );
    expect(out.status).toBe('error');
    expect(out.last_error).toBe('visit https://auth');
  });
  it('adopts name-as-id', () => {
    expect(toProtocolMcpServer(base({ status: 'connected', name: 'foo' })).id).toBe('foo');
  });
  it("does not set last_error when info.error is undefined or empty", () => {
    expect(toProtocolMcpServer(base({ status: 'connected' })).last_error).toBeUndefined();
    expect(
      toProtocolMcpServer(base({ status: 'connected', error: '' })).last_error,
    ).toBeUndefined();
  });
});

// --- Service impl tests -----------------------------------------------------

describe('ToolService.list', () => {
  it('returns [] when no sessions exist (CoreAPI gap)', async () => {
    const svc = new ToolService(makeFakeBridge(freshState()));
    const out = await svc.list();
    expect(out).toEqual([]);
  });

  it('returns adapted tools using the most-recent session id', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s_old', 1));
    state.sessions.push(fakeSession('s_new', 2));
    state.tools.push(
      { name: 'Bash', description: 'b', source: 'builtin' },
      { name: 'mcp:lark:search', description: 'l', source: 'mcp' },
    );
    const svc = new ToolService(makeFakeBridge(state));
    const out = await svc.list();
    expect(out).toHaveLength(2);
    expect(out[0]!.source).toBe('builtin');
    expect(out[1]!.source).toBe('mcp');
    expect(out[1]!.mcp_server_id).toBe('lark');
  });

  it('returns [] when getTools throws (session not loaded)', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s', 1));
    const bridge = makeFakeBridge(state);
    (bridge.rpc).getTools = async () => {
      throw new Error('session not loaded');
    };
    const svc = new ToolService(bridge);
    expect(await svc.list()).toEqual([]);
  });
});

describe('ToolService.listAll', () => {
  it('returns builtin tool descriptors without requiring a session', async () => {
    const svc = new ToolService(makeFakeBridge(freshState()));
    const out = await svc.listAll();
    expect(out.length).toBeGreaterThan(0);
    expect(out.every((t) => t.source === 'builtin')).toBe(true);
    const names = out.map((t) => t.name);
    expect(names).toContain('Read');
    expect(names).toContain('Write');
    expect(names).toContain('Edit');
    expect(names).toContain('Bash');
    expect(names).toContain('Grep');
    expect(names).toContain('Glob');
  });

  it('should merge session-scoped MCP tools when a session exists', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s', 1));
    state.tools.push(
      { name: 'mcp:lark:search', description: 'l', source: 'mcp' },
    );
    const svc = new ToolService(makeFakeBridge(state));
    const out = await svc.listAll();
    // Builtin tools are always present.
    const names = out.map((t) => t.name);
    expect(names).toContain('Read');
    expect(names).toContain('Bash');
    // MCP tool from the session is merged in.
    expect(names).toContain('mcp:lark:search');
    const mcpTool = out.find((t) => t.name === 'mcp:lark:search');
    expect(mcpTool?.source).toBe('mcp');
    expect(mcpTool?.mcp_server_id).toBe('lark');
  });
});

describe('McpService.list', () => {
  it('returns [] when no sessions exist (registrar not reachable)', async () => {
    const svc = new McpService(makeFakeBridge(freshState()));
    expect(await svc.list()).toEqual([]);
  });

  it('returns adapted servers from the most-recent session', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s', 1));
    state.mcpServers.push({
      name: 'lark',
      transport: 'stdio',
      status: 'connected',
      toolCount: 7,
    });
    const svc = new McpService(makeFakeBridge(state));
    const out = await svc.list();
    expect(out).toHaveLength(1);
    expect(out[0]!.id).toBe('lark');
    expect(out[0]!.tool_count).toBe(7);
  });
});

describe('McpService.restart', () => {
  it('throws McpServerNotFoundError when no sessions exist', async () => {
    const svc = new McpService(makeFakeBridge(freshState()));
    await expect(svc.restart('lark')).rejects.toBeInstanceOf(McpServerNotFoundError);
  });

  it('throws McpServerNotFoundError when the id is not in the registrar', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s', 1));
    state.mcpServers.push({
      name: 'lark',
      transport: 'stdio',
      status: 'connected',
      toolCount: 1,
    });
    const svc = new McpService(makeFakeBridge(state));
    await expect(svc.restart('unknown')).rejects.toBeInstanceOf(McpServerNotFoundError);
  });

  it('calls bridge.rpc.reconnectMcpServer({name}) and returns {restarting:true}', async () => {
    const state = freshState();
    state.sessions.push(fakeSession('s', 1));
    state.mcpServers.push({
      name: 'lark',
      transport: 'stdio',
      status: 'connected',
      toolCount: 1,
    });
    const svc = new McpService(makeFakeBridge(state));
    const result = await svc.restart('lark');
    expect(result).toEqual({ restarting: true });
    expect(state.reconnectCalls).toHaveLength(1);
    expect(state.reconnectCalls[0]!.name).toBe('lark');
  });
});

describe('McpService.listAll (global MCP)', () => {
  it('should return global MCP servers and tools without a session', async () => {
    const state = freshState();
    state.globalMcpServers.push({
      name: 'fs',
      transport: 'stdio',
      status: 'connected',
      toolCount: 2,
    });
    state.globalMcpTools.push(
      { name: 'read_file', description: 'Read a file', mcpServerId: 'fs' },
      { name: 'write_file', description: 'Write a file', mcpServerId: 'fs' },
    );
    const svc = new McpService(makeFakeBridge(state));
    const { servers, tools } = await svc.listAll();
    expect(servers).toHaveLength(1);
    expect(servers[0]!.id).toBe('fs');
    expect(servers[0]!.tool_count).toBe(2);
    expect(tools).toHaveLength(2);
    expect(tools[0]!.source).toBe('mcp');
    expect(tools[0]!.mcp_server_id).toBe('fs');
  });

  it('should return empty arrays when no global MCP servers are configured', async () => {
    const svc = new McpService(makeFakeBridge(freshState()));
    const { servers, tools } = await svc.listAll();
    expect(servers).toEqual([]);
    expect(tools).toEqual([]);
  });
});

describe('McpService global MCP CRUD', () => {
  it('should call reloadAll via bridge.rpc.reloadGlobalMcp', async () => {
    const state = freshState();
    const svc = new McpService(makeFakeBridge(state));
    await svc.reloadAll();
    expect(state.globalMcpReloadCalls).toBe(1);
  });

  it('should call createServer via bridge.rpc.createGlobalMcpServer', async () => {
    const state = freshState();
    const svc = new McpService(makeFakeBridge(state));
    await svc.createServer('my-server', { transport: 'stdio', command: 'npx', args: ['-y', 'fs'] });
    expect(state.globalMcpCreateCalls).toHaveLength(1);
    expect(state.globalMcpCreateCalls[0]!.name).toBe('my-server');
    expect(state.globalMcpCreateCalls[0]!.config.transport).toBe('stdio');
    expect((state.globalMcpCreateCalls[0]!.config as { command?: string }).command).toBe('npx');
  });

  it('should call updateServer via bridge.rpc.updateGlobalMcpServer', async () => {
    const state = freshState();
    const svc = new McpService(makeFakeBridge(state));
    await svc.updateServer('my-server', { transport: 'stdio', command: 'node' });
    expect(state.globalMcpUpdateCalls).toHaveLength(1);
    expect(state.globalMcpUpdateCalls[0]!.name).toBe('my-server');
    expect(state.globalMcpUpdateCalls[0]!.config.transport).toBe('stdio');
    expect((state.globalMcpUpdateCalls[0]!.config as { command?: string }).command).toBe('node');
  });

  it('should call deleteServer via bridge.rpc.deleteGlobalMcpServer', async () => {
    const state = freshState();
    const svc = new McpService(makeFakeBridge(state));
    await svc.deleteServer('my-server');
    expect(state.globalMcpDeleteCalls).toHaveLength(1);
    expect(state.globalMcpDeleteCalls[0]!.name).toBe('my-server');
  });
});
