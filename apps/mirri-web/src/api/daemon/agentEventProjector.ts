// apps/mirri-web/src/api/daemon/agentEventProjector.ts
//
// Client-side projector: raw agent-core WS events → AppEvent[]
//
// The real daemon pushes raw agent-core events (NOT the projected "event.*"
// protocol events). This projector translates them into the same AppEvent union
// that the existing reducer (eventReducer.ts) consumes.
//
// Ported from the daemon-side reference implementation:
//   apps/mirri-daemon/src/session/event-projector.ts
//   apps/mirri-daemon/src/session/message-log.ts
//   apps/mirri-daemon/src/session/usage-tracker.ts
//
// Usage:
//   const projector = createAgentProjector();
//   const appEvents = projector.project(rawType, payload, sessionId);
//   // call reset() when re-subscribing / resyncing a session

import type {
  AgentStep,
  AppEvent,
  AppGoal,
  AppInFlightTurn,
  AppMessage,
  AppMessageContent,
  AppSessionUsage,
  AppSubagentPhase,
  AppTask,
  AppTaskStatus,
  TurnUsage,
} from '../types';
import { i18n } from '../../i18n';
import { toolLabel, toolSummary } from '../../lib/toolMeta';
import type { UsageBuckets } from '../../lib/usageStats';
import { toAppMessageContent } from './mappers';
import type { WireMessageContent } from './wire';

// Re-export for existing importers (Task 5/6 consumers import TurnUsage from
// the projector); the canonical declaration lives in api/types.ts.
export type { TurnUsage };

// Subagent turns share the parent session id: their turn / step / delta / tool
// frames stream over the SAME session channel, each tagged with the subagent's
// own agentId (the main agent's is 'main'). They must NOT be folded into the
// parent transcript — doing so created empty "skeleton" assistant bubbles (a
// subagent turn.step.started opens a parent assistant message that never gets
// the main agent's text) and fragmented snippets (subagent deltas appended to
// the parent). The subagent's live progress is surfaced separately via the
// subagent.* → task → right-side detail panel path (the spawning `Agent` tool
// itself renders as a normal tool card in the transcript). This mirrors the
// server's InFlightTurnTracker, which likewise tracks only main-agent activity.
const MAIN_AGENT_ID = 'main';
const MAIN_AGENT_TRANSCRIPT_FRAMES = new Set<string>([
  'turn.started',
  'turn.step.started',
  'turn.step.completed',
  'turn.step.retrying',
  'turn.step.interrupted',
  'turn.ended',
  'thinking.delta',
  'assistant.delta',
  'tool.use',
  'tool.call.started',
  'tool.call.delta',
  'tool.progress',
  'tool.result',
  'agent.status.updated',
  'prompt.completed',
  'prompt.aborted',
  'error',
]);

// Compaction lifecycle frames that carry the session-level compaction state
// in the switch below. They are NOT transcript frames, but a member agent's
// compaction must be gated the same way (see the pre-filter note above the
// switch): only the main agent's (or a legacy unattributed) compaction may
// touch the session-level state.
const COMPACTION_FRAMES = new Set<string>([
  'compaction.started',
  'compaction.completed',
  'compaction.cancelled',
]);

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

function ulid(prefix = 'msg_'): string {
  const t = Date.now().toString(36).padStart(10, '0');
  const r = Math.random().toString(36).slice(2, 12).padEnd(10, '0');
  return `${prefix}${t}${r}`;
}

/** Normalise the raw token usage shape emitted by agent-core. */
function normalizeUsage(raw: unknown): {
  input: number;
  output: number;
  cacheRead: number;
  cacheCreate: number;
} {
  if (!raw || typeof raw !== 'object') {
    return { input: 0, output: 0, cacheRead: 0, cacheCreate: 0 };
  }
  const u = raw as Record<string, number | undefined>;
  return {
    input: u['inputOther'] ?? u['input_tokens'] ?? 0,
    output: u['output'] ?? u['output_tokens'] ?? 0,
    cacheRead: u['inputCacheRead'] ?? u['cache_read_input_tokens'] ?? 0,
    cacheCreate: u['inputCacheCreation'] ?? u['cache_creation_input_tokens'] ?? 0,
  };
}

// ---------------------------------------------------------------------------
// Per-session projector state
// ---------------------------------------------------------------------------

/** Token usage attributed to a single turn (SDR-002 per-turn attribution).
 *  Consumed by the usage-visibility layer (Task 6/7) via
 *  `turnUsageForPromptId`. Declared in api/types.ts; re-exported above. */

export interface SessionState {
  // Turn ID → promptId binding
  turnPromptId: Map<string | number | undefined, string>;
  currentPromptId: string | undefined;
  // String-keyed view of the same binding for the usage-visibility query
  // surface (`turnUsageForPromptId` / Task 6/7). Kept in sync by
  // `bindTurnPromptId`; subagent turns never bind, so their usage stays
  // unattributable at the prompt level by design.
  turnPromptIdByTurnId: Record<string, string>;

  // Per-turn usage accumulation keyed by turn id (main and subagent turns
  // alike — attribution happens at query time via the binding above).
  turnUsageByTurnId: Record<string, TurnUsage>;

  // Assistant message tracking
  currentAssistantMsgId: string | undefined;

  // Per-step accumulated stream lengths — aligned against the (step-relative)
  // wire `offset` on volatile delta frames (v2 sync protocol) to skip
  // duplicates and detect gaps after a snapshot seed.
  turnTextLen: number;
  turnThinkLen: number;

  // Tool timing
  toolStartTimes: Map<string, number>;

  // Usage accumulator
  totalInput: number;
  totalOutput: number;
  totalCacheRead: number;
  totalCacheCreate: number;
  contextTokens: number;
  contextLimit: number;
  turnCount: number;
  model: string;
  // SDR-004: the session's cold-start baseline — the main agent's first
  // LLM call (subagent steps are never the baseline). Captured from the
  // first main-agent turn.step.completed in a live, unseeded session; a
  // snapshot-seeded session keeps its seeded baseline instead, or stays
  // honestly null when the snapshot carried none.
  firstCall: UsageBuckets | null;

  // True once seedUsage applied a REST snapshot's accumulators. The SDR-004
  // capture gate keyed on this instead of a zero-totals heuristic: a fresh
  // live session can accumulate subagent usage before the main agent's step
  // 1 completes (SDR-004 d), so non-zero totals do not imply a seed.
  usageSeeded: boolean;

  // In-memory message log (mirrors daemon message-log.ts)
  messages: AppMessage[];

  // Subagent lifecycle deltas after spawned only carry subagentId. Keep the
  // spawned metadata here so later updates can replace the full AppTask.
  subagentMeta: Map<string, AppTask>;

  // Structured live tool steps per member agent (ADR-009): the projector —
  // not the reducer — owns step identity, because tool frames arrive as raw
  // events with no task row to patch incrementally. Patches carry the whole
  // step list so `taskCreated` (full-row replacement) keeps the panel fresh.
  subagentSteps: Map<string, AgentStep[]>;

  // Bubble cleared by turn.step.retrying, to be reused by the retried
  // step.started (same turn) instead of stacking a new bubble.
  retryReuseMsgId: string | undefined;
}

function createSessionState(): SessionState {
  return {
    turnPromptId: new Map(),
    turnPromptIdByTurnId: {},
    turnUsageByTurnId: {},
    currentPromptId: undefined,
    currentAssistantMsgId: undefined,
    turnTextLen: 0,
    turnThinkLen: 0,
    toolStartTimes: new Map(),
    totalInput: 0,
    totalOutput: 0,
    totalCacheRead: 0,
    totalCacheCreate: 0,
    contextTokens: 0,
    contextLimit: 0,
    turnCount: 0,
    model: '',
    firstCall: null,
    usageSeeded: false,
    messages: [],
    subagentMeta: new Map(),
    subagentSteps: new Map(),
    retryReuseMsgId: undefined,
  };
}

function stringField(source: Record<string, unknown>, key: string): string | undefined {
  const value = source[key];
  return typeof value === 'string' ? value : undefined;
}

/** Normalize a wire turn id into the string key used by the usage-visibility
 *  records. The engine emits `turn-<uuid>` strings; the legacy TS daemon used
 *  numeric counters — both are stringified. Undefined for absent/empty ids. */
function turnKey(turnId: unknown): string | undefined {
  if (typeof turnId === 'string') return turnId.length > 0 ? turnId : undefined;
  if (typeof turnId === 'number' && Number.isFinite(turnId)) return String(turnId);
  return undefined;
}

/** Record a turnId → promptId binding in both lookup shapes: the operational
 *  Map (raw wire keys — lookups index it with whatever the frame carried) and
 *  the string-keyed `turnPromptIdByTurnId` record the usage-visibility layer
 *  reads. */
function bindTurnPromptId(
  s: SessionState,
  turnId: string | number | undefined,
  promptId: string,
): void {
  if (turnId !== undefined) {
    s.turnPromptId.set(turnId, promptId);
  }
  const key = turnKey(turnId);
  if (key !== undefined) s.turnPromptIdByTurnId[key] = promptId;
}

/**
 * Merge the per-turn usage of every turn bound to the given prompt id.
 * Subagent turns carry no prompt binding, so their usage never lands in any
 * prompt's result (session-level totals in `SessionState` still count them —
 * they are agent-agnostic by design). Returns undefined when no bound turn
 * has recorded usage.
 */
export function turnUsageForPromptId(state: SessionState, promptId: string): TurnUsage | undefined {
  let merged: TurnUsage | undefined;
  for (const [key, boundPromptId] of Object.entries(state.turnPromptIdByTurnId)) {
    if (boundPromptId !== promptId) continue;
    const usage = state.turnUsageByTurnId[key];
    if (!usage) continue;
    if (merged === undefined) {
      merged = { ...usage, models: [...usage.models] };
      continue;
    }
    merged.inputTokens += usage.inputTokens;
    merged.outputTokens += usage.outputTokens;
    merged.cacheReadTokens += usage.cacheReadTokens;
    merged.cacheCreationTokens += usage.cacheCreationTokens;
    for (const model of usage.models) {
      if (!merged.models.includes(model)) merged.models.push(model);
    }
  }
  return merged;
}

