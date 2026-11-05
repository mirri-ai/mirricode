<!-- apps/mirri-web/src/components/settings/scheduledTasks/ScheduledTaskForm.vue -->
<!-- The task form (§5 表单基准 / A11): a single-column page shown IN the
     settings content area (no dialog, no drawer), used for both creation and
     editing (A9 — the edit mode prefills the same fields).

     Order top to bottom: the title (label above, full-width input) → the
     schedule (a one-line 「+ 添加日程」 placeholder until the user expresses
     intent, then the six presets and the rule editor; A8: picking 自定义
     opens its editor immediately) → the prediction timeline (A5/B1/B2:
     theoretical past → next → next-next, labelled as a prediction and
     carrying the timezone) → the instruction (a teaching placeholder, never
     empty copy). The four secondary choices sit on ONE compact config line
     with NO labels — workspace / permission mode / model / thinking effort —
     each showing its own value; cancel and submit form their own action row
     (SDR-115), which sticks to the bottom while the edit form scrolls
     (SDR-118). No cards, no dividers: spacing alone groups.

     A3: there is no working-directory input (the workspace answers "where").
     A4: the permission modes are the session's own three, verbatim.
     A9: the workspace is read-only in edit mode (ownership never moves). -->
<script setup lang="ts">
import { computed, ref, toRaw, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppCronPermissionMode, AppModel } from '../../../api/types';
import {
  cronDraftRecurring,
  localTimezone,
  type CronCreateDraft,
} from '../../../lib/cronManagement';
import {
  endsDateOf,
  isOneOffCandidate,
  scheduleComplete,
  type ScheduleKind,
} from '../../../lib/cronSchedule';
import {
  defaultThinkingLevelFor,
  effortLabel,
  modelThinkingAvailability,
  segmentsFor,
} from '../../../lib/modelThinking';
import Button from '../../ui/Button.vue';
import Input from '../../ui/Input.vue';
import Spinner from '../../ui/Spinner.vue';
import Textarea from '../../ui/Textarea.vue';
import SchedulePicker from './SchedulePicker.vue';
import ScheduleTimeline from './ScheduleTimeline.vue';

const props = defineProps<{
  /** The draft to edit: a fresh one for creation, `draftFromTask(…)` for an
   *  edit. The form owns a local copy — cancel discards it. */
  initialDraft: CronCreateDraft;
  /** 'create' renders the create CTA; 'edit' renders save + pause controls. */
  mode: 'create' | 'edit';
  /** The workspaces the task may belong to (A2: the workspace list, the
   *  current one preselected — never「已有任务的工作区」). */
  workspaces: Array<{ id: string; name: string }>;
  /** The catalog models (the model select's rows). */
  models: AppModel[];
  /** The in-flight submit ack. */
  submitting: boolean;
}>();

const emit = defineEmits<{
  submit: [draft: CronCreateDraft, previewT0: number | null];
  cancel: [];
}>();

const { t } = useI18n();

const draft = ref<CronCreateDraft>(
  // The parent hands the draft over through a `ref` — a deep reactive Proxy,
  // which `structuredClone` refuses to clone (DataCloneError → blank form).
  // `toRaw` unwraps the proxy layer back to the plain object beneath.
  structuredClone(toRaw(props.initialDraft)),
);

/** The visible timezone default: an explicit value, never a silent inherit. */
watch(
  () => draft.value.timezone,
  (zone) => {
    if (zone.trim() === '') draft.value.timezone = localTimezone();
  },
  { immediate: true },
);

// -------------------------------------------------------------------------
// The schedule + the prediction (A5 / B1 / B8)
// -------------------------------------------------------------------------
/** The preview the SchedulePicker already rendered (ONE RPC serves the
 *  display and the submit fold). */
const preview = ref<{ occurrences: number[]; previousT0: number | null } | null>(null);

/** The schedule's preset kind (raw/field/interval customs read 'custom') —
 *  the high-frequency rule weakens the past point (B1). */
const scheduleKind = computed<ScheduleKind | undefined>(() => draft.value.schedule?.kind);

const recurring = computed(() =>
  draft.value.schedule === null
    ? false
    : cronDraftRecurring(draft.value.schedule, preview.value?.occurrences[0] ?? null, draft.value.timezone),
);

const isOneOff = computed(() => {
  const schedule = draft.value.schedule;
  if (schedule === null || !isOneOffCandidate(schedule)) return false;
  const t0 = preview.value?.occurrences[0] ?? null;
  if (t0 === null) return false;
  return endsDateOf(schedule) === dayOfZone(t0, draft.value.timezone);
});

