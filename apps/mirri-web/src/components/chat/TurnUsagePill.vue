<!-- apps/mirri-web/src/components/chat/TurnUsagePill.vue -->
<!-- Per-turn token usage pill (usage-visibility Task 6): a compact pill on the
     assistant run footer; clicking it opens an anchored detail panel with the
     exact bucket breakdown and the anti-fake-full-hit cache percentage. -->
<script lang="ts">
// Plain named-export block: the repo has no @vue/test-utils, so the row
// construction is a pure named export and tested directly with vitest
// (test/turn-usage-pill.test.ts).
import { formatCacheHitPercent, formatExactTokens } from '../../lib/usageStats';
import type { TurnUsage } from '../../api/daemon/agentEventProjector';

export type TurnUsageRowKey =
  | 'model'
  | 'cacheHit'
  | 'input'
  | 'cacheRead'
  | 'cacheWrite'
  | 'output';

export interface TurnUsageRow {
  key: TurnUsageRowKey;
  value: string;
}

/** Build the detail-panel rows. A bucket that was never reported renders no
 *  row, and the cache-write row is gated on a non-zero write bucket so an
 *  unreported write bucket is not shown as a fake zero (SDR-003 stopgap —
 *  a model→capability map is not wired yet, so a genuine zero on a
 *  capability-true model is hidden; see the usage-visibility ROADMAP).
 *  The cache-hit denominator always includes the write bucket (SDR-001):
 *  billed = uncached input + cache read + cache write. */
export function buildTurnUsageRows(usage: TurnUsage): TurnUsageRow[] {
  const rows: TurnUsageRow[] = [];
  if (usage.models.length > 0) rows.push({ key: 'model', value: usage.models.join(', ') });
  const billed = usage.inputTokens + usage.cacheReadTokens + usage.cacheCreationTokens;
  const hit = formatCacheHitPercent(usage.cacheReadTokens, billed, 1);
  if (usage.cacheReadTokens > 0 && hit !== null) rows.push({ key: 'cacheHit', value: `${hit}%` });
  rows.push({ key: 'input', value: formatExactTokens(usage.inputTokens) });
  rows.push({ key: 'cacheRead', value: formatExactTokens(usage.cacheReadTokens) });
  if (usage.cacheCreationTokens > 0) rows.push({ key: 'cacheWrite', value: formatExactTokens(usage.cacheCreationTokens) });
  rows.push({ key: 'output', value: formatExactTokens(usage.outputTokens) });
  return rows;
}
</script>

