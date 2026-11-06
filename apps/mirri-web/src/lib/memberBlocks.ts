// apps/mirri-web/src/lib/memberBlocks.ts
// Member narrative content blocks (supervision ADR-009): the read face behind
// the detail panel's narrative area. A member's process — live or settled —
// is normalized into the SAME ordered content-block stream the main
// transcript renders (narrative segments + structured steps: thinking / rich
// text / tool cards), so the panel consumes one model instead of a live text
// dump plus a flattened history.
//
// Two entry points, one output:
//   - memberBlocksFromMessages: a settled member's journal (the member-scoped
//     `message.list` read face — AppMessage[] isomorphic to the main
//     transcript's), pairing each toolUse with its toolResult (name, args,
//     status, output — nothing flattened away, ADR-009 decision ③).
//   - memberBlocksFromLive: the running member's task fields (streamed text +
//     structured steps accumulated by the event projector).
import type { AppMessage } from '../api/types';
import type { AgentStep, NarrativeSegment } from '../api/types';
import type { AssistantRenderBlock } from '../components/chatTurnRendering';
import type { ToolCall, TurnBlock } from '../types';

// ---------------------------------------------------------------------------
// Settled fallback decision (ADR-015 / RC-2)
// ---------------------------------------------------------------------------

/** The narrative to render for a settled member: the journal is authoritative
 *  when loaded (a non-empty journal renders its blocks; an empty journal `[]`
 *  renders no narrative section); only a still-in-flight journal (null or
 *  not yet provided — an optional prop) falls back to the last live blocks so
 *  the panel does not blank between the live stream and the journal read
 *  (ADR-015 / RC-2 — the mis-parenthesized `??` used to short-circuit a
 *  non-empty journal so it never rendered). */
export function settledNarrativeBlocks(
  history: TurnBlock[] | null | undefined,
  liveBlocks: TurnBlock[],
): TurnBlock[] | null {
  if (history !== null && history !== undefined) return history.length > 0 ? history : null;
  return liveBlocks.length > 0 ? liveBlocks : null;
}

// ---------------------------------------------------------------------------
// Settled path: journal messages → blocks
// ---------------------------------------------------------------------------

/** Normalize tool output (`string | ContentPart[]`) into display lines — the
 *  same flattening rule the main transcript's tool cards use (text/think parts
 *  become lines, media parts become placeholders). */
function normalizeOutput(output: unknown): string[] | undefined {
  if (output === null || output === undefined) return undefined;
  if (typeof output === 'string') {
    // '' normalizes to undefined — the live path leaves a step without output
    // undefined too, so both faces present the same "no output" shape.
    return output.length > 0 ? output.split('\n') : undefined;
  }
  if (Array.isArray(output)) {
    const lines: string[] = [];
    for (const part of output) {
      if (typeof part === 'string') {
        lines.push(...part.split('\n'));
      } else if (part && typeof part === 'object') {
        const p = part as Record<string, unknown>;
        if (p['type'] === 'text' && typeof p['text'] === 'string') lines.push(...p['text'].split('\n'));
        else if (p['type'] === 'think' && typeof p['think'] === 'string') lines.push(...p['think'].split('\n'));
        else if (typeof p['type'] === 'string') lines.push(`[${p['type']}]`);
        else lines.push(JSON.stringify(part));
      }
    }
    return lines.length > 0 ? lines : undefined;
  }
  return [JSON.stringify(output)];
}

function argToString(input: unknown): string {
  if (typeof input === 'string') return input;
  try {
    return JSON.stringify(input) ?? '';
  } catch {
    // Unserializable input (e.g. circular reference) — the arg is unreadable,
    // but the call itself still renders.
    return '[unserializable]';
  }
}

/**
 * Flatten a member's journal messages into one continuous ordered block
 * stream (ADR-009 decision ②: single instruction → single report, no turn
 * structure). Blocks appear in true happening order: thinking, streamed text
 * and tool cards interleave exactly where they occurred. toolResult parts
 * pair back onto the open tool card (status ok/error, output lines); a
 * result without its call (a journal starting mid-flight) is dropped — an
 * orphan has no card to belong to.
 */
export function memberBlocksFromMessages(messages: AppMessage[]): TurnBlock[] {
  const blocks: TurnBlock[] = [];
  const openTools = new Map<string, ToolCall>();

  const pushText = (kind: 'text' | 'thinking', value: string): void => {
    if (value.length === 0) return;
    const last = blocks.at(-1);
    // Merge consecutive segments of the same kind (same rule as the main
    // transcript's block builder).
    if (last && last.kind === kind) {
      if (last.kind === 'text' && kind === 'text') last.text += '\n' + value;
      else if (last.kind === 'thinking' && kind === 'thinking') last.thinking += '\n' + value;
      return;
    }
    blocks.push(
      kind === 'text'
        ? { kind: 'text', text: value }
        : { kind: 'thinking', thinking: value },
    );
  };

  for (const message of messages) {
    for (const part of message.content) {
      if (part.type === 'text') {
        // No trim — the main transcript's block builder skips only EMPTY
        // segments; leading/trailing whitespace is content (e.g. a fenced
        // block's indented wrapper line), not noise.
        pushText('text', part.text);
      } else if (part.type === 'thinking') {
        pushText('thinking', part.thinking);
      } else if (part.type === 'toolUse') {
        const tool: ToolCall = {
          id: part.toolCallId,
          name: part.toolName,
          arg: argToString(part.input),
          status: 'running',
          output: part.outputLines,
        };
        openTools.set(part.toolCallId, tool);
        blocks.push({ kind: 'tool', tool });
      } else if (part.type === 'toolResult') {
        const tool = openTools.get(part.toolCallId);
        if (!tool) continue;
        tool.status = part.isError ? 'error' : 'ok';
        tool.output = normalizeOutput(part.output);
        openTools.delete(part.toolCallId);
      }
    }
  }
  return blocks;
}

