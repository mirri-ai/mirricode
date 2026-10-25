import type { AppTask } from '../api/types';
import type { SwarmResultSubagent } from './parseSwarmResult';
import { applyTerminalToTask, isTerminalStatus } from './taskStatus';

/** ADR-010 ④ keep-live 对账依赖：由调用点（useTaskPoller）注入的会话事实，
 * 让折合层在 REST 已注销成员行时按父调用/蜂群结果收敛，而不是永久保留幽灵行。 */
export interface KeepLiveSubagentsOpts {
  /** 父 AgentSwarm/Agent 工具调用是否已有 toolResult 落盘（会话转录中）。 */
  parentCallSettled?: (toolCallId: string) => boolean;
  /** 从已落盘的蜂群结果中取该成员的逐成员终态（按 agentId 匹配）。 */
  swarmOutcomeFor?: (toolCallId: string, agentId: string) => SwarmResultSubagent | undefined;
  /** 会话转录是否已加载（快照/resync 完成）；false = 未加载，保持 keep 等下一轮。 */
  transcriptLoaded?: boolean;
}

/**
 * Append the live-only swarm subagents that a fresh REST `/tasks` list does not
 * contain.
 *
 * REST `/tasks` lists the session TaskManager's rows, keyed by task id. Under
 * the rust engine that manager now also holds foreground swarm members
 * (`TaskKind::Agent` registrations of a running swarm), so REST DOES return
 * foreground swarm subagents — each row keyed by its task id and carrying the
 * agent's id as `agent_id` (mapped to `agentId`). The WS event stream keys the
 * same agent by agent id, so a plain replace of `tasksBySession` with the REST
 * list would temporarily drop the live rows and the next event would re-add
 * them, flickering the swarm/subagent cards (and their live "currently doing"
 * line) about once per second.
 *
 * Fold each REST copy into the WS-owned row so one agent does not surface as
 * two rows with diverging status. The join key is the live row's
 * `backgroundTaskId` (background subagents: the WS row learns the REST task id
 * from the `task.started` registration, which equals the REST row's id)
 * falling back to its `agentId` (foreground swarm members: the REST row id is
 * an unrelated task id, so the match is `rest.agentId === live.agentId` — the
 * same join key the `taskCompleted` reducer uses). The WS row always carries
 * `agentId` from projection (`patchSubagent` and the `subagent.spawned`
 * handler both set it to the sub-agent id, and the `taskCreated` reducer
 * replacement preserves it), so the foreground join key is always present and
 * every agent folds to exactly one row for the whole run. REST still corrects
 * a terminal status the WS row may have missed while disconnected, but is
 * otherwise not a second state source: the WS row owns
 * id/description/phase while running.
 */
export function keepLiveSubagents(
  restBased: AppTask[],
  existing: AppTask[],
  opts?: KeepLiveSubagentsOpts,
): AppTask[] {
  const restIds = new Set(restBased.map((t) => t.id));
  const liveSubagents = existing.filter((t) => t.kind === 'subagent' && !restIds.has(t.id));
  if (liveSubagents.length === 0) return restBased;
  const restById = new Map(restBased.map((t) => [t.id, t] as const));
  const restByAgentId = new Map(
    restBased.flatMap((t) => (t.agentId !== undefined ? [[t.agentId, t] as const] : [])),
  );
  const foldedRestIds = new Set<string>();
  const merged = liveSubagents.map((live) => {
    const rest =
      live.backgroundTaskId !== undefined
        ? restById.get(live.backgroundTaskId)
        : live.agentId !== undefined
          ? restByAgentId.get(live.agentId)
          : undefined;
    if (rest === undefined) {
      // ADR-010 ④ keep-live 收敛：REST 已注销 + 行仍 running 的幽灵行按优先级收敛。
      if (isTerminalStatus(live.status)) return live;
      // S-3：转录未加载（调用点未提供 transcriptLoaded 或显式 false）时维持 keep，
      // 等下一轮 poll——避免加载竞态把真完成成员误标 aborted。
      if (opts?.transcriptLoaded === false) return live;
      // 旧调用点（无对账依赖）维持保留语义。
      if (opts?.parentCallSettled === undefined) return live;
      // ④a：蜂群结果的逐成员终态最权威（成员可能先于父调用异常结束）。
      const member =
        live.parentToolCallId !== undefined && live.agentId !== undefined
          ? opts.swarmOutcomeFor?.(live.parentToolCallId, live.agentId)
          : undefined;
      if (member) {
        const status =
          member.outcome === 'completed'
            ? 'completed'
            : member.outcome === 'aborted'
              ? 'cancelled'
              : 'failed';
        return { ...applyTerminalToTask(live, status), outputPreview: member.body || live.outputPreview };
      }
      // ④b：父调用已定局 → 按父调用正常完成收口。
      // ④c / N-2：无 parentToolCallId 或父调用未落 → 父调用不再产生结果，落 aborted。
      const settled = live.parentToolCallId !== undefined && opts.parentCallSettled(live.parentToolCallId);
      return settled ? applyTerminalToTask(live, 'completed') : applyTerminalToTask(live, 'cancelled');
    }
    foldedRestIds.add(rest.id);
    // True when the fold — not the event stream — is what makes the row terminal.
    const restCompletesLiveRow = live.status === 'running' && rest.status !== 'running';
    return {
      ...live,
      // Terminal-stickiness: never let a lagging poll flip a finished row back
      // to running, but let REST complete a row whose finish event was missed.
      status: live.status === 'running' ? rest.status : live.status,
      // toAgentMember prefers subagentPhase over status, so sync it too —
      // otherwise the detail panel badge keeps showing a stale Working/Queued.
      // 'cancelled' maps to 'aborted' — the phase enum's terminal for
      // user-interrupted work (the dock styles it distinctly from failed).
      subagentPhase: restCompletesLiveRow
        ? rest.status === 'completed'
          ? 'completed'
          : rest.status === 'cancelled'
            ? 'aborted'
            : 'failed'
        : live.subagentPhase,
      completedAt: live.completedAt ?? rest.completedAt,
      // REST output is authoritative once present: agent tasks persist their
      // result at completion, and a previously folded preview would otherwise
      // freeze the detail panel's Result.
      outputPreview: rest.outputPreview ?? live.outputPreview,
      outputBytes: rest.outputBytes ?? live.outputBytes,
    };
  });
  const rest = restBased.filter((t) => !foldedRestIds.has(t.id));
  return [...rest, ...merged];
}

