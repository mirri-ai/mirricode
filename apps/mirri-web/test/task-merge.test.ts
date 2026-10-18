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

function wsTask(overrides: Partial<AppTask> = {}): AppTask {
  return {
    id: 'agent-1',
    sessionId: 's1',
    kind: 'subagent',
    description: 'scaffold the api client',
    status: 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
    ...overrides,
  };
}

function restTask(overrides: Partial<AppTask> = {}): AppTask {
  return {
    id: 'task-9',
    sessionId: 's1',
    kind: 'subagent',
    description: 'REST copy of scaffold',
    status: 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
    swarmIndex: 0,
    ...overrides,
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

  it('should fold a rest swarm member row into the live ws row when matched by agent id', () => {
    const existing = [wsTask({ agentId: 'agent-1', swarmIndex: 0 })];
    const restBased = [restTask({ id: 'task-9', agentId: 'agent-1', swarmIndex: 0 })];

    const merged = keepLiveSubagents(restBased, existing);

    // WS row owns the merged identity; REST is only a status/terminal output
    // reconciler, never a second state source.
    expect(merged).toHaveLength(1);
    expect(merged[0]!.id).toBe('agent-1');
    expect(merged[0]!.description).toBe('scaffold the api client');
    expect(merged[0]!.status).toBe('running');
  });

  it('should complete a live ws row when a terminal rest row matches it by agent id', () => {
    const existing = [wsTask({ agentId: 'agent-1', swarmIndex: 0 })];
    const restBased = [
      restTask({
        id: 'task-9',
        agentId: 'agent-1',
        status: 'failed',
        completedAt: '2026-01-01T00:01:00.000Z',
      }),
    ];

    const merged = keepLiveSubagents(restBased, existing);

    expect(merged).toHaveLength(1);
    expect(merged[0]!.status).toBe('failed');
    expect(merged[0]!.subagentPhase).toBe('failed');
    expect(merged[0]!.completedAt).toBe('2026-01-01T00:01:00.000Z');

    // REST keeps its authoritative output once present.
    const withOutput = [
      wsTask({ agentId: 'agent-1', outputPreview: 'partial ws preview', status: 'completed' }),
    ];
    const restWithOutput = [
      restTask({
        id: 'task-9',
        agentId: 'agent-1',
        status: 'completed',
        outputPreview: 'authoritative rest result',
      }),
    ];
    const [folded] = keepLiveSubagents(restWithOutput, withOutput);
    expect(folded!.outputPreview).toBe('authoritative rest result');
  });

  it('should never resurrect a terminal live row when a matching rest row is running', () => {
    const existing = [
      wsTask({
        agentId: 'agent-1',
        status: 'completed',
        subagentPhase: 'completed',
        completedAt: '2026-01-01T00:01:00.000Z',
      }),
    ];
    const restBased = [restTask({ id: 'task-9', agentId: 'agent-1', status: 'running' })];

    const merged = keepLiveSubagents(restBased, existing);

    expect(merged).toHaveLength(1);
    expect(merged[0]!.status).toBe('completed');
    expect(merged[0]!.subagentPhase).toBe('completed');
    expect(merged[0]!.completedAt).toBe('2026-01-01T00:01:00.000Z');
  });

  it('should keep folding a rest row joined by backgroundTaskId when agentId is absent', () => {
    const existing = [wsTask({ id: 'agent-1', backgroundTaskId: 'task-9' })];
    const restBased = [
      restTask({ id: 'task-9', status: 'failed', completedAt: '2026-01-01T00:01:00.000Z' }),
    ];

    const merged = keepLiveSubagents(restBased, existing);

    expect(merged).toHaveLength(1);
    expect(merged[0]!.id).toBe('agent-1');
    expect(merged[0]!.status).toBe('failed');
  });

  it('should keep the backgroundTaskId fold working when agentId is also present on the live row', () => {
    const existing = [wsTask({ agentId: 'agent-1', backgroundTaskId: 'task-9' })];
    const restBased = [restTask({ id: 'task-9', agentId: 'agent-1', status: 'completed' })];

    const merged = keepLiveSubagents(restBased, existing);

    // Both join keys point at the same real row (a background subagent row
    // carries both its task id and its agent id): the fold must still hit the
    // task-9 copy instead of breaking either path.
    expect(merged).toHaveLength(1);
    expect(merged[0]!.status).toBe('completed');
  });

  it('should preserve a live ws row when no rest row matches it', () => {
    const existing = [wsTask({ agentId: 'agent-1' })];
    const restBased = [restTask({ id: 'task-8', agentId: 'agent-2' })];

    const merged = keepLiveSubagents(restBased, existing);

    expect(merged).toHaveLength(2);
    const live = merged.find((t) => t.id === 'agent-1');
    expect(live?.description).toBe('scaffold the api client');
    const rest = merged.find((t) => t.id === 'task-8');
    expect(rest).toBeDefined();
  });
});