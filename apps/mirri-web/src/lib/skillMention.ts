// apps/mirri-web/src/lib/skillMention.ts
// Pure TS — no Vue, no editor instance: the document-level helpers for the
// Composer's TipTap skill/command mention nodes (ADR-004 follow-up: the chip
// is a REAL DOM node, replacing the textarea+mirror overlay).
//
// This module owns three pure concerns:
//   1. `buildSkillMentionDocJson` — plain text / legacy draft string → a
//      TipTap doc JSON with `skillMention` nodes. Full reference links
//      `[$name](target)` become nodes; legacy compact tokens `[$name]` become
//      targetless nodes (kept for old drafts; the pre-submit gate flags them).
//   2. `serializeDocJsonToText` — doc JSON → the wire-form string. A
//      skillMention node serializes to the FULL reference link
//      `[$display](target)` (the wire format per ADR-004 — the compact token
//      is retired from the payload path); text nodes pass through verbatim;
//      paragraphs join with `\n`; hard breaks become `\n`.
//   3. node-position math (`isCaretAtDocStart`, `docHasContent`) used by the
//      Composer for the history-recall / grown-state wiring.
//
// The TipTap `Node.create` definition lives separately in
// `src/lib/skillMentionNode.ts` (it imports @tiptap/core; keeping the doc
// math here keeps this module editor-free and trivially testable).

import { parseCompactTokens } from './inlineReferences';
import { parseInlineReferences } from '@mirri-ai/protocol';

/** TipTap doc JSON shape (the subset the Composer produces). */
export interface DocJson {
  type: string;
  attrs?: Record<string, unknown>;
  content?: DocJson[];
  text?: string;
  marks?: Array<{ type: string; attrs?: Record<string, unknown> }>;
}

/** The attributes of a skillMention node. */
export interface SkillMentionAttrs {
  display: string;
  target: string;
  kind: 'skill' | 'command';
}

/** A mention parsed out of a raw string, with its source span. */
interface MentionSpan extends SkillMentionAttrs {
  start: number;
  end: number;
}

/** A segment of a source string: plain text or a mention (kind: skill|command). */
export type DraftSegment =
  | { kind: 'plain'; text: string }
  | ({ kind: 'skill' | 'command' } & SkillMentionAttrs);

// ---------------------------------------------------------------------------
// 1. parse — plain string → doc JSON
// ---------------------------------------------------------------------------

/**
 * Split a raw string into plain-text and mention segments, in order. Full
 * reference links take precedence; legacy compact tokens outside them become
 * targetless mentions.
 */
export function splitDraftIntoSegments(text: string): DraftSegment[] {
  // Full links first (authoritative spans), then compact tokens outside them.
  const links: MentionSpan[] = parseInlineReferences(text).references.map((r) => ({
    kind: r.kind,
    display: r.display,
    target: r.target,
    start: r.start,
    end: r.end,
  }));
  const tokens: MentionSpan[] = parseCompactTokens(text)
    .filter((t) => !links.some((l) => t.start >= l.start && t.end <= l.end))
    .map((t) => ({
      kind: 'skill' as const,
      display: t.display,
      target: '', // legacy compact token — no stored target
      start: t.start,
      end: t.end,
    }));
  const mentions = [...links, ...tokens].toSorted((a, b) => a.start - b.start);

  const segments: DraftSegment[] = [];
  let cursor = 0;
  for (const m of mentions) {
    if (m.start > cursor) segments.push({ kind: 'plain', text: text.slice(cursor, m.start) });
    segments.push(m.kind === 'command' ? { kind: 'command', display: m.display, target: m.target } : { kind: 'skill', display: m.display, target: m.target });
    cursor = m.end;
  }
  if (cursor < text.length) segments.push({ kind: 'plain', text: text.slice(cursor) });
  return segments;
}

/**
 * Build the initial TipTap doc JSON from a raw draft string (or an arbitrary
 * plain string — history recall, load-for-edit). Paragraphs are created on
 * `\n`; each mention becomes an inline `skillMention` node.
 */
export function buildSkillMentionDocJson(text: string): DocJson {
  return {
    type: 'doc',
    content: buildParagraphs(text),
  };
}

