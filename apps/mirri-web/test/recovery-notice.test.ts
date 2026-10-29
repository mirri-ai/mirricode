// apps/mirri-web/test/recovery-notice.test.ts
// Pure-logic tests for the recovery notice card's wording selection and the
// epoch-ms → relative-time conversion (the sidebar formatTime takes ISO
// strings, so the notice card owns its own number-based conversion — finding
// 9).

import { describe, expect, it } from 'vitest';
import { pickNoticeTitleKey, relativeTimeFromEpoch } from '../src/lib/recoveryNotice';

describe('pickNoticeTitleKey', () => {
  it('should pick the graceful-exit wording when only graceful-exit interruptions are present', () => {
    expect(
      pickNoticeTitleKey([
        { sessionId: 's1', cause: 'graceful_exit' },
        { sessionId: 's2', cause: 'graceful_exit' },
      ]),
    ).toBe('recovery.noticeTitle.graceful_exit');
  });

  it('should pick the service-exit wording when only service-exit interruptions are present', () => {
    expect(pickNoticeTitleKey([{ sessionId: 's1', cause: 'service_exit' }])).toBe(
      'recovery.noticeTitle.service_exit',
    );
  });

  it('should pick the service-exit wording when both causes are present (the unexpected exit wins, C.7)', () => {
    expect(
      pickNoticeTitleKey([
        { sessionId: 's1', cause: 'graceful_exit' },
        { sessionId: 's2', cause: 'service_exit' },
      ]),
    ).toBe('recovery.noticeTitle.service_exit');
  });

  it('should fall back to the graceful-exit wording when the interruption list is empty', () => {
    expect(pickNoticeTitleKey([])).toBe('recovery.noticeTitle.graceful_exit');
  });
});

describe('relativeTimeFromEpoch', () => {
  const now = Date.UTC(2026, 9, 29, 12, 0, 0); // 2026-10-29T12:00:00Z

  it('should return "just now" when the interruption is less than a minute old', () => {
    expect(relativeTimeFromEpoch(now - 30_000, now)).toBe('just now');
  });

  it('should count minutes when the interruption is under an hour old', () => {
    expect(relativeTimeFromEpoch(now - 5 * 60_000, now)).toBe('5m ago');
  });

  it('should count hours when the interruption is under a day old', () => {
    expect(relativeTimeFromEpoch(now - 3 * 3_600_000, now)).toBe('3h ago');
  });

  it('should count days for older interruptions', () => {
    expect(relativeTimeFromEpoch(now - 2 * 86_400_000, now)).toBe('2d ago');
  });

  it('should floor towards the smaller unit boundary rather than round up', () => {
    expect(relativeTimeFromEpoch(now - 59_000, now)).toBe('just now');
    expect(relativeTimeFromEpoch(now - 119_000, now)).toBe('1m ago');
  });

  it('should treat a future timestamp as just now instead of going negative', () => {
    expect(relativeTimeFromEpoch(now + 10_000, now)).toBe('just now');
  });
});
