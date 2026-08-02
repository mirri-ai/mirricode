/**
 * B3-L8: Plugin System verification tests.
 *
 * Covers the key v2 plugin scenarios at the PluginService and PluginManager
 * level: GitHub install, MCP server surface, command registration, session-start
 * injection rendering, agent profile roots, and marketplace context rejection.
 *
 * Run: pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/plugin/pluginSystem.test.ts
 */

import { execFileSync } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { MIRRICODE_PROVIDER_NAME } from '@mirri-ai/v2-oauth';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  LifecycleScope,
  ScopeActivation,
  _clearScopedRegistryForTests,
  registerScopedService,
} from '#/_base/di/scope';
import { createScopedTestHost, stubPair, type ScopedTestHost } from '#/_base/di/test';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { PluginManager } from '#/app/plugin/manager';
import { IPluginService } from '#/app/plugin/plugin';
import { PluginService } from '#/app/plugin/pluginService';
import { IProviderService } from '#/kosong/provider/provider';
import { ISkillDiscovery } from '#/app/skillCatalog/skillDiscovery';

import { stubBootstrap } from '../bootstrap/stubs';
import { stubProviderService } from '../provider/stubs';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const createdDirs: string[] = [];

async function makeHome(): Promise<string> {
  const home = await mkdtemp(path.join(tmpdir(), 'mirri-home-'));
  createdDirs.push(home);
  return home;
}

async function makePluginDir(
  name: string,
  manifest: Record<string, unknown>,
  options: {
    skills?: readonly string[];
    commands?: Record<string, string>;
    agents?: boolean;
  } = {},
): Promise<string> {
  const root = await mkdtemp(path.join(tmpdir(), `plugin-${name}-`));
  createdDirs.push(root);

  const fullManifest: Record<string, unknown> = { name, ...manifest };

  if (options.skills !== undefined && options.skills.length > 0) {
    fullManifest['skills'] = './skills/';
    await mkdir(path.join(root, 'skills'), { recursive: true });
    for (const skillName of options.skills) {
      await mkdir(path.join(root, 'skills', skillName), { recursive: true });
      await writeFile(
        path.join(root, 'skills', skillName, 'SKILL.md'),
        `---\nname: ${skillName}\ndescription: A demo skill\n---\nbody of ${skillName}`,
        'utf8',
      );
    }
  }

  if (options.commands !== undefined && Object.keys(options.commands).length > 0) {
    fullManifest['commands'] = ['./commands'];
    await mkdir(path.join(root, 'commands'), { recursive: true });
    for (const [file, body] of Object.entries(options.commands)) {
      const filePath = path.join(root, 'commands', file);
      await mkdir(path.dirname(filePath), { recursive: true });
      await writeFile(filePath, body, 'utf8');
    }
  }

  if (options.agents === true) {
    fullManifest['agents'] = './agents/';
    await mkdir(path.join(root, 'agents'), { recursive: true });
    await writeFile(
      path.join(root, 'agents', 'demo-agent.md'),
      '---\nname: demo-agent\ndescription: A demo agent profile\n---\nsystem prompt for demo agent',
      'utf8',
    );
  }

  await writeFile(path.join(root, 'mirri.plugin.json'), JSON.stringify(fullManifest), 'utf8');
  return realpath(root);
}

async function makeValidInstalledFile(homeDir: string): Promise<void> {
  await mkdir(path.join(homeDir, 'plugins'), { recursive: true });
  await writeFile(
    path.join(homeDir, 'plugins', 'installed.json'),
    JSON.stringify({ version: 1, plugins: [] }),
    'utf8',
  );
}

function stubSkillDiscovery(): ISkillDiscovery {
  return {
    _serviceBrand: undefined,
    discover: async () => ({ skills: [], skipped: [], scannedRoots: [] }),
  };
}

