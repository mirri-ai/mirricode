import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';
import { z } from 'zod';

import { agentActivityStateSchema } from '@mirri-ai/klient/contract/agent/activity';
import { agentCommandInfoSchema, agentTaskInfoSchema } from '@mirri-ai/klient/contract/agent/rpc';
import { configContract } from '@mirri-ai/klient/contract/global/config';
import { envContract } from '@mirri-ai/klient/contract/global/env';
import { modelConfigSchema } from '@mirri-ai/klient/contract/global/models';
import { providerConfigSchema } from '@mirri-ai/klient/contract/global/providers';
import { experimentalFeatureStateSchema } from '@mirri-ai/klient/contract/global/flags';
import { hostFsContract } from '@mirri-ai/klient/contract/global/hostFs';
import { mcpConfigEntrySchema, mcpConfigPreviewResultSchema, mcpServerInfoSchema, mcpSyncResultSchema, mcpTestResultSchema, mcpToolsResultSchema } from '@mirri-ai/klient/contract/global/mcp';
import { providerDiscoveryContract } from '@mirri-ai/klient/contract/global/providerDiscovery';
import {
  pluginCommandDefSchema,
  pluginSummarySchema,
  pluginUpdateStatusSchema,
  reloadSummarySchema,
} from '@mirri-ai/klient/contract/global/plugins';

import type { MirriEngineNative, MirriNodeBinding } from '#/sdk-rpc-client-rust';
import {
  decodeRustEngineError,
  RustEngineRpcError,
} from '#/rust-engine/error-channel';

import {
  EVENT_BRANCHES,
  EVENT_METHODS,
  EXTENSION_EVENT_METHODS,
  MAPPED_EVENT_METHODS,
  type RustEngineEventMethod,
} from './fixtures/rust-engine-events';
import {
  RUST_ENGINE_METHODS,
  SETUP_SESSION_ID,
  SETUP_WORKSPACE_ID,
  type RustEngineMethodFixture,
} from './fixtures/rust-engine-methods';

// Load the native binding directly, exactly like rust-engine-e2e.test.ts:
// the .node binary must be built with `--features mock-llm`
//   cd rust && cargo build --features mock-llm -p mirri-node
//   cp target/debug/libmirri_node.dylib ../packages/mirri-node/mirri-node.darwin-arm64.node
//   cp target/debug/libmirri_node.dylib ../packages/mirri-node/mirri-node.node
// Without it the whole engine-gated suite below is skipped.
const binding = require('@mirri-ai/mirri-node') as MirriNodeBinding;

const hasMockLlm = typeof binding.createEngineWithMockLlm === 'function';

const SNAPSHOT_FILE = new URL(
  './fixtures/rust-engine-contract-snapshots.json',
  import.meta.url,
);
const UPDATE_SNAPSHOTS = process.env['UPDATE_SNAPSHOTS'] === '1';

// ---------------------------------------------------------------------------
// Wire-shape normalization (pure, engine-independent — unit-tested below)
// ---------------------------------------------------------------------------

/**
 * A structural description of a wire response: leaves are reduced to type
 * names, objects to key sets with per-key shapes, arrays to their element
 * shape. Concrete values and array lengths never appear, so a snapshot locks
 * the *shape* of the contract, not the data.
 */
export type WireShape =
  | 'string'
  | 'number'
  | 'boolean'
  | 'null'
  | 'undefined'
  | { readonly array: 'empty' | WireShape | readonly WireShape[] }
  | { readonly object: Readonly<Record<string, WireShape>> };

/** Canonical string for a shape (object keys are already sorted). */
function canonicalShapeKey(shape: WireShape): string {
  return JSON.stringify(shape);
}

/** Reduce a wire response to its structural shape description. */
export function normalizeWireShape(value: unknown): WireShape {
  if (value === null) {
    return 'null';
  }
  const type = typeof value;
  if (type === 'undefined') {
    return 'undefined';
  }
  if (type === 'string' || type === 'number' || type === 'boolean') {
    return type;
  }
  if (Array.isArray(value)) {
    if (value.length === 0) {
      return { array: 'empty' };
    }
    const shapes = value.map(normalizeWireShape);
    const distinct = new Map<string, WireShape>();
    for (const shape of shapes) {
      distinct.set(canonicalShapeKey(shape), shape);
    }
    if (distinct.size === 1) {
      return { array: shapes[0]! };
    }
    // Heterogeneous arrays: lock the sorted set of distinct element shapes
    // so element order and repetition never matter.
    const sorted = [...distinct.entries()]
      .toSorted(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([, shape]) => shape);
    return { array: sorted };
  }
  if (type === 'object') {
    const record = value as Record<string, unknown>;
    const fields: Record<string, WireShape> = {};
    for (const key of Object.keys(record).toSorted()) {
      fields[key] = normalizeWireShape(record[key]);
    }
    return { object: fields };
  }
  return 'undefined';
}

/** Read the committed shape snapshots (missing file → empty record). */
function readCommittedSnapshots(): Record<string, WireShape> {
  try {
    return JSON.parse(readFileSync(SNAPSHOT_FILE, 'utf8')) as Record<
      string,
      WireShape
    >;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
      return {};
    }
    throw error;
  }
}

// ---------------------------------------------------------------------------
// Engine-independent coverage of the normalization/compare logic
// ---------------------------------------------------------------------------

describe('wire shape normalization (engine-independent)', () => {
  it('should reduce leaves to type names when normalizing scalar values', () => {
    expect(normalizeWireShape('session-42')).toBe('string');
    expect(normalizeWireShape(7)).toBe('number');
    expect(normalizeWireShape(false)).toBe('boolean');
    expect(normalizeWireShape(null)).toBe('null');
  });

  it('should lock the element shape without locking the length when normalizing a homogeneous array', () => {
    expect(normalizeWireShape([1, 2, 3])).toEqual({ array: 'number' });
    expect(normalizeWireShape(['only-one'])).toEqual({ array: 'string' });
  });

  it('should mark empty arrays as empty when normalizing an array without elements', () => {
    expect(normalizeWireShape([])).toEqual({ array: 'empty' });
  });

  it('should reduce heterogeneous arrays to the sorted set of distinct element shapes', () => {
    expect(normalizeWireShape(['b', 1, 'a'])).toEqual({
      array: ['number', 'string'],
    });
  });

  it('should lock key sets with per-key shapes and sort keys when normalizing an object', () => {
    expect(normalizeWireShape({ b: 'x', a: [true] })).toEqual({
      object: { a: { array: 'boolean' }, b: 'string' },
    });
  });

  it('should never leak concrete values or array lengths into a normalized shape', () => {
    const serialized = JSON.stringify(
      normalizeWireShape({
        id: 'SENTINEL-session',
        items: ['SENTINEL-a', 'SENTINEL-b', 'SENTINEL-c'],
        meta: null,
      }),
    );
    expect(serialized).not.toContain('SENTINEL');
    expect(serialized).not.toMatch(/[0-9]/);
  });
});

// ---------------------------------------------------------------------------
// Fixture integrity (engine-independent)
// ---------------------------------------------------------------------------