<script setup lang="ts">
import { computed, nextTick, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import Icon from '../ui/Icon.vue';
import Tooltip from '../ui/Tooltip.vue';
import { formatTokens } from '../../lib/formatTokens';

const props = defineProps<{
  /** The turn's usage; undefined renders nothing (defensive — callers
   *  usually v-if on the lookup instead). */
  usage?: TurnUsage;
}>();

const { t } = useI18n();

/** Compact total for the pill label: every token the turn consumed. */
const totalTokens = computed(() => {
  const u = props.usage;
  if (!u) return 0;
  return u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheCreationTokens;
});

const rows = computed(() => (props.usage ? buildTurnUsageRows(props.usage) : []));

// Anchored panel — same fixed-positioning + viewport-clamping pattern as
// chat/OpenInButton.vue's menuStyle. Closes on Escape, outside press and resize.
const open = ref(false);
const pillRef = ref<HTMLElement | null>(null);
const panelRef = ref<HTMLElement | null>(null);
const panelStyle = ref<Record<string, string>>({});

function onDocMouseDown(event: MouseEvent): void {
  const target = event.target as Node;
  if (panelRef.value?.contains(target) || pillRef.value?.contains(target)) return;
  closePanel();
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Escape') closePanel();
}

function onResize(): void {
  closePanel();
}

async function togglePanel(): Promise<void> {
  if (open.value) {
    closePanel();
    return;
  }
  open.value = true;
  document.addEventListener('mousedown', onDocMouseDown);
  document.addEventListener('keydown', onKeydown);
  window.addEventListener('resize', onResize);
  await nextTick();
  const pill = pillRef.value;
  const panel = panelRef.value;
  if (!pill || !panel) return;
  const r = pill.getBoundingClientRect();
  const gap = 4;
  const margin = 8;
  const panelW = panel.offsetWidth;
  const panelH = panel.offsetHeight;
  let top = r.bottom + gap;
  if (top + panelH > window.innerHeight - margin) {
    top = Math.max(margin, r.top - panelH - gap);
  }
  let left = r.left;
  if (left + panelW > window.innerWidth - margin) left = window.innerWidth - margin - panelW;
  if (left < margin) left = margin;
  panelStyle.value = {
    top: `${Math.round(top)}px`,
    left: `${Math.round(left)}px`,
  };
}

function closePanel(): void {
  open.value = false;
  document.removeEventListener('mousedown', onDocMouseDown);
  document.removeEventListener('keydown', onKeydown);
  window.removeEventListener('resize', onResize);
}

onUnmounted(closePanel);
</script>

<template>
  <Tooltip :text="t('conversation.turnUsage.title')">
    <button
      ref="pillRef"
      type="button"
      class="tu-pill"
      :class="{ open }"
      :aria-label="t('conversation.turnUsage.title')"
      :aria-expanded="open"
      @click.stop="togglePanel"
    >
      <!-- 'sort' is the icon registry's bar-chart-like glyph (Ri sort-desc). -->
      <Icon name="sort" size="sm" />
      <span class="tu-total">{{ t('conversation.turnUsage.pill', { total: formatTokens(totalTokens) }) }}</span>
    </button>
  </Tooltip>
  <div v-if="open" ref="panelRef" class="tu-panel" :style="panelStyle" @click.stop>
    <div class="tu-title">{{ t('conversation.turnUsage.title') }}</div>
    <dl class="tu-rows">
      <template v-for="row in rows" :key="row.key">
        <dt>{{ t(`conversation.turnUsage.${row.key}`) }}</dt>
        <dd>{{ row.value }}</dd>
      </template>
    </dl>
  </div>
</template>

<style scoped>
/* Pill — same muted→accent family as the footer's duration/copy actions. */
.tu-pill {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  padding: 2px 5px;
  background: none;
  border: none;
  border-radius: var(--radius-sm);
  color: var(--muted);
  font: inherit;
  font-size: var(--text-base);
  line-height: 1;
  cursor: pointer;
  opacity: 0.7;
  transition: opacity 0.12s, color 0.12s, background-color 0.12s;
  min-height: 22px;
  box-sizing: border-box;
}
.tu-pill svg {
  display: block;
  flex: none;
}
.tu-pill:hover,
.tu-pill.open {
  opacity: 1;
  color: var(--color-accent);
  background: var(--hover);
}
.tu-total {
  white-space: nowrap;
}

/* Anchored detail panel (fixed, above the transcript's stacking contexts). */
.tu-panel {
  position: fixed;
  top: 0;
  left: 0;
  z-index: var(--z-dropdown);
  min-width: 220px;
  max-width: min(320px, calc(100vw - 16px));
  padding: 10px 12px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
  box-shadow: var(--shadow-xs);
}
.tu-title {
  font-size: var(--ui-font-size-sm);
  font-weight: 500;
  color: var(--color-text-muted);
  margin-bottom: 8px;
}
.tu-rows {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  gap: 4px 14px;
  margin: 0;
}
.tu-rows dt {
  font-size: var(--text-base);
  color: var(--muted);
}
.tu-rows dd {
  margin: 0;
  font-family: var(--font-mono);
  font-size: var(--text-base);
  color: var(--color-text);
  text-align: right;
  overflow-wrap: anywhere;
}
</style>
