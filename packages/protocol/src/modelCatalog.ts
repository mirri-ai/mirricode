import { z } from 'zod';

export const modelCatalogItemSchema = z.object({
  provider: z.string().min(1),
  model: z.string().min(1),
  display_name: z.string().min(1).optional(),
  max_context_size: z.number().int().min(1),
  capabilities: z.array(z.string()).optional(),
  support_efforts: z.array(z.string()).optional(),
  default_effort: z.string().optional(),
});
export type ModelCatalogItem = z.infer<typeof modelCatalogItemSchema>;

export const providerCatalogStatusSchema = z.enum([
  'connected',
  'error',
  'unconfigured',
]);
export type ProviderCatalogStatus = z.infer<typeof providerCatalogStatusSchema>;

export const providerCatalogItemSchema = z.object({
  id: z.string().min(1),
  type: z.string().min(1),
  base_url: z.string().min(1).optional(),
  default_model: z.string().min(1).optional(),
  has_api_key: z.boolean(),
  status: providerCatalogStatusSchema,
  models: z.array(z.string().min(1)).optional(),
});
export type ProviderCatalogItem = z.infer<typeof providerCatalogItemSchema>;

export const catalogModelSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1).optional(),
  max_output_size: z.number().int().min(1).optional(),
  reasoning_key: z.string().min(1).optional(),
  capability: z.object({
    image_in: z.boolean(),
    video_in: z.boolean(),
    audio_in: z.boolean(),
    thinking: z.boolean(),
    tool_use: z.boolean(),
    max_context_tokens: z.number().int().min(0),
    dynamically_loaded_tools: z.boolean().optional(),
  }),
});
export type CatalogModel = z.infer<typeof catalogModelSchema>;

export const catalogProviderSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1).optional(),
  api: z.string().optional(),
  npm: z.string().optional(),
  type: z.string().optional(),
  wire: z.enum(['anthropic', 'openai', 'google-genai', 'openai_responses', 'vertexai']).optional(),
  models: z.array(catalogModelSchema),
});
export type CatalogProvider = z.infer<typeof catalogProviderSchema>;

export const providerRefreshChangeSchema = z.object({
  provider_id: z.string().min(1),
  provider_name: z.string().min(1),
  added: z.number().int().min(0),
  removed: z.number().int().min(0),
});
export type ProviderRefreshChange = z.infer<typeof providerRefreshChangeSchema>;

export const providerRefreshFailureSchema = z.object({
  provider: z.string().min(1),
  reason: z.string().min(1),
});
export type ProviderRefreshFailure = z.infer<typeof providerRefreshFailureSchema>;
