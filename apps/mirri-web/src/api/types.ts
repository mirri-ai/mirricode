// apps/mirri-web/src/api/types.ts
// App-facing camelCase model + MirriWebApi interface.
// No daemon wire details here — Vue components consume only these types.

import type { UsageBuckets } from '../lib/usageStats';

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

export interface Page<T> {
  items: T[];
  hasMore: boolean;
}

export interface PageRequest {
  beforeId?: string;
  afterId?: string;
  pageSize?: number;
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

export type AppNoticeSeverity = 'info' | 'warning' | 'error';

export interface AppNoticeDetail {
  label: string;
  value: string;
}

export interface AppNotice {
  severity: AppNoticeSeverity;
  title: string;
  message?: string;
  details?: AppNoticeDetail[];
  /** Routing tag: notices produced by a failed turn render in the chat
   *  dock's bottom bar instead of the corner toast stack. */
  channel?: 'turn-failure';
}

export type AppWarning = string | AppNotice;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

export interface AppSessionUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  totalCostUsd: number;
  contextTokens: number;
  contextLimit: number;
  turnCount: number;
  /** The session's first LLM call — the cold-start baseline (SDR-004).
   *  Undefined when the engine does not report it (before any call, or
   *  history predating the field). */
  firstCall?: UsageBuckets;
}

/** Token usage attributed to a single conversation turn (SDR-002 per-turn
 *  attribution). Canonical shape lives with the client-side agent-event
 *  projector (api/daemon/agentEventProjector.ts re-exports this); declared
 *  here so `MirriEventConnection` can expose a per-prompt usage query
 *  without the app-facing module importing the daemon layer. */
export interface TurnUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  /** Model aliases the turn's completed steps were recorded against (deduped). */
  models: string[];
}

/** One `session.search` hit — the rust engine's global literal search
 *  (snake_case wire mapped to the app's camelCase vocabulary). */
export interface AppSessionSearchHit {
  sessionId: string;
  workspaceId: string;
  sessionTitle: string;
  /** `main`, a subagent id, or empty for title-role hits. */
  agentId: string;
  role: 'user' | 'assistant' | 'title';
  /** ~80-char window around the first match (the title itself for
   *  title-role hits). */
  snippet: string;
  /** Epoch ms — the session's updatedAt (context memory carries no
   *  per-message timestamps). */
  time: number;
  /** 0-based turn ordinal; absent for title hits. */
  turn?: number;
}

/** One `session.search` page. `mode` echoes the effective mode (the
 *  `terms` request degrades to `literal` server-side). */
export interface AppSessionSearchPage {
  hits: AppSessionSearchHit[];
  mode: 'literal' | 'terms';
  nextPageToken?: string;
}

export interface AppSession {
  id: string;
  title: string;
  createdAt: string;
  updatedAt: string;
  /** The session's recency-sort key (SDR-014 / event-delivery ADR-003):
   *  bumped ONLY by user interactions — a user send or an explicit
   *  `session.touch` — never by turn activity. Absent on sessions that
   *  predate the field (and on legacy engines); readers fall back to
   *  `updatedAt` for the sort order. */
  lastUsedAt?: string;
  /** Any agent in the session holds an active turn or background lease.
   *  Awaiting states ride the approval/question channels; turn outcomes ride
   *  turn.ended. */
  busy: boolean;
  /** Whether the main agent has an active turn. Unlike busy, this excludes
   *  background tasks and sub-agent work. */
  mainTurnActive?: boolean;
  /** List-level fallback for the action-required badge. */
  pendingInteraction?: 'none' | 'approval' | 'question';
  /** Outcome of the main agent's most recent turn (when the server reports
   *  one). Presentation rule for the "aborted" tag:
   *  `!busy && (cancelled | failed)`. */
  lastTurnReason?: 'completed' | 'cancelled' | 'failed' | 'stalled' | 'interrupted';
  archived: boolean;
  /** Whether the session's wire log is gzip-compressed on disk (the
   *  session-retention domain's shape verdict). Absent on older engines. */
  compressed?: boolean;
  currentPromptId?: string;
  /** Text of the most recent user prompt, for search/preview. */
  lastPrompt?: string;
  cwd: string;
  model: string;
  usage: AppSessionUsage;
  messageCount: number;
  lastSeq: number;
  /**
   * The workspace this session belongs to. Present once the daemon ships the
   * workspace registry (returns `workspace_id` on Session). Until then it is
   * undefined and the composable maps sessions to workspaces by cwd === root.
   */
  workspaceId?: string;
  /**
   * Set on a child ("side chat") session — the id of the parent it was forked
   * from. Used to keep child sessions out of the main session list.
   */
  parentSessionId?: string;
  /** Absolute path to the session's physical storage directory
   *  (e.g. ~/.mirri-code/sessions/<workspace_hash>/session_<id>/).
   *  Optional — older servers may not include it. */
  sessionDir?: string;
}

/**
 * Live runtime state from GET /sessions/{id}/status — the source of truth for
 * the current model + context usage (Session.agent_config.model can be "").
 */
export interface AppSessionRuntimeStatus {
  /** Current model alias, or null if the daemon couldn't resolve it. */
  model: string | null;
  thinkingEffort: string;
  permission: string;
  planMode: boolean;
  swarmMode: boolean;
  /** Collaboration-mode toggle (opt-in exposure of the Collaboration* tools). */
  collaborationMode: boolean;
  contextTokens: number;
  maxContextTokens: number;
  contextUsage: number;
}

// ---------------------------------------------------------------------------
// Workspace — a real folder the client organizes sessions by.
// 1 Workspace : N Sessions. A session inherits the workspace's root as its cwd.
// ---------------------------------------------------------------------------

export interface AppWorkspace {
  /** Stable id. In fallback mode (derived from session cwds) this IS the root. */
  id: string;
  /** Absolute path to the project root. */
  root: string;
  /** Display name — defaults to basename(root), may be renamed on the daemon. */
  name: string;
  /** Whether root is inside a git repository. */
  isGitRepo: boolean;
  /** Current branch, when known. */
  branch?: string;
  /** ISO timestamp of when this workspace was last opened. */
  lastOpenedAt?: string;
  /** Number of sessions belonging to this workspace. */
  sessionCount: number;
  /**
   * Whether the user trusts this workspace (W8, the rust engine's
   * per-workspace trust gate): untrusted workspaces stay usable, but their
   * project-level config (MCP servers + the AGENTS.md project section) is
   * not in effect and the UI honestly shows that. The TS REST daemon has
   * no trust column on its workspace list — its adapter maps `true`.
   */
  trusted: boolean;
}

/** The workspace-deletion mode (the rust engine's three-choice unregister):
 *  `purge` deletes the workspace's data unrecoverably; `remove` only
 *  unregisters it — every byte stays and re-adding the same root restores
 *  the sessions. The TS daemon is remove-only (its DELETE keeps data). */
export type WorkspaceDeleteMode = 'purge' | 'remove';

/** `workspace.delete` result (the rust engine's wire face; the REST client
 *  synthesizes the remove-only equivalent for the TS daemon). */
export interface WorkspaceDeletionResult {
  ok: boolean;
  mode: WorkspaceDeleteMode;
  workspaceId: string;
  /** Index session rows deleted by the purge; always 0 for remove (the
   *  data-preserving mode). */
  deletedSessions: number;
}

/** One directory entry from the daemon folder browser (fs:browse). */
export interface FsBrowseEntry {  name: string;
  path: string;
  isDir: boolean;
  isGitRepo: boolean;
  branch?: string;
}

