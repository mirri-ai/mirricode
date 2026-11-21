// apps/mirri-web/test/greetings.test.ts
// The landing greeting is picked from a time-of-day bucket. These cases pin the
// bucket boundaries (the one thing that drifts when the copy is edited) and the
// twice-a-page invariants: the clock always lands in a bucket, and the same
// clock always yields a line from that bucket's own pool.

import { describe, expect, it } from 'vitest';
import { greetingPeriodAt, pickGreeting } from '../src/components/chat/greetings';

/** A local-time date at the given hour/minute — the buckets read local hours. */
function at(hour: number, minute = 0): Date {
  return new Date(2026, 10, 21, hour, minute, 0, 0);
}

describe('greetingPeriodAt', () => {
  it('should map every boundary hour to the period that starts there', () => {
    expect(greetingPeriodAt(at(5))).toBe('morning');
    expect(greetingPeriodAt(at(9))).toBe('forenoon');
    expect(greetingPeriodAt(at(12))).toBe('noon');
    expect(greetingPeriodAt(at(14))).toBe('afternoon');
    expect(greetingPeriodAt(at(17))).toBe('evening_offwork');
    expect(greetingPeriodAt(at(19))).toBe('night');
    expect(greetingPeriodAt(at(22))).toBe('late_night');
  });

  it('should hold each period until the next boundary is reached', () => {
    expect(greetingPeriodAt(at(8, 59))).toBe('morning');
    expect(greetingPeriodAt(at(11, 59))).toBe('forenoon');
    expect(greetingPeriodAt(at(13, 59))).toBe('noon');
    expect(greetingPeriodAt(at(16, 59))).toBe('afternoon');
    expect(greetingPeriodAt(at(18, 59))).toBe('evening_offwork');
    expect(greetingPeriodAt(at(21, 59))).toBe('night');
    expect(greetingPeriodAt(at(23, 59))).toBe('late_night');
  });

  it('should keep late night across midnight through the small hours', () => {
    expect(greetingPeriodAt(at(0))).toBe('late_night');
    expect(greetingPeriodAt(at(4, 59))).toBe('late_night');
  });
});

describe('pickGreeting', () => {
  it('should return a non-empty line from the matching bucket when the clock is inside it', () => {
    const line = pickGreeting(at(10), 'zh');
    expect(line.length).toBeGreaterThan(0);
    // Same clock, every possible roll: always the forenoon pool.
    const forenoon = new Set(['上午好，打算做些什么？', '有开发想法直接说', '上午好，我随时待命', '想好需求，我来实现', '尽管提，我帮你编码']);
    for (const rounded of [0, 0.2, 0.4, 0.6, 0.999]) {
      expect(forenoon.has(pickGreeting(at(10), 'zh', () => rounded))).toBe(true);
    }
  });

  it('should answer in English when the locale is not Chinese', () => {
    const line = pickGreeting(at(10), 'en', () => 0);
    expect(line).toBe('Morning — what’s the plan?');
  });

  it('should switch language with the locale prefix rather than falling back to English', () => {
    expect(pickGreeting(at(10), 'zh-CN', () => 0)).toBe('上午好，打算做些什么？');
    expect(pickGreeting(at(10), 'en-US', () => 0)).toBe('Morning — what’s the plan?');
  });

  it('should stay inside the pool when the random source returns exactly 1', () => {
    // Math.random is exclusive of 1, but an injected source (or a future
    // switch to a shared RNG) may not be — an out-of-range index would hand the
    // template `undefined`.
    const line = pickGreeting(at(20), 'zh', () => 1);
    expect(typeof line).toBe('string');
    expect(line.length).toBeGreaterThan(0);
  });

  it('should return a greeting from every bucket when sweeping all 24 hours', () => {
    for (let hour = 0; hour < 24; hour++) {
      const zh = pickGreeting(at(hour), 'zh');
      const en = pickGreeting(at(hour), 'en');
      expect(zh.length).toBeGreaterThan(0);
      expect(en.length).toBeGreaterThan(0);
      // Chinese copy for a zh locale must actually be Chinese.
      expect(zh).toMatch(/[\u4E00-\u9FFF]/);
    }
  });
});
