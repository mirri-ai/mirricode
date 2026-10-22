import { describe, expect, it, vi } from 'vitest';

import { detectInstalledOpenInApps, OPEN_IN_APP_REGISTRY } from './open-in-apps';

// 检测依赖注入：目录里"存在"哪些 .app 由 existsSync 决定
function depsWith(appBundles: string[]) {
  const installed = new Set(appBundles);
  return {
    platform: 'darwin' as NodeJS.Platform,
    existsSync: (p: string) => installed.has(p),
    applicationsDirs: ['/Applications', '/Users/demo/Applications'],
    getAppBundleIcon: vi.fn(async () => 'data:image/png;base64,AAA'),
  };
}

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
  it('should contain the ten supported apps when fully enumerated', () => {
    expect(OPEN_IN_APP_REGISTRY.map((a) => a.appId)).toEqual([
      'finder', 'vscode', 'cursor', 'zed', 'sublime',
      'intellij', 'webstorm', 'iterm', 'terminal', 'warp',
    ]);
  });

  it('should never duplicate an appId when the registry is enumerated', () => {
    const ids = OPEN_IN_APP_REGISTRY.map((a) => a.appId);
    expect(new Set(ids).size).toBe(ids.length);
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

  it('should attach an extracted icon data url when getAppBundleIcon resolves', async () => {
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
    expect(deps.getAppBundleIcon).toHaveBeenCalledWith(
      '/System/Library/CoreServices/Finder.app',
    );
  });

  it('should extract the icon for the system Terminal from its absolute bundle path', async () => {
    const deps = depsWith([]);
    const apps = await detectInstalledOpenInApps(deps);
    const terminal = apps.find((a) => a.appId === 'terminal');
    expect(terminal?.iconDataUrl).toBe('data:image/png;base64,AAA');
    expect(deps.getAppBundleIcon).toHaveBeenCalledWith(
      '/System/Applications/Utilities/Terminal.app',
    );
  });

  it('should stay available without an icon when the system bundle path does not exist', async () => {
    const deps = depsWith([]);
    deps.getAppBundleIcon = vi.fn(async () => { throw new Error('no icon'); });
    const apps = await detectInstalledOpenInApps(deps);
    // 系统组件永远可用，图标提取失败不影响条目存在
    const finder = apps.find((a) => a.appId === 'finder');
    expect(finder).toBeDefined();
    expect(finder?.iconDataUrl).toBeUndefined();
  });

  it('should omit the icon gracefully when getAppBundleIcon rejects', async () => {
    const deps = depsWith(['/Applications/Zed.app']);
    deps.getAppBundleIcon = vi.fn(async () => { throw new Error('no icon'); });
    const apps = await detectInstalledOpenInApps(deps);
    const zed = apps.find((a) => a.appId === 'zed');
    expect(zed).toBeDefined();
    expect(zed?.iconDataUrl).toBeUndefined();
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

  it('should not throw when a registry entry has no mac bundle defined', async () => {
    // finder 故意无 bundle（系统组件，永远可用）——空扫描不应抛错
    await expect(detectInstalledOpenInApps(depsWith([]))).resolves.toBeTruthy();
  });
});
