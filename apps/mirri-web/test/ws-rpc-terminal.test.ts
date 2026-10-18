// apps/mirri-web/test/ws-rpc-terminal.test.ts
// Given the rust engine's session-scoped PTY pool (the `terminal_*`
// connection-local WS-RPC methods, the `terminal_output` / `terminal_exit`
// notifications, and the `/sessions/{id}/terminals` REST routes), the
// WsRpcApi terminal bridge must carry the v1 multiplexed-endpoint frame
// shapes over the unified transport: attach/input/resize/detach/close send
// exactly the snake_case payloads the server deserializes, notifications
// route to the onTerminalOutput/onTerminalExit handlers with the seq cursor
// lifted into the envelope, and the REST CRUD maps the snake_case wire onto
// the app-facing AppTerminal.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import { DaemonApiError } from '../src/api/errors';
import type { RpcEvent } from '../src/api/daemon/json-rpc-client';
import type { MirriEventHandlers } from '../src/api/types';

/** The JsonRpcClient.call signature the adapters under test rely on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type EventHandler = (event: RpcEvent) => void;

interface MockClient {
  call: ReturnType<typeof vi.fn<RpcCall>>;
  on: ReturnType<typeof vi.fn<(type: string, handler: EventHandler) => () => void>>;
  onAnyEvent: ReturnType<typeof vi.fn<(handler: EventHandler) => () => void>>;
  close: ReturnType<typeof vi.fn>;
}

/**
 * Build a WsRpcApi whose transport is a mocked client. Every
 * `client.on(type, handler)` registration is recorded so the tests can
 * dispatch notifications straight into the handlers connectEvents installed
 * (the real JsonRpcClient's extractDirectEvent runs before these handlers —
 * the events built here mirror what it produces from the server's frames).
 */
function createApi(): {
  api: WsRpcApi;
  call: MockClient['call'];
  client: MockClient;
  handlerFor: (type: string) => EventHandler;
} {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const registrations: Array<[string, EventHandler]> = [];
  const client: MockClient = {
    // Fire-and-forget control frames chain `.catch` immediately — the mock
    // must resolve by default.
    call: vi.fn<RpcCall>().mockResolvedValue(undefined),
    on: vi.fn((type: string, handler: EventHandler) => {
      registrations.push([type, handler]);
      return () => {};
    }),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  (api as unknown as { client: unknown }).client = client;
  const handlerFor = (type: string): EventHandler => {
    const registration = registrations.find(([registered]) => registered === type);
    if (registration === undefined) {
      throw new Error(`connectEvents did not install a handler for '${type}'`);
    }
    return registration[1];
  };
  return { api, call: client.call, client, handlerFor };
}

function eventHandlers(overrides: Partial<MirriEventHandlers> = {}): MirriEventHandlers {
  return {
    onEvent: vi.fn(),
    onResync: vi.fn(),
    onError: vi.fn(),
    onConnectionChange: vi.fn(),
    ...overrides,
  };
}

/** One `terminal_output` notification as extractDirectEvent shapes it from
 * the server frame `{method: "terminal_output", params: {session_id,
 * terminal_id, seq, data}}`: the seq is lifted into the envelope, the
 * snake_case body stays in the payload, and sessionId is undefined (the
 * server sends `session_id`, not `sessionId`). */
function terminalOutputEvent(
  payload: Record<string, unknown>,
  seq?: number,
): RpcEvent {
  return {
    type: 'terminal_output',
    sessionId: undefined,
    seq,
    epoch: undefined,
    volatile: false,
    payload,
  };
}

function terminalExitEvent(payload: Record<string, unknown>): RpcEvent {
  return { type: 'terminal_exit', sessionId: undefined, seq: undefined, epoch: undefined, volatile: false, payload };
}

/** A running terminal as the rust REST routes serialize it (snake_case). */
function rpcTerminal(overrides: Partial<{
  id: string;
  session_id: string;
  cwd: string;
  shell: string;
  cols: number;
  rows: number;
  created_at: string;
}> = {}) {
  return {
    id: overrides.id ?? 'term_1',
    session_id: overrides.session_id ?? 's1',
    cwd: overrides.cwd ?? 'src',
    shell: overrides.shell ?? '/bin/zsh',
    cols: overrides.cols ?? 80,
    rows: overrides.rows ?? 24,
    status: 'running' as const,
    created_at: overrides.created_at ?? '2026-02-01T00:00:00Z',
  };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// ---------------------------------------------------------------------------
// Notification routing (terminal_output / terminal_exit)
// ---------------------------------------------------------------------------

describe('WsRpcApi terminal notification routing', () => {
  it('should route a terminal_output notification to onTerminalOutput with the payload ids, data and envelope seq', () => {
    const { api, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    const handlers = eventHandlers({ onTerminalOutput });
    api.connectEvents(handlers);

    handlerFor('terminal_output')(
      terminalOutputEvent(
        { session_id: 's1', terminal_id: 'term_1', data: 'echo hi\r\n' },
        7,
      ),
    );

    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'echo hi\r\n', 7);
  });

  it('should default the seq cursor to 0 when the notification carries none', () => {
    const { api, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalOutput }));

    handlerFor('terminal_output')(
      terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'x' }),
    );

    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'x', 0);
  });

  it('should drop a terminal_output notification whose ids cannot be read', () => {
    const { api, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalOutput }));

    // No terminal_id in the payload and no sessionId in the envelope.
    handlerFor('terminal_output')(terminalOutputEvent({ session_id: 's1', data: 'x' }, 1));
    // Malformed payload entirely.
    handlerFor('terminal_output')(terminalOutputEvent({}, 2));

    expect(onTerminalOutput).not.toHaveBeenCalled();
  });

  it('should route a terminal_exit notification to onTerminalExit with the exit code', () => {
    const { api, handlerFor } = createApi();
    const onTerminalExit = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalExit }));

    handlerFor('terminal_exit')(
      terminalExitEvent({ session_id: 's1', terminal_id: 'term_1', exit_code: 3 }),
    );

    expect(onTerminalExit).toHaveBeenCalledWith('s1', 'term_1', 3);
  });

  it('should surface a signal-death exit (null exit_code) and tolerate a missing code as null', () => {
    const { api, handlerFor } = createApi();
    const onTerminalExit = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalExit }));

    handlerFor('terminal_exit')(
      terminalExitEvent({ session_id: 's1', terminal_id: 'term_1', exit_code: null }),
    );
    handlerFor('terminal_exit')(
      terminalExitEvent({ session_id: 's1', terminal_id: 'term_1' }),
    );

    expect(onTerminalExit).toHaveBeenNthCalledWith(1, 's1', 'term_1', null);
    expect(onTerminalExit).toHaveBeenNthCalledWith(2, 's1', 'term_1', null);
  });
});

