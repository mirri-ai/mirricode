// apps/mirri-web/test/snapshot-contract.e2e.test.ts
// Render-wiring contract e2e (plan Task 6, ADR-009): the REAL rust
// `mirri serve` (the prebuilt `rust/target/debug/mirri`) runs a real
// tool-calling turn against a scripted OpenAI-compatible
// FakeProviderServer, and the `session.snapshot` + `message.list` results
// must satisfy the shared protocol contract (`@mirri-ai/protocol`
// `sessionSnapshotResponseSchema`) — the final referee for the R1
// content-part projection (tool_use / tool_result / thinking / … at the
// rpc serialization boundary, ADR-007).
//
// Registered exemptions (ADR-009, docs/specs/conversation-rendering-architecture.md
// + docs/specs/conversation-rendering-context.md 已裁决后续项):
//   - `in_flight_turn.turn_id`: the rust wire mints the `turn-<uuid>`
//     STRING while the TS `inFlightTurnSchema` declares a number — relax
//     the whole field (`z.any().nullable()`), type unification is a
//     follow-up;
//   - nested `session`: the snapshot carries a slim camelCase view (usage /
//     agent_config / permission_rules / real timestamps are follow-ups);
//   - message rows: the camelCase row envelope (`{id, role, content,
//     agentId, createdAt, metadata?}` — the RpcWireMessage TS interface);
//     `createdAt` is a derived, order-faithful timestamp (follow-up
//     "消息真实时间戳"). The CONTENT is the contract: pure-text rows keep
//     the historical string collapse, everything structured must be an
//     array of parts validated by the shared `messageContentSchema` union.
//
// Escape hatch: set MIRRICODE_E2E_SKIP_RUST=1 to skip the whole suite
// (same env key as backend-switch.e2e.test.ts) for environments without
// the prebuilt `rust/target/debug/mirri` binary.

import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer as createHttpServer } from 'node:http';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { WebSocket, WebSocketServer } from 'ws';

import {
  createFakeProviderServer,
  type FakeProviderServer,
} from '@mirri-ai/e2e-harness';

// The shared protocol contract — a declared devDependency of mirri-web so
// the snapshot contract test consumes the same package (and the same
// lockfile-pinned zod@4.3.6 instance) the schemas were built against.
import { z } from 'zod';
import {
  isoDateTimeSchema,
  messageContentSchema,
  messageRoleSchema,
  sessionSnapshotResponseSchema,
} from '@mirri-ai/protocol';

// ---------------------------------------------------------------------------
// Paths / constants
// ---------------------------------------------------------------------------

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../', import.meta.url)));
const RUST_BIN = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');

/** `execFile` without a callback returns a ChildProcess, not a Promise — promisify it so `await` actually waits for the build. */
const execFileAsync = promisify(execFile);

const skipRust = process.env.MIRRICODE_E2E_SKIP_RUST === '1';

// ---- Contract schemas ------------------------------------------------------

/** The snapshot's nested session view — the rust `SnapshotSessionView`
 *  (camelCase subset). The shared `sessionSchema` is the full REST session
 *  row (workspace id, usage, agent_config, permission_rules, wall-clock
 *  timestamps, …) — none of which the snapshot view carries yet: those are
 *  registered follow-ups (context.md "快照 session view 补 usage" 等). This
 *  schema pins the fields the wire DOES emit so the contract cannot silently
 *  drift while the shared schema remains the long-term target. */
const snapshotSessionViewSchema = z.object({
  id: z.string().min(1),
  title: z.string().nullable(),
  busy: z.boolean(),
  agentCount: z.number().int().nonnegative(),
  model: z.string().nullable(),
  permission: z.unknown(),
  messageCount: z.number().int().nonnegative(),
  cwd: z.string().min(1),
});

