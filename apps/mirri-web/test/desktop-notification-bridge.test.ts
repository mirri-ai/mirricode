// apps/mirri-web/test/desktop-notification-bridge.test.ts
//
// The desktop-shell notification path (D4): when the preload bridge is
// present, notifications route through the main process (IPC notify +
// dock bounce, click forwarded back by tag); when the shell can't show one,
// the browser Notification API takes over; with no bridge at all, the
// existing browser path is unchanged.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { i18n } from '../src/i18n';
import type { MirriDesktopBridge } from '../src/lib/desktopBridge';
import { useNotification } from '../src/composables/client/useNotification';

function createMemoryStorage(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    clear() {
      data.clear();
    },
    getItem(key: string) {
      return data.get(key) ?? null;
    },
    key(index: number) {
      return Array.from(data.keys()).at(index) ?? null;
    },
    removeItem(key: string) {
      data.delete(key);
    },
    setItem(key: string, value: string) {
      data.set(key, value);
    },
  };
}

function installStorage(storage: Storage): void {
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: storage,
  });
}

class FakeBrowserNotification {
  static instances: FakeBrowserNotification[] = [];
  onclick: (() => void) | null = null;
  constructor(
    public title: string,
    public options: { body?: string; tag?: string },
  ) {
    FakeBrowserNotification.instances.push(this);
  }
  close(): void {}
}

function installBridge(notifyResult: { shown: boolean }): {
  bridge: MirriDesktopBridge;
  notify: ReturnType<typeof vi.fn>;
  dockBounce: ReturnType<typeof vi.fn>;
} {
  const notify = vi.fn().mockResolvedValue(notifyResult);
  const dockBounce = vi.fn().mockResolvedValue(undefined);
  const bridge = {
    openPath: vi.fn(),
    revealPath: vi.fn(),
    openInEditor: vi.fn(),
    openInApp: vi.fn(),
    openExternal: vi.fn(),
    notify,
    setBadgeCount: vi.fn(),
    dockBounce,
    setLoginItem: vi.fn(),
    getLoginItem: vi.fn(),
    onDeepLink: vi.fn(),
    onNotificationClick: vi.fn(),
  } as unknown as MirriDesktopBridge;
  vi.stubGlobal('window', { mirriDesktop: bridge });
  return { bridge, notify, dockBounce };
}

beforeEach(() => {
  installStorage(createMemoryStorage());
  FakeBrowserNotification.instances = [];
  vi.stubGlobal('Notification', FakeBrowserNotification);
  Object.defineProperty(FakeBrowserNotification, 'permission', {
    configurable: true,
    value: 'granted',
  });
  i18n.global.locale.value = 'en';
});

afterEach(() => {
  vi.unstubAllGlobals();
  installStorage(createMemoryStorage());
});

describe('desktop notification bridge', () => {
  it('routes through the shell bridge instead of the browser Notification', async () => {
    const { notify, dockBounce } = installBridge({ shown: true });
    const { maybeNotifyCompletion } = useNotification();

    maybeNotifyCompletion('s1', {
      isUserWatching: false,
      sessionTitle: 'Refactor',
      promptId: 'p1',
      onClick: vi.fn(),
    });
    // The bridge path is async (invoke) — flush microtasks.
    await vi.waitFor(() => expect(notify).toHaveBeenCalledTimes(1));

    expect(notify).toHaveBeenCalledWith('Mirri Code · Turn finished', 'Refactor', expect.any(String));
    expect(dockBounce).toHaveBeenCalledWith('informational');
    expect(FakeBrowserNotification.instances).toHaveLength(0);
  });

  it('falls back to the browser Notification when the shell cannot show one', async () => {
    installBridge({ shown: false });
    const { maybeNotifyCompletion } = useNotification();

    maybeNotifyCompletion('s1', {
      isUserWatching: false,
      sessionTitle: 'Refactor',
      promptId: 'p2',
      onClick: vi.fn(),
    });
    await vi.waitFor(() =>
      expect(FakeBrowserNotification.instances.length).toBeGreaterThanOrEqual(1),
    );
  });

  it('suppresses the notification when the user is watching the session', async () => {
    const { notify } = installBridge({ shown: true });
    const { maybeNotifyCompletion } = useNotification();

    maybeNotifyCompletion('s1', {
      isUserWatching: true,
      sessionTitle: 'Refactor',
      promptId: 'p3',
      onClick: vi.fn(),
    });
    await Promise.resolve();
    expect(notify).not.toHaveBeenCalled();
    expect(FakeBrowserNotification.instances).toHaveLength(0);
  });

  it('replays the click action when the shell echoes the tag back', async () => {
    const { notify } = installBridge({ shown: true });
    const { maybeNotifyCompletion, handleNativeNotificationClick } = useNotification();
    const onClick = vi.fn();

    maybeNotifyCompletion('s1', {
      isUserWatching: false,
      sessionTitle: 'Refactor',
      promptId: 'p4',
      onClick,
    });
    await vi.waitFor(() => expect(notify).toHaveBeenCalledTimes(1));
    const tag = (notify.mock.calls[0] as unknown[])[2] as string;

    handleNativeNotificationClick(tag);
    expect(onClick).toHaveBeenCalledTimes(1);
  });
});
