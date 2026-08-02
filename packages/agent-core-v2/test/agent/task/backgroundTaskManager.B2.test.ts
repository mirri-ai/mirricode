/**
 * B2-L4: Background Task Manager — persistence across resume,
 * detach/stop/readOutput, and print-mode drain policies.
 *
 * Standalone unit tests that cover the gaps between existing
 * taskManager.test.ts / reconcile.test.ts / persist.test.ts and the
 * B2-L4 requirements:
 *
 *   1. Detached task persists across session resume
 *   2. stopByUser sets killed with user-cancellation reason
 *   3. readOutput returns captured output (including persisted on resume)
 *   4. Print-mode drain policies (resolvePrintBackgroundMode,
 *      applyPrintModeConfigDefaults)
 */

import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { Readable } from 'node:stream';
import type { Writable } from 'node:stream';
import { join } from 'pathe';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore } from '#/_base/di/lifecycle';
import { TestInstantiationService } from '#/_base/di/test';
import { ILogService } from '#/_base/log/log';
import { IAgentTaskService, type AgentTaskInfo } from '#/agent/task/task';
import { ProcessTask } from '#/agent/tools/os/bash/process-task';
import { userCancellationReason } from '#/_base/utils/abort';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import {
  ConfigTarget,
  IConfigRegistry,
  IConfigService,
} from '#/app/config/config';
import { ConfigRegistry, ConfigService } from '#/app/config/configService';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IAtomicTomlDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { TomlAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { InMemoryStorageService } from '#/persistence/backends/memory/inMemoryStorageService';
import {
  resolveAgentTaskConfig,
  resolvePrintBackgroundMode,
} from '#/agent/task/configSection';
import { applyPrintModeConfigDefaults } from '#/agent/task/printDefaults';
import '#/agent/loop/configSection';
import '#/session/subagent/configSection';
import { LOOP_CONTROL_SECTION } from '#/agent/loop/configSection';
import { resolveSubagentTimeoutMs, SUBAGENT_SECTION } from '#/session/subagent/configSection';
import {
  taskServices,
  configServices,
  createTestAgent,
  homeDirServices,
  type TestAgentContext,
} from '../../harness';
import { stubLog } from '../../_base/log/stubs';
import { stubBootstrap } from '../../app/bootstrap/stubs';
import {
  TASK_TEST_AGENT_SCOPE,
  createAgentTaskPersistence,
  type TaskServiceTestManager,
} from './stubs';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type IProcess = import('#/session/process/processRunner').IProcess;

function immediateProcess(exitCode: number, stdoutText = ''): IProcess {
  return {
    stdin: { write: vi.fn(), end: vi.fn() } as unknown as Writable,
    stdout: Readable.from(stdoutText ? [stdoutText] : []),
    stderr: Readable.from([]),
    pid: 10000 + exitCode,
    exitCode,
    wait: vi.fn().mockResolvedValue(exitCode) as IProcess['wait'],
    kill: vi.fn().mockResolvedValue(undefined) as IProcess['kill'],
    dispose: vi.fn().mockResolvedValue(undefined) as IProcess['dispose'],
  };
}

function pendingProcess(exitOnKill = 143): {
  proc: IProcess;
  killSpy: ReturnType<typeof vi.fn>;
} {
  let resolveWait: (n: number) => void = () => {};
  const waitPromise = new Promise<number>((resolve) => {
    resolveWait = resolve;
  });
  let currentExitCode: number | null = null;
  const killSpy = vi.fn(async () => {
    if (currentExitCode !== null) return;
    currentExitCode = exitOnKill;
    resolveWait(exitOnKill);
  });
  const proc: IProcess = {
    stdin: { write: vi.fn(), end: vi.fn() } as unknown as Writable,
    stdout: Readable.from([]),
    stderr: Readable.from([]),
    pid: 54321,
    get exitCode(): number | null {
      return currentExitCode;
    },
    wait: () => waitPromise,
    kill: killSpy as unknown as IProcess['kill'],
    dispose: vi.fn().mockResolvedValue(undefined) as IProcess['dispose'],
  };
  return { proc, killSpy };
}

function registerProcess(
  manager: IAgentTaskService,
  proc: IProcess,
  command: string,
  description: string,
): string {
  return manager.registerTask(new ProcessTask(proc, command, description));
}

async function waitForTerminal(
  manager: IAgentTaskService,
  taskId: string,
  timeoutMs = 10_000,
): Promise<AgentTaskInfo | undefined> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const info = await manager.wait(taskId, 5);
    if (
      info?.status === 'completed' ||
      info?.status === 'failed' ||
      info?.status === 'timed_out' ||
      info?.status === 'killed' ||
      info?.status === 'lost'
    ) {
      return info;
    }
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  return manager.getTask(taskId);
}