function numberField(source: Record<string, unknown>, key: string): number | undefined {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function nullableNumberField(source: Record<string, unknown>, key: string): number | null {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function mapGoalSnapshot(snapshot: unknown): AppGoal | null {
  if (!snapshot || typeof snapshot !== 'object') return null;
  const s = snapshot as Record<string, unknown>;
  const budgetRaw = s['budget'];
  const budget = budgetRaw && typeof budgetRaw === 'object' ? budgetRaw as Record<string, unknown> : {};
  const status = stringField(s, 'status');
  if (status !== 'active' && status !== 'paused' && status !== 'blocked' && status !== 'complete') return null;
  const goalId = stringField(s, 'goalId') ?? stringField(s, 'goal_id') ?? 'goal';
  const objective = stringField(s, 'objective') ?? '';
  return {
    goalId,
    objective,
    completionCriterion: stringField(s, 'completionCriterion') ?? stringField(s, 'completion_criterion'),
    status,
    turnsUsed: numberField(s, 'turnsUsed') ?? numberField(s, 'turns_used') ?? 0,
    tokensUsed: numberField(s, 'tokensUsed') ?? numberField(s, 'tokens_used') ?? 0,
    wallClockMs: numberField(s, 'wallClockMs') ?? numberField(s, 'wall_clock_ms') ?? 0,
    terminalReason: stringField(s, 'terminalReason') ?? stringField(s, 'terminal_reason'),
    budget: {
      tokenBudget: nullableNumberField(budget, 'tokenBudget') ?? nullableNumberField(budget, 'token_budget'),
      remainingTokens: nullableNumberField(budget, 'remainingTokens') ?? nullableNumberField(budget, 'remaining_tokens'),
      turnBudget: nullableNumberField(budget, 'turnBudget') ?? nullableNumberField(budget, 'turn_budget'),
      remainingTurns: nullableNumberField(budget, 'remainingTurns') ?? nullableNumberField(budget, 'remaining_turns'),
      wallClockBudgetMs: nullableNumberField(budget, 'wallClockBudgetMs') ?? nullableNumberField(budget, 'wall_clock_budget_ms'),
      remainingWallClockMs: nullableNumberField(budget, 'remainingWallClockMs') ?? nullableNumberField(budget, 'remaining_wall_clock_ms'),
      overBudget: budget['overBudget'] === true || budget['over_budget'] === true,
    },
  };
}

function patchSubagent(
  state: SessionState,
  sessionId: string,
  subagentId: unknown,
  patch: Partial<AppTask>,
): AppTask | null {
  if (typeof subagentId !== 'string' || subagentId.length === 0) return null;
  const prev = state.subagentMeta.get(subagentId) ?? {
    id: subagentId,
    sessionId,
    kind: 'subagent',
    description: 'Sub Agent',
    status: 'running',
    createdAt: new Date().toISOString(),
    subagentPhase: 'queued',
  } satisfies AppTask;
  // agentId is the cross-pipeline join key (REST `/tasks` rows carry the same
  // id as agent_id) — every projected subagent row must carry it so the fold
  // keeps one row per agent.
  const next: AppTask = {
    ...prev,
    ...patch,
    steps: state.subagentSteps.get(subagentId) ?? prev.steps,
    id: subagentId,
    agentId: subagentId,
    sessionId,
    kind: 'subagent',
  };
  state.subagentMeta.set(subagentId, next);
  return next;
}

/** A concise, human-readable arg summary for a step card (raw string arg
 *  passes through; object args serialize). Mirrors the main transcript's
 *  tool-card arg display rule. */
function stepArgString(args: unknown): string {
  if (args === undefined || args === null) return '';
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args) ?? '';
  } catch {
    return '[unserializable]';
  }
}

/** Normalize a tool result output into display lines for a step card — same
 *  flattening rule as the main transcript's tool cards. */
function normalizeStepOutput(output: unknown): string[] {
  if (output === null || output === undefined) return [];
  if (typeof output === 'string') return output.split('\n');
  if (Array.isArray(output)) {
    const lines: string[] = [];
    for (const part of output) {
      if (typeof part === 'string') lines.push(...part.split('\n'));
      else if (part && typeof part === 'object') {
        const p = part as Record<string, unknown>;
        if (p['type'] === 'text' && typeof p['text'] === 'string') lines.push(...p['text'].split('\n'));
        else if (p['type'] === 'think' && typeof p['think'] === 'string') lines.push(...p['think'].split('\n'));
        else if (typeof p['type'] === 'string') lines.push(`[${p['type']}]`);
        else lines.push(JSON.stringify(part));
      }
    }
    return lines;
  }
  return [JSON.stringify(output)];
}

/** Append a new running step for the member's tool call (ADR-009): the
 *  projector keeps one step per tool call id; a duplicate started frame for
 *  the same id (reconnect replay) refreshes the arg in place instead of
 *  stacking a second card. */
function startStep(state: SessionState, agentId: string, toolCallId: unknown, name: unknown, args: unknown): void {
  if (typeof toolCallId !== 'string' || toolCallId.length === 0) return;
  const steps = state.subagentSteps.get(agentId) ?? [];
  const existing = steps.findIndex((s) => s.id === toolCallId);
  const step: AgentStep = {
    id: toolCallId,
    name: typeof name === 'string' && name.length > 0 ? name : 'tool',
    arg: stepArgString(args),
    status: 'running',
    outputLines: [],
  };
  if (existing >= 0) steps[existing] = step;
  else steps.push(step);
  state.subagentSteps.set(agentId, steps);
}

/** Append a progress line onto the member's open step. */
function appendStepLine(state: SessionState, agentId: string, toolCallId: unknown, line: string): void {
  if (typeof toolCallId !== 'string' || toolCallId.length === 0) return;
  const steps = state.subagentSteps.get(agentId);
  if (!steps) return;
  const step = steps.find((s) => s.id === toolCallId);
  if (!step) return;
  step.outputLines.push(line);
}

/** Settle the member's step from its result frame: status + output lines. */
function finishStep(state: SessionState, agentId: string, toolCallId: unknown, output: unknown, isError: boolean): void {
  if (typeof toolCallId !== 'string' || toolCallId.length === 0) return;
  const steps = state.subagentSteps.get(agentId);
  if (!steps) return;
  const step = steps.find((s) => s.id === toolCallId);
  if (!step) return;
  step.status = isError ? 'error' : 'ok';
  step.outputLines = normalizeStepOutput(output);
}

export function subagentProgressText(rawType: string, payload: Record<string, unknown>): string | null {
  // "Started a step" fires on every step and adds no information — the phase
  // badge already shows the subagent is working, so skip it to cut the noise.
  if (rawType === 'turn.step.started') return null;
  // CR5 attribution: a member agent's own compaction surfaces as a progress
  // line on its task row (the session-level compaction state stays with the
  // main agent — see the COMPACTION_FRAMES gate).
  if (rawType === 'compaction.started') {
    return 'Compacting context…';
  }
  if (rawType === 'compaction.completed') {
    const result = payload['result'];
    const record = result && typeof result === 'object' ? result as Record<string, unknown> : null;
    const before = typeof record?.['tokensBefore'] === 'number' ? record['tokensBefore'] : undefined;
    const after = typeof record?.['tokensAfter'] === 'number' ? record['tokensAfter'] : undefined;
    if (before !== undefined && after !== undefined) {
      return `Context compacted (${before.toLocaleString()} → ${after.toLocaleString()} tokens)`;
    }
    return 'Context compacted';
  }
  if (rawType === 'compaction.cancelled') {
    return 'Compaction cancelled';
  }
  if (rawType === 'tool.use' || rawType === 'tool.call.started') {
    const name = stringField(payload, 'name') ?? stringField(payload, 'toolName') ?? 'tool';
    const label = toolLabel(cleanToolName(name));
    const summary = toolArgSummary(name, payload['args'] ?? payload['input']);
    return summary ? `Calling ${label}: ${summary}` : `Calling ${label}`;
  }
  if (rawType === 'tool.progress') {
    const update = payload['update'];
    if (update && typeof update === 'object') {
      const text = stringField(update as Record<string, unknown>, 'text');
      if (text) return capProgressText(text);
      const message = stringField(update as Record<string, unknown>, 'message');
      if (message) return capProgressText(message);
    }
    const message = stringField(payload, 'message');
    if (message) return capProgressText(message);
  }
  // tool.result lines ("Finished X") add noise without much information — the
  // next call or the final summary already implies completion — so skip them.
  if (rawType === 'tool.result') return null;
  return null;
}

/** Strip a trailing `_N` index that some subagents append to tool names in
 *  `tool.result` events (e.g. `Read_0` → `Read`) so the label resolves. */
function cleanToolName(name: string): string {
  return name.replace(/_\d+$/, '');
}

/** Map the CR4 wire `phase` vocabulary (protocol `AgentPhase` — the rust
 *  engine's serde snake_case enum: idle/running/streaming/tool_call/retrying/
 *  awaiting_approval/interrupted/ended) onto the task-row badge vocabulary
 *  (`AppSubagentPhase`, ADR-008 ①/③). The badge union is the web's display
 *  contract — it drives the phase-count records and the badge labels — so the
 *  wire value is translated at this edge instead of widening the union:
 *    idle → queued            (spawned, turn not yet started)
 *    running/streaming/
 *    tool_call/retrying → working
 *    awaiting_approval → awaiting
 *    interrupted → aborted    (best-effort; the reason is not on the wire —
 *                              the lifecycle event stays authoritative)
 *    ended → completed        (same — subagent.completed follows)
 * Unknown values (a future engine vocabulary the web has not learned) map to
 * undefined: the frame is then ignored rather than corrupting the badge with a
 * foreign string. */
const WIRE_PHASE_TO_SUBAGENT_PHASE: Readonly<Record<string, AppSubagentPhase>> = {
  idle: 'queued',
  running: 'working',
  streaming: 'working',
  tool_call: 'working',
  retrying: 'working',
  awaiting_approval: 'awaiting',
  interrupted: 'aborted',
  ended: 'completed',
};

/** Cap a progress text chunk so a single huge tool output (e.g. a big command
 *  result) cannot dominate the panel. */
const MAX_PROGRESS_TEXT = 2000;
function capProgressText(text: string): string {
  return text.length > MAX_PROGRESS_TEXT ? `${text.slice(0, MAX_PROGRESS_TEXT)}…` : text;
}

