import { describe, expect, it } from 'vitest';
import { createInitialState, reduceAppEvent } from '../src/api/daemon/eventReducer';
import { shouldClearWorkingFlags } from '../src/composables/useMirriWebClient';
import type { AppMessage, AppSession, AppTask } from '../src/api/types';

function makeSession(id: string, updatedAt: string): AppSession {
  return {
    id,
    title: id,
    createdAt: updatedAt,
    updatedAt,
    busy: false,
    archived: false,
    cwd: '/workspace',
    model: 'mirri-code',
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
  };
}

function makeMessage(sessionId: string, createdAt: string): AppMessage {
  return {
    id: `msg_${createdAt}`,
    sessionId,
    role: 'user',
    content: [{ type: 'text', text: 'hi' }],
    createdAt,
  };
}

function makeSubagentTask(id: string, sessionId: string): AppTask {
  return {
    id,
    sessionId,
    kind: 'subagent',
    description: 'subagent task',
    status: 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
  };
}

describe('reduceAppEvent messageCreated', () => {
  it('bumps the session updatedAt so it floats to the top of the sidebar', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s-old', '2026-01-01T00:00:00.000Z')],
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: makeMessage('s-old', '2026-06-01T12:00:00.000Z') },
      { sessionId: 's-old', seq: 1 },
    );
    expect(next.sessions[0]?.updatedAt).toBe('2026-06-01T12:00:00.000Z');
  });

  it('does not move a session backwards when an older message arrives', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s-new', '2026-06-01T12:00:00.000Z')],
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: makeMessage('s-new', '2026-01-01T00:00:00.000Z') },
      { sessionId: 's-new', seq: 1 },
    );
    expect(next.sessions[0]?.updatedAt).toBe('2026-06-01T12:00:00.000Z');
  });

  it('leaves other sessions untouched', () => {
    const state = {
      ...createInitialState(),
      sessions: [
        makeSession('s-a', '2026-01-01T00:00:00.000Z'),
        makeSession('s-b', '2026-01-01T00:00:00.000Z'),
      ],
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: makeMessage('s-a', '2026-06-01T12:00:00.000Z') },
      { sessionId: 's-a', seq: 1 },
    );
    expect(next.sessions.find((s) => s.id === 's-a')?.updatedAt).toBe('2026-06-01T12:00:00.000Z');
    expect(next.sessions.find((s) => s.id === 's-b')?.updatedAt).toBe('2026-01-01T00:00:00.000Z');
  });

  it('reconciles a resolved video echo into the optimistic user message', () => {
    // The optimistic copy still carries the original `video` part (no promptId
    // yet — the echo raced the submit response). The daemon echo carries the
    // server-resolved `<video path=…></video>` text tag. They must collapse into
    // one bubble, not render as a duplicate.
    const optimistic: AppMessage = {
      id: 'msg_opt_1',
      sessionId: 's-vid',
      role: 'user',
      content: [
        { type: 'text', text: 'look at this' },
        { type: 'video', source: { kind: 'file', fileId: 'f_abc' } },
      ],
      createdAt: '2026-06-01T12:00:00.000Z',
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    };
    const echo: AppMessage = {
      id: 'msg_real',
      sessionId: 's-vid',
      role: 'user',
      content: [
        { type: 'text', text: 'look at this' },
        { type: 'text', text: '<video path="/Users/me/.mirri-code/cache/f_abc.mp4"></video>' },
      ],
      createdAt: '2026-06-01T12:00:00.000Z',
      promptId: 'p1',
    };
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s-vid', '2026-01-01T00:00:00.000Z')],
      messagesBySession: { 's-vid': [optimistic] },
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: echo },
      { sessionId: 's-vid', seq: 1 },
    );
    const msgs = next.messagesBySession['s-vid'] ?? [];
    expect(msgs).toHaveLength(1);
    // Keeps the optimistic id so the bubble doesn't remount…
    expect(msgs[0]?.id).toBe('msg_opt_1');
    // …but takes the daemon's resolved content (the video text tag).
    expect(msgs[0]?.content).toEqual(echo.content);
    expect(msgs[0]?.promptId).toBe('p1');
  });

  it('reconciles a compressed-image echo into the optimistic user message', () => {
    // 粘贴图片 + 输入文本 + 发送。optimistic 消息用 {kind:'file'} 引用图片，
    // 但 WS prompt.submitted echo 在 HTTP 响应之前到达，所以 promptId 还没被
    // stamp 到 optimistic 消息上。服务端将 file-source 图片压缩为 base64，
    // 并在图片前插入了一个 <system>Image compressed...</system> caption 文本块。
    // 两者必须合并为一条消息，而非渲染为重复。
    const optimistic: AppMessage = {
      id: 'msg_opt_1',
      sessionId: 's-img',
      role: 'user',
      content: [
        { type: 'text', text: 'what is this' },
        { type: 'image', source: { kind: 'file', fileId: 'f_abc' } },
      ],
      createdAt: '2026-06-01T12:00:00.000Z',
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    };
    const echo: AppMessage = {
      id: 'msg_real',
      sessionId: 's-img',
      role: 'user',
      content: [
        { type: 'text', text: 'what is this' },
        {
          type: 'text',
          text: '<system>Image compressed to fit model limits: original 1920x1080 image/png (2.3 MB) -> sent 1024x576 image/png (180 KB). Fine detail may be lost. The uncompressed original is saved at "/cache/f_abc.png"; if you need fine detail (e.g. small text), call ReadMediaFile on that path with the region parameter (original-pixel coordinates) to view a crop at full fidelity.</system>',
        },
        { type: 'image', source: { kind: 'base64', media_type: 'image/png', data: 'iVBOR...' } },
      ],
      createdAt: '2026-06-01T12:00:00.000Z',
      promptId: 'p1',
    };
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s-img', '2026-01-01T00:00:00.000Z')],
      messagesBySession: { 's-img': [optimistic] },
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: echo },
      { sessionId: 's-img', seq: 1 },
    );
    const msgs = next.messagesBySession['s-img'] ?? [];
    expect(msgs).toHaveLength(1);
    expect(msgs[0]?.id).toBe('msg_opt_1');
    expect(msgs[0]?.promptId).toBe('p1');
  });
});

