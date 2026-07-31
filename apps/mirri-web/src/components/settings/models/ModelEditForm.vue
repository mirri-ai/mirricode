<!-- apps/mirri-web/src/components/settings/models/ModelEditForm.vue -->
<!-- Single model alias editor — full ModelAlias field set, grouped into
     collapsible sections. Writes via PATCH /config (debounced). The "keep as
     override" switch routes edits into the `overrides` sub-object instead of
     top-level fields, so they survive catalog refreshes. -->
<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppModelAlias } from '../../../api/types';
import { getMirriWebApi } from '../../../api';
import { formatTokens } from '../../../lib/formatTokens';
import { resolveAlias } from '../../../lib/resolveAlias';
import Field from '../../ui/Field.vue';
import Input from '../../ui/Input.vue';
import Textarea from '../../ui/Textarea.vue';
import Select from '../../ui/Select.vue';
import Switch from '../../ui/Switch.vue';
import Badge from '../../ui/Badge.vue';
import Button from '../../ui/Button.vue';
import Icon from '../../ui/Icon.vue';
import Spinner from '../../ui/Spinner.vue';

const { t } = useI18n();
const api = getMirriWebApi();

const props = defineProps<{
  modelId: string;
  alias: AppModelAlias;
  /** True while a save is in flight (disables inputs). */
  saving?: boolean;
}>();

const emit = defineEmits<{
  saveError: [modelId: string, error: unknown];
  saved: [modelId: string, patch: Partial<AppModelAlias>];
}>();

// -------------------------------------------------------------------------
// Capabilities — preset chips, custom additions allowed
// -------------------------------------------------------------------------
const CAPABILITY_PRESETS = ['image_in', 'video_in', 'audio_in', 'thinking', 'tool_use', 'dynamically_loaded_tools'] as const;
const EFFORT_PRESETS = ['low', 'medium', 'high', 'max'] as const;

// -------------------------------------------------------------------------
// Local form state — mirrors the alias. Edits update local state; a debounced
// watcher persists the patch to /config.
// -------------------------------------------------------------------------
const showThinking = ref(false);
const showAdvanced = ref(false);
const dirty = ref(false);
const saveTimer = ref<ReturnType<typeof setTimeout> | undefined>(undefined);
/** Three-state save indicator: idle → saving → saved. */
type SaveState = 'idle' | 'saving' | 'saved';
const saveState = ref<SaveState>('idle');
const savedTimer = ref<ReturnType<typeof setTimeout> | undefined>(undefined);
/** Snapshot of the alias at sync time — used to diff only changed fields
 *  into `overrides`, so unmodified fields still follow catalog refreshes. */
const originalAlias = ref<AppModelAlias | null>(null);

interface FormState {
  provider: string;
  model: string;
  displayName: string;
  maxContextSize: string;
  maxOutputSize: string;
  capabilities: string[];
  supportEfforts: string[];
  defaultEffort: string;
  reasoningKey: string;
  adaptiveThinking: boolean;
  description: string;
  protocol: string;
  betaApi: boolean;
}

const form = reactive<FormState>({
  provider: '',
  model: '',
  displayName: '',
  maxContextSize: '',
  maxOutputSize: '',
  capabilities: [],
  supportEfforts: [],
  defaultEffort: '',
  reasoningKey: '',
  adaptiveThinking: false,
  description: '',
  protocol: '',
  betaApi: false,
});

function syncFromAlias(alias: AppModelAlias): void {
  const resolved = resolveAlias(alias);
  form.provider = resolved.provider;
  form.model = resolved.model;
  form.displayName = resolved.displayName ?? '';
  form.maxContextSize = String(resolved.maxContextSize ?? '');
  form.maxOutputSize = resolved.maxOutputSize !== undefined ? String(resolved.maxOutputSize) : '';
  form.capabilities = [...(resolved.capabilities ?? [])];
  form.supportEfforts = [...(resolved.supportEfforts ?? [])];
  form.defaultEffort = resolved.defaultEffort ?? '';
  form.reasoningKey = resolved.reasoningKey ?? '';
  form.adaptiveThinking = resolved.adaptiveThinking ?? false;
  form.description = resolved.description ?? '';
  form.protocol = resolved.protocol ?? '';
  form.betaApi = resolved.betaApi ?? false;
  // Store the resolved alias as the diff baseline. Overrides are collapsed
  // into top-level so buildPatch compares against effective values.
  originalAlias.value = resolved;
  // Auto-expand thinking section if it has values.
  showThinking.value = form.supportEfforts.length > 0 || form.defaultEffort.length > 0 || form.reasoningKey.length > 0;
  showAdvanced.value = form.description.length > 0 || form.protocol.length > 0 || form.betaApi;
  dirty.value = false;
}

