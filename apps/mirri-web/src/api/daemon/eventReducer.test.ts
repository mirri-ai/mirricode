import { describe, expect, it } from 'vitest';

import {
  type EventMeta,
  type MirriClientState,
  createInitialState,
  detectSeqGap,
  reduceAppEvent,
  shouldWatchdogReconcile,
} from './eventReducer';
import type {
  AppApprovalRequest,
  AppEvent,
  AppMcpServerStatus,
  AppMessage,
  AppSession,
  AppSessionUsage,
  AppTask,
} from '../types';

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

describe('messageCreated optimistic steer echo reconciliation (ADR-010)', () => {
  it('should keep the persisted origin payload and the optimistic marker after the steer row reconciles', () => {
    // Given the ctrl+s optimistic echo (marked with the user_steer origin so
    // it folds into the running turn before the engine has persisted anything)
    const optimistic = makeUserMessage({
      id: 'msg_opt_steer',
      content: [{ type: 'text', text: 'focus on the parser first' }],
      metadata: {
        'mirriWeb.optimisticUserMessage': true,
        origin: { kind: 'user_steer' },
      },
    });
    const state = stateWithMessages([optimistic]);

    // When the engine's own persisted row for the same steer arrives carrying
    // an origin payload the optimistic side could not know (the turn the
    // steer was admitted into)
    const persisted = makeUserMessage({
      id: 'msg_server_steer',
      promptId: 'pr_steer',
      content: [{ type: 'text', text: 'focus on the parser first' }],
      metadata: { origin: { kind: 'user_steer', turnId: 't1' } },
    });
    const result = reduceAppEvent(state, messageCreatedEvent(persisted), { ...META, seq: 2 });

    // Then the engine's origin payload survives the merge — `origin` is what
    // the transcript folds on, and the optimistic echo only guessed the kind.
    // Spread order is load-bearing here: the optimistic side must NOT
    // wholesale-replace `origin`, or a future payload field would be masked
    // live and reappear after a reload.
    const messages = result.messagesBySession[SID] ?? [];
    expect(messages).toHaveLength(1);
    expect(messages[0]!.id).toBe('msg_opt_steer');
    const origin = (messages[0]!.metadata as { origin?: { kind?: string; turnId?: string } })
      .origin;
    expect(origin).toEqual({ kind: 'user_steer', turnId: 't1' });
    // …while the client-only marker still survives (the engine never sends it)
    expect(messages[0]!.metadata?.['mirriWeb.optimisticUserMessage']).toBe(true);
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

// ---------------------------------------------------------------------------
// Live todo-write fold (snapshot-loaded todos stay current)
// ---------------------------------------------------------------------------

function assistantWithTodoWrite(id: string, todos: Array<{ title: string; status: string }>): AppMessage {
  return {
    id,
    sessionId: SID,
    role: 'assistant',
    content: [
      { type: 'text', text: 'here is the list' },
      { type: 'toolUse', toolCallId: 'call-1', toolName: 'TodoList', input: { todos } },
    ],
    createdAt: '2025-01-01T00:00:01.000Z',
  };
}

describe('todosBySession live fold', () => {
  it('should fold a live TodoList write from messageUpdated into todosBySession', () => {
    // Given a snapshot-loaded session whose todos state holds the pre-write list
    const state = createInitialState();
    state.todosBySession[SID] = [
      { title: 'Old', status: 'pending' },
      { title: 'Stale', status: 'done' },
    ];
    const msg = assistantWithTodoWrite('msg-1', [
      { title: 'New', status: 'pending' },
      { title: 'Fresh', status: 'in_progress' },
    ]);

    // When the assistant message carrying the newer list lands as messageUpdated
    const next = reduceAppEvent(
      state,
      { type: 'messageUpdated', sessionId: SID, messageId: 'msg-1', content: msg.content, status: 'pending' },
      { ...META, seq: 2 },
    );

    // Then todosBySession reflects the live write — the snapshot copy is never
    // preferred over a newer live write
    expect(next.todosBySession[SID]).toEqual([
      { title: 'New', status: 'pending' },
      { title: 'Fresh', status: 'in_progress' },
    ]);
  });

  it('should fold a live TodoList write from messageCreated into todosBySession', () => {
    // Given a session with no snapshot-loaded todos (older engine fallback path)
    const state = createInitialState();

    // When an assistant message carrying a whole-list write is created
    const next = reduceAppEvent(
      state,
      messageCreatedEvent(
        assistantWithTodoWrite('msg-2', [
          { title: 'Created', status: 'pending' },
          { title: 'Completed', status: 'done' },
        ]),
      ),
      { ...META, seq: 2 },
    );

    // Then the write is folded even without a snapshot — the live path stays
    // self-sufficient for engines that never send snapshot todos
    expect(next.todosBySession[SID]).toEqual([
      { title: 'Created', status: 'pending' },
      { title: 'Completed', status: 'done' },
    ]);
  });

  it('should keep an explicit empty todo write as a present empty list', () => {
    // Given a session whose todos state holds a stale list
    const state = createInitialState();
    state.todosBySession[SID] = [{ title: 'Old', status: 'pending' }];

    // When a TodoList write clears the list (explicit empty todos array)
    const next = reduceAppEvent(
      state,
      { type: 'messageUpdated', sessionId: SID, messageId: 'msg-3', content: assistantWithTodoWrite('msg-3', []).content, status: 'pending' },
      { ...META, seq: 3 },
    );

    // Then the cleared list is present-but-empty — never falls back to deriving
    // the pre-clear list from older messages
    expect(next.todosBySession[SID]).toEqual([]);
  });

  it('should ignore read-only todo queries and non-todo writes', () => {
    // Given a session with no todos state
    const state = createInitialState();

    // When a read-only TodoList query (no todos arg) and a Bash call land
    const query: AppEvent = {
      type: 'messageUpdated',
      sessionId: SID,
      messageId: 'msg-4',
      content: [
        { type: 'toolUse', toolCallId: 'call-q', toolName: 'TodoList', input: { query: true } },
        { type: 'toolUse', toolCallId: 'call-b', toolName: 'Bash', input: { command: 'ls' } },
      ],
      status: 'pending',
    };
    const next = reduceAppEvent(state, query, { ...META, seq: 4 });

    // Then no todos entry is created — read-only queries carry no state
    expect(next.todosBySession[SID]).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// taskCompleted terminal atomization (ADR-010 ①②) — writing a terminal
// status must flip subagentPhase in the same reduction, and a late
// taskCompleted must not resurrect an already-terminal row.
// ---------------------------------------------------------------------------

describe('taskCompleted terminal atomization (ADR-010 ①②)', () => {
  function subagentTask(overrides: Partial<AppTask> & { id: string }): AppTask {
    return {
      sessionId: SID,
      kind: 'subagent',
      description: 'member task',
      status: 'running',
      createdAt: '2025-01-01T00:00:00.000Z',
      ...overrides,
    };
  }

  function taskCompletedEvent(sessionId: string, taskId: string, status: AppTask['status']): AppEvent {
    return { type: 'taskCompleted', sessionId, taskId, status };
  }

  it('should sync subagentPhase when taskCompleted finalizes a running subagent row (ADR-010 ①)', () => {
    // Given a session with a running subagent row keyed by agent id
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'agent-1', status: 'running', subagentPhase: 'working' }),
    ];

    // When the reducer folds a taskCompleted carrying the agent id (fallback join)
    const next = reduceAppEvent(state, taskCompletedEvent('s1', 'agent-1', 'completed'), META);

    // Then both fields flip together
    const row = next.tasksBySession['s1']!.find((t) => t.id === 'agent-1')!;
    expect(row.status).toBe('completed');
    expect(row.subagentPhase).toBe('completed');
  });

  it('should not resurrect a terminal row when a late taskCompleted carries a different status (ADR-010 ②)', () => {
    // Given a row already finalized as completed (sticky terminal state)
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'agent-1', status: 'completed', subagentPhase: 'completed' }),
    ];

    // When a late taskCompleted arrives carrying a different status
    const next = reduceAppEvent(state, taskCompletedEvent('s1', 'agent-1', 'failed'), META);

    // Then the row keeps its first terminal status — no resurrection
    const row = next.tasksBySession['s1']!.find((t) => t.id === 'agent-1')!;
    expect(row.status).toBe('completed');
    expect(row.subagentPhase).toBe('completed');
  });
});

describe('taskProgress write granularity (SDR-003b)', () => {
  function makeAppTask(overrides: Partial<AppTask> & { id: string }): AppTask {
    return {
      sessionId: 's1',
      kind: 'bash',
      description: 'bash task',
      status: 'running',
      createdAt: '2025-01-01T00:00:00.000Z',
      ...overrides,
    };
  }

  it('should keep the task map reference when the chunk hits an unknown task id', () => {
    // Given a session with one task row
    const state = createInitialState();
    state.tasksBySession['s1'] = [makeAppTask({ id: 't-a' })];

    // When a progress chunk arrives for a task id the map does not know
    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 't-unknown', outputChunk: 'x', stream: 'stdout' },
      { sessionId: 's1', seq: 1 },
    );

    // Then the task map keeps its reference — an unknown id must not produce a
    // new map (no invalidation for consumers of s1's tasks at all —
    // SDR-003b's "no change, no commit").
    expect(after.tasksBySession).toBe(state.tasksBySession);
  });

  it('should keep the task map reference when the appended chunk duplicates the last output line', () => {
    // Given a task whose last output line is 'x' (the dedupe shape)
    const state = createInitialState();
    const task = makeAppTask({ id: 't-a', outputLines: ['x'] });
    state.tasksBySession['s1'] = [task];

    // When the same 'x' chunk is resent, the append is a dedupe no-op
    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 't-a', outputChunk: 'x', stream: 'stdout' },
      { sessionId: 's1', seq: 1 },
    );

    // Then the task map keeps its reference — a no-op chunk must not write a new map
    expect(after.tasksBySession).toBe(state.tasksBySession);
  });

  it('should keep sibling task references when one task receives a chunk', () => {
    // Given two task rows in one session (regression guard for the reference
    // contract — keeps the map/entry-level isolation honest after the
    // clone-on-write conversion).
    const state = createInitialState();
    const taskA = makeAppTask({ id: 't-a' });
    const taskB = makeAppTask({ id: 't-b' });
    state.tasksBySession['s1'] = [taskA, taskB];

    // When taskA receives a progress chunk
    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 't-a', outputChunk: 'x', stream: 'stdout' },
      { sessionId: 's1', seq: 1 },
    );

    // Then only the target row is replaced — the sibling keeps its reference
    const next = after.tasksBySession['s1']!;
    expect(next[1]).toBe(taskB);
    expect(next[0]).not.toBe(taskA);
    expect(next[0]!.outputLines).toEqual(['x']);
  });
});