export interface FsBrowseResult {
  path: string;
  parent: string | null;
  entries: FsBrowseEntry[];
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

export type AppMessageRole = 'user' | 'assistant' | 'tool' | 'system';

export type AppMessageContent =
  | { type: 'text'; text: string }
  | { type: 'toolUse'; toolCallId: string; toolName: string; input: unknown; outputLines?: string[] }
  | { type: 'toolResult'; toolCallId: string; output: unknown; isError?: boolean }
  | { type: 'image'; source: ImageSource }
  | { type: 'video'; source: ImageSource }
  | { type: 'file'; fileId: string; name: string; mediaType: string; size: number }
  | { type: 'thinking'; thinking: string; signature?: string }
  | { type: 'unknown'; raw: unknown };

export type ImageSource =
  | { kind: 'url'; url: string }
  | { kind: 'base64'; mediaType: string; data: string }
  | { kind: 'file'; fileId: string };

export interface AppMessage {
  id: string;
  sessionId: string;
  role: AppMessageRole;
  content: AppMessageContent[];
  createdAt: string;
  promptId?: string;
  parentMessageId?: string;
  /** Client-side measured duration from turn.started to turn.ended (ms). */
  durationMs?: number;
  metadata?: Record<string, unknown>;
  /** Activated skills recorded on the rewritten body message (ADR-006). */
  withSkills?: Array<{ name: string; target: string; trigger: string }>;
}

/**
 * Metadata key of the client-side compaction marker message appended on
 * compactionCompleted. The transcript keeps all prior messages (TUI parity);
 * this marker renders as a "context compacted" divider. Snapshot-loaded
 * summary messages (origin kind 'compaction_summary') render the same way
 * but carry no token stats.
 */
export const COMPACTION_MARKER_METADATA_KEY = 'mirriWeb.compaction';

/**
 * Metadata key of the client-side "swarm ended" marker message appended when a
 * `/swarm <task>`-opened session's main turn ends with no running swarm
 * subagents (B10 D-25③). Renders as a transcript divider, mirroring the TUI's
 * 'Swarm ended' marker.
 */
export const SWARM_ENDED_MARKER_METADATA_KEY = 'mirriWeb.swarmEnded';

/**
 * Metadata key of the optimistic user-message echo's E9 file references
 * (M5): the submit pipeline stamps the reference list it actually sent
 * here, and the bubble reads it back to render chips from metadata instead
 * of re-parsing `@path` spellings. In-memory only (client-side optimistic
 * echo) — a reloaded transcript has no metadata and falls back to the
 * `@path` regex rendering.
 */
export const ECHOED_FILE_REFERENCES_METADATA_KEY = 'mirriWeb.echoedFileReferences';

export interface CompactionMarkerMetadata {
  trigger: 'manual' | 'auto';
  tokensBefore?: number;
  tokensAfter?: number;
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/**
 * Runtime thinking level. 'off' disables extended thinking; 'on' is the
 * enable signal for legacy boolean models (those without `support_efforts`);
 * any other string is a model-declared effort level (e.g. 'low'/'high'/'max').
 *
 * `support_efforts` is the single source of truth for which concrete levels a
 * model accepts; providers silently drop unknown efforts rather than erroring.
 * Collapses to `string` at runtime — this is a semantic marker, not a closed
 * enum. Mirrors kosong's `ThinkingEffort`.
 */
export type ThinkingLevel = 'off' | 'on' | (string & {});

export interface PromptSubmission {
  content: AppMessageContent[];
  metadata?: Record<string, unknown>;
  /** Optional non-main agent id, used by BTW side-channel prompts. */
  agentId?: string;
  /** The daemon requires these on every prompt (per-prompt, not session-level). */
  model?: string;
  /** Omit to leave the session profile's thinking untouched — the daemon then
   *  resolves the config/model default (same as an unset [thinking] in the TUI). */
  thinking?: ThinkingLevel;
  /** Per-prompt mode controls below: consumed by the v1/v2 REST channel
   *  (swarm_mode etc. ride the prompt config there). The v3 WsRpcApi channel
   *  INTENTIONALLY drops all of them (B10 D-26) — mode state flows only
   *  through the dedicated RPCs routed by updateSession. */
  permissionMode?: 'manual' | 'auto' | 'yolo';
  planMode?: boolean;
  swarmMode?: boolean;
  collaborationMode?: boolean;
  goalObjective?: string;
  goalControl?: 'pause' | 'resume' | 'cancel';
  /** E9 structured workspace file references — a channel BESIDES the text:
   *  the engine materializes each path into the turn's context (files inline
   *  with a line budget, directories as listings). Only the v3 rust engine
   *  consumes them; the v2 REST transport drops the field entirely (the
   *  `@path` spelling in the text survives as plain text there). */
  references?: Array<{ path: string; kind: 'file' | 'folder'; root?: string }>;
}

/** One E9 file reference of a `PromptSubmission` (the wire shape shared by
 *  the doc collector in `lib/fileMention.ts`). */
export interface PromptReference {
  path: string;
  kind: 'file' | 'folder';
  root?: string;
}

/**
 * Metadata the submit pipeline attaches to the local optimistic user-message
 * echo so the bubble renders EXACTLY the references the user chipped (M5,
 * US9.10) — `@path` spellings that did NOT come from a chip (hand-typed text)
 * stay plain text. Dropped before the wire (the engine has its own
 * materialization record); purely a client-side render hint, in-memory only.
 */
export interface EchoedFileReference {
  path: string;
  kind: 'file' | 'folder';
  /** True when the pre-send line probe found more lines than the engine's
   *  1500-line injection budget (M4, US9.4) — the chip paints a「已截取」mark. */
  truncated?: boolean;
}

export interface PromptSubmitResult {
  promptId: string;
  userMessageId: string;
  /** 'running' when the prompt started a turn immediately; 'queued' when
      another prompt is active and the daemon parked it (steerable). */
  status?: 'running' | 'queued';
  /** E9: references the engine could NOT materialize (fail-soft — the turn
   *  still ran; each entry says why the path was skipped). Absent when every
   *  reference resolved. v3 rust only. */
  invalidReferences?: Array<{ path: string; reason: string }>;
}

/** One entry of the daemon prompt queue (GET /sessions/{id}/prompts). */
export interface AppPromptItem {
  promptId: string;
  status: 'running' | 'queued' | 'blocked';
  content: AppMessageContent[];
  createdAt: string;
}

/**
 * Result of steering content into a session's running turn. `steered` is
 * false only when the turn had already ended and the prompt started a turn
 * of its own (the legacy daemon path reports this; the prompt id then wires
 * up abort handling like a regular send).
 */
export interface SteerSessionResult {
  steered: boolean;
  /** The prompt/turn id, when the backend mints one for the steered content. */
  promptId?: string;
}

// ---------------------------------------------------------------------------
// Approval
// ---------------------------------------------------------------------------

export type ApprovalDecision = 'approved' | 'rejected' | 'cancelled';

export interface ApprovalResponse {
  decision: ApprovalDecision;
  scope?: 'session';
  feedback?: string;
  selectedLabel?: string;
  /** WEB-INTERNAL (B9 / P-012): the "always allow" upgrade directive — the
   *  engine approval-rule facet to persist as a user-level allow rule after
   *  this approval. Never serialized to the approval wire (the engine's gate
   *  reads only `decision`/`feedback`); `lib/alwaysAllowUpgrade.ts` strips
   *  it and performs the rule write. */
  alwaysAllowRule?: string;
}

export interface AppApprovalRequest {
  approvalId: string;
  sessionId: string;
  turnId?: number;
  toolCallId: string;
  toolName: string;
  action: string;
  display: unknown; // ToolInputDisplay — Web renders what it knows, falls back to generic
  /** Honest expiry (D-13): only present when the engine actually sent one —
   *  the rust engine derives no expiry, so this stays undefined today and
   *  the approval card renders no countdown. */
  expiresAt?: string;
  /** The engine approval-rule facet the call was gated on (B9, e.g. 'Bash')
   *  — present only when the tool reported one. Drives the card's
   *  "always allow" upgrade (W4), which persists it as a user-level allow
   *  rule. */
  approvalRule?: string;
  /** The requesting member's full agent id (`agent-<uuid>`, supervision B1)
   *  — present only on member-originated approvals; the main agent's rows
   *  omit it (single-agent sessions stay unchanged). Drives the card's
   *  member badge via the live member roster. */
  agentId?: string;
  createdAt: string;
}

// ---------------------------------------------------------------------------
// Question
// ---------------------------------------------------------------------------

export interface QuestionOption {
  id: string;
  label: string;
  description?: string;
  recommended?: boolean;
}

export interface QuestionItem {
  id: string;
  question: string;
  header?: string;
  body?: string;
  options: QuestionOption[];
  multiSelect?: boolean;
  allowOther?: boolean;
  otherLabel?: string;
  otherDescription?: string;
}

export interface AppQuestionRequest {
  questionId: string;
  sessionId: string;
  turnId?: number;
  toolCallId?: string;
  questions: QuestionItem[];
  createdAt: string;
}

export type QuestionAnswer =
  | { kind: 'single'; optionId: string }
  | { kind: 'multi'; optionIds: string[] }
  | { kind: 'other'; text: string }
  | { kind: 'multiWithOther'; optionIds: string[]; otherText: string }
  | { kind: 'skipped' };

export interface QuestionResponse {
  answers: Record<string, QuestionAnswer>;
  method?: 'enter' | 'space' | 'number_key' | 'click';
  note?: string;
}

// ---------------------------------------------------------------------------
// Background Task
// ---------------------------------------------------------------------------

export type AppTaskStatus = 'running' | 'completed' | 'failed' | 'cancelled';
// 成员状态词汇（ADR-008 ①）：`awaiting` 等待用户决策（审批/作答）；`backoff`
// 限流退避挂起（结构化 rate_limit reason，ADR-011）；`aborted` 用户取消的终态
// 分档——与 `failed` 区分呈现（ADR-008 ③）。`awaiting`/`backoff` 只由引擎挂起
// 事件驱动（P3b 起结构化 reason 投影），旧字符串 reason 兼容映射 `suspended`。
export type AppSubagentPhase =
  | 'queued'
  | 'working'
  | 'suspended'
  | 'awaiting'
  | 'backoff'
  | 'completed'
  | 'failed'
  | 'aborted';

/** 结构化挂起原因（ADR-011）：`subagent.suspended` 事件 reason 的投影形态——
 *  snake_case 线形状（`request_id`/`tool_call_id`）在本层统一转 camelCase。
 *  `approval`/`question` 驱动 `awaiting` 阶段；`rate_limit` 驱动 `backoff`。 */
export interface AppSuspendedReason {
  kind: 'approval' | 'question' | 'rate_limit';
  requestId?: string;
  toolCallId?: string;
}

/** A member's structured live tool step (supervision ADR-009 decision ③):
 *  the event projector maintains one entry per tool call the member makes —
 *  name, human-readable arg summary, accumulated progress/output lines and
 *  status — so the panel's tool cards consume structure instead of
 *  re-parsing "Calling …" text lines. */
export interface AgentStep {
  /** The tool call id — stable across started/progress/result frames. */
  id: string;
  /** Raw tool name (renderer registry resolves the icon/label). */
  name: string;
  /** The call's arguments as a display string (raw arg or JSON). */
  arg: string;
  /** ok | running | error — 'running' until the result frame lands. */
  status: 'ok' | 'running' | 'error';
  /** Accumulated progress/output lines (progress frames append, results replace). */
  outputLines: string[];
}

export interface AppTask {
  id: string;
  sessionId: string;
  kind: 'subagent' | 'bash' | 'tool';
  description: string;
  status: AppTaskStatus;
  command?: string;
  createdAt: string;
  startedAt?: string;
  completedAt?: string;
  outputPreview?: string;
  outputBytes?: number;
  outputLines?: string[]; // accumulated by eventReducer from task.progress chunks
  /** The subagent's concatenated live output (assistant.delta), accumulated by
   *  the event reducer from `taskProgress` chunks of kind `text`. Grows in the
   *  right-side detail panel like a thinking block. */
  text?: string;
  /** The member's concatenated live reasoning (thinking.delta), accumulated
   *  the same way as `text` — the live narrative's thinking block (ADR-009). */
  thinking?: string;
  subagentPhase?: AppSubagentPhase;
  subagentType?: string;
  parentToolCallId?: string;
  /** 挂起原因：结构化（ADR-011 事件投影）或旧引擎的遗留字符串——兼容窗内
   *  两种形态并存；消费侧以 `typeof` 判别、结构化形态只作标记不作散文渲染。 */
  suspendedReason?: AppSuspendedReason | string;
  swarmIndex?: number;
  /** True only for subagents detached into the background task store. Drives
   *  the dock: the dock lists background subagents, while foreground subagents
   *  render inline in the message flow as the `Agent` tool card. */
  runInBackground?: boolean;
  /** Resolved model alias the subagent is using (explicit → profile default → parent). */
  model?: string;
  /** The id this same subagent has in the server's background-task store
   *  (REST `/tasks`), learned from the `task.started` registration event. The
   *  WS event stream keys the agent by agent id while REST keys it by task id;
   *  this links the two so the REST copy can be folded into this row and so
   *  cancel can target the id REST actually knows. */
  backgroundTaskId?: string;
  /** The subagent's agent id, present on REST `/tasks` subagent rows. Joins a
   *  dock (REST) row back to the WS `subagent.*` row that carries the same
   *  agent id, so the completion event terminates both rows. */
  agentId?: string;
  /** The member's live tool steps, maintained by the event projector from
   *  tool.call.started / tool.progress / tool.result frames (ADR-009). Only
   *  the live path fills this; a settled member's narrative comes from its
   *  journal read face instead. */
  steps?: AgentStep[];
  /** Reducer-internal: length of text / thinking at the current step's start.
   *  A retry boundary truncates both fields back to these marks (ADR-015:
   *  boundary-driven replay dedupe — no content sniffing). */
  stepTextLen?: number;
  stepThinkLen?: number;
  /** Reducer-internal one-shot: the next text/thinking chunk after a boundary
   *  is eligible for the exact-tail replay guard; consumed on first use. */
  tailGuard?: boolean;
}

// ---------------------------------------------------------------------------
// Cron task (B6 management surface)
// ---------------------------------------------------------------------------

/** A session cron task (the v3 rust `cron.list` row). Timestamps are epoch ms. */
export interface AppCronTask {
  id: string;
  cron: string;
  prompt: string;
  recurring: boolean;
  timezone?: string | null;
  stale: boolean;
  createdAt: number;
  lastFiredAt?: number | null;
  nextFireAt?: number | null;
}

// ---------------------------------------------------------------------------
// Permission rules (B9 user-level rule management)
// ---------------------------------------------------------------------------

/** A rule decision as the engine's rule chain evaluates it. */
export type AppPermissionRuleDecision = 'allow' | 'deny' | 'ask';

/** One user-level permission rule (the `approval.listRules` row): persisted
 *  in the mirri home `config.toml`'s `[[permission.rules]]`, in effect for
 *  every session without a restart. `scope` is always 'user' on this face
 *  (project / turn-override / session-runtime rules are not manageable). */
export interface AppPermissionRule {
  decision: AppPermissionRuleDecision;
  scope: 'user';
  pattern: string;
  reason?: string;
}

/** The create payload for `approval.addRule` — user scope implied. */
export interface AppPermissionRuleInput {
  decision: AppPermissionRuleDecision;
  pattern: string;
  reason?: string;
}

// ---------------------------------------------------------------------------
// Goal
// ---------------------------------------------------------------------------

export type AppGoalStatus = 'active' | 'paused' | 'blocked' | 'complete';

export interface AppGoal {
  goalId: string;
  objective: string;
  completionCriterion?: string;
  status: AppGoalStatus;
  turnsUsed: number;
  tokensUsed: number;
  wallClockMs: number;
  terminalReason?: string;
  budget: {
    tokenBudget: number | null;
    remainingTokens: number | null;
    turnBudget: number | null;
    remainingTurns: number | null;
    wallClockBudgetMs: number | null;
    remainingWallClockMs: number | null;
    overBudget: boolean;
  };
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

export type AppTerminalStatus = 'running' | 'exited';

export interface AppTerminal {
  id: string;
  sessionId: string;
  cwd: string;
  shell: string;
  cols: number;
  rows: number;
  status: AppTerminalStatus;
  createdAt: string;
  exitedAt?: string;
  exitCode?: number | null;
}

// ---------------------------------------------------------------------------
// File System
// ---------------------------------------------------------------------------

export type FsKind = 'file' | 'directory' | 'symlink';

/** E9: one candidate of a structured `searchFiles` result — the fields the
 *  mention menu needs beyond the legacy {path, name} pair (kind drives the
 *  folder/file glyph and the drill action; matchPositions drives name
 *  highlighting; root disambiguates multi-root results). */
export interface FileCandidate {
  path: string;
  name: string;
  kind: FsKind;
  score: number;
  matchPositions: number[];
  root: string;
}

/** E9: the structured `searchFiles` result. `indexState` aggregates the
 *  index health of every searched root (building wins — the answer is
 *  provisional; then truncated, then degraded; all-ready is 'ready'). */
export interface SearchFilesResult {
  items: FileCandidate[];
  truncated: boolean;
  indexState: 'ready' | 'building' | 'truncated' | 'degraded';
}

export interface FsEntry {
  path: string;
  name: string;
  kind: FsKind;
  size?: number;
  modifiedAt: string;
  etag?: string;
  mime?: string;
  languageId?: string;
  isBinary?: boolean;
  isSymlinkTo?: string;
  gitStatus?: string;
  childCount?: number;
}

// ---------------------------------------------------------------------------
// Events (app-facing, camelCase)
// ---------------------------------------------------------------------------

/** One MCP server pool transition (rust core `event.mcp.server.status`
 *  payload's `server` object, camelCase on the wire). */
export interface AppMcpServerStatus {
  name: string;
  transport: string;
  status: string;
  toolCount: number;
  pool: string;
  error?: string;
  changeKind?: string;
}

export type AppEvent =
  | { type: 'sessionCreated'; session: AppSession }
  | { type: 'workspaceCreated'; workspace: AppWorkspace }
  | { type: 'workspaceUpdated'; workspace: AppWorkspace }
  | { type: 'workspaceDeleted'; workspaceId: string; root: string }
  | { type: 'sessionUpdated'; session: AppSession; changedFields: string[] }
  | { type: 'sessionDeleted'; sessionId: string }
  | {
      type: 'sessionWorkChanged';
      sessionId: string;
      busy: boolean;
      mainTurnActive?: boolean;
      pendingInteraction?: 'none' | 'approval' | 'question';
      lastTurnReason?: 'completed' | 'cancelled' | 'failed' | 'stalled' | 'interrupted';
      // The wire seq rides the app event (same convention as
      // sessionModelSwitched) so clients can dedupe replayed frames. Optional
      // because the deprecated status_changed fold does not carry it.
      seq?: number;
    }
  | { type: 'sessionMetaUpdated'; sessionId: string; title?: string; lastPrompt?: string }
  | { type: 'sessionUsageUpdated'; sessionId: string; usage: AppSessionUsage; model?: string; swarmMode?: boolean; planMode?: boolean; collaborationMode?: boolean; thinking?: string }
  | { type: 'historyCompacted'; sessionId: string; beforeSeq: number; reason: string; summaryMessageId?: string }
  | { type: 'compactionStarted'; sessionId: string; trigger: 'manual' | 'auto'; instruction?: string }
  | { type: 'compactionCompleted'; sessionId: string; tokensBefore?: number; tokensAfter?: number; summary?: string }
  | { type: 'compactionCancelled'; sessionId: string }
  // Durable mid-turn model switch (rust `event.session.model_switched`):
  // journaled, e.g. from a mid-turn `setModel`. The web appends a local
  // `model-switch-${seq}` marker message (see eventReducer) that
  // messagesToTurns folds into a divider turn.
  | {
      type: 'sessionModelSwitched';
      sessionId: string;
      from: string;
      to: string;
      seq: number;
    }
  | { type: 'messageCreated'; message: AppMessage }
  | { type: 'messageUpdated'; sessionId: string; messageId: string; content: AppMessageContent[]; status: 'pending' | 'completed' | 'error'; durationMs?: number }
  | { type: 'assistantDelta'; sessionId: string; messageId: string; contentIndex: number; delta: { text?: string; thinking?: string } }
  // Side-channel / non-main-agent streaming: carries text/thinking deltas for a
  // specific agent (e.g. a BTW side chat) without folding them into the parent
  // transcript. The web layer routes these to the side-chat panel.
  | { type: 'agentDelta'; sessionId: string; agentId: string; delta: { text?: string; thinking?: string } }
  | { type: 'agentTurnEnded'; sessionId: string; agentId: string; reason?: string }
  | { type: 'toolOutput'; sessionId: string; toolCallId: string; outputChunk: string; stream: 'stdout' | 'stderr' }
  | { type: 'approvalRequested'; sessionId: string; approval: AppApprovalRequest }
  | { type: 'approvalResolved'; sessionId: string; approvalId: string; decision: ApprovalDecision; resolvedAt: string }
  | { type: 'approvalExpired'; sessionId: string; approvalId: string }
  | { type: 'questionRequested'; sessionId: string; question: AppQuestionRequest }
  | { type: 'questionAnswered'; sessionId: string; questionId: string; resolvedAt: string }
  | { type: 'questionDismissed'; sessionId: string; questionId: string; dismissedAt: string }
  | { type: 'taskCreated'; sessionId: string; task: AppTask }
  | {
      type: 'taskProgress';
      sessionId: string;
      taskId: string;
      outputChunk: string;
      stream: 'stdout' | 'stderr';
      /**
       * `line` (default) appends a new progress line (tool-call / tool-progress).
       * `text` concatenates onto the subagent's growing streamed output
       * (`AppTask.text`), shown live in the detail panel like a thinking block.
       * `thinking` concatenates onto the member's live reasoning
       * (`AppTask.thinking`), the narrative's thinking block (ADR-009).
       */
      kind?: 'line' | 'text' | 'thinking';
    }
  | {
      type: 'subagentStepBoundary';
      sessionId: string;
      /** The member task (= subagent agent id) whose step boundary this is. */
      taskId: string;
      /** true for turn.step.retrying (truncate to step start); false for
       *  turn/step starts and interrupts (commit marks). */
      retry: boolean;
    }
  | { type: 'taskCompleted'; sessionId: string; taskId: string; status: AppTaskStatus; outputPreview?: string; outputBytes?: number }
  // B7 W6: web-internal vocabulary (NOT a protocol event) — dispatched by
  // App.vue after the task.detach ack for a model Bash tool call. Stamps the
  // card with the background task id so it wears its `◐ backgrounded` badge
  // even after the moved-to-background tool result lands (US12.2).
  | { type: 'toolCallBackgrounded'; sessionId: string; toolCallId: string; taskId: string }
  | { type: 'goalUpdated'; sessionId: string; goal: AppGoal | null }
  // Prompt-level lifecycle (distinct from turn-level): a prompt that never
  // produced a turn — blocked by a pre-submit hook, or aborted while queued —
  // gets no turn.ended and no session status flip, so these are the web layer's
  // only signal to clear the per-session in-flight state. A normal turn's
  // prompt.completed is a no-op for state (the work_changed ahead of it
  // already finished the prompt).
  | { type: 'promptCompleted'; sessionId: string; promptId: string; reason: string }
  | { type: 'promptAborted'; sessionId: string; promptId: string }
  // The turn ended and the engine reported its numeric wire turn id: the
  // turn's rows were keyed by the UUID-backed correlation id so far — rebind
  // them to the durable `turn-<id>` key so fork-at-turn (and the usage
  // attribution surface) sees the same identity a snapshot reload produces.
  | { type: 'turnAnchored'; sessionId: string; promptId: string; anchorId: string }
  // The MAIN agent's turn boundary — the single source of truth for "the main
  // conversation has a turn in flight" (half of the working moon, and the
  // streaming reveal). Deliberately NOT derived from session status: a
  // background subagent or BTW side chat keeps the session busy but must not
  // light up the main conversation's moon. `reason` rides on deactivation;
  // `startedAtMs` (the projector's `turn.started` wall-clock, epoch-ms) rides
  // on activation — the live-observed anchor for the streaming header's
  // elapsed clock (the server snapshot anchor is restored separately).
  | { type: 'turnActiveChanged'; sessionId: string; active: boolean; reason?: string; startedAtMs?: number }
  | { type: 'configChanged'; changedFields: string[]; config: AppConfig }
  | { type: 'agentProfilesChanged' }
  // MCP server pool transition (rust core `event.mcp.server.status`): a
  // background server connect/fail/auth transition. Surfaced as a transient
  // floating notice (self-dismissing toast), never as session state.
  | { type: 'mcpServerStatus'; sessionId: string; server: AppMcpServerStatus }
  | {
      type: 'modelCatalogChanged';
      changed: { providerId: string; providerName: string; added: number; removed: number }[];
      unchanged: string[];
      failed: { provider: string; reason: string }[];
    }
  | { type: 'unknown'; raw: unknown };

// ---------------------------------------------------------------------------
// WebSocket connection helpers
// ---------------------------------------------------------------------------

/** Per-session sync cursor (v2): durable seq + journal epoch. */
export interface AppSessionCursor {
  seq: number;
  epoch?: string;
}

/** In-flight (mid-turn) state recovered from the session snapshot. */
export interface AppInFlightToolCall {
  toolCallId: string;
  name: string;
  args?: unknown;
  description?: string;
  lastProgress?: { kind: string; text?: string; percent?: number };
}

export interface AppInFlightTurn {
  /** The TS daemon's numeric turn counter, or the rust engine's wire
   *  `turn-<uuid>` string — both key the projector's turn→prompt map. */
  turnId: number | string;
  assistantText: string;
  thinkingText: string;
  runningTools: AppInFlightToolCall[];
  /** Authoritative daemon prompt_id for the active prompt, if known. */
  promptId?: string;
  /** The running turn's real start time (epoch-ms) — the rust engine stamps
   *  `current_turn_started_at` from the wire log's open turn. Anchors the
   *  streaming header's live "Working for X" clock across a page refresh;
   *  absent on engines that predate the field. */
  startedAtMs?: number;
}

/**
 * IM-style initial sync result: everything needed to rebuild a session's UI
 * state, consistent at `asOfSeq`. The standard flow is
 * `getSessionSnapshot()` → `subscribe(sessionId, {seq: asOfSeq, epoch})`.
 */
export interface AppSessionSnapshot {
  asOfSeq: number;
  epoch: string;
  session: AppSession;
  /** Most recent messages, chronological ascending. */
  messages: AppMessage[];
  hasMoreMessages: boolean;
  /** Per-turn usage of completed historical turns — seeds the projector's
   *  turn-usage map so the usage pill survives a rerender. Optional: the
   *  kap-server REST path never fills it. */
  turnUsages?: Array<{ promptId: string } & TurnUsage>;
  inFlightTurn: AppInFlightTurn | null;
  /** Live subagent roster at the watermark — rebuilds swarm cards on refresh. */
  subagents: AppTask[];
  pendingApprovals: AppApprovalRequest[];
  pendingQuestions: AppQuestionRequest[];
  /** The session's folded todo list (the rust engine's wire state, the same
   *  source the TUI resume panel reads) — structurally the `TodoView` shape
   *  (`src/types.ts`). Optional: engines that predate the field omit it and
   *  the composable falls back to deriving the list from the message
   *  history. Present-but-empty is a valid cleared list. */
  todos?: Array<{ title: string; status: 'pending' | 'in_progress' | 'done' }>;
}

/** Raw stream coordinates present only for kap-server assistant/thinking
    deltas. They let the render queue merge chunks without guessing continuity. */
export interface MirriEventMeta {
  sessionId: string;
  seq: number;
  stream?: {
    turnId: number;
    offset: number;
    kind: 'text' | 'thinking';
  };
}

export interface MirriEventHandlers {
  onEvent(event: AppEvent, meta: MirriEventMeta): void;
  onResync(sessionId: string, currentSeq: number, epoch?: string): void;
  onError(code: number, msg: string, fatal: boolean): void;
  onConnectionChange(connected: boolean): void;
  onTerminalOutput?(sessionId: string, terminalId: string, data: string, seq: number): void;
  onTerminalExit?(sessionId: string, terminalId: string, exitCode: number | null): void;
}

/**
 * Terminal-stream sinks bound through MirriWebApi.connectTerminalEvents() —
 * additive to the app's MirriEventHandlers. Lets a terminal surface join the
 * shared event connection WITHOUT rebinding the app's handler set.
 */
export interface TerminalEventSinks {
  onTerminalOutput(sessionId: string, terminalId: string, data: string, seq: number): void;
  onTerminalExit(sessionId: string, terminalId: string, exitCode: number | null): void;
  onConnectionChange?(connected: boolean): void;
  onError?(code: number, msg: string, fatal: boolean): void;
}

export interface MirriEventConnection {
  subscribe(sessionId: string, cursor?: AppSessionCursor): void;
  unsubscribe(sessionId: string): void;
  /**
   * Bind the real daemon prompt_id to the next turn for a session, so the
   * client-side projector stops synthesizing a random promptId on turn.started.
   * Call right after submitPrompt() returns.
   */
  bindNextPromptId(sessionId: string, promptId: string): void;
  /**
   * Seed the client-side projector with a snapshot's in-flight turn so a
   * reconnecting client renders mid-turn state immediately; emits the
   * corresponding AppEvents through `onEvent`. Resets per-session projector
   * state first — call BEFORE subscribe(), with the snapshot's cursor.
   */
  seedSnapshot(sessionId: string, snapshot: AppSessionSnapshot): void;
  abort(sessionId: string, promptId: string): void;
  terminalAttach(sessionId: string, terminalId: string, sinceSeq?: number): void;
  terminalInput(sessionId: string, terminalId: string, data: string): void;
  terminalResize(sessionId: string, terminalId: string, cols: number, rows: number): void;
  terminalDetach(sessionId: string, terminalId: string): void;
  terminalClose(sessionId: string, terminalId: string): void;
  /**
   * Mark an agent as a side-channel (e.g. BTW side chat). The client-side
   * projector will then emit its text/thinking deltas as agent-scoped events
   * instead of dropping them like background subagents.
   */
  markSideChannelAgent(agentId: string): void;
  /**
   * Query the client-side projector's per-prompt turn usage (usage-visibility
   * Task 6): merges the usage of every turn bound to the prompt id. Returns
   * undefined when no bound turn recorded usage (e.g. a session loaded from a
   * snapshot before the projector saw any step). Read-only.
   */
  turnUsage(sessionId: string, promptId: string): TurnUsage | undefined;
  /**
   * Report the underlying socket's health. Used to detect a silent-half-open
   * connection after the tab was frozen in the background: the browser still
   * reports OPEN (so no auto-reconnect) yet no frames have arrived for a while.
   */
  health(): { connected: boolean; open: boolean; stale: boolean };
  /**
   * Force a clean reconnect of the underlying socket. Used to recover from a
   * silent-half-open (background-tab freeze) where onclose never fires. The
   * reconnect handshake re-subscribes at the last durable cursor. No-op after
   * close().
   */
  reconnect(): void;
  close(): void;
}

// ---------------------------------------------------------------------------
// Model + Provider (app-facing, camelCase)
// PRESUMED — not in current daemon docs; isolated in adapter, swap when backend defines them.
// ---------------------------------------------------------------------------

export interface AppModel {
  /** Unique identifier for this model (the string passed to PATCH session agent_config.model) */
  id: string;
  /** Provider id this model belongs to */
  provider: string;
  /** Raw model name (e.g. "moonshot-v1-128k") */
  model: string;
  /** Optional human-readable display name */
  displayName?: string;
  /** Maximum context size in tokens */
  maxContextSize: number;
  /** Optional capability tags (e.g. ["vision", "thinking"]) */
  capabilities?: string[];
  /** Effort levels this model supports for extended thinking (e.g. ["low", "high", "max"]).
      Sourced from the model catalog (managed) or config [models.<id>]. */
  supportEfforts?: readonly string[];
  /** Catalog-declared default effort for extended thinking. */
  defaultEffort?: string;
}

export interface AppProvider {
  /** Provider id */
  id: string;
  /** Provider type (e.g. "moonshot", "anthropic", "openai", "custom") */
  type: string;
  /** Optional custom base URL */
  baseUrl?: string;
  /** Optional default model alias */
  defaultModel?: string;
  /** Whether an API key is stored for this provider */
  hasApiKey: boolean;
  /** When the api_key is a pure env-var reference (e.g. `${MY_KEY}`), the
   *  server exposes the literal reference here so the UI can display it
   *  without leaking a secret. `null` or `undefined` means a literal key
   *  or no key. */
  apiKeyDisplay?: string | null;
  /** Provider connectivity status */
  status: 'connected' | 'error' | 'unconfigured';
  /** Model ids available from this provider */
  models?: string[];
}

export interface ProviderRefreshResult {
  changed: Array<{
    providerId: string;
    providerName: string;
    added: number;
    removed: number;
  }>;
  unchanged: string[];
  failed: Array<{ provider: string; reason: string }>;
}

/** Wire protocols the v2 engine can resolve a catalog entry to. */
export type AppProviderWireType =
  | 'kimi'
  | 'openai'
  | 'openai_responses'
  | 'anthropic'
  | 'google-genai'
  | 'vertexai';

/** A model entry in the remote models.dev catalog (v2 flat shape) — used by
 *  the Add Provider dialog to show what models a catalog provider brings in. */
export interface AppCatalogModel {
  id: string;
  name?: string;
  maxContextSize: number;
  capabilities?: string[];
  reasoning: boolean;
}

/** A provider entry in the remote models.dev catalog (v2 shape). `wireType` is
 *  the resolved wire protocol (nullable when the entry can't be resolved);
 *  `needsBaseUrl`/`rejected`/`rejectReason`/`envKey` drive the import form. */
export interface AppCatalogProvider {
  id: string;
  name?: string;
  wireType?: AppProviderWireType | null;
  guessed?: boolean;
  needsBaseUrl?: boolean;
  rejected?: boolean;
  rejectReason?: string | null;
  envKey?: string | null;
  models: AppCatalogModel[];
}

/** One model entry of the v2 manual provider-create payload. */
export interface AppAddProviderModelInput {
  model: string;
  maxContextSize: number;
  displayName?: string;
  capabilities?: string[];
  maxOutputSize?: number;
  supportEfforts?: string[];
  adaptiveThinking?: boolean;
}

/** v2 `POST /providers` create body: explicit id + wire type. The server
 *  schema requires the models key but allows an empty list, so a custom
 *  provider can be created without models (added later from the provider
 *  detail panel). */
export interface AppAddProviderInput {
  id: string;
  type: string;
  apiKey?: string;
  baseUrl?: string;
  defaultModel?: string;
  models: AppAddProviderModelInput[];
}

export interface AppConfigProvider {
  type: string;
  baseUrl?: string;
  defaultModel?: string;
  hasApiKey: boolean;
  /** When the api_key is a pure env-var reference (e.g. `${MY_KEY}`), the
   *  server exposes the literal reference here so the UI can display it
   *  without leaking a secret. `null` means a literal key or no key. */
  apiKeyDisplay?: string | null;
  /** Write-only: used in config patches to set or update the API key. Never
   *  populated in server responses (the server only returns `hasApiKey`
   *  and `apiKeyDisplay`). */
  apiKey?: string;
  /** The provider's env sub-table — per-variable literals the runtime
   *  resolves credentials and base URLs through (the config-side tier
   *  below the credential store). Only the per-type conventional key
   *  participates in resolution; case-sensitive spellings are preserved. */
  env?: Record<string, string>;
}

/** The credential value a provider stores — the tagged union mirroring the
 *  engine's CredentialValue (provider-credential spec D-M4). `oauth` is a
 *  reserved engine slot the web face never writes today. */
export type ProviderCredentialValue =
  | { type: 'key'; key: string }
  | { type: 'external'; method_id: string; metadata: { resource_name: string } };

/** The `credential.status` view: `normal` with no evidence, or
 *  `needs_reconfigure` carrying the secret-redacted failure message. */
export interface ProviderCredentialStatus {
  status: 'normal' | 'needs_reconfigure';
  message?: string;
}

/** The app-facing projection of one `credential.list` row (snake_case wire:
 *  `{provider_id, type, time_created, status, external_available?}`) —
 *  defined here so MirriWebApi references it without a lib→types cycle. */
export interface ProviderCredentialInfo {
  providerId: string;
  /** 'key' | 'external' (| 'oauth' — reserved slot, not reachable yet). */
  type: string;
  timeCreated: number;
  /** 'normal' | 'needs_reconfigure'. */
  status: string;
  /** Only carried by external rows: whether the referenced source's CLI is
   *  present on the machine (a real probe server-side). */
  externalAvailable?: boolean;
}

/** A model alias as stored in config.toml ([models."provider/model"]). Mirrors
 *  agent-core's ModelAlias — all fields except `provider`/`model` are optional
 *  on the edit form. Catalog refreshes never modify existing alias fields, so
 *  there is no separate overrides layer — what you see is what the runtime
 *  uses. */
export interface AppModelAlias {
  provider: string;
  model: string;
  maxContextSize: number;
  maxOutputSize?: number;
  capabilities?: string[];
  displayName?: string;
  description?: string;
  reasoningKey?: string;
  protocol?: 'anthropic';
  adaptiveThinking?: boolean;
  supportEfforts?: string[];
  defaultEffort?: string;
  betaApi?: boolean;
}

export interface AppConfig {
  providers: Record<string, AppConfigProvider>;
  defaultProvider?: string;
  defaultModel?: string;
  models?: Record<string, AppModelAlias>;
  thinking?: { enabled?: boolean; effort?: string };
  planMode?: boolean;
  yolo?: boolean;
  defaultPermissionMode?: string;
  defaultPlanMode?: boolean;
  permission?: unknown;
  hooks?: unknown[];
  services?: unknown;
  mergeAllAvailableSkills?: boolean;
  extraSkillDirs?: string[];
  /** User-level disabled skill names (rust `disabled_skills` config key, B8). */
  disabledSkills?: string[];
  extraAgentDirs?: string[];
  disabledAgents?: string[];
  loopControl?: unknown;
  background?: unknown;
  experimental?: Record<string, boolean>;
  telemetry?: boolean;
  /** The `[retention]` section — the session-retention domain's automatic
   *  archive-compression knobs (camelCase on the wire; the engine's
   *  `config.set` converts to snake_case before persisting). */
  retention?: {
    enabled?: boolean;
    archiveGraceDays?: number;
    intervalHours?: number;
  };
  raw?: Record<string, unknown>;
}

/** A session-scoped skill the user can invoke from the slash menu. */
export interface AppSkill {
  name: string;
  description: string;
  /** Skill source (e.g. 'builtin' | 'project' | 'plugin') for grouping/labels. */
  source: string;
  /** Contributing plugin id for plugin-sourced skills; absent when the
   * engine did not send one (ADR-012 — degrade to the generic extra badge). */
  plugin?: string;
  /** Absolute path of the skill's SKILL.md (rust `skill.list` management view). */
  path?: string;
  /** Skill directory (engine dir field; management view). */
  dir?: string;
  /** False when the user disabled this skill at the user level; absent
   * (older backends without the field) means enabled (B8 D-3). */
  enabled?: boolean;
}

/** Profile source kind — which registry layer contributed the profile. */
export type AgentProfileSource =
  | 'builtin'
  | 'user'
  | 'project'
  | 'extra'
  | 'plugin'
  | 'explicit';

/** A same-name candidate that lost the merge to the resolved profile. */
export interface AppAgentSuppressedCandidate {
  source: AgentProfileSource;
  priority: number;
  reason: string;
}

/** An agent file skipped during discovery (parse failure). */
export interface AppAgentSkippedFile {
  path: string;
  reason: string;
}

/** An agent profile (built-in or custom) the user can manage. */
export interface AppAgentProfile {
  name: string;
  description?: string;
  source: AgentProfileSource;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  hasOverride: boolean;
  filePath?: string;
  defaultModel?: string;
  tools?: string[];
  /** Tools this profile must never use (denylist, editable — A4-2). */
  disallowedTools?: string[];
  /** Capability tags the profile requires (e.g. `code.read`) — read-only
   * display with human-readable descriptions (A2-3). */
  capabilitiesRequired?: string[];
  /** Agent names this profile may dispatch to as subagents. */
  subagents?: string[];
  /** Same-name candidates that lost the merge — expandable in the UI. */
  suppressedCandidates?: AppAgentSuppressedCandidate[];
  whenToUse?: string;
  systemPromptTemplate?: string;
  /**
   * The composition form of the row's prompt body (ADR-010 ①, US-A4 form
   * disclosure): `composed` — the body rides the shared base prompt;
   * `replacement` — the body replaces the shared base entirely.
   */
  promptForm?: 'composed' | 'replacement';
  /**
   * Whether a `<brandHome>/SYSTEM.md` base-prompt override is active
   * (D21 / US-A8). Present on the essential default row only.
   */
  basePromptOverride?: boolean;
  /** The raw SYSTEM.md text — present only when the override exists. */
  basePromptTemplate?: string;
  /**
   * The builtin default base prompt template's RAW text (placeholders
   * intact — NOT the rendered form). Present on the essential default row
   * only: the editor's seed when no override exists. Read-only.
   */
  basePromptDefaultTemplate?: string;
  /**
   * Full rendered system prompt (read-only preview). Only present for
   * built-in profiles, whose prompt is code-defined.
   */
  effectiveSystemPrompt?: string;
}

/** `listAgents` result — the resolved profiles plus discovery warnings. */
export interface AppAgentProfileList {
  profiles: AppAgentProfile[];
  skippedAgentFiles: AppAgentSkippedFile[];
}

/** Editable agent profile fields for the create payload. */
export interface AppAgentProfileInput {
  name: string;
  description?: string;
  defaultModel?: string;
  tools?: string[];
  /** Tools this profile must never use (denylist; edited in the UI, A4-2). */
  disallowedTools?: string[];
  whenToUse?: string;
  systemPromptTemplate?: string;
  /** Agent names this profile may dispatch to. */
  subagents?: string[];
}

/** Editable agent profile fields for the update payload. */
export type AppAgentProfileUpdate = Partial<Omit<AppAgentProfileInput, 'name'>> & {
  /**
   * The `<brandHome>/SYSTEM.md` base-prompt override body (D21 / US-A8) —
   * only meaningful on the essential `agent` row: a non-empty string writes
   * the file, `''` deletes it (reset to default). Absent means "do not touch".
   */
  basePromptTemplate?: string;
};

export interface AppToolDescriptor {
  name: string;
  description: string;
  source: 'builtin' | 'skill' | 'mcp';
  mcpServerId?: string;
  /** The engine's effective enabled verdict for this tool (the rust
   *  `mcp.tools` wire carries it — reflects both the disabledTools
   *  blocklist and the enabledTools allowlist). Undefined (older engines /
   *  the HTTP backend) means enabled — callers treat absence as `true`. */
  enabled?: boolean;
}

export interface AppMcpServer {
  id: string;
  name: string;
  transport: 'stdio' | 'http' | 'sse';
  status: 'connected' | 'connecting' | 'disconnected' | 'error';
  toolCount: number;
  /** Last error message when status is 'error'. */
  lastError?: string;
  /** Server config with sensitive values (env, headers) redacted. */
  config?: AppMcpServerConfig;
  /** Config layer the effective definition lives in (the rust
   *  `mcp.config.list` source; the HTTP backend leaves this undefined). */
  source?: 'user' | 'projectRoot' | 'project' | 'plugin';
  /** Pools holding a same-name shadowed connection (the no-shadowing
   *  pool view's extra entries — e.g. `['user']` when a project-layer
   *  definition shadows a user-layer one). Empty/undefined when the name
   *  exists in a single scope. */
  shadowedPools?: string[];
}

export interface AppMcpTestResult {
  status: 'connected' | 'error';
  toolCount: number;
  error?: string;
  tools: AppToolDescriptor[];
}

/** `mcp.sync` result (F-6) — what the reconcile pass applied, by op kind:
 *  connects, disconnects, teardown-and-reconnects, and in-place config
 *  refreshes. The all-zero summary is the idempotent steady state. */
export interface AppMcpSyncSummary {
  added: number;
  removed: number;
  reconnected: number;
  refreshed: number;
}

/** MCP server config shape for the Settings MCP panel (create/update). */
export interface AppMcpServerConfig {
  transport: 'stdio' | 'http' | 'sse';
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  url?: string;
  headers?: Record<string, string>;
  enabled?: boolean;
  enabledTools?: string[];
  disabledTools?: string[];
  startupTimeoutMs?: number;
  toolTimeoutMs?: number;
  bearerTokenEnvVar?: string;
}

/**
 * Config-content view of one MCP server (from `GET /mcp/global/config`).
 * `source` is the literal config the user wrote (`${VAR}` references kept
 * verbatim — this is what the Settings panel displays and edits), `resolved`
 * is the effective config after environment-variable expansion.
 */
export interface AppMcpGlobalConfigEntry {
  name: string;
  source: AppMcpServerConfig;
  resolved: AppMcpServerConfig;
}

// ---------------------------------------------------------------------------
// MirriWebApi — the app-facing interface
// ---------------------------------------------------------------------------

export interface AppSessionWarning {
  code: string;
  message: string;
  severity: 'info' | 'warning' | 'error';
}

// ---------------------------------------------------------------------------
// Session retention — the archived/compressed management-page domain
// (rust/crates/mirri-proto/src/retention.rs is the authoritative wire shape).
// ---------------------------------------------------------------------------

/** Which management-page view to list. */
export type AppRetentionView = 'archived' | 'compressed';

/** One management-page row (retention.list). All server-computed fields
 *  (sizes, days) arrive as-is; the client never derives them. */
export interface AppRetentionEntry {
  sessionId: string;
  title: string;
  workspace: string;
  /** RFC 3339 creation stamp; empty string on unreadable-state rows. */
  createdAt: string;
  archivedAt: string | null;
  compressedAt: string | null;
  currentSizeBytes: number;
  savedBytes: number | null;
  compressedDays: number | null;
}

/** One per-tier delete estimate (retention.stats) — the tier buttons'
 *  confirmation "N sessions / X MB". */
export interface AppRetentionTierEstimate {
  days: number;
  count: number;
  bytes: number;
}

/** The management-page header counters (retention.stats). */
export interface AppRetentionStats {
  archivedCount: number;
  compressedCount: number;
  compressedTotalBytes: number;
  reclaimedPotential: number;
  tiers: AppRetentionTierEstimate[];
}

/** One per-session failure inside a tiered batch (retention.deleteTiered). */
export interface AppRetentionTieredFailure {
  sessionId: string;
  error: string;
}

/** The tiered batch outcome (retention.deleteTiered). */
export interface AppRetentionTieredResult {
  deleted: string[];
  failed: AppRetentionTieredFailure[];
  reclaimedBytes: number;
}

/** The resolved [retention] policy (retention.getPolicy) — rendered as the
 *  strategy hint, never editable from the page. */
export interface AppRetentionPolicy {
  enabled: boolean;
  archiveGraceDays: number;
  intervalHours: number;
}

/** Interrupted-task recovery (ADR-004 pull model): one session whose active
 *  turn was cut off by the app exiting — `service_exit` = unexpected
 *  termination, `graceful_exit` = the user quit while a task was running. */
export interface AppRecoveryInterruption {
  sessionId: string;
  workspaceKey: string;
  workspaceName: string;
  title: string;
  /** Epoch-milliseconds timestamp of the interruption. */
  interruptedAt: number;
  cause: 'service_exit' | 'graceful_exit';
  /** Present when the cut-off turn had a background task in flight — the
   *  "check its result before re-running" hint (E.3). */
  pendingBackgroundHint?: string;
}

/** The recovery.list snapshot — entries expire server-side once the
 *  session's next turn settles (or the entry is dismissed). */
export interface AppRecoveryList {
  interruptions: AppRecoveryInterruption[];
  expiresAt?: number;
}

export interface MirriWebApi {
  getHealth(): Promise<{ status: 'ok'; uptimeSec: number }>;
  getMeta(): Promise<{ serverVersion: string; serverId: string; startedAt: string; capabilities: Record<string, boolean>; openInApps: string[]; dangerousBypassAuth: boolean; backend: 'v1' | 'v2' | 'v3' }>;
  listSessions(input?: PageRequest & { busy?: boolean; workspaceId?: string; includeArchive?: boolean; archivedOnly?: boolean; excludeEmpty?: boolean }): Promise<Page<AppSession>>;
  /**
   * Global server-side session search (the rust engine's `session.search`
   * literal read face: NFKC + case-folded substring over session titles
   * and user/assistant message text, time-desc, cursor paginated).
   *
   * Optional on purpose: only the rust transport implements it — the v1/v2
   * TS daemon has no wired consumer for its search service, so callers
   * fall back to client-side title filtering when the method is absent.
   */
  searchSessions?(input: { query: string; sessionId?: string; role?: 'user' | 'assistant' | 'title'; pageSize?: number; pageToken?: string }): Promise<AppSessionSearchPage>;
  createSession(input: { title?: string; cwd?: string; model?: string; workspaceId?: string }): Promise<AppSession>;
  /** Fetch one session by id (deep links beyond the first listSessions page). */
  getSession(sessionId: string): Promise<AppSession>;
  /**
   * Update a session. Every field is applied server-side on the rust
   * transport: `title`/`model`/`thinking` ride `session.updateMeta`, while
   * the mode toggles and the goal control route to their dedicated v2 wire
   * methods (`agent.setPermission`, `plan.enter`/`plan.cancel`,
   * `swarm.enter`/`swarm.exit`, `collaboration.enter`/`collaboration.exit`,
   * `goal.create`, `goal.pause`/`resume`/
   * `cancel`) — the v1 "smuggle everything through the session patch"
   * shape must never silently drop a field again. `cwd` is a v1 leftover
   * the v2 wire has no consumer for.
   */
  updateSession(sessionId: string, input: { title?: string; cwd?: string; model?: string; permissionMode?: string; planMode?: boolean; swarmMode?: boolean; collaborationMode?: boolean; goalObjective?: string; goalControl?: 'pause' | 'resume' | 'cancel'; thinking?: string }): Promise<AppSession>;
  getSessionStatus(sessionId: string): Promise<AppSessionRuntimeStatus>;
  /** Current goal snapshot, or null when the session has no active goal. */
  getSessionGoal(sessionId: string): Promise<AppGoal | null>;
  getSessionWarnings(sessionId: string): Promise<AppSessionWarning[]>;
  archiveSession(sessionId: string): Promise<{ archived: true }>;
  /** SDR-014 recency touch — bump ONLY the session's `lastUsedAt` (the
   *  sidebar sort key) without refreshing updatedAt. Fire-and-forget from
   *  user-interaction paths (selecting a session row); snapshot/resync flows
   * must NOT call it. Optional: only the rust engine implements the
   * `session.touch` RPC — older engines simply never sort by it. */
  touchSession?(sessionId: string): Promise<void>;
  restoreSession(sessionId: string): Promise<AppSession>;
  // --- Session retention (the archived/compressed management page, v3 rust
  //     only — the v1/v2 REST face has no retention routes and throws). ---
  /** List one management view (retention.list). */
  listRetention(view: AppRetentionView): Promise<AppRetentionEntry[]>;
  /** The header counters + the four per-tier delete estimates
   *  (retention.stats). */
  getRetentionStats(): Promise<AppRetentionStats>;
  /** Compress one archived session immediately (retention.compressNow). */
  compressRetentionSession(sessionId: string): Promise<{ ok: boolean }>;
  /** Delete one session (retention.delete) — answers the reclaimed bytes. */
  deleteRetentionSession(sessionId: string): Promise<{ ok: boolean; reclaimedBytes: number }>;
  /** The tier-button batch delete (retention.deleteTiered) —
   *  olderThanDays must be one of 30/90/180/365. */
  deleteRetentionTiered(olderThanDays: number): Promise<AppRetentionTieredResult>;
  /** The resolved [retention] knobs (retention.getPolicy). */
  getRetentionPolicy(): Promise<AppRetentionPolicy>;
  // --- Interrupted-task recovery (ADR-004 pull model, v3 rust only —
  //     older engines have no recovery routes; every method is optional so
  //     callers degrade to no notice card / no badge. ---
  /** The interruption list shown at startup (recovery.list). */
  listRecovery?(): Promise<AppRecoveryList>;
  /** Re-fire the cut-off turn in its session (recovery.resume). The
   *  engine passes the message-send fire result through verbatim — no
   *  client-side turnId mapping. */
  resumeInterrupted?(sessionId: string): Promise<unknown>;
  /** Re-fire every interruption (recovery.resumeAll) — per-session fire
   *  turns, no global concurrency cap (C.5 deferred). */
  resumeAllInterrupted?(): Promise<{ accepted: number; queued: number; failed: Array<{ sessionId: string; error: string }> }>;
  /** Drop one interruption entry server-side (recovery.dismiss). */
  dismissInterrupted?(sessionId: string): Promise<void>;
  /** Drop every interruption entry (recovery.dismissAll). */
  dismissAllInterrupted?(): Promise<void>;
  /** `agentId` addresses a member agent's own history (supervision A2 — the
   *  rust `message.list` member-scoped read face); omitted = the session's
   *  main history. */
  listMessages(sessionId: string, input?: PageRequest & { role?: AppMessageRole; agentId?: string }): Promise<Page<AppMessage>>;
  /** v2 initial sync: atomic session state + `asOfSeq` watermark + epoch. */
  getSessionSnapshot(sessionId: string): Promise<AppSessionSnapshot>;
  submitPrompt(sessionId: string, input: PromptSubmission): Promise<PromptSubmitResult>;
  /** Daemon prompt queue for a session — server-side truth for `queued` work. */
  listPrompts(
    sessionId: string,
  ): Promise<{ active: AppPromptItem | null; queued: AppPromptItem[] }>;
  /** Steer daemon-queued prompts into the active turn (TUI ctrl+s). */
  steerPrompts(sessionId: string, promptIds: string[]): Promise<{ steered: boolean; promptIds: string[] }>;
  /** Steer content into the session's running turn (TUI ctrl+s). The rust
   *  engine parks the content in the turn's steer buffer; the legacy daemon
   *  parks the prompt queue entry. Falls back to a normal send when idle. */
  steerSession(sessionId: string, input: PromptSubmission): Promise<SteerSessionResult>;
  abortPrompt(sessionId: string, promptId: string): Promise<{ aborted: boolean; atSeq?: number }>;
  /** Cancel whatever is running in the session, including skill activations. */
  abortSession(sessionId: string): Promise<{ aborted: boolean }>;
  /** Request history compaction. `started: false` = the engine skipped it
   * (nothing to compact — empty history, or it fits the retention budget);
   * callers surface that as an info notice instead of staying silent. */
  compactSession(sessionId: string, instruction?: string): Promise<{ started: boolean }>;
  undoSession(sessionId: string, count?: number): Promise<void>;
  forkSession(sessionId: string, input?: { title?: string; turnId?: number }): Promise<AppSession>;
  /** Create a child session under a parent — POST /sessions/{id}/children. */
  createChildSession(sessionId: string, input?: { title?: string }): Promise<AppSession>;
  /** List a session's child sessions — GET /sessions/{id}/children. */
  listChildSessions(sessionId: string): Promise<AppSession[]>;
  /** Start a BTW side-channel agent under the session — POST /sessions/{id}:btw. */
  startBtw(sessionId: string): Promise<{ agentId: string }>;
  respondApproval(sessionId: string, approvalId: string, response: ApprovalResponse): Promise<{ resolved: true; resolvedAt: string }>;
  respondQuestion(sessionId: string, questionId: string, response: QuestionResponse): Promise<{ resolved: true; resolvedAt: string }>;
  dismissQuestion(sessionId: string, questionId: string): Promise<{ dismissed: true; dismissedAt: string }>;
  listSkills(sessionId: string): Promise<AppSkill[]>;
  /** List skills for a workspace (no session required) — GET /workspaces/{id}/skills. */
  listSkillsForWorkspace(workspaceId: string): Promise<AppSkill[]>;
  activateSkill(
    sessionId: string,
    skillName: string,
    args?: string,
  ): Promise<{ activated: true; skillName: string; promptId: string; status: 'queued' | 'running' }>;
  /** Enable/disable a skill at the user level (v3 rust only). */
  setSkillEnabled(skillName: string, enabled: boolean): Promise<{ name: string; enabled: boolean; registered: boolean; disabledSkills: string[] }>;
  /** Read a skill's SKILL.md content (v3 rust only). */
  readSkillContent(skillName: string, sessionId?: string): Promise<{ name: string; content: string }>;
  /** List the session's cron tasks (v3 rust only). */
  listCronTasks(sessionId: string): Promise<AppCronTask[]>;
  /** Delete one cron task by id (v3 rust only). */
  deleteCronTask(sessionId: string, taskId: string): Promise<{ deleted: true; taskId: string }>;
  /** Run one cron task now, leaving its schedule untouched (v3 rust only). */
  triggerCronTask(sessionId: string, taskId: string): Promise<{ triggered: true; taskId: string }>;
  /** List the user-level permission rules of the shared config snapshot
   *  (v3 rust only, B9 `approval.listRules`). */
  listPermissionRules(): Promise<AppPermissionRule[]>;
  /** Append one user-scope permission rule through the config.toml write
   *  pipeline (v3 rust only, B9 `approval.addRule`); answers the updated
   *  list. Same pattern+decision is an idempotent no-op; a conflicting
   *  decision for the same pattern answers invalid params. */
  addPermissionRule(rule: AppPermissionRuleInput): Promise<AppPermissionRule[]>;
  /** Remove one user-scope rule by {pattern, decision} (v3 rust only, B9
   *  `approval.removeRule`); answers the updated list. Unknown pair answers
   *  not found. */
  removePermissionRule(pattern: string, decision: AppPermissionRuleDecision): Promise<AppPermissionRule[]>;
  listTasks(
    sessionId: string,
    status?: AppTaskStatus,
    options?: { activeOnly?: boolean },
  ): Promise<AppTask[]>;
  /** Fetch one task's accumulated output tail through the single-task face
   *  (`task.output` — tail-capped preview string). Pollers use this instead
   *  of re-listing the session per task (S5). */
  getTaskOutput(sessionId: string, taskId: string, tailChars?: number): Promise<string>;
  getTask(sessionId: string, taskId: string, input?: { withOutput?: boolean; outputBytes?: number }): Promise<AppTask>;
  cancelTask(sessionId: string, taskId: string): Promise<{ cancelled: true }>;
  /** B7 workspace-aggregated task list (v3 rust only): every live session's
   *  tasks across the registered workspaces. Rows carry `sessionId` so the
   *  overview panel joins session titles client-side. */
  listWorkspaceTasks(input?: { activeOnly?: boolean; workspaceId?: string }): Promise<AppTask[]>;
  /** B7 foreground→background conversion (v3 rust only): addresses a model
   *  Bash tool call by its tool call id, or a user `!` shell task by task
   *  id. Null when the addressed task is unknown or already settled. */
  detachTask(
    sessionId: string,
    key: { toolCallId: string } | { taskId: string },
  ): Promise<AppTask | null>;
  listTerminals(sessionId: string): Promise<AppTerminal[]>;
  createTerminal(sessionId: string, input?: { cwd?: string; shell?: string; cols?: number; rows?: number }): Promise<AppTerminal>;
  getTerminal(sessionId: string, terminalId: string): Promise<AppTerminal>;
  closeTerminal(sessionId: string, terminalId: string): Promise<{ closed: true }>;
  listDirectory(sessionId: string, input: { path?: string; depth?: number; includeGitStatus?: boolean }): Promise<{ items: FsEntry[]; childrenByPath?: Record<string, FsEntry[]>; truncated: boolean }>;
  readFile(sessionId: string, input: { path: string; offset?: number; length?: number }): Promise<{ path: string; content: string; encoding: 'utf-8' | 'base64'; size: number; truncated: boolean; etag: string; mime: string; languageId?: string; lineCount?: number; isBinary: boolean }>;
  /** E9 structured file search. `sessionId` may be null in the draft state
   *  (no session created yet) — then `input.workspaceRoot` carries the
   *  workspace root and the engine's session-less arm searches it directly.
   *  `indexState` reports the aggregate index health of every searched root
   *  (older engines that omit it degrade to 'ready' in the adapter). */
  searchFiles(sessionId: string | null, input: { query: string; limit?: number; workspaceRoot?: string }): Promise<SearchFilesResult>;
  /** E9 pre-send reference validation (v3 rust only — `file.stat`): batch
   *  lstat probe of the given paths under the session's roots. A missing
   *  path answers an item with `exists: false` rather than an error. */
  statFiles(sessionId: string, paths: string[]): Promise<{ items: Array<{ path: string; exists: boolean; kind: string; readable: boolean; mtimeMs: number | null }> }>;
  grepFiles(sessionId: string, input: { pattern: string; regex?: boolean; caseSensitive?: boolean }): Promise<{ files: Array<{ path: string; matches: Array<{ line: number; col: number; text: string; before: string[]; after: string[] }> }>; filesScanned: number; truncated: boolean; elapsedMs: number }>;
  getGitStatus(sessionId: string, paths?: string[]): Promise<{ branch: string; ahead: number; behind: number; entries: Record<string, string>; additions: number; deletions: number; pullRequest: { number: number; state: string; url: string } | null }>;
  getFileDiff(sessionId: string, path: string): Promise<{ path: string; diff: string }>;
  getFileDownloadUrl(sessionId: string, path: string): string;
  /** Open a file in the user's editor / OS default app. `cwd` is the session's
   *  workspace root (when the caller knows it) — the desktop shell opens files
   *  outside any session scope, so tier 1 of the degrade chain resolves the
   *  absolute path with it. */
  openFile(sessionId: string, input: { path: string; line?: number; cwd?: string }): Promise<{ opened: true }>;
  /** Reveal a file in the system file manager (same `cwd` semantics). */
  revealFile(sessionId: string, input: { path: string; cwd?: string }): Promise<{ revealed: true }>;
  /** Open the session working directory (or a session-relative path) in an external application. */
  openInApp(sessionId: string, appId: string, path: string, line?: number, cwd?: string): Promise<void>;
  connectEvents(handlers: MirriEventHandlers): MirriEventConnection;
  /**
   * Bind terminal-stream sinks additively on the shared event transport.
   * Only backends with a SINGLE event connection implement it (the rust
   * engine): the returned connection shares the app's control-frame surface
   * and its close() releases ONLY the terminal sinks — it never tears down
   * the shared transport. Backends whose connectEvents() hands out a private
   * connection per call (the v1/v2 TS daemon) don't implement it; callers
   * fall back to connectEvents() there.
   */
  connectTerminalEvents?(sinks: TerminalEventSinks): MirriEventConnection;
  /** Eagerly connect the app-facing transport when the backend has a
   *  dedicated one (the rust engine's WsRpcApi opens its RPC WebSocket).
   *  Optional on purpose: the REST backends (v1/v2 TS daemon) have no
   *  transport to connect, so callers must tolerate its absence. */
  connectTransport?(): Promise<void>;

