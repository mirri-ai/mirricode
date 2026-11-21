// @vitest-environment jsdom
// apps/mirri-web/test/global-pinned-section.test.ts
//
// SDR-019：置顶区全局化。置顶是「全局重要」语义，所以侧栏顶部有一个跨
// workspace 的「已置顶」分区：
// - 它位于「工作区」分区之上，聚合所有 workspace 的置顶会话；
// - 组内按置顶时刻倒序（最后置顶的在最上）；
// - 置顶行同时保留在它所属的 workspace 分组里（SDR-021）——全局分区回答
//   「什么重要」，工作区分组回答「这个项目下有什么」，用户按项目浏览时仍能
//   找到置顶会话；两处渲染的是同一行的两个实例，置顶痕迹必须一致；
// - 无置顶会话时整个分区不渲染；
// - 置顶行的 select / pin / unpin 行为与普通行一致；时间元素外包 Tooltip
//   交代它属于哪个 workspace（跨 workspace 后行本身不再自带归属信息）。
//
// 这里挂载真实 Sidebar + 真实 WorkspaceGroup + 真实 SessionRow，断言的是
// 用户能看到的装配结果，而不是组件内部实现。

import { describe, expect, it, vi } from 'vitest';
import { mount, type DOMWrapper, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import { messages } from '../src/i18n/locales';
import Sidebar from '../src/components/Sidebar.vue';
import type { SortableSessionRow } from '../src/lib/sessionSorting';
import { splitPinnedSessions } from '../src/lib/sessionSorting';
import type { PinnedSessionEntry, WorkspaceGroup as WorkspaceGroupType } from '../src/types';

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

function pinned(
  session: SortableSessionRow,
  workspaceId = 'ws-1',
  workspaceName = '主工作区',
): PinnedSessionEntry {
  return { ...session, workspaceId, workspaceName };
}

function groupOf(
  id: string,
  name: string,
  rows: SortableSessionRow[],
  initialCount = 20,
): WorkspaceGroupType {
  return {
    workspace: {
      id,
      name,
      root: `/${id}`,
      shortPath: `~/${id}`,
      sessionCount: rows.length,
      trusted: true,
    },
    sessions: rows,
    hasMore: false,
    loadingMore: false,
    initialCount,
  };
}

function mountSidebar(options: {
  groups: WorkspaceGroupType[];
  pinnedSessions?: PinnedSessionEntry[];
}): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  return mount(Sidebar, {
    props: {
      activeWorkspace: options.groups[0]?.workspace ?? null,
      activeWorkspaceId: options.groups[0]?.workspace.id ?? null,
      sessions: [],
      groups: options.groups,
      pinnedSessions: options.pinnedSessions ?? [],
      activeId: '',
      workspaceSortMode: 'manual',
    },
    global: { plugins: [i18n] },
  });
}

/** 一个容器里的行标题（按渲染顺序）。 */
function titlesIn(wrapper: VueWrapper, selector: string): string[] {
  return wrapper.findAll(`${selector} .se`).map((el) => el.find('.t').text());
}

const pinnedSection = (wrapper: VueWrapper) => wrapper.find('[data-test-id="pinned-section"]');
const sectionTitles = (wrapper: VueWrapper) =>
  wrapper.findAll('.side-section-title').map((el) => el.text());

