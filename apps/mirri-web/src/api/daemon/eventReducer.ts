// apps/mirri-web/src/api/daemon/eventReducer.ts
// Pure TypeScript state reducer for MirriClient.
// Operates on plain TS state — no Vue reactivity here.
// The reducer consumes AppEvent (camelCase), produced by toAppEvent() in mappers.ts.
//
// REFERENCE REUSE CONTRACT (event-delivery ADR-002 / SDR-003b):
// cloneState keeps every untouched field's input reference. Every case that
// mutates a field clones it (once, shallowly) immediately before the write.
// applyEvent compares by reference and skips unchanged fields, so a session
// A event never invalidates session B consumers. KNOWN EXCEPTION:
// planReviewByToolCallId is keyed by toolCallId across sessions — plan
// review events invalidate all sessions' turns (accepted, low frequency).
//
// No-op-but-known events (tool.*, assistant streaming, assistant.completed)
// are mapped to { type: 'unknown', raw: { _noop: true, ... } } by mappers.ts.
// The reducer detects `_noop: true` and silently advances lastSeqBySession
// without pushing a warning.

import type {
  AppApprovalRequest,
  AppConfig,
  AppEvent,
  AppGoal,
  AppMcpServerStatus,
  AppMessage,
  AppMessageContent,
  AppNotice,
  AppNoticeDetail,
  AppWarning,
  AppQuestionRequest,
  AppSession,
  AppTask,
  CompactionMarkerMetadata,
} from '../types';
import { COMPACTION_MARKER_METADATA_KEY } from '../types';
import { i18n } from '../../i18n';
import { latestTodosFromContent } from '../../lib/todoStatus';
import { applyTerminalToTask, isTerminalStatus } from '../../lib/taskStatus';

const OPTIMISTIC_USER_MESSAGE_METADATA_KEY = 'mirriWeb.optimisticUserMessage';

/** Tail cap for accumulated output lines on task rows (ADR-015 e): bash /
 *  background stdout AND subagent progress (legacy "Calling …" lines and
 *  structured step output lines) all use the same 40-line tail retention. */
const MAX_BACKGROUND_OUTPUT_LINES = 40;

/** ADR-015: the minimum length a chunk must have before the one-shot
 *  post-boundary tail guard may drop it as a replayed duplicate. Short tokens
 *  (blank lines, single words) are far more likely legitimate repeats of
 *  tail-adjacent content than a replay boundary artifact. */
const MIN_TAIL_DEDUPE_LEN = 16;

/** Skeleton description used by `patchSubagent` in agentEventProjector.ts when
 *  a lifecycle event re-projects a subagent the projector never saw spawn
 *  (e.g. after a page refresh, where the snapshot roster — not the WS stream —
 *  carried the real description). */
const PLACEHOLDER_SUBAGENT_DESCRIPTION = 'Sub Agent';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/** Live compaction progress for a session: present (status 'running') only
    while the daemon is compacting. Completion is recorded as a persistent
    divider marker message in the transcript, not as transient status. */
export interface CompactionStatus {
  status: 'running';
  trigger: 'manual' | 'auto';
}

/** One todo item — structurally the app `TodoView` (`src/types.ts`), kept
 *  local so the pure reducer never imports the app-types layer. */
export interface TodoItem {
  title: string;
  status: 'pending' | 'in_progress' | 'done';
}

export interface MirriClientState {
  sessions: AppSession[];
  activeSessionId?: string;
  messagesBySession: Record<string, AppMessage[]>;
  approvalsBySession: Record<string, AppApprovalRequest[]>;
  /** Preserved `plan_review` displays keyed by toolCallId. Plan content survives
   *  approval resolution so the ExitPlanMode tool card can keep rendering the
   *  plan (approved / rejected / revised) instead of losing it. */
  planReviewByToolCallId: Record<string, { plan: string; path?: string }>;
  questionsBySession: Record<string, AppQuestionRequest[]>;
  tasksBySession: Record<string, AppTask[]>;
  goalBySession: Record<string, AppGoal>;
  /** Monotonic per-session counter bumped on EVERY `goalUpdated` event —
   *  including delete/clear ones — so an async recovery read can detect that a
   *  live event won the race even when the goal entry stayed absent. */
  goalVersionBySession: Record<string, number>;
  lastSeqBySession: Record<string, number>;
  /** MAIN-agent turn in flight, per session — set from the main agent's
   *  turn.started/turn.ended boundary events and seeded from the snapshot's
   *  (main-only) inFlightTurn. Half of the working moon; subagent turns never
   *  reach the events that set this. */
  turnActiveBySession: Record<string, boolean>;
  /** Per-session epoch-ms anchor of the RUNNING turn's real start — the
   *  streaming header's live elapsed clock. Set from the server snapshot
   *  (`inFlightTurn.startedAtMs`, authoritative for restored sessions) and
   *  from the projector's `turn.started` wall-clock stamp
   *  (`turnActiveChanged.startedAtMs` for live-observed turns); cleared when
   *  the turn ends or a snapshot applies without an anchor. Never used to
   *  render a completed turn (durationMs wins there). */
  turnStartAtBySession: Record<string, number>;
  compactionBySession: Record<string, CompactionStatus>;
  /** B7 W6 — tool calls detached into the background (task.detach by
   *  toolCallId), keyed session → toolCallId → background task id. The stamp
   * is monotonic: once set it survives tool results, so the card keeps its
   * `◐ backgrounded` badge instead of faking completion (US12.2). */
  backgroundedToolCalls: Record<string, Record<string, string>>;
  /** The session's folded todo list as carried by the engine's
   *  `session.snapshot` (the wire-folded state, kept in sync by
   *  restore/undo), refreshed by live TodoList writes via the reducer's
   *  `messageUpdated`/`messageCreated` folds. The composable prefers it
   *  over the message-history derivation so a compacted transcript does
   *  not empty the panel (G7). Absent sessions keep the derivation
   *  fallback. */
  todosBySession: Record<string, TodoItem[]>;
  config?: AppConfig | null;
  warnings: AppWarning[];
}

