// apps/mirri-web/test/ws-rpc-api-mcp.test.ts
// Given the rust engine's layered MCP config surface (`mcp.config.*`, the
// `mcp.status` pool view, `mcp.config.test` and `mcp.reconnect`) and the
// content-level `message.steer`, the WsRpcApi adapters must call exactly the
// authoritative wire methods with correctly mapped params and translate the
// wire shapes back to the v1 AppMcp* shapes the Settings panel consumes.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import type { AppMcpServerConfig } from '../src/api/types';

/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

/** Build a WsRpcApi whose transport is a mocked `client.call`. */
function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>();
  (api as unknown as { client: { call: RpcCall } }).client = { call };
  return { api, call };
}

const config: AppMcpServerConfig = {
  transport: 'stdio',
  command: 'npx',
  args: ['-y', 'server'],
};

function configEntry(overrides: Partial<{
  name: string;
  source: 'user' | 'projectRoot' | 'project' | 'plugin';
  runtimeName: string | undefined;
  disabledTools: string[] | undefined;
  enabledTools: string[] | undefined;
  enabled: boolean;
}> = {}) {
  return {
    name: overrides.name ?? 'fs',
    source: overrides.source ?? 'user',
    ...(overrides.runtimeName !== undefined ? { runtimeName: overrides.runtimeName } : {}),
    config,
    effective: {
      enabled: overrides.enabled ?? true,
      ...(overrides.disabledTools !== undefined ? { disabledTools: overrides.disabledTools } : {}),
      ...(overrides.enabledTools !== undefined ? { enabledTools: overrides.enabledTools } : {}),
    },
  };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi MCP management', () => {
  it('should map pool rows and joined config into panel cards when the engine answers mcp.status and mcp.config.list', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      switch (method) {
        case 'mcp.status':
          return [
            { name: 'fs', transport: 'stdio', status: 'connected', toolCount: 3, pool: 'user', source: 'user' },
            { name: 'broken', transport: 'http', status: 'failed', toolCount: 0, pool: 'workspace', source: 'project', error: 'boom' },
            { name: 'auth', transport: 'http', status: 'needs-auth', toolCount: 0, pool: 'workspace', source: 'project' },
            { name: 'off', transport: 'stdio', status: 'disabled', toolCount: 0, pool: 'user', source: 'user' },
            { name: 'slow', transport: 'stdio', status: 'pending', toolCount: 0, pool: 'user', source: 'user' },
          ];
        case 'mcp.config.list':
          return [configEntry()];
        default:
          throw new Error(`unexpected method: ${method}`);
      }
    });

    const servers = await api.listGlobalMcpServers();

    expect(servers.map((s) => [s.name, s.status])).toEqual([
      ['fs', 'connected'],
      ['broken', 'error'],
      ['auth', 'error'],
      ['off', 'disconnected'],
      ['slow', 'connecting'],
    ]);
    // The stored config from the layered view is joined in by name.
    expect(servers[0]!.config).toEqual(config);
    // Failure rows carry the engine error as the panel's lastError.
    expect(servers[1]!.lastError).toBe('boom');
    // The needs-auth state has no engine error string — the adapter supplies one.
    expect(servers[2]!.lastError).toBe('authentication required');
    // Cross-pool same-name rows get pool-qualified ids (no Vue key collisions).
    expect(new Set(servers.map((s) => s.id)).size).toBe(servers.length);
  });

  it('should list config-layer entries without a pool slot as disconnected cards when the engine has not loaded their layer', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      switch (method) {
        case 'mcp.status':
          return [];
        case 'mcp.config.list':
          return [configEntry({ name: 'dormant', source: 'project' })];
        default:
          throw new Error(`unexpected method: ${method}`);
      }
    });

    const servers = await api.listGlobalMcpServers();

    expect(servers).toHaveLength(1);
    expect(servers[0]).toMatchObject({ name: 'dormant', status: 'disconnected', config });
  });

  it('should present plugin-sourced entries under their runtime name when listing servers and config entries', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      switch (method) {
        case 'mcp.status':
          return [
            { name: 'plugin-docs:fs', transport: 'stdio', status: 'connected', toolCount: 2, pool: 'plugin', source: 'plugin' },
          ];
        case 'mcp.config.list':
          return [configEntry({ name: 'fs', source: 'plugin', runtimeName: 'plugin-docs:fs' })];
        default:
          throw new Error(`unexpected method: ${method}`);
      }
    });

    const [servers, configEntries] = await Promise.all([
      api.listGlobalMcpServers(),
      api.getGlobalMcpConfig(),
    ]);

    expect(servers).toHaveLength(1);
    expect(servers[0]).toMatchObject({ name: 'plugin-docs:fs', config });
    expect(configEntries[0]).toMatchObject({ name: 'plugin-docs:fs' });
  });

  it('should return the stored config verbatim for both views when the wire has no env-expanded form', async () => {
    const { api, call } = createApi();
    const stored: AppMcpServerConfig = {
      transport: 'stdio',
      command: 'npx',
      env: { KEY: '${MY_SECRET}' },
    };
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [{ ...configEntry(), config: stored }];
      }
      throw new Error(`unexpected method: ${method}`);
    });

    const entries = await api.getGlobalMcpConfig();

    expect(call).toHaveBeenCalledWith('mcp.config.list');
    expect(entries).toHaveLength(1);
    expect(entries[0]!.source).toEqual(stored);
    expect(entries[0]!.resolved).toEqual(stored);
  });

  it('should write server create and update through mcp.config.set scoped to the user layer', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.createMcpServer('fs', config);
    await api.updateMcpServer('fs', config);

    expect(call).toHaveBeenCalledTimes(2);
    expect(call).toHaveBeenNthCalledWith(1, 'mcp.config.set', { scope: 'user', name: 'fs', config });
    expect(call).toHaveBeenNthCalledWith(2, 'mcp.config.set', { scope: 'user', name: 'fs', config });
  });

  it('should delete a server through mcp.config.remove scoped to the user layer', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.deleteMcpServer('fs');

    expect(call).toHaveBeenCalledWith('mcp.config.remove', { scope: 'user', name: 'fs' });
  });

  it('should return the reconcile summary when the wire answers mcp.sync', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ added: 1, removed: 0, reconnected: 0, refreshed: 2 });

    const summary = await api.reloadMcpServers();

    // The reload surface IS the reconcile kernel (F-6/F-12): the summary
    // of what the pass applied comes straight back to the caller.
    expect(call).toHaveBeenCalledWith('mcp.sync');
    expect(summary).toEqual({ added: 1, removed: 0, reconnected: 0, refreshed: 2 });
  });

  it('should rewrite the defining layer with the enabled flag flipped when a server is enabled or disabled', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({ name: 'fs', source: 'user' }),
          configEntry({ name: 'ws', source: 'project' }),
        ];
      }
      return [];
    });

    await api.disableMcpServer('fs');
    await api.enableMcpServer('ws');

    expect(call).toHaveBeenCalledWith('mcp.config.set', {
      scope: 'user',
      name: 'fs',
      config: { ...config, enabled: false },
    });
    expect(call).toHaveBeenCalledWith('mcp.config.set', {
      scope: 'workspace',
      name: 'ws',
      config: { ...config, enabled: true },
    });
  });

  it('should reject enable or disable of read-only layers instead of silently writing elsewhere', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({ name: 'shared', source: 'projectRoot' }),
          configEntry({ name: 'bundled', source: 'plugin', runtimeName: 'plugin-x:bundled' }),
        ];
      }
      return [];
    });

    await expect(api.enableMcpServer('shared')).rejects.toThrow(/read-only/);
    await expect(api.disableMcpServer('plugin-x:bundled')).rejects.toThrow(/plugin/);
    expect(call).not.toHaveBeenCalledWith('mcp.config.set', expect.anything());
  });

  it('should send the complete denylist through mcp.config.setToolFilter when one tool is toggled', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [configEntry({ name: 'fs', disabledTools: ['read', 'write'] })];
      }
      return [];
    });

    await api.disableGlobalMcpTool('fs', 'list');
    await api.enableGlobalMcpTool('fs', 'read');

    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: { disabledTools: ['read', 'write', 'list'] },
    });
    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: { disabledTools: ['write'] },
    });
  });

  it('should forward the stored allowlist alongside the denylist so the patch semantics cannot clear it', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({
            name: 'fs',
            disabledTools: ['read'],
            enabledTools: ['list_dir', 'read_file'],
          }),
        ];
      }
      return [];
    });

    await api.disableGlobalMcpTool('fs', 'stat');

    // The setToolFilter patch is the entry's complete new filter state
    // (absent fields clear), so the allowlist must ride along unchanged.
    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: {
        enabledTools: ['list_dir', 'read_file'],
        disabledTools: ['read', 'stat'],
      },
    });
  });

  it('should omit the allowlist field when the entry stores none so an empty allowlist is never written', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [configEntry({ name: 'fs', disabledTools: [] })];
      }
      return [];
    });

    await api.disableGlobalMcpTool('fs', 'stat');

    // An explicit `enabledTools: []` would mean "no tool allowed" — the
    // field must stay absent when the entry carries no allowlist.
    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: { disabledTools: ['stat'] },
    });
  });


  it('should fold the per-server mcp.tools views into panel descriptors when the engine answers mcp.status', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string, params?: unknown) => {
      if (method === 'mcp.status') {
        // The pool view lists workspace entries first; same-name
        // workspace entries shadow user ones (spec §4.2) — the fold
        // queries each name once.
        return [
          { name: 'fs', transport: 'http', status: 'connected', toolCount: 2, pool: 'workspace', source: 'project' },
          { name: 'fs', transport: 'http', status: 'connected', toolCount: 1, pool: 'user', source: 'user' },
          { name: 'web', transport: 'http', status: 'connected', toolCount: 1, pool: 'user', source: 'user' },
        ];
      }
      if (method === 'mcp.tools') {
        const { name } = params as { name: string };
        if (name === 'fs') {
          return {
            connected: true,
            tools: [
              { name: 'fs_read', description: 'Read a file', enabled: true },
              { name: 'fs_write', description: 'Write a file', enabled: true },
              // A per-tool-disabled entry carries its verdict into the panel list
              // surface, not this popup's callable list.
              { name: 'fs_admin', description: 'Admin only', enabled: false },
            ],
          };
        }
        if (name === 'web') {
          return {
            connected: true,
            tools: [{ name: 'search', description: 'Web search', enabled: true }],
          };
        }
      }
      throw new Error(`unexpected method: ${method}`);
    });

    const tools = await api.listGlobalMcpTools();

    expect(call).toHaveBeenCalledWith('mcp.status');
    expect(call).toHaveBeenCalledWith('mcp.tools', { name: 'fs' });
    expect(call).toHaveBeenCalledWith('mcp.tools', { name: 'web' });
    // One pool view plus one per-server tool listing per unique name (the
    // shadowed user 'fs' entry is not queried twice).
    expect(call).toHaveBeenCalledTimes(3);
    // All advertised tools fold in — disabled ones included, each carrying
    // the engine's effective verdict: the settings popup renders per-tool
    // switches, and a disabled tool must stay listed (with enabled:false)
    // to be re-enableable (SDR-S005 / PS3.2).
    expect(tools).toEqual([
      { name: 'fs_read', description: 'Read a file', source: 'mcp', mcpServerId: 'fs', enabled: true },
      { name: 'fs_write', description: 'Write a file', source: 'mcp', mcpServerId: 'fs', enabled: true },
      { name: 'fs_admin', description: 'Admin only', source: 'mcp', mcpServerId: 'fs', enabled: false },
      { name: 'search', description: 'Web search', source: 'mcp', mcpServerId: 'web', enabled: true },
    ]);
  });

  it('should answer an empty tool list when the engine has no connected MCP servers', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [];
      }
      throw new Error(`unexpected method: ${method}`);
    });

    const tools = await api.listGlobalMcpTools();

    expect(tools).toEqual([]);
  });

  it('should derive the panel toggle state from the effective config entries', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({ name: 'off', enabled: false }),
          configEntry({ name: 'fs', disabledTools: ['read'] }),
          configEntry({ name: 'bundled', source: 'plugin', runtimeName: 'plugin-x:bundled', disabledTools: ['search'] }),
        ];
      }
      return [];
    });

    const state = await api.getGlobalMcpToggleState();

    expect(state.disabledServers).toEqual(['off']);
    expect(state.disabledTools).toEqual([
      'mcp__fs__read',
      'mcp__plugin-x:bundled__search',
    ]);
  });

  it('should translate the fresh-connect test answer into the panel result shape', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ status: 'ok', toolCount: 7 });

    const result = await api.testConnectMcpServer(config);

    expect(call).toHaveBeenCalledWith('mcp.config.test', { config });
    expect(result).toEqual({ status: 'connected', toolCount: 7, tools: [] });
  });

  it('should surface the engine error of a failed test connection', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ status: 'error', toolCount: 0, error: 'spawn failed' });

    const result = await api.testConnectMcpServer(config);

    expect(result).toEqual({ status: 'error', toolCount: 0, error: 'spawn failed', tools: [] });
  });

  it('should reconnect through mcp.reconnect and answer with the refreshed pool row when connecting one server', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      switch (method) {
        case 'mcp.reconnect':
          return { ok: true };
        case 'mcp.status':
          return [
            { name: 'fs', transport: 'stdio', status: 'connected', toolCount: 3, pool: 'user', source: 'user' },
          ];
        case 'mcp.config.list':
          return [configEntry()];
        default:
          throw new Error(`unexpected method: ${method}`);
      }
    });

    const { server, tools } = await api.connectMcpServer('fs');

    expect(call).toHaveBeenCalledWith('mcp.reconnect', { name: 'fs' });
    expect(server).toMatchObject({ name: 'fs', status: 'connected', toolCount: 3, config });
    expect(tools).toEqual([]);
  });
});

