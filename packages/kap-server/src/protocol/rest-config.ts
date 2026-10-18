/**
 * `/config` wire schemas — re-exported from `@mirri-ai/protocol`.
 *
 * These shapes were originally hand-mirrored here; they now live once in
 * the shared protocol package (`packages/protocol/src/rest/config.ts`),
 * which is the reference the web client and the other daemon surfaces
 * consume. This module keeps the historical import paths
 * (`../protocol/rest-config`) working — the public surface (the exported
 * schema constants and inferred types) is unchanged; the protocol schemas
 * are a superset (every additionally accepted key is `.optional()`), so
 * requests and responses that validated before still validate.
 */

export {
  providerConfigResponseSchema,
  type ProviderConfigResponse,
  configResponseSchema,
  type ConfigResponse,
  patchConfigRequestSchema,
  type PatchConfigRequest,
} from '@mirri-ai/protocol';
