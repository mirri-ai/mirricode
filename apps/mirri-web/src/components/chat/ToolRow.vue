<!-- apps/mirri-web/src/components/chat/ToolRow.vue -->
<script setup lang="ts">
import { computed, inject, nextTick, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import Icon from '../ui/Icon.vue';
import Tooltip from '../ui/Tooltip.vue';
import StatusDot from '../ui/StatusDot.vue';
import { iconSvg } from '../../lib/icons';
import type { BashPrefix } from '../../lib/bashDisplay';

const props = withDefaults(
  defineProps<{
    status: 'running' | 'ok' | 'error' | 'suspended';
    /** Inline-SVG glyph string (toolGlyph), or empty for none. */
    icon?: string;
    name: string;
    arg?: string;
    time?: string;
    /** Hover bubble for `arg` (e.g. the pretty JSON payload); defaults to `arg`. */
    tooltip?: string;
    tooltipMaxWidth?: number;
    /** Payload bubble height cap, in lines — longer content scrolls inside. */
    tooltipLines?: number;
    /** Size the payload bubble to the transcript's reading column. */
    tooltipColumn?: boolean;
    /** Working-directory prefix split off the row text (Bash `cd … &&`), shown
     *  as a badge instead of inline text. */
    prefix?: BashPrefix;
    open?: boolean;
    expandable?: boolean;
    stacked?: boolean;
    stackPosition?: 'single' | 'first' | 'middle' | 'last';
  }>(),
  {
    icon: '',
    arg: '',
    tooltip: '',
    tooltipMaxWidth: 280,
    tooltipLines: 4,
    tooltipColumn: false,
    time: '',
    open: false,
    expandable: false,
    stacked: false,
    stackPosition: 'single',
  },
);

const emit = defineEmits<{ toggle: [] }>();

const { t } = useI18n();

const pinScroll = inject<(el: HTMLElement, ms?: number) => void>('pinScroll', () => {});
const bhEl = ref<HTMLElement | null>(null);

const folderGlyph = computed(() => iconSvg('folder', 'sm'));

/** A workspace badge names the directory; any other directory is compressed to
    `…/dir` so it can never be mistaken for the workspace. The `&&` that joined
    the `cd` to the command renders outside the badge — the badge answers "in
    which directory", not "how the command was stitched together". */
const displayPrefix = computed(() => {
  const p = props.prefix;
  if (!p) return '';
  return p.kind === 'workspace' ? `${p.label}/` : `…/${p.label}/`;
});

/** The badge's accessible name. The visible text is just the directory, so the
    label has to say what the directory means — there is no native `title` here
    because it would stack a second bubble on the payload tooltip. */
const prefixAria = computed(() => {
  const p = props.prefix;
  if (!p) return '';
  return p.kind === 'workspace'
    ? `${t('tools.workspacePrefixHint')}: ${p.path}`
    : `${t('tools.otherDirPrefixHint')}: ${p.path}`;
});

function onHeadClick(): void {
  emit('toggle');
  const el = bhEl.value;
  if (el) nextTick(() => pinScroll(el));
}
</script>

<template>
  <div
    class="box"
    :class="{
      open,
      stacked,
      err: status === 'error',
      'stack-first': stackPosition === 'first',
      'stack-middle': stackPosition === 'middle',
      'stack-last': stackPosition === 'last',
    }"
  >
    <div class="bh" ref="bhEl" @click="onHeadClick">
      <span v-if="icon" class="gl" v-html="icon" aria-hidden="true" />
      <span class="a">{{ name }}</span>
      <Tooltip
        :text="tooltip || arg"
        variant="code"
        :max-width="tooltipMaxWidth"
        :code-lines="tooltipLines"
        :column="tooltipColumn"
      >
        <span v-if="arg || prefix" class="p">
          <span
            v-if="prefix"
            class="ws-badge"
            :class="prefix.kind"
            :aria-label="prefixAria"
          >
            <span class="ws-ic" v-html="folderGlyph" aria-hidden="true" />
            <span class="ws-nm">{{ displayPrefix }}</span>
          </span>
          <span v-if="prefix" class="ws-join" aria-hidden="true">&amp;&amp;</span>
          <span v-if="arg" class="p-cmd">{{ arg }}</span>
        </span>
      </Tooltip>
      <span class="rt">
        <span class="status" :class="status" role="status" :aria-label="status">
          <Icon v-if="status === 'ok'" name="check" size="sm" />
          <Icon v-else-if="status === 'error'" name="close" size="sm" />
          <StatusDot v-else-if="status === 'suspended'" status="suspended" />
          <StatusDot v-else status="running" />
        </span>
        <slot name="trailing" />
        <span v-if="time" class="tm">{{ time }}</span>
      </span>
      <Icon v-if="expandable" class="car" :name="open ? 'chevron-down' : 'chevron-right'" size="sm" />
    </div>
    <div class="bb" :class="{ open }" :inert="!open">
      <div class="bb-pad">
        <slot />
      </div>
    </div>
  </div>
