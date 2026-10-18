// apps/mirri-web/src/lib/taskOverview.ts
// Pure TS — no Vue. Overview-panel view logic for the B7 workspace task
// list (task.listWorkspace): joining workspace task rows with the session
// list the sidebar already holds (titles + archived flags), filtering, and
// the status counters the panel header shows. The panel (W4) is a thin
// wiring layer over these functions.

import type { AppSession, AppTask } from '../api/types';

/** One overview row: the task plus the session attribution joined
 *  client-side. `sessionTitle` is empty when the session is unknown to the
 *  local session list (the panel then shows the session id — an honest
 *  placeholder, never a fabricated title). */
export interface WorkspaceTaskRow {
  task: AppTask;
  sessionId: string;
  sessionTitle: string;
  archived: boolean;
}

/** Join the workspace-aggregated task rows with the session list. The
 *  server's order (sessions by recency, tasks by registration) is
 *  preserved — this only attaches the presentation fields. */
export function buildWorkspaceTaskRows(
  tasks: ReadonlyArray<AppTask>,
  sessionsById: ReadonlyMap<string, AppSession>,
): WorkspaceTaskRow[] {
  return tasks.map((task) => {
    const session = sessionsById.get(task.sessionId);
    return {
      task,
      sessionId: task.sessionId,
      sessionTitle: session?.title ?? '',
      archived: session?.archived ?? false,
    };
  });
}

/** Filter rows by status class and a free-text query. The query matches the
 *  session title, the task description, or the session id
 *  (case-insensitive substring) — enough to find a task by what it runs or
 *  by the session it belongs to. An empty query matches everything. */
export function filterWorkspaceTasks(
  rows: ReadonlyArray<WorkspaceTaskRow>,
  filter: 'all' | 'running',
  query: string,
): WorkspaceTaskRow[] {
  const q = query.trim().toLowerCase();
  return rows.filter((row) => {
    if (filter === 'running' && row.task.status !== 'running') return false;
    if (q === '') return true;
    return (
      row.sessionTitle.toLowerCase().includes(q) ||
      row.task.description.toLowerCase().includes(q) ||
      row.sessionId.toLowerCase().includes(q)
    );
  });
}

/** Status counters for the panel header. `other` catches the remaining
 *  vocabulary (cancelled) so the counts always sum to the row count. */
export function countByStatus(
  rows: ReadonlyArray<WorkspaceTaskRow>,
): { running: number; completed: number; failed: number; other: number } {
  let running = 0;
  let completed = 0;
  let failed = 0;
  let other = 0;
  for (const row of rows) {
    if (row.task.status === 'running') running += 1;
    else if (row.task.status === 'completed') completed += 1;
    else if (row.task.status === 'failed') failed += 1;
    else other += 1;
  }
  return { running, completed, failed, other };
}
