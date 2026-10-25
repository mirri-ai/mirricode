// apps/mirri-web/src/api/daemon/wire.ts
// Daemon wire DTOs — ALL fields stay snake_case as they appear on the wire.
// No camelCase conversions here; that is mappers.ts's job.

import type { AppMessageContent } from '../types';

// ---------------------------------------------------------------------------
// Envelope & Page
// ---------------------------------------------------------------------------

export interface WireEnvelope<T> {
  code: number;
  msg: string;
  data: T | null;
  request_id: string;
  details?: unknown;
}

export interface WirePage<T> {
  items: T[];
  has_more: boolean;
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

export type WireSessionStatus =
  | 'idle'
  | 'running'
  | 'awaiting_approval'
  | 'awaiting_question'
  | 'aborted';

/** The first LLM call's four-bucket usage — the cold-start baseline the
 *  "after cold start" hit metric subtracts (SDR-004). */
export interface WireFirstCallUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
}

export interface WireSessionUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  total_cost_usd: number;
  context_tokens: number;
  context_limit: number;
  turn_count: number;
  /** Optional: only engines that persist the first call report it. */
  first_call?: WireFirstCallUsage;
}

export interface WireSessionUsageDelta {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  cost_usd: number;
}

export interface WirePermissionRule {
  id: string;
  tool_name: string;
  matcher?: {
    kind: 'command_prefix' | 'path_glob' | 'exact_input' | 'always';
    value?: string;
  };
  decision: 'approved';
  created_at: string;
  created_by: 'user' | 'agent';
}

export interface WireSession {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  /** SDR-014: the recency-sort key (only user-interaction bumps). The rust
   *  bridge's session rows may carry the camelCase spelling `lastUsedAt`
   *  instead; the mapper accepts both. */
  last_used_at?: string;
  busy: boolean;
  main_turn_active?: boolean;
  pending_interaction?: 'none' | 'approval' | 'question';
  last_turn_reason?: 'completed' | 'cancelled' | 'failed' | 'stalled';
  archived: boolean;
  /** Whether the session's wire log is gzip-compressed on disk (the
   *  session-retention domain's shape verdict — the "已压缩" badge's input).
   *  Optional: older engines omit it and the client maps it to false. */
  compressed?: boolean;
  /** When the wire log was compressed (RFC 3339), when recorded. */
  compressed_at?: string;
  current_prompt_id?: string;
  /** Text of the most recent user prompt, for search/preview. */
  last_prompt?: string;
  // PRESUMED — daemon adds this once it ships the workspace registry; until then
  // it is absent and the client maps sessions by metadata.cwd === workspace.root.
  workspace_id?: string;
  metadata: {
    cwd: string;
    [key: string]: unknown;
  };
  agent_config: {
    model: string;
    system_prompt?: string;
    tools?: string[];
    mcp_servers?: string[];
    // Runtime controls — optional on read (the daemon may not backfill them;
    // live values come from GET /sessions/{id}/status).
    thinking?: string;
    permission_mode?: string;
    plan_mode?: boolean;
    swarm_mode?: boolean;
    collaboration_mode?: boolean;
    goal_objective?: string;
    goal_control?: 'pause' | 'resume' | 'cancel';
  };
  usage: WireSessionUsage;
  permission_rules: WirePermissionRule[];
  message_count: number;
  last_seq: number;
  /** Absolute path to the session's physical storage directory.
   *  Optional — older servers may not include it. */
  session_dir?: string;
}

// ---------------------------------------------------------------------------
// Session retention (Task 11 / Epic RT3) — the `retention.*` RPC wire types.
// The web app re-implements these locally (the no-@mirri-ai/agent-core rule);
// the authoritative shapes live in rust/crates/mirri-proto/src/retention.rs.
// Params follow the proto's snake_case-first dialect (the embedded handlers
// deserialize the plain snake_case spellings), results are the proto's
// `rename_all = "snake_case"` wire forms.
// ---------------------------------------------------------------------------

/** `retention.list` params: which management view to serve. */
export interface WireRetentionListParams {
  include: 'archived' | 'compressed';
}

/** One `retention.list` row — a session as the management page sees it.
 *  Every derived field (sizes, compressed days) is computed SERVER-side. */
