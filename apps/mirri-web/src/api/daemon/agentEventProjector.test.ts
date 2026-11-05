import { describe, expect, it } from 'vitest';

import { classifyFrame, createAgentProjector, turnUsageForPromptId } from './agentEventProjector';
import type { MirriClientState, EventMeta } from './eventReducer';
import { createInitialState, reduceAppEvent } from './eventReducer';
import { isRenderEvent } from '../../composables/client/eventBatcher';
import { memberBlocksFromLive } from '../../lib/memberBlocks';
import { mergeSnapshotSubagents } from '../../lib/taskMerge';
import type { AppMessage } from '../types';

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

describe('terminal row stickiness against late intermediate subagent events (ADR-010 ②)', () => {
  const META: EventMeta = { sessionId: SID, seq: 0 };

  /** Project a wire event through the projector and fold the resulting
   *  AppEvents into the reducer state, returning the folded state. */
  function fold(
    projector: ReturnType<typeof createAgentProjector>,
    state: MirriClientState,
    rawType: string,
    payload: Record<string, unknown>,
  ): MirriClientState {
    let next = state;
    for (const event of projector.project(rawType, payload, SID, { offset: 0 })) {
      next = reduceAppEvent(next, event, META);
    }
    return next;
  }

  /** Seed a terminal subagent row (status/subagentPhase 'completed') by
   *  projecting the full happy-path lifecycle up to subagent.completed. */
  function seedTerminal(
    projector: ReturnType<typeof createAgentProjector>,
    state: MirriClientState,
    subagentId: string,
  ): MirriClientState {
    let next = fold(projector, state, 'subagent.spawned', { subagentId });
    next = fold(projector, next, 'subagent.started', { subagentId });
    return fold(projector, next, 'subagent.completed', { subagentId, resultSummary: 'done' });
  }

  it('should not resurrect a terminal subagent row when a late suspended event replays (ADR-010 ②)', () => {
    // Given: the member already completed (status 'completed', subagentPhase
    // 'completed') in the projected state
    const projector = createAgentProjector();
    let state: MirriClientState = seedTerminal(projector, createInitialState(), 'agent-1');
    expect(taskFor(state, 'agent-1')).toMatchObject({
      status: 'completed',
      subagentPhase: 'completed',
    });

    // When: a late subagent.suspended for the same subagentId replays (a
    // durable-replay / reconnect race against the lifecycle event)
    state = fold(projector, state, 'subagent.suspended', {
      subagentId: 'agent-1',
      reason: { kind: 'approval', request_id: 'req-1', tool_call_id: 'call_x' },
    });

    // Then: the row keeps its terminal status and phase — the late
    // intermediate signal neither resurrects the row nor parks it
    expect(taskFor(state, 'agent-1')).toMatchObject({
      status: 'completed',
      subagentPhase: 'completed',
    });
  });

  it('should not resurrect a terminal subagent row when a late started event or stream delta replays (ADR-010 ②)', () => {
    // Given: subagent-1 terminal (completed) in the projected state
    const projector = createAgentProjector();
    let state: MirriClientState = seedTerminal(projector, createInitialState(), 'subagent-1');
    expect(taskFor(state, 'subagent-1')).toMatchObject({
      status: 'completed',
      subagentPhase: 'completed',
    });

    // When: a late subagent.started replays, then a late assistant.delta and
    // a late thinking.delta for the same member (each delta is dropped on a
    // terminal row — it must neither resurrect the row nor append to its
    // stream)
    state = fold(projector, state, 'subagent.started', { subagentId: 'subagent-1' });
    state = fold(projector, state, 'assistant.delta', {
      agentId: 'subagent-1',
      delta: 'late text fragment',
    });
    state = fold(projector, state, 'thinking.delta', {
      agentId: 'subagent-1',
      delta: 'late thinking fragment',
    });

    // Then: the row stays completed/completed and its stream carries none of
    // the late fragments
    const row = taskFor(state, 'subagent-1');
    expect(row).toMatchObject({ status: 'completed', subagentPhase: 'completed' });
    expect(row?.segments ?? []).toEqual([]);
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

  it('should stamp endedAt from the wire endedAtMs when turn.ended carries it', () => {
    // Given a live turn whose assistant message is streaming
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends and the engine reports the instant it completed
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'completed', durationMs: 1200, endedAtMs: 1_730_000_000_000 },
      'session-1',
      { offset: 0 },
    );

    // Then the footer's endedAt is exactly the engine's instant — never the
    // client's own clock at projection time
    const updated = events.find((e) => e.type === 'messageUpdated');
    expect(updated?.endedAt).toBe(new Date(1_730_000_000_000).toISOString());
  });

  it('should omit endedAt when turn.ended carries no endedAtMs (older engine)', () => {
    // Given a live turn whose assistant message is streaming
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends on an engine that predates the completion instant
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'completed', durationMs: 1200 },
      'session-1',
      { offset: 0 },
    );

    // Then no endedAt is stamped — the footer renders no timestamp rather
    // than a fabricated client clock
    const updated = events.find((e) => e.type === 'messageUpdated');
    expect(updated).toBeDefined();
    expect(updated!.endedAt).toBeUndefined();
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

  it('should not raise an agent-error notice when the turn ended reason is cancelled', () => {
    // Given a live turn that was hard-aborted by the user (ADR-007: the
    // engine projects abort.user_cancelled as reason "cancelled")
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });

    // When the turn ends cancelled (with the abort error payload retained)
    const events = projector.project(
      'turn.ended',
      {
        turnId: 'turn-1',
        reason: 'cancelled',
        error: {
          code: 'abort.user_cancelled',
          message: 'turn aborted by the user via message.abort',
          retryable: false,
        },
      },
      'session-1',
      { offset: 1 },
    );

    // Then no agent-error frame rides out — the failed-only gate keeps the
    // user's own cancellation out of the error-toast channel
    const agentError = events.find(
      (e) =>
        e.type === 'unknown' &&
        (e as { raw?: { _agentError?: boolean } }).raw?._agentError === true,
    );
    expect(agentError).toBeUndefined();
  });
});

