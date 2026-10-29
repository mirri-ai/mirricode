<!-- apps/mirri-web/src/components/chat/MentionMenu.vue -->
<!-- Popup list of workspace file candidates shown when user types @ in the
     Composer editor (E9): highlighted names, parent dirs, drill arrows,
     breadcrumbs, and index-state footer. Pure rendering — all menu logic
     lives in useMentionMenu. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { iconSvg } from '../../lib/icons';
import type { FileCandidate, SearchFilesResult } from '../../api/types';

const props = withDefaults(
  defineProps<{
    items: FileCandidate[];
    activeIndex: number;
    loading: boolean;
    indexState?: SearchFilesResult['indexState'];
    /** True once the user drilled into a directory (breadcrumb header). */
    drilled?: boolean;
    /** The walked path segments above the current query. */
    crumbs?: string[];
    /** The current @query (header 🔍 line when not drilled). */
    query?: string;
  }>(),
  {
    indexState: 'ready',
    drilled: false,
    crumbs: () => [],
    query: '',
  },
);

const emit = defineEmits<{
  select: [item: FileCandidate, action: 'pick' | 'drill'];
  drill: [item: FileCandidate];
  crumb: [index: number];
  hover: [index: number];
}>();

const { t } = useI18n();

// ---------------------------------------------------------------------------
// File-type glyphs: small line-SVG icons (viewBox 0 0 16 16) keyed off the
// extension. Categories: folder, code, doc/markdown, image, generic.
// Subtle + muted; never an emoji.
// ---------------------------------------------------------------------------

const ICON_FOLDER = iconSvg('folder', 'sm');
const ICON_CODE = iconSvg('code', 'sm');
const ICON_DOC = iconSvg('file-text', 'sm');
const ICON_IMAGE = iconSvg('image', 'sm');
const ICON_GENERIC = iconSvg('file', 'sm');
const ICON_CHEVRON = iconSvg('chevron-right', 'sm');

