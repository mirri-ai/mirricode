/**
 * Pure helpers for displaying MCP tool counts in the Settings MCP panel.
 *
 * Wire semantics:
 * - `server.toolCount` is the runtime *enabled* count (`enabledNames.size`),
 *   so disabled tools are already excluded from it.
 * - `disabledTools` from `/mcp/global/toggle-state` are qualified names of
 *   the form `mcp__{server}__{tool}` (same wire contract as engine v1).
 *
 * Keeping these pure makes the counting semantics unit-testable, decoupled
 * from the Vue component wiring.
 */

/** Number of qualified disabled-tool names that belong to `serverName`. */
export function countDisabledForServer(
  qualifiedDisabledTools: readonly string[],
  serverName: string,
): number {
  const prefix = `mcp__${serverName}__`;
  return qualifiedDisabledTools.filter((name) => name.startsWith(prefix)).length;
}

/**
 * Enabled tool count for a server. `toolCount` already excludes disabled
 * tools, so it is returned unchanged — subtracting `disabledForServer` again
 * would double-count the exclusion.
 */
export function enabledToolCount(toolCount: number, disabledForServer: number): number {
  void disabledForServer;
  return toolCount;
}

/**
 * Total discovered tool count for a server = enabled + disabled, reconstructing
 * the full `tools/list` set the toggle modal lets the user see.
 */
export function totalToolCount(toolCount: number, disabledForServer: number): number {
  return toolCount + disabledForServer;
}