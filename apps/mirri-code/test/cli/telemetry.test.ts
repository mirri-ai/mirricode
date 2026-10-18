/**
 * Tests for the CLI telemetry bootstrap helpers, focusing on the
 * `mirri web` / `mirri server run` host wiring added in `cli/telemetry.ts`.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { initializeServerTelemetry } from '#/cli/telemetry';

const mocks = vi.hoisted(() => ({
  initializeTelemetry: vi.fn(),
  createMirriDeviceId: vi.fn(() => 'device-123'),
  resolveMirriHome: vi.fn(() => '/home/.mirri-code'),
  resolveConfigPath: vi.fn(() => '/home/.mirri-code/config.toml'),
  loadRuntimeConfigSafe: vi.fn(
    (): {
      config: {
        defaultModel?: string;
        defaultProvider?: string;
        providers?: Record<string, { type?: string; apiKey?: string }>;
        telemetry?: boolean;
      };
      fileError: Error | undefined;
    } => ({
      config: { defaultModel: 'kimi-k2', telemetry: true },
      fileError: undefined,
    }),
  ),
}));

vi.mock('@mirri-ai/mirri-telemetry', () => ({
  initializeTelemetry: mocks.initializeTelemetry,
  setTelemetryContext: vi.fn(),
  track: vi.fn(),
  withTelemetryContext: vi.fn(),
}));

// `#/cli/telemetry` only imports `createMirriDeviceId` from kosong and the
// config helpers from the SDK — mock them with minimal factories instead of
// loading the real graph (which is heavy and makes the suite flaky).
vi.mock('@mirri-ai/kosong', () => ({
  createMirriDeviceId: mocks.createMirriDeviceId,
}));

vi.mock('@mirri-ai/mirri-code-sdk', () => ({
  loadRuntimeConfigSafe: mocks.loadRuntimeConfigSafe,
  resolveConfigPath: mocks.resolveConfigPath,
  resolveMirriHome: mocks.resolveMirriHome,
}));

describe('initializeServerTelemetry', () => {
  beforeEach(() => {
    mocks.initializeTelemetry.mockClear();
    mocks.loadRuntimeConfigSafe.mockClear();
    mocks.loadRuntimeConfigSafe.mockReturnValue({
      config: { defaultModel: 'kimi-k2', telemetry: true },
      fileError: undefined,
    });
  });

  it('configures the sink with ui_mode="web" and the CLI product identity', () => {
    const client = initializeServerTelemetry({ version: '1.2.3' });

    expect(mocks.initializeTelemetry).toHaveBeenCalledWith(
      expect.objectContaining({
        appName: 'mirri-code-cli',
        version: '1.2.3',
        uiMode: 'web',
        model: 'kimi-k2',
        enabled: true,
        deviceId: 'device-123',
        homeDir: '/home/.mirri-code',
      }),
    );
    // The returned client wraps the module functions so core + the host share
    // the same underlying client.
    expect(client).toEqual(
      expect.objectContaining({
        track: expect.any(Function),
        withContext: expect.any(Function),
        setContext: expect.any(Function),
      }),
    );
  });

  it('disables telemetry when config.toml sets telemetry = false', () => {
    mocks.loadRuntimeConfigSafe.mockReturnValue({
      config: { defaultModel: 'kimi-k2', telemetry: false },
      fileError: undefined,
    });
    initializeServerTelemetry({ version: '1.2.3' });

    expect(mocks.initializeTelemetry).toHaveBeenCalledWith(
      expect.objectContaining({ enabled: false }),
    );
  });

  it('degrades to enabled with no model when config is unreadable', () => {
    mocks.loadRuntimeConfigSafe.mockReturnValue({
      config: {},
      fileError: new Error('bad toml'),
    });
    initializeServerTelemetry({ version: '1.2.3' });

    expect(mocks.initializeTelemetry).toHaveBeenCalledWith(
      expect.objectContaining({ enabled: true, model: undefined }),
    );
  });

  it('resolves the telemetry bearer credential from the default provider api key', async () => {
    mocks.loadRuntimeConfigSafe.mockReturnValue({
      config: {
        defaultModel: 'kimi-k2',
        defaultProvider: 'kimi',
        providers: { kimi: { type: 'openai', apiKey: 'sk-telemetry-key' } },
        telemetry: true,
      },
      fileError: undefined,
    });
    initializeServerTelemetry({ version: '1.2.3' });

    const accessToken = mocks.initializeTelemetry.mock.calls[0]?.[0]?.['getAccessToken'];
    await expect(accessToken()).resolves.toBe('sk-telemetry-key');
  });

  it('leaves telemetry unauthenticated when no provider api key is configured', async () => {
    mocks.loadRuntimeConfigSafe.mockReturnValue({
      config: {
        defaultModel: 'kimi-k2',
        defaultProvider: 'kimi',
        providers: { kimi: { type: 'openai', apiKey: '' } },
        telemetry: true,
      },
      fileError: undefined,
    });
    initializeServerTelemetry({ version: '1.2.3' });

    const accessToken = mocks.initializeTelemetry.mock.calls[0]?.[0]?.['getAccessToken'];
    await expect(accessToken()).resolves.toBeNull();
  });
});