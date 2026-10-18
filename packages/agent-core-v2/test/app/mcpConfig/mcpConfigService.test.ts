/**
 * Scenario: the layered MCP config read/write service — scope views, the
 * three-layer merge, write-target rules (A1/A2), tool-filter layer search,
 * and the workspace-trust gate.
 *
 * Constructs `McpConfigService` directly through a test instantiation service
 * with structural workspace stubs. Run with
 * `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/app/mcpConfig/mcpConfigService.test.ts`.
 */

import { mkdtempSync } from 'node:fs';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';
import { afterEach, describe, expect, it } from 'vitest';

import { stdioFixture } from '../../mcpCore/stubs';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { TestInstantiationService } from '#/_base/di/test';
import type { ServiceIdentifier } from '#/_base/di/instantiation';
import { LifecycleScope } from '#/_base/di/scope';
import { IMcpConfigService, type McpConfigEntry } from '#/app/mcpConfig/mcpConfig';
import { readMcpJsonEntries } from '#/app/mcpConfig/mcpJsonFile';
import { McpConfigService } from '#/app/mcpConfig/mcpConfigService';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IPluginService, type SetPluginMcpServerToolFilterInput } from '#/app/plugin/plugin';
import type { PluginMcpServerEntry } from '#/app/plugin/types';
import { IWorkspaceService, type Workspace } from '#/app/workspace/workspace';
import { IWorkspaceLifecycleService } from '#/app/workspaceLifecycle/workspaceLifecycle';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { Error2, ErrorCodes } from '#/errors';
import { IWorkspaceContext } from '#/workspace/workspaceContext/workspaceContext';
import { IWorkspaceTrust } from '#/workspace/workspaceTrust/workspaceTrust';

import { stubBootstrap } from '../bootstrap/stubs';

const fs = new HostFileSystem();

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-mcp-config-service-'));
  tempDirs.push(dir);
  return dir;
}

async function writeJson(path: string, value: unknown): Promise<void> {
  await mkdir(join(path, '..'), { recursive: true });
  await writeFile(path, JSON.stringify(value), 'utf-8');
}

/** A workspace handle stub exposing one cwd and trust flag. */
function makeHandle(cwd: string, trusted: boolean) {
  return {
    id: 'ws-test',
    kind: LifecycleScope.Workspace,
    accessor: {
      get: <T>(id: ServiceIdentifier<T>): T => {
        if (id === IWorkspaceContext) return { cwd } as T;
        if (id === IWorkspaceTrust) return { isTrusted: () => trusted } as T;
        throw new Error(`unexpected service request in stub handle: ${String(id)}`);
      },
    },
    dispose: () => {},
  };
}

interface ServiceOptions {
  readonly homeDir: string;
  /** The open workspace: [cwd, trusted]. Omit for "no workspace open". */
  readonly workspace?: { readonly cwd: string; readonly trusted: boolean };
  /** The plugin-contributed MCP entries the plugin service reports. */
  readonly pluginEntries?: readonly PluginMcpServerEntry[];
  /** Receives the tool-filter patches routed to the plugin service. */
  readonly onPluginToolFilter?: (input: SetPluginMcpServerToolFilterInput) => void;
}

function makeService(options: ServiceOptions): IMcpConfigService {
  const ix = new TestInstantiationService();
  ix.stub(IBootstrapService, stubBootstrap(options.homeDir));
  ix.stub(IHostFileSystem, fs);
  ix.stub(IPluginService, {
    _serviceBrand: undefined,
    listMcpServerEntries: async () => options.pluginEntries ?? [],
    setPluginMcpServerToolFilter: async (input: SetPluginMcpServerToolFilterInput) => {
      options.onPluginToolFilter?.(input);
    },
  } as unknown as IPluginService);

  const handle = options.workspace === undefined ? undefined : makeHandle(options.workspace.cwd, options.workspace.trusted);
  ix.stub(IWorkspaceLifecycleService, {
    _serviceBrand: undefined,
    handlerFor: async () => {
      throw new Error('the no-workspace tests must not materialize a handle');
    },
    handlers: { list: () => (handle === undefined ? [] : [handle]) },
    sessions: {},
  } as unknown as IWorkspaceLifecycleService);
  ix.stub(IWorkspaceService, {
    _serviceBrand: undefined,
    list: async () => [] as readonly Workspace[],
  } as unknown as IWorkspaceService);

  ix.set(IMcpConfigService, new SyncDescriptor(McpConfigService));
  return ix.get(IMcpConfigService);
}

