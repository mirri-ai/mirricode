import { describe, expect, it } from 'vitest';
import {
  classifyFrame,
  createAgentProjector,
  subagentProgressText,
  turnUsageForPromptId,
} from '../src/api/daemon/agentEventProjector';
import type { AppSessionUsage } from '../src/api/types';

function makeUsage(partial: Partial<AppSessionUsage> = {}): AppSessionUsage {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    totalCostUsd: 0,
    contextTokens: 0,
    contextLimit: 0,
    turnCount: 0,
    ...partial,
  };
}

describe('subagentProgressText', () => {
  it('drops turn.step.started as noise', () => {
    expect(subagentProgressText('turn.step.started', {})).toBeNull();
  });

  it('summarizes a read tool call with its path', () => {
    const text = subagentProgressText('tool.use', { name: 'read', args: { path: 'src/foo.ts' } });
    expect(text).toContain('src/foo.ts');
    expect(text).not.toContain('"path"');
  });

  it('summarizes a bash tool call with its command', () => {
    const text = subagentProgressText('tool.call.started', { name: 'bash', args: { command: 'pnpm test' } });
    expect(text).toContain('pnpm test');
    expect(text).not.toContain('"command"');
  });

  it('drops tool.result lines as noise', () => {
    expect(subagentProgressText('tool.result', { name: 'read' })).toBeNull();
    expect(subagentProgressText('tool.result', { name: 'Read_0' })).toBeNull();
  });

  it('returns tool.progress update text', () => {
    expect(subagentProgressText('tool.progress', { update: { text: 'working…' } })).toBe('working…');
  });

  it('caps a long tool.progress text', () => {
    const long = 'x'.repeat(3000);
    const text = subagentProgressText('tool.progress', { update: { text: long } });
    expect(text).not.toBeNull();
    expect(text!.length).toBeLessThan(long.length);
    expect(text!.endsWith('…')).toBe(true);
  });

  it('returns null for unknown event types', () => {
    expect(subagentProgressText('turn.delta', {})).toBeNull();
  });
});

describe('subagent streaming text', () => {
  it('forwards a subagent assistant.delta as a text-kind taskProgress', () => {
    const projector = createAgentProjector();
    const events = projector.project('assistant.delta', { agentId: 'sub-1', delta: 'Hello' }, 's1');
    expect(events).toContainEqual({
      type: 'taskProgress',
      sessionId: 's1',
      taskId: 'sub-1',
      outputChunk: 'Hello',
      stream: 'stdout',
      kind: 'text',
    });
  });

  it('drops an empty subagent assistant.delta', () => {
    const projector = createAgentProjector();
    const events = projector.project('assistant.delta', { agentId: 'sub-1', delta: '' }, 's1');
    expect(events).toEqual([]);
  });
});

describe('cron.fired', () => {
  it('synthesizes a user message so the cron notice renders live', () => {
    const projector = createAgentProjector();
    const events = projector.project(
      'cron.fired',
      {
        origin: {
          kind: 'cron_job',
          jobId: 'a3f9c2',
          cron: '*/5 * * * *',
          recurring: true,
          coalescedCount: 2,
          stale: false,
        },
        prompt: 'Check the deploy status',
      },
      's1',
    );
    const created = events.find((e) => e.type === 'messageCreated');
    expect(created).toBeDefined();
    expect(created).toMatchObject({
      type: 'messageCreated',
      message: {
        role: 'user',
        content: [{ type: 'text', text: 'Check the deploy status' }],
        metadata: { origin: { kind: 'cron_job', jobId: 'a3f9c2' } },
      },
    });
  });

  it('ignores cron.fired events missing a prompt or a cron_job origin', () => {
    const projector = createAgentProjector();
    expect(projector.project('cron.fired', { origin: { kind: 'cron_job' } }, 's1')).toEqual([]);
    expect(projector.project('cron.fired', { prompt: 'hi' }, 's1')).toEqual([]);
  });
});

describe('cron.fired prompt id isolation', () => {
  it('omits promptId so the synthesized notice does not clobber the abort cache', () => {
    const projector = createAgentProjector();
    projector.project(
      'prompt.submitted',
      { promptId: 'pr_user', userMessageId: 'u1', content: [{ type: 'text', text: 'hi' }] },
      's1',
    );
    const events = projector.project(
      'cron.fired',
      {
        origin: {
          kind: 'cron_job',
          jobId: 'j',
          cron: '* * * * *',
          recurring: true,
          coalescedCount: 1,
          stale: false,
        },
        prompt: 'Check the deploy status',
      },
      's1',
    );
    const created = events.find((e) => e.type === 'messageCreated');
    expect(created).toBeDefined();
    expect((created as { message: { promptId?: string } }).message.promptId).toBeUndefined();
  });
});

