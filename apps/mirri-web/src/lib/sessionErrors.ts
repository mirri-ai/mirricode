// apps/mirri-web/src/lib/sessionErrors.ts
// Session-error classification helpers. Pure functions — no Vue, no API layer
// beyond the error type guards — so they stay trivially testable.

import { isDaemonApiError } from '../api/errors';
import { JsonRpcCallError, NOT_FOUND_CODE } from '../api/daemon/json-rpc-client';

/** REST-daemon domain code for "session not found" (legacy v1 daemon). */
const SESSION_NOT_FOUND_CODE = 40401;

/**
 * True when the error means "the session no longer exists on the server".
 *
 * Recognizes:
 *  1. a `DaemonApiError` carrying the REST daemon's session-not-found code
 *     (40401) — the legacy transport;
 *  2. any object whose `code` is 40401 (defensive bare-object leg kept from
 *     the original inline predicate);
 *  3. a `JsonRpcCallError` carrying the JSON-RPC `NOT_FOUND_CODE` (-32001).
 *
 * `-32001` is the engine's GENERIC "resource not found" code (shared by
 * session/agent/provider/model not-found and `RpcError::NotFound`), so this
 * predicate must only be used in session-read error classification contexts —
 * currently its only consumer is the snapshot sync catch in
 * useMirriWebClient.ts.
 */
export function isSessionNotFoundError(err: unknown): boolean {
  if (isDaemonApiError(err) && err.code === SESSION_NOT_FOUND_CODE) return true;
  if (err instanceof JsonRpcCallError && err.code === NOT_FOUND_CODE) return true;
  return (
    typeof err === 'object' &&
    err !== null &&
    (err as { code?: unknown }).code === SESSION_NOT_FOUND_CODE
  );
}
