// apps/mirri-web/src/composables/useSetupGate.ts
// Model-setup readiness gates the main app. Once the first load finishes and
// auth is still missing (no model/provider configured), show a full-page setup
// guide instead of an in-app banner. While the gate is visible, re-check auth
// every few seconds so the gate lifts itself as soon as the daemon becomes
// ready (e.g. a provider was configured) — then the app hydrates like the old
// login-success handler did.

import { computed, onUnmounted, ref, watch, type Ref } from 'vue';
import type { useMirriWebClient } from './useMirriWebClient';

type MirriWebClient = ReturnType<typeof useMirriWebClient>;

export interface UseSetupGateOptions {
  client: MirriWebClient;
  /** Template ref to the setup-page logo SVG; owned by the component so the
      template `ref=` binding links, passed here so the blink handler can drive it. */
  authLogoRef: Ref<SVGSVGElement | null>;
}

/** How often to re-check auth while the setup gate is visible. */
const SETUP_POLL_INTERVAL_MS = 3000;

export function useSetupGate({ client, authLogoRef }: UseSetupGateOptions) {
  const authReady = computed(() => client.authReady.value);
  const showSetupGate = computed(() => client.initialized.value && !authReady.value);
  const SETUP_PATH = '/setup';
  const setupReturnPath = ref<string | null>(null);
  let authLogoBlinkTimer: ReturnType<typeof setTimeout> | null = null;
  let setupPollTimer: ReturnType<typeof setInterval> | null = null;
  let setupPolling = false;
  let hydrating = false;

  function currentPathWithSuffix(): string {
    if (typeof window === 'undefined') return '/';
    return `${window.location.pathname}${window.location.search}${window.location.hash}`;
  }

  function replaceBrowserPath(path: string): void {
    if (typeof window === 'undefined') return;
    window.history.replaceState(window.history.state, '', path);
  }

  /** One auth re-check while the gate is visible. Hydration is NOT done here:
   *  the showSetupGate watcher owns it, so ANY path that flips authReady — this
   *  poll or an external one (e.g. saving a provider config through the gate's
   *  Settings instance) — hydrates identically and no poll/other-path race can
   *  double-load. */
  async function pollSetupAuth(): Promise<void> {
    if (setupPolling) return;
    setupPolling = true;
    try {
      await client.checkAuth();
    } catch {
      // checkAuth is defensive (never throws); keep the poll alive regardless.
    } finally {
      setupPolling = false;
    }
  }

  function startSetupPolling(): void {
    if (setupPollTimer !== null) return;
    void pollSetupAuth();
    setupPollTimer = setInterval(() => {
      void pollSetupAuth();
    }, SETUP_POLL_INTERVAL_MS);
  }

  function stopSetupPolling(): void {
    if (setupPollTimer !== null) {
      clearInterval(setupPollTimer);
      setupPollTimer = null;
    }
  }

  /** Hydrate the whole app after the gate lifts, mirroring the old login-success
   *  handler (checkAuth + load). Runs at most once per lift — the watcher is
   *  the single caller, and the guard protects against an overlapping run. */
  async function hydrateAfterGateLift(): Promise<void> {
    if (hydrating) return;
    hydrating = true;
    try {
      await client.checkAuth();
      await client.load();
    } catch {
      // checkAuth/load are defensive (never throw); nothing to surface.
    } finally {
      hydrating = false;
    }
  }

  watch(showSetupGate, (show, prev) => {
    if (typeof window === 'undefined') return;
    if (show) {
      startSetupPolling();
      if (window.location.pathname !== SETUP_PATH) {
        setupReturnPath.value = currentPathWithSuffix();
        replaceBrowserPath(SETUP_PATH);
      }
      return;
    }
    stopSetupPolling();
    // Only an actual true→false lift hydrates. The { immediate: true } arm at
    // mount (show=false, prev=undefined — the gate never showed) must not run
    // a checkAuth+load on a perfectly healthy boot.
    if (prev === true) {
      void hydrateAfterGateLift();
    }
    if (window.location.pathname === SETUP_PATH) {
      replaceBrowserPath(setupReturnPath.value ?? '/');
      setupReturnPath.value = null;
    }
  }, { immediate: true });

  function blinkAuthLogo(): void {
    const el = authLogoRef.value;
    if (!el) return;
    el.classList.remove('blink-now');
    void el.getBoundingClientRect();
    el.classList.add('blink-now');
    if (authLogoBlinkTimer !== null) clearTimeout(authLogoBlinkTimer);
    authLogoBlinkTimer = setTimeout(() => {
      authLogoBlinkTimer = null;
      el.classList.remove('blink-now');
    }, 300);
  }

  onUnmounted(() => {
    stopSetupPolling();
    if (authLogoBlinkTimer !== null) clearTimeout(authLogoBlinkTimer);
  });

  return { showSetupGate, blinkAuthLogo };
}
