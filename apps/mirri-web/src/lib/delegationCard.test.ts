// apps/mirri-web/src/lib/delegationCard.test.ts
import { describe, expect, it } from 'vitest';
import type { AppTask } from '../api/types';
import {
  phaseForTask,
  swarmMembersByToolCall,
  type SwarmMember,
} from '../composables/swarmGroups';
import { delegationSummary } from './delegationCard';
import { buildSwarmCardRows } from './swarmCardRows';
import type { SwarmResult } from './parseSwarmResult';

function member(overrides: Partial<SwarmMember> = {}): SwarmMember {
  return {
    id: 'agent-1',
    name: 'scaffold',
    phase: 'working',
    swarmIndex: 0,
    ...overrides,
  };
}

function task(overrides: Partial<AppTask> = {}): AppTask {
  return {
    id: 'agent-1',
    sessionId: 's1',
    kind: 'subagent',
    description: 'scaffold',
    status: 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
    ...overrides,
  };
}

const failedResult = (): SwarmResult => ({
  summary: 'completed: 1, failed: 1',
  completed: 1,
  failed: 1,
  aborted: 0,
  total: 2,
  subagents: [
    { item: 'scaffold', outcome: 'completed', body: 'done' },
    { item: 'tests', outcome: 'failed', body: 'boom' },
  ],
});

describe('delegationSummary', () => {
  it('should report running aggregate with phase counts when members are still working', () => {
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [
        member({ phase: 'working' }),
        member({ id: 'agent-2', name: 'tests', phase: 'queued', swarmIndex: 1 }),
      ],
      result: null,
    });
    expect(summary.aggregateStatus).toBe('running');
    expect(summary.total).toBe(2);
    expect(summary.done).toBe(0);
    expect(summary.inProgress).toBe(2);
    expect(summary.counts).toEqual({
      queued: 1,
      working: 1,
      suspended: 0,
      awaiting: 0,
      backoff: 0,
      completed: 0,
      failed: 0,
      aborted: 0,
    });
    expect(summary.activityLine).toBe('');
  });

  it('should report error aggregate when the result carries a failed member', () => {
    const summary = delegationSummary({
      toolStatus: 'ok',
      members: [],
      result: failedResult(),
    });
    expect(summary.aggregateStatus).toBe('error');
    expect(summary.done).toBe(2);
    expect(summary.counts.completed).toBe(1);
    expect(summary.counts.failed).toBe(1);
  });

  it('should surface the sole member activity line when the delegation has exactly one member', () => {
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ text: 'first\nCalling Read: src/a.ts' })],
      result: null,
    });
    expect(summary.total).toBe(1);
    expect(summary.activityLine).toBe('Calling Read: src/a.ts');
  });

  it('should report zero totals when settled with no members and no result', () => {
    const summary = delegationSummary({ toolStatus: 'ok', members: [], result: null });
    expect(summary.total).toBe(0);
    expect(summary.done).toBe(0);
    expect(summary.aggregateStatus).toBe('ok');
  });

  it('should report error aggregate when the tool itself errored even without result members', () => {
    const summary = delegationSummary({ toolStatus: 'error', members: [], result: null });
    expect(summary.aggregateStatus).toBe('error');
  });

  it('should show attention aggregate when a member is awaiting via the event phase and the tool is running', () => {
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'awaiting' })],
      result: null,
    });
    expect(summary.aggregateStatus).toBe('attention');
  });

  it('should show error aggregate when a member failed and none is awaiting', () => {
    const summary = delegationSummary({
      toolStatus: 'ok',
      members: [member({ phase: 'failed' })],
      result: null,
    });
    expect(summary.aggregateStatus).toBe('error');
  });

  it('should count awaiting members as in-progress not done', () => {
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'awaiting' })],
      result: null,
    });
    expect(summary.inProgress).toBe(1);
    expect(summary.done).toBe(0);
  });

  it('should count aborted members as done (terminal) not in progress', () => {
    const summary = delegationSummary({
      toolStatus: 'ok',
      members: [member({ phase: 'aborted' })],
      result: null,
    });
    expect(summary.done).toBe(1);
    expect(summary.inProgress).toBe(0);
    expect(summary.aggregateStatus).toBe('ok');
  });
});

