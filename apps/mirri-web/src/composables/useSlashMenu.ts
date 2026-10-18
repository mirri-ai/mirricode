// apps/mirri-web/src/composables/useSlashMenu.ts
import { ref } from 'vue';
import type { AppSkill } from '../api/types';
import {
  extractSlashTriggerToken,
} from '../lib/inlineReferences';
import { buildSlashItems, filterCommands, skillDisplayName, type SlashCommand } from '../lib/slashCommands';
import { skillReferenceTarget } from '../lib/skillReferenceTarget';
import type { TextSource } from './useTextSource';

export interface SlashMenuDeps {
  /** The editor-adapter the trigger token is read at and rewritten through. */
  textSource: TextSource;
  /** Current session skills (getter, so the menu stays reactive). */
  skills: () => AppSkill[];
  /** Emit a chosen slash command up to the parent. */
  emitCommand: (cmd: string) => void;
  /** Record a sent command for ↑/↓ recall. */
  historyPush: (entry: string) => void;
  /**
   * Synchronously clear the persisted draft when a bare command is chosen.
   * Mirrors the explicit clear in Composer's submit/steer paths so a draft
   * is not left behind if the Composer unmounts before the text watcher flushes.
   */
  clearDraft?: () => void;
}

/**
 * `/` slash-command menu: filtering, keyboard navigation state, and selection.
 *
 * The composer keeps the keydown orchestration (arrow keys, Enter/Tab, Escape)
 * because it also juggles the mention menu and history recall; this composable
 * owns the menu's open/items/active state, the filter logic, and what happens
 * when an item is chosen.
 *
 * Trigger position is word start (US3.6, M2): the menu opens when the caret
 * sits at the start of the input string or right after whitespace with a `/`
 * as the first character of the word — a `/` typed mid-word (paths, URLs) never
 * opens it. On select, an EMPTY composer keeps the legacy behavior (backfill
 * the canonical command text); a composer with content inserts a skillMention
 * chip node at the caret instead (ADR-004) — mixed messages submit as normal
 * chat text.
 */
export function useSlashMenu(deps: SlashMenuDeps) {
  const { textSource, skills, emitCommand, historyPush, clearDraft } = deps;

  const open = ref(false);
  const items = ref<SlashCommand[]>([]);
  const active = ref(0);

  function update(): void {
    const val = textSource.getText();
    const pos = textSource.getCaret();
    const token = extractSlashTriggerToken(val, pos);
    if (token) {
      // Built-in commands + the active session's skills (shown as /<skill-name>).
      items.value = filterCommands(token.query, buildSlashItems(skills()));
      active.value = 0;
      open.value = items.value.length > 0;
    } else {
      open.value = false;
    }
  }

  function select(item: SlashCommand): void {
    open.value = false;
    const val = textSource.getText();
    const pos = textSource.getCaret();
    const token = extractSlashTriggerToken(val, pos);
    // Bare trigger = the whole input is just the `/query` token: the composer
    // was empty when the user started the trigger, so the legacy behavior
    // (backfill the command text / emit a bare command) applies. Any content
    // outside the token makes it an inline (mid-message) insert.
    const bare = token !== null && token.start === 0 && token.end === val.length;

    // Skills ALWAYS insert a skillMention chip node, even for a bare
    // (first-position) trigger — the chip is the single canonical insertion
    // form (US3.6/SDR-005 follow-up: the split bare-vs-inline behavior was
    // retired after real-world use showed the mixed forms confusing).
    // Builtin/plugin COMMANDS keep the legacy backfill paths below. This
    // check MUST precede the bare branches: skill items carry
    // acceptsInput=true (argument backfill), which would otherwise capture
    // the selection before it reaches here.
    if (item.isSkill) {
      const display = skillDisplayName(item);
      const skill = skills().find((s) => s.name === display);
      // Chip target normalization is centralized in skillReferenceTarget
      // (ADR-004): disk skills resolve by their SKILL.md file path, builtin
      // skills keep their `builtin://` pseudo path verbatim, and path-less
      // entries fall back to the canonical command target.
      const target = skillReferenceTarget(skill, item.name);
      // Remove the typed trigger token first — insertSkillMention inserts at
      // the caret and must not leave the `/query` residue behind.
      if (token) textSource.replaceRange(token.start, token.end, '');
      textSource.insertSkillMention?.({ display, target, kind: 'skill' });
      return;
    }

    // Commands keep the legacy bare behavior: backfill the command text
    // (acceptsInput) or emit the bare command directly.
    if (bare && item.acceptsInput) {
      textSource.replaceRange(0, val.length, `${item.name} `);
      textSource.focus();
      return;
    }
    if (bare) {
      textSource.replaceRange(0, val.length, '');
      clearDraft?.();
      // Menu-selected bare commands (e.g. /model, /status) reach here directly and
      // never go through handleSubmit, so record them for recall too. acceptsInput
      // commands are pushed later by handleSubmit together with their argument.
      historyPush(item.name);
      emitCommand(item.name);
      return;
    }

    // Composer has content — insert a skillMention chip node at the caret
    // (M2/US3.6): command items target `command:<canonical name>` (slash
    // kept). The chip renders as a real DOM node in the editor; mixed messages
    // submit through the ordinary path. The typed `/query` token is removed
    // so its residue is not left behind in the message text.
    const display = item.name.replace(/^\//, '');
    if (token) textSource.replaceRange(token.start, token.end, '');
    textSource.insertSkillMention?.({ display, target: `command:${item.name}`, kind: 'command' });
  }

  return { open, items, active, update, select };
}