const endsBeforeFirstFire = computed(() => {
  const schedule = draft.value.schedule;
  const t0 = preview.value?.occurrences[0] ?? null;
  if (schedule === null || !isOneOffCandidate(schedule) || t0 === null) return false;
  const ends = endsDateOf(schedule);
  return ends !== '' && ends < dayOfZone(t0, draft.value.timezone);
});

const endsDateMissing = computed(() => {
  const schedule = draft.value.schedule;
  if (schedule === null || !isOneOffCandidate(schedule)) return false;
  return endsDateOf(schedule) === '';
});

/** SDR-114: an error appears only after the user has offended the field —
 *  left it on blur, or tried to submit. A pristine form shows zero errors,
 *  and a valid value clears its error the moment it is typed (the valid
 *  check stays live; only the invalid one waits for the offence). The limit
 *  treats '' as「不填 = 不限」, so clearing it after a blur is a LEGAL value,
 *  not an error — only a filled-but-illegal value is challenged. */
const promptTouched = ref(false);
const maxFiresTouched = ref(false);

function markPromptTouched(): void {
  promptTouched.value = true;
}

function markMaxFiresTouched(): void {
  maxFiresTouched.value = true;
}

const promptError = computed(() =>
  promptTouched.value && draft.value.prompt.trim() === ''
    ? t('settings.scheduledTaskPromptRequired')
    : null,
);

/** SDR-114 d: the limit's saved value — what the edit mode prefilled — is
 *  already invalid, so the offence predates this visit. The form says so up
 *  front instead of letting the user scroll to the save button blind. */
const maxFiresInheritedInvalid = computed(() => {
  const prefilled = props.initialDraft.maxFiresText.trim();
  return isMaxFiresInvalid(prefilled) && recurring.value;
});

const maxFiresError = computed(() => {
  if (!maxFiresTouched.value && !maxFiresInheritedInvalid.value) return null;
  return isMaxFiresInvalid(draft.value.maxFiresText.trim())
    ? t('settings.scheduledTaskMaxFiresInvalid')
    : null;
});

/** '' means「不填 = 不限」; only a filled value can be illegal. */
function isMaxFiresInvalid(text: string): boolean {
  if (text === '' || !recurring.value) return false;
  const n = Number.parseInt(text, 10);
  return !Number.isInteger(n) || n < 1;
}

/** SDR-114: the prompt is judged on submit, not on the button — an empty
 *  instruction must be CHALLENGED in place, never silently disabled. So the
 *  button follows only the structural completeness (a schedule, its ends
 *  date, a legal limit, a workspace). `cronDraftSubmittable` still answers
 *  the pure-function question (schedule + prompt) for its own consumers. */
const submittable = computed(
  () =>
    draft.value.schedule !== null &&
    scheduleComplete(draft.value.schedule) &&
    !endsBeforeFirstFire.value &&
    !endsDateMissing.value &&
    !isMaxFiresInvalid(draft.value.maxFiresText.trim()) &&
    draft.value.workspaceId !== '',
);

/** Day-of-month comparison in the task's zone (the one-off rule compares
 *  DAYS, SDR-110). */
function dayOfZone(epochMs: number, zone: string): string {
  return new Intl.DateTimeFormat('en-CA', {
    timeZone: zone.trim() || localTimezone(),
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
  }).format(new Date(epochMs));
}

// -------------------------------------------------------------------------
// The config line: workspace / permission / model / thinking (no labels)
// -------------------------------------------------------------------------
/** A4: the session's own three modes, the session's label keys — zero
 *  learning cost, no fourth option, no unattended concept. */
const PERMISSION_MODES: AppCronPermissionMode[] = ['manual', 'auto', 'yolo'];
const permissionLabelKey: Record<AppCronPermissionMode, string> = {
  manual: 'status.permissionManual',
  auto: 'status.permissionAuto',
  yolo: 'status.permissionYolo',
};

const workspaceName = computed(
  () => props.workspaces.find((w) => w.id === draft.value.workspaceId)?.name ?? draft.value.workspaceId,
);

/** The model the thinking control reads ('' = the workspace default row). */
const effectiveModelId = computed(() => draft.value.model.trim());
const currentModel = computed(() => props.models.find((m) => m.id === effectiveModelId.value));
const thinkingAvailability = computed(() => modelThinkingAvailability(currentModel.value));
const thinkingSegments = computed(() => segmentsFor(currentModel.value));
const thinkingReadonly = computed(
  () => thinkingAvailability.value === 'unsupported' || thinkingSegments.value.length <= 1,
);
const activeThinkingSegment = computed(() =>
  draft.value.effort === ''
    ? defaultThinkingLevelFor(currentModel.value)
    : thinkingSegments.value.includes(draft.value.effort)
      ? draft.value.effort
      : '',
);