describe('rust engine method fixture integrity', () => {
  it('should map all 198 embedded dotted methods exactly once when the fixture is loaded', () => {
    const methods = RUST_ENGINE_METHODS.map((fixture) => fixture.method);
    expect(methods).toHaveLength(198);
    expect(new Set(methods).size).toBe(198);
  });

  it('should assign every fixture entry one of the four contract modes with a mandatory skip reason', () => {
    let schema = 0;
    let snapshot = 0;
    let error = 0;
    let skip = 0;
    for (const fixture of RUST_ENGINE_METHODS) {
      if (fixture.mode === 'schema') {
        schema += 1;
      } else if (fixture.mode === 'snapshot') {
        snapshot += 1;
      } else if (fixture.mode === 'error') {
        error += 1;
        expect(
          fixture.expectDomainCode.trim().length,
          `${fixture.method} must carry an expected domain code`,
        ).toBeGreaterThan(0);
      } else {
        skip += 1;
        expect(fixture.reason.trim().length, `${fixture.method} must carry a skip reason`).toBeGreaterThan(0);
      }
    }
    expect(schema + snapshot + error + skip).toBe(198);
  });

  it('should account every schema-mode method in the klient schema table when the fixture is loaded', () => {
    const schemaMethods = RUST_ENGINE_METHODS
      .filter(fixture => fixture.mode === 'schema')
      .map(fixture => fixture.method);
    // Every schema-mode method is either mapped to a klient zod schema or
    // documented as a known gap — nothing in between.
    const unmapped = schemaMethods.filter(
      method => !KLIENT_RESPONSE_SCHEMAS[method] && !METHODS_WITHOUT_KLIENT_SCHEMA.has(method),
    );
    expect(
      unmapped,
      'schema-mode methods missing from both KLIENT_RESPONSE_SCHEMAS and METHODS_WITHOUT_KLIENT_SCHEMA',
    ).toEqual([]);
    // And the tables carry no stale entries for demoted/renamed methods.
    const stale = [
      ...Object.keys(KLIENT_RESPONSE_SCHEMAS),
      ...METHODS_WITHOUT_KLIENT_SCHEMA,
    ].filter(method => !schemaMethods.includes(method));
    expect(stale, 'schema-table entries with no schema-mode fixture method').toEqual([]);
    // The two tables must be disjoint: `validateSchemaContract` consults
    // KLIENT_RESPONSE_SCHEMAS first, so an overlap would silently shadow the
    // documented gap instead of failing.
    const overlap = Object.keys(KLIENT_RESPONSE_SCHEMAS).filter(method =>
      METHODS_WITHOUT_KLIENT_SCHEMA.has(method),
    );
    expect(overlap, 'methods present in both KLIENT_RESPONSE_SCHEMAS and METHODS_WITHOUT_KLIENT_SCHEMA').toEqual([]);
  });

  it('should fail listing the snapshot-mode methods when they carry no committed shape snapshot entry', () => {
    if (UPDATE_SNAPSHOTS) {
      // Regeneration runs may add snapshot-mode methods before their entries
      // exist; the engine-gated sweep's afterAll writes them.
      return;
    }
    const committedMethods = new Set(Object.keys(readCommittedSnapshots()));
    const missing = RUST_ENGINE_METHODS
      .filter(fixture => fixture.mode === 'snapshot')
      .map(fixture => fixture.method)
      .filter(method => !committedMethods.has(method));
    expect(
      missing,
      'snapshot-mode fixture methods with no committed snapshot entry — run UPDATE_SNAPSHOTS=1 pnpm --filter @mirri-ai/mirri-code-sdk exec vitest run test/rust-engine-contract-e2e.test.ts to generate them',
    ).toEqual([]);
  });

  it('should fail listing the orphan methods when the committed shape snapshots carry entries no snapshot-mode fixture references', () => {
    const snapshotMethods = new Set(
      RUST_ENGINE_METHODS
        .filter(fixture => fixture.mode === 'snapshot')
        .map(fixture => fixture.method),
    );
    const orphans = Object.keys(readCommittedSnapshots()).filter(
      method => !snapshotMethods.has(method),
    );
    expect(
      orphans,
      'orphan snapshot entries (committed in rust-engine-contract-snapshots.json but no snapshot-mode fixture method references them) — delete the stale entries or restore the fixture method',
    ).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// M4-b3: event projection branch-table integrity (engine-independent)
// ---------------------------------------------------------------------------

/** A durable envelope sample (seq/epoch present, `volatile: false`). */
const DURABLE_ENVELOPE = {
  sessionId: 'session-1',
  seq: 1,
  epoch: 'epoch-1',
  volatile: false,
  timestamp: '2026-01-01T00:00:00Z',
} as const;

/** A volatile envelope sample (no seq/epoch, `volatile: true`). */
const VOLATILE_ENVELOPE = {
  sessionId: 'session-1',
  volatile: true,
  timestamp: '2026-01-01T00:00:00Z',
} as const;

/**
 * One representative full-params notification per branch — envelope merged
 * with the branch's payload — used to behaviorally lock each branch's
 * contract: the sample must parse clean, and the same sample plus a stray
 * key must fail (strict-object semantics, no unknown-key passthrough).
 * Branches with more than one wire spelling (turn.ended success vs.
 * failure, session.updated resumed vs. forked) carry one sample each.
 */
const BRANCH_EXAMPLES: ReadonlyArray<{
  method: RustEngineEventMethod;
  params: Record<string, unknown>;
}> = [
  {
    method: 'event.turn.started',
    params: { ...DURABLE_ENVELOPE, agentId: 'main', turnId: 'turn-9ab3' },
  },
  {
    method: 'event.turn.ended',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      turnId: 'turn-9ab3',
      reason: 'completed',
    },
  },
  {
    method: 'event.turn.ended',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      turnId: 'turn-9ab3',
      reason: 'failed',
      error: { code: 'internal', message: 'turn timed out', retryable: false },
    },
  },
  {
    method: 'event.assistant.delta',
    params: { ...VOLATILE_ENVELOPE, agentId: 'main', delta: 'Hel' },
  },
  {
    method: 'event.thinking.delta',
    params: { ...VOLATILE_ENVELOPE, agentId: 'main', delta: 'reas' },
  },
  {
    method: 'event.compaction.started',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      trigger: 'manual',
      instruction: 'summarize the story',
    },
  },
  {
    method: 'event.compaction.blocked',
    params: { ...DURABLE_ENVELOPE, agentId: 'main' },
  },
  {
    method: 'event.hook.result',
    params: {
      ...DURABLE_ENVELOPE,
      hookEvent: 'UserPromptSubmit',
      content: 'hook says hi',
    },
  },
  {
    method: 'event.hook.result',
    params: {
      ...DURABLE_ENVELOPE,
      hookEvent: 'UserPromptSubmit',
      content: 'you shall not pass',
      blocked: true,
    },
  },
  {
    method: 'event.compaction.cancelled',
    params: { ...DURABLE_ENVELOPE, agentId: 'main' },
  },
  {
    method: 'event.compaction.completed',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      result: {
        summary: 'The user asked for a story.',
        compactedCount: 4,
        tokensBefore: 1200,
        tokensAfter: 150,
        droppedCount: 0,
      },
    },
  },
  {
    method: 'event.tool.call.started',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      toolCallId: 'call_1',
      name: 'Bash',
      args: { command: 'echo hello' },
    },
  },
  {
    method: 'event.tool.progress',
    params: {
      ...VOLATILE_ENVELOPE,
      agentId: 'main',
      toolCallId: 'call_1',
      update: { kind: 'stdout', text: 'compiling' },
    },
  },
  {
    method: 'event.tool.call.delta',
    params: {
      ...VOLATILE_ENVELOPE,
      agentId: 'main',
      toolCallId: 'call_1',
      argumentsPart: '{"q":',
    },
  },
  {
    method: 'event.tool.result',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      toolCallId: 'call_1',
      name: 'Bash',
      isError: false,
    },
  },
  {
    method: 'event.turn.step.started',
    params: { ...DURABLE_ENVELOPE, turnId: 'turn-9ab3', step: 1 },
  },
  {
    method: 'event.turn.step.completed',
    params: {
      ...DURABLE_ENVELOPE,
      turnId: 'turn-9ab3',
      step: 1,
      usage: { inputOther: 10, output: 5, inputCacheRead: 0, inputCacheCreation: 0 },
    },
  },
  {
    method: 'event.turn.step.interrupted',
    params: {
      ...DURABLE_ENVELOPE,
      turnId: 'turn-9ab3',
      step: 2,
      reason: 'aborted',
      message: 'cancelled by user',
    },
  },
  {
    method: 'event.turn.step.retrying',
    params: {
      ...DURABLE_ENVELOPE,
      turnId: 'turn-9ab3',
      failedAttempt: 1,
      nextAttempt: 2,
      maxAttempts: 3,
      delayMs: 0,
    },
  },
  {
    method: 'event.agent.status.updated',
    params: {
      ...VOLATILE_ENVELOPE,
      agentId: 'main',
      phase: 'tool_call',
      model: 'kimi-k2',
      thinkingEffort: 'high',
      contextTokens: 120000,
      maxContextTokens: 200000,
      contextUsage: 0.6,
      planMode: true,
      swarmMode: false,
      usage: {
        byModel: {
          'kimi-k2': { inputOther: 10, output: 5, inputCacheRead: 2, inputCacheCreation: 1 },
        },
        currentTurn: { inputOther: 1, output: 1, inputCacheRead: 0, inputCacheCreation: 0 },
        total: { inputOther: 11, output: 6, inputCacheRead: 2, inputCacheCreation: 1 },
      },
    },
  },
  {
    method: 'event.background.task.started',
    params: {
      ...DURABLE_ENVELOPE,
      info: {
        kind: 'process',
        taskId: 'task-1a2b3c4d',
        description: 'release build',
        status: 'running',
        startedAt: 1767225600000,
        endedAt: null,
        command: 'cargo build --release',
      },
    },
  },
  {
    method: 'event.background.task.terminated',
    params: {
      ...DURABLE_ENVELOPE,
      info: {
        kind: 'process',
        taskId: 'task-1a2b3c4d',
        description: 'release build',
        status: 'completed',
        startedAt: 1767225600000,
        endedAt: 1767225615000,
        command: 'cargo build --release',
        exitCode: 0,
      },
    },
  },
  {
    method: 'event.background.task.terminated',
    params: {
      ...DURABLE_ENVELOPE,
      info: {
        kind: 'agent',
        taskId: 'task-3c4d5e6f',
        description: 'agent: refactor module',
        status: 'failed',
        startedAt: 1767225600000,
        endedAt: 1767225615000,
        stopReason: 'subagent panicked',
        agentId: '0192-coder-uuid',
        subagentType: 'coder',
      },
    },
  },
  {
    method: 'event.shell.started',
    params: {
      ...VOLATILE_ENVELOPE,
      commandId: 'cmd-echo',
      taskId: 'task-9d8e7f6a',
    },
  },
  {
    method: 'event.shell.output',
    params: {
      ...VOLATILE_ENVELOPE,
      commandId: 'cmd-echo',
      taskId: 'task-9d8e7f6a',
      update: { kind: 'stdout', text: 'hello\n' },
    },
  },
  {
    method: 'event.shell.completed',
    params: {
      ...DURABLE_ENVELOPE,
      commandId: 'cmd-echo',
      isError: false,
      taskId: 'task-9d8e7f6a',
    },
  },
  {
    method: 'event.approval.requested',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      requestId: '3f5e-req',
      toolName: 'Bash',
    },
  },
  {
    method: 'event.approval.resolved',
    params: { ...DURABLE_ENVELOPE, requestId: '3f5e-req', approved: true },
  },
  {
    method: 'event.question.requested',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      requestId: '7c1a-req',
      question: 'Deploy now?',
    },
  },
  {
    method: 'event.question.answered',
    params: { ...DURABLE_ENVELOPE, requestId: '7c1a-req', answer: 'yes' },
  },
  {
    method: 'event.question.dismissed',
    params: { ...DURABLE_ENVELOPE, requestId: '7c1a-req' },
  },
  {
    method: 'event.message.received',
    params: {
      ...DURABLE_ENVELOPE,
      agentId: 'main',
      role: 'user',
      contentSummary: 'deploy the site',
    },
  },
  {
    method: 'event.session.deleted',
    params: { ...DURABLE_ENVELOPE, reason: 'user_initiated' },
  },
  {
    method: 'event.session.updated',
    params: { ...DURABLE_ENVELOPE, change: 'resumed' },
  },
  {
    method: 'event.session.updated',
    params: {
      ...DURABLE_ENVELOPE,
      change: 'forked',
      sourceSessionId: 'session-0',
      newSessionId: 'session-2',
    },
  },
  {
    method: 'event.agent.created',
    params: { ...DURABLE_ENVELOPE, agentId: 'main' },
  },
  {
    method: 'event.agent.removed',
    params: { ...DURABLE_ENVELOPE, agentId: '0192-coder-uuid' },
  },
  // The server-scoped workspace lifecycle branches: the synthesized empty
  // session id and the volatile mark (no seq/epoch — the server-scope
  // events are fire-and-forget).
  {
    method: 'event.workspace.created',
    params: {
      sessionId: '',
      volatile: true,
      timestamp: '2026-01-01T00:00:00Z',
      workspace: {
        id: 'wd_project_abc123def456',
        root: '/Users/dev/project',
        name: 'project',
      },
    },
  },
  {
    method: 'event.workspace.updated',
    params: {
      sessionId: '',
      volatile: true,
      timestamp: '2026-01-01T00:00:00Z',
      workspace: {
        id: 'wd_project_abc123def456',
        root: '/Users/dev/project',
        name: 'renamed',
      },
    },
  },
  {
    method: 'event.workspace.deleted',
    params: {
      sessionId: '',
      volatile: true,
      timestamp: '2026-01-01T00:00:00Z',
      workspaceId: 'wd_project_abc123def456',
      root: '/Users/dev/project',
    },
  },
];

