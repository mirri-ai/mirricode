/**
 * Scenario: the Session-scope agent-profile catalog projection over the
 * App-scope `IAgentProfileRegistry`.
 *
 * Exercises `SessionAgentProfileCatalogService` directly (no DI scope host):
 * a hand-driven `AgentProfileRegistryService` plus a stub log verify the
 * projection rules — relevant-entry filtering by the seeded workspace key,
 * priority-ordered name dedup, the declaration-free builtin override (D14),
 * change-event fan-out, and the read surface (`get` / `list` / `getDefault` /
 * `inspect`).
 * Run:
 * `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog.test.ts`.
 */

import { describe, expect, it } from 'vitest';

import {
  DEFAULT_AGENT_PROFILE_NAME,
  type AgentProfile,
} from '#/app/agentProfileCatalog/agentProfileCatalog';
import { BUILTIN_AGENT_PROFILE_SOURCE_ID } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { AgentProfileRegistryService } from '#/app/agentProfileCatalog/agentProfileRegistryService';
import { SessionAgentProfileCatalogService } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService';
import { AGENT_PROFILE_SOURCE_PRIORITY } from '#/app/agentProfileCatalog/agentProfileContribution';

import { stubLog } from '../../_base/log/stubs';

const WORKSPACE_KEY = 'wd_a';

function profile(name: string, options?: { readonly enabled?: boolean }): AgentProfile {
  return {
    name,
    enabled: options?.enabled,
    systemPrompt: () => `prompt:${name}`,
  };
}

function makeCatalog(workspaceKey: string = WORKSPACE_KEY) {
  const registry = new AgentProfileRegistryService();
  const catalog = new SessionAgentProfileCatalogService(
    registry,
    { _serviceBrand: undefined, workspaceKey },
    stubLog(),
  );
  return { registry, catalog };
}

