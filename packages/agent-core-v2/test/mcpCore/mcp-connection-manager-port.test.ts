/**
 * Scenario: B1-L11 — MCP Connection Manager port verification.
 *
 * Validates the four B1-L11 requirements:
 *  1. Global MCP config (`~/.mirri-code/mcp.json`) loads into v2's workspace
 *     MCP config through the config-loader → WorkspaceMcpConfigService chain.
 *  2. `mcp__<server>__<tool>` qualified naming works end-to-end when a real
 *     stdio MCP server connects.
 *  3. Per-server and per-tool enable/disable (including the B0-G3 `auth`
 *     field) filters the tool set correctly.
 *  4. Session-specific MCP overrides are handled at the workspace level
 *     (user < project-root < project < plugin merge), matching the design
 *     decision that workspace-shared connections replace v1's per-session
 *     `mergeCallerMcpServers`.
 *
 * Standalone unit tests — no F4 harness dependency. Run with:
 *   pnpm --filter @mirri-ai/agent-core-v2 exec vitest run \
 *     test/mcpCore/mcp-connection-manager-port.test.ts
 */

import { mkdtempSync } from 'node:fs';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { McpConnectionManager } from '#/mcpCore/connection-manager';
import { McpServerConfigSchema, type McpServerConfig } from '#/mcpCore/config-schema';
import { qualifyMcpToolName } from '#/mcpCore/tool-naming';
import { isMcpToolName } from '#/tool/toolContract';
import { loadMcpServers, resolveMcpJsonPaths } from '#/workspace/workspaceMcpConfig/internal/config-loader';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';

import { stdioFixture } from './stubs';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fs = new HostFileSystem();
const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-b1-l11-'));
  tempDirs.push(dir);
  return dir;
}

async function writeJson(path: string, value: unknown): Promise<void> {
  await mkdir(join(path, '..'), { recursive: true });
  await writeFile(path, JSON.stringify(value), 'utf-8');
}

function stdioConfig(args: string[] = [stdioFixture]): McpServerConfig {
  return { transport: 'stdio', command: process.execPath, args };
}

// ---------------------------------------------------------------------------
// 1. Global MCP config loading
// ---------------------------------------------------------------------------

describe('B1-L11: Global MCP config loading', () => {
  it('should resolve the user mcp.json path under the mirri-code home directory', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    const paths = await resolveMcpJsonPaths({ fs, cwd, homeDir: home });

    expect(paths.user).toBe(join(home, 'mcp.json'));
  });

  it('should load servers from the user-global mcp.json when no project config exists', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        github: { transport: 'stdio', command: 'npx', args: ['-y', '@modelcontextprotocol/server-github'] },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    expect(servers['github']).toEqual({
      transport: 'stdio',
      command: 'npx',
      args: ['-y', '@modelcontextprotocol/server-github'],
    });
  });

  it('should merge user-global config with project-level config (project wins on conflict)', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'app');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    // User-global: has "shared" + "userOnly"
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'global-cmd' },
        userOnly: { transport: 'stdio', command: 'user-cmd' },
      },
    });

    // Project-root: overrides "shared", adds "rootOnly"
    await writeJson(join(repoRoot, '.mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'root-cmd' },
        rootOnly: { transport: 'stdio', command: 'root-only-cmd' },
      },
    });

    // Project-local: overrides "shared" again, adds "localOnly"
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        shared: { transport: 'stdio', command: 'project-cmd' },
        localOnly: { transport: 'http', url: 'https://mcp.example.com' },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    // Project-local overrides project-root which overrides user-global
    // Note: only the project-root .mcp.json gets stdio cwd normalization;
    // the project-local .mirri-code/mcp.json does NOT normalize cwd
    expect(servers['shared']).toEqual({
      transport: 'stdio',
      command: 'project-cmd',
    });
    expect(servers['userOnly']).toEqual({ transport: 'stdio', command: 'user-cmd' });
    expect(servers['rootOnly']).toEqual({
      transport: 'stdio',
      command: 'root-only-cmd',
      cwd: repoRoot,
    });
    expect(servers['localOnly']).toEqual({ transport: 'http', url: 'https://mcp.example.com' });
  });

  it('should skip project-level configs when includeProject is false (untrusted workspace)', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        global: { transport: 'stdio', command: 'global-cmd' },
      },
    });
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        local: { transport: 'stdio', command: 'local-cmd' },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home, includeProject: false });

    expect(Object.keys(servers)).toEqual(['global']);
    expect(servers['global']).toEqual({ transport: 'stdio', command: 'global-cmd' });
  });

  it('should load servers from MIRRICODE_HOME when homeDir is omitted', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        fromEnv: { transport: 'stdio', command: 'env-cmd' },
      },
    });

    const saved = process.env['MIRRICODE_HOME'];
    process.env['MIRRICODE_HOME'] = home;
    try {
      const servers = await loadMcpServers({ fs, cwd });
      expect(servers['fromEnv']).toEqual({ transport: 'stdio', command: 'env-cmd' });
    } finally {
      if (saved === undefined) delete process.env['MIRRICODE_HOME'];
      else process.env['MIRRICODE_HOME'] = saved;
    }
  });

  it('should load an HTTP server with auth: "oauth" from the global config', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        notion: {
          transport: 'http',
          url: 'https://api.notion.com/mcp',
          auth: 'oauth',
        },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    expect(servers['notion']).toEqual({
      transport: 'http',
      url: 'https://api.notion.com/mcp',
      auth: 'oauth',
    });
  });
});

