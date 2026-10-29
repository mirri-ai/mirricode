// apps/mirri-web/test/workspace-state-recovery.test.ts
// Interrupted-task recovery (ADR-004 pull model): the workspace-state layer's
// recovery list + optimistic actions, and the ADR-010 read-side merge helper.
// Follows workspace-state.test.ts's vi.hoisted apiMock + plain-object state
// pattern; no jsdom (pure logic, node environment).

import { computed, ref } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppRecoveryInterruption, AppSession } from '../src/api/types';
import { createInitialState } from '../src/api/daemon/eventReducer';
import {
  useWorkspaceState,
  type UseWorkspaceStateDeps,
} from '../src/composables/client/useWorkspaceState';
// Value import on purpose: mergedLastTurnReason is the ADR-010 read-side merge
// exported from the facade for testing (the turnEndStatusFor precedent — the
// facade's module scope is side-effect safe under node).
import { mergedLastTurnReason } from '../src/composables/useMirriWebClient';
import type { ExtendedState } from '../src/composables/useMirriWebClient';

const apiMock = vi.hoisted(() => ({
  connectTransport: vi.fn(),
  dismissAllInterrupted: vi.fn(),
  dismissInterrupted: vi.fn(),
  getAuth: vi.fn(),
  getConfig: vi.fn(),
  getFsHome: vi.fn(),
  getHealth: vi.fn(),
  getMeta: vi.fn(),
  listRecovery: vi.fn(),
  listSessions: vi.fn(),
  listWorkspaces: vi.fn(),
  resumeAllInterrupted: vi.fn(),
  resumeInterrupted: vi.fn(),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

// useWorkspaceState resolves the singleton confirm dialog at init — mock it
// so tests never depend on dialog internals.
const confirmMock = vi.hoisted(() => vi.fn());
vi.mock('../src/composables/useConfirmDialog', () => ({
  useConfirmDialog: () => ({ confirm: confirmMock }),
}));

function createSession(id: string): AppSession {
  return {
    id,
    title: `Session ${id}`,
    createdAt: '2026-01-01T00:00:00.000Z',
    updatedAt: '2026-01-01T00:00:00.000Z',
    busy: false,
    archived: false,
    cwd: '/workspace',
    model: 'mirri-code',
    usage: {
      inputTokens: 0,
      outputTokens: 0,
      cacheReadTokens: 0,
      cacheCreationTokens: 0,
      totalCostUsd: 0,
      contextTokens: 0,
      contextLimit: 0,
      turnCount: 0,
    },
    messageCount: 0,
    lastSeq: 0,
  };
}

function interruption(sessionId: string): AppRecoveryInterruption {
  return {
    sessionId,
    workspaceKey: 'wd_1',
    workspaceName: 'Workspace',
    title: 'Cut-off task',
    interruptedAt: 1760000000000,
    cause: 'service_exit',
  };
}

function createState(): ExtendedState {
  return {
    ...createInitialState(),
    sessions: [createSession('sess_1'), createSession('sess_2'), createSession('sess_3')],
    activeSessionId: 'sess_1',
    connected: true,
    serverVersion: '',
    dangerousBypassAuth: false,
    backend: 'v3',
    workspaceName: 'mirri-web',
    connection: 'connected',
    permission: 'manual',
    thinking: 'high',
    thinkingBySession: {},
    planModeBySession: {},
    swarmModeBySession: {},
    swarmTaskBySession: {},
    collaborationModeBySession: {},
    goalModeBySession: {},
    loading: false,
    sessionLoading: false,
    queuedBySession: {},
    parkedPromptsBySession: {},
    gitStatusBySession: {},
    promptIdBySession: {},
    inFlightBySession: {},
    unreadBySession: {},
    authReady: true,
    defaultModel: null,
    workspaces: [],
    activeWorkspaceId: null,
    fsHome: null,
    recentRoots: [],
    hiddenWorkspaceRoots: [],
    availableOpenInApps: [],
    config: null,
    sideChatMessagesByAgent: {},
    sideChatSendingByAgent: {},
    sideChatUserMessageIdsBySession: {},
    messagesLoadingMoreBySession: {},
    messagesHasMoreBySession: {},
    messagesLoadMoreErrorBySession: {},
  };
}

function createHarness(opts?: { initialized?: boolean }) {
  const state = createState();
  const recoveryInterruptions = ref<AppRecoveryInterruption[]>([]);
  const recoveryNoticeDismissed = ref(false);
  const deps = {
    taskPoller: { loadTasksForSession: vi.fn() },
    sideChat: {},
    modelProvider: {
      resolveThinkingForPrompt: async () => undefined,
      skillsBySession: ref({}),
      loadSkillsForSession: vi.fn(),
      loadModels: vi.fn().mockResolvedValue(undefined),
    },
    pushOperationFailure: vi.fn(),
    activity: computed(() => 'running'),
    sessionsKnownEmpty: new Set<string>(),
    // The facade's real session-list funnels, wired onto this harness state —
    // dismiss must actually mutate the cached session rows.
    setSessions: (next: AppSession[]) => {
      state.sessions = next;
    },
    updateSession: (id: string, update: (session: AppSession) => AppSession) => {
      state.sessions = state.sessions.map((s) => (s.id === id ? update(s) : s));
    },
    upsertSessionFront: vi.fn(),
    appendSession: vi.fn(),
    forgetSession: vi.fn(),
    setActiveSessionId: vi.fn(),
    updateSessionMessages: vi.fn(),
    nextOptimisticMsgId: () => 'msg_opt_1',
    getEventConn: () => null,
    syncSessionFromSnapshot: vi.fn().mockResolvedValue('ok'),
    reopenSession: vi.fn().mockResolvedValue('ok'),
    hasLoadedMessages: () => true,
    refreshSessionStatus: vi.fn(),
    refreshSessionGoal: vi.fn(),
    persistSessionProfile: vi.fn().mockResolvedValue(true),
    mergedWorkspaces: computed(() => []),
    workspacesView: computed(() => []),
    status: computed(() => ({})),
    workspaceIdForSession: () => 'wd_1',
    savePermissionToStorage: vi.fn(),
    savePlanModeToStorage: vi.fn(),
    saveSwarmModeToStorage: vi.fn(),
    saveCollaborationModeToStorage: vi.fn(),
    saveGoalModeToStorage: vi.fn(),
    draftModes: { planMode: false, swarmMode: false, collaborationMode: false, goalMode: false },
    saveUnread: vi.fn(),
    saveActiveWorkspaceToStorage: vi.fn(),
    saveHiddenWorkspacesToStorage: vi.fn(),
    goalErrorMessage: vi.fn(),
    resetFastMoon: vi.fn(),
    initialized: ref(opts?.initialized ?? true),
    connectIssue: ref(null),
    selectedDiffPath: ref(null),
    fileDiffLines: ref([]),
    fileDiffLoading: ref(false),
    recoveryInterruptions,
  } as unknown as UseWorkspaceStateDeps;
  const ws = useWorkspaceState(state, deps);
  return { state, deps, ws, recoveryInterruptions, recoveryNoticeDismissed };
}

describe('useWorkspaceState — interrupted-task recovery', () => {
  beforeEach(() => {
    apiMock.dismissAllInterrupted.mockReset();
    apiMock.dismissInterrupted.mockReset();
    apiMock.listRecovery.mockReset();
    apiMock.resumeAllInterrupted.mockReset();
    apiMock.resumeInterrupted.mockReset();
  });

  it('should remove the row and open the session when a resume succeeds', async () => {
    const { deps, ws, recoveryInterruptions } = createHarness();
    recoveryInterruptions.value = [interruption('sess_1'), interruption('sess_2')];
    apiMock.resumeInterrupted.mockResolvedValue(undefined);

    await ws.resumeInterrupted('sess_1');

    expect(apiMock.resumeInterrupted).toHaveBeenCalledWith('sess_1');
    expect(recoveryInterruptions.value.map((e) => e.sessionId)).toEqual(['sess_2']);
    // selectSession ran: the active-session handoff fired.
    expect(deps.setActiveSessionId).toHaveBeenCalledWith('sess_1');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('should keep the list and surface a warning when the resume RPC fails', async () => {
    const { deps, ws, recoveryInterruptions } = createHarness();
    recoveryInterruptions.value = [interruption('sess_1')];
    apiMock.resumeInterrupted.mockRejectedValue(new Error('engine refused'));

    await ws.resumeInterrupted('sess_1');

    expect(recoveryInterruptions.value).toHaveLength(1);
    expect(deps.pushOperationFailure).toHaveBeenCalledWith(
      'resumeInterrupted',
      expect.any(Error),
      expect.objectContaining({ sessionId: 'sess_1' }),
    );
    expect(deps.setActiveSessionId).not.toHaveBeenCalled();
  });

  it('should remove the row and clear the cached interrupted outcome when a dismissal succeeds', async () => {
    const { state, ws, recoveryInterruptions } = createHarness();
    state.sessions[0]!.lastTurnReason = 'interrupted';
    recoveryInterruptions.value = [interruption('sess_1')];
    apiMock.dismissInterrupted.mockResolvedValue(undefined);

    await ws.dismissInterrupted('sess_1');

    expect(apiMock.dismissInterrupted).toHaveBeenCalledWith('sess_1');
    expect(recoveryInterruptions.value).toEqual([]);
    // V15 pin: the cached outcome is cleared so the event-value fallback arm
    // cannot re-light a stale badge.
    expect(state.sessions[0]!.lastTurnReason).toBeUndefined();
  });

  it('should leave the list and the cached outcome untouched when the dismiss RPC fails', async () => {
    const { state, deps, ws, recoveryInterruptions } = createHarness();
    state.sessions[0]!.lastTurnReason = 'interrupted';
    recoveryInterruptions.value = [interruption('sess_1')];
    apiMock.dismissInterrupted.mockRejectedValue(new Error('engine refused'));

    await ws.dismissInterrupted('sess_1');

    expect(recoveryInterruptions.value).toHaveLength(1);
    expect(state.sessions[0]!.lastTurnReason).toBe('interrupted');
    expect(deps.pushOperationFailure).toHaveBeenCalledWith(
      'dismissInterrupted',
      expect.any(Error),
      expect.objectContaining({ sessionId: 'sess_1' }),
    );
  });

  it('should keep failed rows and push a warning when resumeAll partially fails', async () => {
    const { state, ws, recoveryInterruptions } = createHarness();
    recoveryInterruptions.value = [interruption('sess_1'), interruption('sess_2')];
    apiMock.resumeAllInterrupted.mockResolvedValue({
      accepted: 1,
      queued: 0,
      failed: [{ sessionId: 'sess_2', error: 'session busy' }],
    });

    await ws.resumeAllInterrupted();

    expect(recoveryInterruptions.value.map((e) => e.sessionId)).toEqual(['sess_2']);
    expect(state.warnings).toHaveLength(1);
    const warning = state.warnings[0] as { severity: string; message?: string };
    expect(warning.severity).toBe('error');
    // One aggregated warning carries the i18n count message (Task 4); the
    // failed rows stay on the list so their per-row error stays reachable.
    expect(warning.message).toBe('Failed to resume 1 task(s)');
  });

  it('should clear every row and every cached interrupted outcome when dismissing all', async () => {
    const { state, ws, recoveryInterruptions } = createHarness();
    state.sessions[0]!.lastTurnReason = 'interrupted';
    state.sessions[1]!.lastTurnReason = 'interrupted';
    state.sessions[2]!.lastTurnReason = 'completed';
    recoveryInterruptions.value = [interruption('sess_1'), interruption('sess_2')];
    apiMock.dismissAllInterrupted.mockResolvedValue(undefined);

    await ws.dismissAllInterrupted();

    expect(recoveryInterruptions.value).toEqual([]);
    expect(state.sessions[0]!.lastTurnReason).toBeUndefined();
    expect(state.sessions[1]!.lastTurnReason).toBeUndefined();
    // Sessions that were never on the recovery list keep their own outcome.
    expect(state.sessions[2]!.lastTurnReason).toBe('completed');
  });

  it('should not reset the notice-dismissed flag when the recovery list refreshes', async () => {
    const { ws, recoveryInterruptions, recoveryNoticeDismissed } = createHarness();
    recoveryNoticeDismissed.value = true;
    apiMock.listRecovery.mockResolvedValue({ interruptions: [interruption('sess_1')] });

    await ws.refreshRecovery();

    expect(recoveryNoticeDismissed.value).toBe(true);
    expect(recoveryInterruptions.value).toHaveLength(1);
  });
});

describe('useWorkspaceState — recovery cold start', () => {
  beforeEach(() => {
    apiMock.connectTransport.mockReset().mockResolvedValue(undefined);
    apiMock.getMeta.mockReset().mockResolvedValue({
      serverVersion: '0.0.0',
      openInApps: [],
      dangerousBypassAuth: true,
      backend: 'v3',
    });
    apiMock.getHealth.mockReset().mockResolvedValue({ status: 'ok', uptimeSec: 0 });
    apiMock.getConfig.mockReset().mockResolvedValue({});
    apiMock.listWorkspaces.mockReset().mockResolvedValue([]);
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '', recentRoots: [] });
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [], hasMore: false });
    apiMock.listRecovery.mockReset();
  });

  it('should fetch the recovery list in the background on cold start', async () => {
    apiMock.listRecovery.mockResolvedValue({ interruptions: [interruption('sess_1')] });
    const { ws, recoveryInterruptions } = createHarness({ initialized: false });

    await ws.load();

    expect(apiMock.listRecovery).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => {
      expect(recoveryInterruptions.value.map((e) => e.sessionId)).toEqual(['sess_1']);
    });
  });
});

describe('mergedLastTurnReason (ADR-010 read-side merge)', () => {
  it('should report interrupted when the session is on the recovery list', () => {
    const ids = new Set(['sess_1']);
    // The recovery list wins even over a settled 'completed' outcome.
    expect(mergedLastTurnReason('sess_1', ids, 'completed')).toBe('interrupted');
  });

  it('should fall back to the session outcome when the session is not on the recovery list', () => {
    const ids = new Set(['sess_9']);
    expect(mergedLastTurnReason('sess_1', ids, 'completed')).toBe('completed');
    expect(mergedLastTurnReason('sess_1', ids, undefined)).toBeUndefined();
    // The wire's own interrupted value still shows once the row leaves the list.
    expect(mergedLastTurnReason('sess_1', ids, 'interrupted')).toBe('interrupted');
  });
});
