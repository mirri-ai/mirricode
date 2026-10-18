<!-- apps/mirri-web/src/components/settings/McpServersPanel.vue -->
<!-- MCP server management panel. Renders inside the Settings dialog's
     "mcp" tab. List view + form/JSON dual-mode editor. -->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppMcpServer, AppMcpServerConfig, AppToolDescriptor } from '../../api/types';
import { getMirriWebApi } from '../../api';
import { filterServersByTab, type McpTab } from '../../lib/mcpSourceTabs';
import Button from '../ui/Button.vue';
import Badge from '../ui/Badge.vue';
import Spinner from '../ui/Spinner.vue';
import Field from '../ui/Field.vue';
import Input from '../ui/Input.vue';
import Select from '../ui/Select.vue';
import Switch from '../ui/Switch.vue';
import Textarea from '../ui/Textarea.vue';
import SegmentedControl from '../ui/SegmentedControl.vue';
import Dialog from '../ui/Dialog.vue';
import Tooltip from '../ui/Tooltip.vue';
import ToolbarSearch from '../ui/ToolbarSearch.vue';
import { useConfirmDialog } from '../../composables/useConfirmDialog';
import { countDisabledForServer, enabledToolCount as computeEnabled, totalToolCount as computeTotal } from './mcpToolCounting';

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

// Test-connect state
const testing = ref(false);
const testResult = ref<{ status: 'connected' | 'error'; error?: string; toolCount: number } | null>(null);

// Polling for connecting servers
const pollTimer = ref<ReturnType<typeof setInterval> | null>(null);

// Per-server connect button state
const connecting = ref<string | null>(null);

// Structured env var editor
interface EnvVarEntry { key: string; value: string; }
const envVars = ref<EnvVarEntry[]>([]);
const headerVars = ref<EnvVarEntry[]>([]);

// Preserve tool toggle settings from existing config — the form does not
// expose tool editing (that lives in the dedicated modal), but it must not
// drop these fields when saving.
const preservedEnabledTools = ref<string[] | undefined>(undefined);
const preservedDisabledTools = ref<string[] | undefined>(undefined);

interface McpFormState {
  name: string;
  transport: 'stdio' | 'http' | 'sse';
  command: string;
  args: string;       // space-separated, converted to string[] on save
  cwd: string;
  url: string;
  enabled: boolean;
  startupTimeoutMs: number;
  toolTimeoutMs: number;
  bearerTokenEnvVar: string;
}

const form = reactive<McpFormState>({
  name: '',
  transport: 'stdio',
  command: '',
  args: '',
  cwd: '',
  url: '',
  enabled: true,
  startupTimeoutMs: 30000,
  toolTimeoutMs: 60000,
  bearerTokenEnvVar: '',
});
const jsonText = ref('{}');
const formError = ref('');
const saving = ref(false);

// Resolved-values preview (eye button): the panel edits the literal source
// config (${VAR} references as-written); the preview shows the expanded
// values read-only. Saving always writes back the source form.
const showResolved = ref(false);
const resolvedConfig = ref<AppMcpServerConfig | null>(null);
interface FormSnapshot {
  jsonText: string;
  envVars: EnvVarEntry[];
  headerVars: EnvVarEntry[];
  form: McpFormState;
}
const formSnapshot = ref<FormSnapshot | null>(null);

// -------------------------------------------------------------------------
// Env-ref detection
// -------------------------------------------------------------------------
const ENV_REF_RE = /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/;

function isEnvRef(value: string): boolean {
  return ENV_REF_RE.test(value);
}

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
    const state = await api.getGlobalMcpToggleState();
    disabledServers.value = state.disabledServers;
    disabledTools.value = state.disabledTools;
  } catch {
    // Non-critical — toggle UI defaults to "enabled"
  }
}

// -------------------------------------------------------------------------
// Tool list modal
// -------------------------------------------------------------------------
const toolModalOpen = ref(false);
const toolModalServer = ref<string>('');
const modalTools = ref<AppToolDescriptor[]>([]);
const modalDisabledTools = ref<string[]>([]);
const modalSearch = ref('');
const modalLoading = ref(false);
const modalError = ref('');

async function openToolModal(serverName: string): Promise<void> {
  toolModalServer.value = serverName;
  toolModalOpen.value = true;
  modalSearch.value = '';
  modalLoading.value = true;
  modalError.value = '';
  if (disabledServers.value.includes(serverName)) {
    // Disabled server: the persisted preference snapshot, read-only (PS3.2
    // — an allowlist is the known tool list, a denylist marks its members;
    // both switch states derive from the snapshot itself).
    try {
      const snapshot = await api.getDisabledServerToolSnapshot(serverName);
      modalTools.value = snapshot.tools.map((t) => ({
        name: t.name,
        description: '',
        source: 'mcp' as const,
        mcpServerId: serverName,
        enabled: t.enabled,
      }));
    } catch (e) {
      modalTools.value = [];
      modalError.value = e instanceof Error ? e.message : String(e);
    }
    modalDisabledTools.value = modalTools.value
      .filter((t) => t.enabled === false)
      .map((t) => t.name);
    modalLoading.value = false;
    return;
  }
  try {
    const allTools = await api.listGlobalMcpTools();
    modalTools.value = allTools.filter((t) => t.mcpServerId === serverName);
  } catch (e) {
    modalTools.value = [];
    modalError.value = e instanceof Error ? e.message : String(e);
  }
  try {
    const state = await api.getGlobalMcpToggleState();
    const prefix = `mcp__${serverName}__`;
    modalDisabledTools.value = state.disabledTools
      .filter((t) => t.startsWith(prefix))
      .map((t) => t.slice(prefix.length));
  } catch {
    modalDisabledTools.value = [];
  }
  modalLoading.value = false;
}

