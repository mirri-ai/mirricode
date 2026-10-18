// apps/mirri-web/src/lib/swarmCardRows.ts
// Build the accordion row model for the AgentSwarm inline tool card. Pure
// function of live members (AppTask store, real-time phase) and the parsed
// `<agent_swarm_result>` payload (terminal result) — kept in plain TS so it can
// be unit-tested without mounting the component.

import type { AppSubagentPhase } from '../api/types';
import type { SwarmMember } from '../composables/swarmGroups';
import type { SwarmResult, SwarmResultSubagent } from './parseSwarmResult';
import { suspendedReasonText } from './suspendedReason';

export interface SwarmCardRow {
  id: string;
  name: string;
  activity: string;
  phase: AppSubagentPhase;
  body: string;
  model?: string;
}

function lastNonEmptyLine(text: string | undefined): string {
  if (!text) return '';
  return text.split('\n').map((l) => l.trimEnd()).filter(Boolean).at(-1) ?? '';
}

export function swarmMemberActivity(member: SwarmMember, awaitingLabel = ''): string {
  // 事件驱动的 awaiting（ADR-008 ①，结构化 approval/question 挂起）：等待
  // 用户决策的活动行固定为等待文案；backoff（限流退避）不是等用户，活动行
  // 回落到原因/进度。
  if (member.phase === 'awaiting') return awaitingLabel;
  // Prefer streamed subagent text so a still-composing agent shows its latest
  // line instead of an empty / last-summary row.
  return (
    ((suspendedReasonText(member.suspendedReason) ||
    lastNonEmptyLine(member.text)) ||
    lastNonEmptyLine(member.outputLines?.join('\n')) ||
    member.summary) ??
    ''
  );
}

function swarmMemberBody(member: SwarmMember): string {
  // 结构化 reason 是标记不是散文（suspendedReasonText 返回 ''），body 回落到
  // 流式文本——绝不把对象插值为 "[object Object]"。
  const reason = suspendedReasonText(member.suspendedReason);
  if (reason) return reason;
  if (member.text) return member.text;
  if (member.outputLines && member.outputLines.length > 0) return member.outputLines.join('\n');
  return member.summary ?? '';
}

function outcomeToPhase(outcome: string): AppSubagentPhase {
  if (outcome === 'completed') return 'completed';
  if (outcome === 'failed') return 'failed';
  // 用户取消分档（ADR-008 ③）：aborted 不再混入 failed。
  if (outcome === 'aborted') return 'aborted';
  return 'working';
}

function resultRow(sub: SwarmResultSubagent, index: number): SwarmCardRow {
  return {
    id: sub.agentId ?? sub.item ?? `result-${index}`,
    name: sub.item ?? `subagent ${index + 1}`,
    activity: sub.body.split('\n')[0] ?? '',
    phase: outcomeToPhase(sub.outcome),
    body: sub.body,
  };
}

/**
 * Whether a live member already accounts for a result subagent. Members may
 * come from the projector (task id / description) while the result references
 * agent_id / item; the two ids don't always match, so also treat item ⊆
 * description as a match.
 */
function memberCoversResult(member: SwarmMember, sub: SwarmResultSubagent): boolean {
  if (sub.agentId && member.id === sub.agentId) return true;
  if (sub.item && member.name.includes(sub.item)) return true;
  return false;
}

/**
 * Merge the live members with the agent_swarm_result payload into one row list.
 *
 * - Members are authoritative while present (real-time phase + streamed text).
 * - When a parsed result is also present, append result rows that no member
 *   covers — e.g. interrupted swarms emit `state="not_started"` /
 *   `outcome="aborted"` entries for items that never spawned a task, which
 *   would otherwise be invisible until a refresh dropped the live tasks.
 * - When no members are present (post-refresh), fall back to result-only rows.
 */
export function buildSwarmCardRows(
  members: SwarmMember[],
  result: SwarmResult | null,
  // 等待用户（approval/question，事件驱动）成员的活动行文案——组件以 i18n
  // 标签传入；纯 TS 层不做本地化。
  awaitingLabel = '',
): SwarmCardRow[] {
  const memberRows = members.map((m) => ({
    id: m.id,
    name: m.name,
    activity: swarmMemberActivity(m, awaitingLabel),
    phase: m.phase,
    body: swarmMemberBody(m),
    model: m.model,
  }));
  if (!result) return memberRows;

  const resultOnly = result.subagents
    .filter(
      (sub) =>
        (sub.outcome === 'aborted' || sub.state === 'not_started') &&
        !members.some((m) => memberCoversResult(m, sub)),
    )
    .map((sub, i) => resultRow(sub, i));

  return memberRows.length > 0 ? [...memberRows, ...resultOnly] : result.subagents.map((s, i) => resultRow(s, i));
}
