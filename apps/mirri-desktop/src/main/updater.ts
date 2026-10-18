// apps/mirri-desktop/src/main/updater.ts
//
// Auto-update TECHNICAL READINESS only — no real distribution channel is
// wired (user decision gate). The check stays completely off unless BOTH:
//   - the app is packaged (dev builds never phone anywhere), and
//   - MIRRICODE_DESKTOP_UPDATE_URL points at a generic-provider feed.
//
// electron-updater is dynamically imported so a build that does not ship the
// module (today's builds) simply logs and stays disabled instead of crashing
// at startup. Promoting it to a runtime dependency + shipping it is the
// remaining step once a real channel is decided.

const UPDATE_URL_ENV = 'MIRRICODE_DESKTOP_UPDATE_URL';

export interface UpdaterModule {
  autoUpdater: {
    setFeedURL(url: { provider: 'generic'; url: string }): void;
    checkForUpdatesAndNotify(): Promise<unknown>;
    on(event: string, listener: (...args: unknown[]) => void): unknown;
  };
}

/** True only for packaged apps with an explicit update feed URL. */
export function isAutoUpdateEnabled(
  env: Record<string, string | undefined>,
  isPackaged: boolean,
): boolean {
  if (!isPackaged) return false;
  const url = env[UPDATE_URL_ENV];
  return typeof url === 'string' && url.trim().length > 0;
}

/** Configure and start the updater. Resolves false (never rejects) when
 *  electron-updater is unavailable or the first check fails — the app must
 *  run fine without updates. */
export async function startAutoUpdate(
  url: string,
  importUpdater: () => Promise<UpdaterModule> = () => import('electron-updater'),
): Promise<boolean> {
  try {
    const { autoUpdater } = await importUpdater();
    autoUpdater.setFeedURL({ provider: 'generic', url: url.trim() });
    autoUpdater.on('error', (error: unknown) => {
      process.stderr.write(`[mirri-desktop] updater error: ${String(error)}\n`);
    });
    await autoUpdater.checkForUpdatesAndNotify();
    process.stdout.write(`[mirri-desktop] auto-update check started (${url.trim()})\n`);
    return true;
  } catch (error) {
    process.stderr.write(
      `[mirri-desktop] auto-update unavailable: ${error instanceof Error ? error.message : String(error)}\n`,
    );
    return false;
  }
}