function makeHost(
  homeDir: string,
  providers = stubProviderService(),
  env: NodeJS.ProcessEnv = {},
): ScopedTestHost {
  return createScopedTestHost([
    stubPair(IBootstrapService, stubBootstrap(homeDir, env)),
    stubPair(IProviderService, providers),
    stubPair(ISkillDiscovery, stubSkillDiscovery()),
  ]);
}

async function zipDir(sourceRoot: string): Promise<Buffer> {
  const zipPath = path.join(
    tmpdir(),
    `plugin-${Date.now()}-${Math.random().toString(36).slice(2)}.zip`,
  );
  execFileSync('zip', ['-qr', zipPath, '.'], { cwd: sourceRoot });
  const buffer = await readFile(zipPath);
  await rm(zipPath, { force: true });
  return buffer;
}

async function serveOnce(buffer: Buffer): Promise<string> {
  const server = createServer((_, res) => {
    res.writeHead(200, { 'Content-Type': 'application/zip' });
    res.end(buffer);
    server.close();
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  if (address === null || typeof address === 'string') throw new Error('bad server address');
  return `http://127.0.0.1:${address.port}/plugin.zip`;
}

interface MockGithubFetchOptions {
  releaseTag?: string;
  tarball: Buffer;
}

function mockGithubFetch(options: MockGithubFetchOptions): void {
  const commitSha = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
      const url =
        typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
      if (/^https:\/\/github\.com\/[^/]+\/[^/]+\/releases\/latest$/.test(url)) {
        if (options.releaseTag === undefined) {
          return new Response(null, { status: 404 });
        }
        const tagUrl = url.replace(/\/releases\/latest$/, `/releases/tag/${options.releaseTag}`);
        return new Response(null, { status: 302, headers: { location: tagUrl } });
      }
      if (/^https:\/\/github\.com\/[^/]+\/[^/]+\/commits\/.+\.atom$/.test(url)) {
        return new Response(
          `<entry><id>tag:github.com,2008:Grit::Commit/${commitSha}</id></entry>`,
        );
      }
      if (url.startsWith('https://codeload.github.com/')) {
        if (init?.method === 'HEAD') return new Response(null, { status: 200 });
        return new Response(options.tarball, { status: 200 });
      }
      throw new Error(`mockGithubFetch: unexpected url ${url}`);
    }) as typeof fetch,
  );
}

// ---------------------------------------------------------------------------
// Test suites
// ---------------------------------------------------------------------------

