<!-- apps/mirri-web/src/components/settings/McpServersPanel.vue -->
<!-- MCP server management panel. Renders inside the Settings dialog's
     "mcp" tab. List view + form/JSON dual-mode editor. -->
<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppMcpServer, AppMcpServerConfig, AppToolDescriptor } from '../../api/types';
import { getMirriWebApi } from '../../api';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Spinner from '../ui/Spinner.vue';
import Field from '../ui/Field.vue';
import Input from '../ui/Input.vue';
import Select from '../ui/Select.vue';
import Switch from '../ui/Switch.vue';
import Textarea from '../ui/Textarea.vue';
import SegmentedControl from '../ui/SegmentedControl.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';

const { t } = useI18n();
const { confirm } = useConfirmDialog();
const api = getMirriWebApi();

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------
const servers = ref<AppMcpServer[]>([]);
const loading = ref(false);
const error = ref('');
const searchQuery = ref('');
const editMode = ref<'form' | 'json'>('form');
const editingName = ref<string | null>(null);
const isCreating = ref(false);

// Runtime toggle state
const disabledServers = ref<string[]>([]);
const disabledTools = ref<string[]>([]);
const togglingServer = ref<string | null>(null);
const togglingTool = ref<string | null>(null);
const serverTools = ref<AppToolDescriptor[]>([]);

interface McpFormState {
  name: string;
  transport: 'stdio' | 'http' | 'sse';
  command: string;
  args: string;       // space-separated, converted to string[] on save
  env: string;         // JSON string
  cwd: string;
  url: string;
  headers: string;     // JSON string
  enabled: boolean;
  enabledTools: string;   // comma-separated
  disabledTools: string;  // comma-separated
  startupTimeoutMs: number;
  toolTimeoutMs: number;
  bearerTokenEnvVar: string;
}

const form = reactive<McpFormState>({
  name: '',
  transport: 'stdio',
  command: '',
  args: '',
  env: '{}',
  cwd: '',
  url: '',
  headers: '{}',
  enabled: true,
  enabledTools: '',
  disabledTools: '',
  startupTimeoutMs: 30000,
  toolTimeoutMs: 60000,
  bearerTokenEnvVar: '',
});
const jsonText = ref('{}');
const formError = ref('');
const saving = ref(false);

// -------------------------------------------------------------------------
// Load
// -------------------------------------------------------------------------
async function loadServers(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    servers.value = await api.listGlobalMcpServers();
  } catch (e) {
    error.value = String(e);
  } finally {
    loading.value = false;
  }
}

async function loadToggleState(): Promise<void> {
  try {
    const state = await api.getMcpToggleState();
    disabledServers.value = state.disabledServers;
    disabledTools.value = state.disabledTools;
  } catch {
    // Non-critical — toggle UI defaults to "enabled"
  }
}

async function loadServerTools(serverName: string): Promise<void> {
  try {
    const allTools = await api.listGlobalMcpTools();
    serverTools.value = allTools.filter((t) => t.mcpServerId === serverName);
  } catch {
    serverTools.value = [];
  }
}

onMounted(() => {
  void loadServers();
  void loadToggleState();
});

// -------------------------------------------------------------------------
// Computed
// -------------------------------------------------------------------------
const filteredServers = computed(() => {
  const q = searchQuery.value.trim().toLowerCase();
  if (!q) return servers.value;
  return servers.value.filter((s) => s.name.toLowerCase().includes(q));
});

const connectedCount = computed(() => servers.value.filter((s) => s.status === 'connected').length);
const totalCount = computed(() => servers.value.length);

const showForm = computed(() => isCreating.value || editingName.value !== null);

// -------------------------------------------------------------------------
// Form helpers
// -------------------------------------------------------------------------
function resetForm(): void {
  form.name = '';
  form.transport = 'stdio';
  form.command = '';
  form.args = '';
  form.env = '{}';
  form.cwd = '';
  form.url = '';
  form.headers = '{}';
  form.enabled = true;
  form.enabledTools = '';
  form.disabledTools = '';
  form.startupTimeoutMs = 30000;
  form.toolTimeoutMs = 60000;
  form.bearerTokenEnvVar = '';
  jsonText.value = '{}';
  formError.value = '';
}