// ---------------------------------------------------------------------------
// 2. mcp__<server>__<tool> naming
// ---------------------------------------------------------------------------

describe('B1-L11: mcp__<server>__<tool> naming', () => {
  it('should qualify tool names as mcp__<server>__<tool> through the naming module', () => {
    expect(qualifyMcpToolName('github', 'list_issues')).toBe('mcp__github__list_issues');
    expect(qualifyMcpToolName('filesystem', 'read_file')).toBe('mcp__filesystem__read_file');
    // Hyphens are valid in MCP names (not sanitized), only non-[a-zA-Z0-9_-] are replaced
    expect(qualifyMcpToolName('my-server', 'search')).toBe('mcp__my-server__search');
  });

  it('should detect qualified MCP tool names', () => {
    expect(isMcpToolName('mcp__github__list_issues')).toBe(true);
    expect(isMcpToolName('mcp__filesystem__read_file')).toBe(true);
    expect(isMcpToolName('Read')).toBe(false);
    expect(isMcpToolName('Bash')).toBe(false);
  });

  it('should expose connected tools with qualified names in the resolved entry', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({ 'mock-server': stdioConfig() });
      const resolved = cm.resolved('mock-server');
      expect(resolved).toBeDefined();
      expect(resolved!.tools.length).toBeGreaterThan(0);

      // Verify every enabled tool name produces a valid mcp__ qualified name
      for (const tool of resolved!.tools) {
        if (resolved!.enabledNames.has(tool.name)) {
          const qualified = qualifyMcpToolName('mock-server', tool.name);
          expect(isMcpToolName(qualified)).toBe(true);
          // The qualified name should contain the sanitized server name and tool name
          // Hyphens are valid in MCP names and preserved through sanitization
          expect(qualified).toContain('mock-server');
          expect(qualified).toContain(tool.name);
        }
      }
    } finally {
      await cm.shutdown();
    }
  }, 20000);

  it('should surface tools from a real stdio MCP server connection', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({ echo: stdioConfig() });
      const entry = cm.get('echo');
      expect(entry?.status).toBe('connected');
      expect(entry?.toolCount).toBe(3); // echo, boom, read_env from the mock server

      const resolved = cm.resolved('echo');
      expect(resolved).toBeDefined();
      const toolNames = [...(resolved?.enabledNames ?? [])];
      expect(toolNames).toContain('echo');
      expect(toolNames).toContain('boom');
      expect(toolNames).toContain('read_env');

      // Verify qualified naming for each tool
      expect(qualifyMcpToolName('echo', 'echo')).toBe('mcp__echo__echo');
      expect(qualifyMcpToolName('echo', 'boom')).toBe('mcp__echo__boom');
      expect(qualifyMcpToolName('echo', 'read_env')).toBe('mcp__echo__read_env');
    } finally {
      await cm.shutdown();
    }
  }, 20000);

  it('should call a tool on a connected stdio server and get a result', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({ echo: stdioConfig() });
      const resolved = cm.resolved('echo');
      expect(resolved).toBeDefined();

      const result = await resolved!.client.callTool('echo', { text: 'hello world' });
      expect(result.isError).toBe(false);
      const text = (result.content[0] as { type: 'text'; text: string }).text;
      expect(text).toBe('hello world');
    } finally {
      await cm.shutdown();
    }
  }, 20000);
});

