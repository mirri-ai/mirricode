/**
 * `WsRpcClient` — unified JSON-RPC 2.0 WebSocket client for the rust
 * server's `/api/v1/rpc` endpoint (ADR-001 — the TS-dialect frame layer is
 * retired, no dual protocol).
 *
 * Wire contract (mirrors `rust/crates/mirri-server/src/ws_rpc.rs` header
 * docs; frame shapes re-implemented from
 * `apps/mirri-web/src/api/daemon/json-rpc-client.ts` — pure reference, no
 * import):
 *   - Every frame is stamped `"jsonrpc":"2.0"`; request ids are integers.
 *   - Handshake (strict ordering): the server sends a `server_hello`
 *     notification immediately after the upgrade; the client answers with
 *     a `client_hello` REQUEST carrying `clientId` and
 *     `capabilities.reverseRpc: ["approval","question"]` (without the
 *     whitelist the server never initiates reverse RPC on this
 *     connection); the `{accepted:true}` response completes the handshake.
 *   - Engine events arrive as notifications whose method IS the event name
 *     (`event.turn.ended`, ...) with a flat params envelope
 *     (`sessionId`, `timestamp`, `volatile`, `seq`/`epoch` when durable,
 *     plus the camelCase payload fields merged in — see
 *     `event_projection::event_notification`).
 *   - Reverse RPC: the server sends requests with integer ids
 *     (`approval.ask`, `question.ask`); the client answers with a
 *     `{id, result}` response frame (`respond()` helper).
 *   - Heartbeat: the server probes with `ping` notifications — we answer
 *     `pong` echoing the nonce. Any inbound frame counts as liveness, so
 *     no client-side timer is needed for short-lived e2e runs.
 */
import { ulid } from 'ulid';
import { WebSocket } from 'ws';

/** A JSON-RPC error object echoed back on a failed `call`. */
export class RpcCallError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = 'RpcCallError';
  }
}

export interface RpcNotification {
  method: string;
  params: Record<string, unknown>;
}

export interface RpcRequest {
  id: number | string;
  method: string;
  params: Record<string, unknown>;
}

interface PendingCall {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
}

const HANDSHAKE_TIMEOUT_MS = 10_000;
const DEFAULT_EVENT_TIMEOUT_MS = 30_000;

/**
 * One connection carrying all three directions: forward RPC (`call`),
 * event stream (`onNotification` / `waitForEvent`), and reverse RPC
 * (`onRequest` / `respond`). Connection-only — no reconnect, no queueing;
 * the caller owns lifecycle.
 */
export class WsRpcClient {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, PendingCall>();

  private notificationHandlers = new Set<(n: RpcNotification) => void>();
  private requestHandlers = new Set<(req: RpcRequest) => void>();

  /** Events received before a matching waiter registered — queue, don't drop. */
  private readonly eventQueue: RpcNotification[] = [];
  private readonly eventWaiters: Array<{
    match: (n: RpcNotification) => boolean;
    resolve: (n: RpcNotification) => void;
    reject: (error: Error) => void;
    timer: NodeJS.Timeout;
  }> = [];

  private closed = false;

  constructor(readonly baseUrl: string) {}

  /** Full WS URL: `http(s)://host:port` → `ws(s)://host:port/api/v1/rpc`. */
  get url(): string {
    return `${this.baseUrl.replace(/^http/, 'ws')}/api/v1/rpc`;
  }

