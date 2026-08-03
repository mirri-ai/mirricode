/**
 * Covers: PrintModeService — print-mode (`mirri -p`) turn-completion policy.
 *
 * Tests the drain/steer/exit policies, max-turns cap, and wait-ceiling
 * enforcement without using the F4 harness (standalone unit tests).
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  type IAgentTaskService,
  type AgentTaskInfo,
} from '#/agent/task/task';
import {
  PrintModeService,
} from '#/agent/task/printModeService';
import {
  type IConfigService,
  type ConfigInspectValue,
  ConfigTarget,
} from '#/app/config/config';
import { ILogService } from '#/_base/log/log';
import { TERMINAL_STATUSES } from '#/agent/task/types';

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

/** Minimal mutable task-info shape for test assertions. */
interface MutableTaskInfo {
  taskId: string;
  description: string;
  status: 'running' | 'completed' | 'failed' | 'timed_out' | 'killed' | 'lost';
  detached: boolean;
  kind: 'process';
  command: string;
  pid: number;
  exitCode: number | null;
  startedAt: number;
  endedAt: number | null;
}

function makeTaskInfo(overrides: Partial<MutableTaskInfo> = {}): MutableTaskInfo {
  return {
    taskId: 'task-1',
    description: 'test task',
    status: 'running',
    detached: true,
    kind: 'process',
    command: 'echo hello',
    pid: 12345,
    exitCode: null,
    startedAt: Date.now(),
    endedAt: null,
    ...overrides,
  };
}

function stubTaskService(tasks: MutableTaskInfo[] = []): IAgentTaskService {
  const activeTasks = [...tasks];

  const suppressTerminalNotification = vi.fn(async (_taskId: string): Promise<void> => {
    // no-op — just track the call
  });

  const wait = vi.fn(async (taskId: string, _timeoutMs?: number): Promise<AgentTaskInfo | undefined> => {
    const task = activeTasks.find((t) => t.taskId === taskId);
    if (task === undefined) return undefined;
    // In test we resolve immediately by marking terminal
    task.status = 'completed';
    task.endedAt = Date.now();
    return task as unknown as AgentTaskInfo;
  });

  return {
    _serviceBrand: undefined,
    list(activeOnly?: boolean): readonly AgentTaskInfo[] {
      const source = activeOnly
        ? activeTasks.filter((t) => !TERMINAL_STATUSES.has(t.status))
        : activeTasks;
      return source as unknown as readonly AgentTaskInfo[];
    },
    suppressTerminalNotification,
    wait,
    // Unused stubs
    registerTask: vi.fn(),
    track: vi.fn(),
    getTask: vi.fn(),
    persistOutput: vi.fn(),
    getOutputSnapshot: vi.fn(),
    readOutput: vi.fn(),
    detach: vi.fn(),
    stop: vi.fn(),
    stopByUser: vi.fn(),
    stopAll: vi.fn(),
    stopAllOnExit: vi.fn(),
    waitForForegroundRelease: vi.fn(),
  } as unknown as IAgentTaskService;
}

/**
 * Build a stub IConfigService that returns the given task config under both
 * the `[task]` and `[background]` sections (mirroring the dual registration).
 */
function stubConfigService(taskConfig: Record<string, unknown> = {}): IConfigService {
  const sections: Record<string, Record<string, unknown>> = {
    task: { ...taskConfig },
    background: { ...taskConfig },
  };

  return {
    _serviceBrand: undefined,
    get<T>(domain: string): T | undefined {
      return sections[domain] as T | undefined;
    },
    set(_domain: string, _value: unknown, _target?: ConfigTarget): Promise<void> {
      return Promise.resolve();
    },
    inspect<T>(_domain: string): ConfigInspectValue<T> {
      return {
        value: undefined as unknown as T,
        defaultValue: undefined as unknown as T,
        userValue: undefined,
        memoryValue: undefined,
      };
    },
    replace(_domain: string, _value: unknown, _target?: ConfigTarget): Promise<void> {
      return Promise.resolve();
    },
    getRaw(_domain: string): unknown {
      return undefined;
    },
  } as unknown as IConfigService;
}