function createService(options: {
  sessionDir?: string;
  maxRunningTasks?: number;
} = {}): {
  ctx: TestAgentContext;
  manager: TaskServiceTestManager;
  persistence?: ReturnType<typeof createAgentTaskPersistence>;
} {
  const persistence =
    options.sessionDir === undefined
      ? undefined
      : createAgentTaskPersistence(options.sessionDir);
  const overrides: import('../../harness').TestAgentServiceOverride[] = [];
  if (options.sessionDir !== undefined) {
    overrides.push(homeDirServices(options.sessionDir));
  }
  if (options.maxRunningTasks !== undefined) {
    overrides.push(configServices(() => ({
      providers: {},
      task: { maxRunningTasks: options.maxRunningTasks },
    })));
  }
  const ctx = createTestAgent(...overrides);
  return {
    ctx,
    manager: ctx.get(IAgentTaskService) as TaskServiceTestManager,
    persistence,
  };
}

/**
 * Creates a real ConfigService with in-memory TOML storage, matching the
 * pattern used in config.test.ts. Returns the config service and a
 * disposable store that must be disposed after the test.
 */
async function createConfigService(
  env: Record<string, string> = {},
  toml?: string,
): Promise<{ config: IConfigService; disposables: DisposableStore }> {
  const disposables = new DisposableStore();
  const ix = disposables.add(new TestInstantiationService());
  const storage = new InMemoryStorageService();
  if (toml !== undefined) {
    await storage.write('', 'config.toml', new TextEncoder().encode(toml));
  }
  ix.stub(ILogService, stubLog());
  ix.stub(IBootstrapService, stubBootstrap('/tmp/mirri-cfg', env));
  ix.stub(IFileSystemStorageService, storage);
  ix.set(IAtomicTomlDocumentStore, new SyncDescriptor(TomlAtomicDocumentStore));
  ix.set(IConfigRegistry, new SyncDescriptor(ConfigRegistry));
  ix.set(IConfigService, new SyncDescriptor(ConfigService));
  const config = ix.get(IConfigService);
  await config.ready;
  return { config, disposables };
}

// ===========================================================================
// 1. Detached task persists across session resume
// ===========================================================================

describe('B2-L4: detached task persists across session resume', () => {
  let sessionDir: string;

  beforeEach(async () => {
    sessionDir = await mkdtemp(join(tmpdir(), 'mirri-b2-resume-'));
  });

  afterEach(async () => {
    await rm(sessionDir, { recursive: true, force: true });
  });

  it('should persist a detached running task so a new service instance can load it', async () => {
    // --- First session: register a detached task that stays running ---
    const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
    const { proc: proc1 } = pendingProcess();
    const taskId = registerProcess(mgr1, proc1, 'sleep 999', 'long-lived detached task');

    // The task should be running in the first instance
    expect(mgr1.getTask(taskId)).toMatchObject({
      taskId,
      kind: 'process',
      status: 'running',
      detached: true,
    });

    // Wait for the task to be persisted to disk before disposing the session
    const persistence1 = createAgentTaskPersistence(sessionDir);
    await vi.waitFor(async () => {
      const persisted = await persistence1.readTask(taskId);
      expect(persisted).toBeDefined();
    }, { timeout: 5_000, interval: 10 });

    // Dispose the first service (simulates session close)
    await ctx1.dispose();

    // --- Second session: load from disk, reconcile ---
    const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });

    await mgr2.loadFromDisk();
    await mgr2.reconcile();

    // The task should be loaded as 'lost' (was running, no live process to resume)
    expect(mgr2.getTask(taskId)).toMatchObject({
      taskId,
      kind: 'process',
      status: 'lost',
      command: 'sleep 999',
      description: 'long-lived detached task',
    });

    // The persistence layer should also reflect the lost status
    const persistence = createAgentTaskPersistence(sessionDir);
    const persisted = await persistence.readTask(taskId);
    expect(persisted).toMatchObject({
      taskId,
      status: 'lost',
    });

    await ctx2.dispose();
  });

  it('should persist a completed task readable on resume', async () => {
    const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
    const taskId = registerProcess(
      mgr1,
      immediateProcess(0, 'build output\n'),
      'npm run build',
      'build task',
    );

    const info = await waitForTerminal(mgr1, taskId);
    expect(info).toMatchObject({ status: 'completed' });
    await ctx1.dispose();

    // --- Second session ---
    const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });
    await mgr2.loadFromDisk();
    await mgr2.reconcile();

    expect(mgr2.getTask(taskId)).toMatchObject({
      taskId,
      kind: 'process',
      status: 'completed',
      exitCode: 0,
    });

    await ctx2.dispose();
  });

  it('should persist output so readOutput returns it after resume', async () => {
    const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
    const taskId = registerProcess(
      mgr1,
      immediateProcess(0, 'hello from process\n'),
      'echo hello',
      'output persist test',
    );

    await waitForTerminal(mgr1, taskId);

    // Output should be readable in the first session
    const output1 = await mgr1.readOutput(taskId);
    expect(output1).toContain('hello from process');

    await ctx1.dispose();

    // --- Second session: output should be available from disk ---
    const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });
    await mgr2.loadFromDisk();
    await mgr2.reconcile();

    // Even though the task is a ghost, persisted output is still readable
    const snapshot = await mgr2.getOutputSnapshot(taskId, 1024);
    expect(snapshot.fullOutputAvailable).toBe(true);
    expect(snapshot.preview).toContain('hello from process');

    await ctx2.dispose();
  });

  it('should preserve stopReason across resume for a killed task', async () => {
    const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
    const { proc, killSpy } = pendingProcess();
    const taskId = registerProcess(mgr1, proc, 'sleep 999', 'kill then resume');

    await mgr1.stop(taskId, 'session ending');
    expect(killSpy).toHaveBeenCalled();

    const info1 = await waitForTerminal(mgr1, taskId);
    expect(info1).toMatchObject({
      status: 'killed',
      stopReason: 'session ending',
    });

    await ctx1.dispose();

    // --- Second session ---
    const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });
    await mgr2.loadFromDisk();

    expect(mgr2.getTask(taskId)).toMatchObject({
      taskId,
      status: 'killed',
      stopReason: 'session ending',
    });

    await ctx2.dispose();
  });
});

