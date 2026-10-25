// apps/mirri-web/src/api/daemon/json-rpc-client.ts
// JsonRpcClient — browser WebSocket client for the unified JSON-RPC 2.0
// protocol (mirri-server `/api/v1/rpc`, design doc
// `.tmp/rust-core-design/01-protocol-unification.md`).
//
// One connection carries all three directions:
//   1. forward RPC   — client → server request/response (`call`)
//   2. event stream  — server → client notifications (`on`)
//   3. reverse RPC   — server → client requests the client must answer
//                      (`registerReverseRpc`)
//
// The wire contract below was written against the ACTUAL server
// implementation (rust/crates/mirri-server/src/ws_rpc.rs + json_rpc.rs):
//   - request/response ids are integers (`i64`), echoed verbatim;
//   - every frame is stamped `"jsonrpc":"2.0"`;
//   - handshake: server_hello (notification) → client_hello (request) →
//     `{"accepted": true}` response, within 10s;
//   - events arrive as `{"method":"event","params":{"type","sessionId","seq",
//     "epoch","volatile","payload"}}`; multiple events may be coalesced into
//     one array frame (batch);
//   - heartbeat: `ping` notification with a `nonce`, answered with `pong`;
//     any inbound frame counts as liveness — a connection silent for
//     3 × heartbeatMs is dead.
//   - server may send requests with integer ids (reverse RPC); the client
//     answers `{id, result}` or `{id, error}`.
//
// This class is intentionally connection-only: it does not reconnect or queue
// commands. Callers decide lifecycle policy (see WsRpcApi.connectEvents).

import { traceWsLifecycle } from '../../debug/trace';

// ---------------------------------------------------------------------------
// Wire types (protocol §4)
// ---------------------------------------------------------------------------

/** A JSON-RPC 2.0 request frame (client → server, or server → client for
 *  reverse RPC). Client-issued ids are integers — the server codec rejects
 *  string ids (`"request ids must be integers"`). */
export interface JsonRpcRequest {
  id?: number;
  method: string;
  params?: unknown;
}

/** A JSON-RPC 2.0 response frame. Exactly one of `result`/`error` is present. */
export interface JsonRpcResponse {
  id: number;
  result?: unknown;
  error?: JsonRpcError;
}

/** The JSON-RPC 2.0 error object (protocol §4.2). `data` may carry the
 *  camelCase fields `errorId`, `domainCode`, `retryAfterMs`, `details`. */
export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

/** A JSON-RPC 2.0 notification frame — no id, never answered. */
export interface JsonRpcNotification {
  method: string;
  params?: unknown;
}

/** An event received as a notification with method `"event"`; the `type` is
 *  extracted from `params.type` (protocol §7.0 event envelope). */
export interface RpcEvent {
  /** Event name, e.g. "assistant.delta", "turn.ended". */
  type: string;
  /** Owner session, when the event is session-scoped. */
  sessionId?: string;
  /** Durable events carry a per-session monotonic cursor; volatile events do not. */
  seq?: number;
  /** Journal generation marker — a changed epoch invalidates old cursors. */
  epoch?: string;
  /** true = volatile (not journaled, not replayable after reconnect). */
  volatile?: boolean;
  /** Event-specific fields (the notification params minus the envelope keys). */
  payload: unknown;
}

/** The `server_hello` notification the server sends immediately after the
 *  WebSocket upgrade (protocol §5.2). */
export interface ServerHello {
  serverId: string;
  version: string;
  /** Suggested client heartbeat interval in ms (falls back to 15000). */
  heartbeatMs: number;
  /** Per-connection durable event ring-buffer capacity (cursor replay window). */
  maxEventBufferSize: number;
  capabilities: {
    eventBatching?: boolean;
  };
}

/** Success response of the `client_hello` request (protocol §5.3). */
export interface ClientHelloResult {
  accepted: boolean;
}

/** Diagnostic snapshot of the underlying socket — mirrors
 *  DaemonEventSocket.health() so callers get one consistent shape. */
export interface RpcClientHealth {
  connected: boolean;
  open: boolean;
  stale: boolean;
}

export interface JsonRpcClientOptions {
  /**
   * Hard liveness deadline: when no frame at all has arrived for this many ms,
   * the connection is declared dead and closed. Defaults to
   * 3 × the server-advertised heartbeat interval (protocol §5.5).
   */
  heartbeatTimeout?: number;
  /**
   * WebSocket constructor override (tests inject a mock here; the browser
   * default is `new WebSocket(url, protocols)`).
   */
  wsFactory?: (url: string, protocols: string[]) => WebSocket;
  /** Connection-state observer (fires true after the handshake completes). */
  onStateChange?: (connected: boolean) => void;
  /** Transport-level failure observer (parse errors, protocol errors). */
  onError?: (code: number, message: string, fatal: boolean) => void;
}

