// apps/mirri-web/src/lib/fileMention.ts
// Pure TS — no Vue, no editor instance: the document-level helpers for the
// Composer's TipTap file/folder mention nodes (E9 structured file references).
//
// This module owns three pure concerns (mirroring ./skillMention.ts):
//   1. `buildFileMentionDocJson` / `buildFileMentionNodeJson` — attrs →
//      TipTap doc/node JSON with `fileMention` nodes.
//   2. `serializeDocJsonWithFileMentions` — doc JSON → the flat message
//      string. A fileMention node serializes to `@path` (folder with a
//      trailing `/`); text nodes pass through verbatim; paragraphs join
//      with `\n`; hard breaks become `\n`. This is a copy of
//      `serializeDocJsonToText` from ./skillMention.ts plus the fileMention
//      branch — the original function stays untouched (it keeps its
//      skillMention-only contract elsewhere).
//   3. `collectFileMentionAttrs` — deep-collect every fileMention node's
//      attrs from a doc JSON, in document order, deduped by path.
//
// The TipTap `Node.create` definition lives separately in
// `src/lib/fileMentionNode.ts` (it imports @tiptap/core; keeping the doc
// math here keeps this module editor-free and trivially testable).

import type { DocJson } from './skillMention';

/** The attributes of a fileMention node. */
export interface FileMentionAttrs {
  display: string;
  path: string;
  kind: 'file' | 'folder';
  root?: string;
  size?: number;
}

/** One reference extracted from the doc for submission (E9): the wire shape
 *  of `PromptSubmission.references` — the mention attrs minus the display. */
export interface PromptFileReference {
  path: string;
  kind: 'file' | 'folder';
  root?: string;
}

/** Why a submitted reference did not survive the pre-send `file.stat` check. */
export type InvalidReferenceReason = 'not_found' | 'unreadable' | 'kind_mismatch';

/** One failed reference of the pre-send gate (the dialog's list row). */
export interface InvalidReference {
  path: string;
  reason: InvalidReferenceReason;
}

/** One `file.stat` probe answer (see `MirriApi.statFiles`). */
export interface StatFilesResult {
  items: Array<{ path: string; exists: boolean; kind: string; readable: boolean; mtimeMs: number | null }>;
}

/** The stat path a reference kind maps to: 'folder' references stat as
 *  'directory'; 'file' references accept 'file' or 'symlink'. */
function statKindMatches(refKind: 'file' | 'folder', statKind: string): boolean {
  if (refKind === 'folder') return statKind === 'directory';
  return statKind === 'file' || statKind === 'symlink';
}

/**
 * Split the doc-collected references by their `file.stat` probe answers (E9
 * pre-send gate): every reference resolves to valid (usable) or invalid (with
 * the reason the dialog shows). A path missing from the stat answer is
 * `not_found` — the probe is the only source of truth and silence means the
 * engine does not know the file.
 */
export function extractInvalidReferences(
  statResult: StatFilesResult,
  refs: ReadonlyArray<PromptFileReference>,
): { valid: PromptFileReference[]; invalid: InvalidReference[] } {
  const byPath = new Map(statResult.items.map((item) => [item.path, item]));
  const valid: PromptFileReference[] = [];
  const invalid: InvalidReference[] = [];
  for (const ref of refs) {
    const stat = byPath.get(ref.path);
    if (!stat || !stat.exists) {
      invalid.push({ path: ref.path, reason: 'not_found' });
    } else if (!stat.readable) {
      invalid.push({ path: ref.path, reason: 'unreadable' });
    } else if (!statKindMatches(ref.kind, stat.kind)) {
      invalid.push({ path: ref.path, reason: 'kind_mismatch' });
    } else {
      valid.push(ref);
    }
  }
  return { valid, invalid };
}

/** The wire text of one mention: `@path`, folders with a trailing `/`. */
export function formatFileMentionText(path: string, kind: 'file' | 'folder'): string {
  if (!path) return '';
  const prefixed = path.startsWith('@') ? path : `@${path}`;
  return kind === 'folder' && !prefixed.endsWith('/') ? `${prefixed}/` : prefixed;
}