async function toggleModalTool(toolName: string, enabled: boolean): Promise<void> {
  try {
    if (enabled) {
      await api.enableGlobalMcpTool(toolModalServer.value, toolName);
      modalDisabledTools.value = modalDisabledTools.value.filter((t) => t !== toolName);
    } else {
      await api.disableGlobalMcpTool(toolModalServer.value, toolName);
      if (!modalDisabledTools.value.includes(toolName)) {
        modalDisabledTools.value.push(toolName);
      }
    }
    // Mirror the engine's new verdict locally — the switch model-value reads
    // `tool.enabled !== false` (the allowlist verdict) alongside the denylist,
    // so both surfaces must update or the switch appears stuck OFF after a
    // successful enable (PS3.2 re-enableable).
    const tool = modalTools.value.find((t) => t.name === toolName);
    if (tool !== undefined) tool.enabled = enabled;
    // Refresh both the server list and the toggle state so the card's enabled
    // / total counts reflect the change immediately, without waiting on the
    // server-side 200ms fs watcher.
    await loadServers();
    await loadToggleState();
  } catch (e) {
    modalError.value = e instanceof Error ? e.message : String(e);
  }
}

// -------------------------------------------------------------------------
// Bulk tool toggles (PS3.4) — one RPC with the complete new denylist
// -------------------------------------------------------------------------
const bulkToggling = ref(false);

async function toggleAllTools(enable: boolean): Promise<void> {
  const targets = filteredModalTools.value.map((t) => t.name);
  if (targets.length === 0 || bulkToggling.value || modalReadOnly.value) return;
  const filtered = modalSearch.value.trim() !== '';
  if (filtered) {    // Filtered scope: the button names the subset, and the confirm dialog
    // lists the affected tools before anything changes (PS3.4).
    const shown =
      targets.slice(0, 10).join(t('settings.mcpPoolsSeparator')) +
      (targets.length > 10 ? ` …(+${targets.length - 10})` : '');
    const ok = await confirm({
      title: enable ? t('settings.mcpBulkEnableTitle') : t('settings.mcpBulkDisableTitle'),
      message: t('settings.mcpBulkConfirmMessage', { n: targets.length, tools: shown }),
      confirmLabel: enable ? t('settings.mcpBulkEnableTitle') : t('settings.mcpBulkDisableTitle'),
      variant: enable ? 'primary' : 'danger',
    });
    if (!ok) return;
  }
  bulkToggling.value = true;
  try {
    // One call with the complete new denylist — per-tool writes are
    // read-modify-write races that would collapse into the last writer.
    const current = new Set(modalDisabledTools.value);
    for (const name of targets) {
      if (enable) current.delete(name);
      else current.add(name);
    }
    // Bulk-enable mirrors the single-tool path: also lift the allowlist
    // exclusion, or allowlist-excluded tools stay OFF after "enable all"
    // (PS3.4 — every switch flips with the action). The lifted form is the
    // full advertised list; when no tool is allowlist-excluded (the common
    // case — allowlists only come from hand-edited configs) nothing is
    // passed and the stored allowlist rides along unchanged.
    let lifted: string[] | undefined;
    if (enable) {
      const excluded = modalTools.value.filter((t) => t.enabled === false);
      if (excluded.length > 0) {
        lifted = modalTools.value.map((t) => t.name);
      }
    }
    await api.setMcpToolFilter(toolModalServer.value, [...current], lifted);
    modalDisabledTools.value = [...current];
    // Mirror the engine's new verdicts locally (the switch reads both the
    // allowlist verdict and the denylist — both must update).
    for (const tool of modalTools.value) {
      if (targets.includes(tool.name)) tool.enabled = enable;
    }
    await loadServers();
    await loadToggleState();
  } catch (e) {
    modalError.value = e instanceof Error ? e.message : String(e);
  } finally {
    bulkToggling.value = false;
  }
}

onMounted(() => {
  void loadServers();
  void loadToggleState();
});

// -------------------------------------------------------------------------
// Polling for connecting servers
// -------------------------------------------------------------------------
function startPolling(): void {
  if (pollTimer.value !== null) return;
  pollTimer.value = setInterval(async () => {
    const hasConnecting = servers.value.some((s) => s.status === 'connecting');
    if (!hasConnecting) {
      stopPolling();
      return;
    }
    try {
      servers.value = await api.listGlobalMcpServers();
    } catch {
      // ignore poll errors
    }
  }, 2000);
}

