// @vitest-environment jsdom
// apps/mirri-web/test/sidebar-pin.test.ts
//
// 会话置顶的交互语义（P1 行为变更）：
// - hover 图钉按钮 = 快捷动作：未置顶单击直接无色置顶；已置顶单击取消置顶。
// - 标题旁常驻状态图钉 = 状态标识 + 改色入口：点击打开七色调色板弹层。
// - 调色板选色 = 直接 re-pin 带色（不需要先取消置顶）；「无颜色」= 无色
//   re-pin（清掉颜色但保持置顶）。
// - 已置顶行最左缘渲染 3px 彩色竖条（aria-hidden），颜色 = pinColor。
//
// 这里锁定的就是 sidebar 自己的事件边界：真实 Sidebar + 真实 WorkspaceGroup +
// 真实 SessionRow 一起挂载，用户点图钉 / 点状态图钉 / 选色，断言 Sidebar 发出的
// pin/unpin 与竖条的渲染形态。

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

/** 该行悬停区的快捷图钉按钮（未置顶 = 无色置顶，已置顶 = 取消置顶）。 */
function pinButton(wrapper: VueWrapper): DOMWrapper<HTMLButtonElement> {
  return wrapper.find<HTMLButtonElement>('[data-test-id="session-pin-btn"]');
}

/** 已置顶行标题旁的常驻状态图钉（改色入口）。 */
function statusPin(wrapper: VueWrapper): DOMWrapper<HTMLButtonElement> {
  return wrapper.find<HTMLButtonElement>('[data-test-id="session-pin-status"]');
}

describe('sidebar session pinning', () => {
  it('should pin without a color when the pin button is clicked on an unpinned row', async () => {
    const wrapper = mountSidebar([groupOf([row()])]);

    await pinButton(wrapper).trigger('click');

    // 事件链（WorkspaceGroup/Sidebar 转发）可能显式透传 color 槽位，
    // 所以锁定语义而非参数个数：有且只有一次 pin，id 正确，color 为 undefined。
    const pinCalls = wrapper.emitted('pin');
    expect(pinCalls, '未置顶行单击应发出 pin').toHaveLength(1);
    expect(pinCalls?.[0]?.[0]).toBe('s-1');
    expect(pinCalls?.[0]?.[1], '不应携带颜色').toBeUndefined();
    expect(wrapper.emitted('unpin'), '未置顶行不应发出 unpin').toBeUndefined();
    wrapper.unmount();
  });

  it('should unpin when the pin button is clicked on a pinned row', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await pinButton(wrapper).trigger('click');

    expect(wrapper.emitted('unpin')).toEqual([['s-1']]);
    wrapper.unmount();
  });

  it('should open the color palette when the pinned status icon is clicked', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    expect(statusPin(wrapper).exists(), '已置顶行应渲染常驻状态图钉').toBe(true);
    expect(wrapper.find('[data-test-id="session-pin-palette"]').exists()).toBe(false);
    await statusPin(wrapper).trigger('click');

    expect(
      wrapper.find('[data-test-id="session-pin-palette"]').exists(),
      '点击状态图钉后应打开调色板',
    ).toBe(true);
    wrapper.unmount();
  });

  it('should re-pin with the picked color when a palette swatch is chosen', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await statusPin(wrapper).trigger('click');
    const swatch = wrapper.find('[data-test-id="session-pin-palette"] .pin-red');
    expect(swatch.exists(), '打开色板后应能看到七色色块').toBe(true);
    await swatch.trigger('click');

    // 改色免取消置顶：只发 pin(带色)，不发 unpin。
    expect(wrapper.emitted('pin')).toEqual([['s-1', 'red']]);
    expect(wrapper.emitted('unpin'), '改色不应先取消置顶').toBeUndefined();
    wrapper.unmount();
  });

  it('should clear the pin color when the colorless option is chosen', async () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    await statusPin(wrapper).trigger('click');
    const clear = wrapper.find('[data-test-id="session-pin-clear"]');
    expect(clear.exists(), '调色板应提供「无颜色」选项').toBe(true);
    await clear.trigger('click');

    // 无色 re-pin：保持置顶、清掉颜色（color 槽位为 undefined）。
    const pinCalls = wrapper.emitted('pin');
    expect(pinCalls, '清色应发出无色 pin').toHaveLength(1);
    expect(pinCalls?.[0]?.[0]).toBe('s-1');
    expect(pinCalls?.[0]?.[1], '不应携带颜色').toBeUndefined();
    expect(wrapper.emitted('unpin'), '清色不应取消置顶').toBeUndefined();
    wrapper.unmount();
  });

  it('should render the colored side bar for a pinned session', () => {
    const wrapper = mountSidebar([groupOf([row({ pinned: true, pinColor: 'blue' })])]);

    const bar = wrapper.find('[data-test-id="session-pin-bar"]');
    expect(bar.exists(), '已置顶行应渲染行左缘彩色竖条').toBe(true);
    expect(bar.classes(), '竖条应带 pinColor 的颜色类').toContain('pin-blue');
    expect(bar.attributes('aria-hidden'), '竖条是纯视觉装饰').toBe('true');
    wrapper.unmount();
  });
});
