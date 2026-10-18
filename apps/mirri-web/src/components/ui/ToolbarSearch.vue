<!-- apps/mirri-web/src/components/ui/ToolbarSearch.vue -->
<!-- Design-system composed input: the toolbar search field (leading icon +
     borderless inner input) shared by the settings panels. Extracted from
     the per-panel copies (MCP / Archived / Skills) so the sizing and focus
     treatment live in one place. -->
<script setup lang="ts">
import Icon from './Icon.vue';

defineProps<{
  modelValue: string;
  placeholder?: string;
  /** Accessible name for the input (the visible label is icon-only). */
  ariaLabel?: string;
}>();

const emit = defineEmits<{ 'update:modelValue': [value: string] }>();
</script>

<template>
  <label class="toolbar-search">
    <Icon name="search" size="sm" />
    <input
      :value="modelValue"
      :placeholder="placeholder"
      :aria-label="ariaLabel"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
    />
  </label>
</template>

<style scoped>
.toolbar-search {
  flex: 1;
  min-width: 200px;
  height: 36px;
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: 0 var(--space-3);
  border-radius: var(--radius-md);
  border: 1px solid var(--color-line);
  color: var(--color-text-faint);
  font-size: var(--text-sm);
  background: var(--color-surface-raised);
  transition: border-color var(--duration-fast) var(--ease-out), box-shadow var(--duration-fast) var(--ease-out);
}
.toolbar-search:focus-within {
  border-color: var(--color-accent);
  box-shadow: var(--p-focus-ring);
  color: var(--color-text-muted);
}
.toolbar-search input {
  width: 100%;
  border: none;
  outline: none;
  background: transparent;
  font: inherit;
  color: var(--color-text);
}
</style>
