// apps/mirri-web/src/lib/fileMentionNode.ts
// The TipTap node that makes a workspace file/folder reference chip a REAL
// DOM node in the Composer's contenteditable (E9 structured file references):
// inline, atom, rendered as a `ref-chip file-ref-chip` pill with a file or
// folder glyph — the same DOM-spec technique as the skill/command chips in
// ./skillMentionNode.ts (a ProseMirror renderHTML must be a plain DOM spec —
// no Vue components without a node view, and a node view would be overkill
// for a static pill).
//
// Serialization (doc → `@path` wire text, folder trailing `/`) lives in
// ./fileMention.ts — pure and testable without an editor.

import { Node, mergeAttributes } from '@tiptap/core';
import type { DOMOutputSpec } from '@tiptap/pm/model';
import { buildFileMentionNodeJson, formatFileMentionText, type FileMentionAttrs } from './fileMention';

export interface InsertFileMentionOptions extends FileMentionAttrs {}

declare module '@tiptap/core' {
  interface Commands<ReturnType> {
    fileMention: {
      /**
       * Insert a file/folder reference chip at the current selection,
       * followed by a plain space; the caret lands after the space. Used by
       * the `@` mention menu (pick action).
       */
      insertFileMention: (options: InsertFileMentionOptions) => ReturnType;
    };
  }
}

// 24×24 icons: folder = the classic outline folder shape; file = the
// document outline shape (stroke geometry, same treatment as the skill glyph
// in skillMentionNode.ts). Kept inline (not via ICONS) because renderHTML
// needs a raw DOM spec.
const FOLDER_ICON_PATH =
  'M3 6.5C3 5.39543 3.89543 4.5 5 4.5H9.20711C9.4625 4.5 9.7069 4.60152 9.8856 4.78237L11.4142 6.32843C11.6028 6.51914 11.8597 6.625 12.1276 6.625H19C20.1046 6.625 21 7.52043 21 8.625V17.5C21 18.6046 20.1046 19.5 19 19.5H5C3.89543 19.5 3 18.6046 3 17.5V6.5Z';
const FILE_ICON_PATH =
  'M7 3.5H14L19 8.5V19.5C19 20.6046 18.1046 21.5 17 21.5H7C5.89543 21.5 5 20.6046 5 19.5V5.5C5 4.39543 5.89543 3.5 7 3.5ZM14 3.5V8.5H19';

function iconSvg(path: string): DOMOutputSpec {
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
      fill: 'none',
      stroke: 'currentColor',
      'stroke-width': '1.5',
      'stroke-linecap': 'round',
      'stroke-linejoin': 'round',
      'aria-hidden': 'true',
      class: 'ref-chip-icon',
    },
    ['path', { d: path }],
  ];
}

export const FileMention = Node.create({
  name: 'fileMention',

  inline: true,
  group: 'inline',
  atom: true,
  selectable: true,

  addAttributes() {
    return {
      display: { default: '' },
      path: { default: '' },
      kind: { default: 'file' },
      root: { default: null },
      size: { default: null },
    };
  },

  parseHTML() {
    // Parse the chip form (round-trip through editor HTML — e.g. drag/copy
    // within the editor).
    return [{ tag: 'span[data-file-mention]' }];
  },

  renderHTML({ node, HTMLAttributes }) {
    const { display, path, kind, root, size } = node.attrs as FileMentionAttrs;
    return [
      'span',
      mergeAttributes(HTMLAttributes, {
        'data-file-mention': '',
        'data-kind': kind,
        'data-path': path,
        ...(root !== undefined ? { 'data-root': root } : undefined),
        ...(size !== undefined ? { 'data-size': String(size) } : undefined),
        class: 'ref-chip file-ref-chip',
        // Atom semantics for assistive tech: the chip reads as its name.
        role: 'img',
        'aria-label': display,
      }),
      iconSvg(kind === 'folder' ? FOLDER_ICON_PATH : FILE_ICON_PATH),
      ['span', { class: 'ref-chip-name' }, display],
    ];
  },

  renderText({ node }) {
    // Clipboard / getText serialization — same wire form as the doc
    // serializer in ./fileMention.ts: `@path`, folders with a trailing `/`.
    const { path, kind } = node.attrs as FileMentionAttrs;
    return formatFileMentionText(path, kind);
  },

  addCommands() {
    return {
      insertFileMention:
        ({ display, path, kind, root, size }: InsertFileMentionOptions) =>
        ({ chain }) =>
          // insertContent places the cursor after the inserted content by
          // default (selectionToInsertionEnd) — the trailing space then sits
          // under the caret, exactly like the skill mention path.
          chain()
            .insertContent([
              buildFileMentionNodeJson({ display, path, kind, root, size }),
              { type: 'text', text: ' ' },
            ])
            .run(),
    };
  },
});
