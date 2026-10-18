// apps/mirri-web/src/lib/permissionMirror.ts
import type { PermissionMode } from '../types';

/**
 * Server-authoritative permission-mode mirror (SDR-004 e): the UI displays
 * the ACTIVE session's live mode, never a locally-stored guess. Returns
 * the mode to fold into the composer when the fetched status carries one
 * for the active session; null otherwise (background session, or an
 * empty/unknown value — keep whatever the UI currently shows).
 */
export function permissionFromStatus(
  statusPermission: string | undefined,
  isActiveSession: boolean,
): PermissionMode | null {
  if (!isActiveSession) return null;
  if (
    statusPermission === 'manual' ||
    statusPermission === 'auto' ||
    statusPermission === 'yolo'
  ) {
    return statusPermission;
  }
  return null;
}

/**
 * Composition of one refreshSessionStatus fold (SDR-004 e), kept pure so the
 * wiring semantics are testable: a fetched status MAY update the composer's
 * displayed mode only when it belongs to the ACTIVE session; a background
 * session's status must not flip the visible mode, and an absent/unusable
 * mode keeps the current display.
 */
export function nextPermissionFromStatus(
  statusPermission: string | undefined,
  sessionId: string,
  activeSessionId: string | undefined,
  currentPermission: PermissionMode,
): PermissionMode {
  return (
    permissionFromStatus(statusPermission, sessionId === activeSessionId) ?? currentPermission
  );
}