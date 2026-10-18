import { describe, it, expect } from 'vitest';

import { FooterComponent, formatUsageStatus } from '#/tui/components/chrome/footer';
import type { AppState, SessionUsage } from '#/tui/types';

const ANSI_SGR = /\u001B\[[0-9;]*m/g;
function strip(text: string): string {
  return text.replaceAll(ANSI_SGR, '');
}

function usage(overrides: Partial<SessionUsage> = {}): SessionUsage {
  return {
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheCreationTokens: 0,
    ...overrides,
  };
}

function baseState(overrides: Partial<AppState> = {}): AppState {
  return {
    model: 'k2',
    workDir: '/tmp',
    additionalDirs: [],
    sessionId: 'sess_1',
    permissionMode: 'manual',
    planMode: false,
    thinkingEffort: 'off',
    contextUsage: 0,
    contextTokens: 0,
    maxContextTokens: 0,
    isCompacting: false,
    isReplaying: false,
    streamingPhase: 'idle',
    streamingStartTime: 0,
    theme: 'dark',
    version: 'test',
    editorCommand: null,
    notifications: { enabled: true, condition: 'unfocused' },
    availableModels: {},
    sessionUsage: usage(),
    ...overrides,
  } as AppState;
}

// The footer's usage stats segment (usage-visibility Task 8): a session-level
// cache hit percentage plus in/out token totals appended after the context
// readout on line 2. The billing denominator is the three disjoint prompt-side
// buckets (uncached input + cache read + cache write) — the same denominator
// the hit percentage uses. No billed input → the whole segment disappears
// rather than showing a fake "cache 0%".
describe('formatUsageStatus', () => {
  it('renders hit percent plus in/out totals with 1024-based counts', () => {
    // billed = 24_576 + 950_272 + 0 = 974_848 (952k); hit = 950_272/974_848 ≈ 97%
    expect(formatUsageStatus(usage({ inputTokens: 24_576, outputTokens: 3_277, cacheReadTokens: 950_272 }))).toBe(
      'cache 97% · in 952k · out 3.2k',
    );
  });

  it('uses the billing denominator (not the raw uncached input) for "in"', () => {
    expect(
      formatUsageStatus(usage({ inputTokens: 1_024, cacheReadTokens: 2_048, cacheCreationTokens: 4_096 })),
    ).toBe('cache 29% · in 7k · out 0');
  });

  it('never shows a fake 100% for a partial hit', () => {
    // 999_999/1_000_000 at the default 0 dp would round to 100 → deepens.
    const out = formatUsageStatus(usage({ inputTokens: 1, cacheReadTokens: 999_999 }));
    expect(out).toMatch(/^cache 99\.9+% · /);
  });

  it('disappears when there is no billed input', () => {
    expect(formatUsageStatus(usage())).toBe('');
    expect(formatUsageStatus(usage({ outputTokens: 5_000 }))).toBe('');
  });
});

describe('FooterComponent — usage stats segment', () => {
  it('appends the stats segment after the context readout on line 2', () => {
    const fc = new FooterComponent(
      baseState({
        contextTokens: 178_000,
        maxContextTokens: 524_288,
        sessionUsage: usage({
          inputTokens: 25_088,
          outputTokens: 3_277,
          cacheReadTokens: 950_272,
        }),
      }),
    );
    const [, line2] = fc.render(200);
    const text = strip(line2 ?? '');
    expect(text).toMatch(/context: \d+%/);
    expect(text).toContain(' | cache ');
    expect(text).toMatch(/cache \d+% · in \S+ · out \S+$/);
    // The stats segment follows the context segment (never the other way).
    expect(text.indexOf('context:')).toBeLessThan(text.indexOf('cache '));
  });

  it('hides the stats segment when there is no billed input', () => {
    const fc = new FooterComponent(
      baseState({
        contextTokens: 178_000,
        maxContextTokens: 524_288,
        sessionUsage: usage({ outputTokens: 3_277 }),
      }),
    );
    const [, line2] = fc.render(200);
    const text = strip(line2 ?? '');
    expect(text).toMatch(/context: \d+%/);
    expect(text).not.toContain('cache ');
    expect(text).not.toContain(' · in ');
  });

  it('keeps the segment visible alongside a transient hint', () => {
    const fc = new FooterComponent(
      baseState({
        sessionUsage: usage({ inputTokens: 1_024, outputTokens: 512, cacheReadTokens: 3_072 }),
      }),
    );
    fc.setTransientHint('Press Ctrl-C again to exit');
    const [, line2] = fc.render(200);
    const text = strip(line2 ?? '');
    expect(text).toContain('Press Ctrl-C again to exit');
    expect(text).toContain('cache ');
  });
});
