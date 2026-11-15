// @vitest-environment jsdom
// apps/mirri-web/test/scheduled-run-open.test.ts
//
// The scheduled-task history's 打开 entry: clicking it must open the run's
// execution session the way the sidebar opens any session — the settings
// overlay goes away and the app selects the session — and it must never fail
// silently.
//
// Three boundaries are covered here:
// - `ScheduledTaskDetail` is mounted for real (jsdom): the history row's
//   button has to reach `cron.run.open` AND raise the in-app open signal for
//   the session, in both the success and the degraded (materialize failed)
//   case. `ScheduledTaskForm` is stubbed — it only builds the settings tab and
//   its picker dialogs have nothing to do with opening a run.
// - `ScheduledTaskPanel` is mounted for the settings-page hop: the detail's
//   signal has to leave the panel (SettingsView then forwards it to App, which
//   owns the overlay and the session selection).
// - `openCronRunSession` (the handler App wires the signal to) is a named
//   export, so the close-then-open ordering is asserted directly.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { defineComponent, h } from 'vue';
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import type { AppCronRun, AppCronTask } from '../src/api/types';
import { messages } from '../src/i18n/locales';
import ScheduledTaskDetail from '../src/components/settings/scheduledTasks/ScheduledTaskDetail.vue';
import ScheduledTaskPanel from '../src/components/settings/scheduledTasks/ScheduledTaskPanel.vue';
import { openCronRunSession } from '../src/lib/cronManagement';