/** One snapshot / message.list row — the wire envelope
 *  `{id, role, content, agentId, createdAt, metadata?}` (the RpcWireMessage
 *  TS interface). `content` is the R1 contract: pure-text rows keep the
 *  historical string collapse (locked by the rust guard test
 *  `should_keep_pure_text_rows_as_plain_strings_when_listing_messages`),
 *  everything structured must be an array of parts validated by the shared
 *  `messageContentSchema` union. */
const snapshotMessageRowSchema = z.object({
  id: z.string().min(1),
  role: messageRoleSchema,
  content: z.union([z.string(), z.array(messageContentSchema)]),
  agentId: z.string().min(1),
  createdAt: isoDateTimeSchema,
  metadata: z.record(z.string(), z.unknown()).optional(),
  // Per-turn usage attribution (assistant rows only) — the row's turn
  // prompt id, present only when the fold attributed the row.
  promptId: z.string().optional(),
});

/** `session.snapshot` result — the shared `sessionSnapshotResponseSchema`
 *  with the ADR-009 registered exemptions (see file header): in_flight_turn
 *  turn_id string/number, the slim session view, and the camelCase wire
 *  message rows. Everything else (the snake_case envelope, subagents,
 *  pending_approvals, pending_questions, …) is validated by the shared
 *  schema itself. */
const snapshotContractSchema = sessionSnapshotResponseSchema.extend({
  in_flight_turn: z.any().nullable(),
  session: snapshotSessionViewSchema,
  messages: z.object({
    items: z.array(snapshotMessageRowSchema),
    has_more: z.boolean(),
  }),
  // Per-turn usage of completed turns (usage-pill rerender parity) — the
  // snake_case envelope key with camelCase inner keys pinned so the wire
  // cannot silently drop the fold (R1-class regression guard).
  turn_usages: z.array(
    z.object({
      promptId: z.string(),
      inputTokens: z.number(),
      outputTokens: z.number(),
      cacheReadTokens: z.number(),
      cacheCreationTokens: z.number(),
      models: z.array(z.string()),
    }),
  ),
});

/** `message.list` result — the RPC face `{messages, hasMore}`. The REST
 *  page contract `listMessagesResponseSchema` (`{items, has_more}`) does
 *  NOT match this wire shape, so per the plan the rows are validated with
 *  the same wire row schema and the content-part discriminators are
 *  hand-asserted below. */
const messageListContractSchema = z.object({
  messages: z.array(snapshotMessageRowSchema),
  hasMore: z.boolean(),
});

/** Fail with the FULL zod error when the wire deviates from the contract —
 *  the diagnostics are the point of the contract test. */
function expectContractOk(label: string, value: unknown): void {
  const parsed = snapshotContractSchema.safeParse(value);
  if (!parsed.success) {
    // eslint-disable-next-line no-console
    console.error(
      `[snapshot-contract] ${label} — zod error:\n${JSON.stringify(parsed.error, null, 2)}`,
    );
    // eslint-disable-next-line no-console
    console.error(
      `[snapshot-contract] ${label} — wire value:\n${JSON.stringify(value, null, 2)}`,
    );
  }
  expect(
    parsed.success,
    `${label} must satisfy sessionSnapshotResponseSchema (+ ADR-009 registered exemptions)`,
  ).toBe(true);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Grab a free TCP port on loopback (close the placeholder before returning). */
function getFreePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const srv = createNetServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      const port = typeof addr === 'object' && addr !== null ? addr.port : undefined;
      srv.close(() => (port === undefined ? reject(new Error('no free port')) : resolvePort(port)));
    });
  });
}

