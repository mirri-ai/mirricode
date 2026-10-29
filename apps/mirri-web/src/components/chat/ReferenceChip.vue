<!-- apps/mirri-web/src/components/chat/ReferenceChip.vue -->
<!-- One inline skill/command reference rendered as a chip (M3, US3.6): skills
     carry the unified skill glyph (skill, US3.5), commands carry the command
     visual identity (terminal glyph, same family as the slash-menu command
     rows). Visual language follows AttachmentChip / SlashMenu: small pill,
     mono font, soft accent fill. Used in TWO contexts:
       - the Composer's chip overlay (decorative — it sits ON TOP of the
         textarea text, so it renders only the pill, non-interactive);
       - the ChatPane user bubble (the message content — interactive tooltip
         only, the raw link target stays the clickable truth). -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import Icon from '../ui/Icon.vue';
import type { IconName } from '../../lib/icons';

const props = withDefaults(
  defineProps<{
    /** Skill, command, or file/folder reference (E9 user-bubble echo). */
    kind: 'skill' | 'command' | 'file' | 'folder';
    /** Display name without the trigger char. */
    display: string;
    /** Link target: absolute SKILL.md path, `command:<name>`, or the raw
     *  workspace-relative path of a file reference. */
    target: string;
    /** True when painted over the textarea text (Composer overlay): the chip
     *  is decoration for the identical underlying text — it must not intercept
     *  pointer events, and screen readers get the text from the textarea. */
    overlay?: boolean;
    /** M4 (US9.4): the engine truncated this file at its 1500-line injection
     *  budget — the chip paints a small「已截取」mark so the user knows the
     *  model saw only the head. Metadata-driven bubble chips only; the
     *  regex-fallback chip has no source for it. */
    truncated?: boolean;
  }>(),
  { overlay: false },
);

const emit = defineEmits<{
  /** File references open the preview when clicked (E9, US9.10) — the bubble
   *  host maps this to its `openFile` emit. Skills/commands keep their old
   *  non-interactive bubble rendering; folders are inert (nothing to open). */
  activate: [];
}>();

const { t } = useI18n();

const iconName = computed<IconName>(() => {
  if (props.kind === 'skill') return 'skill';
  if (props.kind === 'command') return 'terminal';
  if (props.kind === 'folder') return 'folder';
  return 'file';
});

const title = computed(() => {
  if (props.kind === 'skill') return t('composer.inlineChipSkill', { name: props.display });
  if (props.kind === 'command') return t('composer.inlineChipCommand', { name: props.display });
  if (props.kind === 'folder') return t('mention.drillInto');
  if (props.truncated) return t('mention.truncatedChipTitle', { name: props.display });
  return t('filePreview.preview');
});

// Only file chips are interactive in the bubble (open the preview); the
// others keep the historical non-interactive span.
const interactive = computed(() => props.kind === 'file' && !props.overlay);
</script>

<template>
  <!-- Overlay chips are aria-hidden (the textarea's own text is the accessible
       text); bubble chips are inline content inside the user message. -->
  <span
    class="ref-chip"
    :class="{ 'is-overlay': overlay, 'is-activatable': interactive }"
    :aria-hidden="overlay || undefined"
    :title="title"
    :data-kind="kind"
    :role="interactive ? 'button' : undefined"
    :tabindex="interactive ? 0 : undefined"
    @keydown.enter.prevent.stop="interactive && emit('activate')"
    @click.stop="interactive && emit('activate')"
  >
    <Icon :name="iconName" size="sm" />
    <span class="ref-chip-name">{{ display }}</span>
    <span v-if="truncated" class="ref-chip-truncated">{{ t('mention.truncatedMark') }}</span>
  </span>
</template>

<style scoped>
.ref-chip {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 0 5px;
  border-radius: var(--radius-xs);
  border: 1px solid var(--color-accent-bd);
  background: var(--color-accent-soft);
  color: var(--color-text);
  font-family: var(--font-mono);
  font-size: inherit;
  line-height: inherit;
  max-width: 220px;
  vertical-align: baseline;
}

.ref-chip :deep(svg) {
  flex: none;
  color: var(--color-accent);
}

/* Command chips swap the accent for the neutral command identity — same shape,
   different glyph family, so the two never read as one kind (US3.6). */
.ref-chip[data-kind='command'] {
  background: var(--color-surface-sunken);
  border-color: var(--color-line);
}
.ref-chip[data-kind='command'] :deep(svg) {
  color: var(--color-text-muted);
}

/* File-reference chips (E9 bubble echo) are clickable — they open the file
   preview. Visual identity stays in the file/folder glyph family (IconName
   'file'/'folder'), same neutral treatment as command chips. */
.ref-chip[data-kind='file'],
.ref-chip[data-kind='folder'] {
  background: var(--color-surface-sunken);
  border-color: var(--color-line);
}

.ref-chip.is-activatable {
  cursor: pointer;
}

.ref-chip.is-activatable:hover,
.ref-chip.is-activatable:focus-visible {
  border-color: var(--color-accent);
}

.ref-chip-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* M4 「已截取」 mark: a tiny badge after the name — muted so it reads as
   metadata, not a second chip. */
.ref-chip-truncated {
  flex: none;
  font-size: 0.7em;
  line-height: 1;
  padding: 1px 3px;
  border-radius: var(--radius-xs);
  background: var(--color-surface-sunken);
  color: var(--color-text-muted);
  white-space: nowrap;
}

/* Overlay variant: it must not shift the text flow (it paints over the link
   span the mirror measured) and must not eat caret/selection interactions. */
.ref-chip.is-overlay {
  pointer-events: none;
  user-select: none;
  position: absolute;
  top: 0;
  left: 0;
  /* Width/height/transform are set inline by the overlay host from the
     measured span rect. */
}
</style>
