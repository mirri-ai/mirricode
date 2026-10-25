import { describe, expect, it } from 'vitest';

import {
  approvalResponseToWire,
  normalizeWireDisplay,
  parseRustEventNotification,
  sdkApprovalRequestFromWire,
  sdkQuestionRequestFromWire,
  translateRustEvent,
  translateSkillCatalogChanged,
} from '#/rust-engine/translate';

describe('parseRustEventNotification', () => {
  it('should reject notifications when the method is not an event method', () => {
    expect(parseRustEventNotification({ method: 'session.create', params: {} })).toBeUndefined();
  });

  it('should parse the notification when the method is event.<type>', () => {
    const parsed = parseRustEventNotification({
      method: 'event.assistant.delta',
      params: {
        sessionId: 'session-1',
        volatile: true,
        timestamp: '2026-01-01T00:00:00Z',
        agentId: '0f4e9cf4-1c4a-4c8c-9bd3-a49262b6c6cd',
        delta: 'hi',
      },
    });
    expect(parsed).toEqual({
      type: 'assistant.delta',
      sessionId: 'session-1',
      payload: {
        agentId: '0f4e9cf4-1c4a-4c8c-9bd3-a49262b6c6cd',
        delta: 'hi',
      },
    });
  });

  it('should strip seq and epoch when the envelope is durable', () => {
    const parsed = parseRustEventNotification({
      method: 'event.turn.ended',
      params: {
        sessionId: 'session-1',
        volatile: false,
        timestamp: '2026-01-01T00:00:00Z',
        seq: 7,
        epoch: 2,
        turnId: 'turn-1',
        reason: 'completed',
      },
    });
    expect(parsed).toEqual({
      type: 'turn.ended',
      sessionId: 'session-1',
      payload: { turnId: 'turn-1', reason: 'completed' },
    });
  });

  it('should reject notifications when they are malformed', () => {
    expect(
      parseRustEventNotification({ method: 'event.assistant.delta', params: undefined }),
    ).toBeUndefined();
    expect(
      parseRustEventNotification({ method: 'event.assistant.delta', params: { delta: 'hi' } }),
    ).toBeUndefined();
    expect(
      parseRustEventNotification({ method: 'event.', params: { sessionId: 'session-1' } }),
    ).toBeUndefined();
    expect(
      parseRustEventNotification({ method: 'events.foo', params: { sessionId: 'session-1' } }),
    ).toBeUndefined();
  });
});

/** The nil uuid — the engine's wire spelling of the main agent's id. */
const NIL_AGENT_ID = '00000000-0000-0000-0000-000000000000';
/** A subagent's wire agent id (the uuid extracted from `agent-<uuid>`). */
const CHILD_AGENT_ID = '0f4e9cf4-1c4a-4c8c-9bd3-a49262b6c6cd';

/** Build one engine event notification with the transport envelope filled in. */
function engineEvent(type: string, payload: Record<string, unknown>): {
  method: string;
  params: Record<string, unknown>;
} {
  return {
    method: `event.${type}`,
    params: {
      sessionId: 'session-1',
      volatile: false,
      timestamp: '2026-01-01T00:00:00Z',
      ...payload,
    },
  };
}

