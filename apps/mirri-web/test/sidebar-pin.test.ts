// @vitest-environment jsdom
// apps/mirri-web/test/sidebar-pin.test.ts
//
// 会话行首指示器（SDR-022：把「是否在跑 / 是否置顶 / 置顶颜色 / 未读」合到一个槽位）：
// - 槽位是标题左侧本来就有的那条 gutter（--sb-gutter + --sb-gap），不新增列；
//   标题起点与搜索、新建会话同一条左边线。
// - 六种状态由 pinned / busy / unread 三个布尔决定：
//   ① 未置顶·空闲 = 空（悬停才浮出可点的空心环）
//   ② 未置顶·未读 = 中性实心点
//   ③ 未置顶·思考中 = 空心环 + 扫描弧
//   ④ 已置顶·空闲 = 实心圆 = 置顶色
//   ⑤ 已置顶·未读 = ④ + 右上角标（2px 背景挖孔，只与行背景比对比度）
//   ⑥ 已置顶·思考中 = ④ + 呼吸发光（MCP 面板那套 glow-pulse）
// - busy 压过 unread：一行只表达一件事。
// - 交互：点圆圈 = 置顶（默认红）/ 取消置顶；悬停已置顶圆圈 = 打开七色调色板；
//   右键 / Shift+F10 也开调色板。
//
// 这里锁定的是 sidebar 自己的事件边界：真实 Sidebar + 真实 WorkspaceGroup +
// 真实 SessionRow + 真实 SessionLeadIndicator 一起挂载。

import { describe, expect, it, vi } from 'vitest';
import { mount, type DOMWrapper, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import { messages } from '../src/i18n/locales';
import Sidebar from '../src/components/Sidebar.vue';
import type { SortableSessionRow } from '../src/lib/sessionSorting';
import type { SessionWorkFacts, WorkspaceGroup as WorkspaceGroupType } from '../src/types';
import { splitPinnedSessions } from '../src/lib/sessionSorting';

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

function mountSidebar(
  groups: WorkspaceGroupType[],
  workFacts: Record<string, Partial<SessionWorkFacts>> = {},
  unread: Record<string, boolean> = {},
): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  // SDR-021：置顶行同时出现在顶部「已置顶」分区和所属工作区分组里。
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
      unreadBySession: unread,
      activeId: '',
      workspaceSortMode: 'manual',
    },
    global: {
      plugins: [i18n],
      // The row reads its running/idle facts from this injected lookup; the
      // breathing-dot assertions need to drive it per session.
      provide: {
        resolveSessionWorkFacts: (id: string): SessionWorkFacts => ({
          busy: false,
          ...workFacts[id],
        }),
      },
    },
  });
}

