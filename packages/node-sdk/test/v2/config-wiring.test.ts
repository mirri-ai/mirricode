/**
 * Regression coverage for the v2 config wiring (`SDKRpcClientV2` config
 * batch): the CLI's startup path calls `getConfig` /
 * `getConfigDiagnostics` unconditionally, so these overrides must read the
 * engine's `IConfigService` instead of falling through to the loud
 * `getRpc()` failure.
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { createMirriHarnessV2 } from '#/sdk-rpc-client-v2';
import type { MirriHarness } from '#/mirri-harness';

const harnesses: MirriHarness[] = [];
const homeDirs = new Map<MirriHarness, string>();

afterEach(async () => {
  await Promise.allSettled(harnesses.splice(0).map((harness) => harness.close()));
});

function createHarness(): MirriHarness {
  const homeDir = mkdtempSync(join(tmpdir(), 'mirri-sdk-config-'));
  const harness = createMirriHarnessV2({
    homeDir,
    identity: {
      userAgentProduct: 'mirri-sdk-test',
      version: '0.0.0',
    },
    uiMode: 'cli',
  });
  harnesses.push(harness);
  homeDirs.set(harness, homeDir);
  return harness;
}

describe('SDKRpcClientV2 config wiring', () => {
  it('should return the v1-shaped config and empty diagnostics on a fresh home', async () => {
    const harness = createHarness();
    await harness.ensureConfigFile();

    const config = await harness.getConfig();
    // v1 shape: top-level `providers` / `models` exist, `cron` (a v2-only
    // domain) is dropped by the projection.
    expect(config).toMatchObject({ providers: {}, models: {} });
    expect('cron' in config).toBe(false);
    // A fresh v2 default document carries no planMode key (the CLI writes
    // it only when the user changes the mode).
    expect('planMode' in config).toBe(false);

    const diagnostics = await harness.getConfigDiagnostics();
    expect(diagnostics.warnings).toEqual([]);

    const features = await harness.getExperimentalFeatures();
    expect(Array.isArray(features)).toBe(true);
    for (const feature of features) {
      expect(feature).toMatchObject({
        id: expect.any(String),
        title: expect.any(String),
        description: expect.any(String),
        enabled: expect.any(Boolean),
      });
    }
  });

  it('should persist a setConfig patch and read it back', async () => {
    const harness = createHarness();
    await harness.ensureConfigFile();

    const before = await harness.getConfig();
    // A fresh v2 default document carries no planMode key.
    expect(before.planMode).toBeUndefined();

    const after = await harness.setConfig({ planMode: true });
    expect(after.planMode).toBe(true);

    const roundTrip = await harness.getConfig();
    expect(roundTrip.planMode).toBe(true);
  });

  it('should keep the user layer intact when an env model overlay is active', async () => {
    // With MIRRICODE_MODEL_API_KEY set, the EFFECTIVE config view carries an
    // env-injected provider that the user never wrote. The removal cascade
    // is planned from `inspect().userValue` precisely so the persisted file
    // only ever reflects the user's own entries.
    process.env['MIRRICODE_MODEL_API_KEY'] = 'env-injected-key';
    try {
      const harness = createHarness();
      await harness.ensureConfigFile();
      await harness.setConfig({
        providers: { example: { apiKey: 'k' }, keeper: { apiKey: 'k2' } },
      });

      const after = await harness.removeProvider('example');

      expect(Object.keys(after.providers ?? {})).toContain('keeper');
      expect(Object.keys(after.providers ?? {})).not.toContain('example');
      const configText = readFileSync(join(homeDirs.get(harness)!, 'config.toml'), 'utf8');
      expect(configText).toContain('keeper');
      expect(configText).not.toContain('env-injected-key');
    } finally {
      delete process.env['MIRRICODE_MODEL_API_KEY'];
    }
  });

  it('should apply the v1 provider-removal cascade on removeProvider', async () => {
    const harness = createHarness();
    await harness.ensureConfigFile();

    await harness.setConfig({
      providers: { example: { apiKey: 'k' } },
      models: { 'example-model': { provider: 'example' } },
      defaultProvider: 'example',
      defaultModel: 'example-model',
    });

    const afterRemove = await harness.removeProvider('example');
    expect(afterRemove.providers).toEqual({});
    expect(afterRemove.models).toEqual({});
    expect(afterRemove.defaultProvider).toBeUndefined();
    expect(afterRemove.defaultModel).toBeUndefined();

    // The write persisted: a fresh read sees the cascade too.
    const roundTrip = await harness.getConfig();
    expect(roundTrip.providers).toEqual({});
    expect(roundTrip.defaultProvider).toBeUndefined();
  });

  it('should report atomic section replace as supported on the v2 engine', async () => {
    const harness = createHarness();
    await harness.ensureConfigFile();

    expect(harness.supportsAtomicSectionReplace()).toBe(true);
  });

  it('should clear and rewrite several sections in one atomic replace', async () => {
    const harness = createHarness();
    await harness.ensureConfigFile();
    await harness.setConfig({
      providers: { old: { apiKey: 'k' } },
      models: { 'old-model': { provider: 'old' } },
      defaultModel: 'old-model',
    });

    // Replace semantics (unlike setConfig's deep merge): the new records
    // stand on their own, and an `undefined` section is cleared.
    await harness.replaceConfigSections({
      providers: { fresh: { apiKey: 'k2' } },
      models: { 'fresh-model': { provider: 'fresh' } },
      defaultModel: undefined,
    });

    const after = await harness.getConfig({ reload: true });
    expect(Object.keys(after.providers ?? {})).toEqual(['fresh']);
    expect(Object.keys(after.models ?? {})).toEqual(['fresh-model']);
    expect(after.defaultModel).toBeUndefined();
  });

  it('should report a config.toml parse error through getConfigDiagnostics', async () => {
    const homeDir = mkdtempSync(join(tmpdir(), 'mirri-sdk-config-'));
    // A broken config.toml in the home dir — the exact shape a real user
    // would hit on startup.
    writeFileSync(join(homeDir, 'config.toml'), '[broken = {]\n');
    try {
      const harness = createMirriHarnessV2({
        homeDir,
        identity: {
          userAgentProduct: 'mirri-sdk-test',
          version: '0.0.0',
        },
        uiMode: 'cli',
      });
      harnesses.push(harness);

      await harness.ensureConfigFile(); // file exists, stays broken
      const diagnostics = await harness.getConfigDiagnostics();
      expect(diagnostics.warnings.length).toBeGreaterThan(0);
      expect(diagnostics.warnings[0]).toContain('config.toml');
    } finally {
      // Close before deleting the dir so the engine's log flush has a
      // target; the shared afterEach then has nothing left to close.
      await harnesses.pop()?.close();
      await new Promise((resolve) => setTimeout(resolve, 50));
      rmSync(homeDir, { recursive: true, force: true });
    }
  });
});