/** Error surfaced when the server answers a request with an error object. */
export class JsonRpcCallError extends Error {
  readonly code: number;
  readonly data?: unknown;

  constructor(error: JsonRpcError) {
    super(error.message);
    this.name = 'JsonRpcCallError';
    this.code = error.code;
    this.data = error.data;
  }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const WS_OPEN = 1; // WebSocket.OPEN — literal so node tests need no DOM global
const HANDSHAKE_TIMEOUT_MS = 10_000; // server drops us after 10s without client_hello
const HEARTBEAT_DEFAULT_MS = 15_000;
const HEARTBEAT_TIMEOUT_MULTIPLIER = 3;
const STALE_SOCKET_FLOOR_MS = 30_000;
const PARSE_ERROR_CODE = -32700;
const METHOD_NOT_FOUND_CODE = -32601;
const REVERSE_HANDLER_FAILURE_CODE = -32603;
/** JSON-RPC error code the engine returns for a missing resource — the
 *  generic "not found" (see RpcError::NotFound in
 *  rust/crates/mirri-server/src/rpc.rs; shared by session/agent/provider/
 *  model not-found). */
export const NOT_FOUND_CODE = -32001;
/** Connection-level torn down by the client (staleness, close). */
const CLOSE_CODE_ABNORMAL = 4000;

/** Reverse-RPC method prefixes this client declares itself able to answer
 *  (protocol §5.3 `capabilities.reverseRpc`). The server must not initiate
 *  reverse RPC outside this whitelist. */
const REVERSE_RPC_ALLOWLIST = ['approval', 'question'] as const;

/** Result of one pending `call()`; stored in the id-keyed map. */
interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

/** Fire-and-forget ping the client sends at the heartbeat interval. */
interface PingNonce {
  nonce: string;
}

// ---------------------------------------------------------------------------
// JsonRpcClient
// ---------------------------------------------------------------------------

export class JsonRpcClient {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending: Map<number, PendingRequest> = new Map();
  private eventHandlers: Map<string, Set<(event: RpcEvent) => void>> = new Map();
  private anyEventHandlers: Set<(event: RpcEvent) => void> = new Set();
  private reverseRpcHandlers: Map<string, (params: unknown) => Promise<unknown>> = new Map();

  /** Resolves when the handshake completes; null while idle or after close(). */
  private connected: Promise<void> | null = null;
  private isConnected = false;

  /** Id of the in-flight client_hello request (drives handshake completion). */
  private helloId: number | null = null;
  private handshakeDeferred: { resolve: () => void; reject: (reason: Error) => void } | null = null;
  private handshakeTimer: ReturnType<typeof setTimeout> | null = null;

  /** Server-advertised heartbeat interval; falls back to the protocol default. */
  private heartbeatMs = HEARTBEAT_DEFAULT_MS;
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  /** Epoch ms of the most recent inbound frame (any frame proves liveness). */
  private lastFrameAt = 0;

  /** Stable client instance id, reused across reconnects (protocol §5.3). */
  private readonly clientId: string;

  constructor(
    private url: string,
    private protocols: string[], // Sec-WebSocket-Protocol, i.e. `mirri-code.bearer.<token>`
    private options?: JsonRpcClientOptions,
  ) {
    this.clientId = createClientId();
  }

  /**
   * Connect, perform the server_hello/client_hello handshake, and resolve
   * when the connection is ready. Safe to call again after close() to
   * reconnect; concurrent callers share one handshake.
   */
  connect(): Promise<void> {
    if (this.connected !== null) return this.connected;
    this.connected = this.performConnect();
    return this.connected;
  }

