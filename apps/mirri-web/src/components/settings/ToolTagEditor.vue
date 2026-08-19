<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AppToolDescriptor, AppMcpServer } from '../../api/types';
import { isCompositionKeyEvent } from '../../lib/keyboard';
import Icon from '../ui/Icon.vue';

const { t } = useI18n();

const props = defineProps<{
  modelValue: string[];
  availableTools: AppToolDescriptor[];
  mcpServers: AppMcpServer[];
}>();

const emit = defineEmits<{
  'update:modelValue': [value: string[]];
}>();

const input = ref('');
const inputEl = ref<HTMLInputElement>();
const isFocused = ref(false);
const panelStyle = ref<Record<string, string>>({});
let blurTimer: ReturnType<typeof setTimeout> | undefined;

function updatePanelPosition(): void {
  const el = inputEl.value;
  if (!el) return;
  const rect = el.getBoundingClientRect();
  panelStyle.value = {
    position: 'fixed',
    top: `${rect.bottom + 4}px`,
    left: `${rect.left}px`,
    width: `${rect.width}px`,
  };
}

function onFocus(): void {
  if (blurTimer) {
    clearTimeout(blurTimer);
    blurTimer = undefined;
  }
  isFocused.value = true;
  nextTick(() => updatePanelPosition());
}

function onBlur(): void {
  // Delay to allow click on suggestion to register before hiding
  blurTimer = setTimeout(() => {
    isFocused.value = false;
  }, 150);
}

// Recompute position on scroll/resize while focused
function onScrollOrResize(): void {
  if (isFocused.value) updatePanelPosition();
}

onMounted(() => {
  window.addEventListener('scroll', onScrollOrResize, true);
  window.addEventListener('resize', onScrollOrResize);
});

onUnmounted(() => {
  if (blurTimer) clearTimeout(blurTimer);
  window.removeEventListener('scroll', onScrollOrResize, true);
  window.removeEventListener('resize', onScrollOrResize);
});

interface Suggestion {
  value: string;
  label: string;
  description: string;
  group: 'builtin' | 'skill' | 'mcp-server' | 'mcp-tool';
  toolCount?: number;
}

const suggestions = computed<Suggestion[]>(() => {
  const result: Suggestion[] = [];

  // Builtin tools
  for (const tool of props.availableTools) {
    if (tool.source === 'builtin') {
      result.push({
        value: tool.name,
        label: tool.name,
        description: tool.description,
        group: 'builtin',
      });
    }
  }

  // Skill tools
  for (const tool of props.availableTools) {
    if (tool.source === 'skill') {
      result.push({
        value: tool.name,
        label: tool.name,
        description: tool.description,
        group: 'skill',
      });
    }
  }

  // MCP servers (all tools from server)
  for (const server of props.mcpServers) {
    result.push({
      value: `mcp__${server.id}__*`,
      label: server.name,
      description: t('agents.toolMcpServerAllTools'),
      group: 'mcp-server',
      toolCount: server.toolCount,
    });
  }

  // MCP individual tools
  for (const tool of props.availableTools) {
    if (tool.source === 'mcp') {
      result.push({
        value: tool.name,
        label: tool.name,
        description: tool.description,
        group: 'mcp-tool',
      });
    }
  }

  return result;
});

const filteredSuggestions = computed<Suggestion[]>(() => {
  const query = input.value.trim().toLowerCase();
  const filtered = suggestions.value.filter(
    (s) => !props.modelValue.includes(s.value),
  );
  if (query.length === 0) return filtered.slice(0, 20);
  return filtered
    .filter(
      (s) =>
        s.value.toLowerCase().includes(query) ||
        s.label.toLowerCase().includes(query) ||
        s.description.toLowerCase().includes(query),
    )
    .slice(0, 20);
});

function addTool(value: string): void {
  if (props.modelValue.includes(value)) return;
  emit('update:modelValue', [...props.modelValue, value]);
  input.value = '';
}

function removeTool(index: number): void {
  emit('update:modelValue', props.modelValue.filter((_, i) => i !== index));
}

function onKeydown(e: KeyboardEvent): void {
  if (isCompositionKeyEvent(e)) return;
  if (e.key === 'Backspace' && input.value.length === 0 && props.modelValue.length > 0) {
    removeTool(props.modelValue.length - 1);
  } else if (e.key === 'Enter' && input.value.trim().length > 0) {
    e.preventDefault();
    // If exact match in filtered, add it; otherwise add raw input
    const exact = filteredSuggestions.value.find(
      (s) => s.value.toLowerCase() === input.value.trim().toLowerCase(),
    );
    addTool(exact?.value ?? input.value.trim());
  }
}

const groupLabel: Record<Suggestion['group'], string> = {
  builtin: t('agents.toolGroupBuiltin'),
  skill: t('agents.toolGroupSkill'),
  'mcp-server': t('agents.toolGroupMcpServer'),
  'mcp-tool': t('agents.toolGroupMcpTool'),
};
</script>

