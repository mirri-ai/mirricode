// apps/mirri-web/src/composables/useSkillTriggerMenu.ts
import { ref } from 'vue';
import type { AppSkill } from '../api/types';
import {
  extractSkillTriggerToken,
} from '../lib/inlineReferences';
import { skillReferenceTarget } from '../lib/skillReferenceTarget';
import {
  buildSkillTriggerItems,
  filterCommands,
  skillDisplayName,
  type SlashCommand,
} from '../lib/slashCommands';
import type { TextSource } from './useTextSource';

export interface SkillTriggerMenuDeps {
  /** The editor-adapter the trigger token is read at and rewritten through. */
  textSource: TextSource;
  /** Current session skills (getter, so the menu stays reactive). */
  skills: () => AppSkill[];
}

/**
 * Skill trigger menu (`$`/`¥`/`￥`): filtering, keyboard navigation state, and
 * selection for the session's skills.
 *
 * Mirrors useMentionMenu's shape: the composer keeps the keydown orchestration
 * (arrow keys, Enter/Tab, Escape) because it also juggles the slash menu, the
 * mention menu and history recall; this composable owns the menu's
 * open/items/active state, the filter logic, and what happens on select.
 *
 * Trigger position is word start (US3.4/US3.6, M2): the caret at the start of
 * the input string or right after whitespace, with the trigger char as the
 * first character of the word. A `$` typed mid-word never opens the menu.
 *
 * Selecting an item (US3.3/US3.6):
 * - Skills ALWAYS insert a skillMention chip node at the caret, even for a
 *   bare (first-position) trigger — the chip is the single canonical
 *   insertion form (US3.6/SDR-005 follow-up), rendered as a real DOM node.
 * - Non-skill items (menu mixin) with a BARE trigger keep the legacy
 *   command-name backfill so builtin commands keep their slash semantics.
 */
export function useSkillTriggerMenu(deps: SkillTriggerMenuDeps) {
  const { textSource, skills } = deps;

  const open = ref(false);
  const items = ref<SlashCommand[]>([]);
  const active = ref(0);

  function update(): void {
    const val = textSource.getText();
    const pos = textSource.getCaret();
    const token = extractSkillTriggerToken(val, pos);
    if (token) {
      items.value = filterCommands(token.query, buildSkillTriggerItems(skills()));
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
    const current = extractSkillTriggerToken(val, pos);

    // Bare trigger = the whole input is just the `$query` token (or the input
    // is empty): the composer was empty when the user started the trigger, so
    // this is a first-position trigger. Any content outside the token makes it
    // an inline (mid-message) insert.
    const bare = val.length === 0 || (current !== null && current.start === 0 && current.end === val.length);
    const start = current?.start ?? pos;
    const end = current?.end ?? pos;

    // Skills ALWAYS insert a skillMention chip node, even for a bare
    // (first-position) trigger — the chip is the single canonical insertion
    // form (US3.6/SDR-005 follow-up: the split bare-vs-inline behavior was
    // retired after real-world use showed the mixed forms confusing).
    if (item.isSkill) {
      const display = skillDisplayName(item);
      const skill = skills().find((s) => s.name === display);
      // Older engines report the skill DIRECTORY as path; builtin skills carry
      // the `builtin://` pseudo path. Both normalize correctly in one place
      // (skillReferenceTarget, the chip-target resolution authority).
      const target = skillReferenceTarget(skill, item.name);
      // Remove the typed trigger token first — insertSkillMention inserts at
      // the caret and must not leave the `$query` residue behind.
      textSource.replaceRange(start, end, '');
      textSource.insertSkillMention?.({ display, target, kind: 'skill' });
      return;
    }

    // Not a skill (menu mixin): command fallback for a bare trigger keeps the
    // legacy backfill so builtin commands keep their slash semantics. No
    // emitCommand and no historyPush: history is recorded when the backfilled
    // text is submitted through the ordinary slash path.
    if (bare) {
      textSource.replaceRange(0, val.length, `${item.name} `);
      textSource.focus();
      return;
    }

    // Composer has content — insert the skillMention chip node at the caret
    // (M2/US3.6). Same centralized target resolution as the skill branch
    // above: disk skills by their SKILL.md path (ADR-004), builtin skills by
    // their pseudo path, path-less entries via the canonical command target
    // (the server-side parse stays authoritative).
    const display = skillDisplayName(item);
    const skill = skills().find((s) => s.name === display);
    const target = skillReferenceTarget(skill, item.name);
    textSource.replaceRange(start, end, '');
    textSource.insertSkillMention?.({ display, target, kind: 'skill' });
  }

  return { open, items, active, update, select };
}
