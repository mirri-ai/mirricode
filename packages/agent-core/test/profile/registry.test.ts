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
    writeAgentYaml(userDir, 'reviewer', 'name: reviewer\nextends: agent\ndescription: Code review specialist\n');

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
    writeAgentYaml(userDir, 'coder', 'name: coder\nextends: agent\ndescription: My custom coder\ntools:\n  - Read\n  - Grep\n');

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

    writeAgentYaml(userDir, 'reviewer', 'name: reviewer\nextends: agent\n');
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
});
