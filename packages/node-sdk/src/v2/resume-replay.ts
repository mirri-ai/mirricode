/**
 * Resume replay fold — rebuilds the v1 `ResumedAgentState.replay` /
 * `toolStore` pair from a v2 agent's `wire.jsonl`.
 *
 * The v2 engine persists each agent's journal at
 * `<sessionDir>/agents/<agentId>/wire.jsonl` using v1's record vocabulary for
 * every replay-relevant op (see `agent-core-v2/src/wire/record.ts`). The v1
 * engine's own restore pipeline (`Agent` + `AgentRecords.replay`) is
 * therefore the correct fold: it owns the subtle semantics a re-implementation
 * would drift from — assistant-message assembly from loop events, `context.undo`
 * removing replayed messages, `context.apply_compaction` patching the last
 * compaction record, etc.
 *
 * The fold runs on a THROWAWAY v1 `Agent` over an in-memory persistence, so
 * it never mutates the journal (a live restore appends synthesized
 * interrupted-tool-result records at `finishResume`; here those appends land
 * in the memory buffer only). Any failure — missing/corrupt file, newer
 * protocol, unexpected record — degrades to an empty result instead of
 * failing the session resume.
 */

import { readFile } from 'node:fs/promises';

import {
  Agent,
  type AgentRecord,
  type AgentRecordPersistence,
  type AgentReplayRecord,
} from '@mirri-ai/agent-core';
import { LocalKaos } from '@mirri-ai/kaos';

export interface FoldedAgentReplay {
  readonly replay: readonly AgentReplayRecord[];
  readonly toolStore: Readonly<Record<string, unknown>>;
}

const EMPTY_FOLD: FoldedAgentReplay = { replay: [], toolStore: {} };

/**
 * Read-only `AgentRecordPersistence` over an already-parsed journal. The
 * throwaway fold agent's live writes (the synthesized interrupted-tool-result
 * records `finishResume` appends) land here and go nowhere — the on-disk
 * journal is never mutated.
 */
class ReadOnlyAgentRecordPersistence implements AgentRecordPersistence {
  constructor(private readonly records: readonly AgentRecord[]) {}

  async *read(): AsyncIterable<AgentRecord> {
    for (const record of this.records) {
      yield record;
    }
  }

  append(_input: AgentRecord): void {}

  rewrite(_records: readonly AgentRecord[]): void {}

  async flush(): Promise<void> {}

  async close(): Promise<void> {}
}

/**
 * Fold one agent's `wire.jsonl` into the v1 replay records and tool-store
 * snapshot. Best-effort: unreadable or malformed journals yield an empty
 * fold, never a rejected resume.
 */
export async function foldAgentWireReplay(wirePath: string): Promise<FoldedAgentReplay> {
  try {
    const records = parseWireRecords(await readFile(wirePath, 'utf-8'));
    if (records.length === 0) return EMPTY_FOLD;
    const agent = new Agent({
      kaos: await LocalKaos.create(),
      persistence: new ReadOnlyAgentRecordPersistence(records),
      type: 'sub',
    });
    await agent.resume({ rewriteMigratedRecords: false });
    return {
      replay: agent.replayBuilder.buildResult(),
      toolStore: agent.tools.storeData(),
    };
  } catch {
    return EMPTY_FOLD;
  }
}

/**
 * The v1 line reader's rules: blank lines skipped, a truncated TAIL line
 * tolerated (the last write may have crashed mid-flush), corruption anywhere
 * else is an error.
 */
function parseWireRecords(content: string): AgentRecord[] {
  const lines = content.split('\n');
  const records: AgentRecord[] = [];
  for (const [index, rawLine] of lines.entries()) {
    const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
    if (line.length === 0) continue;
    try {
      records.push(JSON.parse(line) as AgentRecord);
    } catch (error) {
      if (index === lines.length - 1) break;
      throw error;
    }
  }
  return records;
}