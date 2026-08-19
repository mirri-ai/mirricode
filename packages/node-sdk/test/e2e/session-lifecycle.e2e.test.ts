/**
 * node-sdk e2e: session lifecycle — create, resume, fork.
 *
 * Runs against both engines through `createEngineFixture(options, engine)`.
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

describe.each(['v1', 'v2'] as const)('node-sdk e2e: session lifecycle (engine=%s)', (engine: EngineKind) => {
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

  it('should resume a session and preserve conversation history', async () => {
    fakeProvider.nextText('initial response');

    const session = await fixture.createSession({ id: 'resume-test-ses', workDir });
    await session.prompt('first question');
    await session.waitForTurnEnd();
    await session.close();

    // Resume and continue
    fakeProvider.nextText('follow-up response');
    const resumed = await fixture.resumeSession('resume-test-ses');
    await resumed.prompt('follow-up question');
    await resumed.waitForTurnEnd();

    // The second LLM call should include history from the first turn
    expect(fakeProvider.requests).toHaveLength(2);
    const secondBody = fakeProvider.requests[1]?.bodyJson as { messages?: { content: string }[] };
    const allContent = JSON.stringify(secondBody?.messages ?? []);
    expect(allContent).toContain('first question');

    await resumed.close();
  });

  it('should list sessions', async () => {
    fakeProvider.nextText('response');

    const session = await fixture.createSession({ id: 'list-test-ses', workDir });
    await session.prompt('hello');
    await session.waitForTurnEnd();
    await session.close();

    const sessions = await fixture.listSessions();
    expect(sessions.length).toBeGreaterThanOrEqual(1);
    expect(sessions.some((s) => s.id === 'list-test-ses')).toBe(true);
  });

  it('should fork a session with independent history', async () => {
    fakeProvider.nextText('original response');

    const session = await fixture.createSession({ id: 'fork-test-ses', workDir });
    await session.prompt('original question');
    await session.waitForTurnEnd();
    await session.close();

    // Fork
    fakeProvider.nextText('forked response');
    const forked = await fixture.rawHarness.forkSession({ id: 'fork-test-ses' });
    await forked.prompt('forked question');
    await new Promise((resolve) => {
      const unsub = forked.onEvent((event) => {
        if (event.type === 'turn.ended') {
          resolve(undefined);
          unsub();
        }
      });
    });

    expect(forked.id).not.toBe('fork-test-ses');
    expect(fakeProvider.requests).toHaveLength(2);

    // Forked session's LLM call should contain the original history
    const forkBody = fakeProvider.requests[1]?.bodyJson as { messages?: { content: string }[] };
    const allContent = JSON.stringify(forkBody?.messages ?? []);
    expect(allContent).toContain('original question');

    await forked.close();
  });
});