function stopPolling(): void {
  if (pollTimer.value !== null) {
    clearInterval(pollTimer.value);
    pollTimer.value = null;
  }
}

watch(servers, (newServers) => {
  const hasConnecting = newServers.some((s) => s.status === 'connecting');
  if (hasConnecting) {
    startPolling();
  } else {
    stopPolling();
  }
}, { deep: true });

onUnmounted(() => {
  stopPolling();
});

// -------------------------------------------------------------------------
// Computed
// -------------------------------------------------------------------------
/** Source-tab grouping (D-S5): All / User / Project / Plugin; search
 *  filters within the active tab. */
const activeTab = ref<McpTab>('all');

const tabCounts = computed(() => ({
  all: servers.value.length,
  user: filterServersByTab(servers.value, 'user').length,
  project: filterServersByTab(servers.value, 'project').length,
  plugin: filterServersByTab(servers.value, 'plugin').length,
}));

const tabbedServers = computed(() => filterServersByTab(servers.value, activeTab.value));

const tabEmptyKey = computed(() => {
  switch (activeTab.value) {
    case 'user': return 'settings.mcpTabEmptyUser';
    case 'project': return 'settings.mcpTabEmptyProject';
    case 'plugin': return 'settings.mcpTabEmptyPlugin';
    default: return 'settings.mcpEmpty';
  }
});

const filteredServers = computed(() => {
  const q = searchQuery.value.trim().toLowerCase();
  if (!q) return tabbedServers.value;
  return tabbedServers.value.filter((s) => s.name.toLowerCase().includes(q));
});

const filteredModalTools = computed(() => {
  const q = modalSearch.value.trim().toLowerCase();
  if (!q) return modalTools.value;
  return modalTools.value.filter(
    (t) => t.name.toLowerCase().includes(q) || t.description.toLowerCase().includes(q),
  );
});

/** The tool modal is read-only while its server is disabled (PS3.2: the
 *  snapshot stays viewable — the switches unlock when the server is
 *  re-enabled). */
const modalReadOnly = computed(
  () => toolModalServer.value !== '' && disabledServers.value.includes(toolModalServer.value),
);

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
  form.cwd = '';
  form.url = '';
  form.enabled = true;
  form.startupTimeoutMs = 30000;
  form.toolTimeoutMs = 60000;
  form.bearerTokenEnvVar = '';
  envVars.value = [];
  headerVars.value = [];
  preservedEnabledTools.value = undefined;
  preservedDisabledTools.value = undefined;
  testResult.value = null;
  testing.value = false;
  jsonText.value = '{}';
  formError.value = '';
  showResolved.value = false;
  resolvedConfig.value = null;
  formSnapshot.value = null;
}

function startCreate(): void {
  editingName.value = null;
  isCreating.value = true;
  editMode.value = 'form';
  resetForm();
}

async function startEdit(server: AppMcpServer): Promise<void> {
  editingName.value = server.name;
  isCreating.value = false;
  editMode.value = 'form';
  resetForm();
  form.name = server.name;
  form.transport = server.transport;
  form.enabled = server.status !== 'disconnected';

  // Edit the literal source config (${VAR} references as written); keep the
  // resolved (env-expanded) config for the read-only eye preview.
  let source = server.config;
  let resolved = server.config;
  try {
    const configEntries = await api.getGlobalMcpConfig();
    const entry = configEntries.find((e) => e.name === server.name);
    if (entry !== undefined) {
      source = entry.source;
      resolved = entry.resolved;
    }
  } catch {
    // fall back to the runtime view's config
  }
  if (source) fillFormFromConfig(source);
  resolvedConfig.value = resolved ?? null;
  showResolved.value = false;
  formSnapshot.value = null;
  jsonText.value = JSON.stringify({ config: buildConfigFromForm() }, null, 2);
}

/** Populates the form fields from a single config (source or resolved view). */
function fillFormFromConfig(cfg: AppMcpServerConfig): void {
  if (cfg.transport) form.transport = cfg.transport;
  if (cfg.command) form.command = cfg.command;
  if (cfg.args) form.args = cfg.args.join(' ');
  if (cfg.env) envVars.value = Object.entries(cfg.env).map(([key, value]) => ({ key, value }));
  if (cfg.cwd) form.cwd = cfg.cwd;
  if (cfg.url) form.url = cfg.url;
  if (cfg.headers) headerVars.value = Object.entries(cfg.headers).map(([key, value]) => ({ key, value }));
  if (cfg.enabled !== undefined) form.enabled = cfg.enabled;
  preservedEnabledTools.value = cfg.enabledTools;
  preservedDisabledTools.value = cfg.disabledTools;
  if (cfg.startupTimeoutMs) form.startupTimeoutMs = cfg.startupTimeoutMs;
  if (cfg.toolTimeoutMs) form.toolTimeoutMs = cfg.toolTimeoutMs;
  if (cfg.bearerTokenEnvVar) form.bearerTokenEnvVar = cfg.bearerTokenEnvVar;
}

