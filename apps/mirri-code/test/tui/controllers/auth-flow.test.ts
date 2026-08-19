import { describe, expect, it, vi } from 'vitest';

import type { MirriConfig, MirriConfigPatch, MirriHarness } from '@mirri-ai/mirri-code-sdk';

import { AuthFlowController, type AuthFlowHost } from '#/tui/controllers/auth-flow';
import {
  refreshAllProviderModels,
  type RefreshProviderHost,
  type RefreshProviderOptions,
  type RefreshResult,
} from '#/tui/utils/refresh-providers';

// Stub the orchestrator so the tests capture the exact host `buildRefreshHost()`
// produced and can drive its methods directly. This pins the persistence
// contract (when and how often the harness is written) without depending on the
// oauth package's network fetching.
vi.mock('#/tui/utils/refresh-providers', () => ({
  refreshAllProviderModels: vi.fn(),
}));

const PROVIDER_ID = 'test-provider';

function baseConfig(): MirriConfig {
  return {
    providers: {
      [PROVIDER_ID]: { type: 'openai', baseUrl: 'https://example.test/v1' },
    },
  };
}

function emptyRefreshResult(): RefreshResult {
  return { changed: [], unchanged: [], failed: [] };
}

interface Harness {
  readonly host: AuthFlowHost;
  readonly controller: AuthFlowController;
  readonly capture: {
    readonly refreshHost: RefreshProviderHost | undefined;
    readonly options: RefreshProviderOptions | undefined;
  };
  readonly harness: {
    readonly supportsAtomicSectionReplace: ReturnType<typeof vi.fn>;
    readonly getConfig: ReturnType<typeof vi.fn>;
    readonly removeProvider: ReturnType<typeof vi.fn>;
    readonly setConfig: ReturnType<typeof vi.fn>;
    readonly replaceConfigSections: ReturnType<typeof vi.fn>;
  };
  readonly setAppState: ReturnType<typeof vi.fn>;
}

function createHarness(options: { atomic?: boolean; result?: RefreshResult } = {}): Harness {
  const supportsAtomicSectionReplace = vi.fn(() => options.atomic ?? true);
  const getConfig = vi.fn(async () => baseConfig());
  const removeProvider = vi.fn(async () => baseConfig());
  const setConfig = vi.fn(async (patch: MirriConfigPatch) => ({ ...baseConfig(), ...patch }));
  const replaceConfigSections = vi.fn(async () => {});
  const setAppState = vi.fn();

  const capture: { refreshHost: RefreshProviderHost | undefined; options: RefreshProviderOptions | undefined } =
    { refreshHost: undefined, options: undefined };
  vi.mocked(refreshAllProviderModels).mockImplementation(async (builtHost, refreshOptions) => {
    capture.refreshHost = builtHost;
    capture.options = refreshOptions;
    return options.result ?? emptyRefreshResult();
  });

  const harness = {
    supportsAtomicSectionReplace,
    getConfig,
    removeProvider,
    setConfig,
    replaceConfigSections,
  } as unknown as MirriHarness;

  const host = { harness, setAppState } as unknown as AuthFlowHost;

  return {
    host,
    controller: new AuthFlowController(host),
    capture,
    harness: { supportsAtomicSectionReplace, getConfig, removeProvider, setConfig, replaceConfigSections },
    setAppState,
  };
}

function requireRefreshHost(harness: Harness): RefreshProviderHost {
  if (harness.capture.refreshHost === undefined) {
    throw new Error('refreshAllProviderModels was not invoked; cannot inspect the built host');
  }
  return harness.capture.refreshHost;
}