  /** Open the WebSocket and run the handshake. */
  private async performConnect(): Promise<void> {
    // Tear down a leftover socket (e.g. a silent-half-open one) so connect()
    // doubles as reconnect().
    this.teardownSocket('connect');

    this.lastFrameAt = Date.now();
    traceWsLifecycle('connect', { url: this.url, attempt: 'manual' });
    const ws = this.options?.wsFactory
      ? this.options.wsFactory(this.url, this.protocols)
      : new WebSocket(this.url, this.protocols);
    this.ws = ws;

    try {
      await new Promise<void>((resolve, reject) => {
        this.handshakeDeferred = { resolve, reject };
        this.handshakeTimer = setTimeout(() => {
          this.failHandshake(
            new Error(
              `JsonRpcClient: handshake timed out after ${HANDSHAKE_TIMEOUT_MS}ms ` +
                `(server_hello/client_hello not completed)`,
            ),
          );
        }, HANDSHAKE_TIMEOUT_MS);

        ws.onopen = () => {
          // Not ready yet — the handshake only completes on the client_hello
          // response; the server sends server_hello right after the upgrade.
          traceWsLifecycle('open');
        };

        ws.onmessage = (ev: MessageEvent) => {
          // Any inbound frame proves the link is alive.
          this.lastFrameAt = Date.now();
          this.handleMessage(String(ev.data));
        };

        ws.onerror = () => {
          // The browser does not expose error details; the close event (with
          // its code/reason) follows immediately and drives cleanup.
          traceWsLifecycle('error');
          this.options?.onError?.(0, 'JsonRpcClient: WebSocket error', false);
          if (!this.isConnected) {
            this.failHandshake(new Error('JsonRpcClient: WebSocket error during handshake'));
          }
        };

        ws.onclose = (ev?: CloseEvent) => {
          traceWsLifecycle(
            'close',
            ev ? { code: ev.code, reason: ev.reason, wasClean: ev.wasClean } : undefined,
          );
          this.handleClosed(ev);
        };
      });
    } catch (error) {
      // Failed handshake: clear state so a later connect() retries cleanly.
      this.connected = null;
      throw error;
    }
  }

  /**
   * Send a JSON-RPC request and await the matching response. Rejects with
   * {@link JsonRpcCallError} on a server error response, or an Error when the
   * transport is closed. There is deliberately no per-request timeout:
   * long-lived methods (e.g. `message.send` in wait mode) legitimately block
   * server-side; dead links are caught by the heartbeat.
   */
  async call<T = unknown>(method: string, params?: unknown): Promise<T> {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) {
      throw new Error(`JsonRpcClient: not connected — cannot call '${method}'. Call connect() first.`);
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
      });
      try {
        this.wsSend(ws, {
          jsonrpc: '2.0',
          id,
          method,
          ...(params !== undefined ? { params } : {}),
        });
      } catch (error) {
        this.pending.delete(id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  /** Send a notification (no id — the server never answers). Silently dropped
   *  while not connected, matching the fire-and-forget contract. */
  notify(method: string, params?: unknown): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) return;
    this.wsSend(ws, {
      jsonrpc: '2.0',
      method,
      ...(params !== undefined ? { params } : {}),
    });
  }

  /** Subscribe an event handler; returns the unsubscribe function. */
  on(eventType: string, handler: (event: RpcEvent) => void): () => void {
    let handlers = this.eventHandlers.get(eventType);
    if (!handlers) {
      handlers = new Set();
      this.eventHandlers.set(eventType, handlers);
    }
    handlers.add(handler);
    return () => {
      handlers.delete(handler);
    };
  }

  /** Subscribe to ALL events regardless of type; returns unsubscribe. */
  onAnyEvent(handler: (event: RpcEvent) => void): () => void {
    this.anyEventHandlers.add(handler);
    return () => { this.anyEventHandlers.delete(handler); };
  }

  /**
   * Register a handler for a server-initiated (reverse RPC) method. When the
   * server sends `{id, method, params}`, the handler runs and its resolution
   * is answered with `{id, result}`; a rejection is answered with
   * `{id, error}`. Re-registering a method replaces the previous handler.
   */
  registerReverseRpc(method: string, handler: (params: unknown) => Promise<unknown>): void {
    this.reverseRpcHandlers.set(method, handler);
  }

  /** Close the connection. All in-flight calls reject; no reconnect is
   *  attempted (the caller owns lifecycle). */
  close(): void {
    this.teardownSocket('close');
  }

  /** Snapshot the socket's health. `stale` mirrors DaemonEventSocket: no
   *  inbound frame for 2 × heartbeatMs (floored), even though the browser may
   *  still report OPEN on a half-open connection. */
  health(): RpcClientHealth {
    const open = this.ws !== null && this.ws.readyState === WS_OPEN;
    const threshold = Math.max(this.heartbeatMs * 2, STALE_SOCKET_FLOOR_MS);
    const stale = this.lastFrameAt > 0 && Date.now() - this.lastFrameAt > threshold;
    return { connected: this.isConnected, open, stale };
  }