// -------------------------------------------------------------------------
// Resolved-values preview (eye button)
// -------------------------------------------------------------------------
function enterResolvedPreview(): void {
  const cfg = resolvedConfig.value;
  if (cfg === null || cfg === undefined) return;
  if (formSnapshot.value !== null) return; // already previewing
  formSnapshot.value = {
    jsonText: jsonText.value,
    envVars: envVars.value.map((e) => ({ ...e })),
    headerVars: headerVars.value.map((e) => ({ ...e })),
    form: { ...form },
  };
  envVars.value = [];
  headerVars.value = [];
  fillFormFromConfig(cfg);
  jsonText.value = JSON.stringify({ config: cfg }, null, 2);
  showResolved.value = true;
}

function exitResolvedPreview(): void {
  const snap = formSnapshot.value;
  showResolved.value = false;
  if (snap === null) return;
  Object.assign(form, snap.form);
  envVars.value = snap.envVars;
  headerVars.value = snap.headerVars;
  jsonText.value = snap.jsonText;
  formSnapshot.value = null;
}

function toggleResolvedPreview(): void {
  if (showResolved.value) exitResolvedPreview();
  else enterResolvedPreview();
}

function cancelEdit(): void {
  editingName.value = null;
  isCreating.value = false;
  showResolved.value = false;
  resolvedConfig.value = null;
  formSnapshot.value = null;
  formError.value = '';
}

// -------------------------------------------------------------------------
// Env var editor helpers
// -------------------------------------------------------------------------
function addEnvVar(): void {
  envVars.value.push({ key: '', value: '' });
}

function removeEnvVar(index: number): void {
  envVars.value.splice(index, 1);
}

function addHeaderVar(): void {
  headerVars.value.push({ key: '', value: '' });
}

function removeHeaderVar(index: number): void {
  headerVars.value.splice(index, 1);
}

// -------------------------------------------------------------------------
// Test-connect
// -------------------------------------------------------------------------
async function testConnection(): Promise<void> {
  if (showResolved.value) return; // preview is read-only
  testing.value = true;
  testResult.value = null;
  try {
    const config = buildConfigFromForm();
    const result = await api.testConnectMcpServer(config);
    // Both backends answer with a tool count (the rust test surface carries
    // no tool descriptors).
    testResult.value = {
      status: result.status,
      error: result.error,
      toolCount: result.toolCount,
    };
  } catch (e) {
    testResult.value = { status: 'error', error: e instanceof Error ? e.message : String(e), toolCount: 0 };
  } finally {
    testing.value = false;
  }
}

// -------------------------------------------------------------------------
// JSON sync
// -------------------------------------------------------------------------
function syncJson(): void {
  try {
    const config = buildConfigFromForm();
    jsonText.value = JSON.stringify({ config }, null, 2);
  } catch {
    // Form may be incomplete; don't clobber JSON
  }
}

