// apps/mirri-web/test/file-reference-submit.test.ts
// E9 submit pipeline — the pre-send validation and the v2 degrade contract.
//
// Covers three layers, all pure logic (no component mounts):
//   1. `extractInvalidReferences` — the stat-result classification of the
//      doc-collected file references (valid/invalid split, reasons).
//   2. `submitPromptInternal`'s reference gate — a v3 send with stale
//      references rejects with `invalid_file_references` (code + list) and
//      never POSTs; a v2 send degrades to the plain-text path (references
//      dropped, no stat call, prompt goes through).
//   3. The capability bit — `supportsFileReferences` is true on v3 and false
//      on v2, so the composer can keep inserting plain text there.

import { computed, ref } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { extractInvalidReferences } from '../src/lib/fileMention';
import type { PromptReference } from '../src/api/types';
import {
  useWorkspaceState,
  type UseWorkspaceStateDeps,
} from '../src/composables/client/useWorkspaceState';
import type { ExtendedState } from '../src/composables/useMirriWebClient';

// The workspace-state module reads the api through getMirriWebApi() — mock
// the module so the reference gate can be driven with fake stat/submit/read.
const apiMock = vi.hoisted(() => ({
  statFiles: vi.fn(),
  submitPrompt: vi.fn(),
  readFile: vi.fn(),
}));
vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

function createState(backend: 'v2' | 'v3'): ExtendedState {
  return {
    backend,
    sessions: [
      {
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
      },
    ],
    activeSessionId: 'sess_1',
    connected: true,
    serverVersion: '',
    dangerousBypassAuth: false,
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
  } as unknown as ExtendedState;
}