export interface WireRetentionListEntry {
  session_id: string;
  title: string;
  workspace: string;
  /** RFC 3339; null only on rows whose legacy state predates the stamp. */
  archived_at: string | null;
  /** RFC 3339; null on uncompressed rows. */
  compressed_at: string | null;
  /** The session's current on-disk occupancy in bytes (server-computed). */
  current_size_bytes: number;
  /** The compression savings so far; null on never-compressed rows. */
  saved_bytes: number | null;
  /** Whole days the wire log has been compressed (server clock); null on
   *  uncompressed rows. */
  compressed_days: number | null;
}

/** `retention.list` result. */
export interface WireRetentionListResult {
  items: WireRetentionListEntry[];
}

/** One per-tier delete estimate — the confirmation dialog's "N sessions /
 *  X MB" for one tier button. */
export interface WireRetentionTierEstimate {
  days: number;
  count: number;
  bytes: number;
}

/** `retention.stats` result — the header counters plus the four per-tier
 *  estimates (30/90/180/365, ascending; empty on legacy payloads). */
export interface WireRetentionStatsResult {
  archived_count: number;
  compressed_count: number;
  compressed_total_bytes: number;
  reclaimed_potential: number;
  tier_days?: WireRetentionTierEstimate[];
}

/** `retention.compressNow` params / result. */
export interface WireRetentionCompressNowParams {
  session_id: string;
}
export interface WireRetentionCompressNowResult {
  ok: boolean;
}

/** `retention.delete` params / result. */
export interface WireRetentionDeleteParams {
  session_id: string;
}
export interface WireRetentionDeleteResult {
  ok: boolean;
  /** Bytes the deletion reclaimed (session directory + exclusive
   *  attachments). */
  reclaimed_bytes: number;
}

/** `retention.deleteTiered` params / result — the tier-button batch. */
export interface WireRetentionDeleteTieredParams {
  older_than_days: number;
}
export interface WireRetentionDeleteTieredResult {
  deleted: string[];
  failed: Array<{ session_id: string; error: string }>;
  reclaimed_bytes: number;
}

/** `retention.getPolicy` result — the resolved `[retention]` knobs the
 *  strategy hint (US3.5) renders. */
export interface WireRetentionPolicyResult {
  enabled: boolean;
  archive_grace_days: number;
  interval_hours: number;
}

// GET /sessions/{id}/status — live runtime state, aligned with TUI /status.
export interface WireSessionRuntimeStatus {
  model?: string;
  thinking_level: string;
  permission: string;
  plan_mode: boolean;
  swarm_mode: boolean;
  /** Optional — additive P-next field; older engines omit it. */
  collaboration_mode?: boolean;
  context_tokens: number;
  max_context_tokens: number;
  context_usage: number;
}

// GET /sessions/{id}/goal — camelCase, same shape as the `goal.updated` event
// payload. The endpoint returns null when no goal is active.
export interface WireGoalSnapshot {
  goalId: string;
  objective: string;
  completionCriterion?: string;
  status: 'active' | 'paused' | 'blocked' | 'complete';
  turnsUsed: number;
  tokensUsed: number;
  wallClockMs: number;
  terminalReason?: string;
  budget: {
    tokenBudget: number | null;
    turnBudget: number | null;
    wallClockBudgetMs: number | null;
    remainingTokens: number | null;
    remainingTurns: number | null;
    remainingWallClockMs: number | null;
    tokenBudgetReached: boolean;
    turnBudgetReached: boolean;
    wallClockBudgetReached: boolean;
    overBudget: boolean;
  };
}

// GET /sessions/{id}/warnings — session-level warnings (e.g. oversized AGENTS.md).
export interface WireSessionWarning {
  code: string;
  message: string;
  severity: 'info' | 'warning' | 'error';
}

export interface WireSessionWarningsResponse {
  warnings: WireSessionWarning[];
}

// ---------------------------------------------------------------------------
// Workspace + daemon folder browser wire DTOs
// PRESUMED — not in the live daemon yet; isolated here, swap when backend ships.
// ---------------------------------------------------------------------------

export interface WireWorkspace {
  id: string;
  root: string;
  name: string;
  is_git_repo: boolean;
  branch: string | null;
  last_opened_at?: string;
  session_count: number;
}

/**
 * The rust engine's workspace object in a lifecycle event — the camelCase
 * dialect (`workspace.list` / `createOrTouch` results carry the same
 * spelling). The snapshot is identity-shaped (`id`/`root`/`name`); the
 * optional view fields are present when the emitting engine had them.
 */
