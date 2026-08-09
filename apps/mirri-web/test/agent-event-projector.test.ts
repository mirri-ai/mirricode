import { describe, expect, it } from 'vitest';
import { classifyFrame, createAgentProjector, subagentProgressText } from '../src/api/daemon/agentEventProjector';

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