/** A concise, human-readable summary of a tool call's arguments for progress
 *  lines (e.g. a file path or shell command), instead of the full JSON blob. */
function toolArgSummary(name: string, args: unknown): string {
  if (args === undefined || args === null) return '';
  const arg = typeof args === 'string' ? args : JSON.stringify(args);
  return toolSummary(name, arg);
}

function projectSubagentProgress(
  state: SessionState,
  sessionId: string,
  subagentId: string,
  rawType: string,
  payload: Record<string, unknown>,
  sideChannelAgents: ReadonlySet<string>,
): AppEvent[] {
  // Side-channel agents (e.g. BTW side chat) stream their own transcript via
  // agentDelta events; don't pollute the main task output with generic step
  // placeholders like "Started a step".
  if (sideChannelAgents.has(subagentId) && rawType === 'turn.step.started') return [];

  // ADR-009: a member's tool events maintain structured live steps on the
  // task row (name / arg / status / output lines) IN ADDITION to the legacy
  // text-line synthesis below — the panel consumes the steps; the "Calling …"
  // lines stay for the dock and other line-based consumers until they migrate.
  if (rawType === 'tool.use' || rawType === 'tool.call.started' || rawType === 'tool.progress' || rawType === 'tool.result') {
    const previous = state.subagentMeta.get(subagentId);
    // The lifecycle events (subagent.completed/failed) own the terminal state:
    // never resurrect a finished member row from a late progress frame.
    if (previous && previous.status !== 'running') return [];
    if (rawType !== 'tool.result') {
      if (rawType !== 'tool.progress') {
        startStep(state, subagentId, payload['toolCallId'], payload['name'] ?? payload['toolName'], payload['args'] ?? payload['input']);
      } else {
        const progress = toolProgressOutput(payload);
        if (progress) appendStepLine(state, subagentId, payload['toolCallId'], capProgressText(progress.outputChunk));
      }
    } else {
      finishStep(state, subagentId, payload['toolCallId'], payload['output'], payload['isError'] === true);
    }
    const task = patchSubagent(state, sessionId, subagentId, {
      status: 'running',
      subagentPhase: 'working',
      startedAt: previous?.startedAt ?? new Date().toISOString(),
    });
    const out: AppEvent[] = [];
    if (task) out.push({ type: 'taskCreated', sessionId, task });
    // Legacy line synthesis (dock activity line, outputLines consumers).
    const text = subagentProgressText(rawType, payload);
    if (text !== null && text.length > 0) {
      out.push({ type: 'taskProgress', sessionId, taskId: subagentId, outputChunk: text, stream: 'stdout' });
    }
    return out;
  }

  // CR4: a member's structured status frame (`agent.status.updated` stamped
  // with its agentId) carries its live activity phase. It feeds the task-row
  // badge (AppTask.subagentPhase) — and MUST NOT fall through to the
  // session-level status arm, whose sessionUsageUpdated would otherwise
  // clobber the session's own context watermark / model with the member's
  // numbers. Frames without a `phase` (v2 TS engines, pre-CR4 rust builds) and
  // unknown phase values are tolerated silently, exactly like any other
  // optional-field absence.
  if (rawType === 'agent.status.updated') {
    const phase = stringField(payload, 'phase');
    if (phase === undefined) return [];
    const mapped = WIRE_PHASE_TO_SUBAGENT_PHASE[phase];
    if (mapped === undefined) return [];
    const previous = state.subagentMeta.get(subagentId);
    // The lifecycle events (subagent.completed/failed) own the terminal state:
    // never resurrect a finished member row from a late/racing status frame.
    if (previous && previous.status !== 'running') return [];
    const task = patchSubagent(state, sessionId, subagentId, {
      subagentPhase: mapped,
      startedAt: previous?.startedAt ?? new Date().toISOString(),
    });
    return task ? [{ type: 'taskCreated', sessionId, task }] : [];
  }

  // The subagent's own streamed reasoning (ADR-009 live narrative): forwarded
  // like assistant.delta below, concatenated into `AppTask.thinking` so the
  // panel shows the member's thinking block growing live.
  if (rawType === 'thinking.delta') {
    const delta = stringField(payload, 'delta');
    if (!delta) return [];
    const previous = state.subagentMeta.get(subagentId);
    const task = patchSubagent(state, sessionId, subagentId, {
      status: 'running',
      subagentPhase: 'working',
      startedAt: previous?.startedAt ?? new Date().toISOString(),
    });
    const out: AppEvent[] = [];
    if (task) out.push({ type: 'taskCreated', sessionId, task });
    out.push({
      type: 'taskProgress',
      sessionId,
      taskId: subagentId,
      outputChunk: delta,
      stream: 'stdout',
      kind: 'thinking',
    });
    return out;
  }

  // The subagent's own streamed text: forward each delta as a `text`-kind
  // progress chunk so the reducer concatenates it into `AppTask.text`, letting
  // the right-side detail panel show the subagent's output growing live (like
  // a thinking block) instead of staying blank until the first tool call.
  if (rawType === 'assistant.delta') {
    const delta = stringField(payload, 'delta');
    if (!delta) return [];
    // Ensure the subagent task exists before forwarding the text delta. A client
    // that subscribed from a snapshot after `subagent.spawned` already fired
    // never received the lifecycle taskCreated, and the reducer only applies
    // taskProgress to existing tasks — without this, the deltas are dropped and
    // the live detail stays blank until a non-text frame recreates the task.
    const previous = state.subagentMeta.get(subagentId);
    const task = patchSubagent(state, sessionId, subagentId, {
      status: 'running',
      subagentPhase: 'working',
      startedAt: previous?.startedAt ?? new Date().toISOString(),
    });
    const out: AppEvent[] = [];
    if (task) out.push({ type: 'taskCreated', sessionId, task });
    out.push({
      type: 'taskProgress',
      sessionId,
      taskId: subagentId,
      outputChunk: delta,
      stream: 'stdout',
      kind: 'text',
    });
    return out;
  }

  const text = subagentProgressText(rawType, payload);
  if (text === null || text.length === 0) return [];
  const previous = state.subagentMeta.get(subagentId);
  // The lifecycle events (subagent.completed/failed) own the terminal state:
  // never resurrect a finished member row from a late progress frame — e.g.
  // a trailing compaction or tool frame racing the lifecycle event. Same
  // guard as the agent.status.updated arm above.
  if (previous && previous.status !== 'running') return [];
  const task = patchSubagent(state, sessionId, subagentId, {
    status: 'running',
    subagentPhase: 'working',
    startedAt: previous?.startedAt ?? new Date().toISOString(),
  });
  const out: AppEvent[] = [];
  if (task) out.push({ type: 'taskCreated', sessionId, task });
  out.push({ type: 'taskProgress', sessionId, taskId: subagentId, outputChunk: text, stream: 'stdout' });
  return out;
}

// ---------------------------------------------------------------------------
// Message-log helpers (inlined; mirrors message-log.ts)
// ---------------------------------------------------------------------------

/**
 * Decouple an emitted message from the projector's internal log. The reducer
 * stores emitted messages by reference; the projector keeps mutating its own
 * copy in place (`slot.text += delta`), so sharing the content objects makes
 * the reducer's delta-append run on already-appended text — the first streamed
 * chunk of every text/thinking block rendered twice.
 */
function cloneMessage(msg: AppMessage): AppMessage {
  return { ...msg, content: msg.content.map((c) => ({ ...c })) };
}

function startAssistantMessage(state: SessionState, sessionId: string, promptId: string): AppMessage {
  const msg: AppMessage = {
    id: ulid('msg_'),
    sessionId,
    role: 'assistant',
    content: [],
    createdAt: new Date().toISOString(),
    promptId,
  };
  state.messages.push(msg);
  return msg;
}

function startUserMessage(
  state: SessionState,
  sessionId: string,
  promptId: string,
  userMessageId: string,
  content: AppMessageContent[],
  createdAt: string,
): AppMessage {
  const msg: AppMessage = {
    id: userMessageId,
    sessionId,
    role: 'user',
    content,
    createdAt,
    promptId,
  };
  state.messages.push(msg);
  return msg;
}

function toAppPromptContent(raw: unknown): AppMessageContent[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((part) => toAppMessageContent(part as WireMessageContent));
}

/**
 * Append a streamed text/thinking delta in stream order: continue the LAST
 * content part when it has the same type, otherwise open a NEW part at the
 * end. Returns the content index written (-1 if the message is unknown) so
 * the emitted assistantDelta targets the same slot in the reducer.
 *
 * No per-type fixed slots: a step that goes think → text → think again gets
 * three parts in call order instead of all thinking collapsing into one slot.
 */
function appendAssistantDelta(
  state: SessionState,
  messageId: string,
  kind: 'text' | 'thinking',
  delta: string,
): number {
  const msg = state.messages.find((m) => m.id === messageId);
  if (!msg) return -1;
  const last = msg.content.at(-1);
  if (last && last.type === kind) {
    if (kind === 'text') (last as { type: 'text'; text: string }).text += delta;
    else (last as { type: 'thinking'; thinking: string }).thinking += delta;
    return msg.content.length - 1;
  }
  msg.content.push(kind === 'text' ? { type: 'text', text: delta } : { type: 'thinking', thinking: delta });
  return msg.content.length - 1;
}

function appendToolUse(
  state: SessionState,
  messageId: string,
  toolCallId: string,
  toolName: string,
  input: unknown,
  outputLines?: string[],
): void {
  const msg = state.messages.find((m) => m.id === messageId);
  if (!msg) return;
  msg.content.push({ type: 'toolUse', toolCallId, toolName, input, outputLines });
}

function toolProgressOutput(payload: Record<string, unknown>): { outputChunk: string; stream: 'stdout' | 'stderr' } | null {
  const update = payload['update'];
  const updateRecord = update && typeof update === 'object' ? update as Record<string, unknown> : null;
  const streamRaw = updateRecord?.['stream'] ?? updateRecord?.['kind'] ?? payload['stream'];
  const stream = streamRaw === 'stderr' ? 'stderr' : 'stdout';
  const chunk =
    (typeof updateRecord?.['text'] === 'string' && updateRecord['text']) ||
    (typeof updateRecord?.['message'] === 'string' && updateRecord['message']) ||
    (typeof payload['chunk'] === 'string' && payload['chunk']) ||
    (typeof payload['output'] === 'string' && payload['output']) ||
    (typeof payload['message'] === 'string' && payload['message']) ||
    '';
  return chunk.length > 0 ? { outputChunk: chunk, stream } : null;
}

