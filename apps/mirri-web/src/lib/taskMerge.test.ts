import { describe, expect, it } from 'vitest';

import { keepLiveSubagents, mergeSnapshotSubagents } from './taskMerge';
import type { AppTask } from '../api/types';
import { createInitialState, reduceAppEvent, type EventMeta } from '../api/daemon/eventReducer';

function liveTask(overrides: Partial<AppTask> = {}): AppTask {
  return {
    id: 'agent-p',
    sessionId: 'sess_1',
    kind: 'subagent',
    description: 'member p',
    status: 'running',
    subagentPhase: 'awaiting',
    createdAt: '2025-01-01T00:00:00.000Z',
    ...overrides,
  };
}

describe('restCompletesLiveRow phase mapping', () => {
  it('should map a REST-cancelled row to the aborted phase, not failed', () => {
    // F4: the phase enum HAS an 'aborted' terminal — user-interrupted work
    // must not be shown as a failure.
    const live = liveTask({
        id: 'live-p',
        status: 'running',
        subagentPhase: 'queued',
        agentId: 'agent-p',
      });
    const rest: AppTask = {
      ...liveTask({ id: 'rest-p', status: 'cancelled' }),
      id: 'rest-p',
      agentId: 'agent-p',
    };
    const merged = keepLiveSubagents([rest], [live]);
    expect(merged[0]?.subagentPhase).toBe('aborted');
    expect(merged[0]?.status).toBe('cancelled');
  });
});

describe('mergeSnapshotSubagents preserves the event-derived phase', () => {
  it('should keep a parked member awaiting when the roster row carries no phase', () => {
    // F5: the engine's REST task rows carry no subagent_phase; the roster
    // merge must not wipe the live replayed 'awaiting' (the member is parked
    // on approval across the reload).
    const live = liveTask({
      subagentPhase: 'awaiting',
      suspendedReason: { kind: 'approval', requestId: 'req-7' },
    });
    const roster: AppTask = {
      id: 'agent-p',
      sessionId: 'sess_1',
      kind: 'subagent',
      description: 'member p',
      status: 'running',
      createdAt: '2025-01-01T00:00:00.000Z',
    };
    const merged = mergeSnapshotSubagents([roster], [live]);
    expect(merged[0]?.subagentPhase).toBe('awaiting');
    expect(merged[0]?.suspendedReason).toMatchObject({ kind: 'approval', requestId: 'req-7' });
  });

  it('should let a roster phase win when the roster row carries one', () => {
    const live = liveTask({ subagentPhase: 'awaiting' });
    const roster: AppTask = {
      ...liveTask({ id: 'agent-p', status: 'completed', subagentPhase: 'completed' }),
      id: 'agent-p',
    };
    const merged = mergeSnapshotSubagents([roster], [live]);
    expect(merged[0]?.subagentPhase).toBe('completed');
  });

  it('should keep rows absent from the roster untouched', () => {
    const live = liveTask();
    const merged = mergeSnapshotSubagents([], [live]);
    expect(merged).toHaveLength(1);
    expect(merged[0]?.subagentPhase).toBe('awaiting');
  });
});

describe('agentId join (foreground swarm member fold)', () => {
  const SID = 'sess_1';
  const META: EventMeta = { sessionId: SID, seq: 1 };

  function restRow(): AppTask {
    // The REST `/tasks` row for a foreground swarm member: keyed by its own
    // task id, carrying the agent's id as agentId.
    return {
      ...liveTask({ id: 'task_rest_1', subagentPhase: undefined, agentId: 'agent-p' }),
      id: 'task_rest_1',
    };
  }

  function liveRow(): AppTask {
    // The WS-projected row: keyed by the agent id, carrying agentId since
    // patchSubagent sets it on every projected sub-agent row.
    return liveTask({ id: 'agent-p', subagentPhase: 'working', agentId: 'agent-p' });
  }

  it('should fold a live row and a REST row with matching agent_id into a single row', () => {
    const merged = keepLiveSubagents([restRow()], [liveRow()]);
    expect(merged).toHaveLength(1);
    expect(merged[0]?.id).toBe('agent-p');
    expect(merged[0]?.agentId).toBe('agent-p');
  });

  it('should keep exactly one row across an interleaved poll-fold → lifecycle re-projection → poll-fold sequence', () => {
    // The regression that used to flicker: poll fold merges the REST copy into
    // the WS row, then a lifecycle event re-projects the row as a skeleton —
    // the reducer must preserve agentId or the next poll folds miss the join
    // and the agent surfaces as two rows.
    let state = createInitialState();
    state.tasksBySession[SID] = [liveRow()];

    // Poll 1: REST fold merges the copy into the live row.
    state = {
      ...state,
      tasksBySession: { ...state.tasksBySession, [SID]: keepLiveSubagents([restRow()], state.tasksBySession[SID]!) },
    };
    expect(state.tasksBySession[SID]).toHaveLength(1);

    // Lifecycle event re-projects the row as a skeleton without agentId.
    const projected: AppTask = {
      id: 'agent-p',
      sessionId: SID,
      kind: 'subagent',
      description: 'Sub Agent',
      status: 'running',
      createdAt: '2025-01-01T00:00:01.000Z',
    };
    state = reduceAppEvent(state, { type: 'taskCreated', sessionId: SID, task: projected }, META);

    // Poll 2: the fold must still hit the agentId join.
    state = {
      ...state,
      tasksBySession: { ...state.tasksBySession, [SID]: keepLiveSubagents([restRow()], state.tasksBySession[SID]!) },
    };
    expect(state.tasksBySession[SID]).toHaveLength(1);
    const row = state.tasksBySession[SID]![0]!;
    expect(row.id).toBe('agent-p');
    expect(row.agentId).toBe('agent-p');
  });
});
describe('mergeSnapshotSubagents preserves the live narrative fields (ADR-009)', () => {
  it('should keep the live structured steps when the roster row carries none', () => {
    // Given a member whose live run accumulated structured tool steps before a
    // reload's roster merge (the roster row never carries steps).
    const live = liveTask({
      steps: [{ id: 'tc_1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: ['body'] }],
    });
    const roster: AppTask = { ...live, id: live.id };
    delete (roster as Partial<AppTask>).steps;

    // When the snapshot roster merges over the live rows.
    const merged = mergeSnapshotSubagents([roster], [live]);

    // Then the member's tool cards survive — same rule as outputLines/text.
    expect(merged[0]?.steps).toEqual([
      { id: 'tc_1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: ['body'] },
    ]);
  });
});
