<!-- apps/mirri-web/src/components/settings/AgentManager.vue -->
<!-- Modal overlay for managing agent profiles: list, add, edit, delete, enable/disable. -->
<script setup lang="ts">
import { onMounted, onUnmounted, reactive, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppAgentProfile } from '../../api/types';
import { useDialogFocus } from '../../composables/useDialogFocus';
import Dialog from '../ui/Dialog.vue';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Spinner from '../ui/Spinner.vue';
import Field from '../ui/Field.vue';
import Input from '../ui/Input.vue';
import Select from '../ui/Select.vue';
import Switch from '../ui/Switch.vue';
import Icon from '../ui/Icon.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();

const dialogRef = ref<HTMLElement | null>(null);
useDialogFocus(dialogRef);

const props = defineProps<{
  profiles: AppAgentProfile[];
  loading?: boolean;
  unavailable?: boolean;
}>();

const emit = defineEmits<{
  create: [input: { name: string; description?: string; extends?: string; defaultModel?: string; tools?: string[]; whenToUse?: string; systemPromptTemplate?: string }];
  update: [name: string, data: Partial<{ description: string; extends: string; defaultModel: string; tools: string[]; whenToUse: string; systemPromptTemplate: string }>];
  delete: [name: string];
  enable: [name: string];
  disable: [name: string];
  close: [];
}>();

// -------------------------------------------------------------------------
// Delete confirmation
// -------------------------------------------------------------------------

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

// -------------------------------------------------------------------------
// Add/edit form
// -------------------------------------------------------------------------

const showForm = ref(false);
const editingName = ref<string | null>(null);
const form = reactive({
  name: '',
  description: '',
  extends: '',
  defaultModel: '',
  tools: '',
  whenToUse: '',
  systemPromptTemplate: '',
});
const formError = ref('');

function openAdd(): void {
  editingName.value = null;
  form.name = '';
  form.description = '';
  form.extends = 'agent';
  form.defaultModel = '';
  form.tools = '';
  form.whenToUse = '';
  form.systemPromptTemplate = '';
  formError.value = '';
  showForm.value = true;
}

function openEdit(profile: AppAgentProfile): void {
  if (profile.builtin) return;
  editingName.value = profile.name;
  form.name = profile.name;
  form.description = profile.description ?? '';
  form.extends = profile.extends ?? 'agent';
  form.defaultModel = profile.defaultModel ?? '';
  form.tools = profile.tools?.join(', ') ?? '';
  form.whenToUse = profile.whenToUse ?? '';
  form.systemPromptTemplate = profile.systemPromptTemplate ?? '';
  formError.value = '';
  showForm.value = true;
}

function cancelForm(): void {
  showForm.value = false;
  editingName.value = null;
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

  if (editingName.value !== null) {
    emit('update', editingName.value, {
      description: form.description.trim() || undefined,
      extends: form.extends.trim() || undefined,
      defaultModel: form.defaultModel.trim() || undefined,
      tools,
      whenToUse: form.whenToUse.trim() || undefined,
      systemPromptTemplate: form.systemPromptTemplate.trim() || undefined,
    });
  } else {
    emit('create', {
      name: form.name.trim(),
      description: form.description.trim() || undefined,
      extends: form.extends.trim() || undefined,
      defaultModel: form.defaultModel.trim() || undefined,
      tools,
      whenToUse: form.whenToUse.trim() || undefined,
      systemPromptTemplate: form.systemPromptTemplate.trim() || undefined,
    });
  }
  showForm.value = false;
  editingName.value = null;
}

// -------------------------------------------------------------------------
// Keyboard — Esc closes
// -------------------------------------------------------------------------

function handleKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape') {
    if (showForm.value) { cancelForm(); return; }
    emit('close');
  }
}

onMounted(() => document.addEventListener('keydown', handleKeydown));
onUnmounted(() => document.removeEventListener('keydown', handleKeydown));

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

function toggleProfile(profile: AppAgentProfile): void {
  if (profile.essential) return;
  if (profile.enabled) {
    emit('disable', profile.name);
  } else {
    emit('enable', profile.name);
  }
}
</script>