describe('global pinned section (SDR-019)', () => {
  it('should list pinned sessions in a global section above the workspace groups', () => {
    const wrapper = mountSidebar({
      groups: [
        groupOf('ws-1', '主工作区', [row({ id: 'plain-1', title: '普通会话' })]),
        groupOf('ws-2', '副工作区', []),
      ],
      pinnedSessions: [
        pinned(row({ id: 'pin-1', title: '置顶会话', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' }), 'ws-2', '副工作区'),
      ],
    });

    expect(pinnedSection(wrapper).exists(), '有置顶会话时应渲染全局置顶分区').toBe(true);
    expect(titlesIn(wrapper, '[data-test-id="pinned-section"]')).toEqual(['置顶会话']);

    // 分区标题在「工作区」之上，置顶行整体排在第一个 workspace 组之前。
    expect(sectionTitles(wrapper)).toEqual(['已置顶', '工作区']);
    const section = pinnedSection(wrapper).element;
    const workspaceLabel = wrapper.findAll('.side-section-label')[1]!.element;
    expect(
      section.compareDocumentPosition(workspaceLabel) & Node.DOCUMENT_POSITION_FOLLOWING,
      '置顶分区应排在「工作区」分区之上',
    ).toBeTruthy();
    wrapper.unmount();
  });

  it('should sort the global pinned section by pinnedAt descending', () => {
    // 顺序由排序投影（sessionSorting 的分流函数）给出，Sidebar 只负责按序
    // 装配，不再自己二次排序 —— 这里走的就是 client 的 pinnedSessions 路径。
    const { pinned: pinnedRows } = splitPinnedSessions([
      row({ id: 'pin-mid', title: '中间置顶', pinned: true, pinnedAt: '2026-11-10T00:00:00.000Z' }),
      row({ id: 'pin-old', title: '最早置顶', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' }),
      row({ id: 'pin-new', title: '最后置顶', pinned: true, pinnedAt: '2026-11-15T00:00:00.000Z' }),
    ]);

    const wrapper = mountSidebar({
      groups: [groupOf('ws-1', '主工作区', [])],
      pinnedSessions: pinnedRows.map((r) => pinned(r)),
    });

    expect(titlesIn(wrapper, '[data-test-id="pinned-section"]')).toEqual([
      '最后置顶',
      '中间置顶',
      '最早置顶',
    ]);
    wrapper.unmount();
  });

  it('should keep a pinned session inside its workspace group as well as the pinned section', async () => {
    const wrapper = mountSidebar({
      groups: [
        groupOf('ws-1', '主工作区', [
          row({ id: 'pin-1', title: '置顶会话', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' }),
          row({ id: 'plain-1', title: '普通会话' }),
        ]),
      ],
      pinnedSessions: [
        pinned(row({ id: 'pin-1', title: '置顶会话', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' })),
      ],
    });

    // SDR-021：置顶行在两处都在 —— 全局分区给「重要」，工作区分组给「按项目
    // 浏览」。用户从工作区下找会话时不能因为置顶而找不到。
    expect(titlesIn(wrapper, '[data-test-id="pinned-section"]')).toEqual(['置顶会话']);
    expect(titlesIn(wrapper, '.group-sessions'), '置顶行应保留在工作区分组里').toEqual([
      '置顶会话',
      '普通会话',
    ]);
    wrapper.unmount();
  });

  it('should render a pinned session even when its group is trimmed to the first page', async () => {
    // 置顶行排在组头（sortSessionsByUse 的置顶优先），所以「只看首页」的裁剪
    // 不能把它裁掉 —— 这是组内保留置顶行后最容易回归的地方。
    const wrapper = mountSidebar({
      groups: [
        groupOf(
          'ws-1',
          '主工作区',
          [
            row({ id: 'pin-1', title: '置顶会话', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' }),
            row({ id: 'plain-1', title: '普通会话一' }),
            row({ id: 'plain-2', title: '普通会话二' }),
          ],
          1,
        ),
      ],
      pinnedSessions: [
        pinned(row({ id: 'pin-1', title: '置顶会话', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' })),
      ],
    });

    expect(titlesIn(wrapper, '.group-sessions'), '首页裁剪不应藏掉置顶行').toEqual(['置顶会话']);
    wrapper.unmount();
  });

  it('should show the same pinned state on the row in both places', async () => {
    const wrapper = mountSidebar({
      groups: [
        groupOf('ws-1', '主工作区', [
          row({ id: 'pin-1', title: '置顶会话', pinned: true, pinColor: 'blue', pinnedAt: '2026-11-01T00:00:00.000Z' }),
        ]),
      ],
      pinnedSessions: [
        pinned(row({ id: 'pin-1', title: '置顶会话', pinned: true, pinColor: 'blue', pinnedAt: '2026-11-01T00:00:00.000Z' })),
      ],
    });

    // 两处渲染的是同一行的两个实例，置顶痕迹必须一致：同一条 Pin 轨的横钉 + 同色。
    for (const scope of ['[data-test-id="pinned-section"]', '.group-sessions']) {
      const current = wrapper.find(`${scope} .se`);
      const pin = current.find('[data-test-id="session-pin-btn"]');
      expect(pin.attributes('aria-pressed'), `${scope} 的钉子是已置顶态`).toBe('true');
      expect(pin.classes(), `${scope} 的钉子颜色应一致`).toContain('pin-blue');
    }
    wrapper.unmount();
  });

  it('should hide the global pinned section when no session is pinned', () => {
    const wrapper = mountSidebar({
      groups: [groupOf('ws-1', '主工作区', [row({ id: 'plain-1', title: '普通会话' })])],
      pinnedSessions: [],
    });

    expect(pinnedSection(wrapper).exists(), '无置顶会话时整个分区不渲染').toBe(false);
    expect(sectionTitles(wrapper)).toEqual(['工作区']);
    wrapper.unmount();
  });

  it('should name the owning workspace of a pinned row from another workspace', () => {
    const wrapper = mountSidebar({
      groups: [
        groupOf('ws-1', '主工作区', []),
        groupOf('ws-2', '副工作区', []),
      ],
      pinnedSessions: [
        pinned(row({ id: 'pin-2', title: '副区置顶', pinned: true, pinnedAt: '2026-11-01T00:00:00.000Z' }), 'ws-2', '副工作区'),
      ],
    });

    // 置顶行离开了所属工作区组，行本身要能回答「我属于哪个工作区」。
    // Tooltip 的气泡经 body teleport 渲染（未悬停时 v-show 隐藏但文本已在 DOM）。
    const hints = Array.from(document.querySelectorAll('.ui-tip__bubble')).map(
      (el) => el.textContent?.trim() ?? '',
    );
    expect(hints, '置顶行应给出所属工作区').toContain('工作区：副工作区');
    wrapper.unmount();
  });

  it('should forward select, unpin and pin from a pinned row like a regular row', async () => {
    const wrapper = mountSidebar({
      groups: [groupOf('ws-1', '主工作区', [])],
      pinnedSessions: [
        pinned(row({ id: 'pin-1', title: '置顶会话', pinned: true, pinColor: 'blue', pinnedAt: '2026-11-01T00:00:00.000Z' })),
      ],
    });
    const pinnedRow = wrapper.find('[data-test-id="pinned-section"] .se');

    await pinnedRow.trigger('click');
    expect(wrapper.emitted('select'), '点击置顶行应发出 select').toEqual([['pin-1']]);

    const quickPin = pinnedRow.find<HTMLButtonElement>('[data-test-id="session-pin-btn"]');
    await quickPin.trigger('click');
    expect(wrapper.emitted('unpin'), '已置顶行的快捷图钉=取消置顶').toEqual([['pin-1']]);

    // 改色：右键钉子的调色板 → 选红 = 带色 re-pin（不需要先取消置顶）。
    await pinnedRow.find('[data-test-id="session-pin-btn"]').trigger('contextmenu');
    const swatch: DOMWrapper<Element> = wrapper.find('[data-test-id="session-pin-palette"] .pin-red');
    expect(swatch.exists(), '置顶行应能打开改色调色板').toBe(true);
    await swatch.trigger('click');
    expect(wrapper.emitted('pin'), '置顶行改色应发出带色 pin').toEqual([['pin-1', 'red']]);
    wrapper.unmount();
  });
});
