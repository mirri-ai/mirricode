// apps/mirri-web/test/snapshot-session-merge.test.ts
// Pure-logic tests for the snapshot-sync merge (`mergeSnapshotSession` in
// `src/api/daemon/mappers.ts` — the reducer `syncSessionFromSnapshot` feeds
// to `updateSession`): the `session.snapshot` view models only the
// run-state + message facts, so the merge must preserve the record's richer
// fields the snapshot view does not authoritatively carry. The webverify
// regression: an engine snapshot view without `cwd` wiped the list-row cwd
// and the session fell out of its sidebar workspace group, and the
// placeholder usage zeroed the live context ring ("Used 6.9k" → "Used 0/0").

import { describe, expect, it } from 'vitest';

import {
  isPlaceholderSessionUsage,
  mergeSnapshotSession,
  toAppQuestionRequest,
  toAppSessionUsage,
} from '../src/api/daemon/mappers';
import type { AppSession, AppSessionUsage } from '../src/api/types';

function usage(overrides: Partial<AppSessionUsage> = {}): AppSessionUsage {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    totalCostUsd: 0,
    contextTokens: 0,
    contextLimit: 0,
    turnCount: 0,
    ...overrides,
  };
}

function session(overrides: Partial<AppSession> = {}): AppSession {
  return {
    id: 'sess_1',
    title: 'Session',
    createdAt: '2026-01-01T00:00:00.000Z',
    updatedAt: '2026-01-01T00:00:00.000Z',
    busy: false,
    archived: false,
    cwd: '/workspaces/ws-b',
    model: 'ultra',
    usage: usage({ inputTokens: 6900, contextTokens: 12000, contextLimit: 200000 }),
    messageCount: 3,
    lastSeq: 42,
    ...overrides,
  };
}

/** A snapshot view the way the rust engine's older builds answer it: no cwd
 *  on the wire, which the mapping layer degrades to ''. */
function snapshotView(overrides: Partial<AppSession> = {}): AppSession {
  return session({
    title: '',
    createdAt: '',
    updatedAt: '',
    busy: true,
    cwd: '',
    model: '',
    usage: usage(),
    messageCount: 3,
    lastSeq: 0,
    ...overrides,
  });
}

describe('mergeSnapshotSession — snapshot-sync merge (webverify bug C)', () => {
  it('preserves the record cwd when the snapshot view omits it (degraded to \'\')', () => {
    // Given a list-row record with its workspace's cwd, and a snapshot view
    // whose cwd was never carried on the wire (older engine build)
    const record = session();
    const snapshot = snapshotView();

    // When the snapshot is merged onto the record
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the cwd survives — the session stays in its sidebar workspace
    // group instead of falling out of every group
    expect(merged.cwd).toBe('/workspaces/ws-b');
  });

  it('keeps the live usage when the snapshot usage is the placeholder', () => {
    // Given a record with the live usage folded in from /status
    const record = session();
    const snapshot = snapshotView();

    // When the placeholder-usage snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the live usage survives (no "Used 6.9k" → "Used 0/0" drop)
    expect(merged.usage).toEqual(record.usage);
    expect(isPlaceholderSessionUsage(merged.usage)).toBe(false);
  });

  it('takes the snapshot cwd and usage when the snapshot carries real values', () => {
    // Given a snapshot view whose engine carries the cwd (the W4-parity
    // snapshot view) and a real usage
    const record = session();
    const snapshot = snapshotView({
      cwd: '/workspaces/ws-c',
      model: 'fast',
      usage: usage({ inputTokens: 100, contextTokens: 200, contextLimit: 1000, turnCount: 1 }),
    });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, false);

    // Then the authoritative snapshot facts win — the merge never serves a
    // stale cwd/model/usage
    expect(merged.cwd).toBe('/workspaces/ws-c');
    expect(merged.model).toBe('fast');
    expect(merged.usage).toEqual(snapshot.usage);
  });

  it('takes the snapshot run-state facts as-is', () => {
    // Given a snapshot view reporting the busy run-state and message count
    const record = session({ busy: false, messageCount: 1 });
    const snapshot = snapshotView({ busy: true, messageCount: 7 });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the snapshot's run-state facts replace the record's
    expect(merged.busy).toBe(true);
    expect(merged.messageCount).toBe(7);
  });

  it('keeps the record model when the snapshot model is empty (session without agents)', () => {
    // Given a snapshot of a session whose first agent does not exist yet
    const record = session({ model: 'fast' });
    const snapshot = snapshotView({ model: '' });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the record's last known model survives
    expect(merged.model).toBe('fast');
  });

  it('keeps the record timestamps when the snapshot view omits them (fresh session "NaNy")', () => {
    // Given a freshly created session's list-row record with real
    // createdAt/updatedAt, and a snapshot view whose engine carries no
    // timestamps (degraded to '' by the mapping layer)
    const record = session({
      createdAt: '2026-02-27T08:00:00.000Z',
      updatedAt: '2026-02-27T08:00:00.000Z',
    });
    const snapshot = snapshotView({ createdAt: '', updatedAt: '' });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the record's timestamps survive — the sidebar row renders a real
    // relative time instead of "NaNy" (formatTime('') → Invalid Date → NaN)
    expect(merged.createdAt).toBe('2026-02-27T08:00:00.000Z');
    expect(merged.updatedAt).toBe('2026-02-27T08:00:00.000Z');
  });

  it('keeps the record title when the snapshot view carries none (always null)', () => {
    // Given a record with the session's title and a snapshot view whose
    // engine title is always null (degraded to '' by the mapping layer)
    const record = session({ title: 'Untitled Session' });
    const snapshot = snapshotView({ title: '' });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the record's title survives — the sidebar row keeps its label
    expect(merged.title).toBe('Untitled Session');
  });

  it('takes the snapshot timestamps when the snapshot carries real values', () => {
    // Given a snapshot view whose engine reports fresh timestamps
    const record = session({ createdAt: '2026-01-01T00:00:00.000Z', updatedAt: '2026-01-01T00:00:00.000Z' });
    const snapshot = snapshotView({
      createdAt: '2026-02-27T09:00:00.000Z',
      updatedAt: '2026-02-27T09:30:00.000Z',
      title: 'Renamed',
    });

    // When the snapshot is merged
    const merged = mergeSnapshotSession(record, snapshot, true);

    // Then the authoritative snapshot facts win
    expect(merged.createdAt).toBe('2026-02-27T09:00:00.000Z');
    expect(merged.updatedAt).toBe('2026-02-27T09:30:00.000Z');
    expect(merged.title).toBe('Renamed');
  });
});