function pickThinking(segment: string): void {
  if (thinkingReadonly.value) return;
  const level = segment === 'on' ? defaultThinkingLevelFor(currentModel.value) : segment;
  // A pick that equals the model's own default stays ABSENT on the wire
  // (the engine's absent-key default is the same value — keeping the
  // payload honest rather than spelling out a default).
  draft.value.effort = level === defaultThinkingLevelFor(currentModel.value) ? '' : level;
}

function onSubmit(): void {
  // SDR-114: the submit attempt is itself an offence — mark first, then
  // judge, so an empty instruction is named in place rather than swallowed
  // by a disabled button.
  markPromptTouched();
  if (recurring.value) markMaxFiresTouched();
  if (!submittable.value || promptError.value !== null) return;
  emit('submit', { ...draft.value }, preview.value?.occurrences[0] ?? null);
}
</script>

<template>
  <form class="stf" :class="{ 'is-edit': mode === 'edit' }" @submit.prevent="onSubmit()">
    <!-- §5: label above, full-width input below. -->
    <label class="stf-field">
      <span class="stf-label">{{ t('settings.scheduledTaskTitleFieldLabel') }}</span>
      <Input
        v-model="draft.title"
        class="stf-input"
        :placeholder="t('settings.scheduledTaskTitleFieldPlaceholder')"
      />
    </label>

    <!-- The schedule: a one-line placeholder until the user asks for it. -->
    <div class="stf-field">
      <span class="stf-label">{{ t('settings.scheduledTaskScheduleLabel') }}</span>
      <SchedulePicker
        v-model:schedule="draft.schedule"
        v-model:timezone="draft.timezone"
        @preview="preview = $event"
      />
      <div v-if="isOneOff" class="stf-hint stf-hint-accent">
        {{ t('settings.scheduledTaskOneOffSentence', { date: draft.schedule ? endsDateOf(draft.schedule) : '' }) }}
      </div>
      <div v-if="endsBeforeFirstFire" class="stf-error">
        {{ t('settings.scheduledTaskEndsBeforeFirstFire') }}
      </div>
      <div v-else-if="endsDateMissing" class="stf-error">
        {{ t('settings.scheduledTaskEndsDateMissing') }}
      </div>
      <label v-if="recurring" class="stf-inline-field">
        <span class="stf-label">{{ t('settings.scheduledTaskMaxFiresLabel') }}</span>
        <Input
          v-model="draft.maxFiresText"
          class="stf-narrow"
          type="number"
          min="1"
          step="1"
          :placeholder="t('settings.scheduledTaskMaxFiresPlaceholder')"
          @blur="markMaxFiresTouched"
        />
        <span v-if="maxFiresError" class="stf-error">{{ maxFiresError }}</span>
        <span v-if="maxFiresError && maxFiresInheritedInvalid" class="stf-error">
          {{ t('settings.scheduledTaskInheritedInvalidHint') }}
        </span>
      </label>
    </div>

    <!-- The prediction timeline (A5/B1/B2): always labelled as a prediction. -->
    <div class="stf-field stf-prediction">
      <span
        v-if="draft.schedule !== null && scheduleComplete(draft.schedule)"
        class="stf-prediction-caption"
      >{{ t('settings.scheduledTaskPredictionTitle') }}</span>
      <ScheduleTimeline
        v-if="draft.schedule !== null && scheduleComplete(draft.schedule)"
        :preview="preview"
        :timezone="draft.timezone"
        :hide-past="scheduleKind === 'hourly'"
      />
      <span v-else class="stf-hint">{{ t('settings.scheduledTaskPredictionEmpty') }}</span>
    </div>

    <!-- The instruction: the placeholder teaches by example. -->
    <label class="stf-field">
      <span class="stf-label">{{ t('settings.scheduledTaskInstructionsLabel') }}</span>
      <Textarea
        v-model="draft.prompt"
        :rows="5"
        :placeholder="t('settings.scheduledTaskInstructionsPlaceholder')"
        @blur="markPromptTouched"
      />
      <span v-if="promptError" class="stf-error">{{ promptError }}</span>
    </label>

    <!-- SDR-115: two rows, not one — the four unlabelled pickers keep their
         compact config line, and cancel / submit get their own row so the
         primary action is never wedged between pickers. §5: each control's
         current value says what it is, so there are still no labels. -->
    <div class="stf-config-row" role="group">
      <select
        v-if="mode === 'create'"
        v-model="draft.workspaceId"
        class="stf-chip"
        :aria-label="t('settings.scheduledTaskWorkspaceLabel')"
        :title="t('settings.scheduledTaskWorkspaceLabel')"
      >
        <option v-for="ws in workspaces" :key="ws.id" :value="ws.id">{{ ws.name }}</option>
      </select>
      <span
        v-else
        class="stf-chip stf-chip-static"
        :title="t('settings.scheduledTaskWorkspaceLocked')"
        data-testid="workspace-locked"
      >{{ workspaceName }}</span>

      <select
        v-model="draft.permissionMode"
        class="stf-chip"
        :aria-label="t('settings.scheduledTaskModeGroupLabel')"
        :title="t('settings.scheduledTaskModeGroupLabel')"
      >
        <option v-for="mode in PERMISSION_MODES" :key="mode" :value="mode">
          {{ t(permissionLabelKey[mode]) }}
        </option>
      </select>

      <select
        v-model="draft.model"
        class="stf-chip"
        :aria-label="t('settings.scheduledTaskModel')"
        :title="t('settings.scheduledTaskModel')"
      >
        <option value="">{{ t('settings.scheduledTaskModelDefault') }}</option>
        <option v-for="m in models" :key="m.id" :value="m.id">
          {{ m.displayName ?? m.model }}
        </option>
      </select>

      <select
        v-if="!thinkingReadonly"
        :value="activeThinkingSegment"
        class="stf-chip"
        :aria-label="'Thinking'"
        @change="pickThinking(($event.target as HTMLSelectElement).value)"
      >
        <option v-for="seg in thinkingSegments" :key="seg" :value="seg">
          {{ effortLabel(seg) }}
        </option>
      </select>
      <span v-else class="stf-chip stf-chip-static">—</span>
    </div>

    <!-- SDR-118: in edit mode this row sticks to the bottom of the scrolling
         form, so save stays reachable however far the user has scrolled.
         Create mode is a short form — a plain row is enough. -->
    <div class="stf-actions" :class="{ 'is-sticky': mode === 'edit' }">
      <Button variant="ghost" size="sm" type="button" @click="emit('cancel')">
        {{ t('settings.scheduledTaskFormCancel') }}
      </Button>
      <Button variant="primary" size="sm" type="submit" :disabled="submitting || !submittable">
        <Spinner v-if="submitting" size="sm" />
        {{
          submitting
            ? t('settings.scheduledTaskFormSubmitting')
            : mode === 'edit'
              ? t('settings.scheduledTaskFormSave')
              : t('settings.scheduledTaskFormSubmit')
        }}
      </Button>
    </div>
  </form>
