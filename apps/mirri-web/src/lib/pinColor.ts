// apps/mirri-web/src/lib/pinColor.ts
// SDR-016/017 + SDR-021: the pin color preset, in ONE place.
//
// Pinning always arrives WITH a color. The sidebar marks a pinned row with a
// filled dot, so a colorless pin would leave no visible trace — "pinned" would
// be unreadable at a glance. A NEW pin picks a random preset colour so
// consecutive pins are distinguishable at a glance; the palette (hover a pinned
// dot, or right-click it) switches to any of the seven.

export const PIN_COLORS = [
  'red',
  'orange',
  'yellow',
  'green',
  'blue',
  'purple',
  'gray',
] as const;

export type PinColor = (typeof PIN_COLORS)[number];

/** The colour an existing pin falls back to when its record carries none
 *  (legacy data). Purely a display fallback — never a value we write. */
export const DEFAULT_PIN_COLOR: PinColor = 'red';

/**
 * The colour a NEW pin gets.
 *
 * Random per pin, so a freshly pinned row is distinguishable from the ones
 * above it without the user opening the palette first — the common case is
 * pinning a few things in a row, and identical dots make that list unreadable.
 *
 * The one guard: avoid repeating the previous pick, because two identical
 * colours landing next to each other reads as "the same item" rather than luck.
 */
export function pickPinColor(previous?: PinColor | null): PinColor {
  const pool = PIN_COLORS.filter((c) => c !== previous);
  return pool[Math.floor(Math.random() * pool.length)] ?? DEFAULT_PIN_COLOR;
}
