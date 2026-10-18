/**
 * `task` domain — shared task-info and snapshot formatting helpers.
 *
 * `formatPlainObject` renders a task-info record as `key: value` lines and
 * `formatTaskSnapshotOutput` assembles the snapshot return body shared by the
 * task tools (`TaskOutput` reads and `TaskWait` blocking-wait returns):
 * structured task metadata plus a fixed-size tail preview, with an append
 * point for tool-specific trailing lines (e.g. polling guidance). Kept
 * dependency-free on purpose so both tool implementations can reuse it.
 */

import { IAgentTaskService } from '#/agent/task/task';
import type {
  AgentTaskInfo,
  AgentTaskOutputSnapshot,
} from '#/agent/task/task';
import { TERMINAL_STATUSES, type AgentTaskStatus } from '#/agent/task/types';
import type { ExecutableToolResult } from '#/tool/toolContract';

export const OUTPUT_PREVIEW_BYTES = 32 * 1024;

export function retrievalStatus(status: AgentTaskStatus): 'success' | 'not_ready' {
  return TERMINAL_STATUSES.has(status) ? 'success' : 'not_ready';
}

function formatValue(value: unknown): string {
  return typeof value === 'string' ? value : String(value);
}

function fieldName(key: string): string {
  return key.replaceAll(/[A-Z]/g, (match) => `_${match.toLowerCase()}`);
}

export function formatPlainObject(record: object): string {
  return Object.entries(record)
    .filter(([, value]) => value !== undefined && value !== null)
    .map(([key, value]) => `${fieldName(key)}: ${formatValue(value)}`)
    .join('\n');
}

export interface TaskSnapshotOutputOptions {
  readonly retrievalStatus?: 'success' | 'not_ready';
  /**
   * Pre-fetched task info. When provided the helper renders from it instead
   * of reading the service again, so a caller that already fetched the task
   * (e.g. `TaskOutput`) can do a single read and pass it through — avoiding a
   * second disk read and a TOCTOU divergence between the caller's hint fields
   * and the body fields.
   */
  readonly info?: AgentTaskInfo;
  /** Pre-fetched output snapshot, passed through for the same single-fetch reason. */
  readonly snapshot?: AgentTaskOutputSnapshot;
  /**
   * Terminal classification, rendered right after `output_path` — the same
   * position the pre-refactor `TaskOutput` body used, so the rendered body
   * stays byte-identical for callers that pass it.
   */
  readonly terminalReason?: 'timed_out' | 'stopped' | 'failed';
  readonly extraFields?: Record<string, unknown>;
  readonly trailingLines?: string[];
}

export async function formatTaskSnapshotOutput(
  tasks: Pick<IAgentTaskService, 'getTask' | 'getOutputSnapshot'>,
  taskId: string,
  options: TaskSnapshotOutputOptions = {},
): Promise<ExecutableToolResult> {
  const current = options.info ?? tasks.getTask(taskId);
  if (current === undefined) {
    return { isError: true, output: `Task not found: ${taskId}` };
  }

  const output =
    options.snapshot ?? (await tasks.getOutputSnapshot(taskId, OUTPUT_PREVIEW_BYTES));

  const lines = [
    formatPlainObject({
      retrievalStatus: options.retrievalStatus ?? retrievalStatus(current.status),
      ...current,
      outputPath: output.outputPath,
      terminalReason: options.terminalReason,
      outputSizeBytes: output.outputSizeBytes,
      outputPreviewBytes: output.previewBytes,
      outputTruncated: output.truncated,
      fullOutputAvailable: output.fullOutputAvailable,
      ...options.extraFields,
    }),
    '',
  ];

  if (output.truncated) {
    lines.push(
      output.fullOutputAvailable && output.outputPath !== undefined
        ? `[Truncated. Full output: ${output.outputPath}]`
        : '[Truncated. No persisted full log is available for this task.]',
    );
  }
  lines.push('[output]', output.preview || '[no output available]');
  if (options.trailingLines !== undefined) {
    lines.push(...options.trailingLines);
  }

  return {
    output: lines.join('\n'),
    isError: false,
  };
}
