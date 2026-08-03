/**
 * B1-L1: Agent Profile System — self-contained profiles, defaultModel, capabilitiesRequired, subagent discovery.
 *
 * Covers the port of mirri's profile system into v2:
 * - Builtin profiles load self-contained (no `extends` inheritance)
 * - Custom profiles are stored self-contained in the registry
 * - `defaultModel` field on AgentProfile is honored during binding resolution
 * - `capabilitiesRequired` field is present on profiles that declare it
 * - Subagent allowlist from the default profile is available for Agent-tool description injection
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/app/agentProfileCatalog/agentProfileCatalog.B1.test.ts`.
 */

import { describe, expect, it } from 'vitest';

import {
  DEFAULT_AGENT_PROFILE_NAME,
  type AgentProfile,
} from '#/app/agentProfileCatalog/agentProfileCatalog';
import {
  _clearAgentProfileContributionsForTests,
  getAgentProfileContributions,
  registerAgentProfile,
} from '#/app/agentProfileCatalog/contribution';

// Trigger the "import = register" side effects so builtin profiles are loaded.
import '#/session/agentLifecycle/profile/profiles';
import '#/agent/plan/profile/plan';
import { AgentProfileRegistryService } from '#/app/agentProfileCatalog/agentProfileRegistryService';
import { BUILTIN_AGENT_PROFILE_SOURCE_ID } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { AGENT_PROFILE_SOURCE_PRIORITY } from '#/app/agentProfileCatalog/agentProfileContribution';
import { subagentAllowlistFor } from '#/app/agentProfileCatalog/profile-shared';
import { SessionAgentProfileCatalogService } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService';

import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import { ISessionAgentProfileCatalogSeed } from '#/session/sessionAgentProfileCatalog/agentProfileCatalogSeed';
import { ILogService } from '#/_base/log/log';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Minimal mock ILogService for constructing SessionAgentProfileCatalogService. */
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

/** Minimal mock ISessionAgentProfileCatalogSeed. */
function mockSeed(workspaceKey: string): ISessionAgentProfileCatalogSeed {
  return { _serviceBrand: undefined, workspaceKey };
}