/** Snapshot the current form state as the new diff baseline. Called after a
 *  successful save so the next buildPatch compares against what's now on the
 *  server, not the pre-edit snapshot. */
function syncOriginalFromForm(): void {
  if (!originalAlias.value) return;
  const ctxNum = Number(form.maxContextSize);
  // Store resolved values (no separate overrides) so subsequent diffs
  // compare against the effective state.
  originalAlias.value = {
    provider: originalAlias.value.provider,
    model: form.model.trim() || originalAlias.value.model,
    maxContextSize: Number.isFinite(ctxNum) && ctxNum > 0 ? ctxNum : originalAlias.value.maxContextSize,
    maxOutputSize: form.maxOutputSize ? Number(form.maxOutputSize) : undefined,
    capabilities: form.capabilities.length > 0 ? [...form.capabilities] : undefined,
    displayName: form.displayName.trim() || undefined,
    description: form.description.trim() || undefined,
    reasoningKey: form.reasoningKey.trim() || undefined,
    protocol: originalAlias.value.protocol,
    adaptiveThinking: form.adaptiveThinking,
    supportEfforts: form.supportEfforts.length > 0 ? [...form.supportEfforts] : undefined,
    defaultEffort: form.defaultEffort || undefined,
    betaApi: originalAlias.value.betaApi,
  };
}

// When the parent reloads models (e.g. after a save → load cycle), the alias
// object reference changes and would overwrite the form mid-edit. Only sync
// when switching to a *different* model id, or when not dirty.
watch(
  () => props.modelId,
  () => syncFromAlias(props.alias),
  { immediate: true },
);

// -------------------------------------------------------------------------
// LLM exposure toggle — controls whether `description` is set. When on, the
// model appears in the Agent tool description's "Available models" section.
// -------------------------------------------------------------------------
const llmExposed = computed(() => form.description.trim().length > 0);

function toggleLlmExposed(on: boolean): void {
  if (on) {
    // Turn on: pre-fill with a sensible default if empty.
    if (!form.description.trim()) {
      form.description = t('settings.models.fieldDescriptionPlaceholder');
    }
  } else {
    // Turn off: clear description so the model is hidden from LLM.
    form.description = '';
  }
  scheduleSave();
}

// -------------------------------------------------------------------------
// Capability / effort chip toggles
// -------------------------------------------------------------------------
function toggleCapability(cap: string): void {
  const idx = form.capabilities.indexOf(cap);
  if (idx >= 0) form.capabilities.splice(idx, 1);
  else form.capabilities.push(cap);
  scheduleSave();
}
function toggleEffort(effort: string): void {
  const idx = form.supportEfforts.indexOf(effort);
  if (idx >= 0) {
    form.supportEfforts.splice(idx, 1);
    if (form.defaultEffort === effort) form.defaultEffort = '';
  } else {
    form.supportEfforts.push(effort);
  }
  scheduleSave();
}
const customCapability = ref('');
function addCustomCapability(): void {
  const v = customCapability.value.trim();
  if (v && !form.capabilities.includes(v)) {
    form.capabilities.push(v);
    scheduleSave();
  }
  customCapability.value = '';
}
const customEffort = ref('');
function addCustomEffort(): void {
  const v = customEffort.value.trim();
  if (v && !form.supportEfforts.includes(v)) {
    form.supportEfforts.push(v);
    scheduleSave();
  }
  customEffort.value = '';
}

// -------------------------------------------------------------------------
// Debounced save — builds a patch and POSTs to /config.
// -------------------------------------------------------------------------
function scheduleSave(): void {
  dirty.value = true;
  saveState.value = 'idle';
  if (saveTimer.value !== undefined) clearTimeout(saveTimer.value);
  saveTimer.value = setTimeout(() => {
    void save();
  }, 600);
}

/** Diff the current form against the original alias and produce a patch.
 *  All changed fields are written directly to top-level config — catalog
 *  refreshes never modify existing aliases, so there is no separate override
 *  layer. */