function finishAssistantMessage(state: SessionState, messageId: string): void {
  const msg = state.messages.find((m) => m.id === messageId);
  // We record nothing extra here — status is implicit in the downstream reducer
  void msg;
}

function appendToolResultMessage(
  state: SessionState,
  sessionId: string,
  toolCallId: string,
  output: unknown,
  isError: boolean,
  promptId: string,
): AppMessage {
  const msg: AppMessage = {
    id: ulid('msg_'),
    sessionId,
    role: 'tool',
    content: [{ type: 'toolResult', toolCallId, output, isError }],
    createdAt: new Date().toISOString(),
    promptId,
  };
  state.messages.push(msg);
  return msg;
}

function getMsgById(state: SessionState, messageId: string): AppMessage | undefined {
  return state.messages.find((m) => m.id === messageId);
}

// ---------------------------------------------------------------------------
// Usage snapshot builder
// ---------------------------------------------------------------------------

function buildUsageSnapshot(state: SessionState): AppSessionUsage {
  return {
    inputTokens: state.totalInput,
    outputTokens: state.totalOutput,
    cacheReadTokens: state.totalCacheRead,
    cacheCreationTokens: state.totalCacheCreate,
    totalCostUsd: 0,
    contextTokens: state.contextTokens,
    contextLimit: state.contextLimit,
    turnCount: state.turnCount,
    firstCall: state.firstCall ?? undefined,
  };
}

// ---------------------------------------------------------------------------
// AgentProjector
// ---------------------------------------------------------------------------

export interface ProjectMeta {
  /**
   * Wire-level pre-append stream offset on volatile text-delta frames (v2
   * sync protocol). Used to skip duplicate deltas and detect gaps after a
   * snapshot seed.
   */
  offset?: number;
}

export interface AgentProjector {
  /** Project a single raw agent-core event into zero or more AppEvents. Never throws. */
  project(rawType: string, payload: unknown, sessionId: string, meta?: ProjectMeta): AppEvent[];
  /**
   * Bind an externally-known promptId to the next turn.startd for this session.
   * Call this right after submitPrompt() returns, before the first turn.started arrives.
   */
  bindNextPromptId(sessionId: string, promptId: string): void;
  /**
   * Read-only view of the projector's per-session state (usage records,
   * turn→prompt bindings) for the usage-visibility query helpers such as
   * `turnUsageForPromptId`. Treat the returned object as immutable.
   */
  state(sessionId: string): SessionState;
  /**
   * Seed mid-turn state from a session snapshot's `in_flight_turn` (v2 sync):
   * resets per-session state, builds the partially-streamed assistant message
   * (thinking + text + running tool_use parts — the current step only; earlier
   * steps arrive via the transcript), and returns the messageCreated AppEvent
   * to apply to the reducer. Live deltas continue appending; their wire
   * `offset` aligns against the seeded text so the overlap window around
   * snapshot/subscribe is exact. Session status is NOT seeded here — the REST
   * snapshot's `session.status` is the authoritative value.
   */
  seedInFlight(sessionId: string, turn: AppInFlightTurn): AppEvent[];
  /** Seed per-turn usage folded from a snapshot so the usage pill survives
   *  a rerender. Each entry self-binds: the turn key doubles as the prompt
   *  id the snapshot's assistant rows carry. Call after `reset`, before
   *  `subscribe`. */
  seedTurnUsage(sessionId: string, entries: Array<{ promptId: string; usage: TurnUsage }>): void;
  /** Reset all per-session state (call on re-subscribe / resync). */
  reset(sessionId: string): void;
  /** Seed the usage accumulators from a REST snapshot so the next live
   *  `sessionUsageUpdated` (built from these accumulators) continues from
   *  the snapshot's totals instead of overwriting them with near-zero ones
   *  (the refresh regression), and carries the snapshot's cold-start
   *  baseline (SDR-004). Callers must skip placeholder usage. */
  seedUsage(sessionId: string, usage: AppSessionUsage): void;
  /**
   * Mark an agent id as a side-channel (e.g. BTW side chat) rather than a
   * background subagent. Its text/thinking deltas and turn boundary are then
   * emitted as agent-scoped events instead of being dropped.
   */
  markSideChannelAgent(agentId: string): void;
}

