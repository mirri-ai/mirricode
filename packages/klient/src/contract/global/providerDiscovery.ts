/**
 * `providerDiscovery` — the engine's `IProviderDiscoveryService`: remote
 * provider-model discovery and config sync. Mirrors
 * `agent-core-v2/app/kosongConfig/discovery.ts`; the `listCatalog` /
 * `importCatalog` rows carry the models.dev directory surface (the TS
 * `IModelsDevImportService` browse/import face over the same wire).
 */

import { z } from 'zod';

import type { ServiceContract } from '../types.js';

export const refreshProviderModelsOptionsSchema = z.object({
  scope: z.enum(['all']).optional(),
  providerId: z.string().optional(),
});

/** Response shape mirrors `refreshProviderModelsResponseSchema` in `agent-core-v2/app/kosongConfig/discovery.ts`. */
export const refreshProviderModelsResponseSchema = z.object({
  changed: z.array(
    z.object({
      provider_id: z.string(),
      provider_name: z.string(),
      added: z.number(),
      removed: z.number(),
    }),
  ),
  unchanged: z.array(z.string()),
  failed: z.array(z.object({ provider: z.string(), reason: z.string() })),
});

/**
 * One `providerDiscovery.listCatalog` directory model — the pruned browse
 * shape plus the directory's capability facts (ADR-004, additive): the
 * same derivation the import writes (`context_length` is the directory's
 * `limit.context`; `capabilities` uses the alias vocabulary
 * `image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/
 * `dynamically_loaded_tools`; `reasoning` is the `thinking` fact).
 * Older engines omit the additive fields — never null.
 */
export const providerCatalogModelEntrySchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1).optional(),
  context_length: z.number().int().positive().optional(),
  capabilities: z.array(z.string()).optional(),
  reasoning: z.boolean().optional(),
});

/**
 * One browsable models.dev directory entry — directory facts (never the
 * configured set), never credentials. The field set is the TS
 * `catalogProviderItemSchema` pruned of `guessed` / `rejected` /
 * `reject_reason`; `wire_type` and `env_key` are omitted (not null) when
 * the entry does not declare them.
 */
export const providerCatalogEntrySchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1),
  wire_type: z.string().min(1).optional(),
  needs_base_url: z.boolean(),
  env_key: z.string().min(1).optional(),
  models: z.array(providerCatalogModelEntrySchema),
});

/**
 * `providerDiscovery.importCatalog` arguments — the directory entry to
 * import (`catalogId`), an optional local provider-id override, and the
 * optional credentials the import stores with the provider record. The
 * camelCase spelling is the wire contract (the engine accepts the
 * snake_case aliases for hand-rolled callers).
 */
export const importCatalogArgsSchema = z.object({
  catalogId: z.string().min(1),
  id: z.string().min(1).optional(),
  apiKey: z.string().optional(),
  baseUrl: z.string().optional(),
});

/**
 * `providerDiscovery.importCatalog` result: the local provider id the
 * entry was imported under and the number of minimal model aliases
 * appended under merge semantics — the engine merges into an existing
 * provider (existing aliases untouched, tombstones hold), and
 * `models_imported` counts only the appended ones (never credentials — N4).
 */
export const importCatalogResultSchema = z.object({
  provider: z.string().min(1),
  models_imported: z.number().int().nonnegative(),
});

export const providerDiscoveryContract = {
  refreshProviderModels: {
    input: z.tuple([refreshProviderModelsOptionsSchema.optional()]),
    output: refreshProviderModelsResponseSchema,
  },
  listCatalog: {
    input: z.tuple([]),
    output: z.array(providerCatalogEntrySchema),
  },
  importCatalog: {
    input: z.tuple([importCatalogArgsSchema]),
    output: importCatalogResultSchema,
  },
} satisfies ServiceContract;
