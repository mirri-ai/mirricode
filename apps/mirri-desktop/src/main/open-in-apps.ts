// apps/mirri-desktop/src/main/open-in-apps.ts
//
// The "open in" app registry + installed-app detection. Pure logic with
// injected fs/icon deps so it is unit-testable without Electron; register-ipc.ts
// supplies the real implementations (fs.existsSync / fs.readdirSync /
// readAppBundleIcon).

export interface OpenInAppDescriptor {
  readonly appId: string;
  /** Display name in the "Open in" dropdown. */
  readonly name: string;
  readonly kind: 'file-manager' | 'editor' | 'terminal';
  /** macOS `.app` bundle display names, in preference order. The first one
   *  found in the scanned application directories is what `open -a` gets;
   *  multiple entries cover editions that ship under a different bundle name
   *  (JetBrains' `IntelliJ IDEA` vs `IntelliJ IDEA CE`). Finder has none — it
   *  is always available and opened via the dedicated file-manager path;
   *  Terminal is a system app and likewise always available. Neither is
   *  scanned for. */
  readonly macAppBundles?: readonly string[];
  /** Absolute bundle path for system apps that live outside the scanned
   * application directories (Finder, Terminal). These are always available;
   * the path exists purely so their icons can be extracted like any other
   * registry entry. */
  readonly macSystemBundlePath?: string;
  /** Windows executable candidates as `%VAR%`-template paths (see
   * `expandWindowsPath`). Windows has no bundle directory to scan, so
   * availability is "one of these exes exists". Apps without `winExe` or
   * `winSearch` are never offered on Windows. */
  readonly winExe?: readonly string[];
  /** Windows discovery for installs whose path carries a version / build
   *  number (JetBrains ships `…/IntelliJ IDEA 2024.1/bin/idea64.exe`, or a
   *  Toolbox tree with the build number), where no fixed path applies.
   *  `roots` are `%VAR%`-templates; each is walked for a file named one of
   *  `exeNames`. */
  readonly winSearch?: {
    readonly roots: readonly string[];
    readonly exeNames: readonly string[];
  };
}

/** Roots holding JetBrains IDE installations on Windows: the standalone
 *  installer tree, and the Toolbox app store (whose per-product folder names
 *  are Toolbox-internal codes like `IDEA-U`, so the search does not assume
 *  them). Both embed the version / build number in the path. */
const JETBRAINS_WIN_ROOTS: readonly string[] = [
  '%ProgramFiles%/JetBrains',
  '%LOCALAPPDATA%/JetBrains/Toolbox/apps',
];

