/**
 * Golden file consistency tests.
 *
 * Runs core scenarios through the v1 engine + fake provider,
 * snapshots the wire.jsonl records, and compares against
 * pre-generated golden files.
 *
 * Before a v1→v2 refactor: these tests generate/verify golden files on v1.
 * After the refactor: switch the fixture to V2EngineFixture and re-run.
 * If golden files match → behavior is equivalent.
 * If they differ → the diff pinpoints exactly what changed.
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type EngineFixture,
  type FakeProviderServer,
  createFakeProviderServer,
  createV1EngineFixture,
  makeTempDir,
  removeTempDir,
  compareWireSnapshots,
  readGoldenFile,
  writeGoldenFile,
  snapshotWireRecords,
} from '#/index';
import { join } from 'node:path';

const GOLDEN_DIR = join(new URL('.', import.meta.url).pathname, '..', 'golden');

/**
 * Give the engine's async wire-record writer time to flush pending records
 * to disk before we snapshot. The turn.ended event fires before the final
 * usage.record and step.end records are persisted.
 */
function flushWireRecords(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 200));
}

describe('Wire record golden files', () => {
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

  it('GOLD-01: single turn text response', async () => {
    fakeProvider.nextText('hello response');

    const session = await fixture.createSession({ workDir });
    await session.prompt('hello');
    await session.waitForTurnEnd();
    await flushWireRecords();

    const snapshot = await snapshotWireRecords(session.sessionDir, 'main');

    const goldenPath = join(GOLDEN_DIR, 'GOLD-01-single-turn.json');

    // Set UPDATE_GOLDEN=1 to (re)generate golden files
    if (process.env['UPDATE_GOLDEN'] === '1') {
      writeGoldenFile(goldenPath, snapshot);
      return;
    }

    const golden = await readGoldenFile(goldenPath);
    const diff = compareWireSnapshots(snapshot, golden);
    if (!diff.identical) {
      console.error('Golden file mismatch for GOLD-01:\n', JSON.stringify(diff.differences, null, 2));
    }
    expect(diff.identical).toBe(true);

    await session.close();
  });

  it('GOLD-03: multi-turn conversation', async () => {
    fakeProvider.nextText('first response');
    fakeProvider.nextText('second response');

    const session = await fixture.createSession({ workDir });
    await session.prompt('first question');
    await session.waitForTurnEnd();

    await session.prompt('second question');
    await session.waitForTurnEnd();
    await flushWireRecords();

    const snapshot = await snapshotWireRecords(session.sessionDir, 'main');

    const goldenPath = join(GOLDEN_DIR, 'GOLD-03-multi-turn.json');

    if (process.env['UPDATE_GOLDEN'] === '1') {
      writeGoldenFile(goldenPath, snapshot);
      return;
    }

    const golden = await readGoldenFile(goldenPath);
    const diff = compareWireSnapshots(snapshot, golden);
    if (!diff.identical) {
      console.error('Golden file mismatch for GOLD-03:\n', JSON.stringify(diff.differences, null, 2));
    }
    expect(diff.identical).toBe(true);

    await session.close();
  });

  it('GOLD-04: session resume preserves wire history', async () => {
    fakeProvider.nextText('initial response');

    const session = await fixture.createSession({ id: 'golden-resume', workDir });
    await session.prompt('first question');
    await session.waitForTurnEnd();
    await session.close();

    // Resume and add a second turn
    fakeProvider.nextText('follow-up response');
    const resumed = await fixture.resumeSession('golden-resume');
    await resumed.prompt('follow-up question');
    await resumed.waitForTurnEnd();
    await flushWireRecords();

    const snapshot = await snapshotWireRecords(resumed.sessionDir, 'main');

    const goldenPath = join(GOLDEN_DIR, 'GOLD-04-session-resume.json');

    if (process.env['UPDATE_GOLDEN'] === '1') {
      writeGoldenFile(goldenPath, snapshot);
      return;
    }

    const golden = await readGoldenFile(goldenPath);
    const diff = compareWireSnapshots(snapshot, golden);
    if (!diff.identical) {
      console.error('Golden file mismatch for GOLD-04:\n', JSON.stringify(diff.differences, null, 2));
    }
    expect(diff.identical).toBe(true);

    await resumed.close();
  });
});