function createDeps(): UseWorkspaceStateDeps {
  return {
    taskPoller: {},
    sideChat: {},
    modelProvider: { resolveThinkingForPrompt: async () => undefined },
    pushOperationFailure: vi.fn(),
    activity: computed(() => 'idle'),
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

function statItem(path: string, exists: boolean, kind: string, readable: boolean) {
  return { path, exists, kind, readable, mtimeMs: exists ? 1 : null };
}

describe('extractInvalidReferences', () => {
  it('should split references into valid and invalid by stat results', () => {
    const refs: PromptReference[] = [
      { path: 'a.ts', kind: 'file' },
      { path: 'd', kind: 'folder' },
      { path: 'gone.ts', kind: 'file' },
    ];
    const stat = {
      items: [
        statItem('a.ts', true, 'file', true),
        statItem('d', true, 'directory', true),
        statItem('gone.ts', false, 'file', false),
      ],
    };
    const { valid, invalid } = extractInvalidReferences(stat, refs);
    expect(valid).toHaveLength(2);
    expect(valid.map((r) => r.path)).toEqual(['a.ts', 'd']);
    expect(invalid).toHaveLength(1);
    expect(invalid[0]).toMatchObject({ path: 'gone.ts', reason: 'not_found' });
  });

  it('should mark kind mismatches as invalid', () => {
    // The picker said file, the filesystem says directory (and vice versa) —
    // the injected form would be wrong, so the reference is rejected.
    const refs: PromptReference[] = [
      { path: 'now-a-dir', kind: 'file' },
      { path: 'now-a-file', kind: 'folder' },
    ];
    const stat = {
      items: [
        statItem('now-a-dir', true, 'directory', true),
        statItem('now-a-file', true, 'file', true),
      ],
    };
    const { invalid } = extractInvalidReferences(stat, refs);
    expect(invalid).toHaveLength(2);
    expect(invalid[0]).toMatchObject({ path: 'now-a-dir', reason: 'kind_mismatch' });
    expect(invalid[1]).toMatchObject({ path: 'now-a-file', reason: 'kind_mismatch' });
  });

  it('should accept a symlink as a file reference', () => {
    const { valid, invalid } = extractInvalidReferences(
      { items: [statItem('link.ts', true, 'symlink', true)] },
      [{ path: 'link.ts', kind: 'file' }],
    );
    expect(valid).toHaveLength(1);
    expect(invalid).toHaveLength(0);
  });

  it('should mark an unreadable-but-existing path as unreadable', () => {
    const { invalid } = extractInvalidReferences(
      { items: [statItem('secret.ts', true, 'file', false)] },
      [{ path: 'secret.ts', kind: 'file' }],
    );
    expect(invalid[0]).toMatchObject({ path: 'secret.ts', reason: 'unreadable' });
  });

  it('should treat a path missing from the stat answer as not found', () => {
    // Defensive: an engine that omits items (empty probe result) must not
    // pass the gate — the path is simply unknown → not found.
    const { invalid } = extractInvalidReferences({ items: [] }, [{ path: 'x.ts', kind: 'file' }]);
    expect(invalid[0]).toMatchObject({ path: 'x.ts', reason: 'not_found' });
  });
});

describe('submit pipeline reference gate', () => {
  beforeEach(() => {
    apiMock.statFiles.mockReset();
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'prompt_new' });
    apiMock.readFile.mockReset();
    apiMock.readFile.mockResolvedValue({ lineCount: 10, isBinary: false });
  });

  it('should reject a v3 send with invalid references and not submit', async () => {
    apiMock.statFiles.mockResolvedValue({
      items: [
        statItem('a.ts', true, 'file', true),
        statItem('gone.ts', false, 'file', false),
      ],
    });
    const ws = useWorkspaceState(createState('v3'), createDeps());

    const outcome = await ws.submitPromptInternal('sess_1', '看 @a.ts 和 @gone.ts', undefined, [
      { path: 'a.ts', kind: 'file' },
      { path: 'gone.ts', kind: 'file' },
    ]);

    expect(outcome.ok).toBe(false);
    expect(outcome.error?.code).toBe('invalid_file_references');
    expect(outcome.error?.references).toEqual([{ path: 'gone.ts', reason: 'not_found' }]);
    // The prompt itself never left the client.
    expect(apiMock.submitPrompt).not.toHaveBeenCalled();
  });

  it('should submit with the references attached when every reference resolves', async () => {
    apiMock.statFiles.mockResolvedValue({
      items: [statItem('a.ts', true, 'file', true), statItem('d', true, 'directory', true)],
    });
    const ws = useWorkspaceState(createState('v3'), createDeps());
    const refs: PromptReference[] = [
      { path: 'a.ts', kind: 'file' },
      { path: 'd', kind: 'folder' },
    ];

    const outcome = await ws.submitPromptInternal('sess_1', '看 @a.ts @d/', undefined, refs);

    expect(outcome.ok).toBe(true);
    expect(apiMock.statFiles).toHaveBeenCalledWith('sess_1', ['a.ts', 'd']);
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_1',
      expect.objectContaining({ references: refs }),
    );
  });

  it('should send the prompt without references on v2 (plain-text degrade)', async () => {
    // v2 has no file.stat and no references wire — the @path spellings stay
    // in the text (the composer's serialization) and the references array is
    // dropped entirely; no stat probe is attempted.
    const ws = useWorkspaceState(createState('v2'), createDeps());

    const outcome = await ws.submitPromptInternal('sess_1', '看 @a.ts', undefined, [
      { path: 'a.ts', kind: 'file' },
    ]);

    expect(outcome.ok).toBe(true);
    expect(apiMock.statFiles).not.toHaveBeenCalled();
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_1',
      expect.not.objectContaining({ references: expect.anything() }),
    );
  });

  it('should send without a stat probe when the send carries no references', async () => {
    const ws = useWorkspaceState(createState('v3'), createDeps());

    const outcome = await ws.submitPromptInternal('sess_1', 'plain prompt');

    expect(outcome.ok).toBe(true);
    expect(apiMock.statFiles).not.toHaveBeenCalled();
    expect(apiMock.submitPrompt).toHaveBeenCalledTimes(1);
  });

  it('should send anyway when the stat probe fails (fail-open)', async () => {
    // A stat RPC failure is a degraded transport, not a verdict — the send
    // proceeds and the engine's fail-soft invalidReferences is the backstop.
    apiMock.statFiles.mockRejectedValue(new Error('probe failed'));
    const ws = useWorkspaceState(createState('v3'), createDeps());
    const refs: PromptReference[] = [{ path: 'a.ts', kind: 'file' }];

    const outcome = await ws.submitPromptInternal('sess_1', '看 @a.ts', undefined, refs);

    expect(outcome.ok).toBe(true);
    expect(apiMock.submitPrompt).toHaveBeenCalledWith(
      'sess_1',
      expect.objectContaining({ references: refs }),
    );
  });
});

