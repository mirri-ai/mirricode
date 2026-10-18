/**
 *   POST /v1/sessions/{sid}/prompts
 *     Body:  PromptSubmission { content, metadata?, agent_id?, profile?, model?, thinking?,
 *              permission_mode?, plan_mode?, swarm_mode?, goal_objective?, goal_control?,
 *              disabled_tools? }
 *     Reply: PromptSubmitResult { prompt_id, user_message_id, status, content, created_at,
 *              pending_commands?, skipped_references? }   (US3.6 inline refs)
 *
 *   GET /v1/sessions/{sid}/prompts
 *     Reply: { active: PromptItem | null, queued: PromptItem[] }
 *
 *   POST /v1/sessions/{sid}/prompts/{pid}:steer
 *   POST /v1/sessions/{sid}/prompts:steer
 *     Reply: { steered: true, prompt_ids: string[] }
 *
 *   POST /v1/sessions/{sid}/prompts/{pid}:abort
 *     Reply: { aborted: true, at_seq: number }   (envelope code 0)
 *            { aborted: false, at_seq: number }  (envelope code 40903, idempotent)
 */

import { z } from 'zod';

import { isoDateTimeSchema } from '@mirri-ai/agent-core-v2/_base/utils/isoDateTime';
import { messageContentSchema } from '@mirri-ai/agent-core-v2/agent/contextMemory/protocolMessage';
import {
  promptPermissionModeSchema,
  promptThinkingSchema,
} from '@mirri-ai/agent-core-v2/app/sessionLegacy/sessionProtocol';

export { promptPermissionModeSchema, promptThinkingSchema };
export type { PromptPermissionMode, PromptThinking } from '@mirri-ai/agent-core-v2/app/sessionLegacy/sessionProtocol';

export const promptSubmissionSchema = z.object({
  content: z.array(messageContentSchema).min(1),
  metadata: z.record(z.string(), z.unknown()).optional(),
  agent_id: z.string().min(1).optional(),
  // Agent profile to bind at the target agent's first bind. Once bound,
  // subsequent prompts must repeat the same value or omit the field.
  profile: z.string().min(1).optional(),
  model: z.string().min(1).optional(),
  thinking: promptThinkingSchema.optional(),
  permission_mode: promptPermissionModeSchema.optional(),
  plan_mode: z.boolean().optional(),
  swarm_mode: z.boolean().optional(),
  goal_objective: z.string().optional(),
  goal_control: z.enum(['pause', 'resume', 'cancel']).optional(),
  // Client-managed session tool denylist: full-replace on every submit; the
  // bound profile's own deny always survives. Omit to keep the persisted
  // value, send `[]` to clear the client portion.
  disabled_tools: z.array(z.string()).optional(),
});
export type PromptSubmission = z.infer<typeof promptSubmissionSchema>;

export const promptStatusSchema = z.enum(['running', 'queued', 'blocked']);
export type PromptStatus = z.infer<typeof promptStatusSchema>;

/**
 * A command reference found inline in the submitted message (US3.6 /
 * ADR-004): the server never executes commands — the submitting client
 * dispatches them. Shape mirrors `@mirri-ai/protocol`'s `pendingCommandSchema`.
 */
export const pendingCommandSchema = z.object({
  name: z.string().min(1),
  display: z.string(),
  target: z.string().min(1),
});
export type PendingCommand = z.infer<typeof pendingCommandSchema>;

/**
 * An inline skill reference that failed to activate (US3.6): the message
 * still enters the turn; the failed link stays plain text. Shape mirrors
 * `@mirri-ai/protocol`'s `skippedReferenceSchema`.
 */
export const skippedReferenceSchema = z.object({
  name: z.string(),
  target: z.string(),
  reason: z.enum(['skill_not_found', 'skill_not_activatable']),
});
export type SkippedReference = z.infer<typeof skippedReferenceSchema>;

export const promptItemSchema = z.object({
  prompt_id: z.string().min(1),
  user_message_id: z.string().min(1),
  status: promptStatusSchema,
  content: z.array(messageContentSchema).min(1),
  created_at: isoDateTimeSchema,
});
export type PromptItem = z.infer<typeof promptItemSchema>;

export const promptListResponseSchema = z.object({
  active: promptItemSchema.nullable(),
  queued: z.array(promptItemSchema),
});
export type PromptListResponse = z.infer<typeof promptListResponseSchema>;

export const promptSubmitResultSchema = promptItemSchema.extend({
  /** Command references for the client to dispatch; absent when there are none. */
  pending_commands: z.array(pendingCommandSchema).optional(),
  /** Failed skill references with a reason direction; absent when there are none. */
  skipped_references: z.array(skippedReferenceSchema).optional(),
});
export type PromptSubmitResult = z.infer<typeof promptSubmitResultSchema>;

export const promptSteerRequestSchema = z.object({
  prompt_ids: z.array(z.string().min(1)).min(1),
});
export type PromptSteerRequest = z.infer<typeof promptSteerRequestSchema>;

export const promptSteerResultSchema = z.object({
  steered: z.literal(true),
  prompt_ids: z.array(z.string().min(1)).min(1),
});
export type PromptSteerResult = z.infer<typeof promptSteerResultSchema>;

export const promptAbortResponseSchema = z.object({
  aborted: z.boolean(),
  at_seq: z.number().int().nonnegative().optional(),
});
export type PromptAbortResponse = z.infer<typeof promptAbortResponseSchema>;