/** The row's lead indicator — one control for every state (SDR-022). */
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

  it('should render one indicator in the row lead slot for both pin states', async () => {
    // 两态是同一个槽位上的同一个控件、同一个圆形标记：未置顶空心，已置顶实心。
    const unpinned = mountSidebar([groupOf([row()])]);
    const unpinnedPin = pinButton(unpinned);
    expect(unpinnedPin.exists(), '未置顶行也应有行首指示器').toBe(true);
    expect(unpinnedPin.attributes('aria-pressed'), '未置顶的 aria-pressed 应为 false').toBe('false');
    expect(unpinnedPin.classes(), '未置顶不应带 is-pinned').not.toContain('is-pinned');
    expect(unpinnedPin.find('.orb-dot').exists(), '标记是圆圈而不是图标').toBe(true);
    expect(
      unpinned.find('.lead [data-test-id="session-pin-btn"]').exists(),
      '指示器必须落在行首的 lead 槽位里，不新增列',
    ).toBe(true);
    unpinned.unmount();

    const pinned = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);
    const pinnedPin = pinButton(pinned);
    expect(pinnedPin.attributes('aria-pressed'), '已置顶的 aria-pressed 应为 true').toBe('true');
    expect(pinnedPin.classes(), '已置顶应带 is-pinned').toContain('is-pinned');
    expect(pinnedPin.classes(), '圆圈携带置顶颜色').toContain('pin-blue');
    expect(pinnedPin.find('.orb-dot').exists(), '两态共用同一个圆圈标记').toBe(true);
    pinned.unmount();
  });

  it('should breath on a pinned running row without a second indicator', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])], {
      's-1': { busy: true },
    });

    const btn = pinButton(wrapper);
    expect(btn.classes(), '运行中的置顶圆圈应带呼吸标记').toContain('is-running');
    expect(btn.attributes('aria-busy'), '运行态对辅助技术可读').toBe('true');
    expect(wrapper.find('.ui-spinner').exists(), '一行只有一个指示器，不应再有行首转圈').toBe(false);
    wrapper.unmount();
  });

  it('should sweep the bare ring on an unpinned running row', async () => {
    const wrapper = mountSidebar([groupOf([row()])], { 's-1': { busy: true } });

    const btn = pinButton(wrapper);
    expect(btn.classes(), '未置顶运行中走空心环 + 扫描弧').toContain('is-running');
    expect(btn.classes(), '未置顶不参与呼吸').not.toContain('is-pinned');
    expect(wrapper.find('.ui-spinner').exists(), '转圈并入行首指示器，不再单独渲染').toBe(false);
    wrapper.unmount();
  });

  it('should not breath when a pinned row is idle', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    expect(pinButton(wrapper).classes(), '空闲的置顶圆圈保持静止').not.toContain('is-running');
    wrapper.unmount();
  });

  it('should mark an unread unpinned row with a neutral dot', async () => {
    const wrapper = mountSidebar([groupOf([row()])], {}, { 's-1': true });

    const btn = pinButton(wrapper);
    expect(btn.classes(), '未读行应带 is-unread').toContain('is-unread');
    expect(btn.find('.orb-unread').exists(), '未读用角标表达').toBe(true);
    expect(btn.classes(), '未读不等于置顶').not.toContain('is-pinned');
    wrapper.unmount();
  });

  it('should keep the unread badge on a pinned row so the two states coexist', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'orange' })])], {}, {
      's-1': true,
    });

    const btn = pinButton(wrapper);
    expect(btn.classes(), '置顶与未读可共存').toContain('is-pinned');
    expect(btn.classes(), '置顶与未读可共存').toContain('is-unread');
    expect(btn.find('.orb-unread').exists(), '置顶行的未读也是角标').toBe(true);
    wrapper.unmount();
  });

  it('should let running win over unread so a row says one thing', async () => {
    // 后台跑完置了未读、随后又被续跑：技术上会同时成立，视觉上只表达运行态。
    const wrapper = mountSidebar([groupOf([row()])], { 's-1': { busy: true } }, { 's-1': true });

    const btn = pinButton(wrapper);
    expect(btn.classes(), '运行态压过未读').toContain('is-running');
    expect(btn.classes(), '运行中不叠加未读').not.toContain('is-unread');
    expect(btn.find('.orb-unread').exists(), '运行中不渲染未读角标').toBe(false);
    wrapper.unmount();
  });

  it('should render nothing but the idle ring for a plain unpinned row', async () => {
    // ① 普通空闲：静止时槽位里没有可见标记，环只在悬停/聚焦时浮出（由行负责）。
    const wrapper = mountSidebar([groupOf([row()])]);

    const btn = pinButton(wrapper);
    expect(btn.classes(), '普通空闲属于「无态」').toContain('is-idle-unpinned');
    expect(btn.classes(), '未置顶不应带 is-pinned').not.toContain('is-pinned');
    expect(btn.classes(), '空闲不是运行态').not.toContain('is-running');
    expect(btn.find('.orb-unread').exists(), '空闲且未读才有角标').toBe(false);
    wrapper.unmount();
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

  it('should not open the palette when the pointer only passes over the pinned dot', async () => {
    vi.useFakeTimers();
    try {
      const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);
      const pin = pinButton(wrapper);

      // 指针扫过：enter 之后紧接着 mousemove，停顿时长从未攒够。
      await pin.trigger('mouseenter');
      await vi.advanceTimersByTimeAsync(120);
      await pin.trigger('mousemove');
      await vi.advanceTimersByTimeAsync(120);

      expect(
        wrapper.find('[data-test-id="session-pin-palette"]').exists(),
        '快速扫过圆点不应弹出调色板',
      ).toBe(false);
      wrapper.unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it('should open the palette when the pointer settles on the pinned dot', async () => {
    vi.useFakeTimers();
    try {
      const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

      await pinButton(wrapper).trigger('mouseenter');
      await vi.advanceTimersByTimeAsync(700);

      expect(
        wrapper.find('[data-test-id="session-pin-palette"]').exists(),
        '指针停留后应弹出调色板',
      ).toBe(true);
      wrapper.unmount();
    } finally {
      vi.useRealTimers();
    }
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
