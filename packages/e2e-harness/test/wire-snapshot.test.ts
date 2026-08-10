import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  compareWireSnapshots,
  snapshotWireRecords,
  type WireSnapshot,
} from '#/index';

describe('WireSnapshot', () => {
  let sessionDir: string;

  beforeEach(async () => {
    sessionDir = await mkdtemp(join(tmpdir(), 'mirri-wire-test-'));
  });

  afterEach(async () => {
    const { rm } = await import('node:fs/promises');
    await rm(sessionDir, { recursive: true, force: true });
  });

  it('should read wire.jsonl and normalize records', async () => {
    const wirePath = join(sessionDir, 'agents', 'main', 'wire.jsonl');
    const { mkdir } = await import('node:fs/promises');
    await mkdir(join(sessionDir, 'agents', 'main'), { recursive: true });

    const records = [
      { type: 'metadata', protocol_version: '1.5', created_at: 1700000000000 },
      {
        type: 'turn.prompt',
        input: [{ type: 'text', text: 'hello' }],
        origin: { kind: 'user' },
        time: 1700000001000,
      },
      {
        type: 'context.append_message',
        message: { role: 'user', content: [{ type: 'text', text: 'hello' }], toolCalls: [] },
        time: 1700000001001,
      },
    ];

    await writeFile(wirePath, records.map((r) => JSON.stringify(r)).join('\n') + '\n');

    const snapshot = await snapshotWireRecords(sessionDir, 'main');

    expect(snapshot.agentId).toBe('main');
    expect(snapshot.records).toHaveLength(3);

    // time should be stripped
    expect(snapshot.records[0]).not.toHaveProperty('time');
    expect(snapshot.records[1]).not.toHaveProperty('time');

    // protocol_version should be normalized
    expect(snapshot.records[0]).toMatchObject({ type: 'metadata', protocol_version: '<protocol-version>' });
    expect(snapshot.records[0]).not.toHaveProperty('created_at');
  });

  it('should compare two identical snapshots as identical', async () => {
    const snapshot: WireSnapshot = {
      agentId: 'main',
      records: [
        { type: 'metadata', protocol_version: '<protocol-version>' },
        { type: 'turn.prompt', input: [{ type: 'text', text: 'hello' }], origin: { kind: 'user' } },
      ],
    };

    const diff = compareWireSnapshots(snapshot, snapshot);
    expect(diff.identical).toBe(true);
    expect(diff.differences).toHaveLength(0);
  });

  it('should detect differences between snapshots', async () => {
    const actual: WireSnapshot = {
      agentId: 'main',
      records: [
        { type: 'metadata', protocol_version: '<protocol-version>' },
        { type: 'turn.prompt', input: [{ type: 'text', text: 'hello' }], origin: { kind: 'user' } },
      ],
    };

    const expected: WireSnapshot = {
      agentId: 'main',
      records: [
        { type: 'metadata', protocol_version: '<protocol-version>' },
        { type: 'turn.prompt', input: [{ type: 'text', text: 'different' }], origin: { kind: 'user' } },
      ],
    };

    const diff = compareWireSnapshots(actual, expected);
    expect(diff.identical).toBe(false);
    expect(diff.differences).toHaveLength(1);
    expect(diff.differences[0]?.lineNo).toBe(2);
  });

  it('should detect missing records', async () => {
    const actual: WireSnapshot = {
      agentId: 'main',
      records: [{ type: 'metadata', protocol_version: '<protocol-version>' }],
    };

    const expected: WireSnapshot = {
      agentId: 'main',
      records: [
        { type: 'metadata', protocol_version: '<protocol-version>' },
        { type: 'turn.prompt', input: [], origin: { kind: 'user' } },
      ],
    };

    const diff = compareWireSnapshots(actual, expected);
    expect(diff.identical).toBe(false);
    expect(diff.differences).toHaveLength(1);
  });
});
