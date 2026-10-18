<!-- apps/mirri-web/src/components/settings/AgentProfilesPanel.vue -->
<!-- Master-detail panel for managing agent profiles. Renders inside the
     Settings dialog's "profiles" tab — no standalone modal. -->
<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type {
  AppAgentProfile,
  AppAgentProfileInput,
  AppAgentProfileList,
  AppAgentProfileUpdate,
  AppAgentSkippedFile,
  AppModel,
  AppToolDescriptor,
  AppMcpServer,
} from '../../api/types';
import { getMirriWebApi } from '../../api';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Checkbox from '../ui/Checkbox.vue';
import Spinner from '../ui/Spinner.vue';
import Field from '../ui/Field.vue';
import Input from '../ui/Input.vue';
import Select from '../ui/Select.vue';
import Switch from '../ui/Switch.vue';
import Tooltip from '../ui/Tooltip.vue';
import Icon from '../ui/Icon.vue';
import ToolTagEditor from './ToolTagEditor.vue';
import Markdown from '../chat/Markdown.vue';
import { capabilityDescKey } from '../../lib/agentCapabilities';
import {
  applyFileValue,
  buildConflictRows,
  buildCreateProfilePayload,
  buildUpdateProfilePayload,
  canResetToDefault,
  canToggleProfile,
  editingKind,
  emptyFormValues,
  enabledSubagentOptions,
  formValuesFromProfile,
  overrideBadgeKind,
  shouldInterceptSave,
  validateAgentProfileForm,
  type AgentProfileFormError,
  type ConflictField,
  type ConflictRow,
  type ProfileFormValues,
} from '../../lib/agentProfiles';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t, te } = useI18n();
const { confirm } = useConfirmDialog();

const props = defineProps<{
  profiles: AppAgentProfile[];
  loading?: boolean;
  models?: AppModel[];
  /** Agent files skipped during discovery (parse failures) — US-A1-3. */
  skippedFiles?: AppAgentSkippedFile[];
}>();

const emit = defineEmits<{
  create: [input: AppAgentProfileInput];
  update: [name: string, data: AppAgentProfileUpdate];
  delete: [name: string];
  enable: [name: string];
  disable: [name: string];
  reset: [name: string];
  /** Polled silent refresh — the panel already fetched the data; the parent applies it. */
  refresh: [result: AppAgentProfileList];
}>();

// -------------------------------------------------------------------------
// Selection state — which profile is shown in the detail pane
// -------------------------------------------------------------------------
const selectedName = ref<string | null>(null);
const isCreating = ref(false);

const selectedProfile = computed<AppAgentProfile | undefined>(() => {
  if (selectedName.value === null) return undefined;
  return props.profiles.find((p) => p.name === selectedName.value);
});

const showForm = computed(() => isCreating.value || selectedProfile.value !== undefined);

// -------------------------------------------------------------------------
// Form state
// -------------------------------------------------------------------------
// "Start from" selector — only used during create. When the user picks a base
// profile, all form fields are prefilled from it (name stays blank). The
// resulting create payload is self-contained (no `extends`).
const startFrom = ref('');

const form = reactive<ProfileFormValues>({
  name: '',
  description: '',
  defaultModel: '',
  tools: '',
  whenToUse: '',
  systemPromptTemplate: '',
  subagents: [],
  disallowedTools: [],
});
const formError = ref('');
/** True once the user has touched any form field since the last populate/reset. */
const formDirty = ref(false);
let suppressDirtyMarker = false;

// Any form mutation marks the form dirty — drives the polling conflict guard.
watch(form, () => {
  if (!suppressDirtyMarker) formDirty.value = true;
}, { deep: true });

// -------------------------------------------------------------------------
// External change tracking (US-A4-4) — polling fallback + conflict resolution
// -------------------------------------------------------------------------
const POLL_INTERVAL_MS = 5000;
let pollTimer: ReturnType<typeof setInterval> | undefined;

/** Set when a poll saw the selected profile change on disk while editing. */
const externallyUpdated = ref(false);
const externalProfile = ref<AppAgentProfile | undefined>(undefined);

/** Per-field diff between the on-disk version and the form, when saving is intercepted. */
const conflict = ref<ConflictRow[] | null>(null);

// Tool tag editor state
const availableTools = ref<AppToolDescriptor[]>([]);
const mcpServers = ref<AppMcpServer[]>([]);
const toolTags = ref<string[]>([]);
const toolsManuallyEdited = ref(false);

// Sync toolTags ↔ form.tools (comma-separated string for form submit)
watch(toolTags, (tags) => {
  form.tools = tags.join(', ');
  // Don't mark as manually edited when the startFrom prefill is auto-filling
  if (!suppressStartFromPrefill && !toolsManuallyEdited.value) {
    toolsManuallyEdited.value = true;
  }
}, { deep: true });