const apiMock = vi.hoisted(() => ({
  listCronRuns: vi.fn(),
  listAllCronTasks: vi.fn(),
  listWorkspaces: vi.fn(),
  openCronRun: vi.fn(),
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
// Glyphs are irrelevant here, so <Icon> renders nothing.
vi.mock('../src/lib/icons', () => ({
  SIZE_PX: { sm: 14, md: 16, lg: 20 },
  getIcon: () => undefined,
  iconSvg: () => '',
}));

// The settings tab's form pulls the schedule picker's dialog stack; the run
// history is what this suite exercises.
vi.mock('../src/components/settings/scheduledTasks/ScheduledTaskForm.vue', () => ({
  default: defineComponent({
    name: 'ScheduledTaskForm',
    setup: () => () => h('div', { class: 'scheduled-task-form-stub' }),
  }),
}));

const TASK: AppCronTask = {
  id: 'task-1',
  cron: '0 9 * * *',
  prompt: 'Summarize the new issues',
  recurring: true,
  stale: false,
  createdAt: 1760000000000,
  scope: { kind: 'workspace', workspaceId: 'ws-1' },
};

const RUN: AppCronRun = {
  runId: 'run-1',
  status: 'succeeded',
  startedAt: '2026-01-01T00:00:00.000Z',
  endedAt: '2026-01-01T00:01:00.000Z',
  durationMs: 60_000,
  sessionRef: 'run-sess-1',
};

function mountDetail(): VueWrapper {
  const i18n = createI18n({ legacy: false, locale: 'en', fallbackLocale: 'en', messages });
  return mount(ScheduledTaskDetail, {
    props: {
      task: TASK,
      workspaceId: 'ws-1',
      workspaces: [{ id: 'ws-1', name: 'Workspace' }],
      models: [],
      saving: false,
      refreshKey: 0,
    },
    global: { plugins: [i18n] },
  });
}

/** Switch the detail to its 历史 tab and wait for the run rows to render. */
async function openHistoryTab(wrapper: VueWrapper): Promise<void> {
  const tab = wrapper
    .findAll('[role="tab"]')
    .find((node) => node.text() === messages.en.settings.scheduledTaskTabHistory);
  expect(tab, 'the history tab must be rendered').toBeDefined();
  await tab?.trigger('click');
  await flushPromises();
}

/** The 打开 button of the single history row. */
function openRunButton(wrapper: VueWrapper) {
  return wrapper
    .findAll('.std-run button')
    .find((node) => node.text() === messages.en.settings.scheduledTaskRunOpen);
}

describe('scheduled run open', () => {
  beforeEach(() => {
    apiMock.listCronRuns.mockReset().mockResolvedValue([RUN]);
    apiMock.openCronRun.mockReset().mockResolvedValue({ sessionId: 'run-sess-1', materialized: true });
    clientMock.pushOperationFailure.mockReset();
  });

  it('should open the run session and ask the app to show it when the user opens a run', async () => {
    const wrapper = mountDetail();
    await flushPromises();
    await openHistoryTab(wrapper);

    const button = openRunButton(wrapper);
    expect(button, 'the run row must offer the open entry').toBeDefined();
    await button?.trigger('click');
    await flushPromises();

    // Then the materialize compat call rides the workspace + session ids…
    expect(apiMock.openCronRun).toHaveBeenCalledWith('ws-1', 'run-sess-1');
    // …and the app is told to leave settings and select that session.
    expect(wrapper.emitted('openRun')).toEqual([['run-sess-1']]);
    expect(clientMock.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('should still open the run session and warn when the materialize call fails', async () => {
    apiMock.openCronRun.mockRejectedValue(new Error('cron.run.open unavailable'));
    const wrapper = mountDetail();
    await flushPromises();
    await openHistoryTab(wrapper);

    await openRunButton(wrapper)?.trigger('click');
    await flushPromises();

    // The compat door failing degrades to a visible warning, it never blocks
    // the session the user asked for.
    expect(wrapper.emitted('openRun')).toEqual([['run-sess-1']]);
    expect(clientMock.pushOperationFailure).toHaveBeenCalledWith(
      'openCronRun',
      expect.any(Error),
      expect.objectContaining({
        title: messages.en.settings.scheduledTaskRunOpenDegradedTitle,
        message: messages.en.settings.scheduledTaskRunOpenDegradedMessage,
      }),
    );
  });
});

describe('openCronRunSession', () => {
  it('should leave the settings overlay before selecting the session when a run is opened', async () => {
    const calls: string[] = [];
    const closeSettings = vi.fn(() => calls.push('close'));
    const openSession = vi.fn(async () => {
      calls.push('open');
    });

    await openCronRunSession({ closeSettings, openSession }, 'run-sess-1');

    expect(closeSettings).toHaveBeenCalledTimes(1);
    expect(openSession).toHaveBeenCalledWith('run-sess-1');
    // The app view is the destination: a session that fails to open still
    // lands the user on the app, where the failure notice is visible.
    expect(calls).toEqual(['close', 'open']);
  });
});

describe('ScheduledTaskPanel', () => {
  beforeEach(() => {
    apiMock.listAllCronTasks
      .mockReset()
      .mockResolvedValue({
        sessions: [],
        workspaces: [{ workspaceId: 'ws-1', tasks: [TASK] }],
        disabled: false,
      });
    apiMock.listWorkspaces
      .mockReset()
      .mockResolvedValue([{ id: 'ws-1', root: '/workspace', name: 'Workspace', isGitRepo: false, sessionCount: 1 }]);
    apiMock.listCronRuns.mockReset().mockResolvedValue([RUN]);
    apiMock.openCronRun.mockReset().mockResolvedValue({ sessionId: 'run-sess-1', materialized: true });
  });

  it('should bubble the run-open request out of the settings page when a run is opened', async () => {
    const i18n = createI18n({ legacy: false, locale: 'en', fallbackLocale: 'en', messages });
    const wrapper = mount(ScheduledTaskPanel, {
      props: { models: [] },
      global: { plugins: [i18n] },
    });
    await flushPromises();

    // Task card → detail (the history tab then holds the run rows).
    await wrapper.find('.task-card').trigger('click');
    await flushPromises();
    await openHistoryTab(wrapper);

    await openRunButton(wrapper)?.trigger('click');
    await flushPromises();

    // SettingsView forwards this to App, which closes the overlay and selects
    // the session — the panel's own job is only to pass it on.
    expect(wrapper.emitted('openRun')).toEqual([['run-sess-1']]);
    wrapper.unmount();
  });
});
