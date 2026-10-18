import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { createMirriHarnessRust } from '#/index';
import { writeRustEngineModelConfig } from './rust-engine-model-config';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

async function makeTempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-rust-'));
  tempDirs.push(dir);
  return dir;
}

describe('createMirriHarnessRust', () => {
  it('should create a harness and return config when engine binding is available', async () => {
    const homeDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      const config = await harness.getConfig();
      expect(config).toBeDefined();
      expect(typeof config).toBe('object');
    } finally {
      await harness.close();
    }
  });

  it('should create and list sessions when harness is initialized', async () => {
    const homeDir = await makeTempDir();
    await writeRustEngineModelConfig(homeDir);
    const workDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      const before = await harness.listSessions();
      expect(before).toHaveLength(0);

      const session = await harness.createSession({ workDir });
      expect(session.summary?.title).toBe('Untitled Session');

      const after = await harness.listSessions();
      expect(after).toHaveLength(1);

      await session.close();
      // SDR-004 (W5): this session never conversed — it was memory-only,
      // so closing it leaves nothing persisted and the list answers empty.
      // A conversed session still persists through close (the TUI's
      // `mirri -r` / "continue last session" flow lists it after a first
      // user input).
      const closed = await harness.listSessions();
      expect(closed).toHaveLength(0);
    } finally {
      await harness.close();
    }
  });

  it('should return the default title when created without a title', async () => {
    const homeDir = await makeTempDir();
    await writeRustEngineModelConfig(homeDir);
    const workDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      // The TS v2 `CreateSessionPayload` has no `title` (the embedded
      // engine defaults it server-side); title round-trips go through
      // `session.rename` instead.
      const session = await harness.createSession({ workDir });
      expect(session.summary?.title).toBe('Untitled Session');
      await session.close();
    } finally {
      await harness.close();
    }
  });
});
