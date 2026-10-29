// apps/mirri-desktop-lite/src/mainview/App.vue — the bundled self-check page.
// Used when the daemon is unavailable (and by scripts/selfcheck.mjs): probes
// the bridge structurally and exercises the RPC methods incl. the degraded
// ones (R3 assertions).

<script lang="ts">
import { defineComponent } from "vue";
import { Electroview } from "electrobun/view";

import type { MirriDesktopBridge } from "../../../mirri-web/src/lib/desktopBridgeContract";
import type { MirriSchema } from "../shared/schema";

const rpc = Electroview.defineRPC<MirriSchema>({
  handlers: {
    requests: {},
    messages: {
      "deep-link": ({ url }) => console.log("[bridge] deep-link", url),
      "notification-click": ({ tag }) => console.log("[bridge] notification-click", tag),
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

// The same structural check as the real web's getDesktopBridge().
const REQUIRED_BRIDGE_MEMBERS = [
  "openInEditor",
  "revealPath",
  "notify",
  "setBadgeCount",
  "listOpenInApps",
  "onDeepLink",
] as const satisfies ReadonlyArray<keyof MirriDesktopBridge>;

const structuralCheckPassed = REQUIRED_BRIDGE_MEMBERS.every(
  (m) => typeof (bridge as unknown as Record<string, unknown>)[m] === "function",
);
console.log(`[lite-selfcheck] structural bridge check: ${structuralCheckPassed ? "PASS" : "FAIL"}`);

export default defineComponent({
  name: "App",
  data() {
    return {
      checkPassed: structuralCheckPassed,
      lastResult: "(nothing run yet)",
    };
  },
  methods: {
    async run(method: string): Promise<void> {
      try {
        const b = (window as unknown as { mirriDesktop: MirriDesktopBridge }).mirriDesktop;
        switch (method) {
          case "listOpenInApps":
            this.lastResult = JSON.stringify(await b.listOpenInApps());
            break;
          case "notify":
            this.lastResult = JSON.stringify(await b.notify("Lite 自检通知", "from Electrobun bridge", "lite-tag"));
            break;
          case "openExternal":
            this.lastResult = JSON.stringify(await b.openExternal("https://example.com"));
            break;
          case "openPath":
            this.lastResult = JSON.stringify(await b.openPath("/tmp"));
            break;
          case "setBadgeCount":
            this.lastResult = JSON.stringify(await b.setBadgeCount(7));
            break;
          case "setLoginItemWriteRead": {
            await b.setLoginItem(true);
            this.lastResult = JSON.stringify(await b.getLoginItem());
            break;
          }
        }
      } catch (e) {
        this.lastResult = `ERROR: ${String(e)}`;
      }
    },
  },
});
</script>

<template>
  <div class="poc-root">
    <div class="drag-region">Lite self-check page — drag me to move the window</div>
    <div class="content">
      <h1>Mirri Code Lite — self-check</h1>
      <p>Structural bridge check: <b :class="checkPassed ? 'ok' : 'bad'">{{ checkPassed ? 'PASS' : 'FAIL' }}</b></p>

      <h3>RPC method probes</h3>
      <div class="row">
        <button @click="run('listOpenInApps')">listOpenInApps</button>
        <button @click="run('notify')">notify</button>
        <button @click="run('openExternal')">openExternal</button>
        <button @click="run('openPath')">openPath(/tmp)</button>
        <button @click="run('setBadgeCount')">setBadgeCount(7) → expect {"set":false}</button>
        <button @click="run('setLoginItemWriteRead')">setLoginItem(true)+read → expect {"open":false}</button>
      </div>
      <pre class="result">{{ lastResult }}</pre>
    </div>
  </div>
</template>

<style>
* { box-sizing: border-box; }
body { margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
.poc-root { display: flex; flex-direction: column; height: 100vh; }
.drag-region {
  -webkit-app-region: drag;
  height: 40px;
  background: #2a2a3e;
  color: #eee;
  font-size: 12px;
  display: flex;
  align-items: center;
  padding: 0 12px;
  user-select: none;
}
.content { padding: 20px; overflow: auto; }
.row { display: flex; gap: 8px; flex-wrap: wrap; margin: 8px 0; }
.result { background: #1e1e28; color: #9fe8a2; padding: 10px; border-radius: 6px; min-height: 20px; }
.ok { color: #2e9e44; }
.bad { color: #d33; }
</style>
