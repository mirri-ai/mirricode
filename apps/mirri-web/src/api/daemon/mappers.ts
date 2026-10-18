// apps/mirri-web/src/api/daemon/mappers.ts
// wire→app and app→wire mapper functions.
// All snake_case ↔ camelCase conversion happens ONLY here.

import type {
  AppApprovalRequest,
  AppAddProviderInput,
  AppCatalogModel,
  AppCatalogProvider,
  AppConfig,
  AppConfigProvider,
  AppEvent,
  AppGoal,
  AppModel,
  AppModelAlias,
  AppProvider,
  FsEntry,
  AppMessage,
  AppMessageContent,
  AppMessageRole,
  AppQuestionRequest,
  AppSession,
  AppSessionUsage,
  AppTask,
  AppTaskStatus,
  AppAgentProfile,
  AppAgentProfileList,
  AppWorkspace,
  ApprovalDecision,
  ApprovalResponse,
  ImageSource,
  PromptSubmission,
  QuestionAnswer,
  QuestionItem,
  QuestionOption,
  QuestionResponse,
} from '../types';

import type {
  WireApprovalRequest,
  WireApprovalResponse,
  WireBackgroundTask,
  WireCatalogModel,
  WireCatalogProvider,
  WireFsEntry,
  WireImageSource,
  WireMessage,
  WireMessageContent,
  WireModel,
  WireModelAlias,
  WirePromptSubmission,
  WireProvider,
  WireQuestionAnswer,
  WireQuestionItem,
  WireQuestionOption,
  WireQuestionRequest,
  WireQuestionResponse,
  WireSession,
  WireSessionUsage,
  WireWorkspace,
  WireRustWorkspace,
  WireEvent,
  WireConfig,
  WireAgentProfile,
  WireAgentProfileList,
} from './wire';

// ---------------------------------------------------------------------------
// Session mappers
// ---------------------------------------------------------------------------

export function toAppSessionUsage(wire: WireSessionUsage): AppSessionUsage {
  return {
    inputTokens: wire.input_tokens,
    outputTokens: wire.output_tokens,
    cacheReadTokens: wire.cache_read_tokens,
    cacheCreationTokens: wire.cache_creation_tokens,
    totalCostUsd: wire.total_cost_usd,
    contextTokens: wire.context_tokens,
    contextLimit: wire.context_limit,
    turnCount: wire.turn_count,
    firstCall: wire.first_call
      ? {
          inputTokens: wire.first_call.input_tokens,
          outputTokens: wire.first_call.output_tokens,
          cacheReadTokens: wire.first_call.cache_read_tokens,
          cacheCreationTokens: wire.first_call.cache_creation_tokens,
        }
      : undefined,
  };
}

/**
 * True when a session usage object is the daemon's all-zero placeholder.
 * Both engines return placeholders for the heavy session fields on the
 * list/snapshot read paths; the live values arrive via GET /status and the
 * WS `agent.status.updated` stream. Callers replacing a cached session with
 * a wire record must keep the live usage when the incoming one is this
 * placeholder, or the context ring drops to 0 until the next refresh.
 */
export function isPlaceholderSessionUsage(usage: AppSessionUsage): boolean {
  return (
    usage.contextTokens === 0 &&
    usage.contextLimit === 0 &&
    usage.inputTokens === 0 &&
    usage.outputTokens === 0 &&
    usage.turnCount === 0
  );
}

/**
 * Merge a `session.snapshot` view onto an existing session record (the
 * snapshot-sync path's `updateSession` reducer). The snapshot view models
 * only the run-state + message facts, so the record's richer fields the
 * snapshot does not authoritatively carry must survive the merge:
 *
 * - `cwd` — the engine's snapshot view carries it (the same derivation the
 *   `session.list` rows use), but older builds omit it and the mapping layer
 *   degrades an omitted cwd to `''`. An empty snapshot cwd must not wipe the
 *   record's cwd — that is what drops a session out of its sidebar workspace
 *   group until a manual per-group refresh. A non-empty snapshot cwd stays
 *   authoritative, so a session whose root genuinely changed still reports
 *   its fresh root.
 * - `model` — the snapshot reports `null` until the session's first agent
 *   exists; the record keeps its last known model.
 * - `usage` — a placeholder snapshot usage must not zero the live usage
 *   folded in from /status and the WS status stream.
 * - `title` / `createdAt` / `updatedAt` — the engine's snapshot session view
 *   does not carry them (title is always `null`, the timestamps are absent),
 *   and the mapping layer degrades the absent values to `''`. An empty
 *   snapshot value must not wipe the record's — that is what blanked a fresh
 *   session's sidebar title and rendered its row time as "NaNy"
 *   (`formatTime('')` → Invalid Date → NaN). A non-empty snapshot value stays
 *   authoritative.
 */