/** Build the paragraph child nodes of the doc, one per `\n`-split line. */
function buildParagraphs(text: string): DocJson[] {
  const paragraphs: DocJson[] = [];
  for (const line of text.split('\n')) {
    const nodes: DocJson[] = [];
    for (const seg of splitDraftIntoSegments(line)) {
      if (seg.kind === 'plain') {
        if (seg.text) nodes.push({ type: 'text', text: seg.text });
      } else {
        nodes.push(buildMentionNodeJson(seg));
      }
    }
    paragraphs.push({ type: 'paragraph', content: nodes.length > 0 ? nodes : undefined });
  }
  return paragraphs;
}

/** One inline skillMention node JSON. */
export function buildMentionNodeJson(attrs: SkillMentionAttrs): DocJson {
  return {
    type: 'skillMention',
    attrs: { display: attrs.display, target: attrs.target, kind: attrs.kind },
  };
}

// ---------------------------------------------------------------------------
// 2. serialize — doc JSON → wire-form string
// ---------------------------------------------------------------------------

/**
 * Serialize a TipTap doc JSON into the flat message string. skillMention
 * nodes serialize to the full reference link `[$display](target)`; text nodes
 * pass through verbatim; paragraphs join with `\n`; hard breaks become `\n`.
 * A mention with an empty target degrades to its plain display text (the
 * pre-submit gate already flags such nodes before send, so this is a
 * last-resort defense rather than a normal path).
 */
export function serializeDocJsonToText(doc: DocJson): string {
  if (doc.type === 'skillMention') {
    const attrs = (doc.attrs ?? {}) as Partial<SkillMentionAttrs>;
    const display = attrs.display ?? '';
    const target = attrs.target ?? '';
    if (!target) return display;
    // Same shape as formatSkillReferenceLink from @mirri-ai/protocol, spelled
    // out here to keep this helper free of further imports: BOTH kinds carry
    // the skill trigger prefix in the link text (commands too — the parser
    // only accepts trigger-prefixed link text), the kind differs by target
    // (SKILL.md path vs `command:<name>`).
    return `[${SKILL_LINK_TRIGGER}${display}](${target})`;
  }
  if (doc.type === 'hardBreak') return '\n';
  if (doc.type === 'text') return doc.text ?? '';
  const children = doc.content ?? [];
  if (doc.type === 'doc') return children.map((c) => serializeDocJsonToText(c)).join('\n');
  return children.map((c) => serializeDocJsonToText(c)).join('');
}

/** The trigger char that prefixes skill display names in reference links. */
const SKILL_LINK_TRIGGER = '$';

// ---------------------------------------------------------------------------
// 3. node-position math for the Composer wiring
// ---------------------------------------------------------------------------

/** True when the caret (selection head) sits at position 0 of the doc. */
export function isCaretAtDocStart(selectionFrom: number): boolean {
  return selectionFrom === 0;
}

/**
 * Map a string offset from the doc's textBetween projection (blockSeparator
 * '\n', leafText '〇') back to a doc position. Each paragraph boundary costs
 * +2 positions (open+close tokens) but yields one '\n' char; every other
 * character maps 1:1 (text chars, atomic inline nodes — hardBreak included,
 * which the projection renders as one '〇' leaf char).
 */
export function textOffsetToPos(text: string, offset: number): number {
  // Position 1 = start of the first paragraph's content (after the doc's
  // implicit structure: the first paragraph's content begins at position 1).
  let pos = 1;
  let consumed = 0;
  for (const line of text.split('\n')) {
    // The current line fits within the remaining offset — walk it 1:1.
    if (consumed + line.length >= offset) return pos + (offset - consumed);
    // Skip past this line + its separator char; the block boundary costs +2
    // positions (close token of this paragraph + open token of the next).
    consumed += line.length + 1;
    pos += line.length + 2;
  }
  return pos + Math.max(0, offset - consumed);
}

/**
 * True when the doc carries any user content — a non-empty text node, a
 * skillMention node, or more than one paragraph (a bare newline counts as
 * content, matching the textarea's scrollHeight-based growth heuristic).
 */
export function docHasContent(doc: DocJson): boolean {
  const paragraphs = doc.content ?? [];
  if (paragraphs.length !== 1) return paragraphs.length > 0;
  const only = paragraphs[0];
  if (!only?.content || only.content.length === 0) return false;
  return only.content.some((n) => n.type === 'skillMention' || (n.text ?? '') !== '');
}