describe('SessionAgentProfileCatalogService (registry projection)', () => {
  it('projects global entries and own-workspace entries, filtering other workspace keys', () => {
    const { registry, catalog } = makeCatalog();
    const globalProfile = profile('global-p');
    const ownProfile = profile('own-ws-p');
    registry.register('user', { profiles: [globalProfile] });
    registry.register('workspace', { profiles: [ownProfile] }, { workspaceKey: 'wd_a' });
    registry.register('workspace', { profiles: [profile('other-ws-p')] }, { workspaceKey: 'wd_b' });

    expect(catalog.get('global-p')).toBe(globalProfile);
    expect(catalog.get('own-ws-p')).toBe(ownProfile);
    expect(catalog.get('other-ws-p')).toBeUndefined();
    catalog.dispose();
    registry.dispose();
  });

  it('excludes same-name profiles of other workspace keys from the merge entirely', () => {
    const { registry, catalog } = makeCatalog();
    const userProfile = profile('shared');
    const ownProfile = profile('shared');
    registry.register('user', { profiles: [userProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.user,
    });
    // A higher-priority entry from ANOTHER workspace must not even become a
    // candidate: it neither wins the name nor shows up as suppressed.
    registry.register('workspace', { profiles: [profile('shared')] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.explicit,
      workspaceKey: 'wd_b',
    });
    registry.register('workspace', { profiles: [ownProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      workspaceKey: 'wd_a',
    });

    expect(catalog.get('shared')).toBe(ownProfile);
    expect(catalog.inspect('shared')).toEqual({
      name: 'shared',
      profile: ownProfile,
      sourceId: 'workspace',
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      suppressed: [
        { sourceId: 'user', priority: AGENT_PROFILE_SOURCE_PRIORITY.user, reason: 'priority' },
      ],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('lets the higher-priority source win a name collision and reports the suppressed candidate', () => {
    const { registry, catalog } = makeCatalog();
    const lowProfile = profile('x');
    const highProfile = profile('x');
    registry.register('user', { profiles: [lowProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.user,
    });
    registry.register('workspace', { profiles: [highProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      workspaceKey: 'wd_a',
    });

    expect(catalog.get('x')).toBe(highProfile);
    expect(catalog.inspect('x')).toEqual({
      name: 'x',
      profile: highProfile,
      sourceId: 'workspace',
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      suppressed: [
        { sourceId: 'user', priority: AGENT_PROFILE_SOURCE_PRIORITY.user, reason: 'priority' },
      ],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('lets a same-name file profile replace the essential builtin without any declaration (D14)', () => {
    // Adjudication D14 / ADR-006: a same-name file overrides by virtue of
    // existing — the `override: true` requirement is gone. The displaced
    // builtin definition is recorded as a suppressed candidate on the
    // winner's row (D16 — the same shape a covered non-essential builtin
    // carries).
    const { registry, catalog } = makeCatalog();
    const builtinProfile = profile(DEFAULT_AGENT_PROFILE_NAME);
    const fileProfile = profile(DEFAULT_AGENT_PROFILE_NAME);
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
    });
    registry.register('workspace', { profiles: [fileProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      workspaceKey: 'wd_a',
    });

    expect(catalog.get(DEFAULT_AGENT_PROFILE_NAME)).toBe(fileProfile);
    expect(catalog.inspect(DEFAULT_AGENT_PROFILE_NAME)).toEqual({
      name: DEFAULT_AGENT_PROFILE_NAME,
      profile: fileProfile,
      sourceId: 'workspace',
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      suppressed: [
        {
          sourceId: BUILTIN_AGENT_PROFILE_SOURCE_ID,
          priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
          reason: 'priority',
        },
      ],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('lets a file profile replace a same-name non-essential builtin and records the displaced builtin', () => {
    // A custom file with the same name as a non-essential builtin (e.g.
    // "coder", "explore") replaces it — the displaced builtin definition is
    // the winner's suppressed candidate (D16 isomorphism: the essential and
    // non-essential builtins project the same shape).
    const { registry, catalog } = makeCatalog();
    const builtinCoder = profile('coder');
    const customCoder = profile('coder');
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinCoder] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
    });
    registry.register('workspace', { profiles: [customCoder] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      workspaceKey: 'wd_a',
    });

    expect(catalog.get('coder')).toBe(customCoder);
    expect(catalog.inspect('coder')).toEqual({
      name: 'coder',
      profile: customCoder,
      sourceId: 'workspace',
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
      suppressed: [
        {
          sourceId: BUILTIN_AGENT_PROFILE_SOURCE_ID,
          priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
          reason: 'priority',
        },
      ],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('re-projects and fires the source id on relevant registry changes, ignoring other keys', () => {
    const { registry, catalog } = makeCatalog();
    const seen: string[] = [];
    const subscription = catalog.onDidChange((sourceId) => seen.push(sourceId));

    const ownProfile = profile('own-ws-p');
    registry.register('workspace', { profiles: [ownProfile] }, { workspaceKey: 'wd_a' });
    expect(catalog.get('own-ws-p')).toBe(ownProfile);

    const globalProfile = profile('global-p');
    registry.register('user', { profiles: [globalProfile] });
    expect(catalog.get('global-p')).toBe(globalProfile);

    // Changes tagged with another workspace key are ignored entirely: no
    // re-projection, no event.
    registry.register('workspace', { profiles: [profile('other-ws-p')] }, { workspaceKey: 'wd_b' });
    registry.unregister('workspace', 'wd_b');
    expect(catalog.get('other-ws-p')).toBeUndefined();

    registry.unregister('user');
    expect(catalog.get('global-p')).toBeUndefined();

    expect(seen).toEqual(['workspace', 'user', 'user']);
    subscription.dispose();
    catalog.dispose();
    registry.dispose();
  });

  it("fires 'catalog' on reload", async () => {
    const { registry, catalog } = makeCatalog();
    const seen: string[] = [];
    const subscription = catalog.onDidChange((sourceId) => seen.push(sourceId));

    await catalog.reload();

    expect(seen).toEqual(['catalog']);
    subscription.dispose();
    catalog.dispose();
    registry.dispose();
  });

  it('resolves ready immediately (loader readiness is the workspace handler’s job)', async () => {
    const { registry, catalog } = makeCatalog();

    await expect(catalog.ready).resolves.toBeUndefined();
    await expect(catalog.load()).resolves.toBeUndefined();
    catalog.dispose();
    registry.dispose();
  });

  it('serves the read surface and throws from getDefault without the default profile', () => {
    const { registry, catalog } = makeCatalog();
    expect(catalog.get('missing')).toBeUndefined();
    expect(catalog.inspect('missing')).toBeUndefined();
    expect(catalog.list()).toEqual([]);
    expect(() => catalog.getDefault()).toThrow(
      `Default agent profile "${DEFAULT_AGENT_PROFILE_NAME}" is not registered`,
    );

    const defaultProfile = profile(DEFAULT_AGENT_PROFILE_NAME);
    const coderProfile = profile('coder');
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [defaultProfile] });
    registry.register('user', { profiles: [coderProfile] });

    expect(catalog.getDefault()).toBe(defaultProfile);
    expect(catalog.get('coder')).toBe(coderProfile);
    expect(catalog.list()).toEqual([defaultProfile, coderProfile]);
    expect(catalog.inspect(DEFAULT_AGENT_PROFILE_NAME)).toEqual({
      name: DEFAULT_AGENT_PROFILE_NAME,
      profile: defaultProfile,
      sourceId: BUILTIN_AGENT_PROFILE_SOURCE_ID,
      priority: 0,
      suppressed: [],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('hides a disabled profile from list() and get() while exposing it via listAll() and inspect()', () => {
    const { registry, catalog } = makeCatalog();
    const disabledProfile = profile('reviewer', { enabled: false });
    const enabledProfile = profile('coder');
    registry.register('user', { profiles: [disabledProfile, enabledProfile] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.user,
    });

    expect(catalog.get('reviewer')).toBeUndefined();
    expect(catalog.list().map((p) => p.name)).toEqual(['coder']);
    expect(catalog.listAll().map((p) => p.name)).toEqual(['coder', 'reviewer']);
    expect(catalog.inspect('reviewer')).toMatchObject({
      name: 'reviewer',
      sourceId: 'user',
      disabled: true,
      suppressed: [
        { sourceId: 'user', priority: AGENT_PROFILE_SOURCE_PRIORITY.user, reason: 'disabled' },
      ],
    });
    catalog.dispose();
    registry.dispose();
  });

  it('lets a lower-priority enabled profile win when the higher-priority candidate is disabled', () => {
    const { registry, catalog } = makeCatalog();
    const disabledHigh = profile('shared', { enabled: false });
    const enabledLow = profile('shared');
    registry.register('workspace', { profiles: [disabledHigh] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, workspaceKey: WORKSPACE_KEY,
    });
    registry.register('user', { profiles: [enabledLow] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.user,
    });

    expect(catalog.get('shared')).toBe(enabledLow);
    expect(catalog.inspect('shared')?.suppressed).toContainEqual({
      sourceId: 'workspace', priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, reason: 'disabled',
    });
    catalog.dispose();
    registry.dispose();
  });

  it('ignores enabled: false on the essential default profile and keeps it active', () => {
    const { registry, catalog } = makeCatalog();
    const builtinProfile = profile(DEFAULT_AGENT_PROFILE_NAME);
    const disabledOverride = profile(DEFAULT_AGENT_PROFILE_NAME, {
      enabled: false,
    });
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinProfile] });
    registry.register('workspace', { profiles: [disabledOverride] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, workspaceKey: WORKSPACE_KEY,
    });

    expect(catalog.get(DEFAULT_AGENT_PROFILE_NAME)).toBe(disabledOverride);
    expect(catalog.inspect(DEFAULT_AGENT_PROFILE_NAME)?.disabled).toBeUndefined();
    catalog.dispose();
    registry.dispose();
  });

  it('keeps the builtin profile visible when a same-name disabled file candidate exists', () => {
    const { registry, catalog } = makeCatalog();
    const builtinCoder = profile('coder');
    const disabledCustom = profile('coder', { enabled: false });
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinCoder] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
    });
    registry.register('workspace', { profiles: [disabledCustom] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, workspaceKey: WORKSPACE_KEY,
    });

    expect(catalog.get('coder')).toBe(builtinCoder);
    expect(catalog.inspect('coder')?.suppressed).toContainEqual({
      sourceId: 'workspace', priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, reason: 'disabled',
    });
    catalog.dispose();
    registry.dispose();
  });

  it('keeps the builtin winner in listAll() and inspect() when a same-name candidate is disabled', () => {
    const { registry, catalog } = makeCatalog();
    const builtinCoder = profile('coder');
    const disabledCustom = profile('coder', { enabled: false });
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinCoder] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
    });
    registry.register('workspace', { profiles: [disabledCustom] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, workspaceKey: WORKSPACE_KEY,
    });

    expect(catalog.listAll().find((p) => p.name === 'coder')).toBe(builtinCoder);
    const inspection = catalog.inspect('coder');
    expect(inspection?.disabled).toBeUndefined();
    expect(inspection?.profile).toBe(builtinCoder);
    expect(inspection?.sourceId).toBe(BUILTIN_AGENT_PROFILE_SOURCE_ID);
    expect(inspection?.suppressed).toContainEqual({
      sourceId: 'workspace', priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, reason: 'disabled',
    });
    catalog.dispose();
    registry.dispose();
  });

  it('does not record a disabled winner when the essential-name candidate wins anyway', () => {
    // Post-D14 there is no override-rule suppression: the essential-named
    // candidate wins even with `enabled: false` (the exemption above), so
    // it is the merged winner — never a `disabledWinners` entry.
    const { registry, catalog } = makeCatalog();
    const builtinProfile = profile(DEFAULT_AGENT_PROFILE_NAME);
    const disabledCandidate = profile(DEFAULT_AGENT_PROFILE_NAME, { enabled: false });
    registry.register(BUILTIN_AGENT_PROFILE_SOURCE_ID, { profiles: [builtinProfile] });
    registry.register('workspace', { profiles: [disabledCandidate] }, {
      priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace, workspaceKey: WORKSPACE_KEY,
    });

    expect(catalog.get(DEFAULT_AGENT_PROFILE_NAME)).toBe(disabledCandidate);
    expect(catalog.listAll().find((p) => p.name === DEFAULT_AGENT_PROFILE_NAME)).toBe(disabledCandidate);
    const inspection = catalog.inspect(DEFAULT_AGENT_PROFILE_NAME);
    expect(inspection?.disabled).toBeUndefined();
    expect(inspection?.profile).toBe(disabledCandidate);
    expect(inspection?.suppressed).toEqual([
      {
        sourceId: BUILTIN_AGENT_PROFILE_SOURCE_ID,
        priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
        reason: 'priority',
      },
    ]);
    catalog.dispose();
    registry.dispose();
  });
});
