<!-- apps/mirri-web/src/components/chat/tool-calls/GenericTool.vue -->
<script setup lang="ts">
import { computed, inject, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { FilePreviewRequest, ToolCall, ToolMedia } from '../../../types';
import { toolChip, toolGlyph, toolLabel, toolSummary } from '../../../lib/toolMeta';
import ToolRow from '../ToolRow.vue';

const props = withDefaults(
  defineProps<{
    tool: ToolCall;
    mobile?: boolean;
    stackPosition?: 'single' | 'first' | 'middle' | 'last';
    toolDiffPanel?: boolean;
  }>(),
  { mobile: false, stackPosition: 'single', toolDiffPanel: false },
);

const emit = defineEmits<{
  openMedia: [media: ToolMedia];
  openFile: [target: FilePreviewRequest];
  openToolDiff: [id: string];
  /** B7 W6 — the running Bash call asked to move to the background
   *  (task.detach by tool call id; handled in App.vue). */
  detach: [toolCallId: string];
}>();

const { t } = useI18n();

// The engine generation, provided by App.vue — the run-in-background entry
// rides the v3 rust engine's task.detach (B7 D-6). Defaults honestly to a
// legacy backend when no provider is found up the tree (tests).
const backend = inject<() => 'v1' | 'v2' | 'v3'>('mirriBackend', () => 'v1');
// Subagent side chats suppress the entry: their tool calls belong to the
// side agent's session, not the main one the detach RPC addresses.
const detachDisabled = inject<boolean>('toolDetachDisabled', false);

const isRunningBash = computed(
  () => props.tool.status === 'running' && /^bash$/i.test(props.tool.name),
);
const canDetach = computed(
  () =>
    isRunningBash.value &&
    backend() === 'v3' &&
    !detachDisabled &&
    props.tool.backgroundedTaskId === undefined,
);
const isBackgrounded = computed(() => props.tool.backgroundedTaskId !== undefined);
const hasOutput = computed(() => !!props.tool.output && props.tool.output.length > 0);
const canExpand = computed(() => hasOutput.value || isRunningBash.value);
const open = ref(props.tool.defaultExpanded === true && canExpand.value);

const status = computed<'running' | 'ok' | 'error'>(() => props.tool.status as 'running' | 'ok' | 'error');
const label = computed(() => toolLabel(props.tool.name));
const glyph = computed(() => toolGlyph(props.tool.name));
const summary = computed(() => toolSummary(props.tool.name, props.tool.arg));
const summaryFull = computed(() => toolSummary(props.tool.name, props.tool.arg, true));
const chip = computed(() =>
  toolChip({
    name: props.tool.name,
    arg: props.tool.arg,
    output: props.tool.output,
    timing: props.tool.timing,
    status: props.tool.status,
  }),
);

function toggle(): void {
  if (canExpand.value) open.value = !open.value;
}

watch(
  () => [props.tool.defaultExpanded, props.tool.output?.length, props.tool.status, props.tool.name] as const,
  () => {
    if (props.tool.defaultExpanded === true && canExpand.value) open.value = true;
  },
);
</script>

<template>
  <ToolRow
    :status="status"
    :icon="glyph"
    :name="label"
    :arg="!open ? summary : ''"
    :time="tool.name !== 'bash' ? tool.timing : ''"
    :open="open"
    :expandable="canExpand"
    :stacked="stackPosition !== 'single'"
    :stack-position="stackPosition"
    @toggle="toggle"
  >
    <template #trailing>
      <span v-if="isBackgrounded" class="bgd-badge" :title="t('tasks.backgroundedTask', { id: tool.backgroundedTaskId })"><span aria-hidden="true">◐</span> {{ t('tasks.backgrounded') }}</span>
      <button
        v-if="canDetach"
        type="button"
        class="detach-btn"
        :title="t('tasks.runInBackground')"
        :aria-label="t('tasks.runInBackground')"
        @click.stop="emit('detach', tool.id)"
      >{{ t('tasks.runInBackground') }}</button>
      <span v-if="chip" class="chip">{{ chip }}</span>
    </template>
    <div v-if="summaryFull" class="bb-summary">{{ summaryFull }}</div>
    <div class="bb-code">
      <div v-if="!hasOutput" class="bb-empty">Waiting for output…</div>
      <div v-for="(line, i) in tool.output ?? []" :key="i">{{ line }}</div>
    </div>
  </ToolRow>
</template>

<style scoped>
.bb-summary {
  color: var(--color-text);
  border-bottom: 1px dashed var(--color-line);
  padding-bottom: 6px;
  margin-bottom: 6px;
  word-break: break-all;
}
.chip {
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  flex: none;
}
/* B7 W6: the detached-into-background stamp — survives the tool result. */
.bgd-badge {
  color: var(--color-text-faint);
  font-size: var(--text-xs);
  flex: none;
  white-space: nowrap;
}
.detach-btn {
  flex: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
  color: var(--color-text-muted);
  font: var(--text-xs) var(--font-ui);
  padding: 1px 6px;
  cursor: pointer;
  white-space: nowrap;
  transition: color var(--duration-fast) var(--ease-out), border-color var(--duration-fast) var(--ease-out);
}
.detach-btn:hover {
  color: var(--color-text);
  border-color: var(--color-line-strong);
}
.detach-btn:focus-visible {
  outline: none;
  box-shadow: var(--p-focus-ring);
}
.bb-empty {
  color: var(--color-text-muted);
  font-style: italic;
}
.bb-code {
  margin-top: 10px;
  padding: 11px 13px;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
}
</style>
