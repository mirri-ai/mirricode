// apps/mirri-web/src/lib/approvalExpiry.ts
// Pure TS — no Vue. Approval-expiry countdown helpers (B9 D-13): the rust
// engine derives no `expires_at` for approval asks, so a countdown is only
// ever rendered when the wire actually carries one. These helpers keep that
// honesty rule in one place: null means "no countdown, render nothing".

/** Seconds left until the approval's real expiry, or null when there is
 *  none (D-13: no countdown is rendered without a real expires_at). */
export function expirySecondsLeft(
  expiresAt: string | null | undefined,
  now: number = Date.now(),
): number | null {
  if (expiresAt === null || expiresAt === undefined || expiresAt === '') return null;
  const at = Date.parse(expiresAt);
  if (Number.isNaN(at)) return null;
  return Math.max(0, Math.ceil((at - now) / 1000));
}

/** Format a whole-second countdown as zero-padded mm:ss (minutes may exceed
 *  59 — "62:05", not "1:02:05" — matching the compact chip). */
export function formatExpiryCountdown(secondsLeft: number): string {
  const minutes = Math.floor(secondsLeft / 60);
  const seconds = secondsLeft % 60;
  const pad = (n: number): string => String(n).padStart(2, '0');
  return `${pad(minutes)}:${pad(seconds)}`;
}