  /**
   * Open the socket and complete the `server_hello` / `client_hello`
   * handshake. Resolves on `{accepted:true}`.
   */
  async connect(): Promise<void> {
    if (this.ws) return;
    if (this.closed) throw new Error('WsRpcClient: already closed');
    await new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      ws.once('open', () => resolve());
      ws.once('error', (err) => {
        if (this.closed) return;
        reject(err instanceof Error ? err : new Error(String(err)));
      });
      ws.on('message', (data) =>
        this.handleMessage(
          typeof data === 'string' ? data : Buffer.from(data as ArrayBufferLike).toString('utf8'),
        ),
      );
      ws.on('close', () => this.handleClose());
    });

    // server_hello (notification) → client_hello (request) → {accepted:true}.
    await this.waitForNotification(
      (n) => n.method === 'server_hello',
      HANDSHAKE_TIMEOUT_MS,
    );
    const result = await this.call<{ accepted?: boolean }>('client_hello', {
      client_id: ulid(),
      capabilities: { reverseRpc: ['approval', 'question'] },
    });
    if (result.accepted !== true) {
      throw new Error(`WsRpcClient: client_hello not accepted: ${JSON.stringify(result)}`);
    }
  }

  /** Send a JSON-RPC request and await the matching response. */
  async call<T = unknown>(method: string, params: object): Promise<T> {
    const ws = this.requireSocket();
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
      });
      try {
        ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
      } catch (error) {
        this.pending.delete(id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  /**
   * `session.subscribe` — attach this connection to the session's event
   * stream. A `cursor` replays missed durable events (`seq > cursor.seq`
   * within the same `epoch`).
   */
  subscribe(
    sessionId: string,
    cursor?: { seq: number; epoch: string | number },
  ): Promise<unknown> {
    return this.call('session.subscribe', {
      sessionId,
      ...(cursor !== undefined ? { cursor } : undefined),
    });
  }

  /** `session.unsubscribe` — detach from the session's event stream. */
  unsubscribe(sessionId: string): Promise<unknown> {
    return this.call('session.unsubscribe', { sessionId });
  }

  /** Observe every notification (engine events, ping, ...). Single slot. */
  onNotification(handler: (n: RpcNotification) => void): void {
    this.notificationHandlers.add(handler);
  }

  /** Observe server→client reverse-RPC requests (approval.ask etc.). Single slot. */
  onRequest(handler: (req: RpcRequest) => void): void {
    this.requestHandlers.add(handler);
  }

  /**
   * Answer a reverse-RPC request with `{id, result}` (or `{id, error}` when
   * `error` is given).
   */
  respond(id: number | string, result: unknown): void {
    const ws = this.requireSocket();
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, result: result ?? null }));
  }

  /**
   * Wait for the first notification whose method equals `type` (e.g.
   * `'event.turn.ended'`) and whose `params.sessionId` matches. Events that
   * arrived before this call are queued — nothing is dropped. Rejects on
   * timeout (default 30s).
   */
  async waitForEvent(
    sessionId: string,
    type: string,
    timeoutMs = DEFAULT_EVENT_TIMEOUT_MS,
  ): Promise<Record<string, unknown>> {
    const notification = await this.waitForNotification(
      (n) => n.method === type && n.params['sessionId'] === sessionId,
      timeoutMs,
    );
    return notification.params;
  }

  /** Close the socket. Pending calls reject. Idempotent. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.removeAllListeners();
      try {
        ws.close();
      } catch {
        // already closing
      }
    }
    this.failPendingAndWaiters(new Error('WsRpcClient: connection closed'));
  }

  // ── internals ─────────────────────────────────────────────────────────────

  private requireSocket(): WebSocket {
    if (!this.ws || this.closed) {
      throw new Error('WsRpcClient: not connected — call `await connect()` first');
    }
    return this.ws;
  }

  /**
   * Wait for the first notification matching `match`, draining the queue
   * first so pre-wait events are never lost.
   */
  private waitForNotification(
    match: (n: RpcNotification) => boolean,
    timeoutMs: number,
  ): Promise<RpcNotification> {
    return new Promise((resolve, reject) => {
      for (let i = 0; i < this.eventQueue.length; i++) {
        const notification = this.eventQueue[i];
        if (notification === undefined) continue;
        if (match(notification)) {
          this.eventQueue.splice(i, 1);
          resolve(notification);
          return;
        }
      }
      if (this.closed) {
        reject(new Error('WsRpcClient: connection closed while waiting'));
        return;
      }
      const waiter = {
        match,
        resolve,
        reject,
        timer: setTimeout(() => {
          const idx = this.eventWaiters.indexOf(waiter);
          if (idx >= 0) this.eventWaiters.splice(idx, 1);
          reject(new Error(`WsRpcClient: timed out after ${timeoutMs}ms waiting for notification`));
        }, timeoutMs),
      };
      waiter.timer.unref?.();
      this.eventWaiters.push(waiter);
    });
  }

  private handleMessage(data: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch {
      return; // non-JSON frame — drop
    }
    if (Array.isArray(parsed)) {
      // Event batching: consecutive notifications coalesced into one frame.
      for (const frame of parsed) this.handleFrame(frame);
      return;
    }
    this.handleFrame(parsed);
  }

  /** Classify one frame: id+method → reverse request; id → response; method → notification. */
  private handleFrame(raw: unknown): void {
    if (typeof raw !== 'object' || raw === null) return;
    const frame = raw as Record<string, unknown>;
    const hasId = Object.prototype.hasOwnProperty.call(frame, 'id');
    const method = typeof frame['method'] === 'string' ? frame['method'] : undefined;

    if (hasId && method !== undefined) {
      // Server → client reverse RPC request.
      const req: RpcRequest = {
        id: frame['id'] as number | string,
        method,
        params: (frame['params'] as Record<string, unknown>) ?? {},
      };
      for (const handler of this.requestHandlers) {
        try {
          handler(req);
        } catch {
          // observation handler — swallow
        }
      }
      return;
    }
    if (hasId) {
      // Response to one of our requests.
      const id = typeof frame['id'] === 'number' ? frame['id'] : undefined;
      if (id === undefined) return;
      const pending = this.pending.get(id);
      if (pending === undefined) return;
      this.pending.delete(id);
      const error = frame['error'];
      if (error !== undefined && error !== null) {
        const { code, message } = error as { code?: number; message?: string };
        pending.reject(new RpcCallError(code ?? 0, message ?? 'unknown error', (error as { data?: unknown }).data));
        return;
      }
      pending.resolve(frame['result']);
      return;
    }
    if (method !== undefined) {
      this.handleNotification(method, (frame['params'] as Record<string, unknown>) ?? {});
    }
  }

  private handleNotification(method: string, params: Record<string, unknown>): void {
    if (method === 'ping') {
      // Liveness probe — echo the nonce back.
      this.ws?.send(JSON.stringify({ jsonrpc: '2.0', method: 'pong', params: { nonce: params['nonce'] ?? null } }));
      return;
    }
    const notification: RpcNotification = { method, params };
    for (const handler of this.notificationHandlers) {
      try {
        handler(notification);
      } catch {
        // observation handler — swallow
      }
    }
    // First waiter whose predicate matches consumes it; unmatched notifications
    // are queued for future waitForEvent calls.
    for (let i = 0; i < this.eventWaiters.length; i++) {
      const waiter = this.eventWaiters[i];
      if (waiter === undefined) continue;
      if (notification.test(waiter)) {
        this.eventWaiters.splice(i, 1);
        clearTimeout(waiter.timer);
        waiter.resolve(notification);
        return;
      }
    }
    this.eventQueue.push(notification);
  }

  private handleClose(): void {
    this.closed = true;
    this.ws = null;
    this.failPendingAndWaiters(new Error('WsRpcClient: connection closed by server'));
  }

  private failPendingAndWaiters(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
    for (const waiter of this.eventWaiters.splice(0)) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
  }
}