describe('submit pipeline M4 truncation probe + M5 echo metadata', () => {
  beforeEach(() => {
    apiMock.statFiles.mockReset();
    apiMock.submitPrompt.mockReset();
    apiMock.submitPrompt.mockResolvedValue({ promptId: 'prompt_new' });
    apiMock.readFile.mockReset();
    apiMock.readFile.mockResolvedValue({ lineCount: 10, isBinary: false });
  });

  /** Capture the optimistic echo appended by the submit (updateSessionMessages
   *  side-effect) — the ONLY place the M5 metadata surfaces. */
  function optimisticEchoOf(updateSessionMessages: ReturnType<typeof vi.fn>): {
    metadata: Record<string, unknown> | undefined;
  } {
    const calls = updateSessionMessages.mock.calls as unknown as Array<
      [string, (msgs: unknown[]) => unknown[]]
    >;
    for (const [, updater] of calls) {
      const appended = updater([]) as Array<{ metadata?: Record<string, unknown> }>;
      if (appended.length === 1) return { metadata: appended[0]!.metadata };
    }
    return { metadata: undefined };
  }

  it('should stamp truncated on the echo metadata when the file exceeds the line budget', async () => {
    apiMock.statFiles.mockResolvedValue({ items: [statItem('big.log', true, 'file', true)] });
    apiMock.readFile.mockResolvedValue({ lineCount: 2000, isBinary: false });
    const updateSessionMessages = vi.fn();
    const deps = createDeps();
    deps.updateSessionMessages = updateSessionMessages;
    const ws = useWorkspaceState(createState('v3'), deps as unknown as UseWorkspaceStateDeps);

    const outcome = await ws.submitPromptInternal('sess_1', '看 @big.log', undefined, [
      { path: 'big.log', kind: 'file' },
    ]);

    expect(outcome.ok).toBe(true);
    // Only the head the budget could need — no full-file read for the probe.
    expect(apiMock.readFile).toHaveBeenCalledWith('sess_1', { path: 'big.log', length: 131072 });
    const { metadata } = optimisticEchoOf(updateSessionMessages);
    expect(metadata?.['mirriWeb.echoedFileReferences']).toEqual([
      { path: 'big.log', kind: 'file', truncated: true },
    ]);
  });

  it('should not stamp truncated when the file is within the line budget', async () => {
    apiMock.statFiles.mockResolvedValue({ items: [statItem('small.ts', true, 'file', true)] });
    apiMock.readFile.mockResolvedValue({ lineCount: 42, isBinary: false });
    const updateSessionMessages = vi.fn();
    const deps = createDeps();
    deps.updateSessionMessages = updateSessionMessages;
    const ws = useWorkspaceState(createState('v3'), deps as unknown as UseWorkspaceStateDeps);

    const outcome = await ws.submitPromptInternal('sess_1', '看 @small.ts', undefined, [
      { path: 'small.ts', kind: 'file' },
    ]);

    expect(outcome.ok).toBe(true);
    const { metadata } = optimisticEchoOf(updateSessionMessages);
    expect(metadata?.['mirriWeb.echoedFileReferences']).toEqual([
      { path: 'small.ts', kind: 'file' },
    ]);
  });

  it('should not probe folders and should not fail the send when the probe errors', async () => {
    apiMock.statFiles.mockResolvedValue({ items: [statItem('d', true, 'directory', true)] });
    apiMock.readFile.mockRejectedValue(new Error('read failed'));
    const updateSessionMessages = vi.fn();
    const deps = createDeps();
    deps.updateSessionMessages = updateSessionMessages;
    const ws = useWorkspaceState(createState('v3'), deps as unknown as UseWorkspaceStateDeps);

    const outcome = await ws.submitPromptInternal('sess_1', '看 @d/', undefined, [
      { path: 'd', kind: 'folder' },
    ]);

    expect(outcome.ok).toBe(true);
    expect(apiMock.readFile).not.toHaveBeenCalled();
    const { metadata } = optimisticEchoOf(updateSessionMessages);
    expect(metadata?.['mirriWeb.echoedFileReferences']).toEqual([{ path: 'd', kind: 'folder' }]);
  });

  it('should omit the echo metadata entirely on v2 (references dropped)', async () => {
    const updateSessionMessages = vi.fn();
    const deps = createDeps();
    deps.updateSessionMessages = updateSessionMessages;
    const ws = useWorkspaceState(createState('v2'), deps as unknown as UseWorkspaceStateDeps);

    const outcome = await ws.submitPromptInternal('sess_1', '看 @a.ts', undefined, [
      { path: 'a.ts', kind: 'file' },
    ]);

    expect(outcome.ok).toBe(true);
    expect(apiMock.readFile).not.toHaveBeenCalled();
    const { metadata } = optimisticEchoOf(updateSessionMessages);
    expect(metadata?.['mirriWeb.echoedFileReferences']).toBeUndefined();
    expect(metadata?.['mirriWeb.optimisticUserMessage']).toBe(true);
  });
});

describe('supportsFileReferences capability', () => {
  it('should be true on v3 and false on v2', () => {
    const v3 = useWorkspaceState(createState('v3'), createDeps());
    const v2 = useWorkspaceState(createState('v2'), createDeps());
    expect(v3.supportsFileReferences()).toBe(true);
    expect(v2.supportsFileReferences()).toBe(false);
  });
});
