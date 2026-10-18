// apps/mirri-web/test/session-row-time.test.ts
// Given the sidebar session row's relative-time label (formatTime in
// useMirriWebClient), a fresh session's record can carry an empty or invalid
// updatedAt (the engine's snapshot session view omits the timestamps and the
// mapping layer degrades them to ''). `new Date('')` answers an Invalid Date
// without throwing, so the unguarded formatter rendered "NaNy" (B12 bug 3).

import { describe, expect, it } from 'vitest';

import { formatTime } from '../src/composables/useMirriWebClient';

describe('formatTime — sidebar row relative-time label', () => {
  it('Given an empty updatedAt, When the row time is formatted, Then it falls back to the just-now label instead of "NaNy"', () => {
    expect(formatTime('')).toBe('just now');
  });

  it('Given a non-date string updatedAt, When the row time is formatted, Then it falls back to the just-now label instead of "NaNy"', () => {
    expect(formatTime('not-a-date')).toBe('just now');
  });

  it('Given a valid recent updatedAt, When the row time is formatted, Then the minute bucket renders', () => {
    const fiveMinutesAgo = new Date(Date.now() - 5 * 60_000).toISOString();
    expect(formatTime(fiveMinutesAgo)).toBe('5m');
  });

  it('Given a valid older updatedAt, When the row time is formatted, Then the year bucket renders as a number', () => {
    const twoYearsAgo = new Date(Date.now() - 2 * 365 * 86_400_000).toISOString();
    expect(formatTime(twoYearsAgo)).toBe('2y');
  });
});
