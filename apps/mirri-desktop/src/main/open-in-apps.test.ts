import { describe, expect, it, vi } from 'vitest';

import {
  detectInstalledOpenInApps,
  OPEN_IN_APP_REGISTRY,
  resolveMacBundleName,
  resolveWindowsExe,
} from './open-in-apps';

/** A fake filesystem: `dirs` maps a directory to its entry names, where a
 *  trailing `/` marks a subdirectory. `existsSync` answers for both files and
 *  directories so the same map drives discovery end to end. */
function fakeFs(tree: Record<string, string[]>) {
  const entries = new Map<string, Array<{ name: string; isDirectory(): boolean }>>();
  const existing = new Set<string>();
  for (const [dir, names] of Object.entries(tree)) {
    existing.add(dir);
    entries.set(
      dir,
      names.map((raw) => {
        const isDirectory = raw.endsWith('/');
        const name = isDirectory ? raw.slice(0, -1) : raw;
        existing.add(`${dir}\\${name}`);
        return { name, isDirectory: () => isDirectory };
      }),
    );
  }
  return {
    existsSync: (p: string) => existing.has(p),
    readdirSync: (p: string) => {
      const found = entries.get(p);
      if (found === undefined) throw new Error(`ENOENT: ${p}`);
      return found;
    },
  };
}

// 检测依赖注入：目录里"存在"哪些 .app 由 existsSync 决定
function depsWith(appBundles: string[]) {
  const installed = new Set(appBundles);
  return {
    platform: 'darwin' as NodeJS.Platform,
    existsSync: (p: string) => installed.has(p),
    readdirSync: () => [],
    env: {},
    applicationsDirs: ['/Applications', '/Users/demo/Applications'],
    getAppIcon: vi.fn(async () => 'data:image/png;base64,AAA'),
  };
}

/** Windows deps: only the listed absolute paths exist; env feeds %VAR%. */
function windowsDeps(env: Record<string, string>, existing: string[]) {
  const installed = new Set(existing);
  return {
    platform: 'win32' as NodeJS.Platform,
    existsSync: (p: string) => installed.has(p),
    readdirSync: () => [],
    env,
    getAppIcon: vi.fn(async () => 'data:image/png;base64,AAA'),
  };
}

const WIN_ENV = {
  LOCALAPPDATA: 'C:\\Users\\demo\\AppData\\Local',
  ProgramFiles: 'C:\\Program Files',
};

const BS = '\\';
const IDEA_STANDALONE = `${WIN_ENV.ProgramFiles}${BS}JetBrains${BS}IntelliJ IDEA 2024.1`;
const TOOLBOX_ROOT = `${WIN_ENV.LOCALAPPDATA}${BS}JetBrains${BS}Toolbox${BS}apps`;

// 注册表内 bundle 显示名 → 该 app 在 /Applications 下的文件名
const BUNDLE_FILE: Record<string, string> = {
  'Visual Studio Code': 'Visual Studio Code.app',
  Cursor: 'Cursor.app',
  Zed: 'Zed.app',
  'Sublime Text': 'Sublime Text.app',
  'IntelliJ IDEA': 'IntelliJ IDEA.app',
  WebStorm: 'WebStorm.app',
  iTerm: 'iTerm.app',
  Terminal: 'Terminal.app',
  Warp: 'Warp.app',
};