// ---------------------------------------------------------------------------
// 3. Per-server / per-tool enable/disable
// ---------------------------------------------------------------------------

describe('B1-L11: Per-server and per-tool enable/disable', () => {
  it('should mark a server as disabled when config.enabled is false', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({ off: { ...stdioConfig(), enabled: false } });
      const entry = cm.get('off');
      expect(entry?.status).toBe('disabled');
      expect(entry?.toolCount).toBe(0);

      // Resolved should be undefined for disabled servers
      expect(cm.resolved('off')).toBeUndefined();
    } finally {
      await cm.shutdown();
    }
  });

  it('should filter tools using enabledTools allowlist', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({
        filtered: { ...stdioConfig(), enabledTools: ['echo'] },
      });
      const resolved = cm.resolved('filtered');
      expect(resolved).toBeDefined();
      expect([...resolved!.enabledNames]).toEqual(['echo']);
      expect(cm.get('filtered')?.toolCount).toBe(1);
    } finally {
      await cm.shutdown();
    }
  }, 15000);

  it('should filter tools using disabledTools denylist', async () => {
    const cm = new McpConnectionManager();
    try {
      await cm.connectAll({
        filtered: { ...stdioConfig(), disabledTools: ['boom', 'read_env'] },
      });
      const resolved = cm.resolved('filtered');
      expect(resolved).toBeDefined();
      expect([...resolved!.enabledNames]).toEqual(['echo']);
      expect(cm.get('filtered')?.toolCount).toBe(1);
    } finally {
      await cm.shutdown();
    }
  }, 15000);

  it('should apply both enabledTools and disabledTools (intersection)', async () => {
    const cm = new McpConnectionManager();
    try {
      // enabledTools allows echo + boom, disabledTools removes boom
      await cm.connectAll({
        filtered: { ...stdioConfig(), enabledTools: ['echo', 'boom'], disabledTools: ['boom'] },
      });
      const resolved = cm.resolved('filtered');
      expect(resolved).toBeDefined();
      expect([...resolved!.enabledNames]).toEqual(['echo']);
    } finally {
      await cm.shutdown();
    }
  }, 15000);

  it('should accept auth: "oauth" in config schema and preserve it in loaded config', () => {
    const config = {
      transport: 'http' as const,
      url: 'https://api.example.com/mcp',
      auth: 'oauth' as const,
    };
    const result = McpServerConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.auth).toBe('oauth');
    }
  });

  it('should accept auth: "oauth" on stdio config', () => {
    const config = {
      transport: 'stdio' as const,
      command: 'npx',
      auth: 'oauth' as const,
    };
    const result = McpServerConfigSchema.safeParse(config);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.auth).toBe('oauth');
    }
  });

  it('should load MCP config with auth: "oauth" from a file', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        notion: {
          transport: 'http',
          url: 'https://api.notion.com/mcp',
          auth: 'oauth',
          enabledTools: ['search', 'create_page'],
        },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
    expect(servers['notion']).toEqual({
      transport: 'http',
      url: 'https://api.notion.com/mcp',
      auth: 'oauth',
      enabledTools: ['search', 'create_page'],
    });
  });

  it('should load a disabled server with auth and tool filters from config', async () => {
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        expensive: {
          transport: 'http',
          url: 'https://expensive.example.com/mcp',
          auth: 'oauth',
          enabled: false,
          enabledTools: ['search'],
        },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });
    expect(servers['expensive']).toEqual({
      transport: 'http',
      url: 'https://expensive.example.com/mcp',
      auth: 'oauth',
      enabled: false,
      enabledTools: ['search'],
    });
  });
});

