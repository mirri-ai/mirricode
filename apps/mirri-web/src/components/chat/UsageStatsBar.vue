<!-- apps/mirri-web/src/components/chat/UsageStatsBar.vue -->
<!-- Session-level usage stats bar (usage-visibility Task 7): a compact,
     muted line above the composer showing the whole session's cache hit
     percentage, the after-cold-start hit (SDR-004, when the first call is
     known), and in/out token totals. The live event accumulation
     (subagent-inclusive, agentEventProjector's sessionUsageUpdated) is the
     authoritative source — the snapshot path only back-fills before any
     event arrives. -->
<script lang="ts">
// Plain named-export block: the repo has no @vue/test-utils, so the group
// construction is a pure named export and tested directly with vitest
// (test/usage-stats-bar.test.ts).
import { i18n } from '../../i18n';
import { billedInputTokens, formatCacheHitPercent, formatExactTokens } from '../../lib/usageStats';
import type { AppSessionUsage } from '../../api/types';

/** Build the stats-bar groups (locale-formatted). Group order is fixed:
 *  cache hit, after-cold-start hit (optional), then the in/out token totals.
 *
 *  Honesty rules (SDR-001 / US1.2):
 *  - `输入` is the prompt-side billing denominator (uncached input + cache
 *    read + cache write) — the same denominator the hit percentage uses.
 *  - A session with no billed input AND no output (e.g. every turn failed
 *    before any token was counted) renders NO groups — the whole bar
 *    disappears rather than showing a fake "Cache hit 0%".
 *  - The hit group is dropped when the percentage is null (no billed input),
 *    but the token group survives as long as anything was consumed.
 *  - The hit percentage never displays 100 for a partial hit (anti-100 is
 *    built into formatCacheHitPercent).
 *  - SDR-004: the after-cold-start hit subtracts the session's first LLM call
 *    (the cold-start miss) from both the numerator and the denominator. It is
 *    dropped when the first call is unknown, or when it was the only call
 *    (nothing left to divide — mirroring the primary hit's omission). */
export function buildStatsGroups(u: AppSessionUsage): string[] {
  const groups: string[] = [];
  const billed = billedInputTokens(u);
  const hit = formatCacheHitPercent(u.cacheReadTokens, billed);
  if (hit !== null) {
    groups.push(i18n.global.t('status.usageCacheHit', { pct: hit }));
  }
  // SDR-004: the after-cold-start hit subtracts the session's first LLM
  // call (the cold-start miss) from BOTH the numerator and the denominator.
  // Omitted when the first call is unknown (no baseline to subtract) or
  // when it was the only call (nothing left to divide — mirroring the
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
      groups.push(i18n.global.t('status.usageAfterColdStart', { pct: hitAfter }));
    }
  }
  if (billed > 0 || u.outputTokens > 0) {
    groups.push(
      i18n.global.t('status.usageTokens', {
        input: formatExactTokens(billed),
        output: formatExactTokens(u.outputTokens),
      }),
    );
  }
  return groups;
}
</script>

<script setup lang="ts">
import { computed } from 'vue';
import Tooltip from '../ui/Tooltip.vue';

const props = defineProps<{
  /** The active session's usage (session.usage from useWorkspaceState).
   *  Null/undefined (no session yet) renders nothing. */
  usage?: AppSessionUsage | null;
}>();

const groups = computed(() => (props.usage ? buildStatsGroups(props.usage) : []));

/** The bar's full text — shown both inline (truncated by CSS on narrow
 *  widths) and in the hover Tooltip so the truncated tail stays readable. */
const fullText = computed(() => groups.value.join(' · '));
</script>

<template>
  <Tooltip v-if="fullText" :text="fullText">
    <div class="usage-bar" :aria-label="fullText">
      <template v-for="(group, i) in groups" :key="i">
        <span v-if="i > 0" class="usage-sep">|</span>
        <span class="usage-group">{{ group }}</span>
      </template>
    </div>
  </Tooltip>
</template>

<style scoped>
/* Muted meta line, matching the dock's quiet chrome (e.g. the goal strip's
 * caption tone). Ellipsized on narrow widths; the Tooltip carries the full
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
  text-overflow: ellipsis;
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