// ---------------------------------------------------------------------------
// Terminal stream control (connection-local methods)
// ---------------------------------------------------------------------------

describe('WsRpcApi terminal stream control', () => {
  it('should attach with terminal_attach, including since_seq only for a positive cursor', () => {
    const { api, call } = createApi();
    const connection = api.connectEvents(eventHandlers());

    connection.terminalAttach('s1', 'term_1', 42);
    connection.terminalAttach('s1', 'term_1', 0);
    connection.terminalAttach('s1', 'term_1');

    expect(call).toHaveBeenNthCalledWith(1, 'terminal_attach', {
      session_id: 's1',
      terminal_id: 'term_1',
      since_seq: 42,
    });
    expect(call).toHaveBeenNthCalledWith(2, 'terminal_attach', {
      session_id: 's1',
      terminal_id: 'term_1',
    });
    expect(call).toHaveBeenNthCalledWith(3, 'terminal_attach', {
      session_id: 's1',
      terminal_id: 'term_1',
    });
  });

  it('should write input and resize through the snake_case terminal_input / terminal_resize frames', () => {
    const { api, call } = createApi();
    const connection = api.connectEvents(eventHandlers());

    connection.terminalInput('s1', 'term_1', 'ls -la\n');
    connection.terminalResize('s1', 'term_1', 120, 40);

    expect(call).toHaveBeenCalledWith('terminal_input', {
      session_id: 's1',
      terminal_id: 'term_1',
      data: 'ls -la\n',
    });
    expect(call).toHaveBeenCalledWith('terminal_resize', {
      session_id: 's1',
      terminal_id: 'term_1',
      cols: 120,
      rows: 40,
    });
  });

  it('should detach and close through terminal_detach / terminal_close', () => {
    const { api, call } = createApi();
    const connection = api.connectEvents(eventHandlers());

    connection.terminalDetach('s1', 'term_1');
    connection.terminalClose('s1', 'term_1');

    expect(call).toHaveBeenCalledWith('terminal_detach', {
      session_id: 's1',
      terminal_id: 'term_1',
    });
    expect(call).toHaveBeenCalledWith('terminal_close', {
      session_id: 's1',
      terminal_id: 'term_1',
    });
  });

  it('should surface a failed control frame through the connection error handler instead of throwing', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('not attached'));
    const handlers = eventHandlers();
    const connection = api.connectEvents(handlers);

    connection.terminalInput('s1', 'term_1', 'x');

    await vi.waitFor(() => {
      expect(handlers.onError).toHaveBeenCalledWith(0, expect.stringContaining('terminal_input'), false);
    });
  });
});

