// @vitest-environment jsdom
// apps/mirri-web/test/mobile-switcher-pin.test.ts
//
// 移动端切换器的置顶快捷动作（P1 三入口语义一致）：会话行上的图钉按钮
// 单击 = 无色置顶 / 取消置顶，与侧栏 SessionRow 的 hover 图钉同语义；
// 改色入口不在移动端（保留在侧栏状态图钉）。

import { describe, expect, it, vi } from 'vitest';
import { mount, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import { messages } from '../src/i18n/locales';
import MobileSwitcherSheet from '../src/components/mobile/MobileSwitcherSheet.vue';
import type { SortableSessionRow } from '../src/lib/sessionSorting';
import type { WorkspaceGroup } from '../src/types';

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

function groupOf(rows: SortableSessionRow[]): WorkspaceGroup {
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

function mountSheet(rows: SortableSessionRow[]): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  return mount(MobileSwitcherSheet, {
    props: {
      modelValue: true,
      groups: [groupOf(rows)],
      activeWorkspaceId: 'ws-1',
      activeId: '',
    },
    global: { plugins: [i18n] },
  });
}

/** 会话行上的置顶快捷按钮（多行时按渲染顺序取）。 */
function pinButtons(wrapper: VueWrapper): HTMLButtonElement[] {
  return wrapper.findAll<HTMLButtonElement>('[data-test-id="mobile-pin-btn"]').map((n) => n.element);
}

describe('mobile switcher pinning', () => {
  it('should pin without a color when the pin button is clicked on an unpinned row', async () => {
    const wrapper = mountSheet([row()]);

    expect(pinButtons(wrapper), '会话行应渲染置顶快捷按钮').toHaveLength(1);
    pinButtons(wrapper)[0]?.click();
    await wrapper.vm.$nextTick();

    const pinCalls = wrapper.emitted('pin');
    expect(pinCalls, '未置顶行单击应发出 pin').toHaveLength(1);
    expect(pinCalls?.[0]?.[0]).toBe('s-1');
    expect(pinCalls?.[0]?.[1], '不应携带颜色').toBeUndefined();
    expect(wrapper.emitted('unpin')).toBeUndefined();
    wrapper.unmount();
  });

  it('should unpin when the pin button is clicked on a pinned row', async () => {
    const wrapper = mountSheet([row({ pinned: true, pinColor: 'blue' })]);

    expect(pinButtons(wrapper), '已置顶行也应渲染置顶快捷按钮').toHaveLength(1);
    pinButtons(wrapper)[0]?.click();
    await wrapper.vm.$nextTick();

    expect(wrapper.emitted('unpin')).toEqual([['s-1']]);
    expect(wrapper.emitted('pin')).toBeUndefined();
    wrapper.unmount();
  });
});