describe('phaseForTask', () => {
  it('should classify cancelled members as aborted not failed', () => {
    expect(phaseForTask(task({ status: 'cancelled', subagentPhase: 'working' }))).toBe('aborted');
  });
});

describe('event-driven awaiting source (P4)', () => {
  it('should not carry any derived awaiting marker on grouped members', () => {
    // The P1 pending-approval derivation is gone (ADR-008 ④): awaiting is an
    // event-driven phase only, and the derived shape no longer exists.
    const map = swarmMembersByToolCall([
      task({ parentToolCallId: 'tc-swarm', agentId: 'agent-1' }),
    ]);
    expect(map.get('tc-swarm')?.[0]).not.toHaveProperty('awaitingApproval');
  });
});

describe('event-driven awaiting consumption (P3b)', () => {
  it('should report attention when a member awaits via the event phase alone', () => {
    // Given a member whose awaiting phase came from the structured suspension
    // event (no pending approval row — the P1 derivation has nothing to fill)
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'awaiting' })],
      result: null,
    });

    // Then the aggregate reads attention (a fact signal, not an error) and the
    // member counts as in-progress
    expect(summary.aggregateStatus).toBe('attention');
    expect(summary.counts.awaiting).toBe(1);
    expect(summary.inProgress).toBe(1);
  });

  it('should treat the event phase as the single awaiting source and carry no derived marker', () => {
    // Given an event-driven awaiting member — the only awaiting source there
    // is (the P1 pending-approval derivation was deleted with ADR-008 ④)
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'awaiting' })],
      result: null,
    });
    const rows = buildSwarmCardRows([member({ phase: 'awaiting' })], null, '等待审批中');

    // Then the event phase is the single source: attention once, one awaiting
    // count, the activity label once, and no derived marker on the row
    expect(summary.aggregateStatus).toBe('attention');
    expect(summary.counts.awaiting).toBe(1);
    expect(rows[0]?.activity).toBe('等待审批中');
    expect(rows[0]).not.toHaveProperty('awaitingApproval');
  });

  it('should count backoff members as in-progress without raising attention', () => {
    // Given a member backed off for a rate limit (structured rate_limit reason)
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'backoff' })],
      result: null,
    });

    // Then backoff is an in-progress running substate — not attention (the
    // user does not need to act) and not a terminal state
    expect(summary.counts.backoff).toBe(1);
    expect(summary.inProgress).toBe(1);
    expect(summary.done).toBe(0);
    expect(summary.aggregateStatus).toBe('running');
  });

  it('should treat a legacy string suspension as suspended (not attention) in the aggregate', () => {
    // Given a legacy-string suspension member (phase suspended, prose reason)
    const summary = delegationSummary({
      toolStatus: 'running',
      members: [member({ phase: 'suspended', suspendedReason: 'Provider rate limit; subagent requeued for retry.' })],
      result: null,
    });

    // Then the aggregate stays running — a legacy suspension is not a wait on
    // the user (ADR-008 ② attention stays event-fact-driven)
    expect(summary.aggregateStatus).toBe('running');
    expect(summary.counts.suspended).toBe(1);
  });

  it('should show the structured reason only as a marker, never as prose, in the row body', () => {
    // Given an event-driven awaiting member with a structured reason
    const rows = buildSwarmCardRows(
      [member({ phase: 'awaiting', suspendedReason: { kind: 'approval', requestId: 'r1' } })],
      null,
      '等待审批中',
    );

    // Then the body falls back to the streamed text — the structured object is
    // not rendered as "[object Object]"
    expect(rows[0]?.activity).toBe('等待审批中');
    expect(rows[0]?.body).toBe('');
  });
});
