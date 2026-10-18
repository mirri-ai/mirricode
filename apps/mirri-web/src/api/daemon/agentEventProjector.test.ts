import { describe, expect, it } from 'vitest';

import { classifyFrame, createAgentProjector, turnUsageForPromptId } from './agentEventProjector';
import type { MirriClientState, EventMeta } from './eventReducer';
import { createInitialState, reduceAppEvent } from './eventReducer';
import { mergeSnapshotSubagents } from '../../lib/taskMerge';

const SID = 'sess_1';

function taskFor(state: MirriClientState, id: string) {
  return state.tasksBySession[SID]?.find((t) => t.id === id);
}

describe('classifyFrame — agent-core events behind the "event." prefix', () => {
  // The rust engine publishes agent-core lifecycle events with an "event."
  // prefix (e.g. `event.skill.activated`). Frames the agent projector knows
  // (even as an explicit no-op) must take the agent route; routing them to
  // the protocol path makes toAppEvent() record them as unknown protocol
  // events, which the reducer surfaces as "Unhandled event" warnings.
  const prefixedAgentCoreTypes = [
    'event.skill.activated',
    'event.plugin_command.activated',
    'event.tool.list.updated',
    'event.hook.result',
    'event.compaction.blocked',
  ];

  it.each(prefixedAgentCoreTypes)('should route %s to the agent projector', (rawType) => {
    const decision = classifyFrame(rawType, {});
    expect(decision.route).toBe('agent');
  });

  it('should keep genuine protocol events on the protocol path', () => {
    const decision = classifyFrame('event.message.created', {
      message: { id: 'm1', role: 'user', content: [] },
    });
    expect(decision.route).toBe('protocol');
  });
});

