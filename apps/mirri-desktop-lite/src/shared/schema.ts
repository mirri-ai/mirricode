// apps/mirri-desktop-lite/src/shared/schema.ts
//
// The RPC schema — derived from the web bridge contract (single source of
// truth, ADR-001). Every `bun.requests` entry maps one `MirriDesktopBridge`
// method; the two `webview.messages` map the main→renderer events. Kept
// structurally in sync with the contract via the compile-time gate in
// src/bridge-contract.test.ts.

import type { MirriDesktopBridge } from "../../../mirri-web/src/lib/desktopBridgeContract";

/** The Electrobun RPC schema mirroring the bridge contract. */
export interface MirriSchema {
  bun: {
    requests: {
      openPath: { params: { path: string }; response: Awaited<ReturnType<MirriDesktopBridge['openPath']>> };
      revealPath: { params: { path: string }; response: Awaited<ReturnType<MirriDesktopBridge['revealPath']>> };
      openInEditor: { params: { path: string; line?: number }; response: Awaited<ReturnType<MirriDesktopBridge['openInEditor']>> };
      openInApp: { params: { appId: string; path: string }; response: Awaited<ReturnType<MirriDesktopBridge['openInApp']>> };
      listOpenInApps: {
        params: Record<string, never>;
        response: Awaited<ReturnType<MirriDesktopBridge['listOpenInApps']>>;
      };
      openExternal: { params: { url: string }; response: Awaited<ReturnType<MirriDesktopBridge['openExternal']>> };
      notify: { params: { title: string; body: string; tag?: string }; response: Awaited<ReturnType<MirriDesktopBridge['notify']>> };
      setBadgeCount: { params: { count: number }; response: Awaited<ReturnType<MirriDesktopBridge['setBadgeCount']>> };
      dockBounce: { params: { kind?: 'informational' | 'critical' }; response: { bounced: boolean } };
      setLoginItem: { params: { open: boolean }; response: Awaited<ReturnType<MirriDesktopBridge['setLoginItem']>> };
      getLoginItem: { params: Record<string, never>; response: Awaited<ReturnType<MirriDesktopBridge['getLoginItem']>> };
    };
    messages: {};
  };
  webview: {
    requests: {};
    messages: {
      'deep-link': { url: string };
      'notification-click': { tag: string };
    };
  };
}
