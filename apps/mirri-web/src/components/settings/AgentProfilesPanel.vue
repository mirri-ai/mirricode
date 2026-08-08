<!-- apps/mirri-web/src/components/settings/AgentProfilesPanel.vue -->
<!-- Master-detail panel for managing agent profiles. Renders inside the
     Settings dialog's "profiles" tab — no standalone modal. -->
<script setup lang="ts">
import { computed, nextTick, onMounted, reactive, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppAgentProfile, AppModel, AppToolDescriptor, AppMcpServer } from '../../api/types';
import { getMirriWebApi } from '../../api';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Spinner from '../ui/Spinner.vue';
import Field from '../ui/Field.vue';
import Input from '../ui/Input.vue';
import Select from '../ui/Select.vue';
import Switch from '../ui/Switch.vue';
import Icon from '../ui/Icon.vue';
import ToolTagEditor from './ToolTagEditor.vue';
import Markdown from '../chat/Markdown.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();

const props = defineProps<{
  profiles: AppAgentProfile[];
  loading?: boolean;
  models?: AppModel[];
}>();

const emit = defineEmits<{
  create: [input: { name: string; description?: string; defaultModel?: string; tools?: string[]; whenToUse?: string; systemPromptTemplate?: string }];
  update: [name: string, data: Partial<{ description: string; defaultModel: string; tools: string[]; whenToUse: string; systemPromptTemplate: string }>];
  delete: [name: string];
  enable: [name: string];
  disable: [name: string];
  reset: [name: string];
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

const form = reactive({
  name: '',
  description: '',
  defaultModel: '',
  tools: '',
  whenToUse: '',
  systemPromptTemplate: '',
});
const formError = ref('');

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
    // "None" — reset to blank form
    form.description = '';
    form.defaultModel = '';
    form.tools = '';
    form.whenToUse = '';
    form.systemPromptTemplate = '';
    toolTags.value = [];
    toolsManuallyEdited.value = false;
    return;
  }

  const base = props.profiles.find((p) => p.name === newVal);
  if (!base) return;

  // Prefill all fields from the base profile (name stays blank)
  suppressStartFromPrefill = true;
  form.description = base.description ?? '';
  form.defaultModel = base.defaultModel ?? '';
  form.tools = base.tools?.join(', ') ?? '';
  form.whenToUse = base.whenToUse ?? '';
  form.systemPromptTemplate = base.systemPromptTemplate ?? '';
  toolTags.value = [...(base.tools ?? [])];
  toolsManuallyEdited.value = false;
  formError.value = '';
  await nextTick();
  suppressStartFromPrefill = false;
});

// Suppresses the "manual edit" marker in the toolTags watcher while the
// startFrom prefill is writing tools during a create.
let suppressStartFromPrefill = false;