describe('rust engine event branch table integrity', () => {
  it('should carry exactly the 49 unique event-prefixed methods when the branch table is loaded', () => {
    expect(EVENT_METHODS).toHaveLength(49);
    expect(new Set(EVENT_METHODS).size).toBe(49);
    const tableKeys = Object.keys(EVENT_BRANCHES);
    expect(tableKeys).toHaveLength(49);
    expect(new Set(tableKeys).size).toBe(49);
    expect(tableKeys.toSorted()).toEqual([...EVENT_METHODS].toSorted());
    for (const method of EVENT_METHODS) {
      expect(method.startsWith('event.'), `${method} must keep the event. prefix`).toBe(true);
    }
  });

  it('should keep the mapped and extension branch families disjoint when the table is classified', () => {
    expect(MAPPED_EVENT_METHODS).toHaveLength(33);
    expect(EXTENSION_EVENT_METHODS).toHaveLength(16);
    const overlap = MAPPED_EVENT_METHODS.filter(method =>
      (EXTENSION_EVENT_METHODS as readonly string[]).includes(method),
    );
    expect(overlap, 'methods classified as both mapped and extension').toEqual([]);
    // The two families together are exactly the table's key set.
    expect(
      [...MAPPED_EVENT_METHODS, ...EXTENSION_EVENT_METHODS].toSorted(),
    ).toEqual(Object.keys(EVENT_BRANCHES).toSorted());
  });

  it('should parse the representative notification when a branch matches', () => {
    for (const { method, params } of BRANCH_EXAMPLES) {
      const parsed = EVENT_BRANCHES[method]!.safeParse(params);
      expect(
        parsed.success,
        `${method} must accept its representative notification: ${parsed.success ? '' : JSON.stringify(parsed.error.issues)}`,
      ).toBe(true);
    }
  });

  it('should reject stray keys when a branch parses a notification', () => {
    for (const { method, params } of BRANCH_EXAMPLES) {
      const parsed = EVENT_BRANCHES[method]!.safeParse({
        ...params,
        __unexpectedField: 'stray',
      });
      expect(
        parsed.success,
        `${method} must be a strict object (unknown keys fail, never silently stripped)`,
      ).toBe(false);
    }
  });
});

