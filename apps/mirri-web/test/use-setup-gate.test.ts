// apps/mirri-web/test/use-setup-gate.test.ts
// The setup gate must hydrate the app (checkAuth + load) on ANY visible→hidden
// transition — not only when its own 3s poll detects readiness, but also when
// auth flips ready through another path (e.g. saving a provider config in the
// gate's Settings instance calls client.checkAuth() directly).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { nextTick, ref } from 'vue';
import { useSetupGate } from '../src/composables/useSetupGate';

// The composable registers its poll cleanup via onUnmounted. Outside a component
// (unit test) there is no active instance, so Vue would warn; stub the hook —
// these tests drive the watch/hydration logic directly, not the lifecycle.
vi.mock('vue', async (importOriginal) => {
  const actual = await importOriginal<typeof import('vue')>();
  return { ...actual, onUnmounted: vi.fn() };
});

type SetupGateClient = Parameters<typeof useSetupGate>[0]['client'];

function createClient() {
  const authReady = ref(false);
  const initialized = ref(true);
  return {
    authReady,
    initialized,
    checkAuth: vi.fn(async () => undefined),
    load: vi.fn(async () => undefined),
  };
}

function createGate(client: ReturnType<typeof createClient>) {
  return useSetupGate({
    client: client as unknown as SetupGateClient,
    authLogoRef: ref<SVGSVGElement | null>(null),
  });
}

/** Flush microtasks + the Vue scheduler so watchers run. */
async function flush(): Promise<void> {
  await Promise.resolve();
  await nextTick();
  await nextTick();
}

beforeEach(() => {
  vi.useFakeTimers();
  // The watcher touches window for the /setup rewrite; a healthy-boot path of
  // "/" keeps it inert while still crossing the `typeof window` guard.
  vi.stubGlobal('window', {
    location: { pathname: '/', search: '', hash: '' },
    history: { state: null, replaceState: vi.fn() },
  } as unknown as Window & typeof globalThis);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('useSetupGate — gate-lift hydration', () => {
  it('should hydrate the app once when auth becomes ready outside the poll while the gate is visible', async () => {
    const client = createClient();
    const gate = createGate(client);
    await flush();
    // Gate visible: initialized && !authReady.
    expect(gate.showSetupGate.value).toBe(true);
    expect(client.load).not.toHaveBeenCalled();

    // External flip — e.g. handleUpdateConfig → client.checkAuth() after the
    // daemon accepted a provider config (NOT via the gate's own poll).
    client.authReady.value = true;
    await flush();

    // The gate lifts reactively and the app hydrates exactly once, mirroring
    // the old login-success path (checkAuth first, then load).
    expect(gate.showSetupGate.value).toBe(false);
    expect(client.load).toHaveBeenCalledTimes(1);
    expect(client.checkAuth).toHaveBeenCalled();
    const loadCall = client.load.mock.invocationCallOrder[0];
    const authCalls = client.checkAuth.mock.invocationCallOrder;
    expect(loadCall).toBeGreaterThan(authCalls.at(-1));
  });

  it('should not hydrate on mount when the gate never becomes visible', async () => {
    const client = createClient();
    client.authReady.value = true;
    createGate(client);
    await flush();
    expect(client.load).not.toHaveBeenCalled();
    expect(client.checkAuth).not.toHaveBeenCalled();
  });

  it('should hydrate exactly once when the poll itself detects readiness', async () => {
    const client = createClient();
    // The immediate poll call flips authReady — readiness detected by the
    // gate's own 3s poll path.
    client.checkAuth.mockImplementation(async () => {
      client.authReady.value = true;
    });
    createGate(client);
    await flush();
    expect(client.load).toHaveBeenCalledTimes(1);
  });
});
