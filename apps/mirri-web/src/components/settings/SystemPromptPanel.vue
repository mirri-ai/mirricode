<!-- apps/mirri-web/src/components/settings/SystemPromptPanel.vue -->
<!-- Standalone shared-system-prompt settings tab (US-A8, D20/D21): ONE
     merged content area occupying the tab's height with a 预览/编辑 view
     toggle — preview renders the effective shared base prompt as
     markdown, edit is a monospace textarea seeded with the persisted
     override (or the builtin default template's raw text when no
     override exists — an edit then CREATES the override). Also hosts the
     恢复默认 reset flow. Self-fetches the essential `agent` row through
     the daemon API (the CronPanel / PermissionRulesPanel pattern) — no
     parent props needed. -->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppAgentProfile } from '../../api/types';
import { getMirriWebApi } from '../../api';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import SegmentedControl from '../ui/SegmentedControl.vue';
import Spinner from '../ui/Spinner.vue';
import Textarea from '../ui/Textarea.vue';
import Markdown from '../chat/Markdown.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();

/** The essential default `agent` row — the shared base's wire face. */
const defaultRow = ref<AppAgentProfile | null>(null);
const loading = ref(false);
const saving = ref(false);
const error = ref<string | null>(null);
/** The merged area's active view: preview (rendered effective prompt) or
 *  edit (the textarea). The toggle sits at the content area's top-right. */
const view = ref<'preview' | 'edit'>('preview');
const viewOptions = computed(() => [
  { value: 'preview', label: t('settings.systemPrompt.viewPreview') },
  { value: 'edit', label: t('settings.systemPrompt.viewEdit') },
]);
/** The editor's working copy of the SYSTEM.md body (raw text). */
const draft = ref('');

const persistedBody = computed(() => defaultRow.value?.basePromptTemplate ?? '');
const overrideActive = computed(() => defaultRow.value?.basePromptOverride === true);
const effectivePrompt = computed(() => defaultRow.value?.effectiveSystemPrompt ?? '');
/** The builtin default template's raw text (placeholders intact) — the
 *  edit-mode seed when no override exists. Empty string only on engines
 *  predating the field (the seed falls back to an empty editor). */
const defaultTemplate = computed(() => defaultRow.value?.basePromptDefaultTemplate ?? '');

/** The baseline the dirty-check compares the draft against: the persisted
 *  override when one exists, otherwise the seeded default template — so
 *  with no override, an untouched default seed is a no-op (saving would
 *  create an override identical to the default, pinning the prompt and
 *  losing the default's product updates for no user value). */
const draftBaseline = computed(() =>
  overrideActive.value ? persistedBody.value : defaultTemplate.value,
);

/** Whether the user edited the draft away from its baseline. Trim-normalized:
 *  the server trims the body (a whitespace-only body is the reset), so the
 *  gate compares trimmed text — a whitespace-only draft over an empty
 *  original counts as no-change, and the save button can never fire a
 *  request that would reset without the user's confirmation. */
const dirty = computed(() => draft.value.trim() !== draftBaseline.value.trim());

async function refresh(): Promise<void> {
  loading.value = true;
  error.value = null;
  try {
    const row = await api.getAgent('agent');
    defaultRow.value = row ?? null;
    // Reset the draft to the persisted state — a refresh after save/reset
    // re-seeds the editor with what is now on disk: the override when one
    // exists, otherwise the builtin default template (the edit seed).
    draft.value = row?.basePromptTemplate ?? row?.basePromptDefaultTemplate ?? '';
  } catch (err) {
    error.value = String(err instanceof Error ? err.message : err);
    defaultRow.value = null;
  } finally {
    loading.value = false;
  }
}

onMounted(refresh);

async function onSave(): Promise<void> {
  if (!dirty.value || saving.value) return;
  saving.value = true;
  error.value = null;
  try {
    await api.updateAgent('agent', { basePromptTemplate: draft.value });
    await refresh();
  } catch (err) {
    error.value = String(err instanceof Error ? err.message : err);
  } finally {
    saving.value = false;
  }
}

/** 恢复默认 (US-A8): remove the SYSTEM.md override with a danger confirm
 *  whose message states the scope — the custom system prompt is removed,
 *  every role returns to the default base, effective on next assembly. */
