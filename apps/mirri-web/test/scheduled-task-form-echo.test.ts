// @vitest-environment jsdom
// apps/mirri-web/test/scheduled-task-form-echo.test.ts
//
// The scheduled-task setting form must echo what the user just saved, and it
// must reopen a rule in the SAME representation the user entered it in.
//
// Two reported product defects are pinned here:
// - 保存后表单回退到保存之前的值: the panel bumps its refresh key BEFORE the
//   reload answers, so the detail remounts the form from the stale row and
//   the reload can no longer remount it (the key already changed).
// - 保存后日程显示成「自定义」: a stored expression/draft is replayed through
//   one mapping (`scheduleFromExpression`), so every shape the picker can
//   produce comes back as that same shape — a minute-interval rule reads
//   「每分钟」 instead of a bare 「自定义」 label.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import type { AppCronTask, AppCronTaskEdit } from '../src/api/types';
import type { CronSchedule } from '../src/lib/cronSchedule';
import { messages } from '../src/i18n/locales';
import ScheduledTaskPanel from '../src/components/settings/scheduledTasks/ScheduledTaskPanel.vue';
import ScheduledTaskCard from '../src/components/settings/scheduledTasks/ScheduledTaskCard.vue';

const apiMock = vi.hoisted(() => ({
  listAllCronTasks: vi.fn(),
  listWorkspaces: vi.fn(),
  listCronRuns: vi.fn(),
  updateWorkspaceCronTask: vi.fn(),
  previewCronSchedule: vi.fn(),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

const clientMock = vi.hoisted(() => ({
  pushOperationFailure: vi.fn(),
}));

vi.mock('../src/composables/useMirriWebClient', async () => {
  const { ref } = await import('vue');
  return {
    useMirriWebClient: () => ({
      ...clientMock,
      backend: ref('v3'),
      activeWorkspaceId: ref('ws-1'),
    }),
  };
});

vi.mock('../src/composables/useConfirmDialog', () => ({
  useConfirmDialog: () => ({ confirm: vi.fn() }),
}));

// lib/icons bundles the icon set through unplugin-icons virtual modules
// (`~icons/ri/*`), which Vitest 4 refuses to load outside a bundler pipeline.
vi.mock('../src/lib/icons', () => ({
  SIZE_PX: { sm: 14, md: 16, lg: 20 },
  getIcon: () => undefined,
  iconSvg: () => '',
}));

function task(overrides: Partial<AppCronTask> = {}): AppCronTask {
  return {
    id: 'task-1',
    cron: '0 9 * * *',
    prompt: '汇总构建结果',
    recurring: true,
    stale: false,
    createdAt: 1_760_000_000_000,
    timezone: 'Asia/Tokyo',
    scope: { kind: 'workspace', workspaceId: 'ws-1' },
    ...overrides,
  };
}

function mountPanel(): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
  return mount(ScheduledTaskPanel, {
    props: { models: [] },
    global: { plugins: [i18n] },
  });
}

/** Serve `rows` as the aggregate list from now on. */
function serveTasks(rows: AppCronTask[]): void {
  apiMock.listAllCronTasks.mockResolvedValue({
    sessions: [],
    workspaces: [{ workspaceId: 'ws-1', tasks: rows }],
    disabled: false,
  });
}

/** The settings tab's title input (label above, full-width input). */
function titleInput(wrapper: VueWrapper) {
  return wrapper.find('.stf-input');
}

function submitButton(wrapper: VueWrapper) {
  return wrapper.find('form button[type="submit"]');
}

/** Submit the edit form the way the user does (jsdom does not implement the
 *  click→submit bridge, so the form's own submit event is dispatched). */
async function saveForm(wrapper: VueWrapper): Promise<void> {
  expect(submitButton(wrapper).attributes('disabled')).toBeUndefined();
  await wrapper.find('form').trigger('submit');
  await flushPromises();
}

/** Open the first card's detail view (the settings tab is the default). */
async function openDetail(wrapper: VueWrapper): Promise<void> {
  await wrapper.find('.task-card').trigger('click');
  await flushPromises();
}

describe('the edit form echoes the saved task', () => {
  beforeEach(() => {
    clientMock.pushOperationFailure.mockReset();
    apiMock.updateWorkspaceCronTask.mockReset();
    apiMock.listWorkspaces.mockReset().mockResolvedValue([
      { id: 'ws-1', root: '/workspace', name: '工作区', isGitRepo: false, sessionCount: 1 },
    ]);
    apiMock.listCronRuns.mockReset().mockResolvedValue([]);
    apiMock.previewCronSchedule
      .mockReset()
      .mockResolvedValue({ occurrences: [1_760_000_300_000], previousT0: null });
  });

  it('should show the saved title when the edit form is reloaded after a save', async () => {
    const before = task({ title: '旧标题', prompt: '旧指示' });
    const after = task({ title: '新标题', prompt: '旧指示' });
    serveTasks([before]);
    apiMock.listCronRuns.mockResolvedValue([]);
    apiMock.updateWorkspaceCronTask.mockImplementation(
      async (_workspaceId: string, _taskId: string, edit: AppCronTaskEdit) => {
        // The engine answers the updated row, and the aggregate list serves it
        // from now on (exactly what the live engine does).
        serveTasks([{ ...after, title: edit.title ?? after.title }]);
        return { ...after, title: edit.title ?? after.title };
      },
    );

    const wrapper = mountPanel();
    await flushPromises();
    await openDetail(wrapper);
    expect(titleInput(wrapper).element.value).toBe('旧标题');

    await titleInput(wrapper).setValue('新标题');
    await saveForm(wrapper);

    expect(apiMock.updateWorkspaceCronTask).toHaveBeenCalledTimes(1);
    expect(titleInput(wrapper).element.value).toBe('新标题');
    expect(clientMock.pushOperationFailure).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it('should show the saved schedule when the rule changed and the task was reloaded', async () => {
    serveTasks([task({ cron: '0 9 * * *' })]);
    apiMock.updateWorkspaceCronTask.mockImplementation(async () => {
      const updated = task({ cron: '3 * * * *' });
      serveTasks([updated]);
      return updated;
    });

    const wrapper = mountPanel();
    await flushPromises();
    await openDetail(wrapper);

    // Pick the 每小时 preset, then save — the reload serves the new rule.
    const hourly = wrapper.findAll('[data-preset]').find((node) => node.attributes('data-preset') === 'hourly');
    expect(hourly, 'the preset row must offer 每小时').toBeDefined();
    await hourly?.trigger('click');
    await saveForm(wrapper);

    expect(apiMock.updateWorkspaceCronTask).toHaveBeenCalledTimes(1);
    const active = wrapper.findAll('[data-preset]').filter((node) => node.classes().includes('is-on'));
    expect(active.map((node) => node.attributes('data-preset'))).toEqual(['hourly']);
    wrapper.unmount();
  });
});

describe('a stored rule reopens in the shape it was entered in', () => {
  beforeEach(() => {
    clientMock.pushOperationFailure.mockReset();
    apiMock.listWorkspaces.mockReset().mockResolvedValue([
      { id: 'ws-1', root: '/workspace', name: '工作区', isGitRepo: false, sessionCount: 1 },
    ]);
    apiMock.listCronRuns.mockReset().mockResolvedValue([]);
    apiMock.previewCronSchedule
      .mockReset()
      .mockResolvedValue({ occurrences: [1_760_000_300_000], previousT0: null });
  });

  it.each([
    ['0 9 * * *', 'daily'],
    ['3 * * * *', 'hourly'],
    ['3 9 * * 1-5', 'weekday'],
    ['3 9 * * 1,5', 'weekly'],
    ['3 9 1 * *', 'monthly'],
  ])(
    'should select the %s preset when the stored expression %s is a preset rule',
    async (expr, preset) => {
      serveTasks([task({ cron: expr })]);
      const wrapper = mountPanel();
      await flushPromises();
      await openDetail(wrapper);

      const active = wrapper.findAll('[data-preset]').filter((node) => node.classes().includes('is-on'));
      expect(active.map((node) => node.attributes('data-preset'))).toEqual([preset]);
      wrapper.unmount();
    },
  );

  it('should describe an every-minute rule in human words when it reopens as a custom rule', async () => {
    serveTasks([task({ cron: '* * * * *' })]);
    const wrapper = mountPanel();
    await flushPromises();
    await openDetail(wrapper);

    // The custom preset is the honest home for an expression the six presets
    // cannot express — but the sentence must be the humanizer's, not the raw
    // 「按表达式 … 执行」 fallback.
    const active = wrapper.findAll('[data-preset]').filter((node) => node.classes().includes('is-on'));
    expect(active.map((node) => node.attributes('data-preset'))).toEqual(['custom']);
    const description = wrapper.find('.rule-description').text();
    expect(description).toContain('每分钟');
    expect(description).not.toContain('按表达式');
    wrapper.unmount();
  });

  it('should reopen a minute list as the structured field rule when the expression is a pick list', async () => {
    serveTasks([task({ cron: '0,30 * * * *' })]);
    const wrapper = mountPanel();
    await flushPromises();
    await openDetail(wrapper);

    // A field-mode rule came from the dialog's 按字段重复 path; its echo must
    // be the same structured choice (not the advanced expression).
    const active = wrapper.findAll('[data-preset]').filter((node) => node.classes().includes('is-on'));
    expect(active.map((node) => node.attributes('data-preset'))).toEqual(['custom']);
    expect(wrapper.find('.rule-description').text()).toContain('每小时的');
    wrapper.unmount();
  });

  it('should name the zone once when the rule sentence renders beside its zone chip', async () => {
    // The zone chip is the visible prefix; the sentence itself must not repeat
    // it (「Asia/TokyoAsia/Tokyo 每天 09:00 执行」).
    serveTasks([task({ cron: '0 9 * * *' })]);
    const wrapper = mountPanel();
    await flushPromises();
    await openDetail(wrapper);

    const description = wrapper.find('.rule-description');
    expect(description.find('.rule-tz').text()).toBe('Asia/Tokyo');
    expect(description.text()).toBe('Asia/Tokyo每天 09:00 执行');
    wrapper.unmount();
  });
});

describe('the custom rule dialog confirms into the model', () => {
  // The reported crash: 自定义 → 确认 did nothing at all. The dialog's scratch
  // draft is a deep reactive ref and its write-back deep-copied the value —
  // `structuredClone` throws on a nested Proxy (DataCloneError), so the
  // confirm never reached the model and the rule silently vanished.
  beforeEach(() => {
    apiMock.previewCronSchedule
      .mockReset()
      .mockResolvedValue({ occurrences: [1_760_000_300_000], previousT0: null });
  });

  it('should write the confirmed interval rule into the model when the dialog is confirmed', async () => {
    let wrapper: VueWrapper;
    const { default: SchedulePicker } = await import(
      '../src/components/settings/scheduledTasks/SchedulePicker.vue'
    );
    const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
    const updates: Array<CronSchedule | null> = [];
    wrapper = mount(SchedulePicker, {
      props: {
        schedule: {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 1, unit: 'day', time: '09:00' },
          ends: { kind: 'never' },
        },
        timezone: 'Asia/Tokyo',
        'onUpdate:schedule': (value: CronSchedule | null) => {
          updates.push(value);
          void wrapper.setProps({ schedule: value });
        },
      },
      global: { plugins: [i18n] },
      attachTo: document.body,
    });
    await flushPromises();

    // Reopen the editor from the custom preset's button, then confirm.
    const openButton = wrapper.findAll('button').find((node) => node.text() === '自定义周期规则');
    expect(openButton, 'the custom rule entry must be rendered').toBeDefined();
    await openButton?.trigger('click');
    await flushPromises();

    const confirmButton = [...document.body.querySelectorAll('button')].find(
      (node) => node.textContent?.trim() === '确认',
    );
    expect(confirmButton, 'the dialog must offer 确认').toBeDefined();
    confirmButton?.click();
    await flushPromises();

    expect(updates).toHaveLength(1);
    expect(updates[0]).toEqual({
      kind: 'custom',
      mode: 'interval',
      interval: { every: 1, unit: 'day', time: '09:00' },
      ends: { kind: 'never' },
    });

    wrapper.unmount();
    document.body.innerHTML = '';
  });

  it('should keep the confirmed rule when the dialog switched mode before confirming', async () => {
    // The exact crash the user hit: switching the dialog's mode replaces the
    // whole scratch value by spreading the reactive draft, so the nested
    // `interval` / `field` / `ends` slots end up as Proxies. The old write-back
    // deep-copied them with `structuredClone` → DataCloneError, and the rule
    // was never applied.
    let wrapper: VueWrapper;
    const { default: SchedulePicker } = await import(
      '../src/components/settings/scheduledTasks/SchedulePicker.vue'
    );
    const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
    const updates: Array<CronSchedule | null> = [];
    wrapper = mount(SchedulePicker, {
      props: {
        schedule: {
          kind: 'custom',
          mode: 'interval',
          interval: { every: 1, unit: 'day', time: '09:00' },
          ends: { kind: 'never' },
        },
        timezone: 'Asia/Tokyo',
        'onUpdate:schedule': (value: CronSchedule | null) => {
          updates.push(value);
          void wrapper.setProps({ schedule: value });
        },
      },
      global: { plugins: [i18n] },
      attachTo: document.body,
    });
    await flushPromises();

    await wrapper.findAll('button').find((node) => node.text() === '自定义周期规则')?.trigger('click');
    await flushPromises();

    // 按字段重复 — this is what leaves nested Proxies in the scratch draft.
    const fieldMode = document.body.querySelector<HTMLInputElement>('input[type="radio"][value="field"]');
    expect(fieldMode, 'the dialog must offer the field mode').not.toBeNull();
    fieldMode?.dispatchEvent(new Event('change'));
    await flushPromises();

    // Type the picks the field mode parses.
    const picks = document.body.querySelector<HTMLInputElement>('.rc-picks');
    expect(picks, 'the field mode must offer the picks input').not.toBeNull();
    if (picks) {
      picks.value = '1, 3 , 9';
      picks.dispatchEvent(new Event('input'));
    }
    await flushPromises();

    const confirmButton = [...document.body.querySelectorAll('button')].find(
      (node) => node.textContent?.trim() === '确认',
    );
    expect(confirmButton, 'the dialog must offer 确认').not.toBeNull();
    confirmButton?.click();
    await flushPromises();

    expect(updates).toHaveLength(1);
    expect(updates[0]).toEqual({
      kind: 'custom',
      mode: 'field',
      interval: { every: 1, unit: 'day', time: '09:00' },
      field: { unit: 'day-of-week', picks: [1, 3], time: '09:00' },
      ends: { kind: 'never' },
    });

    wrapper.unmount();
    document.body.innerHTML = '';
  });
});

describe('the card and the form speak the same rule sentence', () => {
  // One mapping (task row → picker shape → sentence) feeds both surfaces, so
  // the words a preview shows while typing are the words the card carries.
  // The zone stays the form's own chip — it is not part of the rule sentence.
  it.each([
    ['0 9 * * *', '每天 09:00 执行'],
    ['3 * * * *', '每小时 :03 分执行'],
    ['3 9 * * 1-5', '每个工作日 09:03 执行'],
  ])('should render the shared rule sentence on the card when the task rule is %s', async (expr, text) => {
    const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
    const wrapper = mount(ScheduledTaskCard, {
      props: { task: task({ cron: expr }), now: 1_760_000_100_000 },
      global: { plugins: [i18n] },
    });
    expect(wrapper.find('.task-card-frequency-rule').text()).toBe(text);
    wrapper.unmount();
  });

  it('should render the humanized rule on the card when the expression is one the presets cannot express', async () => {
    const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
    const wrapper = mount(ScheduledTaskCard, {
      props: { task: task({ cron: '* * * * *', timezone: null }), now: 1_760_000_100_000 },
      global: { plugins: [i18n] },
    });
    // The same words the picker's preview shows — a 每分钟 rule never reads as
    // a bare 自定义 or the raw expression on the card either.
    expect(wrapper.find('.task-card-frequency-rule').text()).toBe('每分钟');
    wrapper.unmount();
  });

  it('should render the interval rule in the same words the form prefills when the task is an interval task', async () => {
    const i18n = createI18n({ legacy: false, locale: 'zh', fallbackLocale: 'en', messages });
    const wrapper = mount(ScheduledTaskCard, {
      props: {
        task: task({
          cron: '',
          interval: { every: 1, unit: 'day', time: '09:00', anchorDate: '2026-11-01' },
        }),
        now: 1_760_000_100_000,
      },
      global: { plugins: [i18n] },
    });
    expect(wrapper.find('.task-card-frequency-rule').text()).toBe('每天 09:00 执行');
    wrapper.unmount();
  });
});