// Watch form fields and sync JSON
watch(
  () => [
    form.name, form.transport, form.command, form.args, form.cwd,
    form.url, form.enabled, form.startupTimeoutMs, form.toolTimeoutMs,
    form.bearerTokenEnvVar, envVars.value, headerVars.value,
  ],
  () => {
    if (editMode.value === 'form' && !showResolved.value) {
      syncJson();
    }
  },
  { deep: true },
);

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
    // Build env from structured entries
    const envObj: Record<string, string> = {};
    for (const entry of envVars.value) {
      if (entry.key.trim()) {
        envObj[entry.key.trim()] = entry.value;
      }
    }
    if (Object.keys(envObj).length > 0) {
      config.env = envObj;
    }
  } else {
    if (form.url) config.url = form.url;
    // Build headers from structured entries
    const headersObj: Record<string, string> = {};
    for (const entry of headerVars.value) {
      if (entry.key.trim()) {
        headersObj[entry.key.trim()] = entry.value;
      }
    }
    if (Object.keys(headersObj).length > 0) {
      config.headers = headersObj;
    }
    if (form.bearerTokenEnvVar) config.bearerTokenEnvVar = form.bearerTokenEnvVar;
  }
  if (preservedEnabledTools.value) config.enabledTools = [...preservedEnabledTools.value];
  if (preservedDisabledTools.value) config.disabledTools = [...preservedDisabledTools.value];
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
  if (showResolved.value) return; // preview is read-only
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
// Connect single server
// -------------------------------------------------------------------------
async function onConnect(name: string): Promise<void> {
  connecting.value = name;
  try {
    const { server } = await api.connectMcpServer(name);
    const idx = servers.value.findIndex((s) => s.name === name);
    if (idx >= 0) {
      servers.value[idx] = server;
    }
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    connecting.value = null;
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
  const cfg = server.config;
  if (!cfg) return server.transport;
  if (cfg.transport === 'stdio' && cfg.command) {
    return cfg.args?.length ? `${cfg.command} ${cfg.args.join(' ')}` : cfg.command;
  }
  if ((cfg.transport === 'http' || cfg.transport === 'sse') && cfg.url) {
    return cfg.url;
  }
  return server.transport;
}

// -------------------------------------------------------------------------
// Source provenance (PS1.1 — merged list with per-entry source labels)
// -------------------------------------------------------------------------
const MCP_SOURCE_BADGE_VARIANT: Record<string, 'info' | 'neutral' | 'solid'> = {
  project: 'info',
  projectRoot: 'info',
  user: 'neutral',
  plugin: 'solid',
};

/** Known wire sources get their i18n label; unknown ones render verbatim so
 *  new engine scopes stay visible instead of being swallowed. */
function sourceLabel(source: string | undefined): string {
  if (source === undefined) return '';
  const labels: Record<string, string> = {
    project: t('settings.mcpSourceProject'),
    projectRoot: t('settings.mcpSourceProjectRoot'),
    user: t('settings.mcpSourceUser'),
    plugin: t('settings.mcpSourcePlugin'),
  };
  return labels[source] ?? source;
}

/** Tooltip explaining same-name shadowing: the effective card plus the
 *  pools still holding a shadowed connection for that name. */
function shadowedHint(server: AppMcpServer): string {
  const pools = server.shadowedPools ?? [];
  const poolLabels = pools
    .map((pool) => {
      const labels: Record<string, string> = {
        user: t('settings.mcpSourceUser'),
        workspace: t('settings.mcpSourceProject'),
        plugin: t('settings.mcpSourcePlugin'),
      };
      return labels[pool] ?? pool;
    })
    .filter(Boolean);
  if (poolLabels.length === 0) return '';
  return t('settings.mcpShadowedHint', { pools: poolLabels.join(t('settings.mcpPoolsSeparator')) });
}

function enabledToolCount(server: AppMcpServer): number {
  return computeEnabled(
    server.toolCount,
    countDisabledForServer(disabledTools.value, server.name),
  );
}

function serverTotalToolCount(server: AppMcpServer): number {
  return computeTotal(
    server.toolCount,
    countDisabledForServer(disabledTools.value, server.name),
  );
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
      <SegmentedControl
        :model-value="activeTab"
        :options="[
          { value: 'all', label: `${t('settings.mcpTabAll')} ${tabCounts.all}` },
          { value: 'user', label: `${t('settings.mcpTabUser')} ${tabCounts.user}` },
          { value: 'project', label: `${t('settings.mcpTabProject')} ${tabCounts.project}` },
          { value: 'plugin', label: `${t('settings.mcpTabPlugin')} ${tabCounts.plugin}` },
        ]"
        size="sm"
        @update:model-value="activeTab = $event as McpTab"
      />
      <div class="toolbar">
        <ToolbarSearch
          v-model="searchQuery"
          :placeholder="t('settings.mcpSearch')"
          :aria-label="t('settings.mcpSearch')"
        />
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

      <div v-else-if="tabbedServers.length === 0" class="empty">
        {{ t(tabEmptyKey) }}
      </div>

      <div v-else-if="filteredServers.length === 0" class="empty">
        {{ t('settings.mcpSearchNoMatch') }}
      </div>

      <div v-else class="server-list">
        <div v-for="s in filteredServers" :key="s.id" class="server-card">
          <div class="server-info">
            <span class="status-dot" :class="`status-dot--${s.status}`" :style="{ background: statusColor(s.status) }" />
            <span class="server-name">{{ s.name }}</span>
            <Badge v-if="s.source" :variant="MCP_SOURCE_BADGE_VARIANT[s.source] ?? 'neutral'" size="sm">{{ sourceLabel(s.source) }}</Badge>
            <Tooltip v-if="s.shadowedPools?.length" :text="shadowedHint(s)">
              <Badge variant="warning" size="sm">{{ t('settings.mcpShadowed') }}</Badge>
            </Tooltip>
            <Badge variant="neutral" size="sm">{{ commandSummary(s) }}</Badge>
            <!-- Tool-list entry: stays clickable for disabled servers too
                 (PS3.2 — the persisted snapshot remains viewable). -->
            <button
              v-if="s.toolCount > 0 || disabledServers.includes(s.name)"
              class="tool-count tool-count--clickable"
              @click="openToolModal(s.name)"
            >{{ t('settings.mcpToolsEnabled', { enabled: enabledToolCount(s), total: serverTotalToolCount(s) }) }}</button>
            <span v-else class="tool-count">{{ t('settings.mcpTools', { n: s.toolCount }) }}</span>
          </div>
          <div v-if="s.status === 'error' && s.lastError" class="server-error">
            {{ s.lastError }}
          </div>
          <div class="server-actions">
            <Button
              v-if="(s.status === 'error' || s.status === 'disconnected') && !disabledServers.includes(s.name)"
              variant="secondary"
              size="sm"
              :disabled="connecting === s.name"
              @click="onConnect(s.name)"
            >
              {{ connecting === s.name ? '...' : t('settings.mcpConnect') }}
            </Button>
            <Switch
              :model-value="!disabledServers.includes(s.name)"
              :disabled="togglingServer === s.name"
              @update:model-value="(val: boolean) => toggleServer(s, val)"
            />
            <!-- Definition editing is read-only for team-shared and plugin
                 assets (PS1.2); the toggle and tool switches stay enabled
                 for every source (PS3.3). -->
            <Tooltip v-if="s.source === 'projectRoot' || s.source === 'plugin'" :text="s.source === 'plugin' ? t('settings.mcpPluginReadOnlyReason') : t('settings.mcpReadOnlyReason')">
              <Button variant="secondary" size="sm" disabled>{{ t('common.edit') }}</Button>
            </Tooltip>
            <Button v-else variant="secondary" size="sm" @click="startEdit(s)">{{ t('common.edit') }}</Button>
            <Tooltip v-if="s.source === 'projectRoot' || s.source === 'plugin'" :text="s.source === 'plugin' ? t('settings.mcpPluginReadOnlyReason') : t('settings.mcpReadOnlyReason')">
              <Button variant="danger-soft" size="sm" disabled>{{ t('common.delete') }}</Button>
            </Tooltip>
            <Button v-else variant="danger-soft" size="sm" @click="onDelete(s.name)">{{ t('common.delete') }}</Button>
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
        <div class="edit-header-actions">
          <Button
            variant="ghost"
            size="sm"
            :disabled="!resolvedConfig"
            :title="showResolved ? t('settings.mcpShowSource') : t('settings.mcpShowResolved')"
            @click="toggleResolvedPreview"
          >
            <svg v-if="!showResolved" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="eye-icon"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z"/><circle cx="12" cy="12" r="3"/></svg>
            <svg v-else viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="eye-icon"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94"/><path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19"/><path d="M14.12 14.12a3 3 0 1 1-4.24-4.24"/><line x1="1" y1="1" x2="23" y2="23"/></svg>
            {{ showResolved ? t('settings.mcpShowSource') : t('settings.mcpShowResolved') }}
          </Button>
          <SegmentedControl
            :model-value="editMode"
            :options="[
              { value: 'form', label: t('settings.mcpFormMode') },
              { value: 'json', label: t('settings.mcpJsonMode') },
            ]"
            @update:model-value="editMode = $event as 'form' | 'json'"
          />
          <Button variant="secondary" size="sm" @click="cancelEdit">{{ t('common.cancel') }}</Button>
          <Button variant="primary" size="sm" :disabled="saving || showResolved" @click="save">
            {{ saving ? t('common.saving') : t('common.save') }}
          </Button>
        </div>
      </div>

      <div v-if="showResolved" class="preview-banner">{{ t('settings.mcpPreviewMode') }}</div>

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
            <Input v-model="form.command" :readonly="showResolved" :placeholder="'npx'" />
          </Field>
          <Field :label="t('settings.mcpArgs')">
            <Input v-model="form.args" :readonly="showResolved" :placeholder="'-y @modelcontextprotocol/server-filesystem /tmp'" />
          </Field>
          <Field :label="t('settings.mcpCwd')">
            <Input v-model="form.cwd" :readonly="showResolved" :placeholder="'/path/to/dir'" />
          </Field>
          <Field :label="t('settings.mcpEnv')">
            <div class="kv-editor">
              <div v-for="(entry, i) in envVars" :key="i" class="kv-row">
                <Input v-model="entry.key" :readonly="showResolved" placeholder="VAR_NAME" class="kv-key" />
                <Input v-model="entry.value" :readonly="showResolved" placeholder="value or ${ENV_VAR}" class="kv-value" :class="{ 'env-ref': !showResolved && isEnvRef(entry.value) }" />
                <Button variant="ghost" size="sm" :disabled="showResolved" @click="removeEnvVar(i)">✕</Button>
              </div>
              <Button variant="secondary" size="sm" :disabled="showResolved" @click="addEnvVar">+ {{ t('settings.mcpAddEnvVar') }}</Button>
            </div>
          </Field>
        </template>

        <template v-else>
          <Field :label="t('settings.mcpUrl')">
            <Input v-model="form.url" :readonly="showResolved" :placeholder="'https://mcp.example.com/sse'" />
          </Field>
          <Field :label="t('settings.mcpHeaders')">
            <div class="kv-editor">
              <div v-for="(entry, i) in headerVars" :key="i" class="kv-row">
                <Input v-model="entry.key" :readonly="showResolved" placeholder="Header-Name" class="kv-key" />
                <Input v-model="entry.value" :readonly="showResolved" placeholder="value or ${ENV_VAR}" class="kv-value" :class="{ 'env-ref': !showResolved && isEnvRef(entry.value) }" />
                <Button variant="ghost" size="sm" :disabled="showResolved" @click="removeHeaderVar(i)">✕</Button>
              </div>
              <Button variant="secondary" size="sm" :disabled="showResolved" @click="addHeaderVar">+ {{ t('settings.mcpAddHeader') }}</Button>
            </div>
          </Field>
          <Field :label="t('settings.mcpBearerEnvVar')">
            <Input v-model="form.bearerTokenEnvVar" :readonly="showResolved" :placeholder="'MY_API_KEY'" />
          </Field>
        </template>

        <Field :label="t('settings.mcpStartupTimeout')">
          <Input :model-value="form.startupTimeoutMs" type="number" :readonly="showResolved" :placeholder="'30000'" @update:model-value="form.startupTimeoutMs = Number($event)" />
        </Field>
        <Field :label="t('settings.mcpToolTimeout')">
          <Input :model-value="form.toolTimeoutMs" type="number" :readonly="showResolved" :placeholder="'60000'" @update:model-value="form.toolTimeoutMs = Number($event)" />
        </Field>
      </div>

      <!-- JSON mode -->
      <div v-else class="json-editor">
        <Textarea v-model="jsonText" :rows="16" :readonly="showResolved" class="code-area" placeholder='Example: {"transport":"stdio","command":"npx","args":["-y","..."]}' />
      </div>

      <div v-if="formError" class="error-msg">{{ formError }}</div>

      <!-- Test connection -->
      <div class="test-section">
        <Button variant="secondary" :disabled="testing || showResolved" @click="testConnection">
          {{ testing ? t('settings.mcpTesting') : t('settings.mcpTestConnection') }}
        </Button>
        <div v-if="testResult?.status === 'connected'" class="test-success">
          ✓ {{ t('settings.mcpTestConnected', { n: testResult.toolCount }) }}
        </div>
        <div v-if="testResult?.status === 'error'" class="test-error">
          ✗ {{ testResult.error }}
        </div>
      </div>
    </template>

    <!-- Tool list modal -->
    <Dialog
      :open="toolModalOpen"
      :title="t('settings.mcpToolsList')"
      size="md"
      @update:open="toolModalOpen = $event"
    >
      <div v-if="modalReadOnly" class="modal-readonly-banner">
        {{ t('settings.mcpToolListReadonly') }}
      </div>
      <div v-if="modalLoading" class="tool-list-empty"><Spinner /></div>
      <div v-else-if="modalError" class="tool-list-error">{{ modalError }}</div>
      <div v-else-if="modalTools.length > 0" class="modal-tool-wrap">
        <div class="modal-bulk-actions">
          <Button variant="secondary" size="sm" :disabled="modalReadOnly || bulkToggling" @click="toggleAllTools(true)">
            {{ modalSearch.trim() ? t('settings.mcpBulkEnableFiltered', { n: filteredModalTools.length }) : t('settings.mcpBulkEnableAll') }}
          </Button>
          <Button variant="secondary" size="sm" :disabled="modalReadOnly || bulkToggling" @click="toggleAllTools(false)">
            {{ modalSearch.trim() ? t('settings.mcpBulkDisableFiltered', { n: filteredModalTools.length }) : t('settings.mcpBulkDisableAll') }}
          </Button>
          <Spinner v-if="bulkToggling" class="bulk-spinner" />
        </div>
        <input
          v-model="modalSearch"
          class="modal-tool-search"
          type="search"
          :aria-label="t('settings.mcpToolSearch')"
          :placeholder="t('settings.mcpToolSearch')"
        />
        <div class="tool-list">
          <div v-for="tool in filteredModalTools" :key="tool.name" class="tool-item">
            <Tooltip
              :text="tool.description || tool.name"
              placement="top"
              :max-width="320"
              :max-lines="8"
            >
              <span class="tool-name">{{ tool.name }}</span>
            </Tooltip>
            <Switch
              :model-value="tool.enabled !== false && !modalDisabledTools.includes(tool.name)"
              :disabled="modalReadOnly"
              @update:model-value="(val: boolean) => toggleModalTool(tool.name, val)"
            />
          </div>
        </div>
      </div>
      <div v-else class="tool-list-empty">
        {{ modalReadOnly ? t('settings.mcpToolListSnapshotEmpty') : t('settings.mcpEmpty') }}
      </div>
    </Dialog>
  </div>