/** One inline fileMention node JSON. */
export function buildFileMentionNodeJson(attrs: FileMentionAttrs): DocJson {
  return {
    type: 'fileMention',
    attrs: {
      display: attrs.display,
      path: attrs.path,
      kind: attrs.kind,
      ...(attrs.root !== undefined ? { root: attrs.root } : undefined),
      ...(attrs.size !== undefined ? { size: attrs.size } : undefined),
    },
  };
}

/** A one-mention doc JSON (the insert form for a fresh chip). */
export function buildFileMentionDocJson(attrs: FileMentionAttrs): DocJson {
  return {
    type: 'doc',
    content: [{ type: 'paragraph', content: [buildFileMentionNodeJson(attrs)] }],
  };
}

/** Mentions → the wire reference array (display/size dropped, root kept for
 *  multi-root disambiguation). Exported for the composer's submit path. */
export function toPromptReferences(attrs: ReadonlyArray<FileMentionAttrs>): PromptFileReference[] {
  return attrs.map((a) => ({
    path: a.path,
    kind: a.kind,
    ...(a.root !== undefined ? { root: a.root } : undefined),
  }));
}

/**
 * Deep-collect every fileMention node's attrs from a doc JSON, in document
 * order, deduped by path (the submit pipeline sends each distinct referenced
 * path once). Atom nodes carry no children, so the walk stops at the chip.
 */
export function collectFileMentionAttrs(doc: DocJson): FileMentionAttrs[] {
  const attrs: FileMentionAttrs[] = [];
  const seen = new Set<string>();
  const walk = (node: DocJson): void => {
    if (node.type === 'fileMention') {
      const a = (node.attrs ?? {}) as Partial<FileMentionAttrs>;
      const path = a.path ?? '';
      if (!seen.has(path)) {
        seen.add(path);
        attrs.push({
          display: a.display ?? '',
          path,
          kind: a.kind === 'folder' ? 'folder' : 'file',
          ...(a.root !== undefined ? { root: a.root } : undefined),
          ...(a.size !== undefined ? { size: a.size } : undefined),
        });
      }
      return; // atom node — no children
    }
    for (const child of node.content ?? []) walk(child);
  };
  walk(doc);
  return attrs;
}

/**
 * Serialize a TipTap doc JSON into the flat message string. skillMention
 * nodes serialize to the full reference link `[$display](target)`; fileMention
 * nodes serialize to `@path` (folders with a trailing `/`); text nodes pass
 * through verbatim; paragraphs join with `\n`; hard breaks become `\n`.
 *
 * Copy of `serializeDocJsonToText` from ./skillMention.ts with the
 * fileMention branch added — the original keeps its skillMention-only
 * contract for its existing callers.
 */
export function serializeDocJsonWithFileMentions(doc: DocJson): string {
  if (doc.type === 'fileMention') {
    const attrs = (doc.attrs ?? {}) as Partial<FileMentionAttrs>;
    return formatFileMentionText(attrs.path ?? '', attrs.kind === 'folder' ? 'folder' : 'file');
  }
  if (doc.type === 'skillMention') {
    const attrs = (doc.attrs ?? {}) as Partial<{ display: string; target: string }>;
    const display = attrs.display ?? '';
    const target = attrs.target ?? '';
    if (!target) return display;
    // Same shape as formatSkillReferenceLink from @mirri-ai/protocol, spelled
    // out here to keep this helper free of further imports: BOTH kinds carry
    // the skill trigger prefix in the link text (commands too — the parser
    // only accepts trigger-prefixed link text), the kind differs by target
    // (SKILL.md path vs `command:<name>`).
    return `[$${display}](${target})`;
  }
  if (doc.type === 'hardBreak') return '\n';
  if (doc.type === 'text') return doc.text ?? '';
  const children = doc.content ?? [];
  if (doc.type === 'doc') return children.map((c) => serializeDocJsonWithFileMentions(c)).join('\n');
  return children.map((c) => serializeDocJsonWithFileMentions(c)).join('');
}
