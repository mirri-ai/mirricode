<!-- apps/mirri-web/src/components/chat/MemberFocus.vue -->
<!-- One member's full work log (supervision L2 view). Extracted from the
     agent detail panel so single-agent delegations, swarm members and (later)
     collaboration team members render through the SAME view — one component,
     three consumers (supervision ADR-003). Reactive content: a still-running
     member keeps streaming here; the list follows the bottom unless the user
     scrolled up. The actions row is a structural slot: stop wires to the
     existing task cancel; pause/directive input mount here once their engine
     capabilities land (ADR-007 — no placeholder controls until then).

     ADR-009: the narrative reuses the main transcript's content-block family
     (ThinkingBlock / Markdown / ToolGroup) instead of a private text dump —
     the task prompt and result render as rich text; the member's process
     renders as ordered thinking / rich-text / tool-card blocks, live from the
     projector's structured steps + streamed text, settled from the journal
     read face (both normalized by lib/memberBlocks.ts). -->
<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AgentMember, TurnBlock } from '../../types';
import { suspendedReasonText } from '../../lib/suspendedReason';
import { createScrollFollow } from '../../lib/scrollFollow';
import { memberBlocksFromLive, settledNarrativeBlocks } from '../../lib/memberBlocks';
import { isMemberLive } from '../../lib/taskStatus';
import { renderBlockKey, renderBlocksFromBlocks, type AssistantRenderBlock } from '../chatTurnRendering';
import Markdown from './Markdown.vue';
import ThinkingBlock from './ThinkingBlock.vue';
import ToolGroup from './ToolGroup.vue';
import ToolCall from './ToolCall.vue';

const props = defineProps<{
  member: AgentMember;
  /** The settled member's journal narrative (supervision A2) — normalized to
   *  content blocks; null while loading or when the member is live. */
  history?: TurnBlock[] | null;
  historyLoading?: boolean;
  /** 该成员正等待用户决策（approval/question）——由事件驱动的 awaiting
   *  阶段填充（ADR-008 ①/④，挂起事件的投影，不再是 P1 待决审批推导）。 */
  awaiting?: boolean;
}>();

const emit = defineEmits<{
  /** Stop this member — the panel routes it to the task cancel. */
  stop: [];
  /** 直达对话区底部 dock 的审批卡（US5.4）。 */
  focusApproval: [];
  /** Open a modify-tool's diff in the shared side slot (US2.6). */
  openToolDiff: [id: string];
}>();

const { t } = useI18n();

// 挂起原因的散文形态（ADR-011 双形态）：结构化 reason 是标记不是散文——
// 取文本为空时不渲染原因行（绝不插值 "[object Object]"）。
const memberReason = computed(() => suspendedReasonText(props.member?.suspendedReason));

/** A still-running member streams its live narrative here; a settled member's
 *  work log comes from the journal history instead (supervision A2). */
const isLive = computed(() => isMemberLive(props.member));

const stoppable = computed(() => isMemberLive(props.member));

// The member's live narrative as content blocks: the reasoning thinking
// block, structured steps (tool cards) in call order, and the streamed text
// as the trailing rich-text block (ADR-009 live path). Rebuilt per change —
// a projection, not state.
const liveBlocks = computed(() =>
  isLive.value
    ? memberBlocksFromLive(props.member.text, props.member.steps, props.member.thinking)
    : [],
);

/** The narrative to render: live blocks while running, the journal's settled
 *  blocks otherwise — the settled fallback decision is extracted into
 *  lib/memberBlocks.ts (settledNarrativeBlocks) so it is testable in
 *  isolation (ADR-015 / RC-2). */
const blocks = computed<TurnBlock[] | null>(() => {
  if (isLive.value) return liveBlocks.value.length > 0 ? liveBlocks.value : null;
  return settledNarrativeBlocks(props.history, liveBlocks.value);
});

/** The render plan over the narrative blocks (consecutive tool cards collapse
 *  into one stack — same rule as the main transcript). */
const renderBlocks = computed<AssistantRenderBlock[]>(() =>
  blocks.value ? renderBlocksFromBlocks(blocks.value) : [],
);

const renderBlockCount = computed(() => renderBlocks.value.length);

/** True when the member is live AND the last block is the streaming text
 *  tail — drives the Markdown streaming flag (typewriter only for the block
 *  that is genuinely growing). */
const streamingLastText = computed(() => {
  if (!isLive.value) return false;
  const last = blocks.value?.at(-1);
  return last !== undefined && last.kind === 'text';
});

function isStreamingBlock(block: AssistantRenderBlock): boolean {
  return streamingLastText.value && block === renderBlocks.value.at(-1) && block.kind === 'text';
}

const bodyEl = ref<HTMLElement | null>(null);
// Follow the bottom as the narrative grows — but only until the user scrolls
// up. Direction-based (not distance-based): a trackpad flick of a few px
// upward stops the follow, because with distance-only checks the next
// streaming delta would pin the view back down before the user's scroll can
// accumulate.
const follow = createScrollFollow(() => bodyEl.value);
watch(
  () =>
    renderBlockCount.value +
    (props.member.text?.length ?? 0) +
    (props.member.steps?.length ?? 0) +
    (props.history?.length ?? 0),
  () => void follow.onContentGrow(),
  { immediate: true },
);

// Wheel/touch arrive BEFORE the browser dispatches `scroll` — stop the follow
// (and cancel any queued pin) so an in-flight streaming delta cannot yank the
// view back down after the user flicked up. Only upward motion counts.
function onBodyWheel(event: WheelEvent): void {
  if (event.defaultPrevented || event.deltaY >= 0) return;
  follow.onUserIntent('up');
}