describe('toAppSessionUsage — first call mapping (SDR-004)', () => {
  it('should map the wire first call onto the app usage', () => {
    const app = toAppSessionUsage({
      input_tokens: 244_730,
      output_tokens: 1_371,
      cache_read_tokens: 189_000,
      cache_creation_tokens: 4_000,
      total_cost_usd: 0,
      context_tokens: 120_000,
      context_limit: 200_000,
      turn_count: 7,
      first_call: {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 10,
        cache_creation_tokens: 5,
      },
    });
    expect(app.firstCall).toEqual({
      inputTokens: 100,
      outputTokens: 50,
      cacheReadTokens: 10,
      cacheCreationTokens: 5,
    });
  });

  it('should leave firstCall undefined when the wire omits it', () => {
    const app = toAppSessionUsage({
      input_tokens: 1,
      output_tokens: 1,
      cache_read_tokens: 0,
      cache_creation_tokens: 0,
      total_cost_usd: 0,
      context_tokens: 0,
      context_limit: 0,
      turn_count: 1,
    });
    expect(app.firstCall).toBeUndefined();
  });
});

// AskUserQuestion 'Other' parity: the TS daemon's wire spells question items
// snake_case, but the rust engine's klient rows spell them camelCase — the
// REST-path question mapper must read both so the restored card renders the
// Other free-text input on either wire.
describe('toAppQuestionRequest — question item spelling (Other parity)', () => {
  const requestShell = {
    question_id: 'q-1',
    session_id: 's1',
    created_at: '2026-10-01T00:00:00Z',
  };

  it('should map the TS daemon snake_case question item flags onto the app spelling', () => {
    const app = toAppQuestionRequest({
      ...requestShell,
      questions: [
        {
          id: 'q0',
          question: 'Proceed?',
          options: [{ id: 'A', label: 'yes' }, { id: 'B', label: 'no' }],
          multi_select: true,
          allow_other: true,
        },
      ],
    } as Parameters<typeof toAppQuestionRequest>[0]);

    expect(app.questions[0]).toMatchObject({ multiSelect: true, allowOther: true });
  });

  it('should map the rust klient camelCase question item flags onto the app spelling', () => {
    const app = toAppQuestionRequest({
      ...requestShell,
      questions: [
        {
          id: 'q0',
          question: 'Proceed?',
          options: [{ id: 'A', label: 'yes' }, { id: 'B', label: 'no' }],
          multiSelect: true,
          allowOther: true,
        },
      ],
    } as unknown as Parameters<typeof toAppQuestionRequest>[0]);

    expect(app.questions[0]).toMatchObject({ multiSelect: true, allowOther: true });
  });
});
