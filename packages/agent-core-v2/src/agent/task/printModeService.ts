/**
 * `task` domain — print-mode (`mirri -p`) turn-completion policy.
 *
 * Decides what the headless print-mode driver should do after the main agent's
 * turn ends with `reason === 'completed'`: return `'finish'` when the run may
 * exit, or `'continue'` when the driver must stay alive so a background-task
 * completion can steer the main agent into a new turn.
 *
 * The three policies map directly to v1's `Session.handlePrintMainTurnCompleted`:
 *
 * - `'exit'`  : finish immediately regardless of pending work.
 * - `'drain'` : suppress terminal notifications for every active background
 *               task, then wait for all of them to reach a terminal state,
 *               bounded by `printWaitCeilingS`. Finish once quiescent.
 * - `'steer'` : while background tasks are still pending, return `'continue'`
 *               so their completions steer new main turns; finish once
 *               quiescent, or when the wall-clock ceiling (`printWaitCeilingS`)
 *               or the turn cap (`printMaxTurns`) is reached. This is the
 *               default mode.
 *
 * Bound at Agent scope. The service is stateful: it tracks the steer deadline
 * and turn counter across calls within a single print run. Call `reset()` when
 * starting a new print run.
 */

import { createDecorator } from '#/_base/di/instantiation';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import { IConfigService } from '#/app/config/config';
import {
  PRINT_MAX_TURNS_DEFAULT,
  PRINT_WAIT_CEILING_S_DEFAULT,
} from './printDefaults';
import {
  resolveAgentTaskConfig,
  resolvePrintBackgroundMode,
} from './configSection';
import { IAgentTaskService } from './task';

export type PrintTurnDecision = 'finish' | 'continue';

export interface IPrintModeService {
  readonly _serviceBrand: undefined;

  /**
   * Reset the internal steer deadline and turn counter. Call at the start of
   * each new print run so state from a previous run does not leak.
   */
  reset(): void;

  /**
   * Decide what the print-mode driver should do after the main agent's turn
   * ends with `reason === 'completed'`.
   *
   * Returns `'finish'` when the run may exit, or `'continue'` when the
   * driver must stay alive so a background-task completion can steer the
   * main agent into a new turn.
   */
  handlePrintMainTurnCompleted(): Promise<PrintTurnDecision>;

  /**
   * Wait for all active background tasks to reach a terminal state, bounded
   * by the configured `printWaitCeilingS`. Terminal notifications are
   * suppressed so completions cannot steer new turns while we wait.
   *
   * Only meaningful when the resolved mode is `'drain'`; returns immediately
   * otherwise.
   */
  waitForBackgroundTasksOnPrint(): Promise<void>;
}

export const IPrintModeService = createDecorator<IPrintModeService>('printModeService');

export class PrintModeService implements IPrintModeService {
  declare readonly _serviceBrand: undefined;

  private printSteerDeadline: number | undefined;
  private printSteerTurns = 0;

  constructor(
    @IConfigService private readonly config: IConfigService,
    @IAgentTaskService private readonly taskService: IAgentTaskService,
    @ILogService private readonly log: ILogService,
  ) {}

  reset(): void {
    this.printSteerDeadline = undefined;
    this.printSteerTurns = 0;
  }

  async waitForBackgroundTasksOnPrint(): Promise<void> {
    const mode = resolvePrintBackgroundMode(this.config);
    if (mode !== 'drain') return;

    const taskConfig = resolveAgentTaskConfig(this.config);
    const ceilingS = taskConfig?.printWaitCeilingS ?? PRINT_WAIT_CEILING_S_DEFAULT;
    const deadline = Date.now() + ceilingS * 1000;

    const seen = new Set<string>();
    const allWaiters: Promise<unknown>[] = [];

    while (Date.now() < deadline) {
      const batch: Promise<unknown>[] = [];
      const suppressions: Promise<void>[] = [];
      let activeCount = 0;

      const activeTasks = this.taskService.list(true);
      for (const task of activeTasks) {
        activeCount++;
        if (seen.has(task.taskId)) continue;
        seen.add(task.taskId);
        suppressions.push(this.taskService.suppressTerminalNotification(task.taskId));
        const remaining = Math.max(1, deadline - Date.now());
        const waiter = this.taskService.wait(task.taskId, remaining);
        batch.push(waiter);
        allWaiters.push(waiter);
      }

      if (suppressions.length > 0) {
        await Promise.all(suppressions);
      }
      if (activeCount === 0 || batch.length === 0) break;

      this.log.info('waiting for background tasks before print exit', {
        active: activeCount,
        new: batch.length,
        ceilingS,
      });
      await Promise.all(batch);
    }

    if (allWaiters.length > 0) {
      await Promise.all(allWaiters);
      this.log.info('background tasks settled before print exit', {
        count: seen.size,
        ceilingS,
      });
    }
  }

  async handlePrintMainTurnCompleted(): Promise<PrintTurnDecision> {
    const mode = resolvePrintBackgroundMode(this.config);
    if (mode === 'exit') return 'finish';
    if (mode === 'drain') {
      await this.waitForBackgroundTasksOnPrint();
      return 'finish';
    }

    // 'steer'
    const taskConfig = resolveAgentTaskConfig(this.config);
    const ceilingS = taskConfig?.printWaitCeilingS ?? PRINT_WAIT_CEILING_S_DEFAULT;
    const maxTurns = taskConfig?.printMaxTurns ?? PRINT_MAX_TURNS_DEFAULT;
    const now = Date.now();
    this.printSteerDeadline ??= now + ceilingS * 1000;
    this.printSteerTurns += 1;

    if (now >= this.printSteerDeadline) {
      this.log.warn('print steer ceiling reached, finishing', { ceilingS });
      return 'finish';
    }
    if (this.printSteerTurns > maxTurns) {
      this.log.warn('print steer max turns reached, finishing', { maxTurns });
      return 'finish';
    }
    if (this.taskService.list(true).length > 0) {
      return 'continue';
    }
    return 'finish';
  }
}

registerScopedService(
  LifecycleScope.Agent,
  IPrintModeService,
  PrintModeService,
  ScopeActivation.OnScopeCreated,
  'task',
);