describe('reduceAppEvent taskProgress', () => {
  it('accumulates the full progress output without truncating to a fixed window', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: { 's1': [makeSubagentTask('t1', 's1')] },
    };
    let next = state;
    for (let i = 0; i < 60; i++) {
      // The real projector emits a taskCreated (without reducer-owned
      // outputLines) right before every taskProgress; progress must survive
      // that replacement.
      next = reduceAppEvent(
        next,
        { type: 'taskCreated', sessionId: 's1', task: makeSubagentTask('t1', 's1') },
        { sessionId: 's1', seq: i * 2 + 1 },
      );
      next = reduceAppEvent(
        next,
        { type: 'taskProgress', sessionId: 's1', taskId: 't1', outputChunk: `line ${i}`, stream: 'stdout' },
        { sessionId: 's1', seq: i * 2 + 2 },
      );
    }
    const lines = next.tasksBySession['s1']?.[0]?.outputLines;
    expect(lines).toHaveLength(60);
    expect(lines?.[0]).toBe('line 0');
    expect(lines?.at(-1)).toBe('line 59');
  });

  it('deduplicates a repeated trailing chunk', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: { 's1': [makeSubagentTask('t1', 's1')] },
    };
    const event = { type: 'taskProgress', sessionId: 's1', taskId: 't1', outputChunk: 'same', stream: 'stdout' } as const;
    const once = reduceAppEvent(state, event, { sessionId: 's1', seq: 1 });
    const twice = reduceAppEvent(once, event, { sessionId: 's1', seq: 2 });
    expect(twice.tasksBySession['s1']?.[0]?.outputLines).toEqual(['same']);
  });

  it('caps accumulated output for non-subagent (background) tasks', () => {
    const bash: AppTask = { ...makeSubagentTask('b1', 's1'), kind: 'bash' };
    const state = { ...createInitialState(), tasksBySession: { 's1': [bash] } };
    let next = state;
    for (let i = 0; i < 60; i++) {
      next = reduceAppEvent(
        next,
        { type: 'taskProgress', sessionId: 's1', taskId: 'b1', outputChunk: `line ${i}`, stream: 'stdout' },
        { sessionId: 's1', seq: i + 1 },
      );
    }
    const lines = next.tasksBySession['s1']?.[0]?.outputLines;
    expect(lines).toHaveLength(40);
    expect(lines?.[0]).toBe('line 20');
    expect(lines?.at(-1)).toBe('line 59');
  });

  it('concatenates subagent text-kind chunks into a growing text block', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: { 's1': [makeSubagentTask('t1', 's1')] },
    };
    let next = state;
    for (const chunk of ['Hello', ', ', 'world', '!']) {
      next = reduceAppEvent(
        next,
        {
          type: 'taskProgress',
          sessionId: 's1',
          taskId: 't1',
          outputChunk: chunk,
          stream: 'stdout',
          kind: 'text',
        },
        { sessionId: 's1', seq: 1 },
      );
    }
    const task = next.tasksBySession['s1']?.[0];
    expect(task?.text).toBe('Hello, world!');
    // Text chunks must not pollute the line-based progress output.
    expect(task?.outputLines ?? []).toHaveLength(0);
  });

  it('preserves accumulated text across a taskCreated replacement', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: { 's1': [{ ...makeSubagentTask('t1', 's1'), text: 'partial' }] },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCreated', sessionId: 's1', task: makeSubagentTask('t1', 's1') },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.text).toBe('partial');
  });

  it('preserves subagent identity metadata across a taskCreated replacement with omitted fields', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [
          {
            ...makeSubagentTask('t1', 's1'),
            parentToolCallId: 'call-1',
            swarmIndex: 2,
            subagentType: 'explore',
            runInBackground: true,
            outputLines: ['old line'],
            text: 'partial',
          },
        ],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCreated', sessionId: 's1', task: makeSubagentTask('t1', 's1') },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]).toMatchObject({
      parentToolCallId: 'call-1',
      swarmIndex: 2,
      subagentType: 'explore',
      runInBackground: true,
      outputLines: ['old line'],
      text: 'partial',
    });
  });

  it('keeps the roster-seeded description when a re-projected task carries the placeholder', () => {
    // After a page refresh the snapshot roster seeds the real description; a
    // later subagent.* lifecycle event re-projects the task with the
    // projector's skeleton default ('Sub Agent') — it must not clobber it.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{ ...makeSubagentTask('t1', 's1'), description: 'explore the auth flow' }],
      },
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'taskCreated',
        sessionId: 's1',
        task: { ...makeSubagentTask('t1', 's1'), description: 'Sub Agent' },
      },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.description).toBe('explore the auth flow');
  });

  it('takes the incoming description when it is a real one', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{ ...makeSubagentTask('t1', 's1'), description: 'Sub Agent' }],
      },
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'taskCreated',
        sessionId: 's1',
        task: { ...makeSubagentTask('t1', 's1'), description: 'write the tests' },
      },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.description).toBe('write the tests');
  });
});

