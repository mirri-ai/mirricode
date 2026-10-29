import { computed, ref, type Ref } from 'vue';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppApprovalRequest, AppQuestionRequest, AppMessage, AppSession, AppTask } from '../src/api/types';
import { DaemonApiError } from '../src/api/errors';
import { JsonRpcCallError } from '../src/api/daemon/json-rpc-client';
import { clearCredential, setCredential } from '../src/api/daemon/serverAuth';
import { createInitialState } from '../src/api/daemon/eventReducer';
import { mergeWorkspaces } from '../src/lib/mergeWorkspaces';
import { loadWorkspaceNameOverrides, saveWorkspaceNameOverrides } from '../src/lib/storage';
import {
  useWorkspaceState,
  pickInitialWorkspaceIdForFirstLoad,
  type UseWorkspaceStateDeps,
} from '../src/composables/client/useWorkspaceState';
import {
  buildTrustWorkspaceDialogModel,
  useWorkspaceTrustPrompt,
} from '../src/composables/useWorkspaceTrustPrompt';
import { i18n } from '../src/i18n';
import type { ExtendedState } from '../src/composables/useMirriWebClient';

const apiMock = vi.hoisted(() => ({
  abortPrompt: vi.fn(),
  abortSession: vi.fn(),
  compactSession: vi.fn(),
  addWorkspace: vi.fn(),
  updateWorkspace: vi.fn(),
  deleteWorkspace: vi.fn(),
  setWorkspaceTrust: vi.fn(),
  createSession: vi.fn(),
  updateSession: vi.fn(),
  submitPrompt: vi.fn(),
  respondQuestion: vi.fn(),
  respondApproval: vi.fn(),
  dismissQuestion: vi.fn(),
  cancelTask: vi.fn(),
  connectTransport: vi.fn(),
  getAuth: vi.fn(),
  getConfig: vi.fn(),
  getFsHome: vi.fn(),
  getHealth: vi.fn(),
  getMeta: vi.fn(),
  listSessions: vi.fn(),
  listWorkspaces: vi.fn(),
  listPrompts: vi.fn(),
  refreshWorkspace: vi.fn(),
  steerSession: vi.fn(),
  // Interrupted-task recovery: load() pulls the recovery list in the
  // background, so every load()-driven test needs a resolving mock.
  listRecovery: vi.fn(),
}));

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

// B10 D-25: swarm/collab enable paths go through the singleton confirm dialog
// in manual permission mode — mock it so tests can drive confirm/cancel.
const confirmMock = vi.hoisted(() => vi.fn());
vi.mock('../src/composables/useConfirmDialog', () => ({
  useConfirmDialog: () => ({ confirm: confirmMock }),
}));