// ---------------------------------------------------------------------------
// Terminal stream gap detection (F-P3-4 — seq monotonicity)
// ---------------------------------------------------------------------------

describe('WsRpcApi terminal stream gap detection', () => {
  it('should drop a re-delivered terminal_output whose seq regressed to or below the cursor', () => {
    const { api, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalOutput }));

    // First frame initializes the cursor at 7.
    handlerFor('terminal_output')(
      terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'f7' }, 7),
    );
    // An exact re-delivery (replay racing the replacement attach)…
    handlerFor('terminal_output')(
      terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'f7' }, 7),
    );
    // …and a stale regression from an old sink.
    handlerFor('terminal_output')(
      terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'stale' }, 5),
    );

    expect(onTerminalOutput).toHaveBeenCalledTimes(1);
    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'f7', 7);
  });

  it('should re-attach at the cursor and splice the missing frames in order when a gap is detected', () => {
    const { api, call, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    api.connectEvents(eventHandlers({ onTerminalOutput }));
    const dispatch = (data: string, seq: number): void => {
      handlerFor('terminal_output')(
        terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data }, seq),
      );
    };

    // In-order frame, then a jump 7 → 12: frames 8-11 were lost (the
    // server's outbox filled). The gap frame is held, not forwarded.
    dispatch('f7', 7);
    dispatch('f12-live', 12);
    expect(onTerminalOutput).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('terminal_attach', {
      session_id: 's1',
      terminal_id: 'term_1',
      since_seq: 7,
    });

    // The recovery replay re-delivers 8-11 and then the gap frame itself.
    dispatch('f8', 8);
    dispatch('f9', 9);
    dispatch('f10', 10);
    dispatch('f11', 11);
    dispatch('f12-replay', 12);

    // Then the client saw one strictly in-order sequence; the held live
    // copy of 12 was dropped in favour of the replay copy.
    expect(onTerminalOutput.mock.calls.map((c) => c[3])).toEqual([7, 8, 9, 10, 11, 12]);
    expect(onTerminalOutput).toHaveBeenLastCalledWith('s1', 'term_1', 'f12-replay', 12);

    // And the stream is healthy again: the next frame streams normally.
    dispatch('f13', 13);
    expect(onTerminalOutput).toHaveBeenLastCalledWith('s1', 'term_1', 'f13', 13);
  });

  it('should flush the held frame and continue when the gap recovery cannot be filled', () => {
    vi.useFakeTimers();
    try {
      const { api, call, handlerFor } = createApi();
      const onTerminalOutput = vi.fn();
      api.connectEvents(eventHandlers({ onTerminalOutput }));
      const dispatch = (data: string, seq: number): void => {
        handlerFor('terminal_output')(
          terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data }, seq),
        );
      };

      dispatch('f7', 7);
      dispatch('f12', 12);
      expect(onTerminalOutput).toHaveBeenCalledTimes(1);
      expect(call).toHaveBeenCalledWith('terminal_attach', {
        session_id: 's1',
        terminal_id: 'term_1',
        since_seq: 7,
      });

      // No replay ever arrives (the server's bounded buffer evicted the
      // missing frames). After the recovery window the held frame flushes
      // — degraded but live.
      vi.advanceTimersByTime(5000);
      expect(onTerminalOutput).toHaveBeenLastCalledWith('s1', 'term_1', 'f12', 12);

      // The stream continues from the new cursor without re-suppression.
      dispatch('f13', 13);
      expect(onTerminalOutput).toHaveBeenLastCalledWith('s1', 'term_1', 'f13', 13);
    } finally {
      vi.useRealTimers();
    }
  });

  it('should stop tracking a terminal stream on detach so a fresh attach replays cleanly', () => {
    const { api, handlerFor } = createApi();
    const onTerminalOutput = vi.fn();
    const connection = api.connectEvents(eventHandlers({ onTerminalOutput }));
    const dispatch = (data: string, seq: number): void => {
      handlerFor('terminal_output')(
        terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data }, seq),
      );
    };

    dispatch('f7', 7);
    connection.terminalDetach('s1', 'term_1');
    // A frame that raced the detach arrives afterwards: with tracking
    // cleared it must be forwarded (a re-attach replays from its own
    // cursor) rather than suppressed as a regression.
    dispatch('late', 3);
    expect(onTerminalOutput).toHaveBeenCalledTimes(2);
    expect(onTerminalOutput).toHaveBeenLastCalledWith('s1', 'term_1', 'late', 3);
  });
});