</template>

<style scoped>
.stf {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
  padding: var(--space-6) 0;
  max-width: 720px;
}
/* SDR-118: in edit mode the action row is the form's last row and sticks to
   the viewport's bottom edge. Its own `padding` + border carry the visual
   gap, so the form's bottom padding would only push the sticky line below
   the reachable area — it is dropped for the edit mode alone. */
.stf.is-edit { padding-bottom: 0; }
/* A11/§5: label above, input below, full width — one vertical reading line. */
.stf-field { display: flex; flex-direction: column; gap: var(--space-2); min-width: 0; }
.stf-label {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text-muted);
}
.stf-input { width: 100%; }
.stf-narrow { max-width: 160px; }
.stf-inline-field { display: flex; flex-direction: column; gap: var(--space-1); margin-top: var(--space-2); }
.stf-hint { font-size: var(--text-xs); color: var(--color-text-faint); }
.stf-hint-accent { color: var(--color-accent); }
.stf-error { font-size: var(--text-xs); color: var(--color-danger); }
.stf-prediction { gap: var(--space-1); }
.stf-prediction-caption { font-size: var(--text-xs); color: var(--color-text-faint); }

/* SDR-115: the pickers' own line — four unlabelled chips shoulder to
   shoulder, wrapping when the width runs out. */
.stf-config-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
  padding-top: var(--space-2);
}
/* SDR-115/118: the actions form their own row; in edit mode the row sticks
   to the bottom of the scrolling viewport the moment any part of it would
   scroll away (`bottom: 0`), so cancel / save stay fully visible at every
   scroll position. */
.stf-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}
.stf-actions.is-sticky {
  position: sticky;
  bottom: 0;
  padding: var(--space-2) 0;
  background: var(--color-surface);
  border-top: 1px solid var(--color-line);
}
.stf-chip {
  appearance: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface);
  color: var(--color-text);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  padding: var(--space-1) var(--space-2);
  max-width: 180px;
  cursor: pointer;
}
.stf-chip:focus-visible { outline: none; box-shadow: var(--p-focus-ring); }
.stf-chip-static { color: var(--color-text-muted); cursor: default; }
</style>