export function mergeSnapshotSession(
  existing: AppSession,
  snapshot: AppSession,
  snapshotUsageIsPlaceholder: boolean,
): AppSession {
  return {
    ...snapshot,
    title: snapshot.title.length > 0 ? snapshot.title : existing.title,
    createdAt: snapshot.createdAt.length > 0 ? snapshot.createdAt : existing.createdAt,
    updatedAt: snapshot.updatedAt.length > 0 ? snapshot.updatedAt : existing.updatedAt,
    cwd: snapshot.cwd.length > 0 ? snapshot.cwd : existing.cwd,
    model:
      snapshot.model && snapshot.model.length > 0
        ? snapshot.model
        : existing.model,
    usage: snapshotUsageIsPlaceholder ? existing.usage : snapshot.usage,
  };
}

export function toAppSession(wire: WireSession): AppSession {
  return {
    id: wire.id,
    title: wire.title,
    createdAt: wire.created_at,
    updatedAt: wire.updated_at,
    busy: wire.busy,
    mainTurnActive: wire.main_turn_active,
    pendingInteraction: wire.pending_interaction,
    lastTurnReason: wire.last_turn_reason,
    archived: wire.archived ?? false,
    currentPromptId: wire.current_prompt_id,
    lastPrompt: wire.last_prompt,
    cwd: wire.metadata.cwd,
    model: wire.agent_config.model,
    usage: toAppSessionUsage(wire.usage),
    messageCount: wire.message_count,
    lastSeq: wire.last_seq,
    workspaceId: wire.workspace_id,
    parentSessionId:
      typeof wire.metadata['parent_session_id'] === 'string'
        ? wire.metadata['parent_session_id']
        : undefined,
    sessionDir: wire.session_dir,
  };
}

/**
 * Map a workspace wire object to the app shape. The wire carries either
 * dialect: the TS daemon's snake_case (`is_git_repo`, `last_opened_at`,
 * `session_count` — the presumed v1 spelling) or the rust engine's
 * camelCase (`isGitRepo`, `lastOpenedAt`, `sessionCount` — the same
 * spelling `workspace.list` / `createOrTouch` results carry). Missing git
 * facts degrade to the honest defaults (`isGitRepo: false`, no branch), and
 * a missing count degrades to `0` so the sidebar's recency merge never sees
 * `NaN`.
 */
export function toAppWorkspace(wire: WireWorkspace | WireRustWorkspace): AppWorkspace {
  const rust = wire as Partial<WireRustWorkspace>;
  return {
    id: wire.id,
    root: wire.root,
    name: wire.name,
    isGitRepo: rust.isGitRepo ?? (wire as Partial<WireWorkspace>).is_git_repo ?? false,
    branch: rust.branch ?? (wire as Partial<WireWorkspace>).branch ?? undefined,
    lastOpenedAt: rust.lastOpenedAt ?? (wire as Partial<WireWorkspace>).last_opened_at,
    sessionCount: rust.sessionCount ?? (wire as Partial<WireWorkspace>).session_count ?? 0,
    // W8: the rust engine's workspace rows carry the live trust verdict;
    // the TS REST daemon's list has no trust column, so its rows map
    // `true` (that backend has no per-workspace web trust entry — the
    // rust backend is the reference surface, the gap is a W9 parity
    // account).
    trusted: rust.trusted ?? true,
  };
}

// ---------------------------------------------------------------------------
// Message mappers
// ---------------------------------------------------------------------------

function toAppImageSource(src: WireImageSource): ImageSource {
  if (src.kind === 'base64') {
    return { kind: 'base64', mediaType: src.media_type, data: src.data };
  }
  if (src.kind === 'file') {
    return { kind: 'file', fileId: src.file_id };
  }
  return { kind: 'url', url: src.url };
}

export function toAppMessageContent(wire: WireMessageContent): AppMessageContent {
  switch (wire.type) {
    case 'text':
      return { type: 'text', text: wire.text };
    case 'tool_use':
      return {
        type: 'toolUse',
        toolCallId: wire.tool_call_id,
        toolName: wire.tool_name,
        input: wire.input,
      };
    case 'tool_result':
      return {
        type: 'toolResult',
        toolCallId: wire.tool_call_id,
        output: wire.output,
        isError: wire.is_error,
      };
    case 'image':
      return {
        type: 'image',
        source: toAppImageSource(wire.source),
      };
    case 'video':
      return {
        type: 'video',
        source: toAppImageSource(wire.source),
      };
    case 'file':
      return {
        type: 'file',
        fileId: wire.file_id,
        name: wire.name,
        mediaType: wire.media_type,
        size: wire.size,
      };
    case 'thinking':
      return {
        type: 'thinking',
        thinking: wire.thinking,
        signature: wire.signature,
      };
    default: {
      // Unknown content type — pass raw through
      return { type: 'unknown', raw: wire };
    }
  }
}

