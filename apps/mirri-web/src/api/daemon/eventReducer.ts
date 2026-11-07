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
  AppQuestionRequest,
  AppSession,
  AppTask,
  AppWarning,
  CompactionMarkerMetadata,
  NarrativeSegment,
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
 *  (outputLines / segment text concatenation) are unchanged — only the write
 *  granularity moved to the caller. Returns the SAME task reference when the
 *  chunk is a no-op (dedupe hit), so the caller can skip the map write. */
function appendTaskProgress(
  task: AppTask,
  event: Extract<AppEvent, { type: 'taskProgress' }>,
): AppTask {
  // Subagent streamed output (assistant.delta / thinking.delta, ADR-011 ②):
  // the projector names the segment the chunk belongs to, and the chunk folds
  // onto THAT segment — the row's narrative lives in the segment list.
  if (task.kind === 'subagent' && (event.kind === 'text' || event.kind === 'thinking')) {
    return appendSegmentProgress(task, event);
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

/** Fold a segment-named `taskProgress` chunk onto its target segment (ADR-011
 *  ②): the row's segments are reducer-owned between taskCreated
 *  replacements, so the chunk only ever extends the segment the projector
 *  named. An unknown segment id is a silent no-op — the row keeps its
 *  reference, so the caller skips the map write entirely. */
function appendSegmentProgress(
  task: AppTask,
  event: Extract<AppEvent, { type: 'taskProgress' }>,
): AppTask {
  const segments = task.segments ?? [];
  const idx = segments.findIndex((s) => s.id === event.segmentId);
  if (idx === -1) return task; // 未知段 id：no-op（连不上投影层的段，静默丢弃）
  const seg = segments[idx]!;
  // 段级一次性尾守卫（ADR-015 d 段化 + ADR-011 ②重复计账不变量）：守卫只由
  // 「刚携带全量段文本的 taskCreated」武装（投影层铸造，或 mergeSegments 在替换
  // 路径重武装）；按发射序，其后首个 chunk 必然是该 taskCreated 已折叠的同一个
  // delta，全量文本必以其结尾——故后缀匹配不设最短长度门槛（旧字段守卫的
  // MIN_TAIL_DEDUPE_LEN 是内容猜测式去重的保险，随旧路径退役删除；这里的覆盖
  // 是结构性的，短 token 同样命中）。命中即吞掉；未命中说明该段不在投影层全量
  // 覆盖范围内，增量照常追加。
  if (seg.tailGuard === true) {
    if (seg.text.length > 0 && seg.text.endsWith(event.outputChunk)) {
      return replaceSeg(task, segments, idx, { ...seg, tailGuard: undefined });
    }
    return replaceSeg(task, segments, idx, {
      ...seg,
      text: seg.text + event.outputChunk,
      tailGuard: undefined,
    });
  }
  return replaceSeg(task, segments, idx, { ...seg, text: seg.text + event.outputChunk });
}

function replaceSeg(
  task: AppTask,
  segments: NarrativeSegment[],
  idx: number,
  seg: NarrativeSegment,
): AppTask {
  const next = [...segments.slice(0, idx), seg, ...segments.slice(idx + 1)];
  return { ...task, segments: next };
}

// taskCreated 是全应用最高频事件——无 tombstone 时复用同一个空集，避免每次
// 调用构造新 Set。
const EMPTY_TOMBSTONES: ReadonlySet<string> = new Set<string>();

/** Merge the row's surviving segments with a replacement taskCreated's
 *  projected segment list (ADR-011 ①重连重放).
 *  `next === undefined`（骨架行不带段）→ 存活段原样保留；`prev === undefined`
 *  （行上从未有过段）→ 投影层列表原样落入。两者都在时以重放列表为骨架（它携带
 *  投影层的最新 seq 序）：同 id 段取更长文本并重武装一次性守卫——重放窗口后到达
 *  的首个增量 chunk 与段内已有尾部精确尾匹配时被 `appendSegmentProgress` 吞掉，
 *  实现「全量为准、增量不重复计账」；投影层新段（id 不在旧表）原样并入，存活
 *  独有段（早于重放起点的段）保序排前。
 *
 *  `tombstones`（重试截断丢弃的段 id）：投影层不截断自己的段表，重放会把这些
 *  失败尝试的残段当「新铸段」重新递回来——无论是 next 列表还是存活独有段前置
 *  路径，命中即过滤。tombstone 是 run-scoped 的：重试截断时记入后随行存活到
 *  成员终态（taskCreated 替换臂、快照名册合并都会保活），不在边界清空；重试后
 *  的新段 id 由投影层的 generation 域保证不与旧 tombstone 碰撞，故重连重放的
 *  同 id 重投不会被误伤。 */
function mergeSegments(
  prev: NarrativeSegment[] | undefined,
  next: NarrativeSegment[] | undefined,
  tombstones: ReadonlySet<string> = EMPTY_TOMBSTONES,
): NarrativeSegment[] | undefined {
  if (next === undefined) return prev;
  if (prev === undefined) {
    if (tombstones.size === 0) return next;
    const adopted = next.filter((s) => !tombstones.has(s.id));
    return adopted.length === next.length ? next : adopted;
  }
  const prevById = new Map(prev.map((s) => [s.id, s] as const));
  const merged = next
    .filter((s) => !tombstones.has(s.id))
    .map((s) => {
      const old = prevById.get(s.id);
      if (!old) return s; // 投影层新铸段
      return s.text.length >= old.text.length
        ? { ...s, tailGuard: true } // 重放更全：保留重放文本、重武装守卫
        : { ...old, tailGuard: true }; // 存活更长：保留存活文本、重武装守卫
    });
  const nextIds = new Set(next.map((s) => s.id));
  const kept = prev.filter((s) => !nextIds.has(s.id) && !tombstones.has(s.id)); // 存活独有段（早于重放起点的段）排在前面
  return kept.length > 0 ? [...kept, ...merged] : merged;
}

/** Arm the one-shot tail guard on the open (last) segment — the
 *  `subagentStepBoundary` reducer branch's segment-side re-arm (ADR-011 ① /
 *  ADR-015 a 段化): a replayed boundary frame must re-arm the surviving
 *  segment's guard so the replay's first chunk is deduped by the segment
 *  tail match. Empty list is a reference no-op. */
function armLastSegment(segments: NarrativeSegment[]): NarrativeSegment[] {
  if (segments.length === 0) return segments;
  const idx = segments.length - 1;
  const last = segments[idx]!;
  if (last.tailGuard === true) return segments;
  return [...segments.slice(0, idx), { ...last, tailGuard: true }];
}

/** Truncate the segment list back to the step-start mark (ADR-011 ① /
 *  ADR-015 a 段化 retry branch): drop every segment minted after the mark,
 *  cut the retained open (last) segment's text to its step-start length and
 *  arm its guard. A mark of zero keeps an empty list; a mark counting MORE
 *  segments than the live list (already truncated / stale mark) keeps the
 *  list as-is and only re-arms the open segment — its text was already cut
 *  to the mark, cutting again would shave legitimate replayed content. */
function truncateSegmentsToMark(
  segments: NarrativeSegment[],
  mark: { count: number; openLen: number },
): NarrativeSegment[] {
  if (mark.count > segments.length) return armLastSegment(segments);
  const kept = segments.slice(0, mark.count);
  const last = kept.at(-1);
  if (last === undefined) return kept; // count 0: everything minted in this step is dropped
  const idx = kept.length - 1;
  return [
    ...kept.slice(0, idx),
    { ...last, text: last.text.slice(0, mark.openLen), tailGuard: true },
  ];
}

/** Merge the retry truncation's dropped-segment ids into the row's tombstone
 *  list (ADR-011 ① fix-2): the projector never truncates its own list, so its
 *  later taskCreated replays re-adopt the failed attempt's segments —
 *  `mergeSegments` filters by these tombstones for the member's whole run
 *  (they are never cleared; the projector's generation-scoped segment ids
 *  guarantee a post-reset re-mint never collides with an old tombstone). A
 *  replayed retrying frame that drops nothing (the list is already
 *  truncated) keeps the existing tombstones; consecutive retries within one
 *  step accumulate — ids are deduped. */
function nextTombstones(
  existing: string[] | undefined,
  segments: NarrativeSegment[],
  mark: { count: number; openLen: number } | undefined,
): string[] | undefined {
  if (mark === undefined) return existing; // 无记账（跨越 reset 的重放）：不截断不动
  const dropped = segments.slice(mark.count).map((s) => s.id);
  if (dropped.length === 0) return existing; // 重放帧无新丢弃：保留既有
  return Array.from(new Set([...(existing ?? []), ...dropped]));
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
          ? {
              ...s,
              title: event.title ?? s.title,
              lastPrompt: event.lastPrompt ?? s.lastPrompt,
              // SDR-016/017: the pin trio patch (the pinned-first re-sort
              // follows from the reactive sort comparator).
              ...(event.pinned === undefined
                ? {}
                : {
                    pinned: event.pinned,
                    pinnedAt: event.pinned ? (event.pinnedAt ?? s.pinnedAt) : undefined,
                    pinColor: event.pinned ? event.pinColor : undefined,
                  }),
            }
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
        // Assistant tool-call echo dedup: a tool call id is unique per call,
        // so an assistant message whose tool_use ids were ALL already
        // appended is a re-delivery of an earlier message (streamed-vs-
        // persisted copy, crash-recovery replay) — appending it again would
        // render every tool card twice. Fold its volatile extras (live
        // progress / start anchor) into the surviving copy's tool parts and
        // drop the message. Text-only duplicates are deliberately NOT
        // matched (a model may legitimately repeat itself).
        if (event.message.role === 'assistant') {
          const incomingToolUses = event.message.content.filter((c) => c.type === 'toolUse');
          if (incomingToolUses.length > 0) {
            const seenIds = new Set<string>();
            for (const m of msgs) {
              for (const c of m.content) {
                if (c.type === 'toolUse') seenIds.add(c.toolCallId);
              }
            }
            if (incomingToolUses.every((c) => seenIds.has(c.toolCallId))) {
              next.messagesBySession = { ...next.messagesBySession };
              next.messagesBySession[sid] = msgs.map((m) => ({
                ...m,
                content: m.content.map((c) => {
                  if (c.type !== 'toolUse') return c;
                  const echo = incomingToolUses.find((u) => u.toolCallId === c.toolCallId);
                  if (echo === undefined) return c;
                  // Gap-fill only; never overwrite what the survivor carries.
                  const fillOutput = c.outputLines === undefined && echo.outputLines?.length;
                  const fillStartedAt = c.startedAtMs === undefined && echo.startedAtMs !== undefined;
                  if (!fillOutput && !fillStartedAt) return c;
                  return {
                    ...c,
                    outputLines: fillOutput ? echo.outputLines : c.outputLines,
                    startedAtMs: fillStartedAt ? echo.startedAtMs : c.startedAtMs,
                  };
                }),
              }));
              break;
            }
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
          endedAt: event.endedAt ?? m.endedAt,
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
          // ADR-011 ①重连重放：投影态已清时 reducer 段存活——同 id 段保留更长文本
          // 并重武装守卫（重放首 chunk 由段级尾匹配吞掉）；投影层新段（id 不在旧表）
          // 原样并入。不用 `??` 直通：重连重放窗口投影层从零重铸段，首个 delta 后的
          // taskCreated 携带「仅含新段的列表」（非 undefined），`??` 不触发会把存活
          // 段清空。
          segments: mergeSegments(
            previous.segments,
            event.task.segments,
            new Set(previous.truncatedSegmentIds ?? []),
          ),
          // ADR-011 ①段化：步起点段记账也是 reducer-owned——投影层的逐 delta
          // taskCreated / 骨架重铸都不携带，替换时保活（否则重试截断失锚）。
          stepSegMark: previous.stepSegMark,
          // ADR-011 ① / fix-1: 重试截断的 tombstone 同款保活——投影层不截断
          // 自己的段表，保活跨 taskCreated 才能在 mergeSegments 里持续拦截。
          truncatedSegmentIds: previous.truncatedSegmentIds,
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
      // ADR-011 ①段化：段轨道的步起点记账与末段守卫状态（N7 no-op 判定）。
      const segs = t.segments ?? [];
      const segMarkEqual =
        t.stepSegMark !== undefined &&
        t.stepSegMark.count === segs.length &&
        t.stepSegMark.openLen === (segs.at(-1)?.text.length ?? 0);
      const lastSegArmed = segs.length === 0 || segs.at(-1)?.tailGuard === true;
      // N7: a non-retry boundary that would only re-commit identical marks onto
      // an already-armed guard is a reference no-op — skip the map write so
      // sibling consumers keep their references (same contract as taskProgress).
      if (!event.retry && segMarkEqual && lastSegArmed) {
        break;
      }
      const patched = event.retry
        ? {
            ...t,
            // ADR-011 ① / ADR-015 a 段化：段轨道的重试截断——段表截回步起点
            // 记账（count），保留的开放段文本截到 openLen 并武装守卫（引擎从
            // 步起点重放，段级尾匹配吞掉重放首 chunk）。记账未定义（跨越
            // reset 的重放）时不截断，仅武装末段（宁可多显不可丢失）。段表为空
            // 的行（旧投影层形状）无段可截。
            // 被丢弃段的 id 记入 tombstone：投影层不截断自己的段表，其后
            // taskCreated 携带的全量列表会把这些残段当新铸段递回来，
            // mergeSegments 按 tombstone 过滤。
            ...(segs.length === 0
              ? {}
              : {
                  segments:
                    t.stepSegMark === undefined
                      ? armLastSegment(segs)
                      : truncateSegmentsToMark(segs, t.stepSegMark),
                  truncatedSegmentIds: nextTombstones(t.truncatedSegmentIds, segs, t.stepSegMark),
                }),
          }
        : {
            ...t,
            // ADR-011 ①段化：非重试边界记段起点账（段表长度 + 开放段文本
            // 长度），无截断。投影层在关段时武装新段守卫，reducer 在这里对
            // 末段重武装——重连重放的 boundary 帧到达时为存活段重新武装守卫
            // （重放首 chunk 由段级尾匹配吞掉）。
            stepSegMark: {
              count: segs.length,
              openLen: segs.at(-1)?.text.length ?? 0,
            },
            ...(segs.length > 0 ? { segments: armLastSegment(segs) } : {}),
            // fix-2: tombstone 存活整个成员 run，非重试边界不清空——投影层
            // generation 域段 id 保证 reset 后重铸不碰撞，无需边界清理。
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

    case 'skillCatalogChanged':
      // Handled before the reducer (the composable's onEvent intercept
      // refreshes the skill caches + pushes the toast); listed here to
      // keep the switch exhaustive.
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
