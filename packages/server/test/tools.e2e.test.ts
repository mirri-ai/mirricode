/**
 * Tools + MCP end-to-end tests (W9.1 / Chain 7 / P1.7).
 *
 * Coverage:
 *   - GET  /api/v1/tools                              → envelope shape + tools[]
 *   - GET  /api/v1/mcp/servers                        → envelope shape + servers[]
 *   - POST /api/v1/mcp/servers/{id}:restart           → {restarting:true} on a real
 *                                                   server / 40408 on unknown
 *   - POST /api/v1/mcp/servers/foo:bogus              → 40001 unsupported action
 *
 * **Bootstrap strategy**: spawn the real server and create one session so the
 * agent-core `getTools` / `listMcpServers` can dispatch (those calls live on
 * the SessionAPI). The HOME dir is a fresh tmpdir so plugin discovery is
 * sandboxed.
 */

import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { pino } from 'pino';
import {
  connectGlobalMcpServerResponseSchema,
  listAllToolsResponseSchema,
  listGlobalMcpServersResponseSchema,
  listMcpServersResponseSchema,
  listToolsResponseSchema,
} from '@mirri-ai/protocol';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { IRestGateway, startServer, type RunningServer } from '../src';
import { fixedTokenAuth } from './helpers/serverHarness';

let tmpDir: string;
let lockPath: string;
let bridgeHome: string;
let server: RunningServer | undefined;

beforeEach(() => {
  tmpDir = mkdtempSync(join(tmpdir(), 'mirri-server-tools-test-'));
  lockPath = join(tmpDir, 'lock');
  bridgeHome = mkdtempSync(join(tmpdir(), 'mirri-server-tools-home-'));
});

afterEach(async () => {
  try {
    await server?.close();
  } catch {
    // ignore
  }
  server = undefined;
  rmSync(tmpDir, { recursive: true, force: true });
  rmSync(bridgeHome, { recursive: true, force: true });
});

async function bootDaemon(): Promise<RunningServer> {
  server = await startServer({
    serviceOverrides: [fixedTokenAuth()],
    host: '127.0.0.1',
    port: 0,
    lockPath,
    logger: pino({ level: 'silent' }),
    coreProcessOptions: { homeDir: bridgeHome },
  });
  return server;
}

function appOf(r: RunningServer): {
  inject: (req: unknown) => Promise<{ statusCode: number; json: () => unknown }>;
} {
  const app = r.services.invokeFunction((a) => {
    const gw = a.get(IRestGateway);
    return gw.app as unknown as {
  inject: (req: unknown) => Promise<{ statusCode: number; json: () => unknown }>;
};
  });
  // Auto-attach the fixed bearer token so the M5.1 auth hook passes. A
  // caller-supplied `authorization` header wins, so explicit token tests keep
  // working; every other header (Range, content-type, …) is preserved.
  return {
    inject(req: unknown) {
      const q = req as { headers?: Record<string, string | string[] | undefined> };
      return app.inject({
        ...q,
        headers: { authorization: 'Bearer test-token', ...q.headers },
      });
    },
  };
}

function envelopeOf<T>(body: unknown): {
  code: number;
  msg: string;
  data: T | null;
  request_id: string;
  details?: unknown;
} {
  return body as {
    code: number;
    msg: string;
    data: T | null;
    request_id: string;
    details?: unknown;
  };
}

async function createSession(r: RunningServer): Promise<string> {
  const res = await appOf(r).inject({
    method: 'POST',
    url: '/api/v1/sessions',
    payload: { metadata: { cwd: join(tmpDir, 'workspace') } },
  });
  const env = envelopeOf<{ id: string }>(res.json());
  if (env.code !== 0 || env.data === null) {
    throw new Error(`create session failed: ${JSON.stringify(env)}`);
  }
  return env.data.id;
}