export function createAgentProjector(): AgentProjector {
  const sessions = new Map<string, SessionState>();
  const sideChannelAgents = new Set<string>();

  function getOrCreate(sessionId: string): SessionState {
    let s = sessions.get(sessionId);
    if (!s) {
      s = createSessionState();
      sessions.set(sessionId, s);
    }
    return s;
  }

  function reset(sessionId: string): void {
    sessions.set(sessionId, createSessionState());
  }

  function seedUsage(sessionId: string, usage: AppSessionUsage): void {
    let s = sessions.get(sessionId);
    if (!s) {
      s = createSessionState();
      sessions.set(sessionId, s);
    }
    s.totalInput = usage.inputTokens;
    s.totalOutput = usage.outputTokens;
    s.totalCacheRead = usage.cacheReadTokens;
    s.totalCacheCreate = usage.cacheCreationTokens;
    s.turnCount = usage.turnCount;
    s.firstCall = usage.firstCall ? { ...usage.firstCall } : null;
    // Seed-protected: live capture must never overwrite the snapshot's
    // baseline (or its honest absence).
    s.usageSeeded = true;
  }

  function markSideChannelAgent(agentId: string): void {
    sideChannelAgents.add(agentId);
  }

  function bindNextPromptId(sessionId: string, promptId: string): void {
    const s = getOrCreate(sessionId);
    s.currentPromptId = promptId;
  }

  function state(sessionId: string): SessionState {
    return getOrCreate(sessionId);
  }

  function seedInFlight(sessionId: string, turn: AppInFlightTurn): AppEvent[] {
    reset(sessionId);
    const s = getOrCreate(sessionId);

    const promptId = turn.promptId ?? ulid('pr_');
    s.currentPromptId = promptId;
    bindTurnPromptId(s, turn.turnId, promptId);

    const msg = startAssistantMessage(s, sessionId, promptId);
    if (turn.thinkingText.length > 0) {
      msg.content.push({ type: 'thinking', thinking: turn.thinkingText });
    }
    if (turn.assistantText.length > 0) {
      msg.content.push({ type: 'text', text: turn.assistantText });
    }
    for (const tool of turn.runningTools) {
      const outputLines =
        typeof tool.lastProgress?.text === 'string' && tool.lastProgress.text.length > 0
          ? [tool.lastProgress.text]
          : undefined;
      msg.content.push({
        type: 'toolUse',
        toolCallId: tool.toolCallId,
        toolName: tool.name,
        input: tool.args ?? {},
        outputLines,
      });
      s.toolStartTimes.set(tool.toolCallId, Date.now());
    }
    s.currentAssistantMsgId = msg.id;
    // Seeded step-relative lengths; the next turn.step.started resets both.
    s.turnTextLen = turn.assistantText.length;
    s.turnThinkLen = turn.thinkingText.length;

    return [{ type: 'messageCreated', message: cloneMessage(msg) }];
  }

  function seedTurnUsage(
    sessionId: string,
    entries: Array<{ promptId: string; usage: TurnUsage }>,
  ): void {
    const s = getOrCreate(sessionId);
    for (const { promptId, usage } of entries) {
      s.turnUsageByTurnId[promptId] = { ...usage, models: [...usage.models] };
      s.turnPromptIdByTurnId[promptId] = promptId;
    }
  }

  function project(
    rawType: string,
    payload: unknown,
    sessionId: string,
    meta?: ProjectMeta,
  ): AppEvent[] {
    try {
      return _project(rawType, payload, sessionId, meta);
    } catch (error) {
      // Defensive: log but never crash the caller
      console.error('[agentProjector] Error projecting event:', rawType, error instanceof Error ? error.message : error);
      return [];
    }
  }

  /**
   * Align a live text-delta against the per-turn accumulated length using the
   * wire `offset`. Returns 'skip' for duplicates (offset behind local state),
   * 'gap' when deltas were missed (offset ahead — trigger a re-snapshot), and
   * 'append' otherwise.
   */
  function alignDelta(localLen: number, offset: number | undefined): 'append' | 'skip' | 'gap' {
    if (offset === undefined) return 'append';
    if (offset < localLen) return 'skip';
    if (offset > localLen) return 'gap';
    return 'append';
  }

  function _project(
    rawType: string,
    payload: unknown,
    sessionId: string,
    meta?: ProjectMeta,
  ): AppEvent[] {
    const s = getOrCreate(sessionId);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const p = payload as any;
    const out: AppEvent[] = [];

    // Usage accumulation is agent-agnostic (SDR-002 session-level full
    // aggregation): every turn.step.completed — main agent, subagent, or
    // side-channel — feeds the session totals and the per-turn record,
    // BEFORE the main-agent transcript filter below can drop subagent frames.
    // Per-prompt attribution happens at query time via turnPromptIdByTurnId
    // (subagent turns never bind a prompt, so they stay unattributable).
    if (rawType === 'turn.step.completed') {
      const u = normalizeUsage(p?.usage);
      // Only the main agent's first step is the session's cold-start
      // baseline (SDR-004 d): subagent steps complete inside the main
      // agent's step-1 tool window, so an agent-agnostic guard would
      // capture a subagent call as the baseline. Frames without an agent
      // tag are main-agent frames by convention (see the frame filter).
      const frameAgentId: unknown = p?.agentId;
      const isMainFrame = frameAgentId === undefined || frameAgentId === MAIN_AGENT_ID;
      // SDR-004: capture on the first MAIN-agent step while the session is
      // still live and unseeded. Despite the agent-agnostic SDR-002 totals
      // above, a subagent completing inside step 1 accumulates first — so
      // seed protection rides an explicit seedUsage marker, not a
      // zero-totals heuristic (which would block the main baseline after
      // subagent accumulation). A snapshot-seeded session keeps its seeded
      // baseline, and a seeded session without one stays honestly absent.
      if (isMainFrame && s.firstCall === null && !s.usageSeeded) {
        s.firstCall = {
          inputTokens: u.input,
          outputTokens: u.output,
          cacheReadTokens: u.cacheRead,
          cacheCreationTokens: u.cacheCreate,
        };
      }
      s.totalInput += u.input;
      s.totalOutput += u.output;
      s.totalCacheRead += u.cacheRead;
      s.totalCacheCreate += u.cacheCreate;

      const key = turnKey(p?.turnId);
      if (key !== undefined) {
        const turnUsage =
          s.turnUsageByTurnId[key] ??
          (s.turnUsageByTurnId[key] = {
            inputTokens: 0,
            outputTokens: 0,
            cacheReadTokens: 0,
            cacheCreationTokens: 0,
            models: [],
          });
        turnUsage.inputTokens += u.input;
        turnUsage.outputTokens += u.output;
        turnUsage.cacheReadTokens += u.cacheRead;
        turnUsage.cacheCreationTokens += u.cacheCreate;
        const model = stringField(p ?? {}, 'model');
        if (model && !turnUsage.models.includes(model)) {
          turnUsage.models.push(model);
        }
      }
    }

    // Drop subagent-scoped transcript frames (see MAIN_AGENT_TRANSCRIPT_FRAMES).
    // A subagent carries its own agentId; only the main agent's stream builds the
    // visible transcript. Lifecycle frames (subagent.*, goal.*, background.*) are
    // intentionally NOT in the set — they describe the subagent for the task view
    // and must always be projected.
    const frameAgentId: unknown = p?.agentId;
    if (typeof frameAgentId === 'string' && frameAgentId !== MAIN_AGENT_ID) {
      const isSideChannel = sideChannelAgents.has(frameAgentId);
      // Side-channel agents (e.g. BTW side chat) stream text/thinking deltas and
      // a turn boundary over the parent session channel. Route them to the web
      // layer as agent-scoped events instead of dropping them or folding them
      // into the parent transcript.
      if (isSideChannel && (rawType === 'thinking.delta' || rawType === 'assistant.delta')) {
        const deltaText: string = p?.delta ?? '';
        if (!deltaText) return [];
        return [
          {
            type: 'agentDelta' as const,
            sessionId,
            agentId: frameAgentId,
            delta: { [rawType === 'thinking.delta' ? ('thinking' as const) : ('text' as const)]: deltaText },
          },
        ];
      }
      if (isSideChannel && rawType === 'turn.ended') {
        return [
          { type: 'agentTurnEnded' as const, sessionId, agentId: frameAgentId, reason: p?.reason },
        ];
      }
      if (MAIN_AGENT_TRANSCRIPT_FRAMES.has(rawType)) {
        return projectSubagentProgress(s, sessionId, frameAgentId, rawType, p ?? {}, sideChannelAgents);
      }
      // CR5 agent attribution: compaction frames are session-level state for
      // the MAIN agent only. A member agent's own auto-compaction replaces the
      // member's history, not the session's visible one — emitting the
      // session-scoped compactionCompleted/historyCompacted here would clobber
      // the session-level compaction state (divider/banner) with the member's
      // numbers. Route the member's frame to its task row as progress instead.
      // Absent agentId falls through (legacy engines: absent === main).
      if (COMPACTION_FRAMES.has(rawType)) {
        return projectSubagentProgress(s, sessionId, frameAgentId, rawType, p ?? {}, sideChannelAgents);
      }
    }

    switch (rawType) {
      // -----------------------------------------------------------------------
      case 'session.meta.updated': {
        // The daemon auto-generates a title from the first prompt (and other
        // clients can rename a session); it also reports the latest user prompt
        // via patch.lastPrompt. It announces all of these via this event. We
        // don't have the full AppSession here, so emit a lightweight
        // sessionMetaUpdated that patches only the changed meta fields.
        const title: string | undefined = p?.patch?.title ?? p?.title;
        const lastPrompt: string | undefined = p?.patch?.lastPrompt;
        const patch: { title?: string; lastPrompt?: string } = {};
        if (typeof title === 'string' && title.length > 0) patch.title = title;
        if (typeof lastPrompt === 'string') patch.lastPrompt = lastPrompt;
        if (patch.title !== undefined || patch.lastPrompt !== undefined) {
          out.push({ type: 'sessionMetaUpdated', sessionId, ...patch });
        }
        break;
      }

      // -----------------------------------------------------------------------
      case 'prompt.submitted': {
        const promptId: string | undefined = p?.promptId;
        const userMessageId: string | undefined = p?.userMessageId;
        if (!promptId || !userMessageId) break;
        const content = toAppPromptContent(p?.content);
        if (content.length === 0) break;
        s.currentPromptId = promptId;
        const msg = startUserMessage(
          s,
          sessionId,
          promptId,
          userMessageId,
          content,
          typeof p?.createdAt === 'string' ? p.createdAt : new Date().toISOString(),
        );
        out.push({ type: 'messageCreated', message: cloneMessage(msg) });
        break;
      }

      // -----------------------------------------------------------------------
      case 'turn.started': {
        // Bind turnId → promptId. Generate a synthetic one if none was pre-bound.
        // Session busy is intentionally NOT projected here — the daemon's
        // `event.session.work_changed` is the single source of the busy fact
        // (it re-reads the authoritative drain registry and dedupes per real
        // transition); projecting a second busy flip per turn from the raw
        // stream made every turn-end consumer fire twice.
        const turnId: string | number | undefined = p?.turnId;
        const existingPromptId = s.currentPromptId ?? ulid('pr_');
        s.currentPromptId = existingPromptId;
        bindTurnPromptId(s, turnId, existingPromptId);
        // Fresh turn → fresh step stream offsets.
        s.turnTextLen = 0;
        s.turnThinkLen = 0;
        // Main-conversation liveness (the moon) keys off the main agent's turn
        // boundary directly — only main-agent frames reach this switch arm.
        // `startedAtMs` stamps the client wall-clock of the observed
        // turn.started: the live-observed anchor for the streaming header's
        // elapsed clock (the server snapshot's `current_turn_started_at` is
        // the authoritative anchor for restored sessions and overwrites this
        // estimate when a snapshot applies).
        out.push({ type: 'turnActiveChanged', sessionId, active: true, startedAtMs: Date.now() });
        break;
      }

      // -----------------------------------------------------------------------
      case 'turn.step.started': {
        const turnId: string | number | undefined = p?.turnId;
        let promptId = s.turnPromptId.get(turnId) ?? s.currentPromptId;
        if (!promptId) {
          // Joined mid-turn (reconnect/resync wiped the binding): synthesize a
          // promptId like turn.started does, so the REST of the turn still
          // renders instead of every following event being dropped.
          promptId = ulid('pr_');
          s.currentPromptId = promptId;
          bindTurnPromptId(s, turnId, promptId);
        }

        // Fresh step → fresh stream offsets: the server's delta `offset` is
        // step-relative, so without this reset every delta from step 2 on is
        // silently skipped or misread as a gap.
        s.turnTextLen = 0;
        s.turnThinkLen = 0;

        // A retry continuation: refill the bubble turn.step.retrying cleared,
        // instead of creating a second bubble with the same step's content.
        if (s.retryReuseMsgId !== undefined) {
          const reuseId = s.retryReuseMsgId;
          s.retryReuseMsgId = undefined;
          if (getMsgById(s, reuseId) !== undefined) {
            s.currentAssistantMsgId = reuseId;
            break;
          }
        }

        // Create a new pending assistant message
        const msg = startAssistantMessage(s, sessionId, promptId);
        s.currentAssistantMsgId = msg.id;

        out.push({ type: 'messageCreated', message: cloneMessage(msg) });
        break;
      }

      // -----------------------------------------------------------------------
      case 'thinking.delta': {
        const msgId = s.currentAssistantMsgId;
        if (!msgId) break;
        const delta: string = p?.delta ?? '';
        if (!delta) break;

        // Same missed-turn-boundary self-heal as assistant.delta (see there).
        if (meta?.offset === 0 && s.turnThinkLen > 0) {
          s.turnThinkLen = 0;
        }

        const align = alignDelta(s.turnThinkLen, meta?.offset);
        if (align === 'skip') break;
        if (align === 'gap') {
          out.push({ type: 'historyCompacted', sessionId, beforeSeq: 0, reason: 'delta_gap' });
          break;
        }

        const thinkIdx = appendAssistantDelta(s, msgId, 'thinking', delta);
        if (thinkIdx < 0) break;
        s.turnThinkLen += delta.length;
        out.push({
          type: 'assistantDelta',
          sessionId,
          messageId: msgId,
          contentIndex: thinkIdx,
          delta: { thinking: delta },
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'assistant.delta': {
        const msgId = s.currentAssistantMsgId;
        if (!msgId) break;
        const delta: string = p?.delta ?? '';
        if (!delta) break;

        // Self-heal a missed turn boundary: a pre-append offset of 0 while we
        // still believe we are mid-stream means the daemon began a fresh
        // assistant stream (new turn / retry) whose turn.started we never saw —
        // e.g. the durable replay and the live volatile deltas raced on the
        // cursor after a reconnect. Without this reset every delta has
        // offset < turnTextLen and is SILENTLY skipped forever (skip, unlike
        // gap, never recovers), so streaming dies until a full page reload.
        if (meta?.offset === 0 && s.turnTextLen > 0) {
          s.turnTextLen = 0;
        }

        const align = alignDelta(s.turnTextLen, meta?.offset);
        if (align === 'skip') break;
        if (align === 'gap') {
          // Deltas were missed in the snapshot↔subscribe window — the only
          // exact recovery is a fresh snapshot. historyCompacted is routed to
          // onResync by the client wrapper, which reloads via snapshot.
          out.push({ type: 'historyCompacted', sessionId, beforeSeq: 0, reason: 'delta_gap' });
          break;
        }

        const textIdx = appendAssistantDelta(s, msgId, 'text', delta);
        if (textIdx < 0) break;
        s.turnTextLen += delta.length;
        out.push({
          type: 'assistantDelta',
          sessionId,
          messageId: msgId,
          contentIndex: textIdx,
          delta: { text: delta },
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'tool.use':
      case 'tool.call.started': {
        const msgId = s.currentAssistantMsgId;
        const turnId: string | number | undefined = p?.turnId;
        const promptId = s.turnPromptId.get(turnId) ?? s.currentPromptId;
        if (!msgId || !promptId) break;

        const toolCallId: string = p?.toolCallId;
        // Real daemon field name is 'name' per event-projector.ts
        const toolName: string = p?.name ?? p?.toolName ?? '';
        const args = p?.args ?? p?.input ?? {};

        appendToolUse(s, msgId, toolCallId, toolName, args);

        const msg = getMsgById(s, msgId);
        const contentIndex = msg ? msg.content.length - 1 : 0;

        // Record start time
        s.toolStartTimes.set(toolCallId, Date.now());

        // Emit messageUpdated so the reducer knows about the new tool-use slot
        if (msg) {
          out.push({
            type: 'messageUpdated',
            sessionId,
            messageId: msgId,
            content: msg.content.map((c) => ({ ...c })),
            status: 'pending',
          });
        }
        void contentIndex;
        break;
      }

      // -----------------------------------------------------------------------
      case 'tool.call.delta': {
        // Input streaming — no-op for the web client (content already in tool.call.started.args)
        break;
      }

      // -----------------------------------------------------------------------
      case 'tool.progress': {
        const toolCallId: string = p?.toolCallId;
        const progress = toolProgressOutput(p ?? {});
        if (toolCallId && progress) {
          out.push({
            type: 'toolOutput',
            sessionId,
            toolCallId,
            outputChunk: progress.outputChunk,
            stream: progress.stream,
          });
        }
        break;
      }

      // -----------------------------------------------------------------------
      case 'tool.result': {
        const turnId: string | number | undefined = p?.turnId;
        let promptId = s.turnPromptId.get(turnId) ?? s.currentPromptId;
        if (!promptId) {
          // Same mid-turn-join fallback as turn.step.started.
          promptId = ulid('pr_');
          s.currentPromptId = promptId;
          bindTurnPromptId(s, turnId, promptId);
        }

        const toolCallId: string = p?.toolCallId;
        const output = p?.output;
        const isError: boolean = p?.isError ?? false;

        const startTime = s.toolStartTimes.get(toolCallId) ?? Date.now();
        s.toolStartTimes.delete(toolCallId);
        void (Date.now() - startTime); // duration — unused at client level

        const resultMsg = appendToolResultMessage(s, sessionId, toolCallId, output, isError, promptId);
        out.push({ type: 'messageCreated', message: cloneMessage(resultMsg) });

        // Reset assistant message tracking — next step.started will create a fresh one
        s.currentAssistantMsgId = undefined;
        break;
      }

      // -----------------------------------------------------------------------
      case 'turn.step.completed': {
        const msgId = s.currentAssistantMsgId;

        // Usage was already accumulated above (agent-agnostically, before the
        // main-agent transcript filter) — both the session totals and the
        // per-turn record keyed by this frame's turnId.

        if (msgId) {
          finishAssistantMessage(s, msgId);
          const msg = getMsgById(s, msgId);
          if (msg) {
            out.push({
              type: 'messageUpdated',
              sessionId,
              messageId: msgId,
              content: msg.content.map((c) => ({ ...c })),
              status: 'completed',
            });
          }
        }
        break;
      }

      // -----------------------------------------------------------------------
      case 'agent.status.updated': {
        if (p?.model) s.model = p.model;
        if (p?.contextTokens !== undefined) s.contextTokens = p.contextTokens;
        if (p?.maxContextTokens !== undefined) s.contextLimit = p.maxContextTokens;

        out.push({
          type: 'sessionUsageUpdated',
          sessionId,
          usage: buildUsageSnapshot(s),
          // Carry the live model so the status bar shows the real running model
          // instead of falling back to the daemon's (empty) REST model.
          model: s.model || undefined,
          swarmMode: p?.swarmMode === true ? true : p?.swarmMode === false ? false : undefined,
          // The agent reports plan mode here too (e.g. it auto-entered plan mode
          // for a "make a plan" prompt). Carry it so the composer's plan toggle
          // reflects the agent's real state, not just the user's manual choice.
          planMode: p?.planMode === true ? true : p?.planMode === false ? false : undefined,
          // Same treatment for the collaboration toggle (opt-in exposure of
          // the Collaboration* tools).
          collaborationMode:
            p?.collaborationMode === true ? true : p?.collaborationMode === false ? false : undefined,
          // The session's own thinking level, so per-session state stays in sync
          // across clients (same treatment as plan/swarm above).
          thinking:
            typeof p?.thinkingEffort === 'string' && p.thinkingEffort.length > 0
              ? p.thinkingEffort
              : undefined,
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'turn.ended': {
        const msgId = s.currentAssistantMsgId;
        const reason: string = p?.reason ?? 'completed';
        const durationMs = numberField(p ?? {}, 'durationMs');
        const wireTurnId = numberField(p ?? {}, 'wireTurnId');
        const oldPromptId = s.currentPromptId;

        // Main-conversation liveness: the prompt this turn served is done.
        // This — not the session-busy status — is what ends the working moon.
        // It MUST be emitted first in this arm: the onMainTurnEnd side effect
        // gates on `seq > lastSeqBySession`, and sibling events in this arm
        // advance that cursor — emitted after them, this event would compare
        // equal and the prompt-finish cleanup (moon, queue drain) would never
        // fire (observed: moon stuck when a turn ends with background tasks
        // still running, where no work_changed(busy:false) fallback exists).
        out.push({ type: 'turnActiveChanged', sessionId, active: false, reason: p?.reason });

        // A failed turn's structured error surfaces through the existing
        // agent-error toast chain (reducer's buildAgentErrorNotice →
        // WarningToasts): single-line bottom-right toast with Show details
        // and Copy. The transcript stays clean — no error text blocks.
        if (reason === 'failed' && p && typeof p === 'object') {
          const err = (p as Record<string, unknown>)['error'];
          const errRecord = err && typeof err === 'object' ? (err as Record<string, unknown>) : null;
          const errMessage = typeof errRecord?.['message'] === 'string' ? errRecord['message'] : undefined;
          const errCode = typeof errRecord?.['code'] === 'string' ? errRecord['code'] : undefined;
          if (errMessage !== undefined || errCode !== undefined) {
            out.push({
              type: 'unknown',
              raw: {
                _agentError: true,
                code: errCode,
                message: errMessage,
                name: undefined,
                details: undefined,
                retryable: typeof errRecord?.['retryable'] === 'boolean' ? errRecord['retryable'] : undefined,
                channel: 'turn-failure',
              },
            });
          }
        }

        if (msgId) {
          finishAssistantMessage(s, msgId);
          const msg = getMsgById(s, msgId);
          if (msg) {
            out.push({
              type: 'messageUpdated',
              sessionId,
              messageId: msgId,
              content: msg.content.map((c) => ({ ...c })),
              status: reason === 'failed' || reason === 'filtered' ? 'error' : 'completed',
              durationMs,
            });
          }
        }

        s.turnCount++;
        const usageSnapshot = buildUsageSnapshot(s);
        out.push({ type: 'sessionUsageUpdated', sessionId, usage: usageSnapshot });

        // Fork-at-turn anchor: the engine reports the turn's numeric wire
        // id — rebind the turn's correlation-id key (the `turn-<uuid>` the
        // submit bound) to the durable `turn-<id>` key, the same identity a
        // snapshot reload's usage fold produces. Without this, rows of a
        // turn that completed live keep the uuid key until a reload, and
        // the fork action (which anchors on `turn-<id>`) can never run.
        if ((wireTurnId ?? 0) > 0 && oldPromptId !== undefined && oldPromptId !== `turn-${wireTurnId}`) {
          const anchorId = `turn-${wireTurnId}`;
          for (const key of Object.keys(s.turnPromptIdByTurnId)) {
            if (s.turnPromptIdByTurnId[key] === oldPromptId) s.turnPromptIdByTurnId[key] = anchorId;
          }
          for (const [key, value] of s.turnPromptId) {
            if (value === oldPromptId) s.turnPromptId.set(key, anchorId);
          }
          out.push({ type: 'turnAnchored', sessionId, promptId: oldPromptId, anchorId });
        }

        // No busy projection here — see turn.started. The daemon's
        // `event.session.work_changed` flips the session busy fact.

        // Clear per-turn state. Reset the stream offsets too so a stale length
        // from this turn can't wedge the next turn's delta alignment into a
        // silent skip if its turn.started is missed across a reconnect. The
        // retry reuse target is per-turn as well: if the turn died between
        // turn.step.retrying and the retried step.started, the next prompt
        // must open a fresh bubble, not refill this turn's emptied one.
        s.currentAssistantMsgId = undefined;
        s.currentPromptId = undefined;
        s.turnTextLen = 0;
        s.turnThinkLen = 0;
        s.retryReuseMsgId = undefined;
        break;
      }

      // -----------------------------------------------------------------------
      case 'prompt.completed': {
        // No state change at AppEvent level — turn.ended / the session
        // work_changed ahead of this event already finished the prompt. The
        // event rides along so the web layer can spot the one case that has no
        // turn-level signal: a prompt blocked before any turn started (reason
        // 'blocked'/'filtered'), which would otherwise pin the in-flight state forever.
        const promptId: string | undefined = p?.promptId;
        if (typeof promptId === 'string' && promptId.length > 0) {
          out.push({ type: 'promptCompleted', sessionId, promptId, reason: p?.reason ?? 'completed' });
        }
        break;
      }

      // -----------------------------------------------------------------------
      case 'prompt.aborted': {
        // Fires both for an active-turn abort (a turn.ended + work_changed
        // precede it — the prompt is already finished) and for a QUEUED prompt
        // that never started a turn (no turn events, no busy flip). The web
        // layer keys on promptId to clear the in-flight state in the latter case.
        const promptId: string | undefined = p?.promptId;
        if (typeof promptId === 'string' && promptId.length > 0) {
          out.push({ type: 'promptAborted', sessionId, promptId });
        }
        break;
      }

      // -----------------------------------------------------------------------
      case 'turn.step.retrying': {
        // The step's stream restarts from offset 0. Reuse the abandoned
        // bubble instead of stacking a new one: strip its streamed parts and
        // keep the id in retryReuseMsgId so the retried step.started refills
        // it in place. Otherwise the failed attempt's partial bubble stays
        // rendered next to the retry's full stream — the "text/tool shown
        // twice" duplication (far more visible since the retry budget grew).
        const msgId = s.currentAssistantMsgId;
        if (msgId !== undefined) {
          const msg = getMsgById(s, msgId);
          if (msg !== undefined) {
            msg.content = msg.content.filter(
              (c) => c.type !== 'text' && c.type !== 'thinking' && c.type !== 'toolUse',
            );
            out.push({
              type: 'messageUpdated',
              sessionId,
              messageId: msgId,
              content: msg.content.map((c) => ({ ...c })),
              status: 'pending',
            });
            s.retryReuseMsgId = msgId;
          }
        }
        s.turnTextLen = 0;
        s.turnThinkLen = 0;
        s.toolStartTimes.clear();
        break;
      }

      case 'turn.step.interrupted': {
        // Discard current assistant message; next step.started will create a new one
        s.currentAssistantMsgId = undefined;
        break;
      }

      // -----------------------------------------------------------------------
      case 'subagent.spawned': {
        const taskId = typeof p?.subagentId === 'string' && p.subagentId.length > 0 ? p.subagentId : ulid('task_');
        const task: AppTask = {
          id: taskId,
          // Cross-pipeline join key — see patchSubagent.
          agentId: taskId,
          sessionId,
          kind: 'subagent',
          description: typeof p?.description === 'string' ? p.description : p?.subagentName ?? 'Sub Agent',
          status: 'running',
          createdAt: new Date().toISOString(),
          subagentPhase: 'queued',
          subagentType: typeof p?.subagentName === 'string' ? p.subagentName : undefined,
          parentToolCallId: typeof p?.parentToolCallId === 'string' ? p.parentToolCallId : undefined,
          swarmIndex: typeof p?.swarmIndex === 'number' ? p.swarmIndex : undefined,
          runInBackground: p?.runInBackground === true,
          model: typeof p?.model === 'string' ? p.model : undefined,
        };
        s.subagentMeta.set(task.id, task);
        out.push({
          type: 'taskCreated',
          sessionId,
          task,
        });
        break;
      }

      case 'subagent.started': {
        const task = patchSubagent(s, sessionId, p?.subagentId, {
          subagentPhase: 'working',
          status: 'running',
          startedAt: new Date().toISOString(),
        });
        if (task) out.push({ type: 'taskCreated', sessionId, task });
        break;
      }

      case 'subagent.suspended': {
        // ADR-011: the wire `reason` is either the structured snake_case object
        // `{ kind: "approval" | "question" | "rate_limit", request_id?,
        // tool_call_id? }` (the engine's event projection) or a pre-ADR-011
        // free-form string (legacy compat window). Discriminate with `typeof`:
        //   structured approval/question → awaiting (ADR-008 ① — the event
        //     drives the awaiting-user state, no derivation)
        //   structured rate_limit        → backoff (退避不是等用户)
        //   legacy string                → suspended (NO string sniffing —
        //     "rate limit" prose in a string maps here, pinned by test; the
        //     compat window ends with Task 7)
        const reason = (p ?? {})['reason'];
        const structured =
          !!reason &&
          typeof reason === 'object' &&
          typeof (reason as Record<string, unknown>)['kind'] === 'string';
        let subagentPhase: AppTask['subagentPhase'];
        let suspendedReason: AppTask['suspendedReason'];
        if (structured) {
          const wireReason = reason as Record<string, unknown>;
          const kind = wireReason['kind'] as string;
          // Only the KNOWN kinds drive the classified phases — an unknown
          // structured kind (a future ADR vocabulary member the client has
          // not learned) falls to the honest generic `suspended`, never to
          // `awaiting` (the member is NOT awaiting the user just because
          // the reason is an object).
          subagentPhase =
            kind === 'rate_limit'
              ? 'backoff'
              : kind === 'approval' || kind === 'question'
                ? 'awaiting'
                : 'suspended';
          if (kind === 'approval' || kind === 'question' || kind === 'rate_limit') {
            suspendedReason = {
              kind,
              requestId: typeof wireReason['request_id'] === 'string' ? wireReason['request_id'] : undefined,
              toolCallId:
                typeof wireReason['tool_call_id'] === 'string' ? wireReason['tool_call_id'] : undefined,
            };
          } else {
            suspendedReason = undefined;
          }
        } else {
          subagentPhase = 'suspended';
          suspendedReason = typeof reason === 'string' ? reason : undefined;
        }
        const task = patchSubagent(s, sessionId, p?.subagentId, {
          subagentPhase,
          status: 'running',
          suspendedReason,
        });
        if (task) out.push({ type: 'taskCreated', sessionId, task });
        break;
      }

      case 'subagent.completed': {
        const outputPreview = typeof p?.resultSummary === 'string' ? p.resultSummary : undefined;
        const task = patchSubagent(s, sessionId, p?.subagentId, {
          subagentPhase: 'completed',
          status: 'completed',
          completedAt: new Date().toISOString(),
          outputPreview,
        });
        if (task) out.push({ type: 'taskCreated', sessionId, task });
        out.push({
          type: 'taskCompleted',
          sessionId,
          taskId: p?.subagentId ?? '',
          status: 'completed',
          outputPreview,
        });
        break;
      }

      case 'subagent.failed': {
        const outputPreview = typeof p?.error === 'string' ? p.error : undefined;
        const task = patchSubagent(s, sessionId, p?.subagentId, {
          subagentPhase: 'failed',
          status: 'failed',
          completedAt: new Date().toISOString(),
          outputPreview,
        });
        if (task) out.push({ type: 'taskCreated', sessionId, task });
        out.push({
          type: 'taskCompleted',
          sessionId,
          taskId: p?.subagentId ?? '',
          status: 'failed',
          outputPreview,
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'error': {
        // Fold into an unknown event so the reducer surfaces it as a structured
        // error notice (semantic title + code/status/requestId details). The
        // wire payload already carries name/details/retryable — pass them
        // through untouched; the reducer decides what to display.
        out.push({
          type: 'unknown',
          raw: {
            _agentError: true,
            code: p?.code,
            message: p?.message,
            name: p?.name,
            details: p?.details,
            retryable: p?.retryable,
          },
        });
        break;
      }

      case 'warning': {
        out.push({
          type: 'unknown',
          raw: { _agentWarning: true, message: p?.message },
        });
        break;
      }

      // -----------------------------------------------------------------------
      // Background tasks (e.g. a backgrounded Bash command). Real daemon shape:
      // payload.info = { taskId, description, status, startedAt(ms), endedAt,
      // kind:'process', command, pid, exitCode }.
      case 'background.task.started': {
        const info = (p?.info ?? {}) as Record<string, unknown>;
        const startedAt =
          typeof info.startedAt === 'number' ? new Date(info.startedAt).toISOString() : undefined;
        const taskId =
          typeof info.taskId === 'string'
            ? info.taskId
            : typeof info.taskId === 'number'
              ? String(info.taskId)
              : ulid('task_');
        const description =
          typeof info.description === 'string'
            ? info.description
            : typeof info.command === 'string'
              ? info.command
              : i18n.global.t('tasks.defaultDescription');
        // A background subagent registers into the background-task store under
        // a fresh task id that differs from its agent id. Record the task id on
        // the existing WS-owned row (keyed by agent id) instead of adding a
        // second row — REST `/tasks` returns the same agent keyed by task id,
        // and keepLiveSubagents folds that copy into this row.
        if (info.kind === 'agent') {
          const agentId =
            typeof info.agentId === 'string' && info.agentId.length > 0
              ? info.agentId
              : undefined;
          if (agentId !== undefined) {
            // Key by agent id even when the spawn event never reached this
            // client (subscribed late): later agent-scoped progress frames are
            // routed by agent id, and seeding subagentMeta here keeps them on
            // this one row instead of synthesizing a second one.
            const task = patchSubagent(s, sessionId, agentId, {
              description,
              backgroundTaskId: taskId,
              runInBackground: true,
            });
            if (task) out.push({ type: 'taskCreated', sessionId, task });
          } else {
            // No agent id — nothing to link; key the row by the background
            // task id so the REST poll dedupes it.
            out.push({
              type: 'taskCreated',
              sessionId,
              task: {
                id: taskId,
                sessionId,
                kind: 'subagent',
                description,
                status: 'running',
                createdAt: startedAt ?? new Date().toISOString(),
                startedAt,
                subagentPhase: 'queued',
                runInBackground: true,
              },
            });
          }
          break;
        }
        const command = typeof info.command === 'string' ? info.command : undefined;
        out.push({
          type: 'taskCreated',
          sessionId,
          task: {
            id: taskId,
            sessionId,
            kind: 'bash',
            description,
            command,
            status: 'running',
            createdAt: startedAt ?? new Date().toISOString(),
            startedAt,
            outputPreview: command !== undefined ? `$ ${command}` : undefined,
          },
        });
        break;
      }
      case 'background.task.terminated': {
        const info = (p?.info ?? {}) as Record<string, unknown>;
        // The rust BackgroundTaskInfo status vocabulary is running /
        // completed / failed / killed (manager-stopped) — killed settles as
        // cancelled, never as a successful completion.
        const terminalStatus: AppTaskStatus =
          info.status === 'failed' ||
          (typeof info.exitCode === 'number' && info.exitCode !== 0)
            ? 'failed'
            : info.status === 'killed'
              ? 'cancelled'
              : 'completed';

        // For agent-kind background tasks, also sync subagentPhase so the
        // detail panel badge updates even when this is the only termination
        // signal (e.g. subagent.completed event was missed due to WS disconnect).
        if (info.kind === 'agent') {
          const agentId =
            typeof info.agentId === 'string' && info.agentId.length > 0
              ? info.agentId
              : undefined;
          if (agentId !== undefined) {
            // `status` rides the task vocabulary (cancelled), while the
            // subagent phase keeps its own vocabulary — a killed member is
            // `aborted`, the closest terminal word AppSubagentPhase carries.
            const task = patchSubagent(s, sessionId, agentId, {
              subagentPhase:
                terminalStatus === 'cancelled' ? 'aborted' : terminalStatus,
              status: terminalStatus,
              completedAt: new Date().toISOString(),
            });
            if (task) out.push({ type: 'taskCreated', sessionId, task });
          }
        }

        out.push({
          type: 'taskCompleted',
          sessionId,
          taskId:
            typeof info.taskId === 'string'
              ? info.taskId
              : typeof info.taskId === 'number'
                ? String(info.taskId)
                : '',
          status: terminalStatus,
          // Do NOT set outputPreview here. The command is already kept on the
          // task as `command`; setting outputPreview to `$ <command>` would
          // clobber any real output captured by polling and prevents the UI
          // from fetching the final terminal output after the task finishes.
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'compaction.completed': {
        // Compaction replaced a batch of old messages with a summary on the
        // daemon side. The visible transcript is NOT reloaded (the client keeps
        // the scrollback and the reducer appends a divider marker); the
        // historyCompacted signal still fires so seq bookkeeping and any
        // non-compaction consumers stay correct.
        const result = (p?.result ?? {}) as Record<string, unknown>;
        out.push({
          type: 'compactionCompleted',
          sessionId,
          tokensBefore: typeof result.tokensBefore === 'number' ? result.tokensBefore : undefined,
          tokensAfter: typeof result.tokensAfter === 'number' ? result.tokensAfter : undefined,
          summary: typeof result.summary === 'string' ? result.summary : undefined,
        });
        out.push({
          type: 'historyCompacted',
          sessionId,
          beforeSeq: 0,
          reason: 'auto_compact',
        });
        break;
      }

      case 'compaction.started': {
        out.push({
          type: 'compactionStarted',
          sessionId,
          trigger: p?.trigger === 'manual' ? 'manual' : 'auto',
          instruction: typeof p?.instruction === 'string' ? p.instruction : undefined,
        });
        break;
      }

      case 'compaction.cancelled': {
        out.push({ type: 'compactionCancelled', sessionId });
        break;
      }

      case 'goal.updated': {
        const goal = mapGoalSnapshot(p?.snapshot ?? null);
        out.push({
          type: 'goalUpdated',
          sessionId,
          goal: goal?.status === 'complete' ? null : goal,
        });
        break;
      }

      // -----------------------------------------------------------------------
      case 'cron.fired': {
        // A scheduled reminder fired into the session. agent-core persists the
        // injected user message (so a refresh renders it via messagesToTurns),
        // but turn.steer() does NOT broadcast a prompt.submitted / message.created
        // for it — synthesize one here so the notice shows up live too. A later
        // snapshot reload replaces the message log wholesale, so this synthesized
        // copy never duplicates the persisted one. The promptId is intentionally
        // omitted: the web client caches every user message's promptId into
        // promptIdBySession for Stop/abort, and a synthetic id the daemon would
        // reject would clobber the real active promptId. The reducer already skips
        // optimistic-echo reconciliation for cron-origin messages, so no promptId
        // is needed for de-dup either.
        const origin = p?.origin;
        const promptText = stringField(p ?? {}, 'prompt');
        if (
          origin &&
          typeof origin === 'object' &&
          (origin as Record<string, unknown>)['kind'] === 'cron_job' &&
          promptText
        ) {
          const msg: AppMessage = {
            id: ulid('cron_'),
            sessionId,
            role: 'user',
            content: [{ type: 'text', text: promptText }],
            createdAt: new Date().toISOString(),
            metadata: { origin: origin as Record<string, unknown> },
          };
          s.messages.push(msg);
          out.push({ type: 'messageCreated', message: cloneMessage(msg) });
        }
        break;
      }

      // -----------------------------------------------------------------------
      // Explicitly known but not projected
      case 'compaction.blocked':
      case 'hook.result':
      case 'mcp.server.status':
      case 'skill.activated':
      case 'plugin_command.activated':
      case 'tool.list.updated':
      // Shell runner lifecycle (rust core P3i): the user's local shell
      // activity streams through event.shell.*; the web client has no shell
      // console surface, so there is no AppEvent to project onto (the
      // terminal panel is driven by the terminal_* WS frames instead).
      case 'shell.started':
      case 'shell.output':
      case 'shell.completed':
        break;

      // -----------------------------------------------------------------------
      default:
        // Unknown future events — safe no-op
        break;
    }

    return out;
  }

  return {
    project,
    bindNextPromptId,
    state,
    seedInFlight,
    seedTurnUsage,
    reset,
    seedUsage,
    markSideChannelAgent,
  };
}

// ---------------------------------------------------------------------------
// Helpers for integration layer
// ---------------------------------------------------------------------------

/**
 * Detect whether an incoming WS frame type is a raw agent-core event
 * (as opposed to a projected "event.*" protocol event or a control frame).
 *
 * Raw agent-core events do NOT start with "event." and are not control frames.
 * Control frames: server_hello, ack, ping, resync_required, error.
 */
const CONTROL_FRAME_TYPES = new Set([
  'server_hello',
  'ack',
  'ping',
  'resync_required',
  'error',
  'pong',
]);

export function isRawAgentCoreEvent(frameType: string): boolean {
  if (frameType.startsWith('event.')) return false;
  if (CONTROL_FRAME_TYPES.has(frameType)) return false;
  return true;
}

/**
 * Agent-core event names the projector knows how to project. These are the
 * raw events the real daemon emits. The same names may arrive WITH an "event."
 * prefix (newer daemon) or WITHOUT it (older daemon).
 */
const KNOWN_AGENT_CORE_TYPES = new Set([
  'turn.started',
  'turn.step.started',
  'turn.step.completed',
  'turn.step.retrying',
  'turn.step.interrupted',
  'turn.ended',
  'thinking.delta',
  'assistant.delta',
  'tool.call.started',
  'tool.use', // alias the daemon may use for tool.call.started
  'tool.call.delta',
  'tool.progress',
  'tool.result',
  'agent.status.updated',
  'prompt.submitted',
  'prompt.completed',
  'prompt.aborted',
  'session.meta.updated',
  'compaction.started',
  'compaction.completed',
  'compaction.cancelled',
  'compaction.blocked',
  'goal.updated',
  'error',
  'warning',
  'subagent.spawned',
  'subagent.started',
  'subagent.suspended',
  'subagent.completed',
  'subagent.failed',
  'background.task.started',
  'background.task.terminated',
  'cron.fired',
  'skill.activated',
  'plugin_command.activated',
  'tool.list.updated',
  'hook.result',
  // Shell runner lifecycle (rust core P3i) — known but deliberately not
  // projected (see the switch's "Explicitly known but not projected" group).
  'shell.started',
  'shell.output',
  'shell.completed',
]);

/**
 * "event."-prefixed names that are GENUINE protocol events (control/projected
 * events produced server-side). The agent projector must NOT re-handle these —
 * they go through the existing toAppEvent() path. This includes approval /
 * question requests (which drive the approval/question UI) and the no-op-but-
 * known streaming/tool protocol events.
 */
const PROTOCOL_EVENT_NAMES = new Set([
  // Session lifecycle (projected)
  'session.created',
  'session.updated',
  'session.deleted',
  'session.status_changed',
  'session.usage_updated',
  'session.history_compacted',
  // Message lifecycle (projected)
  'message.created',
  'message.updated',
  // Approval / Question — MUST stay on the protocol path to drive the UI
  'approval.requested',
  'approval.resolved',
  'approval.expired',
  'question.requested',
  'question.answered',
  'question.dismissed',
  // Background tasks (projected)
  'task.created',
  'task.progress',
  'task.completed',
  // No-op-but-known protocol streaming / tool events
  'assistant.tool_use_started',
  'assistant.tool_use_delta',
  'assistant.tool_use_completed',
  'assistant.completed',
  'tool.started',
  'tool.output',
  'tool.completed',
]);

/**
 * Names that are ambiguous between the raw agent-core form (payload.delta is a
 * STRING) and the already-projected protocol form (payload.delta is an object
 * { text? | thinking? }, or the payload carries message_id / content_index).
 */
const AMBIGUOUS_DELTA_NAMES = new Set(['assistant.delta', 'thinking.delta']);

export type FrameRoute =
  | { route: 'protocol' }
  | { route: 'agent'; agentType: string }
  | { route: 'ignore' };

/**
 * Classify a (possibly "event."-prefixed) WS frame into the path it should take.
 *
 * - 'protocol' → hand the original frame to toAppEvent() (existing path).
 * - 'agent'    → hand `agentType` + payload to the agent projector.
 * - 'ignore'   → drop (no session context / unroutable).
 *
 * Robust to all three observed shapes:
 *   1) raw agent-core (no prefix):        turn.started, assistant.delta{delta:'…'}
 *   2) "event."-prefixed agent-core:      event.turn.started, event.assistant.delta{delta:'…'}
 *   3) genuine protocol "event.*" events: event.message.created, event.session.*, …
 */
export function classifyFrame(rawType: string, payload: unknown): FrameRoute {
  if (CONTROL_FRAME_TYPES.has(rawType)) return { route: 'ignore' };

  const hasPrefix = rawType.startsWith('event.');
  const name = hasPrefix ? rawType.slice('event.'.length) : rawType;

  // Ambiguous delta events: disambiguate by payload shape regardless of prefix.
  if (AMBIGUOUS_DELTA_NAMES.has(name)) {
    if (deltaIsRawAgentCore(payload)) return { route: 'agent', agentType: name };
    // Object delta or protocol-shaped payload → projected protocol event.
    return { route: 'protocol' };
  }

  // Unprefixed frames are raw agent-core (real daemon) when we know the name.
  if (!hasPrefix) {
    if (KNOWN_AGENT_CORE_TYPES.has(name)) return { route: 'agent', agentType: name };
    // Unknown unprefixed name with no protocol meaning → still try the projector
    // (it safely no-ops on unknown types and advances nothing).
    return { route: 'agent', agentType: name };
  }

  // Prefixed frames: genuine protocol events take priority.
  if (PROTOCOL_EVENT_NAMES.has(name)) return { route: 'protocol' };
  // Prefixed agent-core event (e.g. event.turn.started) → strip + project.
  if (KNOWN_AGENT_CORE_TYPES.has(name)) return { route: 'agent', agentType: name };
  // Unknown "event.*" → let toAppEvent() record it as an unknown protocol event.
  return { route: 'protocol' };
}

/**
 * True when an assistant.delta / thinking.delta payload is in the RAW agent-core
 * form: payload.delta is a plain string, and there is no protocol-only field
 * (message_id / content_index). The protocol form uses delta:{text|thinking}.
 */
function deltaIsRawAgentCore(payload: unknown): boolean {
  if (!payload || typeof payload !== 'object') return false;
  const p = payload as Record<string, unknown>;
  if ('message_id' in p || 'content_index' in p) return false;
  return typeof p['delta'] === 'string';
}