// ---------------------------------------------------------------------------
// Terminal REST CRUD (/sessions/{id}/terminals)
// ---------------------------------------------------------------------------

/** Stub global fetch with a queued JSON response carrying the rust envelope. */
function stubFetch(body: unknown, ok = true, status = 200): ReturnType<typeof vi.fn> {
  const fetchMock = vi.fn().mockResolvedValue({
    ok,
    status,
    json: async () => body,
  });
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

describe('WsRpcApi terminal REST', () => {
  it('Given a body-carrying terminal POST, When the request goes out, Then it carries Content-Type: application/json (axum Json rejects a 415 without it)', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({
      code: 0,
      msg: 'ok',
      data: rpcTerminal(),
    });

    await api.createTerminal('s1', { cwd: 'src' });

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(init?.method).toBe('POST');
    expect((init?.headers as Record<string, string>)['Content-Type']).toBe('application/json');
  });

  it('Given a body-carrying terminal close POST, When the request goes out, Then it also carries the JSON Content-Type', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({ code: 0, msg: 'ok', data: { closed: true } });

    await api.closeTerminal('s1', 'term_1');

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(init?.method).toBe('POST');
    expect((init?.headers as Record<string, string>)['Content-Type']).toBe('application/json');
  });

  it('Given a bodyless GET, When the request goes out, Then no Content-Type is forced', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({
      code: 0,
      msg: 'ok',
      data: { items: [rpcTerminal()] },
    });

    await api.listTerminals('s1');

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    const headers = (init?.headers ?? {}) as Record<string, string>;
    expect(Object.keys(headers).some((name) => name.toLowerCase() === 'content-type')).toBe(false);
  });

  it('should list terminals through GET /sessions/{id}/terminals and map the wire onto AppTerminal', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({
      code: 0,
      msg: 'ok',
      data: { items: [rpcTerminal()] },
    });

    const terminals = await api.listTerminals('s1');

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit | undefined];
    expect(url).toBe('http://test.local/api/v1/sessions/s1/terminals');
    expect(init?.method).toBeUndefined(); // GET
    expect(terminals).toEqual([
      {
        id: 'term_1',
        sessionId: 's1',
        cwd: 'src',
        shell: '/bin/zsh',
        cols: 80,
        rows: 24,
        status: 'running',
        createdAt: '2026-02-01T00:00:00Z',
        exitedAt: undefined,
        exitCode: undefined,
      },
    ]);
  });

  it('should create a terminal through POST /sessions/{id}/terminals carrying the requested options', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({
      code: 0,
      msg: 'ok',
      data: rpcTerminal({ cwd: 'rust', cols: 120, rows: 30 }),
    });

    const terminal = await api.createTerminal('s1', { cwd: 'rust', cols: 120, rows: 30 });

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://test.local/api/v1/sessions/s1/terminals');
    expect(init?.method).toBe('POST');
    expect(JSON.parse(init?.body as string)).toEqual({
      cwd: 'rust',
      shell: undefined,
      cols: 120,
      rows: 30,
    });
    expect(terminal).toMatchObject({ id: 'term_1', sessionId: 's1', cwd: 'rust', cols: 120, rows: 30, status: 'running' });
  });

  it('should fetch one terminal through GET /sessions/{id}/terminals/{tid}', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({
      code: 0,
      msg: 'ok',
      data: rpcTerminal({ id: 'term_9' }),
    });

    const terminal = await api.getTerminal('s1', 'term_9');

    const [url] = fetchMock.mock.calls[0] as [string, RequestInit | undefined];
    expect(url).toBe('http://test.local/api/v1/sessions/s1/terminals/term_9');
    expect(terminal).toMatchObject({ id: 'term_9', sessionId: 's1' });
  });

  it('should close a terminal through the /close subpath (axum cannot express the v1 :close suffix)', async () => {
    const { api } = createApi();
    const fetchMock = stubFetch({ code: 0, msg: 'ok', data: { closed: true } });

    const result = await api.closeTerminal('s1', 'term_1');

    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://test.local/api/v1/sessions/s1/terminals/term_1/close');
    expect(init?.method).toBe('POST');
    expect(result).toEqual({ closed: true });
  });

  it('should map an exited terminal with its exit code and timestamp', async () => {
    const { api } = createApi();
    stubFetch({
      code: 0,
      msg: 'ok',
      data: {
        ...rpcTerminal(),
        status: 'exited' as const,
        exit_code: 0,
        exited_at: '2026-02-01T00:01:00Z',
      },
    });

    const terminal = await api.getTerminal('s1', 'term_1');

    expect(terminal).toMatchObject({
      status: 'exited',
      exitCode: 0,
      exitedAt: '2026-02-01T00:01:00Z',
    });
  });

  it('should throw the shared DaemonApiError when the envelope carries a non-zero code', async () => {
    const { api } = createApi();
    stubFetch({ code: -32000, msg: 'terminal not found', data: null });

    await expect(api.getTerminal('s1', 'term_missing')).rejects.toBeInstanceOf(DaemonApiError);
  });

  it('should tolerate an empty items list when the session has no terminals', async () => {
    const { api } = createApi();
    stubFetch({ code: 0, msg: 'ok', data: { items: [] } });

    expect(await api.listTerminals('s1')).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// Additive terminal binding (connectTerminalEvents)
// ---------------------------------------------------------------------------

describe('WsRpcApi connectTerminalEvents', () => {
  it('should dispatch terminal notifications to the additive sinks without rebinding the app handlers', () => {
    const { api, handlerFor } = createApi();
    const appHandlers = eventHandlers();
    api.connectEvents(appHandlers);

    const onTerminalOutput = vi.fn();
    const onTerminalExit = vi.fn();
    const binding = api.connectTerminalEvents({ onTerminalOutput, onTerminalExit });
    expect(binding).toBeDefined();

    handlerFor('terminal_output')(terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'hi' }, 5));
    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'hi', 5);

    // The app's handlers stay bound for the non-terminal stream (the
    // regression this guards: a second connectEvents() call used to REPLACE
    // the app's handler set, freezing the chat event stream).
    handlerFor('resync_required')({ type: 'resync_required', sessionId: 's1', seq: 7, epoch: undefined, volatile: false, payload: null });
    expect(appHandlers.onResync).toHaveBeenCalledWith('s1', 7, undefined);
  });

  it('should bind inert handlers first when the terminal binding arrives before the app', () => {
    const { api, handlerFor } = createApi();

    const onTerminalOutput = vi.fn();
    api.connectTerminalEvents({ onTerminalOutput });

    // The terminal stream still dispatches (connectTerminalEvents bound the
    // no-op handlers that install the subscriptions + transport).
    handlerFor('terminal_output')(terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'early' }, 1));
    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'early', 1);

    // And a later app binding does not steal the terminal stream.
    const appHandlers = eventHandlers({ onTerminalOutput: vi.fn() });
    api.connectEvents(appHandlers);
    handlerFor('terminal_output')(terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'later' }, 2));
    expect(onTerminalOutput).toHaveBeenCalledWith('s1', 'term_1', 'later', 2);
    expect(appHandlers.onTerminalOutput).not.toHaveBeenCalled();
  });

  it('should release only the terminal sinks on binding close, leaving the transport open', () => {
    const { api, client, handlerFor } = createApi();
    api.connectEvents(eventHandlers());

    const onTerminalOutput = vi.fn();
    const binding = api.connectTerminalEvents({ onTerminalOutput });
    binding.close();

    handlerFor('terminal_output')(terminalOutputEvent({ session_id: 's1', terminal_id: 'term_1', data: 'after' }, 3));
    expect(onTerminalOutput).not.toHaveBeenCalled();
    // The shared transport survives the terminal binding's teardown.
    expect(client.close).not.toHaveBeenCalled();
  });

  it('should route terminal control-frame failures to the sinks when bound', async () => {
    const { api, call } = createApi();
    api.connectEvents(eventHandlers());

    const onError = vi.fn();
    const binding = api.connectTerminalEvents({ onTerminalOutput: vi.fn(), onTerminalExit: vi.fn(), onError });
    call.mockRejectedValueOnce(new Error('boom'));

    binding.terminalInput('s1', 'term_1', 'x');

    // The control frame's rejection settles in a microtask (fire-and-forget).
    await vi.waitFor(() => {
      expect(onError).toHaveBeenCalledWith(0, expect.stringContaining('terminal_input'), false);
    });
  });
});
