// apps/mirri-web/src/lib/desktopBridgeContract.test.ts
//
// Guards the contract module's shape: the interface must stay complete and
// DOM-free (the desktop package's tsc has no DOM lib and reads this file
// type-only via the bridge-contract test).

import { describe, expect, it } from 'vitest';

import { REQUIRED_BRIDGE_MEMBERS } from './desktopBridge';

import type { MirriDesktopBridge } from './desktopBridgeContract';

describe('desktopBridgeContract', () => {
  it('should keep all thirteen bridge methods in the contract interface', () => {
    // Compile-time completeness: every member the runtime structural check
    // and the desktop preload rely on. If this object fails to typecheck,
    // a method was renamed/removed from the interface.
    const probe: Record<keyof MirriDesktopBridge, true> = {
      openPath: true,
      revealPath: true,
      openInEditor: true,
      openInApp: true,
      listOpenInApps: true,
      openExternal: true,
      notify: true,
      setBadgeCount: true,
      dockBounce: true,
      setLoginItem: true,
      getLoginItem: true,
      onDeepLink: true,
      onNotificationClick: true,
    };
    expect(probe).toBeDefined();
  });
});

describe('getDesktopBridge structural check', () => {
  it('should only probe members that exist on the bridge contract', () => {
    // The real assertion is compile-time: REQUIRED_BRIDGE_MEMBERS is typed
    // as (keyof MirriDesktopBridge)[], so a typo'd member name fails
    // typecheck instead of silently admitting a broken bridge. The runtime
    // loop just pins the export exists.
    for (const member of REQUIRED_BRIDGE_MEMBERS) {
      expect(typeof member).toBe('string');
    }
  });
});
