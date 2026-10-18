// apps/mirri-web/test/ws-rpc-session-control.test.ts
// Given the rust engine's interaction surface (the `session.createChild`
// fork RPC with the klient scope-handle wire, and `message.abort`'s
// session-wide cancel when no turnId is sent — the shape the v1 daemon's
// `POST /sessions/{id}:abort` answered with `{ aborted: true }`), the
// WsRpcApi session-control methods must send exactly those calls: abort
// cancels whatever runs without a turn id and always reports aborted,
// createChildSession forks through `session.createChild` then reads the
// AppSession view back with `session.get`, and listPrompts answers an
// honest empty view (the engine has no daemon-side prompt queue).
// Also covers the transport-connect contract (connectEvents /
// connectTransport open the RPC WebSocket eagerly, mirroring the TS
// backend's socket.connect()) and the `fs.browse` / `fs.home`
// host-folder-browser face (snake_case wire → camelCase app vocabulary),
// plus `session.snapshot`'s initial-sync mapping (the rust v2 wire:
// snake_case envelope with the `messages.items` page).

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import type { MirriEventHandlers } from '../src/api/types';

/** The JsonRpcClient.call signature the adapters under test rely on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type EventHandler = (event: unknown) => void;

function createApi(): {
  api: WsRpcApi;
  call: ReturnType<typeof vi.fn<RpcCall>>;
  connect: ReturnType<typeof vi.fn<() => Promise<void>>>;
} {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>().mockResolvedValue(undefined);
  const connect = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);
  (api as unknown as { client: unknown }).client = {
    call,
    connect,
    on: vi.fn<(type: string, handler: EventHandler) => () => void>(),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  return { api, call, connect };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi abortSession', () => {
  it('should cancel the whole session through message.abort without a turnId and report aborted', async () => {
    const { api, call } = createApi();

    const result = await api.abortSession('s1');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('message.abort', { session_id: 's1' });
    expect(result).toEqual({ aborted: true });
  });

  it('should propagate the engine error when the session does not exist', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('session not found'));

    await expect(api.abortSession('s_missing')).rejects.toThrow('session not found');
  });
});

describe('WsRpcApi createChildSession', () => {
  it('should fork through session.createChild and read the AppSession view back with session.get', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string, params?: unknown) => {
      if (method === 'session.createChild') {
        expect(params).toEqual({ sourceSessionId: 's1', title: 'Side topic' });
        return { id: 'session-child-1', kind: 2 };
      }
      if (method === 'session.get') {
        expect(params).toEqual({ session_id: 'session-child-1' });
        return {
          id: 'session-child-1',
          title: 'Child: parent',
          createdAt: '2026-02-01T00:00:00Z',
          updatedAt: '2026-02-01T00:00:00Z',
          agentCount: 1,
          tags: [],
          agents: [{ id: 'main' }],
        };
      }
      throw new Error(`unexpected call: ${method}`);
    });

    const child = await api.createChildSession('s1', { title: 'Side topic' });

    expect(call).toHaveBeenCalledTimes(2);
    expect(child).toMatchObject({
      id: 'session-child-1',
      title: 'Child: parent',
      createdAt: '2026-02-01T00:00:00Z',
      updatedAt: '2026-02-01T00:00:00Z',
    });
  });

  it('should omit the title key when the caller passes none (engine default title)', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string, params?: unknown) => {
      if (method === 'session.createChild') {
        expect(params).toEqual({ sourceSessionId: 's1' });
        return { id: 'session-child-2', kind: 2 };
      }
      return {
        id: 'session-child-2',
        title: 'Child: parent',
        createdAt: '2026-02-01T00:00:00Z',
        updatedAt: '2026-02-01T00:00:00Z',
        agentCount: 1,
        tags: [],
      };
    });

    await api.createChildSession('s1');

    expect(call).toHaveBeenCalledWith('session.createChild', { sourceSessionId: 's1' });
  });
});

describe('WsRpcApi listPrompts', () => {
  it('should answer an honest empty view without any RPC (the engine has no prompt queue)', async () => {
    const { api, call } = createApi();

    const result = await api.listPrompts('s1');

    expect(call).not.toHaveBeenCalled();
    expect(result).toEqual({ active: null, queued: [] });
  });
});

describe('WsRpcApi session-view runtime fields (P7c)', () => {
  it('should map the engine SessionView runtime fields onto AppSession in listSessions', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      items: [
        {
          id: 's1',
          title: 'runtime',
          createdAt: '2026-02-01T00:00:00Z',
          updatedAt: '2026-02-01T00:01:00Z',
          agentCount: 1,
          tags: [],
          busy: true,
          model: 'claude-sonnet-4-20250514',
          usage: {
            inputTokens: 100,
            outputTokens: 50,
            cacheReadTokens: 10,
            cacheCreationTokens: 5,
          },
          messageCount: 3,
          lastSeq: 7,
          lastPrompt: 'seed the session',
          archived: true,
        },
        // A persisted-but-not-live row answers the neutral static view.
        {
          id: 's2',
          title: 'persisted',
          createdAt: '2026-02-01T00:00:00Z',
          updatedAt: '2026-02-01T00:00:00Z',
          agentCount: 0,
          tags: [],
        },
      ],
      has_more: false,
    });

    const page = await api.listSessions({});

    // W5: the unsealed filter face — exclude_empty defaults to true
    // (SDR-004: a never-conversed session is hidden, REST parity).
    expect(call).toHaveBeenCalledWith('session.list', { exclude_empty: true });
    expect(page.hasMore).toBe(false);
    const [live, persisted] = page.items;
    expect(live).toMatchObject({
      id: 's1',
      busy: true,
      model: 'claude-sonnet-4-20250514',
      messageCount: 3,
      lastSeq: 7,
      lastPrompt: 'seed the session',
      archived: true,
    });
    expect(live?.usage).toEqual({
      inputTokens: 100,
      outputTokens: 50,
      cacheReadTokens: 10,
      cacheCreationTokens: 5,
      totalCostUsd: 0,
      contextTokens: 0,
      contextLimit: 0,
      turnCount: 0,
    });
    expect(persisted).toMatchObject({ id: 's2', busy: false, model: '', messageCount: 0, lastSeq: 0, lastPrompt: undefined, archived: false });
  });

  it('should forward the W5 filter face onto session.list (workspace, archive, busy, cursors, page size)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ items: [], has_more: true });

    const page = await api.listSessions({
      workspaceId: 'ws-1',
      includeArchive: true,
      archivedOnly: false,
      busy: false,
      beforeId: 's-older',
      afterId: 's-newer',
      pageSize: 25,
      excludeEmpty: false,
    });

    expect(call).toHaveBeenCalledWith('session.list', {
      exclude_empty: false,
      workspace_id: 'ws-1',
      include_archive: true,
      archived_only: false,
      busy: false,
      before_id: 's-older',
      after_id: 's-newer',
      page_size: 25,
    });
    expect(page).toEqual({ items: [], hasMore: true });
  });

  it('should map the P7c session.status runtime fields', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      sessionId: 's1',
      running: true,
      agentCount: 1,
      model: 'claude-sonnet-4-20250514',
      permission: 'manual',
      planMode: false,
      thinkingEffort: 'high',
      swarmMode: true,
      collaborationMode: true,
      contextTokens: 12345,
      maxContextTokens: 200000,
    });

    const status = await api.getSessionStatus('s1');

    expect(call).toHaveBeenCalledWith('session.status', { sessionId: 's1' });
    expect(status).toEqual({
      model: 'claude-sonnet-4-20250514',
      thinkingEffort: 'high',
      permission: 'manual',
      planMode: false,
      swarmMode: true,
      collaborationMode: true,
      contextTokens: 12345,
      maxContextTokens: 200000,
      contextUsage: 0,
    });
  });

  it('should fall back to neutral status fields on an older engine shape', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      sessionId: 's1',
      running: false,
      agentCount: 1,
      model: null,
      permission: 'manual',
      planMode: false,
    });

    const status = await api.getSessionStatus('s1');

    expect(status).toMatchObject({
      model: null,
      thinkingEffort: '',
      swarmMode: false,
      collaborationMode: false,
      contextTokens: 0,
    });
  });

  it('should map the goal.get snapshot onto AppGoal with the derived budget', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      goal: {
        goalId: 'goal-1',
        objective: 'ship the parity batch',
        completionCriterion: null,
        status: 'active',
        turnsUsed: 3,
        tokensUsed: 400,
        wallClockMs: 0,
        budget: { turns: 40, tokens: 500, wallClockMs: 3600000 },
      },
    });

    const goal = await api.getSessionGoal('s1');

    expect(call).toHaveBeenCalledWith('goal.get', { session_id: 's1' });
    expect(goal).toMatchObject({
      goalId: 'goal-1',
      objective: 'ship the parity batch',
      status: 'active',
      turnsUsed: 3,
      tokensUsed: 400,
      budget: {
        tokenBudget: 500,
        remainingTokens: 100,
        turnBudget: 40,
        remainingTurns: 37,
        wallClockBudgetMs: 3600000,
        overBudget: false,
      },
    });
  });

  it('should report an over-budget goal and null when no goal is set', async () => {
    const { api, call } = createApi();
    call.mockResolvedValueOnce({
      goal: {
        goalId: 'goal-1',
        objective: 'overspend',
        status: 'active',
        turnsUsed: 50,
        tokensUsed: 900,
        wallClockMs: 0,
        budget: { turns: 40, tokens: 500, wallClockMs: 3600000 },
      },
    });
    call.mockResolvedValueOnce({ goal: null });

    const over = await api.getSessionGoal('s1');
    const none = await api.getSessionGoal('s1');

    expect(over?.budget?.overBudget).toBe(true);
    expect(over?.budget?.remainingTokens).toBe(0);
    expect(over?.budget?.remainingTurns).toBe(0);
    expect(none).toBeNull();
  });
});

describe('WsRpcApi transport connect (P11c)', () => {
  function eventHandlers(): MirriEventHandlers {
    return {
      onEvent: vi.fn(),
      onResync: vi.fn(),
      onError: vi.fn(),
      onConnectionChange: vi.fn(),
    };
  }

  it('should connect the transport when connectEvents binds handlers', async () => {
    // Given a rust-transport WsRpcApi whose WebSocket is not yet connected
    const { api, connect } = createApi();

    // When connectEvents binds the UI handlers
    api.connectEvents(eventHandlers());

    // Then the transport connect is triggered eagerly (the implied
    // connectEvents contract the TS backend's socket.connect() honours)
    expect(connect).toHaveBeenCalledTimes(1);
  });

  it('should not break the handler binding when the eager connect rejects', async () => {
    // Given a transport whose connect fails (e.g. stale credential)
    const { api, connect } = createApi();
    connect.mockRejectedValue(new Error('WebSocket error during handshake'));

    // When connectEvents binds the UI handlers
    api.connectEvents(eventHandlers());

    // Then the rejection is swallowed (errors surface through the bound
    // handlers instead of an unhandled promise rejection)
    await expect(connect.mock.results[0]?.value).rejects.toThrow(
      'WebSocket error during handshake',
    );
  });

  it('should connect the transport when connectTransport is called from boot', async () => {
    // Given a rust-transport WsRpcApi
    const { api, connect } = createApi();

    // When the boot flow calls connectTransport
    await api.connectTransport();

    // Then the transport connect runs exactly once
    expect(connect).toHaveBeenCalledTimes(1);
  });
});

describe('WsRpcApi browseFs (P11c)', () => {
  it('should send fs.browse with the path and map the snake_case rows onto the app vocabulary when a path is given', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      path: '/Users/dev/project',
      parent: '/Users/dev',
      entries: [
        { name: 'src', path: '/Users/dev/project/src', is_dir: true },
        {
          name: '.git',
          path: '/Users/dev/project/.git',
          is_dir: true,
          is_git_repo: true,
          branch: 'main',
        },
      ],
    });

    const result = await api.browseFs('/Users/dev/project');

    expect(call).toHaveBeenCalledWith('fs.browse', { path: '/Users/dev/project' });
    expect(result).toEqual({
      path: '/Users/dev/project',
      parent: '/Users/dev',
      entries: [
        // Rust directory rows omit the git fields — neutral defaults.
        { name: 'src', path: '/Users/dev/project/src', isDir: true, isGitRepo: false },
        {
          name: '.git',
          path: '/Users/dev/project/.git',
          isDir: true,
          isGitRepo: true,
          branch: 'main',
        },
      ],
    });
  });

  it('should send an empty params object when no path is given (home browse)', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ path: '/Users/dev', parent: null, entries: [] });

    const result = await api.browseFs();

    expect(call).toHaveBeenCalledWith('fs.browse', {});
    expect(result).toEqual({ path: '/Users/dev', parent: null, entries: [] });
  });

  it('should propagate the engine error when the browse fails (caller degrades)', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('path not found: /nope'));

    await expect(api.browseFs('/nope')).rejects.toThrow('path not found: /nope');
  });
});

describe('WsRpcApi getFsHome (P11c)', () => {
  it('should map recent_roots onto recentRoots when the engine answers fs.home', async () => {
    const { api, call } = createApi();
    // The rust engine always answers the honest empty recent_roots.
    call.mockResolvedValue({ home: '/Users/dev', recent_roots: [] });

    const result = await api.getFsHome();

    expect(call).toHaveBeenCalledWith('fs.home');
    expect(result).toEqual({ home: '/Users/dev', recentRoots: [] });
  });

  it('should propagate the engine error when fs.home fails (caller degrades)', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('neither HOME nor USERPROFILE is set'));

    await expect(api.getFsHome()).rejects.toThrow('neither HOME nor USERPROFILE is set');
  });
});

describe('WsRpcApi getSessionSnapshot (P11c fix 2)', () => {
  it('should map the rust session.snapshot wire (snake_case envelope, messages.items page) onto AppSessionSnapshot when the engine answers', async () => {    // Given a rust engine snapshot in the real v2 wire shape: snake_case
    // envelope keys, the messages page nested under `{items, has_more}`, a
    // camelCase session sub-view that carries no title/createdAt/updatedAt,
    // and the (currently always-empty) pending arrays
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 42,
      epoch: 'server-epoch-7',
      session: {
        id: 's1',
        title: null,
        busy: true,
        agentCount: 1,
        model: 'ultra',
        permission: 'auto',
        messageCount: 2,
      },
      messages: {
        items: [
          { id: 'msg-0', role: 'user', content: 'hello', agentId: 'main' },
          {
            id: 'msg-1',
            role: 'assistant',
            content: [{ type: 'text', text: 'hi there' }],
            agentId: 'main',
          },
        ],
        has_more: false,
      },
      in_flight_turn: null,
      subagents: [],
      pending_approvals: [],
      pending_questions: [],
    });

    // When the snapshot is fetched
    const snapshot = await api.getSessionSnapshot('s1');

    // Then the envelope maps onto the v1 app vocabulary: the watermark, the
    // page's messages and has_more flag, and neutral defaults for the
    // fields the engine does not model yet
    expect(call).toHaveBeenCalledWith('session.snapshot', { session_id: 's1' });
    expect(snapshot.asOfSeq).toBe(42);
    expect(snapshot.epoch).toBe('server-epoch-7');
    expect(snapshot.session).toMatchObject({
      id: 's1',
      title: '',
      createdAt: '',
      updatedAt: '',
      busy: true,
      model: 'ultra',
      messageCount: 2,
    });
    expect(snapshot.messages.map((message) => ({ id: message.id, role: message.role, content: message.content }))).toEqual([
      { id: 'msg-0', role: 'user', content: [{ type: 'text', text: 'hello' }] },
      { id: 'msg-1', role: 'assistant', content: [{ type: 'text', text: 'hi there' }] },
    ]);
    expect(snapshot.messages.every((message) => message.sessionId === 's1')).toBe(true);
    expect(snapshot.hasMoreMessages).toBe(false);
    expect(snapshot.inFlightTurn).toBeNull();
    expect(snapshot.subagents).toEqual([]);
    expect(snapshot.pendingApprovals).toEqual([]);
    expect(snapshot.pendingQuestions).toEqual([]);
  });

  it('should carry the work-facts tuple onto the snapshot session view when the engine reports it (US4.4)', async () => {
    // Given a rust engine snapshot whose session sub-view carries the
    // work-facts tuple — a page refresh mid-turn restores the running
    // indicator from the snapshot instead of waiting for the first live
    // work_changed frame.
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 43,
      epoch: 'server-epoch-7',
      session: {
        id: 's1',
        title: null,
        busy: true,
        mainTurnActive: true,
        pendingInteraction: 'none',
        lastTurnReason: 'completed',
        agentCount: 1,
        model: 'ultra',
        messageCount: 2,
      },
      messages: { items: [], has_more: false },
      in_flight_turn: null,
      subagents: [],
      pending_approvals: [],
      pending_questions: [],
    });

    const snapshot = await api.getSessionSnapshot('s1');

    expect(snapshot.session).toMatchObject({
      id: 's1',
      busy: true,
      mainTurnActive: true,
      pendingInteraction: 'none',
      lastTurnReason: 'completed',
    });
  });

  it('should default the work-facts tuple to neutral values when an older engine snapshot omits the fields', async () => {
    // Given a snapshot from an older engine build without the tuple — the
    // neutral defaults keep the sidebar honest (not running) instead of
    // leaving undefined holes the composable would treat as falsey.
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 1,
      epoch: 'e',
      session: { id: 's1', busy: false, agentCount: 1, messageCount: 0 },
      messages: { items: [], has_more: false },
      in_flight_turn: null,
      subagents: [],
      pending_approvals: [],
      pending_questions: [],
    });

    const snapshot = await api.getSessionSnapshot('s1');

    expect(snapshot.session).toMatchObject({
      id: 's1',
      mainTurnActive: false,
      pendingInteraction: undefined,
      lastTurnReason: undefined,
    });
  });

  it('should map pending question items with camelCase multiSelect/allowOther when the rust engine answers a snapshot with pending questions', async () => {
    // Given a rust snapshot row whose pending_questions items carry the
    // klient camelCase spelling (`multiSelect`, `allowOther` — the
    // interaction kernel's normalized question items)
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 7,
      epoch: 'e',
      session: { id: 's1', busy: true, agentCount: 1, messageCount: 0 },
      messages: { items: [], has_more: false },
      in_flight_turn: null,
      subagents: [],
      pending_approvals: [],
      pending_questions: [
        {
          question_id: 'q-1',
          session_id: 's1',
          tool_call_id: 'call_q',
          created_at: '2026-10-01T00:00:00Z',
          questions: [
            {
              question: 'Which?',
              header: 'Pick',
              options: [{ label: 'A' }, { label: 'B' }],
              multiSelect: true,
              allowOther: true,
            },
          ],
        },
      ],
    });

    // When the snapshot is fetched
    const snapshot = await api.getSessionSnapshot('s1');

    // Then the pending question card carries the normalized item flags so
    // the UI renders the Other free-text input on the restore path too
    expect(snapshot.pendingQuestions).toHaveLength(1);
    expect(snapshot.pendingQuestions[0].questionId).toBe('q-1');
    expect(snapshot.pendingQuestions[0].questions[0]).toMatchObject({
      question: 'Which?',
      multiSelect: true,
      allowOther: true,
    });
  });

  it('should default the envelope scalars when an older engine omits them', async () => {
    // Given a snapshot answer missing the page fields entirely
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 0,
      epoch: 'e',
      session: { id: 's2', busy: false, agentCount: 0, messageCount: 0 },
    });

    // When the snapshot is fetched
    const snapshot = await api.getSessionSnapshot('s2');

    // Then the messages and has_more fall back to neutral defaults instead
    // of throwing (the pre-fix code called .map on the page object)
    expect(snapshot.messages).toEqual([]);
    expect(snapshot.hasMoreMessages).toBe(false);
  });
});


// W4 unseal — the create face's workspace routing params. Given a selected
// workspace A, when the composer creates a session (the empty-composer first
// prompt path), the wire params must carry A's id and A's cwd: the engine's
// `session.create` routes the whole construction chain (session bucket,
// workDir, index row) on `workspace_id`, with `metadata.cwd` alongside it —
// the same dual-send the REST client uses (client.ts POST /sessions).
describe('WsRpcApi createSession workspace routing', () => {
  it('should carry the selected workspace id and its cwd on the session.create wire', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ id: 's_new', title: 'Draft' });

    const session = await api.createSession({
      title: 'Draft',
      cwd: '/workspaces/alpha',
      model: 'ultra',
      workspaceId: 'wd_alpha_000000000001',
    });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('session.create', {
      title: 'Draft',
      modelId: 'ultra',
      workspace_id: 'wd_alpha_000000000001',
      metadata: { cwd: '/workspaces/alpha' },
    });
    expect(session.id).toBe('s_new');
  });

  it('should send an empty metadata object and no workspace_id when neither is provided', async () => {
    // The REST client always sends `metadata` (older daemons required the
    // object); the WS face mirrors that shape while omitting the routing
    // keys so the engine keeps its default workspace.
    const { api, call } = createApi();
    call.mockResolvedValue({ id: 's_default' });

    await api.createSession({ title: 'Plain' });

    expect(call).toHaveBeenCalledWith('session.create', {
      title: 'Plain',
      metadata: {},
    });
  });
});

describe('WsRpcApi updateSession mode toggles', () => {
  /** session.get view returned after the routed calls — updateSession always
   *  re-reads the session; only the routing of the mode fields matters here. */
  const SESSION_VIEW = {
    id: 's1',
    title: 'Modes',
    createdAt: '2026-02-01T00:00:00Z',
    updatedAt: '2026-02-01T00:00:00Z',
    agentCount: 1,
    tags: [],
  };

  it('should call collaboration.enter when updateSession receives collaborationMode true', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'collaboration.enter') return undefined;
      if (method === 'session.get') return SESSION_VIEW;
      throw new Error(`unexpected call: ${method}`);
    });

    const session = await api.updateSession('s1', { collaborationMode: true });

    expect(call).toHaveBeenCalledWith('collaboration.enter', { session_id: 's1' });
    expect(call).not.toHaveBeenCalledWith('collaboration.exit', expect.anything());
    expect(session.id).toBe('s1');
  });

  it('should call collaboration.exit when updateSession receives collaborationMode false', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'collaboration.exit') return undefined;
      if (method === 'session.get') return SESSION_VIEW;
      throw new Error(`unexpected call: ${method}`);
    });

    await api.updateSession('s1', { collaborationMode: false });

    expect(call).toHaveBeenCalledWith('collaboration.exit', { session_id: 's1' });
    expect(call).not.toHaveBeenCalledWith('collaboration.enter', expect.anything());
  });

  it('should not touch the collaboration methods when collaborationMode is omitted', async () => {
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'session.updateMeta') return undefined;
      if (method === 'session.get') return SESSION_VIEW;
      throw new Error(`unexpected call: ${method}`);
    });

    await api.updateSession('s1', { model: 'ultra' });

    expect(call).not.toHaveBeenCalledWith('collaboration.enter', expect.anything());
    expect(call).not.toHaveBeenCalledWith('collaboration.exit', expect.anything());
  });
});