describe('event envelope discipline (engine-independent)', () => {
  it('should reject notifications violating the seq/epoch iff-durable rule when validating', () => {
    // Volatile with a stamped cursor — durable fields on a fire-and-forget
    // notification.
    const volatileWithSeq: EngineNotification = {
      method: 'event.assistant.delta',
      params: { ...VOLATILE_ENVELOPE, seq: 1, epoch: 'epoch-1', agentId: 'a-1', delta: 'hi' },
    };
    // Durable without a cursor — a log-ordered event that claims no
    // position (a subscriber could never resync it).
    const durableWithoutSeq: EngineNotification = {
      method: 'event.message.received',
      params: { ...DURABLE_ENVELOPE, seq: undefined, epoch: undefined, agentId: 'a-1', role: 'user', contentSummary: 'hi' },
    };
    // Durable with a seq but no epoch — the two cursor halves must appear
    // together.
    const seqWithoutEpoch: EngineNotification = {
      method: 'event.turn.started',
      params: { ...DURABLE_ENVELOPE, epoch: undefined, agentId: 'main', turnId: 'turn-9ab3' },
    };
    for (const notification of [volatileWithSeq, durableWithoutSeq, seqWithoutEpoch]) {
      expect(() => validateEventNotification(notification)).toThrow(
        /seq\/epoch ⇔ !volatile envelope rule/,
      );
    }
  });

  it('should reject a durable notification carrying seq 0 when validating', () => {
    // The relay stamps seq increment-before-use, so 0 is off-contract even
    // though the imperative cross-rule alone would accept it.
    const zeroSeq: EngineNotification = {
      method: 'event.turn.started',
      params: { ...DURABLE_ENVELOPE, seq: 0, agentId: 'main', turnId: 'turn-9ab3' },
    };
    expect(() => validateEventNotification(zeroSeq)).toThrow(/seq.*positive|violates the event/);
  });
});



// ---------------------------------------------------------------------------
// M1.1: registry reconciliation across the NAPI boundary
// ---------------------------------------------------------------------------

// The `embeddedMethodNames` export exists only in mock-llm builds of the
// native binary (mirri-node feature `mock-llm`); without it this group is
// skipped exactly like the engine-gated sweep below.
const hasRegistryExport = typeof binding.embeddedMethodNames === 'function';

describe.runIf(hasRegistryExport)('M1.1 mirri-proto registry reconciliation', () => {
  it('should expose exactly the fixture\'s 200 embedded methods when the native registry export is read', () => {
    const exported = binding.embeddedMethodNames!();
    // The export is sorted by contract; compare against the sorted fixture
    // set so the diff names every drift (arm added, renamed, or removed in
    // the Rust dispatch without a fixture/registry update).
    const fixtureMethods = RUST_ENGINE_METHODS
      .map(fixture => fixture.method)
      .toSorted();
    expect(exported).toHaveLength(192);
    expect(exported).toEqual(fixtureMethods);
  });

  it('should keep the native registry export sorted and duplicate-free', () => {
    const exported = binding.embeddedMethodNames!();
    expect([...exported].toSorted()).toEqual(exported);
    expect(new Set(exported).size).toBe(exported.length);
  });
});

// ---------------------------------------------------------------------------
// M4-b3: event projection vocabulary reconciliation across the NAPI boundary
// ---------------------------------------------------------------------------

// `embeddedEventMethods` mirrors `embeddedMethodNames`: it exists only in
// mock-llm builds of the native binary, so without it this group is skipped
// exactly like the other registry reconciliation above.
const hasEventRegistryExport = typeof binding.embeddedEventMethods === 'function';

describe.runIf(hasEventRegistryExport)('M4-b3 event projection reconciliation', () => {
  it("should expose exactly the branch table's 49 event methods when the native event registry export is read", () => {
    const exported = binding.embeddedEventMethods!();
    // Compare against the sorted branch-table key set so the diff names
    // every drift: a projection method added, renamed, or removed in the
    // Rust shared projection without a table update (or vice versa).
    const tableMethods = Object.keys(EVENT_BRANCHES).toSorted();
    expect(exported).toHaveLength(49);
    expect(exported).toEqual(tableMethods);
  });

  it('should keep the native event registry export sorted and duplicate-free', () => {
    const exported = binding.embeddedEventMethods!();
    expect([...exported].toSorted()).toEqual(exported);
    expect(new Set(exported).size).toBe(exported.length);
  });
});

// ---------------------------------------------------------------------------
// M0.2: klient zod schema validation for the `schema`-mode methods
// ---------------------------------------------------------------------------

/**
 * klient zod response schemas for the `schema`-mode fixture methods, keyed by
 * the dotted wire method name. These are the same schemas the klient factory
 * validates every call against (`packages/klient/src/contract/`), so a mock
 * engine response that trips one of them is wire-shape drift the real TS
 * client would reject at runtime.
 *
 * Coverage-strength caveat (M0.2): under the mock engine four of the five
 * array-typed methods exercise only the TOP-LEVEL shape —
 * `core.experimentalFeatures` is a hardcoded `json!([])` and
 * `plugin.list` / `plugin.commands` / `task.list` resolve to empty arrays —
 * so their element schemas are vacuously satisfied. Only `plugin.reload`'s
 * nested object shape (`{added, removed, errors}` of arrays) is genuinely
 * validated today; element-level contracts light up once M3's enriched
 * fixtures serve non-empty data.
 */
