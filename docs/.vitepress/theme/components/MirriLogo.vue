<script setup lang="ts">
import logoUrl from '../Mirri.png'

withDefaults(defineProps<{ size?: number }>(), { size: 56 })
</script>

<template>
  <img
    class="MirriLogo"
    :src="logoUrl"
    :width="size"
    :height="size"
    alt="Mirri"
  />
</template>

<style scoped>
.MirriLogo {
  display: block;
  object-fit: contain;
}
</style>