</template>

<style scoped>
.mcp-panel { display: flex; flex-direction: column; height: 100%; gap: var(--space-3); }

.panel-head { margin-bottom: var(--space-2); }
.panel-kicker { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-1); }
.panel-title { margin: 0 0 var(--space-2); font-family: var(--font-ui); font-size: var(--text-2xl); font-weight: var(--weight-semibold); letter-spacing: -0.01em; color: var(--color-text); }
.panel-desc { margin: 0; font-family: var(--font-ui); font-size: var(--text-sm); line-height: var(--leading-normal); color: var(--color-text-muted); max-width: 560px; }

.toolbar { display: flex; align-items: center; gap: var(--space-3); flex-wrap: wrap; }

.stats { font-size: var(--text-sm); color: var(--color-text-muted); white-space: nowrap; }

.empty { padding: var(--space-6) var(--space-4); border: 1px solid var(--color-line); border-radius: var(--radius-xl); color: var(--color-text-faint); font-size: var(--text-sm); text-align: center; background: var(--color-bg); display: flex; align-items: center; justify-content: center; }

.server-list { display: flex; flex-direction: column; gap: var(--space-2); }
.server-card { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: var(--space-3); align-items: center; padding: var(--space-3) var(--space-4); border: 1px solid var(--color-line); border-radius: var(--radius-md); background: var(--color-surface-raised); }
.server-card:hover { border-color: var(--color-line-strong); }
.server-info { display: flex; align-items: center; gap: var(--space-2); min-width: 0; }
.status-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; position: relative; }
.status-dot--connecting::before { content: ''; position: absolute; inset: -2px; border-radius: 50%; background: inherit; animation: pulse-ring-expand 2s ease-out infinite; }
.status-dot--connected { animation: glow-pulse 2s ease-in-out infinite; }

