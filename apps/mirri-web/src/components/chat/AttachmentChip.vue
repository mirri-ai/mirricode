<!-- apps/mirri-web/src/components/chat/AttachmentChip.vue -->
<!-- One attachment rendered as a pill chip — the SAME component for the
     composer's pending-attachment strip and for sent messages in the chat
     bubble. Context differences are props, not restyled variants:
       - composer: uploading spinner, error tint, remove button
       - bubble:   plain chip, click opens preview / downloads
     Tile rule: images show a real thumbnail, videos a play glyph, files a
     neutral file icon with the extension badge next to the name. -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import AuthMedia from './AuthMedia.vue';
import Icon from '../ui/Icon.vue';
import Spinner from '../ui/Spinner.vue';
import Tooltip from '../ui/Tooltip.vue';
import type { IconName } from '../../lib/icons';

const props = withDefaults(
  defineProps<{
    kind: 'image' | 'video' | 'file';
    /** Undefined only for pasted media without a name — a generic label shows. */
    name?: string;
    /** Thumbnail source for images (object URL or the authed file URL). */
    url?: string;
    /** When present, AuthMedia fetches image bytes with auth. */
    fileId?: string;
    mediaType?: string;
    size?: number;
    /** Composer: upload in flight — spinner replaces the ext badge. */
    uploading?: boolean;
    /** Composer: upload failed — chip tinted, info icon replaces the badge. */
    error?: boolean;
    /** Composer: show a remove button. */
    removable?: boolean;
    /** Accessible label for the remove button. */
    removeLabel?: string;
    /**
     * Tile presentation for the composer input: images/videos render as a
     * bare thumbnail square (no name, remove button appears on hover), so
     * pasted media reads as part of the input itself. Generic files keep the
     * named pill — a nameless file icon carries no information.
     */
    tile?: boolean;
  }>(),
  { uploading: false, error: false, removable: false, tile: false },
);

const emit = defineEmits<{
  /** Primary action (preview media / download file) — the parent decides. */
  activate: [];
  remove: [];
}>();

const { t } = useI18n();

const ext = computed(() => {
  const fromName = props.name?.match(/\.([A-Za-z0-9]{1,8})$/)?.[1];
  const e = fromName ?? props.mediaType?.split('/')[1]?.split('+')[0];
  return e ? e.toUpperCase() : undefined;
});

const fileIcon = computed<IconName>(() => {
  const e = ext.value ?? '';
  if (/^(txt|md|doc|docx|rtf|log)$/i.test(e)) return 'file-text';
  return 'file';
});

const displayName = computed(() => {
  if (props.name) return props.name;
  if (props.kind === 'image') return t('composer.attachmentImage');
  if (props.kind === 'video') return t('composer.attachmentVideo');
  return t('composer.attachmentFile');
});

