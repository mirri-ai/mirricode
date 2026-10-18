import { describe, expect, it, vi } from 'vitest';

import { buildTrayMenuTemplate, type TrayMenuHandlers } from './tray-menu';

function makeHandlers(): TrayMenuHandlers {
  return {
    onShowMainWindow: vi.fn(),
    onToggleLoginItem: vi.fn(),
    onQuit: vi.fn(),
  };
}

describe('buildTrayMenuTemplate', () => {
  it('should show the main window, toggle login item, and quit', () => {
    const handlers = makeHandlers();
    const template = buildTrayMenuTemplate(handlers, { loginItemOn: false });

    const items = template as Array<Record<string, unknown>>;
    const show = items.find((i) => i['label'] === '显示主窗口');
    expect(show).toBeDefined();
    (show!['click'] as () => void)();
    expect(handlers.onShowMainWindow).toHaveBeenCalledTimes(1);

    const login = items.find((i) => i['label'] === '开机自动启动');
    expect(login).toBeDefined();
    expect(login!['type']).toBe('checkbox');
    expect(login!['checked']).toBe(false);
    (login!['click'] as (item: { checked: boolean }) => void)({ checked: true });
    expect(handlers.onToggleLoginItem).toHaveBeenCalledWith(true);

    const quit = items.find((i) => i['label'] === '退出 Mirri Code');
    expect(quit).toBeDefined();
    (quit!['click'] as () => void)();
    expect(handlers.onQuit).toHaveBeenCalledTimes(1);
  });

  it('should reflect the current login item state', () => {
    const template = buildTrayMenuTemplate(makeHandlers(), { loginItemOn: true });
    const items = template as Array<Record<string, unknown>>;
    const login = items.find((i) => i['label'] === '开机自动启动');
    expect(login!['checked']).toBe(true);
  });

  it('should place a separator between window actions and quit', () => {
    const items = buildTrayMenuTemplate(makeHandlers(), { loginItemOn: false }) as Array<
      Record<string, unknown>
    >;
    expect(items.some((i) => i['type'] === 'separator')).toBe(true);
    const quitIndex = items.findIndex((i) => i['label'] === '退出 Mirri Code');
    const separatorIndex = items.findIndex((i) => i['type'] === 'separator');
    expect(separatorIndex).toBeGreaterThan(-1);
    expect(quitIndex).toBeGreaterThan(separatorIndex);
  });
});