@keyframes pulse-ring-expand {
  0% { transform: scale(1); opacity: 0.6; }
  100% { transform: scale(2.5); opacity: 0; }
}
@keyframes glow-pulse {
  0%, 100% { box-shadow: 0 0 2px 0px var(--color-success); opacity: 1; }
  50% { box-shadow: 0 0 8px 3px var(--color-success); opacity: 0.7; }
}
.server-name { font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tool-count { font-size: var(--text-xs); color: var(--color-text-faint); }
.tool-count--clickable { background: none; border: none; cursor: pointer; color: var(--color-text-faint); padding: 0; font-size: var(--text-xs); }
.tool-count--clickable:hover { color: var(--color-text); text-decoration: underline; }
.server-actions { display: flex; gap: var(--space-2); flex: none; }
.server-error { grid-column: 1 / -1; font-size: var(--text-xs); color: var(--color-danger); padding: var(--space-1) 0; }

.error-msg { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-danger-soft); color: var(--color-danger); font-size: var(--text-sm); }

.preview-banner { padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); background: var(--color-accent-soft); color: var(--color-text-muted); font-size: var(--text-sm); }
.eye-icon { width: 14px; height: 14px; flex: none; }

.edit-header { display: flex; align-items: center; justify-content: space-between; gap: var(--space-3); }
.edit-header-actions { display: flex; align-items: center; gap: var(--space-2); }
.edit-title-row { display: flex; align-items: center; gap: var(--space-2); }
.back-btn { padding: var(--space-1) var(--space-2); }
.edit-title { margin: 0; font-family: var(--font-ui); font-size: var(--text-lg); font-weight: var(--weight-semibold); color: var(--color-text); }

