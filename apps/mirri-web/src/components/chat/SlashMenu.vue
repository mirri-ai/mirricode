<!-- apps/mirri-web/src/components/chat/SlashMenu.vue -->
<!-- Popup list of slash commands shown above the Composer textarea. -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import {
  groupSlashItems,
  isBlankSkillDesc,
  skillDisplayName,
  type SlashCommand,
} from '../../lib/slashCommands';
import Icon from '../ui/Icon.vue';

const { t } = useI18n();

const props = withDefaults(
  defineProps<{
    items: SlashCommand[];
    activeIndex: number;
    /**
     * Render a "Commands" / "Skills" group title pair (the `/` menu). Skill
     * trigger menu (`$`, skill items only) passes `false` to skip titles.
     */
    grouped?: boolean;
  }>(),
  { grouped: true },
);

const emit = defineEmits<{
  select: [item: SlashCommand];
  hover: [index: number];
}>();

/**
 * Flat render plan: group titles (when `grouped`) interleaved with the items.
 * `flatIndex` is the item's real index in `props.items` — callers feed this
 * menu from `filterCommands`, which returns commands before skills (ADR-003),
 * so the grouped order always matches the caller's array and the
 * hover/active/scroll selection semantics stay identical in both modes.
 */
type SlashRow =
  | { kind: 'title'; key: string; title: string }
  | { kind: 'item'; key: string; item: SlashCommand; flatIndex: number };

const rows = computed<SlashRow[]>(() => {
  if (!props.grouped) {
    return props.items.map((item, flatIndex) => ({
      kind: 'item',
      key: `${item.name}-${flatIndex}`,
      item,
      flatIndex,
    }));
  }
  const { commands, skills } = groupSlashItems(props.items);
  const out: SlashRow[] = [];
  if (commands.length > 0) {
    out.push({ kind: 'title', key: 'group-commands', title: t('commands.groupCommands') });
    commands.forEach((item, i) => {
      out.push({ kind: 'item', key: `${item.name}-${i}`, item, flatIndex: i });
    });
  }
  if (skills.length > 0) {
    out.push({ kind: 'title', key: 'group-skills', title: t('commands.groupSkills') });
    skills.forEach((item, i) => {
      out.push({
        kind: 'item',
        key: `${item.name}-${commands.length + i}`,
        item,
        flatIndex: commands.length + i,
      });
    });
  }
  return out;
});

const itemRefs = ref<HTMLElement[]>([]);

watch(
  () => props.activeIndex,
  (idx) => {
    itemRefs.value[idx]?.scrollIntoView({ block: 'nearest' });
  },
);
</script>

<template>
  <div v-if="items.length > 0" class="slash-menu" role="listbox">
    <template v-for="row in rows" :key="row.key">
      <div
        v-if="row.kind === 'title'"
        class="slash-group-title"
        aria-hidden="true"
      >
        {{ row.title }}
      </div>
      <div
        v-else-if="row.kind === 'item'"
        :ref="(el) => { if (el) itemRefs[row.flatIndex] = el as HTMLElement }"
        class="slash-item"
        :class="{ active: row.flatIndex === props.activeIndex }"
        role="option"
        :aria-selected="row.flatIndex === props.activeIndex"
        @mouseenter="emit('hover', row.flatIndex)"
        @mousedown.prevent="emit('select', row.item)"
      >
        <span class="slash-name">
          <span v-if="row.item.isSkill" class="skill-icon" aria-hidden="true">
            <Icon name="skill" size="sm" />
          </span>
          {{ row.item.isSkill ? skillDisplayName(row.item) : row.item.name }}
        </span>
        <span class="slash-desc">{{
          row.item.isSkill
            ? isBlankSkillDesc(row.item.desc)
              ? t('commands.noDescription')
              : row.item.desc
            : t(row.item.desc)
        }}</span>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* `[role="listbox"]` raises specificity (0,3,0) so the redesign's surface +
   shadow-md win over any global menu styles. */
.slash-menu[role="listbox"] {
  position: absolute;
  bottom: calc(100% + 4px);
  left: 0;
  right: 0;
  padding: var(--space-1);
  background: var(--color-surface-raised);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-sm);
  z-index: var(--z-dropdown);
  max-height: 240px;
  overflow-y: auto;
}

.slash-item {
  display: grid;
  grid-template-columns: minmax(90px, 32%) minmax(0, 1fr);
  align-items: start;
  gap: 10px;
  padding: 6px 10px;
  cursor: pointer;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  border-radius: var(--radius-sm);
}

/* Group title above each section — SectionLabel spec (design-system view):
   13px / 700 / uppercase / +.08em tracking / faint color. Horizontal padding
   matches the .slash-item rows (10px) so titles align with the list below. */
.slash-group-title {
  padding: var(--space-2) 10px var(--space-1);
  font-size: var(--text-sm);
  font-weight: var(--weight-semibold);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--color-text-faint);
}

.slash-item:hover {
  background: var(--color-surface-sunken);
}
.slash-item.active {
  background: var(--color-accent-soft);
}
.slash-item.active .slash-name {
  color: var(--color-accent-hover);
}

.slash-name {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  color: var(--color-accent);
  font-weight: 500;
  min-width: 0;
  line-height: var(--leading-normal);
  overflow-wrap: anywhere;
}

/* Skill glyph — 14px box with the svg one notch smaller (13px), same as
   the MentionMenu file-type icons so rows stay aligned. */
.skill-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  color: var(--color-text-faint);
  flex-shrink: 0;
}
.slash-name .skill-icon :deep(svg) {
  width: 13px;
  height: 13px;
  display: block;
}
.slash-item:hover .skill-icon,
.slash-item.active .skill-icon {
  color: var(--color-text-muted);
}

.slash-desc {
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  min-width: 0;
  line-height: var(--leading-normal);
  overflow-wrap: anywhere;
}

@media (max-width: 520px) {
  .slash-item {
    grid-template-columns: minmax(0, 1fr);
    gap: 2px;
  }
}

/* ---- Menu surface defaults ---- */
.slash-menu { border-radius: var(--radius-lg); box-shadow: var(--sh); }
.slash-desc { font-family: var(--sans); }
</style>