describe('WsRpcApi steerSession', () => {
  it('should park text content in the running turn through message.steer', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    const result = await api.steerSession('s1', {
      content: [{ type: 'text', text: 'one' }, { type: 'text', text: 'two' }],
    });

    expect(call).toHaveBeenCalledWith('message.steer', {
      session_id: 's1',
      input: [
        { type: 'text', text: 'one' },
        { type: 'text', text: 'two' },
      ],
    });
    expect(result).toEqual({ steered: true });
  });

  it('should address the steering to a specific agent when the submission carries one', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.steerSession('s1', {
      content: [{ type: 'text', text: 'hi' }],
      agentId: 'main',
    });

    expect(call).toHaveBeenCalledWith('message.steer', {
      session_id: 's1',
      agent_id: 'main',
      input: [{ type: 'text', text: 'hi' }],
    });
  });

  it('should steer structured parts like message.send does', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.steerSession('s1', {
      content: [
        { type: 'text', text: 'look' },
        { type: 'image', source: { kind: 'file', fileId: 'f1' } },
      ],
    });

    expect(call).toHaveBeenCalledWith('message.steer', {
      session_id: 's1',
      input: [
        { type: 'text', text: 'look' },
        { type: 'image', source: { kind: 'file', file_id: 'f1' } },
      ],
    });
  });
});

