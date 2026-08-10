import { readFile, readdir } from 'node:fs/promises';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface WireSnapshot {
  readonly agentId: string;
  readonly records: NormalizedRecord[];
}

export interface NormalizedRecord {
  readonly type: string;
  readonly [key: string]: unknown;
}

export interface WireSnapshotDiff {
  readonly identical: boolean;
  readonly differences: WireSnapshotDifference[];
}

export interface WireSnapshotDifference {
  readonly lineNo: number;
  readonly field: string;
  readonly expected: unknown;
  readonly actual: unknown;
}

// ---------------------------------------------------------------------------
// Snapshot — read wire.jsonl from a session directory
// ---------------------------------------------------------------------------

/**
 * Read wire.jsonl records for a given session directory and agent.
 * Looks for `<sessionDir>/agents/<agentId>/wire.jsonl`.
 * If `agentId` is omitted, reads the `main` agent.
 */
export async function snapshotWireRecords(
  sessionDir: string,
  agentId: string = 'main',
): Promise<WireSnapshot> {
  const wirePath = join(sessionDir, 'agents', agentId, 'wire.jsonl');
  const content = await readFile(wirePath, 'utf-8');
  const lines = content.split('\n').filter((line) => line.trim().length > 0);

  const records: NormalizedRecord[] = [];
  for (const line of lines) {
    const parsed = JSON.parse(line) as Record<string, unknown>;
    const normalized = normalizeRecord(parsed);
    if (normalized !== null) {
      records.push(normalized);
    }
  }

  return { agentId, records };
}

/**
 * Find all agent directories with wire.jsonl files under a session directory.
 * Useful for multi-agent scenarios (subagents, BTW, swarm).
 */
export async function snapshotAllAgentWireRecords(
  sessionDir: string,
): Promise<WireSnapshot[]> {
  const agentsDir = join(sessionDir, 'agents');
  let entries: string[];
  try {
    entries = await readdir(agentsDir);
  } catch {
    return [];
  }

  const snapshots: WireSnapshot[] = [];
  for (const agentId of entries) {
    try {
      const snapshot = await snapshotWireRecords(sessionDir, agentId);
      snapshots.push(snapshot);
    } catch {
      // Skip agents without wire.jsonl
    }
  }
  return snapshots;
}

// ---------------------------------------------------------------------------
// Normalization — strip non-deterministic fields
// ---------------------------------------------------------------------------

/** Fields to delete from every record during normalization. */
const FIELDS_TO_STRIP = new Set([
  'time',
  'created_at',
]);

/** Fields that contain non-deterministic values; replaced with a type marker. */
const VOLATILE_FIELDS = new Set([
  'uuid',
  'stepUuid',
  'messageId',
  'toolCallId',
  'goalId',
  'systemPromptHash',  // content-addressed, but content embeds env-specific data
  'hash',              // llm.tools_snapshot.hash, mcp.tools_discovered.hash
]);

/**
 * Observability record types whose payload contains environment-specific data
 * (tool lists, MCP server configs, skill paths). For golden file comparison,
 * only the type and deterministic metadata fields are kept.
 */
const OBSERVABILITY_RECORD_TYPES = new Set([
  'llm.tools_snapshot',
  'llm.request',
  'mcp.tools_discovered',
]);

/**
 * Fields to preserve from observability records (everything else is stripped).
 */
const OBSERVABILITY_KEEP_FIELDS = new Set([
  'type',
  'kind',         // llm.request: 'loop' | 'compaction'
  'provider',     // llm.request
  'model',        // llm.request
  'modelAlias',   // llm.request
  'toolSelect',   // llm.request
  'messageCount', // llm.request
  'turnStep',     // llm.request
  'attempt',      // llm.request
]);

/** Fields under loop event `event` object that are latency/timing; stripped. */
const LATENCY_FIELDS = new Set([
  'llmFirstTokenLatencyMs',
  'llmStreamDurationMs',
  'llmRequestBuildMs',
  'llmServerFirstTokenMs',
  'llmServerDecodeMs',
  'llmClientConsumeMs',
  'durationMs',
]);