/**
 * A test fixture mirroring a real repo checkout: `<root>/.git`,
 * `<root>/.mcp.json` (project-root layer), `<root>/sub/.mirri-code/mcp.json`
 * (project layer). The workspace cwd is `<root>/sub`, so the git work-tree
 * probe resolves the project-root layer to `<root>`.
 */
interface Repo {
  readonly root: string;
  readonly cwd: string;
  readonly projectRootFile: string;
  readonly projectFile: string;
}

async function makeRepo(): Promise<Repo> {
  const root = makeTempDir();
  const cwd = join(root, 'sub');
  await mkdir(join(root, '.git'), { recursive: true });
  await mkdir(cwd, { recursive: true });
  return {
    root,
    cwd,
    projectRootFile: join(root, '.mcp.json'),
    projectFile: join(cwd, '.mirri-code', 'mcp.json'),
  };
}

function entryView(entries: readonly McpConfigEntry[]): Array<[string, string]> {
  return entries.map((entry) => [entry.name, entry.source]);
}

function expectError2(promise: Promise<unknown>, code: string): Promise<Error2> {
  return promise.then(
    () => {
      throw new Error('expected the promise to reject');
    },
    (error: unknown) => {
      expect(error).toBeInstanceOf(Error2);
      expect((error as Error2).code).toBe(code);
      return error as Error2;
    },
  );
}

describe('McpConfigService.list', () => {
  it('should merge the three layers with later layers replacing at first-appearance position when no scope is given', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        alpha: { command: 'user-alpha' },
        beta: { command: 'user-beta' },
      },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { beta: { command: 'root-beta' }, gamma: { command: 'root-gamma' } },
    });
    await writeJson(repo.projectFile, {
      mcpServers: { alpha: { command: 'project-alpha' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const entries = await service.list();

    expect(entryView(entries)).toEqual([
      ['alpha', 'project'],
      ['beta', 'projectRoot'],
      ['gamma', 'projectRoot'],
    ]);
    expect(entries[0]!.config).toMatchObject({ transport: 'stdio', command: 'project-alpha' });
  });

  it('should return only the user layer as stored when scope is user', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { beta: { command: 'root-beta' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const entries = await service.list('user');

    expect(entryView(entries)).toEqual([['alpha', 'user']]);
  });

  it('should return the project-root and project layers merged when scope is workspace', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { beta: { command: 'root-beta' } },
    });
    await writeJson(repo.projectFile, {
      mcpServers: { alpha: { command: 'project-alpha' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const entries = await service.list('workspace');

    expect(entryView(entries)).toEqual([
      ['beta', 'projectRoot'],
      ['alpha', 'project'],
    ]);
  });

  it('should show only the user layer when the workspace is untrusted', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { beta: { command: 'root-beta' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: false } });

    expect(entryView(await service.list())).toEqual([['alpha', 'user']]);
    expect(await service.list('workspace')).toEqual([]);
  });

  it('should show only the user layer when no workspace is open', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });

    const service = makeService({ homeDir: home });
    const entries = await service.list();

    expect(entryView(entries)).toEqual([['alpha', 'user']]);
  });

  it('should return an empty workspace view when no workspace is open', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    expect(await service.list('workspace')).toEqual([]);
  });

  it('should present an inferred transport tag and a degraded effective view for a broken entry', async () => {    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        flat: { command: 'npx' },
        broken: { enabled: true },
      },
    });

    const service = makeService({ homeDir: home });
    const entries = await service.list('user');

    expect(entries[0]).toMatchObject({
      name: 'flat',
      config: { transport: 'stdio', command: 'npx' },
      effective: { enabled: true },
    });
    expect(entries[1]).toMatchObject({
      name: 'broken',
      effective: { enabled: true },
    });
  });

  it('should append plugin entries with their user state when no scope is given, and keep scoped views file-only', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });
    const service = makeService({
      homeDir: home,
      pluginEntries: [
        {
          pluginId: 'demo',
          name: 'helper',
          runtimeName: 'plugin-demo:helper',
          config: { transport: 'stdio', command: 'helper-mcp', enabledTools: ['keep-me'] },
          state: undefined,
        },
        {
          pluginId: 'demo',
          name: 'disabled-helper',
          runtimeName: 'plugin-demo:disabled-helper',
          config: { transport: 'stdio', command: 'helper-mcp' },
          state: { enabled: false, enabledTools: ['t1'] },
        },
      ],
    });

    const entries = await service.list();

    // Plugin-bundled entries carry `revision: 'absent'`: no config-layer
    // file backs them (their connection definition is plugin-owned, A1),
    // so there is no layer revision to optimistic-lock on (F-3).
    expect(entries).toEqual([
      expect.objectContaining({ name: 'alpha', source: 'user' }),
      {
        name: 'helper',
        runtimeName: 'plugin-demo:helper',
        source: 'plugin',
        revision: 'absent',
        config: expect.objectContaining({ transport: 'stdio', command: 'helper-mcp' }),
        effective: { enabled: true, enabledTools: ['keep-me'] },
      },
      {
        name: 'disabled-helper',
        runtimeName: 'plugin-demo:disabled-helper',
        source: 'plugin',
        revision: 'absent',
        config: expect.objectContaining({ command: 'helper-mcp' }),
        effective: { enabled: false, enabledTools: ['t1'] },
      },
    ]);

    expect(await service.list('user')).toHaveLength(1);
    expect(await service.list('workspace')).toEqual([]);
  });

  it('should clear a manifest tool filter when the recorded plugin state omits it', async () => {
    const home = makeTempDir();
    const service = makeService({
      homeDir: home,
      pluginEntries: [
        {
          pluginId: 'demo',
          name: 'helper',
          runtimeName: 'plugin-demo:helper',
          config: { transport: 'stdio', command: 'helper-mcp', enabledTools: ['manifest-tool'] },
          state: { enabled: true },
        },
      ],
    });

    const entries = await service.list();

    expect(entries[0]?.effective).toEqual({ enabled: true });
  });
});

