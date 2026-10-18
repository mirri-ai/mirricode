// apps/mirri-web/test/use-terminal.test.ts
// Pure-logic tests for the terminal wiring (B11 / D-35): the `useTerminal`
// composable's session-scoped RPC usage and the 'terminal' slot of the shared
// right-side detail layer. No jsdom / component harness in this repo — the
// REST/WS api is mocked and `WebSocket` is stubbed so `ensureConnection` takes
// the live path instead of bailing on the node test environment.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { nextTick, ref, type Ref } from 'vue';
import { useTerminal } from '../src/composables/useTerminal';
import { useDetailPanel } from '../src/composables/useDetailPanel';
import type { DetailTarget } from '../src/composables/useFilePreview';
import type { useMirriWebClient } from '../src/composables/useMirriWebClient';
import type { AppTask, AppTerminal, TerminalEventSinks } from '../src/api/types';

type MirriWebClient = ReturnType<typeof useMirriWebClient>;

const apiMock = vi.hoisted(() => ({
  listTerminals: vi.fn(),
  createTerminal: vi.fn(),
  closeTerminal: vi.fn(),
  connectEvents: vi.fn(),
  connectTerminalEvents: vi.fn(),
  listMessages: vi.fn(),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

interface ConnectionMock {
  terminalAttach: ReturnType<typeof vi.fn>;
  terminalInput: ReturnType<typeof vi.fn>;
  terminalResize: ReturnType<typeof vi.fn>;
  terminalDetach: ReturnType<typeof vi.fn>;
  terminalClose: ReturnType<typeof vi.fn>;
  close: ReturnType<typeof vi.fn>;
}

/** The app's single event connection (what connectEvents would hand out). */
let appConnection: ConnectionMock;
/** Sinks currently bound through connectTerminalEvents, if any. */
let boundSinks: TerminalEventSinks | null;
/** Handlers of a connectEvents() call, if the fallback path ran. */
let handlers: Record<string, (...args: unknown[]) => void>;

function makeTerminal(overrides: Partial<AppTerminal> = {}): AppTerminal {
  return {
    id: 'term_1',
    sessionId: 'sess_1',
    cwd: '/workspace',
    shell: '/bin/zsh',
    cols: 80,
    rows: 24,
    status: 'running',
    createdAt: '2026-09-13T00:00:00.000Z',
    ...overrides,
  };
}

function emitTerminalOutput(sessionId: string, terminalId: string, data: string, seq: number): void {
  if (boundSinks !== null) boundSinks.onTerminalOutput(sessionId, terminalId, data, seq);
  else handlers.onTerminalOutput?.(sessionId, terminalId, data, seq);
}

function emitTerminalExit(sessionId: string, terminalId: string, exitCode: number | null): void {
  if (boundSinks !== null) boundSinks.onTerminalExit(sessionId, terminalId, exitCode);
  else handlers.onTerminalExit?.(sessionId, terminalId, exitCode);
}

beforeEach(() => {
  // `ensureConnection` bails when WebSocket is undefined (node test env), so
  // stub its mere existence — the connection objects themselves are the mocks
  // below.
  vi.stubGlobal('WebSocket', class {});
  boundSinks = null;
  handlers = {};
  appConnection = {
    terminalAttach: vi.fn(),
    terminalInput: vi.fn(),
    terminalResize: vi.fn(),
    terminalDetach: vi.fn(),
    terminalClose: vi.fn(),
    close: vi.fn(),
  };
  // Default: the api offers the additive terminal binding (the rust backend)
  // — close() on the returned binding releases ONLY the sinks.
  apiMock.connectTerminalEvents.mockImplementation((sinks: TerminalEventSinks) => {
    boundSinks = sinks;
    return {
      ...appConnection,
      close: () => {
        boundSinks = null;
      },
    };
  });
  // Fallback: a private connection per call (the v1/v2 TS daemon).
  apiMock.connectEvents.mockImplementation(
    (h: Record<string, (...args: unknown[]) => void>) => {
      Object.assign(handlers, h);
      return appConnection;
    },
  );
  apiMock.listTerminals.mockReset().mockResolvedValue([]);
  apiMock.createTerminal.mockReset().mockResolvedValue(makeTerminal());
  apiMock.closeTerminal.mockReset().mockResolvedValue({ closed: true });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe('useTerminal', () => {
  it('should create a terminal and attach with lastSeq 0 when none is running', async () => {
    apiMock.listTerminals.mockResolvedValue([]);
    apiMock.createTerminal.mockResolvedValue(makeTerminal({ id: 'term_new' }));

    const { start, terminal } = useTerminal(ref('sess_1'));
    await start({ cols: 100, rows: 30 });

    expect(apiMock.createTerminal).toHaveBeenCalledWith('sess_1', { cols: 100, rows: 30 });
    expect(appConnection.terminalAttach).toHaveBeenCalledWith('sess_1', 'term_new', 0);
    expect(terminal.value?.id).toBe('term_new');
  });

  it('should reuse the existing running terminal instead of creating one', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);

    const { start } = useTerminal(ref('sess_1'));
    await start();

    expect(apiMock.createTerminal).not.toHaveBeenCalled();
    expect(appConnection.terminalAttach).toHaveBeenCalledWith('sess_1', 'term_live', 0);
  });

  it('should bind through connectTerminalEvents and never rebind the app event handlers', async () => {
    // The rust backend's event connection is a singleton whose connectEvents()
    // REPLACES the app's bound handler set — the composable must ride the
    // additive terminal binding instead (regression guard for the connection
    // hijack found in review).
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);

    const { start } = useTerminal(ref('sess_1'));
    await start();

    expect(apiMock.connectTerminalEvents).toHaveBeenCalledTimes(1);
    expect(apiMock.connectEvents).not.toHaveBeenCalled();
    expect(boundSinks).not.toBeNull();
  });

  it('should fall back to owning a private connectEvents connection when the backend has no additive binding', async () => {
    const original = apiMock.connectTerminalEvents;
    apiMock.connectTerminalEvents = undefined as never;
    try {
      apiMock.listTerminals.mockResolvedValue([
        makeTerminal({ id: 'term_live', status: 'running' }),
      ]);

      const { start } = useTerminal(ref('sess_1'));
      await start();

      expect(apiMock.connectEvents).toHaveBeenCalledTimes(1);
      expect(appConnection.terminalAttach).toHaveBeenCalledWith('sess_1', 'term_live', 0);
      expect(typeof handlers.onTerminalOutput).toBe('function');
    } finally {
      apiMock.connectTerminalEvents = original;
    }
  });

  it('should release only the terminal sinks on session switch, never the app connection', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);
    const sessionId = ref('sess_1');

    const { start } = useTerminal(sessionId);
    await start();

    sessionId.value = 'sess_2';
    await nextTick();

    expect(appConnection.terminalDetach).toHaveBeenCalledWith('sess_1', 'term_live');
    // The sinks are released, but the app's shared event connection is NOT
    // closed (regression guard for the connection hijack found in review).
    expect(boundSinks).toBeNull();
    expect(appConnection.close).not.toHaveBeenCalled();
  });

  it('should route matching terminal_output to onOutput and drop foreign sessions or terminals', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);
    const received: string[] = [];

    const { start, onOutput } = useTerminal(ref('sess_1'));
    onOutput((data) => received.push(data));
    await start();

    emitTerminalOutput('sess_1', 'term_live', 'hello', 1);
    emitTerminalOutput('sess_other', 'term_live', 'dropped', 2);
    emitTerminalOutput('sess_1', 'term_other', 'dropped', 3);

    expect(received).toEqual(['hello']);
  });

  it('should become read-only after terminal_exit and stop writing input', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);
    let exited: number | null | undefined;

    const { start, onExit, write, terminal } = useTerminal(ref('sess_1'));
    onExit((code) => {
      exited = code;
    });
    await start();

    emitTerminalExit('sess_1', 'term_live', 0);

    expect(exited).toBe(0);
    expect(terminal.value?.status).toBe('exited');
    write('still typing\n');
    expect(appConnection.terminalInput).not.toHaveBeenCalled();
  });

  it('should write and resize through the connection control frames', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);

    const { start, write, resize } = useTerminal(ref('sess_1'));
    await start();

    write('echo ok\n');
    resize(120, 40);

    expect(appConnection.terminalInput).toHaveBeenCalledWith('sess_1', 'term_live', 'echo ok\n');
    expect(appConnection.terminalResize).toHaveBeenCalledWith('sess_1', 'term_live', 120, 40);
  });

  it('should close through the control frame and REST', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);

    const { start, close } = useTerminal(ref('sess_1'));
    await start();

    await close();

    expect(appConnection.terminalClose).toHaveBeenCalledWith('sess_1', 'term_live');
    expect(apiMock.closeTerminal).toHaveBeenCalledWith('sess_1', 'term_live');
  });

  it('should kill the old terminal and create a fresh one on restart', async () => {
    apiMock.listTerminals.mockResolvedValue([
      makeTerminal({ id: 'term_live', status: 'running' }),
    ]);

    const { start, restart, terminal } = useTerminal(ref('sess_1'));
    await start();

    apiMock.listTerminals.mockResolvedValue([]);
    apiMock.createTerminal.mockResolvedValue(makeTerminal({ id: 'term_2' }));
    await restart();

    expect(appConnection.terminalDetach).toHaveBeenCalledWith('sess_1', 'term_live');
    expect(appConnection.terminalClose).toHaveBeenCalledWith('sess_1', 'term_live');
    expect(apiMock.closeTerminal).toHaveBeenCalledWith('sess_1', 'term_live');
    expect(terminal.value?.id).toBe('term_2');
    expect(appConnection.terminalAttach).toHaveBeenCalledWith('sess_1', 'term_2', 0);
  });
});