async function onReset(): Promise<void> {
  if (!overrideActive.value || saving.value) return;
  const ok = await confirm({
    title: t('settings.systemPrompt.resetConfirmTitle'),
    message: t('settings.systemPrompt.resetConfirmMessage'),
    variant: 'danger',
  });
  if (!ok) return;
  saving.value = true;
  error.value = null;
  try {
    await api.updateAgent('agent', { basePromptTemplate: '' });
    await refresh();
  } catch (err) {
    error.value = String(err instanceof Error ? err.message : err);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="system-prompt-panel">
    <header class="panel-head">
      <div class="head-text">
        <h3 class="panel-title">{{ t('settings.systemPrompt.title') }}</h3>
        <p class="panel-sub">{{ t('settings.systemPrompt.subtitle') }}</p>
      </div>
      <Badge v-if="overrideActive" variant="warning" dot>
        {{ t('settings.systemPrompt.overrideActive') }}
      </Badge>
      <Badge v-else variant="info">
        {{ t('settings.systemPrompt.usingDefault') }}
      </Badge>
    </header>

    <Banner v-if="error" variant="danger" :message="error" />

    <div v-if="loading && !defaultRow" class="loading-row">
      <Spinner size="sm" />
      <span>{{ t('settings.systemPrompt.loading') }}</span>
    </div>

    <template v-else-if="defaultRow">
      <!-- The merged content area: preview/edit share this one region and
           stretch to fill the tab's remaining height. -->
      <section class="content">
        <div class="content-bar">
          <div class="hints">
            <p class="sec-hint">{{ t('settings.systemPrompt.editorHint') }}</p>
            <p class="sec-hint">{{ t('settings.systemPrompt.basePromptHint') }}</p>
          </div>
          <SegmentedControl
            v-model="view"
            :options="viewOptions"
            size="sm"
            class="view-toggle"
          />
        </div>

        <div v-if="view === 'preview'" class="preview-pane">
          <span class="visually-hidden">{{ t('settings.systemPrompt.effectiveTitle') }}</span>
          <div class="preview-scroll">
            <Markdown :text="effectivePrompt" class="preview-body" />
          </div>
        </div>

        <Textarea
          v-else
          v-model="draft"
          :placeholder="t('settings.systemPrompt.editorPlaceholder')"
          :disabled="saving"
          class="editor"
        />

        <div class="actions">
          <Button variant="primary" size="sm" :disabled="!dirty || saving" @click="onSave">
            {{ t('settings.systemPrompt.save') }}
          </Button>
          <Button
            variant="danger-soft"
            size="sm"
            :disabled="!overrideActive || saving"
            :title="overrideActive ? undefined : t('settings.systemPrompt.resetDisabledHint')"
            @click="onReset"
          >
            {{ t('settings.systemPrompt.resetToDefault') }}
          </Button>
        </div>
      </section>
    </template>

    <Banner
      v-else-if="!loading"
      variant="warning"
      :message="t('settings.systemPrompt.unavailable')"
    />
  </div>
</template>

<style scoped>
.system-prompt-panel {
  display: flex;
  flex-direction: column;
  gap: 16px;
  min-height: 0;
  flex: 1;
  padding: 4px 2px;
}
.panel-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}
.panel-title {
  margin: 0;
  font-size: var(--text-lg);
  font-weight: var(--weight-semibold);
}
.panel-sub {
  margin: 4px 0 0;
  font-size: var(--text-sm);
  color: var(--color-text-muted);
}
.loading-row {
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--color-text-muted);
  font-size: var(--text-sm);
  padding: 12px 0;
}
.content {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-height: 0;
  flex: 1;
}
.content-bar {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}
.hints {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.sec-hint {
  margin: 0;
  font-size: var(--text-xs);
  color: var(--color-text-muted);
}
.view-toggle {
  flex-shrink: 0;
}
.preview-pane {
  display: flex;
  flex-direction: column;
  min-height: 0;
  flex: 1;
}
.preview-scroll {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
}
.preview-body {
  font-size: var(--text-sm);
  line-height: 1.6;
}
.editor {
  flex: 1;
  min-height: 0;
}
.editor :deep(.ui-textarea) {
  height: 100%;
  min-height: 0;
  resize: none;
  font-family: var(--font-mono, monospace);
  font-size: 12px;
}
.actions {
  display: flex;
  gap: 8px;
}
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
