// apps/mirri-web/src/lib/sessionRowRecovery.ts
// Pure logic for the session-tree recovery surface (interrupted-task
// recovery, Epic D): the badge guard and the per-workspace interruption
// counts. Kept free of Vue/i18n imports so the guards stay unit-testable
// (the recoveryNotice.ts precedent); SessionRow maps the guard result onto
// its own combined visibility conditions.

import type { AppRecoveryInterruption } from '../api/types';

/** ADR-010 badge guard: the interrupted tag renders for the merged
 *  `lastTurnReason === 'interrupted'` on an idle row that is not being
 *  renamed. The caller layers its own pending-interaction / pending-count
 *  conditions on top (the aborted/stalled badge family) — this function
 *  owns only the facts this row derives locally. */
export function interruptedBadgeVisible(facts: {
  renaming: boolean;
  busy: boolean;
  lastTurnReason?: 'completed' | 'cancelled' | 'failed' | 'stalled' | 'interrupted';
}): boolean {
  return !facts.renaming && !facts.busy && facts.lastTurnReason === 'interrupted';
}

/** Per-workspace interruption counts (D.3): the recovery list grouped by
 *  `workspaceKey` — the engine's registry id (the same identity as the
 *  workspace group's `workspace.id`). Workspaces with no interruptions
 *  carry no entry, so `counts.get(id) ?? 0` reads naturally at the call
 *  site. */
export function interruptionCountsByWorkspace(
  interruptions: AppRecoveryInterruption[],
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const entry of interruptions) {
    counts.set(entry.workspaceKey, (counts.get(entry.workspaceKey) ?? 0) + 1);
  }
  return counts;
}
