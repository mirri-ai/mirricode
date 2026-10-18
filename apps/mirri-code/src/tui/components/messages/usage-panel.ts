/**
 * UsagePanelComponent — wraps pre-coloured `/usage` lines in a blue box
 * border with a left indent, mirroring the PlanBoxComponent layout so
 * the pattern stays consistent across command-triggered panels.
 */

import type { Component } from '@mirri-ai/pi-tui';
import { truncateToWidth, visibleWidth } from '@mirri-ai/pi-tui';
import type { SessionUsage, TokenUsage } from '@mirri-ai/mirri-code-sdk';

import {
  formatCacheHitPercent,
  formatTokenCount,
  ratioSeverity,
  renderProgressBar,
  safeUsageRatio,
  usagePercent,
} from '#/utils/usage/usage-format';
import { currentTheme } from '#/tui/theme';
import type { ColorToken } from '#/tui/theme';
import type { SessionUsage as LiveSessionUsage } from '#/tui/types';

const LEFT_MARGIN = 2;
const SIDE_PADDING = 1;
const BOX_OVERHEAD = LEFT_MARGIN + 2 + 2 * SIDE_PADDING;

type Colorize = (text: string) => string;

export interface UsageReportOptions {
  readonly sessionUsage?: SessionUsage;
  readonly sessionUsageError?: string;
  /** Live session-cumulative usage (TUI-local flat shape) accumulated from
   *  `turn.step.completed` events — includes subagent usage, unlike the
   *  server `session.getUsage()` report. Preferred for the session-level
   *  cache-hit line and the summary total row whenever it has billed
   *  tokens. */
  readonly liveUsage?: LiveSessionUsage;
  readonly contextUsage: number;
  readonly contextTokens: number;
  readonly maxContextTokens: number;
}

function usageNumber(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : 0;
}

function usageInputTotal(usage: TokenUsage): number {
  return (
    usageNumber(usage.inputOther) +
    usageNumber(usage.inputCacheRead) +
    usageNumber(usage.inputCacheCreation)
  );
}

function reportsCacheWrite(usage: TokenUsage): boolean {
  return usage.cacheWriteReported === true;
}

/** Billed prompt tokens of the TUI-local flat session usage (SDR-001
 *  denominator): uncached input + cache read + cache write. */
function liveUsageBilled(live: LiveSessionUsage | undefined): number {
  if (live === undefined) return 0;
  return (
    usageNumber(live.inputTokens) +
    usageNumber(live.cacheReadTokens) +
    usageNumber(live.cacheCreationTokens)
  );
}

/**
 * Session-level cache hit percentage line (spec US2 / SDR-001 anti-100).
 * Data-source precedence: the TUI-local `liveUsage` (accumulated from
 * `turn.step.completed` events, so it includes subagent usage — the same
 * number the footer's stats segment shows) wins whenever it has billed
 * input; otherwise the server `session.getUsage()` report is used (`total`
 * buckets when present, falling back to summing the by-model entries —
 * main-agent only, which covers resumed sessions where AppState is not
 * backfilled yet). Anti-100 is built into `formatCacheHitPercent`; a null
 * result (no billed input) omits the line entirely.
 */
function buildCacheHitLine(
  usage: SessionUsage | undefined,
  liveUsage: LiveSessionUsage | undefined,
): string | null {
  const liveBilled = liveUsageBilled(liveUsage);
  if (liveBilled > 0) {
    const hit = formatCacheHitPercent(usageNumber(liveUsage!.cacheReadTokens), liveBilled);
    return hit === null ? null : `  Cache hit: ${hit}%`;
  }
  if (usage === undefined) return null;
  let cacheRead: number;
  let billed: number;
  const total = usage.total;
  if (total !== undefined) {
    cacheRead = usageNumber(total.inputCacheRead);
    billed = usageInputTotal(total);
  } else {
    const entries = Object.values(usage.byModel ?? {});
    if (entries.length === 0) return null;
    cacheRead = 0;
    billed = 0;
    for (const row of entries) {
      cacheRead += usageNumber(row.inputCacheRead);
      billed += usageInputTotal(row);
    }
  }
  const hit = formatCacheHitPercent(cacheRead, billed);
  return hit === null ? null : `  Cache hit: ${hit}%`;
}

