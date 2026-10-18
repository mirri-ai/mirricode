import { describe, expect, it } from 'vitest';

import { billedInputTokens, formatCacheHitPercent, formatExactTokens } from './usageStats';

describe('usageStats', () => {
  it('should sum the three prompt-side buckets when computing billed input', () => {
    expect(billedInputTokens({ inputTokens: 72_543, outputTokens: 3_000, cacheReadTokens: 1_120_576, cacheCreationTokens: 0 })).toBe(1_193_119);
  });

  it('should return null when there is no billed input instead of inventing zero', () => {
    expect(formatCacheHitPercent(0, 0)).toBeNull();
  });

  it('should show one hundred only for a true full hit', () => {
    expect(formatCacheHitPercent(1_000, 1_000)).toBe('100');
  });

  it('should deepen precision instead of rounding a partial hit up to one hundred', () => {
    expect(formatCacheHitPercent(999, 1_000, 0)).toBe('99.9');
    expect(formatCacheHitPercent(999_999, 1_000_000, 1)).toBe('99.9999');
  });

  it('should render two-decimal precision for session level by default', () => {
    expect(formatCacheHitPercent(939, 1_000)).toBe('93.90');
  });

  it('should format exact token counts with thousands separators', () => {
    expect(formatExactTokens(15842)).toBe('15,842');
  });
});