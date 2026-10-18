/**
 * Scenario: `IAgentProfileManagementService` (R5-2) — the profile data plane
 * ported out of the kap-server `/agents` routes.
 *
 * Covers:
 * - `list` zero-workspace fallback: builtin rows (essential flag), the user
 *   directory scan (disabled rows, parse skips, builtin overrides with
 *   suppressed candidates).
 * - `list` catalog projection: the open handler's merged view, snake_case
 *   wire rows, discovery skips folded from the registry.
 * - `list(workspaceRoot)`: the explicit root routes through
 *   `IWorkspaceLifecycleService.handlerFor`.
 * - `setEnabled`: the full write channel (`.md` rewrite + legacy cleanup +
 *   loader reload) for both a user row (fallback scan) and a builtin row
 *   (catalog projection, `${base_prompt}` override body), the essential
 *   guard, and the not-found rejection.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/app/agentProfileCatalog/agentProfileManagementService.test.ts`.
 */

import { mkdtempSync } from 'node:fs';
import { existsSync } from 'node:fs';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { TestInstantiationService } from '#/_base/di/test';
import type { ServiceIdentifier } from '#/_base/di/instantiation';
import { LifecycleScope, type IWorkspaceScopeHandle } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import { Error2 } from '#/errors';
import { DEFAULT_AGENT_PROFILE_NAME } from '#/app/agentProfileCatalog/agentProfileCatalog';
import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import { AgentProfileRegistryService } from '#/app/agentProfileCatalog/agentProfileRegistryService';
import { IBuiltinAgentProfileLoader } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { BuiltinAgentProfileLoaderService } from '#/app/agentProfileCatalog/builtinAgentProfileLoaderService';
import {
  AgentProfileManagementService,
  agentFilePath,
  userAgentsDir,
} from '#/app/agentProfileCatalog/agentProfileManagementService';
import { IAgentProfileManagementService } from '#/app/agentProfileCatalog/agentProfileManagement';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IWorkspaceService, type Workspace } from '#/app/workspace/workspace';
import { IWorkspaceLifecycleService } from '#/app/workspaceLifecycle/workspaceLifecycle';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IUserAgentProfileLoader } from '#/workspace/workspaceAgentProfileLoader/userAgentProfileLoader';

// Trigger the "import = register" side effects so builtin profiles load.
import '#/session/agentLifecycle/profile/profiles';
import '#/agent/plan/profile/plan';

import { stubBootstrap } from '../bootstrap/stubs';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
  vi.restoreAllMocks();
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-agent-profile-management-'));
  tempDirs.push(dir);
  return dir;
}

function mockLogService(): ILogService {
  return {
    _serviceBrand: undefined,
    debug: () => {},
    info: () => {},
    warn: () => {},
    error: () => {},
    critical: () => {},
    dispose: () => {},
    isDisposed: false,
  } as unknown as ILogService;
}

/** The recorded loader reload calls for one handle. */
class ReloadRecorder {
  readonly calls: number[] = [];
  readonly loader: IUserAgentProfileLoader = {
    _serviceBrand: undefined,
    reload: async () => {
      this.calls.push(Date.now());
    },
  } as unknown as IUserAgentProfileLoader;
}

/**
 * A fake workspace handle: the read projection only reads `id`, while the
 * reload channel resolves `IUserAgentProfileLoader` from the accessor.
 */
function makeHandle(id: string, reload: ReloadRecorder): IWorkspaceScopeHandle {
  return {
    id,
    kind: LifecycleScope.Workspace,
    accessor: {
      get: <T>(token: ServiceIdentifier<T>): T => {
        if (token === IUserAgentProfileLoader) return reload.loader as T;
        throw new Error(`unexpected service request in stub handle: ${String(token)}`);
      },
    },
    dispose: () => {},
  } as unknown as IWorkspaceScopeHandle;
}

interface ServiceOptions {
  readonly homeDir: string;
  /** The open workspace handle. Omit for the zero-workspace fallback. */
  readonly handle?: IWorkspaceScopeHandle;
  /** The known-but-not-open workspaces. */
  readonly workspaces?: readonly Workspace[];
  /** Records `handlerFor` calls (the explicit-root path). */
  readonly onHandlerFor?: (ref: unknown) => void;
}