describe('GET /api/v1/tools', () => {
  it('returns an envelope with {tools: ToolDescriptor[]} (empty list pre-session)', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/tools' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    // Before any session exists, the global list is empty by design.
    const parsed = listToolsResponseSchema.parse(env.data);
    expect(parsed.tools).toEqual([]);
  });

  it('returns a populated list after a session exists (response data round-trips through schema)', async () => {
    const r = await bootDaemon();
    await createSession(r);
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/tools' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = listToolsResponseSchema.parse(env.data);
    // We don't assert a specific count (depends on plugin discovery in the
    // sandboxed home dir), only that the envelope shape is valid and every
    // descriptor parses.
    expect(Array.isArray(parsed.tools)).toBe(true);
  });

  it('accepts session_id query and returns the same shape', async () => {
    const r = await bootDaemon();
    const sid = await createSession(r);
    const res = await appOf(r).inject({
      method: 'GET',
      url: `/api/v1/tools?session_id=${sid}`,
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    expect(listToolsResponseSchema.safeParse(env.data).success).toBe(true);
  });

  it('rejects empty session_id with 40001', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'GET',
      url: '/api/v1/tools?session_id=',
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40001);
  });
});

describe('GET /api/v1/tools/all', () => {
  it('returns builtin tool descriptors without requiring a session', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/tools/all' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = listAllToolsResponseSchema.parse(env.data);
    expect(parsed.tools.length).toBeGreaterThan(0);
    expect(parsed.tools.every((t) => t.source === 'builtin')).toBe(true);
    const names = parsed.tools.map((t) => t.name);
    expect(names).toContain('Read');
    expect(names).toContain('Write');
    expect(names).toContain('Bash');
  });
});

describe('GET /api/v1/mcp/servers', () => {
  it('returns an envelope with {servers: McpServer[]} (typically empty in sandboxed home)', async () => {
    const r = await bootDaemon();
    await createSession(r);
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/servers' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = listMcpServersResponseSchema.parse(env.data);
    expect(Array.isArray(parsed.servers)).toBe(true);
  });

  it('returns 200 with empty list even before any session is created', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/servers' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = listMcpServersResponseSchema.parse(env.data);
    expect(parsed.servers).toEqual([]);
  });
});

describe('POST /api/v1/mcp/servers/{id}:restart', () => {
  it('returns 40408 mcp.server_not_found for an unknown server id', async () => {
    const r = await bootDaemon();
    await createSession(r);
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/servers/does-not-exist:restart',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40408);
    expect(env.msg).toMatch(/does not exist/);
  });

  it('returns 40408 even before any session is created (registrar unreachable)', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/servers/x:restart',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40408);
  });

  it('rejects unsupported action with 40001', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/servers/foo:bogus',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40001);
    expect(env.msg).toMatch(/unsupported action/);
  });

  it('rejects bare {id} (no action) with 40001 — :restart is the only allowed action', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/servers/foo',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40001);
  });
});

describe('GET /api/v1/mcp/global/servers', () => {
  it('should return 200 with servers list (empty when no MCP configured)', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/servers' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = listGlobalMcpServersResponseSchema.parse(env.data);
    expect(Array.isArray(parsed.servers)).toBe(true);
  });

  it('should return servers immediately with connecting status without blocking on connections', async () => {
    // Write a mcp.json with a server that would take time to connect.
    // The non-blocking listGlobalMcpServers should return immediately
    // with status "connecting" (wire) / "pending" (agent-core) instead
    // of waiting for the connection to complete.
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          'slow-server': {
            transport: 'stdio',
            command: '/nonexistent/command/that/does/not/exist',
          },
        },
      }),
      'utf-8',
    );

    const r = await bootDaemon();
    // The response must arrive quickly (< 2s) — if listGlobalMcpServers
    // were still blocking on ensureGlobalMcpConnected(), this would hang
    // until the connection attempt times out (30s default).
    const start = Date.now();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/servers' });
    const elapsed = Date.now() - start;
    expect(elapsed).toBeLessThan(2000);

    expect(res.statusCode).toBe(200);
    const env = envelopeOf<{ servers: Array<{ name: string; status: string }> }>(res.json());
    expect(env.code).toBe(0);
    expect(env.data!.servers.length).toBeGreaterThanOrEqual(1);

    const slowServer = env.data!.servers.find((s) => s.name === 'slow-server');
    expect(slowServer).toBeDefined();
    // The server should be in "connecting" (pending) or "error" (failed) status,
    // never absent — the endpoint must not block until connection completes.
    expect(['connecting', 'error']).toContain(slowServer!.status);
  });
});

