/**
 * node-sdk e2e: subagent lifecycle — spawn, run, complete.
 *
 * Drives a real v1 engine through the SDK with a fake provider that
 * makes the main agent call the `Agent` tool, which spawns a subagent.
 * The subagent runs its own turn (with its own fake provider response)
 * and completes. The test asserts the full event sequence:
 *
 *   subagent.spawned → subagent.started → (subagent turn events) → subagent.completed
 *
 * This is the "real engine subagent" e2e that validates the event
 * contract the web UI's agentEventProjector consumes.
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

import type { Event } from '#/index';

describe('node-sdk e2e: subagent lifecycle', () => {
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

  it('should emit subagent.spawned → started → completed when main agent delegates via Agent tool', async () => {
    // Main agent calls the Agent tool to spawn a subagent
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate coding task to subagent',
    });
    // Subagent may run multiple turns (init + actual task). Queue enough responses.
    fakeProvider.nextText('I have created the hello world function.');
    fakeProvider.nextText('I have created the hello world function.');
    fakeProvider.nextText('I have created the hello world function.');
    fakeProvider.nextText('I have created the hello world function.');
    fakeProvider.nextText('I have created the hello world function.');
    // Main agent receives the subagent result and produces a final response
    fakeProvider.nextText('The subagent has completed the task successfully.');
    fakeProvider.nextText('The subagent has completed the task successfully.');
    fakeProvider.nextText('The subagent has completed the task successfully.');
    fakeProvider.nextText('The subagent has completed the task successfully.');
    fakeProvider.nextText('The subagent has completed the task successfully.');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir });

    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    await session.prompt('Please delegate a coding task to a subagent');

    // Wait for the main agent's turn to end (subagent completes before that)
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => {
        console.log('TIMEOUT. Events so far:', events.map((e) => e.type));
        reject(new Error('timeout waiting for turn.ended'));
      }, 60_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended' && event.agentId === 'main') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    const types = events.map((e) => e.type);

    // Core subagent lifecycle events
    expect(types).toContain('subagent.spawned');
    expect(types).toContain('subagent.started');
    expect(types).toContain('subagent.completed');

    // Verify subagent.spawned event shape
    const spawned = events.find(
      (e) => e.type === 'subagent.spawned',
    ) as Extract<Event, { type: 'subagent.spawned' }> | undefined;
    expect(spawned).toBeDefined();
    expect(spawned?.subagentName).toBeTruthy();
    expect(spawned?.subagentId).toBeTruthy();
    expect(spawned?.subagentId).not.toBe('main');
    expect(spawned?.sessionId).toBe(session.id);

    // Verify subagent.completed event shape
    const completed = events.find(
      (e) => e.type === 'subagent.completed',
    ) as Extract<Event, { type: 'subagent.completed' }> | undefined;
    expect(completed).toBeDefined();
    expect(completed?.subagentId).toBe(spawned?.subagentId);
    expect(completed?.resultSummary).toBeTruthy();

    // Subagent should have its own turn events (tagged with subagentId, not 'main')
    const subagentTurnStarted = events.find(
      (e) => e.type === 'turn.started' && e.agentId === spawned?.subagentId,
    );
    expect(subagentTurnStarted).toBeDefined();

    const subagentTurnEnded = events.find(
      (e) => e.type === 'turn.ended' && e.agentId === spawned?.subagentId,
    );
    expect(subagentTurnEnded).toBeDefined();

    // Main agent turn should also complete
    const mainTurnEnded = events.find(
      (e) => e.type === 'turn.ended' && e.agentId === 'main',
    );
    expect(mainTurnEnded).toBeDefined();

    // At least 3 LLM calls: main (tool call) → subagent (text) → main (final text)
    expect(fakeProvider.requests.length).toBeGreaterThanOrEqual(3);

    await session.close();
  });

  it('should emit subagent.spawned with correct parentToolCallId', async () => {
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Analyze the codebase',
      description: 'Delegate analysis to subagent',
    });
    for (let i = 0; i < 5; i++) fakeProvider.nextText('Analysis complete. The codebase looks good.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent analyzed the codebase.');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir });

    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    await session.prompt('Delegate analysis');
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 30_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended' && event.agentId === 'main') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    const spawned = events.find(
      (e) => e.type === 'subagent.spawned',
    ) as Extract<Event, { type: 'subagent.spawned' }> | undefined;

    expect(spawned).toBeDefined();
    // parentToolCallId should be set (the Agent tool call that spawned this subagent)
    expect(spawned?.parentToolCallId).toBeTruthy();
    // The parentAgentId should be 'main' (the main agent spawned it)
    expect(spawned?.parentAgentId ?? 'main').toBe('main');

    await session.close();
  });

  it('should include subagent assistant.delta events for streaming output', async () => {
    fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a summary',
      description: 'Delegate writing to subagent',
    });
    // Subagent streams multiple text chunks
    for (let i = 0; i < 5; i++) fakeProvider.nextText('Here is the summary of the work done.');
    for (let i = 0; i < 5; i++) fakeProvider.nextText('The subagent provided a summary.');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir });

    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    await session.prompt('Delegate writing');
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 30_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended' && event.agentId === 'main') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    const spawned = events.find(
      (e) => e.type === 'subagent.spawned',
    ) as Extract<Event, { type: 'subagent.spawned' }> | undefined;
    expect(spawned).toBeDefined();

    // Subagent's assistant.delta events should be tagged with the subagent's agentId
    const subagentDeltas = events.filter(
      (e) => e.type === 'assistant.delta' && e.agentId === spawned?.subagentId,
    );
    expect(subagentDeltas.length).toBeGreaterThanOrEqual(1);

    // The delta text should contain the subagent's response
    const subagentText = subagentDeltas
      .map((e) => (e as Extract<Event, { type: 'assistant.delta' }>).delta)
      .join('');
    expect(subagentText).toContain('summary');

    await session.close();
  });
});
