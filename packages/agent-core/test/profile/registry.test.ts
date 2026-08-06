import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { DEFAULT_AGENT_PROFILES } from '../../src/profile/default';
import type { MirriConfig } from '../../src/config';
import { ProfileRegistry } from '../../src/profile/registry';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'profile-registry-'));
  tempDirs.push(dir);
  return dir;
}

function makeConfig(overrides: Partial<MirriConfig> = {}): MirriConfig {
  return { providers: {}, ...overrides };
}

function writeAgentYaml(dir: string, name: string, content: string): void {
  const agentsDir = join(dir, 'agents');
  mkdirSync(agentsDir, { recursive: true });
  writeFileSync(join(agentsDir, `${name}.yaml`), content, 'utf-8');
}

describe('ProfileRegistry', () => {
  it('should include built-in profiles when no custom profiles exist', async () => {
    const userDir = makeTempDir();
    const workDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir, userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const merged = registry.getMergedProfiles();

    expect(merged['agent']).toBeDefined();
    expect(merged['coder']).toBeDefined();
    expect(merged['explore']).toBeDefined();
    expect(merged['plan']).toBeDefined();
  });

  it('should merge custom profile alongside built-ins', async () => {
    const userDir = makeTempDir();
    writeAgentYaml(userDir, 'reviewer', 'name: reviewer\ndescription: Code review specialist\n');

    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const merged = registry.getMergedProfiles();

    expect(merged['reviewer']).toBeDefined();
    expect(merged['reviewer']?.description).toBe('Code review specialist');
    // Built-ins still present
    expect(merged['agent']).toBeDefined();
  });

  it('should let custom profile override built-in by name', async () => {
    const userDir = makeTempDir();
    writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: My custom coder\ntools:\n  - Read\n  - Grep\n');

    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const merged = registry.getMergedProfiles();

    expect(merged['coder']?.description).toBe('My custom coder');
    expect(merged['coder']?.tools).toEqual(['Read', 'Grep']);
  });

  it('should exclude disabled profiles from getMergedProfiles', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig({ disabledAgents: ['explore'] }),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const merged = registry.getMergedProfiles();

    expect(merged['explore']).toBeUndefined();
    expect(merged['agent']).toBeDefined();
    expect(merged['coder']).toBeDefined();
  });

  it('should still list disabled profiles in listEntries with enabled=false', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig({ disabledAgents: ['explore'] }),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const entries = registry.listEntries();

    const explore = entries.find((e) => e.profile.name === 'explore');
    expect(explore).toBeDefined();
    expect(explore?.enabled).toBe(false);
  });

  it('should never disable essential agent profile', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig({ disabledAgents: ['agent'] }),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const merged = registry.getMergedProfiles();

    expect(merged['agent']).toBeDefined();
    const entry = registry.getEntry('agent');
    expect(entry?.essential).toBe(true);
    expect(entry?.enabled).toBe(true);
  });

  it('should mark built-in non-essential as disableable', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const coderEntry = registry.getEntry('coder');
    expect(coderEntry?.builtin).toBe(true);
    expect(coderEntry?.essential).toBe(false);
  });

  it('should reload when called again after filesystem changes', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    expect(registry.getMergedProfiles()['reviewer']).toBeUndefined();

    writeAgentYaml(userDir, 'reviewer', 'name: reviewer\ndescription: Reviewer agent\n');
    await registry.reload();

    expect(registry.getMergedProfiles()['reviewer']).toBeDefined();
  });

  it('should preserve built-in system prompts after reload', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const agentProfile = registry.getMergedProfiles()['agent'];
    expect(agentProfile).toBeDefined();

    // The system prompt renderer should produce non-empty output
    const prompt = agentProfile!.systemPrompt({
      osEnv: { osKind: 'macOS', shellName: 'bash', shellPath: '/bin/bash' } as any,
      cwd: '/tmp',
    });
    expect(prompt.length).toBeGreaterThan(0);
  });

  it('should preserve subagent declarations on agent profile', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig(),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    const agentProfile = registry.getMergedProfiles()['agent'];
    expect(agentProfile?.subagents).toBeDefined();
    expect(agentProfile?.subagents?.['coder']).toBeDefined();
    expect(agentProfile?.subagents?.['explore']).toBeDefined();
    expect(agentProfile?.subagents?.['plan']).toBeDefined();
  });

  it('should return undefined for getProfile when profile is disabled', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig({ disabledAgents: ['coder'] }),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    expect(registry.getProfile('coder')).toBeUndefined();
    expect(registry.getProfile('agent')).toBeDefined();
  });

  it('should ignore disabledAgents referencing non-existent names', async () => {
    const userDir = makeTempDir();
    const registry = new ProfileRegistry(
      () => makeConfig({ disabledAgents: ['nonexistent'] }),
      { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    );

    await registry.reload();
    // Should not throw, should not affect other profiles
    expect(registry.getProfile('agent')).toBeDefined();
  });

  describe('built-in model override via partial-merge', () => {
    it('should override only defaultModel when custom YAML matches built-in name', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const coder = registry.getProfile('coder');
      // defaultModel overridden
      expect(coder?.defaultModel).toBe('gpt-4o');
      // Other fields inherited from built-in
      expect(coder?.tools.length).toBeGreaterThan(0);
      expect(coder?.whenToUse).toBeDefined();
    });

    it('should preserve built-in tools when only defaultModel is overridden', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'explore', 'name: explore\ndescription: Explore override\ndefaultModel: gpt-4o-mini\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const explore = registry.getProfile('explore');
      expect(explore?.defaultModel).toBe('gpt-4o-mini');
      // Explore's built-in tools should be preserved
      expect(explore?.tools).toContain('Grep');
      expect(explore?.tools).toContain('Glob');
      expect(explore?.capabilitiesRequired).toBeDefined();
    });

    it('should mark overridden built-in as builtin with hasOverride=true', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const entry = registry.getEntry('coder');
      expect(entry?.builtin).toBe(true);
      expect(entry?.essential).toBe(false);
      expect(entry?.hasOverride).toBe(true);
    });

    it('should use defaultModel from partial override in available subagents', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ndefaultModel: kimi-for-coding\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const subagents = registry.getAvailableSubagents();
      expect(subagents['coder']?.defaultModel).toBe('kimi-for-coding');
    });

    it('should merge non-defaultModel fields in partial override while preserving others', async () => {
      const userDir = makeTempDir();
      // Override only tools, keep defaultModel from built-in (undefined) and whenToUse
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ntools:\n  - Read\n  - Grep\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const coder = registry.getProfile('coder');
      // tools overridden to the custom set
      expect(coder?.tools).toEqual(['Read', 'Grep']);
      // whenToUse preserved from built-in
      expect(coder?.whenToUse).toBeDefined();
      expect(coder?.whenToUse?.length).toBeGreaterThan(0);
    });

    it('should not override essential agent via direct YAML file', async () => {
      const userDir = makeTempDir();
      // Try to override the essential 'agent' profile
      writeAgentYaml(userDir, 'agent', 'name: agent\ndescription: Agent override\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const agent = registry.getProfile('agent');
      // The override should NOT take effect — essential agent is protected
      expect(agent?.defaultModel).toBeUndefined();
      // Agent should still have its built-in tools and system prompt
      expect(agent?.tools.length).toBeGreaterThan(0);
      expect(agent?.subagents).toBeDefined();
    });
  });

  describe('getAvailableSubagents', () => {
    it('should return declared subagents plus custom agents when all enabled', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'reviewer', 'name: reviewer\ndescription: Code review specialist\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const subagents = registry.getAvailableSubagents();

      // Declared subagents from agent.yaml
      expect(subagents['coder']).toBeDefined();
      expect(subagents['explore']).toBeDefined();
      expect(subagents['plan']).toBeDefined();
      // Custom agent included
      expect(subagents['reviewer']).toBeDefined();
      // Agent itself is NOT in the result
      expect(subagents['agent']).toBeUndefined();
    });

    it('should exclude disabled built-in subagents', async () => {
      const userDir = makeTempDir();
      const registry = new ProfileRegistry(
        () => makeConfig({ disabledAgents: ['coder'] }),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const subagents = registry.getAvailableSubagents();

      expect(subagents['coder']).toBeUndefined();
      expect(subagents['explore']).toBeDefined();
      expect(subagents['plan']).toBeDefined();
    });

    it('should exclude disabled custom agents', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'reviewer', 'name: reviewer\ndescription: Reviewer agent\n');

      const registry = new ProfileRegistry(
        () => makeConfig({ disabledAgents: ['reviewer'] }),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const subagents = registry.getAvailableSubagents();

      expect(subagents['reviewer']).toBeUndefined();
      expect(subagents['coder']).toBeDefined();
    });

    it('should include custom agents not declared in agent.yaml subagents', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'my-coder', 'name: my-coder\ndescription: My custom coder\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const subagents = registry.getAvailableSubagents();

      // Built-in subagents
      expect(subagents['coder']).toBeDefined();
      expect(subagents['explore']).toBeDefined();
      expect(subagents['plan']).toBeDefined();
      // Custom agent included even though not in agent.yaml's subagents
      expect(subagents['my-coder']).toBeDefined();
      expect(subagents['my-coder']?.defaultModel).toBe('gpt-4o');
    });

    it('should reflect enable/disable toggle immediately after reload', async () => {
      const userDir = makeTempDir();
      // Mutable config so we can simulate toggling without recreating the registry.
      const config = makeConfig();
      const registry = new ProfileRegistry(
        () => config,
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      // Initially all subagents are enabled.
      await registry.reload();
      expect(registry.getAvailableSubagents()['explore']).toBeDefined();

      // Simulate disabling explore via config change + reload.
      config.disabledAgents = ['explore'];
      await registry.reload();
      expect(registry.getAvailableSubagents()['explore']).toBeUndefined();
      // Other subagents unaffected.
      expect(registry.getAvailableSubagents()['coder']).toBeDefined();
      expect(registry.getAvailableSubagents()['plan']).toBeDefined();

      // Simulate re-enabling explore.
      config.disabledAgents = [];
      await registry.reload();
      expect(registry.getAvailableSubagents()['explore']).toBeDefined();
    });
  });

  describe('built-in override field propagation', () => {
    /** Minimal SystemPromptContext for renderer tests. */
    const fakeContext = {
      osEnv: { osKind: 'test-os', osArch: 'x64', osVersion: '1.0', shellName: 'bash' as const, shellPath: '/bin/bash' },
      cwd: '/test',
    };

    it('should reflect overridden description and whenToUse in resolved profile', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(
        userDir,
        'coder',
        'name: coder\ndescription: My overridden coder\nwhenToUse: Use for custom tasks\n',
      );

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const merged = registry.getMergedProfiles();

      expect(merged['coder']?.description).toBe('My overridden coder');
      expect(merged['coder']?.whenToUse).toBe('Use for custom tasks');
      // Built-in tools preserved (not overridden)
      expect(merged['coder']?.tools.length).toBeGreaterThan(0);
    });

    it('should reflect overridden tools in resolved profile', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(
        userDir,
        'coder',
        'name: coder\ndescription: Coder override\ntools:\n  - Read\n  - Write\n',
      );

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const merged = registry.getMergedProfiles();

      expect(merged['coder']?.tools).toEqual(['Read', 'Write']);
    });

    it('should reflect overridden systemPromptTemplate in rendered output', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(
        userDir,
        'coder',
        'name: coder\ndescription: Coder override\nsystemPromptTemplate: "CUSTOM_PROMPT: {{MIRRICODE_OS}}"\n',
      );

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const coder = registry.getMergedProfiles()['coder'];
      expect(coder).toBeDefined();

      const rendered = coder!.systemPrompt(fakeContext);
      expect(rendered).toContain('CUSTOM_PROMPT:');
      expect(rendered).toContain('test-os');
    });

    it('should keep builtin=true and hasOverride=true for overridden built-in', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const entry = registry.getEntry('coder');

      expect(entry?.builtin).toBe(true);
      expect(entry?.hasOverride).toBe(true);
    });

    it('should preserve built-in systemPromptTemplate when override does not declare it', async () => {
      const userDir = makeTempDir();
      writeAgentYaml(userDir, 'coder', 'name: coder\ndescription: Coder override\ndefaultModel: gpt-4o\n');

      const registry = new ProfileRegistry(
        () => makeConfig(),
        { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
      );

      await registry.reload();
      const coder = registry.getMergedProfiles()['coder'];
      expect(coder).toBeDefined();

      // defaultModel overridden
      expect(coder?.defaultModel).toBe('gpt-4o');
      // systemPromptTemplate still from built-in — renders non-empty
      const rendered = coder!.systemPrompt(fakeContext);
      expect(rendered.length).toBeGreaterThan(0);
    });
  });
});