function buildSessionUsageSection(
  usage: SessionUsage | undefined,
  error: string | undefined,
  liveUsage: LiveSessionUsage | undefined,
  value: Colorize,
  muted: Colorize,
  errorStyle: Colorize,
): string[] {
  if (error !== undefined) return [errorStyle(`  ${error}`)];
  const byModel = (usage as { readonly byModel?: Record<string, TokenUsage> } | undefined)
    ?.byModel;
  const entries = Object.entries(byModel ?? {});
  if (entries.length === 0) return [muted('  No token usage recorded yet.')];

  // Live session-wide usage (includes subagent tokens) drives the hit line
  // and the summary row whenever it has billed input; the per-model rows stay
  // server (main-agent) data.
  const liveBilled = liveUsageBilled(liveUsage);

  const cacheHitLine = buildCacheHitLine(usage, liveUsage);
  const lines: string[] = cacheHitLine === null ? [] : [value(cacheHitLine)];

  // The write column exists only when at least one model's protocol carries
  // a cache-write bucket (SDR-003): a false capability bit means the zero is
  // a protocol gap, not a real zero, so it renders as a `-` placeholder.
  const showCacheWrite = entries.some(([, row]) => reportsCacheWrite(row));
  const cacheWriteCell = (row: TokenUsage): string =>
    reportsCacheWrite(row) ? formatTokenCount(usageNumber(row.inputCacheCreation)) : '-';

  let totalInput = 0;
  let totalCacheRead = 0;
  let totalCacheWrite = 0;
  let totalOutput = 0;
  for (const [model, row] of entries) {
    const input = usageInputTotal(row);
    const output = usageNumber(row.output);
    totalInput += input;
    totalCacheRead += usageNumber(row.inputCacheRead);
    totalCacheWrite += usageNumber(row.inputCacheCreation);
    totalOutput += output;
    const cells = [
      `input ${value(formatTokenCount(input))}`,
      `cacheRead ${value(formatTokenCount(usageNumber(row.inputCacheRead)))}`,
    ];
    if (showCacheWrite) cells.push(`cacheWrite ${value(cacheWriteCell(row))}`);
    cells.push(
      `output ${value(formatTokenCount(output))}`,
      `total ${value(formatTokenCount(input + output))}`,
    );
    lines.push(`  ${muted(model)}  ${cells.join('  ')}`);
  }
  if (entries.length > 1) {
    let cells: string[];
    if (liveBilled > 0) {
      // Session-wide summary from the live counters (includes subagents).
      // The write cell shows the raw accumulated sum rather than the dash
      // placeholder used in the server branch: the live counters are a real
      // accumulation, and the per-model capability flags only cover the main
      // agent, so a dash here would imply a gap the live data does not have.
      cells = [
        `input ${value(formatTokenCount(liveBilled))}`,
        `cacheRead ${value(formatTokenCount(usageNumber(liveUsage!.cacheReadTokens)))}`,
      ];
      if (showCacheWrite) cells.push(`cacheWrite ${value(formatTokenCount(usageNumber(liveUsage!.cacheCreationTokens)))}`);
      cells.push(
        `output ${value(formatTokenCount(usageNumber(liveUsage!.outputTokens)))}`,
        `total ${value(formatTokenCount(liveBilled + usageNumber(liveUsage!.outputTokens)))}`,
      );
    } else {
      cells = [
        `input ${value(formatTokenCount(totalInput))}`,
        `cacheRead ${value(formatTokenCount(totalCacheRead))}`,
      ];
      // A mixed-capability session cannot sum the write bucket (a false entry
      // would understate it), so the summary row keeps the dash placeholder.
      const totalWriteCell = entries.every(([, row]) => reportsCacheWrite(row))
        ? formatTokenCount(totalCacheWrite)
        : '-';
      if (showCacheWrite) cells.push(`cacheWrite ${value(totalWriteCell)}`);
      cells.push(
        `output ${value(formatTokenCount(totalOutput))}`,
        `total ${value(formatTokenCount(totalInput + totalOutput))}`,
      );
    }
    lines.push(`  ${muted('total')}  ${cells.join('  ')}`);
  }
  if (liveBilled > 0) {
    // The per-model rows above are server (main-agent) data while the hit
    // line is session-wide — flag the mixed scope. The hit line is the only
    // live-derived element guaranteed on screen (the total row renders only
    // for multi-model sessions), so the note is phrased around it.
    lines.push(muted('  Cache hit is session-wide (includes subagent usage).'));
  }
  return lines;
}