export interface WireRustWorkspace {
  id: string;
  root: string;
  name: string;
  isGitRepo?: boolean;
  branch?: string | null;
  lastOpenedAt?: string;
  sessionCount?: number;
  /** The live trust verdict (W8, the rust engine's wire). */
  trusted?: boolean;
}

export interface WireFsBrowseEntry {
  name: string;
  path: string;
  is_dir: boolean;
  is_git_repo: boolean;
  branch?: string;
}

export interface WireFsBrowseResult {
  path: string;
  parent: string | null;
  entries: WireFsBrowseEntry[];
}

export interface WireFsHomeResult {
  home: string;
  recent_roots: string[];
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

export type WireMessageContent =
  | { type: 'text'; text: string }
  | { type: 'tool_use'; tool_call_id: string; tool_name: string; input: unknown }
  | { type: 'tool_result'; tool_call_id: string; output: unknown; is_error?: boolean }
  | { type: 'image'; source: WireImageSource }
  | { type: 'video'; source: WireImageSource }
  | { type: 'file'; file_id: string; name: string; media_type: string; size: number }
  | { type: 'thinking'; thinking: string; signature?: string };

export type WireImageSource =
  | { kind: 'url'; url: string }
  | { kind: 'base64'; media_type: string; data: string }
  | { kind: 'file'; file_id: string };

export interface WireMessage {
  id: string;
  session_id: string;
  role: 'user' | 'assistant' | 'tool' | 'system';
  content: WireMessageContent[];
  created_at: string;
  prompt_id?: string;
  parent_message_id?: string;
  metadata?: Record<string, unknown>;
  /** Activated skills recorded on the rewritten body message (ADR-006). */
  with_skills?: Array<{ name: string; target: string; trigger: string }>;
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

export interface WirePromptSubmission {
  content: WireMessageContent[];
  metadata?: Record<string, unknown>;
  agent_id?: string;
  model?: string;
  thinking?: string;
  permission_mode?: string;
  plan_mode?: boolean;
  swarm_mode?: boolean;
  collaboration_mode?: boolean;
  goal_objective?: string;
  goal_control?: 'pause' | 'resume' | 'cancel';
}

export interface WirePromptSubmitResult {
  prompt_id: string;
  user_message_id: string;
  /** 'running' = started immediately; 'queued' = parked behind the active prompt. */
  status?: 'running' | 'queued';
}

/** One entry of the daemon prompt queue (`GET /sessions/{id}/prompts`). */
export interface WirePromptItem {
  prompt_id: string;
  user_message_id: string;
  /** 'running' = started immediately; 'queued' = parked behind the active prompt. */
  status: 'running' | 'queued' | 'blocked';
  content: AppMessageContent[];
  created_at: string;
}

export interface WirePromptSteerResult {
  steered: boolean;
  prompt_ids: string[];
}

// ---------------------------------------------------------------------------
// Approval
// ---------------------------------------------------------------------------

export interface WireApprovalRequest {
  approval_id: string;
  session_id: string;
  turn_id?: number;
  tool_call_id: string;
  tool_name: string;
  action: string;
  /** ToolInputDisplay — 12 discriminated kinds; client falls back to generic.
      The daemon protocol field is `tool_input_display` (protocol/approval.ts);
      `display` is the stub daemon's older shape, kept for compatibility. */
  tool_input_display?: unknown;
  display?: unknown;
  /** Honest expiry (D-13): the rust engine never derives one, so the field
   *  is optional on the wire — absent means "no countdown". */
  expires_at?: string;
  /** The requesting member's full agent id (`agent-<uuid>`, supervision B1)
   *  — present only on member-originated approvals; the main agent's rows
   *  omit it (single-agent sessions stay unchanged). */
  agent_id?: string;
  created_at: string;
}

export interface WireApprovalResponse {
  decision: 'approved' | 'rejected' | 'cancelled';
  scope?: 'session';
  feedback?: string;
  selected_label?: string;
}

// ---------------------------------------------------------------------------
// Question
// ---------------------------------------------------------------------------

export interface WireQuestionOption {
  id: string;
  label: string;
  description?: string;
  recommended?: boolean;
  is_recommended?: boolean;
}

export interface WireQuestionItem {
  id: string;
  question: string;
  header?: string;
  body?: string;
  options: WireQuestionOption[];
  multi_select?: boolean;
  /** Klient camelCase spelling (rust engine rows) — read alongside the
   *  snake_case form above; both map to the app's `multiSelect`. */
  multiSelect?: boolean;
  allow_other?: boolean;
  /** Klient camelCase spelling (rust engine rows). */
  allowOther?: boolean;
  other_label?: string;
  /** Klient camelCase spelling (rust engine rows). */
  otherLabel?: string;
  other_description?: string;
  /** Klient camelCase spelling (rust engine rows). */
  otherDescription?: string;
}

export interface WireQuestionRequest {
  question_id: string;
  session_id: string;
  turn_id?: number;
  tool_call_id?: string;
  questions: WireQuestionItem[];
  created_at: string;
}

export type WireQuestionAnswer =
  | { kind: 'single'; option_id: string }
  | { kind: 'multi'; option_ids: string[] }
  | { kind: 'other'; text: string }
  | { kind: 'multi_with_other'; option_ids: string[]; other_text: string }
  | { kind: 'skipped' };

export interface WireQuestionResponse {
  answers: Record<string, WireQuestionAnswer>;
  method?: 'enter' | 'space' | 'number_key' | 'click';
  note?: string;
}

// ---------------------------------------------------------------------------
// Background Task
// ---------------------------------------------------------------------------

export type WireTaskStatus = 'running' | 'completed' | 'failed' | 'cancelled';

export interface WireBackgroundTask {
  id: string;
  session_id: string;
  kind: 'subagent' | 'bash' | 'tool';
  description: string;
  status: WireTaskStatus;
  command?: string;
  created_at: string;
  started_at?: string;
  completed_at?: string;
  output_preview?: string;
  output_bytes?: number;
  // 词汇与应用侧 AppSubagentPhase 对齐（ADR-008 ①/③/ADR-011）：awaiting/
  // backoff/aborted 容忍读——旧后端只发 queued/working/suspended/completed/
  // failed，新后端同面可携带扩展词汇。
  subagent_phase?:
    | 'queued'
    | 'working'
    | 'suspended'
    | 'awaiting'
    | 'backoff'
    | 'completed'
    | 'failed'
    | 'aborted';
  subagent_type?: string;
  parent_tool_call_id?: string;
  suspended_reason?: string;
  swarm_index?: number;
  run_in_background?: boolean;
  model?: string;
  /** The subagent's agent id (subagent-kind tasks) — joins this REST row to
   *  the WS `subagent.*` row keyed by the same agent id, so completion events
   *  can terminate the dock (REST) row too. */
  agent_id?: string;
}

// ---------------------------------------------------------------------------
// File System
// ---------------------------------------------------------------------------

export type WireFsKind = 'file' | 'directory' | 'symlink';

export interface WireFsEntry {
  path: string;
  name: string;
  kind: WireFsKind;
  size?: number;
  modified_at: string;
  etag?: string;
  mime?: string;
  language_id?: string;
  is_binary?: boolean;
  is_symlink_to?: string;
  git_status?: string;
  child_count?: number;
}

// ---------------------------------------------------------------------------
// Model + Provider wire DTOs
// PRESUMED — not in current daemon docs; isolated here, swap when backend defines them.
// ---------------------------------------------------------------------------

export interface WireModel {
  provider: string;
  model: string;
  display_name?: string;
  max_context_size: number;
  capabilities?: string[];
  support_efforts?: string[];
  default_effort?: string;
}

export interface WireProvider {
  id: string;
  type: string;
  base_url?: string;
  default_model?: string;
  has_api_key: boolean;
  api_key_display?: string | null;
  status: 'connected' | 'error' | 'unconfigured';
  models?: string[];
}

export interface WireProviderRefreshResult {
  changed: Array<{
    provider_id: string;
    provider_name: string;
    added: number;
    removed: number;
  }>;
  unchanged: string[];
  failed: Array<{ provider: string; reason: string }>;
}

/** Wire protocols the v2 engine (kap-server) can resolve a catalog entry to. */
export type WireProviderWireType =
  | 'kimi'
  | 'openai'
  | 'openai_responses'
  | 'anthropic'
  | 'google-genai'
  | 'vertexai';

export interface WireCatalogModel {
  id: string;
  name?: string;
  max_context_size: number;
  capabilities?: string[];
  reasoning: boolean;
}

export interface WireCatalogProvider {
  id: string;
  name: string;
  wire_type?: WireProviderWireType | null;
  guessed: boolean;
  needs_base_url: boolean;
  rejected: boolean;
  reject_reason: string | null;
  env_key: string | null;
  models: WireCatalogModel[];
}

export interface WireCatalogProvidersResponse {
  items: WireCatalogProvider[];
}

export interface WireImportCatalogProviderResponse {
  provider: WireProvider;
  models_imported: number;
}

export interface WireAddProviderRequest {
  type: string;
  api_key?: string;
  base_url?: string;
  default_model?: string;
}

export interface WireConfigProvider {
  type: string;
  base_url?: string;
  default_model?: string;
  has_api_key: boolean;
  api_key_display?: string | null;
}

/** A model alias as stored in config.toml — the snake_case mirror of
 *  AppModelAlias. `overrides` survive catalog refreshes and win over
 *  top-level fields at runtime. */
export interface WireModelAlias {
  provider: string;
  model: string;
  max_context_size: number;
  max_output_size?: number;
  capabilities?: string[];
  display_name?: string;
  description?: string;
  reasoning_key?: string;
  protocol?: 'anthropic';
  adaptive_thinking?: boolean;
  support_efforts?: string[];
  default_effort?: string;
  beta_api?: boolean;
}

export interface WireConfig {
  providers: Record<string, WireConfigProvider>;
  default_provider?: string;
  default_model?: string;
  models?: Record<string, WireModelAlias>;
  thinking?: unknown;
  plan_mode?: boolean;
  yolo?: boolean;
  default_permission_mode?: string;
  default_plan_mode?: boolean;
  permission?: unknown;
  hooks?: unknown[];
  services?: unknown;
  merge_all_available_skills?: boolean;
  extra_skill_dirs?: string[];
  disabled_skills?: string[];
  extra_agent_dirs?: string[];
  disabled_agents?: string[];
  loop_control?: unknown;
  background?: unknown;
  experimental?: Record<string, boolean>;
  telemetry?: boolean;
  raw?: Record<string, unknown>;
}

export interface WireAgentProfile {
  name: string;
  description?: string;
  source: 'builtin' | 'user' | 'project' | 'extra' | 'plugin' | 'explicit';
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  has_override: boolean;
  file_path?: string;
  default_model?: string;
  tools?: string[];
  disallowed_tools?: string[];
  /** Capability tags the profile requires, e.g. ["code.read", "web.search"]. */
  capabilities_required?: string[];
  subagents?: string[];
  suppressed_candidates?: { source: 'builtin' | 'user' | 'project' | 'extra' | 'plugin' | 'explicit'; priority: number; reason: string }[];
  when_to_use?: string;
  system_prompt_template?: string;
  /** ADR-010 ① (US-A4 form disclosure): the body's composition form. */
  prompt_form?: 'composed' | 'replacement';
  /** D21 / US-A8: SYSTEM.md override active — the essential row only. */
  base_prompt_override?: boolean;
  /** D21 / US-A8: the raw SYSTEM.md text when the override exists. */
  base_prompt_template?: string;
  /**
   * The builtin default base prompt template's raw text (placeholders
   * intact) — the essential row only, the editor's no-override seed.
   */
  base_prompt_default_template?: string;
  effective_system_prompt?: string;
  prompt_vars?: Record<string, string>;
}

/** `GET /agents` response — items plus agent files skipped during discovery. */
export interface WireAgentProfileList {
  items: WireAgentProfile[];
  skipped?: { path: string; reason: string }[];
}

export interface WireToolDescriptor {
  name: string;
  description: string;
  input_schema: unknown;
  source: 'builtin' | 'skill' | 'mcp';
  mcp_server_id?: string;
}

export interface WireMcpServer {
  id: string;
  name: string;
  transport: 'stdio' | 'http' | 'sse';
  status: 'connected' | 'connecting' | 'disconnected' | 'error';
  last_error?: string;
  tool_count: number;
  config?: Record<string, unknown>;
}

/**
 * Config-content view of a global MCP server (`GET /mcp/global/config`):
 * `source` is the user-written literal (environment-variable references kept
 * verbatim), `resolved` is the env-expanded effective config that the runtime
 * actually uses.
 */
export interface WireMcpGlobalConfigEntry {
  name: string;
  source: Record<string, unknown>;
  resolved: Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Auth wire DTOs — REAL endpoints (GET /api/v1/auth)
// ---------------------------------------------------------------------------

export interface WireAuthResult {
  ready: boolean;
  providers_count: number;
  default_model: string | null;
}

// ---------------------------------------------------------------------------
// File upload wire DTOs
// ---------------------------------------------------------------------------

export interface WireFileMeta {
  id: string;
  name: string;
  media_type: string;
  size: number;
  created_at: string;
  expires_at?: string;
}

// ---------------------------------------------------------------------------
// WS Server frames (S→C)
// ---------------------------------------------------------------------------

/** All typed server-to-client WS frames */
export type WireServerFrame =
  | WireServerHello
  | WireAck
  | WirePing
  | WireResyncRequired
  | WireErrorFrame
  | WireEvent;

export interface WireServerHello {
  type: 'server_hello';
  timestamp: string;
  payload: {
    server_id: string;
    heartbeat_ms: number;
    max_event_buffer_size: number;
    capabilities: {
      event_batching: boolean;
      compression: boolean;
    };
  };
}

export interface WireAck {
  type: 'ack';
  id: string;
  code: number;
  msg: string;
  payload: unknown;
}

export interface WirePing {
  type: 'ping';
  timestamp: string;
  payload: { nonce: string };
}

export interface WireResyncRequired {
  type: 'resync_required';
  timestamp: string;
  payload: {
    session_id: string;
    reason: 'buffer_overflow' | 'session_recreated' | 'epoch_changed';
    current_seq: number;
    /** Current journal epoch — adopt it after resyncing (v2 sync protocol). */
    epoch?: string;
  };
}

// ---------------------------------------------------------------------------
// v2 sync protocol: cursors + session snapshot
// ---------------------------------------------------------------------------

/** Per-session sync cursor: durable seq + journal epoch. */
export interface WireSessionCursor {
  seq: number;
  epoch?: string;
}

export interface WireInFlightToolCall {
  tool_call_id: string;
  name: string;
  args?: unknown;
  description?: string;
  display?: unknown;
  last_progress?: {
    kind: 'stdout' | 'stderr' | 'progress' | 'status' | 'custom';
    text?: string;
    percent?: number;
  };
}

export interface WireInFlightTurn {
  turn_id: number;
  assistant_text: string;
  thinking_text: string;
  running_tools: WireInFlightToolCall[];
  current_prompt_id?: string;
}

/** `GET /sessions/{sid}/snapshot` — atomic rebuild state at a watermark. */
export interface WireSessionSnapshot {
  as_of_seq: number;
  epoch: string;
  session: WireSession;
  messages: { items: WireMessage[]; has_more: boolean };
  in_flight_turn: WireInFlightTurn | null;
  /** Live subagent roster at the watermark (absent on older servers). */
  subagents?: WireBackgroundTask[];
  pending_approvals: WireApprovalRequest[];
  pending_questions: WireQuestionRequest[];
}

export interface WireSessionAbortResult {
  aborted: boolean;
}

export interface WireErrorFrame {
  type: 'error';
  timestamp: string;
  payload: {
    code: number;
    msg: string;
    fatal: boolean;
    request_id?: string;
    details?: unknown;
  };
}

// ---------------------------------------------------------------------------
// WS Client control messages (C→S)
// ---------------------------------------------------------------------------

export type WireClientControl =
  | WireClientHello
  | WireSubscribe
  | WireUnsubscribe
  | WireAbort
  | WirePong;

export interface WireClientHello {
  type: 'client_hello';
  id: string;
  payload: {
    client_id: string;
    subscriptions: string[];
    cursors?: Record<string, WireSessionCursor>;
  };
}

export interface WireSubscribe {
  type: 'subscribe';
  id: string;
  payload: {
    session_ids: string[];
    cursors?: Record<string, WireSessionCursor>;
  };
}

export interface WireUnsubscribe {
  type: 'unsubscribe';
  id: string;
  payload: { session_ids: string[] };
}

export interface WireAbort {
  type: 'abort';
  id: string;
  payload: {
    session_id: string;
    prompt_id: string;
  };
}

export interface WirePong {
  type: 'pong';
  payload: { nonce: string };
}

// ---------------------------------------------------------------------------
// WS Events (S→C) — all type: "event.*"
// ---------------------------------------------------------------------------

/** Base shape for all WS event frames */
interface WireEventBase<T extends string, P> {
  type: T;
  seq: number;
  session_id: string;
  timestamp: string;
  payload: P;
}

// Session lifecycle
type WireEventSessionCreated = WireEventBase<'event.session.created', { session: WireSession }>;
type WireEventSessionUpdated = WireEventBase<'event.session.updated', { session: WireSession; changed_fields: string[] }>;
type WireEventSessionDeleted = WireEventBase<'event.session.deleted', { session_id: string }>;
type WireEventSessionWorkChanged = WireEventBase<'event.session.work_changed', {
  busy: boolean;
  main_turn_active?: boolean;
  pending_interaction?: 'none' | 'approval' | 'question';
  last_turn_reason?: 'completed' | 'cancelled' | 'failed' | 'stalled';
}>;
/** @deprecated Old journals may still carry this; mapped onto busy for replay. */
type WireEventSessionStatusChanged = WireEventBase<'event.session.status_changed', {
  status: WireSessionStatus;
  previous_status: WireSessionStatus;
  current_prompt_id?: string;
}>;
type WireEventSessionUsageUpdated = WireEventBase<'event.session.usage_updated', {
  usage: WireSessionUsage;
  delta: WireSessionUsageDelta;
}>;
type WireEventSessionHistoryCompacted = WireEventBase<'event.session.history_compacted', {
  before_seq: number;
  reason: 'auto_compact' | 'manual_compact' | 'history_rewrite';
  summary_message_id?: string;
}>;

// Workspace lifecycle (global — not session-scoped). The payload workspace
// carries either dialect: the TS daemon's snake_case spelling (presumed
// v1) or the rust engine's camelCase spelling (`WireRustWorkspace`).
type WireEventWorkspaceCreated = WireEventBase<'event.workspace.created', { workspace: WireWorkspace | WireRustWorkspace }>;
type WireEventWorkspaceUpdated = WireEventBase<'event.workspace.updated', { workspace: WireWorkspace | WireRustWorkspace }>;
type WireEventWorkspaceDeleted = WireEventBase<'event.workspace.deleted', { workspace_id: string; root: string } | { workspaceId: string; root: string }>;

// Message lifecycle
type WireEventMessageCreated = WireEventBase<'event.message.created', { message: WireMessage }>;
type WireEventMessageUpdated = WireEventBase<'event.message.updated', {
  message_id: string;
  content: WireMessageContent[];
  status: 'pending' | 'completed' | 'error';
}>;

// Assistant streaming
type WireEventAssistantDelta = WireEventBase<'event.assistant.delta', {
  message_id: string;
  content_index: number;
  delta: { text?: string; thinking?: string };
}>;
// No-op-but-known streaming events (advance lastSeq, no UI change)
type WireEventAssistantToolUseStarted = WireEventBase<'event.assistant.tool_use_started', {
  message_id: string;
  tool_call_id: string;
  tool_name: string;
  content_index: number;
}>;
type WireEventAssistantToolUseDelta = WireEventBase<'event.assistant.tool_use_delta', {
  message_id: string;
  tool_call_id: string;
  input_delta: string;
}>;
type WireEventAssistantToolUseCompleted = WireEventBase<'event.assistant.tool_use_completed', {
  message_id: string;
  tool_call_id: string;
  input: unknown;
}>;
type WireEventAssistantCompleted = WireEventBase<'event.assistant.completed', {
  message_id: string;
  finish_reason: 'stop' | 'tool_use' | 'length' | 'cancelled' | 'error';
}>;

// Tool execution (no-op-but-known)
type WireEventToolStarted = WireEventBase<'event.tool.started', {
  tool_call_id: string;
  tool_name: string;
  input: unknown;
  parent_message_id: string;
}>;
type WireEventToolOutput = WireEventBase<'event.tool.output', {
  tool_call_id: string;
  chunk: string;
  stream: 'stdout' | 'stderr';
}>;
type WireEventToolProgress = WireEventBase<'event.tool.progress', {
  tool_call_id: string;
  progress: number;
  message?: string;
}>;
type WireEventToolCompleted = WireEventBase<'event.tool.completed', {
  tool_call_id: string;
  output: unknown;
  is_error: boolean;
  duration_ms: number;
}>;

// Approval
type WireEventApprovalRequested = WireEventBase<'event.approval.requested', WireApprovalRequest>;
type WireEventApprovalResolved = WireEventBase<'event.approval.resolved', {
  approval_id: string;
  decision: 'approved' | 'rejected' | 'cancelled';
  scope?: 'session';
  feedback?: string;
  selected_label?: string;
  resolved_by: string;
  resolved_at: string;
}>;
type WireEventApprovalExpired = WireEventBase<'event.approval.expired', { approval_id: string }>;

// Question
type WireEventQuestionRequested = WireEventBase<'event.question.requested', WireQuestionRequest>;
type WireEventQuestionAnswered = WireEventBase<'event.question.answered', {
  question_id: string;
  answers: Record<string, WireQuestionAnswer>;
  method?: string;
  note?: string;
  resolved_by: string;
  resolved_at: string;
}>;
type WireEventQuestionDismissed = WireEventBase<'event.question.dismissed', {
  question_id: string;
  dismissed_by: string;
  dismissed_at: string;
}>;
// Background tasks
type WireEventTaskCreated = WireEventBase<'event.task.created', { task: WireBackgroundTask }>;
type WireEventTaskProgress = WireEventBase<'event.task.progress', {
  task_id: string;
  output_chunk: string;
  stream: 'stdout' | 'stderr';
}>;
type WireEventTaskCompleted = WireEventBase<'event.task.completed', {
  task_id: string;
  status: WireTaskStatus;
  output_preview?: string;
  output_bytes?: number;
}>;

type WireEventConfigChanged = WireEventBase<'event.config.changed', {
  changed_fields: string[];
  config: WireConfig;
}>;

type WireEventAgentProfilesChanged = WireEventBase<'event.agent.profiles_changed', Record<string, never>>;

type WireEventModelCatalogChanged = WireEventBase<'event.model_catalog.changed', {
  changed: Array<{
    provider_id: string;
    provider_name: string;
    added: number;
    removed: number;
  }>;
  unchanged: string[];
  failed: Array<{ provider: string; reason: string }>;
}>;

/** Rust core MCP pool transition (mirri-core event_projection — payload
 *  camelCase, unlike most snake_case protocol events). */
type WireEventMcpServerStatus = WireEventBase<'event.mcp.server.status', {
  server: {
    name: string;
    transport: string;
    status: string;
    toolCount: number;
    pool: string;
    error?: string;
    changeKind?: string;
  };
}>;

/** Catch-all for unrecognised event frames — keeps lastSeq advancing without warnings */
type WireEventUnknown = { type: string; seq: number; session_id: string; timestamp: string; payload: unknown };

/**
 * Union of all WS event frames the client will process.
 * Visible events (UI updates) + no-op-but-known events (lastSeq only).
 * The catch-all at the end handles future server events gracefully.
 */
export type WireEvent =
  // Session lifecycle
  | WireEventSessionCreated
  | WireEventSessionUpdated
  | WireEventSessionDeleted
  | WireEventSessionWorkChanged
  | WireEventSessionStatusChanged
  | WireEventSessionUsageUpdated
  | WireEventSessionHistoryCompacted
  // Workspace lifecycle
  | WireEventWorkspaceCreated
  | WireEventWorkspaceUpdated
  | WireEventWorkspaceDeleted
  // Message lifecycle
  | WireEventMessageCreated
  | WireEventMessageUpdated
  // Assistant streaming
  | WireEventAssistantDelta
  | WireEventAssistantToolUseStarted
  | WireEventAssistantToolUseDelta
  | WireEventAssistantToolUseCompleted
  | WireEventAssistantCompleted
  // Tool execution
  | WireEventToolStarted
  | WireEventToolOutput
  | WireEventToolProgress
  | WireEventToolCompleted
  // Approval
  | WireEventApprovalRequested
  | WireEventApprovalResolved
  | WireEventApprovalExpired
  // Question
  | WireEventQuestionRequested
  | WireEventQuestionAnswered
  | WireEventQuestionDismissed
  // Background tasks
  | WireEventTaskCreated
  | WireEventTaskProgress
  | WireEventTaskCompleted
  // Config
  | WireEventConfigChanged
  | WireEventAgentProfilesChanged
  | WireEventModelCatalogChanged
  // MCP pool transitions (rust core)
  | WireEventMcpServerStatus
  // Unknown / future events
  | WireEventUnknown;
