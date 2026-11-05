<!-- apps/mirri-web/src/components/chat/tool-calls/DelegationCard.vue -->
<!-- The unified delegation card (supervision L0, SDR-007): head line —
     description · N/M chip · status · Open — plus one collapsed member row
     per started member (identity · status · one activity area). Expanded
     shows only light facts (prompt, output). Process detail NEVER renders
     here — it lives in the supervision panel (SDR-002), with one exception
     (ADR-012 ①): a row's activity area is the member's recent-activity
     lightweight timeline — tool names with status colouring and one-line
     argument digests, plus narrative first-line summaries. Tool output and
     full arguments stay out. Cardinality is data, not paradigm (SDR-001):
     N=1 merely omits the aggregate structures, and a future AgentOverview
     member card reuses this shape (Phase-3 seam). Honest progress only
     (SDR-003): counts, phases, activity markers — no percentages, no ETA. -->
<script setup lang="ts">
import { computed, inject, ref, type Ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { FilePreviewRequest, ToolCall, ToolMedia } from '../../../types';
import { toolGlyph, toolLabel } from '../../../lib/toolMeta';
import { parseSwarmResult } from '../../../lib/parseSwarmResult';
import { delegationSummary } from '../../../lib/delegationCard';
import { buildSwarmCardRows, buildTimelineChips } from '../../../lib/swarmCardRows';
import { memberColorFor } from '../../../lib/memberColor';
import { formatDuration } from '../../chatTurnRendering';
import ToolRow from '../ToolRow.vue';

const { t } = useI18n();

const props = withDefaults(
  defineProps<{
    tool: ToolCall;
    mobile?: boolean;
    stackPosition?: 'single' | 'first' | 'middle' | 'last';
    toolDiffPanel?: boolean;
  }>(),
  { mobile: false, stackPosition: 'single', toolDiffPanel: false },
);

// US3.8 (ADR-7): live elapsed while running with a server anchor — the
// frozen `timing` (server duration or flush freeze) wins once settled.
const tick = inject<Readonly<Ref<number>> | null>('runningToolTick', null);
const liveTime = computed(() =>
  tick?.value !== undefined && props.tool.status === 'running' && props.tool.startedAtMs !== undefined
    ? formatDuration(Math.max(0, tick.value - props.tool.startedAtMs))
    : undefined,
);

const emit = defineEmits<{
  openMedia: [media: ToolMedia];
  openFile: [target: FilePreviewRequest];
  openToolDiff: [id: string];
  openAgent: [toolCallId: string, memberId?: string];
}>();

const resolveSwarmMembers = inject<(id: string) => Parameters<typeof delegationSummary>[0]['members'] | undefined>(
  'resolveSwarmMembers',
);
const resolveAgentTaskId = inject<(id: string) => string | undefined>('resolveAgentTaskId');
const resolveAgentModel = inject<(id: string) => string | undefined>('resolveAgentModel');

const members = computed(() => resolveSwarmMembers?.(props.tool.id) ?? []);
const result = computed(() => parseSwarmResult(props.tool.output));
// 等待用户（approval/question，事件驱动）成员的活动行 i18n 文案。
const awaitingLabel = computed(() => t('tools.swarm.awaitingApproval'));
const summary = computed(() =>
  delegationSummary({
    toolStatus: props.tool.status as 'running' | 'ok' | 'error',
    members: members.value,
    result: result.value,
    awaitingLabel: awaitingLabel.value,
  }),
);

// SDR-007: the collapsed card lists every actually-started member — live
// members while running, the result's rows after a refresh; unlaunched
// members take no row (they never started).
const rows = computed(() => buildSwarmCardRows(members.value, result.value, awaitingLabel.value));

// ADR-012 ①: each member row's activity area is a horizontal lightweight
// timeline over the member's recent steps/segments, replacing the single
// ever-replacing activity line. Rows with no timeline data (result-only after
// a refresh, a member that never started) keep their single-line summary.
const thinkingLabel = computed(() => t('tools.swarm.thinkingLabel'));
const rowViews = computed(() =>
  rows.value.map((row) => {
    const chips =
      row.timeline && row.timeline.elements.length > 0
        ? buildTimelineChips(row.timeline, thinkingLabel.value)
        : undefined;
    // Awaiting / suspended (ADR-008 ②): the member is waiting on the user or
    // on its own pause, so the waiting copy stays the row's activity and the
    // timeline is parked context beside it — dimmed, never the live signal.
    const frozen = row.phase === 'awaiting' || row.phase === 'suspended';
    return { row, chips, frozen };
  }),
);

const label = computed(() => toolLabel(props.tool.name));
const glyph = computed(() => toolGlyph(props.tool.name));

// Open-guard (US1.3): the entry exists only when the panel has something to
// show — a live member task or a member row. Never a dead button.
const canOpen = computed(
  () => members.value.length > 0 || resolveAgentTaskId?.(props.tool.id) !== undefined,
);
const model = computed(() => resolveAgentModel?.(props.tool.id));

const PHASE_CLS = {
  queued: 'dc-q',
  working: 'dc-r',
  suspended: 'dc-w',
  completed: 'dc-o',
  failed: 'dc-f',
} as const;

const segments = computed(() => {
  const entries = Object.entries(summary.value.counts) as [
    keyof typeof PHASE_CLS,
    number,
  ][];
  return entries.filter(([, n]) => n > 0).map(([phase, n]) => ({ cls: PHASE_CLS[phase], n }));
});

function parseInput(arg: string): { prompt?: string; description?: string } {
  if (!arg) return {};
  try {
    const obj = JSON.parse(arg) as Record<string, unknown>;
    const promptRaw = obj['prompt'] ?? obj['prompt_template'];
    return {
      prompt: typeof promptRaw === 'string' ? promptRaw : undefined,
      description: typeof obj['description'] === 'string' ? obj['description'] : undefined,
    };
  } catch {
    return {};
  }
}
const input = computed(() => parseInput(props.tool.arg));
const hasOutput = computed(() => !!props.tool.output && props.tool.output.length > 0);
const canExpand = computed(
  () =>
    Boolean(input.value.prompt) ||
    members.value.length > 0 ||
    hasOutput.value,
);
// Collapsed by default in EVERY state — running included (SDR-002: the
// conversation area is never a monitoring console). Manual toggles stick.
const open = ref(false);
function toggle(): void {
  if (canExpand.value) open.value = !open.value;
}
</script>

<template>
  <div class="dc" :class="{ err: summary.aggregateStatus === 'error', attn: summary.aggregateStatus === 'attention', open }">
    <!-- attention 档在行上保留 running 状态点（工具仍在跑，成员只是等你决策——
         ADR-008 ② 呈现层聚合，非新引擎状态）；档位差异由卡面 attn 视觉与
         「需要你处理」标签承担。 -->
    <ToolRow
      :status="summary.aggregateStatus === 'attention' ? 'running' : summary.aggregateStatus"
      :icon="glyph"
      :name="label"
      :arg="!open ? (input.description ?? '') : ''"
      :time="liveTime ?? tool.timing"
      :open="open"
      :expandable="canExpand"
      :stacked="stackPosition !== 'single'"
      :stack-position="stackPosition"
      @toggle="toggle"
    >
      <template #trailing>
        <span v-if="summary.aggregateStatus === 'attention'" class="dc-attn">
          {{ t('tools.swarm.attention') }}
        </span>
        <span v-if="summary.total > 1" class="dc-chip">{{ summary.done }} / {{ summary.total }}</span>
        <span
          v-if="summary.total > 1 && segments.length > 1"
          class="dc-seg"
          aria-hidden="true"
        >
          <i v-for="(s, i) in segments" :key="i" :class="s.cls" :style="{ flex: s.n }" />
        </span>
        <span v-if="model" class="dc-model">{{ model }}</span>
        <button
          v-if="canOpen"
          type="button"
          class="dc-open"
          @click.stop="emit('openAgent', tool.id)"
        >
          {{ t('tasks.openDetail') }}
        </button>
      </template>
      <div v-if="open" class="dc-body">
        <div v-if="input.prompt" class="dc-task">{{ input.prompt }}</div>
        <div v-if="members.length === 0 && hasOutput" class="bb-code">
          <div v-for="(line, i) in tool.output ?? []" :key="i">{{ line }}</div>
        </div>
      </div>
    </ToolRow>
    <!-- SDR-007 member rows: always visible while collapsed — identity,
         status, one activity area. ADR-012 ①: that area is a lightweight
         timeline of the member's recent activity when the row has timeline
         data, and the single-line activity summary otherwise. Process detail
         never renders here. -->
    <button
      v-for="{ row, chips, frozen } in rowViews"
      :key="row.id"
      type="button"
      class="dc-row"
      :class="{ err: row.phase === 'failed', frozen }"
      @click="emit('openAgent', tool.id, row.id)"
    >
      <i class="dc-mdot" :style="{ background: memberColorFor(row.id) }" />
      <span class="dc-mname">{{ row.name }}</span>
      <!-- Frozen rows (ADR-008 ②): the waiting copy stays the activity; the
           timeline keeps its place beside it, dimmed, so the row reads as
           parked context rather than a live signal. -->
      <template v-if="frozen">
        <span class="dc-row-act">{{ row.activity }}</span>
        <span v-if="chips" class="dc-tl" aria-hidden="true">
          <span v-if="chips.headTruncated" class="dc-tl-more">…</span>
          <span v-for="c in chips.chips" :key="c.id" class="dc-tl-el" :class="`tl-${c.tone}`">
            {{ c.text }}
          </span>
        </span>
      </template>
      <!-- Every other row: the timeline takes the activity position. -->
      <span v-else-if="chips" class="dc-tl">
        <span v-if="chips.headTruncated" class="dc-tl-more">…</span>
        <span v-for="c in chips.chips" :key="c.id" class="dc-tl-el" :class="`tl-${c.tone}`">
          {{ c.text }}
        </span>
      </span>
      <span v-else class="dc-row-act">{{ row.activity }}</span>
      <span class="dc-mphase">
        {{ t(`tools.swarm.phase${row.phase[0]!.toUpperCase()}${row.phase.slice(1)}`) }}
      </span>
    </button>
  </div>
</template>

<style scoped>
.dc {
  border-radius: var(--radius-md);
}
.dc.err {
  /* Honest anomaly marker (US1.4): breathing border, static under
     prefers-reduced-motion (N4). */
  animation: dc-err-pulse 2s ease-in-out infinite;
}
@keyframes dc-err-pulse {
  0%, 100% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--color-danger) 30%, transparent); }
  50% { box-shadow: 0 0 0 2px color-mix(in srgb, var(--color-danger) 30%, transparent); }
}
@media (prefers-reduced-motion: reduce) {
  .dc.err { animation: none; box-shadow: inset 0 0 0 1px var(--color-danger); }
}
/* attention 档视觉（ADR-008 ②）：区别于 error 的中性强调——静态 warning
   描边，无动画（reduced-motion 天然安全），不取任何「异常」语义。 */