function createSession(): AppSession {
  return {
    id: 'sess_1',
    title: 'Session',
    createdAt: '2026-01-01T00:00:00.000Z',
    updatedAt: '2026-01-01T00:00:00.000Z',
    busy: true,
    archived: false,
    currentPromptId: 'prompt_live',
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

function createState(): ExtendedState {
  return {
    ...createInitialState(),
    sessions: [createSession()],
    activeSessionId: 'sess_1',
    connected: true,
    serverVersion: '',
    dangerousBypassAuth: false,
    backend: 'v1',
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
    promptIdBySession: { sess_1: 'prompt_stale' },
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

function createDeps(): UseWorkspaceStateDeps {
  return {
    taskPoller: {},
    sideChat: {},
    modelProvider: { resolveThinkingForPrompt: async () => undefined },
    pushOperationFailure: vi.fn(),
    activity: computed(() => 'running'),
    sessionsKnownEmpty: new Set(),
    setSessions: vi.fn(),
    updateSession: vi.fn(),
    upsertSessionFront: vi.fn(),
    appendSession: vi.fn(),
    forgetSession: vi.fn(),
    setActiveSessionId: vi.fn(),
    updateSessionMessages: vi.fn(),
    nextOptimisticMsgId: () => 'msg_opt_1',
    getEventConn: () => null,
    syncSessionFromSnapshot: vi.fn(),
    subscribeToSessionEvents: vi.fn(),
    hasLoadedMessages: vi.fn(),
    refreshSessionStatus: vi.fn(),
    refreshSessionGoal: vi.fn(),
    persistSessionProfile: vi.fn().mockResolvedValue(true),
    mergedWorkspaces: computed(() => []),
    workspacesView: computed(() => []),
    status: computed(() => ({})),
    workspaceIdForSession: vi.fn(),
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
    basename: (path: string) => path.split('/').at(-1) ?? path,
    resetFastMoon: vi.fn(),
    initialized: ref(true),
    connectIssue: ref(null),
    selectedDiffPath: ref(null),
    fileDiffLines: ref([]),
    fileDiffLoading: ref(false),
  } as unknown as UseWorkspaceStateDeps;
}

function createMemoryStorage(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    clear() {
      data.clear();
    },
    getItem(key: string) {
      return data.get(key) ?? null;
    },
    key(index: number) {
      return Array.from(data.keys()).at(index) ?? null;
    },
    removeItem(key: string) {
      data.delete(key);
    },
    setItem(key: string, value: string) {
      data.set(key, String(value));
    },
  };
}

function installStorage(storage: Storage): void {
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: storage,
  });
}

function workspace(id: string, root: string, name: string, trusted = true) {
  return { id, root, name, isGitRepo: false, sessionCount: 0, trusted };
}

// load() connects the RPC transport eagerly regardless of credential state
// (a bypass-auth engine never issues one), so every load()-driven test needs
// the shared mock in the resolved state unless the test overrides it. Without
// this, a connectTransport mock left rejected by an earlier describe would
// abort later loads at the (non-fatal but gate-skipping) connect step.
beforeEach(() => {
  apiMock.connectTransport.mockReset().mockResolvedValue(undefined);
});

// Recovery-list cold start: load() fires refreshRecovery() in the background
// with a resolving mock by default (individual tests override it).
beforeEach(() => {
  apiMock.listRecovery.mockReset().mockResolvedValue({ interruptions: [] });
});

function questionRequest(questionId: string): AppQuestionRequest {
  return {
    questionId,
    sessionId: 'sess_1',
    questions: [
      {
        id: 'q1',
        question: 'Pick one',
        options: [{ id: 'a', label: 'A' }],
      },
    ],
    createdAt: '2026-01-01T00:00:00.000Z',
  };
}

function approvalRequest(approvalId: string): AppApprovalRequest {
  return {
    approvalId,
    sessionId: 'sess_1',
    toolCallId: 'tc_1',
    toolName: 'bash',
    action: 'shell',
    display: null,
    expiresAt: '2099-01-01T00:00:00.000Z',
    createdAt: '2026-01-01T00:00:00.000Z',
  };
}

function task(id: string, status: AppTask['status'] = 'running'): AppTask {
  return {
    id,
    sessionId: 'sess_1',
    kind: 'bash',
    description: 'run',
    status,
    createdAt: '2026-01-01T00:00:00.000Z',
  };
}

describe('useWorkspaceState — abortCurrentPrompt', () => {
  beforeEach(() => {
    apiMock.abortPrompt.mockReset();
    apiMock.abortSession.mockReset();
  });

  it('falls back to session abort when the cached prompt id is already completed', async () => {
    apiMock.abortPrompt.mockResolvedValue({ aborted: false });
    apiMock.abortSession.mockResolvedValue({ aborted: true });
    const state = createState();
    const workspace = useWorkspaceState(state, createDeps());

    await workspace.abortCurrentPrompt();

    expect(apiMock.abortPrompt).toHaveBeenCalledWith('sess_1', 'prompt_stale');
    expect(apiMock.abortSession).toHaveBeenCalledWith('sess_1');
    expect(state.promptIdBySession).toEqual({});
  });

  it('does not fall back when prompt abort succeeds', async () => {
    apiMock.abortPrompt.mockResolvedValue({ aborted: true });
    const workspace = useWorkspaceState(createState(), createDeps());

    await workspace.abortCurrentPrompt();

    expect(apiMock.abortPrompt).toHaveBeenCalledWith('sess_1', 'prompt_stale');
    expect(apiMock.abortSession).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — compact', () => {
  beforeEach(() => {
    apiMock.compactSession.mockReset();
  });

  it('should show an info notice when the engine reports nothing to compact', async () => {
    apiMock.compactSession.mockResolvedValue({ started: false });
    const state = createState();
    const workspace = useWorkspaceState(state, createDeps());

    workspace.compact();

    await vi.waitFor(() => {
      expect(state.warnings.length).toBe(1);
    });
    const notice = state.warnings[0] as { severity: string; title: string };
    expect(notice.severity).toBe('info');
    expect(typeof notice.title).toBe('string');
    expect(notice.title.length).toBeGreaterThan(0);
    expect(apiMock.compactSession).toHaveBeenCalledWith('sess_1', undefined);
  });

  it('should show no notice when the compaction starts', async () => {
    apiMock.compactSession.mockResolvedValue({ started: true });
    const state = createState();
    const workspace = useWorkspaceState(state, createDeps());

    workspace.compact();
    await vi.waitFor(() => {
      expect(apiMock.compactSession).toHaveBeenCalled();
    });
    // Give any (unexpected) warning a chance to land.
    await new Promise((r) => setTimeout(r, 10));
    expect(state.warnings).toEqual([]);
  });
});

describe('mergeWorkspaces', () => {
  it('collapses registered workspaces that share a root, keeping the first entry and its sessions', () => {
    const result = mergeWorkspaces({
      workspaces: [
        // Server orders by last_opened_at desc, so the most recently opened
        // (typically the canonical re-add) comes first.
        { id: 'wd_current', root: '/agent/GEO', name: 'GEO', isGitRepo: false, sessionCount: 0 },
        { id: 'wd_legacy', root: '/agent/GEO', name: 'GEO', isGitRepo: false, sessionCount: 0 },
      ],
      // A session whose daemon workspace_id points at the dropped (legacy) entry.
      sessions: [{ id: 's1', cwd: '/agent/GEO', workspaceId: 'wd_legacy' }],
      hiddenWorkspaceRoots: [],
      activeRoot: undefined,
      activeBranch: null,
      sessionsHasMoreByWorkspace: { wd_current: false },
    });

    expect(result).toHaveLength(1);
    expect(result[0]?.root).toBe('/agent/GEO');
    // Keeps the first (most recent) entry, matching the sidebar's first-match
    // session assignment so the rendered workspace is the one sessions land under.
    expect(result[0]?.id).toBe('wd_current');
    expect(result[0]?.sessionCount).toBe(1);
  });

  it('keeps distinct roots separate and appends derived cwds after real ones', () => {
    const result = mergeWorkspaces({
      workspaces: [
        { id: 'wd_a', root: '/agent/A', name: 'A', isGitRepo: false, sessionCount: 1 },
      ],
      sessions: [
        { id: 's1', cwd: '/agent/A', workspaceId: 'wd_a' },
        { id: 's2', cwd: '/agent/B', workspaceId: 'wd_b' },
      ],
      hiddenWorkspaceRoots: [],
      activeRoot: undefined,
      activeBranch: null,
      sessionsHasMoreByWorkspace: {},
    });

    expect(result.map((w) => w.root)).toEqual(['/agent/A', '/agent/B']);
    expect(result.find((w) => w.root === '/agent/B')?.id).toBe('wd_b');
  });

  it('hides workspaces whose root the user removed', () => {
    const result = mergeWorkspaces({
      workspaces: [
        { id: 'wd_a', root: '/agent/A', name: 'A', isGitRepo: false, sessionCount: 1 },
      ],
      sessions: [{ id: 's1', cwd: '/agent/A', workspaceId: 'wd_a' }],
      hiddenWorkspaceRoots: ['/agent/A'],
      activeRoot: undefined,
      activeBranch: null,
      sessionsHasMoreByWorkspace: {},
    });

    expect(result.map((w) => w.root)).not.toContain('/agent/A');
  });
});

describe('useWorkspaceState — renameWorkspace', () => {
  beforeEach(() => {
    apiMock.updateWorkspace.mockReset();
    installStorage(createMemoryStorage());
  });

  afterEach(() => {
    installStorage(createMemoryStorage());
  });

  it('renames via the daemon and applies the name locally', async () => {
    apiMock.updateWorkspace.mockResolvedValue({});
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.renameWorkspace('wd_1', 'New');

    expect(apiMock.updateWorkspace).toHaveBeenCalledWith('wd_1', { name: 'New' });
    expect(state.workspaces[0]?.name).toBe('New');
    expect(loadWorkspaceNameOverrides()).toEqual({});
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('falls back to a local override when the daemon reports not found', async () => {
    apiMock.updateWorkspace.mockRejectedValue(
      new DaemonApiError({ code: 40410, msg: 'workspace not found', requestId: 'r' }),
    );
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.renameWorkspace('wd_1', 'New');

    expect(state.workspaces[0]?.name).toBe('New');
    expect(loadWorkspaceNameOverrides()).toEqual({ '/abs/path': 'New' });
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('falls back to a local override when the rust engine reports the unknown-id invalid_params', async () => {
    // The rust engine's rename answers a JSON-RPC -32602 with the W4/W5
    // "workspace {id} does not exist" spelling (no 40410 REST code) — the
    // derived-workspace fallback must catch both faces.
    const error = new JsonRpcCallError({
      code: -32602,
      message: 'workspace wd_1 does not exist',
    });
    apiMock.updateWorkspace.mockRejectedValue(error);
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.renameWorkspace('wd_1', 'New');

    expect(state.workspaces[0]?.name).toBe('New');
    expect(loadWorkspaceNameOverrides()).toEqual({ '/abs/path': 'New' });
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('surfaces daemon errors other than not-found', async () => {
    apiMock.updateWorkspace.mockRejectedValue(
      new DaemonApiError({ code: 50000, msg: 'boom', requestId: 'r' }),
    );
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.renameWorkspace('wd_1', 'New');

    expect(state.workspaces[0]?.name).toBe('Old');
    expect(loadWorkspaceNameOverrides()).toEqual({});
    expect(deps.pushOperationFailure).toHaveBeenCalled();
  });

  it('surfaces a rust invalid_params that is not the not-found spelling', async () => {
    // -32602 alone is NOT the fallback trigger — only the "does not exist"
    // message spelling is; e.g. the empty-name rejection must surface.
    const error = new JsonRpcCallError({
      code: -32602,
      message: 'workspace name must not be empty',
    });
    apiMock.updateWorkspace.mockRejectedValue(error);
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.renameWorkspace('wd_1', 'New');

    expect(state.workspaces[0]?.name).toBe('Old');
    expect(loadWorkspaceNameOverrides()).toEqual({});
    expect(deps.pushOperationFailure).toHaveBeenCalled();
  });

  it('keeps a saved name override when a workspace is upserted (derived → registered)', () => {
    // Simulates: user renamed a derived workspace, then the daemon registers
    // the root (e.g. on first chat) and returns the default basename.
    saveWorkspaceNameOverrides({ '/abs/path': 'Renamed' });
    const state = createState();
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    ws.upsertWorkspacePreserveOrder(workspace('wd_1', '/abs/path', 'path'));

    expect(state.workspaces[0]?.name).toBe('Renamed');
  });
});

describe('useWorkspaceState — addWorkspaceByPath', () => {
  beforeEach(() => {
    apiMock.addWorkspace.mockReset();
  });

  it('registers the workspace with the daemon and selects it', async () => {
    const registered = {
      id: 'wd_abc',
      root: '/abs/path',
      name: 'path',
      isGitRepo: false,
      sessionCount: 0,
    };
    apiMock.addWorkspace.mockResolvedValue(registered);
    const state = createState();
    const deps = createDeps();
    const workspace = useWorkspaceState(state, deps);

    const ok = await workspace.addWorkspaceByPath('  /abs/path  ');

    expect(ok).toBe(true);
    expect(apiMock.addWorkspace).toHaveBeenCalledWith({ root: '/abs/path' });
    expect(state.workspaces).toContainEqual(registered);
    expect(state.activeWorkspaceId).toBe('wd_abc');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('returns false and adds no local workspace on failure', async () => {
    const err = new Error('path not found');
    apiMock.addWorkspace.mockRejectedValue(err);
    const state = createState();
    const deps = createDeps();
    const workspace = useWorkspaceState(state, deps);

    const ok = await workspace.addWorkspaceByPath('/abs/missing');

    expect(ok).toBe(false);
    // The caller (the picker) is responsible for surfacing the failure inline.
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
    expect(state.workspaces).toEqual([]);
    expect(state.activeWorkspaceId).toBeNull();
  });
});

describe('useWorkspaceState — deleteWorkspace', () => {
  beforeEach(() => {
    apiMock.deleteWorkspace.mockReset();
    apiMock.deleteWorkspace.mockResolvedValue({
      ok: true,
      mode: 'remove',
      workspaceId: 'wd_1',
      deletedSessions: 0,
    });
    installStorage(createMemoryStorage());
  });

  afterEach(() => {
    installStorage(createMemoryStorage());
  });

  it('passes the purge mode through to the api and unregisters locally', async () => {
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.deleteWorkspace('wd_1', 'purge');

    expect(apiMock.deleteWorkspace).toHaveBeenCalledWith('wd_1', 'purge');
    expect(state.workspaces).toEqual([]);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('defaults to the data-preserving remove mode on the wire', async () => {
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.deleteWorkspace('wd_1');

    expect(apiMock.deleteWorkspace).toHaveBeenCalledWith('wd_1', 'remove');
  });

  it('keeps the local hide even when the registry delete fails (best-effort)', async () => {
    apiMock.deleteWorkspace.mockRejectedValue(new Error('refused'));
    const state = createState();
    state.workspaces = [workspace('wd_1', '/abs/path', 'Old')];
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.deleteWorkspace('wd_1', 'remove');

    expect(state.workspaces).toEqual([]);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — respondQuestion', () => {
  const response = { answers: {}, method: 'click' as const };

  beforeEach(() => {
    apiMock.respondQuestion.mockReset();
  });

  it('removes the question locally and stays silent when already resolved (40902)', async () => {
    apiMock.respondQuestion.mockRejectedValue(
      new DaemonApiError({ code: 40902, msg: 'question q_1 already resolved', requestId: 'r' }),
    );
    const state = createState();
    state.questionsBySession = { sess_1: [questionRequest('q_1')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.respondQuestion('q_1', response);

    expect(apiMock.respondQuestion).toHaveBeenCalledOnce();
    // Already resolved is the desired end state, so the card is dropped locally
    // without surfacing a duplicate error to the user.
    expect(state.questionsBySession['sess_1']).toEqual([]);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('surfaces genuine errors and keeps the question for retry', async () => {
    apiMock.respondQuestion.mockRejectedValue(
      new DaemonApiError({ code: 50001, msg: 'boom', requestId: 'r' }),
    );
    const state = createState();
    state.questionsBySession = { sess_1: [questionRequest('q_1')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.respondQuestion('q_1', response);

    expect(state.questionsBySession['sess_1']).toHaveLength(1);
    expect(deps.pushOperationFailure).toHaveBeenCalledOnce();
  });

  it('drops a duplicate submit while the first respond is still in flight', async () => {
    let resolveRespond!: (value: { resolved: true; resolvedAt: string }) => void;
    apiMock.respondQuestion.mockReturnValue(
      new Promise<{ resolved: true; resolvedAt: string }>((r) => {
        resolveRespond = r;
      }),
    );
    const state = createState();
    state.questionsBySession = { sess_1: [questionRequest('q_1')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    const first = ws.respondQuestion('q_1', response);
    // Second click while the first request is still in flight must be a no-op.
    await ws.respondQuestion('q_1', response);

    expect(apiMock.respondQuestion).toHaveBeenCalledOnce();

    // Resolve the first request and ensure the question is removed.
    resolveRespond({ resolved: true, resolvedAt: '2026-01-01T00:00:00.000Z' });
    await first;
    expect(state.questionsBySession['sess_1']).toEqual([]);
  });
});

describe('useWorkspaceState — respondApproval', () => {
  beforeEach(() => {
    apiMock.respondApproval.mockReset();
  });

  it('removes the approval locally and stays silent when already resolved (40902)', async () => {
    apiMock.respondApproval.mockRejectedValue(
      new DaemonApiError({ code: 40902, msg: 'approval a_1 already resolved', requestId: 'r' }),
    );
    const state = createState();
    state.approvalsBySession = { sess_1: [approvalRequest('a_1')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.respondApproval('a_1', { decision: 'approved' });

    expect(apiMock.respondApproval).toHaveBeenCalledOnce();
    expect(state.approvalsBySession['sess_1']).toEqual([]);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — cancelTask', () => {
  beforeEach(() => {
    apiMock.cancelTask.mockReset();
  });

  it('stays silent and does not force-cancel when the task already finished (40904)', async () => {
    apiMock.cancelTask.mockRejectedValue(
      new DaemonApiError({ code: 40904, msg: 'task t_1 already finished', requestId: 'r' }),
    );
    const state = createState();
    state.tasksBySession = { sess_1: [task('t_1', 'running')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.cancelTask('t_1');

    expect(apiMock.cancelTask).toHaveBeenCalledOnce();
    // Benign idempotent conflict — no error, and we do NOT lie about the
    // status (the task finished; it was not cancelled).
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
    expect(state.tasksBySession['sess_1']?.[0]?.status).toBe('running');
  });

  it('marks the task cancelled on success', async () => {
    apiMock.cancelTask.mockResolvedValue({ cancelled: true });
    const state = createState();
    state.tasksBySession = { sess_1: [task('t_1', 'running')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.cancelTask('t_1');

    expect(state.tasksBySession['sess_1']?.[0]?.status).toBe('cancelled');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('drops a duplicate cancel while the first is still in flight', async () => {
    let resolveCancel!: (value: { cancelled: true }) => void;
    apiMock.cancelTask.mockReturnValue(
      new Promise<{ cancelled: true }>((r) => {
        resolveCancel = r;
      }),
    );
    const state = createState();
    state.tasksBySession = { sess_1: [task('t_1', 'running')] };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    const first = ws.cancelTask('t_1');
    await ws.cancelTask('t_1');

    expect(apiMock.cancelTask).toHaveBeenCalledOnce();

    resolveCancel({ cancelled: true });
    await first;
  });
});

describe('useWorkspaceState — startSessionAndActivateSkill', () => {
  const registered = { id: 'wd_1', root: '/abs/path', name: 'A', isGitRepo: false, sessionCount: 0 };
  const newSession = { ...createSession(), id: 'sess_new', workspaceId: 'wd_1', cwd: '/abs/path' };

  beforeEach(() => {
    apiMock.addWorkspace.mockReset();
    apiMock.createSession.mockReset();
    apiMock.addWorkspace.mockResolvedValue(registered);
    apiMock.createSession.mockResolvedValue(newSession);
  });

  function skillDeps(activateSkill: ReturnType<typeof vi.fn>): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      taskPoller: { loadTasksForSession: vi.fn() } as unknown as UseWorkspaceStateDeps['taskPoller'],
      modelProvider: {
        draftModel: ref(null),
        skillsBySession: ref({}),
        loadSkillsForSession: vi.fn(),
        activateSkill,
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      mergedWorkspaces: computed(() => [workspace('wd_1', '/abs/path', 'A')]),
    };
  }

  it('creates a session, then activates the skill on the new session id', async () => {
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const deps = skillDeps(activateSkill);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndActivateSkill('wd_1', 'pre-changelog');

    expect(apiMock.createSession).toHaveBeenCalledOnce();
    // W4: the composer's create carries the selected workspace's id and
    // root into the api face (the ws-rpc layer then dual-sends them on the
    // wire) so the session lands in the workspace the user picked.
    expect(apiMock.createSession).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'wd_1', cwd: '/abs/path' }),
    );
    // The activation targets the freshly created session, so a concurrent
    // session switch can't redirect it.
    expect(activateSkill).toHaveBeenCalledWith('pre-changelog', undefined, 'sess_new');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('carries the draft thinking pick into the new session own entry', async () => {
    // A level picked on the empty composer has no session to live in yet; the
    // draft transfer seeds it so the first action submits the pick, not the
    // catalog default.
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const deps = skillDeps(activateSkill);
    const state = createState();
    state.thinking = 'max';
    const ws = useWorkspaceState(state, deps);

    await ws.startSessionAndActivateSkill('wd_1', 'pre-changelog');

    expect(state.thinkingBySession['sess_new']).toBe('max');
  });

  it('captures the draft thinking pick before the creation awaits', async () => {
    // A concurrent session switch mid-creation re-resolves rawState.thinking
    // for the other session — the seed must come from the pre-await capture.
    let resolveCreate!: (session: typeof newSession) => void;
    apiMock.createSession.mockReturnValue(
      new Promise<typeof newSession>((r) => {
        resolveCreate = r;
      }),
    );
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const deps = skillDeps(activateSkill);
    const state = createState();
    state.thinking = 'max';
    const ws = useWorkspaceState(state, deps);

    const pending = ws.startSessionAndActivateSkill('wd_1', 'pre-changelog');
    await new Promise((r) => setTimeout(r, 0));
    // The user switches to another session while createSession is in flight;
    // the watcher would re-resolve rawState.thinking to that session's level.
    state.thinking = 'low';
    resolveCreate(newSession);
    await pending;

    expect(state.thinkingBySession['sess_new']).toBe('max');
  });

  it('passes through skill args', async () => {
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const deps = skillDeps(activateSkill);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndActivateSkill('wd_1', 'write-goal', 'ship it');

    expect(activateSkill).toHaveBeenCalledWith('write-goal', 'ship it', 'sess_new');
  });

  it('awaits the profile POST before activating, so draft controls apply first', async () => {
    // The draft's permission pick is landed inside createDraftSession (SDR-004 —
    // the first persist below); skill activation then persists the rest of the
    // draft controls (plan/swarm) on top of it and must WAIT for that write,
    // otherwise :activate can race ahead of applyAgentState and the first
    // skill turn runs at daemon defaults while the UI shows otherwise.
    let resolveProfile!: (persisted: boolean) => void;
    const profileGate = new Promise<boolean>((r) => {
      resolveProfile = r;
    });
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const persistSessionProfile = vi.fn().mockReturnValue(profileGate);
    const deps = {
      ...skillDeps(activateSkill),
      persistSessionProfile,
      draftModes: { planMode: true, swarmMode: true, collaborationMode: false, goalMode: false },
    };
    const state = createState();
    state.permission = 'auto';
    state.thinking = 'high';
    const ws = useWorkspaceState(state, deps);

    const pending = ws.startSessionAndActivateSkill('wd_1', 'pre-changelog');
    // Yield a macrotask so createDraftSession's chain (which awaits selectSession
    // before persisting the profile) progresses to the in-flight /profile POST.
    // The permission mode lands first, and activation must NOT have started
    // while /profile is still pending.
    await new Promise((r) => setTimeout(r, 0));
    expect(persistSessionProfile).toHaveBeenCalledWith({ permissionMode: 'auto' }, 'sess_new');
    expect(activateSkill).not.toHaveBeenCalled();

    resolveProfile(true);
    await pending;

    // Once the profile write settles, the skill path persists the remaining
    // draft controls and activation proceeds on the new session id.
    expect(persistSessionProfile).toHaveBeenLastCalledWith(
      expect.objectContaining({
        planMode: true,
        swarmMode: true,
        collaborationMode: false,
        permissionMode: 'auto',
      }),
      'sess_new',
    );
    expect(activateSkill).toHaveBeenCalledWith('pre-changelog', undefined, 'sess_new');
  });

  it('does not write thinking in the draft profile patch — activateSkill persists it once', async () => {
    // activateSkill resolves and persists the level itself (gated) right
    // before activating. Duplicating the write in THIS patch would be a
    // redundant profile update whose transient failure could veto an
    // otherwise-ready activation, so the draft patch must not carry it.
    const activateSkill2 = vi.fn().mockResolvedValue(undefined);
    const persistSessionProfile2 = vi.fn().mockResolvedValue(true);
    const state2 = createState();
    state2.thinking = 'max';
    const deps2: UseWorkspaceStateDeps = {
      ...skillDeps(activateSkill2),
      persistSessionProfile: persistSessionProfile2,
      // upsertSessionFront must actually land the new session in rawState.sessions
      // so startSessionAndActivateSkill can read its model.
      upsertSessionFront: vi.fn((s) => {
        state2.sessions = [s, ...state2.sessions.filter((x) => x.id !== s.id)];
      }),
      draftModes: { planMode: true, swarmMode: false, collaborationMode: false, goalMode: false },
    };
    const ws2 = useWorkspaceState(state2, deps2);

    await ws2.startSessionAndActivateSkill('wd_1', 'pre-changelog');

    // createDraftSession lands the permission pick first (SDR-004); the skill
    // path's own draft patch is the lasting write and must not duplicate
    // thinking (activateSkill persists the level itself, single writer).
    expect(persistSessionProfile2).toHaveBeenCalledTimes(2);
    expect(persistSessionProfile2).toHaveBeenNthCalledWith(1, { permissionMode: 'manual' }, 'sess_new');
    const patch = persistSessionProfile2.mock.calls.at(-1)![0] as Record<string, unknown>;
    expect(patch).toMatchObject({ model: 'mirri-code', planMode: true, swarmMode: false });
    expect('thinking' in patch).toBe(false);
    expect(activateSkill2).toHaveBeenCalledWith('pre-changelog', undefined, 'sess_new');
  });

  it('is a no-op for an unknown workspace', async () => {
    const activateSkill = vi.fn().mockResolvedValue(undefined);
    const deps = skillDeps(activateSkill);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndActivateSkill('wd_missing', 'pre-changelog');

    expect(apiMock.createSession).not.toHaveBeenCalled();
    expect(activateSkill).not.toHaveBeenCalled();
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — createGoal from an empty composer', () => {
  const registered = { id: 'wd_1', root: '/abs/path', name: 'A', isGitRepo: false, sessionCount: 0 };
  const newSession = { ...createSession(), id: 'sess_new', workspaceId: 'wd_1', cwd: '/abs/path' };

  beforeEach(() => {
    apiMock.addWorkspace.mockReset();
    apiMock.createSession.mockReset();
    apiMock.updateSession.mockReset();
    apiMock.submitPrompt.mockReset();
    apiMock.addWorkspace.mockResolvedValue(registered);
    apiMock.createSession.mockResolvedValue(newSession);
    apiMock.updateSession.mockResolvedValue({});
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'pr_goal' });
  });

  function emptyComposerState() {
    const state = createState();
    state.activeSessionId = null;
    state.activeWorkspaceId = 'wd_1';
    state.workspaces = [workspace('wd_1', '/abs/path', 'A')];
    state.permission = 'auto'; // skip the interactive goal-start confirmation
    return state;
  }

  function goalDeps(): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      taskPoller: { loadTasksForSession: vi.fn() } as unknown as UseWorkspaceStateDeps['taskPoller'],
      modelProvider: {
        draftModel: ref(null),
        skillsBySession: ref({}),
        loadSkillsForSession: vi.fn(),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      // Something the goal can land in + what's visible in the sidebar.
      mergedWorkspaces: computed(() => [workspace('wd_1', '/abs/path', 'A')]),
      workspacesView: computed(() => [workspace('wd_1', '/abs/path', 'A')]),
    } as unknown as UseWorkspaceStateDeps;
  }

  it('creates a session, sets the goal profile, and submits the objective', async () => {
    const state = emptyComposerState(); // rawState.activeWorkspaceId = 'wd_1'
    const deps = goalDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.createGoal('improve test coverage');

    expect(apiMock.createSession).toHaveBeenCalledOnce();
    // Profile is updated on the new session: that's what marks the prompt as a goal.
    expect(apiMock.updateSession).toHaveBeenCalledWith('sess_new', { goalObjective: 'improve test coverage' });
    // And the objective is sent as the first user prompt on the new session.
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_new',
      expect.objectContaining({
        content: [{ type: 'text', text: 'improve test coverage' }],
      }),
    );
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('falls back to the first visible workspace when raw activeWorkspaceId is unset', async () => {
    // Regression for a real empty-workspace boot: load() never writes
    // rawState.activeWorkspaceId when there are no sessions, so the raw read is
    // null, but the sidebar still shows a usable workspace via the computed
    // fallback. First-session goals must work there too.
    const state = emptyComposerState();
    state.activeWorkspaceId = null;
    const ws = useWorkspaceState(state, goalDeps());

    await ws.createGoal('improve test coverage');

    expect(apiMock.createSession).toHaveBeenCalledOnce();
    expect(apiMock.updateSession).toHaveBeenCalledWith('sess_new', { goalObjective: 'improve test coverage' });
    expect(apiMock.submitPrompt).toHaveBeenCalledOnce();
  });

  it('queues the objective when the active session is running (no queue bypass)', async () => {
    // Regression: creating a goal against an already-active session must honor
    // sendPrompt's queue guard, not bypass straight to submitPromptInternal.
    // Otherwise a /goal message sent while another turn is running races with
    // the active turn instead of being locally queued like normal sends.
    const state = createState();
    state.activeSessionId = 'sess_1';
    state.permission = 'auto'; // skip the interactive goal-start confirmation
    const ws = useWorkspaceState(state, createDeps());

    await ws.createGoal('improve test coverage');

    // Didn't create a session: we targeted the existing one.
    expect(apiMock.createSession).not.toHaveBeenCalled();
    expect(apiMock.updateSession).toHaveBeenCalledWith('sess_1', { goalObjective: 'improve test coverage' });
    // And because the session is running (createDeps' default activity is
    // 'running'), sendPrompt queues rather than posting immediately.
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
    expect(state.queuedBySession['sess_1']).toEqual([
      { text: 'improve test coverage', attachments: undefined },
    ]);
  });

  it('is a no-op when there is no active session and no usable workspace', async () => {
    const state = emptyComposerState();
    state.activeWorkspaceId = null;
    const deps: UseWorkspaceStateDeps = {
      ...createDeps(),
      mergedWorkspaces: computed(() => []),
      workspacesView: computed(() => []),
    };
    const ws = useWorkspaceState(state, deps);

    await ws.createGoal('improve test coverage');

    expect(apiMock.createSession).not.toHaveBeenCalled();
    expect(apiMock.updateSession).not.toHaveBeenCalled();
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('ignores empty/whitespace objectives', async () => {
    const state = emptyComposerState();
    const ws = useWorkspaceState(state, goalDeps());

    await ws.createGoal('   ');

    expect(apiMock.createSession).not.toHaveBeenCalled();
    expect(apiMock.updateSession).not.toHaveBeenCalled();
  });

  it('clears staged goal mode so the objective prompt is submitted once', async () => {
    // Regression for: empty composer with bare `/goal` staged (draftModes.goalMode),
    // then `/goal <objective>`. createDraftSession copies draftModes.goalMode into
    // goalModeBySession[sid]. If we don't clear it after the explicit
    // updateSession(goalObjective), submitPromptInternal re-POSTs a goalObjective,
    // the daemon rejects it (existing goal), and the objective prompt never sends.
    const state = emptyComposerState();
    const deps: UseWorkspaceStateDeps = {
      ...goalDeps(),
      draftModes: { planMode: false, swarmMode: false, collaborationMode: false, goalMode: true },
    };
    const ws = useWorkspaceState(state, deps);

    await ws.createGoal('improve test coverage');

    // The explicit goal objective went through...
    expect(apiMock.updateSession).toHaveBeenCalledWith('sess_new', { goalObjective: 'improve test coverage' });
    // ...and the objective prompt itself was submitted exactly once as a user prompt.
    expect(apiMock.submitPrompt).toHaveBeenCalledTimes(1);
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_new',
      expect.objectContaining({
        content: [{ type: 'text', text: 'improve test coverage' }],
      }),
    );
    // goal mode flag was consumed by the explicit goal.
    expect(state.goalModeBySession['sess_new']).toBe(false);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('surfaces session-creation failures instead of leaking an unhandled rejection', async () => {
    // App.vue invokes createGoal fire-and-forget, so a rejection from
    // createDraftSession must be caught and reported via pushOperationFailure —
    // mirroring the other draft-session paths (skill / BTW / first prompt).
    const state = emptyComposerState();
    const deps = goalDeps();
    const ws = useWorkspaceState(state, deps);
    const err = new Error('snapshot failed');
    apiMock.createSession.mockRejectedValue(err);

    await expect(ws.createGoal('improve test coverage')).resolves.toBeUndefined();

    expect(deps.pushOperationFailure).toHaveBeenCalledWith('createGoal', err);
    expect(apiMock.updateSession).not.toHaveBeenCalled();
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — collaboration mode', () => {
  beforeEach(() => {
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'prompt_new' });
  });

  it('should persist collaboration mode onto the session profile when enabling', async () => {
    const state = createState();
    state.backend = 'v3'; // collaboration has an engine effect only on v3 (D-24)
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.setCollaborationMode(true);

    expect(state.collaborationModeBySession['sess_1']).toBe(true);
    expect(deps.saveCollaborationModeToStorage).toHaveBeenCalled();
    // The rollback notice title rides the third persistSessionProfile arg (B10).
    expect(deps.persistSessionProfile).toHaveBeenCalledWith(
      { collaborationMode: true },
      undefined,
      expect.objectContaining({ title: expect.any(String) }),
    );
  });

  it('should toggle collaboration mode off when mode already active', async () => {
    const state = createState();
    state.backend = 'v3';
    state.collaborationModeBySession = { sess_1: true };
    const deps = createDeps();
    const ws = useWorkspaceState(state, deps);

    await ws.toggleCollaborationMode();

    expect(state.collaborationModeBySession['sess_1']).toBe(false);
    expect(deps.persistSessionProfile).toHaveBeenCalledWith(
      { collaborationMode: false },
      undefined,
      expect.objectContaining({ title: expect.any(String) }),
    );
  });

  it('should stage collaboration mode on the draft when no session is active', async () => {
    const state = createState();
    state.backend = 'v3';
    state.activeSessionId = undefined;
    const draftModes = { planMode: false, swarmMode: false, collaborationMode: false, goalMode: false };
    const deps = createDeps();
    Object.assign(deps, { draftModes });
    const ws = useWorkspaceState(state, deps);

    await ws.toggleCollaborationMode();

    expect(draftModes.collaborationMode).toBe(true);
    expect(deps.persistSessionProfile).not.toHaveBeenCalled();
  });

  it('should carry the collaboration toggle on the submitted prompt', async () => {
    const state = createState();
    state.collaborationModeBySession = { sess_1: true };
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      activity: computed(() => 'idle'),
      modelProvider: {
        models: ref([]),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
    });

    await ws.sendPrompt('collab, please');

    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_1',
      expect.objectContaining({ collaborationMode: true }),
    );
  });
});

describe('useWorkspaceState — steerPrompt optimistic echo', () => {
  beforeEach(() => {
    apiMock.steerSession.mockReset();
  });

  it('should mark the optimistic steer echo with the user_steer origin so the live view folds it like the persisted row', async () => {
    // Given a running turn on the rust engine (v3 — the engine that tags a
    // busy-path steer with user_steer at admission) and a real transcript sink
    // so the optimistic echo lands in state
    apiMock.steerSession.mockResolvedValue({ steered: true });
    const state = createState();
    state.backend = 'v3';
    const deps: UseWorkspaceStateDeps = {
      ...createDeps(),
      updateSessionMessages: (sid, update) => {
        state.messagesBySession[sid] = update(state.messagesBySession[sid] ?? []);
      },
    };
    const ws = useWorkspaceState(state, deps);

    // When the user hits ctrl+s with live composer text
    await ws.steerPrompt('focus on the parser first');

    // Then the locally inserted echo carries BOTH the optimistic marker and
    // the same origin the engine stamps on the durable row (ADR-010) — the
    // origin is what messagesToTurns folds on, so the in-flight transcript
    // and the replayed one project to the same shape
    const echoes: AppMessage[] = state.messagesBySession['sess_1'] ?? [];
    expect(echoes).toHaveLength(1);
    expect(echoes[0]!.role).toBe('user');
    expect(echoes[0]!.metadata).toEqual({
      'mirriWeb.optimisticUserMessage': true,
      origin: { kind: 'user_steer' },
    });
  });

  it('should leave the legacy-engine steer echo untagged so it does not fold against its persisted row', async () => {
    // Given a running turn on a legacy backend (v1/v2): its steer keeps the
    // parked prompt's `origin: user`, so the reopened transcript renders it as
    // its own turn
    apiMock.steerSession.mockResolvedValue({ steered: true });
    const state = createState();
    state.backend = 'v2';
    const deps: UseWorkspaceStateDeps = {
      ...createDeps(),
      updateSessionMessages: (sid, update) => {
        state.messagesBySession[sid] = update(state.messagesBySession[sid] ?? []);
      },
    };
    const ws = useWorkspaceState(state, deps);

    // When the user hits ctrl+s
    await ws.steerPrompt('focus on the parser first');

    // Then tagging it user_steer here would make the LIVE transcript fold a
    // message the replay shows as a turn boundary — the marker stays off
    const echoes: AppMessage[] = state.messagesBySession['sess_1'] ?? [];
    expect(echoes).toHaveLength(1);
    expect(echoes[0]!.metadata).toEqual({ 'mirriWeb.optimisticUserMessage': true });
  });

  it('should keep the plain optimistic marker when an idle send falls back to a normal prompt', async () => {
    // Given an idle session on the rust engine — the backend that DOES tag a
    // busy-path steer, so a mis-tag leaking into the idle branch would
    // actually regress here
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'p_1', userMessageId: 'm_1', status: 'running' });
    const state = createState();
    state.backend = 'v3';
    const deps: UseWorkspaceStateDeps = {
      ...createDeps(),
      activity: computed(() => 'idle'),
      updateSessionMessages: (sid, update) => {
        state.messagesBySession[sid] = update(state.messagesBySession[sid] ?? []);
      },
    };
    const ws = useWorkspaceState(state, deps);

    // When the user hits ctrl+s while nothing is running — steerPrompt has no
    // running turn to fold into, so it degrades to a normal send
    await ws.steerPrompt('a fresh question');

    // Then the steer path is never used and no user_steer echo is created
    expect(apiMock.steerSession).not.toHaveBeenCalled();
    const tags = (state.messagesBySession['sess_1'] ?? []).map(
      (m) => (m.metadata as { origin?: { kind?: string } } | undefined)?.origin?.kind,
    );
    expect(tags).not.toContain('user_steer');
  });
});

describe('useWorkspaceState — startSessionAndOpenSideChat', () => {
  const registered = { id: 'wd_1', root: '/abs/path', name: 'A', isGitRepo: false, sessionCount: 0 };
  const newSession = { ...createSession(), id: 'sess_new', workspaceId: 'wd_1', cwd: '/abs/path' };

  beforeEach(() => {
    apiMock.addWorkspace.mockReset();
    apiMock.createSession.mockReset();
    apiMock.addWorkspace.mockResolvedValue(registered);
    apiMock.createSession.mockResolvedValue(newSession);
  });

  function sideChatDeps(openSideChatOn: ReturnType<typeof vi.fn>): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      taskPoller: { loadTasksForSession: vi.fn() } as unknown as UseWorkspaceStateDeps['taskPoller'],
      sideChat: { openSideChatOn } as unknown as UseWorkspaceStateDeps['sideChat'],
      modelProvider: {
        draftModel: ref(null),
        skillsBySession: ref({}),
        loadSkillsForSession: vi.fn(),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      mergedWorkspaces: computed(() => [workspace('wd_1', '/abs/path', 'A')]),
    };
  }

  it('creates a session, then opens BTW on the new session id with the question', async () => {
    const openSideChatOn = vi.fn().mockResolvedValue(undefined);
    const deps = sideChatDeps(openSideChatOn);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndOpenSideChat('wd_1', 'what changed?');

    expect(apiMock.createSession).toHaveBeenCalledOnce();
    // The BTW sub-agent is opened on the freshly created session, so a
    // concurrent session switch can't redirect it.
    expect(openSideChatOn).toHaveBeenCalledWith('sess_new', 'what changed?');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('works without an initial question (bare /btw)', async () => {
    const openSideChatOn = vi.fn().mockResolvedValue(undefined);
    const deps = sideChatDeps(openSideChatOn);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndOpenSideChat('wd_1');

    expect(openSideChatOn).toHaveBeenCalledWith('sess_new', undefined);
  });

  it('is a no-op for an unknown workspace', async () => {
    const openSideChatOn = vi.fn().mockResolvedValue(undefined);
    const deps = sideChatDeps(openSideChatOn);
    const ws = useWorkspaceState(createState(), deps);

    await ws.startSessionAndOpenSideChat('wd_missing', 'what changed?');

    expect(apiMock.createSession).not.toHaveBeenCalled();
    expect(openSideChatOn).not.toHaveBeenCalled();
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });
});

describe('useWorkspaceState — first-load auth gate', () => {
  beforeEach(() => {
    apiMock.connectTransport.mockReset().mockResolvedValue(undefined);
    apiMock.getAuth.mockReset();
    apiMock.getHealth.mockReset().mockResolvedValue({ ok: true });
    apiMock.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    apiMock.getConfig.mockReset().mockResolvedValue({});
    apiMock.listWorkspaces.mockReset().mockResolvedValue([]);
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '', recentRoots: [] });
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [], hasMore: false });
  });
  function createLoadDeps(
    initialized: Ref<boolean>,
    connectIssue: Ref<string | null>,
  ): UseWorkspaceStateDeps {
    return { ...createDeps(), modelProvider: { loadModels: vi.fn().mockResolvedValue(undefined) }, initialized, connectIssue };
  }
  it('keeps the splash up and retries /auth when the first check fails transiently', async () => {
    vi.useFakeTimers();
    try {
      const initialized = ref(false);
      const connectIssue = ref(null);
      const state = createState();
      state.authReady = false;
      apiMock.getAuth.mockRejectedValueOnce(new Error('connection refused')).mockRejectedValueOnce(new Error('connection refused')).mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
      const ws = useWorkspaceState(state, createLoadDeps(initialized, connectIssue));
      const pending = ws.load();
      await vi.advanceTimersByTimeAsync(0);
      expect(initialized.value).toBe(false);
      expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
      expect(connectIssue.value).toBeNull();
      await vi.advanceTimersByTimeAsync(2000);
      expect(apiMock.getAuth).toHaveBeenCalledTimes(2);
      expect(initialized.value).toBe(false);
      expect(connectIssue.value).toBe('connection refused');
      await vi.advanceTimersByTimeAsync(2000);
      await pending;
      expect(apiMock.getAuth).toHaveBeenCalledTimes(3);
      expect(initialized.value).toBe(true);
      expect(state.authReady).toBe(true);
      expect(connectIssue.value).toBeNull();
    } finally { vi.useRealTimers(); }
  });
  it('initializes normally (into the setup gate) when /auth answers ready:false', async () => {
    const initialized = ref(false);
    const state = createState();
    state.authReady = false;
    apiMock.getAuth.mockResolvedValue({ ready: false, defaultModel: null });
    const ws = useWorkspaceState(state, createLoadDeps(initialized, ref(null)));
    await ws.load();
    expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
    expect(initialized.value).toBe(true);
    expect(state.authReady).toBe(false);
  });
  it.each([40101, 401])('stops without retrying when /auth rejects with %i (server token required)', async (code) => {
    vi.useFakeTimers();
    try {
      const initialized = ref(false);
      const state = createState();
      state.authReady = false;
      apiMock.getAuth.mockRejectedValue(new DaemonApiError({ code, msg: 'Unauthorized', requestId: 'req_1' }));
      const ws = useWorkspaceState(state, createLoadDeps(initialized, ref(null)));
      await ws.load();
      expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
      expect(initialized.value).toBe(false);
      await vi.advanceTimersByTimeAsync(10_000);
      expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
    } finally { vi.useRealTimers(); }
  });
  it('should defer model loading until the auth gate passes when the server requires a token and no credential is stored', async () => {
    // Meta is public and reported an auth gate (dangerousBypassAuth false),
    // so the first-load gate must run BEFORE the model fetch. Otherwise the
    // unauthorized loadModels fires an "operation failed" toast ahead of the
    // ServerAuthDialog.
    apiMock.getAuth.mockRejectedValue(new DaemonApiError({ code: 40101, msg: 'Unauthorized', requestId: 'req_1' }));
    const initialized = ref(false);
    const state = createState();
    state.authReady = false;
    const deps = createLoadDeps(initialized, ref(null));
    const ws = useWorkspaceState(state, deps);
    await ws.load();
    expect(deps.modelProvider.loadModels).not.toHaveBeenCalled();
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
    expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
    expect(initialized.value).toBe(false);
  });
  it('should surface no operation failure when the server requires a token on first load', async () => {
    apiMock.getAuth.mockRejectedValue(new DaemonApiError({ code: 40101, msg: 'Unauthorized', requestId: 'req_1' }));
    const initialized = ref(false);
    const state = createState();
    state.authReady = false;
    const deps = createLoadDeps(initialized, ref(null));
    const ws = useWorkspaceState(state, deps);
    await ws.load();
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });
  it('should load models before the auth gate when the engine bypasses auth', async () => {
    // dangerousBypassAuth true: no gate — the parallel fetches (incl. model
    // loading) proceed as in today's boot order.
    apiMock.getMeta.mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: true, backend: 'v1' });
    const initialized = ref(false);
    const state = createState();
    const deps = createLoadDeps(initialized, ref(null));
    const ws = useWorkspaceState(state, deps);
    await ws.load();
    expect(deps.modelProvider.loadModels).toHaveBeenCalledTimes(1);
    expect(initialized.value).toBe(true);
  });
  it('should fall back to the legacy boot order when /meta is unreachable', async () => {
    // A meta outage must not wedge boot: fall back to today's flow, which runs
    // the parallel fetches (incl. loadModels) before the /auth gate.
    apiMock.getMeta.mockRejectedValue(new Error('connection refused'));
    apiMock.getAuth.mockRejectedValue(new DaemonApiError({ code: 40101, msg: 'Unauthorized', requestId: 'req_1' }));
    const initialized = ref(false);
    const state = createState();
    const deps = createLoadDeps(initialized, ref(null));
    const ws = useWorkspaceState(state, deps);
    await ws.load();
    expect(deps.modelProvider.loadModels).toHaveBeenCalledTimes(1);
  });
});

// Eager transport connect during boot — the rust WsRpcApi's WebSocket must
// be opened by load() (not only by connectEvents on session selection).
// The connect is attempted regardless of a stored credential: a
// bypass-auth engine never issues one, and without the eager connect the
// whole RPC surface stays down (the app is stuck at "No workspace"). An
// auth-required engine without a credential fails the upgrade — non-fatal —
// and the /auth gate still owns prompting.
describe('useWorkspaceState — load eager transport connect (P11c)', () => {
  beforeEach(() => {
    apiMock.connectTransport.mockReset().mockResolvedValue(undefined);
    apiMock.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    apiMock.getHealth.mockReset().mockResolvedValue({ ok: true });
    apiMock.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v3' });
    apiMock.getConfig.mockReset().mockResolvedValue({});
    apiMock.listWorkspaces.mockReset().mockResolvedValue([]);
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '', recentRoots: [] });
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [], hasMore: false });
    installStorage(createMemoryStorage());
    clearCredential();
  });

  afterEach(() => {
    clearCredential();
  });

  it('should call connectTransport before the parallel boot fetches when a credential exists', async () => {
    setCredential('tok-boot');
    const state = createState();
    const callOrder: string[] = [];
    apiMock.connectTransport.mockImplementation(async () => {
      callOrder.push('connectTransport');
    });
    apiMock.getHealth.mockImplementation(async () => {
      callOrder.push('getHealth');
      return { status: 'ok', uptimeSec: 1 };
    });
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      modelProvider: { loadModels: vi.fn().mockResolvedValue(undefined) },
      initialized: ref(true),
      connectIssue: ref(null),
    } as unknown as UseWorkspaceStateDeps);

    await ws.load();

    expect(apiMock.connectTransport).toHaveBeenCalledTimes(1);
    expect(callOrder[0]).toBe('connectTransport');
    expect(callOrder[1]).toBe('getHealth');
  });

  it('should call connectTransport even when no credential exists (a bypass-auth engine never issues one)', async () => {
    // No setCredential — the no-credential boot of a --dangerous-bypass-auth
    // engine. The eager connect must still run, or the RPC surface (workspaces,
    // sessions, models) stays down and the app is stuck at "No workspace".
    const state = createState();
    const callOrder: string[] = [];
    apiMock.connectTransport.mockImplementation(async () => {
      callOrder.push('connectTransport');
    });
    apiMock.getHealth.mockImplementation(async () => {
      callOrder.push('getHealth');
      return { status: 'ok', uptimeSec: 1 };
    });
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      modelProvider: { loadModels: vi.fn().mockResolvedValue(undefined) },
      initialized: ref(true),
      connectIssue: ref(null),
    } as unknown as UseWorkspaceStateDeps);

    await ws.load();

    expect(apiMock.connectTransport).toHaveBeenCalledTimes(1);
    expect(callOrder[0]).toBe('connectTransport');
    expect(callOrder[1]).toBe('getHealth');
  });

  it('should keep booting when connectTransport rejects with no credential (the auth gate owns prompting)', async () => {
    // The auth-required-engine shape: the upgrade fails without a token.
    // Boot continues (non-fatal) and the /auth gate surfaces the dialog.
    apiMock.connectTransport.mockRejectedValue(new Error('WebSocket error during handshake'));
    apiMock.getAuth.mockResolvedValue({ ready: false, defaultModel: null });
    const state = createState();
    state.authReady = false;
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      modelProvider: { loadModels: vi.fn().mockResolvedValue(undefined) },
      initialized: ref(true),
      connectIssue: ref(null),
    } as unknown as UseWorkspaceStateDeps);

    await ws.load();

    expect(apiMock.connectTransport).toHaveBeenCalledTimes(1);
    // The boot fetches still ran and the auth gate decided — the failure is
    // non-fatal and no stale-credential clearing fired (none existed).
    expect(apiMock.getHealth).toHaveBeenCalledTimes(1);
    expect(apiMock.getAuth).toHaveBeenCalledTimes(1);
    expect(state.authReady).toBe(false);
  });

  it('should keep booting when connectTransport rejects (the error surfaces elsewhere)', async () => {
    setCredential('tok-stale');
    apiMock.connectTransport.mockRejectedValue(new Error('WebSocket error during handshake'));
    const state = createState();
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      modelProvider: { loadModels: vi.fn().mockResolvedValue(undefined) },
      initialized: ref(true),
      connectIssue: ref(null),
    } as unknown as UseWorkspaceStateDeps);

    await ws.load();

    expect(apiMock.connectTransport).toHaveBeenCalledTimes(1);
    // The boot fetches still ran — the failure is non-fatal.
    expect(apiMock.getHealth).toHaveBeenCalledTimes(1);
    expect(state.workspaces).toEqual([]);
  });
});

// /meta re-read on every WS (re)connect — keeps version / backend truthful
// across backend restarts and dev-proxy backend switches.
describe('useWorkspaceState — refreshServerMeta', () => {
  beforeEach(() => {
    apiMock.getMeta.mockReset();
  });

  it('applies the meta payload including the v2 backend marker', async () => {
    apiMock.getMeta.mockResolvedValue({
      serverVersion: '9.9.9',
      openInApps: ['finder'],
      dangerousBypassAuth: true,
      backend: 'v2',
    });
    const state = createState();
    const ws = useWorkspaceState(state, createDeps());

    await ws.refreshServerMeta();

    expect(state.serverVersion).toBe('9.9.9');
    expect(state.availableOpenInApps).toEqual(['finder']);
    expect(state.dangerousBypassAuth).toBe(true);
    expect(state.backend).toBe('v2');
  });

  it('keeps the previous meta when /meta fails', async () => {
    apiMock.getMeta.mockRejectedValue(new Error('connection refused'));
    const state = createState();
    state.backend = 'v2';
    const ws = useWorkspaceState(state, createDeps());

    await ws.refreshServerMeta();

    expect(state.backend).toBe('v2');
    expect(state.serverVersion).toBe('');
  });
});

// Regression coverage for wake/reconnect snapshot recovery.
describe('useWorkspaceState — snapshot prompt recovery', () => {
  function promptDeps(overrides: Partial<UseWorkspaceStateDeps> = {}): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      modelProvider: {
        models: ref([]),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      ...overrides,
    };
  }

  beforeEach(() => {
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'prompt_new' });
  });

  it('clears a finished prompt from a terminal snapshot so the next send is immediate', async () => {
    const state = createState();
    state.inFlightBySession = { sess_1: true };
    const ws = useWorkspaceState(
      state,
      promptDeps({ activity: computed(() => 'idle') }),
    );

    ws.handleSessionSnapshot('sess_1', { inFlightTurn: null, busy: false });

    expect(state.inFlightBySession.sess_1).toBe(false);
    expect(state.promptIdBySession.sess_1).toBeUndefined();

    await ws.sendPrompt('next');
    expect(apiMock.submitPrompt).toHaveBeenCalledOnce();
    expect(state.queuedBySession.sess_1).toBeUndefined();
  });

  it('keeps a genuinely running prompt in flight and queues the next send', async () => {
    const state = createState();
    state.inFlightBySession = { sess_1: true };
    const ws = useWorkspaceState(state, promptDeps());

    ws.handleSessionSnapshot('sess_1', {
      inFlightTurn: { turnId: 1, assistantText: '', thinkingText: '', runningTools: [] },
      busy: true,
    });
    await ws.sendPrompt('next');

    expect(state.inFlightBySession.sess_1).toBe(true);
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
    expect(state.queuedBySession.sess_1).toEqual([{ text: 'next', attachments: undefined }]);
  });

  it('drains one queued prompt when the finished turn was locally witnessed', async () => {
    const state = createState();
    state.inFlightBySession = { sess_1: true };
    state.promptIdBySession = { sess_1: 'prompt_old' };
    state.queuedBySession = {
      sess_1: [
        { text: 'first queued', attachments: undefined },
        { text: 'second queued', attachments: undefined },
      ],
    };
    const ws = useWorkspaceState(state, promptDeps());

    ws.handleSessionSnapshot('sess_1', { inFlightTurn: null, busy: true });

    await vi.waitFor(() => expect(apiMock.submitPrompt).toHaveBeenCalledOnce());
    expect(state.queuedBySession.sess_1).toEqual([
      { text: 'second queued', attachments: undefined },
    ]);
  });

  it('clears local prompt state when busy disproves a stale snapshot turn', () => {
    const state = createState();
    state.inFlightBySession = { sess_1: true };
    state.promptIdBySession = { sess_1: 'prompt_old' };
    const ws = useWorkspaceState(state, promptDeps());

    ws.handleSessionSnapshot('sess_1', {
      inFlightTurn: { turnId: 1, assistantText: '', thinkingText: '', runningTools: [] },
      busy: false,
    });

    expect(state.inFlightBySession.sess_1).toBe(false);
    expect(state.promptIdBySession.sess_1).toBeUndefined();
  });

  it('rejects a snapshot when a new local prompt started during the request', async () => {
    const state = createState();
    const ws = useWorkspaceState(state, promptDeps());
    const atRequest = ws.localTurnStartState('sess_1');

    await ws.submitPromptInternal('sess_1', 'fresh prompt');

    expect(ws.isLocalTurnSnapshotCurrent('sess_1', atRequest)).toBe(false);
    expect(state.inFlightBySession.sess_1).toBe(true);
  });

  it('rejects a snapshot requested while the local submit is still pending', async () => {
    let resolveSubmit!: (value: { promptId: string }) => void;
    apiMock.submitPrompt.mockImplementation(
      () =>
        new Promise<{ promptId: string }>((resolve) => {
          resolveSubmit = resolve;
        }),
    );
    const ws = useWorkspaceState(createState(), promptDeps());
    const pendingSubmit = ws.submitPromptInternal('sess_1', 'fresh prompt');
    const atRequest = ws.localTurnStartState('sess_1');
    const retrySnapshot = vi.fn();

    expect(atRequest.pending).toBe(true);
    expect(ws.isLocalTurnSnapshotCurrent('sess_1', atRequest)).toBe(false);
    ws.afterLocalTurnStartsSettle('sess_1', retrySnapshot);
    expect(retrySnapshot).not.toHaveBeenCalled();

    await vi.waitFor(() => expect(apiMock.submitPrompt).toHaveBeenCalled());
    resolveSubmit({ promptId: 'prompt_new' });
    await pendingSubmit;
    expect(ws.localTurnStartState('sess_1').pending).toBe(false);
    expect(retrySnapshot).toHaveBeenCalledOnce();
  });

  // A background session's drained prompt must not inherit the thinking level
  // of whichever session is active when the drain happens — the level is
  // resolved from the prompt's OWN model, never the active-view global.
  it('drains a queued prompt with the level of its own session model, not the active view', async () => {
    const state = createState();
    state.sessions = [{ ...createSession(), id: 'sess_a', model: 'provider/model-a' }];
    state.activeSessionId = 'sess_b'; // the user has switched to another session
    state.thinking = 'max'; // the global now tracks that session's max-only model
    state.inFlightBySession = { sess_a: true };
    state.queuedBySession = { sess_a: [{ text: 'follow up', attachments: undefined }] };
    const resolveThinkingForPrompt = vi.fn(async (_sid: string | null, id: string | undefined) =>
      id === 'provider/model-a' ? 'low' : undefined,
    );
    const ws = useWorkspaceState(
      state,
      promptDeps({
        modelProvider: {
          models: ref([]),
          resolveThinkingForPrompt,
        } as unknown as UseWorkspaceStateDeps['modelProvider'],
      }),
    );

    ws.handleSessionSnapshot('sess_a', { inFlightTurn: null, busy: true });

    await vi.waitFor(() => expect(apiMock.submitPrompt).toHaveBeenCalled());
    expect(resolveThinkingForPrompt).toHaveBeenCalledWith('sess_a', 'provider/model-a');
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_a',
      expect.objectContaining({ model: 'provider/model-a', thinking: 'low' }),
    );
  });

  it('falls back to the active level for a drained prompt whose model left the catalog', async () => {
    const state = createState();
    state.sessions = [{ ...createSession(), id: 'sess_a', model: 'provider/gone-model' }];
    state.thinking = 'max';
    state.inFlightBySession = { sess_a: true };
    state.queuedBySession = { sess_a: [{ text: 'follow up', attachments: undefined }] };
    const ws = useWorkspaceState(
      state,
      promptDeps({
        modelProvider: {
          models: ref([]),
          resolveThinkingForPrompt: async () => undefined,
        } as unknown as UseWorkspaceStateDeps['modelProvider'],
      }),
    );

    ws.handleSessionSnapshot('sess_a', { inFlightTurn: null, busy: true });

    await vi.waitFor(() => expect(apiMock.submitPrompt).toHaveBeenCalled());
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_a',
      expect.objectContaining({ model: 'provider/gone-model', thinking: 'max' }),
    );
  });
});

// Regression: a search-triggered full session-list reload must not clobber the
// live usage (context ring) with the list endpoint's all-zero placeholder.
describe('useWorkspaceState — loadAllSessions usage preservation', () => {
  beforeEach(() => {
    apiMock.listSessions.mockReset();
  });

  function liveUsage() {
    return {
      inputTokens: 100,
      outputTokens: 50,
      cacheReadTokens: 0,
      cacheCreationTokens: 0,
      totalCostUsd: 0,
      contextTokens: 28772,
      contextLimit: 1048576,
      turnCount: 3,
    };
  }

  it('keeps the cached live usage when the reloaded row carries the placeholder', async () => {
    const state = createState();
    state.sessions = [{ ...createSession(), usage: liveUsage() }];
    apiMock.listSessions.mockResolvedValue({
      items: [{ ...createSession(), title: 'Fresh from server' }],
      hasMore: false,
    });
    const setSessions = vi.fn();
    const ws = useWorkspaceState(state, { ...createDeps(), setSessions });

    await ws.loadAllSessions();

    expect(setSessions).toHaveBeenCalledOnce();
    const next = setSessions.mock.calls[0][0];
    expect(next[0].title).toBe('Fresh from server');
    expect(next[0].usage).toEqual(liveUsage());
  });

  it('takes the server row as-is when there is no live usage to preserve', async () => {
    const state = createState();
    apiMock.listSessions.mockResolvedValue({ items: [createSession()], hasMore: false });
    const setSessions = vi.fn();
    const ws = useWorkspaceState(state, { ...createDeps(), setSessions });

    await ws.loadAllSessions();

    const next = setSessions.mock.calls[0][0];
    expect(next[0].usage.contextTokens).toBe(0);
  });
});

describe('useWorkspaceState — pickInitialWorkspaceIdForFirstLoad', () => {
  it('returns the persisted active workspace id when it is still present in the list', () => {
    const id = pickInitialWorkspaceIdForFirstLoad('wd_saved', [
      { id: 'wd_first', name: 'a' },
      { id: 'wd_saved', name: 'b' },
    ]);
    expect(id).toBe('wd_saved');
  });

  it('falls back to the first (most-recent) workspace when the persisted id is gone', () => {
    const id = pickInitialWorkspaceIdForFirstLoad('wd_removed', [
      { id: 'wd_first', name: 'a' },
      { id: 'wd_second', name: 'b' },
    ]);
    expect(id).toBe('wd_first');
  });

  it('returns the first workspace when nothing was persisted', () => {
    const id = pickInitialWorkspaceIdForFirstLoad(null, [
      { id: 'wd_head', name: 'a' },
      { id: 'wd_tail', name: 'b' },
    ]);
    expect(id).toBe('wd_head');
  });

  it('returns null when the workspace list is empty', () => {
    expect(pickInitialWorkspaceIdForFirstLoad('wd_saved', [])).toBeNull();
    expect(pickInitialWorkspaceIdForFirstLoad(null, [])).toBeNull();
  });
});

describe('useWorkspaceState — first-load enters the empty new-task draft without blocking on the session list', () => {
  function draftDeps() {
    const deps = createDeps();
    return {
      ...deps,
      initialized: ref(false),
      workspacesView: computed(() => [
        { id: 'wd_first', name: 'first' },
        { id: 'wd_saved', name: 'saved' },
      ]),
      modelProvider: {
        resolveThinkingForPrompt: async () => undefined,
        loadModels: vi.fn().mockResolvedValue(undefined),
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      setActiveSessionId: vi.fn(),
      saveActiveWorkspaceToStorage: vi.fn(),
    };
  }

  it('opens the draft for the persisted workspace and returns before the session list resolves', async () => {
    const api = apiMock;
    api.getHealth.mockReset().mockResolvedValue({ ok: true });
    api.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    api.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    api.getConfig.mockReset().mockResolvedValue({});
    api.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    api.listWorkspaces.mockReset().mockResolvedValue([
      { id: 'wd_first', root: '/w/first', name: 'first', sessionCount: 0 },
      { id: 'wd_saved', root: '/w/saved', name: 'saved', sessionCount: 0 },
    ]);
    // Never-resolving list: proves first paint is not gated on the session fetch.
    api.listSessions.mockReset().mockReturnValue(new Promise(() => {}));

    const deps = draftDeps();
    const setActiveSessionId = deps.setActiveSessionId as ReturnType<typeof vi.fn>;
    const saveActiveWorkspace = deps.saveActiveWorkspaceToStorage as ReturnType<typeof vi.fn>;
    const state = { ...createState(), activeWorkspaceId: 'wd_saved' };
    const ws = useWorkspaceState(state, deps);

    await ws.load();

    // The draft opens against the persisted workspace...
    expect(saveActiveWorkspace).toHaveBeenCalledWith('wd_saved');
    // ...with no active session selected (centred composer ready)...
    expect(setActiveSessionId).toHaveBeenCalledWith(undefined);
    // ...and load() returned even though the session list never resolved.
    api.listSessions.mock.calls.length; // eslint-disable-line no-unused-expressions
  });

  it('falls back to the first workspace and clears the session when nothing was persisted', async () => {
    const api = apiMock;
    api.getHealth.mockReset().mockResolvedValue({ ok: true });
    api.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    api.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    api.getConfig.mockReset().mockResolvedValue({});
    api.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    api.listWorkspaces.mockReset().mockResolvedValue([
      { id: 'wd_first', root: '/w/first', name: 'first', sessionCount: 0 },
    ]);
    api.listSessions.mockReset().mockResolvedValue({ items: [createSession()], hasMore: false });

    const deps = draftDeps();
    const saveActiveWorkspace = deps.saveActiveWorkspaceToStorage as ReturnType<typeof vi.fn>;
    const state = { ...createState(), activeWorkspaceId: null };
    const ws = useWorkspaceState(state, deps);

    await ws.load();

    // No persisted id → fall back to workspacesView[0], into the draft.
    expect(saveActiveWorkspace).toHaveBeenCalledWith('wd_first');
  });

  it('does not reset the active workspace or session on a non-first load', async () => {
    const api = apiMock;
    api.getHealth.mockReset().mockResolvedValue({ ok: true });
    api.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    api.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    api.getConfig.mockReset().mockResolvedValue({});
    api.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    api.listWorkspaces.mockReset().mockResolvedValue([
      { id: 'wd_first', root: '/w/first', name: 'first', sessionCount: 1 },
    ]);
    api.listSessions.mockReset().mockResolvedValue({ items: [createSession()], hasMore: false });

    const deps = { ...createDeps(), initialized: ref(true) };
    const setActiveSessionId = deps.setActiveSessionId as ReturnType<typeof vi.fn>;
    const saveActiveWorkspace = deps.saveActiveWorkspaceToStorage as ReturnType<typeof vi.fn>;
    // Already loaded: the user is mid-conversation in sess_1.
    const state = {
      ...createState(),
      activeWorkspaceId: 'wd_first',
      activeSessionId: 'sess_1',
    };
    const ws = useWorkspaceState(state, deps);

    await ws.load();

    // Non-first load must NOT reset the active workspace / session (no "new
    // task" draft, no history auto-reselect, no closing of the active view).
    expect(saveActiveWorkspace).not.toHaveBeenCalled();
    expect(setActiveSessionId).not.toHaveBeenCalled();
  });
});

// Lazy first-fill: the newest /workspaces entry is fetched first, then the rest
// drain through SESSIONS_INITIAL_LOAD_WORKERS workers, so at most 3 session
// fetches are ever in flight together.
describe('useWorkspaceState — lazy initial fill (priority-first + 3-worker queue)', () => {
  function loadDeps(): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      modelProvider: {
        loadModels: vi.fn().mockResolvedValue(undefined),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
    };
  }

  beforeEach(() => {
    apiMock.getHealth.mockReset().mockResolvedValue({ ok: true });
    apiMock.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    apiMock.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    apiMock.getConfig.mockReset().mockResolvedValue({});
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    apiMock.listWorkspaces.mockReset().mockResolvedValue([
      workspace('wd_recent', '/w/recent', 'recent'),
      workspace('wd_mid', '/w/mid', 'mid'),
      workspace('wd_old', '/w/old', 'old'),
    ]);
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [], hasMore: false });
  });

  it('given 3 workspaces and a traceable listSessions mock, when load() starts the session fill, then the newest workspace is requested first and in-flight concurrency never exceeds 3', async () => {
    const callOrder: string[] = [];
    let inFlight = 0;
    let maxInFlight = 0;
    const resolvers = new Map<string, () => void>();
    apiMock.listSessions.mockImplementation((input?: { workspaceId?: string }) => {
      const wid = input?.workspaceId ?? 'global';
      callOrder.push(wid);
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      return new Promise<{ items: AppSession[]; hasMore: boolean }>((resolve) => {
        resolvers.set(wid, () => {
          inFlight -= 1;
          resolve({ items: [], hasMore: false });
        });
      });
    });

    const ws = useWorkspaceState(createState(), loadDeps());
    await ws.load();

    // The most-recently-opened workspace (first /workspaces entry) is the very
    // first session request — before any of the others.
    expect(callOrder[0]).toBe('wd_recent');
    expect(callOrder).toHaveLength(1);
    expect(maxInFlight).toBeLessThanOrEqual(3);

    // Let the priority workspace settle: only THEN do the remaining workspaces
    // start (bounded by 3 workers), with at most 3 requests in flight at once.
    resolvers.get('wd_recent')!();
    await vi.waitFor(() => expect(callOrder).toHaveLength(3));
    expect(callOrder.slice(1).toSorted()).toEqual(['wd_mid', 'wd_old']);
    expect(maxInFlight).toBeLessThanOrEqual(3);

    for (const wid of ['wd_mid', 'wd_old']) resolvers.get(wid)?.();
    await vi.waitFor(() => expect(callOrder).toHaveLength(3));
    expect(maxInFlight).toBeLessThanOrEqual(3);
    expect(apiMock.listSessions).toHaveBeenCalledTimes(3);
  });
});

// initialOnly (used by the lazy first fill AND the manual refresh) fetches
// exactly the newest page and never continues into older pages — even when the
// first page still reports hasMore=true with a session inside the recent window.
describe('useWorkspaceState — initialOnly first page does not continue into the recent window', () => {
  function loadDeps(): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      modelProvider: {
        loadModels: vi.fn().mockResolvedValue(undefined),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
    };
  }

  beforeEach(() => {
    apiMock.getHealth.mockReset().mockResolvedValue({ ok: true });
    apiMock.getMeta.mockReset().mockResolvedValue({ serverVersion: '0.0.0', openInApps: [], dangerousBypassAuth: false, backend: 'v1' });
    apiMock.getAuth.mockReset().mockResolvedValue({ ready: true, defaultModel: 'mirri-code' });
    apiMock.getConfig.mockReset().mockResolvedValue({});
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    apiMock.listWorkspaces.mockReset().mockResolvedValue([
      workspace('wd_recent', '/w/recent', 'recent'),
    ]);
  });

  it('given a workspace whose first page reports hasMore with a session still inside the 12h window, when the initial load runs, then exactly one listSessions page is fetched', async () => {
    const recent = { ...createSession(), id: 'sess_recent', updatedAt: new Date().toISOString() };
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [recent], hasMore: true });

    const ws = useWorkspaceState(createState(), loadDeps());
    await ws.load();
    // Let any (buggy) continuation cascade settle — the initialOnly path must
    // never fire a second page.
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(apiMock.listSessions).toHaveBeenCalledTimes(1);
    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'wd_recent', pageSize: 3 }),
    );
  });
});

