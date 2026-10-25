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
import {
  DEFAULT_AGENT_PROFILE_NAME,
  type AgentProfile,
} from '#/app/agentProfileCatalog/agentProfileCatalog';
import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import { AgentProfileRegistryService } from '#/app/agentProfileCatalog/agentProfileRegistryService';
import { IBuiltinAgentProfileLoader } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { BuiltinAgentProfileLoaderService } from '#/app/agentProfileCatalog/builtinAgentProfileLoaderService';
import { AGENT_PROFILE_SOURCE_PRIORITY } from '#/app/agentProfileCatalog/agentProfileContribution';
import {
  AgentProfileManagementService,
  agentFilePath,
  serializeAgentMd,
  userAgentsDir,
  readSystemMdOverride,
  stripBuiltinBasePromptRef,
  composeOntoBasePrompt,
  isComposedBody,
  DEFAULT_BASE_PROMPT_TEMPLATE,
} from '#/app/agentProfileCatalog/agentProfileManagementService';
import { SYSTEM_PROMPT_TEMPLATE } from '#/app/agentProfileCatalog/profile-shared';
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

describe('readSystemMdOverride (D21 / US-A8)', () => {
  it('returns undefined silently when SYSTEM.md is absent', async () => {
    // Given a home with NO SYSTEM.md (the default state), the override
    // read must return undefined WITHOUT a warn — every GET/PUT on the
    // agents surface lists profiles, so a missing-override warn would
    // fire on each healthy request.
    const home = makeTempDir();
    const warns: string[] = [];
    const result = await readSystemMdOverride(new HostFileSystem(), home, (m) =>
      warns.push(m),
    );
    expect(result).toBeUndefined();
    expect(warns).toEqual([]);
  });

  it('warns only on a real read failure', async () => {
    // Given a read that fails with a non-missing error (a directory in the
    // stat path is caught by the isFile check; a permission failure
    // reaches the catch), the failure degrades to undefined WITH a warn.
    const home = makeTempDir();
    const failingFs = {
      stat: () => {
        throw new Error('EACCES: permission denied');
      },
    } as unknown as HostFileSystem;
    const warns: string[] = [];
    const result = await readSystemMdOverride(failingFs, home, (m) => warns.push(m));
    expect(result).toBeUndefined();
    expect(warns).toHaveLength(1);
    expect(warns[0]).toContain('SYSTEM.md override read failed');
  });
});

