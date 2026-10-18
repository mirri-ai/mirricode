// apps/mirri-web/src/lib/memberColor.ts
// Stable per-batch member colors: hash-preferring, collision-avoiding within
// the batch. Pure and deterministic — the same id set always maps to the same
// assignment, so card dots, selector rows and future overview cards agree
// (supervision US2.4).

export const MEMBER_PALETTE: readonly string[] = [
  'var(--color-accent)',
  'var(--color-success)',
  'var(--color-warning)',
  'var(--color-danger)',
  '#8b5cf6',
  '#14b8a6',
  '#f97316',
  '#64748b',
];

/** FNV-1a 32-bit — stable across sessions and runtime versions. */
function hashId(id: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < id.length; i++) {
    hash ^= id.codePointAt(i) ?? 0;
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash;
}

export interface AssignColorsOptions {
  /** Id → color choices the caller already wants honored when possible. */
  preferred?: Readonly<Record<string, string>>;
}

export function assignMemberColors(
  ids: readonly string[],
  options?: AssignColorsOptions,
): Map<string, string> {
  const used = new Set<string>();
  const out = new Map<string, string>();
  for (const id of ids) {
    let color =
      options?.preferred?.[id] ?? MEMBER_PALETTE[hashId(id) % MEMBER_PALETTE.length]!;
    if (used.has(color)) {
      const free = MEMBER_PALETTE.find((c) => !used.has(c));
      if (free !== undefined) color = free;
    }
    used.add(color);
    out.set(id, color);
  }
  return out;
}

/** Color for a single member outside a batch context (no collision info). */
export function memberColorFor(id: string): string {
  return MEMBER_PALETTE[hashId(id) % MEMBER_PALETTE.length]!;
}