function buildPatch(): Partial<AppModelAlias> {
  const o = originalAlias.value;
  if (!o) return {};
  const patch: Partial<AppModelAlias> = {};

  const mdl = form.model.trim();
  const origMdl = (o.model ?? '').trim();
  if (mdl !== origMdl) {
    patch.model = mdl || undefined;
  }

  const maxCtx = form.maxContextSize.trim();
  if (maxCtx !== String(o.maxContextSize ?? '')) {
    const n = Number(maxCtx);
    patch.maxContextSize = Number.isFinite(n) && n > 0 ? n : undefined;
  }

  const maxOut = form.maxOutputSize.trim();
  const origMaxOut = o.maxOutputSize !== undefined ? String(o.maxOutputSize) : '';
  if (maxOut !== origMaxOut) {
    patch.maxOutputSize = maxOut ? Number(maxOut) : undefined;
  }

  const dn = form.displayName.trim();
  const origDn = (o.displayName ?? '').trim();
  if (dn !== origDn) {
    patch.displayName = dn || undefined;
  }

  const formCaps = [...form.capabilities].sort().join(',');
  const origCaps = [...(o.capabilities ?? [])].sort().join(',');
  if (formCaps !== origCaps) {
    patch.capabilities = form.capabilities.length > 0 ? [...form.capabilities] : undefined;
  }

  const formEfforts = [...form.supportEfforts].sort().join(',');
  const origEfforts = [...(o.supportEfforts ?? [])].sort().join(',');
  if (formEfforts !== origEfforts) {
    patch.supportEfforts = form.supportEfforts.length > 0 ? [...form.supportEfforts] : undefined;
  }

  if (form.defaultEffort !== (o.defaultEffort ?? '')) {
    patch.defaultEffort = form.defaultEffort || undefined;
  }

  const rk = form.reasoningKey.trim();
  const origRk = (o.reasoningKey ?? '').trim();
  if (rk !== origRk) {
    patch.reasoningKey = rk || undefined;
  }

  if (form.adaptiveThinking !== (o.adaptiveThinking ?? false)) {
    patch.adaptiveThinking = form.adaptiveThinking;
  }

  const desc = form.description.trim();
  const origDesc = (o.description ?? '').trim();
  if (desc !== origDesc) {
    // Use empty string (not undefined) so the server merge actually clears the
    // field. `undefined` gets stripped by stripUndefinedDeep and never reaches
    // deepMerge, leaving the old value intact.
    patch.description = desc;
  }

  if (form.protocol !== (o.protocol ?? '')) {
    if (form.protocol) patch.protocol = 'anthropic';
  }
  if (form.betaApi !== (o.betaApi ?? false)) {
    patch.betaApi = form.betaApi;
  }

  return Object.keys(patch).length > 0 ? patch : {};
}

async function save(): Promise<void> {
  if (props.saving) return;
  const patch = buildPatch();
  if (Object.keys(patch).length === 0) return; // nothing changed
  const modelsPatch = { [props.modelId]: patch };
  const configPatch = { models: modelsPatch };
  saveState.value = 'saving';
  try {
    await api.setConfig(configPatch);
    dirty.value = false;
    saveState.value = 'saved';
    // Update the diff baseline so subsequent edits compare against the saved
    // state, not the pre-edit snapshot — otherwise clearing a field right
    // after saving it would produce an empty patch.
    syncOriginalFromForm();
    emit('saved', props.modelId, patch);
    if (savedTimer.value !== undefined) clearTimeout(savedTimer.value);
    savedTimer.value = setTimeout(() => { saveState.value = 'idle'; }, 2000);
  } catch (error) {
    saveState.value = 'idle';
    dirty.value = true;
    emit('saveError', props.modelId, error);
  }
}

function flushNow(): void {
  if (saveTimer.value !== undefined) {
    clearTimeout(saveTimer.value);
    saveTimer.value = undefined;
  }
  void save();
}

defineExpose({ flushNow, save });

const maxContextDisplay = computed(() => {
  const n = Number(form.maxContextSize);
  if (!Number.isFinite(n) || n <= 0) return '';
  return formatTokens(n);
});
</script>

