// apps/mirri-web/src/lib/suspendedReason.ts
// Render the member suspension reason as PROSE. The reason is dual-shape
// (ADR-011): a pre-structuring legacy string (free-form prose — shown
// verbatim during the compat window) or a structured object that is a MARKER,
// not prose — the awaiting/backoff phase badge carries the state, so the
// structured object renders as nothing rather than "[object Object]".
// i18n stays out of this pure TS layer (components pass labels in), matching
// swarmCardRows' awaitingLabel convention.

import type { AppSuspendedReason } from '../api/types';

export function suspendedReasonText(reason: AppSuspendedReason | string | undefined): string {
  if (typeof reason === 'string') return reason;
  // Structured reason (object) or absent — no prose to render.
  return '';
}