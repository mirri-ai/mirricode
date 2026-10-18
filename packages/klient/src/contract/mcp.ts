/**
 * Shared MCP server wire schema for session creation, plugin manifests, and
 * the layered config surface. Mirrors `agent-core-v2/mcpCore/config-schema.ts`
 * (the engine's stored-form validation): urls are plain strings — a stored
 * `${VAR}` reference is legal because expansion happens at load/connect time
 * (adjudication A5), and the loader validates the expanded value.
 */

import { z } from 'zod';

const stringRecordSchema = z.record(z.string(), z.string());

export const mcpTimeoutMsSchema = z.number().int().min(1).max(2_147_483_647);

const mcpServerCommonFields = {
  enabled: z.boolean().optional(),
  startupTimeoutMs: mcpTimeoutMsSchema.optional(),
  toolTimeoutMs: mcpTimeoutMsSchema.optional(),
  enabledTools: z.array(z.string()).optional(),
  disabledTools: z.array(z.string()).optional(),
} as const;

export const mcpServerConfigSchema = z.discriminatedUnion('transport', [
  z.object({
    transport: z.literal('stdio'),
    command: z.string().min(1),
    args: z.array(z.string()).optional(),
    env: stringRecordSchema.optional(),
    cwd: z.string().optional(),
    executor: z.enum(['local', 'kaos']).optional(),
    ...mcpServerCommonFields,
  }),
  z.object({
    transport: z.literal('http'),
    url: z.string(),
    headers: stringRecordSchema.optional(),
    bearerTokenEnvVar: z.string().min(1).optional(),
    ...mcpServerCommonFields,
  }),
  z.object({
    transport: z.literal('sse'),
    url: z.string(),
    headers: stringRecordSchema.optional(),
    bearerTokenEnvVar: z.string().min(1).optional(),
    ...mcpServerCommonFields,
  }),
]);

/**
 * A stored filter-only override entry (ADR-006/A7): `mcp.config.setToolFilter`
 * with the workspace scope stores only the filter family on a name whose
 * connection family stays with the shadowed layer — no transport rides
 * along. Only the stored-config read view (`mcp.config.list` entries) can
 * carry this shape; write and test paths take `mcpServerConfigSchema`.
 * The preprocess guard keeps the union arm from swallowing a broken
 * complete config — an entry carrying connection fields (a present-but-
 * invalid transport, a bare `command`) never validates as an override.
 */
export const mcpFilterOnlyOverrideSchema = z.preprocess(
  (raw) =>
    typeof raw === 'object' &&
    raw !== null &&
    !('transport' in raw || 'command' in raw || 'url' in raw)
      ? raw
      : undefined,
  z.object(mcpServerCommonFields),
);

/**
 * A stored config entry as the layered config view reads it: a complete
 * server config or a filter-only override.
 */
export const mcpStoredServerConfigSchema = z.union([
  mcpServerConfigSchema,
  mcpFilterOnlyOverrideSchema,
]);
