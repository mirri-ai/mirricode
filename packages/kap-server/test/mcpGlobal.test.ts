/**
 * `/api/v1/mcp/global` REST routes — server-v2 port.
 *
 * Covers the wire contract of the global MCP server config management endpoints:
 *   - GET    /mcp/global/servers              → {servers: McpServer[]}
 *   - GET    /mcp/global/tools                → {tools: ToolDescriptor[]}
 *   - POST   /mcp/global/servers              → {reloading: true}
 *   - PUT    /mcp/global/servers/{name}       → {reloading: true}
 *   - DELETE /mcp/global/servers/{name}       → {reloading: true}
 *   - POST   /mcp/global/servers/{name}:enable|disable|connect
 *   - POST   /mcp/global/servers:reload       → {reloading: true}
 *   - GET    /mcp/global/toggle-state         → {disabled_servers, disabled_tools}
 *   - POST   /mcp/global/servers/{serverName}/tools/{tail}:enable|disable
 *   - POST   /mcp/global/servers-test         → TestConnectMcpServerResponse
 */

import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { IModelCatalog } from '@mirri-ai/agent-core-v2';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { type RunningServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { authHeaders } from './helpers/auth';

interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

interface McpServerWire {
  id: string;
  name: string;
  transport: string;
  status: string;
  tool_count: number;
  last_error?: string;
}

describe('server-v2 /api/v1/mcp/global', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;
  let base: string;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-v2-mcp-global-'));
    const modelCatalog: IModelCatalog = {
      _serviceBrand: undefined,
      get: () => { throw new Error('modelCatalog.get not exercised'); },
      getRequester: () => { throw new Error('modelCatalog.getRequester not exercised'); },
      inspect: () => { throw new Error('modelCatalog.inspect not exercised'); },
      ping: () => { throw new Error('modelCatalog.ping not exercised'); },
      getById: () => undefined,
      listModels: async () => [],
      listProviders: async () => [],
      getProvider: async () => { throw new Error('modelCatalog.getProvider not exercised'); },
      setDefaultModel: async () => { throw new Error('modelCatalog.setDefaultModel not exercised'); },
      removeModel: async () => ({ deleted: true }),
    };
    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
      seeds: [[IModelCatalog, modelCatalog]],
    });
    base = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true });
      home = undefined;
    }
  });

  async function getJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      headers: authHeaders(server as RunningServer),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function postJson<T>(
    path: string,
    body?: unknown,
  ): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'POST',
      headers: authHeaders(server as RunningServer, { 'content-type': 'application/json' }),
      body: JSON.stringify(body ?? {}),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function putJson<T>(
    path: string,
    body?: unknown,
  ): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'PUT',
      headers: authHeaders(server as RunningServer, { 'content-type': 'application/json' }),
      body: JSON.stringify(body ?? {}),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function deleteJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'DELETE',
      headers: authHeaders(server as RunningServer),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  describe('GET /api/v1/mcp/global/servers', () => {
    it('should return an empty list when no servers are configured', async () => {
      const { status, body } = await getJson<{ servers: McpServerWire[] }>('/api/v1/mcp/global/servers');
      expect(status).toBe(200);
      expect(body.code).toBe(0);
      expect(body.data.servers).toEqual([]);
    });
  });

  describe('GET /api/v1/mcp/global/tools', () => {
    it('should return an empty list when no servers are connected', async () => {
      const { body } = await getJson<{ tools: unknown[] }>('/api/v1/mcp/global/tools');
      expect(body.code).toBe(0);
      expect(body.data.tools).toEqual([]);
    });
  });

  describe('POST /api/v1/mcp/global/servers', () => {
    it('should create a server and verify it appears in list', async () => {
      const { body } = await postJson<{ reloading: true }>('/api/v1/mcp/global/servers', {
        name: 'test-server',
        config: {
          transport: 'stdio',
          command: 'echo',
          args: ['hello'],
        },
      });
      expect(body.code).toBe(0);
      expect(body.data.reloading).toBe(true);

      // Verify the mcp.json was written (homeDir IS the mirri home, no .mirri-code subdirectory)
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['test-server']).toBeDefined();
      expect(parsed.mcpServers['test-server'].transport).toBe('stdio');
    });
  });

  describe('PUT /api/v1/mcp/global/servers/{name}', () => {
    it('should update an existing server', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'updatable-server',
        config: {
          transport: 'stdio',
          command: 'echo',
          args: ['old'],
        },
      });

      const { body } = await putJson<{ reloading: true }>('/api/v1/mcp/global/servers/updatable-server', {
        config: {
          transport: 'stdio',
          command: 'echo',
          args: ['new'],
        },
      });
      expect(body.code).toBe(0);
      expect(body.data.reloading).toBe(true);

      // Verify the mcp.json was updated
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['updatable-server'].args).toEqual(['new']);
    });

    it('should return 40408 for updating a non-existent server', async () => {
      const { body } = await putJson<null>('/api/v1/mcp/global/servers/nonexistent', {
        config: { transport: 'stdio', command: 'echo' },
      });
      expect(body.code).toBe(40408);
    });
  });

  describe('DELETE /api/v1/mcp/global/servers/{name}', () => {
    it('should delete an existing server', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'deletable-server',
        config: { transport: 'stdio', command: 'echo' },
      });

      const { body } = await deleteJson<{ reloading: true }>('/api/v1/mcp/global/servers/deletable-server');
      expect(body.code).toBe(0);
      expect(body.data.reloading).toBe(true);

      // Verify the mcp.json no longer has the entry
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['deletable-server']).toBeUndefined();
    });

    it('should return 40408 for deleting a non-existent server', async () => {
      const { body } = await deleteJson<null>('/api/v1/mcp/global/servers/nonexistent');
      expect(body.code).toBe(40408);
    });
  });

  describe('POST /api/v1/mcp/global/servers/{name}:disable', () => {
    it('should disable a server by setting enabled: false in mcp.json', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'disablable-server',
        config: { transport: 'stdio', command: 'echo' },
      });

      const { body } = await postJson<{ reloading: true }>('/api/v1/mcp/global/servers/disablable-server:disable');
      expect(body.code).toBe(0);

      // Verify the mcp.json has enabled: false
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['disablable-server'].enabled).toBe(false);
    });

    it('should re-enable a server by removing enabled field', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'reenablable-server',
        config: { transport: 'stdio', command: 'echo' },
      });
      await postJson('/api/v1/mcp/global/servers/reenablable-server:disable');

      const { body } = await postJson<{ reloading: true }>('/api/v1/mcp/global/servers/reenablable-server:enable');
      expect(body.code).toBe(0);

      // Verify the mcp.json no longer has enabled: false
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['reenablable-server'].enabled).toBeUndefined();
    });

    it('should return 40408 for disabling a non-existent server', async () => {
      const { body } = await postJson<null>('/api/v1/mcp/global/servers/nonexistent:disable');
      expect(body.code).toBe(40408);
    });
  });

  describe('POST /api/v1/mcp/global/servers:reload', () => {
    it('should return reloading: true', async () => {
      const { body } = await postJson<{ reloading: true }>('/api/v1/mcp/global/servers:reload');
      expect(body.code).toBe(0);
      expect(body.data.reloading).toBe(true);
    });
  });

  describe('GET /api/v1/mcp/global/toggle-state', () => {
    it('should return empty lists when no servers are disabled', async () => {
      const { body } = await getJson<{ disabled_servers: string[]; disabled_tools: string[] }>('/api/v1/mcp/global/toggle-state');
      expect(body.code).toBe(0);
      expect(body.data.disabled_servers).toEqual([]);
      expect(body.data.disabled_tools).toEqual([]);
    });

    it('should list disabled servers', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'disabled-srv',
        config: { transport: 'stdio', command: 'echo', enabled: false },
      });

      const { body } = await getJson<{ disabled_servers: string[]; disabled_tools: string[] }>('/api/v1/mcp/global/toggle-state');
      expect(body.code).toBe(0);
      expect(body.data.disabled_servers).toContain('disabled-srv');
    });

    it('should list disabled tools qualified as mcp__{server}__{tool} in toggle-state', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'toggle-state-tool-srv',
        config: { transport: 'stdio', command: 'echo' },
      });
      const { body: toggleBody } = await postJson<{ ok: true }>('/api/v1/mcp/global/servers/toggle-state-tool-srv/tools/my_tool:disable');
      expect(toggleBody.code).toBe(0);

      const { body } = await getJson<{ disabled_servers: string[]; disabled_tools: string[] }>('/api/v1/mcp/global/toggle-state');
      expect(body.code).toBe(0);
      expect(body.data.disabled_tools).toContain('mcp__toggle-state-tool-srv__my_tool');
    });
  });

  describe('POST /api/v1/mcp/global/servers/{serverName}/tools/{tail}:enable|disable', () => {
    it('should disable a tool in the mcp.json config', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'tool-toggle-srv',
        config: { transport: 'stdio', command: 'echo' },
      });

      const { body } = await postJson<{ ok: true }>('/api/v1/mcp/global/servers/tool-toggle-srv/tools/my_tool:disable');
      expect(body.code).toBe(0);
      expect(body.data.ok).toBe(true);

      // Verify in mcp.json
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['tool-toggle-srv'].disabledTools).toContain('my_tool');
    });

    it('should re-enable a tool by removing from disabledTools and adding to enabledTools', async () => {
      await postJson('/api/v1/mcp/global/servers', {
        name: 'tool-reenable-srv',
        config: { transport: 'stdio', command: 'echo', disabledTools: ['old_tool'] },
      });

      const { body } = await postJson<{ ok: true }>('/api/v1/mcp/global/servers/tool-reenable-srv/tools/old_tool:enable');
      expect(body.code).toBe(0);

      // Verify in mcp.json
      const mcpJsonPath = join(home!, 'mcp.json');
      const content = await readFile(mcpJsonPath, 'utf-8');
      const parsed = JSON.parse(content);
      expect(parsed.mcpServers['tool-reenable-srv'].disabledTools).toBeUndefined();
      expect(parsed.mcpServers['tool-reenable-srv'].enabledTools).toContain('old_tool');
    });

    it('should return 40408 for toggling a tool on a non-existent server', async () => {
      const { body } = await postJson<null>('/api/v1/mcp/global/servers/nonexistent/tools/tool:disable');
      expect(body.code).toBe(40408);
    });
  });
});