const KLIENT_RESPONSE_SCHEMAS: Readonly<Record<string, z.ZodType>> = {
  // `agent.activity` → klient `agentActivityView.state`
  // (agentActivityStateSchema). Expected-idle under the mock engine (no
  // turn in flight during the sweep, so the optional turn/lastTurn keys
  // are absent).
  'agent.activity': agentActivityStateSchema,
  // `agent.getEffectiveThinkingLevel` → klient `agentProfileContract`
  // .getEffectiveThinkingLevel (a plain string).
  'agent.getEffectiveThinkingLevel': z.string(),
  // `agent.listCommands` → klient `agentRpcContract.listCommands`
  // (AgentCommandInfo[]). Expected-vacuous under the mock engine (no plugin
  // manager attached, so the command registry is empty).
  'agent.listCommands': z.array(agentCommandInfoSchema),
  // `core.echo` → boots the same text back (`{echo}`) — the M5 drill row.
  'core.echo': z.object({ echo: z.string() }),
  // `getExperimentalFeatures` → `flagsContract.explainAll`
  'core.experimentalFeatures': z.array(experimentalFeatureStateSchema),
  // `checkUpdates` → `pluginsContract.checkUpdates`
  // (PluginUpdateStatus[]). Expected-vacuous under the mock engine (no
  // plugin manager attached, so there is nothing to check).
  'plugin.checkUpdates': z.array(pluginUpdateStatusSchema),
  // `listPluginCommands` → `pluginsContract.listPluginCommands`
  'plugin.commands': z.array(pluginCommandDefSchema),
  // `listPlugins` → `pluginsContract.listPlugins`
  'plugin.list': z.array(pluginSummarySchema),
  // `reloadPlugins` → `pluginsContract.reloadPlugins`
  'plugin.reload': reloadSummarySchema,
  // Embedded task.list (v1 alias getBackground) ↔ klient
  // `agentTaskContract.list`. Remains expected-vacuous under the mock
  // engine (empty array — no RPC path registers tasks in the fixture).
  // The element schema now mirrors the rust `AgentTask` serialization
  // (TSV row 80), so the kind/stopped drift noted in earlier batches is
  // resolved by M3-c batch 8.
  'task.list': z.array(agentTaskInfoSchema),
  // B5-a: the klient bootstrapService reads (`envContract` field outputs —
  // the twelve stringReads and the clientIdentity object shape).
  'env.arch': envContract.arch.output,
  'env.blobsDir': envContract.blobsDir.output,
  'env.cacheDir': envContract.cacheDir.output,
  'env.clientIdentity': envContract.clientIdentity.output,
  'env.clientVersion': envContract.clientVersion.output,
  'env.configPath': envContract.configPath.output,
  'env.cwd': envContract.cwd.output,
  'env.homeDir': envContract.homeDir.output,
  'env.logsDir': envContract.logsDir.output,
  'env.osHomeDir': envContract.osHomeDir.output,
  'env.platform': envContract.platform.output,
  'env.sessionsDir': envContract.sessionsDir.output,
  'env.storeDir': envContract.storeDir.output,
  // B5-a: the klient flagService reads (the explain output is the klient
  // maybe(experimentalFeatureStateSchema) union — null for an unknown id).
  'flag.enabled': z.boolean(),
  'flag.enabledIds': z.array(z.string()),
  'flag.explain': z.union([experimentalFeatureStateSchema, z.null()]),
  'flag.explainAll': z.array(experimentalFeatureStateSchema),
  'flag.snapshot': z.record(z.string(), z.boolean()),
  // B5-a: the klient hostFolderBrowser reads (snake_case wire shapes).
  'fs.browse': hostFsContract.browse.output,
  'fs.home': hostFsContract.home.output,
  // P6 B4a: the models.dev directory browse (pruned catalog entry rows —
  // the empty array under the mock engine with no catalog loaded).
  'providerDiscovery.listCatalog': providerDiscoveryContract.listCatalog.output,
  // B5-b: the klient configService section reads (getAll is the section
  // record; inspect is the layered snapshot — the rust engine answers only
  // the value/userValue layers).
  'config.getAll': configContract.getAll.output,
  'config.inspect': configContract.inspect.output,
  // B5-b: the klient modelService record read (maybe — an unknown id is
  // JSON null under the mock engine, which has no model aliases).
  'model.get': z.union([modelConfigSchema, z.null()]),
  // B5-b: the klient providerService reads (get is maybe; list is the
  // id → record map, empty under the mock engine when no config.toml
  // holds providers).
  'provider.get': z.union([providerConfigSchema, z.null()]),
  'provider.list': z.record(z.string(), providerConfigSchema),
  // Batch-3 P1: the klient mcpService / mcpConfigService reads (list is
  // the bare McpServerInfo[] array; config.list is the layered-view entry
  // array — the stored config verbatim per A5).
  'mcp.list': z.array(mcpServerInfoSchema),
  'mcp.config.list': z.array(mcpConfigEntrySchema),
  // Batch-4 P2: the no-workspace global view (the same klient server-info
  // entry schema) and the fresh-connect config test's result shape.
  'mcp.status': z.array(mcpServerInfoSchema),
  'mcp.config.test': mcpTestResultSchema,
  // M5: the resolved-value preview (the A5 sole sanctioned outlet — the
  // expanded config plus the undefined-variable names).
  'mcp.config.preview': mcpConfigPreviewResultSchema,
  // M1: the single-server tool listing with per-tool filter state (the
  // explicit not-connected shape answers unknown names).
  'mcp.tools': mcpToolsResultSchema,
  // M6: the idempotent reconcile summary (the all-zero steady state
  // under the pool-less mock engine).
  'mcp.sync': mcpSyncResultSchema,
};

/**
 * `schema`-mode methods with no klient zod schema: klient has no global MCP
 * tool, session-warning, or skill-summary contract. Their wire shapes stay
 * guarded by the explicit assertions in the
 * 855f57ef regression group below until klient grows the matching contracts.
 */
const METHODS_WITHOUT_KLIENT_SCHEMA: ReadonlySet<string> = new Set([
  'mcp.global.tools',
  'session.warnings',
  'skill.list',
  // The session-retention management wire is rust-first: no klient RPC
  // counterpart exists (the TS surface for the management page is the
  // web UI's own API layer), so the getPolicy shape is asserted structurally
  // here instead of against a klient zod schema.
  'retention.getPolicy',
  // ADR-010 Task 6: the manual reload face is rust-first (no klient
  // contract); the {workspaces: [{workspace_id, added, removed, updated,
  // watching}]} shape is pinned by the rust embedded tests and the
  // cross-backend contract suite.
  'skill.reload',
]);

/**
 * Validate a response against the klient zod contract for `method`.
 *
 * A response that fails `safeParse` fails the test with the full zod issue
 * list; a `schema`-mode method that is neither mapped nor documented as a
 * known gap fails loudly so the mapping table can never silently rot.
 */
function validateSchemaContract(method: string, result: unknown): void {
  const schema = KLIENT_RESPONSE_SCHEMAS[method];
  if (schema) {
    const parsed = schema.safeParse(result);
    if (!parsed.success) {
      throw new Error(
        `${method} violates the klient zod response contract: ${JSON.stringify(
          parsed.error.issues,
          null,
          2,
        )}`,
      );
    }
    return;
  }
  if (METHODS_WITHOUT_KLIENT_SCHEMA.has(method)) {
    return;
  }
  throw new Error(
    `no klient zod schema mapped for ${method}: add it to KLIENT_RESPONSE_SCHEMAS or document the gap in METHODS_WITHOUT_KLIENT_SCHEMA`,
  );
}

// ---------------------------------------------------------------------------
// Contract e2e sweep (engine-gated)
// ---------------------------------------------------------------------------