export function toAppMessage(wire: WireMessage): AppMessage {
  return {
    id: wire.id,
    sessionId: wire.session_id,
    role: wire.role as AppMessageRole,
    content: wire.content.map(toAppMessageContent),
    createdAt: wire.created_at,
    promptId: wire.prompt_id,
    parentMessageId: wire.parent_message_id,
    metadata: wire.metadata,
    withSkills: wire.with_skills,
  };
}

// ---------------------------------------------------------------------------
// Prompt mappers
// ---------------------------------------------------------------------------

function toWireMessageContent(app: AppMessageContent): WireMessageContent {
  switch (app.type) {
    case 'text':
      return { type: 'text', text: app.text };
    case 'toolUse':
      return {
        type: 'tool_use',
        tool_call_id: app.toolCallId,
        tool_name: app.toolName,
        input: app.input,
      };
    case 'toolResult':
      return {
        type: 'tool_result',
        tool_call_id: app.toolCallId,
        output: app.output,
        is_error: app.isError,
      };
    case 'image':
    case 'video': {
      const src = app.source;
      let wireSrc: WireImageSource;
      if (src.kind === 'base64') {
        wireSrc = { kind: 'base64', media_type: src.mediaType, data: src.data };
      } else if (src.kind === 'file') {
        wireSrc = { kind: 'file', file_id: src.fileId };
      } else {
        wireSrc = { kind: 'url', url: src.url };
      }
      return { type: app.type, source: wireSrc };
    }
    case 'file':
      return {
        type: 'file',
        file_id: app.fileId,
        name: app.name,
        media_type: app.mediaType,
        size: app.size,
      };
    case 'thinking':
      return { type: 'thinking', thinking: app.thinking, signature: app.signature };
    case 'unknown':
      // Best-effort: pass raw back. May not be a valid WireMessageContent.
      return app.raw as WireMessageContent;
  }
}

export function toWirePromptSubmission(input: PromptSubmission): WirePromptSubmission {
  return {
    content: input.content.map(toWireMessageContent),
    metadata: input.metadata,
    agent_id: input.agentId,
    model: input.model,
    thinking: input.thinking,
    permission_mode: input.permissionMode,
    plan_mode: input.planMode,
    swarm_mode: input.swarmMode,
    collaboration_mode: input.collaborationMode,
    goal_objective: input.goalObjective,
    goal_control: input.goalControl,
  };
}

// ---------------------------------------------------------------------------
// Approval mappers
// ---------------------------------------------------------------------------

export function toWireApprovalResponse(input: ApprovalResponse): WireApprovalResponse {
  return {
    decision: input.decision,
    scope: input.scope,
    feedback: input.feedback,
    selected_label: input.selectedLabel,
  };
}

export function toAppApprovalRequest(wire: WireApprovalRequest): AppApprovalRequest {
  // Dual-shape read: the TS daemon wire is snake_case; the rust engine's
  // `event.approval.requested` projection carries the same facts in
  // camelCase (`requestId`, `toolName`). Either spelling wins when present.
  const w = wire as unknown as Record<string, unknown>;
  const str = (v: unknown): string | undefined => (typeof v === 'string' ? v : undefined);
  const num = (v: unknown): number | undefined => (typeof v === 'number' ? v : undefined);
  return {
    approvalId: str(w['approval_id']) ?? str(w['requestId']) ?? '',
    sessionId: str(w['session_id']) ?? '',
    turnId: num(w['turn_id']) ?? num(w['turnId']),
    toolCallId: str(w['tool_call_id']) ?? str(w['toolCallId']) ?? '',
    toolName: str(w['tool_name']) ?? str(w['toolName']) ?? '',
    action: str(w['action']) ?? '',
    // The real daemon sends `tool_input_display`; the stub sends `display`.
    display: w['tool_input_display'] ?? w['display'],
    // The requesting member's identity (supervision B1) — snake_case only:
    // the full agent id form (`agent-<uuid>`) the ask payload and snapshot
    // rows carry. The klient camelCase `agentId` (a bare uuid on the
    // event rows) is deliberately NOT adopted — it cannot correlate with the
    // roster's full ids.
    agentId: str(w['agent_id']),
    // Honest expiry (D-13): pass through only what the wire carried — no
    // fabricated `now`/empty-string default.
    expiresAt: str(w['expires_at']),
    // The engine approval-rule facet (B9) — klient camelCase on the event
    // rows, snake_case tolerated for symmetry with the other dual reads.
    approvalRule: str(w['approvalRule']) ?? str(w['approval_rule']),
    createdAt: str(w['created_at']) ?? '',
  };
}

// ---------------------------------------------------------------------------
// Question mappers
// ---------------------------------------------------------------------------

function toAppQuestionOption(wire: WireQuestionOption): QuestionOption {
  return {
    id: wire.id,
    label: wire.label,
    description: wire.description,
    recommended: wire.recommended === true || wire.is_recommended === true,
  };
}