describe('subagent step boundary projection (ADR-015 boundary-driven replay dedupe)', () => {
  const META: EventMeta = { sessionId: SID, seq: 0 };

  // local fold for this describe — the file's other fold is scoped to its
  // own describe closure
  function fold(
    projector: ReturnType<typeof createAgentProjector>,
    state: MirriClientState,
    rawType: string,
    payload: Record<string, unknown>,
  ): MirriClientState {
    let next = state;
    for (const event of projector.project(rawType, payload, SID, { offset: 0 })) {
      next = reduceAppEvent(next, event, META);
    }
    return next;
  }

  it('should emit a retry boundary event when a member turn.step.retrying frame arrives', () => {
    // Given a running subagent task row
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'subagent.started', { subagentId: 'sa-1' });

    // When the member's step retries (durable frame, member-attributed)
    const events = projector.project('turn.step.retrying', { agentId: 'sa-1' }, SID);

    // Then exactly one boundary event is emitted with retry: true
    expect(events).toEqual([
      { type: 'subagentStepBoundary', sessionId: SID, taskId: 'sa-1', retry: true },
    ]);
  });

  it('should emit commit boundaries (retry: false) for member turn/step starts and interrupts', () => {
    const projector = createAgentProjector();
    fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });

    for (const rawType of ['turn.started', 'turn.step.started', 'turn.step.interrupted']) {
      const events = projector.project(rawType, { agentId: 'sa-1' }, SID);
      expect(events).toEqual([
        { type: 'subagentStepBoundary', sessionId: SID, taskId: 'sa-1', retry: false },
      ]);
    }
  });

  it('should not emit boundary events for side-channel agents on any boundary frame', () => {
    // A BTW side-chat agent streams its own transcript; the side-channel
    // guard must cover ALL FOUR boundary frames, not just turn.step.started
    // (the only one guarded pre-fix) — otherwise BTW agents would receive
    // task boundary events (review amendment ②).
    const projector = createAgentProjector();
    projector.markSideChannelAgent('btw-1');
    for (const rawType of [
      'turn.started',
      'turn.step.started',
      'turn.step.retrying',
      'turn.step.interrupted',
    ]) {
      expect(projector.project(rawType, { agentId: 'btw-1' }, SID)).toEqual([]);
    }
  });

  it('should not be treated as a render event by the batcher (ordering barrier)', () => {
    // ADR-015 c: the boundary must flush any pending batch so the truncation
    // applies before subsequent deltas — verify via the batcher whitelist.
    expect(isRenderEvent({ type: 'subagentStepBoundary', sessionId: 's', taskId: 't', retry: false })).toBe(false);
  });

  it('should keep the replayed stream free of double-appends when a retry is followed by the replayed stream', () => {
    // Given a member whose first attempt streamed 'abc'
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: 'abc' });
    expect(taskFor(state, 'sa-1')?.segments?.[0]?.text).toBe('abc');

    // When the step retries and the engine re-streams from zero ('abc' then 'def')
    state = fold(projector, state, 'turn.step.retrying', { agentId: 'sa-1' });
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: 'abc' });
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: 'def' });

    // Then the replayed stretch lands with no double-append ('abcdef', not
    // 'abcabcdef' — ADR-011 ②重复计账不变量)；the failed attempt's segment is
    // GONE — the retry boundary's segment truncation (this task's scope) cut
    // it back to the step-start mark {count:0, openLen:0}, so only the
    // replay's own segment remains.
    const segments = taskFor(state, 'sa-1')?.segments ?? [];
    expect(segments.map((s) => s.text)).toEqual(['abcdef']);
  });

  it('should accumulate thinking across taskCreated replacements (RC-4 preserve)', () => {
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });

    // When two thinking deltas stream (each emits a taskCreated first)
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'part-a ' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'part-b' });

    // Then the thinking accumulates in the segment across the taskCreated
    // replacements (the reducer owns the segment list between replacements;
    // the paired incremental chunks are swallowed by the minted guard, so
    // the full text arrives exactly once)
    const segments = taskFor(state, 'sa-1')?.segments ?? [];
    expect(segments).toHaveLength(1);
    expect(segments[0]).toMatchObject({ kind: 'thinking', text: 'part-a part-b' });
  });

  it('should cap a settled step result at the output line limit, keeping the tail', () => {
    // Given a running subagent with an open step
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'subagent.started', { subagentId: 'sa-1' });
    state = fold(
      projector,
      state,
      'tool.use',
      { agentId: 'sa-1', toolCallId: 'tc-1', name: 'Bash', args: { cmd: 'ls' } },
    );

    // When the tool result arrives with an output stringifying to 60 lines
    state = fold(projector, state, 'tool.result', {
      agentId: 'sa-1',
      toolCallId: 'tc-1',
      output: Array.from({ length: 60 }, (_, i) => `out-line-${i}`).join('\n'),
    });

    // Then the step's outputLines is capped at 40 keeping the tail (ADR-015 e)
    const step = taskFor(state, 'sa-1')?.steps?.find((s) => s.id === 'tc-1');
    expect(step?.outputLines).toHaveLength(40);
    expect(step?.outputLines?.[0]).toBe('out-line-20');
    expect(step?.outputLines?.at(-1)).toBe('out-line-59');
  });
});