describe.runIf(hasMockLlm)('Rust engine contract E2E — embedded dotted method surface', () => {
  let engine: MirriEngineNative;
  let setupSessionId: string;
  let setupWorkspaceId: string;
  let homeDir: string | undefined;
  let committed: Record<string, WireShape>;
  const updated: Record<string, WireShape> = {};

  /** Substitute `$setupSessionId` / `$setupWorkspaceId` placeholders with the
   * live session/workspace ids (the W6 `$setupWorkspaceId` face: the mock
   * engine's single registration is the cwd-derived key, resolved through
   * `workspace.list` rather than hardcoded). */
  function resolveParams(
    params: Readonly<Record<string, unknown>> | undefined,
  ): Record<string, unknown> {
    const resolved: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(params ?? {})) {
      resolved[key] =
        value === SETUP_SESSION_ID
          ? setupSessionId
          : value === SETUP_WORKSPACE_ID
            ? setupWorkspaceId
            : value;
    }
    return resolved;
  }

  function callFixture(fixture: RustEngineMethodFixture): Promise<unknown> {
    return engine.callMethod(fixture.method, resolveParams(fixture.params));
  }

  beforeAll(async () => {
    committed = readCommittedSnapshots();
    // One spare mock reply in case anything triggers a turn (nothing should).
    // The engine runs against an ISOLATED temp home (W6): the mock engine
    // previously resolved the developer's real `~/.mirri-code` for every
    // home-relative read — a latent hazard now that the sweep carries
    // mutating workspace calls (the rename below writes
    // `<home>/workspaces.json`).
    homeDir = mkdtempSync(join(tmpdir(), 'mirri-contract-e2e-'));
    engine = binding.createEngineWithMockLlm!(['contract-e2e'], homeDir);
    // The mock engine's config seeds exactly one model alias and no
    // default (spec 11 R4 — session.create resolves modelId or the
    // config's default_model, never a silent fallback), so the setup
    // create pins the seeded `contract-e2e-model` alias explicitly.
    const created = (await engine.callMethod('session.create', {
      title: 'contract-e2e',
      modelId: 'contract-e2e-model',
    })) as { id: string };
    setupSessionId = created.id;
    if (!setupSessionId.startsWith('session-')) {
      throw new Error(
        `session.create returned an unexpected id: ${setupSessionId}`,
      );
    }
    const workspaces = (await engine.callMethod('workspace.list', {})) as Array<{
      id: string;
    }>;
    if (workspaces.length !== 1 || typeof workspaces[0]?.id !== 'string') {
      throw new Error(
        `workspace.list returned an unexpected surface: ${JSON.stringify(workspaces)}`,
      );
    }
    setupWorkspaceId = workspaces[0]!.id;
  });

  afterAll(async () => {
    if (UPDATE_SNAPSHOTS) {
      const sorted = Object.fromEntries(
        Object.keys(updated)
          .toSorted()
          .map(key => [key, updated[key]]),
      );
      writeFileSync(SNAPSHOT_FILE, `${JSON.stringify(sorted, null, 2)}\n`);
      console.info(
        `[rust-engine-contract-e2e] rewrote shape snapshots for ${Object.keys(sorted).length} methods → ${fileURLToPath(SNAPSHOT_FILE)}`,
      );
    }
    await engine.dispose();
    if (homeDir) {
      rmSync(homeDir, { recursive: true, force: true });
    }
  });

  // --- data-driven sweep over the 96-method fixture ----------------------

  for (const fixture of RUST_ENGINE_METHODS) {
    switch (fixture.mode) {
      case 'snapshot': {
        it(`should keep the wire shape of ${fixture.method} stable when the embedded engine serves the dotted protocol`, async () => {
          const shape = normalizeWireShape(await callFixture(fixture));
          if (UPDATE_SNAPSHOTS) {
            updated[fixture.method] = shape;
            return;
          }
          expect(
            committed[fixture.method],
            `no committed shape snapshot for ${fixture.method} — run UPDATE_SNAPSHOTS=1 pnpm --filter @mirri-ai/mirri-code-sdk exec vitest run test/rust-engine-contract-e2e.test.ts to generate it`,
          ).toBeDefined();
          expect(
            shape,
            `wire shape drift for ${fixture.method}: expected the committed snapshot, got ${JSON.stringify(shape)}`,
          ).toEqual(committed[fixture.method]);
        });
        break;
      }
      case 'schema': {
        it(`should route ${fixture.method} through the klient schema contract branch when the engine serves the v2 wire shape`, async () => {
          const result = await callFixture(fixture);
          expect(
            result,
            `${fixture.method} should respond on the v2 wire shape`,
          ).toBeDefined();
          validateSchemaContract(fixture.method, result);
        });
        break;
      }
      case 'error': {
        it(`should reject ${fixture.method} with the ${fixture.expectDomainCode} domain code when the embedded engine serves the dotted protocol`, async () => {
          const caught = await callFixture(fixture).then(
            () => undefined,
            (error: unknown) => error,
          );
          expect(caught, `${fixture.method} must reject`).toBeInstanceOf(Error);
          // The raw NAPI rejection carries the structured suffix on its
          // message (M-3); decode it the same way the SDK client does.
          const decoded = decodeRustEngineError(caught);
          expect(
            decoded,
            `${fixture.method} must carry the [mirri-rpc-error …] suffix across the NAPI boundary`,
          ).toBeInstanceOf(RustEngineRpcError);
          const rpcError = decoded as RustEngineRpcError;
          if (fixture.expectCode !== undefined) {
            expect(rpcError.code, `${fixture.method} must carry the expected JSON-RPC code`).toBe(
              fixture.expectCode,
            );
          }
          expect(
            rpcError.domainCode,
            `${fixture.method} must carry the expected domain code`,
          ).toBe(fixture.expectDomainCode);
        });
        break;
      }
      case 'skip': {
        it.skip(
          `should stay out of the contract sweep when ${fixture.reason} (${fixture.method})`,
        );
        break;
      }
    }
  }

  // --- 855f57ef regression group ------------------------------------------
  // On 2026-09-02 the embedded engine returned `{key:[…]}` single-key
  // wrappers where the TS contract expects bare arrays, crashing the TUI at
  // startup. These assertions are the guard that would have caught it.
  // M0.2-a: the mock-llm factories attach an isolated PluginManager, so
  // `plugin.reload` resolves on the real ReloadSummary branch instead of
  // rejecting with "plugin support not configured".

  describe('855f57ef regression — v2 wire contract', () => {
    // model.list / model.listProviders joined the bare-array family in
    // M3-c (the klient modelResolver.listModels/listProviders contracts —
    // the embedded engine previously wrapped them as {models}/{providers}).
    const BARE_ARRAY_METHODS = [
      'mcp.list',
      'mcp.global.tools',
      'model.list',
      'model.listProviders',
      'plugin.list',
      'plugin.commands',
      'core.experimentalFeatures',
      'session.warnings',
      'task.list',
      'skill.list',
    ] as const;

    it.each(BARE_ARRAY_METHODS)(
      'should return a bare array for %s when the native engine serves the v2 wire contract',
      async (method) => {
        const fixture = RUST_ENGINE_METHODS.find(
          entry => entry.method === method,
        );
        expect(fixture, `${method} must exist in the method fixture`).toBeDefined();

        const result = await callFixture(fixture!);

        // The bare-array assertion subsumes the historical single-key
        // `{key:[…]}` wrapper regression: a wrapper is not an array, so one
        // check with the full context in its message is enough.
        expect(
          Array.isArray(result),
          `${method} must resolve to a bare array, not a single-key {key:[…]} wrapper object (855f57ef regression: the TS contract expects a bare array and the TUI crashed on the wrapped shape)`,
        ).toBe(true);
      },
    );

    it('should return a ReloadSummary object with keys {added, removed, errors} for plugin.reload when the native engine serves the v2 wire contract', async () => {
      const fixture = RUST_ENGINE_METHODS.find(
        entry => entry.method === 'plugin.reload',
      )!;
      // M0.2-a: the mock-llm factories attach an isolated PluginManager over
      // an empty temp plugins dir, so plugin.reload serves the real
      // ReloadSummary branch instead of rejecting with
      // "plugin support not configured".
      const result = await callFixture(fixture);
      expect(result).toBeTypeOf('object');
      expect(result).not.toBeNull();
      expect(
        Object.keys(result as object).toSorted(),
        'plugin.reload must resolve to the ReloadSummary shape fixed in 855f57ef',
      ).toEqual(['added', 'errors', 'removed']);
      for (const key of ['added', 'removed', 'errors']) {
        expect(
          Array.isArray((result as Record<string, unknown>)[key]),
          `plugin.reload.${key} must be an array`,
        ).toBe(true);
      }
    });
  });
});