function toAppQuestionItem(wire: WireQuestionItem): QuestionItem {
  return {
    id: wire.id,
    question: wire.question,
    header: wire.header,
    body: wire.body,
    options: wire.options.map(toAppQuestionOption),
    // Dual-spelling read: the TS daemon wire spells these snake_case, but
    // the rust engine's klient rows spell them camelCase — both map onto
    // the app spelling (AskUserQuestion 'Other' parity).
    multiSelect: wire.multiSelect ?? wire.multi_select,
    allowOther: wire.allowOther ?? wire.allow_other,
    otherLabel: wire.otherLabel ?? wire.other_label,
    otherDescription: wire.otherDescription ?? wire.other_description,
  };
}

export function toAppQuestionRequest(wire: WireQuestionRequest): AppQuestionRequest {
  return {
    questionId: wire.question_id,
    sessionId: wire.session_id,
    turnId: wire.turn_id,
    toolCallId: wire.tool_call_id,
    questions: wire.questions.map(toAppQuestionItem),
    createdAt: wire.created_at,
  };
}

function toWireQuestionAnswer(app: QuestionAnswer): WireQuestionAnswer {
  switch (app.kind) {
    case 'single':
      return { kind: 'single', option_id: app.optionId };
    case 'multi':
      return { kind: 'multi', option_ids: app.optionIds };
    case 'other':
      return { kind: 'other', text: app.text };
    case 'multiWithOther':
      return { kind: 'multi_with_other', option_ids: app.optionIds, other_text: app.otherText };
    case 'skipped':
      return { kind: 'skipped' };
  }
}

export function toWireQuestionResponse(input: QuestionResponse): WireQuestionResponse {
  const wireAnswers: Record<string, WireQuestionAnswer> = {};
  for (const [questionId, answer] of Object.entries(input.answers)) {
    wireAnswers[questionId] = toWireQuestionAnswer(answer);
  }
  return {
    answers: wireAnswers,
    method: input.method,
    note: input.note,
  };
}

// ---------------------------------------------------------------------------
// Task mapper
// ---------------------------------------------------------------------------

export function toAppTask(wire: WireBackgroundTask): AppTask {
  return {
    id: wire.id,
    sessionId: wire.session_id,
    kind: wire.kind,
    description: wire.description,
    status: wire.status as AppTaskStatus,
    command: wire.command,
    createdAt: wire.created_at,
    startedAt: wire.started_at,
    completedAt: wire.completed_at,
    outputPreview: wire.output_preview,
    outputBytes: wire.output_bytes,
    subagentPhase: wire.subagent_phase,
    subagentType: wire.subagent_type,
    parentToolCallId: wire.parent_tool_call_id,
    suspendedReason: wire.suspended_reason,
    swarmIndex: wire.swarm_index,
    // The snapshot's subagent roster carries the explicit flag. REST `/tasks`
    // under the rust engine lists foreground running sub-agents too, so the
    // kind alone does not imply background; the `?? true` fallback now only
    // covers REST-only rows the event stream never saw (detached persistence
    // semantics — keepLiveSubagents folds foreground copies away before the
    // flag can mislead).
    runInBackground: wire.run_in_background ?? (wire.kind === 'subagent' ? true : undefined),
    model: wire.model,
    // Join key back to the WS `subagent.*` row (same agent id) so a WS
    // completion event can also terminate this REST row in the dock.
    agentId: wire.agent_id,
    // outputLines starts undefined; populated by eventReducer via task.progress events
  };
}

// ---------------------------------------------------------------------------
// FsEntry mapper
// ---------------------------------------------------------------------------

export function toAppFsEntry(wire: WireFsEntry): FsEntry {
  return {
    path: wire.path,
    name: wire.name,
    kind: wire.kind,
    size: wire.size,
    modifiedAt: wire.modified_at,
    etag: wire.etag,
    mime: wire.mime,
    languageId: wire.language_id,
    isBinary: wire.is_binary,
    isSymlinkTo: wire.is_symlink_to,
    gitStatus: wire.git_status,
    childCount: wire.child_count,
  };
}

// ---------------------------------------------------------------------------
// WireEvent → AppEvent
// ---------------------------------------------------------------------------

function recordString(source: Record<string, unknown>, key: string): string | undefined {
  const value = source[key];
  return typeof value === 'string' ? value : undefined;
}