describe('subagent.suspended reason projection (ADR-011 structured kinds)', () => {
  const projector = createAgentProjector();

  it('should map the known approval kind to awaiting', () => {
    const events = projector.project(
      'subagent.suspended',
      {
        subagentId: 'agent-a',
        reason: { kind: 'approval', request_id: 'req-1', tool_call_id: 'call_x' },
      },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({
      id: 'agent-a',
      subagentPhase: 'awaiting',
      suspendedReason: { kind: 'approval', requestId: 'req-1', toolCallId: 'call_x' },
    });
  });

  it('should map the known question kind to awaiting', () => {
    const events = projector.project(
      'subagent.suspended',
      { subagentId: 'agent-q', reason: { kind: 'question' } },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-q', subagentPhase: 'awaiting' });
  });

  it('should map the known rate_limit kind to backoff', () => {
    const events = projector.project(
      'subagent.suspended',
      { subagentId: 'agent-r', reason: { kind: 'rate_limit' } },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-r', subagentPhase: 'backoff' });
  });

  it('should fall an UNKNOWN structured kind to the honest suspended, never awaiting', () => {
    // F4: a malformed / future kind (e.g. {kind:'pause'}) is a suspension the
    // client cannot classify as a user wait — the honest phase is the generic
    // 'suspended', not 'awaiting' (the member is NOT awaiting the user just
    // because the reason is an object).
    const events = projector.project(
      'subagent.suspended',
      { subagentId: 'agent-p', reason: { kind: 'pause' } },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-p', subagentPhase: 'suspended' });
    expect(task?.suspendedReason).toBeUndefined();
  });

  it('should keep the legacy string reason mapping to generic suspended', () => {
    const events = projector.project(
      'subagent.suspended',
      { subagentId: 'agent-l', reason: 'Provider rate limit; subagent requeued for retry.' },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-l', subagentPhase: 'suspended' });
    expect(task?.suspendedReason).toBe('Provider rate limit; subagent requeued for retry.');
  });
});

describe('page reload while a member is parked (replay + roster merge)', () => {
  const META: EventMeta = { sessionId: SID, seq: 1 };

  it('should keep a parked member awaiting across the replay-then-roster-merge reload sequence', () => {
    // The reload's subscribe-replay replays the journal's lifecycle events:
    // spawned → started → suspended (the parked member's structured reason).
    const projector = createAgentProjector();
    let state: MirriClientState = createInitialState();
    const replay = [
      ['subagent.spawned', { subagentId: 'agent-p', parentAgentId: 'main' }],
      ['subagent.started', { subagentId: 'agent-p' }],
      [
        'subagent.suspended',
        { subagentId: 'agent-p', reason: { kind: 'approval', request_id: 'req-7' } },
      ],
    ] as const;
    for (const [rawType, payload] of replay) {
      for (const event of projector.project(rawType, payload, SID, { offset: 0 })) {
        state = reduceAppEvent(state, event, META);
      }
    }
    const afterReplay = taskFor(state, 'agent-p');
    expect(afterReplay?.subagentPhase).toBe('awaiting');

    // The snapshot roster then lands; its rows carry NO subagent_phase (the
    // engine's task rows are event-derived only). The merge must not wipe the
    // replayed awaiting phase.
    const roster: import('../types').AppTask[] = [
      {
        id: 'agent-p',
        sessionId: SID,
        kind: 'subagent',
        description: 'member p',
        status: 'running',
        createdAt: '2025-01-01T00:00:00.000Z',
      },
    ];
    const merged = mergeSnapshotSubagents(roster, state.tasksBySession[SID] ?? []);
    const afterMerge = merged.find((t) => t.id === 'agent-p');
    expect(afterMerge?.subagentPhase).toBe('awaiting');
    expect(afterMerge?.suspendedReason).toMatchObject({ kind: 'approval', requestId: 'req-7' });
    // And the row stays awaiting in the resulting store state.
    const storeAfterMerge = { ...state, tasksBySession: { ...state.tasksBySession, [SID]: merged } };
    expect(taskFor(storeAfterMerge, 'agent-p')?.subagentPhase).toBe('awaiting');
  });

  it('should keep a parked member awaiting across the roster-merge-then-replay reload sequence', () => {
    // The other ordering: the snapshot roster merges first (member not yet
    // replayed as suspended), then the replay re-projects the suspension.
    let state: MirriClientState = createInitialState();
    // A roster-seeded skeleton row with no phase.
    const roster: import('../types').AppTask[] = [
      {
        id: 'agent-p',
        sessionId: SID,
        kind: 'subagent',
        description: 'member p',
        status: 'running',
        createdAt: '2025-01-01T00:00:00.000Z',
      },
    ];
    state = { ...state, tasksBySession: { ...state.tasksBySession, [SID]: roster } };

    const projector = createAgentProjector();
    for (const event of projector.project(
      'subagent.suspended',
      { subagentId: 'agent-p', reason: { kind: 'approval', request_id: 'req-7' } },
      SID,
      { offset: 0 },
    )) {
      state = reduceAppEvent(state, event, META);
    }
    const afterReplay = taskFor(state, 'agent-p');
    expect(afterReplay?.subagentPhase).toBe('awaiting');
    expect(afterReplay?.suspendedReason).toMatchObject({ kind: 'approval', requestId: 'req-7' });
  });
});

describe('subagent row identity (agentId projection)', () => {
  it('should carry agentId equal to the row id when subagent.spawned projects a new row', () => {
    // agentId is the cross-pipeline join key the REST poll fold uses — a
    // spawned row must carry it from the very first projection.
    const projector = createAgentProjector();
    const events = projector.project(
      'subagent.spawned',
      { subagentId: 'agent-a', description: 'member a', subagentName: 'agent' },
      SID,
      { offset: 0 },
    );
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-a', agentId: 'agent-a' });
  });

  it('should carry agentId equal to the row id when a lifecycle patch re-projects a row', () => {
    // patchSubagent covers subagent.started/suspended/completed/failed and the
    // status-activity path — the identity must survive every patch.
    const projector = createAgentProjector();
    const events = projector.project('subagent.started', { subagentId: 'agent-b' }, SID, { offset: 0 });
    const task = events.find((e) => e.type === 'taskCreated')?.task;
    expect(task).toMatchObject({ id: 'agent-b', agentId: 'agent-b' });
  });
});

describe('seedTurnUsage (snapshot fold onto the projector)', () => {
  it('should return the seeded usage when a snapshot turn is queried by prompt id', () => {
    const projector = createAgentProjector();
    projector.seedTurnUsage('session-1', [
      {
        promptId: 'turn-1',
        usage: {
          inputTokens: 10,
          outputTokens: 5,
          cacheReadTokens: 1000,
          cacheCreationTokens: 200,
          models: ['kimi-k2'],
        },
      },
    ]);
    expect(turnUsageForPromptId(projector.state('session-1'), 'turn-1')).toEqual({
      inputTokens: 10,
      outputTokens: 5,
      cacheReadTokens: 1000,
      cacheCreationTokens: 200,
      models: ['kimi-k2'],
    });
  });

  it('should not leak seeded usage into a live turn that starts after seeding', () => {
    const projector = createAgentProjector();
    projector.seedTurnUsage('session-1', [
      { promptId: 'turn-1', usage: { inputTokens: 1, outputTokens: 1, cacheReadTokens: 0, cacheCreationTokens: 0, models: [] } },
    ]);
    projector.project('turn.started', { turnId: 'turn-live' }, 'session-1', { offset: 0 });
    expect(
      turnUsageForPromptId(projector.state('session-1'), 'turn-live'),
    ).toBeUndefined();
    expect(turnUsageForPromptId(projector.state('session-1'), 'turn-1')).toBeDefined();
  });

  it('should clear seeded usage when the projector resets', () => {
    const projector = createAgentProjector();
    projector.seedTurnUsage('session-1', [
      { promptId: 'turn-1', usage: { inputTokens: 1, outputTokens: 1, cacheReadTokens: 0, cacheCreationTokens: 0, models: [] } },
    ]);
    projector.reset('session-1');
    expect(turnUsageForPromptId(projector.state('session-1'), 'turn-1')).toBeUndefined();
  });
});

describe('turn.started live elapsed anchor (startedAtMs)', () => {
  it('should stamp the observed wall-clock on the turnActiveChanged activation', () => {
    // Given a live turn starting right now
    const projector = createAgentProjector();
    const before = Date.now();

    // When turn.started projects
    const events = projector.project('turn.started', { turnId: 'turn-live' }, 'session-1', {
      offset: 0,
    });

    // Then the activation event carries the observed wall-clock anchor —
    // the live-observed half of the streaming header's elapsed clock
    const active = events.find(
      (e): e is { type: 'turnActiveChanged'; sessionId: string; active: boolean; startedAtMs: number } =>
        e.type === 'turnActiveChanged' && e.active === true,
    );
    expect(active).toBeDefined();
    expect(active!.startedAtMs).toBeGreaterThanOrEqual(before);
    expect(active!.startedAtMs).toBeLessThanOrEqual(Date.now());
  });
});

describe('turn.ended wireTurnId anchor (fork-at-turn live rebind)', () => {
  it('should rebind the live correlation key to the durable turn key and emit turnAnchored', () => {
    // Given a live turn bound by its UUID correlation key
    const projector = createAgentProjector();
    projector.bindNextPromptId('session-1', 'turn-0192aaaa');
    projector.project('turn.started', { turnId: 'turn-0192aaaa' }, 'session-1', { offset: 0 });

    // When the turn ends with its numeric wire id
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-0192aaaa', reason: 'completed', wireTurnId: 7 },
      'session-1',
      { offset: 0 },
    );

    // Then a turnAnchored event rebinds the rows to the durable key
    const anchored = events.find((e) => e.type === 'turnAnchored');
    expect(anchored).toMatchObject({
      sessionId: 'session-1',
      promptId: 'turn-0192aaaa',
      anchorId: 'turn-7',
    });
    // And the projector's binding follows, so usage queries key on the new id
    expect(turnUsageForPromptId(projector.state('session-1'), 'turn-0192aaaa')).toBeUndefined();
    expect(projector.state('session-1').turnPromptId.get('turn-0192aaaa')).toBe('turn-7');
  });

  it('should not rebind when the ended event carries no wire turn id', () => {
    const projector = createAgentProjector();
    projector.bindNextPromptId('session-1', 'turn-0192aaaa');
    projector.project('turn.started', { turnId: 'turn-0192aaaa' }, 'session-1', { offset: 0 });
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-0192aaaa', reason: 'completed' },
      'session-1',
      { offset: 0 },
    );
    expect(events.some((e) => e.type === 'turnAnchored')).toBe(false);
    expect(projector.state('session-1').turnPromptId.get('turn-0192aaaa')).toBe('turn-0192aaaa');
  });
});

describe('turn.ended server-measured duration (live completion after a resync)', () => {
  it('should stamp the server durationMs on the completed message when the ended event carries it', () => {
    // Given a live turn whose assistant message is streaming
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends and the engine reports its measured duration (the
    // authoritative value the wire turn.ended record carries — the client may
    // have resynced mid-turn and lost its local start anchor, so it cannot
    // estimate this itself)
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'completed', durationMs: 90_324 },
      'session-1',
      { offset: 0 },
    );

    // Then the completed message carries the server's duration — the value
    // the header renders as "Worked for X" instead of a naked "Worked"
    const updated = events.find((e) => e.type === 'messageUpdated');
    expect(updated).toMatchObject({ sessionId: 'session-1', durationMs: 90_324 });
  });

  it('should leave the message duration unset when the ended event carries none', () => {
    // Given a live turn whose assistant message is streaming
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends without a measured duration (engines that predate
    // the field)
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'completed' },
      'session-1',
      { offset: 0 },
    );

    // Then the messageUpdated event carries no durationMs — the header falls
    // back to the naked "Worked" label rather than a fabricated elapsed
    const updated = events.find((e) => e.type === 'messageUpdated');
    expect(updated).toBeDefined();
    expect(updated!.durationMs).toBeUndefined();
  });
});

describe('turn.ended failure error surfacing (failed turns raise the existing agent-error toast)', () => {
  it('should emit an agent-error frame when a failed turn carries a structured error', () => {
    // Given a live turn that failed on the first provider call
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends as failed with the engine's structured error
    const events = projector.project(
      'turn.ended',
      {
        turnId: 'turn-1',
        reason: 'failed',
        error: {
          code: 'provider.api_error',
          message: 'OpenAI API error 400 Bad Request: model not available for integrator',
          retryable: false,
        },
      },
      'session-1',
      { offset: 0 },
    );

    // Then an agent-error frame rides out for the reducer's toast notice —
    // carrying the error's code/message/retryable verbatim
    const agentError = events.find(
      (e) => e.type === 'unknown' && (e as { raw?: { _agentError?: boolean } }).raw?._agentError === true,
    );
    expect(agentError).toBeDefined();
    expect((agentError as { raw?: Record<string, unknown> }).raw).toMatchObject({
      code: 'provider.api_error',
      message: 'OpenAI API error 400 Bad Request: model not available for integrator',
      retryable: false,
      channel: 'turn-failure',
    });
  });

  it('should not pollute the conversation transcript with error text blocks', () => {
    // Given a live turn that failed on the first provider call (no assistant
    // content ever arrived)
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends as failed with a structured error
    const events = projector.project(
      'turn.ended',
      {
        turnId: 'turn-1',
        reason: 'failed',
        error: { code: 'provider.api_error', message: 'boom', retryable: false },
      },
      'session-1',
      { offset: 0 },
    );

    // Then no messageUpdated event carries error text — the transcript keeps
    // no failure bubble; the toast owns the failure surface
    const updates = events.filter(
      (e) => e.type === 'messageUpdated',
    ) as Array<{ content?: Array<{ type: string; text?: string }> }>;
    for (const u of updates) {
      expect(u.content?.some((c) => c.type === 'text' && (c.text ?? '').includes('boom'))).toBe(false);
    }
    expect(events.some((e) => e.type === 'messageCreated')).toBe(false);
  });

  it('should emit no agent-error frame when a failed turn carries no error payload', () => {
    // Given a live turn (older engines may report a bare failed reason)
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends as failed with no structured error
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'failed' },
      'session-1',
      { offset: 0 },
    );

    // Then no agent-error frame is fabricated — nothing to show
    expect(
      events.some(
        (e) => e.type === 'unknown' && (e as { raw?: { _agentError?: boolean } }).raw?._agentError === true,
      ),
    ).toBe(false);
  });
});
