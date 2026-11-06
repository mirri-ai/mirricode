<!-- apps/mirri-web/src/components/chat/UsageStatsBar.vue -->
<!-- Session-level usage stats bar (usage-visibility Task 7): a compact,
     muted line above the composer. Group order (ADR-004 priced buckets):
     [turn count] | cache hit % | [after-cold-start hit %] | cache read |
     uncached input | [cache write] | output. The input side never shows a
     single blended number -- each priced tier keeps its own count, and the
     hit percentage denominator stays the three-bucket sum. The live event
     accumulation (subagent-inclusive, agentEventProjector's
     sessionUsageUpdated) is the authoritative source -- the snapshot path
     only back-fills before any event arrives. -->
<script lang="ts">
// Plain named-export block: the repo has no @vue/test-utils, so the group
// construction is a pure named export and tested directly with vitest
// (test/usage-stats-bar.test.ts).
import { i18n } from '../../i18n';
import {
  billedInputTokens,
  formatCacheHitPercent,
  formatExactTokens,
  splitInputBuckets,
  type InputSegmentKey,
  type StatsGroup,
} from '../../lib/usageStats';
import type { AppSessionUsage } from '../../api/types';

/** Build the stats-bar groups (locale-formatted), in the fixed ADR-004
 *  order: [turn count] | cache hit | [after-cold-start hit] | cache read |
 *  uncached input | [cache write] | output. Each group carries its
 *  drop-priority key -- this function is the single source of truth for
 *  both which groups exist and which key each maps to.
 *
 *  Honesty rules (SDR-001 / US1.2 / SDR-005):
 *  - The three input buckets are shown per priced tier -- never blended.
 *    The cache-write group is omitted when the session never wrote cache;
 *    the cache-read group shows even as a bare 0 (a zero read is the real
 *    fact "never hit once"); the output group is gated on output > 0 alone,
 *    NOT on the input buckets, so a zero read never hides the output.
 *  - The hit group is dropped when the percentage is null (no billed
 *    input), and the hit percentage never displays 100 for a partial hit
 *    (anti-100 is built into formatCacheHitPercent).
 *  - SDR-004: the after-cold-start hit subtracts the session's first LLM
 *    call (the cold-start miss) from both the numerator and the denominator.
 *    It is dropped when the first call is unknown, or when it was the only
 *    call (nothing left to divide -- mirroring the primary hit's omission).
 *  - An all-failed session (no billed input AND no output, US1.2) renders
 *    ONLY the turn count when at least one turn completed, and nothing at
 *    all otherwise -- the whole bar disappears rather than showing a fake
 *    "Cache hit 0%". */
export function buildStatsGroups(u: AppSessionUsage): StatsGroup[] {
  const billed = billedInputTokens(u);
  const turnGroup = (): StatsGroup | null =>
    u.turnCount > 0 ? { key: 'turnCount', text: i18n.global.t('status.usageTurnCount', { n: u.turnCount }) } : null;
  const outputGroup = (): StatsGroup => ({
    key: 'output',
    text: i18n.global.t('status.usageOutput', { n: formatExactTokens(u.outputTokens) }),
  });
  // US1.2 takes priority over every per-group rule below. With no billed
  // input the hit percentage and every input-bucket segment are unassertable
  // (a 0% hit is a claim about data; there is none). Output, though, is a
  // billed fact on its own and survives (US1.2 rule 3); with no output
  // either, only the turn count remains — or nothing at all.
  if (billed === 0) {
    const turn = turnGroup();
    if (u.outputTokens === 0) {
      return turn ? [turn] : [];
    }
    return turn ? [turn, outputGroup()] : [outputGroup()];
  }
  const groups: StatsGroup[] = [];
  const turn = turnGroup();
  if (turn) groups.push(turn);
  const hit = formatCacheHitPercent(u.cacheReadTokens, billed);
  if (hit !== null) {
    groups.push({ key: 'cacheHit', text: i18n.global.t('status.usageCacheHit', { pct: hit }) });
  }
  // SDR-004: the after-cold-start hit subtracts the session's first LLM
  // call (the cold-start miss) from BOTH the numerator and the denominator.
  // Omitted when the first call is unknown (no baseline to subtract) or
  // when it was the only call (nothing left to divide -- mirroring the
  // primary hit's no-billed-input omission).
  if (u.firstCall) {
    const after = {
      inputTokens: u.inputTokens - u.firstCall.inputTokens,
      outputTokens: u.outputTokens - u.firstCall.outputTokens,
      cacheReadTokens: u.cacheReadTokens - u.firstCall.cacheReadTokens,
      cacheCreationTokens: u.cacheCreationTokens - u.firstCall.cacheCreationTokens,
    };
    const hitAfter = formatCacheHitPercent(after.cacheReadTokens, billedInputTokens(after));
    if (hitAfter !== null) {
      groups.push({ key: 'afterColdStart', text: i18n.global.t('status.usageAfterColdStart', { pct: hitAfter }) });
    }
  }
  const segmentLabel: Record<InputSegmentKey, string> = {
    cacheRead: 'status.usageCacheRead',
    input: 'status.usageInput',
    cacheWrite: 'status.usageCacheWrite',
  };
  for (const seg of splitInputBuckets(u)) {
    groups.push({ key: seg.key, text: i18n.global.t(segmentLabel[seg.key], { n: formatExactTokens(seg.tokens) }) });
  }
  if (u.outputTokens > 0) {
    groups.push(outputGroup());
  }
  return groups;
}