<template>
  <div class="mef">
    <div class="mef-head">
      <div class="mef-title">
        <span v-if="saveState === 'saving'" class="mef-save-state saving">
          <Spinner size="sm" /><span>{{ t('settings.models.saving') }}</span>
        </span>
        <span v-else-if="saveState === 'saved'" class="mef-save-state saved">
          <Icon name="check" size="sm" /><span>{{ t('settings.models.saved') }}</span>
        </span>
        <Badge v-else-if="dirty" variant="info" size="sm">{{ t('settings.models.unsaved') }}</Badge>
      </div>
    </div>

    <!-- Basic section (always expanded) -->
    <section class="mef-sec">
      <Field :label="t('settings.models.fieldModelName')" :hint="t('settings.models.fieldModelNameHint')">
        <Input v-model="form.model" :placeholder="t('settings.models.fieldModelNamePlaceholder')" @input="scheduleSave" />
      </Field>
      <Field :label="t('settings.models.fieldDisplayName')">
        <Input v-model="form.displayName" :placeholder="t('settings.models.fieldDisplayNamePlaceholder')" @input="scheduleSave" />
      </Field>
      <div class="mef-row2">
        <Field :label="t('settings.models.fieldMaxContext')" :hint="maxContextDisplay">
          <Input v-model="form.maxContextSize" type="number" :placeholder="'128000'" @input="scheduleSave" />
        </Field>
        <Field :label="t('settings.models.fieldMaxOutput')">
          <Input v-model="form.maxOutputSize" type="number" :placeholder="t('settings.models.fieldOptional')" @input="scheduleSave" />
        </Field>
      </div>
      <Field :label="t('settings.models.fieldCapabilities')">
        <div class="chip-row">
          <button
            v-for="cap in CAPABILITY_PRESETS"
            :key="cap"
            type="button"
            class="chip"
            :class="{ on: form.capabilities.includes(cap) }"
            @click="toggleCapability(cap)"
          >{{ cap }}</button>
          <span v-for="cap in form.capabilities.filter(c => !CAPABILITY_PRESETS.includes(c as typeof CAPABILITY_PRESETS[number]))" :key="cap" class="chip on" @click="toggleCapability(cap)">{{ cap }}</span>
        </div>
        <div class="chip-add">
          <Input v-model="customCapability" size="sm" :placeholder="t('settings.models.addCustomCapability')" @keydown.enter.prevent="addCustomCapability" />
        </div>
      </Field>
    </section>

    <!-- Thinking section (collapsible) -->
    <section class="mef-sec">
      <button type="button" class="sec-toggle" @click="showThinking = !showThinking">
        <Icon :name="showThinking ? 'chevron-down' : 'chevron-right'" size="sm" />
        <span>{{ t('settings.models.sectionThinking') }}</span>
        <Badge v-if="form.supportEfforts.length > 0" variant="neutral" size="sm">{{ form.supportEfforts.length }}</Badge>
      </button>
      <div v-show="showThinking" class="sec-body">
        <Field :label="t('settings.models.fieldSupportEfforts')">
          <div class="chip-row">
            <button
              v-for="eff in EFFORT_PRESETS"
              :key="eff"
              type="button"
              class="chip"
              :class="{ on: form.supportEfforts.includes(eff) }"
              @click="toggleEffort(eff)"
            >{{ eff }}</button>
            <span v-for="eff in form.supportEfforts.filter(e => !EFFORT_PRESETS.includes(e as typeof EFFORT_PRESETS[number]))" :key="eff" class="chip on" @click="toggleEffort(eff)">{{ eff }}</span>
          </div>
          <div class="chip-add">
            <Input v-model="customEffort" size="sm" :placeholder="t('settings.models.addCustomEffort')" @keydown.enter.prevent="addCustomEffort" />
          </div>
        </Field>
        <Field :label="t('settings.models.fieldDefaultEffort')">
          <Select v-model="form.defaultEffort" :disabled="form.supportEfforts.length === 0" @change="scheduleSave">
            <option value="">{{ t('settings.models.noDefaultEffort') }}</option>
            <option v-for="eff in form.supportEfforts" :key="eff" :value="eff">{{ eff }}</option>
          </Select>
        </Field>
        <div class="mef-row2">
          <Field :label="t('settings.models.fieldReasoningKey')">
            <Input v-model="form.reasoningKey" :placeholder="t('settings.models.fieldOptional')" @input="scheduleSave" />
          </Field>
          <Field v-if="form.protocol === 'anthropic'" :label="t('settings.models.fieldAdaptiveThinking')">
            <Switch :model-value="form.adaptiveThinking" :label="t('settings.models.fieldAdaptiveThinking')" @update:model-value="form.adaptiveThinking = $event; scheduleSave()" />
          </Field>
        </div>
      </div>
    </section>

    <!-- Advanced section (always expanded) -->
    <section class="mef-sec">
      <div class="sec-body">
        <div class="mef-llm-toggle">
          <Switch :model-value="llmExposed" :label="t('settings.models.fieldLlmExposed')" @update:model-value="toggleLlmExposed" />
          <span class="mef-llm-desc">{{ t('settings.models.fieldLlmExposedHint') }}</span>
        </div>
        <Field v-if="llmExposed" :label="t('settings.models.fieldDescription')" :hint="t('settings.models.fieldDescriptionHint')">
          <Textarea v-model="form.description" :rows="2" :placeholder="t('settings.models.fieldDescriptionPlaceholder')" @input="scheduleSave" />
        </Field>
        <div class="mef-row2">
          <Field :label="t('settings.models.fieldProtocol')">
            <Select v-model="form.protocol" @change="scheduleSave">
              <option value="">{{ t('settings.models.protocolNone') }}</option>
              <option value="anthropic">anthropic</option>
            </Select>
          </Field>
          <Field :label="t('settings.models.fieldBetaApi')">
            <Switch :model-value="form.betaApi" :label="t('settings.models.fieldBetaApi')" @update:model-value="form.betaApi = $event; scheduleSave()" />
          </Field>
        </div>
      </div>
    </section>

    <div class="mef-foot">
      <Button v-if="dirty" variant="primary" size="sm" @click="flushNow">{{ t('settings.models.saveNow') }}</Button>
      <span class="mef-foot-state">
        <Spinner v-if="saveState === 'saving'" size="sm" />
        <Icon v-else-if="saveState === 'saved'" name="check" size="sm" class="mef-check" />
        <span>{{ saveState === 'saving' ? t('settings.models.saving') : saveState === 'saved' ? t('settings.models.saved') : t('settings.models.autosaveHint') }}</span>
      </span>
    </div>
  </div>