// Load tools and MCP servers on mount
onMounted(async () => {
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
  resetForm();
}

function resetForm(): void {
  form.name = '';
  form.description = '';
  form.defaultModel = '';
  form.tools = '';
  form.whenToUse = '';
  form.systemPromptTemplate = '';
  toolTags.value = [];
  toolsManuallyEdited.value = false;
  startFrom.value = '';
  formError.value = '';
}

async function populateForm(p: AppAgentProfile): Promise<void> {
  // Suppress the "manual edit" marker in the toolTags watcher while we set
  // the profile's own tools.
  suppressStartFromPrefill = true;
  form.name = p.name;
  form.description = p.description ?? '';
  form.defaultModel = p.defaultModel ?? '';
  form.tools = p.tools?.join(', ') ?? '';
  form.whenToUse = p.whenToUse ?? '';
  form.systemPromptTemplate = p.systemPromptTemplate ?? '';
  toolTags.value = [...(p.tools ?? [])];
  toolsManuallyEdited.value = false;
  formError.value = '';
  // Reset the guard after nextTick so the toolTags watcher (which fires
  // async) does not mark this population as a manual edit.
  await nextTick();
  suppressStartFromPrefill = false;
}

// Re-populate the form when the selected profile's data changes externally
// (e.g. after a reset-to-default, which updates props.profiles via App.vue
// without going through selectProfile()).
watch(selectedProfile, (p) => {
  showFullPrompt.value = false;
  promptTab.value = 'code';
  if (p && !isCreating.value) {
    populateForm(p);
  }
});

function cancelForm(): void {
  isCreating.value = false;
  selectedName.value = null;
}

function submitForm(): void {
  if (!form.name.trim()) {
    formError.value = t('agents.nameRequired');
    return;
  }
  formError.value = '';

  const tools = form.tools.trim()
    ? form.tools.split(',').map((s) => s.trim()).filter(Boolean)
    : undefined;

  if (isCreating.value) {
    emit('create', {
      name: form.name.trim(),
      description: form.description.trim() || undefined,
      // Self-contained profile — no `extends`. The "start from" selector is a
      // UI convenience for prefilling fields, not a runtime inheritance link.
      defaultModel: form.defaultModel.trim() || undefined,
      tools,
      whenToUse: form.whenToUse.trim() || undefined,
      systemPromptTemplate: form.systemPromptTemplate.trim() || undefined,
    });
  } else if (selectedName.value !== null) {
    emit('update', selectedName.value, {
      description: form.description.trim() || undefined,
      defaultModel: form.defaultModel.trim() || undefined,
      tools,
      whenToUse: form.whenToUse.trim() || undefined,
      systemPromptTemplate: form.systemPromptTemplate.trim() || undefined,
    });
  }
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
  if (profile.essential) return;
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

// Header text for the detail pane
const detailHeader = computed(() => {
  if (isCreating.value) return t('agents.newAgent');
  if (selectedProfile.value) return t('agents.editing', { name: selectedProfile.value.name });
  return '';
});

// Whether the detail form is in edit mode for a custom profile
const isEditingCustom = computed(() => {
  return !isCreating.value && selectedProfile.value !== undefined && !selectedProfile.value.builtin;
});

// Whether the detail form is editing a built-in profile
const isEditingBuiltin = computed(() => {
  return !isCreating.value && selectedProfile.value !== undefined && selectedProfile.value.builtin;
});

// Whether the detail form is editing the essential built-in profile (`agent`),
// which the server refuses to modify — the form is a read-only view.
const isEditingEssential = computed(() => {
  return !isCreating.value && selectedProfile.value !== undefined && selectedProfile.value.essential;
});
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
                  <Badge v-if="p.builtin" variant="neutral" size="sm">{{ t('agents.source.builtin') }}</Badge>
                  <Badge v-if="p.hasOverride" variant="success" size="sm">{{ t('agents.custom') }}</Badge>
                  <Badge v-if="!p.builtin && !p.hasOverride" variant="success" size="sm">{{ t('agents.custom') }}</Badge>
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
                <Icon v-else name="lock" size="sm" class="lock-icon" />
              </div>
              <!-- Row 2: description (up to 2 lines) -->
              <span v-if="p.description" class="list-row-desc">{{ p.description }}</span>
              <!-- Row 3: meta (model) -->
              <div v-if="p.defaultModel" class="list-row-meta">
                <span class="meta-item">{{ p.defaultModel }}</span>
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
            <Button
              v-if="isEditingBuiltin && selectedProfile?.hasOverride"
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
              <Field v-if="isCreating" :label="t('agents.startFrom')">
                <Select v-model="startFrom">
                  <option value="">{{ t('agents.startFromNone') }}</option>
                  <option
                    v-for="p in profiles"
                    :key="p.name"
                    :value="p.name"
                  >
                    {{ p.name }}
                  </option>
                </Select>
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
                :hint="isEditingBuiltin
                  ? (isEditingEssential ? t('agents.essentialPromptHint') : t('agents.builtinPromptHint'))
                  : undefined"
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
                  :placeholder="isEditingEssential ? t('agents.essentialPromptPlaceholder') : t('agents.systemPromptPlaceholder')"
                  :disabled="isEditingEssential"
                  spellcheck="false"
                  rows="8"
                />
                <div v-else class="prompt-preview-scroll">
                  <Markdown :text="form.systemPromptTemplate ?? ''" class="prompt-preview" />
                </div>
                <button
                  v-if="isEditingBuiltin && selectedProfile?.effectiveSystemPrompt"
                  type="button"
                  class="full-prompt-toggle"
                  @click="showFullPrompt = !showFullPrompt"
                >
                  {{ showFullPrompt ? t('agents.hideFullPrompt') : t('agents.viewFullPrompt') }}
                </button>
                <pre v-if="showFullPrompt" class="full-prompt-preview">{{ selectedProfile?.effectiveSystemPrompt }}</pre>
              </Field>

              <div v-if="formError" class="form-error">{{ formError }}</div>
              <div class="form-btns">
                <Button v-if="!isEditingEssential" variant="primary" size="sm" @click="submitForm">{{ isCreating ? t('agents.create') : t('agents.save') }}</Button>
                <Button variant="secondary" size="sm" @click="cancelForm">{{ t('agents.cancel') }}</Button>
                <Button v-if="isEditingCustom" variant="danger-soft" size="sm" @click="onDeleteProfile(selectedName!)">{{ t('agents.delete') }}</Button>
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
  flex: none;
}
.list-row.disabled .list-row-name { color: var(--color-text-faint); }
.list-row-badges {
  display: flex;
  gap: 4px;
  flex-wrap: nowrap;
  min-width: 0;
}
.list-row-toggle { flex: none; margin-left: auto; }
.lock-icon { flex: none; margin-left: auto; color: var(--color-text-faint); }

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