.form-grid { display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-3); }
.form-grid :deep(.field) { grid-column: span 1; }
.form-grid :deep(.field:first-child) { grid-column: span 2; }

.code-area { font-family: var(--font-mono); font-size: var(--text-xs); }

.json-editor { }
.json-editor .code-area { font-family: var(--font-mono); font-size: var(--text-xs); width: 100%; }

/* KV editor (env vars / headers) */
.kv-editor { display: flex; flex-direction: column; gap: var(--space-2); }
.kv-row { display: flex; gap: var(--space-2); align-items: center; }
.kv-key { flex: 2; font-family: var(--font-mono); font-size: var(--text-xs); }
.kv-value { flex: 3; font-family: var(--font-mono); font-size: var(--text-xs); }
.kv-value.env-ref { border-color: var(--color-accent); background: var(--color-accent-soft); }

.test-section { display: flex; align-items: center; gap: var(--space-3); margin-top: var(--space-3); flex-wrap: wrap; }
.test-success { font-size: var(--text-sm); color: var(--color-success); }
.test-error { font-size: var(--text-sm); color: var(--color-danger); }

.form-actions { display: flex; justify-content: flex-end; gap: var(--space-2); margin-top: var(--space-3); }

.tool-list-section { margin-top: var(--space-4); border-top: 1px solid var(--color-line); padding-top: var(--space-3); }
.section-label { font-size: var(--text-xs); letter-spacing: 0.08em; text-transform: uppercase; color: var(--color-text-faint); margin-bottom: var(--space-2); }
.tool-list { display: flex; flex-direction: column; gap: var(--space-2); }
.tool-item { display: flex; align-items: center; justify-content: space-between; gap: var(--space-3); padding: var(--space-2) var(--space-3); border: 1px solid var(--color-line); border-radius: var(--radius-md); background: var(--color-surface-raised); }
.tool-info { display: flex; flex-direction: column; gap: var(--space-1); min-width: 0; }
.tool-name { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tool-desc { font-size: var(--text-xs); color: var(--color-text-faint); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.modal-tool-wrap { display: flex; flex-direction: column; gap: var(--space-2); }
.modal-readonly-banner { margin-bottom: var(--space-2); padding: var(--space-2) var(--space-3); border-radius: var(--radius-md); border: 1px solid var(--color-warning, #b8860b); background: var(--color-surface-sunken); color: var(--color-text-muted); font-size: var(--text-xs); line-height: var(--leading-normal); }
.tool-list-error { padding: var(--space-3); color: var(--color-danger); font-size: var(--text-sm); }
.modal-bulk-actions { display: flex; align-items: center; gap: var(--space-2); margin-bottom: var(--space-2); }
.bulk-spinner { width: 14px; height: 14px; }
.modal-tool-search { width: 100%; height: 32px; padding: 0 var(--space-3); border-radius: var(--radius-md); border: 1px solid var(--color-line); background: var(--color-surface-raised); color: var(--color-text); font-size: var(--text-sm); outline: none; transition: border-color var(--duration-fast) var(--ease-out), box-shadow var(--duration-fast) var(--ease-out); }
.modal-tool-search:focus { border-color: var(--color-accent); box-shadow: var(--p-focus-ring); }
.modal-tool-wrap .tool-list { max-height: 50vh; overflow-y: auto; }

@media (max-width: 640px) {
  .form-grid { grid-template-columns: 1fr; }
  .form-grid :deep(.field) { grid-column: span 1; }
}
</style>
