// apps/mirri-web/src/lib/operationFailureNotice.ts
// Pure message-copy assembly for the operation-failure toast. The details
// half (`errorDetails`) stays in `useMirriWebClient.ts` because it reads the
// live connection/session state; this module is the testable core: classify
// the failure and pick the default title/message. `t` is injected so the
// module never touches vue-i18n (or any DOM/component state) directly.

import { isDaemonApiError, isDaemonNetworkError } from '../api/errors';

/** The failure family that drives the default toast copy. */
export type OperationFailureKind = 'network' | 'api' | 'other';

export function classifyOperationFailure(err: unknown): OperationFailureKind {
  if (isDaemonNetworkError(err)) return 'network';
  if (isDaemonApiError(err)) return 'api';
  return 'other';
}

/** The toast's title/message pair. */
export interface OperationFailureCopy {
  title: string;
  message: string;
}

/** Default title/message for a classified failure: caller-provided values
 * win, otherwise the family-specific i18n keys (with the api family showing
 * the server's own message). */
export function operationFailureCopy(
  kind: OperationFailureKind,
  err: unknown,
  opts: { title?: string; message?: string },
  t: (key: string) => string,
): OperationFailureCopy {
  switch (kind) {
    case 'network':
      return {
        title: opts.title ?? t('warnings.daemonNetworkTitle'),
        message: opts.message ?? t('warnings.daemonNetworkMessage'),
      };
    case 'api':
      return {
        title: opts.title ?? t('warnings.daemonApiTitle'),
        message: opts.message ?? (isDaemonApiError(err) ? err.message : t('warnings.operationFailedMessage')),
      };
    case 'other':
      return {
        title: opts.title ?? t('warnings.operationFailedTitle'),
        message: opts.message ?? t('warnings.operationFailedMessage'),
      };
  }
}