// ---------------------------------------------------------------------------
// M4-b3: event projection sweep (engine-gated)
// ---------------------------------------------------------------------------

/** One notification as delivered by `MirriEngineNative.onEvent`. */
type EngineNotification = { method: string; params: unknown };

/**
 * Validate one engine notification against the event.* contract:
 *
 * - the method must be a branch-table key — after the b3 flip the event
 *   channel carries only projected `event.*` notifications, so any other
 *   method is cross-boundary drift (hard fail naming the method);
 * - the FULL params object (envelope fields merged with the payload) must
 *   satisfy the branch's strict schema — one safeParse covers both, since
 *   every branch is `EVENT_ENVELOPE_SCHEMA` extended with the payload
 *   fields;
 * - the envelope cross-rule must hold: `seq`/`epoch` are present if and
 *   only if the event is durable (`volatile === false`).
 *
 * Failures throw with the full zod issue list plus the raw notification,
 * mirroring `validateSchemaContract`'s reporting style.
 */
function validateEventNotification(notification: EngineNotification): void {
  const branch = EVENT_BRANCHES[notification.method as RustEngineEventMethod];
  if (branch === undefined) {
    throw new Error(
      `unknown engine event method "${notification.method}" (the event channel carries only projected event.* methods; add the method to the EVENT_BRANCHES table or fix the projection) — notification: ${JSON.stringify(notification)}`,
    );
  }
  const parsed = branch.safeParse(notification.params);
  if (!parsed.success) {
    throw new Error(
      `${notification.method} violates the event.* branch contract: ${JSON.stringify(parsed.error.issues, null, 2)} — notification: ${JSON.stringify(notification)}`,
    );
  }
  // Envelope discipline the schemas cannot express on their own: seq/epoch
  // present ⇔ !volatile. Both must appear or disappear together.
  const params = parsed.data as Record<string, unknown>;
  const durable = params['volatile'] === false;
  const hasSeq = params['seq'] !== undefined;
  const hasEpoch = params['epoch'] !== undefined;
  if (durable !== (hasSeq && hasEpoch) || hasSeq !== hasEpoch) {
    throw new Error(
      `${notification.method} breaks the seq/epoch ⇔ !volatile envelope rule (volatile=${String(params['volatile'])}, seq=${String(params['seq'])}, epoch=${String(params['epoch'])}) — notification: ${JSON.stringify(notification)}`,
    );
  }
}

/**
 * Collect the engine's notifications for one scenario. Notifications are
 * pushed raw (never validated inside the NAPI callback — a throw there is
 * swallowed); `assertContract` validates the whole buffer afterwards.
 */
class NotificationRecorder {
  readonly notifications: EngineNotification[] = [];

  constructor(engine: MirriEngineNative) {
    engine.onEvent((_err, notification) => {
      this.notifications.push(notification);
    });
  }

  /** Validate every collected notification against the event.* contract. */
  assertContract(): void {
    if (this.notifications.length === 0) {
      throw new Error('at least one engine notification must have been observed');
    }
    for (const notification of this.notifications) {
      validateEventNotification(notification);
    }
  }

  /** The params of every collected notification of one method. */
  paramsOf(method: string): Record<string, unknown>[] {
    return this.notifications
      .filter(notification => notification.method === method)
      .map(notification => notification.params as Record<string, unknown>);
  }
}

/** Wait until a notification matching `predicate` has been collected. */
async function waitForEvent(
  recorder: NotificationRecorder,
  predicate: (notification: EngineNotification) => boolean,
): Promise<EngineNotification> {
  let matched: EngineNotification | undefined;
  await vi.waitFor(() => {
    matched = recorder.notifications.find(predicate);
    expect(
      matched,
      `expected a matching notification; methods observed so far: ${recorder.notifications
        .map(notification => notification.method)
        .join(', ')}`,
    ).toBeDefined();
  }, 20_000);
  return matched!;
}

/** Create a session on the given mock engine and return its id. The create
 * pins the mock config's seeded `contract-e2e-model` alias — the engine has
 * no default model (spec 11 R4), so a model-less create rejects. */
async function createSessionId(
  engine: MirriEngineNative,
  params: Record<string, unknown>,
): Promise<string> {
  const created = (await engine.callMethod('session.create', {
    modelId: 'contract-e2e-model',
    ...params,
  })) as {
    id: string;
  };
  expect(created.id.startsWith('session-'), `unexpected session id: ${created.id}`).toBe(true);
  return created.id;
}

