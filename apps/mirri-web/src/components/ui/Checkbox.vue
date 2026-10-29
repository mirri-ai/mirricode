<!-- apps/mirri-web/src/components/ui/Checkbox.vue -->
<!-- Design-system §03 Checkbox: 17×17, filled accent + white check when on. -->
<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';

import Icon from './Icon.vue';

const props = defineProps<{
  modelValue: boolean;
  disabled?: boolean;
  /** The mixed state (a "select all" over a partially selected set): draws a
   *  dash instead of a check. Reflects the native checkbox attribute. */
  indeterminate?: boolean;
}>();

const emit = defineEmits<{ 'update:modelValue': [value: boolean] }>();

const inputRef = ref<HTMLInputElement | null>(null);

watch(
  () => props.indeterminate,
  (mixed) => {
    if (inputRef.value !== null) inputRef.value.indeterminate = mixed === true;
  },
  { immediate: true },
);

// The immediate watch fires before the input mounts — apply the DOM
// property once more after mount so an initially-mixed checkbox carries
// the correct a11y state.
onMounted(() => {
  if (inputRef.value !== null) inputRef.value.indeterminate = props.indeterminate === true;
});

const showCheck = computed(() => props.modelValue && !props.indeterminate);
</script>

<template>
  <label
    class="ui-check"
    :class="{ 'is-on': modelValue, 'is-mixed': indeterminate, 'is-disabled': disabled }"
  >
    <input
      ref="inputRef"
      class="ui-check__input"
      type="checkbox"
      :checked="modelValue"
      :disabled="disabled"
      @change="emit('update:modelValue', ($event.target as HTMLInputElement).checked)"
    />
    <span class="ui-check__box" aria-hidden="true">
      <Icon v-if="showCheck" name="check" size="md" />
    </span>
    <span v-if="$slots.default" class="ui-check__label"><slot /></span>
  </label>
</template>

<style scoped>
.ui-check { display: inline-flex; align-items: center; gap: var(--space-2); cursor: pointer; }
.ui-check.is-disabled { opacity: 0.5; cursor: not-allowed; }
.ui-check__input { position: absolute; width: 1px; height: 1px; opacity: 0; pointer-events: none; }
.ui-check__box {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 17px;
  height: 17px;
  flex: none;
  border: 1.5px solid var(--color-line-strong);
  border-radius: var(--radius-sm);
  background: var(--color-surface-raised);
  color: var(--color-text-on-accent);
  transition: background var(--duration-base) var(--ease-out),
    border-color var(--duration-base) var(--ease-out);
  position: relative;
}
.ui-check.is-on .ui-check__box { background: var(--color-accent); border-color: var(--color-accent); }
.ui-check__input:focus-visible + .ui-check__box { box-shadow: var(--p-focus-ring); }
/* The mixed state (the "select all" over a partially selected set): an
 * accent-filled box with a dash — the native indeterminate look, driven by
 * the prop (the DOM property itself carries no styling hook in scoped CSS). */
.ui-check.is-mixed .ui-check__box { background: var(--color-accent); border-color: var(--color-accent); }
.ui-check.is-mixed .ui-check__box svg { display: none; }
.ui-check.is-mixed .ui-check__box::after {
  content: '';
  display: block;
  width: 9px;
  height: 2px;
  border-radius: 1px;
  background: var(--color-text-on-accent);
}
.ui-check__box svg { width: 12px; height: 12px; }
.ui-check__label { font-family: var(--font-ui); font-size: var(--text-base); color: var(--color-text); }
</style>