// ---------------------------------------------------------------------------
// Reference reuse contract (event-delivery ADR-002 / SDR-003b)
// ---------------------------------------------------------------------------

describe('state reference reuse (SDR-003b invalidation isolation)', () => {
  it('should keep untouched per-session map references when an event only advances seq', () => {
    const before = createInitialState();
    before.messagesBySession = { s1: [], s2: [] };
    before.tasksBySession = { s1: [] };
    before.approvalsBySession = { s1: [] };

    const after = reduceAppEvent(
      before,
      { type: 'unknown', raw: {} },
      { sessionId: 's1', seq: 7 },
    );

    // Untouched maps share the reference — a write-back that compares by
    // reference must skip the assignment (no invalidation for other
    // sessions' consumers).
    expect(after.messagesBySession).toBe(before.messagesBySession);
    expect(after.tasksBySession).toBe(before.tasksBySession);
    expect(after.approvalsBySession).toBe(before.approvalsBySession);
    expect(after.lastSeqBySession.s1).toBe(7);
  });

  // Reducer purity guard — the mechanical safety net for the clone-on-write
  // conversion: a missed clone point would mutate the INPUT state in place
  // (the reducer output aliases reactive shared state), and this test
  // catches every such miss regardless of which case it lives in.
  it('should never mutate the input state for any event type', () => {
    const input = seededState();
    const events: AppEvent[] = [
      messageCreatedEvent(makeMessage('m2', 's1')),
      { type: 'taskProgress', sessionId: 's1', taskId: 't1', outputChunk: 'x', stream: 'stdout' },
      { type: 'taskCreated', sessionId: 's1', task: makePurityAppTask('t2') },
      { type: 'taskCompleted', sessionId: 's1', taskId: 't1', status: 'completed', outputPreview: 'done' },
      { type: 'assistantDelta', sessionId: 's1', messageId: 'm1', contentIndex: 0, delta: { text: 'y' } },
      { type: 'toolOutput', sessionId: 's1', toolCallId: 'c1', outputChunk: 'z', stream: 'stdout' },
      { type: 'sessionWorkChanged', sessionId: 's1', busy: false, mainTurnActive: false, pendingInteraction: 'none', lastTurnReason: 'completed' },
      { type: 'turnActiveChanged', sessionId: 's1', active: false, reason: 'completed' },
      { type: 'approvalRequested', sessionId: 's1', approval: makePurityApproval('a1') },
      { type: 'goalUpdated', sessionId: 's1', goal: null },
      { type: 'sessionMetaUpdated', sessionId: 's1', title: 'renamed' },
      { type: 'sessionUsageUpdated', sessionId: 's1', usage: makePurityUsage() },
      { type: 'sessionDeleted', sessionId: 's2' },
      { type: 'messageUpdated', sessionId: 's1', messageId: 'm1', content: [{ type: 'text', text: 'updated' }], status: 'completed' },
      { type: 'toolCallBackgrounded', sessionId: 's1', toolCallId: 'c1', taskId: 't9' },
      { type: 'compactionCompleted', sessionId: 's1', tokensBefore: 100, tokensAfter: 50, summary: 'compacted' },
    ];
    for (const event of events) {
      const snapshot = structuredClone(input);
      reduceAppEvent(snapshot, event, { sessionId: 's1', seq: 1 });
      expect(snapshot).toEqual(input); // in-place pollution fails here
    }
  });

  // Local fixtures for the purity guard — reference the existing helpers'
  // construction style above (makeUserMessage / subagentTask / approvalEvent).
  function makeMessage(id: string, sessionId: string): AppMessage {
    return {
      id,
      sessionId,
      role: 'user',
      content: [{ type: 'text', text: 'hi' }],
      createdAt: '2025-01-01T00:00:00.000Z',
    };
  }

  function makePurityAppTask(id: string): AppTask {
    return {
      id,
      sessionId: 's1',
      kind: 'bash',
      description: 'bash task',
      status: 'running',
      createdAt: '2025-01-01T00:00:00.000Z',
    };
  }

  function makePurityApproval(id: string): AppApprovalRequest {
    return {
      approvalId: id,
      sessionId: 's1',
      toolCallId: 'call-1',
      toolName: 'Bash',
      action: 'execute Bash',
      display: { kind: 'shell', command: 'ls' },
      createdAt: '2025-01-01T00:00:00.000Z',
    };
  }

  function makePurityUsage(): AppSessionUsage {
    return {
      inputTokens: 1,
      outputTokens: 2,
      cacheReadTokens: 3,
      cacheCreationTokens: 4,
      totalCostUsd: 0.5,
      contextTokens: 100,
      contextLimit: 200,
      turnCount: 1,
    };
  }

  /** sessions + one loaded session's messages/tasks/approvals/goal/todos/
   *  turnActive etc. — the fullest per-session shape the reducer touches. */
  function seededState(): MirriClientState {
    const state = createInitialState();
    state.sessions = [
      {
        id: 's1',
        title: 'session one',
        createdAt: '2025-01-01T00:00:00.000Z',
        updatedAt: '2025-01-01T00:00:00.000Z',
        busy: false,
        archived: false,
        cwd: '/tmp/w',
        model: 'model-a',
        usage: makePurityUsage(),
        messageCount: 1,
        lastSeq: 0,
      },
      {
        id: 's2',
        title: 'session two',
        createdAt: '2025-01-01T00:00:00.000Z',
        updatedAt: '2025-01-01T00:00:00.000Z',
        busy: false,
        archived: false,
        cwd: '/tmp/w',
        model: 'model-a',
        usage: makePurityUsage(),
        messageCount: 0,
        lastSeq: 0,
      },
    ];
    state.messagesBySession = {
      s1: [
        makeMessage('m1', 's1'),
        {
          ...makeMessage('m2', 's1'),
          role: 'assistant',
          content: [{ type: 'toolUse', toolCallId: 'c1', toolName: 'Bash', input: {} }],
        },
      ],
    };
    state.tasksBySession = {
      s1: [
        {
          id: 't1',
          sessionId: 's1',
          kind: 'bash',
          description: 'bash task',
          status: 'running',
          createdAt: '2025-01-01T00:00:00.000Z',
          outputLines: ['line'],
        },
      ],
    };
    state.approvalsBySession = {
      s1: [makePurityApproval('a1')],
    };
    state.goalBySession = {
      s1: {
        goalId: 'g1',
        objective: 'do the thing',
        status: 'active',
        turnsUsed: 1,
        tokensUsed: 10,
        wallClockMs: 1000,
        budget: {
          tokenBudget: null,
          remainingTokens: null,
          turnBudget: null,
          remainingTurns: null,
          wallClockBudgetMs: null,
          remainingWallClockMs: null,
          overBudget: false,
        },
      },
    };
    state.goalVersionBySession = { s1: 1 };
    state.turnActiveBySession = { s1: true };
    state.turnStartAtBySession = { s1: 1750000000000 };
    state.compactionBySession = { s1: { status: 'running', trigger: 'auto' } };
    state.backgroundedToolCalls = { s1: { 'call-9': 'bt-1' } };
    state.todosBySession = { s1: [{ title: 'todo', status: 'pending' }] };
    state.lastSeqBySession = { s1: 1, s2: 1 };
    state.warnings = ['existing warning'];
    return state;
  }
});