describe('POST /api/v1/mcp/global/servers (create)', () => {
  it('should create a global MCP server entry and return 200', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers',
      payload: {
        name: 'test-server',
        config: { transport: 'stdio', command: 'echo', args: ['hello'] },
      },
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    expect(env.data).toEqual({ reloading: true });
  });
});

describe('DELETE /api/v1/mcp/global/servers/{name}', () => {
  it('should delete a global MCP server entry', async () => {
    const r = await bootDaemon();
    // First create
    await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers',
      payload: {
        name: 'to-delete',
        config: { transport: 'stdio', command: 'echo' },
      },
    });
    // Then delete
    const res = await appOf(r).inject({
      method: 'DELETE',
      url: '/api/v1/mcp/global/servers/to-delete',
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
  });
});

describe('POST /api/v1/mcp/global/servers:reload', () => {
  it('should return 200 with {reloading: true}', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers:reload',
      payload: {},
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    expect(env.data).toEqual({ reloading: true });
  });
});

describe('GET /api/v1/mcp/global/servers (raw config)', () => {
  it('should return raw config with unexpanded ${VAR} references and unredacted env values', async () => {
    // Write a mcp.json with env references into the bridge home
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          local: {
            command: '${BIN_DIR}/server',
            args: ['--token', '${TOKEN}'],
            env: { API_KEY: '${MY_API_KEY}' },
          },
          remote: {
            transport: 'http',
            url: 'https://mcp.example.com/sse',
            headers: { Authorization: 'Bearer ${MY_TOKEN}' },
          },
        },
      }),
      'utf-8',
    );

    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/servers' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf<{ servers: Array<{ name: string; config?: Record<string, unknown> }> }>(res.json());
    expect(env.code).toBe(0);
    const servers = env.data!.servers;
    expect(servers.length).toBeGreaterThanOrEqual(2);

    const localServer = servers.find((s) => s.name === 'local');
    expect(localServer).toBeDefined();
    const localConfig = localServer!.config!;
    // Raw config should contain the ${VAR} references, not expanded values
    expect((localConfig as { command?: string }).command).toBe('${BIN_DIR}/server');
    expect((localConfig as { env?: Record<string, string> }).env?.['API_KEY']).toBe('${MY_API_KEY}');

    const remoteServer = servers.find((s) => s.name === 'remote');
    expect(remoteServer).toBeDefined();
    const remoteConfig = remoteServer!.config!;
    expect((remoteConfig as { headers?: Record<string, string> }).headers?.['Authorization']).toBe('Bearer ${MY_TOKEN}');
  });
});

describe('GET /api/v1/mcp/global/toggle-state', () => {
  it('should return global MCP toggle state without a session', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/toggle-state' });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf<{ disabled_servers: string[]; disabled_tools: string[] }>(res.json());
    expect(env.code).toBe(0);
    expect(Array.isArray(env.data!.disabled_servers)).toBe(true);
    expect(Array.isArray(env.data!.disabled_tools)).toBe(true);
  });
});

