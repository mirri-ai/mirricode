import { visibleWidth } from '@mirri-ai/pi-tui';
import { afterEach, describe, expect, it } from 'vitest';

import { buildUsageReportLines, UsagePanelComponent } from '#/tui/components/messages/usage-panel';
import { currentTheme, darkColors, lightColors } from '#/tui/theme';

afterEach(() => {
  currentTheme.setPalette(darkColors);
});

function strip(text: string): string {
  return text.replaceAll(/\u001B\[[0-9;]*m/g, '');
}

describe('UsagePanelComponent', () => {
  it('formats session and context usage sections', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: {
            inputOther: 1000,
            inputCacheRead: 500,
            inputCacheCreation: 500,
            output: 250,
          },
        },
      },
      contextUsage: 0.25,
      contextTokens: 2500,
      maxContextTokens: 10000,
    }).map(strip);

    expect(lines).toContain('Session usage');
    expect(lines).toContain('  kimi  input 2k  cacheRead 500  output 250  total 2.2k');
    expect(lines).toContain('Context window');
    expect(lines.join('\n')).toContain('25%');
  });

  it('shows a session-level cache hit line derived from the total buckets', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: {
            inputOther: 1000,
            inputCacheRead: 500,
            inputCacheCreation: 500,
            output: 250,
          },
        },
        total: {
          inputOther: 1000,
          inputCacheRead: 500,
          inputCacheCreation: 500,
          output: 250,
        },
      },
      contextUsage: 0.25,
      contextTokens: 2500,
      maxContextTokens: 10000,
    }).map(strip);

    // First content line of the Session usage section (top of the panel).
    expect(lines.indexOf('Session usage')).toBeGreaterThanOrEqual(0);
    expect(lines[lines.indexOf('Session usage') + 1]).toBe('  Cache hit: 25%');
  });

  it('never shows 100% for a partial cache hit (anti-fake-full-hit)', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 1, inputCacheRead: 999, inputCacheCreation: 0, output: 0 },
        },
        total: { inputOther: 1, inputCacheRead: 999, inputCacheCreation: 0, output: 0 },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines).toContain('  Cache hit: 99.9%');
  });

  it('hides the cache hit line when there is no billed input', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: { kimi: { inputOther: 0, inputCacheRead: 0, inputCacheCreation: 0, output: 0 } },
        total: { inputOther: 0, inputCacheRead: 0, inputCacheCreation: 0, output: 0 },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines.join('\n')).not.toContain('Cache hit');
  });

  it('omits the cache hit line when usage is unavailable', () => {
    const lines = buildUsageReportLines({
      sessionUsage: undefined,
      sessionUsageError: 'boom',
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines.join('\n')).not.toContain('Cache hit');
  });

  it('sums by-model buckets for the cache hit line when total is absent', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 500, inputCacheRead: 500, inputCacheCreation: 0, output: 0 },
          other: { inputOther: 0, inputCacheRead: 1500, inputCacheCreation: 0, output: 0 },
        },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    // billed 2500, cacheRead 2000 → 80%.
    expect(lines).toContain('  Cache hit: 80%');
  });

  it('should prefer live session usage for the cache hit line when live usage has billed tokens', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          // Server (main-agent) data would report 500/2000 = 25%.
          kimi: { inputOther: 1000, inputCacheRead: 500, inputCacheCreation: 500, output: 250 },
        },
      },
      // Live session-wide counters say 900/1000 = 90% (includes subagents).
      liveUsage: { inputTokens: 100, outputTokens: 50, cacheReadTokens: 900, cacheCreationTokens: 0 },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines).toContain('  Cache hit: 90%');
    expect(lines).not.toContain('  Cache hit: 25%');
  });

  it('should fall back to server byModel data when live usage is zero', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 500, inputCacheRead: 500, inputCacheCreation: 0, output: 0 },
          other: { inputOther: 0, inputCacheRead: 1500, inputCacheCreation: 0, output: 0 },
        },
      },
      // Resumed session: AppState was never backfilled, so live usage is zero.
      liveUsage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheCreationTokens: 0 },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    // billed 2500, cacheRead 2000 → 80% from the server (main-agent) data.
    expect(lines).toContain('  Cache hit: 80%');
  });

  it('should compute the total row from live usage when live billed tokens exist', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: {
            inputOther: 1000,
            inputCacheRead: 500,
            inputCacheCreation: 500,
            output: 250,
            cacheWriteReported: true,
          },
          twin: {
            inputOther: 0,
            inputCacheRead: 0,
            inputCacheCreation: 512,
            output: 0,
            cacheWriteReported: true,
          },
        },
      },
      liveUsage: { inputTokens: 300, outputTokens: 200, cacheReadTokens: 600, cacheCreationTokens: 100 },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    // Session-wide total: billed input 300+600+100=1000, cacheRead 600,
    // cacheWrite 100, output 200, total 1200 — not the server sums (2.5k etc.).
    expect(lines).toContain(
      '  total  input 1000  cacheRead 600  cacheWrite 100  output 200  total 1.2k',
    );
  });

  it('should show the session-wide footnote when live usage drives the numbers', () => {
    const withLive = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 1000, inputCacheRead: 500, inputCacheCreation: 500, output: 250 },
        },
      },
      liveUsage: { inputTokens: 100, outputTokens: 50, cacheReadTokens: 900, cacheCreationTokens: 0 },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(withLive).toContain(
      '  Cache hit is session-wide (includes subagent usage).',
    );

    const withoutLive = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 500, inputCacheRead: 500, inputCacheCreation: 0, output: 0 },
          other: { inputOther: 0, inputCacheRead: 1500, inputCacheCreation: 0, output: 0 },
        },
      },
      liveUsage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheCreationTokens: 0 },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(withoutLive).not.toContain(
      '  Cache hit is session-wide (includes subagent usage).',
    );
  });

  it('shows a cacheWrite column only for models that report it', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: {
            inputOther: 1000,
            inputCacheRead: 500,
            inputCacheCreation: 500,
            output: 250,
            cacheWriteReported: true,
          },
          other: {
            inputOther: 100,
            inputCacheRead: 0,
            inputCacheCreation: 0,
            output: 50,
          },
        },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines).toContain(
      '  kimi  input 2k  cacheRead 500  cacheWrite 500  output 250  total 2.2k',
    );
    // Non-reporting model keeps column alignment with a dash placeholder.
    expect(lines).toContain(
      '  other  input 100  cacheRead 0  cacheWrite -  output 50  total 150',
    );
    // The summary row cannot sum a mixed-capability write bucket → dash.
    expect(lines).toContain(
      '  total  input 2.1k  cacheRead 500  cacheWrite -  output 300  total 2.3k',
    );
  });

  it('drops the whole cacheWrite column when no model reports it', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: { inputOther: 1000, inputCacheRead: 500, inputCacheCreation: 0, output: 250 },
        },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines).toContain('  kimi  input 1.5k  cacheRead 500  output 250  total 1.7k');
    expect(lines.join('\n')).not.toContain('cacheWrite');
  });

  it('sums the cacheWrite column in the total row when every model reports it', () => {
    const lines = buildUsageReportLines({
      sessionUsage: {
        byModel: {
          kimi: {
            inputOther: 1000,
            inputCacheRead: 500,
            inputCacheCreation: 500,
            output: 250,
            cacheWriteReported: true,
          },
          twin: {
            inputOther: 0,
            inputCacheRead: 0,
            inputCacheCreation: 512,
            output: 0,
            cacheWriteReported: true,
          },
        },
      },
      contextUsage: 0,
      contextTokens: 0,
      maxContextTokens: 0,
    }).map(strip);

    expect(lines).toContain(
      '  total  input 2.5k  cacheRead 500  cacheWrite 1012  output 250  total 2.7k',
    );
  });

  it('wraps preformatted usage lines in a bordered panel', () => {
    const component = new UsagePanelComponent(() => ['Session usage'], 'primary');
    const output = component.render(80).map(strip);

    expect(output[0]).toContain(' Usage ');
    expect(output[1]).toContain('Session usage');
  });

  it('truncates lines wider than the terminal so the panel never overflows', () => {
    const longLine = 'error: ' + 'x'.repeat(200);
    const component = new UsagePanelComponent(() => [longLine], 'primary');
    const width = 60;

    const output = component.render(width);

    for (const line of output) {
      expect(visibleWidth(line)).toBeLessThanOrEqual(width);
    }
  });

  it('keeps the bordered panel within narrow terminal widths', () => {
    const component = new UsagePanelComponent(() => ['Session usage', '  kimi  input 2.0k'], 'primary');

    for (const width of [39, 24, 20, 10, 4, 1]) {
      for (const line of component.render(width)) {
        expect(visibleWidth(line)).toBeLessThanOrEqual(width);
      }
    }
  });

  it('rebuilds its body from the active palette on invalidate', () => {
    // Emit the resolved palette value as visible text so the assertion holds
    // regardless of chalk's colour level in the test environment.
    const component = new UsagePanelComponent(() => [`text=${currentTheme.color('text')}`], 'primary');
    const bodyOf = (): string => {
      const line = component.render(80).map(strip).find((l) => l.includes('text='));
      if (line === undefined) throw new Error('body line not found');
      return line;
    };

    expect(bodyOf()).toContain(darkColors.text);
    currentTheme.setPalette(lightColors);
    component.invalidate();
    expect(bodyOf()).toContain(lightColors.text);
  });
});
