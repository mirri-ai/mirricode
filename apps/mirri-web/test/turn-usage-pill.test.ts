// test/turn-usage-pill.test.ts
//
// TurnUsagePill row-building logic (usage-visibility Task 6). The repo has no
// @vue/test-utils, so the component's row construction is a named pure export
// (`buildTurnUsageRows`) and tested directly:
// - a bucket that was never reported renders no row (honest omission),
// - the cache-write row is gated on a non-zero write bucket (SDR-003
//   stopgap for the missing model→capability map),
// - the hit-rate denominator always includes the write bucket (SDR-001),
// - a full cache hit renders exactly 100, a partial one never does (SDR-001 d),
// - the model row lists the turn's models.

import { describe, expect, it } from 'vitest';
import { buildTurnUsageRows } from '../src/components/chat/TurnUsagePill.vue';
import type { TurnUsage } from '../src/api/daemon/agentEventProjector';

function usage(partial: Partial<TurnUsage> = {}): TurnUsage {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    models: [],
    ...partial,
  };
}

describe('buildTurnUsageRows', () => {
  it('should omit the cache-write row when the write bucket is zero', () => {
    const rows = buildTurnUsageRows(usage({ inputTokens: 100, outputTokens: 5 }));
    expect(rows.map((r) => r.key)).toEqual(['input', 'cacheRead', 'output']);
  });

  it('should show the cache-write row when the write bucket is non-zero', () => {
    const rows = buildTurnUsageRows(usage({ inputTokens: 100, cacheCreationTokens: 40 }));
    expect(rows.find((r) => r.key === 'cacheWrite')?.value).toBe('40');
  });

  it('should include cache creation in the hit denominator when writes were reported', () => {
    // billed = 100 input + 512 read + 388 write = 1000 → hit = 51.2%
    const rows = buildTurnUsageRows(usage({ inputTokens: 100, cacheReadTokens: 512, cacheCreationTokens: 388 }));
    expect(rows.find((r) => r.key === 'cacheHit')?.value).toBe('51.2%');
  });

  it('should include cache creation in the hit denominator even when no capability wiring exists', () => {
    // SDR-001: the denominator is unconditional — a turn that only wrote
    // cache (no uncached input, no reads) must not render a 100% hit.
    const rows = buildTurnUsageRows(usage({ cacheReadTokens: 0, cacheCreationTokens: 500 }));
    expect(rows.some((r) => r.key === 'cacheHit')).toBe(false);
  });

  it('should omit the cache-hit row when nothing was read from cache', () => {
    const rows = buildTurnUsageRows(usage({ inputTokens: 100 }));
    expect(rows.some((r) => r.key === 'cacheHit')).toBe(false);
  });

  it('should render a full cache hit as exactly 100 when the whole bill was cache reads', () => {
    const rows = buildTurnUsageRows(usage({ cacheReadTokens: 512 }));
    expect(rows.find((r) => r.key === 'cacheHit')?.value).toBe('100%');
  });

  it('should never render a partial hit as 100 when billed tokens exceed cache reads', () => {
    const rows = buildTurnUsageRows(usage({ cacheReadTokens: 99999, cacheCreationTokens: 1 }));
    const hit = rows.find((r) => r.key === 'cacheHit')?.value ?? '';
    expect(hit.endsWith('%')).toBe(true);
    expect(hit).not.toBe('100%');
    expect(hit).toBe('99.999%');
  });

  it('should list the turn models first when models were recorded', () => {
    const rows = buildTurnUsageRows(usage({ models: ['claude-a', 'claude-b'] }));
    expect(rows[0]).toEqual({ key: 'model', value: 'claude-a, claude-b' });
  });

  it('should omit the model row when no model was recorded', () => {
    const rows = buildTurnUsageRows(usage({}));
    expect(rows.some((r) => r.key === 'model')).toBe(false);
  });

  it('should format token counts with exact grouping when values are large', () => {
    const rows = buildTurnUsageRows(usage({ inputTokens: 1234567, outputTokens: 89 }));
    expect(rows.find((r) => r.key === 'input')?.value).toBe('1,234,567');
    expect(rows.find((r) => r.key === 'output')?.value).toBe('89');
  });
});
