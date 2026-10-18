// apps/mirri-web/src/lib/mcpSourceTabs.ts
// Source-tab grouping for the settings MCP panel (SDR-S005 / D-S5): the
// list is browsed by scope — All / User / Project / Plugin. The project
// tab folds projectRoot (team-shared) and project (project-local) into
// one scope; "team-shared vs personal" stays visible as a secondary
// label on the card, not as separate tabs (file paths are an
// implementation detail, the user's mental model is global vs project).

export type McpTab = 'all' | 'user' | 'project' | 'plugin';

/** Tab membership for one server. An undefined source (older engines /
 *  the HTTP backend) groups with the user tab — the panel's write path
 *  targets the user layer. */
export function tabOfServer(source: string | undefined): Exclude<McpTab, 'all'> {
  if (source === 'plugin') return 'plugin';
  if (source === 'projectRoot' || source === 'project') return 'project';
  return 'user';
}

/** Filter a server list to one tab ('all' passes everything through). */
export function filterServersByTab<T extends { source?: string }>(
  servers: readonly T[],
  tab: McpTab,
): T[] {
  if (tab === 'all') return [...servers];
  return servers.filter((s) => tabOfServer(s.source) === tab);
}
