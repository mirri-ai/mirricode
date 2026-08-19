/**
 * Scenario: MCP config discovery, precedence, normalization, and validation.
 *
 * Exercises the real loader against temporary JSON files. Run with `pnpm
 * --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/workspace/workspaceMcpConfig/config-loader.test.ts`.
 */

import { mkdtempSync } from 'node:fs';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';
import { afterEach, describe, expect, it } from 'vitest';

import { ErrorCodes, Error2 } from '#/errors';
import { loadMcpServers, loadSourceMcpServers, resolveMcpJsonPaths } from '#/workspace/workspaceMcpConfig/internal/config-loader';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';

const fs = new HostFileSystem();

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-mcp-loader-'));
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

    const paths = await resolveMcpJsonPaths({ fs, cwd, homeDir: '/home/user/.mirri-code' });

    expect(paths.user).toBe('/home/user/.mirri-code/mcp.json');
    expect(paths.projectRoot).toBe(join(repoRoot, '.mcp.json'));
    expect(paths.project).toBe(join(cwd, '.mirri-code', 'mcp.json'));
  });
});

describe('loadMcpServers', () => {
  it('returns an empty map when no files exist', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
    expect(servers).toEqual({});
  });

  it('treats empty JSON files as empty maps', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeFile(join(home, 'mcp.json'), '   \n');
    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
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

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

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

  it('loads only the user file when includeProject is false (untrusted workspace)', async () => {
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

    const servers = await loadMcpServers({ fs, cwd, homeDir: home, includeProject: false });

    expect(Object.keys(servers).toSorted()).toEqual(['shared', 'userOnly']);
    expect(servers['shared']).toEqual({
      transport: 'stdio',
      command: 'shared-user',
    });
  });

  it('loads root .mcp.json from the repo root and lets project-local override it', async () => {
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

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

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

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

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

  it('throws Error2(config.invalid) on invalid JSON', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeFile(join(home, 'mcp.json'), '{not json}', 'utf-8');
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).rejects.toBeInstanceOf(Error2);
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('throws Error2(config.invalid) on schema violation with unknown transport', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { bad: { transport: 'websocket', url: 'https://x.example.com' } },
    });
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('throws Error2(config.invalid) on schema violation with missing required field', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { bad: { transport: 'stdio' } },
    });
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('throws Error2(config.invalid) when an MCP timeout exceeds the Node.js timer limit', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        bad: {
          transport: 'stdio',
          command: 'node',
          startupTimeoutMs: 2_147_483_648,
          toolTimeoutMs: 2_147_483_648,
        },
      },
    });
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('loads MCP timeouts at the Node.js timer upper boundary', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        boundary: {
          transport: 'stdio',
          command: 'node',
          startupTimeoutMs: 2_147_483_647,
          toolTimeoutMs: 2_147_483_647,
        },
      },
    });
    await expect(loadMcpServers({ fs, cwd, homeDir: home })).resolves.toEqual({
      boundary: {
        transport: 'stdio',
        command: 'node',
        startupTimeoutMs: 2_147_483_647,
        toolTimeoutMs: 2_147_483_647,
      },
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
    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
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
    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
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
          headers: { 'X-Tenant': 'mirri' },
          bearerTokenEnvVar: 'LEGACY_MCP_TOKEN',
        },
      },
    });
    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
    expect(servers['legacy']).toEqual({
      transport: 'sse',
      url: 'https://mcp.example.com/sse',
      headers: { 'X-Tenant': 'mirri' },
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
      const servers = await loadMcpServers({ fs, cwd });
      expect(servers['from_env']).toEqual({ transport: 'stdio', command: 'env-cmd' });
    } finally {
      if (saved === undefined) delete process.env['MIRRICODE_HOME'];
      else process.env['MIRRICODE_HOME'] = saved;
    }
  });

  it('should expand ${VAR} and ${env:VAR} references in env and command values via the injected env lookup', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        gh: {
          command: 'npx',
          args: ['-y', '@modelcontextprotocol/server-github'],
          env: { TOKEN: '${GH_TOKEN}', PREFIXED: 'sk-${env:GH_TOKEN}' },
        },
      },
    });
    const lookup = (name: string): string | undefined =>
      name === 'GH_TOKEN' ? 'resolved-secret' : undefined;
    const servers = await loadMcpServers({ fs, cwd, homeDir: home, envLookup: lookup });
    expect(servers['gh']).toEqual({
      transport: 'stdio',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
      env: { TOKEN: 'resolved-secret', PREFIXED: 'sk-resolved-secret' },
    });
  });

  it('should replace undefined variables with an empty string', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        unset: { command: 'echo', env: { MISSING: '${NOT_SET_ANYWHERE_123}' } },
      },
    });
    const servers = await loadMcpServers({ fs, cwd, homeDir: home, envLookup: () => undefined });
    expect(servers['unset']).toEqual({
      transport: 'stdio',
      command: 'echo',
      env: { MISSING: '' },
    });
  });

  it('should leave object keys, numbers and booleans untouched', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        odd: {
          command: 'echo',
          startupTimeoutMs: 1000,
          enabled: true,
          env: { 'LITERAL_${NOT_VAR}': 'value' },
        },
      },
    });
    const servers = await loadMcpServers({ fs, cwd, homeDir: home, envLookup: () => 'replaced' });
    expect(servers['odd']).toEqual({
      transport: 'stdio',
      command: 'echo',
      startupTimeoutMs: 1000,
      enabled: true,
      env: { 'LITERAL_${NOT_VAR}': 'value' },
    });
  });

  it('should expand references inside nested arrays and nested objects', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        remote: {
          url: '${BASE_URL}',
          headers: { Authorization: 'Bearer ${env:TOKEN}' },
        },
      },
    });
    const lookup = (name: string): string | undefined =>
      name === 'BASE_URL' ? 'https://mcp.example.com/mcp' : name === 'TOKEN' ? 'tok' : undefined;
    const servers = await loadMcpServers({ fs, cwd, homeDir: home, envLookup: lookup });
    expect(servers['remote']).toEqual({
      transport: 'http',
      url: 'https://mcp.example.com/mcp',
      headers: { Authorization: 'Bearer tok' },
    });
  });

  it('should reject expanded config whose values violate the schema', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        broken: { url: '${UNSET_URL_VAR_123}' },
      },
    });
    await expect(
      loadMcpServers({ fs, cwd, homeDir: home, envLookup: () => undefined }),
    ).rejects.toMatchObject({
      code: ErrorCodes.CONFIG_INVALID,
    });
  });

  it('should return unexpanded config values when reading the source view', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        gh: { command: 'npx', env: { TOKEN: '${GH_TOKEN}' } },
      },
    });
    const servers = await loadSourceMcpServers({
      fs,
      cwd,
      homeDir: home,
      envLookup: () => 'should-not-apply',
    });
    expect(servers['gh']).toEqual({
      transport: 'stdio',
      command: 'npx',
      env: { TOKEN: '${GH_TOKEN}' },
    });
  });
});
