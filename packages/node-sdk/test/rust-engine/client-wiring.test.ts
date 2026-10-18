import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  __setMirriNodeBindingForTest,
  SDKRpcClientRust,
  type MirriEngineNative,
  type MirriNodeBinding,
} from '#/sdk-rpc-client-rust';

/**
 * Fake engine capturing the client's subscriptions and answers, so the wiring
 * tests can drive the event and reverse-RPC channels exactly like the NAPI
 * binding would (callbacks invoked on the Node.js main thread).
 */
class FakeEngine implements MirriEngineNative {
  readonly called: { method: string; params: unknown }[] = [];
  readonly responded: { id: number; result: unknown }[] = [];
  readonly respondedErrors: { id: number; error: string }[] = [];
  eventHandler:
    | ((err: null, event: { method: string; params: unknown }) => void)
    | undefined;
  reverseRpcHandler:
    | ((err: null, request: { id: number; method: string; params: unknown }) => void)
    | undefined;
  disposed = false;

  /** Scripted `callMethod` results keyed by wire method name. */
  readonly results = new Map<string, unknown>();

  async callMethod(method: string, params: unknown): Promise<unknown> {
    this.called.push({ method, params });
    return this.results.get(method) ?? {};
  }

  onEvent(
    callback: (err: null, event: { method: string; params: unknown }) => void,
  ): void {
    this.eventHandler = callback;
  }

  onReverseRpc(
    callback: (
      err: null,
      request: { id: number; method: string; params: unknown },
    ) => void,
  ): void {
    this.reverseRpcHandler = callback;
  }

  async respondReverseRpc(id: number, result: unknown): Promise<void> {
    this.responded.push({ id, result });
  }

  async respondReverseRpcError(id: number, error: string): Promise<void> {
    this.respondedErrors.push({ id, error });
  }

  async dispose(): Promise<void> {
    this.disposed = true;
  }

  /** Emit a notification verbatim (the `event.<type>` projection methods). */
  fireNotification(method: string, params: unknown): void {
    this.eventHandler?.(null, { method, params });
  }

  fireReverseRpc(id: number, method: string, params: unknown): void {
    this.reverseRpcHandler?.(null, { id, method, params });
  }
}

let fakeEngine: FakeEngine;
const tempDirs: string[] = [];

beforeEach(() => {
  fakeEngine = new FakeEngine();
  const binding: MirriNodeBinding = { createEngine: () => fakeEngine };
  __setMirriNodeBindingForTest(binding);
});

afterEach(async () => {
  __setMirriNodeBindingForTest(null);
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
  vi.restoreAllMocks();
});

async function makeTempHome(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-rust-wiring-'));
  tempDirs.push(dir);
  return dir;
}

function makeClient(homeDir: string): SDKRpcClientRust {
  return new SDKRpcClientRust({ homeDir, uiMode: 'cli' });
}

/**
 * Fire a notification with a well-formed envelope: the durable fields
 * (`volatile: false`, `seq`, `epoch`) plus the payload merged flat, exactly
 * the way the shared event projection frames notifications.
 */
function fireEvent(method: string, params: Record<string, unknown>): void {
  fakeEngine.fireNotification(method, {
    volatile: false,
    timestamp: '2026-01-01T00:00:00Z',
    seq: 1,
    epoch: 'epoch-1',
    ...params,
  });
}

/** The main agent's wire uuid (the nil uuid the engine mints for `main`). */
const MAIN_AGENT_UUID = '00000000-0000-0000-0000-000000000000';

/**
 * Establish the session context the way the engine does: the user-message
 * echo on the session's event bus.
 */
function seedSession(sessionId: string): void {
  fireEvent('event.message.received', {
    sessionId,
    agentId: MAIN_AGENT_UUID,
    role: 'user',
    contentSummary: 'hi',
  });
}

/** Await the async reverse-RPC bridge started with `void` by the forwarder. */
async function flushBridge(): Promise<void> {
  await vi.waitFor(() => {
    expect(fakeEngine.responded.length + fakeEngine.respondedErrors.length).toBeGreaterThan(0);
  });
}

