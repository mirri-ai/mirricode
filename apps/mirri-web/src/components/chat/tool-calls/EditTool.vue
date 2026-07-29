<!-- apps/mirri-web/src/components/chat/tool-calls/EditTool.vue -->
<script setup lang="ts">
import { computed, ref } from 'vue';
import type { DiffViewLine, FilePreviewRequest, ToolCall, ToolMedia } from '../../../types';
import { diffStats } from '../../../lib/diffLines';
import { buildEditDiffLines, parseArg } from '../../../lib/toolDiff';
import { normalizeToolName, toolGlyph, toolLabel, toolSummary } from '../../../lib/toolMeta';
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
}>();

const status = computed<'running' | 'ok' | 'error'>(() => props.tool.status as 'running' | 'ok' | 'error');
const label = computed(() => toolLabel(props.tool.name));
const glyph = computed(() => toolGlyph(props.tool.name));
const summary = computed(() => toolSummary(props.tool.name, props.tool.arg));
const summaryFull = computed(() => toolSummary(props.tool.name, props.tool.arg, true));

const editDiff = computed<DiffViewLine[] | null>(() => buildEditDiffLines(props.tool));
const chip = computed(() => {
  const diff = editDiff.value;
  if (diff && props.tool.status !== 'error') {
    const { added, removed } = diffStats(diff);
    if (added || removed) return `+${added} −${removed}`;
  }
  return '';
});

const hasOutput = computed(() => !!props.tool.output && props.tool.output.length > 0);
const open = ref(false);
const canExpand = computed(() => hasOutput.value && !props.toolDiffPanel);

// Write tool calls carry the full file content in tool.arg — when completed
// successfully, clicking the card opens a FilePreview instead of the diff
// panel, showing the written content directly.
const isWriteWithContent = computed(() => {
  if (normalizeToolName(props.tool.name) !== 'write') return false;
  if (props.tool.status === 'error') return false;
  const d = parseArg(props.tool.arg);
  return !!d && typeof d.path === 'string' && typeof d.content === 'string';
});

const writePreviewTarget = computed<FilePreviewRequest | null>(() => {
  if (!isWriteWithContent.value) return null;
  const d = parseArg(props.tool.arg);
  if (!d || typeof d.path !== 'string' || typeof d.content !== 'string') return null;
  return { path: d.path, content: d.content };
});

function toggle(): void {
  if (props.toolDiffPanel) {
    if (isWriteWithContent.value) {
      const target = writePreviewTarget.value;
      if (target) emit('openFile', target);
      return;
    }
    emit('openToolDiff', props.tool.id);
    return;
  }
  if (hasOutput.value) open.value = !open.value;
}
</script>

<template>
  <ToolRow
    :status="status"
    :icon="glyph"
    :name="label"
    :arg="!open ? summary : ''"
    :time="tool.timing"
    :open="open"
    :expandable="canExpand || toolDiffPanel"
    :stacked="stackPosition !== 'single'"
    :stack-position="stackPosition"
    @toggle="toggle"
  >
    <template #trailing>
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
.chip {
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  flex: none;
}
.bb-summary {
  color: var(--color-text);
  border-bottom: 1px dashed var(--color-line);
  padding-bottom: 6px;
  margin-bottom: 6px;
  word-break: break-all;
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
