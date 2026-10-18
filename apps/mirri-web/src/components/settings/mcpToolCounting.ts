/**
 * Pure helpers for displaying MCP tool counts in the Settings MCP panel.
 *
 * Wire semantics (rust engine, `mcp.status`):
 * - `server.toolCount` is the connection's **raw discovered tool count**
 *   (`tools/list` result length — disabled tools included).
 * - `disabledTools` from the toggle state are qualified names of the form
 *   `mcp__{server}__{tool}` (the same wire contract the v1 endpoint used).
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
 * Enabled tool count for a server: the raw discovered count minus the
 * server's disabled tools (the denylist visible in the panel).
 */
export function enabledToolCount(toolCount: number, disabledForServer: number): number {
  return Math.max(0, toolCount - disabledForServer);
}

/**
 * Total discovered tool count for a server — the raw `tools/list` size,
 * which the toggle modal lets the user see in full.
 */
export function totalToolCount(toolCount: number, disabledForServer: number): number {
  void disabledForServer;
  return toolCount;
}
