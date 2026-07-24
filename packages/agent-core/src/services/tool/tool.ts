/**
 * `IToolService` — daemon-facing read-only tool surface.
 *
 * Wraps `ICoreProcessService.rpc.getTools` and translates agent-core's `ToolInfo`
 * (camelCase, includes `'user'` source literal) into SCHEMAS §8 `ToolDescriptor`
 * (snake_case, `'skill'` literal). Adapter helpers (`toProtocolTool`,
 * `AgentCoreToolInfoLike`) are co-located here.
 *
 * **CoreAPI surface used**:
 *   - `bridge.rpc.getTools({}) => readonly ToolInfo[]` (packages/agent-core/src/rpc/core-api.ts:333).
 *
 * **REST.md §3.8 ?session_id behavior**: when caller passes a session_id the
 * route currently returns the same global list — agent-core's `getTools`
 * doesn't differentiate per-session, and `setActiveTools` is the only
 * per-session knob. Documented gap in `ToolService`.
 *
 * **Anti-corruption**: imports `@mirri-ai/agent-core` only for the
 * `createDecorator` value.
 */

import { createDecorator } from '../../di';
import type { ToolDescriptor, ToolSource } from '@mirri-ai/protocol';

// ---------------------------------------------------------------------------
// Adapter helpers (tool side of former adapter/tool-adapter.ts)
// ---------------------------------------------------------------------------

/**
 * In-process minimal shape we accept for tool conversion. Mirrors
 * `@mirri-ai/agent-core` `ToolInfo` without taking a runtime dependency on
 * its exact shape (the adapter is the boundary).
 */
export interface AgentCoreToolInfoLike {
  readonly name: string;
  readonly description: string;
  readonly source: 'builtin' | 'user' | 'mcp';
  /** agent-core may add fields like `active`; we ignore them. */
  readonly active?: boolean;
}

function mapToolSource(s: AgentCoreToolInfoLike['source']): ToolSource {
  switch (s) {
    case 'builtin':
      return 'builtin';
    case 'user':
      return 'skill';
    case 'mcp':
      return 'mcp';
  }
}

/**
 * Parse the server id segment from an MCP tool name. Convention:
 * `mcp:<server>:<tool>` (kosong's `mcpRegistrar.qualifiedName`). Returns
 * `undefined` when the name does not match — caller omits `mcp_server_id`.
 */
function parseMcpServerIdFromToolName(name: string): string | undefined {
  if (!name.startsWith('mcp:')) return undefined;
  const rest = name.slice('mcp:'.length);
  const sep = rest.indexOf(':');
  if (sep <= 0) return undefined;
  return rest.slice(0, sep);
}

export function toProtocolTool(info: AgentCoreToolInfoLike): ToolDescriptor {
  const source = mapToolSource(info.source);
  const base: ToolDescriptor = {
    name: info.name,
    description: info.description,
    // agent-core's ToolInfo lacks a JSON schema today; emit null so the
    // wire schema is honest about "unknown".
    input_schema: null,
    source,
  };
  if (source === 'mcp') {
    const serverId = parseMcpServerIdFromToolName(info.name);
    if (serverId !== undefined) {
      return { ...base, mcp_server_id: serverId };
    }
  }
  return base;
}

// ---------------------------------------------------------------------------
// Interface + implementation
// ---------------------------------------------------------------------------

/**
 * Static builtin tool descriptors returned when no session is available.
 * These are the core tools every session registers regardless of config.
 * MCP, skill, and conditional tools (goal, cron, agent, etc.) are only
 * visible through a session-scoped query.
 */
export const BUILTIN_TOOL_DESCRIPTORS: readonly ToolDescriptor[] = Object.freeze([
  { name: 'Read', description: 'Read a file from the local filesystem.', input_schema: null, source: 'builtin' as const },
  { name: 'Write', description: 'Create or overwrite a file on disk.', input_schema: null, source: 'builtin' as const },
  { name: 'Edit', description: 'Perform exact string replacements in an existing file.', input_schema: null, source: 'builtin' as const },
  { name: 'Grep', description: 'Search file contents using regular expressions.', input_schema: null, source: 'builtin' as const },
  { name: 'Glob', description: 'Find files matching a glob pattern.', input_schema: null, source: 'builtin' as const },
  { name: 'Bash', description: 'Execute a bash command on the system.', input_schema: null, source: 'builtin' as const },
  { name: 'TodoList', description: 'Manage a structured todo list.', input_schema: null, source: 'builtin' as const },
  { name: 'EnterPlanMode', description: 'Enter plan mode for read-only investigation.', input_schema: null, source: 'builtin' as const },
  { name: 'ExitPlanMode', description: 'Exit plan mode and present a plan for approval.', input_schema: null, source: 'builtin' as const },
  { name: 'SelectTools', description: 'Select which tools are available to the agent.', input_schema: null, source: 'builtin' as const },
]);

export interface IToolService {
  readonly _serviceBrand: undefined;

  /**
   * Return the available tool descriptors. When `sessionId` is supplied, the
   * impl may return a session-effective subset; today it returns the global
   * list (CoreAPI gap documented in the impl).
   */
  list(sessionId?: string): Promise<readonly ToolDescriptor[]>;

  /**
   * Return all tool descriptors independent of any session's active/inactive
   * state. Always returns at least the builtin tool catalog; when a session
   * exists, also includes session-scoped tools (MCP, skill) so callers (e.g.
   * web Settings panel) can present the full tool selection.
   */
  listAll(): Promise<readonly ToolDescriptor[]>;
}

// eslint-disable-next-line @typescript-eslint/no-redeclare
export const IToolService = createDecorator<IToolService>('toolService');

void IToolService;