describe('McpConfigService.set', () => {
  it('should write the user layer verbatim when scope is user', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { existing: { command: 'node' } },
    });

    const service = makeService({ homeDir: home });
    await service.set('user', 'fresh', { transport: 'stdio', command: 'npx', args: ['-y', 's'] });

    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({
      existing: { command: 'node' },
      fresh: { transport: 'stdio', command: 'npx', args: ['-y', 's'] },
    });
  });

  it('should write the project layer, not the read-only project-root file, when scope is workspace', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    await service.set('workspace', 'fresh', { transport: 'stdio', command: 'npx' });

    await expect(readMcpJsonEntries(fs, repo.projectFile)).resolves.toEqual({
      fresh: { transport: 'stdio', command: 'npx' },
    });
    await expect(readMcpJsonEntries(fs, repo.projectRootFile)).resolves.toEqual({});
  });

  it('should reject plugin-bundled server names (adjudication A1)', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.set('user', 'plugin-demo:repo', { transport: 'stdio', command: 'npx' }),
      ErrorCodes.VALIDATION_FAILED,
    );
    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({});
  });

  it('should reject the workspace scope when no workspace is open', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.set('workspace', 'fresh', { transport: 'stdio', command: 'npx' }),
      ErrorCodes.VALIDATION_FAILED,
    );
  });

  it('should reject the workspace scope when the workspace is untrusted', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: false } });

    await expectError2(
      service.set('workspace', 'fresh', { transport: 'stdio', command: 'npx' }),
      ErrorCodes.VALIDATION_FAILED,
    );
    await expect(readMcpJsonEntries(fs, repo.projectFile)).resolves.toEqual({});
  });
});

describe('McpConfigService.remove', () => {
  it('should remove the entry from the addressed layer', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'x' }, beta: { command: 'y' } },
    });

    const service = makeService({ homeDir: home });
    await service.remove('user', 'alpha');

    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({ beta: { command: 'y' } });
  });

  it('should be an idempotent success when the name is absent', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expect(service.remove('user', 'missing')).resolves.toBeUndefined();
  });

  it('should reject plugin-bundled server names (adjudication A1)', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.remove('user', 'plugin-demo:repo'),
      ErrorCodes.VALIDATION_FAILED,
    );
  });

  it('should reject the workspace scope when no workspace is open', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.remove('workspace', 'alpha'),
      ErrorCodes.VALIDATION_FAILED,
    );
  });

  it('should reject the workspace scope when the workspace is untrusted', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: false } });

    await expectError2(
      service.remove('workspace', 'alpha'),
      ErrorCodes.VALIDATION_FAILED,
    );
  });
});

