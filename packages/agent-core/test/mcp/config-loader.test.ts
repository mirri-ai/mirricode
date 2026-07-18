import { mkdtempSync } from 'node:fs';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';

import { afterEach, describe, expect, it } from 'vitest';

import { ErrorCodes, MirriError } from '../../src/errors';
import { loadMcpServers, resolveMcpJsonPaths } from '../../src/mcp/config-loader';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-'));
  tempDirs.push(dir);
  return dir;
}

async function writeJson(path: string, value: unknown): Promise<void> {
  await mkdir(join(path, '..'), { recursive: true });
  await writeFile(path, JSON.stringify(value), 'utf-8');
}

describe('resolveMcpJsonPaths', () => {
  it('returns the canonical user, project-root, and project-local paths', async () => {
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'agent-core');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    const paths = await resolveMcpJsonPaths({ cwd, homeDir: '/home/user/.mirricode-code' });

    expect(paths.user).toBe('/home/user/.mirricode-code/mcp.json');
    expect(paths.projectRoot).toBe(join(repoRoot, '.mcp.json'));
    expect(paths.project).toBe(join(cwd, '.mirri-code', 'mcp.json'));
  });
});

describe('loadMcpServers', () => {
  it('returns an empty map when no files exist', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    const servers = await loadMcpServers({ cwd, homeDir: home });
    expect(servers).toEqual({});
  });

  it('treats empty JSON files as empty maps', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeFile(join(home, 'mcp.json'), '   \n');
    const servers = await loadMcpServers({ cwd, homeDir: home });
    expect(servers).toEqual({});
  });

  it('merges project-local mcp.json with user-global, project overriding on conflict', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'shared-user' },
        userOnly: { transport: 'stdio', command: 'user-only' },
      },
    });
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'shared-project' },
        local: { transport: 'http', url: 'http://localhost:8080/mcp' },
      },
    });

    const servers = await loadMcpServers({ cwd, homeDir: home });

    expect(Object.keys(servers).toSorted()).toEqual(['local', 'shared', 'userOnly']);
    expect(servers['shared']).toEqual({
      transport: 'stdio',
      command: 'shared-project',
    });
    expect(servers['userOnly']).toEqual({
      transport: 'stdio',
      command: 'user-only',
    });
    expect(servers['local']).toEqual({
      transport: 'http',
      url: 'http://localhost:8080/mcp',
    });
  });

  it('loads root .mcp.json from the repo root and lets project-local .mirri-code/mcp.json override it', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'agent-core');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'shared-user' },
        userOnly: { transport: 'stdio', command: 'user-only' },
      },
    });
    await writeJson(join(repoRoot, '.mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'shared-root' },
        rootOnly: { command: 'root-only' },
      },
    });
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'shared-project' },
        projectOnly: { transport: 'http', url: 'https://mcp.example.com' },
      },
    });

    const servers = await loadMcpServers({ cwd, homeDir: home });

    expect(Object.keys(servers).toSorted()).toEqual([
      'projectOnly',
      'rootOnly',
      'shared',
      'userOnly',
    ]);
    expect(servers['shared']).toEqual({
      transport: 'stdio',
      command: 'shared-project',
    });
    expect(servers['rootOnly']).toEqual({ transport: 'stdio', command: 'root-only', cwd: repoRoot });
    expect(servers['userOnly']).toEqual({ transport: 'stdio', command: 'user-only' });
    expect(servers['projectOnly']).toEqual({ transport: 'http', url: 'https://mcp.example.com' });
  });

  it('resolves project-root stdio cwd relative to the root .mcp.json directory', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'agent-core');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    await writeJson(join(repoRoot, '.mcp.json'), {
      mcpServers: {
        implicitRoot: { command: './bin/mcp-server' },
        explicitDot: { command: './bin/mcp-server', cwd: '.' },
        nested: { command: 'node', cwd: 'tools/mcp' },
        absolute: { command: 'node', cwd: '/tmp/mcp-workdir' },
        remote: { url: 'https://mcp.example.com' },
      },
    });

    const servers = await loadMcpServers({ cwd, homeDir: home });

    expect(servers['implicitRoot']).toEqual({
      transport: 'stdio',
      command: './bin/mcp-server',
      cwd: repoRoot,
    });
    expect(servers['explicitDot']).toEqual({
      transport: 'stdio',
      command: './bin/mcp-server',
      cwd: repoRoot,
    });
    expect(servers['nested']).toEqual({
      transport: 'stdio',
      command: 'node',
      cwd: join(repoRoot, 'tools', 'mcp'),
    });
    expect(servers['absolute']).toEqual({
      transport: 'stdio',
      command: 'node',
      cwd: '/tmp/mcp-workdir',
    });
    expect(servers['remote']).toEqual({
      transport: 'http',
      url: 'https://mcp.example.com',
    });
  });

  it('throws MirriError(config.invalid) on invalid JSON', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeFile(join(home, 'mcp.json'), '{not json}', 'utf-8');
    await expect(loadMcpServers({ cwd, homeDir: home })).rejects.toBeInstanceOf(MirriError);
    await expect(loadMcpServers({ cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('throws MirriError(config.invalid) on schema violation (unknown transport)', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { bad: { transport: 'websocket', url: 'https://x' } },
    });
    await expect(loadMcpServers({ cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('throws MirriError(config.invalid) on schema violation (missing required field)', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { bad: { transport: 'stdio' } },
    });
    await expect(loadMcpServers({ cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('infers transport=stdio when an entry omits transport but has command', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        gh: { command: 'npx', args: ['-y', '@modelcontextprotocol/server-github'] },
      },
    });
    const servers = await loadMcpServers({ cwd, homeDir: home });
    expect(servers['gh']).toEqual({
      transport: 'stdio',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
    });
  });

  it('infers transport=http when an entry omits transport but has url', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        remote: { url: 'https://mcp.example.com/sse' },
      },
    });
    const servers = await loadMcpServers({ cwd, homeDir: home });
    expect(servers['remote']).toEqual({
      transport: 'http',
      url: 'https://mcp.example.com/sse',
    });
  });

  it('loads explicit SSE server config', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        legacy: {
          transport: 'sse',
          url: 'https://mcp.example.com/sse',
          headers: { 'X-Tenant': 'kimi' },
          bearerTokenEnvVar: 'LEGACY_MCP_TOKEN',
        },
      },
    });
    const servers = await loadMcpServers({ cwd, homeDir: home });
    expect(servers['legacy']).toEqual({
      transport: 'sse',
      url: 'https://mcp.example.com/sse',
      headers: { 'X-Tenant': 'kimi' },
      bearerTokenEnvVar: 'LEGACY_MCP_TOKEN',
    });
  });

  it('honors MIRRICODE_HOME env var when homeDir is not supplied', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { from_env: { transport: 'stdio', command: 'env-cmd' } },
    });
    const saved = process.env['MIRRICODE_HOME'];
    process.env['MIRRICODE_HOME'] = home;
    try {
      const servers = await loadMcpServers({ cwd });
      expect(servers['from_env']).toEqual({ transport: 'stdio', command: 'env-cmd' });
    } finally {
      if (saved === undefined) delete process.env['MIRRICODE_HOME'];
      else process.env['MIRRICODE_HOME'] = saved;
    }
  });
});