// The engine's turn ids are `turn-<uuid>` strings on every turn.* frame
// (the turn.step family aligned with turn.started in the T5 field-level
// alignment). The projector keys its turnPromptId map by the raw wire
// value, so a turn's step events must resolve to the prompt the turn's own
// turn.started bound — the pre-alignment numeric step keys mismatched the
// string turn.started key and silently fell into the mid-turn-join
// fallback on every step.
describe('string turn-id binding', () => {
  it('binds turn.step.started to the prompt its turn.started bound (same wire turn id)', () => {
    const projector = createAgentProjector();
    const turnEvents = projector.project('turn.started', { turnId: 'turn-abc' }, 's1');
    expect(turnEvents.some((e) => e.type === 'turnActiveChanged')).toBe(true);

    // A step under the same string turn id must NOT synthesize a fresh
    // prompt (the mid-turn-join fallback) — it rides the turn's binding.
    // A second turn under a different id demonstrates the map keys the
    // raw wire values apart.
    const stepEvents = projector.project(
      'turn.step.started',
      { turnId: 'turn-abc', step: 1 },
      's1',
    );
    expect(stepEvents.some((e) => e.type === 'messageCreated')).toBe(true);

    // tool.call.started now carries the wire args object — the tool-use
    // slot records what the tool was invoked with.
    const toolEvents = projector.project(
      'tool.call.started',
      { turnId: 'turn-abc', toolCallId: 'call_1', name: 'Bash', args: { command: 'ls' } },
      's1',
    );
    expect(toolEvents.some((e) => e.type === 'messageUpdated')).toBe(true);
  });
});

describe('classifyFrame cron.fired', () => {
  it('routes both raw and event.-prefixed cron.fired to the agent projector', () => {
    const payload = { origin: { kind: 'cron_job' }, prompt: 'x' };
    expect(classifyFrame('cron.fired', payload)).toEqual({ route: 'agent', agentType: 'cron.fired' });
    expect(classifyFrame('event.cron.fired', payload)).toEqual({ route: 'agent', agentType: 'cron.fired' });
  });
});

// Session status has a single source: the daemon's event.session.status_changed
// (mapped by toAppEvent). The raw turn stream must NOT project a second
// sessionWorkChanged per transition — when it did, every turn end fired
// turn-end consumers (completion notification, sound) twice.
describe('session status single-sourcing', () => {
  it('turn.started projects no sessionWorkChanged', () => {
    const projector = createAgentProjector();
    const events = projector.project('turn.started', { turnId: 1 }, 's1');
    expect(events.some((e) => e.type === 'sessionWorkChanged')).toBe(false);
  });

  it('turn.ended finalizes the message and usage but projects no sessionWorkChanged', () => {
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 1 }, 's1');
    projector.project('turn.step.started', { turnId: 1, step: 1 }, 's1');
    const events = projector.project(
      'turn.ended',
      { turnId: 1, reason: 'completed', durationMs: 123 },
      's1',
    );
    expect(events.some((e) => e.type === 'sessionWorkChanged')).toBe(false);
    expect(events).toContainEqual(
      expect.objectContaining({ type: 'messageUpdated', status: 'completed', durationMs: 123 }),
    );
    expect(events).toContainEqual(expect.objectContaining({ type: 'sessionUsageUpdated' }));
  });

  it('seedInFlight returns only the seeded message — status comes from the snapshot', () => {
    const projector = createAgentProjector();
    const events = projector.seedInFlight('s1', {
      turnId: 1,
      assistantText: 'partial',
      thinkingText: '',
      runningTools: [],
    });
    expect(events.some((e) => e.type === 'sessionWorkChanged')).toBe(false);
    expect(events).toContainEqual(
      expect.objectContaining({
        type: 'messageCreated',
        message: expect.objectContaining({ role: 'assistant' }),
      }),
    );
  });
});