function formatSize(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${Math.round(size / 1024)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

const title = computed(() => {
  const parts = [displayName.value];
  if (props.size !== undefined) parts.push(formatSize(props.size));
  return parts.join(' · ');
});
</script>

<template>
  <!-- Tile form (composer, media kinds): a bare thumbnail square — no name,
       remove rides on hover, click opens the preview. Generic files always
       fall back to the named pill below (a nameless file icon says nothing). -->
  <span
    v-if="tile && (kind === 'image' || kind === 'video')"
    class="att-tile2"
    :class="{ 'is-error': error, uploading }"
  >
    <button type="button" class="att-tile2-btn" :aria-label="title" @click="emit('activate')">
      <AuthMedia
        v-if="kind === 'image' && url"
        :url="url"
        kind="image"
        :alt="name"
        :file-id="fileId"
        media-class="att-tile2-img"
      />
      <Icon v-else-if="kind === 'video'" name="play" size="md" />
      <Icon v-else name="image" size="md" />
    </button>
    <!-- Spinner while uploading (composer only) -->
    <span v-if="uploading" class="att-tile2-state"><Spinner size="sm" :label="t('composer.uploading')" /></span>
    <!-- Error indicator (composer only) -->
    <Tooltip v-else-if="error" :text="t('composer.uploadFailed')">
      <span class="att-tile2-state att-tile2-err"><Icon name="info" size="sm" /></span>
    </Tooltip>
    <!-- Remove button (composer only) -->
    <Tooltip v-if="removable" :text="removeLabel ?? t('composer.remove')">
      <button class="att-rm att-tile2-rm" @click="emit('remove')">
        <Icon name="close" size="sm" />
      </button>
    </Tooltip>
  </span>

  <span
    v-else
    class="att-chip"
    :class="{ 'is-error': error, uploading }"
    :title="title"
    :data-kind="kind"
  >
    <button type="button" class="att-activate" :aria-label="title" @click="emit('activate')">
      <span class="att-tile">
        <AuthMedia
          v-if="kind === 'image' && url"
          :url="url"
          kind="image"
          :alt="name"
          :file-id="fileId"
          media-class="att-thumb"
        />
        <Icon v-else-if="kind === 'video'" name="play" size="sm" />
        <Icon v-else-if="kind === 'image'" name="image" size="sm" />
        <Icon v-else :name="fileIcon" size="sm" />
      </span>
      <span class="att-name">{{ displayName }}</span>
    </button>
    <!-- Spinner while uploading (composer only) -->
    <Spinner v-if="uploading" size="sm" :label="t('composer.uploading')" />
    <!-- Error indicator (composer only) -->
    <Tooltip v-else-if="error" :text="t('composer.uploadFailed')">
      <span class="att-err-icon">
        <Icon name="info" size="sm" />
      </span>
    </Tooltip>
    <!-- Remove button (composer only) -->
    <Tooltip v-if="removable" :text="removeLabel ?? t('composer.remove')">
      <button class="att-rm" @click="emit('remove')">
        <Icon name="close" size="sm" />
      </button>
    </Tooltip>
  </span>
</template>

<style scoped>
.att-chip {
  position: relative;
  display: flex;
  align-items: center;
  gap: 5px;
  background: var(--panel2);
  border: 1px solid var(--color-accent-bd);
  border-radius: 4px;
  padding: 3px 6px 3px 4px;
  font-family: var(--mono);
  font-size: calc(var(--ui-font-size) - 3px);
  color: var(--color-text);
  max-width: 220px;
}

.att-chip.is-error {
  border-color: var(--color-danger);
  color: var(--color-danger);
}

.att-activate {
  display: flex;
  align-items: center;
  gap: 5px;
  min-width: 0;
  border: none;
  background: transparent;
  padding: 0;
  cursor: pointer;
  color: inherit;
  font: inherit;
}

.att-activate:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
  border-radius: var(--radius-xs);
}

.att-tile {
  position: relative;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
}

.att-thumb {
  width: 28px;
  height: 28px;
  object-fit: cover;
  border-radius: var(--radius-xs);
  flex-shrink: 0;
  background: var(--line2);
}

.att-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
  min-width: 0;
}

.att-err-icon {
  display: flex;
  align-items: center;
  color: var(--color-danger);
  flex-shrink: 0;
}

.att-rm {
  display: flex;
  align-items: center;
  justify-content: center;
  background: none;
  border: none;
  padding: 1px;
  cursor: pointer;
  color: var(--muted);
  flex-shrink: 0;
}

.att-rm:hover {
  color: var(--color-danger);
}

/* ---- Tile form (composer media) -------------------------------------- */
.att-tile2 {
  position: relative;
  display: inline-flex;
  flex: none;
  width: 64px;
  height: 64px;
  border-radius: var(--radius-md);
  overflow: hidden;
  border: 1px solid var(--line);
  background: var(--panel2);
}
.att-tile2.is-error {
  border-color: var(--color-danger);
}
.att-tile2-btn {
  display: flex;
  width: 100%;
  height: 100%;
  align-items: center;
  justify-content: center;
  border: none;
  background: transparent;
  padding: 0;
  cursor: pointer;
  color: var(--dim);
}
.att-tile2-btn:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
}
.att-tile2-img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
}
/* Uploading / error state — scrim over the thumbnail. */
.att-tile2-state {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  background: color-mix(in srgb, var(--bg) 55%, transparent);
}
.att-tile2-err {
  color: var(--color-danger);
}
/* Remove rides on hover (keyboard users reach it via focus-visible). */
.att-tile2-rm {
  position: absolute;
  top: 2px;
  right: 2px;
  width: 18px;
  height: 18px;
  border-radius: var(--radius-full);
  background: color-mix(in srgb, var(--bg) 82%, transparent);
  color: var(--color-text);
  opacity: 0;
  transition: opacity 0.12s;
}
.att-tile2:hover .att-tile2-rm,
.att-tile2-rm:focus-visible {
  opacity: 1;
}
</style>