function stubLogService(): ILogService {
  return {
    _serviceBrand: undefined,
    debug: vi.fn(),
    info: vi.fn(),
    warn: vi.fn(),
    error: vi.fn(),
    fatal: vi.fn(),
    flush: vi.fn(),
    child: vi.fn(),
    level: 0,
  } as unknown as ILogService;
}

function createPrintModeService(
  taskConfig: Record<string, unknown> = {},
  tasks: MutableTaskInfo[] = [],
): { service: PrintModeService; taskService: IAgentTaskService } {
  const config = stubConfigService(taskConfig);
  const taskService = stubTaskService(tasks);
  const log = stubLogService();
  const service = new PrintModeService(config, taskService, log);
  return { service, taskService };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('PrintModeService', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  // --- exit mode -----------------------------------------------------------

  describe('should finish immediately when printBackgroundMode is exit', () => {
    it('returns finish with no background tasks', async () => {
      const { service } = createPrintModeService({ printBackgroundMode: 'exit' });
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });

    it('returns finish even with active background tasks', async () => {
      const { service } = createPrintModeService(
        { printBackgroundMode: 'exit' },
        [makeTaskInfo()],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });
  });

  // --- drain mode ----------------------------------------------------------

  describe('should drain background tasks when printBackgroundMode is drain', () => {
    it('returns finish after draining all background tasks', async () => {
      const tasks = [makeTaskInfo({ taskId: 'task-1' }), makeTaskInfo({ taskId: 'task-2' })];
      const { service } = createPrintModeService(
        { printBackgroundMode: 'drain' },
        tasks,
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });

    it('suppresses terminal notifications before waiting', async () => {
      const task1 = makeTaskInfo({ taskId: 'task-1' });
      const { service, taskService } = createPrintModeService(
        { printBackgroundMode: 'drain' },
        [task1],
      );
      await service.waitForBackgroundTasksOnPrint();
      expect(taskService.suppressTerminalNotification).toHaveBeenCalledWith('task-1');
    });

    it('returns finish immediately when no background tasks exist', async () => {
      const { service } = createPrintModeService({ printBackgroundMode: 'drain' });
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });
  });

  // --- steer mode (default) ------------------------------------------------

  describe('should steer when printBackgroundMode is steer (default)', () => {
    it('returns continue when background tasks are still active', async () => {
      const { service } = createPrintModeService(
        { printBackgroundMode: 'steer' },
        [makeTaskInfo()],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('continue');
    });

    it('returns finish when no background tasks remain', async () => {
      const { service } = createPrintModeService({ printBackgroundMode: 'steer' });
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });

    it('defaults to steer when printBackgroundMode is not configured', async () => {
      // No printBackgroundMode set — should fall back to 'steer'
      const { service } = createPrintModeService(
        {},
        [makeTaskInfo()],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('continue');
    });

    it('defaults to drain when keepAliveOnExit is true', async () => {
      const { service } = createPrintModeService(
        { keepAliveOnExit: true },
        [makeTaskInfo()],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      // drain mode: finishes after waiting
      expect(decision).toBe('finish');
    });
  });

  // --- max turns -----------------------------------------------------------

  describe('should halt at max turns limit', () => {
    it('returns finish when printSteerTurns exceeds printMaxTurns', async () => {
      const { service } = createPrintModeService(
        { printBackgroundMode: 'steer', printMaxTurns: 2 },
        [makeTaskInfo()],
      );
      // First turn: should continue (turns = 1, max = 2)
      const decision1 = await service.handlePrintMainTurnCompleted();
      expect(decision1).toBe('continue');
      // Second turn: should continue (turns = 2, max = 2)
      const decision2 = await service.handlePrintMainTurnCompleted();
      expect(decision2).toBe('continue');
      // Third turn: exceeds max (turns = 3 > 2) → finish
      const decision3 = await service.handlePrintMainTurnCompleted();
      expect(decision3).toBe('finish');
    });

    it('uses PRINT_MAX_TURNS_DEFAULT when printMaxTurns is not set', async () => {
      const { service } = createPrintModeService(
        { printBackgroundMode: 'steer', printMaxTurns: undefined },
        [makeTaskInfo()],
      );
      // Should continue well beyond a few turns
      for (let i = 0; i < 10; i++) {
        const decision = await service.handlePrintMainTurnCompleted();
        expect(decision).toBe('continue');
      }
    });
  });

  // --- wait ceiling --------------------------------------------------------

  describe('should enforce wait ceiling', () => {
    it('returns finish when wall-clock ceiling is reached', async () => {
      // Set a ceiling of 0 seconds — effectively expired immediately
      const { service } = createPrintModeService(
        { printBackgroundMode: 'steer', printWaitCeilingS: 1 },
        [makeTaskInfo()],
      );
      // First call establishes the deadline; it's 1s from now so it passes
      const decision1 = await service.handlePrintMainTurnCompleted();
      expect(decision1).toBe('continue');

      // Simulate time passing beyond the ceiling by manipulating internals
      // We access private field via type assertion
      const svc = service as unknown as { printSteerDeadline: number };
      svc.printSteerDeadline = Date.now() - 1; // already passed

      const decision2 = await service.handlePrintMainTurnCompleted();
      expect(decision2).toBe('finish');
    });
  });

  // --- reset ---------------------------------------------------------------

  describe('should reset state between print runs', () => {
    it('reset() clears steer deadline and turn counter', async () => {
      const { service } = createPrintModeService(
        { printBackgroundMode: 'steer', printMaxTurns: 2 },
        [makeTaskInfo()],
      );
      // Exhaust turn budget
      await service.handlePrintMainTurnCompleted(); // turns = 1
      await service.handlePrintMainTurnCompleted(); // turns = 2
      const decision = await service.handlePrintMainTurnCompleted(); // turns = 3 > 2 → finish
      expect(decision).toBe('finish');

      // Reset and verify turn counter is cleared
      service.reset();
      const decisionAfterReset = await service.handlePrintMainTurnCompleted();
      expect(decisionAfterReset).toBe('continue'); // turns = 1 again
    });
  });

  // --- exit after drain complete -------------------------------------------

  describe('should exit after drain completes', () => {
    it('drain mode finishes after all tasks settle', async () => {
      const task1 = makeTaskInfo({ taskId: 't1' });
      const task2 = makeTaskInfo({ taskId: 't2' });
      const { service } = createPrintModeService(
        { printBackgroundMode: 'drain' },
        [task1, task2],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });

    it('drain mode is chosen via keepAliveOnExit=true', async () => {
      const { service } = createPrintModeService(
        { keepAliveOnExit: true },
        [makeTaskInfo()],
      );
      const decision = await service.handlePrintMainTurnCompleted();
      expect(decision).toBe('finish');
    });
  });

  // --- waitForBackgroundTasksOnPrint edge cases ----------------------------

  describe('waitForBackgroundTasksOnPrint', () => {
    it('returns immediately when mode is not drain', async () => {
      const { service, taskService } = createPrintModeService(
        { printBackgroundMode: 'steer' },
        [makeTaskInfo()],
      );
      await service.waitForBackgroundTasksOnPrint();
      // suppressTerminalNotification should NOT have been called
      expect(taskService.suppressTerminalNotification).not.toHaveBeenCalled();
    });

    it('waits for newly spawned tasks within the deadline', async () => {
      const task1 = makeTaskInfo({ taskId: 't1' });
      const { service } = createPrintModeService(
        { printBackgroundMode: 'drain', printWaitCeilingS: 300 },
        [task1],
      );
      // The stub wait() marks the task as completed, so the loop
      // will see no active tasks on the second pass and exit.
      await service.waitForBackgroundTasksOnPrint();
      // No assertion needed beyond "does not hang"
      expect(service).toBeDefined();
    });
  });
});