// ===========================================================================
// 2. stopByUser
// ===========================================================================

describe('B2-L4: stopByUser', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('should set status to killed with user-cancellation stop reason', async () => {
    const { ctx, manager } = createService();
    const { proc, killSpy } = pendingProcess();
    const taskId = registerProcess(manager, proc, 'sleep 999', 'stopByUser test');

    const result = await manager.stopByUser(taskId);

    expect(result).toMatchObject({
      taskId,
      status: 'killed',
    });
    // The stop reason should match the user-cancellation message
    const expectedMessage = userCancellationReason().message;
    expect(result?.stopReason).toBe(expectedMessage);
    expect(killSpy).toHaveBeenCalledWith('SIGTERM');

    await ctx.dispose();
  });

  it('should return undefined for an unknown task id', async () => {
    const { ctx, manager } = createService();

    expect(await manager.stopByUser('bash-nonexist')).toBeUndefined();

    await ctx.dispose();
  });

  it('should return the current info for an already-terminal task', async () => {
    const { ctx, manager } = createService();
    const taskId = registerProcess(
      manager,
      immediateProcess(0),
      'echo done',
      'already completed',
    );

    await waitForTerminal(manager, taskId);

    const result = await manager.stopByUser(taskId);
    expect(result).toMatchObject({ status: 'completed' });

    await ctx.dispose();
  });

  it('should persist the user-stopped task so the reason survives resume', async () => {
    const sessionDir = await mkdtemp(join(tmpdir(), 'mirri-b2-stopuser-'));
    try {
      const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
      const { proc } = pendingProcess();
      const taskId = registerProcess(mgr1, proc, 'sleep 999', 'stopByUser persist');

      await mgr1.stopByUser(taskId);
      await ctx1.dispose();

      const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });
      await mgr2.loadFromDisk();

      expect(mgr2.getTask(taskId)).toMatchObject({
        status: 'killed',
      });
      // The reason should be the user-cancellation message
      const stopReason = mgr2.getTask(taskId)?.stopReason;
      expect(stopReason).toBe(userCancellationReason().message);

      await ctx2.dispose();
    } finally {
      await rm(sessionDir, { recursive: true, force: true });
    }
  });
});

// ===========================================================================
// 3. readOutput returns captured output
// ===========================================================================