<template>
  <div class="tool-editor">
    <div class="tags">
      <span v-for="(tool, i) in modelValue" :key="i" class="tag">
        {{ tool }}
        <button class="tag-remove" type="button" @click="removeTool(i)">
          <Icon name="close" size="sm" />
        </button>
      </span>
    </div>
    <input
      ref="inputEl"
      v-model="input"
      class="tool-input"
      :placeholder="t('agents.toolsSearchPlaceholder')"
      spellcheck="false"
      @focus="onFocus"
      @blur="onBlur"
      @keydown="onKeydown"
    />
    <Teleport to="body">
      <!-- No tools available at all -->
      <div
        v-if="isFocused && suggestions.length === 0"
        class="tool-suggestions tool-suggestions-empty"
        :style="panelStyle"
      >
        <li class="tool-suggestion-empty">
          {{ t('agents.toolsNoSuggestions') }}
        </li>
      </div>
      <!-- Tools available but search filtered them all out -->
      <ul
        v-else-if="isFocused && filteredSuggestions.length === 0"
        class="tool-suggestions"
        :style="panelStyle"
      >
        <li class="tool-suggestion-empty">
          {{ t('agents.toolsNoMatches') }}
        </li>
      </ul>
      <ul
        v-else-if="isFocused && filteredSuggestions.length > 0"
        class="tool-suggestions"
        :style="panelStyle"
      >
        <li
          v-for="s in filteredSuggestions"
          :key="s.value"
          class="tool-suggestion"
          @mousedown.prevent="addTool(s.value)"
        >
          <div class="tool-suggestion-header">
            <span class="tool-suggestion-label">{{ s.label }}</span>
            <span class="tool-suggestion-badges">
              <span v-if="s.toolCount !== undefined" class="tool-suggestion-count">{{ s.toolCount }}</span>
              <span class="tool-suggestion-tag" :data-group="s.group">{{ groupLabel[s.group] }}</span>
            </span>
          </div>
          <span v-if="s.value !== s.label" class="tool-suggestion-value">{{ s.value }}</span>
          <span v-if="s.description" class="tool-suggestion-desc">{{ s.description }}</span>
        </li>
      </ul>
    </Teleport>
  </div>
</template>

<style scoped>
.tool-editor {
  position: relative;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  padding: var(--space-2);
  background: var(--color-surface-raised);
}

.tags {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-1);
  margin-bottom: var(--space-1);
}

.tag {
  display: inline-flex;
  align-items: center;
  gap: var(--space-1);
  padding: 2px 8px;
  border-radius: var(--radius-sm);
  background: var(--color-accent-soft);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text);
}

.tag-remove {
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  background: none;
  cursor: pointer;
  padding: 0;
  color: var(--color-text-muted);
}
.tag-remove:hover { color: var(--color-danger); }

.tool-input {
  width: 100%;
  border: none;
  outline: none;
  background: transparent;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text);
}

/* Teleported to <body>, so styles must be global */
:global(.tool-suggestions) {
  z-index: 9999;
  max-height: 240px;
  overflow-y: auto;
  margin: 0;
  padding: 0;
  list-style: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  background: var(--color-surface-raised);
  box-shadow: var(--shadow-md);
}

:global(.tool-suggestion) {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: var(--space-2) var(--space-3);
  cursor: pointer;
  border-bottom: 1px solid var(--color-line);
}
:global(.tool-suggestion:last-child) { border-bottom: none; }
:global(.tool-suggestion:hover) { background: var(--color-surface-sunken); }

:global(.tool-suggestion-header) {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-2);
}

:global(.tool-suggestion-label) {
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  font-weight: var(--weight-medium);
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

:global(.tool-suggestion-badges) {
  display: flex;
  align-items: center;
  gap: var(--space-1);
  flex-shrink: 0;
}

:global(.tool-suggestion-count) {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  font-weight: var(--weight-medium);
  color: var(--color-text-muted);
  background: var(--color-surface-sunken);
  border-radius: var(--radius-sm);
  padding: 1px 6px;
  min-width: 20px;
  text-align: center;
}

:global(.tool-suggestion-tag) {
  font-family: var(--font-ui);
  font-size: 10px;
  font-weight: var(--weight-medium);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  white-space: nowrap;
}
:global(.tool-suggestion-tag[data-group="builtin"]) {
  color: var(--color-success);
  background: var(--color-success-soft);
}
:global(.tool-suggestion-tag[data-group="mcp-server"]) {
  color: var(--color-accent);
  background: var(--color-accent-soft);
}
:global(.tool-suggestion-tag[data-group="mcp-tool"]) {
  color: var(--color-done);
  background: var(--color-done-soft);
}

:global(.tool-suggestion-value) {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

:global(.tool-suggestion-desc) {
  font-family: var(--font-ui);
  font-size: var(--text-xs);
  color: var(--color-text-faint);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

:global(.tool-suggestion-empty) {
  padding: var(--space-2) var(--space-3);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-text-muted);
  list-style: none;
}
</style>