  // Workspaces + daemon folder browser. /workspaces now ships and includes
  // derived workspaces (cwds with sessions that were never explicitly registered).
  listWorkspaces(): Promise<AppWorkspace[]>;
  addWorkspace(input: { root: string; name?: string }): Promise<AppWorkspace>;
  updateWorkspace(id: string, input: { name: string }): Promise<AppWorkspace>;
  /**
   * Set a workspace's trust state (W8, the rust engine's trust gate):
   * trusting loads its project-level config (MCP servers + the AGENTS.md
   * project section); untrusting disconnects the project-level MCP
   * connections immediately. Resolves with the new trust state.
   */
  setWorkspaceTrust(id: string, trusted: boolean): Promise<{ trusted: boolean }>;
  /** Unregister a workspace. `mode` defaults to `'remove'` (data preserved);
   *  `'purge'` deletes the workspace's sessions and bucket data — the rust
   *  engine's three-choice face (the TS daemon ignores it: remove-only). */
  deleteWorkspace(id: string, mode?: WorkspaceDeleteMode): Promise<WorkspaceDeletionResult>;
  /** Re-sync a workspace's sessions from disk into the index — POST /workspaces/{id}/refresh (sidebar manual refresh). */
  refreshWorkspace(workspaceId: string): Promise<{ refreshed: true; inserted: number; removed: number }>;
  browseFs(path?: string): Promise<FsBrowseResult>;
  getFsHome(): Promise<{ home: string; recentRoots: string[] }>;

