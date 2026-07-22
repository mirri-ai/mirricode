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
function makeService(homeDir: string): { service: ProfileService; registry: ProfileRegistry } {
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

  return { service, registry };
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

  it('should reject updating built-in profile', async () => {
    const homeDir = makeTempDir();
    const { service, registry } = makeService(homeDir);
    await registry.reload();

    await expect(
      service.updateProfile('coder', { description: 'hacked' }),
    ).rejects.toThrow();
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
});