/** Poll /api/v1/healthz until the envelope reports ok:true. */
async function waitForHealthz(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown = undefined;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/api/v1/healthz`);
      if (res.ok) {
        const body = (await res.json()) as { data?: { ok?: boolean } };
        if (body.data?.ok === true) return;
      }
    } catch (error) {
      lastError = error;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `server on :${port} not ready within ${timeoutMs}ms` +
      (lastError !== undefined
        ? ` (last error: ${lastError instanceof Error ? lastError.message : JSON.stringify(lastError)})`
        : ''),
  );
}

/**
 * Write the seed config.toml into a fresh MIRRICODE_HOME and spawn the
 * prebuilt rust binary (building it first if the plan's binary is missing).
 * The seed format mirrors `generate_seed_toml` in
 * rust/crates/mirri-embedded/src/embedded_server/tests.rs: one openai
 * provider pointed at the fake server + one `test/model` alias — the model
 * the test sessions are created with.
 */
async function spawnRustServer(opts: {
  port: number;
  homeDir: string;
  workDir: string;
  fakeBaseUrl: string;
}): Promise<{ child: ChildProcess }> {
  if (!existsSync(RUST_BIN)) {
    await execFileAsync('cargo', ['build', '-p', 'mirri-cli'], {
      cwd: join(REPO_ROOT, 'rust'),
      timeout: 600_000,
      maxBuffer: 32 * 1024 * 1024,
    });
  }
  await mkdir(opts.homeDir, { recursive: true });
  const configToml = `
[providers.test]
type = "openai"
api_key = "sk-test"
base_url = "${opts.fakeBaseUrl}"

[models."test/model"]
provider = "test"
model = "gpt-test"
max_context_size = 128000
`;
  await writeFile(join(opts.homeDir, 'config.toml'), configToml, 'utf8');

  const child = spawn(
    RUST_BIN,
    [
      'serve',
      '--host',
      '127.0.0.1',
      '--port',
      String(opts.port),
      '--workspace',
      opts.workDir,
      '--log-level',
      'warn',
      // Loopback open mode — matches backend-switch.e2e.test.ts. The server
      // would otherwise boot with the persistent bearer token.
      '--dangerous-bypass-auth',
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
      // Isolate the engine's Mirri home inside the throwaway dir.
      env: {
        ...process.env,
        MIRRICODE_HOME: opts.homeDir,
        OPENAI_API_KEY: 'dummy',
        OPENAI_BASE_URL: opts.fakeBaseUrl,
      },
    },
  );
  let stderr = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString()).slice(-4000);
  });
  try {
    await waitForHealthz(opts.port, 30_000);
  } catch (error) {
    child.kill('SIGKILL');
    throw new Error(`rust engine failed to boot: ${stderr}\n${String(error)}`, { cause: error });
  }
  return { child };
}

/** Politely stop the rust engine via /api/v1/shutdown, falling back to SIGKILL. */
async function stopRustServer(proc: { child: ChildProcess }, base: string): Promise<void> {
  try {
    // Bounded fetch: a busy engine may not answer promptly; the exit race
    // below plus the SIGKILL fallback must still run within this hook.
    await fetch(`${base}/api/v1/shutdown`, {
      method: 'POST',
      signal: AbortSignal.timeout(5000),
    });
    await Promise.race([
      new Promise<void>((r) => proc.child.once('exit', () => r())),
      new Promise<void>((r) => setTimeout(r, 5000)),
    ]);
  } catch {
    // not running — fine
  }
  if (proc.child.exitCode === null) proc.child.kill('SIGKILL');
}

interface RpcResponse<T = unknown> {
  id: number;
  jsonrpc?: string;
  result?: T;
  error?: { code: number; message: string };
}

/** Minimal JSON-RPC 2.0 WebSocket client for the unified `/api/v1/rpc`
 *  endpoint: completes the server_hello → client_hello handshake and
 *  resolves each request id to its response frame. */
class RpcClient {
  private static nextClientId = 0;
  private readonly ws: WebSocket;
  private readonly pending = new Map<number, (response: RpcResponse) => void>();
  private nextId = 1;

  private constructor(ws: WebSocket) {
    this.ws = ws;
    this.ws.on('message', (data: WebSocket.RawData) => {
      let parsed: RpcResponse;
      try {
        parsed = JSON.parse(data.toString()) as RpcResponse;
      } catch {
        return; // non-JSON frame — ignore
      }
      if (parsed.id !== undefined && this.pending.has(parsed.id)) {
        const resolve = this.pending.get(parsed.id)!;
        this.pending.delete(parsed.id);
        resolve(parsed);
      }
      // Server-side reverse RPC requests (approval.ask / question.ask) are
      // never expected in this suite: the turn runs under permissionMode
      // yolo, so they are intentionally not answered.
    });
  }

  static async connect(url: string): Promise<RpcClient> {
    const client = new RpcClient(new WebSocket(url));
    // Attach the server_hello listener BEFORE awaiting 'open': under load the
    // upgrade response and the greeting frame can coalesce into one socket
    // data event, so ws emits 'open' and 'message' in the same synchronous
    // stack — a listener attached after `await open` (a microtask) would
    // already have missed the frame, and no timeout would ever recover it.
    const helloReceived = new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`server_hello not received within 10s: ${url}`)),
        10_000,
      );
      const onMessage = (data: WebSocket.RawData) => {
        let parsed: { method?: string };
        try {
          parsed = JSON.parse(data.toString()) as { method?: string };
        } catch {
          return;
        }
        if (parsed.method === 'server_hello') {
          clearTimeout(timer);
          client.ws.off('message', onMessage);
          resolve();
        }
      };
      client.ws.on('message', onMessage);
      client.ws.once('error', (err) => {
        clearTimeout(timer);
        reject(err);
      });
    });
    await new Promise<void>((resolve, reject) => {
      client.ws.once('open', () => resolve());
      client.ws.once('error', (err) => reject(err));
    });
    // Strict handshake ordering (design §5): the server greets with
    // `server_hello` before it accepts anything else.
    await helloReceived;
    const hello = await client.call('client_hello', {
      clientId: `snapshot-contract-test-${RpcClient.nextClientId++}`,
      capabilities: { reverseRpc: ['approval', 'question', 'fs', 'terminal'] },
    });
    if (hello.error !== undefined || hello.result === undefined) {
      throw new Error(`client_hello rejected: ${JSON.stringify(hello)}`);
    }
    return client;
  }

  call<T = unknown>(method: string, params: Record<string, unknown>): Promise<RpcResponse<T>> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, resolve);
      this.ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`RPC timeout after 30s: ${method}`));
        }
      }, 30_000);
    });
  }

  close(): void {
    this.ws.close();
  }
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

// Regression guard for the server_hello race (2026-10-26 full-suite flake):
// when the WS upgrade response and the greeting frame coalesce into ONE
// socket data event, ws emits 'open' and 'message' in the same synchronous
// stack — a listener attached after `await open` runs as a microtask only
// after the whole data event, by which time the frame is gone. This pins
// RpcClient.connect against that loss without needing the rust engine.
describe('RpcClient handshake survives a coalesced upgrade+greeting read', () => {
  it('should complete the handshake when the 101 response and server_hello arrive in one data event', async () => {
    const httpServer = createHttpServer();
    const wss = new WebSocketServer({ server: httpServer });
    // Mirror the rust engine: greet immediately on upgrade, answer client_hello.
    wss.on('connection', (socket) => {
      socket.send(
        JSON.stringify({
          jsonrpc: '2.0',
          method: 'server_hello',
          params: { serverId: 'test', version: '0.0.0', heartbeatMs: 15000 },
        }),
      );
      socket.on('message', (data) => {
        let frame: { id?: number; method?: string };
        try {
          frame = JSON.parse(data.toString()) as { id?: number; method?: string };
        } catch {
          return;
        }
        if (frame.method === 'client_hello' && frame.id !== undefined) {
          socket.send(
            JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { accepted: true } }),
          );
        }
      });
    });
    await new Promise<void>((resolve) => httpServer.listen(0, '127.0.0.1', resolve));
    const port = (httpServer.address() as { port: number }).port;
    const url = `ws://127.0.0.1:${port}/api/v1/rpc`;

    try {
      // Start the connect, then block the client's event loop so the server's
      // 101 + greeting accumulate in the socket buffer and are delivered in
      // ONE data event — deterministic, no full-suite load required.
      const connecting = RpcClient.connect(url);
      const blockedUntil = Date.now() + 300;
      while (Date.now() < blockedUntil) {
        /* spin */
      }
      const client = await connecting;
      // The handshake is only complete when a forward RPC round-trips:
      // prove the client (not just the socket) is usable.
      const probe = await client.call('client_hello', {
        clientId: 'regression-probe',
        capabilities: { reverseRpc: [] },
      });
      client.close();
      expect(probe.error).toBeUndefined();
      expect(probe.result).toEqual({ accepted: true });
    } finally {
      await new Promise<void>((resolve) => wss.close(() => resolve()));
      await new Promise<void>((resolve) => httpServer.close(() => resolve()));
    }
  }, 15_000);
});