  // PRESUMED — not in current daemon docs; isolated in adapter, swap when backend defines them.
  listModels(): Promise<AppModel[]>;
  listProviders(): Promise<AppProvider[]>;
  /** v2 manual provider-create: explicit id + wire type; the models list may
   *  be empty (models are added later from the provider detail panel). */
  addProvider(input: AppAddProviderInput): Promise<AppProvider>;
  /** v2 catalog import: `POST /providers:import_catalog`. Importing an id
   *  that already exists appends only catalog models that are neither
   *  configured nor tombstoned — existing aliases are never rewritten and
   *  user-deleted models are never resurrected. */
  importCatalogProvider(input: { catalogId: string; id?: string; apiKey?: string; baseUrl?: string; excludeModelIds?: string[] }): Promise<{ provider: AppProvider; modelsImported: number }>;
  deleteProvider(id: string): Promise<{ deleted: true }>;
  refreshProvider(id: string): Promise<ProviderRefreshResult>;
  refreshAllProviders(): Promise<ProviderRefreshResult>;
  /** Delete model aliases from config — a single call carries the whole
   *  batch (one write transaction on the engine, ADR-005). */
  deleteModel(modelIds: string[]): Promise<{ deleted: true }>;
  /** Browse the remote models.dev catalog (server-proxied). Returns
   *  CATALOG_UNAVAILABLE-aware: throws on catalog fetch failure so the caller
   *  can fall back to the manual add-provider form. */
  listCatalogProviders(): Promise<AppCatalogProvider[]>;

