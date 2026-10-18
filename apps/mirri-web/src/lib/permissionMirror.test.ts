// apps/mirri-web/src/lib/permissionMirror.test.ts
import { describe, expect, it } from 'vitest';
import { nextPermissionFromStatus, permissionFromStatus } from './permissionMirror';

describe('permissionFromStatus', () => {
  it('should mirror the server mode when the status belongs to the active session', () => {
    expect(permissionFromStatus('yolo', true)).toBe('yolo');
    expect(permissionFromStatus('auto', true)).toBe('auto');
    expect(permissionFromStatus('manual', true)).toBe('manual');
  });

  it('should keep the current display when the status belongs to a background session', () => {
    expect(permissionFromStatus('yolo', false)).toBeNull();
  });

  it('should keep the current display when the status carries no usable mode', () => {
    expect(permissionFromStatus(undefined, true)).toBeNull();
    expect(permissionFromStatus('', true)).toBeNull();
    expect(permissionFromStatus('bogus', true)).toBeNull();
  });
});

// Composition layer of the refreshSessionStatus fold (final-review finding
// Important-2b): the gate decides whether the fetched status may write the
// displayed mode, then the fold happens — verified here end to end so a
// regression in the wiring (e.g. dropping the active-session gate) is caught
// by a unit test rather than a full useMirriWebClient instantiation.
describe('nextPermissionFromStatus', () => {
  it('should fold the fetched mode into the displayed mode when the status belongs to the active session', () => {
    // Given the status fetch is for the ACTIVE session and carries a mode
    // When the fold is applied to the current display
    // Then the displayed mode becomes the server mode
    expect(nextPermissionFromStatus('yolo', 'sess_1', 'sess_1', 'manual')).toBe('yolo');
    expect(nextPermissionFromStatus('auto', 'sess_1', 'sess_1', 'manual')).toBe('auto');
    expect(nextPermissionFromStatus('manual', 'sess_1', 'sess_1', 'yolo')).toBe('manual');
  });

  it('should keep the displayed mode when the status belongs to a background session', () => {
    // Given the status fetch is for a BACKGROUND session (or none is active)
    // When the fold is applied to the current display
    // Then the displayed mode is left untouched
    expect(nextPermissionFromStatus('yolo', 'sess_1', 'sess_2', 'manual')).toBe('manual');
    expect(nextPermissionFromStatus('auto', 'sess_1', 'sess_2', 'yolo')).toBe('yolo');
    expect(nextPermissionFromStatus('yolo', 'sess_1', undefined, 'manual')).toBe('manual');
  });

  it('should keep the displayed mode when the status carries no usable mode', () => {
    // Given the active session's status carries no usable mode
    // When the fold is applied to the current display
    // Then the displayed mode is kept as-is (never blanked out)
    expect(nextPermissionFromStatus(undefined, 'sess_1', 'sess_1', 'yolo')).toBe('yolo');
    expect(nextPermissionFromStatus('', 'sess_1', 'sess_1', 'yolo')).toBe('yolo');
    expect(nextPermissionFromStatus('bogus', 'sess_1', 'sess_1', 'yolo')).toBe('yolo');
  });
});