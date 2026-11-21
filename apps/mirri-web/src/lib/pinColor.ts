// apps/mirri-web/src/lib/pinColor.ts
// SDR-016/017 + SDR-021: the pin color preset, in ONE place.
//
// Pinning always arrives WITH a color. The sidebar's pin rail marks a pinned
// row with a filled dot, so a colorless pin would leave no visible trace —
// "pinned" would be unreadable at a glance. Red is the default the rail
// applies on pin; the palette (hover a pinned dot, or right-click it) switches
// to any of the seven.

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

/** The color a new pin gets when the user has not picked one. */
export const DEFAULT_PIN_COLOR: PinColor = 'red';