describe('SDKRpcClientRust engine wiring', () => {
  it('registers the event and reverse-RPC forwarders when the engine is created', async () => {    const client = makeClient(await makeTempHome());
    try {
      expect(fakeEngine.eventHandler).toBeUndefined();
      expect(fakeEngine.reverseRpcHandler).toBeUndefined();

      await client.getConfig();

      expect(fakeEngine.eventHandler).toBeDefined();
      expect(fakeEngine.reverseRpcHandler).toBeDefined();
    } finally {
      await client.close();
    }
  });

  it('disposes the engine on close', async () => {
    const client = makeClient(await makeTempHome());
    await client.getConfig();
    expect(fakeEngine.disposed).toBe(false);
    await client.close();
    expect(fakeEngine.disposed).toBe(true);
  });

  it('should return the agent id string when btw.start answers the serve-wire object shape', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('btw.start', { agent_id: 'agent-btw-1' });
      await expect(client.startBtw({ sessionId: 'session-1' })).resolves.toBe('agent-btw-1');
    } finally {
      await client.close();
    }
  });

  it('should map serve-wire session views onto the v1 SessionSummary shape', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('session.list', [
        {
          id: 'session-1',
          title: 'Untitled Session',
          createdAt: '2026-01-01T00:00:00.000Z',
          updatedAt: '2026-01-02T03:04:05.000Z',
          agentCount: 1,
          tags: [],
          status: 'closed',
          busy: false,
          model: 'ultra',
          messageCount: 2,
          lastSeq: 7,
          cwd: '/tmp/ws',
        },
      ]);
      const sessions = await client.listSessions({ workDir: '/tmp/ws' });
      expect(sessions).toHaveLength(1);
      expect(sessions[0]).toMatchObject({
        id: 'session-1',
        title: 'Untitled Session',
        workDir: '/tmp/ws',
        createdAt: Date.parse('2026-01-01T00:00:00.000Z'),
        updatedAt: Date.parse('2026-01-02T03:04:05.000Z'),
      });
    } finally {
      await client.close();
    }
  });

  it('forwarders are registered exactly once per engine instance', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      await client.getConfigDiagnostics();
      expect(fakeEngine.called.filter((call) => call.method === 'config.get')).toHaveLength(1);
    } finally {
      await client.close();
    }
  });

  it('translates engine events into v1 Events delivered to onEvent listeners', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      const received: unknown[] = [];
      client.onEvent((event) => received.push(event));

      seedSession('session-1');
      fakeEngine.fireNotification('event.assistant.delta', {
        sessionId: 'session-1',
        volatile: true,
        timestamp: '2026-01-01T00:00:00Z',
        agentId: MAIN_AGENT_UUID,
        delta: 'Hel',
      });
      fakeEngine.fireNotification('event.assistant.delta', {
        sessionId: 'session-1',
        volatile: true,
        timestamp: '2026-01-01T00:00:00Z',
        agentId: MAIN_AGENT_UUID,
        delta: 'lo',
      });

      await vi.waitFor(() => {
        expect(received).toHaveLength(2);
      });
      expect(received[0]).toEqual({
        type: 'assistant.delta',
        turnId: 0,
        delta: 'Hel',
        agentId: 'main',
        sessionId: 'session-1',
      });
      expect((received[1] as { delta: string }).delta).toBe('lo');
    } finally {
      await client.close();
    }
  });

  it('routes approval.ask to the session handler and answers the engine', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      let seen: unknown;
      client.setApprovalHandler('session-1', async (request) => {
        seen = request;
        return { decision: 'approved' };
      });

      // The engine event establishes the session context (reverse RPCs carry
      // no session id on the wire).
      seedSession('session-1');
      fakeEngine.fireReverseRpc(7, 'approval.ask', {
        tool_name: 'Bash',
        tool_call_id: 'call_1',
        action: 'execute Bash',
        display: { Command: { command: 'ls' } },
      });

      await flushBridge();
      expect(seen).toEqual({
        toolCallId: 'call_1',
        toolName: 'Bash',
        action: 'execute Bash',
        display: { kind: 'command', command: 'ls' },
        sessionId: 'session-1',
        agentId: 'main',
      });
      expect(fakeEngine.responded).toEqual([{ id: 7, result: { decision: 'approved' } }]);
    } finally {
      await client.close();
    }
  });

  it('answers approval.ask with a cancelled decision when no handler is registered', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      seedSession('session-1');
      fakeEngine.fireReverseRpc(3, 'approval.ask', {
        tool_name: 'Bash',
        tool_call_id: 'call_1',
        action: 'execute Bash',
        display: { Generic: { summary: 'x' } },
      });

      await flushBridge();
      expect(fakeEngine.responded).toHaveLength(1);
      expect(fakeEngine.responded[0]?.id).toBe(3);
      expect(fakeEngine.responded[0]?.result).toMatchObject({ decision: 'cancelled' });
    } finally {
      await client.close();
    }
  });

  it('routes question.ask to the session handler and answers the engine', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      let seen: unknown;
      client.setQuestionHandler('session-1', async (request) => {
        seen = request;
        return { answers: { 'Pick one': 'A' } };
      });

      seedSession('session-1');
      fakeEngine.fireReverseRpc(9, 'question.ask', {
        questions: [{ question: 'Pick one', options: [{ label: 'A' }, { label: 'B' }] }],
      });

      await flushBridge();
      expect(seen).toEqual({
        questions: [
          { question: 'Pick one', options: [{ label: 'A' }, { label: 'B' }] },
        ],
        sessionId: 'session-1',
        agentId: 'main',
      });
      expect(fakeEngine.responded).toEqual([
        { id: 9, result: { answers: { 'Pick one': 'A' } } },
      ]);
    } finally {
      await client.close();
    }
  });

  it('answers an unknown reverse RPC method with an error', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      seedSession('session-1');
      fakeEngine.fireReverseRpc(11, 'fs.proxy', {});

      await flushBridge();
      expect(fakeEngine.respondedErrors).toEqual([
        { id: 11, error: 'unsupported engine reverse RPC method: fs.proxy' },
      ]);
    } finally {
      await client.close();
    }
  });

  it('answers with an error when no session context is known yet', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      fakeEngine.fireReverseRpc(13, 'approval.ask', {
        tool_name: 'Bash',
        tool_call_id: 'call_1',
        action: 'execute Bash',
        display: { Command: { command: 'ls' } },
      });

      await flushBridge();
      expect(fakeEngine.respondedErrors).toHaveLength(1);
      expect(fakeEngine.respondedErrors[0]?.id).toBe(13);
      expect(fakeEngine.respondedErrors[0]?.error).toContain('before any session');
    } finally {
      await client.close();
    }
  });

  it('resolves the session context from session.create result ids', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('session.create', { id: 'session-2' });
      let seen: { sessionId: string } | undefined;
      client.setApprovalHandler('session-2', async (request) => {
        // The base class appends sessionId/agentId to the SDK request at
        // runtime (v1's core does the same); the handler type omits them.
        seen = request as unknown as { sessionId: string };
        return { decision: 'approved' };
      });

      await client.createSession({ workDir: '/tmp/work' });
      fakeEngine.fireReverseRpc(21, 'approval.ask', {
        tool_name: 'Bash',
        tool_call_id: 'call_2',
        action: 'execute Bash',
        display: { Command: { command: 'ls' } },
      });

      await flushBridge();
      expect(seen?.sessionId).toBe('session-2');
    } finally {
      await client.close();
    }
  });

  it('should clear the session context when a session.deleted notification arrives', async () => {
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();
      const received: unknown[] = [];
      client.onEvent((event) => received.push(event));

      seedSession('session-1');
      fireEvent('event.session.deleted', {
        sessionId: 'session-1',
        reason: 'user_initiated',
      });
      fakeEngine.fireReverseRpc(5, 'approval.ask', {
        tool_name: 'Bash',
        tool_call_id: 'call_1',
        action: 'execute Bash',
        display: { Command: { command: 'ls' } },
      });

      await vi.waitFor(() => {
        expect(fakeEngine.respondedErrors.length).toBeGreaterThan(0);
      });
      expect(received).toHaveLength(0);
      expect(fakeEngine.respondedErrors[0]?.error).toContain('before any session');
    } finally {
      await client.close();
    }
  });

  it('threads the harness skillDirs into the native engine creation (v1/v2 parity)', async () => {
    const createdWith: Array<{ homeDir?: string; skillDirs: string[] }> = [];
    __setMirriNodeBindingForTest({
      createEngine: (homeDir?: string, skillDirs?: readonly string[]) => {
        createdWith.push({ homeDir, skillDirs: skillDirs ? [...skillDirs] : [] });
        return fakeEngine;
      },
    });
    const client = new SDKRpcClientRust({
      homeDir: await makeTempHome(),
      skillDirs: ['/tmp/skills/a', '/tmp/skills/b'],
      uiMode: 'cli',
    });
    try {
      await client.getConfig();

      // The engine is created exactly once, with the harness home AND the
      // explicit skill dirs (which the engine applies as its skill
      // discovery set, replacing the default user/project discovery)
      expect(createdWith).toHaveLength(1);
      expect(createdWith[0]?.skillDirs).toEqual(['/tmp/skills/a', '/tmp/skills/b']);
      expect(createdWith[0]?.homeDir).toMatch(/mirri-rust-wiring-/);
    } finally {
      await client.close();
    }
  });

  it('passes an empty skillDirs list when the harness gives none (ambient discovery)', async () => {
    const createdWith: Array<{ skillDirs: string[] }> = [];
    __setMirriNodeBindingForTest({
      createEngine: (_homeDir?: string, skillDirs?: readonly string[]) => {
        createdWith.push({ skillDirs: skillDirs ? [...skillDirs] : [] });
        return fakeEngine;
      },
    });
    const client = makeClient(await makeTempHome());
    try {
      await client.getConfig();

      // Empty (not undefined) keeps the engine's ambient user/project
      // skill discovery — the same shape the v1/v2 clients forward
      expect(createdWith).toEqual([{ skillDirs: [] }]);
    } finally {
      await client.close();
    }
  });
});