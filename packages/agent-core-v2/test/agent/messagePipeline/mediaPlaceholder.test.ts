/**
 * `messagePipeline` domain — placeholder text builder tests.
 *
 * Pins down the three placeholder reasons and the path/no-path variants so
 * the wording contract is explicit and regressions surface as test failures
 * rather than silent LLM-visible changes.
 */

import { describe, expect, it } from 'vitest';

import { buildMediaPlaceholder } from '#/agent/messagePipeline/mediaPlaceholder';

describe('buildMediaPlaceholder', () => {
  describe('capability_gap reason', () => {
    it('returns the short omission statement when no file path is recorded', () => {
      expect(buildMediaPlaceholder({ kind: 'image', reason: 'capability_gap' })).toBe(
        '[image omitted: current model has no image input]',
      );
    });

    it('includes the file path and three recovery paths when a path is recorded', () => {
      const placeholder = buildMediaPlaceholder({
        kind: 'image',
        reason: 'capability_gap',
        filePath: '/tmp/saved.png',
      });
      expect(placeholder).toContain('[image omitted: current model has no image input]');
      expect(placeholder).toContain('The original image has been saved to: /tmp/saved.png');
      expect(placeholder).toContain('Check if a dedicated multimodal sub-agent is available');
      expect(placeholder).toContain('set the "model" parameter to a model that supports image input');
      expect(placeholder).toContain('suggest they switch to a model with image input capability');
    });

    it('uses the correct kind label for video and audio', () => {
      expect(buildMediaPlaceholder({ kind: 'video', reason: 'capability_gap', filePath: '/v.mp4' })).toContain(
        'current model has no video input',
      );
      expect(buildMediaPlaceholder({ kind: 'audio', reason: 'capability_gap', filePath: '/a.mp3' })).toContain(
        'current model has no audio input',
      );
    });
  });

  describe('size_limit reason', () => {
    it('returns the size-limit omission statement without a path', () => {
      expect(buildMediaPlaceholder({ kind: 'image', reason: 'size_limit' })).toBe(
        '[image omitted: dropped to fit the provider request size limit; re-read the file to view it]',
      );
    });

    it('appends the saved-to path when a path is available', () => {
      const placeholder = buildMediaPlaceholder({
        kind: 'image',
        reason: 'size_limit',
        filePath: '/tmp/big.png',
      });
      expect(placeholder).toContain('dropped to fit the provider request size limit');
      expect(placeholder).toContain('The original image has been saved to: /tmp/big.png');
    });
  });

  describe('format_rejected reason', () => {
    it('returns the format-rejection omission statement without a path', () => {
      expect(buildMediaPlaceholder({ kind: 'image', reason: 'format_rejected' })).toBe(
        '[image omitted for provider compatibility; re-read the file to view it or get conversion guidance]',
      );
    });

    it('appends the saved-to path when a path is available', () => {
      const placeholder = buildMediaPlaceholder({
        kind: 'image',
        reason: 'format_rejected',
        filePath: '/tmp/bad-format.png',
      });
      expect(placeholder).toContain('omitted for provider compatibility');
      expect(placeholder).toContain('The original image has been saved to: /tmp/bad-format.png');
    });
  });
});
