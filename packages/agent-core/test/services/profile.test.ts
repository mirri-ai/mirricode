import { mkdtempSync, existsSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';

import { afterEach, describe, expect, it, vi } from 'vitest';

import { ProfileService } from '../../src/services/profile/profileService';
import { ProfileRegistry } from '../../src/profile/registry';
import type { MirriConfig } from '../../src/config';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'profile-service-'));
  tempDirs.push(dir);
  return dir;
}

function makeConfig(overrides: Partial<MirriConfig> = {}): MirriConfig {
  return { providers: {}, ...overrides };
}

/**
 * Create a mock ICoreProcessService + IEventService pair for ProfileService tests.
 * The mock core exposes a mutable config (so enable/disable can write back),
 * the ProfileRegistry, and homeDir.
 */
function makeService(homeDir: string): { service: ProfileService; registry: ProfileRegistry; publishedEvents: unknown[] } {
  let config = makeConfig();
  const registry = new ProfileRegistry(
    () => config,
    { brandHomeDir: homeDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
  );

  const publishedEvents: unknown[] = [];
  const listeners: Array<(event: unknown) => void> = [];

  const mockCore = {
    profileRegistry: registry,
    homeDir,
    rpc: {
      getMirriConfig: async () => config,
      setMirriConfig: async (patch: Record<string, unknown>) => {
        config = { ...config, ...patch } as MirriConfig;
        return config;
      },
    },
  };

  const mockEventService = {
    publish: (event: unknown) => {
      publishedEvents.push(event);
      for (const listener of listeners) {
        listener(event);
      }
    },
    onDidPublish: (handler: (event: unknown) => void) => ({
      dispose: () => {
        const idx = listeners.indexOf(handler);
        if (idx >= 0) listeners.splice(idx, 1);
      },
    }),
  };

  const service = new ProfileService(
    mockCore as any,
    mockEventService as any,
  );

  return { service, registry, publishedEvents };
}

describe('ProfileService', () => {
  it('should list all profiles including built-ins', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    const entries = await service.listProfiles();
    const names = entries.map((e) => e.name);
    expect(names).toContain('agent');
    expect(names).toContain('coder');
    expect(names).toContain('explore');
    expect(names).toContain('plan');
  });

  it('should create a custom profile and persist to disk', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await service.createProfile({
      name: 'reviewer',
      extends: 'agent',
      description: 'Code review specialist',
    });

    const filePath = join(homeDir, 'agents', 'reviewer.yaml');
    expect(existsSync(filePath)).toBe(true);

    const entries = await service.listProfiles();
    expect(entries.find((e) => e.name === 'reviewer')).toBeDefined();

    const merged = registry.getMergedProfiles();
    expect(merged['reviewer']).toBeDefined();
  });

  it('should reject creating a profile with existing name', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await expect(
      service.createProfile({ name: 'coder', extends: 'agent' }),
    ).rejects.toThrow();
  });

  it('should update a custom profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await service.createProfile({
      name: 'reviewer',
      extends: 'agent',
      description: 'Original description',
    });

    await service.updateProfile('reviewer', {
      description: 'Updated description',
    });

    const entry = await service.getProfile('reviewer');
    expect(entry?.description).toBe('Updated description');
  });

  it('should allow updating a built-in profile (creates override)', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    const updated = await service.updateProfile('coder', { description: 'my coder' });
    expect(updated.description).toBe('my coder');
    // Override file created
    expect(existsSync(join(homeDir, 'agents', 'coder.yaml'))).toBe(true);
  });

  it('should delete a custom profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await service.createProfile({ name: 'reviewer', extends: 'agent' });
    await service.deleteProfile('reviewer');

    const entries = await service.listProfiles();
    expect(entries.find((e) => e.name === 'reviewer')).toBeUndefined();
  });

  it('should reject deleting built-in profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await expect(service.deleteProfile('coder')).rejects.toThrow();
  });

  it('should disable a non-essential built-in profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await service.disableProfile('explore');

    const entries = await service.listProfiles();
    const explore = entries.find((e) => e.name === 'explore');
    expect(explore?.enabled).toBe(false);
    expect(registry.getMergedProfiles()['explore']).toBeUndefined();
  });

  it('should reject disabling essential agent profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await expect(service.disableProfile('agent')).rejects.toThrow();
  });

  it('should enable a previously disabled profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await service.disableProfile('explore');
    await service.enableProfile('explore');

    const entries = await service.listProfiles();
    const explore = entries.find((e) => e.name === 'explore');
    expect(explore?.enabled).toBe(true);
    expect(registry.getMergedProfiles()['explore']).toBeDefined();
  });

  describe('builtin override via updateProfile', () => {
    it('should override fields on a built-in agent via updateProfile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.updateProfile('coder', { defaultModel: 'gpt-4o' });

      // Override file created on disk
      const filePath = join(homeDir, 'agents', 'coder.yaml');
      expect(existsSync(filePath)).toBe(true);

      // Registry reflects the override
      await registry.reload();
      const coder = registry.getProfile('coder');
      expect(coder?.defaultModel).toBe('gpt-4o');
      // Other fields preserved (built-in tools still present)
      expect(coder?.tools.length).toBeGreaterThan(0);
      // Profile is still marked as built-in
      const entry = registry.getEntry('coder');
      expect(entry?.builtin).toBe(true);
      expect(entry?.hasOverride).toBe(true);
    });

    it('should update an existing override for a built-in agent', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.updateProfile('coder', { defaultModel: 'gpt-4o' });
      await service.updateProfile('coder', { defaultModel: 'claude-sonnet-4' });

      await registry.reload();
      const coder = registry.getProfile('coder');
      expect(coder?.defaultModel).toBe('claude-sonnet-4');
    });

    it('should reject updateProfile for essential agent', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await expect(
        service.updateProfile('agent', { defaultModel: 'gpt-4o' }),
      ).rejects.toThrow();
    });

    it('should reject updateProfile for non-existent profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await expect(
        service.updateProfile('nonexistent', { defaultModel: 'gpt-4o' }),
      ).rejects.toThrow();
    });
  });

  describe('resetBuiltinOverride', () => {
    it('should remove override file and restore defaults', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.updateProfile('coder', { defaultModel: 'gpt-4o' });
      const filePath = join(homeDir, 'agents', 'coder.yaml');
      expect(existsSync(filePath)).toBe(true);

      await service.resetBuiltinOverride('coder');

      expect(existsSync(filePath)).toBe(false);

      await registry.reload();
      const coder = registry.getProfile('coder');
      expect(coder?.defaultModel).toBeUndefined();
      const entry = registry.getEntry('coder');
      expect(entry?.hasOverride).toBe(false);
    });

    it('should reject reset for non-built-in profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();
      await service.createProfile({ name: 'reviewer', extends: 'agent' });

      await expect(
        service.resetBuiltinOverride('reviewer'),
      ).rejects.toThrow();
    });

    it('should be a no-op if no override exists', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.resetBuiltinOverride('coder');

      const coder = registry.getProfile('coder');
      expect(coder).toBeDefined();
    });
  });

  describe('profiles_changed event', () => {
    it('should publish profiles_changed event when creating a profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry, publishedEvents } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({ name: 'reviewer', extends: 'agent' });

      expect(publishedEvents).toContainEqual(
        expect.objectContaining({ type: 'event.agent.profiles_changed' }),
      );
    });

    it('should publish profiles_changed event when updating a built-in profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry, publishedEvents } = makeService(homeDir);
      await registry.reload();

      await service.updateProfile('coder', { defaultModel: 'gpt-4o' });

      expect(publishedEvents).toContainEqual(
        expect.objectContaining({ type: 'event.agent.profiles_changed' }),
      );
    });

    it('should publish profiles_changed event when deleting a profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry, publishedEvents } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({ name: 'reviewer', extends: 'agent' });
      publishedEvents.length = 0; // reset
      await service.deleteProfile('reviewer');

      expect(publishedEvents).toContainEqual(
        expect.objectContaining({ type: 'event.agent.profiles_changed' }),
      );
    });

    it('should publish profiles_changed event when disabling a profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry, publishedEvents } = makeService(homeDir);
      await registry.reload();

      await service.disableProfile('explore');

      expect(publishedEvents).toContainEqual(
        expect.objectContaining({ type: 'event.agent.profiles_changed' }),
      );
    });
  });

  describe('format-aware save', () => {
    it('should create a new profile as .yaml by default', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Code reviewer',
        systemPromptTemplate: 'You are a code reviewer.',
      });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });

    it('should write .yaml with systemPromptTemplate field when creating with a system prompt', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Code reviewer',
        systemPromptTemplate: 'You review code.',
      });

      const { readFile } = await import('node:fs/promises');
      const content = await readFile(join(homeDir, 'agents', 'reviewer.yaml'), 'utf-8');
      // YAML format, no frontmatter fence
      expect(content.startsWith('---\n')).toBe(false);
      // systemPromptTemplate present as a YAML field
      expect(content).toContain('systemPromptTemplate: You review code.');
    });

    it('should preserve .yaml format when updating an existing .yaml profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      // Manually create a .yaml profile file
      const { mkdir, writeFile } = await import('node:fs/promises');
      await mkdir(join(homeDir, 'agents'), { recursive: true });
      await writeFile(
        join(homeDir, 'agents', 'reviewer.yaml'),
        'name: reviewer\nextends: agent\ndescription: Original\n',
        'utf-8',
      );
      await registry.reload();

      await service.updateProfile('reviewer', { description: 'Updated' });

      // File should still be .yaml, not .md
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });

    it('should preserve .md format when updating an existing .md profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);

      // Manually create a .md profile file
      const { mkdir, writeFile } = await import('node:fs/promises');
      await mkdir(join(homeDir, 'agents'), { recursive: true });
      await writeFile(
        join(homeDir, 'agents', 'reviewer.md'),
        '---\nname: reviewer\nextends: agent\ndescription: Original\n---\nYou review code.',
        'utf-8',
      );
      await registry.reload();

      await service.updateProfile('reviewer', { description: 'Updated' });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(false);

      const entry = await service.getProfile('reviewer');
      expect(entry?.description).toBe('Updated');
    });

    it('should round-trip save → load with consistent data', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Code reviewer',
        systemPromptTemplate: 'You review code.',
        defaultModel: 'gpt-4o',
      });

      await registry.reload();
      const entry = await service.getProfile('reviewer');
      expect(entry?.description).toBe('Code reviewer');
      expect(entry?.systemPromptTemplate).toBe('You review code.');
      expect(entry?.defaultModel).toBe('gpt-4o');
    });

    it('should remove existing .md when creating a .yaml profile with the same name', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);

      // Manually create a stale .md file
      const { mkdir, writeFile } = await import('node:fs/promises');
      await mkdir(join(homeDir, 'agents'), { recursive: true });
      await writeFile(
        join(homeDir, 'agents', 'reviewer.md'),
        '---\nname: reviewer\nextends: agent\ndescription: Old\n---\nbody',
        'utf-8',
      );

      // Delete via service (so registry knows it's gone), then recreate
      await registry.reload();
      await service.deleteProfile('reviewer');

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'New reviewer',
      });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });

    it('should not touch unrelated agent files when creating a profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({ name: 'reviewer', extends: 'agent' });
      await service.createProfile({ name: 'linter', extends: 'agent' });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'linter.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
      expect(existsSync(join(homeDir, 'agents', 'linter.md'))).toBe(false);
    });

    it('should remove stale .md when updating a .yaml profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);

      // Create the profile as .yaml (default)
      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Original',
      });

      // Manually create a stale .md with the same name
      const { writeFile } = await import('node:fs/promises');
      await writeFile(
        join(homeDir, 'agents', 'reviewer.md'),
        '---\nname: reviewer\nextends: agent\ndescription: Stale\n---\nbody',
        'utf-8',
      );

      await service.updateProfile('reviewer', { description: 'Updated' });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });

    it('should return updated defaultModel after save and reload', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Code reviewer',
        defaultModel: 'gpt-4o',
      });

      await service.updateProfile('reviewer', { defaultModel: 'claude-sonnet-4' });

      await registry.reload();
      const entry = await service.getProfile('reviewer');
      expect(entry?.defaultModel).toBe('claude-sonnet-4');
    });

    it('should return updated defaultModel after save and reload with .yaml source', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);

      // Manually create a .yaml profile
      const { mkdir, writeFile } = await import('node:fs/promises');
      await mkdir(join(homeDir, 'agents'), { recursive: true });
      await writeFile(
        join(homeDir, 'agents', 'reviewer.yaml'),
        'name: reviewer\nextends: agent\ndescription: Code reviewer\ndefaultModel: gpt-4o\n',
        'utf-8',
      );
      await registry.reload();

      await service.updateProfile('reviewer', { defaultModel: 'claude-sonnet-4' });

      await registry.reload();
      const entry = await service.getProfile('reviewer');
      expect(entry?.defaultModel).toBe('claude-sonnet-4');
    });

    it('should not create duplicate files when updating an existing profile', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Original',
      });
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);

      await service.updateProfile('reviewer', { description: 'Updated' });

      // Only .yaml should exist, no .md duplicate
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });

    it('should clean up stale format file after update when both .md and .yaml exist', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);

      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Original',
        defaultModel: 'gpt-4o',
      });

      // Simulate a stale .md appearing (e.g. from a previous format)
      const { writeFile } = await import('node:fs/promises');
      await writeFile(
        join(homeDir, 'agents', 'reviewer.md'),
        '---\nname: reviewer\nextends: agent\ndescription: Stale\ndefaultModel: old-model\n---\nbody',
        'utf-8',
      );

      await service.updateProfile('reviewer', { defaultModel: 'claude-sonnet-4' });

      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);

      const entry = await service.getProfile('reviewer');
      expect(entry?.defaultModel).toBe('claude-sonnet-4');
    });

    it('should maintain data consistency across create → update → delete → recreate cycle', async () => {
      const homeDir = makeTempDir();
      const { service, registry } = makeService(homeDir);
      await registry.reload();

      // Create
      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'First version',
        defaultModel: 'gpt-4o',
      });

      // Update
      await service.updateProfile('reviewer', { defaultModel: 'claude-sonnet-4' });
      let entry = await service.getProfile('reviewer');
      expect(entry?.defaultModel).toBe('claude-sonnet-4');

      // Delete
      await service.deleteProfile('reviewer');
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(false);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);

      // Recreate
      await service.createProfile({
        name: 'reviewer',
        extends: 'agent',
        description: 'Second version',
        defaultModel: 'gemini-pro',
      });

      entry = await service.getProfile('reviewer');
      expect(entry?.description).toBe('Second version');
      expect(entry?.defaultModel).toBe('gemini-pro');
      expect(existsSync(join(homeDir, 'agents', 'reviewer.yaml'))).toBe(true);
      expect(existsSync(join(homeDir, 'agents', 'reviewer.md'))).toBe(false);
    });
  });
});