export const OPEN_IN_APP_REGISTRY: readonly OpenInAppDescriptor[] = [
  {
    appId: 'finder',
    name: 'Finder',
    kind: 'file-manager',
    macSystemBundlePath: '/System/Library/CoreServices/Finder.app',
  },
  {
    appId: 'vscode',
    name: 'VS Code',
    kind: 'editor',
    macAppBundles: ['Visual Studio Code'],
    winExe: [
      '%LOCALAPPDATA%/Programs/Microsoft VS Code/Code.exe',
      '%ProgramFiles%/Microsoft VS Code/Code.exe',
    ],
  },
  {
    appId: 'cursor',
    name: 'Cursor',
    kind: 'editor',
    macAppBundles: ['Cursor'],
    winExe: ['%LOCALAPPDATA%/Programs/cursor/Cursor.exe', '%LOCALAPPDATA%/Programs/Cursor/Cursor.exe'],
  },
  {
    appId: 'zed',
    name: 'Zed',
    kind: 'editor',
    macAppBundles: ['Zed'],
    winExe: ['%LOCALAPPDATA%/Programs/Zed/Zed.exe'],
  },
  {
    appId: 'sublime',
    name: 'Sublime Text',
    kind: 'editor',
    macAppBundles: ['Sublime Text'],
    winExe: [
      '%ProgramFiles%/Sublime Text/sublime_text.exe',
      '%ProgramFiles%/Sublime Text 3/sublime_text.exe',
    ],
  },
  {
    appId: 'intellij',
    name: 'IntelliJ IDEA',
    kind: 'editor',
    macAppBundles: ['IntelliJ IDEA', 'IntelliJ IDEA CE', 'IntelliJ IDEA Ultimate'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['idea64.exe', 'idea.exe'] },
  },
  {
    appId: 'webstorm',
    name: 'WebStorm',
    kind: 'editor',
    macAppBundles: ['WebStorm'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['webstorm64.exe', 'webstorm.exe'] },
  },
  {
    appId: 'pycharm',
    name: 'PyCharm',
    kind: 'editor',
    macAppBundles: ['PyCharm', 'PyCharm CE', 'PyCharm Professional'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['pycharm64.exe', 'pycharm.exe'] },
  },
  {
    appId: 'goland',
    name: 'GoLand',
    kind: 'editor',
    macAppBundles: ['GoLand'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['goland64.exe', 'goland.exe'] },
  },
  {
    appId: 'phpstorm',
    name: 'PhpStorm',
    kind: 'editor',
    macAppBundles: ['PhpStorm'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['phpstorm64.exe', 'phpstorm.exe'] },
  },
  {
    appId: 'clion',
    name: 'CLion',
    kind: 'editor',
    macAppBundles: ['CLion'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['clion64.exe', 'clion.exe'] },
  },
  {
    appId: 'rider',
    name: 'Rider',
    kind: 'editor',
    macAppBundles: ['Rider'],
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['rider64.exe', 'rider.exe'] },
  },
  {
    appId: 'rustrover',
    name: 'RustRover',
    kind: 'editor',
    macAppBundles: ['RustRover'],
    // Recent JetBrains releases ship `rustrover.exe`; older ones used the
    // `64` suffix, so both spellings are listed.
    winSearch: { roots: JETBRAINS_WIN_ROOTS, exeNames: ['rustrover.exe', 'rustrover64.exe'] },
  },
  { appId: 'iterm', name: 'iTerm', kind: 'terminal', macAppBundles: ['iTerm'] },
  {
    appId: 'terminal',
    name: 'Terminal',
    kind: 'terminal',
    macSystemBundlePath: '/System/Applications/Utilities/Terminal.app',
    macAppBundles: ['Terminal'],
  },
  { appId: 'warp', name: 'Warp', kind: 'terminal', macAppBundles: ['Warp'] },
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
  /** Directory listing for Windows versioned-install discovery (see
   *  `winSearch`). Only the name + directory flag are used. */
  readdirSync(path: string): ReadonlyArray<{ name: string; isDirectory(): boolean }>;
  /** Environment for `%VAR%` expansion in Windows exe templates. */
  env: Record<string, string | undefined>;
  /** Directories to scan for `<Name>.app` bundles. Defaults to `/Applications`
   * only; production wiring passes the full dir list explicitly. */
  readonly applicationsDirs?: readonly string[];
  /** Icon extractor taking an app bundle or executable path; absent/throwing
   * → icon omitted. */
  getAppIcon?(appPath: string): Promise<string | undefined>;
}

const DEFAULT_APPLICATION_DIRS = ['/Applications'];

/** Bundle file name for a display name: `Visual Studio Code` → `Visual Studio Code.app`. */
function bundleFileName(appName: string): string {
  return `${appName}.app`;
}

/** Env lookup, case-insensitive: Windows' `process.env` is case-insensitive
 *  while the injected test maps are plain objects. */
function lookupEnv(
  env: Record<string, string | undefined>,
  name: string,
): string | undefined {
  const exact = env[name];
  if (typeof exact === 'string' && exact.length > 0) return exact;
  const upper = name.toUpperCase();
  for (const [key, value] of Object.entries(env)) {
    if (key.toUpperCase() === upper && typeof value === 'string' && value.length > 0) return value;
  }
  return undefined;
}

