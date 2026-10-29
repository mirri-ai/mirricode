// apps/mirri-web/src/composables/useTextSource.ts
import { ref, type Ref } from 'vue';

/**
 * The editor-agnostic "text field" the trigger menus read from and write to.
 *
 * The menus (useSlashMenu, useSkillTriggerMenu, useMentionMenu) were built
 * around a textarea (`text` ref + `textareaRef` selection). The TipTap
 * migration re-hosts them on the contenteditable without touching their
 * filter/grouping logic: the Composer passes an adapter whose `getText()`
 * returns the plain text before the caret (mentions render as `〇`
 * placeholders so the caret offset math stays contiguous) and whose selection
 * write-backs go through `insertText` / `insertSkillMention`.
 *
 * It also keeps a mock-friendly shape: tests can construct one from a plain
 * `{ text, caret }` pair via `useStaticTextSource`.
 */

export interface TextSource {
  /** The full plain text of the field (mentions as placeholder glyphs). */
  getText(): string;
  /** The caret offset in the text returned by getText(). */
  getCaret(): number;
  /** Replace the token span [start, end) with plain text (no trailing space
   *  added — callers pass exactly what should land, including any space). */
  replaceRange(start: number, end: number, insert: string): void;
  /** Insert a skill/command reference chip at the caret (replaces the token
   *  span the caller just removed, or the current selection). */
  insertSkillMention?(options: { display: string; target: string; kind: 'skill' | 'command' }): void;
  /** Insert a workspace file/folder reference chip at the caret (E9). The
   *  caller removes the typed @token span first; the chip lands in its place
   *  followed by a plain space. */
  insertFileMention?(options: { display: string; path: string; kind: 'file' | 'folder'; root?: string }): void;
  /** Whether the document already carries a fileMention chip for `path`
   *  (E9 review: pick dedup — a duplicate pick closes the menu without
   *  inserting a second chip for the same path). */
  hasFileMention?(path: string): boolean;
  /** Focus the field (caret already positioned by replaceRange). */
  focus(): void;
}

/** A TextSource over a plain ref — for tests and non-editor callers. */
export function useStaticTextSource(text: Ref<string>, caret: () => number): TextSource {
  const currentCaret = ref(0);
  return {
    getText: () => text.value,
    getCaret: () => (currentCaret.value = caret()),
    replaceRange(start, end, insert) {
      text.value = text.value.slice(0, start) + insert + text.value.slice(end);
      currentCaret.value = start + insert.length;
    },
    focus: () => {},
  };
}
