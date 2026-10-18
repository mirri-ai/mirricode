import { beforeEach, describe, expect, it, vi } from 'vitest';

import { isAutoUpdateEnabled, startAutoUpdate, type UpdaterModule } from './updater';

const mockedAutoUpdater = {
  setFeedURL: vi.fn(),
  checkForUpdatesAndNotify: vi.fn().mockResolvedValue(null),
  on: vi.fn(),
};

vi.mock('electron-updater', () => ({
  autoUpdater: mockedAutoUpdater,
}));

describe('isAutoUpdateEnabled', () => {
  it('should stay off by default (no env, packaged)', () => {
    expect(isAutoUpdateEnabled({}, true)).toBe(false);
  });

  it('should stay off in dev even with the env set', () => {
    expect(
      isAutoUpdateEnabled({ MIRRICODE_DESKTOP_UPDATE_URL: 'https://updates.example.com' }, false),
    ).toBe(false);
  });

  it('should enable only when packaged AND the env URL is set', () => {
    const env = { MIRRICODE_DESKTOP_UPDATE_URL: 'https://updates.example.com' };
    expect(isAutoUpdateEnabled(env, true)).toBe(true);
    expect(isAutoUpdateEnabled({ ...env, MIRRICODE_DESKTOP_UPDATE_URL: '  ' }, true)).toBe(false);
  });
});

describe('startAutoUpdate', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('should configure the feed URL and start a check', async () => {
    const result = await startAutoUpdate('https://updates.example.com/mirri-desktop');
    expect(result).toBe(true);
    expect(mockedAutoUpdater.setFeedURL).toHaveBeenCalledWith({
      provider: 'generic',
      url: 'https://updates.example.com/mirri-desktop',
    });
    expect(mockedAutoUpdater.checkForUpdatesAndNotify).toHaveBeenCalledTimes(1);
  });

  it('should degrade gracefully when electron-updater is unavailable', async () => {
    const brokenImporter = async (): Promise<UpdaterModule> => {
      throw new Error('module not shipped in this build');
    };
    await expect(startAutoUpdate('https://updates.example.com/x', brokenImporter)).resolves.toBe(
      false,
    );
  });

  it('should degrade gracefully when the check itself throws', async () => {
    const failing = {
      autoUpdater: {
        ...mockedAutoUpdater,
        checkForUpdatesAndNotify: vi.fn().mockRejectedValue(new Error('network down')),
      },
    };
    const importer = async (): Promise<UpdaterModule> => failing;
    await expect(startAutoUpdate('https://updates.example.com/x', importer)).resolves.toBe(false);
  });
});