describe('reduceAppEvent sessions reference stability', () => {
  // The sidebar computeds (sessionsForView / workspaceGroups / mergedWorkspaces)
  // depend on `rawState.sessions`. Events that do not change sessions must keep
  // the SAME array reference so those computeds are not dirtied; events that do
  // change sessions must produce a NEW array.

  it('reuses the sessions reference for an event that does not touch sessions', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s1', '2026-01-01T00:00:00.000Z')],
      messagesBySession: { s1: [makeMessage('s1', '2026-01-01T00:00:00.000Z')] },
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'messageUpdated',
        sessionId: 's1',
        messageId: 'msg_2026-01-01T00:00:00.000Z',
        content: [{ type: 'text', text: 'updated' }],
        status: 'completed',
      },
      { sessionId: 's1', seq: 2 },
    );
    expect(next.sessions).toBe(state.sessions);
  });

  it('produces a new sessions array for an event that changes sessions', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s1', '2026-01-01T00:00:00.000Z')],
    };
    const next = reduceAppEvent(
      state,
      { type: 'sessionCreated', session: makeSession('s2', '2026-02-01T00:00:00.000Z') },
      { sessionId: 's2', seq: 3 },
    );
    expect(next.sessions).not.toBe(state.sessions);
    expect(next.sessions.map((s) => s.id)).toEqual(['s2', 's1']);
  });
});

