<!-- apps/mirri-web/src/components/settings/scheduledTasks/SchedulePicker.vue -->
<!-- The rule entry of the task form (A5 / A8 / §5 字段顺序): zero
     schedule knowledge assumed. With no rule set the component is ONE
     compact 「+ 添加日程」 row; clicking it reveals the six presets, and
     picking one replaces the whole value (the type system's structural
     reset — switching never leaks sub-options). A8: choosing 自定义 opens
     its editor on that same click — there is no intermediate button. The
     readable sentence renders live under the controls; the timezone is a
     visible, clickable prefix. The prediction timeline is the FORM's
     business (ScheduledTaskForm renders it from the emitted preview). -->
<script setup lang="ts">
import { computed, onBeforeUnmount, ref, toRaw, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { getMirriWebApi } from '../../../api';
import Button from '../../ui/Button.vue';
import Dialog from '../../ui/Dialog.vue';
import Icon from '../../ui/Icon.vue';
import IconButton from '../../ui/IconButton.vue';
import Input from '../../ui/Input.vue';
import Select from '../../ui/Select.vue';
import {
  compileSchedule,
  describeSchedule,
  newSchedule,
  scheduleComplete,
  type CronSchedule,
  type ScheduleEnds,
  type ScheduleKind,
} from '../../../lib/cronSchedule';
import type { AppCronPreview } from '../../../api/types';
import { localTimezone } from '../../../lib/cronManagement';

/** The rule value (null = the empty state) + the zone it evaluates in (a
 *  separate model — the zone is the presentation and evaluation basis, not
 *  part of the rule's structure). */
const schedule = defineModel<CronSchedule | null>('schedule', { default: null });
const timezone = defineModel<string>('timezone', { default: '' });

const emit = defineEmits<{
  /** The engine's preview of the current rule (the form renders the timeline
   *  and folds the one-off derivation from it — ONE RPC, one truth). */
  preview: [preview: AppCronPreview | null];
}>();

const { t } = useI18n();
const api = getMirriWebApi();

const PRESETS: ScheduleKind[] = ['hourly', 'daily', 'weekday', 'weekly', 'monthly', 'custom'];

// An empty zone falls back to the visible local default rather than an
// ambiguous blank (both the preview RPC and the sentence need a zone).
watch(
  () => timezone.value,
  (zone) => {
    if (zone.trim() === '') timezone.value = localTimezone();
  },
  { immediate: true },
);

const effectiveTimezone = computed(() =>
  timezone.value.trim() === '' ? localTimezone() : timezone.value.trim(),
);

/** Whether the preset row is expanded (§5: collapsed to one placeholder row
 *  until the user expresses intent; an already-set rule opens expanded). */
const expanded = ref(schedule.value !== null);
const customOpen = ref(false);

function pickPreset(kind: ScheduleKind): void {
  expanded.value = true;
  // The structural reset: a preset pick replaces the WHOLE value.
  schedule.value = newSchedule(kind);
  if (kind === 'custom') openCustomDialog();
}

function clearSchedule(): void {
  schedule.value = null;
  expanded.value = false;
}

const presetLabel = (kind: ScheduleKind) => t(`settings.cronRulePreset_${kind}`);

// --- per-preset sub-option editing (whole-value replacement) --------------

function patchSchedule(patch: Record<string, unknown>): void {
  if (schedule.value === null) return;
  schedule.value = { ...schedule.value, ...patch } as CronSchedule;
}

const minuteOptions = Array.from({ length: 60 }, (_, i) => i);
const hourOptions = Array.from({ length: 24 }, (_, i) => i);
const dayOfMonthOptions = Array.from({ length: 31 }, (_, i) => i + 1);
const weekdays = [1, 2, 3, 4, 5, 6, 7] as const;

/** HH:MM options for the time selects — every minute of the hour (SDR-117:
 *  a 15-minute grid silently forbade 09:07 on a 每天 schedule).
 *
 *  SDR-117 follow-up: the hour and the minute are TWO selects, not one
 *  1440-row list — a native dropdown that long is a scroll obligation, and
 *  the two segments say exactly what a wall-clock time is made of. The pair
 *  writes back as one `HH:MM` string through `patchTime`. */
const hourTiles = hourOptions.map((h) => String(h).padStart(2, '0'));
const minuteTiles = minuteOptions.map((m) => String(m).padStart(2, '0'));

function timePartsOf(time: string): { hour: string; minute: string } {
  const [hour = '', minute = ''] = time.split(':');
  return { hour, minute };
}

/** Replace one segment of the schedule's time, keeping the other. */
function patchTimeSegment(time: string, segment: 'hour' | 'minute', value: string): void {
  const next = { ...timePartsOf(time), [segment]: value };
  patchSchedule({ time: `${next.hour}:${next.minute}` });
}

function toggleWeekday(day: number): void {
  const current = schedule.value;
  if (current === null || current.kind !== 'weekly') return;
  const days = current.days.includes(day)
    ? current.days.filter((d) => d !== day)
    : [...current.days, day].toSorted((a, b) => a - b);
  patchSchedule({ days });
}

// --- the custom rule editor (A8: opens the moment 自定义 is picked) -------

type CustomSchedule = Extract<CronSchedule, { kind: 'custom' }>;

/** The editor's scratch copy — cancel discards, confirm writes back (the
 *  in-dialog edits never touch the model until confirmed). */
const customDraft = ref<CustomSchedule | null>(null);
/** The picks text mirror — a comma list parsed into `field.picks`. */
const customPicksText = ref('');
/** The raw-expression text mirror (field mode's Advanced fold). */
const customRawText = ref('');

/** The per-unit inclusive pick bounds (minute 0 / hour 0 are VALID picks). */
const PICK_BOUNDS: Record<CustomScheduleFieldUnit, { min: number; max: number }> = {
  'minute-of-hour': { min: 0, max: 59 },
  'hour-of-day': { min: 0, max: 23 },
  'day-of-month': { min: 1, max: 31 },
  'day-of-week': { min: 1, max: 7 },
};
type CustomScheduleFieldUnit = NonNullable<CustomSchedule['field']>['unit'];

function openCustomDialog(): void {
  const current = schedule.value;
  if (current === null || current.kind !== 'custom') return;
  // The model reads the parent's reactive draft (a Proxy that structuredClone
  // refuses to clone) — unwrap to the plain object before copying it. Only
  // the top layer needs unwrapping: the write-back in onCustomConfirm now
  // deep-copies, so the model never holds nested proxies.
  customDraft.value = structuredClone(toRaw(current));
  customPicksText.value = (current.field?.picks ?? []).join(',');
  customRawText.value = current.raw?.expr ?? '';
  customOpen.value = true;
}

function onCustomConfirm(): void {
  const draft = customDraft.value;
  if (draft === null) return;
  // A deep copy of the reactive scratch draft: the shallow spread keeps the
  // nested `ends` / `raw` / `field` objects as reactive proxies, and the model
  // must never hold nested proxies (a later reopen re-clones the value with
  // `structuredClone`, which cannot clone a proxy).
  const value: CustomSchedule = structuredClone(toRaw(draft));
  // field mode + a non-empty Advanced expression means the user opted for the
  // expression over the structured options — confirm switches to raw.
  if (value.mode === 'field' && customRawText.value.trim() !== '') {
    value.mode = 'raw';
    value.raw = { expr: customRawText.value.trim() };
  }
  if (value.mode === 'field') {
    const field = value.field ?? { unit: 'day-of-week' as const, picks: [1], time: '09:00' };
    const bounds = PICK_BOUNDS[field.unit];
    const picks = [
      ...new Set(
        customPicksText.value
          .split(',')
          .map((s) => Number(s.trim()))
          .filter((n) => Number.isInteger(n) && n >= bounds.min && n <= bounds.max),
      ),
    ].toSorted((a, b) => a - b);
    value.field = { ...field, picks };
  }
  if (value.mode === 'raw' && value.raw === undefined && customRawText.value.trim() !== '') {
    value.raw = { expr: customRawText.value.trim() };
  }
  schedule.value = value;
  customOpen.value = false;
}

function setCustomMode(mode: 'interval' | 'field' | 'raw'): void {
  const draft = customDraft.value;
  if (draft === null) return;
  customDraft.value = {
    ...draft,
    mode,
    interval: draft.interval ?? { every: 1, unit: 'day', time: '09:00' },
    field: draft.field ?? { unit: 'day-of-week', picks: [1], time: '09:00' },
    raw: draft.raw ?? { expr: '' },
  };
}

function setCustomEnds(kind: 'never' | 'on'): void {
  const draft = customDraft.value;
  if (draft === null) return;
  const ends: ScheduleEnds =
    kind === 'never'
      ? { kind: 'never' }
      : { kind: 'on', date: draft.ends.kind === 'on' ? draft.ends.date : '' };
  customDraft.value = { ...draft, ends };
}

// --- the live preview (A5 / ADR-017: the engine evaluates, never the UI) ---

let previewSeq = 0;
let previewTimer: ReturnType<typeof setTimeout> | null = null;

/** The compiled shape only when structurally complete — an incomplete rule
 *  renders 「—」 honestly, never an error dialog. */
const compiled = computed(() =>
  schedule.value !== null && scheduleComplete(schedule.value)
    ? compileSchedule(schedule.value, effectiveTimezone.value)
    : null,
);

const description = computed(() =>
  schedule.value === null ? '' : describeSchedule(schedule.value, effectiveTimezone.value, t),
);

watch(
  [compiled, effectiveTimezone],
  () => {
    if (previewTimer !== null) clearTimeout(previewTimer);
    // Debounce ~300ms: 改完即见 without hammering the RPC on every keystroke.
    previewTimer = setTimeout(() => void runPreview(), 300);
  },
  { immediate: true },
);

async function runPreview(): Promise<void> {
  const shape = compiled.value;
  const seq = ++previewSeq;
  if (shape === null) {
    emit('preview', null);
    return;
  }
  try {
    const answer = await api.previewCronSchedule(
      shape.type === 'cron' ? { cron: shape.cron } : { interval: shape.interval },
      { timezone: effectiveTimezone.value, count: 2 },
    );
    // Drop stale answers — only the latest request may render.
    if (seq !== previewSeq) return;
    emit('preview', answer);
  } catch {
    // A failed preview is a display degradation, never a form blocker (the
    // engine's create/update validation is authoritative).
    if (seq !== previewSeq) return;
    emit('preview', null);
  }
}

// A pending debounce firing after unmount would call into a dead RPC.
onBeforeUnmount(() => {
  if (previewTimer !== null) {
    clearTimeout(previewTimer);
    previewTimer = null;
  }
  previewSeq++;
});

// --- the timezone editor (click the visible prefix to change) -------------

const timezoneEditing = ref(false);
/** A compact common-zone list; the free-text input covers the rest. */
const COMMON_TIMEZONES = [
  'UTC',
  'Asia/Shanghai',
  'Asia/Tokyo',
  'Asia/Singapore',
  'Europe/London',
  'Europe/Berlin',
  'Europe/Paris',
  'America/New_York',
  'America/Chicago',
  'America/Los_Angeles',
  'Australia/Sydney',
];

function setTimezone(zone: string): void {
  timezone.value = zone;
}
</script>

<template>
  <div class="rule-picker">
    <!-- §5: the collapsed placeholder row, one line. -->
    <button
      v-if="!expanded && schedule === null"
      type="button"
      class="rule-add"
      @click="expanded = true"
    >
      <Icon name="plus" size="sm" />
      <span>{{ t('settings.scheduledTaskAddSchedule') }}</span>
    </button>

    <template v-else>
      <!-- The six presets: always visible once the user asked for a rule
           (A8: 自定义 opens its editor on this very click). -->
      <div class="rule-presets" role="radiogroup">
        <button
          v-for="preset in PRESETS"
          :key="preset"
          type="button"
          role="radio"
          class="rule-preset"
          :class="{ 'is-on': schedule?.kind === preset }"
          :aria-checked="schedule?.kind === preset"
          :data-preset="preset"
          @click="pickPreset(preset)"
        >
          {{ presetLabel(preset) }}
        </button>
      </div>

      <div v-if="schedule !== null" class="rule-controls">
        <!-- hourly: the minute picker -->
        <template v-if="schedule.kind === 'hourly'">
          <span class="rule-inline-label">{{ t('settings.cronRuleAtMinute') }}</span>
          <Select
            :model-value="String(schedule.minute)"
            size="sm"
            class="rule-narrow"
            @update:model-value="patchSchedule({ minute: Number($event) })"
          >
            <option v-for="m in minuteOptions" :key="m" :value="String(m)">
              :{{ String(m).padStart(2, '0') }}
            </option>
          </Select>
        </template>

        <!-- daily / weekday: the time picker (hour + minute, two segments) -->
        <template v-else-if="schedule.kind === 'daily' || schedule.kind === 'weekday'">
          <span class="rule-inline-label">{{ t('settings.cronRuleAtTime') }}</span>
          <Select
            :model-value="timePartsOf(schedule.time).hour"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtHour')"
            @update:model-value="patchTimeSegment(schedule.time, 'hour', $event)"
          >
            <option v-for="h in hourTiles" :key="h" :value="h">{{ h }}</option>
          </Select>
          <span class="rule-time-sep">:</span>
          <Select
            :model-value="timePartsOf(schedule.time).minute"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtMinute')"
            @update:model-value="patchTimeSegment(schedule.time, 'minute', $event)"
          >
            <option v-for="m in minuteTiles" :key="m" :value="m">{{ m }}</option>
          </Select>
        </template>

        <!-- weekly: weekday multi-select + the time picker -->
        <template v-else-if="schedule.kind === 'weekly'">
          <div class="rule-weekdays" role="group">
            <button
              v-for="day in weekdays"
              :key="day"
              type="button"
              class="rule-weekday"
              :class="{ active: schedule.days.includes(day) }"
              :aria-pressed="schedule.days.includes(day)"
              @click="toggleWeekday(day)"
            >
              {{ t(`settings.cronRuleWeekdayShort_${day}`) }}
            </button>
          </div>
          <span class="rule-inline-label">{{ t('settings.cronRuleAtTime') }}</span>
          <Select
            :model-value="timePartsOf(schedule.time).hour"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtHour')"
            @update:model-value="patchTimeSegment(schedule.time, 'hour', $event)"
          >
            <option v-for="h in hourTiles" :key="h" :value="h">{{ h }}</option>
          </Select>
          <span class="rule-time-sep">:</span>
          <Select
            :model-value="timePartsOf(schedule.time).minute"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtMinute')"
            @update:model-value="patchTimeSegment(schedule.time, 'minute', $event)"
          >
            <option v-for="m in minuteTiles" :key="m" :value="m">{{ m }}</option>
          </Select>
        </template>

        <!-- monthly: day-of-month + the time picker -->
        <template v-else-if="schedule.kind === 'monthly'">
          <span class="rule-inline-label">{{ t('settings.cronRuleOnDay') }}</span>
          <Select
            :model-value="String(schedule.day)"
            size="sm"
            class="rule-narrow"
            @update:model-value="patchSchedule({ day: Number($event) })"
          >
            <option v-for="d in dayOfMonthOptions" :key="d" :value="String(d)">{{ d }}</option>
          </Select>
          <span class="rule-inline-label">{{ t('settings.cronRuleAtTime') }}</span>
          <Select
            :model-value="timePartsOf(schedule.time).hour"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtHour')"
            @update:model-value="patchTimeSegment(schedule.time, 'hour', $event)"
          >
            <option v-for="h in hourTiles" :key="h" :value="h">{{ h }}</option>
          </Select>
          <span class="rule-time-sep">:</span>
          <Select
            :model-value="timePartsOf(schedule.time).minute"
            size="sm"
            class="rule-narrow"
            :aria-label="t('settings.cronRuleAtMinute')"
            @update:model-value="patchTimeSegment(schedule.time, 'minute', $event)"
          >
            <option v-for="m in minuteTiles" :key="m" :value="m">{{ m }}</option>
          </Select>
        </template>

        <!-- custom: reopen the editor (also opened right away on pick). -->
        <template v-else>
          <Button variant="secondary" size="sm" @click="openCustomDialog">
            {{ t('settings.cronRuleCustomTitle') }}
          </Button>
        </template>

        <IconButton
          class="rule-clear"
          size="sm"
          :label="t('settings.scheduledTaskRemoveSchedule')"
          @click="clearSchedule"
        >
          <Icon name="close" size="sm" />
        </IconButton>
      </div>

      <!-- The readable sentence (所选 → 提交值 → 描述 one source) with the
           clickable zone prefix. -->
      <div v-if="schedule !== null" class="rule-description">
        <span
          class="rule-tz"
          :title="t('settings.cronRuleTimezoneClickHint')"
          tabindex="0"
          role="button"
          @click="timezoneEditing = !timezoneEditing"
          @keydown.enter.prevent="timezoneEditing = !timezoneEditing"
        >{{ effectiveTimezone }}</span>
        <span>{{ description }}</span>
      </div>
      <div v-if="timezoneEditing" class="rule-tz-editor">
        <button
          v-for="zone in COMMON_TIMEZONES"
          :key="zone"
          type="button"
          class="rule-tz-option"
          :class="{ active: zone === effectiveTimezone }"
          @click="setTimezone(zone)"
        >
          {{ zone }}
        </button>
        <Input
          :model-value="effectiveTimezone"
          size="sm"
          class="rule-tz-input"
          @update:model-value="setTimezone"
        />
      </div>
    </template>

    <!-- The custom rule editor. -->
    <Dialog
      :open="customOpen"
      :title="t('settings.cronRuleCustomTitle')"
      :description="t('settings.cronRuleCustomDesc')"
      @update:open="(open) => { if (!open) customOpen = false; }"
    >
      <div v-if="customDraft" class="rc">
        <!-- Mode: interval (elapsed) / field (expression-expressible) / raw. -->
        <div class="rc-row" role="radiogroup">
          <label class="rc-mode" :class="{ active: customDraft.mode === 'interval' }">
            <input
              :checked="customDraft.mode === 'interval'"
              type="radio"
              value="interval"
              class="visually-hidden"
              @change="setCustomMode('interval')"
            />
            <span>{{ t('settings.cronRuleCustomModeInterval') }}</span>
          </label>
          <label class="rc-mode" :class="{ active: customDraft.mode === 'field' }">
            <input
              :checked="customDraft.mode === 'field'"
              type="radio"
              value="field"
              class="visually-hidden"
              @change="setCustomMode('field')"
            />
            <span>{{ t('settings.cronRuleCustomModeField') }}</span>
          </label>
          <label
            v-if="customDraft.mode !== 'interval'"
            class="rc-mode"
            :class="{ active: customDraft.mode === 'raw' }"
          >
            <input
              :checked="customDraft.mode === 'raw'"
              type="radio"
              value="raw"
              class="visually-hidden"
              @change="setCustomMode('raw')"
            />
            <span>{{ t('settings.cronRuleCustomModeRaw') }}</span>
          </label>
        </div>

        <!-- interval mode: every N + unit + the daily time. -->
        <div v-if="customDraft.mode === 'interval' && customDraft.interval" class="rc-row">
          <span class="rule-inline-label">{{ t('settings.cronRuleEvery') }}</span>
          <Input
            :model-value="customDraft.interval.every"
            size="sm"
            type="number"
            min="1"
            step="1"
            class="rc-num"
            @update:model-value="customDraft.interval.every = Number($event)"
          />
          <Select
            :model-value="customDraft.interval.unit"
            size="sm"
            class="rule-narrow"
            @update:model-value="customDraft.interval.unit = $event as 'day' | 'week' | 'month'"
          >
            <option value="day">{{ t('settings.cronRuleIntervalUnit_day') }}</option>
            <option value="week">{{ t('settings.cronRuleIntervalUnit_week') }}</option>
            <option value="month">{{ t('settings.cronRuleIntervalUnit_month') }}</option>
          </Select>
          <span class="rule-inline-label">{{ t('settings.cronRuleAtTime') }}</span>
          <input v-model="customDraft.interval.time" class="rc-time" type="time" />
        </div>

        <!-- field mode: the unit + picks + time. -->
        <template v-else-if="customDraft.mode === 'field' && customDraft.field">
          <div class="rc-row">
            <Select
              :model-value="customDraft.field.unit"
              size="sm"
              class="rc-unit"
              @update:model-value="customDraft.field.unit = $event as typeof customDraft.field.unit"
            >
              <option value="day-of-week">{{ t('settings.cronRuleFieldUnit_day-of-week') }}</option>
              <option value="day-of-month">{{ t('settings.cronRuleFieldUnit_day-of-month') }}</option>
              <option value="hour-of-day">{{ t('settings.cronRuleFieldUnit_hour-of-day') }}</option>
              <option value="minute-of-hour">{{ t('settings.cronRuleFieldUnit_minute-of-hour') }}</option>
            </Select>
            <Input
              v-model="customPicksText"
              size="sm"
              class="rc-picks"
              :placeholder="t('settings.cronRuleFieldPicksPlaceholder')"
            />
            <template
              v-if="customDraft.field.unit === 'day-of-week' || customDraft.field.unit === 'day-of-month'"
            >
              <span class="rule-inline-label">{{ t('settings.cronRuleAtTime') }}</span>
              <input v-model="customDraft.field.time" class="rc-time" type="time" />
            </template>
          </div>
          <!-- Advanced fold: the raw expression (field mode only). -->
          <details class="rc-advanced">
            <summary>{{ t('settings.cronRuleCustomModeRaw') }}</summary>
            <Input v-model="customRawText" size="sm" class="rc-raw" :placeholder="'*/5 * * * *'" />
            <div class="rc-note">{{ t('settings.cronRuleAdvancedNote') }}</div>
            <div class="rc-note">{{ t('settings.cronRuleAdvancedExample') }}</div>
          </details>
        </template>

        <!-- raw mode: the expression IS the rule (the advanced path — the
             technical spelling appears only here, with examples beside it). -->
        <div v-else-if="customDraft.mode === 'raw'" class="rc-row rc-raw-field">
          <Input v-model="customRawText" size="sm" class="rc-raw" :placeholder="'*/5 * * * *'" />
          <div class="rc-note">{{ t('settings.cronRuleAdvancedExample') }}</div>
        </div>

        <!-- interval mode's honest boundary note (SDR-113). -->
        <div v-if="customDraft.mode === 'interval'" class="rc-note">
          {{ t('settings.cronRuleIntervalNoRawNote') }}
        </div>

        <!-- Ends (SDR-110): never (default) / on an inclusive date. -->
        <div class="rc-row rc-ends" role="radiogroup">
          <label class="rc-mode" :class="{ active: customDraft.ends.kind === 'never' }">
            <input
              :checked="customDraft.ends.kind === 'never'"
              type="radio"
              value="never"
              class="visually-hidden"
              @change="setCustomEnds('never')"
            />
            <span>{{ t('settings.cronRuleEndsNever') }}</span>
          </label>
          <label class="rc-mode" :class="{ active: customDraft.ends.kind === 'on' }">
            <input
              :checked="customDraft.ends.kind === 'on'"
              type="radio"
              value="on"
              class="visually-hidden"
              @change="setCustomEnds('on')"
            />
            <span>{{ t('settings.cronRuleEndsOn') }}</span>
          </label>
          <input
            v-if="customDraft.ends.kind === 'on'"
            :value="customDraft.ends.date"
            class="rc-time"
            type="date"
            @input="customDraft.ends = { kind: 'on', date: ($event.target as HTMLInputElement).value }"
          />
        </div>
        <div v-if="customDraft.ends.kind === 'on'" class="rc-note">
          {{ t('settings.cronRuleEndsInclusiveNote') }}
        </div>
      </div>
      <template #foot>
        <Button variant="secondary" size="sm" @click="customOpen = false">
          {{ t('common.cancel') }}
        </Button>
        <Button variant="primary" size="sm" @click="onCustomConfirm">
          {{ t('common.confirm') }}
        </Button>
      </template>
    </Dialog>
  </div>
</template>

<style scoped>
.rule-picker { display: flex; flex-direction: column; gap: var(--space-2); }
/* §5: one compact placeholder row, dashed — no card, no icon decoration. */
.rule-add {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  width: fit-content;
  padding: var(--space-2) var(--space-3);
  border: 1px dashed var(--color-line-strong);
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  cursor: pointer;
}
.rule-add:hover { color: var(--color-text); border-color: var(--color-text-faint); }
.rule-add:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.rule-presets { display: flex; flex-wrap: wrap; gap: var(--space-1); }
.rule-preset {
  border: 1px solid var(--color-line);
  background: transparent;
  border-radius: var(--radius-md);
  padding: var(--space-1) var(--space-3);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text-muted);
  cursor: pointer;
}
.rule-preset:hover { color: var(--color-text); border-color: var(--color-line-strong); }
.rule-preset.is-on { border-color: var(--color-accent); color: var(--color-accent); }
.rule-preset:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.rule-controls { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-2); }
.rule-narrow { width: auto; min-width: 90px; }
/* The ':' between the hour and the minute segments — they read as one time. */
.rule-time-sep { font-family: var(--font-mono); color: var(--color-text-muted); }
.rule-inline-label { font-size: var(--text-sm); color: var(--color-text-muted); }
.rule-weekdays { display: inline-flex; gap: 2px; }
.rule-weekday {
  border: 1px solid var(--color-line-strong);
  background: var(--color-surface-raised);
  border-radius: var(--radius-sm);
  padding: 2px 6px;
  font-size: var(--text-xs);
  cursor: pointer;
  color: var(--color-text-muted);
}
.rule-weekday.active { border-color: var(--color-accent); color: var(--color-accent); }
.rule-description { display: flex; gap: var(--space-2); align-items: baseline; font-size: var(--text-sm); }
.rule-tz {
  font-family: var(--font-mono);
  color: var(--color-accent);
  cursor: pointer;
  border-bottom: 1px dotted var(--color-accent);
}
.rule-tz-editor { display: flex; flex-wrap: wrap; gap: var(--space-1); align-items: center; }
.rule-tz-option {
  border: 1px solid var(--color-line-strong);
  background: var(--color-surface-raised);
  border-radius: var(--radius-sm);
  padding: 2px 6px;
  font-size: var(--text-xs);
  font-family: var(--font-mono);
  cursor: pointer;
  color: var(--color-text);
}
.rule-tz-option.active { border-color: var(--color-accent); color: var(--color-accent); }
.rule-tz-input { width: 200px; }