describe('AuthFlowController buildRefreshHost', () => {
  it('should commit the refresh in one atomic write when the engine supports section replace', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshProviderModels();
    const refreshHost = requireRefreshHost(fixture);

    await refreshHost.getConfig();
    await refreshHost.removeProvider(PROVIDER_ID);
    await refreshHost.setConfig({ providers: {} });

    // The remove+set round trip must not touch disk: the refresh is persisted
    // by exactly one replaceConfigSections call, nothing in between.
    expect(fixture.harness.replaceConfigSections).toHaveBeenCalledTimes(1);
    expect(fixture.harness.removeProvider).not.toHaveBeenCalled();
    expect(fixture.harness.setConfig).not.toHaveBeenCalled();
    expect(fixture.harness.getConfig).toHaveBeenCalledTimes(1);
    expect(fixture.harness.getConfig).toHaveBeenCalledWith({ reload: true });
  });

  it('should keep undefined patch values (e.g. a cleared defaultModel) in the sections handed to replaceConfigSections', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshProviderModels();
    const refreshHost = requireRefreshHost(fixture);

    await refreshHost.getConfig();
    await refreshHost.setConfig({
      providers: { [PROVIDER_ID]: { type: 'openai' } },
      defaultModel: undefined,
    });

    expect(fixture.harness.replaceConfigSections).toHaveBeenCalledTimes(1);
    const replaced = fixture.harness.replaceConfigSections.mock.calls[0]?.[0] as Record<string, unknown>;
    // `defaultModel: undefined` is how a cleared section is expressed;
    // Object.entries/Object.fromEntries must not drop the key while keeping it.
    expect(Object.prototype.hasOwnProperty.call(replaced, 'defaultModel')).toBe(true);
    expect(replaced['defaultModel']).toBeUndefined();
    expect(replaced['providers']).toEqual({ [PROVIDER_ID]: { type: 'openai' } });
  });

  it('should persist directly via removeProvider and setConfig when the engine does not support section replace', async () => {
    const fixture = createHarness({ atomic: false });

    await fixture.controller.refreshProviderModels();
    const refreshHost = requireRefreshHost(fixture);

    await refreshHost.getConfig();
    await refreshHost.removeProvider(PROVIDER_ID);
    await refreshHost.setConfig({ providers: {} });

    expect(fixture.harness.removeProvider).toHaveBeenCalledTimes(1);
    expect(fixture.harness.removeProvider).toHaveBeenCalledWith(PROVIDER_ID);
    expect(fixture.harness.setConfig).toHaveBeenCalledTimes(1);
    expect(fixture.harness.setConfig).toHaveBeenCalledWith({ providers: {} });
    expect(fixture.harness.replaceConfigSections).not.toHaveBeenCalled();
  });

  it('should throw when removeProvider is called before getConfig on the atomic host', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshProviderModels();
    const refreshHost = requireRefreshHost(fixture);

    expect(() => refreshHost.removeProvider(PROVIDER_ID)).toThrow(
      'getConfig must be called before writes',
    );
  });

  it('should reject when setConfig is called before getConfig on the atomic host', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshProviderModels();
    const refreshHost = requireRefreshHost(fixture);

    await expect(refreshHost.setConfig({ providers: {} })).rejects.toThrow(
      'getConfig must be called before writes',
    );
  });

  it('should forward the "all" scope from refreshProviderModels to the orchestrator', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshProviderModels();

    expect(fixture.capture.options).toEqual({ scope: 'all' });
    expect(fixture.capture.refreshHost).toBeDefined();
  });

  it('should forward the "oauth" scope from refreshOAuthProviderModels to the orchestrator', async () => {
    const fixture = createHarness({ atomic: true });

    await fixture.controller.refreshOAuthProviderModels();

    expect(fixture.capture.options).toEqual({ scope: 'oauth' });
    expect(fixture.capture.refreshHost).toBeDefined();
  });

  it('should refresh locally available providers/models when the refresh reported a change', async () => {
    const fixture = createHarness({
      atomic: true,
      result: {
        changed: [{ providerId: PROVIDER_ID, providerName: 'Test Provider', added: 1, removed: 0 }],
        unchanged: [],
        failed: [],
      },
    });

    await fixture.controller.refreshProviderModels();

    expect(fixture.capture.options).toEqual({ scope: 'all' });
    expect(fixture.harness.getConfig).toHaveBeenCalledWith({ reload: true });
    expect(fixture.setAppState).toHaveBeenCalledWith({
      availableModels: {},
      availableProviders: { [PROVIDER_ID]: { type: 'openai', baseUrl: 'https://example.test/v1' } },
    });
  });
});