describe('terminal detail-panel wiring', () => {
  function createPanel() {
    const detailTarget = ref<DetailTarget | null>(null);
    const client = {
      turns: ref([]),
      activeAppTasks: ref([]),
      activeSessionId: ref<string | null>('sess_1'),
      sideChatVisible: ref(false),
      loadGitStatus: vi.fn(),
      clearFileDiff: vi.fn(),
      openSideChat: vi.fn(),
      closeSideChat: vi.fn(),
    } as unknown as MirriWebClient;
    const panel = useDetailPanel({
      client,
      sideWidth: ref(240),
      detailTarget,
      closeFilePreview: vi.fn(),
    });
    return { panel, detailTarget, client };
  }

  it('should claim the shared detail slot when the terminal panel is opened', () => {
    const { panel, detailTarget } = createPanel();

    panel.openTerminalPanel();

    expect(detailTarget.value).toBe('terminal');
    expect(panel.sidePanelVisible.value).toBe(true);
  });

  it('should not open the terminal panel without an active session', () => {
    const { panel, detailTarget, client } = createPanel();
    client.activeSessionId.value = null;

    panel.openTerminalPanel();

    expect(detailTarget.value).toBeNull();
    expect(panel.sidePanelVisible.value).toBe(false);
  });

  it('should toggle the terminal panel closed when it is already open', () => {
    const { panel, detailTarget } = createPanel();
    panel.openTerminalPanel();

    panel.openTerminalPanel();

    expect(detailTarget.value).toBeNull();
    expect(panel.sidePanelVisible.value).toBe(false);
  });

  it('should close an open terminal panel through the escape cascade', () => {
    const { panel, detailTarget } = createPanel();
    panel.openTerminalPanel();

    const handled = panel.closeOpenSidePanel();

    expect(handled).toBe(true);
    expect(detailTarget.value).toBeNull();
  });

  it('should not report an escape cascade hit when no panel is open', () => {
    const { panel } = createPanel();

    expect(panel.closeOpenSidePanel()).toBe(false);
  });

  it('should close the terminal panel when the active session switches', async () => {
    const { panel, detailTarget, client } = createPanel();
    panel.openTerminalPanel();

    client.activeSessionId.value = 'sess_2';
    await nextTick();

    expect(detailTarget.value).toBeNull();
  });
});

