// @vitest-environment jsdom
// apps/mirri-web/test/search-sessions-pin.test.ts
//
// 搜索会话对话框的置顶快捷动作（P1 三入口语义一致）：结果行上的图钉按钮
// 单击 = 无色置顶 / 取消置顶，与侧栏 SessionRow 的 hover 图钉同语义；
// 改色入口不在搜索对话框（保留在侧栏状态图钉）。

import { afterEach, describe, expect, it, vi } from 'vitest';
import { mount, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import { messages } from '../src/i18n/locales';
import SearchSessionsDialog from '../src/components/dialogs/SearchSessionsDialog.vue';
import type { Session } from '../src/types';

// lib/icons 通过 unplugin-icons 的虚拟模块（~icons/ri/*）引入图标集，
// Vitest 在打包管线之外无法加载它们。
vi.mock('../src/lib/icons', () => ({
  SIZE_PX: { sm: 14, md: 16, lg: 20 },
  getIcon: () => undefined,
  iconSvg: () => '',
}));

function session(overrides: Partial<Session> = {}): Session {
  return {
    id: 's-1',
    title: '会话一',
    time: '10:00',
    busy: false,
    pinned: false,
    ...overrides,
  };
}

function mountDialog(sessions: Session[]): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  return mount(SearchSessionsDialog, {
    props: { sessions, activeId: '' },
    global: { plugins: [i18n] },
    // The dialog panel is teleported to <body>; attach there so the result
    // rows are reachable (and clean the DOM after each test).
    attachTo: document.body,
  });
}

/** 行上的置顶快捷按钮（多行时按结果顺序取）。 */
function pinButtons(): HTMLButtonElement[] {
  return [...document.body.querySelectorAll<HTMLButtonElement>('[data-test-id="search-pin-btn"]')];
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('search sessions pinning', () => {
  it('should pin without a color when the pin button is clicked on an unpinned result', async () => {
    const wrapper = mountDialog([session()]);

    expect(pinButtons(), '结果行应渲染置顶快捷按钮').toHaveLength(1);
    pinButtons()[0]?.click();
    await wrapper.vm.$nextTick();

    const pinCalls = wrapper.emitted('pin');
    expect(pinCalls, '未置顶结果单击应发出 pin').toHaveLength(1);
    expect(pinCalls?.[0]?.[0]).toBe('s-1');
    expect(pinCalls?.[0]?.[1], '不应携带颜色').toBeUndefined();
    expect(wrapper.emitted('unpin')).toBeUndefined();
    wrapper.unmount();
  });

  it('should unpin when the pin button is clicked on a pinned result', async () => {
    const wrapper = mountDialog([session({ pinned: true })]);

    expect(pinButtons(), '已置顶结果也应渲染置顶快捷按钮').toHaveLength(1);
    pinButtons()[0]?.click();
    await wrapper.vm.$nextTick();

    expect(wrapper.emitted('unpin')).toEqual([['s-1']]);
    expect(wrapper.emitted('pin')).toBeUndefined();
    wrapper.unmount();
  });
});
