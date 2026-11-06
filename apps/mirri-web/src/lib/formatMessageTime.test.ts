// apps/mirri-web/src/lib/formatMessageTime.test.ts
// The end-of-turn timestamp renders in the reader's local timezone WITHOUT a
// zone label — the footer anchors it to the right edge, and the reader is
// always looking at their own local clock. The assertions target the shape of
// the output, not the zone the test process happens to run in.

import { describe, expect, it } from 'vitest';

import { formatMessageTime } from './formatMessageTime';

describe('formatMessageTime', () => {
  it('should render a same-day moment as bare HH:MM without a zone label', () => {
    // Given a moment a minute ago (always "today" locally)
    const d = new Date(Date.now() - 60_000);
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');

    // When formatted
    const out = formatMessageTime(d.toISOString(), 'Yesterday');

    // Then only the clock time is shown, with no trailing zone text
    expect(out).toBe(`${hh}:${mm}`);
    expect(out).toMatch(/^\d{2}:\d{2}$/);
  });

  it('should prefix yesterday with the supplied label and still omit the zone', () => {
    // Given a moment from yesterday at a fixed local time
    const yesterday = new Date();
    yesterday.setDate(yesterday.getDate() - 1);
    yesterday.setHours(9, 5, 0, 0);

    // When formatted
    const out = formatMessageTime(yesterday.toISOString(), 'Yesterday');

    // Then the label prefixes the clock time and nothing names a zone
    expect(out).toBe('Yesterday 09:05');
  });

  it('should render an older-year moment as YYYY-MM-DD HH:MM without a zone label', () => {
    // Given a moment from an older year
    const iso = '2024-06-15T05:32:00.000Z';
    const d = new Date(iso);
    const pad2 = (n: number) => String(n).padStart(2, '0');

    // When formatted
    const out = formatMessageTime(iso, 'Yesterday');

    // Then the full date form is used and ends on the clock time
    expect(out).toBe(
      `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`,
    );
  });

  it('should return the original string unchanged for invalid input', () => {
    expect(formatMessageTime('not-a-date', 'Yesterday')).toBe('not-a-date');
  });

  it('should never contain a UTC offset or an ASCII zone suffix', () => {
    const iso = new Date().toISOString();

    const out = formatMessageTime(iso, 'Yesterday');

    expect(out).not.toMatch(/UTC|GMT|[+-]\d{2}:\d{2}/);
  });
});
