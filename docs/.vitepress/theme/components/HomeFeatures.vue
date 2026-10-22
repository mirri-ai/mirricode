<script setup lang="ts">
import { useData } from 'vitepress'
import { computed } from 'vue'
import { resolveLink } from '../link'

const { lang, frontmatter, page } = useData()
const isZh = computed(() => lang.value.startsWith('zh'))

interface Feature {
  icon: string
  title: string
  details: string
  link: string
}

// The homepage cards are the `features` list in each locale's index.md. Keep
// this component free of copy so editing the markdown is enough to change the
// page.
const features = computed<Feature[]>(() => frontmatter.value.features ?? [])

const featuresTitle = computed(() => isZh.value ? '能力一览' : 'Capabilities')
const featuresLede = computed(() => isZh.value
  ? '关键能力开箱即用，扩展点按需接入，围绕你的工作流展开。'
  : 'The essentials work out of the box, with extension points to shape the workflow around you.')

const ctaText = computed(() => isZh.value ? '了解' : 'Learn more')

function hrefOf(link: string): string {
  return resolveLink(link, page.value.relativePath)
}
</script>

<template>
  <section class="MirriHome__section MirriFeatures">
    <h2 class="MirriHome__sectionTitle">{{ featuresTitle }}</h2>
    <p class="MirriHome__sectionLede">{{ featuresLede }}</p>
    <div class="MirriFeatures__grid">
      <a
        v-for="f in features"
        :key="f.title"
        class="MirriFeatures__card"
        :href="hrefOf(f.link)"
      >
        <div class="MirriFeatures__icon" aria-hidden="true">{{ f.icon }}</div>
        <h3 class="MirriFeatures__title">{{ f.title }}</h3>
        <p class="MirriFeatures__desc">{{ f.details }}</p>
        <span class="MirriFeatures__cta">
          {{ ctaText }}
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M6 3l5 5-5 5" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </span>
      </a>
    </div>
  </section>
</template>

<style scoped>
/* === Feature cards (clickable) === */
.MirriFeatures__grid {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 20px;
}

@media (max-width: 1024px) {
  .MirriFeatures__grid {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}
@media (max-width: 640px) {
  .MirriFeatures__grid {
    grid-template-columns: 1fr;
  }
}

.MirriFeatures__card {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  padding: 28px 24px 26px;
  border-radius: var(--mirri-radius-card);
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg);
  color: var(--vp-c-text-1);
  text-decoration: none;
  transition: transform var(--mirri-transition), border-color var(--mirri-transition),
              box-shadow var(--mirri-transition), background var(--mirri-transition);
  overflow: hidden;
}

.MirriFeatures__card::before {
  content: '';
  position: absolute;
  inset: 0;
  background: var(--mirri-brand-gradient-soft);
  opacity: 0;
  transition: opacity var(--mirri-transition);
  pointer-events: none;
  border-radius: inherit;
}

.MirriFeatures__card:hover {
  transform: translateY(-3px);
  border-color: var(--vp-c-brand-1);
  box-shadow: var(--vp-shadow-3);
}
.MirriFeatures__card:hover::before {
  opacity: 1;
}

.MirriFeatures__icon {
  position: relative;
  z-index: 1;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 44px;
  height: 44px;
  border-radius: 12px;
  background: var(--mirri-brand-soft);
  font-size: 22px;
  margin-bottom: 18px;
}

.MirriFeatures__title {
  position: relative;
  z-index: 1;
  font-size: 18px;
  font-weight: 700;
  letter-spacing: -0.015em;
  margin: 0 0 8px;
  color: var(--vp-c-text-1);
}

.MirriFeatures__desc {
  position: relative;
  z-index: 1;
  font-size: 14.5px;
  line-height: 1.6;
  color: var(--vp-c-text-2);
  margin: 0 0 20px;
}

.MirriFeatures__cta {
  position: relative;
  z-index: 1;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 14px;
  font-weight: 600;
  color: var(--vp-c-brand-1);
  margin-top: auto;
  transition: transform var(--mirri-transition);
}

.MirriFeatures__card:hover .MirriFeatures__cta {
  transform: translateX(3px);
}
</style>
