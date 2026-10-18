<!-- apps/mirri-web/src/components/settings/SkillsPanel.vue -->
<!-- Skills management panel (B8). Renders inside the Settings dialog's
     "skills" tab. Self-fetching (McpServersPanel mode): the skill list does
     not ride the App-level props chain — it reads the workspace catalog via
     the API directly. Per-skill enablement and SKILL.md viewing are v3-only
     (backend-gated); the extra-dirs editor works on every backend.
     Like the MCP/profiles tabs, this section is v-if-mounted: unsaved editor
     rows and the search query are dropped on tab switch (established
     settings-panel precedent). -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppConfig, AppSkill } from '../../api/types';
import { getMirriWebApi } from '../../api';
import { useMirriWebClient } from '../../composables/useMirriWebClient';
import { filterSkills, normalizeExtraDirs, SKILL_SOURCE_ORDER } from '../../lib/skillsManagement';
import Badge from '../ui/Badge.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import Dialog from '../ui/Dialog.vue';
import EmptyState from '../ui/EmptyState.vue';
import Icon from '../ui/Icon.vue';
import Input from '../ui/Input.vue';
import Spinner from '../ui/Spinner.vue';
import Switch from '../ui/Switch.vue';
import ToolbarSearch from '../ui/ToolbarSearch.vue';

const props = defineProps<{
  /** Global daemon config — the extra-dirs editor reads `extraSkillDirs`. */
  config?: AppConfig | null;
  /** True while POST /api/v1/config is saving (disables the dirs save button). */
  configSaving?: boolean;
}>();

const emit = defineEmits<{
  updateConfig: [patch: Partial<AppConfig>];
}>();

const { t } = useI18n();
const api = getMirriWebApi();
const { backend, activeSessionId, activeWorkspaceId, refreshSkills } = useMirriWebClient();

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const skills = ref<AppSkill[]>([]);
const loading = ref(false);
const error = ref('');
const searchQuery = ref('');
const togglingName = ref<string | null>(null);
const activatingName = ref<string | null>(null);

// SKILL.md viewer (v3-only read face)
const viewingOpen = ref(false);
const viewingName = ref<string | null>(null);
const viewingContent = ref('');
const contentLoading = ref(false);

// Per-skill enablement + content viewing ride the v3 rust engine's
// management RPCs (B8 D-6); older backends get an honest hint instead.
const isV3 = computed(() => backend.value === 'v3');
const filtered = computed(() => filterSkills(skills.value, searchQuery.value));