</template>

<style scoped>
.mef { display: flex; flex-direction: column; gap: var(--space-3); padding: var(--space-3) 0; }
.mef-head { display: flex; align-items: center; justify-content: space-between; gap: var(--space-3); flex-wrap: wrap; }
.mef-title { display: flex; align-items: center; gap: var(--space-2); }
.mef-alias { font-family: var(--font-mono); font-size: var(--text-xs); max-width: 280px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.mef-sec { display: flex; flex-direction: column; gap: var(--space-2); padding: var(--space-2) 0; border-bottom: 1px solid var(--color-line); }
.mef-sec:last-of-type { border-bottom: none; }
.mef-row2 { display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-3); }

.chip-row { display: flex; flex-wrap: wrap; gap: 6px; }
.chip {
  font-family: var(--font-mono); font-size: var(--text-xs);
  padding: 4px 8px; border-radius: var(--radius-sm);
  border: 1px solid var(--color-line); background: var(--color-surface-raised);
  color: var(--color-text-muted); cursor: pointer;
  transition: all var(--duration-fast) var(--ease-out);
}
.chip:hover { border-color: var(--color-line-strong); }
.chip.on { background: var(--color-accent-soft); border-color: var(--color-accent-bd); color: var(--color-accent); }
.chip-add { margin-top: 6px; max-width: 200px; }

.sec-toggle {
  display: flex; align-items: center; gap: var(--space-2);
  background: transparent; border: none; cursor: pointer;
  font-family: var(--font-ui); font-size: var(--text-sm); font-weight: var(--weight-medium);
  color: var(--color-text-muted); padding: var(--space-1) 0;
}
.sec-toggle:hover { color: var(--color-text); }
.sec-body { display: flex; flex-direction: column; gap: var(--space-3); padding-top: var(--space-2); }

.mef-llm-toggle { display: flex; align-items: flex-start; gap: var(--space-2); padding: var(--space-2) var(--space-3); border: 1px solid var(--color-accent-bd); border-radius: var(--radius-md); background: var(--color-accent-soft); }
.mef-llm-desc { font-size: var(--text-xs); color: var(--color-text-muted); line-height: 1.4; flex: 1; min-width: 0; }

.mef-foot { display: flex; align-items: center; gap: var(--space-3); padding-top: var(--space-2); }
.mef-foot-state { display: inline-flex; align-items: center; gap: var(--space-1); font-size: var(--text-xs); color: var(--color-text-faint); }
.mef-foot-state .mef-check { color: var(--color-success); }
.mef-save-state { display: inline-flex; align-items: center; gap: 4px; font-size: var(--text-xs); }
.mef-save-state.saving { color: var(--color-text-muted); }
.mef-save-state.saved { color: var(--color-success); }

@media (max-width: 640px) {
  .mef-row2 { grid-template-columns: 1fr; }
}
</style>
