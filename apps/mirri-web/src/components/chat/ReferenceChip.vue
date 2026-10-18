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
    /** Skill (`SKILL.md` target) or command (`command:` target). */
    kind: 'skill' | 'command';
    /** Display name without the trigger char. */
    display: string;
    /** Link target: absolute SKILL.md path, or `command:<name>`. */
    target: string;
    /** True when painted over the textarea text (Composer overlay): the chip
     *  is decoration for the identical underlying text — it must not intercept
     *  pointer events, and screen readers get the text from the textarea. */
    overlay?: boolean;
  }>(),
  { overlay: false },
);

const { t } = useI18n();

const iconName = computed<IconName>(() => (props.kind === 'skill' ? 'skill' : 'terminal'));

const title = computed(() =>
  props.kind === 'skill'
    ? t('composer.inlineChipSkill', { name: props.display })
    : t('composer.inlineChipCommand', { name: props.display }),
);
</script>

<template>
  <!-- Overlay chips are aria-hidden (the textarea's own text is the accessible
       text); bubble chips are inline content inside the user message. -->
  <span
    class="ref-chip"
    :class="{ 'is-overlay': overlay }"
    :aria-hidden="overlay || undefined"
    :title="title"
    :data-kind="kind"
  >
    <Icon :name="iconName" size="sm" />
    <span class="ref-chip-name">{{ display }}</span>
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

.ref-chip-name {
  overflow: hidden;
  text-overflow: ellipsis;
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
