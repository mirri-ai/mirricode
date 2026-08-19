/**
 * Unit tests for `foldAgentWireReplay` — the v1 fold of a v2 agent's
 * `wire.jsonl` journal that rebuilds the `{ replay, toolStore }` pair for the
 * session-replay surface.
 *
 * The wire journal is the v1 record vocabulary in JSONL form (see
 * `agent-core-v2/src/wire/record.ts`); the fold hands it to a throwaway v1
 * `Agent` whose restore pipeline owns the v1 replay semantics. These tests pin
 * the fold's contract:
 *
 *   - a valid journal rehydrates the accumulated replay records and the final
 *     tool-store snapshot;
 *   - a missing/empty/corrupt journal degrades to an empty fold, never an
 *     exception — the session must resume even when the journal is garbage.
 */

import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { AGENT_WIRE_PROTOCOL_VERSION, type AgentReplayRecord, type ContextMessage } from '@mirri-ai/agent-core';
import { afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';

import { foldAgentWireReplay } from '#/v2/resume-replay';

// ---------------------------------------------------------------------------
// Journal seeds — hand-written v1-vocabulary journal lines shared by the
// fold-heavy cases (built once in beforeAll, not per test).
// ---------------------------------------------------------------------------

const USER_MESSAGE: ContextMessage = {
  role: 'user',
  content: [{ type: 'text', text: 'write the resume fold test' }],
  toolCalls: [],
};

const FIRST_TODO_STATE = [
  { title: 'write fold unit test', status: 'in_progress' },
  { title: 'run the suite', status: 'pending' },
];

const FINAL_TODO_STATE = [
  { title: 'write fold unit test', status: 'done' },
  { title: 'run the suite', status: 'done' },
];

const ASSISTANT_PLAN_MESSAGE: ContextMessage = {
  role: 'assistant',
  content: [{ type: 'text', text: 'entering plan mode' }],
  toolCalls: [],
};

interface JournalSeed {
  /** A minimal valid journal: message, two store writes, plan-mode enter. */
  readonly golden: readonly unknown[];
  /** A journal that ends mid-JSON on the final line (crash before newline). */
  readonly truncatedTail: readonly unknown[];
  /** A journal with a corrupt line in the middle and valid lines after. */
  readonly corruptMiddle: readonly unknown[];
  /** A journal with a well-delimited but malformed final line. */
  readonly corruptTail: readonly unknown[];
}

let seed: JournalSeed;

beforeAll(() => {
  const metadata = {
    type: 'metadata',
    protocol_version: AGENT_WIRE_PROTOCOL_VERSION,
    created_at: 1_000,
  };
  const userMessage = {
    type: 'context.append_message',
    time: 100,
    message: USER_MESSAGE,
  };
  const planMessage = {
    type: 'context.append_message',
    time: 300,
    message: ASSISTANT_PLAN_MESSAGE,
  };
  const todoWriteOne = {
    type: 'tools.update_store',
    time: 200,
    key: 'todo',
    value: FIRST_TODO_STATE,
  };
  const todoWriteTwo = {
    type: 'tools.update_store',
    time: 201,
    key: 'todo',
    value: FINAL_TODO_STATE,
  };
  const planEnter = { type: 'plan_mode.enter', time: 400, id: 'plan-1' };

  seed = {
    golden: [metadata, userMessage, todoWriteOne, todoWriteTwo, planMessage, planEnter],
    // The final write crashed mid-flush: the last line is an unterminated
    // `context.append_message` with no trailing newline.
    truncatedTail: [
      metadata,
      userMessage,
      '{ "type": "context.append_message", "message": { "role": "assistant"',
    ],
    // Corruption in the middle: valid records both before and after the bad line.
    corruptMiddle: [metadata, userMessage, 'this is not valid json', planEnter],
    // The final line is delimited but its JSON is broken.
    corruptTail: [
      metadata,
      userMessage,
      todoWriteOne,
      '{ "type": "plan_mode.enter", "id":',
    ],
  };
});

describe('resume-replay foldAgentWireReplay', () => {
  let workDir: string;

  beforeEach(async () => {
    workDir = await mkdtemp(join(tmpdir(), 'mirri-fold-'));
  });

  afterEach(async () => {
    await rm(workDir, { recursive: true, force: true });
  });

  const wirePath = (): string => join(workDir, 'wire.jsonl');

  async function foldJournal(lines: readonly unknown[]): Promise<Awaited<ReturnType<typeof foldAgentWireReplay>>> {
    const content = lines.map((line) =>
      typeof line === 'string' ? line : JSON.stringify(line),
    ).join('\n');
    await writeFile(wirePath(), content, 'utf-8');
    return foldAgentWireReplay(wirePath());
  }

  it('given a minimal valid journal, when folded, should rehydrate message/plan_updated replay records and the final tool-store write', async () => {
    const fold = await foldJournal(seed.golden);

    const expectedReplay: AgentReplayRecord[] = [
      { type: 'message', time: 100, message: USER_MESSAGE },
      { type: 'message', time: 300, message: ASSISTANT_PLAN_MESSAGE },
      { type: 'plan_updated', time: 400, enabled: true },
    ];
    expect(fold.replay).toEqual(expectedReplay);
    // The second `tools.update_store` (the final write) wins the snapshot.
    expect(fold.toolStore).toEqual({ todo: FINAL_TODO_STATE });
  });

  it('given an empty journal file, when folded, should return an empty replay and tool store', async () => {
    await writeFile(wirePath(), '', 'utf-8');

    await expect(foldAgentWireReplay(wirePath())).resolves.toEqual({ replay: [], toolStore: {} });
  });

  it('given a missing journal file, when folded, should degrade to an empty fold instead of throwing', async () => {
    const missing = join(workDir, 'does-not-exist.jsonl');

    await expect(foldAgentWireReplay(missing)).resolves.toEqual({ replay: [], toolStore: {} });
  });

  it('given a journal with a corrupt middle line, when folded, should degrade to an empty fold instead of throwing', async () => {
    const fold = await foldJournal(seed.corruptMiddle);

    expect(fold).toEqual({ replay: [], toolStore: {} });
  });

  it('given a journal truncated mid-JSON on the final line, when folded, should drop the broken tail line and keep the earlier records', async () => {
    const fold = await foldJournal(seed.truncatedTail);

    expect(fold.replay).toEqual([{ type: 'message', time: 100, message: USER_MESSAGE }]);
  });

  it('given a journal whose final line is malformed JSON, when folded, should drop the corrupt tail line and keep the earlier records', async () => {
    const fold = await foldJournal(seed.corruptTail);

    expect(fold.replay).toEqual([{ type: 'message', time: 100, message: USER_MESSAGE }]);
    expect(fold.toolStore).toEqual({ todo: FIRST_TODO_STATE });
  });
});