/**
 *   GET  /v1/sessions/{session_id}/skills
 *   POST /v1/sessions/{session_id}/skills/{skill_name}:activate
 */

import { z } from 'zod';

import { skillDescriptorSchema } from './skill';

export const listSkillsResponseSchema = z.object({
  skills: z.array(skillDescriptorSchema),
});
export type ListSkillsResponse = z.infer<typeof listSkillsResponseSchema>;

export const activateSkillRequestSchema = z.object({
  /** Raw argument string appended after the slash command, e.g. `/review --fix` → `--fix`. */
  args: z.string().optional(),
});
export type ActivateSkillRequest = z.infer<typeof activateSkillRequestSchema>;

export const activateSkillResultSchema = z.object({
  activated: z.literal(true),
  skill_name: z.string().min(1),
  /**
   * The queued prompt's id. When the loop is idle the skill turn starts
   * immediately and this doubles as its prompt id; while a turn is running the
   * activation is parked and the id lets callers track when it launches.
   */
  prompt_id: z.string().min(1),
  /** `running` when the skill turn launched immediately; `queued` otherwise. */
  status: z.enum(['queued', 'running']),
});
export type ActivateSkillResult = z.infer<typeof activateSkillResultSchema>;
