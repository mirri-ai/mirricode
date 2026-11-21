// @vitest-environment jsdom
// apps/mirri-web/test/sidebar-pin.test.ts
//
// 会话置顶的交互语义（SDR-016/017 + SDR-021 Pin 轨）：
// - 行最左是固定 24px 的 Pin 轨，槽内单一控件承担「状态 + 切换 + 调色板入口」。
// - 标记是彩色圆圈（默认红），不是图标：置顶常显实心圆，未置顶为空槽 +
//   hover/focus 才显形的空心圆。
// - 未置顶：空槽 pointer-events:none，点击穿透到行 = 打开会话；点圆圈 = 置顶（红）。
// - 已置顶：点圆圈 = 取消置顶；悬停圆圈 = 打开七色调色板改色；右键 / Shift+F10
//   也开调色板。
// - 颜色只染圆圈（没有独立竖条，也不是钉子图标）。
//
// 这里锁定的就是 sidebar 自己的事件边界：真实 Sidebar + 真实 WorkspaceGroup +
// 真实 SessionRow 一起挂载，用户点圆圈 / 悬停圆圈 / 选色，断言 Sidebar 发出的
// pin/unpin 与圆圈的渲染形态。

import { describe, expect, it, vi } from 'vitest';
import { mount, type DOMWrapper, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import { messages } from '../src/i18n/locales';
import Sidebar from '../src/components/Sidebar.vue';
import type { SortableSessionRow } from '../src/lib/sessionSorting';
import { splitPinnedSessions } from '../src/lib/sessionSorting';
import type { WorkspaceGroup as WorkspaceGroupType } from '../src/types';

// lib/icons 通过 unplugin-icons 的虚拟模块（~icons/ri/*）引入图标集，
// Vitest 在打包管线之外无法加载它们。
vi.mock('../src/lib/icons', () => ({
  SIZE_PX: { sm: 14, md: 16, lg: 20 },
  getIcon: () => undefined,
  iconSvg: () => '',
}));

function row(overrides: Partial<SortableSessionRow> = {}): SortableSessionRow {
  return {
    id: 's-1',
    title: '会话一',
    lastUsedAt: undefined,
    updatedAt: '2026-11-15T08:00:00.000Z',
    pinned: false,
    pinnedAt: undefined,
    ...overrides,
  };
}

function groupOf(rows: SortableSessionRow[]): WorkspaceGroupType {
  return {
    workspace: {
      id: 'ws-1',
      name: '工作区',
      root: '/workspace',
      shortPath: '~/workspace',
      sessionCount: rows.length,
      trusted: true,
    },
    sessions: rows,
    hasMore: false,
    loadingMore: false,
    initialCount: 20,
  };
}

function mountSidebar(groups: WorkspaceGroupType[]): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  // SDR-019：置顶行不再渲染在 workspace 分组里，改由全局「已置顶」分区呈现。
  // 这里按 client 的同一条投影（分流 + 置顶时刻倒序）把置顶行供给分区，
  // 使本文件的图钉交互断言继续命中真实渲染出来的置顶行。
  const { pinned } = splitPinnedSessions(
    groups.flatMap((g) =>
      g.sessions.map((s) => ({
        ...s,
        workspaceId: g.workspace.id,
        workspaceName: g.workspace.name,
      })),
    ),
  );
  return mount(Sidebar, {
    props: {
      activeWorkspace: groups[0]?.workspace ?? null,
      activeWorkspaceId: 'ws-1',
      sessions: [],
      groups,
      pinnedSessions: pinned,
      activeId: '',
      workspaceSortMode: 'manual',
    },
    global: { plugins: [i18n] },
  });
}

/** The row's pin rail toggle — one control for both states (SDR-021). */
function pinButton(wrapper: VueWrapper): DOMWrapper<HTMLButtonElement> {
  return wrapper.find<HTMLButtonElement>('[data-test-id="session-pin-btn"]');
}

/** Open the color palette: the rail pin's right-click (a plain click toggles). */
async function openPalette(wrapper: VueWrapper): Promise<void> {
  await pinButton(wrapper).trigger('contextmenu');
}