// ---------------------------------------------------------------------------
// 4. Session-specific MCP overrides (workspace-level merge)
// ---------------------------------------------------------------------------

describe('B1-L11: Workspace-level MCP config merge (replaces v1 mergeCallerMcpServers)', () => {
  it('should merge user-global, project-root, and project-local configs with correct precedence', async () => {
    const home = makeTempDir();
    const repoRoot = makeTempDir();
    const cwd = join(repoRoot, 'packages', 'app');
    await mkdir(join(repoRoot, '.git'), { recursive: true });
    await mkdir(cwd, { recursive: true });

    // User-global: base server definition
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        github: { transport: 'stdio', command: 'global-github-cmd' },
        filesystem: { transport: 'stdio', command: 'global-fs-cmd' },
      },
    });

    // Project-root: adds a server, overrides one
    await writeJson(join(repoRoot, '.mcp.json'), {
      mcpServers: {
        github: { transport: 'stdio', command: 'project-github-cmd', enabledTools: ['search'] },
        custom: { transport: 'stdio', command: 'project-custom-cmd' },
      },
    });

    // Project-local: further overrides
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        custom: { transport: 'http', url: 'https://custom.example.com/mcp', auth: 'oauth' },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    // Precedence: user < project-root < project-local
    expect(servers['github']).toEqual({
      transport: 'stdio',
      command: 'project-github-cmd',
      enabledTools: ['search'],
      cwd: repoRoot,
    });
    expect(servers['filesystem']).toEqual({ transport: 'stdio', command: 'global-fs-cmd' });
    expect(servers['custom']).toEqual({
      transport: 'http',
      url: 'https://custom.example.com/mcp',
      auth: 'oauth',
    });
  });

  it('should treat the workspace-level merge as the equivalent of v1 mergeCallerMcpServers', async () => {
    // In v1, mergeCallerMcpServers merges caller-provided servers on top of
    // the file-based config. In v2, the equivalent is the workspace-level
    // config chain: user < project-root < project, plus plugin contributions
    // (with file config winning on name collision). This test verifies that
    // adding a server at a higher-precedence level overrides the lower one.

    const home = makeTempDir();
    const cwd = makeTempDir();

    // Base: user-global config
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        search: { transport: 'http', url: 'https://search.example.com/mcp' },
      },
    });

    // Override: project-local config (analogous to caller override)
    await writeJson(join(cwd, '.mirri-code', 'mcp.json'), {
      mcpServers: {
        search: { transport: 'http', url: 'https://search-staging.example.com/mcp', auth: 'oauth' },
        extra: { transport: 'stdio', command: 'extra-cmd' },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    // Project-local "search" overrides the user-global "search"
    expect(servers['search']).toEqual({
      transport: 'http',
      url: 'https://search-staging.example.com/mcp',
      auth: 'oauth',
    });
    // Extra server from project-local is present
    expect(servers['extra']).toEqual({ transport: 'stdio', command: 'extra-cmd' });
  });

  it('should provide all config layers to the connection manager in a single merged snapshot', async () => {
    // This simulates what WorkspaceMcpConfigService does: merges all sources
    // and feeds the result to McpConnectionManager.connectAll()
    const home = makeTempDir();
    const cwd = makeTempDir();

    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        github: { transport: 'stdio', command: 'npx', args: ['-y', '@modelcontextprotocol/server-github'] },
      },
    });

    const servers = await loadMcpServers({ fs, cwd, homeDir: home });

    // Feed the merged config into a connection manager
    const cm = new McpConnectionManager({ stdioCwd: cwd });
    try {
      // The github server won't actually connect (npx may not be available in CI),
      // but the connection manager should accept the config and create entries
      await cm.connectAll(servers);
      const entry = cm.get('github');
      // It may be 'connected' or 'failed' depending on npx availability,
      // but it must exist (not undefined)
      expect(entry).toBeDefined();
      expect(entry!.name).toBe('github');
      expect(['connected', 'failed', 'pending']).toContain(entry!.status);
    } finally {
      await cm.shutdown();
    }
  }, 30000);
});
