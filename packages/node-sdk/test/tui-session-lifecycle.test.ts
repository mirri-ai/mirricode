import { rm } from 'node:fs/promises';
import { mkdtempSync } from 'node:fs';
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
  const dir = mkdtempSync(join(tmpdir(), 'mirri-tui-'));
  tempDirs.push(dir);
  return dir;
}

describe('TUI session lifecycle (Rust engine)', () => {
  it('should create session, get context, plan, and usage when TUI starts a conversation', async () => {
    const homeDir = await makeTempDir();
    await writeRustEngineModelConfig(homeDir);
    const workDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      // No-op: the model config written above already exists (create 'wx').
      await harness.ensureConfigFile();

      const session = await harness.createSession({ workDir });
      expect(session.id).toBeDefined();
      expect(session.summary?.title).toBe('Untitled Session');

      // TUI calls getContext on session start
      const context = await session.getContext();
      expect(context).toBeDefined();

      // TUI calls getPlan to check plan state
      const plan = await session.getPlan();
      expect(plan).toBeDefined();

      // TUI calls getUsage for token tracking
      const usage = await session.getUsage();
      expect(usage).toBeDefined();

      // TUI calls getStatus for session status
      const status = await session.getStatus();
      expect(status).toBeDefined();

      await session.close();
    } finally {
      await harness.close();
    }
  });

  it('should return session warnings when TUI checks for startup warnings', async () => {
    const homeDir = await makeTempDir();
    await writeRustEngineModelConfig(homeDir);
    const workDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      // No-op: the model config written above already exists (create 'wx').
      await harness.ensureConfigFile();

      const session = await harness.createSession({ workDir });
      const warnings = await session.getSessionWarnings();
      expect(warnings).toBeDefined();

      await session.close();
    } finally {
      await harness.close();
    }
  });

  it('should return experimental features when TUI checks feature flags', async () => {
    const homeDir = await makeTempDir();
    const harness = createMirriHarnessRust({ homeDir, uiMode: 'cli' });

    try {
      await harness.ensureConfigFile();
      const features = await harness.getExperimentalFeatures();
      expect(features).toBeDefined();
    } finally {
      await harness.close();
    }
  });
});