describe('step-boundary delta alignment', () => {
  it('resets stream offsets at step boundaries — a post-step delta ahead of local state signals a gap', () => {
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 1 }, 's1');
    projector.project('turn.step.started', { turnId: 1, step: 1 }, 's1');
    projector.project('assistant.delta', { turnId: 1, delta: 'step-one text' }, 's1', { offset: 0 });
    projector.project('turn.step.completed', { turnId: 1, step: 1 }, 's1');
    projector.project('turn.step.started', { turnId: 1, step: 2 }, 's1');

    const events = projector.project('assistant.delta', { turnId: 1, delta: 'tail' }, 's1', { offset: 12 });
    expect(events).toContainEqual(
      expect.objectContaining({ type: 'historyCompacted', reason: 'delta_gap' }),
    );
  });

  it('appends step-2 deltas to the fresh step message at step-relative offsets', () => {
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 1 }, 's1');
    projector.project('turn.step.started', { turnId: 1, step: 1 }, 's1');
    projector.project('assistant.delta', { turnId: 1, delta: 'step one' }, 's1', { offset: 0 });
    projector.project('turn.step.completed', { turnId: 1, step: 1 }, 's1');

    const step2 = projector.project('turn.step.started', { turnId: 1, step: 2 }, 's1');
    const created = step2.find((e) => e.type === 'messageCreated');
    const msgId = (created as { message: { id: string } } | undefined)?.message.id;
    expect(msgId).toBeDefined();

    // Offset restarts at 0 for the new step and appends to ITS message.
    const events = projector.project('assistant.delta', { turnId: 1, delta: 'step two' }, 's1', { offset: 0 });
    expect(events).toContainEqual(
      expect.objectContaining({
        type: 'assistantDelta',
        messageId: msgId,
        delta: { text: 'step two' },
      }),
    );
  });

  it('seeds only the current step and aligns live deltas against the seeded length', () => {
    const projector = createAgentProjector();
    const seeded = projector.seedInFlight('s1', {
      turnId: 7,
      promptId: 'pr_1',
      thinkingText: 'step two thinking',
      assistantText: 'step two partial',
      runningTools: [{ toolCallId: 'tc_1', name: 'bash', args: { command: 'ls' } }],
    });
    const created = seeded.find((e) => e.type === 'messageCreated');
    const message = (created as { message: { id: string; content: unknown[] } } | undefined)?.message;
    expect(message).toBeDefined();

    expect(message!.content).toEqual([
      { type: 'thinking', thinking: 'step two thinking' },
      { type: 'text', text: 'step two partial' },
      { type: 'toolUse', toolCallId: 'tc_1', toolName: 'bash', input: { command: 'ls' } },
    ]);

    const dup = projector.project('assistant.delta', { turnId: 7, delta: 'two part' }, 's1', { offset: 5 });
    expect(dup).toEqual([]);

    const cont = projector.project(
      'assistant.delta',
      { turnId: 7, delta: ' continues' },
      's1',
      { offset: 'step two partial'.length },
    );
    expect(cont).toContainEqual(
      expect.objectContaining({
        type: 'assistantDelta',
        messageId: message!.id,
        contentIndex: 3,
        delta: { text: ' continues' },
      }),
    );
  });
});