// -------------------------------------------------------------------------
// Load
// -------------------------------------------------------------------------
async function load(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const wid = activeWorkspaceId.value;
    const sid = activeSessionId.value;
    if (wid) {
      skills.value = await api.listSkillsForWorkspace(wid);
    } else if (sid) {
      skills.value = await api.listSkills(sid);
    } else {
      skills.value = [];
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

// Reload when the effective skill source changes: the active workspace, or —
// before any workspace exists — the session fallback. A workspace switch
// means a different catalog; the fallback source changes with the session.
// The backend rides along so the dev backend switcher (v2 ↔ v3) refetches
// and the v3 gating re-evaluates against fresh rows.
watch(
  () => `${backend.value}:${activeWorkspaceId.value ?? `session:${activeSessionId.value}`}`,
  () => { void load(); },
  { immediate: true },
);

// -------------------------------------------------------------------------
// Source badges
// -------------------------------------------------------------------------
const SOURCE_BADGE_VARIANT: Record<string, 'info' | 'neutral' | 'success' | 'solid'> = {
  project: 'info',
  user: 'neutral',
  extra: 'success',
  builtin: 'solid',
};

/** Known wire sources get their i18n label; unknown ones render verbatim so
 *  new engine scopes stay visible instead of being swallowed. */
function sourceLabel(source: string): string {
  if (!(SKILL_SOURCE_ORDER as readonly string[]).includes(source)) return source;
  const key = `skillsSource${source.charAt(0).toUpperCase()}${source.slice(1)}`;
  return t(`settings.${key}`);
}

// -------------------------------------------------------------------------
// Row actions
// -------------------------------------------------------------------------
async function onToggle(skill: AppSkill, enabled: boolean): Promise<void> {
  togglingName.value = skill.name;
  error.value = '';
  try {
    const result = await api.setSkillEnabled(skill.name, enabled);
    if (!result.registered) {
      // Unknown name: the server persisted nothing — restore the wire truth
      // instead of flipping the row locally. Set the message AFTER the
      // reload: load() clears the error line on entry.
      await load();
      error.value = t('settings.skillsNotRegistered', { name: skill.name });
      return;
    }
    // Local row update + slash-menu data-source refresh (the composer's `/`
    // list derives from the same caches). The refresh is best-effort: a
    // cache-reload failure must not surface as a failed toggle — the write
    // itself already succeeded.
    skill.enabled = enabled;
    await refreshSkills().catch(() => undefined);
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    togglingName.value = null;
  }
}

async function onView(skill: AppSkill): Promise<void> {
  viewingName.value = skill.name;
  viewingContent.value = '';
  viewingOpen.value = true;
  contentLoading.value = true;
  error.value = '';
  try {
    const result = await api.readSkillContent(skill.name, activeSessionId.value || undefined);
    viewingContent.value = result.content;
  } catch (e) {
    viewingOpen.value = false;
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    contentLoading.value = false;
  }
}

async function onActivate(skill: AppSkill): Promise<void> {
  const sid = activeSessionId.value;
  if (!sid) return;
  activatingName.value = skill.name;
  error.value = '';
  try {
    await api.activateSkill(sid, skill.name);
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    activatingName.value = null;
  }
}

// -------------------------------------------------------------------------
// Extra skill directories editor (v2/v3)
// -------------------------------------------------------------------------
const extraDirRows = ref<string[]>([]);
// The value this panel last saved — distinguishes our own save round-trip
// (skip re-seeding; the editor keeps any in-progress edits) from an external
// config change (re-seed: external truth wins).
const lastSavedDirs = ref<string[] | null>(null);

function dirsEqual(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

// The committed dirs changing (our own save or an external edit) also means
// the backend resynced its catalog — refresh the list without a restart.
watch(
  () => props.config?.extraSkillDirs,
  (dirs, prev) => {
    const committed = dirs ?? [];
    if (lastSavedDirs.value === null || !dirsEqual(committed, lastSavedDirs.value)) {
      extraDirRows.value = [...committed];
    }
    if (prev !== undefined && !dirsEqual(committed, prev ?? [])) {
      void load();
    }
  },
  { immediate: true },
);

function addDir(): void {
  extraDirRows.value.push('');
}

function removeDir(index: number): void {
  extraDirRows.value.splice(index, 1);
}

function saveDirs(): void {
  const dirs = normalizeExtraDirs(extraDirRows.value);
  // Collapse the visible rows to the normalized value too — the editor must
  // not keep ghost rows (empties/duplicates) that were NOT persisted.
  extraDirRows.value = dirs;
  lastSavedDirs.value = dirs;
  emit('updateConfig', { extraSkillDirs: dirs });
}
</script>

<template>
  <div class="skills-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker"><Icon name="skill" size="sm" /> {{ t('settings.tabs.skills') }}</div>
      <h4 class="panel-title">{{ t('settings.skillsTitle') }}</h4>
      <p class="panel-desc">{{ t('settings.skillsDesc') }}</p>
    </div>

    <!-- Toolbar -->
    <div class="toolbar">
      <ToolbarSearch
        v-model="searchQuery"
        :placeholder="t('settings.skillsSearch')"
        :aria-label="t('settings.skillsSearch')"
      />
      <Button variant="secondary" size="sm" @click="load">{{ t('settings.skillsRefresh') }}</Button>
    </div>

    <div v-if="error" class="error-msg">{{ error }}</div>
    <Banner v-if="!isV3" variant="info">{{ t('settings.skillsV3Only') }}</Banner>

    <EmptyState v-if="loading">
      <Spinner />
    </EmptyState>

    <EmptyState
      v-else-if="filtered.length === 0 && searchQuery.trim() !== ''"
      :title="t('settings.skillsNoMatch', { query: searchQuery.trim() })"
    />

    <EmptyState
      v-else-if="filtered.length === 0"
      :title="t('settings.skillsEmpty')"
      :hint="t('settings.skillsEmptyHint')"
    />

    <div v-else class="skill-list">
      <div v-for="s in filtered" :key="`${s.source}:${s.name}`" class="skill-card">
        <div class="skill-info">
          <div class="skill-name-row">
            <Icon name="skill" size="sm" />
            <span class="skill-name" :title="s.name">{{ s.name }}</span>
            <Badge :variant="SOURCE_BADGE_VARIANT[s.source] ?? 'neutral'" size="sm">{{ sourceLabel(s.source) }}</Badge>
            <Badge v-if="s.enabled === false" variant="warning" size="sm">{{ t('settings.skillsDisabled') }}</Badge>
          </div>
          <div class="skill-desc" :title="s.description">{{ s.description }}</div>
          <div v-if="s.path" class="skill-path" :title="s.path">{{ s.path }}</div>
        </div>
        <div class="skill-actions">
          <Button v-if="isV3" variant="secondary" size="sm" @click="onView(s)">{{ t('settings.skillsView') }}</Button>
          <Button
            variant="secondary"
            size="sm"
            :disabled="!activeSessionId || s.enabled === false || activatingName === s.name"
            :title="!activeSessionId ? t('settings.skillsActivateNoSession') : undefined"
            @click="onActivate(s)"
          >
            {{ activatingName === s.name ? '...' : t('settings.skillsActivate') }}
          </Button>
          <Switch
            v-if="isV3"
            :model-value="s.enabled !== false"
            :disabled="togglingName === s.name"
            :label="s.enabled === false ? t('settings.skillsToggleLabel') : t('settings.skillsDisableToggleLabel')"
            :title="s.enabled === false ? t('settings.skillsDisabled') : t('settings.skillsEnabledState')"
            @update:model-value="(val: boolean) => onToggle(s, val)"
          />
        </div>
      </div>
    </div>

    <!-- Extra skill directories -->
    <section class="extra-dirs">
      <h4 class="section-title">{{ t('settings.skillsExtraDirs') }}</h4>
      <p class="section-hint">{{ t('settings.skillsExtraDirsHint') }}</p>
      <div class="dir-editor">
        <div v-for="(_, i) in extraDirRows" :key="i" class="dir-row">
          <Input v-model="extraDirRows[i]" class="dir-input" placeholder="./extra-skills" />
          <Button variant="ghost" size="sm" :aria-label="t('common.delete')" @click="removeDir(i)"><Icon name="close" size="sm" /></Button>
        </div>
        <div class="dir-actions">
          <Button variant="secondary" size="sm" @click="addDir">+ {{ t('settings.skillsAddDir') }}</Button>
          <Button variant="primary" size="sm" :disabled="configSaving" @click="saveDirs">
            {{ configSaving ? t('settings.saving') : t('common.save') }}
          </Button>
        </div>
      </div>
    </section>

    <!-- SKILL.md viewer -->
    <Dialog
      :open="viewingOpen"
      :title="viewingName ? `${viewingName} · ${t('settings.skillsContentTitle')}` : t('settings.skillsContentTitle')"
      size="lg"
      @update:open="viewingOpen = $event"
    >
      <div class="content-wrap">
        <div v-if="contentLoading" class="content-loading"><Spinner /></div>
        <div v-else-if="viewingContent === ''" class="content-empty">{{ t('settings.skillsEmptyFile') }}</div>
        <pre v-else class="skill-md">{{ viewingContent }}</pre>
      </div>
    </Dialog>
  </div>
</template>

<style scoped>
.skills-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

.toolbar { display: flex; align-items: center; gap: var(--space-3); flex-wrap: wrap; }

.error-msg { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-danger-soft); color: var(--color-danger); font-size: var(--text-sm); }

.skill-list { display: flex; flex-direction: column; gap: var(--space-2); }
.skill-card { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: var(--space-3); align-items: center; padding: var(--space-3) var(--space-4); border: 1px solid var(--color-line); border-radius: var(--radius-md); background: var(--color-surface-raised); }
.skill-card:hover { border-color: var(--color-line-strong); }
.skill-info { min-width: 0; display: flex; flex-direction: column; gap: var(--space-1); }
.skill-name-row { display: flex; align-items: center; gap: var(--space-2); flex-wrap: wrap; }
.skill-name { font-family: var(--font-mono); font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.skill-desc { font-size: var(--text-sm); color: var(--color-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.skill-path { font-family: var(--font-mono); font-size: var(--text-xs); color: var(--color-text-faint); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.skill-actions { display: flex; gap: var(--space-2); flex: none; align-items: center; flex-wrap: wrap; justify-content: flex-end; }

.extra-dirs { margin-top: var(--space-4); border-top: 1px solid var(--color-line); padding-top: var(--space-3); }
.section-title { margin: 0 0 var(--space-1); font-family: var(--font-ui); font-size: var(--text-xs); font-weight: var(--weight-medium); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-muted); }
.section-hint { margin: 0 0 var(--space-3); font-family: var(--font-ui); font-size: var(--text-sm); color: var(--color-text-faint); }

.dir-editor { display: flex; flex-direction: column; gap: var(--space-2); }
.dir-row { display: flex; gap: var(--space-2); align-items: center; }
.dir-input { flex: 1; font-family: var(--font-mono); font-size: var(--text-xs); }
.dir-actions { display: flex; gap: var(--space-2); }

.content-wrap { max-height: 60vh; overflow-y: auto; }
.content-loading { display: flex; align-items: center; justify-content: center; padding: var(--space-6); }
.content-empty { padding: var(--space-4); color: var(--color-text-faint); font-size: var(--text-sm); font-family: var(--font-mono); }
.skill-md { margin: 0; font-family: var(--font-mono); font-size: var(--text-xs); line-height: var(--leading-normal); color: var(--color-text); white-space: pre-wrap; word-break: break-word; }

@media (max-width: 640px) {
  .skill-card { grid-template-columns: 1fr; }
  .skill-actions { justify-content: flex-start; }
  .dir-row { flex-direction: column; align-items: stretch; }
}
</style>
