// apps/mirri-web/src/components/chatTurnRendering.ts
// Pure turn-rendering helpers: pure functions of their arguments (no Vue
// reactivity, no component state). Shared by ChatPane.vue's template and its
// stateful copy/edit helpers.
import type { ChatTurn, NotificationTurnData, TurnBlock } from '../types';

// Shared 1024-based token formatter (lib/formatTokens); re-exported so the
// existing ChatPane import keeps working.
export { formatTokens } from '../lib/formatTokens';

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const s = ((ms % 60_000) / 1000).toFixed(1);
  return `${m}m${s}s`;
}

// Ordered render blocks for an assistant turn. messagesToTurns supplies `blocks`
// (thinking + text + tool cards in call order); fall back to deriving them from
// the aggregate fields for any turn built without blocks (e.g. unit tests).
export function turnBlocks(turn: ChatTurn): TurnBlock[] {
  if (turn.blocks) return turn.blocks;
  const blocks: TurnBlock[] = [];
  if (turn.thinking) blocks.push({ kind: 'thinking', thinking: turn.thinking });
  if (turn.text) blocks.push({ kind: 'text', text: turn.text });
  for (const tool of turn.tools ?? []) blocks.push({ kind: 'tool', tool });
  return blocks;
}

export type ToolStackPosition = 'single' | 'first' | 'middle' | 'last';

export type ToolStackItem = {
  tool: Extract<TurnBlock, { kind: 'tool' }>['tool'];
  sourceIndex: number;
};

export type AssistantRenderBlock =
  | { kind: 'thinking'; thinking: string; sourceIndex: number }
  | { kind: 'text'; text: string; sourceIndex: number }
  | { kind: 'tool'; tool: ToolStackItem['tool']; sourceIndex: number }
  | { kind: 'tool-stack'; tools: ToolStackItem[] }
  | { kind: 'notification'; text: string; notification: NotificationTurnData; sourceIndex: number };

export function rendersToolCard(block: Extract<TurnBlock, { kind: 'tool' }>): boolean {
  return !(block.tool.status === 'ok' && block.tool.media);
}

export function toolStackPosition(index: number, count: number): ToolStackPosition {
  if (count <= 1) return 'single';
  if (index === 0) return 'first';
  if (index === count - 1) return 'last';
  return 'middle';
}

export function assistantRenderBlocks(turn: ChatTurn): AssistantRenderBlock[] {
  return renderBlocksFromBlocks(turnBlocks(turn));
}

/** Render-plan over a raw ordered block list (no ChatTurn wrapper) — the
 *  member panel's narrative path (ADR-009) feeds blocks directly. */
export function renderBlocksFromBlocks(blocks: TurnBlock[]): AssistantRenderBlock[] {
  const rendered: AssistantRenderBlock[] = [];
  let toolRun: ToolStackItem[] = [];

  const flushToolRun = () => {
    if (toolRun.length === 1) {
      const [item] = toolRun;
      if (item) rendered.push({ kind: 'tool', tool: item.tool, sourceIndex: item.sourceIndex });
    } else if (toolRun.length > 1) {
      rendered.push({ kind: 'tool-stack', tools: toolRun });
    }
    toolRun = [];
  };

  blocks.forEach((block, sourceIndex) => {
    if (block.kind === 'tool') {
      if (rendersToolCard(block)) {
        toolRun.push({ tool: block.tool, sourceIndex });
        return;
      }
      flushToolRun();
      rendered.push({ kind: 'tool', tool: block.tool, sourceIndex });
      return;
    }

    flushToolRun();
    if (block.kind === 'thinking') {
      rendered.push({ kind: 'thinking', thinking: block.thinking, sourceIndex });
    } else if (block.kind === 'text') {
      rendered.push({ kind: 'text', text: block.text, sourceIndex });
    } else if (block.kind === 'notification') {
      rendered.push({
        kind: 'notification',
        text: block.text,
        notification: block.notification,
        sourceIndex,
      });
    }
  });

  flushToolRun();
  return rendered;
}

