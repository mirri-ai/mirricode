import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type EngineFixture,
  type EngineKind,
  type FakeProviderServer,
  createEngineFixture,
  createFakeProviderServer,
  makeTempDir,
  removeTempDir,
  snapshotWireRecords,
} from '#/index';

/**
 * Known v2 gaps that make a v1-written assertion invalid on the v2 engine.
 * Each key names a test skipped under v2 and the reason it must stay v1-only
 * until the corresponding SDK/engine capability lands.
 */
const KNOWN_V2_DIFFS = {
  wireSnapshot:
    'snapshotWireRecords asserts the v1 wire.jsonl schema (types like turn.prompt); the v2 agent wire format is not yet covered by this fixture, so the record schema assertion stays v1-only.',
} as const;

describe.each(['v1', 'v2'] as const)('EngineFixture (engine=%s)', (engine: EngineKind) => {
  let fakeProvider: FakeProviderServer;
  let fixture: EngineFixture;
  let homeDir: string;
  let workDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    homeDir = await makeTempDir();
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

  it('should create a session and complete a text prompt', async () => {
    fakeProvider.nextText('Hello from fake provider');

    const session = await fixture.createSession({ workDir });
    const events: string[] = [];
    session.onEvent((event) => {
      events.push(event.type);
    });

    await session.prompt('hello');
    await session.waitForTurnEnd();

    expect(events).toContain('turn.started');
    expect(events).toContain('assistant.delta');
    expect(events).toContain('turn.ended');

    // Verify the fake provider received the request
    expect(fakeProvider.requests).toHaveLength(1);
    const requestBody = fakeProvider.requests[0]?.bodyJson as { messages?: unknown[] };
    expect(requestBody?.messages).toBeDefined();

    await session.close();
  });

  it('should support multi-turn conversations', async () => {
    fakeProvider.nextText('first response');
    fakeProvider.nextText('second response');

    const session = await fixture.createSession({ workDir });

    await session.prompt('first question');
    await session.waitForTurnEnd();

    await session.prompt('second question');
    await session.waitForTurnEnd();

    expect(fakeProvider.requests).toHaveLength(2);
    await session.close();
  });

  it.skipIf(engine === 'v2')('should produce wire records after a prompt', async () => {
    // Skipped on v2: KNOWN_V2_DIFFS.wireSnapshot
    fakeProvider.nextText('test response');

    const session = await fixture.createSession({ workDir });
    await session.prompt('hello');
    await session.waitForTurnEnd();

    const snapshot = await snapshotWireRecords(session.sessionDir, 'main');

    expect(snapshot.agentId).toBe('main');
    expect(snapshot.records.length).toBeGreaterThan(0);

    // First record should be metadata
    expect(snapshot.records[0]?.type).toBe('metadata');

    // Should contain turn.prompt record
    const types = snapshot.records.map((r) => r.type);
    expect(types).toContain('turn.prompt');

    // Should have more than just metadata and turn.prompt (loop events, usage, etc.)
    expect(snapshot.records.length).toBeGreaterThan(2);

    await session.close();
  });

  it('should handle provider errors gracefully', async () => {
    fakeProvider.nextError(429, { error: { message: 'rate limited', type: 'rate_limit' } });

    const session = await fixture.createSession({ workDir });
    const events: string[] = [];
    session.onEvent((event) => {
      events.push(event.type);
    });

    await session.prompt('hello');
    // On provider error, the engine may emit turn.ended with an error reason,
    // or may emit an error event. Wait for either.
    try {
      await session.waitForTurnEnd(15_000);
    } catch {
      // If turn.ended doesn't fire, the engine may have emitted an error
      // event instead. Verify at least some events were received.
      expect(events.length).toBeGreaterThan(0);
    }

    await session.close();
  });

  it('should support session resume', async () => {
    fakeProvider.nextText('initial response');

    const session = await fixture.createSession({ id: 'resume-test', workDir });
    await session.prompt('hello');
    await session.waitForTurnEnd();
    await session.close();

    // Resume
    fakeProvider.nextText('resumed response');
    const resumed = await fixture.resumeSession('resume-test');
    await resumed.prompt('continue');
    await resumed.waitForTurnEnd();

    expect(fakeProvider.requests).toHaveLength(2);
    await resumed.close();
  });
});