describe('WsRpcApi MCP management (allowlist re-enable, SDR-S005)', () => {
  it('should lift the allowlist exclusion when re-enabling a tool the allowlist filtered out', async () => {
    // Given a server whose stored allowlist excludes one advertised tool
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({
            name: 'fs',
            disabledTools: [],
            enabledTools: ['list_dir', 'read_file'],
          }),
        ];
      }
      return [];
    });

    // When that excluded tool is switched back on
    await api.enableGlobalMcpTool('fs', 'admin_wipe');

    // Then the allowlist is rewritten with the tool lifted in — the
    // engine's effective verdict flips to enabled (a denylist-only write
    // would leave it enabled:false forever, PS3.2)
    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: { enabledTools: ['list_dir', 'read_file', 'admin_wipe'], disabledTools: [] },
    });
  });
});

describe('WsRpcApi MCP management (bulk filter and snapshot, SDR-S005)', () => {
  it('should send one complete-list setToolFilter call and forward the stored allowlist unchanged when the denylist is bulk-rewritten', async () => {
    // Given a server with both filter arrays stored
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({
            name: 'fs',
            disabledTools: ['read'],
            enabledTools: ['list_dir', 'read_file'],
          }),
        ];
      }
      return [];
    });

    // When the panel bulk-rewrites the denylist in one call
    await api.setMcpToolFilter('fs', ['write', 'chmod']);

    // Then a single setToolFilter carries the complete new denylist plus
    // the stored allowlist riding along unchanged (one write RPC — the only
    // other call is the mcp.config.list read it derives from)
    const writes = call.mock.calls.filter((c) => c[0] === 'mcp.config.setToolFilter');
    expect(writes).toHaveLength(1);
    expect(call).toHaveBeenCalledWith('mcp.config.setToolFilter', {
      name: 'fs',
      filter: { enabledTools: ['list_dir', 'read_file'], disabledTools: ['write', 'chmod'] },
    });
  });

  it('should answer the allowlist snapshot with denylist overrides applied when a disabled server has both arrays', async () => {
    // Given a disabled server whose allowlist lists three tools and whose
    // denylist excludes one of them
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [
          configEntry({
            name: 'fs',
            disabledTools: ['admin'],
            enabledTools: ['list_dir', 'read_file', 'admin'],
          }),
        ];
      }
      return [];
    });

    // When the settings panel asks for the disabled server's snapshot
    const snapshot = await api.getDisabledServerToolSnapshot('fs');

    // Then every allowlisted tool appears, the denylisted one reads
    // disabled (the engine's is_tool_allowed puts the denylist first)
    expect(snapshot.tools).toEqual([
      { name: 'list_dir', enabled: true },
      { name: 'read_file', enabled: true },
      { name: 'admin', enabled: false },
    ]);
    void call;
  });

  it('should answer an empty snapshot when no filter preference is stored', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.config.list') {
        return [configEntry({ name: 'fs' })];
      }
      return [];
    });

    expect(await api.getDisabledServerToolSnapshot('fs')).toEqual({ tools: [] });
    expect(await api.getDisabledServerToolSnapshot('ghost')).toEqual({ tools: [] });
    void call;
  });
});