// "Start from" watcher — prefills all form fields from the selected base
// profile during create. This is a UI convenience; the stored profile is
// self-contained (no `extends`). Only fires in create mode.
watch(startFrom, async (newVal, oldVal) => {
  if (!isCreating.value) return;
  if (newVal === oldVal) return;

  if (!newVal) {
    // "None" — reset to blank form (name stays untouched)
    applyFormValues(emptyFormValues(), false);
    toolTags.value = [];
    toolsManuallyEdited.value = false;
    await nextTick();
    formDirty.value = false;
    return;
  }

  const base = props.profiles.find((p) => p.name === newVal);
  if (!base) return;

  // Prefill all fields from the base profile (name stays blank)
  suppressStartFromPrefill = true;
  suppressDirtyMarker = true;
  applyFormValues(formValuesFromProfile(base), false);
  toolTags.value = [...(base.tools ?? [])];
  toolsManuallyEdited.value = false;
  formError.value = '';
  await nextTick();
  suppressStartFromPrefill = false;
  suppressDirtyMarker = false;
  formDirty.value = false;
});

/** Copy a value set onto the reactive form. `includeName` is false for the
 * "start from" affordance, which must leave a typed name intact. */
function applyFormValues(values: ProfileFormValues, includeName: boolean): void {
  if (includeName) form.name = values.name;
  form.description = values.description;
  form.defaultModel = values.defaultModel;
  form.tools = values.tools;
  form.whenToUse = values.whenToUse;
  form.systemPromptTemplate = values.systemPromptTemplate;
  form.subagents = values.subagents;
  form.disallowedTools = values.disallowedTools;
}

// Suppresses the "manual edit" marker in the toolTags watcher while the
// startFrom prefill is writing tools during a create.
let suppressStartFromPrefill = false;

// Load tools and MCP servers on mount; start the list poll (US-A4-4).
onMounted(async () => {
  pollTimer = setInterval(() => { void pollExternalChanges(); }, POLL_INTERVAL_MS);
  try {
    const api = getMirriWebApi();
    const [tools, servers] = await Promise.all([
      api.listToolsCatalog().catch((e) => {
        console.warn('[AgentProfiles] Failed to load tools catalog:', e);
        return [];
      }),
      api.listGlobalMcpServers().catch((e) => {
        console.warn('[AgentProfiles] Failed to load MCP servers:', e);
        return [];
      }),
    ]);
    availableTools.value = tools;
    mcpServers.value = servers;
  } catch {
    // Tools/MCP are optional — don't block the form
  }
});

onUnmounted(() => {
  if (pollTimer !== undefined) {
    clearInterval(pollTimer);
    pollTimer = undefined;
  }
});

/**
 * Polling fallback for realtime profile sync (US-A4-4, architecture risk 1
 * ruling): fetch the list on a fixed interval. With no unsaved edits the
 * refresh is applied silently via `refresh`; with unsaved edits on a profile
 * that changed on disk, mark it and intercept the next save with a
 * three-way conflict resolution.
 */
async function pollExternalChanges(): Promise<void> {
  if (typeof document !== 'undefined' && document.hidden) return;
  let result: AppAgentProfileList;
  try {
    result = await getMirriWebApi().listAgents();
  } catch {
    return; // best-effort — a transient backend error must not disrupt the panel
  }
  if (
    JSON.stringify(result.profiles) === JSON.stringify(props.profiles) &&
    JSON.stringify(result.skippedAgentFiles) === JSON.stringify(props.skippedFiles ?? [])
  ) return;

  const editingExisting = !isCreating.value && selectedName.value !== null;
  if (editingExisting && formDirty.value) {
    const next = result.profiles.find((p) => p.name === selectedName.value);
    const prev = props.profiles.find((p) => p.name === selectedName.value);
    if (next !== undefined && JSON.stringify(next) !== JSON.stringify(prev)) {
      externalProfile.value = next;
      externallyUpdated.value = true;
    }
  }
  emit('refresh', result);
}

function clearExternalUpdateState(): void {
  externallyUpdated.value = false;
  externalProfile.value = undefined;
  conflict.value = null;
}

// Whether the full effective system prompt of the selected built-in profile
// is expanded (read-only preview).
const showFullPrompt = ref(false);

// System prompt editor mode: raw template ('code') or rendered markdown ('preview').
const promptTab = ref<'code' | 'preview'>('code');

// -------------------------------------------------------------------------
// Actions
// -------------------------------------------------------------------------
async function selectProfile(name: string): Promise<void> {
  selectedName.value = name;
  isCreating.value = false;
  showFullPrompt.value = false;
  promptTab.value = 'code';
  clearExternalUpdateState();
  // Populate form for editing (works for both custom and built-in profiles)
  const p = props.profiles.find((x) => x.name === name);
  if (p) {
    await populateForm(p);
  }
}

function startCreate(): void {
  isCreating.value = true;
  selectedName.value = null;
  showFullPrompt.value = false;
  promptTab.value = 'code';
  clearExternalUpdateState();
  resetForm();
}

function resetForm(): void {
  applyFormValues(emptyFormValues(), true);
  toolTags.value = [];
  toolsManuallyEdited.value = false;
  startFrom.value = '';
  formError.value = '';
  formDirty.value = false;
}

