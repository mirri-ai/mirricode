// apps/mirri-web/src/lib/recoveryNotice.ts
// Pure logic for the recovery notice card (interrupted-task recovery, Epic C).
// Kept free of Vue/i18n imports so the wording rules stay unit-testable —
// the component maps these results onto i18n keys.

import type { AppRecoveryInterruption } from '../api/types';

/** Which notice-title wording applies (C.7): service_exit describes the
 *  unexpected exit and is the more severe cause — when both causes are in
 *  the list, the unexpected-exit wording wins. */
export function pickNoticeTitleKey(
  interruptions: AppRecoveryInterruption[],
): 'recovery.noticeTitle.service_exit' | 'recovery.noticeTitle.graceful_exit' {
  return interruptions.some((e) => e.cause === 'service_exit')
    ? 'recovery.noticeTitle.service_exit'
    : 'recovery.noticeTitle.graceful_exit';
}

/** Epoch-ms → coarse relative time (minutes/hours/days granularity, floored;
 *  future or <1min → "just now"). Own conversion — the sidebar's formatTime
 *  takes ISO date strings and must not be fed a number (finding 9). */
export function relativeTimeFromEpoch(ms: number, now: number): string {
  const elapsed = Math.max(0, now - ms);
  if (elapsed < 60_000) return 'just now';
  const minutes = Math.floor(elapsed / 60_000);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}
