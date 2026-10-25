import { z } from 'zod';

const suppressedCandidateSchema = z.object({
  source: z.enum(['builtin', 'user', 'project', 'extra', 'plugin', 'explicit']),
  priority: z.number(),
  reason: z.enum(['priority', 'builtin-override-required', 'disabled']),
});

export const profileEntrySchema = z.object({
  name: z.string(),
  description: z.string().optional(),
  source: z.enum(['builtin', 'user', 'project', 'extra', 'plugin', 'explicit']),
  builtin: z.boolean(),
  essential: z.boolean(),
  enabled: z.boolean(),
  has_override: z.boolean(),
  file_path: z.string().optional(),
  extends: z.string().optional(),
  default_model: z.string().optional(),
  capabilities: z.array(z.string()).optional(),
  capabilities_required: z.array(z.string()).optional(),
  tools: z.array(z.string()).optional(),
  disallowed_tools: z.array(z.string()).optional(),
  subagents: z.array(z.string()).optional(),
  when_to_use: z.string().optional(),
  system_prompt_template: z.string().optional(),
  /** Same-name candidates that lost the merge — expandable in the settings page (US-A1-2). */
  suppressed_candidates: z.array(suppressedCandidateSchema).optional(),
  /**
   * The profile's effective system prompt with its full context rendered.
   * Populated for every profile carrying a system prompt (ADR-007) —
   * built-ins render their code-defined prompt; file-sourced rows render
   * their template with `${base_prompt}` bound — so the UI can show the
   * actual behavior read-only while editing the profile's own role
   * definition in `system_prompt_template`.
   */
  effective_system_prompt: z.string().optional(),
  /**
   * The composition form of the profile's prompt body (ADR-010 ①, US-A4
   * form disclosure): `composed` — the body carries the `${base_prompt}`
   * reference and rides the shared base; `replacement` — the body is a
   * full replacement with no base reference. Emitted whenever the row's
   * underlying prompt body is known (builtin rows are composed by the A16
   * model; file rows by their body).
   */
  prompt_form: z.enum(['composed', 'replacement']).optional(),
  /**
   * D21 / US-A8: whether a `<brandHome>/SYSTEM.md` base-prompt override is
   * active. Emitted on the essential `agent` row only — SYSTEM.md is the
   * default base's override, and the settings surface addresses it through
   * that row.
   */
  base_prompt_override: z.boolean().optional(),
  /**
   * D21 / US-A8: the raw `<brandHome>/SYSTEM.md` text (the editable
   * override body, `${base_prompt}` included). Present only when the
   * override exists (the flag above is true).
   */
  base_prompt_template: z.string().optional(),
  /**
   * The builtin default base prompt template's RAW text (placeholders
   * intact — NOT the rendered form). Emitted on the essential `agent`
   * row only: the settings editor's seed when no override exists, so an
   * edit starts from the real default instead of a rendered form. Read-only
   * — never part of the update request.
   */
  base_prompt_default_template: z.string().optional(),
  prompt_vars: z.record(z.string(), z.string()).optional(),
});
export type ProfileEntry = z.infer<typeof profileEntrySchema>;

export const listProfilesResponseSchema = z.object({
  items: z.array(profileEntrySchema),
  /** Agent files skipped during discovery (parse failures) — surfaced as
   *  readable warnings in the settings page (US-A1-3). */
  skipped: z.array(z.object({ path: z.string(), reason: z.string() })).optional(),
});
export type ListProfilesResponse = z.infer<typeof listProfilesResponseSchema>;

export const createProfileRequestSchema = z.object({
  name: z.string().min(1),
  description: z.string().optional(),
  extends: z.string().optional(),
  default_model: z.string().optional(),
  tools: z.array(z.string()).optional(),
  disallowed_tools: z.array(z.string()).optional(),
  subagents: z.array(z.string()).optional(),
  system_prompt_template: z.string().optional(),
  when_to_use: z.string().optional(),
  prompt_vars: z.record(z.string(), z.string()).optional(),
  capabilities: z.array(z.string()).optional(),
  capabilities_required: z.array(z.string()).optional(),
});
export type CreateProfileRequest = z.infer<typeof createProfileRequestSchema>;

export const updateProfileRequestSchema = createProfileRequestSchema
  .partial()
  .extend({
    /**
     * D21 / US-A8: the `<brandHome>/SYSTEM.md` base-prompt override body —
     * writeable ONLY on the essential `agent` row (a non-empty string
     * writes the file; `''` deletes it). Absent means "do not touch the
     * override".
     */
    base_prompt_template: z.string().optional(),
  });
export type UpdateProfileRequest = z.infer<typeof updateProfileRequestSchema>;
