// apps/mirri-web/test/task-poller.test.ts
// Given the web client polls background tasks every second (useTaskPoller),
// when a heavy session holds hundreds of terminal tasks plus a few running
// ones, then the poller must scope each poll to the RUNNING rows
// (activeOnly) and fetch per-task output through the single-task face —
// never re-list the whole session per task per second (S5: the task.list
// polling amplifier). The full list loads once per session open.

import { computed, nextTick, reactive } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppTask as AppTaskType } from '../src/api/types';
import { createInitialState } from '../src/api/daemon/eventReducer';
import { useTaskPoller } from '../src/composables/client/useTaskPoller';
import type { ExtendedState } from '../src/composables/useMirriWebClient';

const apiMock = vi.hoisted(() => ({
  listTasks: vi.fn(),
  getTaskOutput: vi.fn(),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

function makeTask(id: string, status: AppTaskType['status']): AppTaskType {
  return {
    id,
    sessionId: 's-1',
    kind: 'bash',
    description: `task ${id}`,
    status,
    createdAt: '2026-11-01T00:00:00Z',
    startedAt: '2026-11-01T00:00:00Z',
    runInBackground: true,
  };
}

/** The production `rawState` is a Vue reactive object — the poller's
 * watchers key on its reactivity, so the test drives the same shape. */
function createState(): ExtendedState {
  const state = reactive(createInitialState()) as ExtendedState;
  state.activeSessionId = 's-1';
  return state;
}

beforeEach(() => {
  vi.restoreAllMocks();
  apiMock.listTasks.mockReset();
  apiMock.getTaskOutput.mockReset();
});

describe('useTaskPoller polling scope', () => {
  it('should poll task.list with activeOnly instead of re-listing the whole session each second', async () => {
    const state = createState();
    apiMock.listTasks.mockResolvedValue([makeTask('t-run', 'running')]);
    apiMock.getTaskOutput.mockResolvedValue('tail');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    // Seed a running row so the watcher arms the 1-second polling.
    state.tasksBySession = { ...state.tasksBySession, 's-1': [makeTask('t-run', 'running')] };
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(apiMock.listTasks).toHaveBeenCalledWith('s-1', undefined, { activeOnly: true });
    void poller;
  });

  it('should fetch running-task output through the single-task face, not task.list per task', async () => {
    const state = createState();
    const running = makeTask('t-run', 'running');
    apiMock.listTasks.mockResolvedValue([running]);
    apiMock.getTaskOutput.mockResolvedValue('building…');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    state.tasksBySession = { ...state.tasksBySession, 's-1': [running] };
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(apiMock.getTaskOutput).toHaveBeenCalledWith('s-1', 't-run', expect.any(Number));
    // One poll cycle, one list call — the running-row count bounds the
    // output calls, not the session's task count.
    expect(apiMock.listTasks).toHaveBeenCalledTimes(1);
    expect(apiMock.getTaskOutput).toHaveBeenCalledTimes(1);
    void poller;
  });

  it('should leave WS-owned subagent rows running when the poll lists REST rows keyed by task id', async () => {
    const state = createState();
    const subagent: AppTask = {
      id: 'agent-7',
      sessionId: 's-1',
      kind: 'subagent',
      description: 'explore',
      status: 'running',
      createdAt: '2026-11-01T00:00:00Z',
      startedAt: '2026-11-01T00:00:00Z',
      agentId: 'agent-7',
      runInBackground: true,
    };
    state.tasksBySession = { ...state.tasksBySession, 's-1': [subagent] };
    // The activeOnly poll answers REST rows keyed by task id — the WS row's
    // agent id never matches, which used to make the poller mistake every
    // running subagent for "just finished" and flip it terminal.
    apiMock.listTasks.mockResolvedValue([]);
    apiMock.getTaskOutput.mockResolvedValue('');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve, 10));

    const row = state.tasksBySession['s-1'].find((t) => t.id === 'agent-7');
    expect(row?.status).toBe('running');
    void poller;
  });

  it('should settle a just-finished row with its real terminal status from a one-shot full list', async () => {
    const state = createState();
    const running = makeTask('t-run', 'running');
    state.tasksBySession = { ...state.tasksBySession, 's-1': [running] };
    apiMock.listTasks.mockResolvedValue([]); // the poll answers: nothing running
    // The one-shot settle net fetches the full list, which knows the truth.
    apiMock.listTasks.mockResolvedValueOnce([]);
    apiMock.listTasks.mockResolvedValueOnce([
      { ...makeTask('t-run', 'failed'), outputPreview: 'boom' },
    ]);
    apiMock.getTaskOutput.mockResolvedValue('boom');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve, 10));

    const row = state.tasksBySession['s-1'].find((t) => t.id === 't-run');
    expect(row?.status).toBe('failed');
    expect(row?.outputPreview).toBe('boom');
    void poller;
  });

  it('should keep the session terminal task history when a poll replaces only the running rows', async () => {
    const state = createState();
    const done = { ...makeTask('t-done', 'completed'), outputPreview: 'final' };
    const running = makeTask('t-run', 'running');
    state.tasksBySession = { ...state.tasksBySession, 's-1': [done, running] };
    // First call: the running-only poll (nothing running anymore). Second
    // call: the one-shot settle net — the full list carrying the real row.
    apiMock.listTasks
      .mockResolvedValueOnce([])
      .mockResolvedValue([{ ...makeTask('t-run', 'completed'), outputPreview: 'settled' }]);
    apiMock.getTaskOutput.mockResolvedValue('settled');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve, 10));

    const rows = state.tasksBySession['s-1'];
    // The poll's running-only list must not wipe the session's terminal history.
    expect(rows.find((t) => t.id === 't-done')).toMatchObject({ status: 'completed' });
    // The row that just left the running set settles terminal with its final output.
    expect(rows.find((t) => t.id === 't-run')).toMatchObject({
      status: 'completed',
      outputPreview: 'settled',
    });
    void poller;
  });

  it('should load the full task list once per session when the session opens', async () => {
    const state = createState();
    apiMock.listTasks.mockResolvedValue([makeTask('t-done', 'completed')]);
    apiMock.getTaskOutput.mockResolvedValue('final');

    const poller = useTaskPoller(state, computed(() => state.tasksBySession['s-1'] ?? []));
    await poller.loadTasksForSession('s-1');

    expect(apiMock.listTasks).toHaveBeenCalledWith('s-1');
    expect(state.tasksBySession['s-1']).toHaveLength(1);
    // The terminal row's final output backfills through the single-task face.
    expect(apiMock.getTaskOutput).toHaveBeenCalledWith('s-1', 't-done', expect.any(Number));
  });
});