/** `%LOCALAPPDATA%/Programs/…/Code.exe` → `C:\Users\…\Programs\…\Code.exe`.
 *  null when a referenced variable is unset (the candidate is unusable). */
function expandWindowsPath(
  template: string,
  env: Record<string, string | undefined>,
): string | null {
  let missing = false;
  const expanded = template.replaceAll(/%([A-Za-z_][A-Za-z0-9_]*)%/g, (_match, name: string) => {
    const value = lookupEnv(env, name);
    if (value === undefined) {
      missing = true;
      return '';
    }
    return value;
  });
  return missing ? null : expanded.replaceAll('/', '\\');
}

/** Bounds for the `winSearch` walk: a JetBrains install nests at most
 *  `…/apps/<product>/<channel>/<build>/bin/<exe>`, and the caps keep a stray
 *  huge directory from stalling app detection. */
const WIN_SEARCH_MAX_DEPTH = 4;
const WIN_SEARCH_MAX_DIRS = 300;

/** Walk `root` for a file named one of `exeNames`, at most `maxDepth`
 *  directories deep. Returns null when nothing matches, the root is absent, a
 *  listing fails (permission), or the directory cap is hit — discovery is
 *  best-effort and must never stall or throw. */
function findFileInTree(
  root: string,
  exeNames: readonly string[],
  deps: Pick<OpenInAppsDeps, 'existsSync' | 'readdirSync'>,
): string | null {
  if (!deps.existsSync(root)) return null;
  const lowerNames = new Set(exeNames.map((n) => n.toLowerCase()));
  const queue: Array<{ dir: string; depth: number }> = [{ dir: root, depth: 0 }];
  let scannedDirs = 0;

  while (queue.length > 0) {
    const current = queue.shift()!;
    scannedDirs += 1;
    if (scannedDirs > WIN_SEARCH_MAX_DIRS) return null;

    let entries: ReadonlyArray<{ name: string; isDirectory(): boolean }>;
    try {
      entries = deps.readdirSync(current.dir);
    } catch {
      continue; // unreadable subdirectory — skip, keep walking its siblings
    }

    // Windows filesystems are case-insensitive; compare folded names so the
    // registry does not have to enumerate every casing of an exe.
    const hit = entries.find(
      (e) => !e.isDirectory() && lowerNames.has(e.name.toLowerCase()),
    );
    if (hit !== undefined) return `${current.dir}\\${hit.name}`;

    if (current.depth >= WIN_SEARCH_MAX_DEPTH) continue;
    for (const entry of entries) {
      if (entry.isDirectory()) {
        queue.push({ dir: `${current.dir}\\${entry.name}`, depth: current.depth + 1 });
      }
    }
  }
  return null;
}

/** The installed Windows executable for a registry app, or null when the app
 *  is not installed (or has no Windows discovery). Shared by detection and
 *  the launch resolver so both agree on which exe is "the app". */
