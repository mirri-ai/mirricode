/**
 * node-sdk e2e: the P0 session-service batch on the v2 engine —
 * `reloadSession`, `getSessionWarnings`, and the print-mode policy pair
 * (`waitForBackgroundTasksOnPrint` / `handlePrintMainTurnCompleted`).
 *
 * These four used to fall through to `getRpc()`, which throws
 * `not_implemented`, so the coverage that matters is that each one now
 * reaches the v2 engine and returns its v1-shaped answer. Both engines run
 * the same assertions so the v2 wiring is checked against v1's behavior
 * rather than against itself.
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

describe.each(['v1', 'v2'] as const)(
  'node-sdk e2e: session service batch (engine=%s)',
  (engine: EngineKind) => {
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

    it('should return an empty warning list when AGENTS.md is within budget', async () => {
      await fixture.createSession({ id: 'warn-ok-ses', workDir });
      const session = await fixture.rawHarness.resumeSession({ id: 'warn-ok-ses' });

      const warnings = await session.getSessionWarnings();

      expect(Array.isArray(warnings)).toBe(true);
      expect(warnings).toEqual([]);
    });

    it('should rebuild the session and preserve history when reloading an idle session', async () => {
      fakeProvider.nextText('first answer');
      const created = await fixture.createSession({ id: 'reload-ses', workDir });
      await created.prompt('first question');
      await created.waitForTurnEnd();

      const reloaded = await fixture.rawHarness.reloadSession({ id: 'reload-ses' });

      expect(reloaded.id).toBe('reload-ses');
      // The reloaded session still carries its prior history into the model.
      fakeProvider.nextText('second answer');
      const continued = await fixture.resumeSession('reload-ses');
      await continued.prompt('second question');
      await continued.waitForTurnEnd();
      const lastBody = fakeProvider.requests.at(-1)?.bodyJson as {
        messages?: { content: string }[];
      };
      expect(JSON.stringify(lastBody?.messages ?? [])).toContain('first question');
    });

    it('should reject a reload of an unknown session id', async () => {
      await expect(
        fixture.rawHarness.reloadSession({ id: 'no-such-session' }),
      ).rejects.toThrow();
    });

    it('should finish immediately on print turn completion when no background task is pending', async () => {
      await fixture.createSession({ id: 'print-idle-ses', workDir });
      const session = await fixture.rawHarness.resumeSession({ id: 'print-idle-ses' });

      const decision = await session.handlePrintMainTurnCompleted();

      expect(decision).toBe('finish');
    });

    it('should return without waiting when draining background tasks on an idle print run', async () => {
      await fixture.createSession({ id: 'print-drain-ses', workDir });
      const session = await fixture.rawHarness.resumeSession({ id: 'print-drain-ses' });

      await expect(session.waitForBackgroundTasksOnPrint()).resolves.toBeUndefined();
    });
  },
);
