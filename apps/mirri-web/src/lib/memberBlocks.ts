// apps/mirri-web/src/lib/memberBlocks.ts
// Member narrative content blocks (supervision ADR-009): the read face behind
// the detail panel's narrative area. A member's process — live or settled —
// is normalized into the SAME ordered content-block stream the main
// transcript renders (thinking / rich text / tool cards), so the panel
// consumes one model instead of a live text dump plus a flattened history.
//
// Two entry points, one output:
//   - memberBlocksFromMessages: a settled member's journal (the member-scoped
//     `message.list` read face — AppMessage[] isomorphic to the main
//     transcript's), pairing each toolUse with its toolResult (name, args,
//     status, output — nothing flattened away, ADR-009 decision ③).
//   - memberBlocksFromLive: the running member's task fields (streamed text +
//     structured steps accumulated by the event projector).
import type { AppMessage } from '../api/types';
import type { AgentStep } from '../api/types';
import type { ToolCall, TurnBlock } from '../types';

// ---------------------------------------------------------------------------
// Settled path: journal messages → blocks
// ---------------------------------------------------------------------------

/** Normalize tool output (`string | ContentPart[]`) into display lines — the
 *  same flattening rule the main transcript's tool cards use (text/think parts
 *  become lines, media parts become placeholders). */
function normalizeOutput(output: unknown): string[] | undefined {
  if (output === null || output === undefined) return undefined;
  if (typeof output === 'string') return output.split('\n');
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
      // Media / file parts are not narrative material — same rule the old
      // flattening layer applied, kept here (the member's journal, not the
      // main transcript, decides nothing about upload rendering).
    }
  }
  return blocks;
}

// ---------------------------------------------------------------------------
// Live path: task fields → blocks
// ---------------------------------------------------------------------------

/**
 * The running member's live block stream: the member's reasoning as the
 * leading thinking block, structured steps (accumulated by the event
 * projector) as tool cards, and the streamed text as the trailing rich-text
 * block. Blocks are rebuilt per call — this is a projection, not an
 * accumulation.
 *
 * Live-path ordering is a deliberate degradation (ADR-009 open item): the
 * reducer accumulates all streamed text into one field, so live text renders
 * AFTER the tool cards rather than interleaved where it occurred; the
 * settled journal path restores true happening order.
 */
export function memberBlocksFromLive(
  text: string | undefined,
  steps: AgentStep[] | undefined,
  thinking?: string,
): TurnBlock[] {
  const blocks: TurnBlock[] = [];
  const liveThinking = (thinking ?? '').trimEnd();
  if (liveThinking.length > 0) blocks.push({ kind: 'thinking', thinking: liveThinking });
  for (const step of steps ?? []) {
    blocks.push({
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
  const live = (text ?? '').trimEnd();
  if (live.length > 0) blocks.push({ kind: 'text', text: live });
  return blocks;
}
