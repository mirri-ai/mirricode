<!-- apps/mirri-web/src/components/settings/scheduledTasks/ScheduleTimeline.vue -->
<!-- The prediction timeline (A5 / B1 / B2): 理论上次 → 下次 → 下下次 on one
     axis. Every point is a PREDICTION computed by the engine from the
     current rule — the card and the form both label it as such, and the past
     point is de-emphasised and marked 理论值 so it can never be mistaken for
     a run record (facts live in the history tab only). Each point carries the
     timezone offset it was rendered in, computed with an explicit zone so
     the label and the instant always agree. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronPreview } from '../../../api/types';
import {
  predictionTimeline,
  rulePastInstant,
  zoneOffsetLabel,
} from '../../../lib/cronManagement';

const props = withDefaults(
  defineProps<{
    /** The engine's preview answer (null while absent / failed — every point
     *  degrades to 「—」 honestly). */
    preview: AppCronPreview | null;
    /** The zone the rule is evaluated in ('' = the host's local zone). */
    timezone: string;
    /** B1: high-frequency rules drop the past point (no decision value). */
    hidePast?: boolean;
    /** The compact layout (one line) for a caller that has no room for the
     *  full axis. The caption is the caller's own (the form prints it). */
    compact?: boolean;
  }>(),
  { hidePast: false, compact: false },
);

const { t, locale } = useI18n();

const timeline = computed(() =>
  predictionTimeline(props.preview, {
    timezone: props.timezone,
    showPast: !props.hidePast,
  }),
);

const pastAt = computed(() => rulePastInstant(props.preview ?? undefined));

const effectiveZone = computed(() =>
  props.timezone.trim() === '' ? Intl.DateTimeFormat().resolvedOptions().timeZone : props.timezone.trim(),
);

interface RenderedPoint {
  kind: 'past' | 'next' | 'nextNext';
  label: string;
  /** The zone-local time text, or the dash. */
  value: string;
  /** The offset label riding beside the value (B2); '' when there is no
   *  value to label. */
  offset: string;
}

const points = computed<RenderedPoint[]>(() =>
  timeline.value.points.map((point) => {
    const label =
      point.kind === 'past'
        ? t('settings.scheduledTaskPredictionPast')
        : point.kind === 'next'
          ? t('settings.scheduledTaskPredictionNext')
          : t('settings.scheduledTaskPredictionNextNext');
    if (point.at === null) {
      return { kind: point.kind, label, value: t('settings.scheduledTaskPredictionDash'), offset: '' };
    }
    return {
      kind: point.kind,
      label,
      value: new Intl.DateTimeFormat(locale.value, {
        timeZone: effectiveZone.value,
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
      }).format(new Date(point.at)),
      offset: zoneOffsetLabel(point.at, props.timezone),
    };
  }),
);
</script>

<template>
  <div class="prediction" :class="{ compact }" data-testid="schedule-timeline">
    <ol class="prediction-axis">
      <li
        v-for="(point, index) in points"
        :key="point.kind"
        class="prediction-point"
        :class="`is-${point.kind}`"
        :data-kind="point.kind"
      >
        <span v-if="index > 0" class="prediction-sep" aria-hidden="true">→</span>
        <span class="prediction-label">{{ point.label }}</span>
        <span class="prediction-value">{{ point.value }}</span>
        <span v-if="point.offset" class="prediction-zone">{{ point.offset }}</span>
        <span
          v-if="point.kind === 'past' && pastAt !== null"
          class="prediction-tag"
        >{{ t('settings.scheduledTaskPredictionPastValue') }}</span>
      </li>
    </ol>
  </div>
</template>

<style scoped>
.prediction { display: flex; flex-direction: column; gap: var(--space-2); }
.prediction-axis {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--space-2);
  margin: 0;
  padding: 0;
  list-style: none;
}
.prediction-point {
  display: inline-flex;
  align-items: baseline;
  gap: 6px;
  font-size: var(--text-sm);
  color: var(--color-text);
}
/* B1: the theoretical past is de-emphasised — it is a prediction, not a run. */
.prediction-point.is-past { color: var(--color-text-muted); }
.prediction-point.is-past .prediction-value { text-decoration: underline dotted var(--color-text-faint); }
.prediction-point.is-nextNext { color: var(--color-text-muted); }
.prediction-sep { color: var(--color-text-faint); }
.prediction-label { font-size: var(--text-xs); color: var(--color-text-faint); }
.prediction-value { font-variant-numeric: tabular-nums; }
.prediction-zone { font-size: var(--text-xs); color: var(--color-text-faint); font-family: var(--font-mono); }
.prediction-tag {
  font-size: var(--text-xs);
  color: var(--color-text-faint);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-sm);
  padding: 0 4px;
}
.prediction.compact .prediction-point { font-size: var(--text-xs); }
</style>