export function turnFinalText(turn: ChatTurn): string {
  return turnBlocks(turn)
    .flatMap((blk) => (blk.kind === 'text' && blk.text ? [blk.text] : []))
    .join('\n\n');
}

// ---- Assistant-run grouping + collapse plans ---------------------------------
// Completed assistant runs auto-collapse in the conversation view (mirroring
// the lynx app's TurnView): a compact header + the run's final text stay
// visible, while thinking blocks, tool cards and intermediate text fold away
// until the user expands the run. These helpers are pure functions of their
// arguments — no Vue reactivity, no component state.

/**
 * A run of CONSECUTIVE assistant turns in the transcript. Any non-assistant
 * turn (user, compaction, modelSwitch, cron, notification, …) splits runs.
 */
export interface AssistantRun {
  /** Consecutive assistant turns in transcript order. */
  turns: ChatTurn[];
  /** Index of the run's first turn in the full turns array. */
  startIndex: number;
  /** Index of the run's last turn (the run-end turn). */
  endIndex: number;
}

/** Group consecutive assistant turns into runs; any non-assistant turn splits. */
export function groupAssistantRuns(turns: ChatTurn[]): AssistantRun[] {
  const runs: AssistantRun[] = [];
  let start = -1;
  for (let i = 0; i <= turns.length; i++) {
    const isAssistant = i < turns.length && turns[i]?.role === 'assistant';
    if (isAssistant) {
      if (start === -1) start = i;
    } else if (start !== -1) {
      runs.push({ turns: turns.slice(start, i), startIndex: start, endIndex: i - 1 });
      start = -1;
    }
  }
  return runs;
}

/**
 * The run's final text, shown outside the fold. The lynx terminal-text rule
 * at run granularity: only the run's last turn's TERMINAL block may be pulled
 * out, and only when it is a text block. If the run ends with a tool call, no
 * earlier text block is pulled forward (it would silently drop everything
 * after it) — returns ''.
 *
 * Trailing notification blocks are skipped when finding the terminal block:
 * historical sessions (pre-core-fix replays) folded background-task notices
 * onto the turn tail after the final summary text, and that summary must stay
 * visible instead of showing "No text output".
 */
export function runFinalText(run: AssistantRun): string {
  const last = run.turns.at(-1);
  if (!last) return '';
  const blocks = turnBlocks(last);
  let i = blocks.length - 1;
  while (i >= 0 && blocks[i]?.kind === 'notification') i -= 1;
  const terminal = i >= 0 ? blocks[i] : undefined;
  return terminal !== undefined && terminal.kind === 'text' ? terminal.text : '';
}

/**
 * Per-turn fold decision for the auto-collapse rules:
 *
 * - Completed runs (no turn in the run is the streaming turn) fold unless the
 *   user has expanded the run (its first-turn id is in `expandedRunKeys`).
 * - The streaming run always renders fully expanded, live.
 * - Folded runs render only their first turn (the collapse header row) and
 *   their last turn (the final text + run-end footer); middle turns emit no
 *   DOM at all.
 *
 * Returns null for non-assistant turns.
 */
export function assistantTurnRenderPlan(
  turns: ChatTurn[],
  index: number,
  streamingTurnId: string | null,
  expandedRunKeys: ReadonlySet<string>,
): AssistantTurnRenderPlan | null {
  const turn = turns[index];
  if (!turn || turn.role !== 'assistant') return null;

  // Walk backward/forward to find this turn's run bounds (runs are usually a
  // single turn, so this is O(run length) per call).
  let start = index;
  while (start > 0 && turns[start - 1]?.role === 'assistant') start -= 1;
  let end = index;
  while (end < turns.length - 1 && turns[end + 1]?.role === 'assistant') end += 1;

  const runTurns = turns.slice(start, end + 1);
  const first = runTurns[0]!;
  const last = runTurns.at(-1)!;
  const streaming = runTurns.some((t) => t.id === streamingTurnId);
  const folded = !streaming && !expandedRunKeys.has(first.id);
  const isRunStart = index === start;
  const isRunEnd = index === end;

  return {
    runKey: first.id,
    isRunStart,
    isRunEnd,
    streaming,
    folded,
    renderBlocks: !folded,
    // Expanded / streaming runs render the terminal text inside their blocks;
    // only a folded run pulls it out (on the run-end turn).
    finalText: folded && isRunEnd ? runFinalText({ turns: runTurns, startIndex: start, endIndex: end }) : '',
    durationMs: last.durationMs,
    createdAt: last.createdAt,
    visible: isRunStart || isRunEnd || !folded,
  };
}