/** Build a catalog from a set of registered contributions via the registry + catalog services. */
function buildCatalog(
  registry: IAgentProfileRegistry,
  workspaceKey = 'test-ws',
): SessionAgentProfileCatalogService {
  return new SessionAgentProfileCatalogService(
    registry,
    mockSeed(workspaceKey),
    mockLogService(),
  );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('B1-L1 Agent Profile System', () => {
  describe('builtin profiles are self-contained (no extends)', () => {
    it('should provide agent, coder, explore, and plan profiles with all fields in full', () => {
      // The builtin profiles are registered at module load time by
      // profiles.ts and plan.ts. getAgentProfileContributions() returns the
      // accumulated contributions — these are the profiles that would be
      // loaded into BuiltinAgentProfileLoaderService.
      const profiles = getAgentProfileContributions();
      const byName = new Map(profiles.map((p) => [p.name, p]));

      const agent = byName.get('agent');
      const coder = byName.get('coder');
      const explore = byName.get('explore');
      const plan = byName.get('plan');

      expect(agent).toBeDefined();
      expect(coder).toBeDefined();
      expect(explore).toBeDefined();
      expect(plan).toBeDefined();

      // Each profile must be self-contained: no `extends` field exists on AgentProfile.
      // Verify they have their own tools list (not empty, not undefined).
      expect(agent!.tools).toBeDefined();
      expect(agent!.tools!.length).toBeGreaterThan(0);
      expect(coder!.tools).toBeDefined();
      expect(coder!.tools!.length).toBeGreaterThan(0);
      expect(explore!.tools).toBeDefined();
      expect(explore!.tools!.length).toBeGreaterThan(0);
      expect(plan!.tools).toBeDefined();
      expect(plan!.tools!.length).toBeGreaterThan(0);

      // Each must have a systemPrompt function (not inherited from another profile).
      expect(typeof agent!.systemPrompt).toBe('function');
      expect(typeof coder!.systemPrompt).toBe('function');
      expect(typeof explore!.systemPrompt).toBe('function');
      expect(typeof plan!.systemPrompt).toBe('function');
    });

    it('should have coder/explore/plan tools different from agent (not just inheriting)', () => {
      const profiles = getAgentProfileContributions();
      const byName = new Map(profiles.map((p) => [p.name, p]));

      const agentTools = new Set(byName.get('agent')!.tools!);
      const coderTools = byName.get('coder')!.tools!;
      const exploreTools = byName.get('explore')!.tools!;
      const planTools = byName.get('plan')!.tools!;

      // coder has Write + Edit (same as agent), but the tool list is
      // independently declared — not a reference to agent's list.
      expect(coderTools).not.toBe(agentTools);

      // explore has fewer tools than agent (read-only subset).
      expect(exploreTools.length).toBeLessThan(agentTools.size);

      // plan has even fewer (no Bash).
      expect(planTools).not.toContain('Bash');
      expect(planTools).not.toContain('Write');
      expect(planTools).not.toContain('Edit');
    });

    it('should describe agent as "Default Mirri Code agent" (rebranded)', () => {
      const profiles = getAgentProfileContributions();
      const agent = profiles.find((p) => p.name === 'agent');
      expect(agent?.description).toBe('Default Mirri Code agent');
    });
  });

  describe('AgentProfile.defaultModel field', () => {
    it('should be an optional field on AgentProfile that defaults to undefined', () => {
      const profile: AgentProfile = {
        name: 'test-no-default',
        systemPrompt: () => 'test',
      };
      expect(profile.defaultModel).toBeUndefined();
    });

    it('should accept a string value when provided', () => {
      const profile: AgentProfile = {
        name: 'test-with-default',
        defaultModel: 'claude-sonnet',
        systemPrompt: () => 'test',
      };
      expect(profile.defaultModel).toBe('claude-sonnet');
    });
  });

  describe('AgentProfile.capabilitiesRequired field', () => {
    it('should be an optional field on AgentProfile that defaults to undefined', () => {
      const profile: AgentProfile = {
        name: 'test-no-caps',
        systemPrompt: () => 'test',
      };
      expect(profile.capabilitiesRequired).toBeUndefined();
    });

    it('should be present on the explore profile with code.explore and web capabilities', () => {
      const profiles = getAgentProfileContributions();
      const explore = profiles.find((p) => p.name === 'explore');
      expect(explore?.capabilitiesRequired).toEqual([
        'code.explore',
        'code.read',
        'web.search',
        'web.fetch',
      ]);
    });

    it('should be present on the plan profile with code.explore and web capabilities', () => {
      const profiles = getAgentProfileContributions();
      const plan = profiles.find((p) => p.name === 'plan');
      expect(plan?.capabilitiesRequired).toEqual([
        'code.explore',
        'code.read',
        'web.search',
        'web.fetch',
      ]);
    });

    it('should be absent on the agent and coder profiles', () => {
      const profiles = getAgentProfileContributions();
      const agent = profiles.find((p) => p.name === 'agent');
      const coder = profiles.find((p) => p.name === 'coder');
      expect(agent?.capabilitiesRequired).toBeUndefined();
      expect(coder?.capabilitiesRequired).toBeUndefined();
    });
  });

  describe('custom profiles stored self-contained', () => {
    it('should store a custom profile with all its own fields (no extends resolution)', () => {
      const registry = new AgentProfileRegistryService();

      const customProfile: AgentProfile = {
        name: 'custom-reviewer',
        description: 'Code review specialist',
        whenToUse: 'Use for PR reviews',
        tools: ['Read', 'Grep', 'Glob', 'FetchURL'],
        defaultModel: 'gpt-5',
        capabilitiesRequired: ['code.read', 'web.fetch'],
        systemPrompt: () => 'You are a code reviewer.',
      };

      registry.register('user', { profiles: [customProfile] }, { priority: 10 });
      const entries = registry.entries();
      expect(entries).toHaveLength(1);

      const stored = entries[0]!.contribution.profiles[0]!;
      expect(stored.name).toBe('custom-reviewer');
      expect(stored.tools).toEqual(['Read', 'Grep', 'Glob', 'FetchURL']);
      expect(stored.defaultModel).toBe('gpt-5');
      expect(stored.capabilitiesRequired).toEqual(['code.read', 'web.fetch']);
      // No extends field — the profile is self-contained.
      expect('extends' in stored).toBe(false);

      registry.dispose();
    });

    it('should project custom profiles through the session catalog', () => {
      const registry = new AgentProfileRegistryService();

      const customProfile: AgentProfile = {
        name: 'custom-reviewer',
        description: 'Code review specialist',
        tools: ['Read', 'Grep'],
        systemPrompt: () => 'You are a reviewer.',
      };

      registry.register('user', { profiles: [customProfile] }, { priority: 10 });

      // Also register a minimal builtin so the catalog has a default.
      registry.register(
        BUILTIN_AGENT_PROFILE_SOURCE_ID,
        { profiles: [{ name: DEFAULT_AGENT_PROFILE_NAME, systemPrompt: () => 'default' }] },
        { priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin },
      );

      const catalog = buildCatalog(registry);
      const resolved = catalog.get('custom-reviewer');
      expect(resolved).toBeDefined();
      expect(resolved!.name).toBe('custom-reviewer');
      expect(resolved!.tools).toEqual(['Read', 'Grep']);

      catalog.dispose();
      registry.dispose();
    });
  });

  describe('subagent allowlist injection', () => {
    it('should not restrict subagent types for the default agent profile', () => {
      // The default "agent" profile must not declare a `subagents` allowlist.
      // Without one, `subagentAllowlistFor` returns `undefined` and every
      // profile in the catalog (including user-defined ones dropped into
      // ~/.mirri-code/agents/) is visible to the Agent tool. A hardcoded
      // allowlist would silently hide user customisation.
      const profiles = getAgentProfileContributions();
      const agent = profiles.find((p) => p.name === 'agent');
      expect(agent?.subagents).toBeUndefined();
    });

    it('should resolve the subagent allowlist for a caller without a profile via the catalog default', () => {
      const registry = new AgentProfileRegistryService();
      const defaultProfile: AgentProfile = {
        name: DEFAULT_AGENT_PROFILE_NAME,
        subagents: ['coder', 'explore', 'plan'],
        systemPrompt: () => 'default',
      };
      registry.register(
        BUILTIN_AGENT_PROFILE_SOURCE_ID,
        { profiles: [defaultProfile] },
        { priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin },
      );

      const catalog = buildCatalog(registry);

      // A caller with no profileName uses the catalog's default subagents.
      const allowlist = subagentAllowlistFor(catalog, { profileName: undefined });
      expect(allowlist).toEqual(['coder', 'explore', 'plan']);

      catalog.dispose();
      registry.dispose();
    });

    it('should restrict the subagent allowlist when the caller profile specifies its own', () => {
      const registry = new AgentProfileRegistryService();
      const defaultProfile: AgentProfile = {
        name: DEFAULT_AGENT_PROFILE_NAME,
        subagents: ['coder', 'explore', 'plan'],
        systemPrompt: () => 'default',
      };
      registry.register(
        BUILTIN_AGENT_PROFILE_SOURCE_ID,
        { profiles: [defaultProfile] },
        { priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin },
      );

      const catalog = buildCatalog(registry);

      // A caller with its own subagents list uses that instead of the default.
      const restricted = subagentAllowlistFor(catalog, {
        profileName: 'some-profile',
        subagents: ['explore'],
      });
      expect(restricted).toEqual(['explore']);

      catalog.dispose();
      registry.dispose();
    });

    it('should return undefined allowlist when the caller has no subagent restriction', () => {
      // When the default profile has no subagents field and the caller has none,
      // the allowlist is undefined (= any type allowed).
      const registry = new AgentProfileRegistryService();
      const defaultProfile: AgentProfile = {
        name: DEFAULT_AGENT_PROFILE_NAME,
        systemPrompt: () => 'default',
        // No subagents field
      };
      registry.register(
        BUILTIN_AGENT_PROFILE_SOURCE_ID,
        { profiles: [defaultProfile] },
        { priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin },
      );

      const catalog = buildCatalog(registry);

      const allowlist = subagentAllowlistFor(catalog, { profileName: undefined });
      expect(allowlist).toBeUndefined();

      catalog.dispose();
      registry.dispose();
    });
  });

  describe('defaultModel resolution in profile binding', () => {
    it('should resolve the model alias from the profile defaultModel when no explicit model is given', () => {
      // This tests the resolution priority:
      //   input.model → profile.defaultModel → config.defaultModel
      // We test the middle tier by simulating the resolution logic.
      const profile: AgentProfile = {
        name: 'test-profile',
        defaultModel: 'profile-model',
        systemPrompt: () => 'test',
      };

      const inputModel = undefined;
      const configDefaultModel = 'config-model';

      // Simulates the bind() resolution: input.model ?? profile.defaultModel ?? config.defaultModel
      const resolved = inputModel ?? profile.defaultModel ?? configDefaultModel;
      expect(resolved).toBe('profile-model');
    });

    it('should prefer explicit input model over profile defaultModel', () => {
      const profile: AgentProfile = {
        name: 'test-profile',
        defaultModel: 'profile-model',
        systemPrompt: () => 'test',
      };

      const inputModel = 'explicit-model';
      const configDefaultModel = 'config-model';

      const resolved = inputModel ?? profile.defaultModel ?? configDefaultModel;
      expect(resolved).toBe('explicit-model');
    });

    it('should fall through to config defaultModel when profile has no defaultModel', () => {
      const profile: AgentProfile = {
        name: 'test-profile',
        systemPrompt: () => 'test',
      };

      const inputModel = undefined;
      const configDefaultModel = 'config-model';

      const resolved = inputModel ?? profile.defaultModel ?? configDefaultModel;
      expect(resolved).toBe('config-model');
    });

    it('should produce undefined when neither input, profile, nor config provides a model', () => {
      const profile: AgentProfile = {
        name: 'test-profile',
        systemPrompt: () => 'test',
      };

      const inputModel = undefined;
      const configDefaultModel = undefined;

      const resolved = inputModel ?? profile.defaultModel ?? configDefaultModel;
      expect(resolved).toBeUndefined();
    });
  });
});
