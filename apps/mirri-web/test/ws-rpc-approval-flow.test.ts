// apps/mirri-web/test/ws-rpc-approval-flow.test.ts
// Given the rust engine's HITL surfaces (the `approval.ask` / `question.ask`
// reverse RPCs and the camelCase resolution events), the WsRpcApi approval
// flow must present the ask as a card, answer the exact ask the card came
// from, fall back to `approval.decide` / `question.answer` when no ask is
// parked (treating the first-one-wins gate's NotFound answer as idempotent
// success), and clear cards + parked asks when resolution events arrive.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { JsonRpcCallError, JsonRpcClient } from '../src/api/daemon/json-rpc-client';
import type { RpcEvent } from '../src/api/daemon/json-rpc-client';
import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import { ParkedAskRegistry } from '../src/api/daemon/ws-rpc-event-bridge';
import { routeRpcEventToAppEvents } from '../src/api/daemon/ws-rpc-event-bridge';
import { createAgentProjector } from '../src/api/daemon/agentEventProjector';
import type { MirriEventHandlers } from '../src/api/types';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function rpcEvent(type: string, payload: unknown, sessionId = 's1', seq = 1): RpcEvent {
  return { type, sessionId, seq, epoch: undefined, volatile: false, payload };
}

/** The JsonRpcClient.call signature the adapters under test rely on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type AskHandler = (params: unknown) => Promise<unknown>;

interface MockClient {
  call: ReturnType<typeof vi.fn<RpcCall>>;
  on: ReturnType<typeof vi.fn<(type: string, handler: (event: RpcEvent) => void) => () => void>>;
  onAnyEvent: ReturnType<typeof vi.fn<(handler: (event: RpcEvent) => void) => () => void>>;
  close: ReturnType<typeof vi.fn>;
}

/**
 * Build a WsRpcApi whose transport is a mocked client. The ask handlers are
 * registered in the constructor on the real JsonRpcClient, so they are
 * captured through a prototype spy before the client is swapped for the mock
 * — this also proves the registration happens without connectEvents(). The
 * spy is left in place (restored by the next beforeEach) so the recorded
 * calls stay readable for the whole test.
 */