describe('background.task.terminated for agent-kind tasks', () => {
  it('should patch subagent phase and status when agent id is available', () => {
    // Simulate a full background sub-agent lifecycle:
    // 1. subagent.spawned → task created (agent id keyed)
    // 2. background.task.started → backgroundTaskId linked
    // 3. background.task.terminated → should also sync subagentPhase
    const projector = createAgentProjector();
    projector.project('subagent.spawned', {
      subagentId: 'agent-1',
      subagentName: 'coder',
      parentToolCallId: 'call-1',
      runInBackground: true,
      description: 'explore the codebase',
    }, 's1');

    projector.project('background.task.started', {
      info: {
        taskId: 'bg-task-1',
        kind: 'agent',
        agentId: 'agent-1',
        detached: true,
        description: 'explore the codebase',
      },
    }, 's1');

    const events = projector.project('background.task.terminated', {
      info: {
        taskId: 'bg-task-1',
        kind: 'agent',
        agentId: 'agent-1',
        status: 'completed',
      },
    }, 's1');

    // Should emit taskCreated with updated subagentPhase
    const taskCreated = events.find(
      (e: any) => e.type === 'taskCreated' && e.task?.id === 'agent-1',
    );
    expect(taskCreated).toBeDefined();
    expect((taskCreated as any).task.subagentPhase).toBe('completed');
    expect((taskCreated as any).task.status).toBe('completed');

    // Should also emit taskCompleted for the background task id
    const taskCompleted = events.find(
      (e: any) => e.type === 'taskCompleted' && e.taskId === 'bg-task-1',
    );
    expect(taskCompleted).toBeDefined();
    expect((taskCompleted as any).status).toBe('completed');
  });

  it('should mark subagent as failed when background task terminates with failure', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', {
      subagentId: 'agent-2',
      subagentName: 'coder',
      parentToolCallId: 'call-2',
      runInBackground: true,
      description: 'fix the bug',
    }, 's1');

    projector.project('background.task.started', {
      info: {
        taskId: 'bg-task-2',
        kind: 'agent',
        agentId: 'agent-2',
        detached: true,
        description: 'fix the bug',
      },
    }, 's1');

    const events = projector.project('background.task.terminated', {
      info: {
        taskId: 'bg-task-2',
        kind: 'agent',
        agentId: 'agent-2',
        status: 'failed',
      },
    }, 's1');

    const taskCreated = events.find(
      (e: any) => e.type === 'taskCreated' && e.task?.id === 'agent-2',
    );
    expect(taskCreated).toBeDefined();
    expect((taskCreated as any).task.subagentPhase).toBe('failed');
    expect((taskCreated as any).task.status).toBe('failed');
  });

  it('should map the killed terminal word onto cancelled when the engine stopped a background process', () => {
    // The rust BackgroundTaskInfo status vocabulary is running/completed/
    // failed/killed — a manager-stopped task must not settle as completed.
    const projector = createAgentProjector();
    const events = projector.project('background.task.terminated', {
      info: {
        taskId: 'bg-task-5',
        kind: 'process',
        status: 'killed',
      },
    }, 's1');

    const taskCompleted = events.find(
      (e: any) => e.type === 'taskCompleted' && e.taskId === 'bg-task-5',
    );
    expect(taskCompleted).toBeDefined();
    expect((taskCompleted as any).status).toBe('cancelled');
  });

  it('should not add agent-kind patching when kind is not agent', () => {
    // Non-agent background.task.terminated (e.g. bash process) should not
    // emit a subagentPhase taskCreated — only the standard taskCompleted.
    const projector = createAgentProjector();
    const events = projector.project('background.task.terminated', {
      info: {
        taskId: 'bg-task-3',
        kind: 'process',
        status: 'completed',
      },
    }, 's1');

    expect(events.some((e: any) => e.type === 'taskCreated')).toBe(false);
    expect(events.some((e: any) => e.type === 'taskCompleted')).toBe(true);
  });

  it('should not add agent-kind patching when agent id is absent', () => {
    const projector = createAgentProjector();
    const events = projector.project('background.task.terminated', {
      info: {
        taskId: 'bg-task-4',
        kind: 'agent',
        status: 'completed',
      },
    }, 's1');

    expect(events.some((e: any) => e.type === 'taskCreated')).toBe(false);
    expect(events.some((e: any) => e.type === 'taskCompleted')).toBe(true);
  });
});