// Manual refresh (sidebar header button): POST refresh → reload workspaces →
// re-pull the newest page → replace the workspace's sessions, idempotent while
// a refresh for the same workspace is still in flight.
describe('useWorkspaceState — refreshWorkspaceSessions', () => {
  beforeEach(() => {
    apiMock.refreshWorkspace.mockReset();
    apiMock.listWorkspaces.mockReset();
    apiMock.getFsHome.mockReset().mockResolvedValue({ home: '/home', recentRoots: [] });
    apiMock.listSessions.mockReset().mockResolvedValue({ items: [], hasMore: false });
  });

  function stateWithSessions(): ExtendedState {
    const state = createState();
    state.workspaces = [workspace('w-a', '/w/a', 'A'), workspace('w-b', '/w/b', 'B')];
    state.sessions = [
      { ...createSession(), id: 'sess_a_old', workspaceId: 'w-a', cwd: '/w/a' },
      { ...createSession(), id: 'sess_b_keep', workspaceId: 'w-b', cwd: '/w/b' },
      { ...createSession(), id: 'sess_orphan', cwd: '/elsewhere' },
    ];
    return state;
  }

  function refreshDeps(state: ExtendedState): UseWorkspaceStateDeps {
    return {
      ...createDeps(),
      workspaceIdForSession: (s: { workspaceId?: string; cwd: string }) => s.workspaceId ?? s.cwd,
      setSessions: vi.fn((next: AppSession[]) => {
        state.sessions = next;
      }),
    } as unknown as UseWorkspaceStateDeps;
  }

  it('given the user triggers a refresh for one workspace, then refreshWorkspace/listWorkspaces/listSessions are called, the workspace’s sessions are replaced while other workspaces keep theirs, and a second click during the refresh is idempotent', async () => {
    let resolveRefresh!: (v: { refreshed: true; inserted: number; removed: number }) => void;
    apiMock.refreshWorkspace.mockReturnValue(
      new Promise<{ refreshed: true; inserted: number; removed: number }>((resolve) => {
        resolveRefresh = resolve;
      }),
    );
    apiMock.listWorkspaces.mockResolvedValue([workspace('w-a', '/w/a', 'A'), workspace('w-b', '/w/b', 'B')]);
    apiMock.listSessions.mockResolvedValue({
      items: [{ ...createSession(), id: 'sess_a_new', workspaceId: 'w-a', cwd: '/w/a', updatedAt: new Date().toISOString() }],
      hasMore: false,
    });

    const state = stateWithSessions();
    const deps = refreshDeps(state);
    const ws = useWorkspaceState(state, deps);

    const first = ws.refreshWorkspaceSessions('w-a');
    // Second click while the first refresh is still in flight: ignored.
    await ws.refreshWorkspaceSessions('w-a');
    expect(apiMock.refreshWorkspace).toHaveBeenCalledTimes(1);
    expect(ws.refreshingWorkspaceIds.value.has('w-a')).toBe(true);

    resolveRefresh({ refreshed: true, inserted: 1, removed: 1 });
    await first;

    expect(apiMock.refreshWorkspace).toHaveBeenCalledWith('w-a');
    expect(apiMock.listWorkspaces).toHaveBeenCalled();
    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'w-a', pageSize: 3 }),
    );
    const ids = state.sessions.map((s) => s.id);
    expect(ids).toContain('sess_a_new');
    expect(ids).not.toContain('sess_a_old');
    expect(ids).toContain('sess_b_keep');
    expect(ids).toContain('sess_orphan');
    expect(ws.refreshingWorkspaceIds.value.has('w-a')).toBe(false);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('given a v1 daemon without the refresh route, when the user refreshes, then the session list is still re-pulled (pure re-pull fallback) and no error is surfaced', async () => {
    apiMock.refreshWorkspace.mockRejectedValue(
      new DaemonApiError({ code: 40410, msg: 'route not found', requestId: 'r' }),
    );
    apiMock.listWorkspaces.mockResolvedValue([workspace('w-a', '/w/a', 'A'), workspace('w-b', '/w/b', 'B')]);
    apiMock.listSessions.mockResolvedValue({
      items: [{ ...createSession(), id: 'sess_a_new', workspaceId: 'w-a', cwd: '/w/a' }],
      hasMore: false,
    });

    const state = stateWithSessions();
    const deps = refreshDeps(state);
    const ws = useWorkspaceState(state, deps);

    await ws.refreshWorkspaceSessions('w-a');

    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'w-a' }),
    );
    expect(state.sessions.map((s) => s.id)).toContain('sess_a_new');
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
    expect(ws.refreshingWorkspaceIds.value.size).toBe(0);
  });
});

