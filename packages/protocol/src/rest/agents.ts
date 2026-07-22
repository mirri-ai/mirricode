import { z } from 'zod';

export const profileEntrySchema = z.object({
  name: z.string(),
  description: z.string().optional(),
  source: z.enum(['builtin', 'user', 'project', 'extra']),
  builtin: z.boolean(),
  essential: z.boolean(),
  enabled: z.boolean(),
  file_path: z.string().optional(),
  extends: z.string().optional(),
  default_model: z.string().optional(),
  tools: z.array(z.string()).optional(),
  when_to_use: z.string().optional(),
  system_prompt_template: z.string().optional(),
});
export type ProfileEntry = z.infer<typeof profileEntrySchema>;

export const listProfilesResponseSchema = z.object({
  items: z.array(profileEntrySchema),
});
export type ListProfilesResponse = z.infer<typeof listProfilesResponseSchema>;

export const createProfileRequestSchema = z.object({
  name: z.string().min(1),
  description: z.string().optional(),
  extends: z.string().optional(),
  default_model: z.string().optional(),
  tools: z.array(z.string()).optional(),
  system_prompt_template: z.string().optional(),
  when_to_use: z.string().optional(),
  prompt_vars: z.record(z.string(), z.string()).optional(),
  capabilities: z.array(z.string()).optional(),
  capabilities_required: z.array(z.string()).optional(),
});
export type CreateProfileRequest = z.infer<typeof createProfileRequestSchema>;

export const updateProfileRequestSchema = createProfileRequestSchema.partial();
export type UpdateProfileRequest = z.infer<typeof updateProfileRequestSchema>;
