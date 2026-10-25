// apps/mirri-web/src/composables/client/writeBack.ts
// SDR-003b: write back only fields whose references changed, and write
// per-session maps with nested-key assignment so Vue's (target, key)
// dependency tracking only invalidates consumers of the changed session.
import type { MirriClientState } from '../../api/daemon/eventReducer';

type MapOf<T> = { [k: string]: T };

/** Assign next[key] onto target[key] only when the reference differs. */
export function writeIfChanged<K extends keyof MirriClientState>(
  target: MirriClientState,
  next: MirriClientState,
  key: K,
): void {
  if (target[key] !== next[key]) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any -- record map or array assign
    (target as any)[key] = next[key];
  }
}

/**
 * Per-session map write-back: instead of replacing the whole map object,
 * diff it per key and assign only changed entries as nested keys
 * (target.map[sid] = value) — a Vue reactive proxy triggers only the
 * effects that read that sid, not every reader of the map property.
 */
export function writeMapIfChanged<V>(
  targetMap: MapOf<V>,
  nextMap: MapOf<V>,
): boolean {
  if (targetMap === nextMap) return false;
  let changed = false;
  for (const key of Object.keys(nextMap)) {
    if (targetMap[key] !== nextMap[key]) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any -- V is
      // a bare type parameter; Object.keys cannot refine its optionality.
      targetMap[key] = nextMap[key] as any;
      changed = true;
    }
  }
  for (const key of Object.keys(targetMap)) {
    if (!(key in nextMap)) {
      delete targetMap[key];
      changed = true;
    }
  }
  return changed;
}