function makeService(options: ServiceOptions): IAgentProfileManagementService {
  const ix = new TestInstantiationService();
  const registry = new AgentProfileRegistryService();
  const builtin = new BuiltinAgentProfileLoaderService(registry);
  ix.stub(IAgentProfileRegistry, registry);
  ix.stub(IBuiltinAgentProfileLoader, builtin);
  ix.stub(IHostFileSystem, new HostFileSystem());
  ix.stub(ILogService, mockLogService());
  ix.stub(IBootstrapService, stubBootstrap(options.homeDir));

  const handlerFor = vi.fn(async (ref: { root?: string; workspaceId?: string }) => {
    options.onHandlerFor?.(ref);
    return options.handle ?? makeHandle('ws-materialized', new ReloadRecorder());
  });
  ix.stub(IWorkspaceLifecycleService, {
    _serviceBrand: undefined,
    handlerFor,
    handlers: { list: () => (options.handle === undefined ? [] : [options.handle]) },
    sessions: {},
  } as unknown as IWorkspaceLifecycleService);
  ix.stub(IWorkspaceService, {
    _serviceBrand: undefined,
    list: async () => options.workspaces ?? [],
  } as unknown as IWorkspaceService);
  ix.set(IAgentProfileManagementService, new SyncDescriptor(AgentProfileManagementService));
  return ix.get(IAgentProfileManagementService);
}

async function writeAgentFile(homeDir: string, name: string, text: string): Promise<string> {
  const dir = userAgentsDir(homeDir);
  await mkdir(dir, { recursive: true });
  const path = join(dir, `${name}.md`);
  await writeFile(path, text, 'utf-8');
  return path;
}

function agentMd(frontmatter: Record<string, unknown>, body = 'You are a specialist.'): string {
  const lines = Object.entries(frontmatter).map(([k, v]) => `${k}: ${JSON.stringify(v)}`);
  return `---\n${lines.join('\n')}\n---\n${body}`;
}

function rowByName(view: { entries: readonly { name: string }[] }, name: string) {
  return view.entries.find((entry) => entry.name === name) as
    | import('#/app/agentProfileCatalog/agentProfileManagement').AgentProfileEntry
    | undefined;
}