  // File upload / download
  uploadFile(input: { file: Blob; name?: string }): Promise<{ id: string; name: string; mediaType: string; size: number }>;
  getFileUrl(fileId: string): string;
  /** Fetch a file's bytes with auth — feed the resulting Blob to a blob URL for <video>/<img> src. */
  getFileBlob(fileId: string): Promise<Blob>;

  // Config — REAL endpoints
  getConfig(): Promise<AppConfig>;
  setConfig(patch: Omit<Partial<AppConfig>, 'models' | 'providers' | 'disabledAgents'> & { models?: Record<string, Partial<AppModelAlias>>; providers?: Record<string, Partial<AppConfigProvider>> }): Promise<AppConfig>;

  // Provider credentials — the credential-store face (provider-credential
  // spec §5.3): list carries no plaintext, set is write-only, remove /
  // status read the per-provider view. `credential.updated` is the change
  // signal subscribers re-read `credentialList` on.
  credentialList(providerId?: string): Promise<ProviderCredentialInfo[]>;
  credentialSet(providerId: string, value: ProviderCredentialValue): Promise<void>;
  credentialRemove(providerId: string): Promise<void>;
  credentialStatus(providerId: string): Promise<ProviderCredentialStatus>;
  onCredentialUpdated(handler: (providerId: string | undefined) => void): () => void;

