// apps/mirri-web/src/lib/suspendedReason.test.ts
import { describe, expect, it } from 'vitest';
import { suspendedReasonText } from './suspendedReason';

describe('suspendedReasonText', () => {
  it('should return the legacy prose verbatim when the reason is a string', () => {
    expect(suspendedReasonText('Provider rate limit; subagent requeued for retry.')).toBe(
      'Provider rate limit; subagent requeued for retry.',
    );
  });

  it('should return an empty string when the reason is a structured object (the phase badge carries the state)', () => {
    expect(suspendedReasonText({ kind: 'approval', requestId: 'r1', toolCallId: 't1' })).toBe('');
    expect(suspendedReasonText({ kind: 'question', requestId: 'q1' })).toBe('');
    expect(suspendedReasonText({ kind: 'rate_limit' })).toBe('');
  });

  it('should return an empty string when the reason is absent', () => {
    expect(suspendedReasonText(undefined)).toBe('');
  });
});