describe('translateRustEvent', () => {
  it('should translate an assistant.delta when the engine streams a delta chunk', () => {
    expect(
      translateRustEvent(
        engineEvent('assistant.delta', { agentId: NIL_AGENT_ID, delta: 'Hello' }),
      ),
    ).toEqual({
      type: 'assistant.delta',
      turnId: 0,
      delta: 'Hello',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should keep the subagent agent id when the event carries a non-nil agent uuid', () => {
    expect(
      translateRustEvent(
        engineEvent('assistant.delta', { agentId: CHILD_AGENT_ID, delta: 'Hello' }),
      ),
    ).toMatchObject({
      type: 'assistant.delta',
      agentId: CHILD_AGENT_ID,
      sessionId: 'session-1',
    });
  });

  it('should translate a thinking.delta when the engine streams a reasoning chunk', () => {
    expect(
      translateRustEvent(
        engineEvent('thinking.delta', { agentId: NIL_AGENT_ID, delta: 'hmm' }),
      ),
    ).toEqual({
      type: 'thinking.delta',
      turnId: 0,
      delta: 'hmm',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a thinking.delta when it carries no delta text', () => {
    expect(translateRustEvent(engineEvent('thinking.delta', { agentId: NIL_AGENT_ID }))).toBeUndefined();
    expect(
      translateRustEvent(engineEvent('thinking.delta', { agentId: NIL_AGENT_ID, delta: '' })),
    ).toBeUndefined();
  });

  it('should translate a turn.started when the engine begins a turn', () => {
    expect(
      translateRustEvent(engineEvent('turn.started', { turnId: 'turn-0d9a1f3e' })),
    ).toEqual({
      type: 'turn.started',
      turnId: 0,
      origin: { kind: 'user' },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a failed turn.ended when the engine reports a failed turn', () => {
    const event = translateRustEvent(
      engineEvent('turn.ended', {
        agentId: NIL_AGENT_ID,
        turnId: 'turn-1',
        reason: 'failed',
        error: { code: 'provider.api_error', message: 'boom', retryable: false },
      }),
    );
    expect(event).toMatchObject({
      type: 'turn.ended',
      turnId: 0,
      reason: 'failed',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(event).toMatchObject({ error: { code: 'provider.api_error', message: 'boom' } });
  });

  it('should fall back to the generic engine error when a failure carries no message', () => {
    const event = translateRustEvent(engineEvent('turn.ended', { reason: 'failed' }));
    expect(event).toMatchObject({
      type: 'turn.ended',
      turnId: 0,
      reason: 'failed',
      agentId: 'main',
      sessionId: 'session-1',
      error: { code: 'internal', message: 'engine turn failed' },
    });
  });

  it('should fall back to the internal error code when a failure carries an unknown code', () => {
    const event = translateRustEvent(
      engineEvent('turn.ended', {
        reason: 'failed',
        error: { code: 'not.a.code', message: 'boom', retryable: false },
      }),
    );
    expect(event).toMatchObject({ error: { code: 'internal', message: 'boom' } });
  });

  it('should translate the non-failed turn.ended stop reasons', () => {
    expect(
      translateRustEvent(engineEvent('turn.ended', { turnId: 'turn-1', reason: 'completed' })),
    ).toMatchObject({ type: 'turn.ended', reason: 'completed' });
    expect(
      translateRustEvent(engineEvent('turn.ended', { turnId: 'turn-1', reason: 'cancelled' })),
    ).toMatchObject({ type: 'turn.ended', reason: 'cancelled' });
    expect(
      translateRustEvent(engineEvent('turn.ended', { turnId: 'turn-1', reason: 'filtered' })),
    ).toMatchObject({ type: 'turn.ended', reason: 'filtered' });
  });

  it('should translate a blocked turn.ended as a completed turn', () => {
    // The v1 TurnEndReason vocabulary has no `blocked`; a hook-blocked turn is
    // still over, and the block reason rides the hook.result event.
    expect(
      translateRustEvent(engineEvent('turn.ended', { turnId: 'turn-1', reason: 'blocked' })),
    ).toMatchObject({ type: 'turn.ended', reason: 'completed' });
  });

  it('should translate a turn.ended with an unknown reason as completed', () => {
    expect(
      translateRustEvent(engineEvent('turn.ended', { turnId: 'turn-1', reason: 'future' })),
    ).toMatchObject({ type: 'turn.ended', reason: 'completed' });
  });

  it('should translate a tool.call.started when the engine starts a tool call', () => {
    expect(
      translateRustEvent(
        engineEvent('tool.call.started', { toolCallId: 'call_1', name: 'Bash' }),
      ),
    ).toEqual({
      type: 'tool.call.started',
      turnId: 0,
      toolCallId: 'call_1',
      name: 'Bash',
      args: undefined,
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should pass the wire args through on a tool.call.started that carries them', () => {
    // The field-level alignment: the wire now carries the full parsed
    // argument object, the same field the TS engine's emit carries.
    expect(
      translateRustEvent(
        engineEvent('tool.call.started', {
          toolCallId: 'call_1',
          name: 'Bash',
          args: { command: 'echo hello' },
        }),
      ),
    ).toEqual({
      type: 'tool.call.started',
      turnId: 0,
      toolCallId: 'call_1',
      name: 'Bash',
      args: { command: 'echo hello' },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a tool.call.started when its identity fields are malformed', () => {
    expect(translateRustEvent(engineEvent('tool.call.started', { toolCallId: 'call_1' }))).toBeUndefined();
    expect(translateRustEvent(engineEvent('tool.call.started', { name: 'Bash' }))).toBeUndefined();
    expect(
      translateRustEvent(engineEvent('tool.call.started', { toolCallId: '', name: 'Bash' })),
    ).toBeUndefined();
  });

  it('should translate a tool.call.delta when the engine streams tool arguments', () => {
    expect(
      translateRustEvent(
        engineEvent('tool.call.delta', {
          agentId: NIL_AGENT_ID,
          toolCallId: 'call_1',
          name: 'Bash',
          argumentsPart: '{"command":"ls',
        }),
      ),
    ).toEqual({
      type: 'tool.call.delta',
      turnId: 0,
      toolCallId: 'call_1',
      name: 'Bash',
      argumentsPart: '{"command":"ls',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a tool.call.delta when the wire carried no tool identity', () => {
    expect(
      translateRustEvent(engineEvent('tool.call.delta', { argumentsPart: '{}' })),
    ).toBeUndefined();
    expect(
      translateRustEvent(engineEvent('tool.call.delta', { toolCallId: 'call_1' })),
    ).toBeUndefined();
  });

  it('should translate a tool.progress when a running tool streams an update', () => {
    expect(
      translateRustEvent(
        engineEvent('tool.progress', {
          toolCallId: 'call_1',
          update: { kind: 'stdout', text: 'line 1\n' },
        }),
      ),
    ).toEqual({
      type: 'tool.progress',
      turnId: 0,
      toolCallId: 'call_1',
      update: { kind: 'stdout', text: 'line 1\n' },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a tool.progress when its update shape is unknown', () => {
    expect(
      translateRustEvent(engineEvent('tool.progress', { toolCallId: 'call_1' })),
    ).toBeUndefined();
    expect(
      translateRustEvent(
        engineEvent('tool.progress', { toolCallId: 'call_1', update: { kind: 'future' } }),
      ),
    ).toBeUndefined();
  });

  it('should translate a tool.result when the engine completes a tool call', () => {
    expect(
      translateRustEvent(
        engineEvent('tool.result', { toolCallId: 'call_1', isError: true }),
      ),
    ).toEqual({
      type: 'tool.result',
      turnId: 0,
      toolCallId: 'call_1',
      output: '',
      isError: true,
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should pass the wire name through on a tool.result that carries it', () => {
    // The field-level alignment: the wire's tool.result now carries the
    // tool's name, so a consumer that missed the started event (a
    // mid-stream join) still attributes the card.
    expect(
      translateRustEvent(
        engineEvent('tool.result', { toolCallId: 'call_1', name: 'Bash', isError: false }),
      ),
    ).toEqual({
      type: 'tool.result',
      turnId: 0,
      toolCallId: 'call_1',
      output: '',
      isError: false,
      name: 'Bash',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a tool.result when the tool call id is malformed', () => {
    expect(translateRustEvent(engineEvent('tool.result', { isError: false }))).toBeUndefined();
  });

  it('should map the structured agent status fields when the wire carries them', () => {
    // The wire payload is the CR4 structured status bag (Task 8): `agentId`
    // + `phase` always, every other field optional and entirely absent when
    // unknown. Every field maps 1:1 onto the v1 `AgentStatusUpdatedEvent`.
    const usage = {
      byModel: {
        'kimi-k2': { inputOther: 10, output: 5, inputCacheRead: 2, inputCacheCreation: 1 },
      },
      currentTurn: { inputOther: 1, output: 1, inputCacheRead: 0, inputCacheCreation: 0 },
      total: { inputOther: 11, output: 6, inputCacheRead: 2, inputCacheCreation: 1 },
    };
    expect(
      translateRustEvent(
        engineEvent('agent.status.updated', {
          agentId: NIL_AGENT_ID,
          phase: 'streaming',
          model: 'kimi-k2',
          thinkingEffort: 'high',
          contextTokens: 120000,
          maxContextTokens: 200000,
          contextUsage: 0.6,
          planMode: true,
          swarmMode: false,
          usage,
        }),
      ),
    ).toEqual({
      type: 'agent.status.updated',
      agentId: 'main',
      sessionId: 'session-1',
      phase: 'streaming',
      model: 'kimi-k2',
      thinkingEffort: 'high',
      contextTokens: 120000,
      maxContextTokens: 200000,
      contextUsage: 0.6,
      planMode: true,
      swarmMode: false,
      usage,
    });
  });

  it('should emit only the required status fields when the wire carries the minimal payload', () => {
    expect(
      translateRustEvent(engineEvent('agent.status.updated', { agentId: 'main', phase: 'idle' })),
    ).toEqual({
      type: 'agent.status.updated',
      agentId: 'main',
      sessionId: 'session-1',
      phase: 'idle',
    });
  });

  it('should translate a hook.result when a hook renders into the context', () => {
    expect(
      translateRustEvent(
        engineEvent('hook.result', {
          hookEvent: 'UserPromptSubmit',
          content: 'lint clean',
          blocked: true,
        }),
      ),
    ).toEqual({
      type: 'hook.result',
      turnId: 0,
      hookEvent: 'UserPromptSubmit',
      content: 'lint clean',
      blocked: true,
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('hook.result', { hookEvent: 'UserPromptSubmit', content: 'lint clean' }),
      ),
    ).toEqual({
      type: 'hook.result',
      turnId: 0,
      hookEvent: 'UserPromptSubmit',
      content: 'lint clean',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a hook.result when its fields are malformed', () => {
    expect(translateRustEvent(engineEvent('hook.result', { content: 'x' }))).toBeUndefined();
    expect(translateRustEvent(engineEvent('hook.result', { hookEvent: 'H' }))).toBeUndefined();
  });

  it('should translate a compaction.started when a compaction begins', () => {
    expect(
      translateRustEvent(
        engineEvent('compaction.started', { trigger: 'manual', instruction: 'keep APIs' }),
      ),
    ).toEqual({
      type: 'compaction.started',
      trigger: 'manual',
      instruction: 'keep APIs',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(translateRustEvent(engineEvent('compaction.started', { trigger: 'auto' }))).toEqual({
      type: 'compaction.started',
      trigger: 'auto',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a compaction.started when its trigger is unknown', () => {
    expect(translateRustEvent(engineEvent('compaction.started', { trigger: 'cron' }))).toBeUndefined();
  });

  it('should translate a compaction.completed when a compaction lands its summary', () => {
    expect(
      translateRustEvent(
        engineEvent('compaction.completed', {
          result: {
            summary: 's',
            compactedCount: 4,
            tokensBefore: 100,
            tokensAfter: 40,
            droppedCount: 1,
          },
        }),
      ),
    ).toEqual({
      type: 'compaction.completed',
      result: {
        summary: 's',
        compactedCount: 4,
        tokensBefore: 100,
        tokensAfter: 40,
        droppedCount: 1,
      },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a compaction.completed when its result is malformed', () => {
    expect(translateRustEvent(engineEvent('compaction.completed', { result: {} }))).toBeUndefined();
    expect(
      translateRustEvent(
        engineEvent('compaction.completed', {
          result: { summary: 's', compactedCount: 'x', tokensBefore: 1, tokensAfter: 1 },
        }),
      ),
    ).toBeUndefined();
  });

  it('should translate a compaction.cancelled when an in-flight compaction is cancelled', () => {
    expect(translateRustEvent(engineEvent('compaction.cancelled', {}))).toEqual({
      type: 'compaction.cancelled',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a compaction.blocked when a turn blocks on an auto compaction', () => {
    expect(translateRustEvent(engineEvent('compaction.blocked', {}))).toEqual({
      type: 'compaction.blocked',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a background.task.started when a process task registers', () => {
    expect(
      translateRustEvent(
        engineEvent('background.task.started', {
          info: {
            kind: 'process',
            taskId: 'task_1',
            description: 'npm test',
            status: 'running',
            startedAt: 1767225600000,
            endedAt: null,
            command: 'npm test',
          },
        }),
      ),
    ).toEqual({
      type: 'background.task.started',
      info: {
        kind: 'process',
        taskId: 'task_1',
        description: 'npm test',
        status: 'running',
        startedAt: 1767225600000,
        endedAt: null,
        command: 'npm test',
        pid: 0,
        exitCode: null,
      },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a background.task.started when an agent task registers', () => {
    expect(
      translateRustEvent(
        engineEvent('background.task.started', {
          info: {
            kind: 'agent',
            taskId: 'task_2',
            description: 'refactor widgets',
            status: 'running',
            startedAt: 1767225600000,
            endedAt: null,
            agentId: CHILD_AGENT_ID,
            subagentType: 'coder',
          },
        }),
      ),
    ).toEqual({
      type: 'background.task.started',
      info: {
        kind: 'agent',
        taskId: 'task_2',
        description: 'refactor widgets',
        status: 'running',
        startedAt: 1767225600000,
        endedAt: null,
        agentId: CHILD_AGENT_ID,
        subagentType: 'coder',
      },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a background.task.terminated when a task reaches a terminal status', () => {
    expect(
      translateRustEvent(
        engineEvent('background.task.terminated', {
          info: {
            kind: 'question',
            taskId: 'task_3',
            description: 'pick one',
            status: 'completed',
            startedAt: 1767225600000,
            endedAt: 1767225660000,
            questionCount: 2,
          },
        }),
      ),
    ).toEqual({
      type: 'background.task.terminated',
      info: {
        kind: 'question',
        taskId: 'task_3',
        description: 'pick one',
        status: 'completed',
        startedAt: 1767225600000,
        endedAt: 1767225660000,
        questionCount: 2,
      },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a background task event when its info shape is malformed', () => {
    expect(translateRustEvent(engineEvent('background.task.started', {}))).toBeUndefined();
    expect(
      translateRustEvent(engineEvent('background.task.started', { info: { kind: 'future' } })),
    ).toBeUndefined();
    expect(
      translateRustEvent(
        engineEvent('background.task.started', {
          info: {
            kind: 'process',
            taskId: 'task_1',
            description: 'd',
            status: 'exploded',
            startedAt: 1,
            endedAt: null,
          },
        }),
      ),
    ).toBeUndefined();
  });

  it('should drop event notifications when their type has no v1 counterpart', () => {
    for (const method of [
      'event.message.received',
      'event.session.deleted',
      'event.session.updated',
      'event.agent.created',
      'event.agent.removed',
      'event.approval.requested',
      'event.approval.resolved',
      'event.question.requested',
      'event.question.answered',
      'event.question.dismissed',
      'event.future.type',
    ]) {
      expect(
        translateRustEvent({ method, params: { sessionId: 'session-1', volatile: false, timestamp: 't' } }),
      ).toBeUndefined();
    }
  });

  it('should drop notifications when they are malformed', () => {
    expect(
      translateRustEvent({ method: 'event.assistant.delta', params: undefined }),
    ).toBeUndefined();
    expect(translateRustEvent({ method: 'other', params: {} })).toBeUndefined();
  });

  it('should drop an assistant.delta when it carries no delta text', () => {
    expect(
      translateRustEvent(
        engineEvent('assistant.delta', { agentId: NIL_AGENT_ID }),
      ),
    ).toBeUndefined();
    expect(
      translateRustEvent(
        engineEvent('assistant.delta', { agentId: NIL_AGENT_ID, delta: '' }),
      ),
    ).toBeUndefined();
  });
});

describe('translateRustEvent P7a families', () => {
  it('should translate a subagent.spawned when the engine spawns a child', () => {
    expect(
      translateRustEvent(
        engineEvent('subagent.spawned', {
          subagentId: 'agent-child-1',
          subagentName: 'coder',
          parentToolCallId: 'call-1',
          description: 'delegate work',
          runInBackground: false,
          model: 'kimi/alpha',
        }),
      ),
    ).toEqual({
      type: 'subagent.spawned',
      subagentId: 'agent-child-1',
      subagentName: 'coder',
      parentToolCallId: 'call-1',
      description: 'delegate work',
      runInBackground: false,
      model: 'kimi/alpha',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a subagent.spawned when the wire carries no parent tool call id', () => {
    expect(
      translateRustEvent(
        engineEvent('subagent.spawned', {
          subagentId: 'agent-child-1',
          subagentName: 'coder',
          runInBackground: false,
        }),
      ),
    ).toBeUndefined();
  });

  it('should translate the subagent started/suspended/completed/failed lifecycle', () => {
    expect(
      translateRustEvent(engineEvent('subagent.started', { subagentId: 'agent-child-1' })),
    ).toEqual({
      type: 'subagent.started',
      subagentId: 'agent-child-1',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('subagent.suspended', { subagentId: 'agent-child-1', reason: 'rate_limited' }),
      ),
    ).toEqual({
      type: 'subagent.suspended',
      subagentId: 'agent-child-1',
      reason: 'rate_limited',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('subagent.completed', {
          subagentId: 'agent-child-1',
          resultSummary: 'done',
          usage: { inputOther: 1, output: 2, inputCacheRead: 3, inputCacheCreation: 4 },
        }),
      ),
    ).toEqual({
      type: 'subagent.completed',
      subagentId: 'agent-child-1',
      resultSummary: 'done',
      usage: { inputOther: 1, output: 2, inputCacheRead: 3, inputCacheCreation: 4 },
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('subagent.failed', { subagentId: 'agent-child-1', error: 'boom' }),
      ),
    ).toEqual({
      type: 'subagent.failed',
      subagentId: 'agent-child-1',
      error: 'boom',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should carry a structured approval suspension reason through as its JSON text', () => {
    // The rust projection emits the ADR-011 structured reason as a wire OBJECT
    // (event_projection.rs: "the wire keeps both shapes and consumers
    // discriminate with typeof"). The SDK event contract is reason: string, so
    // the translate serializes the object verbatim — the event must NOT be
    // dropped (pre-fix it was: typeof reason !== 'string' → undefined).
    expect(
      translateRustEvent(
        engineEvent('subagent.suspended', {
          subagentId: 'agent-child-1',
          reason: { kind: 'approval', request_id: 'r1', tool_call_id: 'tc1' },
        }),
      ),
    ).toEqual({
      type: 'subagent.suspended',
      subagentId: 'agent-child-1',
      reason: '{"kind":"approval","request_id":"r1","tool_call_id":"tc1"}',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should carry a structured rate-limit suspension reason through as its JSON text', () => {
    expect(
      translateRustEvent(
        engineEvent('subagent.suspended', {
          subagentId: 'agent-child-1',
          reason: { kind: 'rate_limit' },
        }),
      ),
    ).toEqual({
      type: 'subagent.suspended',
      subagentId: 'agent-child-1',
      reason: '{"kind":"rate_limit"}',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate the turn.step lifecycle with the synthetic turn id', () => {
    // The wire turn ids are the `turn-<uuid>` strings the dispatch minted
    // (the same key `turn.started` carries — the field-level alignment);
    // the v1 events require a number, so the translation keeps the
    // synthetic id regardless of the wire's real one.
    expect(
      translateRustEvent(
        engineEvent('turn.step.started', { turnId: 'turn-abc', step: 1 }),
      ),
    ).toEqual({
      type: 'turn.step.started',
      turnId: 0,
      step: 1,
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('turn.step.completed', {
          turnId: 'turn-abc',
          step: 1,
          usage: { inputOther: 1, output: 2, inputCacheRead: 0, inputCacheCreation: 0 },
        }),
      ),
    ).toMatchObject({ type: 'turn.step.completed', turnId: 0, step: 1 });
    expect(
      translateRustEvent(
        engineEvent('turn.step.retrying', {
          turnId: 'turn-abc',
          step: 2,
          failedAttempt: 1,
          nextAttempt: 2,
          maxAttempts: 3,
          delayMs: 0,
        }),
      ),
    ).toMatchObject({
      type: 'turn.step.retrying',
      turnId: 0,
      step: 2,
      failedAttempt: 1,
      nextAttempt: 2,
      maxAttempts: 3,
      delayMs: 0,
    });
    expect(
      translateRustEvent(
        engineEvent('turn.step.interrupted', { turnId: 'turn-abc', step: 2, reason: 'aborted' }),
      ),
    ).toEqual({
      type: 'turn.step.interrupted',
      turnId: 0,
      step: 2,
      reason: 'aborted',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a goal.updated folding the budget limits into the report', () => {
    expect(
      translateRustEvent(
        engineEvent('goal.updated', {
          snapshot: {
            goalId: 'goal-1',
            objective: 'ship it',
            status: 'active',
            turnsUsed: 2,
            tokensUsed: 100,
            wallClockMs: 5000,
            budget: { turns: 10, tokens: 1000, wallClockMs: 60000 },
          },
          change: { kind: 'lifecycle', status: 'active', actor: 'user' },
        }),
      ),
    ).toEqual({
      type: 'goal.updated',
      snapshot: {
        goalId: 'goal-1',
        objective: 'ship it',
        status: 'active',
        turnsUsed: 2,
        tokensUsed: 100,
        wallClockMs: 5000,
        budget: {
          tokenBudget: 1000,
          turnBudget: 10,
          wallClockBudgetMs: 60000,
          remainingTokens: 900,
          remainingTurns: 8,
          remainingWallClockMs: 55000,
          tokenBudgetReached: false,
          turnBudgetReached: false,
          wallClockBudgetReached: false,
          overBudget: false,
        },
      },
      change: { kind: 'lifecycle', status: 'active', actor: 'user' },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a goal.updated with the null snapshot when the goal is cleared', () => {
    expect(
      translateRustEvent(
        engineEvent('goal.updated', {
          snapshot: null,
          change: { kind: 'lifecycle', reason: 'cancelled', actor: 'user' },
        }),
      ),
    ).toEqual({
      type: 'goal.updated',
      snapshot: null,
      change: { kind: 'lifecycle', reason: 'cancelled', actor: 'user' },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate the model-actor goal.updated and model-tool skill.activated (the agent-tool path)', () => {
    expect(
      translateRustEvent(
        engineEvent('goal.updated', {
          snapshot: {
            goalId: 'goal-1',
            objective: 'ship it',
            status: 'complete',
            turnsUsed: 3,
            tokensUsed: 42,
            wallClockMs: 0,
            budget: null,
          },
          change: { kind: 'lifecycle', status: 'complete', actor: 'model' },
        }),
      ),
    ).toEqual({
      type: 'goal.updated',
      snapshot: {
        goalId: 'goal-1',
        objective: 'ship it',
        status: 'complete',
        turnsUsed: 3,
        tokensUsed: 42,
        wallClockMs: 0,
        // A null budget folds into the v1 GoalBudgetReport shape (all
        // limits absent — the same spelling the RPC snapshot serves).
        budget: {
          turnBudget: null,
          tokenBudget: null,
          wallClockBudgetMs: null,
          remainingTurns: null,
          remainingTokens: null,
          remainingWallClockMs: null,
          turnBudgetReached: false,
          tokenBudgetReached: false,
          wallClockBudgetReached: false,
          overBudget: false,
        },
      },
      change: { kind: 'lifecycle', status: 'complete', actor: 'model' },
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('skill.activated', {
          activationId: 'act-9',
          skillName: 'commit',
          args: '-m "fix"',
          trigger: 'model-tool',
          skillPath: '/ws/skills/commit/SKILL.md',
          skillSource: 'project',
        }),
      ),
    ).toEqual({
      type: 'skill.activated',
      activationId: 'act-9',
      skillName: 'commit',
      args: '-m "fix"',
      trigger: 'model-tool',
      skillPath: '/ws/skills/commit/SKILL.md',
      skillSource: 'project',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate skill.activated / plugin_command.activated / mcp.server.status', () => {
    expect(
      translateRustEvent(
        engineEvent('skill.activated', {
          activationId: 'act-1',
          skillName: 'commit',
          trigger: 'user-slash',
          skillSource: 'project',
        }),
      ),
    ).toEqual({
      type: 'skill.activated',
      activationId: 'act-1',
      skillName: 'commit',
      trigger: 'user-slash',
      skillSource: 'project',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('plugin_command.activated', {
          activationId: 'act-2',
          pluginId: 'plugin-1',
          commandName: 'build',
          trigger: 'user-slash',
        }),
      ),
    ).toEqual({
      type: 'plugin_command.activated',
      activationId: 'act-2',
      pluginId: 'plugin-1',
      commandName: 'build',
      trigger: 'user-slash',
      agentId: 'main',
      sessionId: 'session-1',
    });
    expect(
      translateRustEvent(
        engineEvent('mcp.server.status', {
          server: {
            name: 'srv',
            transport: 'stdio',
            status: 'connected',
            toolCount: 3,
            pool: 'user',
          },
        }),
      ),
    ).toEqual({
      type: 'mcp.server.status',
      server: {
        name: 'srv',
        transport: 'stdio',
        status: 'connected',
        toolCount: 3,
        pool: 'user',
      },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate session.meta.updated and cron.fired', () => {
    expect(
      translateRustEvent(
        engineEvent('session.meta.updated', { title: 'renamed' }),
      ),
    ).toEqual({
      type: 'session.meta.updated',
      title: 'renamed',
      agentId: 'main',
      sessionId: 'session-1',
    });

    expect(
      translateRustEvent(
        engineEvent('cron.fired', {
          origin: {
            kind: 'cron_job',
            jobId: 'cron-1',
            cron: '* * * * *',
            recurring: false,
            coalescedCount: 1,
            stale: false,
          },
          prompt: 'check the build',
        }),
      ),
    ).toEqual({
      type: 'cron.fired',
      origin: {
        kind: 'cron_job',
        jobId: 'cron-1',
        cron: '* * * * *',
        recurring: false,
        coalescedCount: 1,
        stale: false,
      },
      prompt: 'check the build',
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should translate a session.model_switched onto the meta patch when the engine switches models mid-turn', () => {
    // The v1 `Event` union has no first-class model-switch variant, so the
    // wire's durable switch (spec 11 §2.5 A2) rides the free-form
    // `session.meta.updated` patch as `modelSwitched` — the same channel a
    // real meta change uses; the TUI renders the divider from it.
    expect(
      translateRustEvent(
        engineEvent('session.model_switched', { from: 'model-a', to: 'model-b' }),
      ),
    ).toEqual({
      type: 'session.meta.updated',
      patch: { modelSwitched: { from: 'model-a', to: 'model-b' } },
      agentId: 'main',
      sessionId: 'session-1',
    });
  });

  it('should drop a session.model_switched when its aliases are malformed', () => {
    expect(translateRustEvent(engineEvent('session.model_switched', { from: 'model-a' }))).toBeUndefined();
    expect(translateRustEvent(engineEvent('session.model_switched', { from: '', to: 'model-b' }))).toBeUndefined();
    expect(
      translateRustEvent(engineEvent('session.model_switched', { from: 'model-a', to: 3 })),
    ).toBeUndefined();
  });
});

describe('translateSkillCatalogChanged', () => {
  it('should translate the camelCase workspace payload into the no-session event', () => {
    expect(
      translateSkillCatalogChanged({
        workspaceId: 'wd_demo_ab12',
        added: ['alpha'],
        removed: [],
        updated: ['beta'],
        trigger: 'file',
      }),
    ).toEqual({
      type: 'skill.catalogChanged',
      workspaceId: 'wd_demo_ab12',
      added: ['alpha'],
      removed: [],
      updated: ['beta'],
      trigger: 'file',
      agentId: 'main',
      sessionId: '',
    });
  });

  it('should drop the notification when the payload is malformed', () => {
    expect(translateSkillCatalogChanged(undefined)).toBeUndefined();
    expect(translateSkillCatalogChanged({ added: ['a'] })).toBeUndefined();
    expect(
      translateSkillCatalogChanged({
        workspaceId: 'ws',
        added: 'alpha',
        removed: [],
        updated: [],
        trigger: 'file',
      }),
    ).toBeUndefined();
    expect(
      translateSkillCatalogChanged({
        workspaceId: 'ws',
        added: [],
        removed: [],
        updated: [],
        trigger: 'bogus',
      }),
    ).toBeUndefined();
  });
});

describe('sdkApprovalRequestFromWire', () => {
  it('maps the engine wire params onto the camelCase SDK request', () => {
    expect(
      sdkApprovalRequestFromWire({
        tool_name: 'Bash',
        tool_call_id: 'call_1',
        action: 'execute Bash',
        display: { Command: { command: 'ls -la' } },
        reason: 'approval required',
      }),
    ).toEqual({
      toolCallId: 'call_1',
      toolName: 'Bash',
      action: 'execute Bash',
      display: { kind: 'command', command: 'ls -la' },
    });
  });

  it('returns undefined for malformed params', () => {
    expect(sdkApprovalRequestFromWire(undefined)).toBeUndefined();
    expect(sdkApprovalRequestFromWire({ tool_name: 'Bash' })).toBeUndefined();
    expect(sdkApprovalRequestFromWire({ tool_name: 'Bash', tool_call_id: '', action: 'x' })).toBeUndefined();
  });
});

describe('approvalResponseToWire', () => {
  it('maps the SDK response onto the engine wire shape', () => {
    expect(approvalResponseToWire({ decision: 'approved' })).toEqual({ decision: 'approved' });
    expect(
      approvalResponseToWire({ decision: 'rejected', feedback: 'no', selectedLabel: 'session' }),
    ).toEqual({ decision: 'rejected', feedback: 'no', selected_label: 'session' });
  });
});

describe('sdkQuestionRequestFromWire', () => {
  it('maps the engine tool input onto the SDK question request', () => {
    expect(
      sdkQuestionRequestFromWire({
        questions: [
          {
            question: 'Pick one',
            header: 'H',
            options: [
              { label: 'A', description: 'first' },
              { label: 'B' },
            ],
            multi_select: true,
          },
        ],
      }),
    ).toEqual({
      questions: [
        {
          question: 'Pick one',
          header: 'H',
          options: [
            { label: 'A', description: 'first' },
            { label: 'B' },
          ],
          multiSelect: true,
        },
      ],
    });
  });

  it('defaults a missing options array to an empty array', () => {
    expect(sdkQuestionRequestFromWire({ questions: [{ question: 'Free text?' }] })).toEqual({
      questions: [{ question: 'Free text?', options: [] }],
    });
  });

  it('returns undefined for malformed payloads', () => {
    expect(sdkQuestionRequestFromWire(undefined)).toBeUndefined();
    expect(sdkQuestionRequestFromWire({})).toBeUndefined();
    expect(sdkQuestionRequestFromWire({ questions: [] })).toBeUndefined();
    expect(sdkQuestionRequestFromWire({ questions: [{ question: '' }] })).toBeUndefined();
    expect(sdkQuestionRequestFromWire({ questions: [{ question: 'x', options: [{}] }] })).toBeUndefined();
    expect(
      sdkQuestionRequestFromWire({ questions: [{ question: 'x', options: 'nope' }] }),
    ).toBeUndefined();
  });
});

describe('normalizeWireDisplay', () => {
  it('maps every known externally-tagged variant onto the SDK union', () => {
    expect(normalizeWireDisplay({ Command: { command: 'ls' } })).toEqual({
      kind: 'command',
      command: 'ls',
    });
    expect(normalizeWireDisplay({ FileIo: { path: '/a', operation: 'read' } })).toEqual({
      kind: 'file_io',
      operation: 'read',
      path: '/a',
    });
    expect(normalizeWireDisplay({ Diff: { path: '/a' } })).toEqual({
      kind: 'diff',
      path: '/a',
      before: '',
      after: '',
    });
    expect(normalizeWireDisplay({ Search: { pattern: 'foo', path: '/src' } })).toEqual({
      kind: 'search',
      query: 'foo',
      scope: '/src',
    });
    expect(normalizeWireDisplay({ UrlFetch: { url: 'https://x.dev' } })).toEqual({
      kind: 'url_fetch',
      url: 'https://x.dev',
    });
    expect(
      normalizeWireDisplay({ AgentCall: { agent_type: 'coder', prompt: 'do it' } }),
    ).toEqual({ kind: 'agent_call', agent_name: 'coder', prompt: 'do it' });
    expect(normalizeWireDisplay({ SkillCall: { skill: 'commit' } })).toEqual({
      kind: 'skill_call',
      skill_name: 'commit',
    });
    expect(normalizeWireDisplay({ TaskStop: { task_id: 't1' } })).toEqual({
      kind: 'task_stop',
      task_id: 't1',
      task_description: '',
    });
  });

  it('given an internally-tagged file_io display, when normalized, then it maps with the wire operation validated', () => {
    expect(normalizeWireDisplay({ kind: 'file_io', path: '/a.md', operation: 'read' })).toEqual({
      kind: 'file_io',
      operation: 'read',
      path: '/a.md',
    });
    // An operation the protocol enum rejects falls back to 'write'.
    expect(normalizeWireDisplay({ kind: 'file_io', path: '/a.md', operation: 'Read' })).toEqual({
      kind: 'file_io',
      operation: 'write',
      path: '/a.md',
    });
  });

  it('given an internally-tagged diff display, when normalized, then before and after carry through', () => {
    expect(
      normalizeWireDisplay({ kind: 'diff', path: '/a.rs', before: 'old', after: 'new' }),
    ).toEqual({ kind: 'diff', path: '/a.rs', before: 'old', after: 'new' });
  });

  it('given an internally-tagged search display, when normalized, then query and scope map onto the SDK union', () => {
    expect(normalizeWireDisplay({ kind: 'search', query: 'foo', scope: 'src' })).toEqual({
      kind: 'search',
      query: 'foo',
      scope: 'src',
    });
    expect(normalizeWireDisplay({ kind: 'search', query: 'foo' })).toEqual({
      kind: 'search',
      query: 'foo',
    });
  });

  it('given an internally-tagged agent_call display, when normalized, then agent_name maps from the wire key', () => {
    expect(
      normalizeWireDisplay({ kind: 'agent_call', agent_name: 'coder', prompt: 'do it' }),
    ).toEqual({ kind: 'agent_call', agent_name: 'coder', prompt: 'do it' });
    expect(
      normalizeWireDisplay({
        kind: 'agent_call',
        agent_name: 'coder',
        prompt: 'do it',
        background: true,
      }),
    ).toEqual({ kind: 'agent_call', agent_name: 'coder', prompt: 'do it', background: true });
  });

  it('given an internally-tagged skill_call display, when normalized, then skill_name maps from the wire key', () => {
    expect(normalizeWireDisplay({ kind: 'skill_call', skill_name: 'commit' })).toEqual({
      kind: 'skill_call',
      skill_name: 'commit',
    });
    expect(
      normalizeWireDisplay({ kind: 'skill_call', skill_name: 'commit', args: '-m "x"' }),
    ).toEqual({ kind: 'skill_call', skill_name: 'commit', args: '-m "x"' });
  });

  it('given an internally-tagged task_stop display, when normalized, then a missing task_description defaults to an empty string', () => {
    expect(normalizeWireDisplay({ kind: 'task_stop', task_id: 't-1' })).toEqual({
      kind: 'task_stop',
      task_id: 't-1',
      task_description: '',
    });
  });

  it('given an internally-tagged plan_review display, when normalized, then option descriptions default to an empty string', () => {
    expect(
      normalizeWireDisplay({
        kind: 'plan_review',
        plan: 'the plan',
        options: [{ label: 'Approach A' }, { label: 'Approach B', description: 'safer' }],
      }),
    ).toEqual({
      kind: 'plan_review',
      plan: 'the plan',
      options: [
        { label: 'Approach A', description: '' },
        { label: 'Approach B', description: 'safer' },
      ],
    });
    expect(
      normalizeWireDisplay({ kind: 'plan_review', plan: 'the plan', path: '/p.md' }),
    ).toEqual({ kind: 'plan_review', plan: 'the plan', path: '/p.md' });
  });

  it('given an internally-tagged goal_start display, when normalized, then completion_criterion maps and mode defaults to manual', () => {
    expect(
      normalizeWireDisplay({
        kind: 'goal_start',
        objective: 'ship it',
        completion_criterion: 'tests green',
      }),
    ).toEqual({
      kind: 'goal_start',
      objective: 'ship it',
      completionCriterion: 'tests green',
      mode: 'manual',
    });
    expect(normalizeWireDisplay({ kind: 'goal_start', objective: 'ship it' })).toEqual({
      kind: 'goal_start',
      objective: 'ship it',
      mode: 'manual',
    });
  });

  it('given an internally-tagged command/url_fetch/generic display, when normalized, then each maps onto the SDK union', () => {
    expect(normalizeWireDisplay({ kind: 'command', command: 'ls' })).toEqual({
      kind: 'command',
      command: 'ls',
    });
    expect(normalizeWireDisplay({ kind: 'url_fetch', url: 'https://x.dev' })).toEqual({
      kind: 'url_fetch',
      url: 'https://x.dev',
    });
    expect(normalizeWireDisplay({ kind: 'generic', summary: 's' })).toEqual({
      kind: 'generic',
      summary: 's',
    });
  });

  it('given a bare internally-tagged todo_list display, when normalized, then it degrades to a generic display', () => {
    const fallback = normalizeWireDisplay({ kind: 'todo_list' });
    expect(fallback).toMatchObject({ kind: 'generic', summary: 'Tool call' });
    expect(fallback).toMatchObject({ detail: { kind: 'todo_list' } });
  });

  it('given a bare internally-tagged background_task display, when normalized, then it degrades to a generic display', () => {
    const fallback = normalizeWireDisplay({ kind: 'background_task' });
    expect(fallback).toMatchObject({ kind: 'generic', summary: 'Tool call' });
    expect(fallback).toMatchObject({ detail: { kind: 'background_task' } });
  });

  it('given an unknown kind tag, when normalized, then it degrades to a generic display', () => {
    const fallback = normalizeWireDisplay({ kind: 'future_variant', x: 1 });
    expect(fallback).toMatchObject({ kind: 'generic', summary: 'Tool call' });
    expect(fallback).toMatchObject({ detail: { kind: 'future_variant', x: 1 } });
  });

  it('falls back to a generic display for unknown or malformed shapes', () => {
    const fallback = normalizeWireDisplay({ FutureVariant: { x: 1 } });
    expect(fallback).toMatchObject({ kind: 'generic', summary: 'Tool call' });
    expect(fallback).toMatchObject({ detail: { FutureVariant: { x: 1 } } });

    expect(normalizeWireDisplay('not-an-object')).toMatchObject({ kind: 'generic' });
    expect(normalizeWireDisplay({ Command: 'not-an-object' })).toMatchObject({ kind: 'generic' });
    expect(normalizeWireDisplay({ a: 1, b: 2 })).toMatchObject({ kind: 'generic' });
  });
});