function onBodyTouch(): void {
  follow.onUserIntent('up');
}
</script>

<template>
  <div
    ref="bodyEl"
    class="mf-body"
    @scroll.passive="follow.onScroll"
    @wheel.passive="onBodyWheel"
    @touchstart.passive="onBodyTouch"
  >
    <!-- 等待用户横幅（ADR-008 ①/④，事件驱动）：成员在等你决策——专属状态
         标记 + 直达对话区底部 dock 的跳转按钮（US5.4）。 -->
    <div v-if="awaiting" class="mf-awaiting">
      <span class="mf-awaiting-label">{{ t('tools.swarm.awaitingApproval') }}</span>
      <button type="button" class="mf-jump" @click="emit('focusApproval')">
        {{ t('tools.swarm.attention') }}
      </button>
    </div>
    <div v-if="memberReason" class="mf-reason">{{ memberReason }}</div>
    <div v-if="member.prompt" class="mf-field">
      <span class="mf-field-label">{{ t('tools.swarm.taskLabel') }}</span>
      <Markdown :text="member.prompt" class="mf-md" />
    </div>
    <div class="mf-actions">
      <button v-if="stoppable" type="button" class="mf-stop" @click="emit('stop')">
        {{ t('tasks.overview.stop') }}
      </button>
      <!-- Reserved slots (supervision ADR-007): pause/resume + directive input
           mount here once the engine capabilities land (P-025/P-026). No
           placeholder controls until then — an entry without a working
           backend is a defect (supervision NFR N2). -->
    </div>
    <!-- The settled member's journal narrative (supervision A2): loaded before
         the narrative area renders. Honest absence — no journal reads as no
         section, never a placeholder. Live members stream straight into the
         narrative below (ADR-009). -->
    <div v-if="historyLoading" class="mf-field">
      <span class="mf-field-label">{{ t('tools.swarm.history') }}</span>
      <div class="mf-field-body mf-hist-muted">{{ t('tools.swarm.historyLoading') }}</div>
    </div>
    <!-- The narrative area (ADR-009): the main transcript's content-block
         family over the member's process — thinking, rich text and tool cards
         in true happening order. Same DOM structure as ChatPane's block loop. -->
    <div v-if="renderBlocks.length > 0" class="mf-narrative">
      <template v-for="(blk, bi) in renderBlocks" :key="renderBlockKey(blk, bi)">
        <ThinkingBlock v-if="blk.kind === 'thinking'" :text="blk.thinking" mobile :foldable="false" />
        <div v-else-if="blk.kind === 'text' && blk.text" class="msg">
          <Markdown :text="blk.text" :streaming="isStreamingBlock(blk)" class="mf-md" />
        </div>
        <ToolGroup
          v-else-if="blk.kind === 'tool-stack'"
          :tools="blk.tools"
          mobile
          tool-diff-panel
          @open-tool-diff="emit('openToolDiff', $event)"
        />
        <ToolCall v-else-if="blk.kind === 'tool'" :tool="blk.tool" mobile tool-diff-panel @open-tool-diff="emit('openToolDiff', $event)" />
      </template>
    </div>
    <div v-if="member.summary" class="mf-field">
      <span class="mf-field-label">{{ t('tools.swarm.resultLabel') }}</span>
      <Markdown :text="member.summary" class="mf-md" />
    </div>
  </div>
</template>

<style scoped>
.mf-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 14px;
  font: var(--text-base)/var(--leading-normal) var(--font-ui);
  color: var(--color-text);
}
.mf-reason {
  color: var(--color-warning);
  margin-bottom: 8px;
}
/* 等待用户横幅：warning 低强调（静态，无动画），与失败/异常视觉区分。 */
.mf-awaiting {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
  padding: 7px 10px;
  border: 1px solid var(--color-warning-bd);
  border-radius: var(--radius-md);
  background: var(--color-warning-soft);
}
.mf-awaiting-label {
  flex: 1;
  min-width: 0;
  color: var(--color-warning);
  font: var(--text-sm) var(--font-ui);
  font-weight: var(--weight-medium);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.mf-jump {
  flex: none;
  background: none;
  border: 1px solid var(--color-warning-bd);
  border-radius: var(--radius-xs);
  color: var(--color-warning);
  font: var(--text-xs) var(--font-ui);
  padding: 1px 7px;
  cursor: pointer;
  white-space: nowrap;
}
.mf-jump:hover {
  background: color-mix(in srgb, var(--color-warning) 8%, transparent);
}
.mf-jump:focus-visible {
  outline: 2px solid var(--color-accent-soft);
  outline-offset: 1px;
}
.mf-field + .mf-field {
  margin-top: 12px;
}
.mf-field-label {
  display: block;
  color: var(--color-text-muted);
  font: var(--text-xs) var(--font-mono);
  text-transform: uppercase;
  letter-spacing: 0.04em;
  margin-bottom: 4px;
}
.mf-field-body {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
.mf-actions {
  display: flex;
  justify-content: flex-end;
  margin-bottom: 8px;
}
.mf-stop {
  background: none;
  border: 1px solid var(--color-line);
  border-radius: var(--radius-xs);
  color: var(--color-danger);
  font: var(--text-xs) var(--font-ui);
  padding: 1px 7px;
  cursor: pointer;
}
.mf-stop:hover {
  background: color-mix(in srgb, var(--color-danger) 8%, transparent);
}
/* Rich text in the panel: match the main conversation's message spacing. */
.mf-md {
  font-size: var(--text-sm);
}
.mf-narrative {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-width: 0;
}
.mf-narrative :deep(.tool-group) {
  min-width: 0;
}
.mf-hist-muted {
  color: var(--color-text-muted);
  font-size: var(--text-sm);
}
</style>