function startCreate(): void {
  editingName.value = null;
  isCreating.value = true;
  editMode.value = 'form';
  resetForm();
}

function startEdit(server: AppMcpServer): void {
  editingName.value = server.name;
  isCreating.value = false;
  editMode.value = 'form';
  resetForm();
  // Populate form with existing config — we need to fetch the config
  // from the JSON file directly. Since listAll doesn't return config,
  // we read it from the server via a separate call.
  // For now, start with an empty form since the list endpoint doesn't
  // return the full config. The user can edit and save.
  form.name = server.name;
  form.transport = server.transport;
  form.enabled = server.status !== 'disconnected';
  void loadServerTools(server.name);
}

function cancelEdit(): void {
  editingName.value = null;
  isCreating.value = false;
  formError.value = '';
}

// -------------------------------------------------------------------------
// Runtime toggle handlers
// -------------------------------------------------------------------------
async function toggleServer(server: AppMcpServer, enabled: boolean): Promise<void> {
  togglingServer.value = server.name;
  try {
    if (enabled) {
      await api.enableMcpServer(server.name);
      disabledServers.value = disabledServers.value.filter((s) => s !== server.name);
    } else {
      await api.disableMcpServer(server.name);
      if (!disabledServers.value.includes(server.name)) {
        disabledServers.value.push(server.name);
      }
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    togglingServer.value = null;
  }
}

async function toggleTool(toolName: string, enabled: boolean): Promise<void> {
  togglingTool.value = toolName;
  try {
    if (enabled) {
      await api.enableMcpTool(toolName);
      disabledTools.value = disabledTools.value.filter((t) => t !== toolName);
    } else {
      await api.disableMcpTool(toolName);
      if (!disabledTools.value.includes(toolName)) {
        disabledTools.value.push(toolName);
      }
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    togglingTool.value = null;
  }
}

// -------------------------------------------------------------------------
// Build config from form
// -------------------------------------------------------------------------
function buildConfigFromForm(): AppMcpServerConfig {
  const config: AppMcpServerConfig = {
    transport: form.transport,
    enabled: form.enabled,
  };
  if (form.transport === 'stdio') {
    if (form.command) config.command = form.command;
    if (form.args) {
      config.args = form.args.split(/\s+/).filter(Boolean);
    }
    if (form.cwd) config.cwd = form.cwd;
    if (form.env && form.env !== '{}') {
      try {
        config.env = JSON.parse(form.env);
      } catch {
        throw new Error('Invalid JSON in env field');
      }
    }
  } else {
    if (form.url) config.url = form.url;
    if (form.headers && form.headers !== '{}') {
      try {
        config.headers = JSON.parse(form.headers);
      } catch {
        throw new Error('Invalid JSON in headers field');
      }
    }
    if (form.bearerTokenEnvVar) config.bearerTokenEnvVar = form.bearerTokenEnvVar;
  }
  if (form.enabledTools) {
    config.enabledTools = form.enabledTools.split(',').map((s) => s.trim()).filter(Boolean);
  }
  if (form.disabledTools) {
    config.disabledTools = form.disabledTools.split(',').map((s) => s.trim()).filter(Boolean);
  }
  if (form.startupTimeoutMs) config.startupTimeoutMs = form.startupTimeoutMs;
  if (form.toolTimeoutMs) config.toolTimeoutMs = form.toolTimeoutMs;
  return config;
}

function buildConfigFromJson(): AppMcpServerConfig {
  const parsed = JSON.parse(jsonText.value);
  // Support both `{ config: {...} }` and bare `{...}`
  const config = parsed.config ?? parsed;
  if (!config.transport) throw new Error('Missing "transport" field');
  return config as AppMcpServerConfig;
}

// -------------------------------------------------------------------------
// Save (create / update)
// -------------------------------------------------------------------------
async function save(): Promise<void> {
  saving.value = true;
  formError.value = '';
  try {
    const config = editMode.value === 'form'
      ? buildConfigFromForm()
      : buildConfigFromJson();
    const name = form.name.trim();
    if (!name) throw new Error('Name is required');

    if (editingName.value !== null) {
      await api.updateMcpServer(editingName.value, config);
    } else {
      await api.createMcpServer(name, config);
    }
    await loadServers();
    cancelEdit();
  } catch (e) {
    formError.value = e instanceof Error ? e.message : String(e);
  } finally {
    saving.value = false;
  }
}

// -------------------------------------------------------------------------
// Delete
// -------------------------------------------------------------------------
async function onDelete(name: string): Promise<void> {
  const ok = await confirm({
    title: t('settings.mcpDeleteTitle'),
    message: t('settings.mcpDeleteConfirm', { name }),
    confirmLabel: t('common.delete'),
    variant: 'danger',
  });
  if (!ok) return;
  try {
    await api.deleteMcpServer(name);
    await loadServers();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

// -------------------------------------------------------------------------
// Reload all
// -------------------------------------------------------------------------
async function onReload(): Promise<void> {
  try {
    await api.reloadMcpServers();
    await loadServers();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

// -------------------------------------------------------------------------
// Status helpers
// -------------------------------------------------------------------------
function statusColor(status: AppMcpServer['status']): string {
  switch (status) {
    case 'connected': return 'var(--color-success)';
    case 'connecting': return 'var(--color-warning)';
    case 'error': return 'var(--color-danger)';
    case 'disconnected': return 'var(--color-text-faint)';
  }
}

function commandSummary(server: AppMcpServer): string {
  // The list endpoint doesn't return command details;
  // show transport as a summary.
  return server.transport;
}
</script>

<template>
  <div class="mcp-panel">
    <!-- Header -->
    <div class="panel-head">
      <div class="panel-kicker">{{ t('settings.mcpServers') }}</div>
      <h4 class="panel-title">{{ t('settings.mcpTitle') }}</h4>
      <p class="panel-desc">{{ t('settings.mcpDesc') }}</p>
    </div>

    <!-- List view -->
    <template v-if="!showForm">
      <div class="toolbar">
        <label class="search">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3" /></svg>
          <input v-model="searchQuery" :placeholder="t('settings.mcpSearch')" />
        </label>
        <div class="stats">
          <span>{{ t('settings.mcpConnected', { n: connectedCount, total: totalCount }) }}</span>
        </div>
        <Button variant="secondary" size="sm" @click="onReload">{{ t('settings.mcpReload') }}</Button>
        <Button variant="primary" size="sm" @click="startCreate">+ {{ t('settings.mcpAdd') }}</Button>
      </div>

      <div v-if="error" class="error-msg">{{ error }}</div>

      <div v-if="loading" class="empty">
        <Spinner />
      </div>

      <div v-else-if="filteredServers.length === 0" class="empty">
        {{ t('settings.mcpEmpty') }}
      </div>

      <div v-else class="server-list">
        <div v-for="s in filteredServers" :key="s.id" class="server-card">
          <div class="server-info">
            <span class="status-dot" :style="{ background: statusColor(s.status) }" />
            <span class="server-name">{{ s.name }}</span>
            <Badge variant="neutral" size="sm">{{ commandSummary(s) }}</Badge>
            <span class="tool-count">{{ t('settings.mcpTools', { n: s.toolCount }) }}</span>
          </div>
          <div class="server-actions">
            <Switch
              :model-value="!disabledServers.includes(s.name)"
              :disabled="togglingServer === s.name"
              @update:model-value="(val: boolean) => toggleServer(s, val)"
            />
            <Button variant="secondary" size="sm" @click="startEdit(s)">{{ t('common.edit') }}</Button>
            <Button variant="danger-soft" size="sm" @click="onDelete(s.name)">{{ t('common.delete') }}</Button>
          </div>
        </div>
      </div>
    </template>

    <!-- Edit / Create form -->
    <template v-else>
      <div class="edit-header">
        <div class="edit-title-row">
          <Button variant="ghost" size="sm" class="back-btn" @click="cancelEdit">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6"/></svg>
            {{ t('common.back') }}
          </Button>
          <h4 class="edit-title">
            {{ editingName ? t('settings.mcpEdit', { name: editingName }) : t('settings.mcpAdd') }}
          </h4>
        </div>
        <SegmentedControl
          :model-value="editMode"
          :options="[
            { value: 'form', label: t('settings.mcpFormMode') },
            { value: 'json', label: t('settings.mcpJsonMode') },
          ]"
          @update:model-value="editMode = $event as 'form' | 'json'"
        />
      </div>

      <!-- Form mode -->
      <div v-if="editMode === 'form'" class="form-grid">
        <Field :label="t('settings.mcpName')">
          <Input v-model="form.name" :disabled="editingName !== null" :placeholder="'my-server'" />
        </Field>

        <Field :label="t('settings.mcpTransport')">
          <Select :model-value="form.transport" @update:model-value="form.transport = $event as 'stdio' | 'http' | 'sse'">
            <option value="stdio">stdio</option>
            <option value="http">http</option>
            <option value="sse">sse</option>
          </Select>
        </Field>

        <Field :label="t('settings.mcpEnabled')">
          <Switch :model-value="form.enabled" @update:model-value="form.enabled = $event" />
        </Field>

        <template v-if="form.transport === 'stdio'">
          <Field :label="t('settings.mcpCommand')">
            <Input v-model="form.command" :placeholder="'npx'" />
          </Field>
          <Field :label="t('settings.mcpArgs')">
            <Input v-model="form.args" :placeholder="'-y @modelcontextprotocol/server-filesystem /tmp'" />
          </Field>
          <Field :label="t('settings.mcpCwd')">
            <Input v-model="form.cwd" :placeholder="'/path/to/dir'" />
          </Field>
          <Field :label="t('settings.mcpEnv')">
            <Textarea v-model="form.env" :rows="3" placeholder='Example: {"API_KEY":"xxx"}' class="code-area" />
          </Field>
        </template>

        <template v-else>
          <Field :label="t('settings.mcpUrl')">
            <Input v-model="form.url" :placeholder="'https://mcp.example.com/sse'" />
          </Field>
          <Field :label="t('settings.mcpHeaders')">
            <Textarea v-model="form.headers" :rows="3" placeholder='Example: {"Authorization":"Bearer ..."}' class="code-area" />
          </Field>
          <Field :label="t('settings.mcpBearerEnvVar')">
            <Input v-model="form.bearerTokenEnvVar" :placeholder="'MY_API_KEY'" />
          </Field>
        </template>

        <Field :label="t('settings.mcpEnabledTools')">
          <Input v-model="form.enabledTools" :placeholder="'tool1, tool2'" />
        </Field>
        <Field :label="t('settings.mcpDisabledTools')">
          <Input v-model="form.disabledTools" :placeholder="'tool3, tool4'" />
        </Field>
        <Field :label="t('settings.mcpStartupTimeout')">
          <Input :model-value="form.startupTimeoutMs" type="number" :placeholder="'30000'" @update:model-value="form.startupTimeoutMs = Number($event)" />
        </Field>
        <Field :label="t('settings.mcpToolTimeout')">
          <Input :model-value="form.toolTimeoutMs" type="number" :placeholder="'60000'" @update:model-value="form.toolTimeoutMs = Number($event)" />
        </Field>
      </div>

      <!-- JSON mode -->
      <div v-else class="json-editor">
        <Textarea v-model="jsonText" :rows="16" class="code-area" placeholder='Example: {"transport":"stdio","command":"npx","args":["-y","..."]}' />
      </div>

      <div v-if="formError" class="error-msg">{{ formError }}</div>

      <!-- Tool list with runtime toggles (only when editing) -->
      <div v-if="editingName" class="tool-list-section">
        <div class="section-label">{{ t('settings.mcpToolsList') }}</div>
        <div v-if="serverTools.length === 0" class="tool-empty">
          {{ t('settings.mcpNoTools') }}
        </div>
        <div v-else class="tool-list">
          <div v-for="tool in serverTools" :key="tool.name" class="tool-item">
            <div class="tool-info">
              <span class="tool-name">{{ tool.name }}</span>
              <span class="tool-desc">{{ tool.description }}</span>
            </div>
            <Switch
              :model-value="!disabledTools.includes(tool.name)"
              :disabled="togglingTool === tool.name"
              @update:model-value="(val: boolean) => toggleTool(tool.name, val)"
            />
          </div>
        </div>
      </div>

      <div class="form-actions">
        <Button variant="secondary" @click="cancelEdit">{{ t('common.cancel') }}</Button>
        <Button variant="primary" :disabled="saving" @click="save">
          {{ saving ? t('common.saving') : t('common.save') }}
        </Button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.mcp-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.05em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

.toolbar { display: flex; align-items: center; gap: var(--space-3); flex-wrap: wrap; }
.search { flex: 1; min-width: 200px; height: 36px; display: flex; align-items: center; gap: var(--space-2); padding: 0 var(--space-3); border-radius: var(--radius-md); border: 1px solid var(--color-line); color: var(--color-text-faint); font-size: var(--text-sm); background: var(--color-surface-raised); transition: border-color var(--duration-fast) var(--ease-out), box-shadow var(--duration-fast) var(--ease-out); }
.search:focus-within { border-color: var(--color-accent); box-shadow: var(--p-focus-ring); color: var(--color-text-muted); }
.search svg { width: 15px; height: 15px; flex: none; }
.search input { width: 100%; border: none; outline: none; background: transparent; font: inherit; color: var(--color-text); }
.stats { font-size: var(--text-sm); color: var(--color-text-muted); white-space: nowrap; }

.empty { padding: var(--space-6) var(--space-4); border: 1px solid var(--color-line); border-radius: var(--radius-xl); color: var(--color-text-faint); font-size: var(--text-sm); text-align: center; background: var(--color-bg); display: flex; align-items: center; justify-content: center; }

.server-list { display: flex; flex-direction: column; gap: var(--space-2); }
.server-card { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: var(--space-3); align-items: center; padding: var(--space-3) var(--space-4); border: 1px solid var(--color-line); border-radius: var(--radius-md); background: var(--color-surface-raised); }
.server-card:hover { border-color: var(--color-line-strong); }
.server-info { display: flex; align-items: center; gap: var(--space-2); min-width: 0; }
.status-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; }
.server-name { font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tool-count { font-size: var(--text-xs); color: var(--color-text-faint); }
.server-actions { display: flex; gap: var(--space-2); flex: none; }

.error-msg { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-danger-soft); color: var(--color-danger); font-size: var(--text-sm); }

.edit-header { display: flex; align-items: center; justify-content: space-between; gap: var(--space-3); }
.edit-title-row { display: flex; align-items: center; gap: var(--space-2); }
.back-btn { padding: var(--space-1) var(--space-2); }
.edit-title { margin: 0; font-family: var(--font-ui); font-size: var(--text-lg); font-weight: var(--weight-semibold); color: var(--color-text); }

.form-grid { display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-3); }
.form-grid :deep(.field) { grid-column: span 1; }
.form-grid :deep(.field:first-child) { grid-column: span 2; }

.code-area { font-family: var(--font-mono); font-size: var(--text-xs); }

.json-editor { }
.json-editor .code-area { font-family: var(--font-mono); font-size: var(--text-xs); width: 100%; }

.form-actions { display: flex; justify-content: flex-end; gap: var(--space-2); margin-top: var(--space-3); }

.tool-list-section { margin-top: var(--space-4); border-top: 1px solid var(--color-line); padding-top: var(--space-3); }
.section-label { font-size: var(--text-xs); letter-spacing: 0.05em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-2); }
.tool-list { display: flex; flex-direction: column; gap: var(--space-2); }
.tool-item { display: flex; align-items: center; justify-content: space-between; gap: var(--space-3); padding: var(--space-2) var(--space-3); border: 1px solid var(--color-line); border-radius: var(--radius-md); background: var(--color-surface-raised); }
.tool-info { display: flex; flex-direction: column; gap: var(--space-1); min-width: 0; }
.tool-name { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tool-desc { font-size: var(--text-xs); color: var(--color-text-faint); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tool-empty { padding: var(--space-3); color: var(--color-text-faint); font-size: var(--text-sm); text-align: center; }

@media (max-width: 640px) {
  .form-grid { grid-template-columns: 1fr; }
  .form-grid :deep(.field) { grid-column: span 1; }
}
</style>
