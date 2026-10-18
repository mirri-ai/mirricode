import { describe, expect, it } from 'vitest';

import {
  type EventMeta,
  type MirriClientState,
  createInitialState,
  detectSeqGap,
  reduceAppEvent,
  shouldWatchdogReconcile,
} from './eventReducer';
import type { AppEvent, AppMcpServerStatus, AppMessage, AppTask } from '../types';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const SID = 'sess_1';
const META: EventMeta = { sessionId: SID, seq: 1 };

function makeUserMessage(overrides: Partial<AppMessage> & { id: string }): AppMessage {
  return {
    sessionId: SID,
    role: 'user',
    content: [{ type: 'text', text: 'hello' }],
    createdAt: '2025-01-01T00:00:00.000Z',
    ...overrides,
  };
}

function stateWithMessages(messages: AppMessage[]): MirriClientState {
  const state = createInitialState();
  state.messagesBySession[SID] = messages;
  return state;
}

function messageCreatedEvent(message: AppMessage): AppEvent {
  return { type: 'messageCreated', message };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('messageCreated dual-echo dedup', () => {
  it('should skip second user messageCreated when promptId already exists in transcript', () => {
    // Given: a user message already merged (NOT optimistic anymore) with promptId "pr_abc"
    const existingUserMessage = makeUserMessage({
      id: 'msg_first_echo',
      promptId: 'pr_abc',
    });
    const state = stateWithMessages([existingUserMessage]);

    // When: a messageCreated event arrives for a user message with the same promptId but different id
    const secondEcho = makeUserMessage({
      id: 'msg_second_echo',
      promptId: 'pr_abc',
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondEcho), { ...META, seq: 2 });

    // Then: the message list has exactly one user message (the second was deduped)
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(1);
    expect(userMessages[0]!.id).toBe('msg_first_echo');
  });

  it('should not dedup user messages with different promptIds', () => {
    // Given: a user message with promptId "pr_abc"
    const firstMessage = makeUserMessage({
      id: 'msg_1',
      promptId: 'pr_abc',
    });
    const state = stateWithMessages([firstMessage]);

    // When: a messageCreated event arrives for a user message with a different promptId "pr_xyz"
    const secondMessage = makeUserMessage({
      id: 'msg_2',
      promptId: 'pr_xyz',
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondMessage), { ...META, seq: 2 });

    // Then: both messages exist
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(2);
  });

  it('should not dedup user messages when promptId is undefined', () => {
    // Given: a user message without promptId
    const firstMessage = makeUserMessage({
      id: 'msg_1',
      // promptId deliberately omitted
    });
    const state = stateWithMessages([firstMessage]);

    // When: a messageCreated event arrives for a user message also without promptId
    const secondMessage = makeUserMessage({
      id: 'msg_2',
      // promptId deliberately omitted
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondMessage), { ...META, seq: 2 });

    // Then: existing behavior preserved (append, no false dedup)
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(2);
  });

  it('should dedup second echo after optimistic message is merged by first echo', () => {
    // Given: an optimistic user message (as the client creates on submit)
    const optimisticMsg = makeUserMessage({
      id: 'msg_opt_1',
      promptId: undefined, // not yet stamped by submit response
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    });
    const state = stateWithMessages([optimisticMsg]);

    // When: first echo arrives (prompt.submitted path) — merges into optimistic
    const firstEcho = makeUserMessage({
      id: 'msg_server_1',
      promptId: 'pr_abc',
      content: [{ type: 'text', text: 'hello' }],
    });
    const afterFirst = reduceAppEvent(state, messageCreatedEvent(firstEcho), { ...META, seq: 2 });

    // And: second echo arrives (event.message.created path) — same promptId, different id
    const secondEcho = makeUserMessage({
      id: 'msg_server_2',
      promptId: 'pr_abc',
      content: [{ type: 'text', text: 'hello' }],
    });
    const afterSecond = reduceAppEvent(afterFirst, messageCreatedEvent(secondEcho), { ...META, seq: 3 });

    // Then: exactly one user message remains (optimistic merged by first echo, second deduped)
    const messages = afterSecond.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(1);
    // The merged message keeps the optimistic id (reducer merges into existing slot)
    expect(userMessages[0]!.id).toBe('msg_opt_1');
  });
});

describe('sessionModelSwitched marker', () => {
  const MODEL_SWITCH_EVENT = {
    type: 'sessionModelSwitched' as const,
    sessionId: SID,
    from: 'model-a',
    to: 'model-b',
    seq: 7,
  };

  function findMarkers(state: MirriClientState): AppMessage[] {
    return (state.messagesBySession[SID] ?? []).filter((m) =>
      (m.metadata as { origin?: { kind?: string } } | undefined)?.origin?.kind === 'model_switched');
  }

  it('should append a model-switch marker message when a session model switch event is reduced', () => {
    // Given an initialized session transcript
    const state = stateWithMessages([]);
    // When a durable model switch event arrives
    const next = reduceAppEvent(state, MODEL_SWITCH_EVENT, { ...META, seq: 7 });
    // Then the transcript gains one marker message carrying the origin
    const messages = next.messagesBySession[SID] ?? [];
    const marker = messages.find((m) =>
      (m.metadata as { origin?: { kind?: string } } | undefined)?.origin?.kind === 'model_switched');
    expect(marker).toBeDefined();
    expect(marker?.id).toBe('model-switch-7');
    expect(marker?.metadata).toEqual({
      origin: { kind: 'model_switched', from: 'model-a', to: 'model-b' },
    });
  });

  it('should not duplicate the marker when the same event is reduced twice', () => {
    // Given a state where seq 7 was already reduced
    const first = reduceAppEvent(stateWithMessages([]), MODEL_SWITCH_EVENT, { ...META, seq: 7 });
    // When the same sessionModelSwitched event is reduced again
    const second = reduceAppEvent(first, MODEL_SWITCH_EVENT, { ...META, seq: 7 });
    // Then the marker count stays at one
    const markers = findMarkers(second);
    expect(markers).toHaveLength(1);
    expect(markers[0]?.id).toBe('model-switch-7');
  });
});

describe('messageCreated background-task notification echo guard', () => {
  it('should append a background_task-origin user message instead of reconciling it into an optimistic echo', () => {
    // Given a still-optimistic user message whose text coincides with the
    // notification envelope the engine is about to deliver (the exact
    // collision the cron guard protects against — without the origin guard
    // the reducer would treat the notification as THE echo and swallow the
    // user's turn)
    const envelopeText =
      '<notification id="task:process-abc:completed" category="task" type="task.completed" ' +
      'source_kind="background_task" source_id="process-abc">\n' +
      'Title: Background process completed\n' +
      'Severity: info\nhello\n</notification>';
    const optimistic = makeUserMessage({
      id: 'msg_opt_1',
      content: [{ type: 'text', text: envelopeText }],
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    });
    const state = stateWithMessages([optimistic]);

    // When the engine's terminal notification arrives as a user message
    // (origin background_task — the <notification> envelope the card renders)
    const notification = makeUserMessage({
      id: 'msg_notif_1',
      content: [{ type: 'text', text: envelopeText }],
      metadata: {
        origin: {
          kind: 'background_task',
          taskId: 'process-abc',
          status: 'completed',
          notificationId: 'task:process-abc:completed',
        },
      },
    });
    const result = reduceAppEvent(state, messageCreatedEvent(notification), { ...META, seq: 2 });

    // Then the notification appends as its own message — the optimistic echo
    // stays untouched instead of being swallowed (and replaced) by the
    // notification, so the transcript keeps both turns distinct
    const messages = result.messagesBySession[SID]!;
    expect(messages).toHaveLength(2);
    expect(messages.map((m) => m.id)).toEqual(['msg_opt_1', 'msg_notif_1']);
    expect(messages[0]!.content).toEqual([{ type: 'text', text: envelopeText }]);
  });
});

describe('approvalRequested identity merge (supervision B1)', () => {
  function approvalEvent(agentId?: string): AppEvent {
    return {
      type: 'approvalRequested',
      sessionId: SID,
      approval: {
        approvalId: 'req-1',
        sessionId: SID,
        toolCallId: 'call-1',
        toolName: 'Bash',
        action: 'execute Bash',
        display: { kind: 'shell', command: 'ls' },
        ...(agentId !== undefined ? { agentId } : {}),
        createdAt: '2026-09-14T00:00:00.000Z',
      },
    };
  }

  it('should enrich an existing unattributed approval row when the member identity arrives second', () => {
    // Given the bus event (`event.approval.requested`, no agent_id) landed
    // first and the approval.ask reverse-RPC (carrying the member identity)
    // arrives second — the race the first-writer-wins dedupe must survive
    const state = createInitialState();
    const afterEvent = reduceAppEvent(state, approvalEvent(), META);
    expect(afterEvent.approvalsBySession[SID]).toHaveLength(1);
    expect(afterEvent.approvalsBySession[SID]![0]!.agentId).toBeUndefined();

    // When the ask-derived row for the same request id carries the member
    const afterAsk = reduceAppEvent(
      afterEvent,
      approvalEvent('agent-44444444-4444-4444-8444-444444444444'),
      { ...META, seq: 2 },
    );

    // Then the row is enriched in place — one row, now attributed
    expect(afterAsk.approvalsBySession[SID]).toHaveLength(1);
    expect(afterAsk.approvalsBySession[SID]![0]!.agentId).toBe(
      'agent-44444444-4444-4444-8444-444444444444',
    );
  });

  it('should keep the first attribution when both paths carry the member identity', () => {
    // Given an attributed row already exists
    const state = createInitialState();
    const afterAsk = reduceAppEvent(
      state,
      approvalEvent('agent-44444444-4444-4444-8444-444444444444'),
      META,
    );

    // When the same request arrives again with the same identity
    const again = reduceAppEvent(
      afterAsk,
      approvalEvent('agent-44444444-4444-4444-8444-444444444444'),
      { ...META, seq: 2 },
    );

    // Then the row holds the first attribution — no duplicate, no value
    // churn. (The per-field merge rewrites the row with a fresh object even
    // when no field changes, so the assertion is on values, not array
    // identity — the `merged !== existing` guard is a change marker, not a
    // deep-equality check.)
    expect(again.approvalsBySession[SID]).toHaveLength(1);
    expect(again.approvalsBySession[SID]).toEqual(afterAsk.approvalsBySession[SID]);
  });

  it('should enrich a display-less approval row when the ask payload arrives second', () => {
    // Given the thin bus event row landed first (no display/action — the
    // event.approval.requested projection carries only ids)
    const state = createInitialState();
    const afterEvent = reduceAppEvent(state, {
      type: 'approvalRequested',
      sessionId: SID,
      approval: {
        approvalId: 'req-1',
        sessionId: SID,
        toolCallId: '',
        toolName: 'Write',
        action: '',
        display: undefined,
        createdAt: '',
      },
    }, META);
    expect(afterEvent.approvalsBySession[SID]).toHaveLength(1);

    // When the approval.ask payload (carrying the structured display) arrives
    const afterAsk = reduceAppEvent(afterEvent, {
      type: 'approvalRequested',
      sessionId: SID,
      approval: {
        approvalId: 'req-1',
        sessionId: SID,
        toolCallId: 'call-1',
        toolName: 'Write',
        action: 'execute Write',
        display: { kind: 'file_io', path: '/tmp/a.md', operation: 'write' },
        createdAt: '2026-09-16T00:00:00Z',
      },
    }, META);

    // Then the row is enriched per-field — the approval card can render the
    // write target instead of degrading to a bare action string
    expect(afterAsk.approvalsBySession[SID]).toHaveLength(1);
    expect(afterAsk.approvalsBySession[SID]![0]!.display).toEqual({
      kind: 'file_io',
      path: '/tmp/a.md',
      operation: 'write',
    });
    expect(afterAsk.approvalsBySession[SID]![0]!.action).toBe('execute Write');
  });
});

describe('mcpServerStatus notice', () => {
  const mcpEvent = (server: AppMcpServerStatus): AppEvent =>
    ({ type: 'mcpServerStatus', sessionId: SID, server });

  it('should push an error notice when an MCP server fails to connect', () => {
    // Given a clean state, When a failed pool transition arrives (rust core
    // `event.mcp.server.status`, payload {server:{name,status,error}})
    const state = createInitialState();
    const next = reduceAppEvent(state, mcpEvent({
      name: 'github',
      transport: 'stdio',
      status: 'failed',
      toolCount: 0,
      pool: 'workspace',
      error: 'spawn ENOENT',
    }), META);

    // Then the warning carries a structured error notice (not the raw
    // "Unhandled event" string) so the floating toast renders it
    const warning = next.warnings[0];
    expect(warning).toEqual({
      severity: 'error',
      title: expect.stringContaining('github'),
      message: 'spawn ENOENT',
    });
  });

  it('should push an info notice that self-dismisses when an MCP server connects', () => {
    const state = createInitialState();
    const next = reduceAppEvent(state, mcpEvent({
      name: 'github',
      transport: 'stdio',
      status: 'connected',
      toolCount: 12,
      pool: 'workspace',
    }), META);

    const warning = next.warnings[0];
    expect(warning).toEqual({
      severity: 'info',
      title: expect.stringContaining('github'),
      message: expect.any(String),
    });
  });

  it('should map a needs-auth server to a warning-level notice', () => {
    const state = createInitialState();
    const next = reduceAppEvent(state, mcpEvent({
      name: 'linear',
      transport: 'http',
      status: 'needs-auth',
      toolCount: 0,
      pool: 'user',
    }), META);

    expect(next.warnings[0]).toMatchObject({ severity: 'warning' });
  });

  it('should not push any notice for pending or disabled transitions', () => {
    const state = createInitialState();
    const afterPending = reduceAppEvent(state, mcpEvent({
      name: 'github', transport: 'stdio', status: 'pending', toolCount: 0, pool: 'workspace',
    }), META);
    expect(afterPending.warnings).toHaveLength(0);

    const afterDisabled = reduceAppEvent(afterPending, mcpEvent({
      name: 'github', transport: 'stdio', status: 'disabled', toolCount: 0, pool: 'workspace',
    }), META);
    expect(afterDisabled.warnings).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// detectSeqGap — durable seq gap detection
// ---------------------------------------------------------------------------

describe('detectSeqGap (durable seq gap detection)', () => {
  it('should report a gap when the seq jumps by more than one', () => {
    expect(detectSeqGap(5, 7)).toBe(true); // 中间丢了 seq 6
  });

  it('should report no gap when the seq is contiguous or a replay duplicate', () => {
    expect(detectSeqGap(5, 6)).toBe(false); // 连续
    expect(detectSeqGap(5, 5)).toBe(false); // 重复投递
    expect(detectSeqGap(5, 4)).toBe(false); // 乱序/重放
  });

  it('should report no gap when the seq is zero (volatile event)', () => {
    expect(detectSeqGap(0, 0)).toBe(false);
    expect(detectSeqGap(5, 0)).toBe(false);
  });

  it('should report no gap on the first observed durable event', () => {
    expect(detectSeqGap(0, 1)).toBe(false);
    expect(detectSeqGap(0, 100)).toBe(false); // 无基线的首事件不判间隙
  });
});

// ---------------------------------------------------------------------------
// shouldWatchdogReconcile — turn-active watchdog decision
// ---------------------------------------------------------------------------

describe('shouldWatchdogReconcile (turn-active watchdog)', () => {
  it('should reconcile when a subscribed session is turn-active and silent past the threshold', () => {
    expect(shouldWatchdogReconcile(300_001, 300_000, true)).toBe(true);
    expect(shouldWatchdogReconcile(600_000, 300_000, true)).toBe(true);
  });

  it('should stay quiet when events are still flowing within the threshold', () => {
    expect(shouldWatchdogReconcile(120_000, 300_000, true)).toBe(false);
    expect(shouldWatchdogReconcile(299_999, 300_000, true)).toBe(false);
  });

  it('should stay quiet for unsubscribed sessions (no events is normal for them)', () => {
    expect(shouldWatchdogReconcile(600_000, 300_000, false)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// turnAnchored — fork-at-turn live rebind
// ---------------------------------------------------------------------------

describe('turnAnchored (fork-at-turn live rebind)', () => {
  it('should rebind the turn\'s rows from the correlation key to the durable turn key', () => {
    // Given a live-completed turn whose rows carry the UUID correlation key
    const state = stateWithMessages([
      makeUserMessage({ id: 'msg-user', promptId: 'turn-0192aaaa' }),
      {
        ...makeUserMessage({ id: 'msg-a' }),
        role: 'assistant',
        promptId: 'turn-0192aaaa',
      },
      makeUserMessage({ id: 'msg-other-turn', promptId: 'turn-1' }),
    ]);

    // When the engine reports the turn's numeric wire id
    const next = reduceAppEvent(
      state,
      { type: 'turnAnchored', sessionId: SID, promptId: 'turn-0192aaaa', anchorId: 'turn-7' },
      META,
    );

    // Then only the turn's rows are rekeyed — the durable-keyed row is untouched
    const msgs = next.messagesBySession[SID]!;
    expect(msgs.map((m) => [m.id, m.promptId])).toEqual([
      ['msg-user', 'turn-7'],
      ['msg-a', 'turn-7'],
      ['msg-other-turn', 'turn-1'],
    ]);
  });
});

// ---------------------------------------------------------------------------
// turnActiveChanged — streaming run's live elapsed anchor (turnStartAtBySession)
// ---------------------------------------------------------------------------

describe('turnActiveChanged (live elapsed anchor)', () => {
  it('should store the projector\'s turn.started wall-clock as the session anchor', () => {
    // Given an idle session with no stored anchor
    const state = createInitialState();

    // When a turn activates with the observed turn.started stamp
    const next = reduceAppEvent(
      state,
      { type: 'turnActiveChanged', sessionId: SID, active: true, startedAtMs: 1750000000000 },
      META,
    );

    // Then the anchor is stored per session for the streaming header
    expect(next.turnActiveBySession[SID]).toBe(true);
    expect(next.turnStartAtBySession[SID]).toBe(1750000000000);
  });

  it('should clear a stale anchor when a turn activates without a stamp', () => {
    // Given a stored anchor from a previous snapshot/restore
    const state = createInitialState();
    state.turnStartAtBySession[SID] = 1750000000000;

    // When a turn activates on an older engine that carries no startedAtMs
    const next = reduceAppEvent(
      state,
      { type: 'turnActiveChanged', sessionId: SID, active: true },
      META,
    );

    // Then the anchor is dropped — never tick a stale elapsed from an
    // earlier turn
    expect(next.turnActiveBySession[SID]).toBe(true);
    expect(next.turnStartAtBySession[SID]).toBeUndefined();
  });

  it('should drop the anchor when the turn ends', () => {
    // Given an active turn with a stored anchor
    const state = createInitialState();
    state.turnActiveBySession[SID] = true;
    state.turnStartAtBySession[SID] = 1750000000000;

    // When the turn deactivates
    const next = reduceAppEvent(
      state,
      { type: 'turnActiveChanged', sessionId: SID, active: false, reason: 'completed' },
      META,
    );

    // Then both the liveness flag and the anchor clear — the completed turn
    // renders "Worked for X" from the server-stamped durationMs instead
    expect(next.turnActiveBySession[SID]).toBeUndefined();
    expect(next.turnStartAtBySession[SID]).toBeUndefined();
  });
});

describe('taskCreated identity preserve (agentId)', () => {
  it('should preserve the previous agentId when the replacing projected task omits it', () => {
    // Given: a live subagent row that already carries its agentId (seeded by
    // projection or a REST fold) — the cross-pipeline join key
    const previous: AppTask = {
      id: 'agent-p',
      sessionId: SID,
      kind: 'subagent',
      description: 'member p',
      status: 'running',
      createdAt: '2025-01-01T00:00:00.000Z',
      agentId: 'agent-p',
    };
    const state = createInitialState();
    state.tasksBySession[SID] = [previous];

    // When: a lifecycle event re-projects the task as a skeleton without
    // agentId (the shape a resync/re-projection produces)
    const projected: AppTask = {
      id: 'agent-p',
      sessionId: SID,
      kind: 'subagent',
      description: 'Sub Agent',
      status: 'running',
      createdAt: '2025-01-01T00:00:01.000Z',
    };
    const next = reduceAppEvent(state, { type: 'taskCreated', sessionId: SID, task: projected }, META);

    // Then: the row's agentId survives the replacement — otherwise the next
    // REST poll fold misses the join and a second row flickers in
    const row = next.tasksBySession[SID]?.find((t) => t.id === 'agent-p');
    expect(row?.agentId).toBe('agent-p');
  });
});

describe('failed-turn agent-error frame reaches the warning toast state', () => {
  it('should add a structured error notice to warnings when the agent-error frame is reduced', () => {
    // Given a clean client state and the frame the turn.ended projector
    // emits for a failed turn carrying a provider error
    const state = createInitialState();
    const frame = {
      type: 'unknown',
      raw: {
        _agentError: true,
        code: 'provider.api_error',
        message: 'OpenAI API error 400 Bad Request: model not available for integrator',
        retryable: false,
      },
    } as unknown as Parameters<typeof reduceAppEvent>[1];

    // When the frame is reduced
    const next = reduceAppEvent(state, frame, { ...META, seq: 2 });

    // Then exactly one structured error notice lands in warnings — the
    // payload WarningToasts renders as the bottom-right toast
    expect(next.warnings).toHaveLength(1);
    const notice = next.warnings[0];
    expect(typeof notice).toBe('object');
    expect(notice).toMatchObject({
      severity: 'error',
      message: 'OpenAI API error 400 Bad Request: model not available for integrator',
    });
  });
});

describe('turn-failure channel routing', () => {
  it('should carry the channel through to the warnings notice', () => {
    // Given a clean state and the frame the turn.ended projector emits
    const state = createInitialState();
    const frame = {
      type: 'unknown',
      raw: {
        _agentError: true,
        code: 'provider.api_error',
        message: 'boom',
        retryable: false,
        channel: 'turn-failure',
      },
    } as unknown as Parameters<typeof reduceAppEvent>[1];

    // When the frame is reduced
    const next = reduceAppEvent(state, frame, { ...META, seq: 2 });

    // Then the notice carries the turn-failure channel
    expect(next.warnings).toHaveLength(1);
    expect((next.warnings[0] as { channel?: string }).channel).toBe('turn-failure');
  });

  it('should clear stale turn-failure notices when a new turn starts', () => {
    // Given a state holding a turn-failure notice and an unrelated warning
    const state = createInitialState();
    const withFailure = reduceAppEvent(
      state,
      {
        type: 'unknown',
        raw: { _agentError: true, code: 'provider.api_error', message: 'boom', retryable: false, channel: 'turn-failure' },
      } as unknown as Parameters<typeof reduceAppEvent>[1],
      { ...META, seq: 2 },
    );
    const withBoth = {
      ...withFailure,
      warnings: [...withFailure.warnings, 'unrelated warning' as const],
    };

    // When the next turn activates
    const next = reduceAppEvent(
      withBoth,
      { type: 'turnActiveChanged', sessionId: SID, active: true, startedAtMs: 1 },
      { ...META, seq: 3 },
    );

    // Then only the unrelated warning survives
    expect(next.warnings).toEqual(['unrelated warning']);
  });
});