</script>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import Tooltip from '../ui/Tooltip.vue';
import { fitSegments, measureGroupWidths } from '../../lib/usageStats';
import type { GroupWidths, SizedSegment } from '../../lib/usageStats';

const props = defineProps<{
  /** The active session's usage (session.usage from useWorkspaceState).
   *  Null/undefined (no session yet) renders nothing. */
  usage?: AppSessionUsage | null;
}>();

const groups = computed<StatsGroup[]>(() => (props.usage ? buildStatsGroups(props.usage) : []));

/** The bar's full text -- shown in the hover Tooltip (and the a11y label)
 *  so the complete line stays readable no matter what width fitting hides. */
const fullText = computed(() => groups.value.map((g) => g.text).join(' | '));

const container = ref<HTMLElement>();
const availableWidth = ref<number | null>(null);
/** Measured group widths keyed by group key -- never by index: a group
 *  dropped from the DOM keeps its last measured value, so dropping groups
 *  cannot pollute the next fitting decision. */
const groupWidths = ref<GroupWidths>({});

/** The groups that fit the container width. Width-fitting (ADR-004) drops
 *  the lowest-value groups first -- turn count, then after-cold-start, then
 *  output, then the cache tiers -- while the Tooltip above always carries
 *  the full text. Falls back to showing everything until a width is
 *  measured. */
const visibleGroups = computed<StatsGroup[]>(() => {
  if (availableWidth.value === null) return groups.value;
  const segments: SizedSegment[] = groups.value.map((g) => ({
    key: g.key,
    text: g.text,
    width: groupWidths.value[g.key] ?? 0,
  }));
  return fitSegments(segments, availableWidth.value).map((s) => ({ key: s.key, text: s.text }));
});

/** Re-measure the rendered group widths after every change that can alter
 *  them (usage update, locale switch, container resize). Groups are looked
 *  up by their `data-group` key, so the measurement stays correct after
 *  width fitting has dropped groups from the DOM (they simply keep their
 *  last measured width); separator spans are never matched. */
function measure(): void {
  groupWidths.value = measureGroupWidths(
    container.value?.querySelectorAll<HTMLElement>('[data-group]') ?? [],
    groupWidths.value,
  );
}

let observer: ResizeObserver | undefined;

onMounted(() => {
  // jsdom has no ResizeObserver -- the fitting stays dormant there (and in
  // any environment without it), showing the full line instead.
  if (typeof ResizeObserver === 'undefined') return;
  measure();
  if (container.value) {
    observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        availableWidth.value = entry.contentRect.width;
      }
      measure();
    });
    observer.observe(container.value);
  }
});

watch([groups, availableWidth], () => {
  if (typeof ResizeObserver === 'undefined') return;
  requestAnimationFrame(measure);
});

onBeforeUnmount(() => {
  observer?.disconnect();
});
</script>

<template>
  <Tooltip v-if="fullText" :text="fullText">
    <div ref="container" class="usage-bar" :aria-label="fullText">
      <template v-for="(group, i) in visibleGroups" :key="i">
        <span v-if="i > 0" class="usage-sep">|</span>
        <span class="usage-group" :data-group="group.key">{{ group.text }}</span>
      </template>
    </div>
  </Tooltip>
</template>

<style scoped>
/* Muted meta line, matching the dock's quiet chrome (e.g. the goal strip's
 * caption tone). Overflowing groups are dropped by priority in script
 * (fitSegments), not by CSS truncation -- the Tooltip carries the full
 * text. */
.usage-bar {
  box-sizing: border-box;
  max-width: 100%;
  padding: 0 4px 2px;
  font-size: var(--text-base);
  line-height: 1.4;
  color: var(--muted);
  text-align: center;
  white-space: nowrap;
  overflow: hidden;
  user-select: none;
}
.usage-group {
  white-space: nowrap;
}
.usage-sep {
  color: var(--color-line);
  margin: 0 8px;
}
</style>