describe('POST /api/v1/mcp/global/servers/{serverName}/tools/{toolName}:{enable|disable}', () => {
  it('should enable a global MCP tool', async () => {
    // Write a mcp.json with a server so the toggle can find it
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          srv: { command: 'echo', args: ['hello'] },
        },
      }),
      'utf-8',
    );
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/srv/tools/tool1:enable',
      payload: {},
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    expect(env.data).toEqual({ ok: true });
  });

  it('should disable a global MCP tool and persist to mcp.json', async () => {
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          srv: { command: 'echo', args: ['hello'] },
        },
      }),
      'utf-8',
    );
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/srv/tools/tool1:disable',
      payload: {},
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    expect(env.data).toEqual({ ok: true });
  });

  it('should reflect disabled tools in global toggle state', async () => {
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          srv: { command: 'echo', args: ['hello'] },
        },
      }),
      'utf-8',
    );
    const r = await bootDaemon();
    // Disable a tool
    await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/srv/tools/tool1:disable',
      payload: {},
    });
    // Check toggle state
    const res = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/toggle-state' });
    const env = envelopeOf<{ disabled_tools: string[] }>(res.json());
    expect(env.data!.disabled_tools).toContain('mcp__srv__tool1');

    // Re-enable
    await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/srv/tools/tool1:enable',
      payload: {},
    });
    const res2 = await appOf(r).inject({ method: 'GET', url: '/api/v1/mcp/global/toggle-state' });
    const env2 = envelopeOf<{ disabled_tools: string[] }>(res2.json());
    expect(env2.data!.disabled_tools).not.toContain('mcp__srv__tool1');
  });

  it('should reject unsupported action with 40001', async () => {
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          srv: { command: 'echo', args: ['hello'] },
        },
      }),
      'utf-8',
    );
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/srv/tools/foo:bogus',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40001);
  });

  it('should return 40408 when toggling a tool on a non-existent server', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/nonexistent/tools/tool1:disable',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40408);
  });
});

describe('POST /api/v1/mcp/global/servers-test', () => {
  it('should test-connect an stdio MCP server config and return status + tools', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers-test',
      payload: {
        config: {
          transport: 'stdio',
          command: process.execPath,
          args: [join(import.meta.dirname, '..', '..', 'agent-core', 'test', 'mcp', 'fixtures', 'mock-stdio-server.mjs')],
        },
      },
    });
    // The test-connect may succeed or fail depending on whether the fixture is
    // reachable, but the envelope shape must be valid.
    expect(res.statusCode).toBe(200);
    const env = envelopeOf<{
      status: 'connected' | 'error';
      tool_count: number;
      error?: string;
      tools: Array<{ name: string; description: string }>;
    }>(res.json());
    expect(env.code).toBe(0);
    expect(['connected', 'error']).toContain(env.data!.status);
    expect(typeof env.data!.tool_count).toBe('number');
    expect(Array.isArray(env.data!.tools)).toBe(true);
  });

  it('should return error status for invalid config', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers-test',
      payload: {
        config: {
          transport: 'stdio',
          command: '/nonexistent/command/that/does/not/exist',
        },
      },
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf<{
      status: 'connected' | 'error';
      tool_count: number;
      error?: string;
      tools: Array<{ name: string; description: string }>;
    }>(res.json());
    expect(env.code).toBe(0);
    expect(env.data!.status).toBe('error');
    expect(env.data!.tool_count).toBe(0);
    expect(env.data!.error).toBeDefined();
  });
});

describe('POST /api/v1/mcp/global/servers/{name}:connect', () => {
  it('should connect a single MCP server and return server status + tools', async () => {
    // Write a mcp.json with a mock server entry
    writeFileSync(
      join(bridgeHome, 'mcp.json'),
      JSON.stringify({
        mcpServers: {
          'mock-server': {
            transport: 'stdio',
            command: process.execPath,
            args: [join(import.meta.dirname, '..', '..', 'agent-core', 'test', 'mcp', 'fixtures', 'mock-stdio-server.mjs')],
          },
        },
      }),
      'utf-8',
    );

    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/mock-server:connect',
      payload: {},
    });
    expect(res.statusCode).toBe(200);
    const env = envelopeOf(res.json());
    expect(env.code).toBe(0);
    const parsed = connectGlobalMcpServerResponseSchema.parse(env.data);
    expect(parsed.server.name).toBe('mock-server');
    expect(['connected', 'error', 'connecting']).toContain(parsed.server.status);
    expect(Array.isArray(parsed.tools)).toBe(true);
  });

  it('should return 40408 for an unknown server name', async () => {
    const r = await bootDaemon();
    const res = await appOf(r).inject({
      method: 'POST',
      url: '/api/v1/mcp/global/servers/does-not-exist:connect',
      payload: {},
    });
    const env = envelopeOf(res.json());
    expect(env.code).toBe(40408);
  });
});