function recordNumber(source: Record<string, unknown>, key: string): number | undefined {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function recordNullableNumber(source: Record<string, unknown>, key: string): number | null {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

export function toAppGoal(snapshot: unknown): AppGoal | null {
  if (!snapshot || typeof snapshot !== 'object') return null;
  const source = snapshot as Record<string, unknown>;
  const status = recordString(source, 'status');
  if (status !== 'active' && status !== 'paused' && status !== 'blocked' && status !== 'complete') {
    return null;
  }

  const budgetRaw = source['budget'];
  const budget = budgetRaw && typeof budgetRaw === 'object' ? budgetRaw as Record<string, unknown> : {};

  return {
    goalId: recordString(source, 'goalId') ?? recordString(source, 'goal_id') ?? 'goal',
    objective: recordString(source, 'objective') ?? '',
    completionCriterion: recordString(source, 'completionCriterion') ?? recordString(source, 'completion_criterion'),
    status,
    turnsUsed: recordNumber(source, 'turnsUsed') ?? recordNumber(source, 'turns_used') ?? 0,
    tokensUsed: recordNumber(source, 'tokensUsed') ?? recordNumber(source, 'tokens_used') ?? 0,
    wallClockMs: recordNumber(source, 'wallClockMs') ?? recordNumber(source, 'wall_clock_ms') ?? 0,
    terminalReason: recordString(source, 'terminalReason') ?? recordString(source, 'terminal_reason'),
    budget: {
      tokenBudget: recordNullableNumber(budget, 'tokenBudget') ?? recordNullableNumber(budget, 'token_budget'),
      remainingTokens: recordNullableNumber(budget, 'remainingTokens') ?? recordNullableNumber(budget, 'remaining_tokens'),
      turnBudget: recordNullableNumber(budget, 'turnBudget') ?? recordNullableNumber(budget, 'turn_budget'),
      remainingTurns: recordNullableNumber(budget, 'remainingTurns') ?? recordNullableNumber(budget, 'remaining_turns'),
      wallClockBudgetMs: recordNullableNumber(budget, 'wallClockBudgetMs') ?? recordNullableNumber(budget, 'wall_clock_budget_ms'),
      remainingWallClockMs: recordNullableNumber(budget, 'remainingWallClockMs') ?? recordNullableNumber(budget, 'remaining_wall_clock_ms'),
      overBudget: budget['overBudget'] === true || budget['over_budget'] === true,
    },
  };
}

/**
 * Map a WireEvent to an AppEvent.
 *
 * Decision: reducer consumes AppEvent.
 * - Visible events are fully mapped to their camelCase AppEvent variant.
 * - No-op-but-known streaming/tool events (tool.*, assistant.tool_use_*,
 *   assistant.completed) are folded to { type: 'unknown', raw } so the reducer
 *   can advance lastSeqBySession without emitting warnings.
 *   We use a dedicated sentinel raw: { _noop: true } so Task 7 reducer can
 *   distinguish real unknowns (push warning) from no-op knowns (silent advance).
 * - Truly unknown events are also { type: 'unknown', raw } but raw._noop is absent.
 */
export function toAppEvent(wire: WireEvent): AppEvent {
  // TypeScript cannot narrow the WireEvent union through specific `case` arms
  // because the catch-all `WireEventUnknown` member has `type: string` (broad)
  // and `payload: unknown`, which prevents discriminated-union narrowing.
  // We cast to `any` once here; individual cases are still logically type-safe
  // because the union member types document the actual payload shapes.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const w = wire as any;
  switch ((wire as { type: string }).type) {
    // ----- Session lifecycle -----
    case 'event.session.created':
      return { type: 'sessionCreated', session: toAppSession(w.payload.session) };

    case 'event.session.updated':
      return {
        type: 'sessionUpdated',
        session: toAppSession(w.payload.session),
        changedFields: w.payload.changed_fields,
      };

    case 'event.session.deleted':
      return { type: 'sessionDeleted', sessionId: w.session_id };

    // ----- Workspace lifecycle -----
    case 'event.workspace.created':
      return { type: 'workspaceCreated', workspace: toAppWorkspace(w.payload.workspace) };

    case 'event.workspace.updated':
      return { type: 'workspaceUpdated', workspace: toAppWorkspace(w.payload.workspace) };

    case 'event.workspace.deleted': {
      // Both dialects: the TS daemon's snake_case and the rust engine's
      // camelCase (`workspaceId`) — the same tolerant read the created/
      // updated branches apply to the workspace object.
      const payload = w.payload as { workspace_id?: string; workspaceId?: string; root?: string };
      return {
        type: 'workspaceDeleted',
        workspaceId: payload.workspace_id ?? payload.workspaceId ?? '',
        root: payload.root ?? '',
      };
    }

    case 'event.session.work_changed':
      return {
        type: 'sessionWorkChanged',
        sessionId: w.session_id,
        busy: w.payload.busy,
        mainTurnActive: w.payload.main_turn_active,
        pendingInteraction: w.payload.pending_interaction,
        lastTurnReason: w.payload.last_turn_reason,
        // The wire seq rides the app event (the same convention
        // model_switched uses) so clients can dedupe replayed frames.
        seq: w.seq,
      };

    // Deprecated: old journals may still carry status_changed; fold it onto
    // the busy flag (awaiting/running were live work, aborted was not).
    case 'event.session.status_changed':
      return {
        type: 'sessionWorkChanged',
        sessionId: w.session_id,
        busy: w.payload.status !== 'idle' && w.payload.status !== 'aborted',
        mainTurnActive: w.payload.status !== 'idle' && w.payload.status !== 'aborted',
        pendingInteraction:
          w.payload.status === 'awaiting_approval'
            ? 'approval'
            : w.payload.status === 'awaiting_question'
              ? 'question'
              : 'none',
        lastTurnReason: w.payload.status === 'aborted' ? 'cancelled' : undefined,
      };

    case 'event.session.usage_updated':
      return {
        type: 'sessionUsageUpdated',
        sessionId: w.session_id,
        usage: toAppSessionUsage(w.payload.usage),
      };

    case 'event.session.model_switched':
      return {
        type: 'sessionModelSwitched',
        sessionId: w.session_id,
        // The rust engine frames params flat camelCase; the envelope keys
        // (sessionId/seq/epoch/volatile) are lifted by the RPC client, so the
        // payload carries the model names and `seq` rides the wire seq.
        from: w.payload.from,
        to: w.payload.to,
        seq: w.seq,
      };

    case 'event.session.history_compacted':
      return {
        type: 'historyCompacted',
        sessionId: w.session_id,
        beforeSeq: w.payload.before_seq,
        reason: w.payload.reason,
        summaryMessageId: w.payload.summary_message_id,
      };

    case 'event.goal.updated': {
      const goal = toAppGoal(w.payload.snapshot ?? null);
      return {
        type: 'goalUpdated',
        sessionId: w.session_id,
        goal: goal?.status === 'complete' ? null : goal,
      };
    }

    // ----- Message lifecycle -----
    case 'event.message.created':
      return { type: 'messageCreated', message: toAppMessage(w.payload.message) };

    // The engine's message-receipt ack (a 120-char content summary the
    // turn publishes before running). The full user message arrives as
    // event.message.created — this is a redundant no-op for the web.
    case 'event.message.received':
      return { type: 'unknown', raw: { _noop: true, _wireType: w.type } };

    case 'event.message.updated':
      return {
        type: 'messageUpdated',
        sessionId: w.session_id,
        messageId: w.payload.message_id,
        content: w.payload.content.map(toAppMessageContent),
        status: w.payload.status,
      };

    // ----- Assistant streaming -----
    case 'event.assistant.delta':
      return {
        type: 'assistantDelta',
        sessionId: w.session_id,
        messageId: w.payload.message_id,
        contentIndex: w.payload.content_index,
        delta: w.payload.delta,
      };

    // No-op streaming events — advance seq silently
    case 'event.assistant.tool_use_started':
    case 'event.assistant.tool_use_delta':
    case 'event.assistant.tool_use_completed':
    case 'event.assistant.completed':
    case 'event.tool.started':
      return { type: 'unknown', raw: { _noop: true, _wireType: w.type } };

    case 'event.tool.output':
      return {
        type: 'toolOutput',
        sessionId: w.session_id,
        toolCallId: w.payload.tool_call_id,
        outputChunk: w.payload.chunk,
        stream: w.payload.stream,
      };

    case 'event.tool.progress':
      if (typeof w.payload.message === 'string' && w.payload.message.length > 0) {
        return {
          type: 'toolOutput',
          sessionId: w.session_id,
          toolCallId: w.payload.tool_call_id,
          outputChunk: w.payload.message,
          stream: 'stdout',
        };
      }
      return { type: 'unknown', raw: { _noop: true, _wireType: w.type } };

    case 'event.tool.completed':
      return { type: 'unknown', raw: { _noop: true, _wireType: w.type } };

    // ----- Approval -----
    case 'event.approval.requested':
      return {
        type: 'approvalRequested',
        sessionId: w.session_id,
        approval: toAppApprovalRequest(w.payload),
      };

    case 'event.approval.resolved': {
      // Dual-shape read (see toAppApprovalRequest): the TS daemon wire is
      // snake_case (`approval_id`/`decision`/`resolved_at`); the rust
      // engine's projection sends camelCase `{requestId, approved}` with no
      // timestamp. The id MUST survive both — the UI clears the approval
      // card by this id, and the engine's gate is first-one-wins.
      const p = w.payload as Record<string, unknown>;
      return {
        type: 'approvalResolved',
        sessionId: w.session_id,
        approvalId: recordString(p, 'approval_id') ?? recordString(p, 'requestId') ?? '',
        decision: (recordString(p, 'decision') ?? (p['approved'] === true ? 'approved' : 'rejected')) as ApprovalDecision,
        resolvedAt: recordString(p, 'resolved_at') ?? new Date().toISOString(),
      };
    }

    case 'event.approval.expired': {
      // Dual-shape read: `approval_id` (TS daemon) / `requestId` (rust).
      const p = w.payload as Record<string, unknown>;
      return {
        type: 'approvalExpired',
        sessionId: w.session_id,
        approvalId: recordString(p, 'approval_id') ?? recordString(p, 'requestId') ?? '',
      };
    }

    // ----- Question -----
    case 'event.question.requested':
      return {
        type: 'questionRequested',
        sessionId: w.session_id,
        question: toAppQuestionRequest(w.payload),
      };

    case 'event.question.answered': {
      // Dual-shape read: `question_id`/`resolved_at` (TS daemon) /
      // `requestId` (rust projection, no timestamp — stamped at receipt).
      const p = w.payload as Record<string, unknown>;
      return {
        type: 'questionAnswered',
        sessionId: w.session_id,
        questionId: recordString(p, 'question_id') ?? recordString(p, 'requestId') ?? '',
        resolvedAt: recordString(p, 'resolved_at') ?? new Date().toISOString(),
      };
    }

    case 'event.question.dismissed': {
      // Dual-shape read: `question_id`/`dismissed_at` (TS daemon) /
      // `requestId` (rust projection).
      const p = w.payload as Record<string, unknown>;
      return {
        type: 'questionDismissed',
        sessionId: w.session_id,
        questionId: recordString(p, 'question_id') ?? recordString(p, 'requestId') ?? '',
        dismissedAt: recordString(p, 'dismissed_at') ?? new Date().toISOString(),
      };
    }

    // ----- Background tasks -----
    case 'event.task.created':
      return {
        type: 'taskCreated',
        sessionId: w.session_id,
        task: toAppTask(w.payload.task),
      };

    case 'event.task.progress':
      return {
        type: 'taskProgress',
        sessionId: w.session_id,
        taskId: w.payload.task_id,
        outputChunk: w.payload.output_chunk,
        stream: w.payload.stream,
      };

    case 'event.task.completed':
      return {
        type: 'taskCompleted',
        sessionId: w.session_id,
        taskId: w.payload.task_id,
        status: w.payload.status as AppTaskStatus,
        outputPreview: w.payload.output_preview,
        outputBytes: w.payload.output_bytes,
      };

    case 'event.config.changed':
      return {
        type: 'configChanged',
        changedFields: w.payload.changed_fields,
        config: toAppConfig(w.payload.config),
      };

    case 'event.agent.profiles_changed':
      return { type: 'agentProfilesChanged' };

    case 'event.model_catalog.changed':
      return {
        type: 'modelCatalogChanged',
        changed: w.payload.changed.map(
          (item: { provider_id: string; provider_name: string; added: number; removed: number }) => ({
            providerId: item.provider_id,
            providerName: item.provider_name,
            added: item.added,
            removed: item.removed,
          }),
        ),
        unchanged: w.payload.unchanged,
        failed: w.payload.failed,
      };

    // Rust core MCP pool transition — the payload is already camelCase
    // (event_projection.rs), so it passes through verbatim.
    case 'event.mcp.server.status':
      return {
        type: 'mcpServerStatus',
        sessionId: w.session_id,
        server: w.payload.server,
      };

    default: {
      // Truly unknown event — record warning
      return { type: 'unknown', raw: wire };
    }
  }
}

// ---------------------------------------------------------------------------
// Model + Provider mappers
// PRESUMED — not in current daemon docs; isolated here, swap when backend defines them.
// ---------------------------------------------------------------------------

export function toAppModel(wire: WireModel): AppModel {
  return {
    id: wire.model,
    provider: wire.provider,
    model: wire.model,
    displayName: wire.display_name,
    maxContextSize: wire.max_context_size,
    capabilities: wire.capabilities,
    supportEfforts: wire.support_efforts,
    defaultEffort: wire.default_effort,
  };
}

export function toAppProvider(wire: WireProvider): AppProvider {
  return {
    id: wire.id,
    type: wire.type,
    baseUrl: wire.base_url,
    defaultModel: wire.default_model,
    hasApiKey: wire.has_api_key,
    apiKeyDisplay: wire.api_key_display ?? null,
    status: wire.status,
    models: wire.models,
  };
}

export function toAppCatalogModel(wire: WireCatalogModel): AppCatalogModel {
  return {
    id: wire.id,
    name: wire.name,
    maxContextSize: wire.max_context_size,
    capabilities: wire.capabilities,
    reasoning: wire.reasoning,
  };
}

export function toAppCatalogProvider(wire: WireCatalogProvider): AppCatalogProvider {
  return {
    id: wire.id,
    name: wire.name,
    wireType: wire.wire_type,
    guessed: wire.guessed,
    needsBaseUrl: wire.needs_base_url,
    rejected: wire.rejected,
    rejectReason: wire.reject_reason,
    envKey: wire.env_key,
    models: (wire.models ?? []).map(toAppCatalogModel),
  };
}

/** v2 `POST /providers` create body: explicit id + wire type + models. The
 *  server zod schema strips unknown/undefined keys, so absent optionals are
 *  passed as `undefined` (dropped by JSON.stringify). */
export function toWireAddProviderBody(input: AppAddProviderInput): Record<string, unknown> {
  return {
    id: input.id,
    type: input.type,
    api_key: input.apiKey,
    base_url: input.baseUrl,
    default_model: input.defaultModel,
    models: input.models.map((model) => ({
      model: model.model,
      max_context_size: model.maxContextSize,
      display_name: model.displayName,
      capabilities: model.capabilities,
      max_output_size: model.maxOutputSize,
      support_efforts: model.supportEfforts,
      adaptive_thinking: model.adaptiveThinking,
    })),
  };
}

/** `POST /providers:import_catalog` body: import a models.dev entry as a
 *  configured provider. `id` overrides the catalog id as the local provider
 *  id; `base_url` is required when the entry resolves to needs-base-url. */
export function toWireImportCatalogBody(input: {
  catalogId: string;
  id?: string;
  apiKey?: string;
  baseUrl?: string;
}): Record<string, unknown> {
  return {
    catalog_id: input.catalogId,
    id: input.id,
    api_key: input.apiKey,
    base_url: input.baseUrl,
  };
}

export function toAppModelAlias(wire: WireModelAlias): AppModelAlias {
  return {
    provider: wire.provider,
    model: wire.model,
    maxContextSize: wire.max_context_size,
    maxOutputSize: wire.max_output_size,
    capabilities: wire.capabilities,
    displayName: wire.display_name,
    description: wire.description,
    reasoningKey: wire.reasoning_key,
    protocol: wire.protocol,
    adaptiveThinking: wire.adaptive_thinking,
    supportEfforts: wire.support_efforts,
    defaultEffort: wire.default_effort,
    betaApi: wire.beta_api,
  };
}

export function toAppConfig(wire: WireConfig): AppConfig {
  const providers: Record<string, AppConfigProvider> = {};
  for (const [id, provider] of Object.entries(wire.providers)) {
    providers[id] = {
      type: provider.type,
      baseUrl: provider.base_url,
      defaultModel: provider.default_model,
      hasApiKey: provider.has_api_key,
      apiKeyDisplay: provider.api_key_display ?? null,
    };
  }
  const models: Record<string, AppModelAlias> | undefined = wire.models
    ? Object.fromEntries(Object.entries(wire.models).map(([id, m]) => [id, toAppModelAlias(m)]))
    : undefined;
  return {
    providers,
    defaultProvider: wire.default_provider,
    defaultModel: wire.default_model,
    models,
    thinking: wire.thinking as { enabled?: boolean; effort?: string } | undefined,
    planMode: wire.plan_mode,
    yolo: wire.yolo,
    defaultPermissionMode: wire.default_permission_mode,
    defaultPlanMode: wire.default_plan_mode,
    permission: wire.permission,
    hooks: wire.hooks,
    services: wire.services,
    mergeAllAvailableSkills: wire.merge_all_available_skills,
    extraSkillDirs: wire.extra_skill_dirs,
    disabledSkills: wire.disabled_skills,
    extraAgentDirs: wire.extra_agent_dirs,
    disabledAgents: wire.disabled_agents,
    loopControl: wire.loop_control,
    background: wire.background,
    experimental: wire.experimental,
    telemetry: wire.telemetry,
    raw: wire.raw,
  };
}

export function toAppProfile(wire: WireAgentProfile): AppAgentProfile {
  return {
    name: wire.name,
    description: wire.description,
    source: wire.source,
    builtin: wire.builtin,
    essential: wire.essential,
    enabled: wire.enabled,
    hasOverride: wire.has_override,
    filePath: wire.file_path,
    defaultModel: wire.default_model,
    tools: wire.tools,
    disallowedTools: wire.disallowed_tools,
    capabilitiesRequired: wire.capabilities_required,
    subagents: wire.subagents,
    suppressedCandidates: wire.suppressed_candidates?.map((c) => ({
      source: c.source,
      priority: c.priority,
      reason: c.reason,
    })),
    whenToUse: wire.when_to_use,
    systemPromptTemplate: wire.system_prompt_template,
    effectiveSystemPrompt: wire.effective_system_prompt,
  };
}

/** Map the `GET /agents` list response — items plus skipped-file warnings. */
export function toAppAgentProfileList(wire: WireAgentProfileList): AppAgentProfileList {
  return {
    profiles: (wire.items ?? []).map(toAppProfile),
    skippedAgentFiles: (wire.skipped ?? []).map((s) => ({ path: s.path, reason: s.reason })),
  };
}

// Helper to extract sessionId from a WireEvent (needed by reducer for lastSeq update)
export function wireEventSessionId(wire: WireEvent): string {
  return wire.session_id;
}

export function wireEventSeq(wire: WireEvent): number {
  return wire.seq;
}