describe('composition discriminator (ADR-010 ①, two-shape rule)', () => {
  it('treats the ref+separator form and the exact bare reference as composed', () => {
    expect(isComposedBody('${base_prompt}\n\nRole text')).toBe(true);
    expect(isComposedBody('${base_prompt}')).toBe(true);
    // A mid-text mention is a full replacement.
    expect(isComposedBody('Prefix ${base_prompt} mention')).toBe(false);
    expect(isComposedBody('You replace the base entirely.')).toBe(false);
  });

  it('does not double-wrap the exact bare reference', () => {
    // Given a body that is exactly the bare reference, composing must pass
    // it through verbatim — not wrap it again.
    expect(composeOntoBasePrompt('${base_prompt}')).toBe('${base_prompt}');
    expect(composeOntoBasePrompt('${base_prompt}\n\nRole')).toBe('${base_prompt}\n\nRole');
    // A mid-text mention is a full replacement — wrapped like plain text.
    expect(composeOntoBasePrompt('mention ${base_prompt} mid-text')).toBe(
      '${base_prompt}\n\nmention ${base_prompt} mid-text',
    );
  });

  it('strips both the double- and single-newline separators, and the bare reference to empty', () => {
    // ADR-010 ② + cross-engine alignment: `${base_prompt}\n\nRole` and
    // `${base_prompt}\nRole` both yield `Role`; the bare reference (or a
    // whitespace-only remainder) strips to the empty string; a mid-text
    // mention passes through verbatim.
    expect(stripBuiltinBasePromptRef('${base_prompt}\n\nRole')).toBe('Role');
    expect(stripBuiltinBasePromptRef('${base_prompt}\nRole')).toBe('Role');
    expect(stripBuiltinBasePromptRef('${base_prompt}')).toBe('');
    expect(stripBuiltinBasePromptRef('${base_prompt}\n\n')).toBe('');
    expect(stripBuiltinBasePromptRef('${base_prompt}\n\n\n  \n')).toBe('');
    expect(stripBuiltinBasePromptRef('Role ${base_prompt} mention')).toBe(
      'Role ${base_prompt} mention',
    );
  });
});

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
    // A composed custom row → the wire template is the stripped role text
    // (ADR-010 ②: the strip applies to ALL file rows).
    await writeAgentFile(
      home,
      'composed-role',
      `---\nname: composed-role\ndescription: "Composed"\n---\n\${base_prompt}\n\nYou ride the shared base.`,
    );
    // A bare-reference custom row → no wire template at all.
    await writeAgentFile(
      home,
      'bare-ref-role',
      `---\nname: bare-ref-role\ndescription: "Bare"\n---\n\${base_prompt}`,
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

    // ADR-010 ②: a composed custom row projects the stripped role text —
    // the strip applies to all file rows, custom rows included.
    const composedRow = rowByName(view, 'composed-role')!;
    expect(composedRow.system_prompt_template).toBe('You ride the shared base.');
    // A bare-reference row projects NO template (strips to empty → undefined,
    // the rust "key not emitted" shape).
    const bareRefRow = rowByName(view, 'bare-ref-role')!;
    expect(bareRefRow.system_prompt_template).toBeUndefined();

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

  it('strips the base reference from composed file rows on the catalog projection path', async () => {
    // ADR-010 ②: on the MAIN read path (the catalog projection over the
    // registry), a composed custom row projects the stripped role text and
    // a bare-reference row projects no template at all — the same shapes
    // the fallback scan and the rust wire row produce. The file rows are
    // seeded directly into the registry the way the user-file loader
    // registers them (raw body in `systemPromptTemplate`).
    const home = makeTempDir();
    const ix = new TestInstantiationService();
    const registry = new AgentProfileRegistryService();
    const builtin = new BuiltinAgentProfileLoaderService(registry);
    ix.stub(IAgentProfileRegistry, registry);
    ix.stub(IBuiltinAgentProfileLoader, builtin);
    ix.stub(IHostFileSystem, new HostFileSystem());
    ix.stub(ILogService, mockLogService());
    ix.stub(IBootstrapService, stubBootstrap(home));
    const handle = makeHandle('ws-open', new ReloadRecorder());
    ix.stub(IWorkspaceLifecycleService, {
      _serviceBrand: undefined,
      handlerFor: async () => handle,
      handlers: { list: () => [handle] },
      sessions: {},
    } as unknown as IWorkspaceLifecycleService);
    ix.stub(IWorkspaceService, {
      _serviceBrand: undefined,
      list: async () => [],
    } as unknown as IWorkspaceService);
    ix.set(
      IAgentProfileManagementService,
      new SyncDescriptor(AgentProfileManagementService),
    );

    const fileProfile = (name: string, prompt: string): AgentProfile => ({
      name,
      description: `${name} description`,
      filePath: `/tmp/agents/${name}.md`,
      systemPromptTemplate: prompt,
      systemPrompt: () => prompt,
    });
    registry.register(
      'user',
      {
        profiles: [
          fileProfile('composed-role', '${base_prompt}\n\nYou ride the shared base.'),
          fileProfile('bare-ref-role', '${base_prompt}'),
          fileProfile('replacement-role', 'You replace the base entirely.'),
        ],
      },
      { priority: AGENT_PROFILE_SOURCE_PRIORITY.user },
    );
    const service = ix.get(IAgentProfileManagementService);

    const view = await service.list();

    expect(rowByName(view, 'composed-role')!.system_prompt_template).toBe(
      'You ride the shared base.',
    );
    expect(rowByName(view, 'bare-ref-role')!.system_prompt_template).toBeUndefined();
    expect(rowByName(view, 'replacement-role')!.system_prompt_template).toBe(
      'You replace the base entirely.',
    );
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

  it('carries the builtin default template on the essential row (the no-override edit seed)', async () => {
    // Given a home with no SYSTEM.md, the essential row's edit seed rides:
    // the builtin default template's RAW text (the same asset the renderer
    // uses), placeholders intact — the settings editor seeds from it when
    // no override exists.
    const home = makeTempDir();
    const service = makeService({ homeDir: home });

    const view = await service.list();
    const agentRow = rowByName(view, DEFAULT_AGENT_PROFILE_NAME)!;
    expect(agentRow.base_prompt_override).toBe(false);
    expect(agentRow.base_prompt_template).toBeUndefined();
    expect(agentRow.base_prompt_default_template).toBeDefined();
    expect(agentRow.base_prompt_default_template).toBe(SYSTEM_PROMPT_TEMPLATE);
    expect(agentRow.base_prompt_default_template).toContain('You are');
    // The seed is the RAW template, never a rendered form: placeholders
    // survive (a rendered form would bake empty date/cwd values that an
    // override save would freeze).
    expect(agentRow.base_prompt_default_template).toContain('${cwd}');

    // And non-essential rows carry no disclosure fields at all
    const coderRow = rowByName(view, 'coder')!;
    expect(coderRow.base_prompt_default_template).toBeUndefined();
    expect(coderRow.base_prompt_override).toBeUndefined();
  });

  it('carries the default template alongside an active SYSTEM.md override', async () => {
    // Given a home carrying a SYSTEM.md, the essential row discloses the
    // override AND keeps the default template (the wire stays stable
    // across override states — the seed is simply not read then).
    const home = makeTempDir();
    await writeFile(join(home, 'SYSTEM.md'), 'CUSTOM_LAYER ${base_prompt}', 'utf-8');
    const service = makeService({ homeDir: home });

    const view = await service.list();
    const agentRow = rowByName(view, DEFAULT_AGENT_PROFILE_NAME)!;
    expect(agentRow.base_prompt_override).toBe(true);
    expect(agentRow.base_prompt_template).toBe('CUSTOM_LAYER ${base_prompt}');
    expect(agentRow.base_prompt_default_template).toBe(SYSTEM_PROMPT_TEMPLATE);
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

  it('wraps a user row with a prompt onto the base reference when toggling', async () => {
    // Given a custom profile whose file body is plain role text (no base
    // reference), a toggle rewrite must compose the body onto the base
    // reference — the custom row is isomorphic with a builtin override
    // (SDR-014 ②): disabling it must not degrade its prompt to a literal
    // replacement body.
    const home = makeTempDir();
    await writeAgentFile(home, 'reviewer', agentMd({
      name: 'reviewer',
      description: 'Reviews code',
    }));

    const service = makeService({ homeDir: home });
    await service.setEnabled('reviewer', false);

    const written = await readFile(agentFilePath(home, 'reviewer'), 'utf-8');
    expect(written).toContain('enabled: false');
    expect(written).toContain('${base_prompt}\n\nYou are a specialist.');
    expect(written).not.toContain('You are a helpful AI assistant.');
  });

  it('writes the bare base reference when toggling a user row without a template', async () => {
    // Given a custom profile whose row projects no editable template
    // (already bare-reference composed body), the toggle rewrite lands the
    // bare `${base_prompt}` reference — never the placeholder
    // (SDR-014 ②③).
    const home = makeTempDir();
    await writeAgentFile(home, 'sleeper', agentMd({
      name: 'sleeper',
      description: 'No prompt of its own',
    }, '${base_prompt}'));

    const service = makeService({ homeDir: home });
    await service.setEnabled('sleeper', false);

    const written = await readFile(agentFilePath(home, 'sleeper'), 'utf-8');
    expect(written).toContain('enabled: false');
    const body = written
      .split('---\n')
      .slice(2)
      .join('---\n')
      .trim();
    expect(body).toBe('${base_prompt}');
    expect(written).not.toContain('You are a helpful AI assistant.');
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

describe('serializeAgentMd (composition semantics, SDR-014)', () => {
  it('writes the bare base reference when no system prompt template is given', () => {
    // Given a profile serialized without a prompt, the body is the bare
    // `${base_prompt}` reference — the placeholder text is retired from
    // every TS write channel (SDR-014 ②③).
    const text = serializeAgentMd({
      name: 'sleeper',
      description: 'No prompt of its own',
    });
    expect(text).toContain('---\n');
    expect(text).toContain('${base_prompt}');
    expect(text).not.toContain('You are a helpful AI assistant.');
  });

  it('writes the given template verbatim as the body', () => {
    // Given a template, the body is that template — the callers own the
    // wrapping decision (the REST routes and setEnabled wrap plain text
    // onto the base reference; an already-composed body passes through).
    const text = serializeAgentMd({
      name: 'reviewer',
      description: 'Reviews code',
      systemPromptTemplate: '${base_prompt}\n\nYou review code.',
    });
    expect(text.endsWith('${base_prompt}\n\nYou review code.')).toBe(true);
  });
});