describe('McpConfigService.setToolFilter', () => {
  it('should patch the highest-precedence layer defining the name', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { shared: { command: 'user-shared' } },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { shared: { command: 'root-shared' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    await service.setToolFilter('shared', { enabledTools: ['t1'] });

    await expect(readMcpJsonEntries(fs, repo.projectRootFile)).resolves.toEqual({
      shared: { command: 'root-shared', enabledTools: ['t1'] },
    });
    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({ shared: { command: 'user-shared' } });
  });

  it('should patch the project layer when every layer defines the name', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { shared: { command: 'user-shared' } },
    });
    await writeJson(repo.projectRootFile, {
      mcpServers: { shared: { command: 'root-shared' } },
    });
    await writeJson(repo.projectFile, {
      mcpServers: { shared: { command: 'project-shared' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    await service.setToolFilter('shared', { enabledTools: ['t1'] });

    await expect(readMcpJsonEntries(fs, repo.projectFile)).resolves.toEqual({
      shared: { command: 'project-shared', enabledTools: ['t1'] },
    });
    await expect(readMcpJsonEntries(fs, repo.projectRootFile)).resolves.toEqual({
      shared: { command: 'root-shared' },
    });
    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({ shared: { command: 'user-shared' } });
  });

  it('should fall back to the user layer when no project layer defines the name', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    await service.setToolFilter('alpha', { disabledTools: ['t2'] });

    await expect(
      readMcpJsonEntries(fs, join(home, 'mcp.json')),
    ).resolves.toEqual({ alpha: { command: 'user-alpha', disabledTools: ['t2'] } });
  });

  it('should surface the patched filter in the effective view', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: 'user-alpha' } },
    });

    const service = makeService({ homeDir: home });
    await service.setToolFilter('alpha', { enabledTools: ['t1', 't2'] });

    const entries = await service.list('user');
    expect(entries[0]!.effective).toEqual({ enabled: true, enabledTools: ['t1', 't2'] });
  });

  it('should search only the user layer when the workspace is untrusted', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(repo.projectRootFile, {
      mcpServers: { shared: { command: 'root-shared' } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: false } });

    await expectError2(
      service.setToolFilter('shared', { enabledTools: ['t1'] }),
      ErrorCodes.MCP_SERVER_NOT_FOUND,
    );
    await expect(readMcpJsonEntries(fs, repo.projectRootFile)).resolves.toEqual({
      shared: { command: 'root-shared' },
    });
  });

  it('should throw MCP_SERVER_NOT_FOUND when no layer defines the name', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.setToolFilter('missing', { enabledTools: ['t1'] }),
      ErrorCodes.MCP_SERVER_NOT_FOUND,
    );
  });

  it('should answer MCP_SERVER_NOT_FOUND when no installed plugin defines the runtime name', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.setToolFilter('plugin-demo:repo', { enabledTools: ['t1'] }),
      ErrorCodes.MCP_SERVER_NOT_FOUND,
    );
  });

  it('should route the patch to the plugin user state when the name is plugin-bundled', async () => {
    const home = makeTempDir();
    const patches: SetPluginMcpServerToolFilterInput[] = [];
    const service = makeService({
      homeDir: home,
      pluginEntries: [
        {
          pluginId: 'demo',
          name: 'repo',
          runtimeName: 'plugin-demo:repo',
          config: { transport: 'stdio', command: 'repo-mcp' },
          state: undefined,
        },
      ],
      onPluginToolFilter: (input) => patches.push(input),
    });

    await service.setToolFilter('plugin-demo:repo', { enabledTools: ['t1'] });

    expect(patches).toEqual([
      { id: 'demo', server: 'repo', filter: { enabledTools: ['t1'] } },
    ]);
  });
});