describe('messageCreated recency (SDR-014: content output is not use)', () => {
  it('should not bump the session sort key when an assistant message is created', () => {
    const state = createInitialState();
    state.sessions = [makeRecencySession('s1')];
    const originalUpdated = state.sessions[0]!.updatedAt;
    const originalLastUsed = state.sessions[0]!.lastUsedAt;

    const after = reduceAppEvent(
      state,
      messageCreatedEvent({
        id: 'm1',
        sessionId: 's1',
        role: 'assistant',
        content: [{ type: 'text', text: 'hi' }],
        createdAt: '2099-01-01T00:00:00Z',
      }),
      { sessionId: 's1', seq: 1 },
    );

    expect(after.sessions[0]!.updatedAt).toBe(originalUpdated);
    expect(after.sessions[0]!.lastUsedAt).toBe(originalLastUsed);
  });

  it('should bump lastUsedAt but not updatedAt when a user message is created', () => {
    const state = createInitialState();
    state.sessions = [makeRecencySession('s1')];

    const after = reduceAppEvent(
      state,
      messageCreatedEvent({
        id: 'm1',
        sessionId: 's1',
        role: 'user',
        content: [{ type: 'text', text: 'hi' }],
        createdAt: '2099-01-01T00:00:00Z',
      }),
      { sessionId: 's1', seq: 1 },
    );

    expect(after.sessions[0]!.updatedAt).toBe(state.sessions[0]!.updatedAt);
    expect(after.sessions[0]!.lastUsedAt).toBe('2099-01-01T00:00:00Z');
  });

  it('should not bump lastUsedAt for a cron-injected user message', () => {
    const state = createInitialState();
    state.sessions = [makeRecencySession('s1')];

    const after = reduceAppEvent(
      state,
      messageCreatedEvent({
        id: 'm1',
        sessionId: 's1',
        role: 'user',
        content: [{ type: 'text', text: 'scheduled' }],
        createdAt: '2099-01-01T00:00:00Z',
        metadata: { origin: { kind: 'cron_job' } },
      }),
      { sessionId: 's1', seq: 1 },
    );

    expect(after.sessions[0]!.lastUsedAt).toBeUndefined();
  });

  it('should not move lastUsedAt backwards when an older user message arrives', () => {
    const state = createInitialState();
    state.sessions = [
      makeRecencySession('s1', { lastUsedAt: '2099-06-01T00:00:00Z' }),
    ];

    const after = reduceAppEvent(
      state,
      messageCreatedEvent({
        id: 'm1',
        sessionId: 's1',
        role: 'user',
        content: [{ type: 'text', text: 'hi' }],
        createdAt: '2098-01-01T00:00:00Z',
      }),
      { sessionId: 's1', seq: 1 },
    );

    expect(after.sessions[0]!.lastUsedAt).toBe('2099-06-01T00:00:00Z');
  });

  /** A minimal session row for the recency cases — only the fields the
   *  messageCreated recency fold reads. */
  function makeRecencySession(
    id: string,
    overrides: Partial<AppSession> = {},
  ): AppSession {
    return {
      id,
      title: 'session one',
      createdAt: '2025-01-01T00:00:00.000Z',
      updatedAt: '2025-01-01T00:00:00.000Z',
      busy: false,
      archived: false,
      cwd: '/tmp/w',
      model: 'model-a',
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
      messageCount: 1,
      lastSeq: 0,
      ...overrides,
    };
  }
});