const CODE_EXT = new Set([
  'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'vue', 'json', 'py', 'go', 'rs',
  'java', 'kt', 'c', 'h', 'cpp', 'cc', 'hpp', 'cs', 'rb', 'php', 'swift',
  'sh', 'bash', 'zsh', 'css', 'scss', 'less', 'html', 'htm', 'xml', 'sql',
  'yaml', 'yml', 'toml', 'lua', 'dart', 'scala', 'clj', 'ex', 'exs',
]);
const DOC_EXT = new Set(['md', 'markdown', 'mdx', 'txt', 'rst', 'adoc', 'pdf', 'doc', 'docx']);
const IMAGE_EXT = new Set(['png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'bmp', 'ico', 'avif']);

function isDirectory(item: FileCandidate): boolean {
  return item.kind === 'directory' || item.path.endsWith('/');
}

function fileIcon(item: FileCandidate): string {
  if (isDirectory(item)) return ICON_FOLDER;
  const base = item.name || item.path.split('/').pop() || item.path;
  const dot = base.lastIndexOf('.');
  const ext = dot > 0 ? base.slice(dot + 1).toLowerCase() : '';
  if (!ext) return ICON_GENERIC;
  if (CODE_EXT.has(ext)) return ICON_CODE;
  if (DOC_EXT.has(ext)) return ICON_DOC;
  if (IMAGE_EXT.has(ext)) return ICON_IMAGE;
  return ICON_GENERIC;
}

// ---------------------------------------------------------------------------
// E9 rendering contracts
// ---------------------------------------------------------------------------

/** The item's parent directory, multi-root aware: the first result's root is
 *  the main root (shown bare); any other root prefixes its own name. */
function parentDir(item: FileCandidate): string {
  const mainRoot = props.items[0]?.root ?? '';
  let parent = item.path.slice(0, item.path.length - (item.name?.length ?? 0));
  parent = parent.replace(/\/+$/, '');
  if (!parent) return item.root && item.root !== mainRoot ? `${item.root}/` : '';
  if (item.root && item.root !== mainRoot) return `${item.root}/${parent}`;
  return parent;
}

/** Split the name into bold/plain segments by matchPositions (char indices,
 *  Array.from-aligned so surrogate pairs count as one). */
function nameSegments(item: FileCandidate): Array<{ text: string; bold: boolean }> {
  const chars = Array.from(item.name ?? '');
  const marks = new Set(item.matchPositions ?? []);
  const segments: Array<{ text: string; bold: boolean }> = [];
  let current = '';
  let currentBold = false;
  for (let i = 0; i < chars.length; i++) {
    const bold = marks.has(i);
    if (bold !== currentBold && current) {
      segments.push({ text: current, bold: currentBold });
      current = '';
    }
    currentBold = bold;
    current += chars[i];
  }
  if (current) segments.push({ text: current, bold: currentBold });
  return segments;
}

/** A query that can only name an absolute/home path — outside the workspace
 *  (uiux §2.1/US9.6): the menu shows the note and no candidates. */
const outsideWorkspace = computed(() => {
  const q = props.query ?? '';
  return q.startsWith('/') || q.startsWith('~/');
});

/** Skeleton rows while the first search of a token is in flight. */
const skeletonRows = [0, 1, 2, 3, 4, 5];

/** The bottom status line: index health, or nothing when ready. */
const footerKey = computed<string | null>(() => {
  if (props.loading) return null;
  if (props.indexState === 'building') return 'mention.indexBuilding';
  if (props.indexState === 'truncated') return 'mention.indexTruncated';
  if (props.indexState === 'degraded') return 'mention.indexDegraded';
  return null;
});

/** Empty-state copy: the outside-workspace note beats no-match, and a fully
 *  degraded empty answer reads as "search unavailable" (uiux §6). */
const emptyKey = computed<string | null>(() => {
  if (props.loading || props.items.length > 0) return null;
  if (outsideWorkspace.value) return 'mention.outsideWorkspace';
  if (props.indexState === 'degraded') return 'mention.searchUnavailable';
  return 'mention.noMatch';
});
</script>

<template>
  <div class="mention-menu" role="listbox" :aria-label="t('mention.searching')">
    <!-- Header: breadcrumb trail when drilled, else the live query -->
    <div v-if="props.drilled || props.crumbs.length > 0" class="mention-header">
      <button
        type="button"
        class="mention-crumb"
        role="option"
        :aria-selected="false"
        @click="emit('crumb', -1)"
      >@</button>
      <template v-for="(crumb, i) in props.crumbs" :key="`${i}-${crumb}`">
        <span class="mention-crumb-sep" aria-hidden="true">/</span>
        <button
          type="button"
          class="mention-crumb"
          role="option"
          :aria-selected="false"
          @click="emit('crumb', i)"
        >{{ crumb }}</button>
      </template>
    </div>
    <div v-else-if="props.query" class="mention-header dim">🔍 {{ props.query }}</div>

    <!-- Skeleton rows: first search of a token, nothing to show yet -->
    <template v-if="props.loading && props.items.length === 0">
      <div v-for="i in skeletonRows" :key="i" class="mention-item mention-skeleton" aria-hidden="true">
        <span class="mention-icon" />
        <span class="mention-sk-name" />
        <span class="mention-sk-path" />
      </div>
    </template>

    <!-- Empty state (not loading, no items) -->
    <div v-else-if="emptyKey" class="mention-state dim">{{ t(emptyKey) }}</div>

    <!-- File items -->
    <div
      v-for="(item, i) in props.items"
      v-else
      :key="item.path"
      class="mention-item"
      :class="{ active: i === props.activeIndex }"
      role="option"
      :aria-selected="i === props.activeIndex"
      @mouseenter="emit('hover', i)"
      @mousedown.prevent="emit('select', item, 'pick')"
    >
      <!-- file-type glyph (line-SVG) -->
      <!-- eslint-disable-next-line vue/no-v-html -->
      <span class="mention-icon" v-html="fileIcon(item)" aria-hidden="true" />
      <span class="mention-name">
        <template v-for="(seg, si) in nameSegments(item)" :key="si">
          <b v-if="seg.bold">{{ seg.text }}</b>
          <template v-else>{{ seg.text }}</template>
        </template>
      </span>
      <span class="mention-path">{{ parentDir(item) }}</span>
      <!-- Folder rows carry an explicit drill affordance (Tab / click). -->
      <button
        v-if="isDirectory(item)"
        type="button"
        class="mention-drill"
        :title="t('mention.drillInto')"
        :aria-label="t('mention.drillInto')"
        @mousedown.prevent
        @click="emit('drill', item)"
      >
        <!-- eslint-disable-next-line vue/no-v-html -->
        <span v-html="ICON_CHEVRON" aria-hidden="true" />
      </button>
    </div>

    <!-- Footer: index health / degradation note -->
    <div v-if="footerKey" class="mention-footer dim">{{ t(footerKey) }}</div>
  </div>
</template>

<style scoped>
/* `[role="listbox"]` raises specificity (0,3,0) so the redesign's surface +
   shadow-md win over any global menu styles. 8 rows (~30px each) then scroll. */
.mention-menu[role="listbox"] {
  position: absolute;
  bottom: calc(100% + 4px);
  left: 0;
  right: 0;
  padding: var(--space-1);
  background: var(--color-surface-raised);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-sm);
  z-index: var(--z-dropdown);
  max-height: 240px;
  overflow-y: auto;
}

.mention-state,
.mention-header,
.mention-footer {
  padding: 6px 12px;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
}

.mention-header {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 2px;
  color: var(--color-text-muted);
  border-bottom: 1px solid var(--color-line);
  margin-bottom: var(--space-1);
  padding: 4px 8px 6px;
}

.mention-footer {
  border-top: 1px solid var(--color-line);
  margin-top: var(--space-1);
  padding: 4px 12px 6px;
  font-size: var(--text-xs);
}

.mention-crumb {
  border: none;
  background: none;
  padding: 1px 4px;
  border-radius: var(--radius-xs);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  color: var(--color-accent);
  cursor: pointer;
}

.mention-crumb:hover {
  background: var(--color-accent-soft);
}

.mention-crumb-sep {
  color: var(--color-text-faint);
}

.dim {
  color: var(--color-text-muted);
}

.mention-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  cursor: pointer;
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  border-radius: var(--radius-sm);
}