// "load more" pagination: the per-workspace cursor is the id of the workspace's
// oldest loaded session. Archiving/deleting that session leaves the cursor
// pointing at a session the server no longer lists — v2 answers an unknown
// `before_id` with an empty terminal page (has_more: false), which would
// silently end pagination while sessions remain (the show-more button vanishes
// and the older sessions never load). loadMoreSessions must re-anchor the
// cursor before requesting.
describe('useWorkspaceState — loadMoreSessions recovers from a stale cursor (archived cursor session)', () => {
  /** A session in workspace wd_1, `updatedAt` ordered for the sidebar. */
  function wdSession(id: string, updatedAt: string): AppSession {
    return { ...createSession(), id, workspaceId: 'wd_1', cwd: '/w/a', updatedAt };
  }

  /** State where wd_1 has `cursor` still set to an archived session id that is
   *  no longer in the loaded list — exactly what archiveSession leaves behind. */
  function staleCursorState(loaded: AppSession[]) {
    const state = createState();
    state.sessions = loaded;
    state.sessionsHasMoreByWorkspace = { wd_1: true };
    state.sessionsLoadingMoreByWorkspace = {};
    state.sessionsCursorByWorkspace = { wd_1: 'sess_archived' };
    state.sessionsInitialCountByWorkspace = {};
    const setSessions = vi.fn((next: AppSession[]) => {
      state.sessions = next;
    });
    const deps = {
      ...createDeps(),
      setSessions,
      workspaceIdForSession: (s: { workspaceId?: string; cwd: string }) =>
        s.workspaceId ?? s.cwd,
    } as unknown as UseWorkspaceStateDeps;
    const ws = useWorkspaceState(state, deps);
    return { state, ws };
  }

  beforeEach(() => {
    apiMock.listSessions.mockReset();
  });

  it('given the load-more cursor points at an archived session, when loadMoreSessions runs, then it re-anchors the cursor at the oldest still-loaded session and appends the next page', async () => {
    const { state, ws } = staleCursorState([
      wdSession('sess_newest', '2026-01-03T00:00:00.000Z'),
      wdSession('sess_mid', '2026-01-02T00:00:00.000Z'),
    ]);
    apiMock.listSessions.mockResolvedValue({
      items: [
        wdSession('sess_older_a', '2026-01-01T00:00:00.000Z'),
        wdSession('sess_older_b', '2025-12-31T00:00:00.000Z'),
      ],
      hasMore: true,
    });

    await ws.loadMoreSessions('wd_1');

    // The request must not carry the stale (archived) cursor — it is re-anchored
    // at the oldest still-loaded session so older sessions are actually returned.
    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'wd_1', beforeId: 'sess_mid' }),
    );
    expect(state.sessions.map((s) => s.id)).toEqual([
      'sess_newest',
      'sess_mid',
      'sess_older_a',
      'sess_older_b',
    ]);
    // Cursor advanced to the end of the fetched page; hasMore reflects the page.
    expect(state.sessionsCursorByWorkspace.wd_1).toBe('sess_older_b');
    expect(state.sessionsHasMoreByWorkspace.wd_1).toBe(true);
  });

  it('given a workspace whose sessions are all archived away, when loadMoreSessions runs, then it re-fetches the newest page without any cursor', async () => {
    const { state, ws } = staleCursorState([]);
    apiMock.listSessions.mockResolvedValue({
      items: [wdSession('sess_first', '2026-01-04T00:00:00.000Z')],
      hasMore: true,
    });

    await ws.loadMoreSessions('wd_1');

    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'wd_1', beforeId: undefined }),
    );
    expect(state.sessions.map((s) => s.id)).toContain('sess_first');
    expect(state.sessionsCursorByWorkspace.wd_1).toBe('sess_first');
  });

  it('given a cursor that still exists in the loaded sessions, when loadMoreSessions runs, then the request keeps that cursor unchanged', async () => {
    const { state, ws } = staleCursorState([
      wdSession('sess_newest', '2026-01-03T00:00:00.000Z'),
      wdSession('sess_mid', '2026-01-02T00:00:00.000Z'),
    ]);
    state.sessionsCursorByWorkspace = { wd_1: 'sess_mid' };
    apiMock.listSessions.mockResolvedValue({ items: [], hasMore: false });

    await ws.loadMoreSessions('wd_1');

    expect(apiMock.listSessions).toHaveBeenCalledWith(
      expect.objectContaining({ workspaceId: 'wd_1', beforeId: 'sess_mid' }),
    );
  });
});

