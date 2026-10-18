import type { AppTask } from '../api/types';

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
export function keepLiveSubagents(restBased: AppTask[], existing: AppTask[]): AppTask[] {
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
    if (rest === undefined) return live;
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
    return {
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
    };
  });
  const kept = existing.filter((t) => !rosterIds.has(t.id));
  return kept.length === 0 ? merged : [...merged, ...kept];
}