describe('subagent narrative segments (ADR-011)', () => {
  const META: EventMeta = { sessionId: SID, seq: 0 };

  // local fold — the file's other fold helpers are scoped to their own
  // describe closures
  function fold(
    projector: ReturnType<typeof createAgentProjector>,
    state: MirriClientState,
    rawType: string,
    payload: Record<string, unknown>,
  ): MirriClientState {
    let next = state;
    for (const event of projector.project(rawType, payload, SID, { offset: 0 })) {
      next = reduceAppEvent(next, event, META);
    }
    return next;
  }

  function segmentsFor(state: MirriClientState, id: string) {
    return taskFor(state, id)?.segments ?? [];
  }

  it('should interleave text and thinking segments with tool steps by arrival order when both stream', () => {
    // Given a running subagent row
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });

    // When reasoning, a tool call and text stream in arrival order — every
    // tool frame is preceded by its step boundary (which closes the open segment)
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '考虑一下' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'tool.use', {
      agentId: 'sa-1',
      toolCallId: 'tc1',
      name: 'Read',
      args: { path: 'a.ts' },
    });
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: '先读文件' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '继续' });
    state = fold(projector, state, 'tool.result', { agentId: 'sa-1', toolCallId: 'tc1', output: 'ok' });

    // Then the segments keep arrival order (thinking → text → thinking)…
    expect(segmentsFor(state, 'sa-1').map((s) => [s.kind, s.text])).toEqual([
      ['thinking', '考虑一下'],
      ['text', '先读文件'],
      ['thinking', '继续'],
    ]);

    // …and the tool step's seq sorts between the segments that bracketed it,
    // so the panel can interleave steps and segments on one arrival key.
    const segments = segmentsFor(state, 'sa-1');
    const step = taskFor(state, 'sa-1')?.steps?.find((s) => s.id === 'tc1');
    expect(step?.seq).toBeGreaterThan(segments[0]!.seq);
    expect(step?.seq).toBeLessThan(segments[1]!.seq);
    expect(segments[0]!.seq).toBeLessThan(segments[1]!.seq);
    expect(segments[1]!.seq).toBeLessThan(segments[2]!.seq);
  });

  it('should continue appending to the open segment when no boundary frame intervenes', () => {
    // Given a member whose thinking stream is open
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '考虑' });

    // When two more thinking deltas stream without a boundary
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '一下' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '...' });

    // Then one segment carries the concatenated text
    const segments = segmentsFor(state, 'sa-1');
    expect(segments).toHaveLength(1);
    expect(segments[0]).toMatchObject({ kind: 'thinking', text: '考虑一下...' });
  });

  it('should close the open segment when a boundary frame arrives without truncating', () => {
    // Given an open thinking segment with content X
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'X' });

    // When a step boundary arrives
    const boundaryEvents = projector.project('turn.step.started', { agentId: 'sa-1' }, SID);

    // Then it emits no progress chunk (the boundary itself carries no panel text)
    expect(boundaryEvents.some((e) => e.type === 'taskProgress')).toBe(false);

    // …and the next delta opens a NEW segment instead of extending X
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '新段' });
    const segments = segmentsFor(state, 'sa-1');
    expect(segments.map((s) => s.text)).toEqual(['X', '新段']);
    expect(segments[0]!.id).not.toBe(segments[1]!.id);
  });

  it('should arm a fresh tail guard on each newly opened segment when a boundary closed the previous one', () => {
    // Given segment A opened by the first thinking delta — the minted guard
    // swallows its paired incremental chunk, so A's text arrives exactly once
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'A' });
    expect(segmentsFor(state, 'sa-1')[0]?.text).toBe('A');

    // When a boundary closes A and a new segment B opens with content
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'B' });

    // Then A no longer advertises an armed guard (it is closed, and its guard was
    // consumed by its own paired chunk) and B's fresh guard likewise landed
    // its text exactly once ('B', not 'BB')
    const segments = segmentsFor(state, 'sa-1');
    expect(segments.map((s) => s.text)).toEqual(['A', 'B']);
    // B's guard was consumed by its own paired chunk; A may carry a re-armed
    // (dead) guard from the merge path — no future chunk targets a closed
    // segment, so it is inert state, not contract.
    expect(segments[1]?.tailGuard).not.toBe(true);
  });

  it('should carry the target segment id and the chunk kind on every projected stream chunk', () => {
    // Given a member streaming both kinds
    const projector = createAgentProjector();
    fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });

    // When a thinking delta then a text delta arrive
    const thinkingEvents = projector.project('thinking.delta', { agentId: 'sa-1', delta: '想' }, SID, {
      offset: 0,
    });
    const textEvents = projector.project('assistant.delta', { agentId: 'sa-1', delta: '说' }, SID, {
      offset: 0,
    });

    // Then each chunk keeps its kind and names the segment it belongs to (the
    // reducer's append target); the two kinds open two distinct segments.
    const thinkingChunk = thinkingEvents.find((e) => e.type === 'taskProgress');
    const textChunk = textEvents.find((e) => e.type === 'taskProgress');
    expect(thinkingChunk).toMatchObject({
      kind: 'thinking',
      outputChunk: '想',
      segmentId: 'sa-1-g0-seg-0',
    });
    expect(textChunk).toMatchObject({ kind: 'text', outputChunk: '说', segmentId: 'sa-1-g0-seg-1' });
  });

  it('should survive a first-step retry with an empty segment table (mark {0,0})', () => {
    // Given a member whose very first step retries before any delta streams —
    // the step-start mark counts zero segments
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'draft' });

    // When the first step retries (mark {count:0, openLen:0} → truncate to [])
    let after: MirriClientState = state;
    expect(() => {
      after = fold(projector, after, 'turn.step.retrying', { agentId: 'sa-1' });
    }).not.toThrow();

    // Then the segment table is emptied and every dropped id lands in the
    // tombstone (N-1: count 0 must not crash on the empty kept list)
    const row = after.tasksBySession[SID]?.find((t) => t.id === 'sa-1');
    expect(row?.segments).toEqual([]);
    expect(row?.truncatedSegmentIds).toHaveLength(1);

    // And the retried delta streams into a fresh segment normally
    after = fold(projector, after, 'assistant.delta', { agentId: 'sa-1', delta: 'retry' });
    const retriedRow = after.tasksBySession[SID]?.find((t) => t.id === 'sa-1');
    expect(retriedRow?.segments?.map((s) => s.text)).toEqual(['retry']);
  });

  it('should not resurrect a truncated segment across a retried step boundary and taskCreated replays', () => {
    // Given a member whose first attempt streamed a thinking segment
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'failed attempt' });
    const droppedId = segmentsFor(state, 'sa-1')[0]!.id;

    // When the step retries (truncation drops the failed attempt's segment)
    state = fold(projector, state, 'turn.step.retrying', { agentId: 'sa-1' });
    expect(segmentsFor(state, 'sa-1')).toHaveLength(0);

    // And the retried attempt's step start arrives (non-retry boundary — must
    // NOT clear the tombstone), then the retry re-streams
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'fresh' });

    // Then the failed attempt's segment never comes back — the projector's
    // taskCreated replays carry its full list (the projector never truncates),
    // and the run-scoped tombstone filters it out on every one
    const segments = segmentsFor(state, 'sa-1');
    expect(segments.map((s) => s.id)).not.toContain(droppedId);
    expect(segments.map((s) => s.text)).toEqual(['fresh']);
    const row = state.tasksBySession[SID]?.find((t) => t.id === 'sa-1');
    expect(row?.truncatedSegmentIds).toContain(droppedId);
  });

  it('should re-mint post-reset segments under a fresh generation so old tombstones never swallow them', () => {
    // Given a member with a tombstoned segment from a retry truncation
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'old' });
    state = fold(projector, state, 'turn.step.retrying', { agentId: 'sa-1' });
    const tombstones = state.tasksBySession[SID]?.find((t) => t.id === 'sa-1')?.truncatedSegmentIds ?? [];
    expect(tombstones).toHaveLength(1);
    const tombstonedId = tombstones[0]!;

    // When the session resyncs (projector.reset) and the same agent streams
    // again — the projector re-mints the segment from an empty list, but the
    // generation counter makes the new id distinct from any pre-reset id
    projector.reset(SID);
    state = fold(projector, state, 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: 'after reset' });

    // Then the re-minted segment survives (its id carries the new generation —
    // the old tombstone cannot match it) and the reducer keeps both the
    // narrative and the still-live tombstone
    const segments = segmentsFor(state, 'sa-1');
    expect(segments).toHaveLength(1);
    expect(segments[0]).toMatchObject({ kind: 'thinking', text: 'after reset' });
    expect(segments[0]!.id).not.toBe(tombstonedId);
    expect(segments[0]!.id).toMatch(/-g1-seg-/);
    const row = state.tasksBySession[SID]?.find((t) => t.id === 'sa-1');
    expect(row?.truncatedSegmentIds).toContain(tombstonedId);
  });

  it('should keep post-reset segment seq above every pre-reset seq so the merged timeline never reorders', () => {
    // Given a running member with several pre-reset segments minted at
    // generation 0 (plus a tool step interleaved between them)
    const projector = createAgentProjector();
    let state = fold(projector, createInitialState(), 'subagent.spawned', { subagentId: 'sa-1' });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '早期推理' });
    state = fold(projector, state, 'turn.step.started', { agentId: 'sa-1' });
    state = fold(projector, state, 'tool.use', {
      agentId: 'sa-1',
      toolCallId: 'tc1',
      name: 'Read',
      args: { path: 'a.ts' },
    });
    state = fold(projector, state, 'thinking.delta', { agentId: 'sa-1', delta: '更多推理' });
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: '早期输出' });
    const preReset = segmentsFor(state, 'sa-1');
    expect(preReset.length).toBeGreaterThanOrEqual(3);
    const preResetSeqs = [...preReset.map((s) => s.seq), ...(taskFor(state, 'sa-1')?.steps ?? []).map((st) => st.seq)];

    // When the session resyncs (projector.reset clears the per-member counters)
    // and the member keeps streaming — first straight into a text delta, then
    // into a retried step whose duplicate started frame reuses the open seq
    projector.reset(SID);
    state = fold(projector, state, 'assistant.delta', { agentId: 'sa-1', delta: '重连后输出' });
    state = fold(projector, state, 'tool.call.started', {
      agentId: 'sa-1',
      toolCallId: 'tc2',
      name: 'Bash',
      args: { command: 'ls' },
    });
    // A duplicate started frame (reconnect replay) must reuse the step's
    // original seq — no new counter burn, no intra-generation reordering.
    state = fold(projector, state, 'tool.call.started', {
      agentId: 'sa-1',
      toolCallId: 'tc2',
      name: 'Bash',
      args: { command: 'ls -la' },
    });

    // Then every post-reset mint — segments AND steps — sorts strictly above
    // every pre-reset one (generation bit, I-1: the reducer's surviving old
    // segments otherwise swallow the reconnected narrative in front).
    const row = taskFor(state, 'sa-1');
    const postResetSeqs = [
      ...(row?.segments ?? []).filter((s) => s.id.includes('-g1-')).map((s) => s.seq),
      ...(row?.steps ?? []).filter((st) => st.id === 'tc2').map((st) => st.seq),
    ];
    expect(postResetSeqs.length).toBeGreaterThan(0);
    for (const seq of postResetSeqs) {
      for (const old of preResetSeqs) {
        expect(seq).toBeGreaterThan(old);
      }
    }

    // And the panel timeline reflects it: after the reducer folds the replay,
    // post-reset content renders strictly AFTER the pre-reset narrative.
    const blocks = memberBlocksFromLive(row?.segments, row?.steps);
    const flatText = blocks
      .map((b) => (b.kind === 'text' ? b.text : b.kind === 'thinking' ? b.thinking : ''))
      .filter(Boolean)
      .join('\n');
    expect(flatText.indexOf('早期输出')).toBeLessThan(flatText.indexOf('重连后输出'));

    // And the duplicate started frame kept one step card (same seq reuse)
    expect((row?.steps ?? []).filter((st) => st.id === 'tc2')).toHaveLength(1);
  });
});

