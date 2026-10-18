// apps/mirri-web/src/api/daemon/json-rpc-client.test.ts
// JsonRpcClient unit tests against a scriptable MockWebSocket. The test runs
// in the node env (no DOM WebSocket), so every client is built with the
// wsFactory option; the mock replays the server side of the protocol:
// server_hello → client_hello → {accepted} handshake, responses, events,
// reverse-RPC requests, pings and closes.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { JsonRpcCallError, JsonRpcClient } from './json-rpc-client';
import type { JsonRpcClientOptions, RpcEvent } from './json-rpc-client';

// ---------------------------------------------------------------------------
// Mock WebSocket — implements the DOM WebSocket surface the client touches
// ---------------------------------------------------------------------------

class MockWebSocket {
  static instances: MockWebSocket[] = [];

  readonly url: string;
  readonly protocols: string[];
  readyState = 0; // CONNECTING
  sent: string[] = [];

  onopen: ((ev: Event) => unknown) | null = null;
  onmessage: ((ev: MessageEvent) => unknown) | null = null;
  onerror: ((ev: Event) => unknown) | null = null;
  onclose: ((ev: CloseEvent) => unknown) | null = null;

  constructor(url: string, protocols?: string[]) {
    this.url = url;
    this.protocols = protocols ?? [];
    MockWebSocket.instances.push(this);
  }

  // --- client side (the JsonRpcClient calls these) ---

  send(data: string): void {
    this.sent.push(data);
  }

  close(code = 1000, reason = ''): void {
    this.readyState = 3;
    // A real browser fires onclose after close(); the client relies on this
    // for the staleness path, and detaches handlers before its own close().
    this.onclose?.({ code, reason, wasClean: code === 1000 } as CloseEvent);
  }

  // --- test controls (the test plays the server) ---

  open(): void {
    this.readyState = 1;
    this.onopen?.(new Event('open'));
  }

  serverSend(frame: unknown): void {
    this.receiveText(JSON.stringify(frame));
  }

  serverSendRaw(text: string): void {
    this.receiveText(text);
  }

  serverClose(code = 1000, reason = ''): void {
    this.readyState = 3;
    this.onclose?.({ code, reason, wasClean: code === 1000 } as CloseEvent);
  }

  serverError(): void {
    this.onerror?.(new Event('error'));
  }

  sentFrames(): Array<Record<string, unknown>> {
    return this.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
  }

  sentFramesSince(index: number): Array<Record<string, unknown>> {
    return this.sentFrames().slice(index);
  }

  lastSent(): Record<string, unknown> | undefined {
    const last = this.sent.at(-1);
    return last === undefined ? undefined : (JSON.parse(last) as Record<string, unknown>);
  }