describe('subagentStepBoundary reduction (ADR-015)', () => {
  // local helper — the file's other subagentTask is scoped to its own describe
  function subagentTask(overrides: Partial<AppTask> & { id: string }): AppTask {
    return {
      sessionId: 's1',
      kind: 'subagent',
      status: 'running',
      createdAt: '2026-01-01T00:00:00Z',
      description: 'member',
      ...overrides,
    };
  }

  it('should truncate text and thinking to the step-start marks and arm the tail guard when a retry boundary arrives', () => {
    // Given a running subagent with marks recorded at an empty step start,
    // then step text/thinking accumulated
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({
        id: 'sa-1',
        text: 'step1step2',
        thinking: 'think1think2',
        stepTextLen: 5,
        stepThinkLen: 6,
      }),
    ];

    // When a retry boundary arrives
    const after = reduceAppEvent(
      state,
      { type: 'subagentStepBoundary', sessionId: 's1', taskId: 'sa-1', retry: true },
      { sessionId: 's1', seq: 1 },
    );

    // Then both fields are truncated back to the step start and the one-shot
    // tail guard is armed (design F-1 ③: BOTH boundary branches arm it — the
    // replay-after-retry is the guard's primary scenario)
    const row = after.tasksBySession['s1']![0]!;
    expect(row.text).toBe('step1');
    expect(row.thinking).toBe('think1');
    expect(row.tailGuard).toBe(true);
  });

  it('should commit new marks on a non-retry boundary and arm the tail guard', () => {
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'sa-1', text: 'abc', thinking: 'xyz' }),
    ];

    const after = reduceAppEvent(
      state,
      { type: 'subagentStepBoundary', sessionId: 's1', taskId: 'sa-1', retry: false },
      { sessionId: 's1', seq: 1 },
    );

    const row = after.tasksBySession['s1']![0]!;
    expect(row.stepTextLen).toBe(3);
    expect(row.stepThinkLen).toBe(3);
    expect(row.tailGuard).toBe(true);
  });

  it('should not write when the boundary names an unknown task', () => {
    const state = createInitialState();
    state.tasksBySession['s1'] = [subagentTask({ id: 't-a' })];
    const after = reduceAppEvent(
      state,
      { type: 'subagentStepBoundary', sessionId: 's1', taskId: 'ghost', retry: false },
      { sessionId: 's1', seq: 1 },
    );
    expect(after.tasksBySession).toBe(state.tasksBySession);
  });

  it('should keep whole-turn text and arm the tail guard when a retry boundary arrives with no marks (mid-subscription reconnect)', () => {
    // Given a running subagent whose turn.started was never seen — text
    // accumulated across the whole turn and NO step-start marks exist
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'sa-1', text: 'whole turn narrative' }),
    ];

    // When a retry boundary arrives
    const after = reduceAppEvent(
      state,
      { type: 'subagentStepBoundary', sessionId: 's1', taskId: 'sa-1', retry: true },
      { sessionId: 's1', seq: 1 },
    );

    // Then the text is unchanged (truncating via `?? 0` would wipe the whole
    // turn while the engine only replays the current step — the one-shot tail
    // guard is the designed backstop for replay overlap; 宁可多显不可丢失)
    const row = after.tasksBySession['s1']![0]!;
    expect(row.text).toBe('whole turn narrative');
    expect(row.tailGuard).toBe(true);
  });

  it('should not truncate a terminal member row on a late retry boundary (ADR-010 ② guard)', () => {
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({
        id: 'sa-1',
        status: 'completed',
        text: 'final narrative',
        stepTextLen: 0,
      }),
    ];

    const after = reduceAppEvent(
      state,
      { type: 'subagentStepBoundary', sessionId: 's1', taskId: 'sa-1', retry: true },
      { sessionId: 's1', seq: 1 },
    );

    expect(after.tasksBySession['s1']![0]!.text).toBe('final narrative');
  });

  it('should keep thinking and marks across a taskCreated replacement', () => {
    // Given a row with reducer-owned accumulated fields
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({
        id: 'sa-1',
        thinking: 'reasoning so far',
        text: 'narrative',
        steps: [{ id: 'tc-1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: ['body'] }],
        stepTextLen: 9,
        stepThinkLen: 17,
      }),
    ];

    // When the projector re-projects the row (skeleton — no thinking/steps/marks)
    const after = reduceAppEvent(
      state,
      {
        type: 'taskCreated',
        sessionId: 's1',
        task: subagentTask({ id: 'sa-1' }),
      },
      { sessionId: 's1', seq: 1 },
    );

    // Then the reducer-owned narrative fields survive the replacement
    // (steps is already preserved today — the pre-fix red comes from thinking)
    const row = after.tasksBySession['s1']![0]!;
    expect(row.thinking).toBe('reasoning so far');
    expect(row.stepTextLen).toBe(9);
    expect(row.stepThinkLen).toBe(17);
    expect(row.steps).toHaveLength(1);
  });

  it('should preserve suspendedReason across a skeleton taskCreated replacement', () => {
    // Given an awaiting member row carrying a structured suspension reason
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({
        id: 'sa-1',
        suspendedReason: { kind: 'approval', requestId: 'req-1' },
      }),
    ];

    // When a resync skeleton row (no suspendedReason) replaces it
    const after = reduceAppEvent(
      state,
      {
        type: 'taskCreated',
        sessionId: 's1',
        task: subagentTask({ id: 'sa-1' }),
      },
      { sessionId: 's1', seq: 1 },
    );

    // Then the suspension reason survives (conditional preserve, like description)
    expect(after.tasksBySession['s1']![0]!.suspendedReason).toEqual({
      kind: 'approval',
      requestId: 'req-1',
    });
  });
});

