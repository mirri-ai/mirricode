/**
 * Constants for the /feedback command — the status messages shown around the
 * feedback submission flow.
 *
 * Dialog-internal copy (the box title, subtitle, footer) lives next to
 * the dialog component itself, since it is part of that component's
 * visual contract.
 */

import { FEEDBACK_ISSUE_URL } from '#/constant/app';

export { FEEDBACK_ISSUE_URL };

export const FEEDBACK_STATUS_NOT_SIGNED_IN =
  'Managed feedback is retired. Opening GitHub Issues for feedback…';

// Hint shown beneath session-level error messages in the TUI to point users
// at the `/export-debug-zip` workflow so they can share diagnostics with us.
export function errorReportHintLine(): string {
  return "If this persists, run `/export-debug-zip` and share the file with us for diagnosis. Please don't share it publicly.";
}
