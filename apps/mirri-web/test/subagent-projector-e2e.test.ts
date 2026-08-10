/**
 * Web UI projector e2e: real engine subagent events → projector → task state.
 *
 * Uses V1EngineFixture + FakeProviderServer to produce real subagent
 * events, then feeds each event through the web UI's agentEventProjector
 * to verify the projector correctly maps them to taskCreated / taskProgress /
 * taskCompleted — the state updates that drive the "Sub Agent" dock panel
 * and AgentDetailPanel in the web UI.
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type EngineFixture,
  type FakeProviderServer,
  createFakeProviderServer,
  createV1EngineFixture,
  makeTempDir,
  removeTempDir,
} from '@mirri-ai/e2e-harness';

import { createAgentProjector } from '../src/api/daemon/agentEventProjector';
import type { ProjectorEvent } from '../src/api/daemon/agentEventProjector';

interface TaskState {
  id: string;
  status: string;
  subagentPhase?: string;
  outputChunks: string[];
}

function reduceProjectorEvents(
  events: ProjectorEvent[],
): Map<string, TaskState> {
  const tasks = new Map<string, TaskState>();
  for (const event of events) {
    if (event.type === 'taskCreated' && event.task) {
      const existing = tasks.get(event.task.id) ?? { id: event.task.id, status: '', outputChunks: [] };
      existing.status = event.task.status ?? existing.status;
      existing.subagentPhase = event.task.subagentPhase ?? existing.subagentPhase;
      tasks.set(event.task.id, existing);
    } else if (event.type === 'taskProgress' && event.taskId) {
      const task = tasks.get(event.taskId) ?? { id: event.taskId, status: 'running', outputChunks: [] };
      if (event.outputChunk) {
        task.outputChunks.push(event.outputChunk);
      }
      tasks.set(event.taskId, task);
    } else if (event.type === 'taskCompleted' && event.taskId) {
      const task = tasks.get(event.taskId) ?? { id: event.taskId, status: '', outputChunks: [] };
      task.status = 'completed';
      tasks.set(event.taskId, task);
    }
  }
  return tasks;
}

describe('Web UI projector: real engine subagent events', () => {
  let fakeProvider: FakeProviderServer;
  let fixture: EngineFixture;
  let workDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    const homeDir = await makeTempDir();
    workDir = await makeTempDir();
    tempDirs.push(homeDir, workDir);
    fixture = await createV1EngineFixture({ fakeProvider, homeDir });
  });

  afterEach(async () => {
    await fixture.close();
    await fakeProvider.close();
    for (const dir of tempDirs.splice(0)) {
      await removeTempDir(dir);
    }
  });

  it('should project subagent.spawned → started → completed into task lifecycle', async () => {
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task to subagent',
    });
    for (let i = 0; i < 5; i++) fakeProvider.nextText('I have created the hello world function.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent has completed the task successfully.');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir });

    const projector = createAgentProjector();
    const projectorEvents: ProjectorEvent[] = [];

    session.onEvent((event) => {
      // Feed each engine event through the projector (same as the web UI's ws.ts does)
      const projected = projector.project(event.type, event as Record<string, unknown>, session.id);
      projectorEvents.push(...projected);
    });

    await session.prompt('Please delegate a coding task to a subagent');
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 60_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended' && event.agentId === 'main') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    // The projector should have produced taskCreated for the subagent
    const taskCreatedEvents = projectorEvents.filter(
      (e) => e.type === 'taskCreated',
    );
    expect(taskCreatedEvents.length).toBeGreaterThanOrEqual(1);

    // Reduce to final task states
    const tasks = reduceProjectorEvents(projectorEvents);

    // Find the subagent task (not 'main')
    const subagentTask = Array.from(tasks.values()).find((t) => t.id !== 'main');
    expect(subagentTask).toBeDefined();
    expect(subagentTask?.status).toBe('completed');

    // The projector should have produced taskProgress with the subagent's text output
    const progressEvents = projectorEvents.filter(
      (e) => e.type === 'taskProgress' && e.taskId === subagentTask?.id,
    );
    expect(progressEvents.length).toBeGreaterThanOrEqual(1);

    // The output chunks should contain the subagent's response text
    const outputText = progressEvents
      .map((e) => e.outputChunk ?? '')
      .join('');
    expect(outputText).toContain('hello world');

    await session.close();
  });

  it('should project subagent assistant.delta as streaming taskProgress', async () => {
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a summary',
      description: 'Delegate writing to subagent',
    });
    for (let i = 0; i < 5; i++) fakeProvider.nextText('Here is the summary of the work done.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent provided a summary.');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir });

    const projector = createAgentProjector();
    const projectorEvents: ProjectorEvent[] = [];

    session.onEvent((event) => {
      const projected = projector.project(event.type, event as Record<string, unknown>, session.id);
      projectorEvents.push(...projected);
    });

    await session.prompt('Delegate writing');
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 60_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended' && event.agentId === 'main') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    // Find text-kind progress events (subagent assistant.delta → taskProgress kind=text)
    const textProgress = projectorEvents.filter(
      (e) => e.type === 'taskProgress' && e['kind'] === 'text',
    );
    expect(textProgress.length).toBeGreaterThanOrEqual(1);

    // Verify the streaming text contains the subagent's output
    const streamText = textProgress
      .map((e) => e.outputChunk ?? '')
      .join('');
    expect(streamText).toContain('summary');

    await session.close();
  });
});