</template>

<style scoped>
.box {
  margin: 0;
  background: var(--color-surface);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
  overflow: hidden;
  transition: border-color var(--duration-base) var(--ease-out);
}
.box.err {
  border-color: color-mix(in srgb, var(--color-danger) 25%, var(--bg));
}

/* Stacked calls: the group owns the outer border + radius, so each row is flat
   and separated only by a top hairline. */
.box.stacked {
  border: none;
  border-radius: 0;
}
.box.stacked .bh {
  border-radius: 0;
}
.box.stack-middle,
.box.stack-last {
  border-top: 1px solid var(--color-line);
}

.bh {
  display: flex;
  align-items: center;
  gap: 8px;
  min-height: 30px;
  padding: 0 11px;
  cursor: pointer;
  font: var(--text-sm) var(--font-mono);
  color: var(--color-text);
}
.box.open .bh,
.bh:hover {
  background: var(--color-surface-sunken);
}
.box.err .bh {
  background: color-mix(in srgb, var(--color-danger) 4%, var(--bg));
}
.box.err .bh:hover {
  background: color-mix(in srgb, var(--color-danger) 7%, var(--bg));
}

.gl {
  display: inline-flex;
  align-items: center;
  color: var(--color-text-faint);
  flex: none;
}
.a {
  color: var(--color-text);
  font-weight: var(--weight-medium);
  flex: none;
}
.p {
  color: var(--color-text-muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
  min-width: 0;
  /* The row text is a badge + command pair; the command keeps the ellipsis, the
     badge never shrinks (it is the shortest, most load-bearing part). */
  display: flex;
  align-items: center;
  gap: 6px;
}
.p-cmd {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}
/* Working-directory badge: quiet enough to read past, explicit enough that the
   `cd` is never hidden. `other` directories keep a `…/` prefix and a muted
   folder glyph so they can't be mistaken for the session workspace. */
.ws-badge {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  flex: none;
  max-width: 40%;
  padding: 0 5px;
  border-radius: var(--radius-sm);
  background: var(--color-surface-sunken);
  border: 1px solid var(--color-line);
  color: var(--color-text-faint);
  font-size: var(--text-xs);
  line-height: 18px;
}
.ws-badge.other {
  color: var(--color-text-muted);
  border-style: dashed;
}
.ws-ic {
  display: inline-flex;
  align-items: center;
  flex: none;
}
.ws-ic :deep(svg) {
  width: 11px;
  height: 11px;
}
.ws-nm {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* The joiner stays outside the badge and fainter than it: it records that the
   model stitched a `cd` onto the command without being read as part of it. */
.ws-join {
  flex: none;
  color: var(--color-text-faint);
  font-size: var(--text-xs);
  opacity: 0.7;
}
.rt {
  margin-left: auto;
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}
.tm {
  color: var(--color-text-faint);
}
:slotted(.chip) {
  color: var(--color-text-muted);
  font-size: var(--text-xs);
  flex: none;
}

/* Status indicator at the right edge of the row: done = green ✓, error = red ✗,
   running = pulsing accent dot. */
.status {
  display: inline-flex;
  align-items: center;
  flex: none;
}
.status.ok {
  color: var(--color-success);
}
.status.error {
  color: var(--color-danger);
}

/* Expanded detail: sunken panel under the row. Opens downward / collapses upward
   via a `grid-template-rows` transition (0fr ↔ 1fr), which animates smoothly in
   every modern browser — unlike `height: auto`, which only interpolates in
   Chromium (via `interpolate-size`) and snaps everywhere else. The inner
   `.bb-pad` needs `min-height: 0` + `overflow: hidden` so the 0fr track can
   collapse fully. */
.bb {
  display: grid;
  grid-template-rows: minmax(0, 0fr);
  overflow: hidden;
  transition: grid-template-rows var(--duration-base) var(--ease-out);
}
.bb.open {
  grid-template-rows: minmax(0, 1fr);
}
.bb-pad {
  min-height: 0;
  overflow: hidden;
  padding: 0 11px 11px;
  background: var(--color-surface-sunken);
  border-top: 1px solid var(--color-line);
  color: var(--color-text);
  font: var(--text-sm)/1.65 var(--font-mono);
  white-space: pre-wrap;
  word-break: break-word;
}

/* Mobile bubble layout: no left gutter indent, softer corners. */
.box.mob {
  margin: 0;
}
</style>