describe('OPEN_IN_APP_REGISTRY', () => {
  it('should contain the supported apps when fully enumerated', () => {
    expect(OPEN_IN_APP_REGISTRY.map((a) => a.appId)).toEqual([
      'finder', 'vscode', 'cursor', 'zed', 'sublime',
      'intellij', 'webstorm', 'pycharm', 'goland', 'phpstorm',
      'clion', 'rider', 'rustrover',
      'iterm', 'terminal', 'warp',
    ]);
  });

  it('should never duplicate an appId when the registry is enumerated', () => {
    const ids = OPEN_IN_APP_REGISTRY.map((a) => a.appId);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('should give every non-system editor a macOS bundle when it has one', () => {
    // 系统组件（finder/terminal）走绝对路径，其余编辑器/终端必须能 `open -a`。
    for (const app of OPEN_IN_APP_REGISTRY) {
      if (app.kind === 'file-manager' || app.macSystemBundlePath !== undefined) continue;
      expect(app.macAppBundles?.length, app.appId).toBeGreaterThan(0);
    }
  });
});

describe('detectInstalledOpenInApps', () => {
  it('should report finder and terminal on macOS when they always exist', async () => {
    const apps = await detectInstalledOpenInApps(depsWith([]));
    const ids = apps.map((a) => a.appId);
    expect(ids).toContain('finder');
    expect(ids).toContain('terminal');
  });

  it('should report an editor when its bundle exists in either applications dir', async () => {
    const apps = await detectInstalledOpenInApps(
      depsWith(['/Users/demo/Applications/Zed.app']),
    );
    expect(apps.map((a) => a.appId)).toContain('zed');
  });

  it('should omit apps whose bundle is absent when scanning both dirs', async () => {
    const apps = await detectInstalledOpenInApps(
      depsWith(['/Applications/Cursor.app']),
    );
    const ids = apps.map((a) => a.appId);
    expect(ids).toContain('cursor');
    expect(ids).not.toContain('vscode');
    expect(ids).not.toContain('zed');
  });

  it('should attach an extracted icon data url when the icon extractor resolves', async () => {
    const apps = await detectInstalledOpenInApps(depsWith(['/Applications/Zed.app']));
    const zed = apps.find((a) => a.appId === 'zed');
    expect(zed?.iconDataUrl).toBe('data:image/png;base64,AAA');
  });

  it('should extract the icon for the system Finder from its absolute bundle path', async () => {
    const deps = depsWith([]);
    const apps = await detectInstalledOpenInApps(deps);
    const finder = apps.find((a) => a.appId === 'finder');
    expect(finder?.iconDataUrl).toBe('data:image/png;base64,AAA');
    // 图标提取走注册表里的系统 bundle 绝对路径，而非扫描目录
    expect(deps.getAppIcon).toHaveBeenCalledWith(
      '/System/Library/CoreServices/Finder.app',
    );
  });

  it('should extract the icon for the system Terminal from its absolute bundle path', async () => {
    const deps = depsWith([]);
    const apps = await detectInstalledOpenInApps(deps);
    const terminal = apps.find((a) => a.appId === 'terminal');
    expect(terminal?.iconDataUrl).toBe('data:image/png;base64,AAA');
    expect(deps.getAppIcon).toHaveBeenCalledWith(
      '/System/Applications/Utilities/Terminal.app',
    );
  });

  it('should stay available without an icon when the system bundle path does not exist', async () => {
    const deps = depsWith([]);
    deps.getAppIcon = vi.fn(async () => { throw new Error('no icon'); });
    const apps = await detectInstalledOpenInApps(deps);
    // 系统组件永远可用，图标提取失败不影响条目存在
    const finder = apps.find((a) => a.appId === 'finder');
    expect(finder).toBeDefined();
    expect(finder?.iconDataUrl).toBeUndefined();
  });

  it('should omit the icon gracefully when getAppIcon rejects', async () => {
    const deps = depsWith(['/Applications/Zed.app']);
    deps.getAppIcon = vi.fn(async () => { throw new Error('no icon'); });
    const apps = await detectInstalledOpenInApps(deps);
    const zed = apps.find((a) => a.appId === 'zed');
    expect(zed).toBeDefined();
    expect(zed?.iconDataUrl).toBeUndefined();
  });

  it('should report a JetBrains IDE installed under its Community Edition bundle', async () => {
    const apps = await detectInstalledOpenInApps(depsWith(['/Applications/PyCharm CE.app']));
    // 社区版 bundle 名不同，但仍是同一个 PyCharm 条目
    expect(apps.map((a) => a.appId)).toContain('pycharm');
  });

  it('should return only the file manager when the platform is not macOS', async () => {
    const apps = await detectInstalledOpenInApps({
      ...depsWith(Object.values(BUNDLE_FILE).map((f) => `/Applications/${f}`)),
      platform: 'linux',
    });
    // 非 macOS：注册表里 mac-only 的编辑器/终端全部不可用，仅 finder 类文件管理器保留
    expect(apps.map((a) => a.appId)).toEqual(['finder']);
    // The macOS app name must not leak onto other platforms.
    expect(apps[0]?.name).toBe('File Manager');
  });

  it('should label the file manager as File Explorer when the platform is Windows', async () => {
    const apps = await detectInstalledOpenInApps({
      ...depsWith([]),
      platform: 'win32',
    });
    // 非 darwin：注册表的 mac 系统 bundle 路径不参与，无图标
    expect(apps).toEqual([{ appId: 'finder', name: 'File Explorer' }]);
  });

  it('should offer a Windows editor whose exe exists', async () => {
    const apps = await detectInstalledOpenInApps(
      windowsDeps(WIN_ENV, ['C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe']),
    );
    expect(apps.map((a) => a.appId)).toEqual(['finder', 'vscode']);
    expect(apps.find((a) => a.appId === 'vscode')?.iconDataUrl).toBe('data:image/png;base64,AAA');
  });

  it('should omit a Windows editor whose exe is absent', async () => {
    const apps = await detectInstalledOpenInApps(windowsDeps(WIN_ENV, []));
    expect(apps.map((a) => a.appId)).toEqual(['finder']);
  });

  it('should skip Windows exe candidates whose env variable is unset', async () => {
    const apps = await detectInstalledOpenInApps(
      windowsDeps({}, ['C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe']),
    );
    // 无法展开 %LOCALAPPDATA% 时不能凭一个"恰好存在"的字面路径认定已安装
    expect(apps.map((a) => a.appId)).toEqual(['finder']);
  });

  it('should expand env variables case-insensitively when resolving Windows exes', () => {
    const exe = resolveWindowsExe('vscode', {
      ...fakeFs({}),
      env: { LOCALAPPDATA: 'C:\\Users\\demo\\AppData\\Local', PROGRAMFILES: 'C:\\Program Files' },
    });
    // 两个变量都存在，但 fs 里没有任何 exe —— 只断言展开结果由大小写不敏感的查找驱动
    expect(exe).toBeNull();
    const found = resolveWindowsExe('vscode', {
      ...fakeFs({
        'C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code': ['Code.exe'],
      }),
      env: { LocalAppData: 'C:\\Users\\demo\\AppData\\Local' },
    });
    expect(found).toBe('C:\\Users\\demo\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe');
  });

  it('should find a JetBrains IDE installed under a versioned standalone directory', async () => {
    const jetbrainsRoot = `${WIN_ENV.ProgramFiles}${BS}JetBrains`;
    const tree = {
      [jetbrainsRoot]: ['IntelliJ IDEA 2024.1/'],
      [IDEA_STANDALONE]: ['bin/'],
      [`${IDEA_STANDALONE}${BS}bin`]: ['idea64.exe'],
    };
    const apps = await detectInstalledOpenInApps({
      platform: 'win32',
      ...fakeFs(tree),
      env: WIN_ENV,
      getAppIcon: vi.fn(async () => 'data:image/png;base64,AAA'),
    });
    expect(apps.map((a) => a.appId)).toEqual(['finder', 'intellij']);
    expect(resolveWindowsExe('intellij', { ...fakeFs(tree), env: WIN_ENV })).toBe(
      `${IDEA_STANDALONE}${BS}bin${BS}idea64.exe`,
    );
  });

  it('should find a JetBrains IDE inside the Toolbox app store tree', async () => {
    const toolboxIdea = `${TOOLBOX_ROOT}${BS}IDEA-U${BS}ch-0${BS}241.15989.150`;
    const apps = await detectInstalledOpenInApps({
      platform: 'win32',
      ...fakeFs({
        [TOOLBOX_ROOT]: ['IDEA-U/'],
        [`${TOOLBOX_ROOT}${BS}IDEA-U`]: ['ch-0/'],
        [`${TOOLBOX_ROOT}${BS}IDEA-U${BS}ch-0`]: ['241.15989.150/'],
        [toolboxIdea]: ['bin/'],
        [`${toolboxIdea}${BS}bin`]: ['idea64.exe'],
      }),
      env: WIN_ENV,
      getAppIcon: vi.fn(async () => 'data:image/png;base64,AAA'),
    });
    expect(apps.map((a) => a.appId)).toEqual(['finder', 'intellij']);
  });

  it('should match JetBrains exe names case-insensitively', async () => {
    const jetbrainsRoot = `${WIN_ENV.ProgramFiles}${BS}JetBrains`;
    const apps = await detectInstalledOpenInApps({
      platform: 'win32',
      ...fakeFs({
        [jetbrainsRoot]: ['IntelliJ IDEA 2024.1/'],
        [IDEA_STANDALONE]: ['bin/'],
        [`${IDEA_STANDALONE}${BS}bin`]: ['IDEA64.EXE'],
      }),
      env: WIN_ENV,
      getAppIcon: vi.fn(async () => undefined),
    });
    expect(apps.map((a) => a.appId)).toContain('intellij');
  });

  it('should find RustRover through its exe name in the JetBrains tree', async () => {
    const jetbrainsRoot = `${WIN_ENV.ProgramFiles}${BS}JetBrains`;
    const install = `${jetbrainsRoot}${BS}RustRover 2024.3`;
    const apps = await detectInstalledOpenInApps({
      platform: 'win32',
      ...fakeFs({
        [jetbrainsRoot]: ['RustRover 2024.3/'],
        [install]: ['bin/'],
        [`${install}${BS}bin`]: ['rustrover64.exe'],
      }),
      env: WIN_ENV,
      getAppIcon: vi.fn(async () => undefined),
    });
    expect(apps.map((a) => a.appId)).toContain('rustrover');
    expect(resolveWindowsExe('rustrover', {
      ...fakeFs({
        [jetbrainsRoot]: ['RustRover 2024.3/'],
        [install]: ['bin/'],
        [`${install}${BS}bin`]: ['rustrover64.exe'],
      }),
      env: WIN_ENV,
    })).toBe(`${install}${BS}bin${BS}rustrover64.exe`);
  });

  it('should stop searching a directory tree that cannot be read', async () => {
    // 权限不足的目录不能让整个检测抛错 —— 探测必须尽力而为。
    const deps = {
      platform: 'win32' as NodeJS.Platform,
      existsSync: (p: string) => p === WIN_ENV.ProgramFiles + BS + 'JetBrains',
      readdirSync: () => { throw new Error('EACCES'); },
      env: WIN_ENV,
      getAppIcon: vi.fn(async () => undefined),
    };
    await expect(detectInstalledOpenInApps(deps)).resolves.toEqual([
      { appId: 'finder', name: 'File Explorer' },
    ]);
  });

  it('should not throw when a registry entry has no mac bundle defined', async () => {
    // finder 故意无 bundle（系统组件，永远可用）——空扫描不应抛错
    await expect(detectInstalledOpenInApps(depsWith([]))).resolves.toBeTruthy();
  });
});

describe('resolveMacBundleName', () => {
  it('should prefer the professional bundle when both editions are installed', () => {
    expect(
      resolveMacBundleName('pycharm', {
        existsSync: (p: string) => p === '/Applications/PyCharm CE.app' || p === '/Applications/PyCharm.app',
        applicationsDirs: ['/Applications'],
      }),
    ).toBe('PyCharm');
  });

  it('should resolve the Community Edition bundle when only it is installed', () => {
    expect(
      resolveMacBundleName('pycharm', {
        existsSync: (p: string) => p === '/Applications/PyCharm CE.app',
        applicationsDirs: ['/Applications'],
      }),
    ).toBe('PyCharm CE');
  });

  it('should return null when no candidate bundle is installed', () => {
    expect(
      resolveMacBundleName('pycharm', { existsSync: () => false, applicationsDirs: ['/Applications'] }),
    ).toBeNull();
  });

  it('should resolve the system Terminal from its System directory bundle', () => {
    // Terminal 不在 /Applications 下 —— 解析必须走 macSystemBundlePath，
    // 否则「Open in Terminal」会因为扫不到 bundle 而报未安装。
    expect(
      resolveMacBundleName('terminal', {
        existsSync: (p: string) => p === '/System/Applications/Utilities/Terminal.app',
        applicationsDirs: ['/Applications'],
      }),
    ).toBe('Terminal');
  });
});