describe.skipIf(skipRust)('rust snapshot wiring satisfies the shared protocol contract', () => {
  let fakeProvider: FakeProviderServer | undefined;
  let rust: { child: ChildProcess } | undefined;
  let base = '';
  let client: RpcClient | undefined;
  let homeDir = '';
  let workDir = '';

  beforeAll(async () => {
    fakeProvider = await createFakeProviderServer();
    // Non-zero prompt-cache bucket on every scripted response's usage — the
    // per-turn usage fold must carry cacheReadTokens > 0 for the completed
    // turn (mirrors MockResponse::with_usage in the rust test harness).
    fakeProvider.setUsage({ promptTokens: 10, completionTokens: 5, cachedTokens: 4 });
    // Scripted LLM: response 1 = a Bash tool call (R1 tool_use path),
    // response 2 = thinking + text (R1 thinking path).
    fakeProvider.nextToolCall('Bash', { command: 'pwd' });
    fakeProvider.nextThinkThenText(
      'I should run pwd to see the cwd',
      'The working directory is confirmed. Task done.',
    );
    // Response 3/4 belong to the mid-turn in_flight_turn test: a DELAYED
    // tool call keeps the fire-mode turn in flight long enough to snapshot
    // it, then a plain text reply closes the turn.
    fakeProvider.setStreamDelay(1500);
    fakeProvider.nextToolCall('Bash', { command: 'echo fire' });
    fakeProvider.nextText('Fire turn done.');

    const dir = await mkdtemp(join(tmpdir(), 'mirri-snapshot-contract-'));
    homeDir = join(dir, 'mirri-home');
    workDir = join(dir, 'workspace');
    await mkdir(workDir, { recursive: true });

    const port = await getFreePort();
    base = `http://127.0.0.1:${port}`;
    rust = await spawnRustServer({
      port,
      homeDir,
      workDir,
      fakeBaseUrl: fakeProvider.baseUrl,
    });

    client = await RpcClient.connect(`${base.replace(/^http/, 'ws')}/api/v1/rpc`);
  }, 120_000);

  afterAll(async () => {
    client?.close();
    client = undefined;
    // Stop the engine BEFORE the fake provider: a fire-mode turn left
    // in flight keeps an SSE stream open against the provider, which
    // blocks fastify's close and would time out this hook — abandoning
    // the rest of the cleanup and leaking the serve process.
    if (rust !== undefined) {
      await stopRustServer(rust, base);
      rust = undefined;
    }
    if (fakeProvider !== undefined) {
      await fakeProvider.close();
      fakeProvider = undefined;
    }
    if (homeDir !== '') {
      await rm(join(homeDir, '..'), { recursive: true, force: true });
      homeDir = '';
    }
  }, 30_000);

  it('should serve a snapshot and message list that satisfy the shared protocol schema through a tool-calling turn', async () => {
    // Given a yolo-permission session on the model alias the seed config
    // points at the fake provider
    const created = await client!.call<{ id: string }>('session.create', {
      title: 'snapshot-contract',
      metadata: { cwd: workDir },
      permissionMode: 'yolo',
      modelId: 'test/model',
    });
    expect(created.error).toBeUndefined();
    expect(created.result?.id).toMatch(/^session-/);
    const sessionId = created.result!.id;

    // When a full turn runs in wait mode (tool call → thinking + text)
    const sent = await client!.call<{
      ok: boolean;
      turn: { stopReason: string; steps: number };
    }>('message.send', { sessionId, text: 'Run pwd please', mode: 'wait' });
    expect(sent.error).toBeUndefined();
    expect(sent.result?.ok).toBe(true);
    expect(sent.result?.turn.stopReason).toBe('end_turn');
    expect(sent.result?.turn.steps).toBeGreaterThanOrEqual(2);

    // Then `session.snapshot` satisfies the shared contract schema
    const snap = await client!.call<Record<string, unknown>>('session.snapshot', {
      session_id: sessionId,
    });
    expect(snap.error).toBeUndefined();
    const snapshotResult = snap.result!;
    expectContractOk('session.snapshot', snapshotResult);

    // And the R1 content projections are present with complete fields:
    const rows = (snapshotResult['messages'] as { items: Array<Record<string, unknown>> }).items;
    const toolUseParts = rows.flatMap((row) =>
      Array.isArray(row['content'])
        ? (row['content'] as Array<Record<string, unknown>>).filter((part) => part['type'] === 'tool_use')
        : [],
    );
    expect(toolUseParts.length).toBeGreaterThanOrEqual(1);
    expect(toolUseParts[0]).toMatchObject({
      type: 'tool_use',
      tool_call_id: 'call_Bash',
      tool_name: 'Bash',
      input: { command: 'pwd' },
    });

    const toolResultParts = rows.flatMap((row) =>
      Array.isArray(row['content'])
        ? (row['content'] as Array<Record<string, unknown>>).filter((part) => part['type'] === 'tool_result')
        : [],
    );
    expect(toolResultParts.length).toBeGreaterThanOrEqual(1);
    expect(toolResultParts[0]).toMatchObject({
      type: 'tool_result',
      tool_call_id: 'call_Bash',
    });
    expect(typeof toolResultParts[0]['output']).toBe('string');
    expect(String(toolResultParts[0]['output'])).toContain('workspace');

    const thinkingParts = rows.flatMap((row) =>
      Array.isArray(row['content'])
        ? (row['content'] as Array<Record<string, unknown>>).filter((part) => part['type'] === 'thinking')
        : [],
    );
    expect(thinkingParts.length).toBeGreaterThanOrEqual(1);
    expect(thinkingParts[0]).toMatchObject({
      type: 'thinking',
      thinking: 'I should run pwd to see the cwd',
    });

    // G19 order guard: within the assistant row that carries both, the
    // thinking part must come BEFORE the text part — the streamed arrival
    // order (reasoning first) the live projector renders, and the wire
    // record must preserve (ADR-008), or a re-rendered session shows the
    // thinking block after the answer.
    const rowWithBoth = rows.find(
      (row) =>
        Array.isArray(row['content']) &&
        (row['content'] as Array<Record<string, unknown>>).some((part) => part['type'] === 'thinking') &&
        (row['content'] as Array<Record<string, unknown>>).some((part) => part['type'] === 'text'),
    );
    expect(rowWithBoth).toBeDefined();
    const bothTypes = (rowWithBoth!['content'] as Array<Record<string, unknown>>).map((part) => part['type']);
    expect(bothTypes.indexOf('thinking')).toBeLessThan(bothTypes.indexOf('text'));

    // And the per-turn usage read face (usage-pill rerender parity): the
    // completed turn's folded usage rides the `turn_usages` envelope with a
    // non-zero cache bucket, and its assistant rows (and only those) are
    // stamped with the turn's prompt id.
    const turnUsages = snapshotResult['turn_usages'] as Array<{
      promptId: string;
      cacheReadTokens: number;
    }>;
    expect(turnUsages.length).toBeGreaterThanOrEqual(1);
    expect(turnUsages[0]!.cacheReadTokens).toBeGreaterThan(0);
    const stamped = rows.filter((row) => row['promptId'] !== undefined && row['role'] === 'assistant');
    expect(stamped.length).toBeGreaterThan(0);
    expect(stamped.every((row) => row['role'] === 'assistant')).toBe(true);
    expect(
      turnUsages.some((entry) => entry.promptId === stamped[0]!['promptId']),
    ).toBe(true);

    // And `message.list` — the REST page schema (`listMessagesResponseSchema`
    // items/has_more envelope) does not fit the RPC face (`{messages,
    // hasMore}`), so rows are validated with the shared wire row schema and
    // the content-part discriminators are hand-asserted (per the plan).
    const list = await client!.call<{ messages: unknown[]; hasMore: boolean }>('message.list', {
      sessionId,
    });
    expect(list.error).toBeUndefined();
    const listContract = messageListContractSchema.safeParse(list.result);
    if (!listContract.success) {
      // eslint-disable-next-line no-console
      console.error(`[snapshot-contract] message.list — zod error:\n${JSON.stringify(listContract.error, null, 2)}`);
    }
    expect(listContract.success, 'message.list rows must satisfy the wire row contract').toBe(true);

    const listToolCalls = list.result!.messages.flatMap((row) =>
      Array.isArray((row as Record<string, unknown>)['content'])
        ? ((row as Record<string, unknown>)['content'] as Array<Record<string, unknown>>).filter(
            (part) => part['type'] === 'tool_use',
          )
        : [],
    );
    expect(listToolCalls.length).toBeGreaterThanOrEqual(1);
    expect(listToolCalls[0]).toMatchObject({
      type: 'tool_use',
      tool_call_id: 'call_Bash',
      tool_name: 'Bash',
      input: { command: 'pwd' },
    });
  }, 60_000);

  it('should expose a running in_flight_turn with a string turn_id while a fire-mode turn is in flight (ADR-009 deviation)', async () => {
    // Given a second session (provider script queue is already positioned:
    // a DELAYED tool call then a text reply)
    const created = await client!.call<{ id: string }>('session.create', {
      title: 'in-flight-turn',
      metadata: { cwd: workDir },
      permissionMode: 'yolo',
      modelId: 'test/model',
    });
    expect(created.error).toBeUndefined();
    const sessionId = created.result!.id;

    // When the turn runs in fire mode and the snapshot is taken while the
    // provider is still streaming the delayed response
    const sent = await client!.call<{ ok: boolean; turn_id: string }>('message.send', {
      sessionId,
      text: 'Fire please',
      mode: 'fire',
    });
    expect(sent.error).toBeUndefined();
    expect(sent.result?.ok).toBe(true);
    await new Promise((r) => setTimeout(r, 400));

    const snap = await client!.call<Record<string, unknown>>('session.snapshot', {
      session_id: sessionId,
    });
    expect(snap.error).toBeUndefined();
    expectContractOk('session.snapshot (mid-turn)', snap.result!);

    // Then the in-flight turn is visible with the rust wire's string turn id
    // — the ADR-009 registered deviation the schema exempts.
    const inFlight = snap.result!['in_flight_turn'] as {
      turn_id: unknown;
      assistant_text?: string;
      running_tools?: unknown[];
    } | null;
    expect(inFlight).not.toBeNull();
    expect(typeof inFlight?.turn_id).toBe('string');
    expect(String(inFlight?.turn_id)).toMatch(/^turn-/);
    expect(Array.isArray(inFlight?.running_tools)).toBe(true);
  }, 60_000);
});