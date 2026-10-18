<!-- apps/mirri-web/src/components/chat/SupervisionPanel.vue -->
<!-- The supervision L1 panel: member selector (N>1) + the shared MemberFocus
     view. The selector block is standalone so a future AgentOverview roster
     can reuse the same rows (supervision architecture §3, Phase-3 seam).
     N=1 skips the selector entirely — the delegation card and the panel agree
     that cardinality is data, not a different paradigm (SDR-001). -->
<script setup lang="ts">
import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import type { AgentMember, TurnBlock } from '../../types';
import type { SwarmMember } from '../../composables/swarmGroups';
import { MEMBER_PALETTE, assignMemberColors } from '../../lib/memberColor';
import Badge from '../ui/Badge.vue';
import PanelHeader from '../ui/PanelHeader.vue';
import StatusDot from '../ui/StatusDot.vue';
import MemberFocus from './MemberFocus.vue';

const props = defineProps<{
  members: SwarmMember[];
  member: AgentMember | null;
  selectedId?: string;
  /** The settled member's journal narrative (supervision A2) — normalized to
   *  the main transcript's content blocks (ADR-009); null while loading or
   *  when the member is live (streaming) / has no readable journal. */
  history?: TurnBlock[] | null;
  historyLoading?: boolean;
}>();

const emit = defineEmits<{
  close: [];
  select: [id: string];
  stop: [id: string];
  /** 点击等待审批的成员行时直达对话区底部 dock 的审批卡（US5.4）。 */
  focusApproval: [];
  /** Open a modify-tool's diff in the shared side slot (US2.6). */
  openToolDiff: [id: string];
}>();

const { t } = useI18n();

const colors = computed(() => assignMemberColors(props.members.map((m) => m.id)));

function memberColor(id: string): string {
  return colors.value.get(id) ?? MEMBER_PALETTE[0]!;
}

// 聚焦成员是否正等待用户决策——由事件驱动的 awaiting 阶段（挂起事件投影，
// ADR-008 ①/④）经成员 id 关联，不再是 P1 待决审批推导。
const focusedAwaiting = computed(() => {
  const focusedId = props.selectedId ?? props.member?.id;
  if (!focusedId) return false;
  return props.members.find((m) => m.id === focusedId)?.phase === 'awaiting';
});

function handleRowClick(m: SwarmMember): void {
  // 等待用户的成员行点击直达对话区底部 dock；其余行维持选择语义。
  if (m.phase === 'awaiting') {
    emit('focusApproval');
    return;
  }
  emit('select', m.id);
}

const subtitle = computed(() =>
  props.members.length > 1 ? t('tools.swarm.members') : (props.member?.name ?? ''),
);

// The focused member's identity (name + model alias) sits in the panel
// TITLE's leftmost position — the member's detail content below carries no
// repeated identity line (moved out of the focus view per user direction).
const focusTitle = computed(() => props.member?.subagentType ?? t('common.preview'));
</script>

<template>
  <div class="sp">
    <PanelHeader
      :title="focusTitle"
      :subtitle="subtitle"
      :close-label="t('thinking.close')"
      @close="emit('close')"
    >
      <Badge v-if="member?.model" variant="neutral" size="sm">{{ member.model }}</Badge>
      <Badge variant="neutral" size="sm">Sub Agent</Badge>
      <Badge v-if="member" variant="neutral" size="sm">{{ member.phase }}</Badge>
    </PanelHeader>
    <div v-if="members.length > 1" class="sp-selector">
      <button
        v-for="m in members"
        :key="m.id"
        type="button"
        class="sp-row"
        :class="{ active: (selectedId ?? '') === m.id }"
        @click="handleRowClick(m)"
      >
        <i class="sp-dot" :style="{ background: memberColor(m.id) }" />
        <span class="sp-name">{{ m.name }}</span>
        <Badge
          v-if="m.phase === 'awaiting'"
          variant="warning"
          size="sm"
          class="sp-awaiting"
          dot
        >{{ t('tools.swarm.awaitingApproval') }}</Badge>
        <StatusDot class="sp-status" :status="m.phase" />
      </button>
    </div>
    <MemberFocus
      v-if="member"
      :member="member"
      :awaiting="focusedAwaiting"
      :history="history"
      :history-loading="historyLoading"
      @stop="emit('stop', selectedId ?? member.id)"
      @focus-approval="emit('focusApproval')"
      @open-tool-diff="emit('openToolDiff', $event)"
    />
  </div>
</template>

<style scoped>
.sp {
  height: 100%;
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--color-bg);
}
.sp-selector {
  flex: none;
  display: flex;
  flex-direction: column;
  border-bottom: 1px solid var(--color-line);
  background: var(--color-surface);
}
.sp-row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  min-height: 32px;
  padding: 0 12px;
  border: none;
  background: transparent;
  color: var(--color-text);
  font-family: var(--font-ui);
  font-size: var(--text-sm);
  text-align: left;
  cursor: pointer;
  user-select: none;
}
.sp-row:hover,
.sp-row.active {
  background: var(--color-surface-sunken);
}
.sp-row:focus-visible {
  outline: none;
  box-shadow: inset 0 0 0 2px var(--color-accent-soft);
}
.sp-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex: none;
}
.sp-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: var(--weight-medium);
}
.sp-awaiting {
  flex: none;
}
.sp-status {
  flex: none;
}
</style>