.dc.attn {
  box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--color-warning) 40%, transparent);
}
.dc-attn {
  flex: none;
  color: var(--color-warning);
  font: var(--text-xs) var(--font-ui);
  font-weight: var(--weight-medium);
  padding: 1px 7px;
  border: 1px solid var(--color-warning-bd);
  border-radius: var(--radius-full);
  background: var(--color-warning-soft);
}

.dc-chip {
  color: var(--color-text-muted);
  font-family: var(--font-mono);
  flex: none;
}
.dc-row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  min-height: 26px;
  padding: 2px 4px;
  border: none;
  border-radius: var(--radius-xs);
  background: transparent;
  cursor: pointer;
  text-align: left;
}
.dc-row:hover,
.dc-row:focus-visible {
  background: var(--color-surface-sunken);
  outline: none;
}
.dc-row:focus-visible {
  box-shadow: inset 0 0 0 2px var(--color-accent-soft);
}
.dc-row.err .dc-mdot {
  background: var(--color-danger);
}
.dc-row-act {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--color-text-faint);
  font-size: var(--text-xs);
}
/* ADR-012 ① lightweight timeline: one horizontal line of chips inside the
   member row, oldest → newest left to right. The list is pinned to the row's
   right edge (justify-content: flex-end), so a crowded window overflows on the
   LEFT: the newest activity — the thing the row exists to show — stays
   legible next to the phase label, and the eye tracks a fixed "now" edge.
   Nothing wraps and nothing scrolls, so a full timeline can never grow the row
   past its single-line height (the head ellipsis clips first in that extreme,
   where the cut edge itself already reads as truncation). */
