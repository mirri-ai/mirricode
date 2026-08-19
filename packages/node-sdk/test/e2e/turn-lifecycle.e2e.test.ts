/**
 * node-sdk e2e: turn lifecycle — basic prompt, tool call, approval, question.
 *
 * These tests boot a real engine via `EngineFixture` + `FakeProviderServer`
 * (v1 or v2, see the `engine` parameter) and exercise the SDK `Session` API
 * (prompt, onEvent, approval/question handlers) without any vi.mock. The
 * fake provider returns scripted LLM responses over HTTP.
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type EngineFixture,
  type EngineKind,
  type FakeProviderServer,
  createEngineFixture,
  createFakeProviderServer,
  makeTempDir,
  removeTempDir,
} from '@mirri-ai/e2e-harness';

import type { Event } from '#/index';

describe.each(['v1', 'v2'] as const)('node-sdk e2e: turn lifecycle (engine=%s)', (engine: EngineKind) => {
  let fakeProvider: FakeProviderServer;
  let fixture: EngineFixture;
  let workDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    const homeDir = await makeTempDir();
    workDir = await makeTempDir();
    tempDirs.push(homeDir, workDir);
    fixture = await createEngineFixture({ fakeProvider, homeDir }, engine);
  });

  afterEach(async () => {
    await fixture.close();
    await fakeProvider.close();
    for (const dir of tempDirs.splice(0)) {
      await removeTempDir(dir);
    }
  });

  it('should emit turn.started → assistant.delta → turn.ended for a text prompt', async () => {
    fakeProvider.nextText('hello response');

    const session = await fixture.createSession({ workDir });
    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    await session.prompt('hello');
    await session.waitForTurnEnd();

    const types = events.map((e) => e.type);
    expect(types).toContain('turn.started');
    expect(types).toContain('assistant.delta');
    expect(types).toContain('turn.ended');

    const deltaEvent = events.find(
      (e) => e.type === 'assistant.delta',
    ) as Extract<Event, { type: 'assistant.delta' }> | undefined;
    expect(deltaEvent?.delta).toContain('hello response');

    const turnEnded = events.find(
      (e) => e.type === 'turn.ended',
    ) as Extract<Event, { type: 'turn.ended' }> | undefined;
    expect(turnEnded?.reason).toBe('completed');

    await session.close();
  });

  it('should handle tool call with approval handler', async () => {
    fakeProvider.nextToolCall('Bash', { command: 'echo hi' });
    fakeProvider.nextText('tool completed successfully');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir, permission: 'manual' });

    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    // Set up approval handler that auto-approves — use raw SDK session
    let approvalRequested = false;
    session.setApprovalHandler((request) => {
      approvalRequested = true;
      expect(request.toolName).toBe('Bash');
      return { decision: 'approved' as const };
    });

    await session.prompt('run echo hi');

    // Wait for turn.ended
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 10_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    expect(approvalRequested).toBe(true);
    expect(fakeProvider.requests).toHaveLength(2);

    const types = events.map((e) => e.type);
    // Tool call should produce tool-related events (exact type names vary by SDK mapping)
    const hasToolEvent = types.some((t) => t.includes('tool'));
    expect(hasToolEvent).toBe(true);

    await session.close();
  });

  it('should handle tool call with denial', async () => {
    fakeProvider.nextToolCall('Bash', { command: 'rm -rf /' });
    fakeProvider.nextText('understood, I will not run that');

    const harness = fixture.rawHarness;
    const session = await harness.createSession({ workDir, permission: 'manual' });

    const events: Event[] = [];
    session.onEvent((event) => events.push(event));

    session.setApprovalHandler(() => {
      return { decision: 'rejected' as const, feedback: 'dangerous command' };
    });

    await session.prompt('delete everything');

    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('timeout')), 10_000);
      const unsub = session.onEvent((event) => {
        if (event.type === 'turn.ended') {
          clearTimeout(timeout);
          unsub();
          resolve();
        }
      });
    });

    expect(fakeProvider.requests).toHaveLength(2);

    // Tool-related events should be present (tool.call, tool.result, or permission.*)
    const types = events.map((e) => e.type);
    const hasToolEvent = types.some(
      (t) => t.startsWith('tool.') || t.startsWith('permission.'),
    );
    expect(hasToolEvent).toBe(true);

    // The rejection feedback reached the engine: the second provider request
    // carries a tool message whose content states the denial reason.
    const secondBody = fakeProvider.requests[1]?.bodyJson as {
      messages?: Array<{ role?: string; content?: unknown }>;
    };
    const secondToolMessages = (secondBody?.messages ?? []).filter((m) => m.role === 'tool');
    expect(secondToolMessages.length).toBeGreaterThan(0);
    const toolResultContent = JSON.stringify(
      secondToolMessages.map((message) => message.content),
    );
    expect(toolResultContent).toContain('dangerous command');

    await session.close();
  });

  it('should support multi-turn conversation with context accumulation', async () => {
    fakeProvider.nextText('first answer');
    fakeProvider.nextText('second answer');

    const session = await fixture.createSession({ workDir });

    // First turn
    const events1: Event[] = [];
    const unsub1 = session.onEvent((e) => events1.push(e));
    await session.prompt('first question');
    await session.waitForTurnEnd();
    unsub1();

    // Second turn — the provider should receive history from the first turn
    const events2: Event[] = [];
    const unsub2 = session.onEvent((e) => events2.push(e));
    await session.prompt('second question');
    await session.waitForTurnEnd();
    unsub2();

    expect(fakeProvider.requests).toHaveLength(2);

    // Second request body should contain messages from both turns
    const secondBody = fakeProvider.requests[1]?.bodyJson as { messages?: { role: string; content: string }[] };
    const secondMessages = secondBody?.messages ?? [];
    expect(secondMessages.length).toBeGreaterThan(2); // system + user1 + assistant1 + user2

    await session.close();
  });
});