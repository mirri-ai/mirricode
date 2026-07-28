/**
 *   GET  /v1/tools
 *     Query: `{ session_id?: string }` — when omitted returns global tool list;
 *            when present returns the session-effective list (REST §3.8 line 430).
 *     Response data: `{ tools: ToolDescriptor[] }`
 *
 *   GET  /v1/tools/all
 *     Response data: `{ tools: ToolDescriptor[] }`
 *     Returns builtin tool descriptors independent of any session; used by
 *     clients that need the tool catalog without an active session.
 *
 *   GET  /v1/mcp/servers
 *     Response data: `{ servers: McpServer[] }`
 *
 *   POST /v1/mcp/servers/{mcp_server_id}:restart
 *     Body: empty
 *     Response data: `{ restarting: true }` (REST §3.8 line 442)
 *     Errors: 40408 mcp.server_not_found
 */

import { z } from 'zod';

import { mcpServerSchema, toolDescriptorSchema } from '../tool';

// ---------------------------------------------------------------------------
// MCP server config (wire-level) — mirrors agent-core McpServerConfig
// ---------------------------------------------------------------------------

export const mcpServerConfigTransportSchema = z.enum(['stdio', 'http', 'sse']);

export const mcpServerConfigSchema = z.object({
  transport: mcpServerConfigTransportSchema,
  // stdio
  command: z.string().optional(),
  args: z.array(z.string()).optional(),
  env: z.record(z.string(), z.string()).optional(),
  cwd: z.string().optional(),
  // http / sse
  url: z.string().optional(),
  headers: z.record(z.string(), z.string()).optional(),
  // common
  enabled: z.boolean().optional(),
  enabledTools: z.array(z.string()).optional(),
  disabledTools: z.array(z.string()).optional(),
  startupTimeoutMs: z.number().int().positive().optional(),
  toolTimeoutMs: z.number().int().positive().optional(),
  bearerTokenEnvVar: z.string().optional(),
});
export type McpServerConfigWire = z.infer<typeof mcpServerConfigSchema>;

// ---------------------------------------------------------------------------
// Request / response schemas
// ---------------------------------------------------------------------------

export const listToolsQuerySchema = z.object({
  session_id: z.string().min(1).optional(),
});
export type ListToolsQuery = z.infer<typeof listToolsQuerySchema>;

export const listToolsResponseSchema = z.object({
  tools: z.array(toolDescriptorSchema),
});
export type ListToolsResponse = z.infer<typeof listToolsResponseSchema>;

export const listAllToolsResponseSchema = z.object({
  tools: z.array(toolDescriptorSchema),
});
export type ListAllToolsResponse = z.infer<typeof listAllToolsResponseSchema>;

export const listMcpServersResponseSchema = z.object({
  servers: z.array(mcpServerSchema),
});
export type ListMcpServersResponse = z.infer<typeof listMcpServersResponseSchema>;

export const restartMcpServerResultSchema = z.object({
  restarting: z.literal(true),
});
export type RestartMcpServerResult = z.infer<typeof restartMcpServerResultSchema>;

// Global MCP endpoints

export const listGlobalMcpServersResponseSchema = z.object({
  servers: z.array(mcpServerSchema),
});
export type ListGlobalMcpServersResponse = z.infer<typeof listGlobalMcpServersResponseSchema>;

export const listGlobalMcpToolsResponseSchema = z.object({
  tools: z.array(toolDescriptorSchema),
});
export type ListGlobalMcpToolsResponse = z.infer<typeof listGlobalMcpToolsResponseSchema>;

export const createMcpServerBodySchema = z.object({
  name: z.string().min(1),
  config: mcpServerConfigSchema,
});
export type CreateMcpServerBody = z.infer<typeof createMcpServerBodySchema>;

export const updateMcpServerBodySchema = z.object({
  config: mcpServerConfigSchema,
});
export type UpdateMcpServerBody = z.infer<typeof updateMcpServerBodySchema>;

export const reloadMcpServersResultSchema = z.object({
  reloading: z.literal(true),
});
export type ReloadMcpServersResult = z.infer<typeof reloadMcpServersResultSchema>;

export const toggleMcpServerResponseSchema = z.object({
  ok: z.literal(true),
}) satisfies z.ZodType<{ ok: true }>;

export const mcpToggleStateResponseSchema = z.object({
  disabled_servers: z.array(z.string()),
  disabled_tools: z.array(z.string()),
}) satisfies z.ZodType<{
  disabled_servers: readonly string[];
  disabled_tools: readonly string[];
}>;

export const testConnectMcpServerBodySchema = z.object({
  config: mcpServerConfigSchema,
});
export type TestConnectMcpServerBody = z.infer<typeof testConnectMcpServerBodySchema>;

export const testConnectMcpServerResponseSchema = z.object({
  status: z.enum(['connected', 'error']),
  tool_count: z.number().int().nonnegative(),
  error: z.string().optional(),
  tools: z.array(z.object({
    name: z.string(),
    description: z.string(),
  })),
});
export type TestConnectMcpServerResponse = z.infer<typeof testConnectMcpServerResponseSchema>;

export const connectGlobalMcpServerResponseSchema = z.object({
  server: mcpServerSchema,
  tools: z.array(toolDescriptorSchema),
});
export type ConnectGlobalMcpServerResponse = z.infer<typeof connectGlobalMcpServerResponseSchema>;