async function expectError2(promise: Promise<unknown>, code: string): Promise<Error2> {
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

describe('AgentProfileManagementService.list', () => {
  it('falls back to builtin + user scan with no workspace: essential builtin, disabled row, override row, and parse skips', async () => {
    const home = makeTempDir();
    await writeAgentFile(home, 'code-reviewer', agentMd({
      name: 'code-reviewer',
      description: 'Reviews code',
    }));
    await writeAgentFile(home, 'off-role', agentMd({
      name: 'off-role',
      description: 'Disabled role',
      enabled: false,
    }));
    // A user file covering the builtin default → has_override + suppressed.
    await writeAgentFile(
      home,
      DEFAULT_AGENT_PROFILE_NAME,
      `---\nname: ${DEFAULT_AGENT_PROFILE_NAME}\ndescription: "Custom default"\n---\nYou are the overridden default.`,
    );
    // A parse failure → skipped.
    await writeAgentFile(home, 'broken', 'not frontmatter at all');

    const service = makeService({ homeDir: home });
    const view = await service.list();

    const names = view.entries.map((entry) => entry.name);
    expect(names).toContain(DEFAULT_AGENT_PROFILE_NAME);
    expect(names).toContain('code-reviewer');
    expect(names).toContain('off-role');
    expect(names).toContain('coder'); // a builtin other than the default

    const agentRow = rowByName(view, DEFAULT_AGENT_PROFILE_NAME)!;
    expect(agentRow.source).toBe('user');
    expect(agentRow.builtin).toBe(false);
    expect(agentRow.essential).toBe(true);
    expect(agentRow.enabled).toBe(true);
    expect(agentRow.has_override).toBe(true);
    expect(agentRow.suppressed_candidates).toEqual([
      { source: 'builtin', priority: 0, reason: 'priority' },
    ]);
    expect(agentRow.system_prompt_template).toBe('You are the overridden default.');
    expect(typeof agentRow.effective_system_prompt).toBe('string');

    const coderRow = rowByName(view, 'coder')!;
    expect(coderRow.source).toBe('builtin');
    expect(coderRow.builtin).toBe(true);
    expect(coderRow.essential).toBe(false);

    const offRow = rowByName(view, 'off-role')!;
    expect(offRow.enabled).toBe(false);
    expect(offRow.source).toBe('user');
    expect(offRow.essential).toBe(false);

    const reviewRow = rowByName(view, 'code-reviewer')!;
    expect(reviewRow.enabled).toBe(true);
    expect(reviewRow.file_path).toBe(agentFilePath(home, 'code-reviewer'));

    // The broken file surfaces as a skip with the parse reason.
    expect(view.skipped).toHaveLength(1);
    expect(view.skipped[0]!.path).toBe(agentFilePath(home, 'broken'));
    expect(view.skipped[0]!.reason).toBeTruthy();
  });

  it('projects through the catalog when a workspace handler is open', async () => {
    const home = makeTempDir();
    const reload = new ReloadRecorder();
    const handle = makeHandle('ws-open', reload);
    const service = makeService({ homeDir: home, handle });

    const view = await service.list();

    const agentRow = rowByName(view, DEFAULT_AGENT_PROFILE_NAME)!;
    expect(agentRow.source).toBe('builtin');
    expect(agentRow.builtin).toBe(true);
    expect(agentRow.essential).toBe(true);
    expect(agentRow.enabled).toBe(true);
    expect(agentRow.has_override).toBe(false);
    expect(typeof agentRow.effective_system_prompt).toBe('string');
    expect(view.skipped).toEqual([]);
  });

  it('routes an explicit workspace root through handlerFor', async () => {
    const home = makeTempDir();
    const seen: unknown[] = [];
    const service = makeService({
      homeDir: home,
      onHandlerFor: (ref) => seen.push(ref),
    });
    await service.list('/some/workspace/root');
    expect(seen).toEqual([{ root: '/some/workspace/root' }]);
  });
});

describe('AgentProfileManagementService.setEnabled', () => {
  it('writes the .md rewrite + legacy cleanup for a user row found by the fallback scan', async () => {
    const home = makeTempDir();
    await writeAgentFile(home, 'code-reviewer', agentMd({
      name: 'code-reviewer',
      description: 'Reviews code',
      tools: ['Read', 'Grep'],
      defaultModel: 'ultra',
    }));
    // A stale legacy yaml that must be cleaned up once the .md exists.
    await writeFile(join(userAgentsDir(home), 'code-reviewer.yaml'), 'old: true', 'utf-8');

    const service = makeService({ homeDir: home });
    await service.setEnabled('code-reviewer', false);

    const written = await readFile(agentFilePath(home, 'code-reviewer'), 'utf-8');
    expect(written).toContain('name: code-reviewer');
    expect(written).toContain('description: Reviews code');
    expect(written).toContain('enabled: false');
    expect(written).toContain('defaultModel: ultra');
    expect(written).toContain('- Read');
    // The legacy file is gone.
    expect(existsSync(join(userAgentsDir(home), 'code-reviewer.yaml'))).toBe(false);
  });

  it('reloads the user loader through the open handle after the builtin-override write', async () => {
    const home = makeTempDir();
    const reload = new ReloadRecorder();
    const handle = makeHandle('ws-open', reload);
    const service = makeService({ homeDir: home, handle });

    // `coder` is a builtin row: the toggle writes an override file whose
    // body embeds the shared base through `${base_prompt}`.
    await service.setEnabled('coder', false);

    const written = await readFile(agentFilePath(home, 'coder'), 'utf-8');
    expect(written).toContain('enabled: false');
    expect(written).toContain('${base_prompt}');
    expect(reload.calls).toHaveLength(1);
  });

  it('rejects disabling the essential profile with agent_profile.essential', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });
    const error = await expectError2(
      service.setEnabled(DEFAULT_AGENT_PROFILE_NAME, false),
      'agent_profile.essential',
    );
    expect(error.message).toBe(
      `Cannot disable the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
    );
  });

  it('rejects an unknown name with agent_profile.not_found', async () => {
    const home = makeTempDir();
    const service = makeService({ homeDir: home });
    const error = await expectError2(
      service.setEnabled('no-such-role', true),
      'agent_profile.not_found',
    );
    expect(error.message).toBe('Agent profile "no-such-role" was not found');
  });
});

describe('AgentProfileManagementService.reload', () => {
  it('reloads the user loader on the open handle and is a no-op without one', async () => {
    const home = makeTempDir();
    const reload = new ReloadRecorder();
    const handle = makeHandle('ws-open', reload);
    const withHandle = makeService({ homeDir: home, handle });
    await withHandle.reload();
    expect(reload.calls).toHaveLength(1);

    const withoutHandle = makeService({ homeDir: home });
    await expect(withoutHandle.reload()).resolves.toBeUndefined();
  });
});