  private receiveText(text: string): void {
    this.onmessage?.({ data: text } as MessageEvent);
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function serverHelloFrame(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    jsonrpc: '2.0',
    method: 'server_hello',
    params: {
      serverId: 'test-server',
      version: '0.0.0-test',
      heartbeatMs: 15_000,
      maxEventBufferSize: 256,
      capabilities: { eventBatching: true },
      ...overrides,
    },
  };
}

let mock: MockWebSocket;

function makeClient(
  options: JsonRpcClientOptions = {},
  protocols: string[] = [],
): JsonRpcClient {
  return new JsonRpcClient('ws://test/rpc', protocols, {
    ...options,
    wsFactory: (url: string, wsProtocols: string[]) => {
      mock = new MockWebSocket(url, wsProtocols);
      return mock as unknown as WebSocket;
    },
  });
}

/** Drive the handshake to completion: open, server_hello, client_hello, ack. */
async function connectClient(client: JsonRpcClient): Promise<void> {
  const connecting = client.connect();
  mock.open();
  expect(mock.readyState).toBe(1); // the socket must exist and be open
  mock.serverSend(serverHelloFrame());
  const hello = mock.sentFrames().find((frame) => frame['method'] === 'client_hello');
  expect(hello).toBeDefined();
  expect(hello?.['id']).toBeTypeOf('number');
  mock.serverSend({ jsonrpc: '2.0', id: hello?.['id'], result: { accepted: true } });
  await connecting;
}

function findFrame(frames: Array<Record<string, unknown>>, method: string): Record<string, unknown> | undefined {
  return frames.find((frame) => frame['method'] === method);
}

beforeEach(() => {
  MockWebSocket.instances = [];
});

afterEach(() => {
  for (const instance of MockWebSocket.instances) {
    instance.sent.length = 0;
  }
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('JsonRpcClient handshake', () => {
  it('should connect and complete the handshake', async () => {
    const stateChanges: boolean[] = [];
    const client = makeClient({ onStateChange: (connected) => stateChanges.push(connected) });
    const connecting = client.connect();

    mock.open();
    mock.serverSend(serverHelloFrame());
    const hello = findFrame(mock.sentFrames(), 'client_hello');
    expect(hello).toBeDefined();
    expect(hello?.['jsonrpc']).toBe('2.0');
    expect(hello?.['id']).toBeTypeOf('number');
    expect(hello?.['params']).toMatchObject({
      clientId: expect.any(String),
      clientType: 'web',
      capabilities: { reverseRpc: ['approval', 'question'] },
    });

    mock.serverSend({ jsonrpc: '2.0', id: hello?.['id'], result: { accepted: true } });
    await connecting;

    expect(stateChanges).toEqual([true]);
    expect(client.health()).toEqual({ connected: true, open: true, stale: false });
    client.close();
  });

  it('should reject the handshake when the client_hello answer carries an error', async () => {
    const client = makeClient();
    const connecting = client.connect();

    mock.open();
    mock.serverSend(serverHelloFrame());
    const hello = findFrame(mock.sentFrames(), 'client_hello');
    mock.serverSend({
      jsonrpc: '2.0',
      id: hello?.['id'],
      error: { code: -32002, message: 'unauthorized' },
    });

    await expect(connecting).rejects.toMatchObject({ code: -32002, message: 'unauthorized' });
    // A failed handshake leaves the client re-connectable, not half-open.
    await expect(client.call('session.list')).rejects.toThrow(/not connected/);
  });

  it('should pass the bearer subprotocol to the WebSocket constructor', async () => {
    const client = makeClient({}, ['mirri-code.bearer.secret-token']);
    const connecting = client.connect();
    expect(connecting).toBeInstanceOf(Promise);
    // The factory created the socket with the given protocols.
    expect(mock.protocols).toEqual(['mirri-code.bearer.secret-token']);
    mock.serverClose(4000, 'teardown');
    await expect(connecting).rejects.toThrow(/closed/);
    client.close();
  });
});

describe('JsonRpcClient call', () => {
  it('should send a request frame and resolve with the response result', async () => {
    const client = makeClient();
    await connectClient(client);

    const response = client.call('session.get', { session_id: 'sess-1' });
    const request = mock.lastSent();
    expect(request).toMatchObject({ jsonrpc: '2.0', method: 'session.get' });
    expect(request?.['id']).toBeTypeOf('number');
    expect(request?.['params']).toEqual({ session_id: 'sess-1' });

    mock.serverSend({
      jsonrpc: '2.0',
      id: request?.['id'],
      result: { id: 'sess-1', title: 'T', createdAt: '2026-01-01T00:00:00Z', updatedAt: '2026-01-01T00:00:00Z', agentCount: 1, tags: [] },
    });
    await expect(response).resolves.toMatchObject({ id: 'sess-1', title: 'T' });
    client.close();
  });

  it('should reject with JsonRpcCallError when the server answers an error', async () => {
    const client = makeClient();
    await connectClient(client);

    const response = client.call<unknown>('session.get', { session_id: 'missing' });
    const request = mock.lastSent();
    mock.serverSend({
      jsonrpc: '2.0',
      id: request?.['id'],
      error: { code: -32001, message: 'session not found: missing', data: { errorId: 'uuid-1' } },
    });

    await expect(response).rejects.toBeInstanceOf(JsonRpcCallError);
    await expect(response).rejects.toMatchObject({ code: -32001, data: { errorId: 'uuid-1' } });
    client.close();
  });

  it('should reject immediately when not connected', async () => {
    const client = makeClient();
    await expect(client.call('session.list')).rejects.toThrow(/not connected/);
  });
});

describe('JsonRpcClient events', () => {
  it('should dispatch "event" notifications to registered handlers', async () => {
    const client = makeClient();
    await connectClient(client);

    const seen: RpcEvent[] = [];
    const unsubscribe = client.on('assistant.delta', (event) => seen.push(event));

    mock.serverSend({
      jsonrpc: '2.0',
      method: 'event',
      params: {
        type: 'assistant.delta',
        sessionId: 'sess-1',
        seq: 3,
        epoch: 'epoch-1',
        volatile: true,
        payload: { textDelta: 'hel', done: false },
      },
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]).toEqual({
      type: 'assistant.delta',
      sessionId: 'sess-1',
      seq: 3,
      epoch: 'epoch-1',
      volatile: true,
      payload: { textDelta: 'hel', done: false },
    });

    // Unsubscribe stops delivery.
    unsubscribe();
    mock.serverSend({
      jsonrpc: '2.0',
      method: 'event',
      params: { type: 'assistant.delta', volatile: true, payload: { textDelta: 'more' } },
    });
    expect(seen).toHaveLength(1);
    client.close();
  });

  it('should dispatch the direct-form notifications (method IS the event name)', async () => {
    const client = makeClient();
    await connectClient(client);

    const seen: RpcEvent[] = [];
    client.on('turn.ended', (event) => seen.push(event));

    mock.serverSend({
      jsonrpc: '2.0',
      method: 'turn.ended',
      params: { sessionId: 'sess-1', seq: 5, epoch: 'epoch-1', stopReason: 'end_turn' },
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]).toEqual({
      type: 'turn.ended',
      sessionId: 'sess-1',
      seq: 5,
      epoch: 'epoch-1',
      volatile: false,
      payload: { stopReason: 'end_turn' },
    });
    client.close();
  });

  it('should handle batched array frames', async () => {
    const client = makeClient();
    await connectClient(client);

    const seen: string[] = [];
    client.on('config.changed', (event) => seen.push(String(event.payload)));
    client.on('session.updated', (event) => seen.push(String(event.payload)));

    mock.serverSendRaw(
      JSON.stringify([
        { jsonrpc: '2.0', method: 'event', params: { type: 'config.changed', volatile: false, payload: { changedFields: ['models'] } } },
        { jsonrpc: '2.0', method: 'event', params: { type: 'session.updated', sessionId: 'sess-1', seq: 1, volatile: false, payload: { title: 'X' } } },
      ]),
    );
    expect(seen).toEqual(['[object Object]', '[object Object]']);
    client.close();
  });
});

describe('JsonRpcClient reverse RPC', () => {
  it('should answer a server request with the handler result', async () => {
    const client = makeClient();
    await connectClient(client);

    client.registerReverseRpc('fs.readTextFile', async (params) => {
      expect(params).toEqual({ path: '/tmp/a.txt' });
      return { content: 'hello' };
    });

    mock.serverSend({ jsonrpc: '2.0', id: 42, method: 'fs.readTextFile', params: { path: '/tmp/a.txt' } });
    await flushMicrotasks();

    expect(mock.lastSent()).toEqual({ jsonrpc: '2.0', id: 42, result: { content: 'hello' } });
    client.close();
  });

  it('should answer an unregistered method with -32601', async () => {
    const client = makeClient();
    await connectClient(client);

    mock.serverSend({ jsonrpc: '2.0', id: 43, method: 'approval.ask', params: {} });
    await flushMicrotasks();

    expect(mock.lastSent()).toMatchObject({
      jsonrpc: '2.0',
      id: 43,
      error: { code: -32601, message: 'method not found: approval.ask' },
    });
    client.close();
  });

  it('should answer a rejected handler with -32603 and the error message', async () => {
    const client = makeClient();
    await connectClient(client);

    client.registerReverseRpc('question.ask', async () => {
      throw new Error('cannot answer right now');
    });

    mock.serverSend({ jsonrpc: '2.0', id: 44, method: 'question.ask', params: {} });
    await flushMicrotasks();

    expect(mock.lastSent()).toMatchObject({
      jsonrpc: '2.0',
      id: 44,
      error: { code: -32603, message: 'cannot answer right now' },
    });
    client.close();
  });
});

describe('JsonRpcClient connection errors', () => {
  it('should reject the handshake when the server closes before it completes', async () => {
    const client = makeClient();
    const connecting = client.connect();
    mock.open();
    await flushMicrotasks();
    mock.serverClose(4000, 'no client_hello in time');
    await expect(connecting).rejects.toThrow(/closed/);
    client.close();
  });

  it('should reject in-flight calls when the connection drops', async () => {
    const stateChanges: boolean[] = [];
    const client = makeClient({ onStateChange: (connected) => stateChanges.push(connected) });
    await connectClient(client);

    const response = client.call('session.get', { session_id: 'sess-1' });
    mock.serverClose(1006, 'gone');
    await expect(response).rejects.toThrow(/closed \(code 1006/);
    expect(stateChanges).toEqual([true, false]);
    client.close();
  });

  it('should reject the handshake on a WebSocket error', async () => {
    const client = makeClient();
    const connecting = client.connect();
    mock.open();
    mock.serverError();
    await expect(connecting).rejects.toThrow(/WebSocket error during handshake/);
    client.close();
  });
});

describe('JsonRpcClient heartbeat', () => {
  it('should answer a server ping with a pong echoing the nonce', async () => {
    const client = makeClient();
    await connectClient(client);

    mock.serverSend({ jsonrpc: '2.0', method: 'ping', params: { nonce: 'server-nonce-1' } });
    expect(mock.lastSent()).toEqual({ jsonrpc: '2.0', method: 'pong', params: { nonce: 'server-nonce-1' } });
    client.close();
  });

  it('should send ping notifications and stay alive while pongs arrive', async () => {
    vi.useFakeTimers();
    const client = makeClient();
    // 500ms heartbeat; the handshake is timer-free (driven by explicit calls).
    const connecting = client.connect();
    mock.open();
    mock.serverSend(serverHelloFrame({ heartbeatMs: 500 }));
    const hello = findFrame(mock.sentFrames(), 'client_hello');
    mock.serverSend({ jsonrpc: '2.0', id: hello?.['id'], result: { accepted: true } });
    await connecting;
    // Only the handshake frames (client_hello) were sent so far; pings start
    // after the negotiated interval elapses.
    const sentBefore = mock.sent.length;

    for (let tick = 0; tick < 8; tick += 1) {
      vi.advanceTimersByTime(500);
      const ping = findFrame(mock.sentFramesSince(sentBefore), 'ping');
      expect(ping).toBeDefined();
      expect(ping?.['params']).toMatchObject({ nonce: expect.any(String) });
      // The server answers each ping — every inbound frame counts as liveness.
      mock.serverSend({ jsonrpc: '2.0', method: 'pong', params: ping?.['params'] });
    }

    expect(client.health().connected).toBe(true);
    expect(mock.readyState).toBe(1);
    vi.useRealTimers();
    client.close();
  });

  it('should close the connection when no frame arrives within the heartbeat timeout', async () => {
    vi.useFakeTimers();
    const stateChanges: boolean[] = [];
    const client = makeClient({ heartbeatTimeout: 1200, onStateChange: (connected) => stateChanges.push(connected) });

    const connecting = client.connect();
    mock.open();
    mock.serverSend(serverHelloFrame({ heartbeatMs: 400 }));
    const hello = findFrame(mock.sentFrames(), 'client_hello');
    mock.serverSend({ jsonrpc: '2.0', id: hello?.['id'], result: { accepted: true } });
    await connecting;

    // 3 × 400ms = 1200ms hard deadline; the client silently drops the dead link.
    vi.advanceTimersByTime(400);
    vi.advanceTimersByTime(400);
    vi.advanceTimersByTime(400);
    expect(client.health().connected).toBe(true); // 1200ms is not > 1200ms yet
    vi.advanceTimersByTime(400); // 1600ms — past the deadline, socket closed

    expect(stateChanges).toEqual([true, false]);
    expect(mock.readyState).toBe(3);
    expect(client.health().connected).toBe(false);
    vi.useRealTimers();
    client.close();
  });
});

// ---------------------------------------------------------------------------
// Helper: flush the microtask queue (reverse-RPC handler chains, ...)
// ---------------------------------------------------------------------------

async function flushMicrotasks(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}