describe('loadMcpServers: environment-variable expansion', () => {
  const env = (vars: Record<string, string>) => (name: string): string | undefined => vars[name];

  it('should expand ${VAR} and ${env:VAR} in stdio command/args/env', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        local: {
          command: '${env:BIN_DIR}/server',
          args: ['--token', '${TOKEN}'],
          env: { REGION: '${env:REGION}' },
        },
      },
    });

    const servers = await loadMcpServers({
      cwd,
      homeDir: home,
      envLookup: env({ BIN_DIR: '/opt/bin', TOKEN: 'secret', REGION: 'us-east-1' }),
    });

    expect(servers['local']).toEqual({
      transport: 'stdio',
      command: '/opt/bin/server',
      args: ['--token', 'secret'],
      env: { REGION: 'us-east-1' },
    });
  });

  it('should expand ${VAR} in http url and headers', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        remote: {
          transport: 'http',
          url: 'https://${HOST}:${PORT}/mcp',
          headers: { Authorization: 'Bearer ${env:TOKEN}' },
        },
      },
    });

    const servers = await loadMcpServers({
      cwd,
      homeDir: home,
      envLookup: env({ HOST: 'mcp.example.com', PORT: '8443', TOKEN: 'abc123' }),
    });

    expect(servers['remote']).toEqual({
      transport: 'http',
      url: 'https://mcp.example.com:8443/mcp',
      headers: { Authorization: 'Bearer abc123' },
    });
  });

  it('should resolve undefined variables to empty strings for fields that tolerate them', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'agent-core');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    // http url with an empty host (`https:///mcp`) is accepted by the URL
    // constructor, so it survives; stdio args accept empty strings too.
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { user: { transport: 'http', url: 'https://${UNSET_HOST}/mcp' } },
    });
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: { proj: { command: 'node', args: ['${UNSET_ARG}'] } },
    });

    const servers = await loadMcpServers({ cwd, homeDir: home, envLookup: env({}) });

    expect(servers['user']).toEqual({ transport: 'http', url: 'https:///mcp' });
    expect(servers['proj']).toEqual({ transport: 'stdio', command: 'node', args: [''] });
  });

  it('should let zod reject an empty stdio command produced by an unset variable', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    // `${env:UNSET_CMD}` → "" → command violates z.string().min(1).
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { root: { command: '${env:UNSET_CMD}' } },
    });

    await expect(
      loadMcpServers({ cwd, homeDir: home, envLookup: env({}) }),
    ).rejects.toMatchObject({ code: ErrorCodes.CONFIG_INVALID });
  });

  it('should let zod reject an expansion that produces an invalid http url', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    // `${SCHEME}` unset → "" → url becomes "://example.com/mcp" with no scheme,
    // which `new URL(...)` rejects, so zod's `.url()` validation fails.
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { bad: { transport: 'http', url: '${SCHEME}://example.com/mcp' } },
    });

    await expect(
      loadMcpServers({ cwd, homeDir: home, envLookup: env({}) }),
    ).rejects.toMatchObject({ code: ErrorCodes.CONFIG_INVALID });
  });

  it('should expand variables merged across all three files with override semantics', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'agent-core');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: '${USER_CMD}' },
        userOnly: { transport: 'http', url: 'https://${HOST}/user' },
      },
    });
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: '${env:PROJ_CMD}' },
        projOnly: { transport: 'http', url: 'https://${HOST}/proj' },
      },
    });

    const servers = await loadMcpServers({
      cwd,
      homeDir: home,
      envLookup: env({ USER_CMD: 'user-bin', PROJ_CMD: 'proj-bin', HOST: 'example.com' }),
    });

    expect(servers['shared']).toEqual({ transport: 'stdio', command: 'proj-bin' });
    expect(servers['userOnly']).toEqual({ transport: 'http', url: 'https://example.com/user' });
    expect(servers['projOnly']).toEqual({ transport: 'http', url: 'https://example.com/proj' });
  });
});