describe('per-turn usage aggregation', () => {
  it('should accumulate per-turn usage when turn steps complete under one turn', () => {
    const projector = createAgentProjector();
    projector.bindNextPromptId('s1', 'prompt-1');
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      {
        turnId: 'turn-1',
        step: 1,
        usage: { inputOther: 100, output: 5, inputCacheRead: 200, inputCacheCreation: 0 },
        model: 'expert',
      },
      's1',
    );
    projector.project(
      'turn.step.completed',
      {
        turnId: 'turn-1',
        step: 2,
        usage: { inputOther: 50, output: 3, inputCacheRead: 80, inputCacheCreation: 0 },
        model: 'expert',
      },
      's1',
    );

    const usage = turnUsageForPromptId(projector.state('s1'), 'prompt-1');
    expect(usage).toMatchObject({
      inputTokens: 150,
      outputTokens: 8,
      cacheReadTokens: 280,
      cacheCreationTokens: 0,
      models: ['expert'],
    });
  });

  it('should collect distinct model aliases across steps of a turn', () => {
    const projector = createAgentProjector();
    projector.bindNextPromptId('s1', 'prompt-1');
    projector.project('turn.started', { turnId: 'turn-m' }, 's1');
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-m', step: 1, usage: { inputOther: 10, output: 1, inputCacheRead: 0, inputCacheCreation: 0 }, model: 'expert' },
      's1',
    );
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-m', step: 2, usage: { inputOther: 10, output: 1, inputCacheRead: 0, inputCacheCreation: 0 }, model: 'fast' },
      's1',
    );
    // A repeat of an already-seen alias must not duplicate the entry.
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-m', step: 3, usage: { inputOther: 10, output: 1, inputCacheRead: 0, inputCacheCreation: 0 }, model: 'expert' },
      's1',
    );

    const usage = turnUsageForPromptId(projector.state('s1'), 'prompt-1');
    expect(usage?.models).toEqual(['expert', 'fast']);
    expect(usage?.inputTokens).toBe(30);
  });

  it('should not attribute subagent turns to the main prompt when the turn id is unbound', () => {
    // A subagent's turn.started streams with its own agentId and never binds
    // a prompt, so its steps must not land in any prompt's per-turn usage.
    const projector = createAgentProjector();
    projector.bindNextPromptId('s1', 'prompt-1');
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      {
        turnId: 'turn-sub',
        agentId: 'agent-sub',
        step: 1,
        usage: { inputOther: 10, output: 0, inputCacheRead: 0, inputCacheCreation: 0 },
      },
      's1',
    );

    expect(turnUsageForPromptId(projector.state('s1'), 'prompt-1')).toBeUndefined();
  });

  it('should exclude a subagent turn usage from the bound main prompt of the same session', () => {
    const projector = createAgentProjector();
    projector.bindNextPromptId('s1', 'prompt-1');
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, usage: { inputOther: 100, output: 5, inputCacheRead: 0, inputCacheCreation: 0 } },
      's1',
    );
    projector.project(
      'turn.step.completed',
      {
        turnId: 'turn-sub',
        agentId: 'agent-sub',
        step: 1,
        usage: { inputOther: 10, output: 2, inputCacheRead: 0, inputCacheCreation: 0 },
      },
      's1',
    );

    const usage = turnUsageForPromptId(projector.state('s1'), 'prompt-1');
    expect(usage).toMatchObject({ inputTokens: 100, outputTokens: 5 });
  });

  it('should count subagent steps in the session-level usage totals (SDR-002 full aggregation)', () => {
    // Session totals are agent-agnostic: every agent's turn.step.completed —
    // main agent or subagent — feeds state.totalInput and friends. Pinned so
    // the main-agent-only transcript filter never silently scopes the totals.
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, usage: { inputOther: 100, output: 5, inputCacheRead: 0, inputCacheCreation: 0 } },
      's1',
    );
    projector.project(
      'turn.step.completed',
      {
        turnId: 'turn-sub',
        agentId: 'agent-sub',
        step: 1,
        usage: { inputOther: 10, output: 2, inputCacheRead: 30, inputCacheCreation: 0 },
      },
      's1',
    );

    const events = projector.project('turn.ended', { turnId: 'turn-1', reason: 'completed' }, 's1');
    expect(events).toContainEqual(
      expect.objectContaining({
        type: 'sessionUsageUpdated',
        usage: expect.objectContaining({ inputTokens: 110, outputTokens: 7, cacheReadTokens: 30 }),
      }),
    );
  });
});

describe('subagent.spawned model propagation', () => {
  it('should carry the resolved model alias onto the created AppTask', () => {
    const projector = createAgentProjector();
    const events = projector.project('subagent.spawned', {
      subagentId: 'agent-m1',
      subagentName: 'coder',
      parentToolCallId: 'call-m1',
      runInBackground: false,
      description: 'explore the codebase',
      model: 'claude-sonnet',
    }, 's1');

    const taskCreated = events.find(
      (e: any) => e.type === 'taskCreated' && e.task?.id === 'agent-m1',
    );
    expect(taskCreated).toBeDefined();
    expect((taskCreated as any).task.model).toBe('claude-sonnet');
  });

  it('should leave model undefined when the spawn event carries none', () => {
    const projector = createAgentProjector();
    const events = projector.project('subagent.spawned', {
      subagentId: 'agent-m2',
      subagentName: 'coder',
      parentToolCallId: 'call-m2',
      runInBackground: true,
    }, 's1');

    const taskCreated = events.find(
      (e: any) => e.type === 'taskCreated' && e.task?.id === 'agent-m2',
    );
    expect(taskCreated).toBeDefined();
    expect((taskCreated as any).task.model).toBeUndefined();
  });
});