async function populateForm(p: AppAgentProfile): Promise<void> {
  // Suppress the "manual edit" markers in the watchers while we set the
  // profile's own values.
  suppressStartFromPrefill = true;
  suppressDirtyMarker = true;
  applyFormValues(formValuesFromProfile(p, p.name), true);
  toolTags.value = [...(p.tools ?? [])];
  toolsManuallyEdited.value = false;
  formError.value = '';
  // Reset the guards after nextTick so the watchers (which fire async) do
  // not mark this population as a manual edit.
  await nextTick();
  suppressStartFromPrefill = false;
  suppressDirtyMarker = false;
  formDirty.value = false;
}

// Re-populate the form when the selected profile's data changes externally
// (e.g. after a reset-to-default, which updates props.profiles via App.vue
// without going through selectProfile()). Never clobbers unsaved edits —
// those are handled by the polling conflict flow instead.
watch(selectedProfile, (p) => {
  showFullPrompt.value = false;
  promptTab.value = 'code';
  if (p && !isCreating.value && !formDirty.value) {
    populateForm(p);
  }
});

function cancelForm(): void {
  isCreating.value = false;
  selectedName.value = null;
  clearExternalUpdateState();
}

// The i18n key for each structured validation failure (US-A3-2) — the panel
// translates the pure-function error code, keeping the lib locale-free.
const FORM_VALIDATION_KEYS: Record<AgentProfileFormError['code'], string> = {
  nameRequired: 'agents.validation.nameRequired',
  namePattern: 'agents.validation.namePattern',
  nameTooLong: 'agents.validation.nameTooLong',
  descriptionRequired: 'agents.validation.descriptionRequired',
};

function submitForm(): void {
  // Front-end validation (US-A3-2): invalid name / empty description are
  // intercepted here, naming the offending field — no request is sent.
  const validationError = validateAgentProfileForm(form, { creating: isCreating.value });
  if (validationError !== null) {
    formError.value = t(FORM_VALIDATION_KEYS[validationError.code]);
    return;
  }
  formError.value = '';

  // Conflict interception (US-A4-4): when the profile changed on disk while
  // editing, the save is held back until the user resolves external vs form
  // values field by field.
  if (shouldInterceptSave(isCreating.value, externallyUpdated.value, externalProfile.value)) {
    const external = externalProfile.value;
    if (external !== undefined && openConflict(external)) return;
  }

  doSave();
}

function doSave(): void {
  // Always send the tools/arrays — the form is populated from the existing
  // profile on every edit, so an empty value means "no tools", not "leave
  // unchanged". `undefined` would make the kap-server PUT merge preserve the
  // old list, so clearing every tool tag would silently no-op (same defect
  // class as the subagents fix). `[]` clears the persisted frontmatter key.
  if (isCreating.value) {
    emit('create', buildCreateProfilePayload(form));
  } else if (selectedName.value !== null) {
    emit('update', selectedName.value, buildUpdateProfilePayload(form));
  }
  clearExternalUpdateState();
  formDirty.value = false;
  isCreating.value = false;
  selectedName.value = null;
}

async function onDeleteProfile(name: string): Promise<void> {
  if (
    await confirm({
      title: t('agents.delete'),
      message: t('agents.confirmDelete'),
      variant: 'danger',
    })
  ) {
    emit('delete', name);
  }
}

function toggleProfile(profile: AppAgentProfile): void {
  if (!canToggleProfile(profile)) return;
  if (profile.enabled) {
    emit('disable', profile.name);
  } else {
    emit('enable', profile.name);
  }
}

async function onResetProfile(name: string): Promise<void> {
  if (
    await confirm({
      title: t('agents.resetToDefault'),
      message: t('agents.confirmReset'),
      variant: 'danger',
    })
  ) {
    emit('reset', name);
  }
}

// -------------------------------------------------------------------------
// Source / reason labels (US-A1-1)
// -------------------------------------------------------------------------
const SOURCE_KINDS = ['builtin', 'user', 'project', 'extra', 'plugin', 'explicit'] as const;

function sourceLabel(source: string): string {
  const key = `agents.source.${source}`;
  return SOURCE_KINDS.includes(source as (typeof SOURCE_KINDS)[number]) && te(key)
    ? t(key)
    : source;
}

function suppressedReasonLabel(reason: string): string {
  const key = `agents.suppressed.reasons.${reason}`;
  return te(key) ? t(key) : reason;
}

// -------------------------------------------------------------------------
// Suppressed candidates expansion (US-A1-2) + skipped warnings (US-A1-3)
// -------------------------------------------------------------------------
const expandedSuppressedNames = ref<string[]>([]);
const skippedExpanded = ref(true);

function toggleSuppressed(name: string): void {
  const idx = expandedSuppressedNames.value.indexOf(name);
  if (idx >= 0) {
    expandedSuppressedNames.value.splice(idx, 1);
  } else {
    expandedSuppressedNames.value.push(name);
  }
}

function isSuppressedExpanded(name: string): boolean {
  return expandedSuppressedNames.value.includes(name);
}

// -------------------------------------------------------------------------
// Subagents editing (US-A4-3) — options are the currently enabled profiles
// -------------------------------------------------------------------------
const subagentOptions = computed(() => {
  return enabledSubagentOptions(props.profiles, selectedName.value);
});

function toggleSubagent(name: string): void {
  const idx = form.subagents.indexOf(name);
  if (idx >= 0) {
    form.subagents.splice(idx, 1);
  } else {
    form.subagents.push(name);
  }
}