/**
 * Seed the task store from the snapshot's subagent roster. The roster is
 * authoritative for identity/status/phase; keep reducer-owned accumulated
 * output (outputLines/text) from any already-live task, and keep tasks the
 * roster does not know about (background bash tasks from REST).
 */
export function mergeSnapshotSubagents(roster: AppTask[], existing: AppTask[]): AppTask[] {
  if (roster.length === 0) return existing;
  const existingById = new Map(existing.map((t) => [t.id, t] as const));
  const rosterIds = new Set(roster.map((t) => t.id));
  const merged = roster.map((task) => {
    const live = existingById.get(task.id);
    if (!live) return task;
    // N-6：live 累积的叙事字段（steps/outputLines/text/suspendedReason）在名册
    // 合并中存活——名册行不带这些，丢掉即丢掉成员的工具卡片与输出。
    const base: AppTask = {
      ...task,
      // The engine's REST task rows carry no subagent_phase /
      // suspended_reason (the phase is event-derived). Keep the live
      // event-driven value when the roster row lacks it — otherwise a
      // parked (awaiting) member flips to an undetermined phase after the
      // reload's roster merge wipes its last suspension event.
      subagentPhase: task.subagentPhase ?? live.subagentPhase,
      suspendedReason: task.suspendedReason ?? live.suspendedReason,
      outputLines: live.outputLines,
      text: live.text,
      // ADR-009 live steps: same survival rule as outputLines/text — the
      // roster row carries none, so the live event-built steps must survive
      // the reload's roster merge or the member's tool cards vanish.
      steps: live.steps,
      // ADR-015 b: the reducer-owned narrative and bookkeeping fields must
      // also survive — a delta-gap resync's roster merge would otherwise wipe
      // the live member's thinking narrative and its step-start marks (and
      // a lost mark later disables the retry truncation below).
      thinking: live.thinking,
      stepTextLen: live.stepTextLen,
      stepThinkLen: live.stepThinkLen,
      tailGuard: live.tailGuard,
    };
    // (a) 名册终态原子合并：名册行（服务端权威）已终态时，status/subagentPhase
    // 经 applyTerminalToTask 原子落终态——名册行的 wire mapper 从不带
    // subagentPhase，若只取 status 而让 phase 回退 live 的中间阶段（如 working），
    // 就会得到 completed+working 的分裂态，且 reducer 的终态粘滞使其永久化。
    if (isTerminalStatus(task.status)) {
      return applyTerminalToTask(base, task.status);
    }
    // S-1 名册粘滞：live 行已终态而名册行仍 running（快照先于终态事件拍摄的
    // 陈旧名册）时，保留 live 的终态不被拉回 running；applyTerminalToTask 套在
    // 名册行的字段展开之后，保证 status/subagentPhase 原子（ADR-010 ①）。
    if (isTerminalStatus(live.status) && task.status === 'running') {
      return applyTerminalToTask(base, live.status);
    }
    return base;
  });
  const kept = existing.filter((t) => !rosterIds.has(t.id));
  return kept.length === 0 ? merged : [...merged, ...kept];
}