describe('subagent stream tail guard (one-shot post-boundary replay dedupe)', () => {
  // local helper — the file's other subagentTask is scoped to its own describe
  function subagentTask(overrides: Partial<AppTask> & { id: string }): AppTask {
    return {
      sessionId: 's1',
      kind: 'subagent',
      status: 'running',
      createdAt: '2026-01-01T00:00:00Z',
      description: 'member',
      ...overrides,
    };
  }

  it('should skip a post-boundary chunk that exactly repeats the field tail (>= 16 chars)', () => {
    // Given a boundary just committed (tail guard armed) and a field ending in a long chunk
    const tail = 'x'.repeat(20);
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'sa-1', text: `head${tail}`, tailGuard: true }),
    ];

    // When the same long chunk is re-delivered as the first chunk after the boundary
    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 'sa-1', outputChunk: tail, stream: 'stdout', kind: 'text' },
      { sessionId: 's1', seq: 1 },
    );

    // Then the chunk is dropped (no double-append) and the guard is consumed
    const row = after.tasksBySession['s1']![0]!;
    expect(row.text).toBe(`head${tail}`);
    expect(row.tailGuard).toBeUndefined();
  });

  it('should not apply the tail guard to steady-state chunks', () => {
    // Given a steady-state row — the guard was never armed (or already
    // consumed): content is never dropped outside the post-boundary window
    const chunk = 'y'.repeat(20);
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'sa-1', text: `head${chunk}` }),
    ];

    // When the same chunk text arrives again
    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 'sa-1', outputChunk: chunk, stream: 'stdout', kind: 'text' },
      { sessionId: 's1', seq: 1 },
    );

    // Then it appends (content is never dropped in steady state)
    expect(after.tasksBySession['s1']![0]!.text).toBe(`head${chunk}${chunk}`);
  });

  it('should ignore short repeated chunks even when the guard is armed', () => {
    // Guard armed, but the chunk is 4 chars — below the 16-char floor
    const state = createInitialState();
    state.tasksBySession['s1'] = [
      subagentTask({ id: 'sa-1', text: 'abcd', tailGuard: true }),
    ];

    const after = reduceAppEvent(
      state,
      { type: 'taskProgress', sessionId: 's1', taskId: 'sa-1', outputChunk: 'abcd', stream: 'stdout', kind: 'text' },
      { sessionId: 's1', seq: 1 },
    );

    // Then it appends (short tokens like "\n\n" must never be dropped)
    expect(after.tasksBySession['s1']![0]!.text).toBe('abcdabcd');
  });
});