// -------------------------------------------------------------------------
// Capabilities (A2-3) — read-only display with human-readable descriptions.
// Unknown/custom tags render the raw tag alone (no description) — the
// registry accepts any non-empty tag, so a missing entry is not an error.
// -------------------------------------------------------------------------
const selectedCapabilities = computed(() => {
  return selectedProfile.value?.capabilitiesRequired ?? [];
});

function capabilityDescription(tag: string): string {
  const key = capabilityDescKey(tag);
  return key !== undefined && te(key) ? t(key) : '';
}

// -------------------------------------------------------------------------
// Conflict resolution (US-A4-4) — external vs form values, three-way
// -------------------------------------------------------------------------
const CONFLICT_FIELD_LABELS: Record<ConflictField, () => string> = {
  description: () => t('agents.description'),
  defaultModel: () => t('agents.defaultModel'),
  tools: () => t('agents.tools'),
  whenToUse: () => t('agents.whenToUse'),
  systemPromptTemplate: () => t('agents.systemPrompt'),
  subagents: () => t('agents.subagents'),
  disallowedTools: () => t('agents.disallowedTools'),
};

/** Build the per-field diff rows; returns false when nothing actually differs. */
function openConflict(ext: AppAgentProfile): boolean {
  const rows = buildConflictRows(ext, form);
  if (rows.length === 0) return false;
  conflict.value = rows;
  return true;
}

/** Three-way choice — adopt the on-disk version wholesale (no save). */
async function conflictUseFile(): Promise<void> {
  const ext = externalProfile.value;
  if (ext === undefined) return;
  conflict.value = null;
  await populateForm(ext);
  clearExternalUpdateState();
}

/** Three-way choice — keep the form values and save over the file. */
function conflictKeepMine(): void {
  conflict.value = null;
  clearExternalUpdateState();
  doSave();
}

/** Three-way choice — apply the per-field picks, then save. */
function conflictApplyChoices(): void {
  const ext = externalProfile.value;
  const rows = conflict.value;
  if (ext === undefined || rows === null) return;
  for (const row of rows) {
    if (row.useFile) applyFileValue(form, ext, row.field);
  }
  conflict.value = null;
  clearExternalUpdateState();
  doSave();
}

/** Three-way choice — collapse the picker and keep editing (decide later). */
function conflictCancel(): void {
  if (conflict.value !== null) conflict.value = null;
}

// Header text for the detail pane
const detailHeader = computed(() => {
  if (isCreating.value) return t('agents.newAgent');
  if (selectedProfile.value) return t('agents.editing', { name: selectedProfile.value.name });
  return '';
});

// Edit-face gating (D16): which face the detail pane is in — custom,
// builtin, or essential. The essential profile is never "custom" on the
// delete face even when a user override covers it (D16: `builtin: false` +
// `has_override: true`) — its delete button must always render the locked
// variant. Its content IS editable (every field, saved as a user override
// while the built-in registration stays as the fallback identity) — only the
// destructive operations (delete/disable) remain locked.
const editFace = computed(() => editingKind(isCreating.value, selectedProfile.value));
const isEditingCustom = computed(() => editFace.value === 'custom');
const isEditingBuiltin = computed(() => editFace.value === 'builtin' || editFace.value === 'essential');
const isEditingEssential = computed(() => editFace.value === 'essential');
</script>