describe('cold-start baseline (SDR-004)', () => {
  it('should record the first completed step as the cold-start baseline', () => {
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, usage: { inputOther: 100, output: 5, inputCacheRead: 200, inputCacheCreation: 0 } },
      's1',
    );
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 2, usage: { inputOther: 50, output: 3, inputCacheRead: 80, inputCacheCreation: 0 } },
      's1',
    );
    expect(projector.state('s1').firstCall).toEqual({
      inputTokens: 100,
      outputTokens: 5,
      cacheReadTokens: 200,
      cacheCreationTokens: 0,
    });
  });

  it('should not capture a subagent step as the baseline when it completes before the main agent first step', () => {
    const projector = createAgentProjector();
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    // A subagent spawned inside the main agent's step 1 completes its own
    // step while the main accumulators are still zero.
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, agentId: 'agent-sub', usage: { inputOther: 30, output: 2, inputCacheRead: 0, inputCacheCreation: 0 } },
      's1',
    );
    // The main agent's step 1 completes only after the subagent tool returns.
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, usage: { inputOther: 100, output: 5, inputCacheRead: 200, inputCacheCreation: 0 } },
      's1',
    );
    expect(projector.state('s1').firstCall).toEqual({
      inputTokens: 100,
      outputTokens: 5,
      cacheReadTokens: 200,
      cacheCreationTokens: 0,
    });
  });

  it('should not capture a baseline when the accumulators were seeded with usage but no baseline', () => {
    const projector = createAgentProjector();
    projector.reset('s1');
    projector.seedUsage('s1', makeUsage({ inputTokens: 500, cacheReadTokens: 100, turnCount: 3 }));
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    projector.project(
      'turn.step.completed',
      { turnId: 'turn-1', step: 1, usage: { inputOther: 10, output: 1, inputCacheRead: 0, inputCacheCreation: 0 } },
      's1',
    );
    expect(projector.state('s1').firstCall).toBeNull();
  });
});

describe('seedUsage', () => {
  it('should seed the accumulators so the next sessionUsageUpdated continues from the snapshot totals', () => {
    // The refresh regression: without seeding, the first live event after a
    // reload overwrote the REST snapshot's totals with near-zero ones.
    const projector = createAgentProjector();
    projector.reset('s1');
    projector.seedUsage(
      's1',
      makeUsage({ inputTokens: 244_730, outputTokens: 1_371, cacheReadTokens: 189_000, turnCount: 7 }),
    );
    projector.project('turn.started', { turnId: 'turn-9' }, 's1');
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-9', reason: 'completed', durationMs: 1 },
      's1',
    );
    const usageEvent = events.find((e) => e.type === 'sessionUsageUpdated');
    expect(usageEvent).toMatchObject({
      usage: expect.objectContaining({ inputTokens: 244_730, outputTokens: 1_371, turnCount: 8 }),
    });
  });

  it('should carry the seeded baseline onto the sessionUsageUpdated usage', () => {
    const projector = createAgentProjector();
    projector.reset('s1');
    projector.seedUsage(
      's1',
      makeUsage({
        inputTokens: 500,
        cacheReadTokens: 100,
        firstCall: { inputTokens: 90, outputTokens: 0, cacheReadTokens: 10, cacheCreationTokens: 0 },
      }),
    );
    projector.project('turn.started', { turnId: 'turn-1' }, 's1');
    const events = projector.project(
      'turn.ended',
      { turnId: 'turn-1', reason: 'completed', durationMs: 1 },
      's1',
    );
    const usageEvent = events.find((e) => e.type === 'sessionUsageUpdated');
    expect(usageEvent).toMatchObject({
      usage: expect.objectContaining({
        firstCall: { inputTokens: 90, outputTokens: 0, cacheReadTokens: 10, cacheCreationTokens: 0 },
      }),
    });
  });
});

