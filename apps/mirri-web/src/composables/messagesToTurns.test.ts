// apps/mirri-web/src/composables/messagesToTurns.test.ts
// Transcript projection: system-injected user turns (engine system
// reminders carried under metadata.origin) must not render as user chat
// messages, while real user input always does.

import { describe, expect, it } from 'vitest';

import type { AppMessage, AppTask } from '../api/types';
import { SWARM_ENDED_MARKER_METADATA_KEY } from '../api/types';
import { messagesToTurns, toAgentMember } from './messagesToTurns';

function message(overrides: Partial<AppMessage>): AppMessage {
  return {
    id: 'msg-1',
    sessionId: 'session-1',
    role: 'user',
    content: [{ type: 'text', text: 'hello' }],
    createdAt: '2026-09-12T00:00:00.000Z',
    ...overrides,
  };
}

describe('messagesToTurns (system-injected turns)', () => {
  it('should hide a user turn whose origin is an injection (the engine system-reminder path)', () => {
    // Given a reloaded history where the engine injected the <tools_added>
    // system reminder as a user-role message (origin rides the wire under
    // metadata.origin, TS toProtocolMessage parity)
    const injection = message({
      id: 'msg-0',
      content: [
        {
          type: 'text',
          text: '<system-reminder>\n<tools_added>\nBash\n</tools_added>\n\nUse the select_tools tool with exact names.\n</system-reminder>',
        },
      ],
      metadata: { origin: { kind: 'injection' } },
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([injection], []);

    // Then the reminder renders nowhere — it is model-visible context, not
    // user input
    expect(turns).toHaveLength(0);
  });

  it('should show a user turn whose origin is the user', () => {
    // Given a real user message (origin user rides the wire like any other
    // provenance-tagged row)
    const user = message({
      id: 'msg-1',
      metadata: { origin: { kind: 'user' } },
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([user], []);

    // Then the user turn renders exactly once
    expect(turns).toHaveLength(1);
    expect(turns[0]?.role).toBe('user');
    expect(turns[0]?.text).toBe('hello');
  });
});

describe('messagesToTurns (swarm-ended divider)', () => {
  it('Given a swarm-ended marker message Then it becomes a swarmEnded divider turn (not an assistant turn)', () => {
    // Given a client-side /swarm <task> end marker (assistant role + the
    // swarm-ended metadata key, seq-derived id)
    const marker = message({
      id: 'swarm_ended_session-1_42',
      role: 'assistant',
      content: [],
      metadata: { [SWARM_ENDED_MARKER_METADATA_KEY]: true },
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([marker], []);

    // Then exactly one divider turn renders — never a chat bubble
    expect(turns).toHaveLength(1);
    expect(turns[0]?.role).toBe('swarmEnded');
    expect(turns[0]?.id).toBe('swarm_ended_session-1_42');
  });

  it('Given messages without the swarm-ended key Then projection is unaffected', () => {
    const user = message({ id: 'msg-2', metadata: { origin: { kind: 'user' } } });

    const turns = messagesToTurns([user], []);

    expect(turns).toHaveLength(1);
    expect(turns[0]?.role).toBe('user');
  });
});

describe('messagesToTurns (background task notifications)', () => {
  function notificationMessage(overrides: Partial<AppMessage> = {}): AppMessage {
    return message({
      id: 'msg-n1',
      content: [
        {
          type: 'text',
          text:
            '<notification id="task:process-abc:completed" category="task" type="task.completed" ' +
            'source_kind="background_task" source_id="process-abc">\n' +
            'Title: Background process completed\n' +
            'Severity: info\n' +
            'npm run watch completed.\n' +
            '</notification>',
        },
      ],
      metadata: {
        origin: {
          kind: 'background_task',
          taskId: 'process-abc',
          status: 'completed',
          notificationId: 'task:process-abc:completed',
        },
      },
      ...overrides,
    });
  }

  it('should render a notification turn from a background_task-origin user message', () => {
    // Given a terminal background-task notification the engine delivered as a
    // user-role message (origin background_task + the <notification> envelope)
    const msg = notificationMessage();

    // When the transcript turns are projected
    const turns = messagesToTurns([msg], []);

    // Then exactly one notification turn renders, carrying the task identity
    // and the envelope's body (never the raw XML shell)
    expect(turns).toHaveLength(1);
    expect(turns[0]!.role).toBe('notification');
    expect(turns[0]!.notification?.taskId).toBe('process-abc');
    expect(turns[0]!.notification?.status).toBe('completed');
    expect(turns[0]!.notification?.notificationId).toBe('task:process-abc:completed');
    expect(turns[0]!.text).toContain('npm run watch completed.');
    expect(turns[0]!.text).not.toContain('<notification');
    expect(turns[0]!.text).not.toContain('Title:');
    expect(turns[0]!.text).not.toContain('Severity:');
  });

  it('should never fall through to the plain-user display path', () => {
    // Given the same notification message, whose text could coincide with a
    // prior user prompt
    const msg = notificationMessage({
      content: [
        {
          type: 'text',
          text:
            '<notification id="task:process-abc:completed" category="task" type="task.completed" ' +
            'source_kind="background_task" source_id="process-abc">\n' +
            'Title: Background process completed\n' +
            'Severity: info\nhello\n</notification>',
        },
      ],
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([msg], []);

    // Then the card is the ONLY presentation — no duplicate user bubble
    expect(turns).toHaveLength(1);
    expect(turns[0]!.role).toBe('notification');
    expect(turns.filter((t) => t.role === 'user')).toHaveLength(0);
  });

  it('should keep a failed notification body and drop the model-facing output children', () => {
    // Given a failed task notification carrying the multi-line recovery body
    // plus a model-facing <output-file> pointer block
    const msg = notificationMessage({
      content: [
        {
          type: 'text',
          text:
            '<notification id="task:process-abc:failed" category="task" type="task.failed" ' +
            'source_kind="background_task" source_id="process-abc">\n' +
            'Title: Background process failed\n' +
            'Severity: warning\n' +
            'Background process failed. Reason: exit 2\n' +
            '<output-file path="/tmp/out.log" bytes="42000">\n' +
            'Read the output file to retrieve the result: /tmp/out.log\n' +
            '</output-file>\n' +
            '</notification>',
        },
      ],
      metadata: {
        origin: { kind: 'background_task', taskId: 'process-abc', status: 'failed' },
      },
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([msg], []);

    // Then the body survives while the output pointer block does not
    expect(turns[0]!.role).toBe('notification');
    expect(turns[0]!.notification?.status).toBe('failed');
    expect(turns[0]!.text).toContain('Background process failed. Reason: exit 2');
    expect(turns[0]!.text).not.toContain('<output-file');
  });

  it('should route by the origin only (a notification envelope without origin metadata is plain user input, TUI parity)', () => {
    // Given a background-task envelope that lost its origin metadata (legacy
    // or hand-crafted), the origin route is the single card trigger — the
    // plain-user display path stays in charge, exactly like the TUI's
    // isReplayUserTurnRecord (origin absent ⇒ user input)
    const msg = notificationMessage({ metadata: undefined });

    // When the transcript turns are projected
    const turns = messagesToTurns([msg], []);

    // Then it renders through the plain-user path, not as a notification card
    expect(turns).toHaveLength(1);
    expect(turns[0]!.role).toBe('user');
  });

  function assistantRow(id: string, promptId: string, text: string): AppMessage {
    return message({ id, role: 'assistant', promptId, content: [{ type: 'text', text }] });
  }

  it('should fold a mid-turn steered notification into the running turn instead of splitting it', () => {
    // Given one logical turn whose assistant rows share a promptId, with the
    // engine's background-task notification steered between them (the wire's
    // turn.steer → before_step flush: the notification lands as a user
    // message INSIDE the still-running turn, not as a turn boundary)
    const messages = [
      message({ id: 'msg-u1', content: [{ type: 'text', text: 'run the tests' }], metadata: { origin: { kind: 'user' } } }),
      assistantRow('msg-a1', 'turn-1', 'step one'),
      notificationMessage(),
      assistantRow('msg-a2', 'turn-1', 'step two'),
    ];

    // When the transcript turns are projected
    const turns = messagesToTurns(messages, []);

    // Then the turn stays ONE assistant turn — the notice rides inside it as
    // an ordered notification block (TUI parity: an inline status entry), and
    // no standalone notification turn splits the run
    expect(turns.map((t) => t.role)).toEqual(['user', 'assistant']);
    const blocks = turns[1]!.blocks ?? [];
    expect(blocks.map((b) => b.kind)).toEqual(['text', 'notification', 'text']);
    const notice = blocks[1];
    if (notice?.kind !== 'notification') throw new Error('expected a notification block');
    expect(notice.text).toContain('npm run watch completed.');
    expect(notice.notification.taskId).toBe('process-abc');
    expect(turns.filter((t) => t.role === 'notification')).toHaveLength(0);
  });

  it('should keep a steered notification folded when the turn is still running (no assistant row after it yet)', () => {
    // Given a mid-turn steer that just landed — the running turn's next
    // assistant row has not been persisted yet (the live/reloaded in-flight
    // shape)
    const messages = [
      message({ id: 'msg-u1', content: [{ type: 'text', text: 'run the tests' }], metadata: { origin: { kind: 'user' } } }),
      assistantRow('msg-a1', 'turn-1', 'step one'),
      notificationMessage(),
    ];

    // When the transcript turns are projected
    const turns = messagesToTurns(messages, []);

    // Then the notice folds into the pending group rather than cutting the
    // turn the engine is still producing
    expect(turns.map((t) => t.role)).toEqual(['user', 'assistant']);
    expect((turns[1]!.blocks ?? []).map((b) => b.kind)).toEqual(['text', 'notification']);
  });

  it('should render an idle notification as its own turn when the next assistant row opens a different turn', () => {
    // Given a terminal notification the engine delivered while the session
    // was idle (the notification driver's own turn): the assistant rows after
    // it belong to a DIFFERENT turn than the rows before it
    const messages = [
      message({ id: 'msg-u1', content: [{ type: 'text', text: 'first question' }], metadata: { origin: { kind: 'user' } } }),
      assistantRow('msg-a1', 'turn-1', 'first answer'),
      notificationMessage(),
      assistantRow('msg-a2', 'turn-2', 'reacting to the notice'),
    ];

    // When the transcript turns are projected
    const turns = messagesToTurns(messages, []);

    // Then the notice keeps its standalone card turn and the two assistant
    // turns stay separate (a real turn boundary, not a steer)
    expect(turns.map((t) => t.role)).toEqual(['user', 'assistant', 'notification', 'assistant']);
  });
});

function baseSubagentTask(): AppTask {
  return {
    id: 'task-m1',
    sessionId: 'session-1',
    kind: 'subagent',
    description: 'member one',
    status: 'running',
    createdAt: '2026-10-23T00:00:00.000Z',
  } as AppTask;
}

describe('toAgentMember (ADR-010 ③ 统一派生)', () => {
  it('should derive the member phase through the unified derivation when status and subagentPhase diverge (ADR-010 ③)', () => {
    // Given a task whose status is completed but whose subagentPhase still
    // reads working (symptom B's divergent shape — the writer path has not
    // landed the phase update yet)
    const task = { ...baseSubagentTask(), status: 'completed', subagentPhase: 'working' } as AppTask;
    // Then the member phase derives from the terminal status, not the stale
    // intermediate phase
    expect(toAgentMember(task).phase).toBe('completed');

    // Given the dual divergence — still running but the phase already reads
    // completed (phase wins over running, phaseForTask semantics)
    const task2 = { ...baseSubagentTask(), status: 'running', subagentPhase: 'completed' } as AppTask;
    expect(toAgentMember(task2).phase).toBe('completed');
  });
});

describe('messagesToTurns (model-switch divider)', () => {
  function assistantTextMessage(id: string, text: string): AppMessage {
    return message({ id, role: 'assistant', content: [{ type: 'text', text }] });
  }

  function userTextMessage(id: string, text: string): AppMessage {
    return message({ id, role: 'user', content: [{ type: 'text', text }] });
  }

  function markerMessage(id: string, origin: { from: string; to: string }): AppMessage {
    return message({
      id,
      role: 'user',
      content: [{ type: 'text', text: '' }],
      metadata: { origin: { kind: 'model_switched', ...origin } },
    });
  }

  it('should render a modelSwitch divider turn when a message carries the model_switched origin', () => {
    // Given a transcript with an assistant message, a model-switch marker, and a user message
    const messages = [
      assistantTextMessage('m1', 'old model answer'),
      markerMessage('m2', { from: 'model-a', to: 'model-b' }),
      userTextMessage('m3', 'next question'),
    ];
    // When folded into turns
    const turns = messagesToTurns(messages, []);
    // Then a divider turn sits between them with the switch identity
    const divider = turns.find((t) => t.role === 'modelSwitch');
    expect(divider?.modelSwitch).toEqual({ from: 'model-a', to: 'model-b' });
    expect(divider?.text).toBe('');
  });

  it('should keep the marker out of adjacent user turns when it follows a user message', () => {
    // Given a user message immediately followed by a model-switch marker
    const messages = [userTextMessage('m1', 'hi'), markerMessage('m2', { from: 'a', to: 'b' })];
    // When folded into turns
    const turns = messagesToTurns(messages, []);
    // Then the user turn is not merged with the marker and the marker is its own turn
    expect(turns.map((t) => t.role)).toEqual(['user', 'modelSwitch']);
  });
});

describe('messagesToTurns (mid-turn reload: transcript + in-flight seed)', () => {
  it('should render one running tool card when the seed and the transcript row share the turn prompt id', () => {
    // Given a page reload while a subagent task is running: the snapshot's
    // transcript already holds the running step's assistant row (the engine
    // commits it before executing the tool), stamped with the in-flight
    // turn's key, and the projector's seedInFlight replay appends a second
    // copy of the same running step under the SAME prompt id
    // (`current_prompt_id` from the snapshot)
    const transcriptRow = message({
      id: 'msg-7',
      role: 'assistant',
      promptId: 'turn-2',
      content: [
        { type: 'text', text: '进入 F6 收口批：' },
        {
          type: 'toolUse',
          toolCallId: 'call-2',
          toolName: 'Task',
          input: { prompt: 'Implement F6 closing batch' },
        },
      ],
    });
    const seededCopy = message({
      id: 'msg-seed',
      role: 'assistant',
      promptId: 'turn-2',
      content: [
        { type: 'text', text: '进入 F6 收口批：' },
        {
          type: 'toolUse',
          toolCallId: 'call-2',
          toolName: 'Task',
          input: { prompt: 'Implement F6 closing batch' },
          outputLines: ['Implement F6 closing batch'],
        },
      ],
    });

    // When the turns are projected (the session is still active — the
    // running tool keeps its spinner)
    const turns = messagesToTurns([transcriptRow, seededCopy], []);

    // Then the seed folds into the transcript's turn: exactly one assistant
    // turn with exactly ONE Task card (the duplicate split into a settled
    // done card plus a running card was the reload regression)
    expect(turns).toHaveLength(1);
    const assistant = turns[0]!;
    expect(assistant.role).toBe('assistant');
    expect(assistant.tools).toHaveLength(1);
    expect(assistant.tools?.[0]?.name).toBe('Task');
    expect(assistant.tools?.[0]?.status).toBe('running');
    // And the seed's volatile progress lines survive the fold
    expect(assistant.tools?.[0]?.output).toEqual(['Implement F6 closing batch']);
  });
});

describe('messagesToTurns (turn duration)', () => {
  it('should carry the duration from the last message when an assistant group spans multiple messages', () => {
    // Given a restored transcript whose completed turn arrived as several
    // assistant rows — the server stamps the turn's measured duration on
    // the LAST row only (the same row the live path stamps)
    const first = message({
      id: 'msg-1',
      role: 'assistant',
      promptId: 'turn-1',
      content: [{ type: 'text', text: 'intermediate' }],
    });
    const last = message({
      id: 'msg-2',
      role: 'assistant',
      promptId: 'turn-1',
      content: [{ type: 'text', text: 'final report' }],
      durationMs: 42420,
    });

    // When the transcript turns are projected
    const turns = messagesToTurns([first, last], []);

    // Then the single group turn carries the stamped duration — the
    // collapsed header renders "Worked for X", not a bare "Worked"
    expect(turns).toHaveLength(1);
    expect(turns[0]?.role).toBe('assistant');
    expect(turns[0]?.durationMs).toBe(42420);
  });

  it('should keep a single-message assistant turn duration unchanged', () => {
    const solo = message({
      id: 'msg-1',
      role: 'assistant',
      promptId: 'turn-2',
      content: [{ type: 'text', text: 'reply' }],
      durationMs: 27996,
    });

    const turns = messagesToTurns([solo], []);

    expect(turns).toHaveLength(1);
    expect(turns[0]?.durationMs).toBe(27996);
  });
});

describe('messagesToTurns (user-inline skill wrapper ordering)', () => {
  /** A user-inline skill activation wrapper message (the head of an ADR-004
   *  decision-3 turn message sequence). */
  function inlineWrapper(id: string, skillName: string): AppMessage {
    return message({
      id,
      role: 'user',
      content: [{ type: 'text', text: `<skill-activation>${skillName}</skill-activation>` }],
      metadata: {
        origin: { kind: 'skill_activation', trigger: 'user-inline', skillName, skillArgs: '' },
      },
    });
  }

  it('should keep the previous assistant turn above the user-inline skill wrappers when they follow it', () => {
    // Given a finished assistant turn, then a mixed message whose activation
    // wrappers precede the body (the ADR-004 turn message-sequence model)
    const turns = messagesToTurns(
      [
        message({ id: 'u1', role: 'user', content: [{ type: 'text', text: 'first question' }] }),
        message({
          id: 'a1',
          role: 'assistant',
          content: [{ type: 'text', text: 'first answer' }],
          promptId: 'turn-1',
        }),
        inlineWrapper('w1', 'writing-plans'),
        inlineWrapper('w2', 'parallel-worktree-execution'),
        message({
          id: 'u2',
          role: 'user',
          content: [{ type: 'text', text: '做计划之后去落地' }],
        }),
        message({
          id: 'a2',
          role: 'assistant',
          content: [{ type: 'text', text: 'second answer' }],
          promptId: 'turn-2',
        }),
      ],
      [],
    );

    // Then the wrapper turns render AFTER the previous assistant turn and
    // BEFORE the body message — the activation cards no longer float above
    // the previous turn
    expect(turns.map((t) => t.id)).toEqual(['u1', 'a1', 'w1', 'w2', 'u2', 'a2']);

    // And each wrapper keeps the body turn's gutter number (wrappers ride
    // inside the body's turn and consume no number of their own)
    const wrapper1 = turns[2]!;
    const wrapper2 = turns[3]!;
    const body = turns[4]!;
    expect(wrapper1.role).toBe('user');
    expect(wrapper1.skillActivation?.name).toBe('writing-plans');
    expect(wrapper2.skillActivation?.name).toBe('parallel-worktree-execution');
    expect(wrapper1.no).toBe(body.no);
    expect(wrapper2.no).toBe(body.no);
  });

  it('should preserve wire order when wrappers arrive before their body with no assistant row in between', () => {
    // Given a reloaded transcript whose turn sequence is
    // wrapper → body → assistant (one turn, no assistant row between
    // the wrapper and the body)
    const turns = messagesToTurns(
      [
        inlineWrapper('w1', 'writing-plans'),
        message({ id: 'u1', role: 'user', content: [{ type: 'text', text: '做计划' }] }),
        message({
          id: 'a1',
          role: 'assistant',
          content: [{ type: 'text', text: 'the answer' }],
          promptId: 'turn-1',
        }),
      ],
      [],
    );

    // Then all rows project in wire order with the wrapper sharing the
    // body turn's gutter number
    expect(turns.map((t) => t.id)).toEqual(['w1', 'u1', 'a1']);
    expect(turns[0]!.no).toBe(turns[1]!.no);
  });

  it('should settle gracefully when a wrapper is followed directly by an assistant row without a body', () => {
    // Given a truncated/aborted transcript where the wrapper's body message
    // was never persisted, leaving the wrapper directly before the turn's
    // assistant rows
    const turns = messagesToTurns(
      [
        message({
          id: 'a0',
          role: 'assistant',
          content: [{ type: 'text', text: 'earlier turn' }],
          promptId: 'turn-0',
        }),
        inlineWrapper('w1', 'writing-plans'),
        message({
          id: 'a1',
          role: 'assistant',
          content: [{ type: 'text', text: 'the turn reply' }],
          promptId: 'turn-1',
        }),
      ],
      [],
    );

    // Then the earlier turn stays above the wrapper and the wrapper renders
    // (sharing a gutter number with the following turn — an accepted
    // degraded shape for a truncated transcript, never reordering history)
    expect(turns.map((t) => t.id)).toEqual(['a0', 'w1', 'a1']);
    expect(turns[1]!.role).toBe('user');
    expect(turns[1]!.skillActivation?.name).toBe('writing-plans');
  });
});