describe('McpConfigService.test', () => {
  it('should report ok with the tool count when a direct config connects', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    const result = await service.test({
      config: { transport: 'stdio', command: process.execPath, args: [stdioFixture] },
    });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);

  it('should expand ${VAR} references in a direct config at test time (A5)', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });
    process.env['MIRRI_TEST_MCP_NODE'] = process.execPath;
    try {
      const result = await service.test({
        config: {
          transport: 'stdio',
          command: '${MIRRI_TEST_MCP_NODE}',
          args: [stdioFixture],
        },
      });

      expect(result).toEqual({ status: 'ok', toolCount: 3 });
    } finally {
      delete process.env['MIRRI_TEST_MCP_NODE'];
    }
  }, 20000);

  it('should report error with a message when the connection fails', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    const result = await service.test({
      config: { transport: 'stdio', command: '/this/path/does/not/exist/anywhere' },
    });

    expect(result.status).toBe('error');
    expect(result.toolCount).toBe(0);
    expect(result.error).toBeTruthy();
  }, 20000);

  it('should resolve a user-scoped persisted entry with the loader semantics', async () => {
    const home = makeTempDir();
    process.env['MIRRI_TEST_MCP_NODE'] = process.execPath;
    try {
      await writeJson(join(home, 'mcp.json'), {
        mcpServers: {
          alpha: { command: '${MIRRI_TEST_MCP_NODE}', args: [stdioFixture] },
        },
      });

      const service = makeService({ homeDir: home });
      const result = await service.test({ scope: 'user', name: 'alpha' });

      expect(result).toEqual({ status: 'ok', toolCount: 3 });
    } finally {
      delete process.env['MIRRI_TEST_MCP_NODE'];
    }
  }, 20000);

  it('should resolve a workspace-scoped entry from the project layer over the project root', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(repo.projectRootFile, {
      mcpServers: { alpha: { command: '/this/path/does/not/exist/anywhere' } },
    });
    await writeJson(repo.projectFile, {
      mcpServers: { alpha: { command: process.execPath, args: [stdioFixture] } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const result = await service.test({ scope: 'workspace', name: 'alpha' });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);

  it('should fall back to the project-root layer when the project layer does not define the name', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await writeJson(repo.projectRootFile, {
      mcpServers: { alpha: { command: process.execPath, args: [stdioFixture] } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const result = await service.test({ scope: 'workspace', name: 'alpha' });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);

  it('should still connect a persisted entry that is stored disabled', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: {
        alpha: { command: process.execPath, args: [stdioFixture], enabled: false },
      },
    });

    const service = makeService({ homeDir: home });
    const result = await service.test({ scope: 'user', name: 'alpha' });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);

  it('should throw MCP_SERVER_NOT_FOUND when no layer in the addressed scope defines the name', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: process.execPath, args: [stdioFixture] } },
    });

    const service = makeService({ homeDir: home });

    await expectError2(
      service.test({ scope: 'user', name: 'missing' }),
      ErrorCodes.MCP_SERVER_NOT_FOUND,
    );
  });

  it('should reject when neither a config nor a scope and name pair is given', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(service.test({}), ErrorCodes.VALIDATION_FAILED);
    await expectError2(service.test({ scope: 'user' }), ErrorCodes.VALIDATION_FAILED);
    await expectError2(service.test({ name: 'alpha' }), ErrorCodes.VALIDATION_FAILED);
  });

  it('should skip an unparseable layer file and resolve the entry from a later layer', async () => {
    const home = makeTempDir();
    const repo = await makeRepo();
    await mkdir(join(repo.cwd, '.mirri-code'), { recursive: true });
    await writeFile(repo.projectFile, 'not json at all', 'utf-8');
    await writeJson(repo.projectRootFile, {
      mcpServers: { alpha: { command: process.execPath, args: [stdioFixture] } },
    });

    const service = makeService({ homeDir: home, workspace: { cwd: repo.cwd, trusted: true } });
    const result = await service.test({ scope: 'workspace', name: 'alpha' });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);

  it('should reject a direct config that is invalid after expansion', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    await expectError2(
      service.test({
        config: { transport: 'stdio', command: '${MIRRI_TEST_MCP_UNSET_VAR}', args: [] },
      }),
      ErrorCodes.VALIDATION_FAILED,
    );
  });

  it('should prefer the direct config when both spellings are given', async () => {
    const home = makeTempDir();
    await writeJson(join(home, 'mcp.json'), {
      mcpServers: { alpha: { command: '/this/path/does/not/exist/anywhere' } },
    });

    const service = makeService({ homeDir: home });
    const result = await service.test({
      scope: 'user',
      name: 'alpha',
      config: { transport: 'stdio', command: process.execPath, args: [stdioFixture] },
    });

    expect(result).toEqual({ status: 'ok', toolCount: 3 });
  }, 20000);
});
