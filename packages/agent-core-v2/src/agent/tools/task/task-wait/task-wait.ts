/**
 * `tools` domain — `ITaskWaitTool` contract (the `TaskWait` tool).
 *
 * Public contract of the `TaskWait` tool (block until a managed background
 * task settles, with an honest partial-output return on timeout): the input
 * zod schema the model-facing parameters are derived from and the
 * `ITaskWaitTool` DI decorator. Bound at Agent scope.
 */

import { z } from 'zod';

import { createDecorator } from '#/_base/di/instantiation';
import { type AgentTool } from '#/tool/toolContract';

export const TaskWaitInputSchema = z.object({
  task_id: z
    .string()
    .describe('The background task ID to wait for.'),
  timeout: z
    .number()
    .int()
    .min(0)
    .max(3600)
    .optional()
    .describe(
      'Maximum seconds to wait for the task to settle. ' +
        'Discouraged — background tasks notify automatically on completion; ' +
        'wait only when the next step truly depends on this result and there is ' +
        'no other useful work to do. Default 30.',
    ),
});

export type TaskWaitInput = z.infer<typeof TaskWaitInputSchema>;

export interface ITaskWaitTool extends AgentTool<TaskWaitInput> { readonly _serviceBrand: undefined }
export const ITaskWaitTool = createDecorator<ITaskWaitTool>('taskWaitTool');
