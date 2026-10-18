import { describe, expect, it } from 'vitest';

import { turnFailureCopyText, turnFailureSummary } from '../src/components/chat/TurnFailureBar.vue';
import type { AppNotice } from '../src/api/types';

describe('turnFailureSummary', () => {
  it('should join the notice title and message into one line', () => {
    // Given a structured turn-failure notice
    const notice: AppNotice = {
      severity: 'error',
      title: 'Model API error',
      message: 'OpenAI API error 400 Bad Request: model not supported',
      channel: 'turn-failure',
    };

    // When summarized
    expect(turnFailureSummary(notice)).toBe(
      'Model API error: OpenAI API error 400 Bad Request: model not supported',
    );
  });

  it('should fall back to the title alone when the notice carries no message', () => {
    // Given a notice with only a title
    const notice: AppNotice = { severity: 'error', title: 'Model API error', channel: 'turn-failure' };

    // When summarized
    expect(turnFailureSummary(notice)).toBe('Model API error');
  });
});

describe('turnFailureCopyText', () => {
  it('should include the details block when the notice carries diagnostics', () => {
    // Given a turn-failure notice with diagnostic details
    const notice: AppNotice = {
      severity: 'error',
      title: 'Model API error',
      message: 'boom',
      details: [
        { label: 'Error code', value: 'provider.api_error' },
        { label: 'Retryable', value: 'false' },
      ],
      channel: 'turn-failure',
    };

    // When copied
    const text = turnFailureCopyText(notice);

    // Then the copy carries title, message, and every detail row
    expect(text).toContain('Model API error');
    expect(text).toContain('boom');
    expect(text).toContain('Error code: provider.api_error');
    expect(text).toContain('Retryable: false');
  });

  it('should omit the details block when the notice carries none', () => {
    // Given a turn-failure notice without details
    const notice: AppNotice = { severity: 'error', title: 'Model API error', message: 'boom', channel: 'turn-failure' };

    // When copied
    const text = turnFailureCopyText(notice);

    // Then no diagnostics header is fabricated
    expect(text).not.toContain(':');
    expect(text.split('\n')).toHaveLength(2);
  });
});