describe('useWorkspaceState — syncParkedPromptsFromServer', () => {
  beforeEach(() => {
    apiMock.listPrompts.mockReset();
  });

  it('should restore queued daemon prompts into the parked queue area for a busy session', async () => {
    const state = createState();
    apiMock.listPrompts.mockResolvedValue({
      active: null,
      queued: [
        {
          promptId: 'prompt_parked',
          createdAt: '2026-01-01T00:00:00.000Z',
          status: 'queued',
          created_at: '2026-01-01T00:00:00.000Z',
          content: [{ type: 'text', text: '/review --fix' }],
        },
      ],
    });
    const ws = useWorkspaceState(state, createDeps());

    await ws.syncParkedPromptsFromServer('sess_1');

    expect(state.parkedPromptsBySession['sess_1']).toEqual([
      { prompt_id: 'prompt_parked', text: '/review --fix' },
    ]);
  });

  it('skips running prompts and keeps the previous display when the fetch fails', async () => {
    const state = createState();
    state.parkedPromptsBySession = { sess_1: [{ prompt_id: 'keep', text: '/keep' }] };
    apiMock.listPrompts.mockResolvedValue({
      active: {
        promptId: 'prompt_running',
        user_message_id: 'msg_run',
        status: 'running',
        created_at: '2026-01-01T00:00:00.000Z',
        content: [{ type: 'text', text: 'running one' }],
      },
      queued: [
        {
          promptId: 'prompt_running_q',
          user_message_id: 'msg_q',
          status: 'running',
          created_at: '2026-01-01T00:00:00.000Z',
          content: [{ type: 'text', text: 'not queued' }],
        },
      ],
    });
    const ws = useWorkspaceState(state, createDeps());

    await ws.syncParkedPromptsFromServer('sess_1');

    // running items are not parked
    expect(state.parkedPromptsBySession['sess_1']).toEqual([]);

    apiMock.listPrompts.mockRejectedValue(new Error('daemon down'));
    await expect(ws.syncParkedPromptsFromServer('sess_1')).resolves.toBeUndefined();
    expect(state.parkedPromptsBySession['sess_1']).toEqual([]);
  });
});