describe('B2-L4: readOutput returns captured output', () => {
  it('should return full output from a completed process task', async () => {
    const { ctx, manager } = createService();
    const taskId = registerProcess(
      manager,
      immediateProcess(0, 'line1\nline2\nline3\n'),
      'echo three lines',
      'readOutput test',
    );

    await waitForTerminal(manager, taskId);

    const output = await manager.readOutput(taskId);
    expect(output).toContain('line1');
    expect(output).toContain('line2');
    expect(output).toContain('line3');

    await ctx.dispose();
  });

  it('should return a tail slice when the tail parameter is provided', async () => {
    const { ctx, manager } = createService();
    const longOutput = 'a'.repeat(500) + 'TAIL_END';
    const taskId = registerProcess(
      manager,
      immediateProcess(0, longOutput),
      'echo long',
      'tail test',
    );

    await waitForTerminal(manager, taskId);

    const full = await manager.readOutput(taskId);
    expect(full).toContain('TAIL_END');

    const tail = await manager.readOutput(taskId, 8);
    expect(tail).toBe('TAIL_END');

    await ctx.dispose();
  });

  it('should return empty string for an unknown task', async () => {
    const { ctx, manager } = createService();
    expect(await manager.readOutput('bash-nonexist')).toBe('');
    await ctx.dispose();
  });

  it('should read persisted output after session resume', async () => {
    const sessionDir = await mkdtemp(join(tmpdir(), 'mirri-b2-readoutput-'));
    try {
      // First session: create a task that completes with output
      const { ctx: ctx1, manager: mgr1 } = createService({ sessionDir });
      const taskId = registerProcess(
        mgr1,
        immediateProcess(0, 'persisted output here\n'),
        'echo persist',
        'output persist',
      );

      await waitForTerminal(mgr1, taskId);
      // Ensure the output is flushed to persistence
      const output1 = await mgr1.readOutput(taskId);
      expect(output1).toContain('persisted output here');

      await ctx1.dispose();

      // Second session: read output from disk
      const { ctx: ctx2, manager: mgr2 } = createService({ sessionDir });
      await mgr2.loadFromDisk();
      await mgr2.reconcile();

      const snapshot = await mgr2.getOutputSnapshot(taskId, 4096);
      expect(snapshot.preview).toContain('persisted output here');
      expect(snapshot.fullOutputAvailable).toBe(true);

      await ctx2.dispose();
    } finally {
      await rm(sessionDir, { recursive: true, force: true });
    }
  });
});

// ===========================================================================
// 4. Print-mode drain policies
// ===========================================================================

describe('B2-L4: print-mode drain policies', () => {
  it('should resolve printBackgroundMode as "steer" by default (keepAliveOnExit unset)', async () => {
    const { config, disposables } = await createConfigService();

    expect(resolvePrintBackgroundMode(config)).toBe('steer');

    disposables.dispose();
  });

  it('should resolve printBackgroundMode as "drain" when keepAliveOnExit is true', async () => {
    const { config, disposables } = await createConfigService(
      {},
      '[task]\nkeep_alive_on_exit = true\n',
    );

    expect(resolvePrintBackgroundMode(config)).toBe('drain');

    disposables.dispose();
  });

  it('should prefer an explicit printBackgroundMode over keepAliveOnExit', async () => {
    const { config, disposables } = await createConfigService(
      {},
      '[task]\nkeep_alive_on_exit = true\nprint_background_mode = "exit"\n',
    );

    // Explicit mode wins over keepAliveOnExit inference
    expect(resolvePrintBackgroundMode(config)).toBe('exit');

    disposables.dispose();
  });

  it('should fill unset config keys with effectively unbounded values via applyPrintModeConfigDefaults', async () => {
    const { config, disposables } = await createConfigService();

    // Before applying defaults, bashTaskTimeoutS is unset
    expect(resolveAgentTaskConfig(config)?.bashTaskTimeoutS).toBeUndefined();

    await applyPrintModeConfigDefaults(config);

    // After applying defaults, bashTaskTimeoutS should be 0 (effectively unlimited)
    expect(resolveAgentTaskConfig(config)?.bashTaskTimeoutS).toBe(0);

    disposables.dispose();
  });

  it('should not override user-configured bashTaskTimeoutS', async () => {
    const { config, disposables } = await createConfigService(
      {},
      '[task]\nbash_task_timeout_s = 30\n',
    );

    await applyPrintModeConfigDefaults(config);

    // The user value should be preserved
    expect(resolveAgentTaskConfig(config)?.bashTaskTimeoutS).toBe(30);

    disposables.dispose();
  });

  it('should fill subagent timeoutMs to 0 (unlimited) when unset', async () => {
    const { config, disposables } = await createConfigService();

    await applyPrintModeConfigDefaults(config);

    // Subagent timeoutMs should be 0 after print defaults
    expect(resolveSubagentTimeoutMs(config)).toBe(0);

    disposables.dispose();
  });

  it('should fill maxStepsPerTurn to 0 (unlimited) when unset', async () => {
    const { config, disposables } = await createConfigService();

    await applyPrintModeConfigDefaults(config);

    const loopControl = config.get<{ maxStepsPerTurn?: number }>(LOOP_CONTROL_SECTION);
    expect(loopControl?.maxStepsPerTurn).toBe(0);

    disposables.dispose();
  });

  it('should respect legacy background section for bashTaskTimeoutS when user set it there', async () => {
    const { config, disposables } = await createConfigService(
      {},
      '[background]\nbash_task_timeout_s = 15\n',
    );

    await applyPrintModeConfigDefaults(config);

    // The legacy value should be preserved (not overwritten by default of 0)
    expect(resolveAgentTaskConfig(config)?.bashTaskTimeoutS).toBe(15);

    disposables.dispose();
  });
});