describe('B3-L8 Plugin System', () => {
  beforeEach(() => {
    _clearScopedRegistryForTests();
    registerScopedService(
      LifecycleScope.App,
      IPluginService,
      PluginService,
      ScopeActivation.OnDemand,
      'plugin',
    );
  });

  afterEach(async () => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
    while (createdDirs.length > 0) {
      const dir = createdDirs.pop();
      if (dir !== undefined) await rm(dir, { recursive: true, force: true });
    }
  });

  // -----------------------------------------------------------------------
  // 1. Plugin install from GitHub
  // -----------------------------------------------------------------------

  describe('plugin install from GitHub', () => {
    it('should install a plugin from a GitHub URL with a release tag via PluginService', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const sourceRoot = await makePluginDir('gh-plugin', { version: '2.0.0' });
      const zipBuffer = await zipDir(sourceRoot);
      mockGithubFetch({ releaseTag: 'v2.0.0', tarball: zipBuffer });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        const summary = await svc.installPlugin({ source: 'https://github.com/example/gh-plugin' });

        expect(summary.id).toBe('gh-plugin');
        expect(summary.source).toBe('github');
        expect(summary.version).toBe('2.0.0');
        expect(summary.github?.ref).toEqual({ kind: 'tag', value: 'v2.0.0' });
        expect(summary.enabled).toBe(true);

        const plugins = await svc.listPlugins();
        expect(plugins).toEqual([expect.objectContaining({ id: 'gh-plugin' })]);
      } finally {
        host.dispose();
      }
    });

    it('should install a plugin from a GitHub /tree/branch URL via PluginService', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const sourceRoot = await makePluginDir('branch-plugin', { version: '3.0.0' });
      const zipBuffer = await zipDir(sourceRoot);
      mockGithubFetch({ tarball: zipBuffer });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        const summary = await svc.installPlugin({
          source: 'https://github.com/example/branch-plugin/tree/main',
        });

        expect(summary.id).toBe('branch-plugin');
        expect(summary.source).toBe('github');
        expect(summary.github?.ref).toEqual({ kind: 'branch', value: 'main' });
      } finally {
        host.dispose();
      }
    });

    it('should install a plugin from a zip URL via PluginService', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const sourceRoot = await makePluginDir('zip-plugin', { version: '1.0.0' });
      const zipBuffer = await zipDir(sourceRoot);
      const url = await serveOnce(zipBuffer);

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        const summary = await svc.installPlugin({ source: url });

        expect(summary.id).toBe('zip-plugin');
        expect(summary.source).toBe('zip-url');
        expect(summary.version).toBe('1.0.0');
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 2. Plugin MCP server surfaces
  // -----------------------------------------------------------------------

  describe('plugin MCP server surfaces', () => {
    it('should surface enabled plugin MCP servers through enabledMcpServers()', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('mcp-demo', {
        mcpServers: {
          search: { command: 'search-mcp', args: ['--port', '3000'] },
          docs: { url: 'https://example.com/mcp' },
        },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const servers = await svc.enabledMcpServers();
        // stdio server should get managed env + cwd
        expect(servers['plugin-mcp-demo:search']).toEqual(
          expect.objectContaining({
            command: 'search-mcp',
            args: ['--port', '3000'],
            env: expect.objectContaining({
              MIRRICODE_HOME: home,
              MIRRI_PLUGIN_ROOT: expect.any(String),
            }),
          }),
        );
        // http server should pass through
        expect(servers['plugin-mcp-demo:docs']).toEqual(
          expect.objectContaining({ url: 'https://example.com/mcp' }),
        );
      } finally {
        host.dispose();
      }
    });

    it('should exclude MCP servers from disabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('disabled-mcp', {
        mcpServers: { tool: { command: 'tool-mcp' } },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });
        await svc.setPluginEnabled({ id: 'disabled-mcp', enabled: false });

        const servers = await svc.enabledMcpServers();
        expect(Object.keys(servers)).toHaveLength(0);
      } finally {
        host.dispose();
      }
    });

    it('should toggle individual MCP server on/off via setPluginMcpServerEnabled', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('toggle-mcp', {
        mcpServers: {
          primary: { command: 'primary-mcp' },
          secondary: { command: 'secondary-mcp' },
        },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        let servers = await svc.enabledMcpServers();
        expect(servers['plugin-toggle-mcp:primary']).toBeDefined();
        expect(servers['plugin-toggle-mcp:secondary']).toBeDefined();

        await svc.setPluginMcpServerEnabled({ id: 'toggle-mcp', server: 'secondary', enabled: false });
        servers = await svc.enabledMcpServers();
        expect(servers['plugin-toggle-mcp:primary']).toBeDefined();
        expect(servers['plugin-toggle-mcp:secondary']).toBeUndefined();
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 3. Plugin commands register
  // -----------------------------------------------------------------------

  describe('plugin commands register', () => {
    it('should list enabled plugin commands through listPluginCommands()', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('cmd-demo', {}, {
        commands: {
          'deploy.md': '---\nname: deploy\ndescription: Deploy the app\n---\nDeploy it now.',
          'test.md': '---\nname: test\ndescription: Run tests\n---\nRun the test suite.',
        },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const commands = await svc.listPluginCommands();
        const names = commands.map((c) => c.name).toSorted();
        expect(names).toEqual(['deploy', 'test']);

        const deploy = commands.find((c) => c.name === 'deploy');
        expect(deploy).toEqual(
          expect.objectContaining({
            pluginId: 'cmd-demo',
            description: 'Deploy the app',
            body: 'Deploy it now.',
          }),
        );
      } finally {
        host.dispose();
      }
    });

    it('should exclude commands from disabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('disabled-cmd', {}, {
        commands: { 'run.md': '---\nname: run\n---\nRun it.' },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });
        await svc.setPluginEnabled({ id: 'disabled-cmd', enabled: false });

        const commands = await svc.listPluginCommands();
        expect(commands).toEqual([]);
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 4. Session-start injection fires (AgentPluginService rendering)
  // -----------------------------------------------------------------------

  describe('session-start injection', () => {
    it('should expose enabled session-start declarations from installed plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('session-start-demo', {
        sessionStart: { skill: 'onboard' },
      }, { skills: ['onboard'] });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const sessionStarts = await svc.enabledSessionStarts();
        expect(sessionStarts).toEqual([
          { pluginId: 'session-start-demo', skillName: 'onboard' },
        ]);
      } finally {
        host.dispose();
      }
    });

    it('should exclude session-start from disabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('disabled-session', {
        sessionStart: { skill: 'onboard' },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });
        await svc.setPluginEnabled({ id: 'disabled-session', enabled: false });

        const sessionStarts = await svc.enabledSessionStarts();
        expect(sessionStarts).toEqual([]);
      } finally {
        host.dispose();
      }
    });

    it('should return empty session-starts for a plugin without sessionStart declaration', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('no-session-start', { version: '1.0.0' });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const sessionStarts = await svc.enabledSessionStarts();
        expect(sessionStarts).toEqual([]);
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 5. Plugin agent profile loader (pluginAgentRoots)
  // -----------------------------------------------------------------------

  describe('plugin agent profile roots', () => {
    it('should return agent roots from enabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('agent-demo', {}, { agents: true });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const roots = await svc.pluginAgentRoots();
        expect(roots).toEqual([
          expect.objectContaining({ source: 'plugin' }),
        ]);
        // The path should point to the agents directory in the managed copy
        expect(roots[0]!.path).toContain('agents');
      } finally {
        host.dispose();
      }
    });

    it('should exclude agent roots from disabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('disabled-agent', {}, { agents: true });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });
        await svc.setPluginEnabled({ id: 'disabled-agent', enabled: false });

        const roots = await svc.pluginAgentRoots();
        expect(roots).toEqual([]);
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 6. Marketplace context rejection
  // -----------------------------------------------------------------------

  describe('marketplace.json support', () => {
    it('should ignore forged marketplace context from install callers', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('marketplace-demo', { version: '1.0.0' });

      const manager = new PluginManager({ kimiHomeDir: home });
      await manager.load();
      // Pass marketplace option as extra argument — should be ignored
      const record = await (manager.install as (source: string, options?: unknown) => Promise<unknown>)(
        pluginRoot,
        { marketplace: { id: 'marketplace-demo', tier: 'official' } },
      );
      expect((record as { marketplace?: unknown }).marketplace).toBeUndefined();
      await rm(home, { recursive: true, force: true });
    });

    it('should ignore marketplace metadata when installing from GitHub via PluginService', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const sourceRoot = await makePluginDir('gh-marketplace', { version: '1.0.0' });
      const zipBuffer = await zipDir(sourceRoot);
      mockGithubFetch({ releaseTag: 'v1.0.0', tarball: zipBuffer });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        // The PluginService.installPlugin only accepts { source: string }
        // — marketplace is not part of the contract, which is the correct
        // v2 design: no marketplace metadata leaks in.
        const summary = await svc.installPlugin({
          source: 'https://github.com/example/gh-marketplace',
        });
        expect(summary.id).toBe('gh-marketplace');
        // No marketplace field on the summary type — by design
        expect((summary as unknown as Record<string, unknown>)['marketplace']).toBeUndefined();
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 7. Plugin hooks surface
  // -----------------------------------------------------------------------

  describe('plugin hooks surface', () => {
    it('should expose enabled plugin hooks through enabledHooks()', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('hooks-demo', {
        hooks: [
          { event: 'PreToolUse', command: './hooks/guard.sh', timeout: 15 },
          { event: 'PostToolUse', command: './hooks/log.sh' },
        ],
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });

        const hooks = await svc.enabledHooks();
        expect(hooks).toHaveLength(2);
        expect(hooks[0]).toEqual(
          expect.objectContaining({
            event: 'PreToolUse',
            command: './hooks/guard.sh',
            timeout: 15,
          }),
        );
        // Hooks from enabled plugins should get cwd and env injected
        for (const hook of hooks) {
          expect(hook).toEqual(
            expect.objectContaining({
              cwd: expect.any(String),
              env: expect.objectContaining({
                MIRRICODE_HOME: home,
                MIRRI_PLUGIN_ROOT: expect.any(String),
              }),
            }),
          );
        }
      } finally {
        host.dispose();
      }
    });

    it('should exclude hooks from disabled plugins', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('disabled-hooks', {
        hooks: [{ event: 'PreToolUse', command: './x.sh' }],
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);
        await svc.installPlugin({ source: pluginRoot });
        await svc.setPluginEnabled({ id: 'disabled-hooks', enabled: false });

        const hooks = await svc.enabledHooks();
        expect(hooks).toEqual([]);
      } finally {
        host.dispose();
      }
    });
  });

  // -----------------------------------------------------------------------
  // 8. Full lifecycle: install → list → info → disable → enable → remove
  // -----------------------------------------------------------------------

  describe('plugin lifecycle', () => {
    it('should complete the full install-list-disable-enable-remove lifecycle', async () => {
      const home = await makeHome();
      await makeValidInstalledFile(home);
      const pluginRoot = await makePluginDir('lifecycle-demo', {
        version: '1.0.0',
        mcpServers: { tool: { command: 'tool-mcp' } },
        systemPrompt: 'Always be helpful.',
      }, {
        skills: ['onboard'],
        commands: { 'run.md': '---\nname: run\ndescription: Run it\n---\nRun.' },
      });

      const host = makeHost(home);
      try {
        const svc = host.app.accessor.get(IPluginService);

        // Install
        const installed = await svc.installPlugin({ source: pluginRoot });
        expect(installed.id).toBe('lifecycle-demo');
        expect(installed.enabled).toBe(true);

        // List
        const listed = await svc.listPlugins();
        expect(listed).toEqual([expect.objectContaining({ id: 'lifecycle-demo' })]);

        // Info
        const info = await svc.getPluginInfo({ id: 'lifecycle-demo' });
        expect(info.version).toBe('1.0.0');
        expect(info.mcpServerCount).toBe(1);
        expect(info.hookCount).toBe(0);
        expect(info.commandCount).toBe(1);

        // Consumption reads
        const servers = await svc.enabledMcpServers();
        expect(servers['plugin-lifecycle-demo:tool']).toBeDefined();

        const prompts = await svc.enabledSystemPrompts();
        expect(prompts).toEqual([
          { pluginId: 'lifecycle-demo', content: 'Always be helpful.' },
        ]);

        // Disable
        await svc.setPluginEnabled({ id: 'lifecycle-demo', enabled: false });
        const afterDisable = await svc.listPlugins();
        expect(afterDisable[0]!.enabled).toBe(false);
        const noServers = await svc.enabledMcpServers();
        expect(Object.keys(noServers)).toHaveLength(0);

        // Enable
        await svc.setPluginEnabled({ id: 'lifecycle-demo', enabled: true });
        const afterEnable = await svc.listPlugins();
        expect(afterEnable[0]!.enabled).toBe(true);
        const restoredServers = await svc.enabledMcpServers();
        expect(restoredServers['plugin-lifecycle-demo:tool']).toBeDefined();

        // Remove
        await svc.removePlugin({ id: 'lifecycle-demo' });
        const afterRemove = await svc.listPlugins();
        expect(afterRemove).toEqual([]);
      } finally {
        host.dispose();
      }
    });
  });
});
