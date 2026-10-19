// apps/mirri-desktop/src/main/ipc-handlers.ts
//
// The desktop IPC handlers, built via a dependency-injected factory so the
// protocol is unit-testable without Electron (register-ipc.ts supplies the
// real `shell`/`Notification`/`app` implementations).
//
// Every payload crosses validateXxx from ipc.ts before touching an OS
// surface — the renderer is treated as untrusted input.

import {
  IPC_CHANNELS,
  validateBadgeCountPayload,
  validateExternalUrlPayload,
  validateLoginItemPayload,
  validateNotifyPayload,
  validateOpenInAppPayload,
  validatePathPayload,
} from './ipc';
import { editorLaunchFor, launchDetached, openInAppLaunch, type LaunchCommand } from './file-launch';
import type { DetectedOpenInApp } from './open-in-apps';

/** OS surfaces the handlers need. register-ipc.ts adapts Electron's modules
 *  to this shape; tests provide mocks. */
export interface DesktopIpcDeps {
  shellOpenPath(path: string): Promise<string>;
  shellRevealPath(path: string): void;
  shellOpenExternal(url: string): Promise<void>;
  launch(cmd: LaunchCommand): Promise<void>;
  /** Show a native notification; returns false when unsupported. */
  showNotification(title: string, body: string, tag: string | undefined): boolean;
  /** Detect installed open-in apps (with icons). */
  detectOpenInApps(): Promise<DetectedOpenInApp[]>;
  setBadgeCount(count: number): boolean | Promise<boolean>;
  dockBounce(kind: 'informational' | 'critical'): void;
  setLoginItem(open: boolean): void;
  getLoginItem(): boolean;
  env: Record<string, string | undefined>;
  platform: NodeJS.Platform;
}

type Handler = (payload: unknown) => Promise<unknown>;

export type DesktopIpcHandlers = Record<string, Handler>;

function invalid(what: string): never {
  throw new Error(`desktop IPC ${what}: invalid payload`);
}

export function createDesktopIpcHandlers(deps: DesktopIpcDeps): DesktopIpcHandlers {
  return {
    [IPC_CHANNELS.openPath]: async (payload) => {
      const parsed = validatePathPayload(payload) ?? invalid('openPath');
      const error = await deps.shellOpenPath(parsed.path);
      if (error !== '') throw new Error(error);
      return { opened: true };
    },

    [IPC_CHANNELS.revealPath]: async (payload) => {
      const parsed = validatePathPayload(payload) ?? invalid('revealPath');
      deps.shellRevealPath(parsed.path);
      return { revealed: true };
    },

    [IPC_CHANNELS.openInEditor]: async (payload) => {
      const parsed = validatePathPayload(payload) ?? invalid('openInEditor');
      const launch = editorLaunchFor(parsed.path, parsed.line, deps.env, deps.platform);
      if (launch !== null) {
        // A missing editor binary is not fatal — fall back to the OS default
        // app for the file type (same contract as the v1 daemon).
        try {
          await deps.launch(launch);
          return { opened: true };
        } catch {
          // fall through to openPath
        }
      }
      const error = await deps.shellOpenPath(parsed.path);
      if (error !== '') throw new Error(error);
      return { opened: true };
    },

    [IPC_CHANNELS.openInApp]: async (payload) => {
      const parsed = validateOpenInAppPayload(payload) ?? invalid('openInApp');
      await deps.launch(openInAppLaunch(parsed.appId, parsed.path, {}, deps.platform));
      return { opened: true };
    },

    [IPC_CHANNELS.listOpenInApps]: async () => {
      return deps.detectOpenInApps();
    },

    [IPC_CHANNELS.openExternal]: async (payload) => {
      const url = validateExternalUrlPayload(payload) ?? invalid('openExternal');
      await deps.shellOpenExternal(url);
      return { opened: true };
    },

    [IPC_CHANNELS.notify]: async (payload) => {
      const parsed = validateNotifyPayload(payload) ?? invalid('notify');
      const shown = deps.showNotification(parsed.title, parsed.body, parsed.tag);
      return { shown };
    },

    [IPC_CHANNELS.setBadgeCount]: async (payload) => {
      const count = validateBadgeCountPayload(payload) ?? invalid('setBadgeCount');
      const set = await deps.setBadgeCount(count);
      return { set };
    },

    [IPC_CHANNELS.dockBounce]: async (payload) => {
      const kind =
        payload === undefined || payload === null || (payload as { kind?: unknown })?.kind ===
        undefined
          ? 'informational'
          : (payload as { kind?: unknown }).kind;
      if (kind !== 'informational' && kind !== 'critical') invalid('dockBounce');
      deps.dockBounce(kind);
      return { bounced: true };
    },

    [IPC_CHANNELS.setLoginItem]: async (payload) => {
      const open = validateLoginItemPayload(payload) ?? invalid('setLoginItem');
      deps.setLoginItem(open);
      return { open };
    },

    [IPC_CHANNELS.getLoginItem]: async () => {
      return { open: deps.getLoginItem() };
    },
  };
}

// Re-exported for the wiring module's convenience (single import site).
export { launchDetached };
