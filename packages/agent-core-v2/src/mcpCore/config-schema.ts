/**
 * `mcpCore` domain — MCP server configuration schemas.
 *
 * Owns the `McpServerConfig` schema and its transport variants. These describe
 * the shape of MCP server entries as they appear in configuration (whether in
 * `config.toml` or an MCP-specific config file).
 */

import { z } from 'zod';

const StringRecordSchema = z.record(z.string(), z.string());

export const MAX_MCP_TIMEOUT_MS = 2_147_483_647;
export const McpTimeoutMsSchema = z.number().int().min(1).max(MAX_MCP_TIMEOUT_MS);

const McpServerCommonFields = {
  enabled: z.boolean().optional(),
  startupTimeoutMs: McpTimeoutMsSchema.optional(),
  toolTimeoutMs: McpTimeoutMsSchema.optional(),
  enabledTools: z.array(z.string()).optional(),
  disabledTools: z.array(z.string()).optional(),
  auth: z.literal('oauth').optional(),
} as const;

export const McpServerStdioConfigSchema = z.object({
  transport: z.literal('stdio'),
  command: z.string().min(1),
  args: z.array(z.string()).optional(),
  env: StringRecordSchema.optional(),
  cwd: z.string().optional(),
  executor: z.enum(['local', 'kaos']).optional(),
  ...McpServerCommonFields,
});

export type McpServerStdioConfig = z.infer<typeof McpServerStdioConfigSchema>;

export const McpServerHttpConfigSchema = z.object({
  transport: z.literal('http'),
  url: z.string().url(),
  headers: StringRecordSchema.optional(),
  bearerTokenEnvVar: z.string().min(1).optional(),
  ...McpServerCommonFields,
});

export type McpServerHttpConfig = z.infer<typeof McpServerHttpConfigSchema>;

export const McpServerSseConfigSchema = z.object({
  transport: z.literal('sse'),
  url: z.string().url(),
  headers: StringRecordSchema.optional(),
  bearerTokenEnvVar: z.string().min(1).optional(),
  ...McpServerCommonFields,
});

export type McpServerSseConfig = z.infer<typeof McpServerSseConfigSchema>;
export type McpRemoteServerConfig = McpServerHttpConfig | McpServerSseConfig;

const McpServerConfigDiscriminatedSchema = z.discriminatedUnion('transport', [
  McpServerStdioConfigSchema,
  McpServerHttpConfigSchema,
  McpServerSseConfigSchema,
]);

/**
 * A stored filter-only override entry (ADR-006/A7): `mcp.config.setToolFilter`
 * with the workspace scope stores only the filter family on a name whose
 * connection family stays with the shadowed layer — no transport rides
 * along. Only the stored-config read view (`mcp.config.list` entries) can
 * carry this shape; write and test paths take `McpServerConfig`. The
 * preprocess guard keeps the union arm from swallowing a broken complete
 * config — an entry carrying connection fields never validates as an
 * override.
 */
export const McpFilterOnlyOverrideSchema = z.preprocess(
  (raw) =>
    typeof raw === 'object' &&
    raw !== null &&
    !('transport' in raw || 'command' in raw || 'url' in raw)
      ? raw
      : undefined,
  z.object(McpServerCommonFields),
);

export type McpFilterOnlyOverride = z.infer<typeof McpFilterOnlyOverrideSchema>;

/**
 * A stored config entry as the layered config view reads it: a complete
 * server config or a filter-only override.
 */
export const McpStoredServerConfigSchema = z.union([
  McpServerConfigDiscriminatedSchema,
  McpFilterOnlyOverrideSchema,
]);

export type McpStoredServerConfig = z.infer<typeof McpStoredServerConfigSchema>;

/**
 * Infer the flat `transport` tag a stored entry omits (`command` → `stdio`,
 * `url` → `http`) — the shared preprocess behind {@link McpServerConfigSchema}
 * and the config-surface views, which present stored entries in the same
 * normalized shape.
 */
export function inferTransportTag(raw: unknown): unknown {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return raw;
  const obj = raw as Record<string, unknown>;
  if ('transport' in obj) return obj;
  if (typeof obj['command'] === 'string') return { ...obj, transport: 'stdio' };
  if (typeof obj['url'] === 'string') return { ...obj, transport: 'http' };
  return obj;
}

export const McpServerConfigSchema = z.preprocess(inferTransportTag, McpServerConfigDiscriminatedSchema);

export type McpServerConfig = z.infer<typeof McpServerConfigSchema>;