<template>
  <div class="app-panel">
    <!-- Loading -->
    <div v-if="loading" class="state-row">
      <Spinner size="sm" />
      <span>{{ t('agents.loading') }}</span>
    </div>

    <div v-else class="master-detail">
      <!-- Left: profile list -->
      <div class="list-pane" :class="{ 'hidden-mobile': showForm }">
        <div class="list-header">
          <span class="list-title">{{ t('agents.title') }}</span>
          <Button variant="primary" size="sm" @click="startCreate">
            <Icon name="plus" size="sm" />
          </Button>
        </div>
        <!-- Skipped agent files (US-A1-3) — collapsible warning area -->
        <div v-if="(skippedFiles ?? []).length > 0" class="skipped-warnings">
          <button type="button" class="skipped-header" @click="skippedExpanded = !skippedExpanded">
            <Icon name="alert-triangle" size="sm" class="skipped-icon" />
            <span class="skipped-title">{{ t('agents.skippedFiles.title', { count: skippedFiles!.length }) }}</span>
            <Icon :name="skippedExpanded ? 'chevron-up' : 'chevron-down'" size="sm" class="skipped-chevron" />
          </button>
          <ul v-if="skippedExpanded" class="skipped-list">
            <li v-for="s in skippedFiles" :key="s.path" class="skipped-item">
              <span class="skipped-path">{{ s.path }}</span>
              <span class="skipped-reason">{{ s.reason }}</span>
            </li>
          </ul>
        </div>
        <div class="list-scroll">
          <button
            v-for="p in profiles"
            :key="p.name"
            type="button"
            class="list-row"
            :class="{
              selected: selectedName === p.name && !isCreating,
              disabled: !p.enabled,
            }"
            @click="selectProfile(p.name)"
          >
            <div class="list-row-main">
              <!-- Row 1: name + badges + toggle (compact) -->
              <div class="list-row-header">
                <span class="list-row-name">{{ p.name }}</span>
                <div class="list-row-badges">
                  <Badge variant="neutral" size="sm">{{ sourceLabel(p.source) }}</Badge>
                  <!-- US-A6-2: a built-in profile covered by a user file is
                       labeled as overridden, not as plain custom. -->
                  <Badge v-if="overrideBadgeKind(p) === 'overridden'" variant="success" size="sm">{{ t('agents.overridden') }}</Badge>
                  <Badge v-else-if="overrideBadgeKind(p) === 'custom'" variant="success" size="sm">{{ t('agents.custom') }}</Badge>
                  <Badge v-if="p.essential" variant="warning" size="sm">{{ t('agents.essential') }}</Badge>
                </div>
                <div
                  v-if="!p.essential"
                  class="list-row-toggle"
                  @click.stop
                >
                  <Switch
                    :model-value="p.enabled"
                    @update:model-value="toggleProfile(p)"
                  />
                </div>
                <!-- Essential profile: the disable toggle stays visible but
                     greyed out, with the reason on hover (D16 keeps the
                     operation locked while content editing is open). -->
                <Tooltip v-else :text="t('agents.essentialDisableLocked')" placement="left">
                  <div class="list-row-toggle">
                    <Switch :model-value="p.enabled" disabled />
                  </div>
                </Tooltip>
              </div>
              <!-- Row 2: description (up to 2 lines) -->
              <span v-if="p.description" class="list-row-desc">{{ p.description }}</span>
              <!-- Row 3: meta (model) -->
              <div v-if="p.defaultModel" class="list-row-meta">
                <span class="meta-item">{{ p.defaultModel }}</span>
              </div>
              <!-- Row 4: suppressed candidates (US-A1-2) -->
              <div v-if="(p.suppressedCandidates?.length ?? 0) > 0" class="suppressed-block">
                <button
                  type="button"
                  class="suppressed-toggle"
                  @click.stop="toggleSuppressed(p.name)"
                >
                  <Icon :name="isSuppressedExpanded(p.name) ? 'chevron-up' : 'chevron-down'" size="sm" />
                  <span>{{ t('agents.suppressed.toggle', { count: p.suppressedCandidates!.length }) }}</span>
                </button>
                <div v-if="isSuppressedExpanded(p.name)" class="suppressed-detail">
                  <div v-for="(c, i) in p.suppressedCandidates" :key="i" class="suppressed-row">
                    <span class="suppressed-cell suppressed-source">{{ sourceLabel(c.source) }}</span>
                    <span class="suppressed-cell suppressed-priority">{{ t('agents.suppressed.priority') }} {{ c.priority }}</span>
                    <span class="suppressed-cell">{{ suppressedReasonLabel(c.reason) }}</span>
                  </div>
                </div>
              </div>
            </div>
          </button>
        </div>
      </div>

      <!-- Right: detail pane -->
      <div class="detail-pane" :class="{ 'hidden-mobile': !showForm }">
        <!-- Empty state -->
        <div v-if="!showForm" class="empty-state">
          <Icon name="users" size="lg" />
          <p>{{ t('agents.selectPrompt') }}</p>
        </div>

        <!-- Creating or editing any profile (custom or built-in) -->
        <template v-else-if="showForm">
          <div class="detail-header">
            <button v-if="isCreating || selectedName" class="back-btn" type="button" @click="cancelForm">
              <Icon name="arrow-left" size="sm" />
              <span>{{ t('agents.backToList') }}</span>
            </button>
            <span class="detail-title">{{ detailHeader }}</span>
            <div v-if="isCreating" class="header-start-from">
              <span class="header-start-from-label">{{ t('agents.startFrom') }}:</span>
              <Select v-model="startFrom" size="sm" class="header-start-from-select">
                <option value="">{{ t('agents.startFromNone') }}</option>
                <option
                  v-for="p in profiles"
                  :key="p.name"
                  :value="p.name"
                >
                  {{ p.name }}
                </option>
              </Select>
            </div>
            <Button
              v-if="isEditingBuiltin && canResetToDefault(selectedProfile)"
              variant="danger-soft"
              size="sm"
              class="reset-btn"
              @click="onResetProfile(selectedName!)"
            >
              {{ t('agents.resetToDefault') }}
            </Button>
          </div>
          <div class="detail-scroll">
            <div class="detail-form">
              <!-- D16 identity warning: editing the essential base agent is
                   open, but the built-in registration is the fallback — the
                   destructive operations stay locked. -->
              <Banner v-if="isEditingEssential" variant="warning">
                {{ t('agents.essentialEditWarning') }}
              </Banner>
              <Field v-if="isCreating" :label="t('agents.add')">
                <Input v-model="form.name" :placeholder="t('agents.namePlaceholder')" spellcheck="false" />
              </Field>
              <Field :label="t('agents.description')">
                <textarea
                  v-model="form.description"
                  class="role-textarea"
                  :placeholder="t('agents.descriptionPlaceholder')"
                  spellcheck="false"
                  rows="1"
                />
              </Field>

              <Field :label="t('agents.defaultModel')">
                <Select v-model="form.defaultModel">
                  <option value="">{{ t('agents.modelInherited') }}</option>
                  <option
                    v-for="m in (models ?? [])"
                    :key="m.id"
                    :value="m.id"
                  >
                    {{ m.displayName ?? m.model }}
                  </option>
                </Select>
              </Field>
              <Field :label="t('agents.tools')">
                <ToolTagEditor
                  v-model="toolTags"
                  :available-tools="availableTools"
                  :mcp-servers="mcpServers"
                />
              </Field>
              <!-- Disallowed tools — editable denylist (A4-2 edit face),
                   symmetric with the tools editor above. -->
              <Field :label="t('agents.disallowedTools')" :hint="t('agents.disallowedToolsHint')">
                <ToolTagEditor
                  v-model="form.disallowedTools"
                  :available-tools="availableTools"
                  :mcp-servers="mcpServers"
                />
              </Field>
              <!-- Capabilities — read-only display with human-readable
                   descriptions (A2-3). Unknown tags show the raw tag only. -->
              <div v-if="selectedCapabilities.length > 0" class="capabilities">
                <span class="capabilities-label">{{ t('agents.capabilities') }}:</span>
                <div class="capabilities-list">
                  <div v-for="cap in selectedCapabilities" :key="cap" class="capability">
                    <span class="capability-tag">{{ cap }}</span>
                    <span v-if="capabilityDescription(cap)" class="capability-desc">{{ capabilityDescription(cap) }}</span>
                  </div>
                </div>
              </div>
              <!-- Subagents multi-select (US-A4-3) — options = enabled profiles -->
              <Field :label="t('agents.subagents')" :hint="t('agents.subagentsHint')">
                <div class="subagents-list">
                  <label
                    v-for="opt in subagentOptions"
                    :key="opt.name"
                    class="subagent-option"
                  >
                    <Checkbox
                      :model-value="form.subagents.includes(opt.name)"
                      @update:model-value="toggleSubagent(opt.name)"
                    >
                      {{ opt.name }}
                    </Checkbox>
                  </label>
                  <span v-if="subagentOptions.length === 0" class="subagents-empty">
                    {{ t('agents.subagentsEmpty') }}
                  </span>
                </div>
              </Field>
              <Field :label="t('agents.whenToUse')">
                <textarea
                  v-model="form.whenToUse"
                  class="role-textarea"
                  :placeholder="t('agents.whenToUsePlaceholder')"
                  spellcheck="false"
                  rows="1"
                />
              </Field>
              <Field
                :label="t('agents.systemPrompt')"
                class="prompt-field"
                :hint="isEditingEssential
                  ? t('agents.essentialPromptHint')
                  : (isEditingBuiltin ? t('agents.builtinPromptHint') : undefined)"
              >
                <div class="prompt-tabs">
                  <button
                    type="button"
                    class="prompt-tab"
                    :class="{ active: promptTab === 'code' }"
                    @click="promptTab = 'code'"
                  >
                    {{ t('agents.promptTabCode') }}
                  </button>
                  <button
                    type="button"
                    class="prompt-tab"
                    :class="{ active: promptTab === 'preview' }"
                    @click="promptTab = 'preview'"
                  >
                    {{ t('agents.promptTabPreview') }}
                  </button>
                </div>
                <textarea
                  v-if="promptTab === 'code'"
                  v-model="form.systemPromptTemplate"
                  class="system-prompt-textarea"
                  :placeholder="t('agents.systemPromptPlaceholder')"
                  spellcheck="false"
                  rows="8"
                />
                <div v-else class="prompt-preview-scroll">
                  <Markdown :text="form.systemPromptTemplate ?? ''" class="prompt-preview" />
                </div>
                <!-- D12 (ADR-007): every row with an effective prompt carries
                     the read-only preview — not just builtins. -->
                <button
                  v-if="selectedProfile?.effectiveSystemPrompt"
                  type="button"
                  class="full-prompt-toggle"
                  @click="showFullPrompt = !showFullPrompt"
                >
                  {{ showFullPrompt ? t('agents.hideFullPrompt') : t('agents.viewFullPrompt') }}
                </button>
                <pre v-if="showFullPrompt" class="full-prompt-preview">{{ selectedProfile?.effectiveSystemPrompt }}</pre>
              </Field>

              <!-- External change banner (US-A4-4): the file changed on disk
                   while the user has unsaved edits — the save is intercepted. -->
              <div
                v-if="externallyUpdated && !isCreating && conflict === null"
                class="external-update-banner"
              >
                <Icon name="alert-triangle" size="sm" class="external-update-icon" />
                <span>{{ t('agents.conflict.externallyUpdated') }}</span>
              </div>

              <!-- Conflict resolution (US-A4-4): external vs form, three-way -->
              <div v-if="conflict !== null" class="conflict-box">
                <div class="conflict-title">{{ t('agents.conflict.title') }}</div>
                <p class="conflict-intro">{{ t('agents.conflict.intro') }}</p>
                <div class="conflict-table" role="table">
                  <div class="conflict-row conflict-head" role="row">
                    <span class="conflict-cell conflict-field">{{ t('agents.conflict.field') }}</span>
                    <span class="conflict-cell">{{ t('agents.conflict.fileValue') }}</span>
                    <span class="conflict-cell">{{ t('agents.conflict.myValue') }}</span>
                    <span class="conflict-cell conflict-choice">{{ t('agents.conflict.mine') }}</span>
                  </div>
                  <div v-for="row in conflict" :key="row.field" class="conflict-row" role="row">
                    <span class="conflict-cell conflict-field">{{ CONFLICT_FIELD_LABELS[row.field]() }}</span>
                    <span class="conflict-cell conflict-value">{{ row.fileValue }}</span>
                    <span class="conflict-cell conflict-value">{{ row.mineValue }}</span>
                    <span class="conflict-cell conflict-choice">
                      <Checkbox
                        :model-value="row.useFile"
                        :label="t('agents.conflict.file')"
                        @update:model-value="row.useFile = $event"
                      />
                    </span>
                  </div>
                </div>
                <div class="conflict-actions">
                  <Button variant="secondary" size="sm" @click="conflictUseFile">{{ t('agents.conflict.useFile') }}</Button>
                  <Button variant="secondary" size="sm" @click="conflictKeepMine">{{ t('agents.conflict.keepMine') }}</Button>
                  <Button variant="primary" size="sm" @click="conflictApplyChoices">{{ t('agents.conflict.saveWithChoices') }}</Button>
                  <Button variant="secondary" size="sm" @click="conflictCancel">{{ t('agents.cancel') }}</Button>
                </div>
              </div>

              <div v-if="formError" class="form-error">{{ formError }}</div>
              <div class="form-btns">
                <Button variant="primary" size="sm" @click="submitForm">{{ isCreating ? t('agents.create') : t('agents.save') }}</Button>
                <Button variant="secondary" size="sm" @click="cancelForm">{{ t('agents.cancel') }}</Button>
                <Button v-if="isEditingCustom" variant="danger-soft" size="sm" @click="onDeleteProfile(selectedName!)">{{ t('agents.delete') }}</Button>
                <!-- Essential profile: the delete action stays visible but
                     greyed out, with the reason on hover (D16). -->
                <Tooltip v-else-if="isEditingEssential" :text="t('agents.essentialDeleteLocked')">
                  <Button variant="danger-soft" size="sm" disabled>{{ t('agents.delete') }}</Button>
                </Tooltip>
              </div>
            </div>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.app-panel {
  height: 100%;
  display: flex;
  flex-direction: column;
}