  // -------------------------------------------------------------------------
  // Handshake
  // -------------------------------------------------------------------------

  private handleServerHello(params: unknown): void {
    const hello = (params ?? {}) as Partial<ServerHello>;
    if (typeof hello.heartbeatMs === 'number' && hello.heartbeatMs > 0) {
      this.heartbeatMs = hello.heartbeatMs;
    }
    if (!this.isConnected && this.helloId === null) {
      this.sendClientHello();
    }
  }

  /** Send the client_hello request; its response completes the handshake. */
  private sendClientHello(): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) return;
    const id = this.nextId++;
    this.helloId = id;
    this.pending.set(id, {
      // The client_hello response is the handshake-completion signal.
      resolve: () => this.completeHandshake(),
      reject: (error: Error) => this.failHandshake(error),
    });
    this.wsSend(ws, {
      jsonrpc: '2.0',
      id,
      method: 'client_hello',
      params: {
        clientId: this.clientId,
        clientType: 'web',
        capabilities: { reverseRpc: [...REVERSE_RPC_ALLOWLIST] },
      },
    });
  }

  private completeHandshake(): void {
    if (this.isConnected) return;
    this.isConnected = true;
    this.helloId = null;
    this.clearHandshakeTimer();
    const deferred = this.handshakeDeferred;
    this.handshakeDeferred = null;
    traceWsLifecycle('handshake-complete', { heartbeatMs: this.heartbeatMs });
    this.startHeartbeat();
    deferred?.resolve();
    this.options?.onStateChange?.(true);
  }

  private failHandshake(error: Error): void {
    if (this.helloId !== null) {
      this.pending.delete(this.helloId);
      this.helloId = null;
    }
    this.clearHandshakeTimer();
    const deferred = this.handshakeDeferred;
    this.handshakeDeferred = null;
    if (deferred) {
      this.teardownSocket('handshake-failed');
      deferred.reject(error);
    }
    // If the socket is already gone there is no handshake to fail — the
    // teardown path already rejected it.
  }

  private clearHandshakeTimer(): void {
    if (this.handshakeTimer !== null) {
      clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
    }
  }

  // -------------------------------------------------------------------------
  // Heartbeat
  // -------------------------------------------------------------------------

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.heartbeatTimer = setInterval(() => this.heartbeatTick(), this.heartbeatMs);
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer !== null) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private heartbeatTick(): void {
    if (!this.isConnected || this.ws === null) return;
    // Hard liveness deadline: no frame at all within the timeout (protocol
    // §5.5: 3 × heartbeat interval) → the link is presumed half-open. The
    // socket is closed; onclose drives the pending-rejection + state-change
    // cleanup.
    const hardTimeoutMs =
      this.options?.heartbeatTimeout ?? this.heartbeatMs * HEARTBEAT_TIMEOUT_MULTIPLIER;
    if (Date.now() - this.lastFrameAt > hardTimeoutMs) {
      traceWsLifecycle('stale', { hardTimeoutMs });
      this.options?.onError?.(
        0,
        `JsonRpcClient: no frames received within ${hardTimeoutMs}ms; closing connection`,
        true,
      );
      try {
        this.ws.close(CLOSE_CODE_ABNORMAL, 'heartbeat timeout');
      } catch {
        // The socket may already be closing.
      }
      return;
    }
    this.sendPing();
  }

  private sendPing(): void {
    const nonce = createNonce();
    this.notify('ping', { nonce } satisfies PingNonce);
  }

  private sendPong(params: unknown): void {
    const nonce = (params as Partial<PingNonce> | null | undefined)?.nonce;
    this.notify('pong', { nonce: nonce ?? null });
  }

  // -------------------------------------------------------------------------
  // Inbound frames
  // -------------------------------------------------------------------------

  private handleMessage(data: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch (error) {
      this.options?.onError?.(PARSE_ERROR_CODE, `Failed to parse JSON-RPC frame: ${String(error)}`, false);
      return;
    }
    // Batching (protocol §4.1.4): the server may coalesce a run of
    // notifications into one array frame after negotiating eventBatching.
    if (Array.isArray(parsed)) {
      for (const frame of parsed) this.handleFrame(frame);
      return;
    }
    this.handleFrame(parsed);
  }

  /** Route one parsed frame by the single JSON-RPC classification rule
   *  (protocol §2.1): has id + method → reverse request; has id → response to
   *  our request; has method only → notification. */
  private handleFrame(raw: unknown): void {
    if (typeof raw !== 'object' || raw === null) return;
    const frame = raw as Record<string, unknown>;
    const hasId = Object.prototype.hasOwnProperty.call(frame, 'id');
    const method = typeof frame['method'] === 'string' ? (frame['method'] as string) : undefined;
    const id = typeof frame['id'] === 'number' ? (frame['id'] as number) : undefined;

    if (hasId && method !== undefined && id !== undefined) {
      this.handleReverseRequest(id, method, frame['params']);
      return;
    }
    if (hasId && id !== undefined) {
      this.handleResponse(id, frame);
      return;
    }
    if (method !== undefined) {
      this.handleNotification(method, frame['params']);
    }
    // Structurally invalid id-less junk: dropped silently (notification rule).
  }

  /** A response to one of our requests (`{id, result|error}`). */
  private handleResponse(id: number, frame: Record<string, unknown>): void {
    const pending = this.pending.get(id);
    if (!pending) return; // stale response (post-close, duplicate) — ignore
    this.pending.delete(id);
    if (id === this.helloId) this.helloId = null;

    const error = frame['error'];
    if (error !== undefined && error !== null) {
      pending.reject(new JsonRpcCallError(error as JsonRpcError));
      return;
    }
    pending.resolve(frame['result']);
  }

  /** A server-initiated request — answer with `{id, result}` via the
   *  registered handler, or `{id, error: -32601}` when unregistered. */
  private handleReverseRequest(id: number, method: string, params: unknown): void {
    const handler = this.reverseRpcHandlers.get(method);
    if (!handler) {
      this.notifyReverseError(id, {
        code: METHOD_NOT_FOUND_CODE,
        message: `method not found: ${method}`,
      });
      return;
    }
    void Promise.resolve()
      .then(() => handler(params))
      .then((result) => this.sendReverseResult(id, result))
      .catch((error: unknown) => {
        this.notifyReverseError(id, reverseHandlerError(error));
      });
  }

  private sendReverseResult(id: number, result: unknown): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) return; // client is gone — drop
    this.wsSend(ws, { jsonrpc: '2.0', id, result: result ?? null });
  }

  private notifyReverseError(id: number, error: JsonRpcError): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WS_OPEN) return;
    this.wsSend(ws, {
      jsonrpc: '2.0',
      id,
      error: { code: error.code, message: error.message, ...(error.data !== undefined ? { data: error.data } : {}) },
    });
  }

  private handleNotification(method: string, params: unknown): void {
    switch (method) {
      case 'server_hello':
        this.handleServerHello(params);
        return;
      case 'ping':
        // The server probes liveness with its own nonce — answer immediately.
        this.sendPong(params);
        return;
      case 'pong':
        // Liveness is tracked per-frame in onmessage; nothing else to do.
        return;
      case 'event': {
        const event = extractEventEnvelope(params);
        if (event !== null) this.dispatchEvent(event);
        return;
      }
      case 'error': {
        const error = (params ?? {}) as Partial<JsonRpcError>;
        this.options?.onError?.(
          typeof error.code === 'number' ? error.code : 0,
          typeof error.message === 'string' ? error.message : 'JsonRpcClient: server error notification',
          false,
        );
        return;
      }
      default:
        // Protocol §7.0 alternative form: the notification method IS the
        // event name (`turn.started`, ...). Dispatch it the same way.
        this.dispatchEvent(extractDirectEvent(method, params));
        return;
    }
  }

  private dispatchEvent(event: RpcEvent): void {
    for (const handler of this.anyEventHandlers) handler(event);
    const handlers = this.eventHandlers.get(event.type);
    if (!handlers || handlers.size === 0) return;
    for (const handler of handlers) {
      handler(event);
    }
  }

  // -------------------------------------------------------------------------
  // Teardown
  // -------------------------------------------------------------------------

  /** Close the socket (client-initiated) or handle a server-side close. */
  private handleClosed(ev?: CloseEvent): void {
    const wasConnected = this.isConnected;
    this.ws = null;
    this.isConnected = false;
    // A dropped connection must not short-circuit a later connect() — reset so
    // the caller can reconnect on the same instance.
    this.connected = null;
    this.stopHeartbeat();
    this.clearHandshakeTimer();

    const reason = ev
      ? `WebSocket closed (code ${ev.code}${ev.reason ? `, ${ev.reason}` : ''})`
      : 'WebSocket closed';
    this.rejectAllPending(new Error(reason));

    if (this.handshakeDeferred) {
      const deferred = this.handshakeDeferred;
      this.handshakeDeferred = null;
      deferred.reject(new Error(reason));
    }

    if (wasConnected) {
      this.options?.onStateChange?.(false);
    }
  }

  /** Close the socket ourselves; state is torn down inline instead of waiting
   *  for the (detached) onclose handler. */
  private teardownSocket(reason: string): void {
    this.stopHeartbeat();
    this.clearHandshakeTimer();
    const ws = this.ws;
    this.ws = null;

    this.isConnected = false;
    this.connected = null;

    this.rejectAllPending(
      new Error(`JsonRpcClient: connection closed (${reason})`),
    );
    if (this.handshakeDeferred) {
      const deferred = this.handshakeDeferred;
      this.handshakeDeferred = null;
      deferred.reject(new Error(`JsonRpcClient: connection closed (${reason})`));
    }
    if (this.helloId !== null) {
      this.pending.delete(this.helloId);
      this.helloId = null;
    }

    if (ws !== null) {
      // Detach so the browser's eventual onclose cannot double-run cleanup.
      ws.onopen = null;
      ws.onmessage = null;
      ws.onerror = null;
      ws.onclose = null;
      try {
        if (ws.readyState === WS_OPEN) ws.close(1000, reason);
      } catch {
        // The socket may already be closing.
      }
    }
    traceWsLifecycle('close', { reason });
  }

  private rejectAllPending(error: Error): void {
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
  }

  private wsSend(ws: WebSocket, frame: unknown): void {
    ws.send(JSON.stringify(frame));
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Build the RpcEvent for the `{"method":"event","params":{...}}` form: the
 *  envelope keys (type/sessionId/seq/epoch/volatile) are lifted out of params.
 *  The server ships the event body under a dedicated `payload` key; a flat
 *  params object (no `payload` key) is treated as the body itself. */
function extractEventEnvelope(params: unknown): RpcEvent | null {
  if (typeof params !== 'object' || params === null) return null;
  const fields = params as Record<string, unknown>;
  const type = typeof fields['type'] === 'string' ? (fields['type'] as string) : undefined;
  if (type === undefined || type === '') return null;
  const envelopeKeys = ['type', 'sessionId', 'seq', 'epoch', 'volatile'];
  const hasPayloadKey = Object.prototype.hasOwnProperty.call(fields, 'payload');
  return {
    type,
    sessionId: typeof fields['sessionId'] === 'string' ? (fields['sessionId'] as string) : undefined,
    seq: typeof fields['seq'] === 'number' ? (fields['seq'] as number) : undefined,
    epoch: typeof fields['epoch'] === 'string' ? (fields['epoch'] as string) : undefined,
    volatile: fields['volatile'] === true,
    payload: hasPayloadKey ? fields['payload'] : stripKeys(fields, envelopeKeys),
  };
}

/** Build the RpcEvent for the design-doc §7.0 form where the notification
 *  method itself is the event name and its params carry the envelope. */
function extractDirectEvent(method: string, params: unknown): RpcEvent {
  if (typeof params !== 'object' || params === null) {
    return { type: method, payload: params ?? null };
  }
  const fields = params as Record<string, unknown>;
  return {
    type: method,
    sessionId: typeof fields['sessionId'] === 'string' ? (fields['sessionId'] as string) : undefined,
    seq: typeof fields['seq'] === 'number' ? (fields['seq'] as number) : undefined,
    epoch: typeof fields['epoch'] === 'string' ? (fields['epoch'] as string) : undefined,
    volatile: fields['volatile'] === true,
    payload: stripKeys(fields, ['sessionId', 'seq', 'epoch', 'volatile']),
  };
}

/** Copy `fields` without the given keys (the event envelope). */
function stripKeys(fields: Record<string, unknown>, keys: string[]): Record<string, unknown> {
  const rest: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (!keys.includes(key)) rest[key] = value;
  }
  return rest;
}

/** Turn a rejected reverse-RPC handler into a wire error object: a
 *  JsonRpcCallError thrown by the handler keeps its code; anything else is an
 *  internal error (-32603). */
function reverseHandlerError(error: unknown): JsonRpcError {
  if (error instanceof JsonRpcCallError) {
    return { code: error.code, message: error.message, ...(error.data !== undefined ? { data: error.data } : {}) };
  }
  return {
    code: REVERSE_HANDLER_FAILURE_CODE,
    message: error instanceof Error ? error.message : String(error),
  };
}

function createClientId(): string {
  return `web_${globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2)}`;
}

function createNonce(): string {
  return globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2);
}