function severityColor(sev: 'ok' | 'warn' | 'danger'): 'success' | 'warning' | 'error' {
  return sev === 'danger' ? 'error' : sev === 'warn' ? 'warning' : 'success';
}

export function buildUsageReportLines(options: UsageReportOptions): string[] {
  const accent = (text: string) => currentTheme.boldFg('primary', text);
  const value = (text: string) => currentTheme.fg('text', text);
  const muted = (text: string) => currentTheme.fg('textDim', text);
  const errorStyle = (text: string) => currentTheme.fg('error', text);

  const lines: string[] = [
    accent('Session usage'),
    ...buildSessionUsageSection(
      options.sessionUsage,
      options.sessionUsageError,
      options.liveUsage,
      value,
      muted,
      errorStyle,
    ),
  ];

  if (options.maxContextTokens > 0) {
    const ratio = safeUsageRatio(options.contextUsage);
    const bar = renderProgressBar(ratio, 20);
    const pct = `${String(usagePercent(options.contextTokens, options.maxContextTokens))}%`;
    const barColoured = currentTheme.fg(severityColor(ratioSeverity(ratio)), bar);
    lines.push('');
    lines.push(accent('Context window'));
    lines.push(
      `  ${barColoured}  ${value(pct.padStart(6, ' '))}  ` +
        muted(
          `(${formatTokenCount(options.contextTokens)} / ${formatTokenCount(
            options.maxContextTokens,
          )})`,
        ),
    );
  }

  return lines;
}

export class UsagePanelComponent implements Component {
  /** Cached coloured lines; rebuilt from `buildLines` on every invalidate. */
  private lines: readonly string[];

  constructor(
    private readonly buildLines: () => readonly string[],
    private readonly borderToken: ColorToken,
    private readonly title: string = ' Usage ',
  ) {
    this.lines = buildLines();
  }

  invalidate(): void {
    // Report bodies embed palette colours, so a theme switch must re-run the
    // builder to repaint the cached lines (the data itself is captured).
    this.lines = this.buildLines();
  }

  render(width: number): string[] {
    const safeWidth = Math.max(0, width);
    if (safeWidth <= 0) return [''];

    const paint = (s: string): string => currentTheme.fg(this.borderToken, s);
    const availableInterior = safeWidth - BOX_OVERHEAD;
    if (availableInterior < 1) {
      return [
        truncateToWidth(this.title.trim(), safeWidth, '…'),
        ...this.lines.map((line) => truncateToWidth(line, safeWidth, '…')),
      ];
    }

    const indent = ' '.repeat(LEFT_MARGIN);
    const longestLine = this.lines.reduce((max, line) => Math.max(max, visibleWidth(line)), 0);
    const contentWidth = Math.max(
      1,
      Math.min(availableInterior, Math.max(longestLine, visibleWidth(this.title))),
    );
    const horzLen = contentWidth + 2 * SIDE_PADDING;
    const title = truncateToWidth(this.title, horzLen, '…');

    const trailingDashLen = Math.max(0, horzLen - visibleWidth(title));
    const top =
      indent + paint('╭') + paint(title) + paint('─'.repeat(trailingDashLen)) + paint('╮');
    const bottom = indent + paint('╰' + '─'.repeat(horzLen) + '╯');

    const out: string[] = [top];
    for (const line of this.lines) {
      const clipped = visibleWidth(line) > contentWidth ? truncateToWidth(line, contentWidth) : line;
      const pad = Math.max(0, contentWidth - visibleWidth(clipped));
      out.push(indent + paint('│') + ' ' + clipped + ' '.repeat(pad) + ' ' + paint('│'));
    }
    out.push(bottom);
    return out.map((line) => truncateToWidth(line, safeWidth, '…'));
  }
}
