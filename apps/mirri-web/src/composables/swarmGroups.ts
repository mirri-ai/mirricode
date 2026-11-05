import type { AppSubagentPhase, AppSuspendedReason, AppTask } from '../api/types';
import { liveTextTail } from '../lib/memberBlocks';
import type { SwarmTimelineView } from '../lib/swarmCardRows';
import { buildMemberTimeline } from '../lib/swarmCardRows';
import { deriveMemberPhase } from '../lib/taskStatus';

export interface SwarmMember {
  id: string;
  name: string;
  subagentType?: string;
  phase: AppSubagentPhase;
  summary?: string;
  outputLines?: string[];
  /** The member's current text segment tail, derived from the tail narrative
   *  segment (ADR-011 ②) — the swarm row's activity/body source when the
   *  member is streaming text; `undefined` when the tail is reasoning or the
   *  member has no segments (the row falls back to outputLines/summary). */
  text?: string;
  /** 挂起原因：结构化（ADR-011 投影）或旧引擎遗留字符串；渲染经
   *  `suspendedReasonText` 取文本。 */
  suspendedReason?: AppSuspendedReason | string;
  swarmIndex: number;
  /** Resolved model alias the subagent is using. */
  model?: string;
  /** The member's collapsed-row timeline (ADR-012 ①), derived at the member
   *  constructor so every downstream row passes the same view object through
   *  (reference stability comes from `buildMemberTimeline`'s memo, which is
   *  keyed on the member's own `steps` / `segments` arrays — ADR-012 ③).
   *
   *  Derived for terminal members too: a settled member's steps are its
   *  evidence of what it did, not a live-progress signal.
   *
   *  Optional only because a hand-built member (tests, non-task sources) has no
   *  step/segment array to derive from; both live constructors always fill it.
   *  An empty view means "nothing arrived yet" — the row falls back to its
   *  single activity line. */
  timeline?: SwarmTimelineView;
}

export interface SwarmGroup {
  id: string;
  members: SwarmMember[];
  counts: Record<AppSubagentPhase, number>;
}

const PHASES: readonly AppSubagentPhase[] = [
  'queued',
  'working',
  'suspended',
  'awaiting',
  'backoff',
  'completed',
  'failed',
  'aborted',
];

export function phaseForTask(task: AppTask): AppSubagentPhase {
  // 终态权威：terminal status 优先于可能滞后的 subagentPhase（例如被取消的
  // 任务停留在 'working'，否则会永远保持"进行中"并压制 finished swarm card 的
  // 展示）。统一派生点见 ADR-010 ③（lib/taskStatus.ts 的 deriveMemberPhase）。
  return deriveMemberPhase(task);
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

export function buildSwarmGroups(tasks: AppTask[]): SwarmGroup[] {
  const buckets = new Map<string, SwarmMember[]>();

  for (const task of tasks) {
    if (task.kind !== 'subagent' || task.swarmIndex === undefined) continue;
    const key = task.parentToolCallId ?? 'swarm';
    const list = buckets.get(key) ?? [];
    list.push({
      id: task.id,
      name: task.description,
      subagentType: task.subagentType,
      phase: phaseForTask(task),
      summary: task.outputPreview,
      outputLines: task.outputLines,
      // ADR-011 ②：活动行取「最后一个 text 段」的尾部；尾部是推理段或无段时
      // 为 undefined，消费侧回落 outputLines/summary。
      text: liveTextTail(task.segments),
      suspendedReason: task.suspendedReason,
      swarmIndex: task.swarmIndex,
      model: task.model,
      // ADR-012 ①：折叠态时间线在构造点派生一次，下游行直接透传同一视图
      // 对象（引用稳定由 buildMemberTimeline 的 memo 保证）。
      timeline: buildMemberTimeline(task.steps, task.segments),
    });
    buckets.set(key, list);
  }

  return [...buckets.entries()]
    .map(([id, members]) => {
      const sorted = members.toSorted((a, b) => a.swarmIndex - b.swarmIndex || a.id.localeCompare(b.id));
      const counts = emptyCounts();
      for (const member of sorted) counts[member.phase]++;
      return { id, members: sorted, counts };
    })
    .filter((group) => group.members.length > 1)
    .toSorted((a, b) => {
      const ai = a.members.at(0)?.swarmIndex ?? 0;
      const bi = b.members.at(0)?.swarmIndex ?? 0;
      if (ai !== bi) return ai - bi;
      return a.id.localeCompare(b.id);
    });
}

export function countSwarmMembers(groups: SwarmGroup[]): { done: number; total: number } {
  let done = 0;
  let total = 0;
  for (const group of groups) {
    total += group.members.length;
    for (const phase of PHASES) {
      // 终态计入 done：completed / failed / aborted（取消即结束，不再进行中）。
      if (phase === 'completed' || phase === 'failed' || phase === 'aborted') done += group.counts[phase];
    }
  }
  return { done, total };
}

/**
 * Bucket foreground/background subagent tasks by their spawning tool call and
 * return one member list per AgentSwarm call. Unlike buildSwarmGroups this keeps
 * single-member "swarms" (e.g. AgentSwarm used with one resume_agent_ids entry),
 * so the inline card can show a resumed subagent's live progress before the
 * structured result arrives. Also includes subagents without a swarmIndex so a
 * late-synced task still appears (sorted last).
 */
export function swarmMembersByToolCall(tasks: AppTask[]): Map<string, SwarmMember[]> {
  const buckets = new Map<string, SwarmMember[]>();
  for (const task of tasks) {
    if (task.kind !== 'subagent' || !task.parentToolCallId) continue;
    const list = buckets.get(task.parentToolCallId) ?? [];
    list.push({
      id: task.id,
      name: task.description,
      subagentType: task.subagentType,
      phase: phaseForTask(task),
      summary: task.outputPreview,
      outputLines: task.outputLines,
      // ADR-011 ②：inline AgentSwarm 卡的 live 进度行同样取段派生尾部。
      text: liveTextTail(task.segments),
      suspendedReason: task.suspendedReason,
      swarmIndex: task.swarmIndex ?? Number.MAX_SAFE_INTEGER,
      model: task.model,
      // ADR-012 ①：与 buildSwarmGroups 同构派生——两个构造点产出的成员形状
      // 一致，下游行无需区分来源。
      timeline: buildMemberTimeline(task.steps, task.segments),
    });
    buckets.set(task.parentToolCallId, list);
  }
  for (const [key, members] of buckets) {
    buckets.set(
      key,
      members.toSorted((a, b) => a.swarmIndex - b.swarmIndex || a.id.localeCompare(b.id)),
    );
  }
  return buckets;
}