function createApi(): { api: WsRpcApi; client: MockClient; ask: (method: string) => AskHandler } {
  const spy = vi.spyOn(JsonRpcClient.prototype, 'registerReverseRpc');
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const client: MockClient = {
    call: vi.fn<RpcCall>(),
    on: vi.fn(() => () => {}),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  (api as unknown as { client: unknown }).client = client;
  const ask = (method: string): AskHandler => {
    const registration = spy.mock.calls.find(([registered]) => registered === method);
    if (registration === undefined) {
      throw new Error(`WsRpcApi did not register a reverse-RPC handler for '${method}'`);
    }
    return registration[1];
  };
  return { api, client, ask };
}

function eventHandlers(): MirriEventHandlers {
  return {
    onEvent: vi.fn(),
    onResync: vi.fn(),
    onError: vi.fn(),
    onConnectionChange: vi.fn(),
  };
}

/** The catch-all handler connectEvents installed on the mocked client. */
function anyEventHandler(client: MockClient): (event: RpcEvent) => void {
  const registration = client.onAnyEvent.mock.calls[0];
  if (registration === undefined) throw new Error('connectEvents did not install onAnyEvent');
  return registration[0];
}

beforeEach(() => {
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Resolution event shapes (rust camelCase vs TS daemon snake_case)
// ---------------------------------------------------------------------------

describe('rust resolution event shapes', () => {
  it('should map event.approval.resolved onto an approvalResolved card-clear event when the rust engine sends the camelCase payload', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.approval.resolved', { requestId: 'req-1', approved: true }),
    );
    expect(events).toEqual([
      expect.objectContaining({ type: 'approvalResolved', approvalId: 'req-1', decision: 'approved' }),
    ]);
  });

  it('should map a rejected event.approval.resolved when the rust engine answers approved:false', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.approval.resolved', { requestId: 'req-1', approved: false }),
    );
    expect(events).toEqual([
      expect.objectContaining({ type: 'approvalResolved', approvalId: 'req-1', decision: 'rejected' }),
    ]);
  });

  it('should keep reading the TS daemon snake_case approval.resolved shape when the v1 daemon emits it', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.approval.resolved', {
        approval_id: 'req-2',
        decision: 'cancelled',
        resolved_at: '2026-01-01T00:00:00.000Z',
      }),
    );
    expect(events).toEqual([
      expect.objectContaining({
        type: 'approvalResolved',
        approvalId: 'req-2',
        decision: 'cancelled',
        resolvedAt: '2026-01-01T00:00:00.000Z',
      }),
    ]);
  });

  it('should map event.approval.expired onto an approvalExpired event carrying the rust request id', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.approval.expired', { requestId: 'req-3' }),
    );
    expect(events).toEqual([expect.objectContaining({ type: 'approvalExpired', approvalId: 'req-3' })]);
  });

  it('should keep reading the TS daemon snake_case approval.expired shape', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.approval.expired', { approval_id: 'req-4' }),
    );
    expect(events).toEqual([expect.objectContaining({ type: 'approvalExpired', approvalId: 'req-4' })]);
  });

  it('should map event.question.answered onto a questionAnswered event carrying the rust request id', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.question.answered', { requestId: 'req-q1', answer: 'Rust' }),
    );
    expect(events).toEqual([expect.objectContaining({ type: 'questionAnswered', questionId: 'req-q1' })]);
  });

  it('should keep reading the TS daemon snake_case question.answered shape', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.question.answered', {
        question_id: 'req-q2',
        resolved_at: '2026-01-01T00:00:00.000Z',
      }),
    );
    expect(events).toEqual([
      expect.objectContaining({
        type: 'questionAnswered',
        questionId: 'req-q2',
        resolvedAt: '2026-01-01T00:00:00.000Z',
      }),
    ]);
  });

  it('should map event.question.dismissed onto a questionDismissed event carrying the rust request id', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.question.dismissed', { requestId: 'req-q3' }),
    );
    expect(events).toEqual([expect.objectContaining({ type: 'questionDismissed', questionId: 'req-q3' })]);
  });

  it('should keep reading the TS daemon snake_case question.dismissed shape', () => {
    const events = routeRpcEventToAppEvents(
      createAgentProjector(),
      rpcEvent('event.question.dismissed', {
        question_id: 'req-q4',
        dismissed_at: '2026-01-01T00:00:00.000Z',
      }),
    );
    expect(events).toEqual([
      expect.objectContaining({
        type: 'questionDismissed',
        questionId: 'req-q4',
        dismissedAt: '2026-01-01T00:00:00.000Z',
      }),
    ]);
  });
});

// ---------------------------------------------------------------------------
// WsRpcApi approval / question ask flow
// ---------------------------------------------------------------------------