<template>
  <Dialog :open="true" :close-on-esc="false" :title="t('agents.title')" size="xl" height="fixed" @close="emit('close')">
    <div ref="dialogRef" class="am">
      <!-- Profile list -->
      <div class="profile-list">
        <!-- Loading state -->
        <div v-if="loading" class="state-row">
          <Spinner size="sm" />
          <span>{{ t('agents.loading') }}</span>
        </div>
        <!-- Unavailable -->
        <div v-else-if="unavailable" class="state-row unavail">
          <Icon name="alert-triangle" size="md" />
          <span>Agent profile management is not available</span>
        </div>
        <!-- Profile rows -->
        <template v-else>
          <div v-for="p in profiles" :key="p.name" class="profile-row">
            <!-- Enable/disable toggle -->
            <Switch
              :model-value="p.enabled"
              :disabled="p.essential"
              :label="p.enabled ? t('agents.enabled') : t('agents.disabled')"
              @update:model-value="toggleProfile(p)"
            />
            <!-- Info -->
            <div class="profile-info">
              <div class="profile-header">
                <span class="profile-name">{{ p.name }}</span>
                <Badge :variant="p.builtin ? 'neutral' : 'success'" size="sm">
                  {{ p.builtin ? t('agents.source.builtin') : t('agents.custom') }}
                </Badge>
                <Badge v-if="p.essential" variant="warning" size="sm">{{ t('agents.essential') }}</Badge>
              </div>
              <span v-if="p.description" class="profile-desc">{{ p.description }}</span>
              <div class="profile-meta">
                <span v-if="p.defaultModel" class="meta-item">{{ t('agents.defaultModel') }}: {{ p.defaultModel }}</span>
                <span v-if="p.extends" class="meta-item">{{ t('agents.extends') }}: {{ p.extends }}</span>
              </div>
            </div>
            <!-- Actions -->
            <div class="profile-actions">
              <Button v-if="!p.builtin" variant="secondary" size="sm" @click="openEdit(p)">{{ t('agents.edit') }}</Button>
              <Button v-if="!p.builtin" variant="danger-soft" size="sm" @click="onDeleteProfile(p.name)">{{ t('agents.delete') }}</Button>
            </div>
          </div>
        </template>
      </div>

      <!-- Add/edit form -->
      <div class="form-section">
        <template v-if="!showForm">
          <Button variant="primary" size="sm" @click="openAdd">
            <Icon name="plus" size="sm" />
            {{ t('agents.add') }}
          </Button>
        </template>
        <template v-else>
          <div class="add-form">
            <Field v-if="editingName === null" :label="t('agents.add')">
              <Input v-model="form.name" :placeholder="t('agents.namePlaceholder')" spellcheck="false" />
            </Field>
            <Field :label="t('agents.description')">
              <Input v-model="form.description" :placeholder="t('agents.descriptionPlaceholder')" spellcheck="false" />
            </Field>
            <Field :label="t('agents.extends')">
              <Select v-model="form.extends">
                <option value="agent">agent</option>
                <option value="coder">coder</option>
                <option value="explore">explore</option>
                <option value="plan">plan</option>
              </Select>
            </Field>
            <Field :label="t('agents.defaultModel')">
              <Input v-model="form.defaultModel" :placeholder="t('agents.defaultModelPlaceholder')" spellcheck="false" />
            </Field>
            <Field :label="t('agents.tools')">
              <Input v-model="form.tools" :placeholder="t('agents.toolsPlaceholder')" spellcheck="false" />
            </Field>
            <Field :label="t('agents.whenToUse')">
              <Input v-model="form.whenToUse" :placeholder="t('agents.whenToUsePlaceholder')" spellcheck="false" />
            </Field>
            <div v-if="formError" class="form-error">{{ formError }}</div>
            <div class="form-btns">
              <Button variant="primary" size="sm" @click="submitForm">{{ editingName !== null ? t('agents.save') : t('agents.create') }}</Button>
              <Button variant="secondary" size="sm" @click="cancelForm">{{ t('agents.cancel') }}</Button>
            </div>
          </div>
        </template>
      </div>

      <!-- Footer -->
      <div class="footer-hint">{{ t('agents.escClose') }}</div>
    </div>
  </Dialog>
</template>

<style scoped>
.am { display: flex; flex-direction: column; gap: var(--space-4); }

.profile-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.state-row {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-4) 0;
  color: var(--color-text-muted);
  font-family: var(--font-ui);
  font-size: var(--text-base);
}
.state-row.unavail { color: var(--color-warning); }

.profile-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) 0;
  border-bottom: 1px solid var(--color-line);
  transition: background var(--duration-fast) var(--ease-out);
}
.profile-row:last-child { border-bottom: none; }

.profile-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.profile-header {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.profile-name {
  font-family: var(--font-ui);
  font-size: var(--text-base);
  font-weight: var(--weight-medium);
  color: var(--color-text);
}
.profile-desc {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text-muted);
}
.profile-meta {
  display: flex;
  gap: var(--space-3);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
}
.meta-item { white-space: nowrap; }

.profile-actions {
  display: flex;
  gap: var(--space-2);
  flex: none;
  align-items: center;
  flex-wrap: wrap;
}

.form-section {
  border-top: 1px solid var(--color-line);
  padding-top: var(--space-4);
}
.add-form { display: flex; flex-direction: column; gap: var(--space-3); }
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

.footer-hint {
  padding-top: var(--space-2);
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
  border-top: 1px solid var(--color-line);
}

@media (max-width: 640px) {
  .profile-row {
    align-items: flex-start;
    flex-wrap: wrap;
  }
  .profile-actions {
    flex: 1 1 100%;
    justify-content: flex-end;
  }
}
</style>
