// apps/mirri-web/test/task-overview.test.ts
// Pure view logic of the B7 Tasks overview panel (W3): joining the
// workspace-aggregated task rows (task.listWorkspace) with the session list
// the sidebar already holds, filtering, and the status counters the header
// shows. No Vue — the panel is a thin wiring layer over these functions.

import { describe, expect, it } from 'vitest';

import type { AppSession, AppTask } from '../src/api/types';
import {
  buildWorkspaceTaskRows,
  countByStatus,
  filterWorkspaceTasks,
} from '../src/lib/taskOverview';

function session(overrides: Partial<AppSession> & { id: string }): AppSession {
  return {
    title: 'untitled session',
    createdAt: '2026-11-01T00:00:00Z',
    updatedAt: '2026-11-01T00:00:00Z',
    busy: false,
    archived: false,
    cwd: '/repo',
    model: 'test-model',
    usage: {
      inputTokens: 0,
      outputTokens: 0,
      cacheReadTokens: 0,
      cacheCreationTokens: 0,
      totalCostUsd: 0,
      contextTokens: 0,
      contextLimit: 0,
      turnCount: 0,
    },
    messageCount: 0,
    lastSeq: 0,
    ...overrides,
  };
}

function task(overrides: Partial<AppTask> & { id: string; sessionId: string }): AppTask {
  return {
    kind: 'bash',
    description: 'a background task',
    status: 'running',
    createdAt: '2026-11-01T00:00:00Z',
    ...overrides,
  };
}

describe('buildWorkspaceTaskRows', () => {
  it('should join the session title and archived flag onto each task row when the session is known', () => {
    // Given two live sessions (one archived) each holding one task
    const tasks = [
      task({ id: 't-1', sessionId: 's-1', description: 'npm run watch' }),
      task({ id: 't-2', sessionId: 's-2', description: 'explore the repo', status: 'completed' }),
    ];
    const sessionsById = new Map([
      ['s-1', session({ id: 's-1', title: 'Frontend work', archived: false })],
      ['s-2', session({ id: 's-2', title: 'Old exploration', archived: true })],
    ]);

    // When the rows are built
    const rows = buildWorkspaceTaskRows(tasks, sessionsById);

    // Then each row carries its task plus the joined title and archived flag
    expect(rows).toHaveLength(2);
    expect(rows[0]).toMatchObject({
      task: { id: 't-1' },
      sessionId: 's-1',
      sessionTitle: 'Frontend work',
      archived: false,
    });
    expect(rows[1]).toMatchObject({
      task: { id: 't-2' },
      sessionId: 's-2',
      sessionTitle: 'Old exploration',
      archived: true,
    });
  });

  it('should degrade honestly when the session id is unknown (no fabricated title)', () => {
    // Given a task whose session is not in the local session list (e.g. a
    // session of another workspace the sidebar has not loaded)
    const rows = buildWorkspaceTaskRows(
      [task({ id: 't-9', sessionId: 's-gone' })],
      new Map<string, AppSession>(),
    );

    // Then the row still renders, with an empty title and not archived —
    // the panel shows the session id in place of a title
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ sessionId: 's-gone', sessionTitle: '', archived: false });
  });
});

describe('filterWorkspaceTasks', () => {
  const rows = buildWorkspaceTaskRows(
    [
      task({ id: 't-1', sessionId: 's-1', description: 'npm run watch' }),
      task({ id: 't-2', sessionId: 's-1', description: 'clean build', status: 'completed' }),
      task({ id: 't-3', sessionId: 's-2', description: 'explore the repo' }),
    ],
    new Map([
      ['s-1', session({ id: 's-1', title: 'Frontend work' })],
      ['s-2', session({ id: 's-2', title: 'Old exploration' })],
    ]),
  );

  it('should keep every row when the filter is all and the query is empty', () => {
    expect(filterWorkspaceTasks(rows, 'all', '')).toHaveLength(3);
  });

  it('should keep only running rows when the filter is running', () => {
    const filtered = filterWorkspaceTasks(rows, 'running', '');
    expect(filtered.map((r) => r.task.id)).toEqual(['t-1', 't-3']);
  });

  it('should match the query against the session title, the task description and the session id, case-insensitively', () => {
    expect(filterWorkspaceTasks(rows, 'all', 'frontend').map((r) => r.task.id)).toEqual(['t-1', 't-2']);
    expect(filterWorkspaceTasks(rows, 'all', 'WATCH').map((r) => r.task.id)).toEqual(['t-1']);
    expect(filterWorkspaceTasks(rows, 'all', 's-2').map((r) => r.task.id)).toEqual(['t-3']);
  });

  it('should combine the status filter with the query', () => {
    expect(filterWorkspaceTasks(rows, 'running', 'frontend').map((r) => r.task.id)).toEqual(['t-1']);
  });
});

describe('countByStatus', () => {
  it('should bucket rows into running / completed / failed / other', () => {
    // Given one row per status vocabulary entry (cancelled lands in "other")
    const rows = buildWorkspaceTaskRows(
      [
        task({ id: 't-1', sessionId: 's', status: 'running' }),
        task({ id: 't-2', sessionId: 's', status: 'completed' }),
        task({ id: 't-3', sessionId: 's', status: 'failed' }),
        task({ id: 't-4', sessionId: 's', status: 'cancelled' }),
      ],
      new Map<string, AppSession>(),
    );

    // When counted
    const counts = countByStatus(rows);

    // Then each bucket holds its rows
    expect(counts).toEqual({ running: 1, completed: 1, failed: 1, other: 1 });
  });
});
