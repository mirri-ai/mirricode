/**
 * `capability` domain — integrations.yaml schema types.
 *
 * One integration declaration in `integrations.yaml`. The key in the parent
 * record is matched against an MCP server name (see
 * `CapabilityRegistry.applyIntegrations`).
 */

import { z } from 'zod';

export const IntegrationSpecSchema = z.object({
  capabilities: z.array(z.string().min(1)).default([]),
  preferOver: z.array(z.string().min(1)).optional(),
});

export type IntegrationSpec = z.infer<typeof IntegrationSpecSchema>;

/** Root schema for .mirri-code/integrations.yaml. */
export const IntegrationsConfigSchema = z.object({
  integrations: z.record(z.string().min(1), IntegrationSpecSchema).default({}),
});

export type IntegrationsConfig = z.infer<typeof IntegrationsConfigSchema>;

/** Provider record used by CapabilityRegistry.resolveProviders(). */
export interface CapabilityProvider {
  readonly toolName: string;
  readonly source: 'builtin' | 'mcp';
  /** Names this provider claims to be preferred over (from integrations.yaml preferOver). */
  readonly preferOver: readonly string[];
}
