// apps/mirri-web/src/lib/skillMentionNode.ts
// The TipTap node that makes a skill/command reference chip a REAL DOM node
// in the Composer's contenteditable (the textarea+mirror overlay migration):
// inline, atom (its interior is not editable), rendered as the same chip pill
// the overlay used to paint (ReferenceChip visuals, US3.5 wand glyph for
// skills / terminal glyph for commands, inlined here as SVG paths because a
// ProseMirror renderHTML must be a plain DOM spec — no Vue components without
// a node view, and a node view would be overkill for a static pill).
//
// Serialization (doc → `[$display](target)` wire text) lives in
// ./skillMention.ts — pure and testable without an editor.

import { Node, mergeAttributes } from '@tiptap/core';
import type { DOMOutputSpec } from '@tiptap/pm/model';
import { buildMentionNodeJson, type SkillMentionAttrs } from './skillMention';

export interface InsertSkillMentionOptions extends SkillMentionAttrs {}

declare module '@tiptap/core' {
  interface Commands<ReturnType> {
    skillMention: {
      /**
       * Insert a skill/command reference chip at the current selection,
       * followed by a plain space; the caret lands after the space. Used by
       * the `/`, `$` and slash menus.
       */
      insertSkillMention: (options: InsertSkillMentionOptions) => ReturnType;
    };
  }
}

// 24×24 icons: command = ri/terminal-box-line (fill, same source as
// src/lib/icons.ts); skill = the dedicated wand+star glyph (ADR-005), stroke
// geometry from src/icons/mirri/skill.svg, chained into one compound path.
// Kept inline (not via ICONS) because renderHTML needs a raw DOM spec.
// Exported so the anti-drift test (icons.test.ts) can keep it in sync with
// the registered asset; the export itself leaks no editor-layer state.
export const SKILL_ICON_PATH =
  'M3 21 10.5 13.5 ' +
  'M13 2.5 14.3 5.7 17.5 7 14.3 8.3 13 11.5 11.7 8.3 8.5 7l3.2-1.3Z ' +
  'M19 13.5v3 M20.5 15h-3 M5.5 4.5v2 M6.5 5.5h-2 M18.5 5v1.6 M19.3 5.8h-1.6';
const TERMINAL_ICON_PATH = 'M4 3H20C20.5523 3 21 3.44772 21 4V20C21 20.5523 20.5523 21 20 21H4C3.44772 21 3 20.5523 3 20V4C3 3.44772 3.44772 3 4 3ZM4 5V19H20V5H4ZM9 8.41421L7.58579 9.82843L9.75736 12L7.58579 14.1716L9 15.5858L11.8787 12.7071C12.2692 12.3166 12.2692 11.6834 11.8787 11.2929L9 8.41421ZM11 15H17V17H11V15Z';

function iconSvg(path: string, stroke = false): DOMOutputSpec {
  // The root tag MUST carry the SVG namespace prefix: prosemirror-model's
  // renderSpec only calls createElementNS when the tag is
  // "http://www.w3.org/2000/svg svg" — a bare "svg" is created via
  // createElement, lands in the HTML namespace, and the browser renders it
  // as an unknown 0×0 element (the chip icon vanishes). Children (path)
  // inherit the namespace from the parent, so they stay bare.
  return [
    'http://www.w3.org/2000/svg svg',
    {
      viewBox: '0 0 24 24',
      width: '14',
      height: '14',
      fill: stroke ? 'none' : 'currentColor',
      ...(stroke
        ? {
            stroke: 'currentColor',
            'stroke-width': '1.5',
            'stroke-linecap': 'round',
            'stroke-linejoin': 'round',
          }
        : {}),
      'aria-hidden': 'true',
      class: 'ref-chip-icon',
    },
    ['path', { d: path }],
  ];
}

export const SkillMention = Node.create({
  name: 'skillMention',

  inline: true,
  group: 'inline',
  atom: true,
  selectable: true,

  addAttributes() {
    return {
      display: { default: '' },
      target: { default: '' },
      kind: { default: 'skill' },
    };
  },

  parseHTML() {
    // Parse the chip form (round-trip through editor HTML — e.g. drag/copy
    // within the editor) and the raw markdown link form `[$name](target)`
    // (draft strings parsed via ProseMirror's text rules).
    return [
      { tag: 'span[data-skill-mention]' },
      {
        tag: 'span[data-type="skillMention"]',
      },
    ];
  },

  renderHTML({ node, HTMLAttributes }) {
    const { display, target, kind } = node.attrs as SkillMentionAttrs;
    return [
      'span',
      mergeAttributes(HTMLAttributes, {
        'data-skill-mention': '',
        'data-kind': kind,
        'data-target': target,
        class: 'ref-chip',
        // Atom semantics for assistive tech: the chip reads as its name.
        role: 'img',
        'aria-label': display,
      }),
      iconSvg(
        kind === 'command' ? TERMINAL_ICON_PATH : SKILL_ICON_PATH,
        kind !== 'command',
      ),
      ['span', { class: 'ref-chip-name' }, display],
    ];
  },

  renderText({ node }) {
    // Clipboard / getText serialization — same wire form as the doc
    // serializer in ./skillMention.ts. A targetless (legacy compact-token)
    // node degrades to its display name.
    const { display, target } = node.attrs as SkillMentionAttrs;
    return target ? `[$${display}](${target})` : display;
  },

  addCommands() {
    return {
      insertSkillMention:
        ({ display, target, kind }: InsertSkillMentionOptions) =>
        ({ chain }) =>
          // insertContent places the cursor after the inserted content by
          // default (selectionToInsertionEnd) — the trailing space then sits
          // under the caret, exactly like the old textarea path.
          chain()
            .insertContent([buildMentionNodeJson({ display, target, kind }), { type: 'text', text: ' ' }])
            .run(),
    };
  },
});