/* --- the custom rule editor body --- */
.rc { display: flex; flex-direction: column; gap: var(--space-3); }
.rc-row { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-2); }
.rc-raw-field { flex-direction: column; align-items: stretch; }
.rc-mode {
  display: inline-flex;
  align-items: center;
  border: 1px solid var(--color-line-strong);
  border-radius: var(--radius-sm);
  padding: var(--space-1) var(--space-2);
  font-size: var(--text-sm);
  cursor: pointer;
  color: var(--color-text);
}
.rc-mode.active { border-color: var(--color-accent); color: var(--color-accent); }
.rc-num { width: 80px; }
.rc-picks { width: 130px; }
.rc-unit { width: auto; min-width: 160px; }
.rc-time {
  height: 32px;
  border: 1px solid var(--color-line-strong);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
  color: var(--color-text);
  padding: 0 var(--space-2);
  font-size: var(--text-sm);
}
.rc-raw { font-family: var(--font-mono); }
.rc-advanced { font-size: var(--text-sm); }
.rc-advanced summary { cursor: pointer; color: var(--color-text-muted); }
.rc-note { font-size: var(--text-xs); color: var(--color-text-muted); }
.rc-ends { margin-top: var(--space-1); }
.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  margin: -1px;
  padding: 0;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
  border: 0;
}
</style>