.state-row {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-6) 0;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-base);
}

.master-detail {
  display: grid;
  grid-template-columns: 260px 1fr;
  gap: var(--space-3);
  height: 100%;
  min-height: 0;
}

/* --- Left list pane --- */
.list-pane {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  overflow: hidden;
  min-height: 0;
}

.list-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--space-2) var(--space-3);
  border-bottom: 1px solid var(--color-line);
}

.list-title {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text-muted);
}

.list-scroll {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-1);
}

.list-row {
  display: flex;
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border: none;
  border-radius: var(--radius-md);
  background: transparent;
  cursor: pointer;
  text-align: left;
  transition: background var(--duration-fast) var(--ease-out);
}
.list-row:hover { background: var(--color-surface-sunken); }
.list-row.selected { background: var(--color-accent-soft); }

.list-row-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

/* Row 1: name + badges + toggle on one line */
.list-row-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  min-width: 0;
}
.list-row-name {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.list-row.disabled .list-row-name { color: var(--color-text-faint); }
.list-row-badges {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
  min-width: 0;
}
.list-row-toggle { flex: none; margin-left: auto; }

/* Row 2: description (2 lines max) */
.list-row-desc {
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.list-row-meta {
  display: flex;
  gap: var(--space-2);
  font-family: var(--font-mono);
  font-size: 10px;
  color: var(--color-text-faint);
}

/* --- Skipped files warning area (US-A1-3) --- */
.skipped-warnings {
  border-bottom: 1px solid var(--color-line);
  background: var(--color-surface-sunken);
}
.skipped-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border: none;
  background: transparent;
  cursor: pointer;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-warning, var(--color-text-muted));
  text-align: left;
}
.skipped-title { flex: 1; min-width: 0; }
.skipped-icon, .skipped-chevron { flex: none; color: inherit; }
.skipped-list {
  margin: 0;
  padding: 0 var(--space-3) var(--space-2);
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.skipped-item {
  display: flex;
  flex-direction: column;
  gap: 1px;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
}
.skipped-path {
  font-family: var(--font-mono);
  color: var(--color-text-muted);
  word-break: break-all;
}
.skipped-reason { color: var(--color-text-faint); }

/* --- Suppressed candidates (US-A1-2) --- */
.suppressed-block {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.suppressed-toggle {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  align-self: flex-start;
  padding: 0;
  border: none;
  background: transparent;
  cursor: pointer;
  font-family: var(--font-ui);
  font-size: 10px;
  color: var(--color-text-faint);
}
.suppressed-toggle:hover { color: var(--color-text-muted); }
.suppressed-detail {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding-left: var(--space-2);
  border-left: 1px solid var(--color-line);
}
.suppressed-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  font-family: var(--font-ui);
  font-size: 10px;
  color: var(--color-text-faint);
}
.suppressed-source { color: var(--color-text-muted); }
.suppressed-priority { font-family: var(--font-mono); }

/* --- Capabilities (read-only, A2-3) --- */
.capabilities {
  display: flex;
  align-items: baseline;
  gap: var(--space-2);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
}
.capabilities-label {
  color: var(--color-text-muted);
  flex: none;
}
.capabilities-list {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.capability {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--space-2);
}
.capability-tag {
  font-family: var(--font-mono);
  color: var(--color-text-muted);
  word-break: break-word;
}
.capability-desc {
  color: var(--color-text-faint);
  word-break: break-word;
}

/* --- Subagents multi-select (US-A4-3) --- */
.subagents-list {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2) var(--space-4);
  padding: var(--space-2) 0;
}
.subagents-empty {
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
}