describe('WsRpcApi approval ask flow', () => {
  it('should register the approval.ask and question.ask handlers at construction time before connectEvents binds handlers', () => {
    const spy = vi.spyOn(JsonRpcClient.prototype, 'registerReverseRpc');
    new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    expect(spy).toHaveBeenCalledWith('approval.ask', expect.any(Function));
    expect(spy).toHaveBeenCalledWith('question.ask', expect.any(Function));
    spy.mockRestore();
  });

  it('should surface an approval.ask as a card and answer the exact ask when the user responds', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    const askPromise = ask('approval.ask')({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      display: { kind: 'command', command: 'pnpm test' },
      request_id: 'req-9',
    });
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'approvalRequested',
        sessionId: 's1',
        approval: expect.objectContaining({ approvalId: 'req-9', toolName: 'bash' }),
      }),
      { sessionId: 's1', seq: 0 },
    );

    const result = await api.respondApproval('s1', 'req-9', { decision: 'rejected', feedback: 'not allowed' });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
    // The parked reverse-RPC ask resolves with the engine's wire shape.
    await expect(askPromise).resolves.toEqual({ decision: 'rejected', feedback: 'not allowed' });
    // Parked hit — the approval.decide fallback must not fire.
    expect(client.call).not.toHaveBeenCalled();
  });

  it('should carry the engine approval rule facet onto the approvalRequested event', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    const askPromise = ask('approval.ask')({
      tool_name: 'Bash',
      tool_call_id: 'call-1',
      action: 'execute Bash',
      display: { kind: 'command', command: 'true' },
      request_id: 'req-10',
      approval_rule: 'Bash',
    });
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'approvalRequested',
        approval: expect.objectContaining({ approvalId: 'req-10', approvalRule: 'Bash' }),
      }),
      { sessionId: 's1', seq: 0 },
    );
    await api.respondApproval('s1', 'req-10', { decision: 'approved' });
    await askPromise;
  });

  it('should leave approvalRule absent when the engine ask carries no facet', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    const askPromise = ask('approval.ask')({
      tool_name: 'Bash',
      tool_call_id: 'call-2',
      action: 'execute Bash',
      display: { kind: 'command', command: 'true' },
      request_id: 'req-11',
    });
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'approvalRequested',
        approval: expect.not.objectContaining({ approvalRule: expect.anything() }),
      }),
      { sessionId: 's1', seq: 0 },
    );
    await api.respondApproval('s1', 'req-11', { decision: 'approved' });
    await askPromise;
  });

  it('should surface a question.ask as a card and answer the exact ask when the user responds', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    const askPromise = ask('question.ask')({
      request_id: 'req-q9',
      questions: [{ question: 'Which language?', options: [{ label: 'Rust' }, { label: 'TypeScript' }] }],
    });
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'questionRequested',
        sessionId: 's1',
        question: expect.objectContaining({ questionId: 'req-q9' }),
      }),
      { sessionId: 's1', seq: 0 },
    );

    const result = await api.respondQuestion('s1', 'req-q9', {
      answers: { q0: { kind: 'single', optionId: 'Rust' } },
    });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
    await expect(askPromise).resolves.toEqual({ answers: { q0: 'Rust' } });
  });

  it('should fall back to approval.decide when no approval ask is parked', async () => {
    const { api, client } = createApi();
    client.call.mockResolvedValue({ ok: true });

    const result = await api.respondApproval('s1', 'req-1', { decision: 'rejected', feedback: 'not allowed' });
    expect(client.call).toHaveBeenCalledWith('approval.decide', {
      session_id: 's1',
      request_id: 'req-1',
      approved: false,
      reason: 'not allowed',
    });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
  });

  it('should pass the session scope through the approval.decide fallback', async () => {
    const { api, client } = createApi();
    client.call.mockResolvedValue({ ok: true });

    const result = await api.respondApproval('s1', 'req-1', {
      decision: 'approved',
      scope: 'session',
    });
    expect(client.call).toHaveBeenCalledWith('approval.decide', {
      session_id: 's1',
      request_id: 'req-1',
      approved: true,
      scope: 'session',
    });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
  });

  it('should treat approval.decide\'s NotFound answer as idempotent success when another client already decided', async () => {
    const { api, client } = createApi();
    client.call.mockRejectedValue(
      new JsonRpcCallError({ code: -32001, message: 'approval request not found' }),
    );

    const result = await api.respondApproval('s1', 'req-1', { decision: 'approved' });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
  });

  it('should still surface approval.decide transport errors other than NotFound', async () => {
    const { api, client } = createApi();
    client.call.mockRejectedValue(new JsonRpcCallError({ code: -32002, message: 'unauthorized' }));

    await expect(api.respondApproval('s1', 'req-1', { decision: 'approved' })).rejects.toThrow('unauthorized');
  });

  it('should fall back to question.answer when no question ask is parked', async () => {
    const { api, client } = createApi();
    client.call.mockResolvedValue({ ok: true });

    const result = await api.respondQuestion('s1', 'req-q1', {
      answers: { q0: { kind: 'single', optionId: 'Rust' } },
    });
    expect(client.call).toHaveBeenCalledWith('question.answer', {
      session_id: 's1',
      request_id: 'req-q1',
      answer: 'Rust',
    });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
  });

  it('should treat question.answer\'s NotFound answer as idempotent success when the ask was already resolved elsewhere', async () => {
    const { api, client } = createApi();
    client.call.mockRejectedValue(
      new JsonRpcCallError({ code: -32001, message: 'question request not found' }),
    );

    const result = await api.respondQuestion('s1', 'req-q1', { answers: {} });
    expect(result).toEqual({ resolved: true, resolvedAt: expect.any(String) });
  });

  it('should drop the parked approval ask when event.approval.expired arrives and fall back to approval.decide', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    const askPromise = ask('approval.ask')({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      request_id: 'req-9',
    });
    anyEventHandler(client)(rpcEvent('event.approval.expired', { requestId: 'req-9' }, 's1', 2));

    // The card clears through the (now dual-shape) approvalExpired mapping.
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'approvalExpired', sessionId: 's1', approvalId: 'req-9' }),
      { sessionId: 's1', seq: 2 },
    );

    client.call.mockResolvedValue({ ok: true });
    await api.respondApproval('s1', 'req-9', { decision: 'approved' });
    expect(client.call).toHaveBeenCalledWith('approval.decide', {
      session_id: 's1',
      request_id: 'req-9',
      approved: true,
    });
    // The dropped ask promise stays parked — the engine already timed it out.
    await expect(
      Promise.race([
        askPromise.then(() => 'resolved' as const),
        new Promise<'pending'>((resolve) => setTimeout(() => resolve('pending'), 20)),
      ]),
    ).resolves.toBe('pending');
  });

  it('should drop the parked approval ask when another client resolves it via event.approval.resolved', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    void ask('approval.ask')({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      request_id: 'req-9',
    });
    anyEventHandler(client)(rpcEvent('event.approval.resolved', { requestId: 'req-9', approved: true }, 's1', 2));
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'approvalResolved', sessionId: 's1', approvalId: 'req-9' }),
      { sessionId: 's1', seq: 2 },
    );

    client.call.mockResolvedValue({ ok: true });
    await api.respondApproval('s1', 'req-9', { decision: 'approved' });
    expect(client.call).toHaveBeenCalledWith('approval.decide', expect.anything());
  });

  it('should buffer ask cards that arrive before connectEvents and flush them when handlers bind', async () => {
    const { api, client, ask } = createApi();
    // No connectEvents yet — the ask arrives right after the handshake.
    const askPromise = ask('approval.ask')({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      request_id: 'req-early',
    });

    const handlers = eventHandlers();
    api.connectEvents(handlers);
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'approvalRequested',
        approval: expect.objectContaining({ approvalId: 'req-early' }),
      }),
      expect.anything(),
    );

    await api.respondApproval('s1', 'req-early', { decision: 'approved' });
    await expect(askPromise).resolves.toEqual({ decision: 'approved' });
    expect(client.call).not.toHaveBeenCalled();
  });

  it('should clear parked asks when the transport closes', async () => {
    const { api, client, ask } = createApi();
    const handlers = eventHandlers();
    api.connectEvents(handlers);
    anyEventHandler(client)(rpcEvent('event.turn.started', { turnId: 1 }, 's1', 1));

    void ask('approval.ask')({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      request_id: 'req-9',
    });
    api.close();

    client.call.mockResolvedValue({ ok: true });
    await api.respondApproval('s1', 'req-9', { decision: 'approved' });
    expect(client.call).toHaveBeenCalledWith('approval.decide', expect.anything());
    expect(client.close).toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// ParkedAskRegistry (the pure parked-map layer)
// ---------------------------------------------------------------------------

describe('ParkedAskRegistry', () => {
  it('should answer the first take and report the second as absent when an ask is taken twice', () => {
    const registry = new ParkedAskRegistry<string>();
    const respond = vi.fn();
    registry.park('req-1', respond);

    expect(registry.take('req-1')).toBe(respond);
    expect(registry.take('req-1')).toBeUndefined();
    expect(respond).not.toHaveBeenCalled();
  });

  it('should drop a parked entry on demand when resolution arrives elsewhere', () => {
    const registry = new ParkedAskRegistry<string>();
    registry.park('req-1', vi.fn());
    registry.drop('req-1');

    expect(registry.take('req-1')).toBeUndefined();
  });

  it('should clear every parked entry when the transport closes', () => {
    const registry = new ParkedAskRegistry<string>();
    registry.park('req-1', vi.fn());
    registry.park('req-2', vi.fn());
    registry.clear();

    expect(registry.take('req-1')).toBeUndefined();
    expect(registry.take('req-2')).toBeUndefined();
  });
});