export function resolveWindowsExe(
  appId: string,
  deps: Pick<OpenInAppsDeps, 'existsSync' | 'readdirSync' | 'env'>,
): string | null {
  const app = OPEN_IN_APP_REGISTRY.find((a) => a.appId === appId);
  if (app === undefined) return null;

  for (const template of app.winExe ?? []) {
    const candidate = expandWindowsPath(template, deps.env);
    if (candidate !== null && deps.existsSync(candidate)) return candidate;
  }

  if (app.winSearch !== undefined) {
    for (const template of app.winSearch.roots) {
      const root = expandWindowsPath(template, deps.env);
      if (root === null) continue;
      const found = findFileInTree(root, app.winSearch.exeNames, deps);
      if (found !== null) return found;
    }
  }
  return null;
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

/** The macOS bundle a registry app should launch: the system bundle when the
 *  app is a macOS system component (Terminal lives outside the scanned
 *  directories), otherwise `<dir>/<name>.app` for the first candidate bundle
 *  found in the scanned directories. null when nothing is installed. The
 *  resolved name is what `open -a` receives, so a Community Edition install
 *  launches as `PyCharm CE` rather than failing on `PyCharm`. */
export function resolveMacBundleName(
  appId: string,
  deps: Pick<OpenInAppsDeps, 'existsSync' | 'applicationsDirs'>,
): string | null {
  const app = OPEN_IN_APP_REGISTRY.find((a) => a.appId === appId);
  if (app === undefined) return null;

  // macOS 系统组件（Terminal）装在 System 目录，不在扫描范围内，条目本身就是名字。
  if (app.macSystemBundlePath !== undefined) {
    return deps.existsSync(app.macSystemBundlePath)
      ? (app.macAppBundles?.[0] ?? null)
      : null;
  }

  if (app.macAppBundles === undefined) return null;
  const dirs = deps.applicationsDirs ?? DEFAULT_APPLICATION_DIRS;
  for (const name of app.macAppBundles) {
    if (dirs.some((dir) => deps.existsSync(`${dir}/${bundleFileName(name)}`))) return name;
  }
  return null;
}

export async function detectInstalledOpenInApps(
  deps: OpenInAppsDeps,
): Promise<DetectedOpenInApp[]> {
  const detected: DetectedOpenInApp[] = [];

  for (const app of OPEN_IN_APP_REGISTRY) {
    // Finder（文件管理器）与 Terminal（macOS 系统自带 Terminal.app）是系统组件，
    // 永远可用 —— 无 bundle 也不做扫描。两者由 open -a 直接启动。
    // macSystemBundlePath 仅用于图标提取，与可用性无关。
    if (app.kind === 'file-manager') {
      detected.push({
        appId: app.appId,
        name: fileManagerName(deps.platform),
        iconDataUrl: await extractIcon(deps, deps.platform === 'darwin' ? app.macSystemBundlePath : undefined),
      });
      continue;
    }
    if (app.appId === 'terminal' && deps.platform === 'darwin') {
      detected.push({
        appId: app.appId,
        name: app.name,
        iconDataUrl: await extractIcon(deps, app.macSystemBundlePath),
      });
      continue;
    }
    if (deps.platform === 'win32') {
      // Windows：没有 .app 目录可扫，exe 命中（固定路径或版本目录搜索）即视为已安装。
      const exe = resolveWindowsExe(app.appId, deps);
      if (exe === null) continue;
      detected.push({
        appId: app.appId,
        name: app.name,
        iconDataUrl: await extractIcon(deps, exe),
      });
      continue;
    }
    if (deps.platform !== 'darwin') {
      // 其它非 macOS：注册表的 mac bundle 语义不适用，只保留文件管理器。
      continue;
    }
    const bundleName = resolveMacBundleName(app.appId, deps);
    if (bundleName === null) continue;
    // 图标走解析出的那个 bundle —— 社区版/专业版是同一个注册表条目。
    const bundlePath = (deps.applicationsDirs ?? DEFAULT_APPLICATION_DIRS)
      .map((dir) => `${dir}/${bundleFileName(bundleName)}`)
      .find((p) => deps.existsSync(p));

    detected.push({
      appId: app.appId,
      name: app.name,
      iconDataUrl: await extractIcon(deps, bundlePath),
    });
  }
  return detected;
}

/** Best-effort app icon; undefined (no icon) on any failure. */
async function extractIcon(
  deps: OpenInAppsDeps,
  appPath: string | undefined,
): Promise<string | undefined> {
  if (appPath === undefined || deps.getAppIcon === undefined) return undefined;
  try {
    return await deps.getAppIcon(appPath);
  } catch {
    // 图标提取失败不致命 —— 下拉里显示无图标条目。
  }
  return undefined;
}
