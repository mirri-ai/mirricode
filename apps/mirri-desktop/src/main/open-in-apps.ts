// apps/mirri-desktop/src/main/open-in-apps.ts
//
// The "open in" app registry + installed-app detection. Pure logic with
// injected fs/icon deps so it is unit-testable without Electron; register-ipc.ts
// supplies the real implementations (fs.existsSync + app.getFileIcon).

export interface OpenInAppDescriptor {
  readonly appId: string;
  /** Display name; also the `open -a` application name on macOS. */
  readonly name: string;
  readonly kind: 'file-manager' | 'editor' | 'terminal';
  /** macOS application display name for `open -a`. Finder has none — it is
   * always available and opened via the dedicated file-manager path;
   * Terminal is a system app (/System/Applications/Utilities/Terminal.app)
   * and likewise always available. Neither is scanned for. */
  readonly macAppBundle?: string;
}

export const OPEN_IN_APP_REGISTRY: readonly OpenInAppDescriptor[] = [
  { appId: 'finder', name: 'Finder', kind: 'file-manager' },
  { appId: 'vscode', name: 'VS Code', kind: 'editor', macAppBundle: 'Visual Studio Code' },
  { appId: 'cursor', name: 'Cursor', kind: 'editor', macAppBundle: 'Cursor' },
  { appId: 'zed', name: 'Zed', kind: 'editor', macAppBundle: 'Zed' },
  { appId: 'sublime', name: 'Sublime Text', kind: 'editor', macAppBundle: 'Sublime Text' },
  { appId: 'intellij', name: 'IntelliJ IDEA', kind: 'editor', macAppBundle: 'IntelliJ IDEA' },
  { appId: 'webstorm', name: 'WebStorm', kind: 'editor', macAppBundle: 'WebStorm' },
  { appId: 'iterm', name: 'iTerm', kind: 'terminal', macAppBundle: 'iTerm' },
  { appId: 'terminal', name: 'Terminal', kind: 'terminal', macAppBundle: 'Terminal' },
  { appId: 'warp', name: 'Warp', kind: 'terminal', macAppBundle: 'Warp' },
];

export const OPEN_IN_APP_IDS: readonly string[] = OPEN_IN_APP_REGISTRY.map((a) => a.appId);

/** Any registry member id; a plain string alias so callers can pass ids
 *  without narrowing against the const array (validated at the IPC layer). */
export type OpenInAppId = string;

export interface DetectedOpenInApp {
  readonly appId: string;
  readonly name: string;
  readonly iconDataUrl?: string;
}

export interface OpenInAppsDeps {
  platform: NodeJS.Platform;
  existsSync(path: string): boolean;
  /** Directories to scan for `<Name>.app` bundles. Defaults to `/Applications`
   * only; production wiring passes the full dir list explicitly. */
  readonly applicationsDirs?: readonly string[];
  /** Electron's app.getFileIcon, adapted. Absent/throwing → icon omitted. */
  getFileIcon?(path: string): Promise<{ toDataURL(): string } | null>;
}

const DEFAULT_APPLICATION_DIRS = ['/Applications'];

/** Bundle file name for a display name: `Visual Studio Code` → `Visual Studio Code.app`. */
function bundleFileName(appName: string): string {
  return `${appName}.app`;
}

/** Display name of the always-available system file manager on this platform. */
function fileManagerName(platform: NodeJS.Platform): string {
  switch (platform) {
    case 'darwin':
      return 'Finder';
    case 'win32':
      return 'File Explorer';
    default:
      return 'File Manager';
  }
}

export async function detectInstalledOpenInApps(
  deps: OpenInAppsDeps,
): Promise<DetectedOpenInApp[]> {
  const dirs = deps.applicationsDirs ?? DEFAULT_APPLICATION_DIRS;
  const detected: DetectedOpenInApp[] = [];

  for (const app of OPEN_IN_APP_REGISTRY) {
    // Finder（文件管理器）与 Terminal（macOS 系统自带 Terminal.app）是系统组件，
    // 永远可用 —— 无 bundle 也不做扫描。两者由 open -a 直接启动。
    if (app.kind === 'file-manager') {
      detected.push({ appId: app.appId, name: fileManagerName(deps.platform) });
      continue;
    }
    if (app.appId === 'terminal' && deps.platform === 'darwin') {
      detected.push({ appId: app.appId, name: app.name });
      continue;
    }
    if (deps.platform !== 'darwin') {
      // 非 macOS：注册表的 mac bundle 语义不适用，只保留文件管理器。
      continue;
    }
    if (app.macAppBundle === undefined) continue;
    const bundleFile = bundleFileName(app.macAppBundle);
    const found = dirs
      .map((dir) => `${dir}/${bundleFile}`)
      .find((p) => deps.existsSync(p));
    if (found === undefined) continue;

    let iconDataUrl: string | undefined;
    if (deps.getFileIcon !== undefined) {
      try {
        const icon = await deps.getFileIcon(found);
        iconDataUrl = icon?.toDataURL();
      } catch {
        // 图标提取失败不致命 —— 下拉里显示无图标条目。
      }
    }
    detected.push({ appId: app.appId, name: app.name, iconDataUrl });
  }
  return detected;
}