export function createInitialState(): MirriClientState {
  return {
    sessions: [],
    activeSessionId: undefined,
    messagesBySession: {},
    approvalsBySession: {},
    planReviewByToolCallId: {},
    questionsBySession: {},
    tasksBySession: {},
    goalBySession: {},
    goalVersionBySession: {},
    lastSeqBySession: {},
    turnActiveBySession: {},
    turnStartAtBySession: {},
    compactionBySession: {},
    backgroundedToolCalls: {},
    todosBySession: {},
    warnings: [],
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function cloneState(s: MirriClientState): MirriClientState {
  return {
    ...s,
    // Reuse the `sessions` array reference when an event does not touch it.
    // Every session-mutating case below already builds its own array via
    // `[...]` / `.map` / `.filter`, so sharing the reference is safe — and it
    // keeps `rawState.sessions` stable for events that don't change sessions,
    // so the sidebar computeds (sessionsForView / workspaceGroups /
    // mergedWorkspaces) are not dirtied by unrelated events.
    sessions: s.sessions,
    // SDR-003b: untouched fields keep their input references — applyEvent's
    // reference-comparing write-back skips them, so an event for session A
    // never invalidates consumers of session B. Every case below that
    // mutates a map clones it first (shallow) and replaces only the target
    // session's entry; the module-level "reference reuse contract" comment
    // at the top of this file lists the known cross-session exception
    // (planReviewByToolCallId).
    messagesBySession: s.messagesBySession,
    approvalsBySession: s.approvalsBySession,
    planReviewByToolCallId: s.planReviewByToolCallId,
    questionsBySession: s.questionsBySession,
    tasksBySession: s.tasksBySession,
    goalBySession: s.goalBySession,
    goalVersionBySession: s.goalVersionBySession,
    lastSeqBySession: s.lastSeqBySession,
    turnActiveBySession: s.turnActiveBySession,
    turnStartAtBySession: s.turnStartAtBySession,
    compactionBySession: s.compactionBySession,
    backgroundedToolCalls: s.backgroundedToolCalls,
    todosBySession: s.todosBySession,
    warnings: s.warnings,
  };
}

function advanceSeq(state: MirriClientState, sessionId: string | undefined, seq: number | undefined): void {
  if (sessionId !== undefined && seq !== undefined && seq > 0) {
    const prev = state.lastSeqBySession[sessionId] ?? 0;
    if (seq > prev) {
      // Clone before write — the reference-reuse contract forbids mutating
      // the shared map in place (a later event on another session would
      // see the polluted map AND the input state would have changed).
      state.lastSeqBySession = { ...state.lastSeqBySession };
      state.lastSeqBySession[sessionId] = seq;
    }
  }
}

/** Whether a durable event's `seq` skipped over positions the client never
 * observed — a server-side loss (the journal task lagged its bus and jumped
 * the seq) that cursor replay cannot fill. `prev` is the last durable seq
 * observed for the session (0 = no baseline); volatile events (`seq === 0`)
 * never participate. */
export function detectSeqGap(prev: number, seq: number): boolean {
  if (seq === 0) return false;
  if (prev === 0) return false;
  return seq > prev + 1;
}

/** Whether the turn-active watchdog should reconcile a session: a main
 * turn is in flight, the session's event stream has been silent longer
 * than the threshold (the terminal turn.ended may have been lost with no
 * resync signal surviving either), and the session is still subscribed
 * (an unsubscribed session receiving no events is normal). */
export function shouldWatchdogReconcile(
  lastEventAgeMs: number,
  quietThresholdMs: number,
  subscribed: boolean,
): boolean {
  return subscribed && lastEventAgeMs > quietThresholdMs;
}

function isOptimisticUserMessage(message: AppMessage): boolean {
  return (
    message.role === 'user' &&
    message.metadata?.[OPTIMISTIC_USER_MESSAGE_METADATA_KEY] === true
  );
}

function isCronOriginMessage(message: AppMessage): boolean {
  const origin = message.metadata?.['origin'] as { kind?: string } | undefined;
  return origin?.kind === 'cron_job' || origin?.kind === 'cron_missed';
}

/** A background-task terminal notification (B2 T11): like a cron injection,
 *  its envelope text can coincide with a still-optimistic user message, but
 *  it must append as its own turn (the notice card) rather than reconcile
 *  into — and replace — that optimistic echo. */
function isNotificationOriginMessage(message: AppMessage): boolean {
  const origin = message.metadata?.['origin'] as { kind?: string } | undefined;
  return origin?.kind === 'background_task';
}

function sameMessageContent(a: AppMessage, b: AppMessage): boolean {
  return JSON.stringify(a.content) === JSON.stringify(b.content);
}

/** Concatenated text + count of image/file parts — a serialization-independent
    shape of a user message. The daemon's echo carries images as a resolved
    URL/base64 while our optimistic copy carries `{kind:'file',fileId}`, so the
    raw content never matches; comparing (text, image-count) does. */
// Matches the self-contained media path tag the server substitutes for an
// uploaded image/video/audio in a prompt (e.g. `<video path="/cache/f.mp4"></video>`).
// A tag is its own text part, so anchoring keeps ordinary prose from matching.
const MEDIA_PATH_TAG_SHAPE_RE = /^<(image|video|audio)\s+path="[^"]+"><\/\1>$/;

// Matches the compression caption the server inserts before a compressed image
// (buildImageCompressionCaption in agent-core). It is server-generated metadata,
// not user-typed text — skip it entirely so the echo's loose shape still matches
// the optimistic copy (which has no caption).
const IMAGE_COMPRESSION_CAPTION_RE = /^<system>Image compressed to fit model limits:[\s\S]*<\/system>$/;

function userMessageShape(m: AppMessage): { text: string; media: number } {
  let text = '';
  let media = 0;
  for (const c of m.content) {
    if (c.type === 'text') {
      const trimmed = c.text.trim();
      // A video/image upload reaches us (after the server resolves it) as a
      // `<video path=…></video>` text tag, not a media part — count it as media
      // and drop it from the text so the echo reconciles with our optimistic copy.
      if (MEDIA_PATH_TAG_SHAPE_RE.test(trimmed)) media += 1;
      else if (IMAGE_COMPRESSION_CAPTION_RE.test(trimmed)) {
        // Server-generated compression caption — skip entirely (not text, not media).
      } else text += c.text;
    } else if (c.type === 'image' || c.type === 'video' || c.type === 'file') media += 1;
  }
  return { text, media };
}

function sameUserMessageLoosely(a: AppMessage, b: AppMessage): boolean {
  const sa = userMessageShape(a);
  const sb = userMessageShape(b);
  return sa.text === sb.text && sa.media === sb.media;
}

function findOptimisticUserEchoIndex(messages: AppMessage[], message: AppMessage): number {
  // Prefer matching by prompt_id: image content serializes differently between
  // our optimistic copy ({source:{kind:'file',fileId}}) and the daemon's echo
  // (a resolved URL/base64), so content-equality alone lets an image steer's
  // echo slip through as a duplicate. The submit response's prompt_id is stamped
  // onto the optimistic message, so a shared prompt_id is the reliable match.
  const promptId = message.promptId;
  if (promptId !== undefined) {
    for (let i = messages.length - 1; i >= 0; i--) {
      const candidate = messages[i]!;
      if (isOptimisticUserMessage(candidate) && candidate.promptId === promptId) {
        return i;
      }
    }
  }
  for (let i = messages.length - 1; i >= 0; i--) {
    const candidate = messages[i]!;
    if (isOptimisticUserMessage(candidate) && sameMessageContent(candidate, message)) {
      return i;
    }
  }
  // Loose fallback for image steers: the daemon's messageCreated echo can arrive
  // over the WS *before* submitPrompt resolves and stamps the prompt_id onto the
  // optimistic copy, so neither the prompt_id nor the exact-content match fires —
  // and because the image serializes differently, the echo used to slip through
  // as a SECOND user bubble. Match on (text, image-count) instead so the echo
  // still reconciles into the optimistic message.
  for (let i = messages.length - 1; i >= 0; i--) {
    const candidate = messages[i]!;
    if (isOptimisticUserMessage(candidate) && sameUserMessageLoosely(candidate, message)) {
      return i;
    }
  }
  return -1;
}

function appendToolOutputToMessages(messages: AppMessage[], toolCallId: string, outputChunk: string): AppMessage[] {
  let changed = false;
  const next = messages.map((message) => {
    let contentChanged = false;
    const content = message.content.map((part) => {
      if (part.type !== 'toolUse' || part.toolCallId !== toolCallId) return part;
      contentChanged = true;
      return {
        ...part,
        outputLines: [...(part.outputLines ?? []), outputChunk],
      };
    });
    if (!contentChanged) return message;
    changed = true;
    return { ...message, content };
  });
  return changed ? next : messages;
}

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

/** Fold one `taskProgress` chunk onto its task row. Extracted verbatim from
 *  the reducer's taskProgress case (SDR-003b): the chunk-append semantics
 *  (outputLines/thinking/text concatenation) are unchanged — only the write
 *  granularity moved to the caller. Returns the SAME task reference when the
 *  chunk is a no-op (dedupe hit), so the caller can skip the map write. */
function appendTaskProgress(
  task: AppTask,
  event: Extract<AppEvent, { type: 'taskProgress' }>,
): AppTask {
  // Subagent streamed output (assistant.delta) concatenates into a single
  // growing text block rather than fragmenting each delta into its own
  // line — the detail panel renders it like a thinking block. Same for
  // thinking deltas (ADR-009 live narrative's thinking block).
  if (task.kind === 'subagent' && (event.kind === 'text' || event.kind === 'thinking')) {
    // ADR-015: one-shot post-boundary replay guard. Both branches consume
    // `tailGuard` on the FIRST text/thinking chunk after a boundary; a hit
    // requires an exact ≥16-char tail repeat, so legitimate steady-state
    // content is never dropped.
    if (task.tailGuard === true) {
      const current = event.kind === 'text' ? (task.text ?? '') : (task.thinking ?? '');
      if (
        event.outputChunk.length >= MIN_TAIL_DEDUPE_LEN &&
        current.length > 0 &&
        current.endsWith(event.outputChunk)
      ) {
        return { ...task, tailGuard: undefined }; // replay of the pre-boundary tail: drop
      }
      return event.kind === 'text'
        ? { ...task, text: (task.text ?? '') + event.outputChunk, tailGuard: undefined }
        : { ...task, thinking: (task.thinking ?? '') + event.outputChunk, tailGuard: undefined };
    }
    if (event.kind === 'text') return { ...task, text: (task.text ?? '') + event.outputChunk };
    return { ...task, thinking: (task.thinking ?? '') + event.outputChunk };
  }
  const outputLines = task.outputLines ?? [];
  if (outputLines.at(-1) === event.outputChunk) return task;
  const lines = [...outputLines, event.outputChunk];
  return {
    ...task,
    // ADR-015 e: legacy synthesized "Calling …" lines are capped for
    // subagents too — the detail panel's narrative is driven by the
    // structured steps (ADR-009); these lines feed the dock's activity
    // line, which needs the tail, not the history.
    outputLines: lines.slice(-MAX_BACKGROUND_OUTPUT_LINES),
  };
}

/** Agent error code → semantic title key under `warnings.agentError`. Codes
 *  come from the protocol error domain (agent-core-v2 `ProtocolErrors`);
 *  anything unmapped falls back to the generic `title`. */
const AGENT_ERROR_TITLE_KEYS: Readonly<Record<string, string>> = {
  'provider.connection_error': 'connection',
  'provider.auth_error': 'auth',
  'provider.rate_limit': 'rateLimit',
  'provider.overloaded': 'overloaded',
  'provider.filtered': 'filtered',
  'provider.api_error': 'api',
  'context.overflow': 'contextOverflow',
};

interface AgentErrorRaw {
  code?: string;
  message?: string;
  name?: string;
  details?: Record<string, unknown>;
  /** Routing tag carried through to the notice (see AppNotice.channel). */
  channel?: 'turn-failure';
}

/**
 * Build the structured error notice for a failed agent turn (typically a
 * model-provider failure). The wire payload already carries the coded error —
 * surface it in full so a rate-limit / auth / endpoint failure is diagnosable
 * from the toast: semantic title, the provider's raw message as the body, and
 * a diagnostics list (error code, HTTP status, request id, SDK error name,
 * plus any extra detail fields such as finishReason).
 */
function buildAgentErrorNotice(raw: AgentErrorRaw): AppNotice {
  const t = i18n.global.t;
  const details: AppNoticeDetail[] = [];
  const push = (label: string, value: unknown): void => {
    if (typeof value === 'number' || typeof value === 'boolean') {
      details.push({ label, value: String(value) });
    } else if (typeof value === 'string' && value.length > 0) {
      details.push({ label, value });
    }
  };
  push(t('warnings.details.code'), raw.code);
  const rawDetails = raw.details ?? {};
  push(t('warnings.details.status'), rawDetails['statusCode']);
  push(t('warnings.details.requestId'), rawDetails['requestId']);
  push(t('warnings.details.errorName'), raw.name);
  // Keep any remaining detail fields (finishReason, rawFinishReason, …) so no
  // diagnostics the daemon sent are hidden.
  for (const [key, value] of Object.entries(rawDetails)) {
    if (key === 'statusCode' || key === 'requestId') continue;
    push(key, value);
  }
  const titleKey = (raw.code !== undefined ? AGENT_ERROR_TITLE_KEYS[raw.code] : undefined) ?? 'title';
  return {
    severity: 'error',
    title: t(`warnings.agentError.${titleKey}`),
    message: raw.message,
    details: details.length > 0 ? details : undefined,
    ...(raw.channel !== undefined ? { channel: raw.channel } : {}),
  };
}

/**
 * Build the transient floating notice for an MCP server pool transition
 * (rust core `event.mcp.server.status`). Connection failures need the user's
 * attention (error severity, raw reason kept); a fresh connect is a gentle
 * self-dismissing confirmation (info); needs-auth asks for an OAuth login
 * (warning). Pending/disabled transitions carry nothing actionable — no
 * notice, so reconnect churn never spams the toast stack.
 */
function buildMcpStatusNotice(server: AppMcpServerStatus): AppNotice | null {
  if (server.status === 'pending' || server.status === 'disabled') return null;
  const t = i18n.global.t;
  if (server.status === 'failed') {
    const notice: AppNotice = {
      severity: 'error',
      title: t('warnings.mcp.failed', { name: server.name }),
    };
    if (server.error) notice.message = server.error;
    return notice;
  }
  if (server.status === 'needs-auth') {
    return {
      severity: 'warning',
      title: t('warnings.mcp.needsAuth', { name: server.name }),
      message: t('warnings.mcp.needsAuthHint'),
    };
  }
  // connected — light confirmation that fades on its own
  return {
    severity: 'info',
    title: t('warnings.mcp.connected', { name: server.name }),
    message: t('warnings.mcp.tools', { n: server.toolCount }),
  };
}

/**
 * Apply a single AppEvent to the state, returning a new state object.
 * The event carries `_wireSeq` and `_wireSessionId` as hidden extras when
 * produced by the client wrapper, but the reducer only depends on the
 * AppEvent.type discriminant.
 *
 * Extra metadata attached by the caller:
 *   meta.sessionId — wire session_id for lastSeqBySession update
 *   meta.seq       — wire seq for lastSeqBySession update
 */
export interface EventMeta {
  sessionId: string;
  seq: number;
}

/** How much renderable content one question request carries: item count plus
 *  the option count across items. Used only to arbitrate between duplicate
 *  announcements of the same request id — a richer card always wins. */
function questionWeight(request: AppQuestionRequest): number {
  return request.questions.reduce((total, item) => total + 1 + (item.options?.length ?? 0), 0);
}

export function reduceAppEvent(
  state: MirriClientState,
  event: AppEvent,
  meta: EventMeta,
): MirriClientState {
  const next = cloneState(state);

  // Always advance lastSeqBySession for every event that carries seq info.
  advanceSeq(next, meta.sessionId, meta.seq);

  switch (event.type) {
    // -------------------------------------------------------------------------
    case 'sessionCreated': {
      const exists = next.sessions.some((s) => s.id === event.session.id);
      if (!exists) {
        next.sessions = [event.session, ...next.sessions];
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionUpdated': {
      next.sessions = next.sessions.map((s) =>
        s.id === event.session.id ? event.session : s,
      );
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionDeleted': {
      const id = event.sessionId;
      next.sessions = next.sessions.filter((s) => s.id !== id);
      // Reference-reuse contract: clone before delete (an in-place delete
      // would pollute the shared input maps).
      next.messagesBySession = { ...next.messagesBySession };
      delete next.messagesBySession[id];
      next.tasksBySession = { ...next.tasksBySession };
      delete next.tasksBySession[id];
      next.goalBySession = { ...next.goalBySession };
      delete next.goalBySession[id];
      next.approvalsBySession = { ...next.approvalsBySession };
      delete next.approvalsBySession[id];
      next.questionsBySession = { ...next.questionsBySession };
      delete next.questionsBySession[id];
      // advanceSeq may already have cloned lastSeqBySession for this event's
      // own session, but this delete targets another session's entry — clone
      // unconditionally (the in-place delete would pollute the input map).
      next.lastSeqBySession = { ...next.lastSeqBySession };
      delete next.lastSeqBySession[id];
      next.turnActiveBySession = { ...next.turnActiveBySession };
      delete next.turnActiveBySession[id];
      next.turnStartAtBySession = { ...next.turnStartAtBySession };
      delete next.turnStartAtBySession[id];
      next.backgroundedToolCalls = { ...next.backgroundedToolCalls };
      delete next.backgroundedToolCalls[id];
      if (next.activeSessionId === id) {
        next.activeSessionId = undefined;
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionWorkChanged': {
      next.sessions = next.sessions.map((s) => {
        if (s.id !== event.sessionId) return s;
        return {
          ...s,
          busy: event.busy,
          mainTurnActive: event.mainTurnActive ?? (event.busy ? s.mainTurnActive : false),
          pendingInteraction: event.pendingInteraction ?? s.pendingInteraction,
          // Authoritative, not nullish-merge: an omitted last_turn_reason is
          // how the server says "no current outcome" (a fresh turn cleared
          // the previous one), so the stale value must not survive.
          lastTurnReason: event.lastTurnReason,
        };
      });
      if (event.mainTurnActive === true) {
        next.turnActiveBySession = { ...next.turnActiveBySession };
        next.turnActiveBySession[event.sessionId] = true;
      } else if (!event.busy) {
        next.turnActiveBySession = { ...next.turnActiveBySession };
        delete next.turnActiveBySession[event.sessionId];
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionMetaUpdated': {
      // Lightweight meta patch — the daemon's auto-generated title (or a title
      // changed by another client) and the latest user prompt arrive via
      // session.meta.updated. We keep prior values for any field the event does
      // not carry; the full session object otherwise stays as-is. Keeping
      // lastPrompt fresh lets sidebar search match the most recent prompt
      // without a full reload.
      next.sessions = next.sessions.map((s) =>
        s.id === event.sessionId
          ? { ...s, title: event.title ?? s.title, lastPrompt: event.lastPrompt ?? s.lastPrompt }
          : s,
      );
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionUsageUpdated': {
      next.sessions = next.sessions.map((s) => {
        if (s.id !== event.sessionId) return s;
        // The live model name (from agent.status.updated) rides along with usage.
        // Only overwrite model when a non-empty one is supplied.
        const model = event.model && event.model.length > 0 ? event.model : s.model;
        return { ...s, usage: event.usage, model };
      });
      break;
    }

    // -------------------------------------------------------------------------
    case 'historyCompacted': {
      // Only advance lastSeqBySession; actual reload is triggered by client wrapper
      // when it sees this event type (before_seq is in event.beforeSeq).
      // The advanceSeq at top already handled seq update.
      break;
    }

    // -------------------------------------------------------------------------
    case 'compactionStarted': {
      next.compactionBySession = {
        ...next.compactionBySession,
        [event.sessionId]: { status: 'running', trigger: event.trigger },
      };
      break;
    }

    case 'compactionCompleted': {
      const sid = event.sessionId;
      const prev = next.compactionBySession[sid];
      const { [sid]: _doneEntry, ...rest } = next.compactionBySession;
      // Reference-reuse contract: `rest` is a fresh object only when the
      // session had an entry; when it did not, clone explicitly so the
      // shared input map is never mutated by the marker append below.
      next.compactionBySession = sid in next.compactionBySession ? rest : { ...next.compactionBySession };

      // Append a persistent "context compacted" divider to the loaded
      // transcript (TUI parity: the scrollback is kept untouched; only a
      // one-line marker records that compaction happened). The marker id is
      // derived from the wire seq so an event replay after reconnect can't
      // duplicate it.
      if (Object.prototype.hasOwnProperty.call(next.messagesBySession, sid)) {
        const msgs = next.messagesBySession[sid] ?? [];
        const markerId = `compaction_${sid}_${meta.seq}`;
        if (!msgs.some((m) => m.id === markerId)) {
          const marker: CompactionMarkerMetadata = {
            trigger: prev?.trigger ?? 'auto',
            tokensBefore: event.tokensBefore,
            tokensAfter: event.tokensAfter,
          };
          next.messagesBySession = { ...next.messagesBySession };
          next.messagesBySession[sid] = [
            ...msgs,
            {
              id: markerId,
              sessionId: sid,
              role: 'assistant',
              content: event.summary ? [{ type: 'text', text: event.summary }] : [],
              createdAt: new Date().toISOString(),
              metadata: {
                origin: { kind: 'compaction_summary' },
                [COMPACTION_MARKER_METADATA_KEY]: marker,
              },
            },
          ];
        }
      }
      break;
    }

    case 'compactionCancelled': {
      const { [event.sessionId]: _gone, ...rest } = next.compactionBySession;
      // Reference-reuse contract: a session absent from the map makes `rest`
      // alias the input map — clone so nothing is shared with the input.
      next.compactionBySession =
        event.sessionId in next.compactionBySession ? rest : { ...next.compactionBySession };
      break;
    }

    // -------------------------------------------------------------------------
    case 'sessionModelSwitched': {
      // A durable mid-turn model switch: append a local marker message so the
      // divider shows immediately (messagesToTurns folds it into a
      // 'modelSwitch' turn) without waiting for the server's message.list.
      // Same append idiom + id-dedup as the compactionCompleted marker:
      // `model-switch-${event.seq}` derives from the event's durable seq, so
      // an event replay after a reconnect can't duplicate it.
      const sid = event.sessionId;
      if (Object.prototype.hasOwnProperty.call(next.messagesBySession, sid)) {
        const msgs = next.messagesBySession[sid] ?? [];
        const markerId = `model-switch-${event.seq}`;
        if (!msgs.some((m) => m.id === markerId)) {
          const marker: AppMessage = {
            id: markerId,
            sessionId: sid,
            role: 'user',
            content: [{ type: 'text', text: '' }],
            createdAt: new Date().toISOString(),
            metadata: {
              origin: { kind: 'model_switched', from: event.from, to: event.to },
            },
          };
          next.messagesBySession = { ...next.messagesBySession };
          next.messagesBySession[sid] = [...msgs, marker];
        }
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'messageCreated': {
      const sid = event.message.sessionId;
      // SDR-014: content output is not "use" — updatedAt no longer bumps for
      // messages. A USER message IS use: bump the local sort key immediately
      // (the server's send handler bumps the durable one; this is the optimistic
      // local mirror). Cron/notification injections are NOT user sends —
      // excluded by the same origin guards the echo-dedup path uses.
      const createdAt = event.message.createdAt;
      const isUserSend =
        event.message.role === 'user' &&
        !isCronOriginMessage(event.message) &&
        !isNotificationOriginMessage(event.message);
      next.sessions = next.sessions.map((s) =>
        s.id === sid && isUserSend && createdAt > (s.lastUsedAt ?? s.updatedAt)
          ? { ...s, lastUsedAt: createdAt }
          : s,
      );
      const msgs = next.messagesBySession[sid] ?? [];
      const exists = msgs.some((m) => m.id === event.message.id);
      if (exists) {
        // The optimistic echo (prompt.submitted) beat the engine's
        // event.message.created to the list. The engine copy carries the
        // authoritative metadata (origin: skill_activation, attachments
        // provenance) that the optimistic constructor could not know — merge
        // it into the existing message so the SAME-session view matches the
        // post-reload snapshot view (e.g. the skill-activation card).
        next.messagesBySession = { ...next.messagesBySession };
        next.messagesBySession[sid] = msgs.map((m) =>
          m.id === event.message.id
            ? { ...m, metadata: { ...m.metadata, ...event.message.metadata } }
            : m,
        );
        break;
      }
      if (!exists) {
        // Cron-injected user messages (origin cron_job/cron_missed) and
        // background-task notifications (origin background_task) carry the
        // engine's own text, which can coincide with a still-optimistic user
        // message. They must append as their own turn rather than reconcile
        // into (and replace) that optimistic echo — so skip the echo lookup
        // entirely for them.
        if (
          event.message.role === 'user' &&
          !isCronOriginMessage(event.message) &&
          !isNotificationOriginMessage(event.message)
        ) {
          const optimisticIndex = findOptimisticUserEchoIndex(msgs, event.message);
          if (optimisticIndex !== -1) {
            const updated = [...msgs];
            const optimistic = updated[optimisticIndex]!;
            updated[optimisticIndex] = {
              ...event.message,
              id: optimistic.id,
              promptId: event.message.promptId ?? optimistic.promptId,
              // The ENGINE side wins on a shared key: `origin` is
              // admission-side provenance only the engine can know (it may
              // carry a payload the optimistic constructor could not guess,
              // e.g. the turn a steer was admitted into). Spreading the
              // optimistic side last would mask that payload live and
              // resurrect it after a reload — the divergence ADR-010 exists
              // to prevent. Client-only keys (`mirriWeb.optimisticUserMessage`,
              // `mirriWeb.echoedFileReferences`) are never sent by the engine,
              // so they survive either way.
              metadata: {
                ...optimistic.metadata,
                ...event.message.metadata,
              },
            };
            next.messagesBySession = { ...next.messagesBySession };
            next.messagesBySession[sid] = updated;
            break;
          }
        }
        // Dual-echo dedup: when both prompt.submitted (agent path) and
        // event.message.created (protocol path) emit a messageCreated for
        // the same prompt, the first echo already merged into (or replaced)
        // the optimistic message. A second messageCreated with the same
        // promptId must not append again.
        if (event.message.role === 'user' && event.message.promptId !== undefined) {
          const alreadyExists = msgs.some(
            (m) => m.role === 'user' && m.promptId === event.message.promptId,
          );
          if (alreadyExists) {
            break;
          }
        }
        next.messagesBySession = { ...next.messagesBySession };
        next.messagesBySession[sid] = [...msgs, event.message];
      }
      // A protocol-path assistant message can carry the whole-list TodoList
      // write as a toolUse part (v2 cold-transcript fold; the rust
      // projector's tool.call.started emits messageUpdated instead). Fold a
      // write here too so a live write refreshes the snapshot-loaded copy on
      // every path.
      if (event.message.role === 'assistant') {
        const liveTodos = latestTodosFromContent(event.message.content);
        if (liveTodos !== undefined) {
          next.todosBySession = { ...next.todosBySession, [sid]: liveTodos };
        }
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'turnAnchored': {
      // The engine reported the turn's numeric wire id: rebind the turn's
      // rows from the UUID-backed correlation key to the durable
      // `turn-<id>` key — the identity fork-at-turn anchors on and a
      // snapshot reload would produce.
      const sid = event.sessionId;
      const msgs = next.messagesBySession[sid] ?? [];
      next.messagesBySession = { ...next.messagesBySession };
      next.messagesBySession[sid] = msgs.map((m) =>
        m.promptId === event.promptId ? { ...m, promptId: event.anchorId } : m,
      );
      break;
    }

    // -------------------------------------------------------------------------
    case 'messageUpdated': {
      const sid = event.sessionId;
      const msgs = next.messagesBySession[sid] ?? [];
      next.messagesBySession = { ...next.messagesBySession };
      next.messagesBySession[sid] = msgs.map((m) => {
        if (m.id !== event.messageId) return m;
        return {
          ...m,
          content: event.content,
          durationMs: event.durationMs ?? m.durationMs,
        };
      });
      // A live TodoList write lands as a toolUse part of an updated assistant
      // message. Fold it into the todos state so the snapshot-loaded copy is
      // refreshed the moment a newer write arrives (a live write always wins
      // over the snapshot's copy — it is the newest authoritative state).
      const liveTodos = latestTodosFromContent(event.content);
      if (liveTodos !== undefined) {
        next.todosBySession = { ...next.todosBySession, [sid]: liveTodos };
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'assistantDelta': {
      const sid = event.sessionId;
      const msgs = next.messagesBySession[sid] ?? [];
      next.messagesBySession = { ...next.messagesBySession };
      next.messagesBySession[sid] = msgs.map((m) => {
        if (m.id !== event.messageId) return m;
        const content = [...m.content];
        const idx = event.contentIndex;
        // Ensure the slot exists
        while (content.length <= idx) {
          content.push({ type: 'text', text: '' });
        }
        const existing = content[idx]!;
        let patched: AppMessageContent;
        if (event.delta.text !== undefined) {
          if (existing.type === 'text') {
            patched = { type: 'text', text: existing.text + event.delta.text };
          } else {
            patched = { type: 'text', text: event.delta.text };
          }
        } else if (event.delta.thinking !== undefined) {
          if (existing.type === 'thinking') {
            patched = {
              type: 'thinking',
              thinking: existing.thinking + event.delta.thinking,
              signature: existing.signature,
            };
          } else {
            patched = { type: 'thinking', thinking: event.delta.thinking };
          }
        } else {
          patched = existing;
        }
        content[idx] = patched;
        return { ...m, content };
      });
      break;
    }

    // -------------------------------------------------------------------------
    case 'toolOutput': {
      const sid = event.sessionId;
      const msgs = next.messagesBySession[sid] ?? [];
      next.messagesBySession = { ...next.messagesBySession };
      next.messagesBySession[sid] = appendToolOutputToMessages(msgs, event.toolCallId, event.outputChunk);
      break;
    }

    // -------------------------------------------------------------------------
    case 'approvalRequested': {
      const sid = event.sessionId;
      const list = next.approvalsBySession[sid] ?? [];
      // The `event.approval.requested` bus projection and the `approval.ask`
      // reverse-RPC race for the same request id, and either can arrive
      // first. Rows merge per-field: whatever the row already carries wins,
      // whatever it lacks is filled in — display, action, facet, expiry,
      // member identity (see the per-field branch below). Supervision B1:
      // only the ask carries the requesting member's identity, so identity
      // arriving second enriches the existing unattributed row instead of
      // being dropped.
      const idx = list.findIndex((a) => a.approvalId === event.approval.approvalId);
      if (idx === -1) {
        next.approvalsBySession = { ...next.approvalsBySession };
        next.approvalsBySession[sid] = [...list, event.approval];
      } else {
        // Per-field enrichment: the thin `event.approval.requested`
        // projection and the rich `approval.ask` reverse-RPC race for the
        // same request id, and either can arrive first. Whatever the row
        // already carries wins; whatever it lacks is filled in — so a
        // display-less event-first row still gets the structured display,
        // action, facet, expiry, and member identity from the ask (and
        // vice versa). See 01-protocol-unification.md §7.6.
        const existing = list[idx]!;
        const incoming = event.approval;
        const merged = {
          ...existing,
          ...(existing.display === undefined && incoming.display !== undefined
            ? { display: incoming.display }
            : {}),
          ...(existing.action === '' && incoming.action !== '' ? { action: incoming.action } : {}),
          ...(existing.approvalRule === undefined && incoming.approvalRule !== undefined
            ? { approvalRule: incoming.approvalRule }
            : {}),
          ...(existing.expiresAt === undefined && incoming.expiresAt !== undefined
            ? { expiresAt: incoming.expiresAt }
            : {}),
          ...(existing.agentId === undefined && incoming.agentId !== undefined
            ? { agentId: incoming.agentId }
            : {}),
        };
        if (merged !== existing) {
          const mergedList = [...list];
          mergedList[idx] = merged;
          next.approvalsBySession = { ...next.approvalsBySession };
          next.approvalsBySession[sid] = mergedList;
        }
      }
      // Preserve a plan_review display so the plan stays visible in the
      // ExitPlanMode tool card after the approval resolves.
      const display = event.approval.display as
        | { kind?: unknown; plan?: unknown; path?: unknown }
        | null
        | undefined;
      if (display?.kind === 'plan_review' && typeof display.plan === 'string' && display.plan.length > 0) {
        next.planReviewByToolCallId = {
          ...next.planReviewByToolCallId,
          [event.approval.toolCallId]: {
            plan: display.plan,
            path: typeof display.path === 'string' ? display.path : undefined,
          },
        };
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'approvalResolved':
    case 'approvalExpired': {
      const sid = event.sessionId;
      const aid = event.approvalId;
      const list = next.approvalsBySession[sid] ?? [];
      next.approvalsBySession = { ...next.approvalsBySession };
      next.approvalsBySession[sid] = list.filter((a) => a.approvalId !== aid);
      break;
    }

    // -------------------------------------------------------------------------
    case 'questionRequested': {
      const sid = event.sessionId;
      const list = next.questionsBySession[sid] ?? [];
      const index = list.findIndex((q) => q.questionId === event.question.questionId);
      // Two sources can announce the same request: the reverse-RPC ask (the
      // tool payload) and the durable event. They share the gate's request
      // id, so first-write-wins would let a question-text-only card shadow
      // the real one and silently drop every option. Keep whichever carries
      // more (items, then options per item); an identical duplicate is a
      // no-op.
      if (index === -1) {
        next.questionsBySession = { ...next.questionsBySession };
        next.questionsBySession[sid] = [...list, event.question];
      } else if (questionWeight(event.question) > questionWeight(list[index]!)) {
        const nextList = [...list];
        nextList[index] = event.question;
        next.questionsBySession = { ...next.questionsBySession };
        next.questionsBySession[sid] = nextList;
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'questionAnswered':
    case 'questionDismissed': {
      const sid = event.sessionId;
      const qid = event.questionId;
      const list = next.questionsBySession[sid] ?? [];
      next.questionsBySession = { ...next.questionsBySession };
      next.questionsBySession[sid] = list.filter((q) => q.questionId !== qid);
      break;
    }

    // -------------------------------------------------------------------------
    case 'taskCreated': {
      const sid = event.sessionId;
      const list = next.tasksBySession[sid] ?? [];
      const idx = list.findIndex((t) => t.id === event.task.id);
      if (idx === -1) {
        next.tasksBySession = { ...next.tasksBySession };
        next.tasksBySession[sid] = [...list, event.task];
      } else {
        const patched = [...list];
        const previous = list[idx]!;
        // The projected task does not carry reducer-owned accumulated progress;
        // preserve it across the replacement so subagent output keeps growing.
        // A resync also rebuilds skeleton tasks without their identity metadata,
        // so keep the previous value when the projected task omits it.
        patched[idx] = {
          ...event.task,
          outputLines: previous.outputLines,
          text: previous.text,
          // A post-refresh lifecycle event re-projects the task with skeleton
          // metadata; don't let its placeholder clobber the roster-seeded
          // description.
          description:
            event.task.description === PLACEHOLDER_SUBAGENT_DESCRIPTION &&
            previous.description !== PLACEHOLDER_SUBAGENT_DESCRIPTION
              ? previous.description
              : event.task.description,
          swarmIndex: event.task.swarmIndex ?? previous.swarmIndex,
          parentToolCallId: event.task.parentToolCallId ?? previous.parentToolCallId,
          subagentType: event.task.subagentType ?? previous.subagentType,
          runInBackground: event.task.runInBackground ?? previous.runInBackground,
          backgroundTaskId: event.task.backgroundTaskId ?? previous.backgroundTaskId,
          // The projected task may omit agentId (skeleton re-projection); the
          // row's cross-pipeline identity must survive the replacement or the
          // next REST poll fold misses the join and a second row appears.
          agentId: event.task.agentId ?? previous.agentId,
          // ADR-009 live steps ride the projected row; a skeleton
          // re-projection without them must not blank the accumulated steps.
          steps: event.task.steps ?? previous.steps,
          // ADR-015 / RC-4: the projector's per-delta taskCreated emissions must
          // not wipe the reducer-owned narrative and bookkeeping fields.
          thinking: previous.thinking,
          stepTextLen: previous.stepTextLen,
          stepThinkLen: previous.stepThinkLen,
          tailGuard: previous.tailGuard,
          suspendedReason: event.task.suspendedReason ?? previous.suspendedReason,
        };
        next.tasksBySession = { ...next.tasksBySession };
        next.tasksBySession[sid] = patched;
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'taskProgress': {
      const sid = event.sessionId;
      const tasks = next.tasksBySession[sid] ?? [];
      const idx = tasks.findIndex((t) => t.id === event.taskId);
      if (idx === -1) break; // unknown id: no map write at all (SDR-003b)
      // SDR-003b: replace ONLY the target task — siblings keep their
      // references so per-task consumers (swarm cards, dock rows) don't
      // re-render for another task's chunk.
      const updated = appendTaskProgress(tasks[idx]!, event);
      if (updated === tasks[idx]) break; // no-op chunk (dedupe hit): no write
      next.tasksBySession = {
        ...next.tasksBySession,
        [sid]: [...tasks.slice(0, idx), updated, ...tasks.slice(idx + 1)],
      };
      break;
    }

    // -------------------------------------------------------------------------
    case 'subagentStepBoundary': {
      const sid = event.sessionId;
      const tasks = next.tasksBySession[sid] ?? [];
      const idx = tasks.findIndex((t) => t.id === event.taskId);
      if (idx === -1) break; // unknown id: no write (matches taskProgress)
      const t = tasks[idx]!;
      // Terminal stickiness (ADR-010 ②): a late retrying frame racing the
      // lifecycle event must not truncate a settled member's narrative.
      if (isTerminalStatus(t.status)) break;
      // N7: a non-retry boundary that would only re-commit identical marks onto
      // an already-armed guard is a reference no-op — skip the map write so
      // sibling consumers keep their references (same contract as taskProgress).
      if (
        !event.retry &&
        t.tailGuard === true &&
        t.stepTextLen === (t.text ?? '').length &&
        t.stepThinkLen === (t.thinking ?? '').length
      ) {
        break;
      }
      const patched = event.retry
        ? {
            ...t,
            // ADR-015 / L4: with BOTH marks undefined the member's
            // turn.started was never seen (mid-subscription reconnect) —
            // truncating via `?? 0` would wipe text/thinking accumulated
            // across the whole turn while the engine only replays the
            // current step. Arm the one-shot tail guard instead and keep the
            // accumulated narrative (宁可多显不可丢失); truncate only when at
            // least one step-start mark is defined.
            ...(t.stepTextLen === undefined && t.stepThinkLen === undefined
              ? {}
              : {
                  text: (t.text ?? '').slice(0, t.stepTextLen ?? 0),
                  thinking: (t.thinking ?? '').slice(0, t.stepThinkLen ?? 0),
                }),
            tailGuard: true,
          }
        : {
            ...t,
            stepTextLen: (t.text ?? '').length,
            stepThinkLen: (t.thinking ?? '').length,
            tailGuard: true,
          };
      next.tasksBySession = {
        ...next.tasksBySession,
        [sid]: [...tasks.slice(0, idx), patched, ...tasks.slice(idx + 1)],
      };
      break;
    }

    // -------------------------------------------------------------------------
    case 'taskCompleted': {
      const sid = event.sessionId;
      const list = next.tasksBySession[sid] ?? [];
      next.tasksBySession = { ...next.tasksBySession };
      next.tasksBySession[sid] = list.map((t) => {
        // Primary match: the terminal event's task id equals the row id (WS
        // subagent rows are keyed by agent id; bash rows by task id).
        if (t.id === event.taskId) {
          // ADR-010 ② 终态粘滞：迟到的 taskCompleted 不得复活已终态行。
          if (isTerminalStatus(t.status)) return t;
          // ADR-010 ① 终态-阶段原子更新：status 与 subagentPhase 一起翻转。
          return {
            ...applyTerminalToTask(t, event.status),
            outputPreview: event.outputPreview,
            outputBytes: event.outputBytes,
          };
        }
        // Fallback match 1: a background task terminated event carries the
        // background task id, while the WS-owned subagent row is keyed by agent
        // id — link through backgroundTaskId so the termination event can
        // finalize a row whose subagent.completed event was missed.
        if (t.backgroundTaskId === event.taskId && t.status === 'running') {
          return {
            ...applyTerminalToTask(t, event.status),
            outputPreview: event.outputPreview ?? t.outputPreview,
            outputBytes: event.outputBytes ?? t.outputBytes,
          };
        }
        // Fallback match 2: a WS subagent.completed/failed carries the AGENT id
        // while a REST-synced background row is keyed by its own task id — link
        // them through agentId so the dock row flips to completed too. This is
        // the same subagent surfacing twice (WS row + REST row).
        if (t.agentId === event.taskId && t.status === 'running') {
          return {
            ...applyTerminalToTask(t, event.status),
            outputPreview: event.outputPreview ?? t.outputPreview,
            outputBytes: event.outputBytes ?? t.outputBytes,
          };
        }
        return t;
      });
      break;
    }

    // -------------------------------------------------------------------------
    case 'toolCallBackgrounded': {
      // B7 W6 — web-internal (never from the wire): App.vue dispatches this
      // after the task.detach ack for a model Bash tool call. The stamp is
      // monotonic: a later detach answer for the same call must not clear
      // the earlier one.
      const sid = event.sessionId;
      // Reference-reuse contract: cloneState no longer deep-copies the outer
      // map — clone it here (once) before the nested write.
      next.backgroundedToolCalls = { ...next.backgroundedToolCalls };
      const map = { ...next.backgroundedToolCalls[sid] };
      map[event.toolCallId] = event.taskId;
      next.backgroundedToolCalls[sid] = map;
      break;
    }

    // -------------------------------------------------------------------------
    case 'goalUpdated': {
      const sid = event.sessionId;
      // Bump on every goal event — including clears — so refreshSessionGoal's
      // recovery read can detect any live event that landed mid-flight.
      next.goalVersionBySession = { ...next.goalVersionBySession };
      next.goalVersionBySession[sid] = (next.goalVersionBySession[sid] ?? 0) + 1;
      next.goalBySession = { ...next.goalBySession };
      if (event.goal === null || event.goal.status === 'complete') {
        delete next.goalBySession[sid];
      } else {
        next.goalBySession[sid] = event.goal;
      }
      break;
    }

    // -------------------------------------------------------------------------
    case 'configChanged': {
      next.config = event.config;
      break;
    }

    case 'agentProfilesChanged': {
      // Signal that profiles need reloading — the UI layer can refetch
      // via listAgents() when it receives this event.
      break;
    }

    // -------------------------------------------------------------------------
    // MCP pool transition (rust core): a transient floating notice only —
    // the settings' MCP panel remains the durable server-status surface.
    case 'mcpServerStatus': {
      const notice = buildMcpStatusNotice(event.server);
      if (notice) {
        next.warnings = [...next.warnings, notice];
      }
      break;
    }

    // -------------------------------------------------------------------------
    // Provider-model catalog refresh result. The daemon already persisted the
    // new catalog; the web picks it up on the next explicit model/provider load
    // (model picker, session switch). Advance seq silently.
    case 'modelCatalogChanged':
      break;

    // -------------------------------------------------------------------------
    // Agent-scoped side-channel events (e.g. BTW side chat) are consumed by the
    // web layer, not the session reducer. Advance seq silently.
    case 'agentDelta':
    case 'agentTurnEnded':
      break;

    // -------------------------------------------------------------------------
    // Prompt-level lifecycle events drive the web layer's in-flight cleanup
    // (see useMirriWebClient.processEvent), not reducer state. Advance seq
    // silently.
    case 'promptCompleted':
    case 'promptAborted':
      break;

    // -------------------------------------------------------------------------
    case 'turnActiveChanged': {
      next.sessions = next.sessions.map((session) =>
        session.id === event.sessionId
          ? { ...session, mainTurnActive: event.active }
          : session,
      );
      if (event.active) {
        next.turnActiveBySession = { ...next.turnActiveBySession };
        next.turnActiveBySession[event.sessionId] = true;
        // A new turn starting retires any stale turn-failure notice — the
        // dock's bottom failure bar belongs to the PREVIOUS turn's failure
        // only.
        next.warnings = next.warnings.filter(
          (w) => !(typeof w === 'object' && w !== null && w.channel === 'turn-failure'),
        );
        // The live elapsed anchor: the projector's turn.started wall-clock.
        // An activation without a stamp (older engines) must NOT keep a stale
        // anchor from an earlier turn — the header falls back to the bare
        // "Working…" label instead of ticking a wrong elapsed.
        next.turnStartAtBySession = { ...next.turnStartAtBySession };
        if (event.startedAtMs !== undefined) {
          next.turnStartAtBySession[event.sessionId] = event.startedAtMs;
        } else {
          delete next.turnStartAtBySession[event.sessionId];
        }
      } else {
        next.turnActiveBySession = { ...next.turnActiveBySession };
        delete next.turnActiveBySession[event.sessionId];
        next.turnStartAtBySession = { ...next.turnStartAtBySession };
        delete next.turnStartAtBySession[event.sessionId];
      }
      break;
    }

    case 'unknown': {
      // Distinguish no-op known events (sentinel _noop) from agent errors/warnings
      // and truly unknown events.
      const raw = event.raw as {
        _noop?: boolean;
        _agentError?: boolean;
        _agentWarning?: boolean;
        code?: string;
        message?: string;
        name?: string;
        details?: Record<string, unknown>;
        type?: string;
      } | null;
      if (raw && raw._noop === true) {
        // No-op streaming/tool event — seq already advanced, nothing else to do
      } else if (raw && raw._agentError) {
        // Surface the agent's real error (e.g. a 429 from the model provider)
        // as a structured notice: semantic title + raw provider message +
        // diagnostics (code / HTTP status / request id) for troubleshooting.
        next.warnings = [...next.warnings, buildAgentErrorNotice(raw)];
      } else if (raw && raw._agentWarning) {
        const msg = raw.message ?? raw.code ?? 'agent warning';
        next.warnings = [...next.warnings, `${i18n.global.t('warnings.noteLabel')}: ${msg}`];
      } else {
        // Truly unknown — push a warning
        const wireType = raw?.type ?? '(unknown)';
        next.warnings = [...next.warnings, `Unhandled event: ${wireType}`];
      }
      break;
    }

    // Workspace lifecycle events are handled in the composable (rawState), not
    // here — listed explicitly to keep the switch exhaustive.
    case 'workspaceCreated':
    case 'workspaceUpdated':
    case 'workspaceDeleted':
      break;

    default: {
      // TypeScript exhaustiveness guard — should not reach here
      const _exhaustive: never = event;
      void _exhaustive;
      break;
    }
  }

  return next;
}
