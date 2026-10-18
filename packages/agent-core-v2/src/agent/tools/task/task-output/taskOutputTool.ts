/**
 * `tools` domain — `TaskOutputTool` implementation (the `TaskOutput`
 * tool).
 *
 * Returns structured task metadata plus a fixed-size tail preview of the
 * task's output, read through `IAgentTaskService` (`agentTask` domain). The
 * snapshot body itself is assembled by the shared
 * `formatTaskSnapshotOutput` helper (`#/agent/task/tools/format`); this tool
 * adds its own terminal metadata (`terminal_reason`, full-output hints) and —
 * for running tasks — a not-ready line telling the model the completion will
 * be delivered automatically instead of polling.
 *
 * The full, never-truncated output lives on disk at `output_path`; the caller
 * is always pointed at the `Read` tool to page through the complete log, and
 * the preview also carries a banner when it has been truncated to a tail.
 *
 * For terminal tasks the output also surfaces why the task ended:
 * `stop_reason` records the concrete reason; `terminal_reason` classifies
 * timeout vs. explicit stop vs. failure for callers that need stable labels.
 *
 * Registered via the module-level `registerAgentToolService(ITaskOutputTool,
 * TaskOutputTool)` at the bottom of this file — the same "import = register"
 * pattern used by every agent tool. Bound at Agent scope.
 */

import { toInputJsonSchema } from '#/tool/input-schema';
import { matchesGlobRuleSubject } from '#/tool/rule-match';
import { type ExecutableToolResult, type ToolExecution } from '#/tool/toolContract';
import { registerAgentToolService } from '#/agent/toolRegistry/toolContribution';

import { IAgentTaskService } from '#/agent/task/task';
import type {
  AgentTaskInfo,
  AgentTaskOutputSnapshot,
} from '#/agent/task/task';
import {
  OUTPUT_PREVIEW_BYTES,
  formatTaskSnapshotOutput,
  retrievalStatus,
} from '#/agent/task/tools/format';
import { ITaskOutputTool, TaskOutputInputSchema, type TaskOutputInput } from './task-output';
import TASK_OUTPUT_DESCRIPTION from './task-output.md?raw';

const PAGING_HINT_LINES = 300;

const NOT_READY_GUIDANCE =
  '[note] The task is still running. Its completion will be delivered to you ' +
  'automatically as a message \u2014 do not keep polling for it.';

function terminalReason(info: AgentTaskInfo): 'timed_out' | 'stopped' | 'failed' | undefined {
  if (info.status === 'timed_out') return 'timed_out';
  if (info.status === 'killed' && info.stopReason !== undefined) return 'stopped';
  if (info.status === 'failed' && info.stopReason !== undefined) return 'failed';
  return undefined;
}

function fullOutputHint(output: AgentTaskOutputSnapshot): string | undefined {
  if (!output.fullOutputAvailable || output.outputPath === undefined) return undefined;
  if (output.truncated) {
    return (
      `Only the last ${String(OUTPUT_PREVIEW_BYTES)} bytes are shown above. ` +
      'Use the Read tool with the output_path to page through the full log ' +
      `(parameters: path, line_offset, n_lines; read about ${String(PAGING_HINT_LINES)} ` +
      'lines per page).'
    );
  }
  return (
    'The preview above is the complete output. Use the Read tool with the output_path ' +
    'if you need to re-read the full log later ' +
    `(parameters: path, line_offset, n_lines; read about ${String(PAGING_HINT_LINES)} ` +
    'lines per page).'
  );
}

export class TaskOutputTool implements ITaskOutputTool {
  declare readonly _serviceBrand: undefined;
  readonly name = 'TaskOutput' as const;
  readonly description: string = TASK_OUTPUT_DESCRIPTION;
  readonly parameters: Record<string, unknown> = toInputJsonSchema(TaskOutputInputSchema);

  constructor(@IAgentTaskService private readonly tasks: IAgentTaskService) {}

  resolveExecution(args: TaskOutputInput): ToolExecution {
    return {
      description: `Reading output of task ${args.task_id}`,
      approvalRule: this.name,
      matchesRule: (ruleArgs) => matchesGlobRuleSubject(ruleArgs, args.task_id),
      execute: () => this.execute(args),
    };
  }

  private async execute(args: TaskOutputInput): Promise<ExecutableToolResult> {
    const current = this.tasks.getTask(args.task_id);
    if (!current) {
      return { isError: true, output: `Task not found: ${args.task_id}` };
    }

    const output = await this.tasks.getOutputSnapshot(args.task_id, OUTPUT_PREVIEW_BYTES);

    // Single fetch: the task info and output snapshot read above are passed
    // straight through to the shared body formatter (via `info`/`snapshot`),
    // so the rendered fields — including the full-output hints computed from
    // `output` — all come from the same read.
    return formatTaskSnapshotOutput(this.tasks, args.task_id, {
      info: current,
      snapshot: output,
      terminalReason: terminalReason(current),
      extraFields: {
        fullOutputTool:
          output.fullOutputAvailable && output.outputPath !== undefined ? 'Read' : undefined,
        fullOutputHint: fullOutputHint(output),
      },
      trailingLines:
        retrievalStatus(current.status) === 'not_ready' ? [NOT_READY_GUIDANCE] : undefined,
    });
  }
}

registerAgentToolService(ITaskOutputTool, TaskOutputTool, { name: 'TaskOutput', domain: 'agentTask', description: 'Retrieve a snapshot of a running or completed background task.' });