describe('reduceAppEvent messageCreated cron origin', () => {
  it('appends a cron-origin user message instead of reconciling it into an optimistic echo', () => {
    const sid = 's-cron';
    const optimistic: AppMessage = {
      id: 'opt_1',
      sessionId: sid,
      role: 'user',
      content: [{ type: 'text', text: 'check the BTC price' }],
      createdAt: '2026-01-01T00:00:00.000Z',
      promptId: 'pr_user',
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    };
    const state = {
      ...createInitialState(),
      sessions: [makeSession(sid, '2026-01-01T00:00:00.000Z')],
      messagesBySession: { [sid]: [optimistic] },
    };
    const cronMessage: AppMessage = {
      id: 'cron_1',
      sessionId: sid,
      role: 'user',
      content: [{ type: 'text', text: 'check the BTC price' }],
      createdAt: '2026-01-01T00:01:00.000Z',
      promptId: 'cron_pr_x',
      metadata: {
        origin: {
          kind: 'cron_job',
          jobId: 'j',
          cron: '* * * * *',
          recurring: true,
          coalescedCount: 1,
          stale: false,
        },
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'messageCreated', message: cronMessage },
      { sessionId: sid, seq: 2 },
    );
    const msgs = next.messagesBySession[sid];
    expect(msgs).toHaveLength(2);
    expect(msgs.map((m) => m.id)).toEqual(['opt_1', 'cron_1']);
  });
});

describe('reduceAppEvent sessionWorkChanged turn-active preservation', () => {
  // The server computes `busy` and `main_turn_active` as INDEPENDENT fields:
  //   busy = mainTurnActive || hasPendingApproval || hasPendingQuestion
  // So `busy: true, mainTurnActive: false` is a legitimate state — the main
  // turn ended but an approval/question is pending. The reducer must NOT clear
  // `turnActiveBySession` in that window, or the Stop button disappears while
  // the session is still working.

  it('should preserve turnActiveBySession when mainTurnActive is false but session is busy', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s1', '2026-01-01T00:00:00.000Z')],
      turnActiveBySession: { s1: true },
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'sessionWorkChanged',
        sessionId: 's1',
        busy: true,
        mainTurnActive: false,
        pendingInteraction: 'approval',
      },
      { sessionId: 's1', seq: 1 },
    );
    // turnActiveBySession must survive — the session is still busy
    expect(next.turnActiveBySession['s1']).toBe(true);
  });

  it('should clear turnActiveBySession only when session becomes not busy', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s1', '2026-01-01T00:00:00.000Z')],
      turnActiveBySession: { s1: true },
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'sessionWorkChanged',
        sessionId: 's1',
        busy: false,
        mainTurnActive: false,
      },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.turnActiveBySession['s1']).toBeUndefined();
  });

  it('should set turnActiveBySession when mainTurnActive becomes true', () => {
    const state = {
      ...createInitialState(),
      sessions: [makeSession('s1', '2026-01-01T00:00:00.000Z')],
    };
    const next = reduceAppEvent(
      state,
      {
        type: 'sessionWorkChanged',
        sessionId: 's1',
        busy: true,
        mainTurnActive: true,
      },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.turnActiveBySession['s1']).toBe(true);
  });
});

describe('shouldClearWorkingFlags', () => {
  // processEvent calls clearWorkingFlags only when shouldClearWorkingFlags
  // returns true. The OLD (buggy) code used `mainTurnActive === false` as a
  // trigger — which fired even while `busy === true`, clearing the local
  // inFlightBySession and turnActiveBySession flags and hiding the Stop button
  // during an approval/question pause. The fix gates purely on `!busy`.

  const baseEvent = {
    type: 'sessionWorkChanged' as const,
    sessionId: 's1',
  };

  it('should return false when session is busy even if mainTurnActive is false', () => {
    // This is the exact bug scenario: main turn ended, approval pending.
    // Old code cleared flags here → Stop button vanished.
    const result = shouldClearWorkingFlags(
      { ...baseEvent, busy: true, mainTurnActive: false, pendingInteraction: 'approval' },
      2, // seq
      1, // prevSeq
    );
    expect(result).toBe(false);
  });

  it('should return true when session is not busy', () => {
    const result = shouldClearWorkingFlags(
      { ...baseEvent, busy: false, mainTurnActive: false },
      2,
      1,
    );
    expect(result).toBe(true);
  });

  it('should return false when seq does not advance past prevSeq', () => {
    const result = shouldClearWorkingFlags(
      { ...baseEvent, busy: false, mainTurnActive: false },
      1, // seq === prevSeq — late duplicate
      1,
    );
    expect(result).toBe(false);
  });

  it('should return false when busy is true and mainTurnActive is true', () => {
    const result = shouldClearWorkingFlags(
      { ...baseEvent, busy: true, mainTurnActive: true },
      2,
      1,
    );
    expect(result).toBe(false);
  });

  it('should return false when busy is true and mainTurnActive is undefined', () => {
    // Server may omit main_turn_active; the old code's second branch
    // (mainTurnActive === undefined && !busy) was fine here, but the first
    // branch (mainTurnActive === false && wasMainTurnActive) was the bug.
    // New code: only !busy triggers.
    const result = shouldClearWorkingFlags(
      { ...baseEvent, busy: true, mainTurnActive: undefined },
      2,
      1,
    );
    expect(result).toBe(false);
  });
});