.dc-tl {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 6px;
  overflow: hidden;
  white-space: nowrap;
  font-size: var(--text-xs);
}
.dc-tl-more {
  flex: none;
  color: var(--color-text-faint);
}
.dc-tl-el {
  flex: none;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 160px;
}
/* Colour rules (ADR-012 ①②): a running tool is the accent, a failed tool the
   anomaly colour, every settled tool recedes to the faint default — and a
   narrative element colours by position, so the newest one (the current
   activity) takes the emphasis colour. */
.dc-tl-el.tl-active {
  color: var(--color-text);
}
.dc-tl-el.tl-run {
  color: var(--color-accent);
}
.dc-tl-el.tl-err {
  color: var(--color-danger);
}
.dc-tl-el.tl-done {
  color: var(--color-text-faint);
}
/* Frozen rows (ADR-008 ②): the timeline is parked context, so it reads dimmer
   and static. The waiting copy beside it keeps full legibility — it is the
   one thing the user must act on — and keeps its natural width (shrinking with
   an ellipsis only when the row runs out of room) instead of splitting the
   row with the timeline. */
.dc-row.frozen .dc-row-act {
  flex: 0 1 auto;
}
.dc-row.frozen .dc-tl {
  opacity: 0.55;
}
.dc-seg {
  display: inline-flex;
  width: 36px;
  height: 4px;
  border-radius: var(--radius-full);
  overflow: hidden;
  gap: 1px;
  flex: none;
}
.dc-seg i {
  height: 100%;
  border-radius: var(--radius-full);
  min-width: 2px;
}
.dc-o { background: var(--color-success); }
.dc-r { background: var(--color-accent); }
.dc-w { background: var(--color-warning); }
.dc-f { background: var(--color-danger); }
.dc-q { background: var(--color-line); }

.dc-model {
  flex: none;
  padding: 1px 6px;
  border-radius: var(--radius-xs);
  background: var(--color-surface-sunken);
  color: var(--color-text-faint);
  font: var(--text-xs) var(--font-mono);
  white-space: nowrap;
}
.dc-open {
  flex: none;
  background: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-xs);
  color: var(--color-text-muted);
  font: var(--text-xs) var(--font-ui);
  padding: 1px 7px;
  cursor: pointer;
}
.dc-open:hover {
  color: var(--color-text);
  background: var(--color-surface-sunken);
}
.dc-open:focus-visible {
  outline: 2px solid var(--color-accent-soft);
  outline-offset: 1px;
}

.dc-body {
  padding: 4px 0 8px;
}
.dc-task {
  color: var(--color-text);
  white-space: pre-wrap;
  word-break: break-word;
  margin-bottom: 6px;
}
.dc-mdot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex: none;
}
.dc-mname {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--color-text);
  font-size: var(--text-sm);
}
.dc-mphase {
  flex: none;
  font: var(--text-xs) var(--font-mono);
  color: var(--color-text-faint);
}
.bb-code {
  padding: 11px 13px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
}
</style>