describe('tool timing projection (ADR-7)', () => {
  it('should stamp server startedAtMs on the tool-use slot when the started frame carries it', () => {
    // Given a live turn and a started frame with a server start timestamp
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    const startedAtMs = Date.now() - 45_000;
    const events = projector.project(
      'tool.call.started',
      { toolCallId: 'call-1', name: 'bash', args: { command: 'ls' }, startedAtMs },
      'session-1',
      { offset: 2 },
    );
    // When the tool-use slot is emitted, it carries the server timestamp
    const updated = events.find((e) => e.type === 'messageUpdated');
    const toolUse = updated && 'content' in updated
      ? (updated as { content: AppMessage['content'] }).content.find((c) => c.type === 'toolUse')
      : undefined;
    expect(toolUse && 'startedAtMs' in toolUse ? toolUse.startedAtMs : undefined).toBe(startedAtMs);
  });

  it('should stamp durationMs on the tool result when the result frame carries it', () => {
    // Given a running tool call seeded via started
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('tool.call.started',
      { toolCallId: 'call-1', name: 'bash', args: {}, startedAtMs: 1000 }, 'session-1', { offset: 2 });
    // When the result frame carries a server-measured duration
    const events = projector.project(
      'tool.result',
      { toolCallId: 'call-1', output: 'done', isError: false, durationMs: 1_234 },
      'session-1',
      { offset: 3 },
    );
    // Then the created tool-result message carries durationMs
    const created = events.find((e) => e.type === 'messageCreated');
    const result = created && 'message' in created
      ? (created as { message: AppMessage }).message.content.find((c) => c.type === 'toolResult')
      : undefined;
    expect(result && 'durationMs' in result ? result.durationMs : undefined).toBe(1_234);
  });

  it('should leave both timing fields absent when an older engine omits them', () => {
    // Given a started frame with no server timestamp (ADR-7-predating engine)
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    projector.project('turn.step.started', { turnId: 'turn-1' }, 'session-1', { offset: 0 });
    const startedEvents = projector.project(
      'tool.call.started',
      { toolCallId: 'call-1', name: 'bash', args: {} },
      'session-1',
      { offset: 2 },
    );
    // When the result frame likewise carries no server duration
    const resultEvents = projector.project(
      'tool.result',
      { toolCallId: 'call-1', output: 'done', isError: false },
      'session-1',
      { offset: 3 },
    );
    // Then neither slot fabricates a number from the local clock
    const updated = startedEvents.find((e) => e.type === 'messageUpdated');
    const toolUse = updated && 'content' in updated
      ? (updated as { content: AppMessage['content'] }).content.find((c) => c.type === 'toolUse')
      : undefined;
    const created = resultEvents.find((e) => e.type === 'messageCreated');
    const result = created && 'message' in created
      ? (created as { message: AppMessage }).message.content.find((c) => c.type === 'toolResult')
      : undefined;
    expect(toolUse).toBeDefined();
    expect(result).toBeDefined();
    expect(toolUse).not.toHaveProperty('startedAtMs');
    expect(result).not.toHaveProperty('durationMs');
  });
});