describe.runIf(hasMockLlm)('Rust engine event projection E2E — event.* notification surface', () => {
  it('should emit message.received, assistant deltas, and a completed turn.ended when a wait-mode message is answered', async () => {
    const engine = binding.createEngineWithMockLlm!(['Wait mode reply']);
    try {
      const recorder = new NotificationRecorder(engine);
      const sessionId = await createSessionId(engine, { title: 'event sweep wait' });

      await engine.callMethod('message.send', {
        session_id: sessionId,
        text: 'Blocking ask',
        mode: 'wait',
      });
      const ended = await waitForEvent(
        recorder,
        notification => notification.method === 'event.turn.ended',
      );
      expect((ended.params as Record<string, unknown>)['reason']).toBe('completed');
      // CR5: both turn brackets carry the turn-owning agent's identity —
      // the main agent's `"main"` spelling, never a UUID conversion.
      expect((ended.params as Record<string, unknown>)['agentId']).toBe('main');
      const turnStartedParams = recorder.paramsOf('event.turn.started');
      expect(turnStartedParams, 'event.turn.started must be emitted').toHaveLength(1);
      expect(turnStartedParams[0]!['agentId']).toBe('main');

      recorder.assertContract();

      // The user-message echo precedes the first streamed delta.
      const methods = recorder.notifications.map(notification => notification.method);
      expect(methods.indexOf('event.message.received')).toBeGreaterThanOrEqual(0);
      expect(methods.indexOf('event.message.received')).toBeLessThan(
        methods.indexOf('event.assistant.delta'),
      );
      expect(
        recorder.paramsOf('event.assistant.delta').length,
        'at least one assistant delta must have streamed',
      ).toBeGreaterThan(0);
    } finally {
      await engine.dispose();
    }
  });

  it('should emit a delta sequence under envelope discipline when a fire-mode message streams', async () => {
    const replyText = 'ABCDEFGH';
    const engine = binding.createEngineWithMockLlm!([replyText]);
    try {
      const recorder = new NotificationRecorder(engine);
      const sessionId = await createSessionId(engine, { title: 'event sweep fire' });

      await engine.callMethod('message.send', {
        session_id: sessionId,
        text: 'count chunks',
        mode: 'fire',
      });
      await waitForEvent(
        recorder,
        notification => notification.method === 'event.turn.ended',
      );

      recorder.assertContract();

      // The mock streams one delta per character, in order, volatile.
      const deltas = recorder.paramsOf('event.assistant.delta');
      expect(deltas.map(params => params['delta']).join('')).toBe(replyText);
    } finally {
      await engine.dispose();
    }
  });

  it('should emit tool.call.started and tool.result when a turn executes a tool', async () => {
    const engine = binding.createEngineWithScriptedMockLlm!([
      { toolCalls: [{ name: 'Bash', args: { command: 'echo hello' } }] },
      { text: 'Done', toolCalls: [] },
    ]);
    try {
      const recorder = new NotificationRecorder(engine);
      const sessionId = await createSessionId(engine, {
        title: 'event sweep tool',
        permissionMode: 'yolo',
      });

      await engine.callMethod('message.send', {
        session_id: sessionId,
        text: 'run echo',
        mode: 'fire',
      });
      await waitForEvent(
        recorder,
        notification => notification.method === 'event.turn.ended',
      );

      recorder.assertContract();

      const started = recorder.paramsOf('event.tool.call.started');
      expect(started, 'event.tool.call.started must be emitted').toHaveLength(1);
      expect(started[0]!['name']).toBe('Bash');
      expect(started[0]!['args']).toEqual({ command: 'echo hello' });
      // CR5: the tool frames are attributed to the turn-owning agent — the
      // same `"main"` spelling the turn brackets carry.
      expect(started[0]!['agentId']).toBe('main');
      const results = recorder.paramsOf('event.tool.result');
      expect(results, 'event.tool.result must be emitted').toHaveLength(1);
      expect(results[0]!['isError']).toBe(false);
      expect(results[0]!['name']).toBe('Bash');
      expect(results[0]!['agentId']).toBe('main');
      // The turn.step family routes by the same wire turn id the
      // turn.started bracket announced (the T5 field-level alignment).
      const turnStarted = recorder.paramsOf('event.turn.started');
      const stepStarted = recorder.paramsOf('event.turn.step.started');
      expect(turnStarted, 'event.turn.started must be emitted').toHaveLength(1);
      expect(turnStarted[0]!['agentId']).toBe('main');
      expect(stepStarted.length).toBeGreaterThan(0);
      for (const step of stepStarted) {
        expect(step['turnId']).toBe(turnStarted[0]!['turnId']);
      }
    } finally {
      await engine.dispose();
    }
  });

  it('should emit goal.updated with the model actor when the model calls the goal tools', async () => {
    const engine = binding.createEngineWithScriptedMockLlm!([
      { toolCalls: [{ name: 'CreateGoal', args: { objective: 'Ship it' } }] },
      { toolCalls: [{ name: 'SetGoalBudget', args: { turns: 5 } }] },
      { text: 'Done', toolCalls: [] },
    ]);
    try {
      const recorder = new NotificationRecorder(engine);
      // `auto` (not `yolo`): the goal-start review policy asks for
      // CreateGoal in every mode except Auto.
      const sessionId = await createSessionId(engine, {
        title: 'event sweep goal tools',
        permissionMode: 'auto',
      });

      await engine.callMethod('message.send', {
        session_id: sessionId,
        text: 'manage your goal',
        mode: 'fire',
      });
      await waitForEvent(
        recorder,
        notification => notification.method === 'event.turn.ended',
      );

      recorder.assertContract();

      const goalEvents = recorder.paramsOf('event.goal.updated');
      // Only the model-actor mutations are per-tool-call; a runtime-actor
      // pause event may also arrive when the goal driver's follow-up turn
      // exhausts the scripted mock (a settling race the sweep must not
      // depend on — filter by actor, not by total count).
      const modelEvents = goalEvents.filter(
        (event) => (event['change'] as Record<string, unknown>)['actor'] === 'model',
      );
      expect(
        modelEvents,
        'event.goal.updated must be emitted per model mutation',
      ).toHaveLength(2);
      expect(modelEvents[0]!['change']).toMatchObject({
        kind: 'lifecycle',
        status: 'active',
        actor: 'model',
      });
      expect(modelEvents[0]!['snapshot']).toMatchObject({
        goalId: 'goal-1',
        objective: 'Ship it',
        status: 'active',
      });
      // A budget change is a lifecycle change that leaves the status
      // untouched — no `status` key on the change, the budget rides the
      // snapshot.
      expect(modelEvents[1]!['change']).toMatchObject({
        kind: 'lifecycle',
        actor: 'model',
      });
      // `change`/`snapshot` are `unknown` off the wire — assert through
      // typed locals so the noUncheckedIndexedAccess indexing stays sound.
      const budgetChange = modelEvents[1]!['change'] as Record<string, unknown>;
      expect(budgetChange['status']).toBeUndefined();
      const budgetSnapshot = modelEvents[1]!['snapshot'] as Record<string, unknown>;
      expect(budgetSnapshot['budget']).toMatchObject({ turns: 5 });
    } finally {
      await engine.dispose();
    }
  });

  it('should emit a failed turn.ended with an error object when an in-flight fire-mode turn is cancelled', async () => {
    // The turn parks on a manual-permission approval.ask that the test
    // never answers, so the in-flight window is deterministic and
    // `message.abort` (not a timing race) ends the turn.
    const engine = binding.createEngineWithScriptedMockLlm!([
      { toolCalls: [{ name: 'Bash', args: { command: 'echo hello' } }] },
    ]);
    try {
      const approvals: { id: number; params: unknown }[] = [];
      engine.onReverseRpc((_err, request) => {
        if (request.method === 'approval.ask') approvals.push(request);
        // Deliberately unanswered: the turn stays parked on the gate.
      });
      const recorder = new NotificationRecorder(engine);
      const sessionId = await createSessionId(engine, {
        title: 'event sweep cancel',
        permissionMode: 'manual',
      });

      const sent = (await engine.callMethod('message.send', {
        session_id: sessionId,
        text: 'run echo',
        mode: 'fire',
      })) as { turn_id: string };
      await vi.waitFor(() => {
        expect(approvals, 'the turn must park on an approval.ask').toHaveLength(1);
      }, 20_000);

      await engine.callMethod('message.abort', {
        session_id: sessionId,
        turn_id: sent.turn_id,
      });
      const ended = await waitForEvent(
        recorder,
        notification =>
          notification.method === 'event.turn.ended' &&
          (notification.params as Record<string, unknown>)['reason'] === 'failed',
      );

      const params = ended.params as Record<string, unknown>;
      expect(params['turnId']).toBe(sent.turn_id);
      // The structured error object reports the real domain code of the
      // engine failure — a user cancellation, not a generic internal — with
      // its retryability verdict (aborts are terminal, never retried).
      expect(params['error']).toMatchObject({
        code: 'abort.user_cancelled',
        retryable: false,
      });
      expect((params['error'] as Record<string, unknown>)['message']).toContain('abort');

      recorder.assertContract();
    } finally {
      await engine.dispose();
    }
  });

  it('should emit session.deleted when the session is closed', async () => {
    const engine = binding.createEngineWithMockLlm!(['spare']);
    try {
      const recorder = new NotificationRecorder(engine);
      const sessionId = await createSessionId(engine, { title: 'event sweep close' });

      await engine.callMethod('session.close', { session_id: sessionId });
      const deleted = await waitForEvent(
        recorder,
        notification => notification.method === 'event.session.deleted',
      );

      expect((deleted.params as Record<string, unknown>)['reason']).toBe('user_initiated');
      recorder.assertContract();
    } finally {
      await engine.dispose();
    }
  });
});