  // Agent profiles — REAL endpoints
  listAgents(): Promise<AppAgentProfileList>;
  getAgent(name: string): Promise<AppAgentProfile | undefined>;
  createAgent(data: AppAgentProfileInput): Promise<AppAgentProfile>;
  updateAgent(name: string, data: AppAgentProfileUpdate): Promise<AppAgentProfile>;
  deleteAgent(name: string): Promise<void>;
  enableAgent(name: string): Promise<void>;
  disableAgent(name: string): Promise<void>;
  resetAgent(name: string): Promise<void>;
  listTools(): Promise<AppToolDescriptor[]>;
  listToolsCatalog(): Promise<AppToolDescriptor[]>;
  // Global MCP (session-independent)
  listGlobalMcpServers(): Promise<AppMcpServer[]>;
  /** Fetch the global MCP config content in both views (source literal + resolved expanded). */
  getGlobalMcpConfig(): Promise<AppMcpGlobalConfigEntry[]>;
  listGlobalMcpTools(): Promise<AppToolDescriptor[]>;
  createMcpServer(name: string, config: AppMcpServerConfig): Promise<void>;
  updateMcpServer(name: string, config: AppMcpServerConfig): Promise<void>;
  deleteMcpServer(name: string): Promise<void>;
  reloadMcpServers(): Promise<AppMcpSyncSummary>;
  enableMcpServer(serverId: string): Promise<void>;
  disableMcpServer(serverId: string): Promise<void>;
  enableGlobalMcpTool(serverName: string, toolName: string): Promise<void>;
  disableGlobalMcpTool(serverName: string, toolName: string): Promise<void>;
  /** Write the complete per-server denylist in one call (bulk toggles —
   *  one RPC, one reconcile; no per-tool read-modify-write race). */
  setMcpToolFilter(serverName: string, disabledTools: string[], enabledTools?: string[]): Promise<void>;
  getGlobalMcpToggleState(): Promise<{ disabledServers: string[]; disabledTools: string[] }>;
  /** Read-only tool snapshot for a disabled server (the persisted filter
   *  arrays — an allowlist is the known list with all entries enabled, a
   *  denylist marks its members disabled; empty when no preference is
   *  stored). The WS-RPC backend derives it from mcp.config.list; older
   *  engines answer empty and the modal falls back to its empty state. */
  getDisabledServerToolSnapshot(serverName: string): Promise<{ tools: Array<{ name: string; enabled: boolean }> }>;
  testConnectMcpServer(config: AppMcpServerConfig): Promise<AppMcpTestResult>;
  connectMcpServer(name: string): Promise<{ server: AppMcpServer; tools: AppToolDescriptor[] }>;

  // Auth — REAL endpoints
  getAuth(): Promise<{
    ready: boolean;
    providersCount: number;
    defaultModel: string | null;
  }>;
}