.mention-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  color: var(--color-text-faint);
  flex-shrink: 0;
}

/* Pin every glyph to the same 14px box so rows line up regardless of icon kind. */
.mention-icon :deep(svg),
.mention-drill :deep(svg) {
  width: 13px;
  height: 13px;
  display: block;
}

.mention-item:hover .mention-icon,
.mention-item.active .mention-icon {
  color: var(--color-text-muted);
}

.mention-item:hover {
  background: var(--color-surface-sunken);
}
.mention-item.active {
  background: var(--color-accent-soft);
}

.mention-name {
  color: var(--color-text);
  font-weight: 500;
  flex-shrink: 1;
  min-width: 80px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.mention-name b {
  /* Match highlight — same ink, accent color, medium weight (the scale's
     max allowed; a heavier weight would need an off-scale value). */
  font-weight: 500;
  color: var(--color-accent);
}

.mention-path {
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
  min-width: 0;
}

/* Folder drill affordance — trailing chevron. */
.mention-drill {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  flex: none;
  border: none;
  background: none;
  padding: 0;
  border-radius: var(--radius-xs);
  color: var(--color-text-faint);
  cursor: pointer;
}

.mention-drill:hover {
  background: var(--color-surface-sunken);
  color: var(--color-accent);
}

/* Skeleton rows (first search of a token): pulsing bars in the row layout. */
.mention-skeleton {
  cursor: default;
  pointer-events: none;
}

.mention-skeleton .mention-sk-name,
.mention-skeleton .mention-sk-path {
  height: 10px;
  border-radius: var(--radius-xs);
  background: var(--color-surface-sunken);
  animation: mention-sk-pulse 1.2s ease-in-out infinite;
}

.mention-sk-name {
  width: 96px;
  flex-shrink: 0;
}

.mention-sk-path {
  width: 40%;
  flex: 1;
}

.mention-skeleton:nth-child(odd) .mention-sk-path {
  width: 55%;
}

@keyframes mention-sk-pulse {
  0%,
  100% {
    opacity: 0.55;
  }
  50% {
    opacity: 1;
  }
}

/* ---- Menu surface defaults ---- */
.mention-menu { border-radius: var(--radius-lg); box-shadow: var(--sh); }
.mention-state { font-family: var(--sans); }
</style>