describe('reduceAppEvent taskCompleted', () => {
  it('should update task status when task id matches event task id', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: { 's1': [{ ...makeSubagentTask('agent-1', 's1') }] },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'agent-1', status: 'completed', outputPreview: 'done' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('completed');
    expect(next.tasksBySession['s1']?.[0]?.outputPreview).toBe('done');
  });

  it('should update task status via backgroundTaskId when task id does not match', () => {
    // Core fix: background.task.terminated carries the background task id,
    // while the WS-owned subagent row is keyed by agent id — link them
    // through the backgroundTaskId field.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('agent-1', 's1'),
          backgroundTaskId: 'bg-task-1',
          runInBackground: true,
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'bg-task-1', status: 'completed' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('completed');
  });

  it('should not regress a completed task back to running via backgroundTaskId fallback', () => {
    // Terminal-stickiness: a completed task must not be flipped back to running
    // by a lagging background.task.terminated event.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('agent-1', 's1'),
          backgroundTaskId: 'bg-task-1',
          status: 'completed',
          completedAt: '2026-01-01T00:00:00.000Z',
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'bg-task-1', status: 'running' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('completed');
  });

  it('should preserve existing outputPreview when backgroundTaskId fallback provides none', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('agent-1', 's1'),
          backgroundTaskId: 'bg-task-1',
          outputPreview: 'partial result from stream',
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'bg-task-1', status: 'completed' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.outputPreview).toBe('partial result from stream');
  });

  it('should take outputPreview from background task terminated event when provided', () => {
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('agent-1', 's1'),
          backgroundTaskId: 'bg-task-1',
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'bg-task-1', status: 'failed', outputPreview: 'Error: timeout' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('failed');
    expect(next.tasksBySession['s1']?.[0]?.outputPreview).toBe('Error: timeout');
  });

  it('should not match a backgroundTaskId fallback when task is not running', () => {
    // A failed task should not be overridden by a late completed event
    // arriving via the backgroundTaskId fallback path.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('agent-1', 's1'),
          backgroundTaskId: 'bg-task-1',
          status: 'failed',
          completedAt: '2026-01-01T00:00:00.000Z',
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'bg-task-1', status: 'completed' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('failed');
  });

  it('should complete a REST-synced subagent row via agentId when the WS row completes', () => {
    // Regression: a background subagent surfaces TWICE — once as the WS row
    // (keyed by agent id) and once as a REST `/tasks` row (keyed by its own
    // task id, with agent_id joining it back). The WS `subagent.completed`
    // event carries the AGENT id; without the agentId match the dock (REST)
    // row stays "Running" forever even though the subagent finished.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [
          { ...makeSubagentTask('agent-1', 's1'), runInBackground: false },
          {
            ...makeSubagentTask('rest-task-9', 's1'),
            agentId: 'agent-1',
            runInBackground: true,
            status: 'running',
          },
        ],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'agent-1', status: 'completed', outputPreview: 'done' },
      { sessionId: 's1', seq: 1 },
    );
    const rows = next.tasksBySession['s1'] ?? [];
    expect(rows.find((t) => t.id === 'agent-1')?.status).toBe('completed');
    expect(rows.find((t) => t.id === 'rest-task-9')?.status).toBe('completed');
    expect(rows.find((t) => t.id === 'rest-task-9')?.outputPreview).toBe('done');
  });

  it('should not flip an already-terminal REST row via the agentId fallback', () => {
    // Terminal-stickiness: a failed REST row must not regress to completed by a
    // late WS completion arriving through the agentId link.
    const state = {
      ...createInitialState(),
      tasksBySession: {
        's1': [{
          ...makeSubagentTask('rest-task-9', 's1'),
          agentId: 'agent-1',
          status: 'failed',
          completedAt: '2026-01-01T00:00:00.000Z',
        }],
      },
    };
    const next = reduceAppEvent(
      state,
      { type: 'taskCompleted', sessionId: 's1', taskId: 'agent-1', status: 'completed' },
      { sessionId: 's1', seq: 1 },
    );
    expect(next.tasksBySession['s1']?.[0]?.status).toBe('failed');
  });
});