/* --- External update banner + conflict box (US-A4-4) --- */
.external-update-banner {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-left: 3px solid var(--color-warning, var(--color-accent));
  border-radius: var(--radius-md);
  background: var(--color-surface-sunken);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
}
.external-update-icon { flex: none; }

.conflict-box {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-sunken);
}
.conflict-title {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
}
.conflict-intro {
  margin: 0;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
}
.conflict-table {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.conflict-row {
  display: grid;
  grid-template-columns: minmax(80px, 0.8fr) 2fr 2fr auto;
  gap: var(--space-2);
  align-items: start;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
}
.conflict-head { color: var(--color-text-faint); }
.conflict-cell { min-width: 0; }
.conflict-field { color: var(--color-text-muted); }
.conflict-value {
  font-family: var(--font-mono);
  color: var(--color-text-muted);
  display: -webkit-box;
  -webkit-line-clamp: 3;
  -webkit-box-orient: vertical;
  overflow: hidden;
  word-break: break-word;
  white-space: pre-wrap;
}
.conflict-choice { display: flex; justify-content: flex-end; }
.conflict-actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}

/* --- Right detail pane --- */
.detail-pane {
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  overflow: hidden;
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.detail-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-3);
  border-bottom: 1px solid var(--color-line);
}

.back-btn {
  display: flex;
  align-items: center;
  gap: 4px;
  border: none;
  background: transparent;
  cursor: pointer;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  padding: 2px 4px;
  border-radius: var(--radius-sm);
  transition: color var(--duration-fast) var(--ease-out);
}
.back-btn:hover { color: var(--color-text); }