describe('subagent detail-panel history loading', () => {
  /** The panel's client only reads these fields for the agent panel path:
   * activeAppTasks (live member lookup + history fetch gating),
   * swarmMembersByToolCallId, activeSessionId and the api for journal fetches. */
  function createAgentPanelClient(activeAppTasks: Ref<AppTask[]>) {
    return {
      turns: ref([]),
      activeAppTasks,
      swarmMembersByToolCallId: ref(new Map<string, never[]>()),
      activeSessionId: ref<string | null>('sess_1'),
      sideChatVisible: ref(false),
      cancelTask: vi.fn(),
      loadGitStatus: vi.fn(),
      clearFileDiff: vi.fn(),
      openSideChat: vi.fn(),
      closeSideChat: vi.fn(),
    } as unknown as MirriWebClient;
  }

  function makeSubagentTask(overrides: Partial<AppTask> = {}): AppTask {
    return {
      id: 'agent_1',
      sessionId: 'sess_1',
      kind: 'subagent',
      description: 'Explore the codebase',
      status: 'cancelled',
      subagentPhase: 'aborted',
      createdAt: '2026-10-14T00:00:00.000Z',
      parentToolCallId: 'tc_1',
      agentId: 'agent_1',
      ...overrides,
    };
  }

  function flushMicrotasks(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
  }

  it('should fetch the member history once for an aborted member while unrelated task events keep refreshing the task list', async () => {
    // Given an aborted subagent task and an unrelated running sibling that
    // keeps streaming (its taskProgress events rebuild the tasks array on
    // every frame), the panel must fetch the journal once — not once per
    // refresh. (Regression guard: the panel used to flicker because each
    // rebuilt task object re-triggered the history watch and re-fetched.)
    const tasks = ref<AppTask[]>([
      makeSubagentTask(),
      makeSubagentTask({ id: 'agent_2', agentId: 'agent_2', status: 'running', subagentPhase: 'working' }),
    ]);
    const client = createAgentPanelClient(tasks);
    const detailTarget = ref<DetailTarget | null>(null);
    apiMock.listMessages.mockResolvedValue({ items: [], hasMore: false });

    const panel = useDetailPanel({
      client,
      sideWidth: ref(240),
      detailTarget,
      closeFilePreview: vi.fn(),
    });
    panel.openAgentPanel('tc_1');
    await flushMicrotasks();

    expect(apiMock.listMessages).toHaveBeenCalledTimes(1);

    // When the sibling keeps streaming: the tasks array is rebuilt many times
    // a second (eventReducer clones the whole tasksBySession map per event),
    // but the aborted member's identity did not change.
    for (let i = 0; i < 10; i += 1) {
      tasks.value = tasks.value.map((t) => ({ ...t }));
      await nextTick();
    }

    // Then the panel must not have re-fetched the member's journal.
    expect(apiMock.listMessages).toHaveBeenCalledTimes(1);
    expect(panel.agentPanelHistoryLoading.value).toBe(false);
  });

  it('should re-fetch the member history when the focused member actually changes', async () => {
    // Given two aborted members in the same delegation, switching the focused
    // member must fetch the new member's journal.
    const tasks = ref<AppTask[]>([
      makeSubagentTask(),
      makeSubagentTask({ id: 'agent_2', agentId: 'agent_2' }),
    ]);
    const client = createAgentPanelClient(tasks);
    const detailTarget = ref<DetailTarget | null>(null);
    apiMock.listMessages.mockResolvedValue({ items: [], hasMore: false });

    const panel = useDetailPanel({
      client,
      sideWidth: ref(240),
      detailTarget,
      closeFilePreview: vi.fn(),
    });
    panel.openAgentPanel('tc_1');
    await flushMicrotasks();
    expect(apiMock.listMessages).toHaveBeenCalledTimes(1);

    // When the user selects the other member.
    panel.selectPanelMember('agent_2');
    await flushMicrotasks();

    // Then the new member's journal is fetched.
    expect(apiMock.listMessages).toHaveBeenCalledTimes(2);
    expect(apiMock.listMessages).toHaveBeenLastCalledWith(
      'sess_1',
      { agentId: 'agent_2' },
    );
  });
});
