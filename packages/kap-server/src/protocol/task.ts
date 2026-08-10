import { z } from 'zod';

import { isoDateTimeSchema } from '@mirri-ai/agent-core-v2/_base/utils/isoDateTime';

export const taskKindSchema = z.enum(['subagent', 'bash', 'tool']);
export type TaskKind = z.infer<typeof taskKindSchema>;

export const taskStatusSchema = z.enum([
  'running',
  'completed',
  'failed',
  'cancelled',
]);
export type TaskStatus = z.infer<typeof taskStatusSchema>;

export const taskSchema = z.object({
  id: z.string().min(1),
  session_id: z.string().min(1),
  kind: taskKindSchema,
  description: z.string(),
  status: taskStatusSchema,
  command: z.string().optional(),
  /** Resolved model alias the subagent is using (subagent-kind tasks only). */
  model: z.string().optional(),
  /** The subagent's agent id (subagent-kind tasks only). Links this REST task
   *  row (keyed by background-task id) to the WS `subagent.*` row (keyed by
   *  agent id) so completion events can terminate both. */
  agent_id: z.string().optional(),
  created_at: isoDateTimeSchema,
  started_at: isoDateTimeSchema.optional(),
  completed_at: isoDateTimeSchema.optional(),
  output_preview: z.string().optional(),
  output_bytes: z.number().int().nonnegative().optional(),
});
export type Task = z.infer<typeof taskSchema>;
