// apps/mirri-web/src/lib/delegationCard.ts
// Aggregate facts for the unified delegation card head (supervision L0).
// Pure derivation over live members + the parsed swarm result — every number
// maps 1:1 to a real event (honest-progress rule, supervision SDR-003): no
// percentages, no ETA. N=1 is just "omit the aggregate structures".
import type { AppSubagentPhase } from '../api/types';
import type { SwarmMember } from '../composables/swarmGroups';
import type { SwarmResult } from './parseSwarmResult';
import { buildSwarmCardRows, swarmMemberActivity } from './swarmCardRows';

export interface DelegationSummary {
  // 四档聚合（ADR-008 ②）：attention = 存在等待成员且无终态——等待用户是
  // 事实信号，不计入异常；与 running / ok / error 并列。
  aggregateStatus: 'running' | 'attention' | 'ok' | 'error';
  done: number;
  total: number;
  inProgress: number;
  counts: Record<AppSubagentPhase, number>;
  /** The sole member's latest one-line activity (N=1); '' for N>1. */
  activityLine: string;
}

function emptyCounts(): Record<AppSubagentPhase, number> {
  return {
    queued: 0,
    working: 0,
    suspended: 0,
    awaiting: 0,
    backoff: 0,
    completed: 0,
    failed: 0,
    aborted: 0,
  };
}

export function delegationSummary(input: {
  toolStatus: 'running' | 'ok' | 'error';
  members: SwarmMember[];
  result: SwarmResult | null;
  /** 等待用户（approval/question，事件驱动）成员的活动行 i18n 文案。 */
  awaitingLabel?: string;
}): DelegationSummary {
  const rows = buildSwarmCardRows(input.members, input.result, input.awaitingLabel);
  const counts = emptyCounts();
  for (const row of rows) counts[row.phase] += 1;
  // 终态计入 done（completed / failed / aborted）；awaiting/backoff 计入
  // inProgress——backoff 是限流退避的运行子状态（ADR-011），awaiting 等用户。
  const done = counts.completed + counts.failed + counts.aborted;
  const inProgress =
    counts.working + counts.suspended + counts.queued + counts.awaiting + counts.backoff;
  // attention = 存在等待用户成员且无终态（ADR-008 ②）。awaiting 由引擎挂起
  // 事件驱动（ADR-008 ①/④），本身即非终态，是等待事实，不计入异常；
  // backoff 不是等用户，不进 attention。终态或工具错误优先按 error 呈现——
  // 等待不掩盖真实失败。
  const hasAwaitingMember = input.members.some((m) => m.phase === 'awaiting');
  const hasTerminalMember = input.members.some(
    (m) => m.phase === 'completed' || m.phase === 'failed' || m.phase === 'aborted',
  );
  const aggregateStatus: DelegationSummary['aggregateStatus'] =
    input.toolStatus === 'error' || counts.failed > 0
      ? 'error'
      : hasAwaitingMember && !hasTerminalMember
        ? 'attention'
        : input.toolStatus === 'running'
          ? 'running'
          : 'ok';
  const activityLine =
    input.members.length === 1 ? swarmMemberActivity(input.members[0]!, input.awaitingLabel) : '';
  return { aggregateStatus, done, total: rows.length, inProgress, counts, activityLine };
}
