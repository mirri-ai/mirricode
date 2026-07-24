import { describe, expect, it } from 'vitest';
import { keepLiveSubagents } from '../src/lib/taskMerge';
import type { AppTask } from '../src/api/types';

function makeBgSubagent(
  id: string,
  bgTaskId: string,
  status: AppTask['status'] = 'running',
): AppTask {
  return {
    id,
    sessionId: 's1',
    kind: 'subagent',
    description: 'subagent task',
    status,
    createdAt: '2026-01-01T00:00:00.000Z',
    runInBackground: true,
    backgroundTaskId: bgTaskId,
  };
}

describe('keepLiveSubagents', () => {
  it('should fold REST row into WS row via backgroundTaskId and sync terminal status', () => {
    const wsRow: AppTask = makeBgSubagent('agent-1', 'bg-1');
    const restRow: AppTask = {
      ...makeBgSubagent('bg-1', 'bg-1'),
      status: 'completed',
      completedAt: '2026-01-01T00:01:00.000Z',
    };
    const result = keepLiveSubagents([restRow], [wsRow]);
    // Should merge into one row keyed by agent id, with completed status
    expect(result).toHaveLength(1);
    expect(result[0]!.id).toBe('agent-1');
    expect(result[0]!.status).toBe('completed');
  });

  it('should keep WS row unchanged when REST does not contain a matching row', () => {
    // REST did not return a matching background task (possibly evicted) —
    // the WS row should retain its current status without forced modification.
    const wsRow: AppTask = makeBgSubagent('agent-1', 'bg-1', 'running');
    const result = keepLiveSubagents([], [wsRow]);
    expect(result).toHaveLength(1);
    expect(result[0]!.id).toBe('agent-1');
    expect(result[0]!.status).toBe('running');
  });

  it('should not regress a completed WS row back to running via a stale REST row', () => {
    // Terminal-stickiness: if the WS row was already marked completed by a
    // subagent.completed event, a lagging REST poll returning 'running' must
    // not flip it back.
    const wsRow: AppTask = makeBgSubagent('agent-1', 'bg-1', 'completed');
    const restRow: AppTask = {
      ...makeBgSubagent('bg-1', 'bg-1'),
      status: 'running',
    };
    const result = keepLiveSubagents([restRow], [wsRow]);
    expect(result).toHaveLength(1);
    expect(result[0]!.id).toBe('agent-1');
    expect(result[0]!.status).toBe('completed');
  });

  it('should sync subagentPhase when REST completes a running WS row', () => {
    const wsRow: AppTask = {
      ...makeBgSubagent('agent-1', 'bg-1'),
      subagentPhase: 'working',
    };
    const restRow: AppTask = {
      ...makeBgSubagent('bg-1', 'bg-1'),
      status: 'completed',
      completedAt: '2026-01-01T00:01:00.000Z',
    };
    const result = keepLiveSubagents([restRow], [wsRow]);
    expect(result[0]!.subagentPhase).toBe('completed');
  });

  it('should set subagentPhase to failed when REST reports failure for a running WS row', () => {
    const wsRow: AppTask = {
      ...makeBgSubagent('agent-1', 'bg-1'),
      subagentPhase: 'working',
    };
    const restRow: AppTask = {
      ...makeBgSubagent('bg-1', 'bg-1'),
      status: 'failed',
    };
    const result = keepLiveSubagents([restRow], [wsRow]);
    expect(result[0]!.subagentPhase).toBe('failed');
  });
});
