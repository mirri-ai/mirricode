// apps/mirri-desktop-lite/src/preload/bridge.ts
//
// The preload injected into the daemon-served web UI: bundles the
// Electroview RPC client and exposes window.mirriDesktop satisfying the web
// bridge contract. Built by esbuild into dist/preload.js (single IIFE).

import { Electroview } from "electrobun/view";

import type { MirriDesktopBridge } from "../../../mirri-web/src/lib/desktopBridgeContract";
import type { MirriSchema } from "../shared/schema";

const rpc = Electroview.defineRPC<MirriSchema>({
  handlers: {
    requests: {},
    messages: {
      "deep-link": ({ url }) => {
        window.dispatchEvent(new CustomEvent("mirri:deep-link", { detail: url }));
      },
      "notification-click": ({ tag }) => {
        window.dispatchEvent(new CustomEvent("mirri:notification-click", { detail: tag }));
      },
    },
  },
});

new Electroview({ rpc });

const bridge: MirriDesktopBridge = {
  openPath: (path) => rpc.request("openPath", { path }),
  revealPath: (path) => rpc.request("revealPath", { path }),
  openInEditor: (path, line) => rpc.request("openInEditor", { path, line }),
  openInApp: (appId, path) => rpc.request("openInApp", { appId, path }),
  listOpenInApps: () => rpc.request("listOpenInApps", {}),
  openExternal: (url) => rpc.request("openExternal", { url }),
  notify: (title, body, tag) => rpc.request("notify", { title, body, tag }),
  setBadgeCount: (count) => rpc.request("setBadgeCount", { count }),
  dockBounce: async (kind) => {
    await rpc.request("dockBounce", { kind });
  },
  setLoginItem: (open) => rpc.request("setLoginItem", { open }),
  getLoginItem: () => rpc.request("getLoginItem", {}),
  onDeepLink: (cb) => rpc.addMessageListener("deep-link", ({ url }) => cb(url)),
  onNotificationClick: (cb) =>
    rpc.addMessageListener("notification-click", ({ tag }) => cb(tag)),
};

(window as unknown as Record<string, unknown>).mirriDesktop = bridge;

console.log("[lite-preload] window.mirriDesktop injected (electrobun bridge)");