describe('subagent legacy progress line cap (ADR-015 e)', () => {
  // local helper — the file's other subagentTask instances are scoped to
  // their own describes
  function subagentTask(overrides: Partial<AppTask> & { id: string }): AppTask {
    return {
      sessionId: 's1',
      kind: 'subagent',
      status: 'running',
      createdAt: '2026-01-01T00:00:00Z',
      description: 'member',
      ...overrides,
    };
  }

  it('should cap subagent progress lines at the background limit', () => {
    // Given a running subagent task (legacy "Calling …" lines accumulate one
    // per tool event, unbounded before this fix)
    const state = createInitialState();
    state.tasksBySession['s1'] = [subagentTask({ id: 'sa-1' })];

    // When 60 distinct progress lines arrive (alternating so the adjacent
    // dedupe cannot eat them)
    let after = state;
    for (let i = 0; i < 60; i++) {
      after = reduceAppEvent(
        after,
        {
          type: 'taskProgress',
          sessionId: 's1',
          taskId: 'sa-1',
          outputChunk: `line-${i % 2 === 0 ? 'a' : 'b'}-${i}`,
          stream: 'stdout',
        },
        { sessionId: 's1', seq: i + 1 },
      );
    }

    // Then the row keeps only the trailing 40 lines
    const lines = after.tasksBySession['s1']![0]!.outputLines!;
    expect(lines).toHaveLength(40);
    expect(lines[0]).toBe('line-a-20');
    expect(lines.at(-1)).toBe('line-b-59');
  });
});