export interface AssistantTurnRenderPlan {
  /** Stable key of the run: its first turn's id. Doubles as the expansion
   *  state key and the data-turn-id scroll anchor of the collapsed header. */
  runKey: string;
  /** True when this turn opens the run: the collapse header row renders here. */
  isRunStart: boolean;
  /** True when this turn ends the run: the run-end footer (a-msg-ft) renders here. */
  isRunEnd: boolean;
  /** True when the run contains the actively streaming turn: it renders fully
   *  expanded/live and the header toggle is disabled (lynx parity). */
  streaming: boolean;
  /** True when the run's work log is hidden behind the fold. */
  folded: boolean;
  /** True when this turn's own blocks are rendered (expanded or streaming). */
  renderBlocks: boolean;
  /** The final text rendered OUTSIDE the fold — on the run-end turn of a
   *  folded run only; '' elsewhere (expanded runs render it in the blocks). */
  finalText: string;
  /** The run-end turn's durationMs (the existing footer's semantics). */
  durationMs: number | undefined;
  /** The run-end turn's createdAt (header relative time). */
  createdAt: string | undefined;
  /** True when the turn emits DOM at all. Folded runs render only their first
   *  and last turn; hidden middle turns are skipped. */
  visible: boolean;
}

/** Convert a single turn to Markdown. */
export function turnToMarkdown(turn: ChatTurn): string {
  const parts: string[] = [];
  for (const blk of turnBlocks(turn)) {
    if (blk.kind === 'thinking' && blk.thinking) {
      parts.push(`> **Thinking**\n> ${blk.thinking.split('\n').join('\n> ')}`);
    } else if (blk.kind === 'text' && blk.text) {
      parts.push(blk.text);
    } else if (blk.kind === 'notification' && blk.text) {
      // A mid-turn background-task notice — blockquote the body so the copy
      // keeps the settlement report without the raw envelope shell.
      parts.push(`> **${blk.notification.status}** ${blk.text}`.split('\n').join('\n> '));
    } else if (blk.kind === 'tool' && blk.tool.output && blk.tool.output.length > 0) {
      const output = blk.tool.output.join('\n');
      parts.push(`\`\`\`\n[${blk.tool.name}]\n${output}\n\`\`\``);
    }
  }
  return parts.join('\n\n');
}

export function toolStackKey(item: ToolStackItem): string {
  return item.tool.id || `tool-${item.sourceIndex}`;
}

export function renderBlockKey(block: AssistantRenderBlock, index: number): string {
  if (block.kind === 'tool-stack') {
    return `tool-stack-${block.tools[0]?.sourceIndex ?? index}`;
  }
  if (block.kind === 'tool') return toolStackKey({ tool: block.tool, sourceIndex: block.sourceIndex });
  return `${block.kind}-${block.sourceIndex}`;
}

/**
 * Parse a turn's promptId into the numeric fork-at-turn anchor. Only the
 * durable `turn-<n>` key (the usage fold's wire-id key over completed
 * turns) is a valid anchor: the live correlation id (`turn-<uuid>`) and
 * the aborted-bracket keys (`aborted-<n>`) never resolve, so the fork
 * action must not render for them.
 */
export function forkAnchorOf(promptId: string | undefined): number | undefined {
  if (promptId === undefined) return undefined;
  const match = /^turn-(\d+)$/.exec(promptId);
  return match !== null ? Number.parseInt(match[1]!, 10) : undefined;
}

