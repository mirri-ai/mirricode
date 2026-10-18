/**
 * `tools` domain — `TaskWaitTool` implementation (the `TaskWait` tool).
 *
 * Explicit blocking wait for one managed background task, with a duration
 * cap: the next step that truly depends on a background result can wait for
 * it instead of polling non-blocking snapshots.
 *
 * Delivery contract: while the wait is outstanding the task's automatic
 * terminal notification is suppressed, so a settled result is delivered
 * exactly once — the wait's return is the answer. When the wait times out
 * (or is interrupted) the suppression is lifted, the honest partial output
 * is returned with a "still running" marking plus end-turn guidance, and
 * the eventual completion still notifies on its own.
 *
 * Registered via the module-level `registerAgentToolService(ITaskWaitTool,
 * TaskWaitTool)` at the bottom of this file. Bound at Agent scope.
 */

import { toInputJsonSchema } from '#/tool/input-schema';
import { matchesGlobRuleSubject } from '#/tool/rule-match';
import { type ExecutableToolResult, type ToolExecution } from '#/tool/toolContract';
import { registerAgentToolService } from '#/agent/toolRegistry/toolContribution';

import { IAgentTaskService } from '#/agent/task/task';
import type { AgentTaskInfo } from '#/agent/task/task';
import { formatTaskSnapshotOutput } from '#/agent/task/tools/format';
import { TERMINAL_STATUSES } from '#/agent/task/types';
import { ITaskWaitTool, TaskWaitInputSchema, type TaskWaitInput } from './task-wait';
import TASK_WAIT_DESCRIPTION from './task-wait.md?raw';

const DEFAULT_TIMEOUT_SECONDS = 30;

const TIMEOUT_GUIDANCE =
  'The task is still running after waiting. Do not wait on it again in a row — ' +
  'either end the turn and its completion will be delivered to you automatically, ' +
  'or pick work that does not depend on this result.';

export class TaskWaitTool implements ITaskWaitTool {
  declare readonly _serviceBrand: undefined;
  readonly name = 'TaskWait' as const;
  readonly description: string = TASK_WAIT_DESCRIPTION;
  readonly parameters: Record<string, unknown> = toInputJsonSchema(TaskWaitInputSchema);

  constructor(@IAgentTaskService private readonly tasks: IAgentTaskService) {}

  resolveExecution(args: TaskWaitInput): ToolExecution {
    return {
      description: `Waiting for task ${args.task_id}`,
      approvalRule: this.name,
      matchesRule: (ruleArgs) => matchesGlobRuleSubject(ruleArgs, args.task_id),
      execute: (ctx) => this.execute(args, ctx.signal),
    };
  }

  private async execute(
    args: TaskWaitInput,
    signal: AbortSignal,
  ): Promise<ExecutableToolResult> {
    const current = this.tasks.getTask(args.task_id);
    if (!current) {
      return { isError: true, output: `Task not found: ${args.task_id}` };
    }
    if (TERMINAL_STATUSES.has(current.status)) {
      return this.renderSettled(args.task_id);
    }
    if (signal.aborted) {
      return this.renderSettled(args.task_id);
    }

    const timeoutMs = (args.timeout ?? DEFAULT_TIMEOUT_SECONDS) * 1000;
    // Suppress before waiting: settleTask fires terminal effects before
    // waking waiters, so entry-time suppression is race-free — a settle that
    // happens after this point sees the flag and stays silent, making the
    // wait's return the single delivery of the result.
    //
    // One-waiter-per-task: concurrent TaskWait calls on the SAME task are not
    // refcounted — a timeout on one lifts the suppression while the other
    // still waits, so the second waiter's return and the eventual notification
    // can both deliver. Keep two waiters off the same task.
    let settled: AgentTaskInfo | undefined;
    let delivered = false;
    let suppressionLifted = false;
    try {
      // Safety net: any exit that is NOT the settled return below (a failing
      // suppress that already set the flag, an aborted wait, ...) restores
      // automatic notification. That guarantee is delivery-honest only before
      // settlement: once the task has settled under suppression, the
      // suppressed notification has passed its one delivery point and
      // unsuppression cannot resurrect it — the wait's return IS the only
      // delivery, so a render failure after settlement loses it (accepted:
      // that path is disk-error-only).
      await this.tasks.suppressTerminalNotification(args.task_id);

      try {
        settled = await this.tasks.wait(args.task_id, timeoutMs, signal);
      } catch (error) {
        // Interrupted (user cancellation): lift the suppression so the eventual
        // completion still notifies.
        suppressionLifted = true;
        await this.tasks.unsuppressTerminalNotification(args.task_id);
        throw error;
      }

      if (settled !== undefined && TERMINAL_STATUSES.has(settled.status)) {
        const result = await this.renderSettled(args.task_id, { waitTimeoutMs: timeoutMs });
        delivered = true;
        return result;
      }

      // Timed out with the task still running: lift the suppression — the
      // completion must still arrive as its own notification later.
      suppressionLifted = true;
      await this.tasks.unsuppressTerminalNotification(args.task_id);
      return this.renderTimedOut(args.task_id, settled, timeoutMs);
    } catch (error) {
      if (!delivered && !suppressionLifted) {
        await this.tasks.unsuppressTerminalNotification(args.task_id);
      }
      throw error;
    }
  }

  private renderSettled(
    taskId: string,
    extra?: { waitTimeoutMs: number },
  ): Promise<ExecutableToolResult> {
    return formatTaskSnapshotOutput(this.tasks, taskId, {
      retrievalStatus: 'success',
      extraFields: extra === undefined ? undefined : { waitTimeoutMs: extra.waitTimeoutMs },
    });
  }

  private async renderTimedOut(
    taskId: string,
    info: AgentTaskInfo | undefined,
    waitTimeoutMs: number,
  ): Promise<ExecutableToolResult> {
    // Render from the info `wait()` returned instead of re-fetching: a task
    // that settles right after the timeout would otherwise render
    // `status: completed` under `retrieval_status: not_ready` with "still
    // running" guidance. The output snapshot is still fetched for the preview.
    return formatTaskSnapshotOutput(this.tasks, taskId, {
      retrievalStatus: 'not_ready',
      info,
      extraFields: { waitTimedOut: true, waitTimeoutMs },
      trailingLines: [TIMEOUT_GUIDANCE],
    });
  }
}

registerAgentToolService(ITaskWaitTool, TaskWaitTool, { name: 'TaskWait', domain: 'agentTask', description: 'Wait for a background task to settle, with an honest partial-output return on timeout.' });