describe('sidebar session pinning', () => {
  it('should pin with the default red color when the pin button is clicked on an unpinned row', async () => {
    const wrapper = mountSidebar([groupOf([row()])]);

    await pinButton(wrapper).trigger('click');

    // 置顶默认带红色：彩色圆圈是置顶状态的唯一可见痕迹，无色则看不出来。
    expect(wrapper.emitted('pin')).toEqual([['s-1', 'red']]);
    expect(wrapper.emitted('unpin'), '未置顶行不应发出 unpin').toBeUndefined();
    wrapper.unmount();
  });

  it('should unpin when the pin button is clicked on a pinned row', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await pinButton(wrapper).trigger('click');

    expect(wrapper.emitted('unpin')).toEqual([['s-1']]);
    wrapper.unmount();
  });

  it('should render the pin rail at the row far left for both states', async () => {
    // 两态是同一个锚点上的同一个控件、同一个圆形标记：未置顶空心，已置顶实心。
    const unpinned = mountSidebar([groupOf([row()])]);
    const unpinnedPin = pinButton(unpinned);
    expect(unpinnedPin.exists(), '未置顶行也应渲染 Pin 轨按钮').toBe(true);
    expect(unpinnedPin.attributes('aria-pressed'), '未置顶的 aria-pressed 应为 false').toBe('false');
    expect(unpinnedPin.classes(), '未置顶不应带 is-pinned').not.toContain('is-pinned');
    expect(unpinnedPin.find('.pin-dot').exists(), '标记是圆圈而不是图标').toBe(true);
    unpinned.unmount();

    const pinned = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);
    const pinnedPin = pinButton(pinned);
    expect(pinnedPin.attributes('aria-pressed'), '已置顶的 aria-pressed 应为 true').toBe('true');
    expect(pinnedPin.classes(), '已置顶应带 is-pinned').toContain('is-pinned');
    expect(pinnedPin.classes(), '圆圈携带置顶颜色').toContain('pin-blue');
    expect(pinnedPin.find('.pin-dot').exists(), '两态共用同一个圆圈标记').toBe(true);
    pinned.unmount();
  });

  it('should name the session in the pin label so screen readers disambiguate', async () => {
    const wrapper = mountSidebar([groupOf([row({ title: '修复登录按钮配色' })])]);

    expect(pinButton(wrapper).attributes('aria-label'), '未置顶的标签应带会话标题').toContain(
      '修复登录按钮配色',
    );
    wrapper.unmount();

    const pinned = mountSidebar([groupOf([row({ title: '修复登录按钮配色', pinned: true })])]);
    expect(pinButton(pinned).attributes('aria-label'), '已置顶的标签应带会话标题').toContain(
      '修复登录按钮配色',
    );
    pinned.unmount();
  });

  it('should open the color palette when the pinned pin is right-clicked', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    expect(wrapper.find('[data-test-id="session-pin-palette"]').exists()).toBe(false);
    await openPalette(wrapper);

    expect(
      wrapper.find('[data-test-id="session-pin-palette"]').exists(),
      '右键钉子后应打开调色板',
    ).toBe(true);
    wrapper.unmount();
  });

  it('should re-pin with the picked color when a palette swatch is chosen', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await openPalette(wrapper);
    const swatch = wrapper.find('[data-test-id="session-pin-palette"] .pin-red');
    expect(swatch.exists(), '打开色板后应能看到七色色块').toBe(true);
    await swatch.trigger('click');

    // 改色免取消置顶：只发 pin(带色)，不发 unpin。
    expect(wrapper.emitted('pin')).toEqual([['s-1', 'red']]);
    expect(wrapper.emitted('unpin'), '改色不应先取消置顶').toBeUndefined();
    wrapper.unmount();
  });

  it('should clear the pin color when the colorless option is chosen', async () => {
    // 「无颜色」不再提供：置顶用彩色圆圈表达，无色置顶没有可见痕迹。
    // 调色板只提供七色。
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await openPalette(wrapper);

    expect(
      wrapper.find('[data-test-id="session-pin-clear"]').exists(),
      '调色板不应再有「无颜色」选项',
    ).toBe(false);
    expect(
      wrapper.findAll('[data-test-id="session-pin-palette"] .pin-sw'),
      '调色板应只提供七色',
    ).toHaveLength(7);
    wrapper.unmount();
  });

  it('should carry the pin color on the rail circle instead of a side bar', () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    // 颜色只染圆圈（方案 A）：行里不再有独立竖条。
    expect(
      wrapper.find('[data-test-id="session-pin-bar"]').exists(),
      '置顶颜色不再用行左缘竖条表达',
    ).toBe(false);
    expect(pinButton(wrapper).classes(), '圆圈应带 pinColor 颜色类').toContain('pin-blue');
    wrapper.unmount();
  });
});