// B10 D-26: the v3 transport sends NO per-prompt mode fields on message.send —
// mode state flows exclusively through its dedicated RPCs (routed by
// updateSession). The drop is explicit (pinned + voided), never silent.
describe('WsRpcApi submitPrompt — per-prompt mode fields are intentionally dropped (D-26)', () => {
  it('Given a prompt carrying swarm/collaboration/plan/permission/goal fields Then message.send params carry none of them', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ turn_id: 't1', agent_id: 'a1' });

    await api.submitPrompt('s1', {
      content: [{ type: 'text', text: 'hi' }],
      model: 'm1',
      thinking: 'high',
      permissionMode: 'manual',
      planMode: true,
      swarmMode: true,
      collaborationMode: true,
      goalObjective: 'do it',
      goalControl: 'pause',
    });

    expect(call).toHaveBeenCalledWith('message.send', {
      session_id: 's1',
      input: [{ type: 'text', text: 'hi' }],
      mode: 'fire',
      model: 'm1',
    });
  });
});

// AskUserQuestion 'Other' parity: the snapshot's pending-question mapper must
// read BOTH question-item spellings — the rust engine's klient camelCase
// (`multiSelect`/`allowOther`) and the TS daemon's snake_case
// (`multi_select`/`allow_other`) — so the restored question card renders the
// Other free-text input on either wire.
describe('WsRpcApi getSessionSnapshot pending question item spelling', () => {
  it('should map snake_case pending question items onto multiSelect/allowOther when an older TS daemon answers', async () => {
    // Given a snapshot row whose pending_questions items use the TS daemon's
    // snake_case spelling
    const { api, call } = createApi();
    call.mockResolvedValue({
      as_of_seq: 3,
      epoch: 'e',
      session: { id: 's1', busy: false, agentCount: 1, messageCount: 0 },
      messages: { items: [], has_more: false },
      in_flight_turn: null,
      subagents: [],
      pending_approvals: [],
      pending_questions: [
        {
          question_id: 'q-2',
          session_id: 's1',
          created_at: '2026-10-01T00:00:00Z',
          questions: [
            {
              question: 'Proceed?',
              options: [{ label: 'yes' }, { label: 'no' }],
              multi_select: false,
              allow_other: true,
            },
          ],
        },
      ],
    });

    // When the snapshot is fetched
    const snapshot = await api.getSessionSnapshot('s1');

    // Then the snake_case flags map onto the app spelling unchanged
    expect(snapshot.pendingQuestions[0].questions[0]).toMatchObject({
      multiSelect: false,
      allowOther: true,
    });
  });
});