// W8 — the trust management flow (SDR-007): selecting an untrusted workspace
// fires the NON-BLOCKING trust confirmation; 暂不 keeps the workspace
// usable with its untrusted state honestly visible; 信任 performs the
// setWorkspaceTrust write and flips the local row.
describe('useWorkspaceState — workspace trust (W8)', () => {
  beforeEach(() => {
    apiMock.setWorkspaceTrust.mockReset();
    apiMock.setWorkspaceTrust.mockResolvedValue({ trusted: true });
    installStorage(createMemoryStorage());
    // Reset the module-singleton prompt state between tests.
    useWorkspaceTrustPrompt().settle(false);
  });

  afterEach(() => {
    installStorage(createMemoryStorage());
    useWorkspaceTrustPrompt().settle(false);
  });

  it('fires the trust confirmation when an untrusted workspace is opened', () => {
    // Given an untrusted workspace in the merged view
    const state = createState();
    state.workspaces = [workspace('wd_new', '/abs/new', 'New', false)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    // When the workspace is selected (the sidebar switch / composer pick)
    ws.openWorkspace('wd_new');

    // Then the workspace IS selected (选中即用 — the open is not blocked)
    // and the trust confirmation appeared
    expect(state.activeWorkspaceId).toBe('wd_new');
    expect(prompt.current.value?.target.id).toBe('wd_new');
  });

  it('keeps the workspace usable and untrusted when the user picks 暂不', () => {
    const state = createState();
    state.workspaces = [workspace('wd_new', '/abs/new', 'New', false)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    // When the workspace is selected and the user answers 暂不
    ws.openWorkspace('wd_new');
    prompt.settle(false);

    // Then no trust write happened, the workspace stays usable, and its
    // untrusted state stays visible (the badge's source row)
    expect(apiMock.setWorkspaceTrust).not.toHaveBeenCalled();
    expect(state.activeWorkspaceId).toBe('wd_new');
    expect(state.workspaces[0]?.trusted).toBe(false);
    expect(prompt.current.value).toBeNull();
  });

  it('performs the setWorkspaceTrust write and flips the row when the user trusts', async () => {
    const state = createState();
    state.workspaces = [workspace('wd_new', '/abs/new', 'New', false)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    // When the workspace is selected and the user answers 信任此工作区
    ws.openWorkspace('wd_new');
    prompt.settle(true);
    // The fire-and-forget write needs a microtask to land
    await Promise.resolve();
    await vi.waitFor(() => {
      expect(apiMock.setWorkspaceTrust).toHaveBeenCalledWith('wd_new', true);
    });

    // Then the local row flipped (the badge clears) and no failure surfaced
    expect(state.workspaces[0]?.trusted).toBe(true);
    expect(deps.pushOperationFailure).not.toHaveBeenCalled();
  });

  it('does not fire the confirmation for a trusted workspace selection', () => {
    const state = createState();
    state.workspaces = [workspace('wd_known', '/abs/known', 'Known', true)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    ws.openWorkspace('wd_known');

    expect(prompt.current.value).toBeNull();
    expect(apiMock.setWorkspaceTrust).not.toHaveBeenCalled();
  });

  it('fires the confirmation when a draft is opened in an untrusted workspace', () => {
    const state = createState();
    state.workspaces = [workspace('wd_new', '/abs/new', 'New', false)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    // When the workspace-level "+ New" enters the draft state
    ws.openWorkspaceDraft('wd_new');

    expect(state.activeWorkspaceId).toBe('wd_new');
    expect(prompt.current.value?.target.id).toBe('wd_new');
  });

  it('surfaces a failed trust write and keeps the row untrusted', async () => {
    apiMock.setWorkspaceTrust.mockRejectedValue(new Error('no Mirri home'));
    const state = createState();
    state.workspaces = [workspace('wd_new', '/abs/new', 'New', false)];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    ws.openWorkspace('wd_new');
    prompt.settle(true);
    await vi.waitFor(() => {
      expect(deps.pushOperationFailure).toHaveBeenCalled();
    });
    expect(state.workspaces[0]?.trusted).toBe(false);
  });

  it('supersedes an open confirmation with 暂不 when another untrusted workspace is selected', async () => {
    const state = createState();
    state.workspaces = [
      workspace('wd_a', '/abs/a', 'A', false),
      workspace('wd_b', '/abs/b', 'B', false),
    ];
    const deps = createDeps();
    deps.mergedWorkspaces = computed(() => state.workspaces);
    const ws = useWorkspaceState(state, deps);
    const prompt = useWorkspaceTrustPrompt();

    // When A is selected (confirmation for A opens), then B is selected
    ws.openWorkspace('wd_a');
    ws.openWorkspace('wd_b');

    // Then the older request resolved as 暂不 (no write for A) and the
    // dialog now describes B
    await Promise.resolve();
    expect(apiMock.setWorkspaceTrust).not.toHaveBeenCalledWith('wd_a', true);
    expect(prompt.current.value?.target.id).toBe('wd_b');
  });
});

// The dialog's pure view-model builder (the DeleteWorkspaceDialog model
// pattern — node-env testable without mounting).
describe('buildTrustWorkspaceDialogModel', () => {
  it('builds the closed model from a null target', () => {
    const model = buildTrustWorkspaceDialogModel(null, {
      trust: 'Trust this workspace',
      later: 'Not now',
      hint: 'hint',
    });
    expect(model.title).toBe('');
    expect(model.rootLine).toBe('');
  });

  it('builds the two-action model for a target', () => {
    const model = buildTrustWorkspaceDialogModel(
      { id: 'wd_new', name: 'New', root: '/abs/new' },
      { trust: 'Trust this workspace', later: 'Not now', hint: 'not in effect' },
    );
    expect(model.title).toBe('New');
    expect(model.rootLine).toBe('/abs/new');
    expect(model.trustLabel).toBe('Trust this workspace');
    expect(model.laterLabel).toBe('Not now');
    expect(model.hint).toBe('not in effect');
  });
});

// --- B10 D-25: swarm enable consistency + mode-toggle failure rollback --------

describe('useWorkspaceState — swarm enable consistency (D-25)', () => {
  beforeEach(() => {
    confirmMock.mockReset().mockResolvedValue(true);
  });

  function swarmDeps(overrides: Partial<UseWorkspaceStateDeps> = {}) {
    const deps = createDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    return { ...deps, ...overrides } as UseWorkspaceStateDeps;
  }

  it('Given manual permission and /swarm on Then enableSwarmMode confirms before flipping', async () => {
    confirmMock.mockResolvedValue(false); // user cancels
    const state = createState(); // permission: 'manual'
    const workspace = useWorkspaceState(state, swarmDeps());

    await workspace.enableSwarmMode();

    expect(confirmMock).toHaveBeenCalledTimes(1);
    expect(confirmMock).toHaveBeenCalledWith(
      expect.objectContaining({ title: i18n.global.t('workspace.swarmEnableConfirm') }),
    );
    expect(state.swarmModeBySession['sess_1']).toBeUndefined();
  });

  it('Given auto permission and /swarm on Then no confirmation is shown', async () => {
    const state = { ...createState(), permission: 'auto' as const };
    const workspace = useWorkspaceState(state, swarmDeps());

    await workspace.enableSwarmMode();

    expect(confirmMock).not.toHaveBeenCalled();
    expect(state.swarmModeBySession['sess_1']).toBe(true);
  });

  it('Given a persist failure Then the optimistic swarm flip is rolled back', async () => {
    const deps = swarmDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(false);
    const state = createState();
    state.swarmModeBySession = { sess_1: false };
    const workspace = useWorkspaceState(state, deps);

    await workspace.setSwarmMode(true);

    expect(state.swarmModeBySession['sess_1']).toBe(false); // reverted, not left lying on
    expect(deps.saveSwarmModeToStorage).toHaveBeenCalledTimes(2); // flip + revert
  });

  it('Given a persist failure Then the failure notice names the mode toggle (dedicated title)', async () => {
    const deps = swarmDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(false);
    const state = createState();
    state.backend = 'v3';
    const workspace = useWorkspaceState(state, deps);

    await workspace.setCollaborationMode(true);

    // The dedicated title is threaded through persistSessionProfile's opts:
    expect(deps.persistSessionProfile).toHaveBeenCalledWith(
      { collaborationMode: true },
      undefined,
      expect.objectContaining({ title: i18n.global.t('warnings.collaborationModePersistFailed') }),
    );
    expect(deps.pushOperationFailure).not.toHaveBeenCalled(); // useWorkspaceState 不重复推送（facade 内单次）
  });

  it('Given swarm already in the requested state Then setSwarmMode is a no-op (no flicker RPC)', async () => {
    const state = createState();
    state.swarmModeBySession = { sess_1: true };
    const deps = swarmDeps();
    const workspace = useWorkspaceState(state, deps);

    await workspace.setSwarmMode(true);

    expect(deps.persistSessionProfile).not.toHaveBeenCalled();
  });
});

// --- B10 D-25③: /swarm <task> opens a manual swarm + sends the first task ---

describe('useWorkspaceState — runSwarmTask (D-25③)', () => {
  beforeEach(() => {
    confirmMock.mockReset().mockResolvedValue(true);
    apiMock.submitPrompt.mockReset();
    apiMock.updateSession.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'turn_1', userMessageId: 'u1', status: 'running' });
    apiMock.updateSession.mockResolvedValue(undefined);
  });

  it('Given manual permission and a cancelled confirm Then no prompt is sent and no task entry recorded', async () => {
    confirmMock.mockResolvedValue(false);
    const state = createState();
    const workspace = useWorkspaceState(state, createDeps());

    await workspace.runSwarmTask('梳理仓库');

    expect(confirmMock).toHaveBeenCalled();
    expect(state.swarmTaskBySession['sess_1']).toBeUndefined();
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
  });

  it('Given a confirmed /swarm <task> Then swarm is enabled, the prompt sent, and the task entry recorded', async () => {
    const state = createState();
    const deps = {
      ...createDeps(),
      activity: computed(() => 'idle' as const),
    };
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    const workspace = useWorkspaceState(state, deps);

    await workspace.runSwarmTask('梳理仓库');

    expect(state.swarmModeBySession['sess_1']).toBe(true);
    expect(state.swarmTaskBySession['sess_1']).toBe(true);
    expect(apiMock.submitPrompt).toHaveBeenCalledTimes(1);
  });

  it('Given the session is busy running another turn Then the task is queued, not submitted concurrently', async () => {
    const state = createState();
    const deps = createDeps(); // default activity: 'running'
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    const workspace = useWorkspaceState(state, deps);

    await workspace.runSwarmTask('梳理仓库');

    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
    expect(state.queuedBySession['sess_1']).toEqual([
      { text: '梳理仓库', attachments: undefined },
    ]);
    expect(state.swarmTaskBySession['sess_1']).toBe(true);
  });

  it('Given the first instruction fails to submit Then the task entry is dropped (no false ended marker)', async () => {
    const state = createState();
    const deps = {
      ...createDeps(),
      activity: computed(() => 'idle' as const),
    };
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    apiMock.submitPrompt.mockRejectedValue(new Error('network down'));
    const workspace = useWorkspaceState(state, deps);

    await workspace.runSwarmTask('梳理仓库');

    expect(state.swarmTaskBySession['sess_1']).toBeUndefined();
    expect(state.swarmModeBySession['sess_1']).toBe(true); // the switch's own persist succeeded — stays on
  });

  it('Given a failed swarm enable (persist rollback) Then the task is not sent outside a swarm', async () => {
    const state = createState();
    const deps = createDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(false);
    const workspace = useWorkspaceState(state, deps);

    await workspace.runSwarmTask('梳理仓库');

    expect(state.swarmModeBySession['sess_1']).toBe(false); // rolled back
    expect(state.swarmTaskBySession['sess_1']).toBeUndefined();
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
  });

  it('Given swarm is turned off Then the task entry is cleared', async () => {
    const state = createState();
    state.swarmModeBySession = { sess_1: true };
    state.swarmTaskBySession = { sess_1: true };
    const deps = createDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    const workspace = useWorkspaceState(state, deps);

    await workspace.setSwarmMode(false);

    expect(state.swarmModeBySession['sess_1']).toBe(false);
    expect(state.swarmTaskBySession['sess_1']).toBeUndefined();
  });
});

// --- B10 D-24: collaboration toggle is backend-gated ---------------------------

describe('useWorkspaceState — collaboration capability gate (D-24)', () => {
  it('Given a v1/v2 backend Then setCollaborationMode is a no-op (no persist, no flip)', async () => {
    const state = createState(); // backend: 'v1' — same unsupported family
    const deps = createDeps();
    const workspace = useWorkspaceState(state, deps);

    await workspace.setCollaborationMode(true);

    expect(state.collaborationModeBySession['sess_1']).toBeUndefined();
    expect(deps.saveCollaborationModeToStorage).not.toHaveBeenCalled();
    expect(deps.persistSessionProfile).not.toHaveBeenCalled();

    const v2 = createState();
    v2.backend = 'v2';
    const ws2 = useWorkspaceState(v2, createDeps());
    await ws2.setCollaborationMode(true);
    expect(v2.collaborationModeBySession['sess_1']).toBeUndefined();
  });

  it('Given a v3 backend Then setCollaborationMode persists and flips', async () => {
    const state = createState();
    state.backend = 'v3';
    const deps = createDeps();
    (deps.persistSessionProfile as ReturnType<typeof vi.fn>).mockResolvedValue(true);
    const workspace = useWorkspaceState(state, deps);

    await workspace.setCollaborationMode(true);

    expect(state.collaborationModeBySession['sess_1']).toBe(true);
  });
});

// --- SDR-004: draft permission mode lands on the new session ------------------

describe('startSessionAndSendPrompt: draft permission mode', () => {
  const newSession = { ...createSession(), id: 'sess_new', workspaceId: 'wd_1', cwd: '/abs/path' };

  it('should land the composer-selected permission mode on the session before the first prompt is submitted', async () => {
    // Given the user picked a mode in the empty composer (no session yet)
    apiMock.createSession.mockReset();
    apiMock.createSession.mockResolvedValue(newSession);
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'p_1', userMessageId: 'm_1', status: 'running' });
    let resolvePersist!: (persisted: boolean) => void;
    const persistGate = new Promise<boolean>((r) => {
      resolvePersist = r;
    });
    const persistSessionProfile = vi.fn().mockReturnValue(persistGate);
    const state = createState();
    state.permission = 'yolo';
    const ws = useWorkspaceState(state, {
      ...createDeps(),
      persistSessionProfile,
      // Same first-prompt fixture deps every other describe in this file uses:
      // a workspace to draft into, plus a modelProvider whose draftModel ref
      // createDraftSession reads for the new session's model.
      taskPoller: { loadTasksForSession: vi.fn() } as unknown as UseWorkspaceStateDeps['taskPoller'],
      modelProvider: {
        draftModel: ref(null),
        skillsBySession: ref({}),
        loadSkillsForSession: vi.fn(),
        resolveThinkingForPrompt: async () => undefined,
      } as unknown as UseWorkspaceStateDeps['modelProvider'],
      mergedWorkspaces: computed(() => [workspace('wd_1', '/abs/path', 'A')]),
    });

    // When the first prompt is sent from the empty composer
    const pending = ws.startSessionAndSendPrompt('wd_1', 'hello');
    await new Promise((r) => setTimeout(r, 0));

    // Then the selected mode lands on the new session BEFORE the prompt is
    // submitted (SDR-004 c: land first, adjudicate later)
    expect(persistSessionProfile).toHaveBeenCalledWith({ permissionMode: 'yolo' }, 'sess_new');
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();

    resolvePersist(true);
    await pending;
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_new',
      expect.objectContaining({ permissionMode: 'yolo' }),
    );
  });
});