.detail-title {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
}

.detail-scroll {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-3);
}

.detail-form {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  min-height: 100%;
}

.form-error {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-danger);
}

.form-btns {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}

.role-textarea {
  width: 100%;
  resize: vertical;
  min-height: 40px;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text);
  outline: none;
}
.role-textarea:focus {
  border-color: var(--color-accent);
}

/* --- System prompt field: tabs + editor/preview filling remaining height --- */
.prompt-field {
  flex: 1;
  min-height: 0;
}

.prompt-tabs {
  display: flex;
  gap: var(--space-1);
}

.prompt-tab {
  padding: 2px 10px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-sm);
  background: transparent;
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  cursor: pointer;
  transition: background var(--duration-fast) var(--ease-out), color var(--duration-fast) var(--ease-out);
}
.prompt-tab:hover {
  background: var(--color-surface-sunken);
}
.prompt-tab.active {
  background: var(--color-accent-soft);
  border-color: var(--color-accent);
  color: var(--color-text);
}

.system-prompt-textarea {
  flex: 1;
  min-height: 160px;
  width: 100%;
  resize: vertical;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  color: var(--color-text);
  outline: none;
  line-height: 1.5;
}
.system-prompt-textarea:focus {
  border-color: var(--color-accent);
}
.system-prompt-textarea:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}

.prompt-preview-scroll {
  flex: 1;
  min-height: 160px;
  overflow-y: auto;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
}

.prompt-preview {
  font-size: var(--text-sm);
  line-height: 1.6;
}

.full-prompt-toggle {
  align-self: flex-start;
  border: none;
  background: transparent;
  color: var(--color-accent);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  cursor: pointer;
  padding: 0;
}
.full-prompt-toggle:hover {
  text-decoration: underline;
}

.full-prompt-preview {
  margin: 0;
  max-height: 320px;
  overflow-y: auto;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-sunken);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: 1.5;
  color: var(--color-text-muted);
  white-space: pre-wrap;
  word-break: break-word;
}

/* --- Empty state --- */
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--space-3);
  height: 100%;
  color: var(--color-text-faint);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
}

/* --- Header "Start from" selector (create mode only) --- */
.header-start-from {
  display: flex;
  align-items: center;
  gap: var(--space-1);
  margin-left: auto;
}

.header-start-from-label {
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  white-space: nowrap;
}

.header-start-from-select {
  width: auto;
  min-width: 120px;
}

/* --- Reset button --- */
.reset-btn {
  margin-left: auto;
}

/* --- Mobile --- */
@media (max-width: 640px) {
  .master-detail {
    grid-template-columns: 1fr;
  }
  .hidden-mobile {
    display: none;
  }
}
</style>
