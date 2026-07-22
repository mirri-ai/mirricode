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
});