describe('subagent structured live steps (ADR-009)', () => {
  const AGENT = 'agent_1';

  function projectMember(projector: ReturnType<typeof createAgentProjector>, type: string, payload: Record<string, unknown>) {
    return projector.project(type, { ...payload, agentId: AGENT }, 's1');
  }

  it('should open a running step on tool.call.started and carry it on the task row', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    const events = projectMember(projector, 'tool.call.started', {
      toolCallId: 'tc_1',
      name: 'Read',
      args: { path: 'a.ts' },
    });

    const created = events.find((e) => e.type === 'taskCreated');
    expect(created).toBeDefined();
    if (created?.type === 'taskCreated') {
      expect(created.task.steps).toEqual([
        { id: 'tc_1', name: 'Read', arg: JSON.stringify({ path: 'a.ts' }), status: 'running', outputLines: [] },
      ]);
    }
  });

  it('should append progress lines and settle status/output on the result frame', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    projectMember(projector, 'tool.use', { toolCallId: 'tc_1', name: 'Bash', args: 'cargo test' });
    projectMember(projector, 'tool.progress', { toolCallId: 'tc_1', update: { text: 'running 1/3' } });
    const events = projectMember(projector, 'tool.result', {
      toolCallId: 'tc_1',
      output: 'test result: ok',
      isError: false,
    });

    const created = events.filter((e) => e.type === 'taskCreated').at(-1);
    expect(created).toBeDefined();
    expect(created?.type === 'taskCreated' && created.task.steps?.[0]?.status).toBe('ok');
    expect(created?.type === 'taskCreated' && created.task.steps?.[0]?.outputLines).toEqual(['test result: ok']);
  });

  it('should mark a failed result as an error step', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    projectMember(projector, 'tool.use', { toolCallId: 'tc_1', name: 'Bash', args: 'ls' });
    const events = projectMember(projector, 'tool.result', { toolCallId: 'tc_1', output: 'boom', isError: true });

    const created = events.filter((e) => e.type === 'taskCreated').at(-1);
    expect(created).toBeDefined();
    expect(created?.type === 'taskCreated' && created.task.steps?.[0]?.status).toBe('error');
  });

  it('should ignore member tool frames after the member settled', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');
    projector.project('subagent.completed', { subagentId: AGENT, resultSummary: 'done' }, 's1');

    const events = projectMember(projector, 'tool.use', { toolCallId: 'tc_late', name: 'Read', args: 'x' });

    expect(events).toEqual([]);
  });

  it('should refresh a duplicate started frame for the same call id instead of stacking a second card', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    projectMember(projector, 'tool.use', { toolCallId: 'tc_1', name: 'Read', args: 'a.ts' });
    const events = projectMember(projector, 'tool.call.started', { toolCallId: 'tc_1', name: 'Read', args: 'a.ts' });

    const created = events.filter((e) => e.type === 'taskCreated').at(-1);
    expect(created).toBeDefined();
    expect(created?.type === 'taskCreated' && created.task.steps?.length).toBe(1);
  });

  it('should drop a tool progress frame when no step was started for its call id', () => {
    // Given the member exists but a progress frame races ahead of its
    // tool.call.started (ordering race / replay partial).
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    // When the orphan progress frame arrives.
    projectMember(projector, 'tool.progress', { toolCallId: 'tc_orphan', update: { text: 'stray line' } });

    // Then no step was created — the stray line must not resurrect a card.
    const events = projectMember(projector, 'tool.call.started', { toolCallId: 'tc_1', name: 'Read', args: 'a.ts' });
    const created = events.filter((e) => e.type === 'taskCreated').at(-1);
    expect(created?.type === 'taskCreated' && created.task.steps?.map((s) => s.id)).toEqual(['tc_1']);
  });

  it('should forward member thinking.delta as a thinking-kind taskProgress', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    const events = projectMember(projector, 'thinking.delta', { delta: 'Considering the layout…' });

    const textProgress = events.filter((e) => e.type === 'taskProgress');
    expect(textProgress).toHaveLength(1);
    expect(textProgress[0]).toMatchObject({ kind: 'thinking', outputChunk: 'Considering the layout…' });
  });

  it('should keep the legacy Calling line alongside the structured steps for line-based consumers', () => {
    const projector = createAgentProjector();
    projector.project('subagent.spawned', { subagentId: AGENT, description: 'Explore' }, 's1');

    const events = projectMember(projector, 'tool.use', {
      toolCallId: 'tc_1',
      name: 'Read',
      args: { file_path: 'note.txt' },
    });

    const line = events.find((e) => e.type === 'taskProgress');
    expect(line).toMatchObject({ taskId: AGENT });
    expect(line?.type === 'taskProgress' && line.outputChunk).toContain('Read');
  });
});
