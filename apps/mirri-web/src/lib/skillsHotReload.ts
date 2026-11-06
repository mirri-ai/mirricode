// apps/mirri-web/src/lib/skillsHotReload.ts
// ADR-013 decision 3: the workspaceId → cache-key invalidation mapping for
// the skill-catalog change event. Pure — tested without Vue.

/** The skill caches to refetch for one skillCatalogChanged event: every
 *  cached session key belonging to the event's workspace, plus the
 *  workspace key — but only keys already populated (the UI carried skill
 *  candidates for them; never fetch for a workspace the UI never pulled). */
export function skillCacheKeysToRefresh(
  workspaceId: string,
  sessions: ReadonlyArray<{ id: string; workspaceId?: string; cwd: string }>,
  workspaces: ReadonlyArray<{ id: string; root: string }>,
  cachedSessions: ReadonlySet<string>,
  cachedWorkspaces: ReadonlySet<string>,
): { sessions: string[]; workspace: boolean } {
  const rootsById = new Map(workspaces.map((w) => [w.id, w.root] as const));
  const belongs =
    workspaceId === '' ? () => false : (s: { workspaceId?: string; cwd: string }) =>
      s.workspaceId === workspaceId ||
      (s.workspaceId === undefined && rootsById.get(workspaceId) === s.cwd);
  const sessionsToRefresh = sessions
    .filter((s) => cachedSessions.has(s.id) && belongs(s))
    .map((s) => s.id);
  return { sessions: sessionsToRefresh, workspace: cachedWorkspaces.has(workspaceId) };
}