function normalizeRecord(record: Record<string, unknown>): NormalizedRecord | null {
  const recordType = record['type'] as string;

  // Observability records carry environment-specific payloads (tool lists, MCP
  // server configs, prompts). llm.request keeps only deterministic metadata
  // fields; the tool-list records hold no deterministic metadata worth keeping
  // and are dropped entirely, which keeps golden diff files stable across
  // environments.
  if (OBSERVABILITY_RECORD_TYPES.has(recordType)) {
    if (recordType === 'llm.request') {
      const normalized: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(record)) {
        if (OBSERVABILITY_KEEP_FIELDS.has(key)) {
          normalized[key] = normalizeValue(key, value);
        }
      }
      return normalized as NormalizedRecord;
    }
    return null;
  }

  const normalized: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(record)) {
    if (FIELDS_TO_STRIP.has(key)) continue;
    normalized[key] = normalizeValue(key, value);
  }
  return normalized as NormalizedRecord;
}

function normalizeValue(key: string, value: unknown): unknown {
  // Replace volatile ID fields with a type-based placeholder
  if (VOLATILE_FIELDS.has(key) && typeof value === 'string') {
    return `<${key}>`;
  }

  // Normalize cwd paths in config.update
  if (key === 'cwd' && typeof value === 'string') {
    return '<cwd>';
  }

  // Replace systemPrompt content with a placeholder — it embeds environment-specific
  // data (skill paths, AGENTS.md content, etc.) that varies across machines.
  // The systemPromptHash in llm.request records is the deterministic fingerprint.
  if (key === 'systemPrompt' && typeof value === 'string') {
    return '<system-prompt>';
  }

  // Normalize protocol_version
  if (key === 'protocol_version' && typeof value === 'string') {
    return '<protocol-version>';
  }

  // Deep-normalize objects and arrays
  if (value !== null && typeof value === 'object') {
    // Replace usage token counts entirely (non-deterministic across runs)
    if (key === 'usage') {
      return '<usage>';
    }
    if (Array.isArray(value)) {
      return value.map((item) => normalizeValue('', item));
    }
    const result: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (LATENCY_FIELDS.has(k)) continue;
      result[k] = normalizeValue(k, v);
    }
    return result;
  }

  return value;
}

// ---------------------------------------------------------------------------
// Comparison — diff two snapshots
// ---------------------------------------------------------------------------

export function compareWireSnapshots(
  actual: WireSnapshot,
  expected: WireSnapshot,
): WireSnapshotDiff {
  const differences: WireSnapshotDifference[] = [];

  const maxLines = Math.max(actual.records.length, expected.records.length);
  for (let i = 0; i < maxLines; i++) {
    const actualRecord = actual.records[i];
    const expectedRecord = expected.records[i];

    if (actualRecord === undefined && expectedRecord !== undefined) {
      differences.push({
        lineNo: i + 1,
        field: '<record>',
        expected: expectedRecord,
        actual: undefined,
      });
      continue;
    }
    if (expectedRecord === undefined && actualRecord !== undefined) {
      differences.push({
        lineNo: i + 1,
        field: '<record>',
        expected: undefined,
        actual: actualRecord,
      });
      continue;
    }

    const actualJson = JSON.stringify(actualRecord);
    const expectedJson = JSON.stringify(expectedRecord);
    if (actualJson !== expectedJson) {
      differences.push({
        lineNo: i + 1,
        field: '<record>',
        expected: expectedRecord,
        actual: actualRecord,
      });
    }
  }

  return {
    identical: differences.length === 0,
    differences,
  };
}

// ---------------------------------------------------------------------------
// Golden file I/O
// ---------------------------------------------------------------------------

export function writeGoldenFile(path: string, snapshot: WireSnapshot): void {
  writeFileSync(path, JSON.stringify(snapshot, null, 2) + '\n', 'utf-8');
}

export async function readGoldenFile(path: string): Promise<WireSnapshot> {
  const content = await readFile(path, 'utf-8');
  return JSON.parse(content) as WireSnapshot;
}