// ---------------------------------------------------------------------------
// Live path: task fields → blocks
// ---------------------------------------------------------------------------

/**
 * The running member's live block stream (ADR-011 ①): narrative segments
 * (the projector mints one per uninterrupted text/thinking run) and structured
 * steps are merged into ONE seq-ordered entry timeline, so thinking, rich
 * text and tool cards interleave in true happening order — the same order the
 * settled journal restores. Blocks are rebuilt per call — this is a
 * projection, not an accumulation.
 */
export function memberBlocksFromLive(
  segments: NarrativeSegment[] | undefined,
  steps: AgentStep[] | undefined,
): TurnBlock[] {
  type LiveEntry =
    | { seq: number; kind: 'text'; text: string }
    | { seq: number; kind: 'thinking'; text: string }
    | {
        seq: number;
        kind: 'tool';
        tool: { id: string; name: string; arg: string; status: AgentStep['status']; output?: string[] };
      };

  const entries: LiveEntry[] = [];
  for (const seg of segments ?? []) entries.push({ seq: seg.seq, kind: seg.kind, text: seg.text });
  for (const step of steps ?? []) {
    entries.push({
      seq: step.seq,
      kind: 'tool',
      tool: {
        id: step.id,
        name: step.name,
        arg: step.arg,
        status: step.status,
        output: step.outputLines.length > 0 ? step.outputLines : undefined,
      },
    });
  }
  // Single monotonic counter stamps both segments and steps, so one stable
  // sort restores the member's true happening order.
  entries.sort((a, b) => a.seq - b.seq);

  const blocks: TurnBlock[] = [];
  const pushText = (kind: 'text' | 'thinking', value: string): void => {
    // Empty segments are skipped (the same rule as the journal path); no
    // trim — segment-granularity merged tail whitespace is content, and a
    // tool block naturally breaks the merge.
    if (value.length === 0) return;
    const last = blocks.at(-1);
    if (last && last.kind === kind) {
      if (kind === 'text' && last.kind === 'text') last.text += '\n' + value;
      else if (kind === 'thinking' && last.kind === 'thinking') last.thinking += '\n' + value;
      return;
    }
    blocks.push(kind === 'text' ? { kind: 'text', text: value } : { kind: 'thinking', thinking: value });
  };

  for (const entry of entries) {
    if (entry.kind === 'tool') {
      blocks.push({ kind: 'tool', tool: entry.tool });
    } else {
      pushText(entry.kind, entry.text);
    }
  }
  return blocks;
}

/**
 * The segment id whose mapped block is the streaming tail (ADR-011 ③): the
 * last segment after normalization. A text block the tools grew past is
 * final on the spot (non-last → not streaming); a newly opened segment makes
 * the last block streaming again. No segments → no streaming anchor.
 */
export function liveOpenSegmentId(segments: NarrativeSegment[] | undefined): string | undefined {
  return segments?.at(-1)?.id;
}

/**
 * The member's current text tail — the LAST text segment (ADR-011 ②
 * activity-line semantics; the single-field accumulation is retired). A
 * member that has moved on to reasoning has no current text tail
 * (`undefined`, not the stale earlier output), and a segment that opened but
 * has not accumulated text yet is not a tail either. No segments — a legacy
 * engine row or a snapshot-restored one — also yields `undefined`, so
 * callers fall back through outputLines / summary.
 */
export function liveTextTail(segments: NarrativeSegment[] | undefined): string | undefined {
  const last = segments?.at(-1);
  return last?.kind === 'text' && last.text.length > 0 ? last.text : undefined;
}

/**
 * The MemberFocus streaming decision (ADR-011 ③), as a pure function over the
 * member's live fields: a render block streams when the member is live, an
 * open segment exists, and the block IS the last render block — the position
 * the last segment maps to after normalization. A text block the tools grew
 * past is final on the spot (non-last → not streaming, so a new tool card
 * landing never flip-flops it back); a newly opened segment makes the last
 * block stream again. Tool/tool-stack blocks never stream.
 */
export function liveStreamingView(
  segments: NarrativeSegment[] | undefined,
  isLive: boolean,
  renderBlocks: AssistantRenderBlock[],
): (block: AssistantRenderBlock) => boolean {
  const openSegId = liveOpenSegmentId(segments);
  const last = renderBlocks.at(-1);
  if (!isLive || openSegId === undefined) return () => false;
  if (!last || last.kind === 'tool' || last.kind === 'tool-stack') return () => false;
  return (block) => block === last;
}
