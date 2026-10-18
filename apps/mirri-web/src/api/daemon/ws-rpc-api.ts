// apps/mirri-web/src/api/daemon/ws-rpc-api.ts
// WsRpcApi — app-facing API surface over the unified JSON-RPC WebSocket
// transport (JsonRpcClient → mirri-server `/api/v1/rpc`).
//
// This is the ADDITIVE Phase-2 transport: it does not replace DaemonMirriWebApi
// yet, and it implements only a subset of MirriWebApi (the most important
// session/message/approval/question methods) plus connectEvents. Commands and
// events share ONE WebSocket connection — there is no dual-channel
// correlation anymore.
//
// Wire contract notes (authoritative source: rust/crates/mirri-server):
//   - inbound params are per-method mixed snake_case/camelCase today (the
//     server is mid-migration; e.g. session.get takes `session_id` while
//     session.status takes `sessionId`). Each method below sends exactly the
//     field names its handler deserializes.
//   - outbound results are camelCase (SessionView: id/title/createdAt/...);
//     the model-provider rows are the exception — `model.list` /
//     `model.listProviders` stay snake_case per the klient catalog schemas
//     (see RpcModel / RpcProvider and their toApp* mappers).
//   - the server rejects unknown params? No — serde ignores unknown fields,
//     but we still only send fields the handler reads, so a future
//     `deny_unknown_fields` cannot break us.
//   - `message.send` accepts a structured `input` part array (text / image /
//     video / file, the v1 TS REST content spellings — see
//     promptContentToInputParts); file references are resolved server-side
//     at the entry.

import { JsonRpcCallError, JsonRpcClient } from './json-rpc-client';
import type { RpcEvent } from './json-rpc-client';
import { DaemonApiError } from '../errors';
import { SERVER_AUTH_UNAUTHORIZED_CODE } from './http';
import { createAgentProjector, turnUsageForPromptId } from './agentEventProjector';
import type { AgentProjector } from './agentEventProjector';
import {
  ParkedAskRegistry,
  approvalRequestFromAskParams,
  approvalResponseToEngineWire,
  questionRequestFromAskParams,
  questionResponseToEngineWire,
  resolutionEventRequestId,
  routeRpcEventToAppEvents,
} from './ws-rpc-event-bridge';
import { getCredential, markAuthRequired } from './serverAuth';
import { buildRestUrl } from '../config';
import {
  ExternalOpenUnavailableError,
  getDesktopBridge,
  resolveWorkspacePath,
} from '../../lib/desktopBridge';
import type {
  AppAddProviderInput,
  AppAgentProfile,
  AppAgentProfileInput,
  AppAgentProfileList,
  AppAgentProfileUpdate,
  AppConfig,
  AppConfigProvider,
  AppCatalogProvider,
  AppCronTask,
  AppApprovalRequest,
  AppPermissionRule,
  AppPermissionRuleDecision,
  AppPermissionRuleInput,
  AppEvent,
  AppGoal,
  AppMcpGlobalConfigEntry,
  AppMcpServer,
  AppMcpServerConfig,
  AppMcpSyncSummary,
  AppMcpTestResult,
  AppMessage,
  AppMessageContent,
  AppMessageRole,
  AppModel,
  AppModelAlias,
  AppPromptItem,
  AppProvider,
  AppProviderWireType,
  AppQuestionRequest,
  AppSession,
  AppSessionSearchPage,
  AppSessionRuntimeStatus,
  AppSessionSnapshot,
  AppSessionWarning,
  AppSkill,
  AppTask,
  AppTaskStatus,
  AppTerminal,
  AppToolDescriptor,
  AppWorkspace,
  ApprovalResponse,
  FsBrowseResult,
  FsEntry,
  FsKind,
  MirriEventConnection,
  MirriEventHandlers,
  MirriWebApi,
  Page,
  PageRequest,
  PromptSubmission,
  PromptSubmitResult,
  ProviderRefreshResult,
  QuestionResponse,
  SteerSessionResult,
  TerminalEventSinks,
  WorkspaceDeletionResult,
  WorkspaceDeleteMode,
} from '../types';

// The shared protocol-part → app-content mapper (mappers.ts toAppMessageContent):
// the rust `message.list` / `session.snapshot` content parts are the SAME shape
// contract as WireMessageContent (the protocolMessage.ts union), so the snapshot
// path reuses the v1 mapper instead of mirroring a second copy.
import { isPlaceholderSessionUsage, toAppMessageContent, toAppTask as toAppTaskFromWire } from './mappers';
import type { WireBackgroundTask } from './wire';
import type { WireMessageContent } from './wire';

// Same subprotocol prefix as DaemonEventSocket (ws.ts) — mirrors the Rust
// server's BEARER_SUBPROTOCOL_PREFIX ("mirri-code.bearer.").
const WS_BEARER_PROTOCOL_PREFIX = 'mirri-code.bearer.';

/** Build the unified-protocol WebSocket URL from a server origin
 *  (`http://host:port` → `ws://host:port/api/v1/rpc`). */
export function buildRpcWsUrl(origin: string): string {
  const url = new URL(`${origin}/api/v1/rpc`);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
}

/**
 * A session view as the server serializes it (SessionView — camelCase).
 * NOTE: the current server's SessionView is a subset of the values the v1
 * REST wire carries, so `toAppSession` back-fills neutral defaults for the
 * fields the v2 view does not expose yet. The snapshot's nested session
 * view is an even smaller subset (no title/createdAt/updatedAt/tags), so
 * those fields are optional and default to neutral values in the mapper.
 */
interface RpcSessionView {
  id: string;
  title?: string | null;
  createdAt?: string;
  updatedAt?: string;
  agentCount: number;
  tags?: string[];
  /** P7c runtime fields — optional so older engine builds (and the static
   *  `SessionInfo`-derived views) fall back to the neutral defaults. */
  busy?: boolean;
  model?: string | null;
  usage?: Partial<RpcSessionUsage> | null;
  messageCount?: number;
  lastSeq?: number;
  /** The session's workspace root, echoed by the rust engine on every
   *  session view (P11a F-P9-8 — sessions bind to the engine's one
   *  workspace). Older engine builds omit it; callers treat missing as
   *  unknown. */
  cwd?: string;
  /** The most recent user prompt (W5 — the session index's
   *  `last_prompt`, surfaced for list previews; empty sessions have
   *  none). */
  lastPrompt?: string | null;
  /** W5 — the engine's SessionView reports the honest archive state;
   *  older builds omit it and map to false. */
  archived?: boolean;
  /** The work-facts tuple (US4.4/SDR-013) — optional: older engine builds
   *  omit them and the sidebar falls back to the neutral defaults.
   *  `stalled` (ADR-004): the watchdog's no-progress termination. */
  mainTurnActive?: boolean;
  pendingInteraction?: 'none' | 'approval' | 'question';
  lastTurnReason?: 'completed' | 'cancelled' | 'failed' | 'stalled';
}

/** `session.list` — the W5 page envelope `{items, has_more}` (the
 *  kap-server list route's shape). Items are SessionView wire objects. */
interface RpcSessionListPage {
  items?: RpcSessionView[];
  has_more?: boolean;
}

/** The first LLM call's four-bucket usage — the cold-start baseline
 *  (SDR-004), camelCase like the engine's SessionView. */
interface RpcFirstCallUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
}

/** The four-bucket token usage the engine's SessionView carries. */
interface RpcSessionUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  firstCall?: RpcFirstCallUsage;
}

/** `goal.get` — the TS GoalSnapshot wire (under the `{goal}` envelope). */
interface RpcGoalSnapshot {
  goalId: string;
  objective: string;
  completionCriterion?: string | null;
  status: 'active' | 'paused' | 'blocked' | 'complete';
  turnsUsed: number;
  tokensUsed: number;
  wallClockMs: number;
  budget?: { turns: number; tokens: number; wallClockMs: number } | null;
}

/** `session.get` — SessionView plus the session's agents. */
interface RpcSessionDetails extends RpcSessionView {
  agents: Array<{ id: string }>;
}

/** `session.status` — run-state snapshot. The P7c fields
 *  (`thinkingEffort`/`swarmMode`/`contextTokens`) and the additive
 *  `collaborationMode` are optional so older engine builds fall back to the
 *  neutral defaults. */
interface RpcSessionStatus {
  sessionId: string;
  running: boolean;
  agentCount: number;
  model: string | null;
  permission: unknown;
  planMode: boolean;
  thinkingEffort?: string;
  swarmMode?: boolean;
  collaborationMode?: boolean;
  contextTokens?: number;
  /** The model's context window resolved from the engine's model catalog
   *  (0 / absent = unknown model — "no ceiling wired"). */
  maxContextTokens?: number;
}

/** `message.send` result (fire mode) — snake_case envelope. */
interface RpcMessageSendResult {
  ok: boolean;
  session_id: string;
  agent_id: string;
  turn_id: string;
}

/** One `session.search` hit — the rust engine's snake_case wire. */
interface RpcSessionSearchHit {
  session_id: string;
  workspace_id: string;
  session_title: string;
  agent_id: string;
  role: 'user' | 'assistant' | 'title';
  snippet: string;
  time: number;
  turn?: number;
}

/** `session.search` result — the literal-mode page. */
interface RpcSessionSearchPage {
  hits: RpcSessionSearchHit[];
  mode: 'literal' | 'terms';
  next_page_token?: string;
}

/** One `message.list` row. */
interface RpcWireMessage {
  id: string;
  role: string;
  content: unknown;
  agentId: string;
  /** The turn the row belongs to (`turn-<n>` from the usage fold) —
   *  present on assistant rows, absent on older engines. */
  promptId?: string;
  /** The turn's server-measured duration (ms) — stamped on the turn's
   *  LAST assistant row so a restored transcript renders the same
   *  "Worked for X" pill as the live one (absent on older engines /
   *  turns without a duration). */
  durationMs?: number;
  /** Provenance (`{"origin": {"kind": ...}}`, TS toProtocolMessage parity) —
   *  the transcript filter keys on `metadata.origin.kind` to hide
   *  system-injected user turns (injections, compaction summaries, …). */
  metadata?: Record<string, unknown>;
  /** The admission-time inline-skill activation list (ADR-006) — the
   *  product record that lets the user bubble rebuild `[name]` markers
   *  as chips on a reloaded session (absent on older engines / messages
   *  that activated nothing). */
  with_skills?: Array<{ name: string; target: string; trigger: string }>;
}

/** `message.list` result. */
interface RpcMessageListResult {
  messages: RpcWireMessage[];
  hasMore: boolean;
}

/** One `fs.browse` entry — the engine's snake_case wire (directory rows;
 * the git fields are optional because the rust engine's rows omit them). */
interface RpcFsBrowseEntry {
  name: string;
  path: string;
  is_dir: boolean;
  is_git_repo?: boolean;
  branch?: string;
}

/** `fs.browse` result — the host-folder-browser wire (snake_case per the
 * protocol `rest/fsBrowse` schema: path, nullable parent, directory rows). */
interface RpcFsBrowseResult {
  path: string;
  parent: string | null;
  entries: RpcFsBrowseEntry[];
}

/** `fs.home` result — the OS user home plus the recent-roots list (the rust
 * engine always answers the honest empty `recent_roots`). */
interface RpcFsHomeResult {
  home: string;
  recent_roots: string[];
}

/** A terminal as the rust server serializes it (protocol `rest/terminal.ts`
 *  `Terminal` shape — snake_case). */
interface RpcTerminal {
  id: string;
  session_id: string;
  cwd: string;
  shell: string;
  cols: number;
  rows: number;
  status: 'running' | 'exited';
  created_at: string;
  exited_at?: string;
  exit_code?: number | null;
}

/** Map the snake_case terminal wire onto the app-facing `AppTerminal`. */
function toAppTerminal(data: RpcTerminal): AppTerminal {
  return {
    id: data.id,
    sessionId: data.session_id,
    cwd: data.cwd,
    shell: data.shell,
    cols: data.cols,
    rows: data.rows,
    status: data.status,
    createdAt: data.created_at,
    exitedAt: data.exited_at,
    exitCode: data.exit_code,
  };
}

const NOT_FOUND_CODE = -32001;
/** JSON-RPC error code the Rust server returns for missing/invalid
 *  credentials (RpcError::Unauthorized — see rust/crates/mirri-server/src/rpc.rs). */
const AUTH_UNAUTHORIZED_CODE = -32002;

export class WsRpcApi implements Partial<MirriWebApi> {
  private readonly client: JsonRpcClient;
  /** Event handlers bound by the most recent connectEvents() call. */
  private handlers: MirriEventHandlers | null = null;
  /** Terminal-stream sinks bound by connectTerminalEvents() — additive to
   *  `handlers`, so a terminal surface can join the app's single event
   *  connection without rebinding the app's handler set. Single slot: at
   *  most one terminal surface is mounted at a time (the shared right-side
   *  detail layer owns the slot). */
  private terminalSinks: TerminalEventSinks | null = null;
  /** The connection object handed out by connectEvents() — shared with
   *  connectTerminalEvents() so the terminal rides the same control frames. */
  private eventConnection: MirriEventConnection | null = null;
  private unsubscribeResync: (() => void) | null = null;
  private projector: AgentProjector = createAgentProjector();
  private unsubscribeAnyEvent: (() => void) | null = null;
  private unsubscribeTerminalOutput: (() => void) | null = null;
  private unsubscribeTerminalExit: (() => void) | null = null;
  /** Last session seen on the event stream — reverse-RPC asks carry no session id, so the ask is attributed here (mirrors the SDK's latestSessionId approach). */
  private lastSessionId: string | undefined;
  /** Parked `approval.ask` reverse-RPC responses, keyed by the minted request id. */
  private readonly approvalAsks = new ParkedAskRegistry<ApprovalResponse>();
  /** Parked `question.ask` reverse-RPC responses, keyed by the minted request id. */
  private readonly questionAsks = new ParkedAskRegistry<QuestionResponse>();
  /** Ask cards that arrived before connectEvents() bound handlers — flushed
   *  at the next connectEvents() call (registration happens in the
   *  constructor, so an ask can beat the UI hookup). */
  private pendingAskEvents: Array<{ appEvent: AppEvent; sessionId: string }> = [];

  constructor(url: string, private readonly origin: string, token: string | undefined) {
    const protocols = token ? [`${WS_BEARER_PROTOCOL_PREFIX}${token}`] : [];
    this.client = new JsonRpcClient(url, protocols, {
      onStateChange: (connected: boolean) => {
        this.handlers?.onConnectionChange(connected);
        this.terminalSinks?.onConnectionChange?.(connected);
      },
      onError: (code: number, message: string, fatal: boolean) => {
        if (code === AUTH_UNAUTHORIZED_CODE) {
          markAuthRequired();
        }
        this.handlers?.onError(code, message, fatal);
        this.terminalSinks?.onError?.(code, message, fatal);
      },
    });
    // Reverse-RPC asks are registered at construction (not in
    // connectEvents): the handshake can complete — and the engine fire an
    // approval.ask — before the UI binds its event handlers, and an
    // unregistered method would be answered method-not-found, stalling the
    // turn. Cards emitted before connectEvents are buffered instead.
    this.installAskHandlers();
  }

  /** Register the engine reverse-RPC ask handlers on the transport. The
   *  handlers read instance state dynamically (lastSessionId / parked asks /
   *  handlers), so a single constructor-time registration serves every later
   *  connectEvents call; re-running it simply replaces the previous handler. */
  private installAskHandlers(): void {
    this.client.registerReverseRpc('approval.ask', (params: unknown) => {
      return new Promise<Record<string, unknown>>((resolve, reject) => {
        const sessionId = this.lastSessionId ?? '';
        const approval = approvalRequestFromAskParams(params, sessionId);
        if (approval === null) {
          reject(new Error('invalid approval.ask params'));
          return;
        }
        this.approvalAsks.park(approval.approvalId, (response) =>
          resolve(approvalResponseToEngineWire(response)),
        );
        this.emitAskEvent({ type: 'approvalRequested', sessionId, approval }, sessionId);
      });
    });
    this.client.registerReverseRpc('question.ask', (params: unknown) => {
      return new Promise<Record<string, unknown>>((resolve, reject) => {
        const sessionId = this.lastSessionId ?? '';
        const question = questionRequestFromAskParams(params, sessionId);
        if (question === null) {
          reject(new Error('invalid question.ask params'));
          return;
        }
        this.questionAsks.park(question.questionId, (response) =>
          resolve(questionResponseToEngineWire(response)),
        );
        this.emitAskEvent({ type: 'questionRequested', sessionId, question }, sessionId);
      });
    });
  }

  /** Deliver one ask card to the UI — buffered when connectEvents has not
   *  bound handlers yet (the buffer is flushed when it does). */
  private emitAskEvent(appEvent: AppEvent, sessionId: string): void {
    if (this.handlers === null) {
      this.pendingAskEvents.push({ appEvent, sessionId });
      return;
    }
    this.handlers.onEvent(appEvent, { sessionId, seq: 0 });
  }

  /** ADR-011 e (审批连接无关性): after a (re)subscribe, rebuild the approval
   *  cards from the gate's read face — `session.snapshot` back-fills the
   *  pending approvals, so a reconnected (or freshly loaded) client
   *  restores its `approvalsBySession` even though the pre-disconnect
   *  `approval.ask` notification died with the connection. Emitted through
   *  the existing `approvalRequested` reducer shape WITHOUT parking an
   *  answerable reverse-RPC responder: deciding a rebuilt card falls
   *  through to `approval.decide`, which resolves the engine's gate (the
   *  first-one-wins source of truth). Best-effort — a failed snapshot read
   *  is surfaced through the connection error handler, never fatal (the
   *  server-side replay and the live event stream still cover the common
   *  paths). */
  private async resyncPendingApprovals(sessionId: string): Promise<void> {
    try {
      const snapshot = await this.client.call<RpcSnapshot>('session.snapshot', {
        session_id: sessionId,
      });
      if (!snapshot) return;
      for (const event of pendingApprovalEventsFromSnapshot(snapshot, sessionId)) {
        // A pending row's parked responder (if any) is STALE by definition:
        // either it was parked by a pre-disconnect ask whose delivery died
        // with the old connection (answering it would resolve a server-side
        // one-shot nobody waits on — the decision would be lost), or it
        // belongs to an ask the engine is in the middle of re-delivering.
        // The gate is the first-one-wins arbiter either way, so drop the
        // parked responder and let answering fall through to
        // `approval.decide` — the exact read/decide surface a reconnected
        // client resolves through.
        this.approvalAsks.drop(event.approval.approvalId);
        this.emitAskEvent(event, sessionId);
      }
    } catch (error) {
      const message = error instanceof JsonRpcCallError ? error.message : String(error);
      this.handlers?.onError(0, `approval resync: ${message}`, false);
    }
  }

  /** Drop parked ask entries whose request was resolved or expired through
   *  another path — another client decided (the engine's first-one-wins gate
   *  will never read our reply) or the engine timed the ask out. The parked
   *  promise is left unresolved; the engine has already moved on. */
  private dropParkedAskForResolution(event: RpcEvent): void {
    switch (event.type) {
      case 'event.approval.resolved':
      case 'event.approval.expired': {
        const requestId = resolutionEventRequestId(event.payload);
        if (requestId !== null) this.approvalAsks.drop(requestId);
        return;
      }
      case 'event.question.answered':
      case 'event.question.dismissed': {
        const requestId = resolutionEventRequestId(event.payload);
        if (requestId !== null) this.questionAsks.drop(requestId);
        return;
      }
    }
  }

  /** Build a fetch Request init with the Bearer token, when one is available. */
  private authHeaders(): Record<string, string> {
    const credential = getCredential();
    return credential !== undefined ? { Authorization: `Bearer ${credential}` } : {};
  }

  /** Append the token as a query parameter for browser-native URL contexts
   *  (<img src>, <a download>) that cannot set Authorization headers. */
  private withTokenQuery(url: string): string {
    const credential = getCredential();
    if (credential === undefined) return url;
    const separator = url.includes('?') ? '&' : '?';
    return `${url}${separator}token=${encodeURIComponent(credential)}`;
  }

  /** Transport-ready promise: performs the server_hello/client_hello handshake.
   *  If the WebSocket upgrade is rejected (missing/wrong token → 401 from the
   *  server), clears the stale credential so the UI shows the auth modal. */
  async connect(): Promise<void> {
    try {
      await this.client.connect();
    } catch (error) {
      // The browser does not surface a 401 on WS upgrade as an HTTP status —
      // the socket simply fails to open. If we had a credential but the
      // upgrade still failed, it is likely stale; clear it so the user is
      // prompted to re-enter one.
      if (getCredential() !== undefined) {
        markAuthRequired();
      }
      throw error;
    }
  }

  /** `MirriWebApi.connectTransport` — the eager boot-time connect (the rust
   *  engine's WebSocket). Returns the same promise connect() does, so a
   *  racing connectEvents shares one handshake. */
  async connectTransport(): Promise<void> {
    return this.connect();
  }

  /** Close the transport. The same instance can connect() again afterwards. */
  close(): void {
    this.unsubscribeResync?.();
    this.unsubscribeResync = null;
    this.unsubscribeAnyEvent?.();
    this.unsubscribeAnyEvent = null;
    // Parked asks can never be answered over a closed transport — drop them
    // (and any cards still waiting for a connectEvents that never came) so
    // state does not leak across close/connect cycles.
    this.approvalAsks.clear();
    this.questionAsks.clear();
    this.pendingAskEvents = [];
    // A terminal surface's sinks die with the transport.
    this.terminalSinks = null;
    this.client.close();
  }

  /**
   * Subscribe to raw protocol events of one type (e.g. "assistant.delta").
   * This is the transport-level channel below the AppEvent projection; the
   * projection from RpcEvent → AppEvent (agentEventProjector/eventReducer)
   * is a later migration step. Returns the unsubscribe function.
   */
  onRpcEvent(type: string, handler: (event: RpcEvent) => void): () => void {
    return this.client.on(type, handler);
  }

  // -------------------------------------------------------------------------
  // Sessions
  // -------------------------------------------------------------------------

  async listSessions(
    input?: PageRequest & {
      busy?: boolean;
      workspaceId?: string;
      includeArchive?: boolean;
      archivedOnly?: boolean;
      excludeEmpty?: boolean;
    },
  ): Promise<Page<AppSession>> {
    // W5 unseal — the engine's `session.list` now carries the v2 filter
    // face (the kap-server list route's parity): the alias-folded
    // workspace filter, the SDR-004 `exclude_empty` (default true, REST
    // parity — a never-conversed session is hidden; an explicit false
    // forwards like the REST client does), the keyset cursors,
    // and the busy/archive filters. Params go out snake_case (the REST
    // client's exact spellings); the response is the page envelope
    // `{items, has_more}`.
    const params: Record<string, unknown> = {
      exclude_empty: input?.excludeEmpty ?? true,
    };
    if (input?.workspaceId !== undefined) params['workspace_id'] = input.workspaceId;
    if (input?.includeArchive !== undefined) params['include_archive'] = input.includeArchive;
    if (input?.archivedOnly !== undefined) params['archived_only'] = input.archivedOnly;
    if (input?.busy !== undefined) params['busy'] = input.busy;
    if (input?.beforeId !== undefined) params['before_id'] = input.beforeId;
    if (input?.afterId !== undefined) params['after_id'] = input.afterId;
    if (input?.pageSize !== undefined) params['page_size'] = input.pageSize;
    const page = await this.client.call<RpcSessionListPage>('session.list', params);
    return {
      items: (page.items ?? []).map(toAppSession),
      hasMore: page.has_more === true,
    };
  }

  /** `session.search` — the rust engine's global literal search (the P10
   *  read face). Params go out in the method's snake_case wire spelling;
   *  the hits come back snake_case and map onto the app's camelCase
   *  vocabulary. Optional on `MirriWebApi` — this transport is the only
   *  implementor. */
  async searchSessions(input: {
    query: string;
    sessionId?: string;
    role?: 'user' | 'assistant' | 'title';
    pageSize?: number;
    pageToken?: string;
  }): Promise<AppSessionSearchPage> {
    const params: Record<string, unknown> = { query: input.query };
    if (input.sessionId !== undefined) params['container'] = { session_id: input.sessionId };
    if (input.role !== undefined) params['role'] = input.role;
    if (input.pageSize !== undefined) params['page_size'] = input.pageSize;
    if (input.pageToken !== undefined) params['page_token'] = input.pageToken;
    const page = await this.client.call<RpcSessionSearchPage>('session.search', params);
    return {
      hits: page.hits.map((hit) => ({
        sessionId: hit.session_id,
        workspaceId: hit.workspace_id,
        sessionTitle: hit.session_title,
        agentId: hit.agent_id,
        role: hit.role,
        snippet: hit.snippet,
        time: hit.time,
        ...(hit.turn !== undefined ? { turn: hit.turn } : {}),
      })),
      mode: page.mode === 'terms' ? 'terms' : 'literal',
      ...(page.next_page_token !== undefined ? { nextPageToken: page.next_page_token } : {}),
    };
  }

  async createSession(input: {
    title?: string;
    cwd?: string;
    model?: string;
    workspaceId?: string;
  }): Promise<AppSession> {
    const params: Record<string, unknown> = {};
    if (input.title !== undefined) params['title'] = input.title;
    // `modelId` seeds the session's main agent scope (request's explicit
    // model → config default_model → the engine's built-in default); the
    // pick reaches the LLM factory at the first turn.
    if (input.model !== undefined) params['modelId'] = input.model;
    // W4 unseal — the v2 `session.create` workspace routing face:
    // `workspace_id` selects the registered workspace the session is
    // created in, paired with `metadata.cwd` — the same dual-send the
    // REST client uses (client.ts POST /sessions), so the session lands
    // in the composer's selected workspace. An unknown id is rejected by
    // the engine (invalid params), never silently re-routed; absent
    // fields keep the server's default workspace.
    if (input.workspaceId !== undefined) params['workspace_id'] = input.workspaceId;
    params['metadata'] = input.cwd !== undefined ? { cwd: input.cwd } : {};
    const view = await this.client.call<RpcSessionView>('session.create', params);
    return toAppSession(view);
  }

  async getSession(sessionId: string): Promise<AppSession> {
    const details = await this.client.call<RpcSessionDetails>('session.get', {
      session_id: sessionId,
    });
    return toAppSession(details);
  }

  /** Close a session (maps to `session.close`, v2 terminology). Additive —
   *  MirriWebApi has no closeSession member. */
  async closeSession(sessionId: string): Promise<{ ok: true }> {
    await this.client.call<{ ok: boolean }>('session.close', { session_id: sessionId });
    return { ok: true };
  }

  async getSessionStatus(sessionId: string): Promise<AppSessionRuntimeStatus> {
    const status = await this.client.call<RpcSessionStatus>('session.status', { sessionId });
    // `session.status` exposes model/permission/planMode/running plus the
    // P7c runtime fields (thinkingEffort/swarmMode/contextTokens) and the
    // model's context window (maxContextTokens — the config model catalog
    // resolves the agent's model; absent on older engines → 0, the unknown
    // spelling the composer ring renders as no ceiling).
    return {
      model: status.model && status.model.length > 0 ? status.model : null,
      thinkingEffort: typeof status.thinkingEffort === 'string' ? status.thinkingEffort : '',
      permission: typeof status.permission === 'string' ? status.permission : '',
      planMode: status.planMode === true,
      swarmMode: status.swarmMode === true,
      collaborationMode: status.collaborationMode === true,
      contextTokens: typeof status.contextTokens === 'number' ? status.contextTokens : 0,
      maxContextTokens:
        typeof status.maxContextTokens === 'number' ? status.maxContextTokens : 0,
      contextUsage: 0,
    };
  }

  // -------------------------------------------------------------------------
  // Messages
  // -------------------------------------------------------------------------

  async listMessages(
    sessionId: string,
    input?: PageRequest & { role?: AppMessageRole; agentId?: string },
  ): Promise<Page<AppMessage>> {
    const params: Record<string, unknown> = { sessionId };
    if (input?.beforeId !== undefined) params['beforeId'] = input.beforeId;
    if (input?.afterId !== undefined) params['afterId'] = input.afterId;
    if (input?.pageSize !== undefined) params['pageSize'] = input.pageSize;
    // Supervision A2: address a member agent's own journal — the rust
    // `message.list` member-scoped read face (agentId / agent_id alias).
    if (input?.agentId !== undefined) params['agentId'] = input.agentId;
    // v2 message.list has no role filter; input.role is dropped.
    void input?.role;
    const result = await this.client.call<RpcMessageListResult>('message.list', params);
    return {
      items: result.messages.map((message) => toAppMessage(message, sessionId)),
      hasMore: result.hasMore === true,
    };
  }

  async submitPrompt(sessionId: string, input: PromptSubmission): Promise<PromptSubmitResult> {
    // The rust `message.send` accepts structured `input` parts: the v1 TS
    // REST content spellings (text / image / video / file — see
    // promptContentToInputParts), with file references resolved server-side
    // at the entry. Parts without a prompt wire form (tool uses, thinking)
    // are dropped, exactly like the pre-P4 flattening dropped them.
    const params: Record<string, unknown> = {
      session_id: sessionId,
      input: promptContentToInputParts(input.content),
      mode: 'fire',
    };
    if (input.agentId !== undefined) params['agent_id'] = input.agentId;
    // The per-prompt model override: applied to the agent's live scope
    // config before the LLM is created (idempotent in the common case —
    // the web sends the session's current model on every prompt).
    if (input.model !== undefined) params['model'] = input.model;
    // Per-prompt mode fields are INTENTIONALLY dropped on the v3 channel: the
    // rust message.send has no per-prompt mode deserialization, so mode state
    // must flow exclusively through its dedicated RPCs — swarm.enter/exit,
    // collaboration.enter/exit, plan.enter/cancel, agent.setPermission,
    // goal.create/pause/resume/cancel (routed by updateSession below). The
    // v1/v2 REST channel still consumes swarm_mode per-prompt; this strip is
    // v3-only by construction (WsRpcApi is the v3 transport). The satisfies-
    // pinned list makes the drop explicit (D-26) and fails to compile if a
    // listed field disappears from PromptSubmission — a removed/renamed
    // per-prompt control must be re-decided here, not silently kept.
    const droppedV3PerPromptFields = {
      thinking: input.thinking,
      permissionMode: input.permissionMode,
      planMode: input.planMode,
      swarmMode: input.swarmMode,
      collaborationMode: input.collaborationMode,
      goalObjective: input.goalObjective,
      goalControl: input.goalControl,
    } satisfies Record<
      keyof Pick<
        PromptSubmission,
        | 'thinking'
        | 'permissionMode'
        | 'planMode'
        | 'swarmMode'
        | 'collaborationMode'
        | 'goalObjective'
        | 'goalControl'
      >,
      unknown
    >;
    void droppedV3PerPromptFields;
    const result = await this.client.call<RpcMessageSendResult>('message.send', params);
    // v2 `message.send` returns {turn_id, agent_id} and no prompt/user-message
    // ids: the turn id is the correlation id for the event stream, and the
    // user message arrives as a messageCreated event. `userMessageId` has no
    // v2 source yet — empty string tells callers no id is available.
    return {
      promptId: result.turn_id,
      userMessageId: '',
      status: 'running',
    };
  }

  async abortPrompt(sessionId: string, promptId: string): Promise<{ aborted: boolean; atSeq?: number }> {
    // v2 aborts by turnId — submitPrompt returns the turnId as promptId, so
    // the caller can hand it straight back. A NotFound answer (turn already
    // finished) mirrors the REST path's "already completed → aborted: false".
    try {
      await this.client.call<{ ok: boolean }>('message.abort', {
        sessionId,
        turnId: promptId,
      });
      return { aborted: true };
    } catch (error) {
      if (error instanceof JsonRpcCallError && error.code === NOT_FOUND_CODE) {
        return { aborted: false };
      }
      throw error;
    }
  }

  // -------------------------------------------------------------------------
  // Approval / Question
  // -------------------------------------------------------------------------

  async respondApproval(
    sessionId: string,
    approvalId: string,
    response: ApprovalResponse,
  ): Promise<{ resolved: true; resolvedAt: string }> {
    // Parked reverse-RPC ask wins: answer the exact `approval.ask` that
    // produced the card. The engine's permission gate is first-resolution-
    // wins, so when no ask is parked (other client already decided, or the
    // ask was protocol-event-only) fall through to `approval.decide`.
    const respondParked = this.approvalAsks.take(approvalId);
    if (respondParked !== undefined) {
      respondParked(response);
      return { resolved: true, resolvedAt: new Date().toISOString() };
    }
    const reason = response.feedback ?? response.selectedLabel;
    try {
      await this.client.call<{ ok: boolean }>('approval.decide', {
        session_id: sessionId,
        request_id: approvalId,
        approved: response.decision === 'approved',
        ...(reason !== undefined ? { reason } : {}),
        ...(response.scope !== undefined ? { scope: response.scope } : {}),
      });
    } catch (error) {
      // First-one-wins gate: when another client (or the ask's own reverse
      // reply) already resolved this request, the engine answers NotFound.
      // The decision HAS taken effect — idempotent success, not a failure.
      if (error instanceof JsonRpcCallError && error.code === NOT_FOUND_CODE) {
        return { resolved: true, resolvedAt: new Date().toISOString() };
      }
      throw error;
    }
    // The rust server's approval.decide answers `{ok: true}` with no
    // timestamp; the client stamps resolvedAt so the v1 contract holds.
    return { resolved: true, resolvedAt: new Date().toISOString() };
  }

  async respondQuestion(
    sessionId: string,
    questionId: string,
    response: QuestionResponse,
  ): Promise<{ resolved: true; resolvedAt: string }> {
    // Parked reverse-RPC ask first (same first-resolution-wins semantics as
    // respondApproval); fall back to the `question.answer` RPC.
    const respondParked = this.questionAsks.take(questionId);
    if (respondParked !== undefined) {
      respondParked(response);
      return { resolved: true, resolvedAt: new Date().toISOString() };
    }
    // v2 question.answer takes ONE `answer` string per request; the v1
    // response is a per-question map. The first answer wins.
    const firstAnswer = Object.values(response.answers)[0];
    const answer = firstAnswer === undefined ? '' : questionAnswerToString(firstAnswer);
    try {
      await this.client.call<{ ok: boolean }>('question.answer', {
        session_id: sessionId,
        request_id: questionId,
        answer,
      });
    } catch (error) {
      // Same first-one-wins idempotency as respondApproval: a NotFound answer
      // means the ask was already resolved (or expired) elsewhere.
      if (error instanceof JsonRpcCallError && error.code === NOT_FOUND_CODE) {
        return { resolved: true, resolvedAt: new Date().toISOString() };
      }
      throw error;
    }
    return { resolved: true, resolvedAt: new Date().toISOString() };
  }

  // -------------------------------------------------------------------------
  // Events — one connection, no separate WS
  // -------------------------------------------------------------------------

  connectEvents(handlers: MirriEventHandlers): MirriEventConnection {
    this.handlers = handlers;
    this.unsubscribeResync?.();
    this.unsubscribeAnyEvent?.();

    // Eager transport connect — the implied connectEvents contract the TS
    // backend honours (DaemonMirriWebApi calls socket.connect() at handler
    // bind time). Without it a fresh browser with no selected session would
    // never open the RPC WebSocket (connectEventsIfNeeded only fires on
    // session selection), so every command call would be rejected with
    // "not connected". Failures surface through the transport's onError
    // wiring into these handlers; a stale credential is cleared by connect().
    void this.connect().catch(() => undefined);

    // Flush ask cards that arrived between the (constructor-time) reverse
    // RPC registration and this call — the engine can ask as soon as the
    // handshake completes, before the UI binds its handlers.
    const buffered = this.pendingAskEvents;
    this.pendingAskEvents = [];
    for (const { appEvent, sessionId } of buffered) {
      handlers.onEvent(appEvent, { sessionId, seq: 0 });
    }

    // v2 cursor-resync signal
    this.unsubscribeResync = this.client.on('resync_required', (event: RpcEvent) => {
      if (event.sessionId !== undefined) {
        handlers.onResync(event.sessionId, event.seq ?? 0, event.epoch);
      }
    });

    // Terminal stream — the server's `terminal_output` / `terminal_exit`
    // notifications (the unified endpoint dispatches every unknown
    // notification method as a direct event, so these surface as RpcEvents
    // with the snake_case payload and `seq` lifted into the envelope). The
    // v1 multiplexed endpoint's frame shapes, carried over the JSON-RPC
    // transport.
    //
    // Gap detection (F-P3-4): the server queues live terminal frames with
    // `try_send` and silently drops them when its outbox fills, so the
    // client tracks each terminal's seq cursor itself. A regression
    // (seq <= cursor) is a duplicate re-delivery (replay racing a
    // replacement attach) and is dropped; a jump (seq > cursor + 1) is a
    // lost stretch — the frame is held and `terminal_attach` is re-sent at
    // the cursor so the server's replay re-delivers the missing frames in
    // order (the held copy then drops out as the duplicate). A recovery
    // that cannot fill the gap (the server's bounded replay buffer
    // evicted the frames, or the re-attach failed) gives up after the
    // window below and flushes what it holds — degraded but live.
    const TERMINAL_GAP_RECOVERY_TIMEOUT_MS = 5000;
    interface TerminalStreamCursor {
      lastSeq: number;
      recovering: boolean;
      pending: Array<{ data: string; seq: number }>;
      flushTimer: ReturnType<typeof setTimeout> | null;
    }
    const terminalStreams = new Map<string, TerminalStreamCursor>();
    const terminalStreamKey = (sessionId: string, terminalId: string): string =>
      `${sessionId}\0${terminalId}`;

    const dispatchTerminalOutput = (
      sessionId: string,
      terminalId: string,
      data: string,
      seq: number,
    ): void => {
      // A dedicated connectTerminalEvents() binding owns the terminal stream;
      // otherwise the terminal_* handler slots of the connectEvents() caller
      // apply (backward compatible — the app itself passes none).
      const sinks = this.terminalSinks;
      if (sinks !== null) {
        sinks.onTerminalOutput(sessionId, terminalId, data, seq);
      } else {
        handlers.onTerminalOutput?.(sessionId, terminalId, data, seq);
      }
    };

    /** Route a terminal-surface error to the dedicated sinks when bound,
     *  else to the connectEvents() caller's error handler. */
    const reportTerminalError = (msg: string): void => {
      const sinks = this.terminalSinks;
      if (sinks?.onError !== undefined) {
        sinks.onError(0, msg, false);
      } else {
        handlers.onError(0, msg, false);
      }
    };

    /** Give up on the in-flight gap recovery: flush the held frames in seq
     * order (the missing stretch is unrecoverable) and continue from the
     * highest seq seen. Returns the flushed frames, in delivery order. */
    const flushHeldTerminalFrames = (stream: TerminalStreamCursor): Array<{ data: string; seq: number }> => {
      if (stream.flushTimer !== null) {
        clearTimeout(stream.flushTimer);
        stream.flushTimer = null;
      }
      stream.recovering = false;
      const pending = stream.pending.splice(0).toSorted((a, b) => a.seq - b.seq);
      for (const frame of pending) {
        stream.lastSeq = Math.max(stream.lastSeq, frame.seq);
      }
      return pending;
    };

    this.unsubscribeTerminalOutput?.();
    this.unsubscribeTerminalExit?.();
    this.unsubscribeTerminalOutput = this.client.on('terminal_output', (event: RpcEvent) => {
      const payload = event.payload as
        | { session_id?: unknown; terminal_id?: unknown; data?: unknown }
        | null
        | undefined;
      const payloadFields = payload !== null && typeof payload === 'object' ? payload : {};
      const sessionId =
        typeof payloadFields.session_id === 'string' ? payloadFields.session_id : event.sessionId;
      const terminalId =
        typeof payloadFields.terminal_id === 'string' ? payloadFields.terminal_id : undefined;
      const data = typeof payloadFields.data === 'string' ? payloadFields.data : '';
      if (sessionId === undefined || terminalId === undefined) return;
      const seq = event.seq ?? 0;
      const key = terminalStreamKey(sessionId, terminalId);
      let stream = terminalStreams.get(key);
      if (stream === undefined) {
        // First frame from this terminal: start the cursor at it (no gap
        // detection before a cursor exists — a fresh attach replays from
        // wherever the terminal's bounded buffer starts).
        terminalStreams.set(key, { lastSeq: seq, recovering: false, pending: [], flushTimer: null });
        dispatchTerminalOutput(sessionId, terminalId, data, seq);
        return;
      }
      if (seq <= stream.lastSeq) {
        // Regression: duplicate re-delivery — drop.
        return;
      }
      if (seq === stream.lastSeq + 1) {
        stream.lastSeq = seq;
        if (stream.pending.length > 0) {
          // The recovery replay re-delivers held frames' seqs: those
          // copies are the ordered ones, so the held copies drop out.
          stream.pending = stream.pending.filter((frame) => frame.seq > stream.lastSeq);
          if (stream.pending.length === 0 && stream.recovering && stream.flushTimer !== null) {
            clearTimeout(stream.flushTimer);
            stream.flushTimer = null;
            stream.recovering = false;
          }
        }
        dispatchTerminalOutput(sessionId, terminalId, data, seq);
        return;
      }
      // Gap: hold the frame and re-attach at the cursor for the delta.
      stream.pending.push({ data, seq });
      if (!stream.recovering) {
        stream.recovering = true;
        void this.client
          .call('terminal_attach', {
            session_id: sessionId,
            terminal_id: terminalId,
            since_seq: stream.lastSeq,
          })
          .catch((error: unknown) => {
            reportTerminalError(`terminal gap recovery failed: ${String(error)}`);
          });
        // If no replay fills the gap within the window, flush the held
        // frames and continue from the new cursor.
        stream.flushTimer = setTimeout(() => {
          stream.flushTimer = null;
          if (!stream.recovering) return;
          for (const frame of flushHeldTerminalFrames(stream)) {
            dispatchTerminalOutput(sessionId, terminalId, frame.data, frame.seq);
          }
        }, TERMINAL_GAP_RECOVERY_TIMEOUT_MS);
      }
    });
    this.unsubscribeTerminalExit = this.client.on('terminal_exit', (event: RpcEvent) => {
      const payload = event.payload as
        | { session_id?: unknown; terminal_id?: unknown; exit_code?: unknown }
        | null
        | undefined;
      const payloadFields = payload !== null && typeof payload === 'object' ? payload : {};
      const sessionId =
        typeof payloadFields.session_id === 'string' ? payloadFields.session_id : event.sessionId;
      const terminalId =
        typeof payloadFields.terminal_id === 'string' ? payloadFields.terminal_id : undefined;
      const exitCode =
        typeof payloadFields.exit_code === 'number' ? payloadFields.exit_code : null;
      if (sessionId === undefined || terminalId === undefined) return;
      // The exit settles any in-flight gap recovery: what was held flushes
      // ahead of the exit (the terminal will stream nothing more).
      const stream = terminalStreams.get(terminalStreamKey(sessionId, terminalId));
      if (stream !== undefined && (stream.recovering || stream.pending.length > 0)) {
        for (const frame of flushHeldTerminalFrames(stream)) {
          dispatchTerminalOutput(sessionId, terminalId, frame.data, frame.seq);
        }
      }
      const sinks = this.terminalSinks;
      if (sinks !== null) {
        sinks.onTerminalExit(sessionId, terminalId, exitCode);
      } else {
        handlers.onTerminalExit?.(sessionId, terminalId, exitCode);
      }
    });

    // Catch-all event handler: route every RpcEvent through the shared
    // classifier (agent-core vs protocol vs control frame), then feed the
    // resulting AppEvents to the UI. The rust engine frames events in the
    // direct form — the notification method IS the `event.<type>` name —
    // and the projector's vocabulary is unprefixed, so the prefix must be
    // classified away here (see ws-rpc-event-bridge).
    this.unsubscribeAnyEvent = this.client.onAnyEvent((event: RpcEvent) => {
      // W2 unseal: session-scoped events keep the last-session tracking;
      // the server-scoped `event.workspace.*` lifecycle frames carry the
      // synthesized empty session id (or none at all) and must NOT clobber
      // the last-session pointer the approval/question ask routing reads.
      if (event.sessionId === undefined) {
        if (!event.type.startsWith('event.workspace.')) return;
      } else if (event.sessionId !== '') {
        this.lastSessionId = event.sessionId;
      }
      this.dropParkedAskForResolution(event);
      const appEvents = routeRpcEventToAppEvents(this.projector, event);
      const seq = event.seq ?? 0;
      // The server-scoped events carry the empty session id — the
      // synthesized meta keeps `sessionId` a string for every consumer
      // (the workspace lifecycle handler ignores it).
      const metaSessionId = event.sessionId ?? '';
      for (const appEvent of appEvents) {
        handlers.onEvent(appEvent, { sessionId: metaSessionId, seq });
      }
    });

    // Engine reverse RPC — the primary HITL path (the approval.ask /
    // question.ask handlers are installed at construction; see
    // installAskHandlers). The rust engine asks the client to approve tool
    // executions and answer AskUserQuestion prompts; an unanswered ask
    // stalls the turn until the engine timeout. The handler presents the
    // full-fidelity card through the same AppEvent stream and parks the
    // response so respondApproval / respondQuestion can answer the exact ask
    // that produced it.

    const client = this.client;
    const report = (action: string, error: unknown): void => {
      if (error instanceof JsonRpcCallError) {
        handlers.onError(error.code, `${action}: ${error.message}`, false);
      } else {
        handlers.onError(0, `${action}: ${String(error)}`, false);
      }
    };
    /** Terminal control-frame failures surface on the terminal surface (the
     *  sinks bound by connectTerminalEvents) when one is mounted, else on the
     *  connectEvents() caller's error handler. */
    const reportTerminal = (action: string, error: unknown): void => {
      const sinks = this.terminalSinks;
      if (sinks?.onError !== undefined) {
        const msg =
          error instanceof JsonRpcCallError ? `${action}: ${error.message}` : `${action}: ${String(error)}`;
        sinks.onError(error instanceof JsonRpcCallError ? error.code : 0, msg, false);
        return;
      }
      report(action, error);
    };

    this.eventConnection = {
      subscribe: (sessionId: string, cursor?: { seq: number; epoch?: string }) => {
        const params: Record<string, unknown> = { sessionId };
        if (cursor !== undefined) {
          params['cursor'] = { seq: cursor.seq, ...(cursor.epoch !== undefined ? { epoch: cursor.epoch } : {}) };
        }
        // ADR-011 e: after the (re)subscribe lands, rebuild the approval
        // cards from the gate's read face so a reconnect/fresh load
        // restores them without depending on pre-disconnect notifications.
        void client
          .call('session.subscribe', params)
          .then(() => this.resyncPendingApprovals(sessionId))
          .catch((error) => report('session.subscribe', error));
      },
      unsubscribe: (sessionId: string) => {
        void client
          .call('session.unsubscribe', { sessionId })
          .catch((error) => report('session.unsubscribe', error));
      },
      abort: (sessionId: string, promptId: string) => {
        void client
          .call('message.abort', { sessionId, turnId: promptId })
          .catch((error) => report('abort', error));
      },
      health: () => client.health(),
      reconnect: () => {
        void client.connect().catch((error) => report('reconnect', error));
      },
      close: () => {
        this.close();
      },
      bindNextPromptId: (sessionId: string, promptId: string) => {
        this.projector.bindNextPromptId(sessionId, promptId);
      },
      turnUsage: (sessionId: string, promptId: string) =>
        turnUsageForPromptId(this.projector.state(sessionId), promptId),
      seedSnapshot: (sessionId: string, snapshot: AppSessionSnapshot) => {
        this.projector.reset(sessionId);
        if (snapshot.inFlightTurn !== null) {
          const seedEvents = this.projector.seedInFlight(sessionId, snapshot.inFlightTurn);
          const seq = snapshot.asOfSeq;
          for (const appEvent of seedEvents) {
            handlers.onEvent(appEvent, { sessionId, seq });
          }
        }
        // Refresh-regression fix + SDR-004: seed the usage accumulators from
        // the snapshot so the first live sessionUsageUpdated continues from
        // the snapshot's totals instead of zeroing them. The placeholder
        // guard keeps engines that answer all-zero placeholders from
        // zeroing genuinely-live accumulators.
        if (!isPlaceholderSessionUsage(snapshot.session.usage)) {
          this.projector.seedUsage(sessionId, snapshot.session.usage);
        }
        // Seed per-turn usage AFTER the in-flight branch: seedInFlight performs
        // its own internal reset, which would wipe usage seeded before it. This
        // order keeps prior-turn usage visible on reload even while a live turn
        // is still streaming (the same snapshot carries both).
        if (snapshot.turnUsages !== undefined && snapshot.turnUsages.length > 0) {
          this.projector.seedTurnUsage(
            sessionId,
            snapshot.turnUsages.map((entry) => ({
              promptId: entry.promptId,
              usage: {
                inputTokens: entry.inputTokens,
                outputTokens: entry.outputTokens,
                cacheReadTokens: entry.cacheReadTokens,
                cacheCreationTokens: entry.cacheCreationTokens,
                models: entry.models,
              },
            })),
          );
        }
      },
      markSideChannelAgent: (agentId: string) => {
        this.projector.markSideChannelAgent(agentId);
      },
      // Terminal stream control — connection-local methods on the unified
      // endpoint (the same names and snake_case payloads the v1 multiplexed
      // endpoint's terminal_* frames carry). Fire-and-forget like the v1
      // client: errors surface through the connection error handler.
      terminalAttach: (sessionId: string, terminalId: string, sinceSeq?: number) => {
        const params: Record<string, unknown> = {
          session_id: sessionId,
          terminal_id: terminalId,
        };
        if (sinceSeq !== undefined && sinceSeq > 0) {
          params['since_seq'] = sinceSeq;
        }
        void client
          .call('terminal_attach', params)
          .catch((error) => reportTerminal('terminal_attach', error));
      },
      terminalInput: (sessionId: string, terminalId: string, data: string) => {
        void client
          .call('terminal_input', { session_id: sessionId, terminal_id: terminalId, data })
          .catch((error) => reportTerminal('terminal_input', error));
      },
      terminalResize: (sessionId: string, terminalId: string, cols: number, rows: number) => {
        void client
          .call('terminal_resize', { session_id: sessionId, terminal_id: terminalId, cols, rows })
          .catch((error) => reportTerminal('terminal_resize', error));
      },
      terminalDetach: (sessionId: string, terminalId: string) => {
        // The stream's seq cursor dies with the attachment: a later
        // re-attach replays from its own cursor, so frames that raced the
        // detach must not be suppressed as regressions.
        const stream = terminalStreams.get(terminalStreamKey(sessionId, terminalId));
        if (stream !== undefined) {
          if (stream.flushTimer !== null) clearTimeout(stream.flushTimer);
          terminalStreams.delete(terminalStreamKey(sessionId, terminalId));
        }
        void client
          .call('terminal_detach', { session_id: sessionId, terminal_id: terminalId })
          .catch((error) => reportTerminal('terminal_detach', error));
      },
      terminalClose: (sessionId: string, terminalId: string) => {
        const stream = terminalStreams.get(terminalStreamKey(sessionId, terminalId));
        if (stream !== undefined) {
          if (stream.flushTimer !== null) clearTimeout(stream.flushTimer);
          terminalStreams.delete(terminalStreamKey(sessionId, terminalId));
        }
        void client
          .call('terminal_close', { session_id: sessionId, terminal_id: terminalId })
          .catch((error) => reportTerminal('terminal_close', error));
      },
    };
    return this.eventConnection;
  }

  /**
   * Bind terminal-stream sinks additively on the shared event transport —
   * WITHOUT rebinding the app's connectEvents() handler set (this api is a
   * single-connection design: a second connectEvents() call would replace
   * the app's handlers, and the returned connection's close() would tear
   * down the transport the app's event stream runs on).
   *
   * The returned connection shares the app connection's control-frame
   * surface; its close() releases ONLY the terminal sinks and never closes
   * the transport.
   */
  connectTerminalEvents(sinks: TerminalEventSinks): MirriEventConnection {
    this.terminalSinks = sinks;
    // The terminal stream subscriptions live inside connectEvents(); when
    // the app has not bound its handlers yet (no session selected), bind
    // inert no-op handlers so the subscriptions + transport exist — there
    // are no app handlers to clobber in that state. When the app later
    // binds its real handlers, the terminal dispatch below still reads
    // this.terminalSinks, so nothing is lost.
    if (this.handlers === null) {
      // connectEvents flushes pendingAskEvents into its caller's onEvent;
      // the inert bind must not consume them — park them across the call so
      // the app's real connectEvents still delivers the ask cards.
      const parkedAsks = this.pendingAskEvents;
      this.pendingAskEvents = [];
      this.connectEvents({
        onEvent: () => {},
        onResync: () => {},
        onError: () => {},
        onConnectionChange: () => {},
      });
      this.pendingAskEvents = parkedAsks;
    } else {
      void this.connect().catch(() => undefined);
    }
    const base = this.eventConnection;
    if (base === null) {
      // Unreachable: connectEvents() above always builds the connection.
      throw new Error('WsRpcApi.connectTerminalEvents: no event connection');
    }
    const releaseSinks = (): void => {
      // Identity check: only the CURRENT binder may release — a stale
      // binding must not unbind a newer one.
      if (this.terminalSinks === sinks) this.terminalSinks = null;
    };
    return {
      ...base,
      close: releaseSinks,
    };
  }

  // -------------------------------------------------------------------------
  // Health / Meta (REST fallbacks)
  // -------------------------------------------------------------------------

  /**
   * Fetch one REST route and unwrap the rust server's
   * `{code, msg, data, request_id}` envelope: a non-zero `code` throws the
   * shared `DaemonApiError`; otherwise the route's `data` payload returns.
   */
  private async restCall<T>(path: string, init?: RequestInit): Promise<T> {
    // Callers pass at most a plain record of headers (never the array or
    // Headers forms of HeadersInit), so the record spread stays sound.
    const initHeaders = init?.headers as Record<string, string> | undefined;
    const headers = { ...this.authHeaders(), ...initHeaders };
    // A JSON body must declare its Content-Type: the rust server's axum `Json`
    // extractor answers HTTP 415 when the header is missing, so every
    // body-carrying call (terminal create/close) labels itself. Only set when
    // the caller did not bring its own (case-insensitive) content type.
    if (
      init?.body !== undefined &&
      !Object.keys(headers).some((name) => name.toLowerCase() === 'content-type')
    ) {
      headers['Content-Type'] = 'application/json';
    }
    const res = await fetch(buildRestUrl(this.origin, path), { ...init, headers });
    if (!res.ok) {
      if (res.status === 401) markAuthRequired();
      throw new Error(`WsRpcApi REST ${path}: HTTP ${res.status}`);
    }
    const body = (await res.json()) as { code?: number; msg?: string; data?: T };
    if (typeof body.code === 'number' && body.code !== 0) {
      throw new DaemonApiError({
        code: body.code,
        msg: body.msg ?? `WsRpcApi REST ${path} failed`,
        requestId: 'rest',
      });
    }
    return body.data as T;
  }

  async getHealth(): Promise<{ status: 'ok'; uptimeSec: number }> {
    const res = await fetch(buildRestUrl(this.origin, '/healthz'), { headers: this.authHeaders() });
    if (!res.ok) throw new Error(`WsRpcApi.getHealth: HTTP ${res.status}`);
    return res.json();
  }

  async getMeta(): Promise<{
    serverVersion: string;
    serverId: string;
    startedAt: string;
    capabilities: Record<string, boolean>;
    openInApps: string[];
    dangerousBypassAuth: boolean;
    backend: 'v1' | 'v2' | 'v3';
  }> {
    const res = await fetch(buildRestUrl(this.origin, '/meta'), { headers: this.authHeaders() });
    if (!res.ok) throw new Error(`WsRpcApi.getMeta: HTTP ${res.status}`);
    // The server wraps /meta in a `{ code, msg, data }` envelope — unwrap it,
    // and tolerate a bare body for older engines.
    const body = (await res.json()) as { data?: unknown };
    const data: Record<string, unknown> =
      body !== null && typeof body === 'object' && body.data !== null && typeof body.data === 'object'
        ? (body.data as Record<string, unknown>)
        : ((body ?? {}) as Record<string, unknown>);

    const stringField = (...keys: string[]): string => {
      for (const key of keys) {
        const value = data[key];
        if (typeof value === 'string') return value;
      }
      return '';
    };
    const boolField = (...keys: string[]): boolean => {
      for (const key of keys) {
        const value = data[key];
        if (typeof value === 'boolean') return value;
      }
      return false;
    };
    const capabilities = data['capabilities'];
    const openInApps = data['openInApps'] ?? data['open_in_apps'];

    return {
      serverVersion: stringField('serverVersion', 'server_version', 'version'),
      serverId: stringField('serverId', 'server_id'),
      startedAt: stringField('startedAt', 'started_at'),
      capabilities:
        capabilities !== null && typeof capabilities === 'object'
          ? (capabilities as Record<string, boolean>)
          : {},
      openInApps: Array.isArray(openInApps) ? (openInApps as string[]) : [],
      dangerousBypassAuth: boolField('dangerousBypassAuth', 'dangerous_bypass_auth'),
      backend: 'v3',
    };
  }

  // -------------------------------------------------------------------------
  // Session lifecycle — extended
  // -------------------------------------------------------------------------

  async updateSession(
    sessionId: string,
    input: {
      title?: string;
      cwd?: string;
      model?: string;
      permissionMode?: string;
      planMode?: boolean;
      swarmMode?: boolean;
      collaborationMode?: boolean;
      goalObjective?: string;
      goalControl?: 'pause' | 'resume' | 'cancel';
      thinking?: string;
    },
  ): Promise<AppSession> {
    // The v1 REST session patch carried the per-session mode toggles and
    // the goal control; the v2 wire gives each its own authoritative
    // method, so every field routes to its dedicated call below instead of
    // riding session.updateMeta (whose rust handler consumes only
    // title/tags/model/thinking — anything else there is silently dropped).
    const metaParams: Record<string, unknown> = { session_id: sessionId };
    if (input.title !== undefined) metaParams['title'] = input.title;
    if (input.model !== undefined) metaParams['model'] = input.model;
    if (input.thinking !== undefined) metaParams['thinking'] = input.thinking;
    if (Object.keys(metaParams).length > 1) {
      await this.client.call('session.updateMeta', metaParams);
    }
    if (input.permissionMode !== undefined) {
      await this.client.call('agent.setPermission', {
        session_id: sessionId,
        mode: input.permissionMode,
      });
    }
    if (input.planMode !== undefined) {
      await this.client.call(input.planMode ? 'plan.enter' : 'plan.cancel', {
        session_id: sessionId,
      });
    }
    if (input.swarmMode !== undefined) {
      await this.client.call(input.swarmMode ? 'swarm.enter' : 'swarm.exit', {
        session_id: sessionId,
      });
    }
    if (input.collaborationMode !== undefined) {
      await this.client.call(input.collaborationMode ? 'collaboration.enter' : 'collaboration.exit', {
        session_id: sessionId,
      });
    }
    if (input.goalObjective !== undefined) {
      await this.client.call('goal.create', {
        session_id: sessionId,
        objective: input.goalObjective,
      });
    }
    if (input.goalControl !== undefined) {
      const method =
        input.goalControl === 'pause'
          ? 'goal.pause'
          : input.goalControl === 'resume'
            ? 'goal.resume'
            : 'goal.cancel';
      await this.client.call(method, { session_id: sessionId });
    }
    return this.getSession(sessionId);
  }

  async getSessionGoal(sessionId: string): Promise<AppGoal | null> {
    // The engine's `goal.get` answers the TS `GoalSnapshot` wire under a
    // `{goal}` envelope (JSON null when no goal is set). The snapshot's
    // budget is the raw limits triple `{turns, tokens, wallClockMs}`, so
    // the AppGoal view derives the remaining/over-budget fields here —
    // the same derivation the v1 REST client's goal mapping performed.
    const result = await this.client.call<{ goal: RpcGoalSnapshot | null }>('goal.get', {
      session_id: sessionId,
    });
    const goal = result?.goal ?? null;
    if (goal === null) return null;
    const turnBudget = goal.budget?.turns ?? null;
    const tokenBudget = goal.budget?.tokens ?? null;
    const wallClockBudgetMs = goal.budget?.wallClockMs ?? null;
    return {
      goalId: goal.goalId,
      objective: goal.objective,
      completionCriterion: goal.completionCriterion ?? undefined,
      status: goal.status,
      turnsUsed: goal.turnsUsed,
      tokensUsed: goal.tokensUsed,
      wallClockMs: goal.wallClockMs,
      terminalReason: undefined,
      budget: {
        tokenBudget,
        remainingTokens: tokenBudget !== null ? Math.max(0, tokenBudget - goal.tokensUsed) : null,
        turnBudget,
        remainingTurns: turnBudget !== null ? Math.max(0, turnBudget - goal.turnsUsed) : null,
        wallClockBudgetMs,
        // The engine reports no wall-clock elapsed time on the snapshot
        // (`wallClockMs` stays 0), so the remaining derivation has no
        // source — null rather than a fabricated number.
        remainingWallClockMs: null,
        overBudget:
          (tokenBudget !== null && goal.tokensUsed > tokenBudget) ||
          (turnBudget !== null && goal.turnsUsed > turnBudget),
      },
    };
  }

  async getSessionWarnings(sessionId: string): Promise<AppSessionWarning[]> {
    const result = await this.client.call<AppSessionWarning[]>(
      'session.warnings',
      { session_id: sessionId },
    );
    return result ?? [];
  }

  async archiveSession(sessionId: string): Promise<{ archived: true }> {
    await this.client.call('session.archive', { session_id: sessionId });
    return { archived: true };
  }

  async restoreSession(sessionId: string): Promise<AppSession> {
    await this.client.call('session.restore', { session_id: sessionId });
    return this.getSession(sessionId);
  }

  async getSessionSnapshot(sessionId: string): Promise<AppSessionSnapshot> {
    const snapshot = await this.client.call<RpcSnapshot>('session.snapshot', {
      session_id: sessionId,
    });
    return toAppSnapshot(snapshot, sessionId);
  }

  async listPrompts(
    sessionId: string,
  ): Promise<{ active: AppPromptItem | null; queued: AppPromptItem[] }> {
    // The rust engine has no daemon-side prompt queue — `message.send` runs
    // turns directly (wait mode serializes on the call, fire mode runs
    // concurrently), so there is no parked-prompt state to list. The web
    // composable queues locally instead (`enqueue` in useWorkspaceState);
    // an honest empty view here keeps the re-sync display correct rather
    // than failing the best-effort caller. A server-side queue surface, if
    // ever needed, is a recorded followup.
    void sessionId;
    return { active: null, queued: [] };
  }

  async steerPrompts(
    sessionId: string,
    promptIds: string[],
  ): Promise<{ steered: boolean; promptIds: string[] }> {
    // The rust engine's steer is content-level (`message.steer`, wired
    // below as steerSession) — it has no parked-prompt orchestration.
    void sessionId; void promptIds;
    throw new Error('WsRpcApi.steerPrompts: not supported on the rust backend; use steerSession');
  }

  async steerSession(sessionId: string, input: PromptSubmission): Promise<SteerSessionResult> {
    // `message.steer` parks the content in the running turn's steer buffer
    // (flushed into the turn's context at its next step boundary, extending
    // the turn). The same structured-input mapping submitPrompt uses applies
    // here; parts without a prompt wire form are dropped.
    const params: Record<string, unknown> = {
      session_id: sessionId,
      input: promptContentToInputParts(input.content),
    };
    if (input.agentId !== undefined) params['agent_id'] = input.agentId;
    await this.client.call('message.steer', params);
    return { steered: true };
  }

  async abortSession(sessionId: string): Promise<{ aborted: boolean }> {
    // Session-level cancel: `message.abort` without a turnId cancels every
    // in-flight turn of the session and is a safe no-op when idle — the
    // same contract the v1 daemon's `POST /sessions/{id}:abort` answers
    // (always `{ aborted: true }` once the session exists, per its
    // `cancel({})` pass-through).
    await this.client.call('message.abort', { session_id: sessionId });
    return { aborted: true };
  }

  async compactSession(sessionId: string, instruction?: string): Promise<void> {
    const params: Record<string, unknown> = { session_id: sessionId };
    if (instruction !== undefined) params['instruction'] = instruction;
    await this.client.call('session.beginCompaction', params);
  }

  async undoSession(sessionId: string, count?: number): Promise<void> {
    const params: Record<string, unknown> = { session_id: sessionId };
    if (count !== undefined) params['count'] = count;
    await this.client.call('session.undo', params);
  }

  async forkSession(sessionId: string, input?: { title?: string; turnId?: number }): Promise<AppSession> {
    const params: Record<string, unknown> = { session_id: sessionId };
    if (input?.title !== undefined) params['title'] = input.title;
    // The completed turn to fork at (`turn.ended`'s numeric turn id) —
    // absent forks at the most recent completed turn.
    if (input?.turnId !== undefined) params['turnId'] = input.turnId;
    const view = await this.client.call<RpcSessionView>('session.fork', params);
    return toAppSession(view);
  }

  async createChildSession(sessionId: string, input?: { title?: string }): Promise<AppSession> {
    // `session.createChild` forks the parent's first agent into a child
    // session (context inherited, engine-minted id) and answers the klient
    // scope-handle wire `{ id, kind }`; the AppSession view is then read
    // back with `session.get` (the same pattern restoreSession uses). The
    // v1 REST child carried `parent_session_id` / `child_session_kind`
    // metadata — the engine's fork does not model that linkage (recorded
    // in the authoritative-methods TSV row), and no current web view
    // consumes it (side chats run on `btw.start` subagents instead).
    const params: Record<string, unknown> = { sourceSessionId: sessionId };
    if (input?.title !== undefined) params['title'] = input.title;
    const handle = await this.client.call<{ id: string; kind: number }>(
      'session.createChild',
      params,
    );
    const details = await this.client.call<RpcSessionDetails>('session.get', {
      session_id: handle.id,
    });
    return toAppSession(details);
  }

  async listChildSessions(sessionId: string): Promise<AppSession[]> {
    const views = await this.client.call<RpcSessionView[]>('session.children', {
      session_id: sessionId,
    });
    return (views ?? []).map(toAppSession);
  }

  async startBtw(sessionId: string): Promise<{ agentId: string }> {
    // The rust backend returns the historical snake_case `agent_id` key
    // (a deliberate wire shape; see StartBtwResult in the server's rpc.rs).
    const result = await this.client.call<{ agent_id: string }>('btw.start', {
      session_id: sessionId,
    });
    return { agentId: result.agent_id };
  }

  // -------------------------------------------------------------------------
  // Question — dismiss
  // -------------------------------------------------------------------------

  async dismissQuestion(
    sessionId: string,
    questionId: string,
  ): Promise<{ dismissed: true; dismissedAt: string }> {
    await this.client.call('question.dismiss', {
      session_id: sessionId,
      request_id: questionId,
    });
    return { dismissed: true, dismissedAt: new Date().toISOString() };
  }

  // -------------------------------------------------------------------------
  // Skills
  // -------------------------------------------------------------------------

  async listSkills(sessionId: string): Promise<AppSkill[]> {
    const skills = await this.client.call<RpcSkill[]>('skill.list', { sessionId });
    return (skills ?? []).map(toAppSkill);
  }

  async listSkillsForWorkspace(workspaceId: string): Promise<AppSkill[]> {
    // The rust engine's `skill.list` is workspace-scoped: its dispatch
    // ignores the session id entirely and returns the engine workspace's
    // whole catalog (every session shares it), so the workspace view reads
    // the same route the session view uses — no separate workspace-scoped
    // RPC exists.
    return fetchWorkspaceSkills(
      (method, params) => this.client.call(method, params),
      workspaceId,
    );
  }

  async activateSkill(
    sessionId: string,
    skillName: string,
    args?: string,
  ): Promise<{
    activated: true;
    skillName: string;
    promptId: string;
    status: 'queued' | 'running';
  }> {
    const params: Record<string, unknown> = { session_id: sessionId, name: skillName };
    if (args !== undefined) params['args'] = args;
    await this.client.call('agent.activateSkill', params);
    // The v2 activation is immediate: the server injects the skill's
    // SKILL.md as a system reminder and answers with an activation ack —
    // no turn, no prompt id.
    return {
      activated: true,
      skillName,
      promptId: '',
      status: 'running',
    };
  }

  async setSkillEnabled(
    skillName: string,
    enabled: boolean,
  ): Promise<{ name: string; enabled: boolean; registered: boolean; disabledSkills: string[] }> {
    // The rust management write face persists the user-level
    // `disabled_skills` config key and answers with the whole resulting
    // set so the client can reconcile its view.
    const result = await this.client.call<{
      name: string; enabled: boolean; registered: boolean; disabled_skills: string[];
    }>('skill.setEnabled', { name: skillName, enabled });
    return {
      name: result.name,
      enabled: result.enabled,
      registered: result.registered,
      disabledSkills: result.disabled_skills ?? [],
    };
  }

  async readSkillContent(
    skillName: string,
    sessionId?: string,
  ): Promise<{ name: string; content: string }> {
    const params: Record<string, unknown> = { name: skillName };
    if (sessionId !== undefined) params['session_id'] = sessionId;
    const result = await this.client.call<{ name: string; content: string }>(
      'skill.getContent', params,
    );
    return { name: result.name, content: result.content };
  }

  // -------------------------------------------------------------------------
  // Tasks
  // -------------------------------------------------------------------------

  async listTasks(
    sessionId: string,
    status?: AppTaskStatus,
    options?: { activeOnly?: boolean },
  ): Promise<AppTask[]> {
    // `task.list` serves the SAME direct AgentTask serialization
    // `task.listWorkspace` does (snake_case fields, tagged status/kind
    // enums — session_handlers.rs "TSV row 80"); the shared row mapper
    // keeps the two read faces from drifting apart. The wire method has no
    // status filter, so an optional filter applies client-side.
    // `activeOnly` scopes the wire call itself (S5: a polling client must
    // not haul the whole session's task list every second); the summary
    // view caps `output` per row, with `output_bytes` carrying the full
    // length when the engine truncated.
    const tasks = await this.client.call<RpcWorkspaceTask[]>('task.list', {
      sessionId,
      activeOnly: options?.activeOnly,
    });
    const rows = (tasks ?? []).map(toAppTaskFromAgentTask);
    return status === undefined ? rows : rows.filter((task) => task.status === status);
  }

  async getTaskOutput(sessionId: string, taskId: string, tailChars?: number): Promise<string> {
    // The single-task output face (`task.output`): the accumulated output
    // preview as a plain string, tail-capped when `tail` rides. The
    // per-second pollers fetch task tails through this instead of
    // re-listing the session per task (S5).
    const params: Record<string, unknown> = { sessionId, taskId };
    if (tailChars !== undefined) params['tail'] = tailChars;
    const output = await this.client.call<string>('task.output', params);
    return typeof output === 'string' ? output : '';
  }

  async getTask(
    sessionId: string,
    taskId: string,
    input?: { withOutput?: boolean; outputBytes?: number },
  ): Promise<AppTask> {
    // The rust backend has no single-task fetch; list and filter.
    const tasks = await this.listTasks(sessionId);
    const task = tasks.find((taskItem) => taskItem.id === taskId);
    if (task === undefined) {
      throw new Error(
        `WsRpcApi.getTask: task ${taskId} not found in session ${sessionId}`,
      );
    }
    if (input?.withOutput === true) {
      try {
        // The rust `task.output` takes a `tail` cap (not `bytes`) and returns
        // the accumulated output as a plain string.
        const outputParams: Record<string, unknown> = { sessionId, taskId };
        if (input.outputBytes !== undefined) outputParams['tail'] = input.outputBytes;
        const outputResult = await this.client.call<string>('task.output', outputParams);
        task.outputLines = outputResult.split('\n');
      } catch {
        // task.output may not be available for all task kinds — leave output empty.
      }
    }
    return task;
  }

  async cancelTask(sessionId: string, taskId: string): Promise<{ cancelled: true }> {
    await this.client.call('task.cancel', { sessionId, taskId });
    return { cancelled: true };
  }

  async listWorkspaceTasks(input?: { activeOnly?: boolean; workspaceId?: string }): Promise<AppTask[]> {
    // B7: the workspace-aggregated list over the engine's live sessions.
    // Every element is the direct AgentTask serialization task.list serves
    // (snake_case fields, tagged `status`/`kind` enums) — `session_id`
    // carries the attribution, so the overview joins titles client-side.
    const params: Record<string, unknown> = {};
    if (input?.activeOnly !== undefined) params['activeOnly'] = input.activeOnly;
    if (input?.workspaceId !== undefined) params['workspaceId'] = input.workspaceId;
    const tasks = await this.client.call<RpcWorkspaceTask[] | null>('task.listWorkspace', params);
    return (tasks ?? []).map(toAppTaskFromAgentTask);
  }

  async detachTask(
    sessionId: string,
    key: { toolCallId: string } | { taskId: string },
  ): Promise<AppTask | null> {
    // B7: address the conversion by the model-side tool call id (a
    // foreground Bash tool call) or by task id (a user `!` shell command).
    // The engine answers the converted task's BackgroundTaskInfo snapshot;
    // null is the klient noResult spelling of unknown/already-settled.
    const params: Record<string, unknown> = { sessionId };
    if ('toolCallId' in key) {
      params['toolCallId'] = key.toolCallId;
    } else {
      params['taskId'] = key.taskId;
    }
    const info = await this.client.call<RpcDetachedTaskInfo | null>('task.detach', params);
    return info === null || info === undefined
      ? null
      : toAppTaskFromBackgroundInfo(sessionId, info);
  }

  // -------------------------------------------------------------------------
  // Cron tasks — the session's scheduled prompts (B6 management face,
  // v3-only; the REST daemon has no cron routes)
  // -------------------------------------------------------------------------

  async listCronTasks(sessionId: string): Promise<AppCronTask[]> {
    // The engine answers the `{tasks: [...]}` envelope (CronTasksResult in
    // rust/crates/mirri-embedded/src/embedded_server/responses.rs), not a bare
    // array — same `{rows}`-envelope family as `approval.listRules`. A null /
    // absent envelope degrades to the honest empty list.
    const result = await this.client.call<RpcCronTaskList | null>('cron.list', { sessionId });
    return (result?.tasks ?? []).map(toAppCronTask);
  }

  async deleteCronTask(sessionId: string, taskId: string): Promise<{ deleted: true; taskId: string }> {
    const result = await this.client.call<{ deleted: boolean; task_id: string }>(
      'cron.delete', { sessionId, taskId },
    );
    if (result?.deleted !== true) {
      throw new Error(`cron.delete did not confirm the removal of task ${taskId}`);
    }
    return { deleted: true, taskId: result.task_id ?? taskId };
  }

  async triggerCronTask(sessionId: string, taskId: string): Promise<{ triggered: true; taskId: string }> {
    const result = await this.client.call<{ triggered: boolean; task_id: string }>(
      'cron.trigger', { sessionId, taskId },
    );
    if (result?.triggered !== true) {
      throw new Error(`cron.trigger did not confirm the run of task ${taskId}`);
    }
    return { triggered: true, taskId: result.task_id ?? taskId };
  }

  // -------------------------------------------------------------------------
  // User-level permission rules (B9 management face, v3-only; the REST
  // daemon has no rule routes). All three methods answer the shared
  // `{rules: [...]}` wire list — mapped onto the AppPermissionRule rows the
  // Settings rules panel consumes.
  // -------------------------------------------------------------------------

  async listPermissionRules(): Promise<AppPermissionRule[]> {
    const result = await this.client.call<RpcPermissionRuleList | null>('approval.listRules', {});
    return toAppPermissionRules(result);
  }

  async addPermissionRule(rule: AppPermissionRuleInput): Promise<AppPermissionRule[]> {
    // `scope` stays absent — this face manages user-scope rules only (B9 4.d).
    const wireRule: Record<string, unknown> = { decision: rule.decision, pattern: rule.pattern };
    if (rule.reason !== undefined) wireRule['reason'] = rule.reason;
    const result = await this.client.call<RpcPermissionRuleList | null>('approval.addRule', {
      rule: wireRule,
    });
    return toAppPermissionRules(result);
  }

  async removePermissionRule(
    pattern: string,
    decision: AppPermissionRuleDecision,
  ): Promise<AppPermissionRule[]> {
    const result = await this.client.call<RpcPermissionRuleList | null>('approval.removeRule', {
      pattern,
      decision,
    });
    return toAppPermissionRules(result);
  }

  // -------------------------------------------------------------------------
  // Terminals — the session's PTY pool over REST (protocol
  // `rest/terminal.ts` wire shapes, snake_case)
  // -------------------------------------------------------------------------

  async listTerminals(sessionId: string): Promise<AppTerminal[]> {
    const data = await this.restCall<{ items: RpcTerminal[] }>(
      `/sessions/${encodeURIComponent(sessionId)}/terminals`,
    );
    return (data.items ?? []).map(toAppTerminal);
  }

  async createTerminal(
    sessionId: string,
    input?: { cwd?: string; shell?: string; cols?: number; rows?: number },
  ): Promise<AppTerminal> {
    const body: Record<string, unknown> = {
      cwd: input?.cwd,
      shell: input?.shell,
      cols: input?.cols,
      rows: input?.rows,
    };
    const data = await this.restCall<RpcTerminal>(
      `/sessions/${encodeURIComponent(sessionId)}/terminals`,
      { method: 'POST', body: JSON.stringify(body) },
    );
    return toAppTerminal(data);
  }

  async getTerminal(sessionId: string, terminalId: string): Promise<AppTerminal> {
    const data = await this.restCall<RpcTerminal>(
      `/sessions/${encodeURIComponent(sessionId)}/terminals/${encodeURIComponent(terminalId)}`,
    );
    return toAppTerminal(data);
  }

  async closeTerminal(sessionId: string, terminalId: string): Promise<{ closed: true }> {
    // The rust server serves close as a `/close` subpath (axum's path syntax
    // cannot express the v1 `{tid}:close` suffix); this client is its only
    // consumer.
    return this.restCall<{ closed: true }>(
      `/sessions/${encodeURIComponent(sessionId)}/terminals/${encodeURIComponent(terminalId)}/close`,
      { method: 'POST', body: JSON.stringify({}) },
    );
  }

  // -------------------------------------------------------------------------
  // File system
  // -------------------------------------------------------------------------

  async listDirectory(
    sessionId: string,
    input: { path?: string; depth?: number; includeGitStatus?: boolean },
  ): Promise<{
    items: FsEntry[];
    childrenByPath?: Record<string, FsEntry[]>;
    truncated: boolean;
  }> {
    const params: Record<string, unknown> = { sessionId };
    if (input.path !== undefined) params['path'] = input.path;
    if (input.depth !== undefined) params['depth'] = input.depth;
    if (input.includeGitStatus !== undefined) params['includeGitStatus'] = input.includeGitStatus;
    const result = await this.client.call<RpcListDirResult>('file.list', params);
    const childrenByPath: Record<string, FsEntry[]> | undefined = result.childrenByPath
      ? Object.fromEntries(
          Object.entries(result.childrenByPath).map(([key, entries]) => [
            key,
            (entries ?? []).map(toFsEntry),
          ]),
        )
      : undefined;
    return {
      items: (result.items ?? []).map(toFsEntry),
      childrenByPath,
      truncated: result.truncated ?? false,
    };
  }

  async readFile(
    sessionId: string,
    input: { path: string; offset?: number; length?: number },
  ): Promise<{
    path: string;
    content: string;
    encoding: 'utf-8' | 'base64';
    size: number;
    truncated: boolean;
    etag: string;
    mime: string;
    languageId?: string;
    lineCount?: number;
    isBinary: boolean;
  }> {
    const params: Record<string, unknown> = { sessionId, path: input.path };
    if (input.offset !== undefined) params['offset'] = input.offset;
    if (input.length !== undefined) params['length'] = input.length;
    return this.client.call('file.read', params);
  }

  async searchFiles(
    sessionId: string,
    input: { query: string; limit?: number },
  ): Promise<{
    items: Array<{
      path: string;
      name: string;
      kind: FsKind;
      score: number;
      matchPositions: number[];
    }>;
    truncated: boolean;
  }> {
    const params: Record<string, unknown> = { sessionId, query: input.query };
    if (input.limit !== undefined) params['limit'] = input.limit;
    return this.client.call('file.search', params);
  }

  async grepFiles(
    sessionId: string,
    input: { pattern: string; regex?: boolean; caseSensitive?: boolean },
  ): Promise<{
    files: Array<{
      path: string;
      matches: Array<{
        line: number;
        col: number;
        text: string;
        before: string[];
        after: string[];
      }>;
    }>;
    filesScanned: number;
    truncated: boolean;
    elapsedMs: number;
  }> {
    const params: Record<string, unknown> = { sessionId, pattern: input.pattern };
    if (input.regex !== undefined) params['regex'] = input.regex;
    if (input.caseSensitive !== undefined) params['caseSensitive'] = input.caseSensitive;
    return this.client.call('file.grep', params);
  }

  async getGitStatus(
    sessionId: string,
    paths?: string[],
  ): Promise<{
    branch: string;
    ahead: number;
    behind: number;
    entries: Record<string, string>;
    additions: number;
    deletions: number;
    pullRequest: { number: number; state: string; url: string } | null;
  }> {
    const params: Record<string, unknown> = { sessionId };
    if (paths !== undefined) params['paths'] = paths;
    return this.client.call('file.git_status', params);
  }

  async getFileDiff(
    sessionId: string,
    path: string,
  ): Promise<{ path: string; diff: string }> {
    return this.client.call('file.diff', { sessionId, path });
  }

  getFileDownloadUrl(sessionId: string, path: string): string {
    return this.withTokenQuery(
      buildRestUrl(
        this.origin,
        `/files/${encodeURIComponent(sessionId)}/${path}`,
      ),
    );
  }

  // --- external open degrade chain (D3/D8) -----------------------------------
  //
  // openFile/revealFile/openInApp used to be hard throws on the rust backend.
  // They now walk a three-tier chain:
  //   1. the desktop shell's preload bridge (window.mirriDesktop, main
  //      process shell.openPath/showItemInFolder/$EDITOR spawn) — paths are
  //      resolved against the session cwd first because the shell opens
  //      outside any session scope;
  //   2. the engine's fs.open/fs.reveal RPC — only when the host declares the
  //      capability in /meta (browser connected to a capable engine; the
  //      rust engine does not declare it today);
  //   3. neither — the ExternalOpenUnavailableError sentinel, which the UI
  //      degrades to a copy-path prompt instead of a raw failure dialog.

  /** Cached /meta capability probes for the fs.open/fs.reveal tiers. */
  private readonly engineFsCapabilityCache = new Map<string, boolean>();

  private async engineSupportsFs(method: 'fs.open' | 'fs.reveal'): Promise<boolean> {
    const cached = this.engineFsCapabilityCache.get(method);
    if (cached !== undefined) return cached;
    let supported = false;
    try {
      const meta = await this.getMeta();
      const caps = meta.capabilities;
      supported = caps[method] === true || caps[method.replaceAll('.', '_')] === true;
    } catch {
      supported = false;
    }
    this.engineFsCapabilityCache.set(method, supported);
    return supported;
  }

  /** Resolve the session cwd for tier-1 absolute paths: the caller's explicit
   *  hint (the workspace root the UI knows) first, then the session view's
   *  cwd when the engine echoes one. Returns null when neither is known —
   *  the bridge tier is skipped (the shell cannot open a path it cannot
   *  place). */
  private async sessionCwd(
    sessionId: string,
    hint: string | undefined,
  ): Promise<string | null> {
    if (typeof hint === 'string' && hint.length > 0) return hint;
    const session = await this.getSession(sessionId);
    return typeof session.cwd === 'string' && session.cwd.length > 0 ? session.cwd : null;
  }

  async openFile(
    sessionId: string,
    input: { path: string; line?: number; cwd?: string },
  ): Promise<{ opened: true }> {
    const bridge = getDesktopBridge();
    if (bridge !== null) {
      const cwd = await this.sessionCwd(sessionId, input.cwd);
      if (cwd !== null) {
        await bridge.openInEditor(resolveWorkspacePath(cwd, input.path), input.line);
        return { opened: true };
      }
    }
    if (await this.engineSupportsFs('fs.open')) {
      const params: Record<string, unknown> = {
        session_id: sessionId,
        path: input.path,
      };
      if (input.line !== undefined) params['line'] = input.line;
      await this.client.call('fs.open', params);
      return { opened: true };
    }
    throw new ExternalOpenUnavailableError('openFile', input.path);
  }

  async revealFile(
    sessionId: string,
    input: { path: string; cwd?: string },
  ): Promise<{ revealed: true }> {
    const bridge = getDesktopBridge();
    if (bridge !== null) {
      const cwd = await this.sessionCwd(sessionId, input.cwd);
      if (cwd !== null) {
        await bridge.revealPath(resolveWorkspacePath(cwd, input.path));
        return { revealed: true };
      }
    }
    if (await this.engineSupportsFs('fs.reveal')) {
      await this.client.call('fs.reveal', { session_id: sessionId, path: input.path });
      return { revealed: true };
    }
    throw new ExternalOpenUnavailableError('revealFile', input.path);
  }

  async openInApp(
    sessionId: string,
    appId: string,
    path: string,
    line?: number,
    cwd?: string,
  ): Promise<void> {
    void line; // The app launches (Finder/VS Code/...) target directories today.
    const bridge = getDesktopBridge();
    if (bridge !== null) {
      const root = await this.sessionCwd(sessionId, cwd);
      if (root !== null) {
        // The UI opens the workspace root ('.') — the desktop shell gets the
        // resolved absolute root, not a './'-suffixed path.
        const target = path === '.' ? root : resolveWorkspacePath(root, path);
        await bridge.openInApp(appId, target);
        return;
      }
    }
    // No engine fs.open-in-app tier exists on any backend — straight to the
    // sentinel.
    throw new ExternalOpenUnavailableError('openInApp', path);
  }

  // -------------------------------------------------------------------------
  // Workspaces
  // -------------------------------------------------------------------------

  async listWorkspaces(): Promise<AppWorkspace[]> {
    const workspaces = await this.client.call<RpcWorkspace[]>('workspace.list');
    return (workspaces ?? []).map(toAppWorkspace);
  }

  async addWorkspace(input: { root: string; name?: string }): Promise<AppWorkspace> {
    const params: Record<string, unknown> = { root: input.root };
    if (input.name !== undefined) params['name'] = input.name;
    const workspace = await this.client.call<RpcWorkspace>('workspace.createOrTouch', params);
    return toAppWorkspace(workspace);
  }

  async updateWorkspace(id: string, input: { name: string }): Promise<AppWorkspace> {
    const workspace = await this.client.call<RpcWorkspace>('workspace.update', {
      workspaceId: id,
      name: input.name,
    });
    return toAppWorkspace(workspace);
  }

  async setWorkspaceTrust(id: string, trusted: boolean): Promise<{ trusted: boolean }> {
    // `workspace.setTrust` (W8) — the TS v2 trust face's shape on the RPC
    // wire: `{workspaceId, trusted}` → `{trusted}`. The engine flips the
    // live gates (project-layer connect / immediate disconnect + the
    // AGENTS.md project-section gate) before answering.
    const result = await this.client.call<{ trusted: boolean }>('workspace.setTrust', {
      workspaceId: id,
      trusted,
    });
    return { trusted: result.trusted };
  }

  async deleteWorkspace(
    id: string,
    mode: WorkspaceDeleteMode = 'remove',
  ): Promise<WorkspaceDeletionResult> {
    // The W6 three-choice unregister: `{workspaceId, mode}` (mode required on
    // the wire — never defaulted into the destructive path client-side either)
    // → `{ok, mode, workspaceId, deletedSessions}`.
    const result = await this.client.call<RpcWorkspaceDeletionResult>(
      'workspace.delete',
      { workspaceId: id, mode },
    );
    return {
      ok: result.ok,
      mode: result.mode,
      workspaceId: result.workspaceId,
      deletedSessions: result.deletedSessions,
    };
  }

  async refreshWorkspace(
    workspaceId: string,
  ): Promise<{ refreshed: true; inserted: number; removed: number }> {
    // `workspace.refresh` was retired from the rust backend in batch ⑥. The
    // callers only use it as a pre-step before re-pulling the real data, so
    // this is an honest local no-op mirroring the retired server stub's
    // zero counts (the caller's refresh flow re-pulls workspaces/sessions
    // right after this).
    void workspaceId;
    return { refreshed: true, inserted: 0, removed: 0 };
  }

  async browseFs(path?: string): Promise<FsBrowseResult> {
    // `fs.browse` — the rust engine's host-folder-browser face (snake_case
    // wire per protocol `rest/fsBrowse`). Directory rows only; a non-absolute
    // path is an invalid-params error and a missing path is not found — both
    // propagate so the caller's degrade path (the picker's browse-failed
    // fallback) engages, exactly like the TS backend's error semantics.
    const result = await this.client.call<RpcFsBrowseResult>(
      'fs.browse',
      path !== undefined ? { path } : {},
    );
    return {
      path: result.path,
      parent: result.parent,
      entries: (result.entries ?? []).map((entry) => ({
        name: entry.name,
        path: entry.path,
        isDir: entry.is_dir,
        // The rust engine's directory rows omit the git fields — the neutral
        // defaults keep the mapping tolerant of engines that do carry them.
        isGitRepo: entry.is_git_repo ?? false,
        branch: entry.branch,
      })),
    };
  }

  async getFsHome(): Promise<{ home: string; recentRoots: string[] }> {
    // `fs.home` — the OS user home plus the recent-roots list. The rust
    // engine has no recent-workspace history store, so `recent_roots` is the
    // honest empty list; errors propagate to the caller's degrade path.
    const result = await this.client.call<RpcFsHomeResult>('fs.home');
    return { home: result.home, recentRoots: result.recent_roots ?? [] };
  }

  // -------------------------------------------------------------------------
  // Models / Providers
  // -------------------------------------------------------------------------

  async listModels(): Promise<AppModel[]> {
    const models = await this.client.call<RpcModel[]>('model.list');
    return (models ?? []).map(toAppModel);
  }

  async listProviders(): Promise<AppProvider[]> {
    const providers = await this.client.call<RpcProvider[]>('model.listProviders');
    return (providers ?? []).map(toAppProvider);
  }

  /** Read one configured provider's `model.getProvider` row and map it to
   *  the app shape — the read-back path `addProvider` /
   *  `importCatalogProvider` answer with (the TS services return the
   *  imported provider item, and this backend's write methods answer only
   *  the id, so the adapter fetches the row). */
  private async fetchAppProvider(id: string): Promise<AppProvider> {
    const provider = await this.client.call<RpcProvider>('model.getProvider', {
      providerId: id,
    });
    return toAppProvider(provider);
  }

  async addProvider(input: AppAddProviderInput): Promise<AppProvider> {
    // v2 `createProvider` alignment on the rust backend: reject a taken id,
    // then write the provider record and its model aliases in one atomic
    // `config.set` (the engine's set deep-merges the camelCase patch —
    // objects merge recursively, scalars replace). A
    // fresh environment (no defaultModel configured anywhere) additionally
    // gets the provider's default (or first) model seeded as the global
    // default so the first provider leaves the daemon usable; an existing
    // pointer is never moved.
    const config = await this.client.call<AppConfig>('config.get');
    if (config.providers?.[input.id] !== undefined) {
      throw new Error(`provider ${input.id} already exists`);
    }
    const providerRecord: Record<string, unknown> = { type: input.type };
    if (input.apiKey !== undefined) providerRecord['apiKey'] = input.apiKey;
    if (input.baseUrl !== undefined) providerRecord['baseUrl'] = input.baseUrl;
    // The provider-level default references the model alias id
    // (`<id>/<model>`), the same form the import path persists.
    if (input.defaultModel !== undefined) {
      providerRecord['defaultModel'] = `${input.id}/${input.defaultModel}`;
    }
    // One atomic config.set: the provider record, its model aliases, and
    // the fresh-environment default land in a single write — a failure can
    // never strand a provider with no models (B7).
    const models: Record<string, unknown> = {};
    for (const entry of input.models) {
      const alias: Record<string, unknown> = {
        provider: input.id,
        model: entry.model,
        maxContextSize: entry.maxContextSize,
      };
      if (entry.displayName !== undefined) alias['displayName'] = entry.displayName;
      if (entry.capabilities !== undefined) alias['capabilities'] = [...entry.capabilities];
      if (entry.maxOutputSize !== undefined) alias['maxOutputSize'] = entry.maxOutputSize;
      if (entry.supportEfforts !== undefined) alias['supportEfforts'] = [...entry.supportEfforts];
      if (entry.adaptiveThinking !== undefined) {
        alias['adaptiveThinking'] = entry.adaptiveThinking;
      }
      models[`${input.id}/${entry.model}`] = alias;
    }
    const patch: Record<string, unknown> = {
      providers: { [input.id]: providerRecord },
      models,
    };
    if (config.defaultModel === undefined && input.models.length > 0) {
      patch['defaultModel'] =
        input.defaultModel !== undefined
          ? `${input.id}/${input.defaultModel}`
          : `${input.id}/${input.models[0]!.model}`;
    }
    await this.client.call('config.set', patch);

    return this.fetchAppProvider(input.id);
  }

  async importCatalogProvider(input: {
    catalogId: string;
    id?: string;
    apiKey?: string;
    baseUrl?: string;
  }): Promise<{ provider: AppProvider; modelsImported: number }> {
    // `providerDiscovery.importCatalog` — the models.dev directory import:
    // the engine derives the alias fields from the directory facts and writes
    // them in ONE config transaction. An OAuth-managed id answers
    // provider.oauth_managed; any other existing id MERGES (existing aliases
    // untouched, tombstones hold, only new models append, unset default
    // seeded — models_imported counts appended aliases); an unloaded
    // directory answers provider.not_found.
    const result = await this.client.call<{ provider: string; models_imported: number }>(
      'providerDiscovery.importCatalog',
      {
        catalogId: input.catalogId,
        id: input.id,
        apiKey: input.apiKey,
        baseUrl: input.baseUrl,
      },
    );
    const provider = await this.fetchAppProvider(result.provider);
    return { provider, modelsImported: result.models_imported };
  }

  async deleteProvider(id: string): Promise<{ deleted: true }> {
    // `config.removeProvider` — the cascade semantics (the provider, every
    // alias pointing at it, and a now-dangling defaultModel pointer).
    // `provider.delete` is deliberately NOT used here: it never cascades
    // (C6) — the record CRUD and the cascade removal are different
    // operations on this backend.
    await this.client.call('config.removeProvider', { providerId: id });
    return { deleted: true };
  }

  async refreshProvider(id: string): Promise<ProviderRefreshResult> {
    // `providerDiscovery.refreshProviderModels` scoped to the addressed
    // provider — the engine pulls the models.dev directory and merges one
    // configured provider's model list (an unknown id answers
    // `provider.not_found`, like `model.getProvider`).
    const result = await this.client.call<RpcProviderRefreshResult>(
      'providerDiscovery.refreshProviderModels',
      { providerId: id },
    );
    return toAppProviderRefreshResult(result);
  }

  async refreshAllProviders(): Promise<ProviderRefreshResult> {
    // The scope-wide `{ scope: 'all' }` options — the canonical refresh-all
    // spelling (the kap-server model-catalog scheduler calls the same form).
    // Only the configured set is ever addressed — directory data never
    // enters `model.list` (D-M9).
    const result = await this.client.call<RpcProviderRefreshResult>(
      'providerDiscovery.refreshProviderModels',
      { scope: 'all' },
    );
    return toAppProviderRefreshResult(result);
  }

  async deleteModel(modelId: string): Promise<{ deleted: true }> {
    // `model.delete` — alias removal located by the config key (the model
    // alias id), never a web-assembled id (C6).
    await this.client.call('model.delete', { id: modelId });
    return { deleted: true };
  }

  async listCatalogProviders(): Promise<AppCatalogProvider[]> {
    // `providerDiscovery.listCatalog` — the models.dev directory browse
    // (read-only cache, zero network; the empty array is the honest
    // not-yet-refreshed signal).
    const rows = await this.client.call<RpcCatalogProvider[]>(
      'providerDiscovery.listCatalog',
    );
    return (rows ?? []).map(toAppCatalogProvider);
  }

  // -------------------------------------------------------------------------
  // File upload / download
  // -------------------------------------------------------------------------

  async uploadFile(input: {
    file: Blob;
    name?: string;
  }): Promise<{ id: string; name: string; mediaType: string; size: number }> {
    // The rust backend's `POST /api/v1/files` (P4): multipart upload with
    // the same field spellings the v1 client sends (file + optional name),
    // answered in the `{code, msg, data}` envelope with the FileMeta wire
    // shape (snake_case media_type).
    const formData = new FormData();
    formData.append(
      'file',
      input.file,
      input.name ?? (input.file instanceof File ? input.file.name : 'upload'),
    );
    if (input.name !== undefined) {
      formData.append('name', input.name);
    }
    const res = await fetch(buildRestUrl(this.origin, '/files'), {
      method: 'POST',
      headers: this.authHeaders(),
      body: formData,
    });
    if (!res.ok) {
      if (res.status === 401) markAuthRequired();
      throw new Error(`WsRpcApi.uploadFile: HTTP ${res.status}`);
    }
    const body = (await res.json()) as {
      code?: number;
      msg?: string;
      data?: { id: string; name: string; media_type: string; size: number };
    };
    if (typeof body.code === 'number' && body.code !== 0) {
      throw new DaemonApiError({
        code: body.code,
        msg: body.msg ?? 'WsRpcApi.uploadFile failed',
        requestId: 'rest',
      });
    }
    const data = body.data;
    if (data === undefined) {
      throw new Error('WsRpcApi.uploadFile: response carried no file metadata');
    }
    return {
      id: data.id,
      name: data.name,
      mediaType: data.media_type,
      size: data.size,
    };
  }

  getFileUrl(fileId: string): string {
    return this.withTokenQuery(
      buildRestUrl(this.origin, `/files/${encodeURIComponent(fileId)}`),
    );
  }

  async getFileBlob(fileId: string): Promise<Blob> {
    const url = this.getFileUrl(fileId);
    const res = await fetch(url, { headers: this.authHeaders() });
    if (!res.ok) {
      if (res.status === 401) markAuthRequired();
      throw new Error(`WsRpcApi.getFileBlob: HTTP ${res.status}`);
    }
    return res.blob();
  }

  // -------------------------------------------------------------------------
  // Config
  // -------------------------------------------------------------------------

  async getConfig(): Promise<AppConfig> {
    return this.client.call<AppConfig>('config.get');
  }

  async setConfig(
    patch: Omit<Partial<AppConfig>, 'models' | 'providers' | 'disabledAgents'> & {
      models?: Record<string, Partial<AppModelAlias>>;
      providers?: Record<string, Partial<AppConfigProvider>>;
    },
  ): Promise<AppConfig> {
    // Runtime strip (R5-4) doubling the type-level Omit: the retired
    // `disabledAgents` key must never reach the daemon's `config.set`, even
    // from a JS caller or a stale bundle that still carries it. Legal
    // patches pass through untouched.
    const { disabledAgents: _retired, ...wirePatch } = patch as Partial<AppConfig>;
    return this.client.call<AppConfig>('config.set', wirePatch);
  }

  // -------------------------------------------------------------------------
  // Agent profiles
  // -------------------------------------------------------------------------

  async listAgents(): Promise<AppAgentProfileList> {
    // The profile catalog method: rust's `agent.list` is a different,
    // session-scoped live-agent enumeration (it requires a sessionId);
    // `agent.listProfiles` is the profile-directory read this surface wants.
    // The response is the {items, skipped} envelope (R3-6): the rows under
    // `items`, the discovery pass's skipped files as {path, reason}.
    const listed = await this.client.call<RpcAgentProfileList>('agent.listProfiles');
    return {
      profiles: (listed?.items ?? []).map(toAppAgentProfile),
      skippedAgentFiles: listed?.skipped ?? [],
    };
  }

  async getAgent(name: string): Promise<AppAgentProfile | undefined> {
    try {
      const agent = await this.client.call<RpcAgentProfile>('agent.getProfile', { name });
      return toAppAgentProfile(agent);
    } catch (error) {
      if (error instanceof JsonRpcCallError && error.code === NOT_FOUND_CODE) {
        return undefined;
      }
      throw error;
    }
  }

  async createAgent(data: AppAgentProfileInput): Promise<AppAgentProfile> {
    // The wire payload mirrors the REST client's create shape (client.ts):
    // the editable fields in snake_case, absent — never null — when unset.
    const wire: Record<string, unknown> = { name: data.name };
    if (data.description !== undefined) wire['description'] = data.description;
    if (data.defaultModel !== undefined) wire['default_model'] = data.defaultModel;
    if (data.tools !== undefined) wire['tools'] = data.tools;
    if (data.disallowedTools !== undefined) wire['disallowed_tools'] = data.disallowedTools;
    if (data.subagents !== undefined) wire['subagents'] = data.subagents;
    if (data.systemPromptTemplate !== undefined) wire['system_prompt_template'] = data.systemPromptTemplate;
    if (data.whenToUse !== undefined) wire['when_to_use'] = data.whenToUse;
    try {
      const row = await this.client.call<RpcAgentProfile>('agent.createProfile', wire);
      return toAppAgentProfile(row);
    } catch (error) {
      throw agentProfileRpcError('agent.createProfile', error);
    }
  }

  async updateAgent(
    name: string,
    data: AppAgentProfileUpdate,
  ): Promise<AppAgentProfile> {
    const wire: Record<string, unknown> = { name };
    if (data.description !== undefined) wire['description'] = data.description;
    if (data.defaultModel !== undefined) wire['default_model'] = data.defaultModel;
    if (data.tools !== undefined) wire['tools'] = data.tools;
    if (data.disallowedTools !== undefined) wire['disallowed_tools'] = data.disallowedTools;
    if (data.subagents !== undefined) wire['subagents'] = data.subagents;
    if (data.systemPromptTemplate !== undefined) wire['system_prompt_template'] = data.systemPromptTemplate;
    if (data.whenToUse !== undefined) wire['when_to_use'] = data.whenToUse;
    try {
      const row = await this.client.call<RpcAgentProfile>('agent.updateProfile', wire);
      return toAppAgentProfile(row);
    } catch (error) {
      throw agentProfileRpcError('agent.updateProfile', error);
    }
  }

  async deleteAgent(name: string): Promise<void> {
    // Fire-and-forget: the wire answers the bare `{deleted: true}` envelope,
    // not the row — the caller re-fetches through the list refresh path.
    try {
      await this.client.call<unknown>('agent.deleteProfile', { name });
    } catch (error) {
      throw agentProfileRpcError('agent.deleteProfile', error);
    }
  }

  async enableAgent(name: string): Promise<void> {
    // Fire-and-forget: `{ok: true}` bare envelope; the refresh path re-reads
    // the row (the Task 25 lifecycle methods never return the updated row).
    try {
      await this.client.call<unknown>('agent.enableProfile', { name });
    } catch (error) {
      throw agentProfileRpcError('agent.enableProfile', error);
    }
  }

  async disableAgent(name: string): Promise<void> {
    try {
      await this.client.call<unknown>('agent.disableProfile', { name });
    } catch (error) {
      throw agentProfileRpcError('agent.disableProfile', error);
    }
  }

  async resetAgent(name: string): Promise<void> {
    try {
      await this.client.call<unknown>('agent.resetProfile', { name });
    } catch (error) {
      throw agentProfileRpcError('agent.resetProfile', error);
    }
  }

  async listTools(): Promise<AppToolDescriptor[]> {
    const tools = await this.client.call<RpcToolDescriptor[]>('tool.list');
    return (tools ?? []).map(toAppToolDescriptor);
  }

  async listToolsCatalog(): Promise<AppToolDescriptor[]> {
    // The rust `tool.catalog` answers `{tools: [...]}` (the TSV-authoritative
    // rust shape), not a bare array.
    const result = await this.client.call<{ tools?: RpcToolDescriptor[] }>('tool.catalog');
    return (result.tools ?? []).map(toAppToolDescriptor);
  }

  // -------------------------------------------------------------------------
  // MCP — the layered `mcp.config.*` surface (docs/specs/mcp-architecture.md
  // §4.1) plus the pool views (`mcp.status`) and `mcp.config.test` /
  // `mcp.reconnect`. The v1-era `mcp.global.*` REST surface is retired on
  // this wire.
  // -------------------------------------------------------------------------

  /** The layered config view (`mcp.config.list`, the merged three-layer
   *  view: every entry with its source layer, stored config verbatim and
   *  derived effective state). */
  private async mcpConfigEntries(): Promise<RpcMcpConfigEntry[]> {
    const entries = await this.client.call<RpcMcpConfigEntry[]>('mcp.config.list');
    return entries ?? [];
  }

  async listGlobalMcpServers(): Promise<AppMcpServer[]> {
    // `mcp.status` is the app-level global view: every pool entry (user and
    // workspace alike, no shadowing) with `pool`/`source` provenance. The
    // stored config is joined in from the layered config view by name
    // (plugin-bundled servers match under their `plugin-<id>:<name>`
    // runtime name).
    const [pool, configEntries] = await Promise.all([
      this.client.call<RpcMcpPoolEntry[]>('mcp.status'),
      this.mcpConfigEntries(),
    ]);
    const configByName = new Map<string, RpcMcpConfigEntry>();
    for (const entry of configEntries) {
      const key = entry.runtimeName ?? entry.name;
      // Same-name entries from multiple layers collapse onto the effective
      // (highest-precedence) one — the card edits/joins that definition.
      const existing = configByName.get(key);
      if (existing === undefined || mcpSourceRank(entry.source) > mcpSourceRank(existing.source)) {
        configByName.set(key, entry);
      }
    }
    const servers = (pool ?? []).map((entry) => {
      // A filter-only override is not the server's definition — only a
      // complete entry's stored config joins the card. The layered config
      // entry also names the layer this name's effective definition lives
      // in (user / projectRoot / project / plugin) — the pool entry itself
      // only knows the pool it runs in.
      const configEntry = configByName.get(entry.name);
      const config = configEntry?.config;
      const server = toAppMcpServer(entry, isCompleteMcpConfig(config) ? config : undefined);
      server.source = configEntry?.source ?? entry.source;
      return server;
    });
    // Config-layer entries with no pool slot yet (a layer the engine has
    // not loaded — e.g. project layers before the workspace was trusted)
    // still list, as unloaded cards the panel can edit. A filter-only
    // override entry is never a card of its own — it rides its name's
    // connection, which the shadowed layer defines.
    const pooled = new Set((pool ?? []).map((entry) => entry.name));
    for (const entry of configEntries) {
      const runtimeName = entry.runtimeName ?? entry.name;
      if (pooled.has(runtimeName) || !isCompleteMcpConfig(entry.config)) {
        continue;
      }
      servers.push({
        id: `config:${runtimeName}`,
        name: runtimeName,
        transport: entry.config.transport,
        status: 'disconnected',
        toolCount: 0,
        source: entry.source,
        config: entry.config,
      });
    }
    // Every pool lists its own connection (no-shadowing view, spec §4.4),
    // so a name defined in both the user layer and a project layer
    // arrives twice. The settings page is the merged effective view
    // (PS1.1): one card per name, the effective entry's data with its
    // source layer, the shadowed pools kept as provenance.
    return mergeSameNameMcpServers(servers);
  }

  async getGlobalMcpConfig(): Promise<AppMcpGlobalConfigEntry[]> {
    // The rust config surface presents the stored text verbatim — `${VAR}`
    // references stay unresolved (expansion happens at connect/test time
    // only, adjudication A5), so there is no env-expanded view on this
    // wire: `resolved` mirrors the stored source. Filter-only override
    // entries are not editable server definitions — the shadowed layer's
    // complete entry is the config this view serves.
    const entries = await this.mcpConfigEntries();
    return entries.flatMap((entry) =>
      isCompleteMcpConfig(entry.config)
        ? [
            {
              name: entry.runtimeName ?? entry.name,
              source: entry.config,
              resolved: entry.config,
            },
          ]
        : [],
    );
  }

  async listGlobalMcpTools(): Promise<AppToolDescriptor[]> {
    // `mcp.tools` (ADR-004) is the per-server tool listing with per-tool
    // filter state — the endgame replacement for the retired
    // `mcp.global.tools` wire (which the serve-side gate never let
    // through, F-1). The popup folds every pool entry's view into one
    // flat descriptor list: same-name workspace entries shadow user ones
    // (spec §4.2 — `mcp.status` lists workspace entries first), and a
    // `connected: false` answer (a name with no live connection)
    // contributes no tools, exactly like the retired wire's silence.
    // Every advertised tool folds in — disabled ones included, each
    // carrying the engine's effective verdict (see the per-tool mapping
    // below; SDR-S005 / PS3.2).
    const pool = await this.client.call<RpcMcpPoolEntry[]>('mcp.status');
    const names: string[] = [];
    for (const entry of pool ?? []) {
      if (!names.includes(entry.name)) {
        names.push(entry.name);
      }
    }
    const perServer = await Promise.all(
      names.map(async (name) => {
        const result = await this.client.call<{
          connected?: boolean;
          tools?: Array<{ name: string; description?: string; enabled?: boolean }>;
        }>('mcp.tools', { name });
        // All advertised tools fold in — disabled ones included, each with
        // the engine's effective verdict: the settings popup renders
        // per-tool switches, and a disabled tool must stay listed (with
        // `enabled: false`) to be re-enableable (SDR-S005 / PS3.2).
        return (result?.tools ?? []).map((tool) => ({
          name: tool.name,
          description: tool.description ?? '',
          source: 'mcp' as const,
          mcpServerId: name,
          enabled: tool.enabled !== false,
        }));
      }),
    );
    return perServer.flat();
  }

  async createMcpServer(name: string, config: AppMcpServerConfig): Promise<void> {
    // The Settings panel is the global editor — writes target the user
    // layer (the project-root `.mcp.json` is a read-only git-shared asset).
    await this.client.call('mcp.config.set', { scope: 'user', name, config });
  }

  async updateMcpServer(name: string, config: AppMcpServerConfig): Promise<void> {
    await this.client.call('mcp.config.set', { scope: 'user', name, config });
  }

  async deleteMcpServer(name: string): Promise<void> {
    // Idempotent on the engine: removing an absent name succeeds.
    await this.client.call('mcp.config.remove', { scope: 'user', name });
  }

  /** The `mcp.sync` result — the reconcile summary (F-6/F-12). */
  async reloadMcpServers(): Promise<AppMcpSyncSummary> {
    // F-12: `mcp.sync` IS the reload surface — the idempotent reconcile
    // entry that converges the running pools with the config layers
    // through the same kernel the engine's config writes ride, effective
    // immediately (never "at the next engine start"). The summary counts
    // what the pass applied, by kind — page-copy material for the
    // settings view.
    return this.client.call<AppMcpSyncSummary>('mcp.sync');
  }

  async enableMcpServer(serverId: string): Promise<void> {
    await this.setMcpServerEnabled(serverId, true);
  }

  async disableMcpServer(serverId: string): Promise<void> {
    await this.setMcpServerEnabled(serverId, false);
  }

  /** Flip one entry's `enabled` override via a read-modify-write of its
   *  defining layer (the engine applies it at the next start). */
  private async setMcpServerEnabled(serverId: string, enabled: boolean): Promise<void> {
    const entry = (await this.mcpConfigEntries()).find(
      (candidate) => (candidate.runtimeName ?? candidate.name) === serverId,
    );
    if (entry === undefined) {
      throw new Error(
        `WsRpcApi: MCP server '${serverId}' is not defined in any configuration layer`,
      );
    }
    await this.client.call('mcp.config.set', {
      scope: mcpWritableScope(entry.source),
      name: entry.name,
      config: { ...entry.config, enabled },
    });
  }

  async enableGlobalMcpTool(serverName: string, toolName: string): Promise<void> {
    await this.setMcpToolDisabled(serverName, toolName, false);
  }

  async disableGlobalMcpTool(serverName: string, toolName: string): Promise<void> {
    await this.setMcpToolDisabled(serverName, toolName, true);
  }

  /** Write the complete per-server denylist in one `setToolFilter` call —
   *  the bulk-toggle primitive. The patch is the entry's complete new
   *  filter state; a bulk **enable** also lifts the allowlist exclusion for
   *  the targeted tools (mirroring the single-tool path — otherwise a
   *  "enable all" would leave allowlist-excluded tools stuck off, PS3.4).
   *  One call, one reconcile — concurrent per-tool writes are
   *  read-modify-write races and would collapse into the last writer's
   *  list. */
  async setMcpToolFilter(serverName: string, disabledTools: string[], enabledTools?: string[]): Promise<void> {
    const entry = (await this.mcpConfigEntries()).find(
      (candidate) => (candidate.runtimeName ?? candidate.name) === serverName,
    );
    // Without an explicit allowlist argument the stored allowlist rides
    // along unchanged; the bulk-enable path passes the lifted-in form.
    const allowlist = enabledTools ?? entry?.effective.enabledTools;
    await this.client.call('mcp.config.setToolFilter', {
      name: serverName,
      filter: {
        ...(allowlist !== undefined ? { enabledTools: allowlist } : {}),
        disabledTools,
      },
    });
  }

  /** Maintain the per-server tool denylist. The `setToolFilter` patch is
   *  the entry's complete new filter state (absent fields clear), so the
   *  stored allowlist must ride along unchanged — only the denylist is
   *  recomputed. */
  private async setMcpToolDisabled(
    serverName: string,
    toolName: string,
    disabled: boolean,
  ): Promise<void> {
    const entry = (await this.mcpConfigEntries()).find(
      (candidate) => (candidate.runtimeName ?? candidate.name) === serverName,
    );
    const current = entry?.effective.disabledTools ?? [];
    const next = disabled
      ? current.includes(toolName)
        ? current
        : [...current, toolName]
      : current.filter((tool) => tool !== toolName);
    // Re-enabling a tool the allowlist excluded must also lift the
    // allowlist exclusion — otherwise the engine's effective verdict stays
    // `enabled: false` no matter how often the switch is flipped on
    // (SDR-S005 / PS3.2: a disabled tool must be re-enableable).
    let enabledTools = entry?.effective.enabledTools;
    if (!disabled && enabledTools !== undefined && !enabledTools.includes(toolName)) {
      enabledTools = [...enabledTools, toolName];
    }
    await this.client.call('mcp.config.setToolFilter', {
      name: serverName,
      filter: {
        ...(enabledTools !== undefined ? { enabledTools } : {}),
        disabledTools: next,
      },
    });
  }

  async getGlobalMcpToggleState(): Promise<{
    disabledServers: string[];
    disabledTools: string[];
  }> {
    // Single source of truth stance (authoritative-methods.tsv, the
    // `mcp.toggleState` row): the retired RPC stays retired — this
    // derivation reads `mcp.config.list`'s effective state, itself an
    // engine-sourced view, so no second engine truth is minted. Servers
    // with `enabled: false` and the per-server denylist entries,
    // qualified with the `mcp__{server}__{tool}` names the panel counts
    // with (the same wire contract the v1 toggle-state endpoint answered).
    const disabledServers: string[] = [];
    const disabledTools: string[] = [];
    for (const entry of await this.mcpConfigEntries()) {
      const runtimeName = entry.runtimeName ?? entry.name;
      if (entry.effective.enabled === false) {
        disabledServers.push(runtimeName);
      }
      for (const tool of entry.effective.disabledTools ?? []) {
        disabledTools.push(`mcp__${runtimeName}__${tool}`);
      }
    }
    return { disabledServers, disabledTools };
  }

  /** Read-only tool snapshot for a disabled server (PS3.2): the persisted
   *  preference view from `mcp.config.list`'s effective entry — both filter
   *  arrays, so an enabledTools allowlist serves as the known tool list
   *  (every advertised tool not in it reads as disabled) and a plain
   *  denylist marks exactly its members disabled. Empty when the server
   *  has no stored preference. */
  async getDisabledServerToolSnapshot(serverName: string): Promise<{
    tools: Array<{ name: string; enabled: boolean }>;
  }> {
    const entry = (await this.mcpConfigEntries()).find(
      (candidate) => (candidate.runtimeName ?? candidate.name) === serverName,
    );
    if (entry === undefined) return { tools: [] };
    const allowlist = entry.effective.enabledTools;
    const denylist = entry.effective.disabledTools;
    if (allowlist !== undefined) {
      // The denylist wins over the allowlist in the engine's verdict
      // (`is_tool_allowed`), so a name present in both reads disabled.
      const denied = new Set(denylist);
      return { tools: allowlist.map((name) => ({ name, enabled: !denied.has(name) })) };
    }
    if (denylist !== undefined) {
      return { tools: denylist.map((name) => ({ name, enabled: false })) };
    }
    return { tools: [] };
  }

  async testConnectMcpServer(config: AppMcpServerConfig): Promise<AppMcpTestResult> {
    const result = await this.client.call<RpcMcpConfigTestResult>('mcp.config.test', {
      config,
    });
    return {
      status: result.status === 'ok' ? 'connected' : 'error',
      toolCount: result.toolCount,
      ...(result.error !== undefined ? { error: result.error } : {}),
      // The test result carries a tool count, not tool descriptors.
      tools: [],
    };
  }

  async connectMcpServer(
    name: string,
  ): Promise<{ server: AppMcpServer; tools: AppToolDescriptor[] }> {
    // `mcp.reconnect` performs a fresh connect + tool-discovery cycle on the
    // pool that owns the name (workspace first, then the user pool).
    await this.client.call('mcp.reconnect', { name });
    const [pool, configEntries] = await Promise.all([
      this.client.call<RpcMcpPoolEntry[]>('mcp.status'),
      this.mcpConfigEntries(),
    ]);
    // Same name-join + source-layer pick as listGlobalMcpServers, so the
    // refreshed card keeps its source badge and shadowed provenance instead
    // of degrading to the bare pool view.
    const effectiveConfig = pickEffectiveConfigEntry(configEntries, name);
    const entry = (pool ?? []).find((candidate) => candidate.name === name);
    if (entry === undefined) {
      throw new Error(`WsRpcApi.connectMcpServer: server '${name}' not found after reconnect`);
    }
    const config = effectiveConfig?.config;
    const server = toAppMcpServer(entry, isCompleteMcpConfig(config) ? config : undefined);
    server.source = effectiveConfig?.source ?? entry.source;
    // A filter-only override is not the server's definition — only a
    // complete entry's stored config joins the card.
    return {
      server,
      tools: [],
    };
  }

  // -------------------------------------------------------------------------
  // Auth — neutral default (auth is a REST infra route; the rust backend has
  // no auth RPC domain)
  // -------------------------------------------------------------------------

  async getAuth(): Promise<{
    ready: boolean;
    providersCount: number;
    defaultModel: string | null;
  }> {
    // The auth summary is a REST infra route (`GET /api/v1/auth`, the same v1
    // AuthSummary contract the TS daemon serves), and the REST probe doubles
    // as the server-credential check: a 401 here is what surfaces the
    // server-token dialog on first paint — the WS transport itself only
    // fails to connect, which the browser cannot distinguish from an
    // unreachable server.
    const res = await fetch(buildRestUrl(this.origin, '/auth'), {
      headers: this.authHeaders(),
    });
    if (res.status === 401) {
      markAuthRequired();
      throw new DaemonApiError({
        code: SERVER_AUTH_UNAUTHORIZED_CODE,
        msg: 'unauthorized',
        requestId: 'auth',
        status: res.status,
      });
    }
    if (!res.ok) throw new Error(`WsRpcApi.getAuth: HTTP ${res.status}`);
    const body = (await res.json()) as {
      code?: number;
      msg?: string;
      request_id?: string;
      data?: {
        ready?: boolean;
        providers_count?: number;
        default_model?: string | null;
      };
    };
    if (body.code !== 0) {
      throw new DaemonApiError({
        code: body.code ?? res.status,
        msg: body.msg ?? res.statusText,
        requestId: body.request_id ?? 'auth',
        status: res.status,
      });
    }
    const data = body.data ?? {};
    return {
      ready: data.ready === true,
      providersCount: data.providers_count ?? 0,
      defaultModel: data.default_model ?? null,
    };
  }
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

/** Map the v2 SessionView to the v1 AppSession; fields the v2 view does not
 *  expose get neutral defaults (documented per field where it matters). */
function toAppSession(view: RpcSessionView): AppSession {
  const usage = view.usage ?? {};
  return {
    id: view.id,
    title: view.title ?? '',
    createdAt: view.createdAt ?? '',
    updatedAt: view.updatedAt ?? '',
    // The P7c runtime fields: the engine's SessionView reports busy, the
    // first agent's model, the four-bucket usage, and the context message
    // count; older engine builds (or static-info-derived views) answer
    // neutral values, which map onto the same defaults as before.
    busy: view.busy === true,
    archived: view.archived === true,
    // The work-facts tuple (US4.4/SDR-013): the main conversation's turn
    // state — feeds the sidebar's "running" indicator after a refresh
    // (isMainTurnActive's listed fallback) and the action-required badge.
    mainTurnActive: view.mainTurnActive === true,
    pendingInteraction: view.pendingInteraction,
    lastTurnReason: view.lastTurnReason,
    lastPrompt: view.lastPrompt ?? undefined,
    model: view.model ?? '',
    cwd: view.cwd ?? '',
    usage: {
      inputTokens: usage.inputTokens ?? 0,
      outputTokens: usage.outputTokens ?? 0,
      cacheReadTokens: usage.cacheReadTokens ?? 0,
      cacheCreationTokens: usage.cacheCreationTokens ?? 0,
      totalCostUsd: 0,
      contextTokens: 0,
      contextLimit: 0,
      turnCount: 0,
      firstCall: usage.firstCall
        ? {
            inputTokens: usage.firstCall.inputTokens,
            outputTokens: usage.firstCall.outputTokens,
            cacheReadTokens: usage.firstCall.cacheReadTokens,
            cacheCreationTokens: usage.firstCall.cacheCreationTokens,
          }
        : undefined,
    },
    messageCount: view.messageCount ?? 0,
    lastSeq: view.lastSeq ?? 0,
  };
}

/** Map prompt content onto the rust `message.send` `input` parts (the v1
 *  TS REST content spellings the engine's entry normalizes): text parts pass
 *  through, image/video parts carry their `source` union, file parts pass
 *  their upload metadata (resolved server-side). Parts with no prompt wire
 *  form (tool uses, tool results, thinking) are dropped. */
export function promptContentToInputParts(content: AppMessageContent[]): Array<Record<string, unknown>> {
  const parts: Array<Record<string, unknown>> = [];
  for (const part of content) {
    switch (part.type) {
      case 'text':
        parts.push({ type: 'text', text: part.text });
        break;
      case 'image':
      case 'video': {
        const src = part.source;
        if (src.kind === 'base64') {
          parts.push({
            type: part.type,
            source: { kind: 'base64', media_type: src.mediaType, data: src.data },
          });
        } else if (src.kind === 'file') {
          parts.push({ type: part.type, source: { kind: 'file', file_id: src.fileId } });
        } else {
          parts.push({ type: part.type, source: { kind: 'url', url: src.url } });
        }
        break;
      }
      case 'file':
        parts.push({
          type: 'file',
          file_id: part.fileId,
          name: part.name,
          media_type: part.mediaType,
          size: part.size,
        });
        break;
      default:
        // toolUse / toolResult / thinking / unknown — no prompt wire form.
        break;
    }
  }
  return parts;
}

/** Map one `message.list` row to the v1 AppMessage shape. */
export function toAppMessage(wire: RpcWireMessage, sessionId: string): AppMessage {
  return {
    id: wire.id,
    sessionId,
    role: wire.role as AppMessageRole, // server serializes the role enum as a string
    content: rpcContentToAppContent(wire.content),
    // v2 message.list rows carry no createdAt yet — empty string = unknown.
    createdAt: '',
    // Passed through (undefined on older engines — the transcript key that
    // links the row to its per-turn usage pill).
    promptId: wire.promptId,
    // The turn's server-measured duration — rides the same last-assistant-
    // row position the live path stamps, so a restored transcript renders
    // the same "Worked for X" pill as the live one.
    durationMs: wire.durationMs,
    // Provenance rides through so the transcript filter can hide
    // system-injected user turns (metadata.origin.kind).
    metadata: wire.metadata,
    // The admission-time activation record (ADR-006) — feeds the bubble's
    // marker→chip rebuild on reloaded sessions (undefined on older engines).
    withSkills: wire.with_skills,
  };
}

/** Map the wire content (string when pure text, array of parts otherwise) to
 *  v1 content blocks; unrecognized parts degrade to `{type: 'unknown'}`.
 *  The protocol part shapes (tool_use / tool_result / thinking / image /
 *  video / file) ride the shared mapper `toAppMessageContent` from mappers.ts
 *  — the rust projection emits exactly that `WireMessageContent` shape
 *  contract; only the two degraded forms below are handled locally. */
function rpcContentToAppContent(content: unknown): AppMessageContent[] {
  if (typeof content === 'string') {
    return content.length > 0 ? [{ type: 'text', text: content }] : [];
  }
  if (!Array.isArray(content)) return [];
  return content.map((part): AppMessageContent => {
    if (typeof part !== 'object' || part === null) return { type: 'unknown', raw: part };
    const fields = part as Record<string, unknown>;
    if (fields['type'] === 'text' && typeof fields['text'] === 'string') {
      return { type: 'text', text: fields['text'] as string };
    }
    // Context-memory inline file form (resolved bytes, no store id — the
    // server degrades it to `{data,...}` because the protocol `file` part
    // requires a file_id; see rpc.rs project_content_part). The shared
    // mapper contracts on the id-carrying file shape, and the app file
    // block renders by id only — degrade honestly with the raw part kept.
    const hasFileId = fields['file_id'] !== undefined && fields['file_id'] !== null;
    const hasData = fields['data'] !== undefined && fields['data'] !== null;
    if (fields['type'] === 'file' && !hasFileId && hasData) {
      return { type: 'unknown', raw: part };
    }

    // Everything else: the shared protocol→app mapper (mappers.ts
    // toAppMessageContent); unknown part types degrade to `{type:'unknown'}`
    // there and never throw.
    return toAppMessageContent(part as WireMessageContent);
  });
}

/** Reduce a v1 QuestionAnswer to the single answer string v2 expects. The
 *  multi/other kinds cannot be expressed losslessly — the option ids are
 *  joined. `skipped` has no v2 question.answer form yet (the server has no
 *  question.dismiss handler) and is answered with an empty string. */
function questionAnswerToString(answer: { kind: string; optionId?: string; optionIds?: string[]; text?: string }): string {
  switch (answer.kind) {
    case 'single':
      return answer.optionId ?? '';
    case 'multi':
    case 'multiWithOther':
      return (answer.optionIds ?? []).join(', ');
    case 'other':
      return answer.text ?? '';
    case 'skipped':
      return '';
    default:
      return '';
  }
}

// ---------------------------------------------------------------------------
// RPC response interfaces + mapping helpers (new methods)
// ---------------------------------------------------------------------------

/** One `session.snapshot` `turn_usages` entry — the per-turn fold of a
 *  completed turn's step usage (camelCase inner keys, the same buckets
 *  `event.turn.step.completed` streams live). */
interface RpcTurnUsage {
  promptId: string;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
  models?: string[];
}

/** `session.snapshot` result — atomic initial-sync view. The rust server's
 *  wire shape mirrors the v2 REST contract (`rest-snapshot.ts`
 *  `sessionSnapshotResponseSchema`): a snake_case envelope (`as_of_seq`,
 *  `messages.{items,has_more}`, `in_flight_turn`, `subagents`,
 *  `pending_approvals`, `pending_questions`, `turn_usages`) around a
 *  camelCase nested session view. `subagents` is optional for
 *  cross-version tolerance (older servers omit it); `in_flight_turn` and
 *  the pending arrays are nullable/optional for cross-version tolerance
 *  too (older servers sent null/empty placeholders). */
interface RpcSnapshot {
  as_of_seq: number;
  epoch: string;
  session: RpcSessionView;
  messages: { items: RpcWireMessage[]; has_more: boolean };
  /** Optional-on-the-wire: an older engine omits the field entirely
   *  (undefined), newer engines send null when no turn is in flight. */
  in_flight_turn?: RpcInFlightTurn | null;
  /** Per-turn usage of completed historical turns — optional on the wire:
   *  older engines omit it entirely. */
  turn_usages?: RpcTurnUsage[];
  /** The supervision member roster at the watermark — the engine's
   *  subagent-registry rows joined with honest statuses (supervision
   *  architecture §4.2). Same wire shape as the REST background-task row. */
  subagents?: WireBackgroundTask[];
  pending_approvals?: RpcApprovalRequest[];
  pending_questions?: RpcQuestionRequest[];
}

/** `session.snapshot`'s `in_flight_turn` — the TS `inFlightTurnSchema`
 *  shape. The rust engine's `turn_id` is its wire `turn-<uuid>` string
 *  (not the TS numeric turn counter); `current_prompt_id` has no rust
 *  counterpart and is omitted from the accumulator — the server stamps it
 *  (and `current_turn_started_at`) onto the snapshot's in-flight turn from
 *  the wire-log fold's still-open tail bracket. */
interface RpcInFlightTurn {
  turn_id: string | number;
  assistant_text: string;
  thinking_text: string;
  running_tools: Array<{
    tool_call_id: string;
    name: string;
    args?: unknown;
    description?: string;
    last_progress?: {
      kind: string;
      text?: string;
      percent?: number;
    };
  }>;
  current_prompt_id?: string;
  /** The running turn's real start time (epoch-ms), stamped by the rust
   *  engine from the wire log (see `read_only_open_turn_started_at`).
   *  Absent on engines that predate the field or when the log cannot
   *  resolve the open turn. */
  current_turn_started_at?: number;
}

/** `pending_approvals` row — the TS `approvalRequestSchema` shape (the rust
 *  gate snapshot; `expires_at` has no engine counterpart and is omitted).
 *  `approvalRule` is the klient camelCase facet spelling (B9 R4): the rust
 *  snapshot rows do not carry it yet — declared so the hydration path stays
 *  wire-faithful the day they do. */
interface RpcApprovalRequest {
  approval_id: string;
  session_id: string;
  turn_id?: number;
  tool_call_id?: string;
  tool_name: string;
  action: string;
  tool_input_display?: unknown;
  approvalRule?: string;
  approval_rule?: string;
  /** The requesting member's full agent id (supervision B1) — present only
   *  on member-originated approvals. */
  agent_id?: string;
  created_at: string;
}

/** `pending_questions` row — the TS `questionRequestSchema` shape. */
interface RpcQuestionRequest {
  question_id: string;
  session_id: string;
  turn_id?: number;
  tool_call_id?: string;
  questions: unknown;
  created_at: string;
}

/** `workspace.list` / `workspace.create` / `workspace.update` row. The
 *  `trusted` field (W8) is the live store-read verdict on the rust wire. */
interface RpcWorkspace {
  id: string;
  root: string;
  name: string;
  isGitRepo: boolean;
  branch?: string;
  lastOpenedAt?: string;
  sessionCount: number;
  trusted: boolean;
}

/** `workspace.delete` result — the W6 three-choice wire face
 *  `{ok, mode, workspaceId, deletedSessions}` (camelCase keys). */
interface RpcWorkspaceDeletionResult {
  ok: boolean;
  mode: WorkspaceDeleteMode;
  workspaceId: string;
  deletedSessions: number;
}

/** `model.list` row — snake_case wire fields per the klient
 *  `modelCatalogItemSchema`, plus `id` (the canonical `[models.<id>]`
 *  config key — the model alias, the identifier every write path
 *  addresses models by, ADR-003). Engines that predate the field omit
 *  it; the adapter then falls back to composing `${provider}/${model}`. */
export interface RpcModel {
  id?: string;
  provider: string;
  model: string;
  display_name?: string;
  max_context_size: number;
  capabilities?: string[];
  support_efforts?: string[];
  default_effort?: string;
}

/** `model.listProviders` row — snake_case wire fields per the klient
 *  `providerCatalogItemSchema`. */
export interface RpcProvider {
  id: string;
  type: string;
  base_url?: string;
  default_model?: string;
  has_api_key: boolean;
  status: 'connected' | 'error' | 'unconfigured';
  models?: string[];
}

/** `providerDiscovery.listCatalog` model row — snake_case wire fields per
 *  the klient providerCatalogModelEntrySchema (the browse shape plus the
 *  directory's capability facts, ADR-004 additive: `context_length` is
 *  the directory's limit.context, `capabilities` the alias vocabulary,
 *  `reasoning` the thinking fact — all omitted when the directory does
 *  not declare them, never null). */
export interface RpcCatalogModel {
  id: string;
  name?: string;
  context_length?: number;
  capabilities?: string[];
  reasoning?: boolean;
}

/** `providerDiscovery.listCatalog` row — snake_case wire fields per the
 *  klient providerCatalogEntrySchema (the pruned browse shape: no
 *  guessed/rejected/reject_reason facts, optional fields omitted). */
export interface RpcCatalogProvider {
  id: string;
  name: string;
  wire_type?: AppProviderWireType;
  needs_base_url: boolean;
  env_key?: string;
  models: RpcCatalogModel[];
}

/** `providerDiscovery.refreshProviderModels` result — snake_case wire fields
 *  per the klient refreshProviderModelsResponseSchema (the managed-login
 *  `auth.refreshOAuthProviderModels` surface was retired with the managed
 *  auth domain; providerDiscovery is the one refresh path). */
export interface RpcProviderRefreshResult {
  changed: Array<{
    provider_id: string;
    provider_name: string;
    added: number;
    removed: number;
  }>;
  unchanged: string[];
  failed: Array<{ provider: string; reason: string }>;
}

/** `agent.listProfiles` / `agent.getProfile` row — the rust `ProfileEntry`
 *  wire shape (snake_case, `packages/protocol/src/rest/agents.ts`). */
interface RpcAgentProfile {
  name: string;
  description?: string;
  source: RpcAgentProfileSource;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  has_override: boolean;
  file_path?: string;
  default_model?: string;
  tools?: string[];
  disallowed_tools?: string[];
  capabilities_required?: string[];
  subagents?: string[];
  suppressed_candidates?: RpcAgentSuppressedCandidate[];
  when_to_use?: string;
  system_prompt_template?: string;
  effective_system_prompt?: string;
  prompt_vars?: Record<string, string>;
}

/** The six-value wire `source` enum (Project occupies the TS registry's
 *  workspace slot — same source, priority 30). */
type RpcAgentProfileSource =
  | 'builtin'
  | 'user'
  | 'project'
  | 'extra'
  | 'plugin'
  | 'explicit';

/** A same-name candidate that lost the merge to the resolved profile. */
interface RpcAgentSuppressedCandidate {
  source: RpcAgentProfileSource;
  priority: number;
  reason: string;
}

/** `agent.listProfiles` response — the `{items, skipped}` envelope (R3-6):
 *  the profile rows plus the discovery pass's skipped files. */
interface RpcAgentProfileList {
  items: RpcAgentProfile[];
  skipped?: { path: string; reason: string }[];
}

/** `tool.list` / `tool.catalog` row. */
interface RpcToolDescriptor {
  name: string;
  description: string;
  source: 'builtin' | 'skill' | 'mcp';
  mcpServerId?: string;
}

/** `mcp.list` / `mcp.status` row — the pool view of one connected server
 *  (runtime state plus `pool`/`source` provenance; spec §4.4). */
interface RpcMcpPoolEntry {
  name: string;
  transport: 'stdio' | 'http' | 'sse';
  status: 'pending' | 'connected' | 'failed' | 'disabled' | 'needs-auth';
  toolCount: number;
  pool: 'user' | 'workspace' | 'plugin';
  source: 'user' | 'project' | 'plugin';
  error?: string;
}

/** `mcp.config.list` row — one layered-config entry (the stored config
 *  verbatim, `${VAR}` references included, plus the derived effective
 *  state; spec §4.1). The stored config may be a filter-only override
 *  entry (ADR-006/A7): no transport, only the filter family. */
interface RpcMcpConfigEntry {
  name: string;
  source: 'user' | 'projectRoot' | 'project' | 'plugin';
  /** Present on plugin-sourced entries (`plugin-<id>:<name>`). */
  runtimeName?: string;
  config: AppMcpServerConfig | AppMcpFilterOnlyOverride;
  effective: {
    enabled: boolean;
    enabledTools?: string[];
    disabledTools?: string[];
    startupTimeoutMs?: number;
    toolTimeoutMs?: number;
  };
}

/** A stored filter-only override entry (ADR-006/A7): the connection family
 *  stays with the shadowed layer, so only the filter family rides along. */
interface AppMcpFilterOnlyOverride {
  enabled?: boolean;
  enabledTools?: string[];
  disabledTools?: string[];
  startupTimeoutMs?: number;
  toolTimeoutMs?: number;
}

/** Whether a stored config entry defines a connection (the complete
 *  shape the panel edits) rather than a filter-only override. */
function isCompleteMcpConfig(
  config: AppMcpServerConfig | AppMcpFilterOnlyOverride | undefined,
): config is AppMcpServerConfig {
  return config !== undefined && 'transport' in config;
}

/** `mcp.config.test` result — the fresh-connect probe's answer. */
interface RpcMcpConfigTestResult {
  status: 'ok' | 'error';
  toolCount: number;
  error?: string;
}

/** Map the writable config-layer scope for a `mcp.config.set` write onto the
 *  entry's defining layer (A2: the project root's `.mcp.json` is a read-only
 *  git-shared asset; plugin definitions are manifest-owned). */
function mcpWritableScope(source: RpcMcpConfigEntry['source']): 'user' | 'workspace' {
  switch (source) {
    case 'user':
      return 'user';
    case 'project':
      return 'workspace';
    case 'projectRoot':
      throw new Error(
        "the project root's .mcp.json is a read-only git-shared asset; edit it in the repository",
      );
    case 'plugin':
      throw new Error(
        'plugin-bundled MCP server definitions are read-only; use the plugin surface to change their state',
      );
  }
}

/** Map one `mcp.status` pool row to the v1 AppMcpServer card shape (joining
 *  in the stored config when the layered view defines the name). */
function toAppMcpServer(entry: RpcMcpPoolEntry, config?: AppMcpServerConfig): AppMcpServer {
  let status: AppMcpServer['status'];
  let lastError: string | undefined = entry.error;
  switch (entry.status) {
    case 'connected':
      status = 'connected';
      break;
    case 'pending':
      status = 'connecting';
      break;
    case 'failed':
      status = 'error';
      break;
    case 'needs-auth':
      status = 'error';
      lastError ??= 'authentication required';
      break;
    default:
      status = 'disconnected';
  }
  return {
    // The pool-qualified id keeps cross-pool same-name entries (the
    // no-shadowing view) from colliding as Vue list keys.
    id: `${entry.pool}:${entry.name}`,
    name: entry.name,
    transport: entry.transport,
    status,
    toolCount: entry.toolCount,
    ...(lastError !== undefined ? { lastError } : {}),
    ...(config !== undefined ? { config } : {}),
  };
}

/** Config-layer precedence for same-name entries (user < projectRoot <
 *  project; plugin definitions lose to any file layer) — mirrors the
 *  engine's merge order from the config spec. */
const MCP_SOURCE_PRECEDENCE: readonly RpcMcpConfigEntry['source'][] = [
  'plugin',
  'user',
  'projectRoot',
  'project',
];

/** Precedence rank of a config layer (higher wins for same-name entries). */
function mcpSourceRank(source: RpcMcpConfigEntry['source']): number {
  return MCP_SOURCE_PRECEDENCE.indexOf(source);
}

/** The effective (highest-precedence) layered config entry for a server
 *  name — the card edits/joins that definition, not a shadowed one's. */
function pickEffectiveConfigEntry(
  entries: readonly RpcMcpConfigEntry[],
  name: string,
): RpcMcpConfigEntry | undefined {
  let effective: RpcMcpConfigEntry | undefined;
  for (const entry of entries) {
    if ((entry.runtimeName ?? entry.name) !== name) continue;
    if (effective === undefined || mcpSourceRank(entry.source) > mcpSourceRank(effective.source)) {
      effective = entry;
    }
  }
  return effective;
}

/**
 * Collapse the no-shadowing pool view into the settings page's merged
 * effective view (PS1.1): one card per server name. The highest-precedence
 * entry supplies the card (its connection state is the effective one —
 * the engine connects every pool, but the workspace-pool connection owns
 * the name's tools); the remaining same-name entries are attached as
 * `shadowedPools` provenance so the UI can surface "defined in N scopes".
 */
function mergeSameNameMcpServers(servers: AppMcpServer[]): AppMcpServer[] {
  const byName = new Map<string, AppMcpServer[]>();
  for (const server of servers) {
    const bucket = byName.get(server.name);
    if (bucket === undefined) byName.set(server.name, [server]);
    else bucket.push(server);
  }
  const merged: AppMcpServer[] = [];
  for (const bucket of byName.values()) {
    if (bucket.length === 1) {
      merged.push(bucket[0]!);
      continue;
    }
    // Descending by precedence: the first entry is the effective one, the
    // rest are the shadowed pools' provenance.
    const ranked = [...bucket].toSorted(
      (a, b) => mcpSourceRank(b.source ?? 'user') - mcpSourceRank(a.source ?? 'user'),
    );
    merged.push({
      ...ranked[0]!,
      shadowedPools: ranked.slice(1).map((entry) => entry.id.split(':', 2)[0]!),
    });
  }
  return merged;
}

/** `skill.list` row. */
interface RpcSkill {
  name: string;
  description: string;
  source: string;
  /** Absolute SKILL.md path — the inline-reference resolution authority
   * (US3.6/ADR-004; older engines omit, dir-only engines return the skill
   * directory here — web consumers must not assume it ends in SKILL.md). */
  path?: string;
  /** Skill directory (mirrors the engine's dir field; optional). */
  dir?: string;
  /** False when user-disabled; absent (older engines) means enabled. */
  enabled?: boolean;
}

/** `task.list` / `task.listWorkspace` row — the direct AgentTask
 * serialization the engine serves for every task-list face: snake_case
 * fields with the internally tagged `status`/`kind` enums
 * (`{"status": "completed", "detail": {"exit_code": 0}}`,
 * `{"kind": "process", "command": "…"}`). */
interface RpcWorkspaceTask {
  id: string;
  description: string;
  kind:
    | { kind: 'process'; command?: string }
    | {
        kind: 'agent';
        agentId?: string;
        subagentType?: string;
        parentToolCallId?: string;
        swarmIndex?: number;
        model?: string;
      }
    | { kind: 'question'; questionCount: number };
  status:
    | { status: 'running' }
    | { status: 'completed'; detail: { exit_code: number } }
    | { status: 'failed'; detail: string }
    | { status: 'stopped' };
  created_at: string;
  ended_at: string | null;
  output: string;
  /** Riding only when the engine truncated `output` in the summary view
   * (S5): the full output length in bytes, so the row's byte count stays
   * honest. */
  output_bytes?: number;
  output_truncated?: boolean;
  /** Riding inside `kind` only when the engine head-capped the command line
   * in the summary view (S5). */
  command_truncated?: boolean;
  session_id: string;
  detached: boolean;
}

/** `task.detach` answer — the BackgroundTaskInfo snapshot of the converted
 * task (camelCase fields; the flattened kind union rides its discriminator
 * and members at the top level). Null = unknown or already settled. */
interface RpcDetachedTaskInfo {
  kind: 'process' | 'agent' | 'question';
  command?: string;
  agentId?: string;
  subagentType?: string;
  taskId: string;
  description: string;
  /** Protocol status vocabulary — `killed` is the manager-stopped spelling. */
  status: 'running' | 'completed' | 'failed' | 'killed';
  /** Registration instant, epoch milliseconds. */
  startedAt: number;
  /** Terminal instant, epoch milliseconds; null while running. */
  endedAt: number | null;
  stopReason?: string;
  exitCode?: number;
}

/** `cron.list` row (B6; timestamps are epoch ms, `nextFireAt` absent once a
 *  one-shot has fired). */
interface RpcCronTask {
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

/** `cron.list` result (B6): the session's task rows under the `{tasks: [...]}`
 *  envelope (the rust CronTasksResult — same envelope family as
 *  RpcPermissionRuleList). */
interface RpcCronTaskList {
  tasks: RpcCronTask[];
}

/** `approval.listRules` / `approval.addRule` / `approval.removeRule`'s shared
 *  result (B9): the user-level rule list of the shared config snapshot.
 *  Each row omits `reason` when absent (zod `.optional()` rejects null). */
interface RpcPermissionRuleList {
  rules: Array<{
    decision: 'allow' | 'deny' | 'ask';
    scope: 'user';
    pattern: string;
    reason?: string;
  }>;
}

/** `file.list` result. */
interface RpcListDirResult {
  items: RpcFsEntry[];
  childrenByPath?: Record<string, RpcFsEntry[]>;
  truncated: boolean;
}

/** One entry from `file.list`. */
interface RpcFsEntry {
  path: string;
  name: string;
  kind: string;
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

/** Map `session.snapshot` to `AppSessionSnapshot`. The wire is the v2 REST
 *  shape (snake_case envelope, `messages.items` page); the in-flight turn
 *  seeds the projector's mid-turn state (the seeding runs in
 *  `seedSnapshot` — seed must stay before subscribe) and the pending rows
 *  reuse the shared approval/question mappers. */
export function toAppSnapshot(snapshot: RpcSnapshot, sessionId: string): AppSessionSnapshot {  return {
    asOfSeq: snapshot.as_of_seq ?? 0,
    epoch: snapshot.epoch ?? '',
    session: toAppSession(snapshot.session),
    messages: (snapshot.messages?.items ?? []).map((message) => toAppMessage(message, sessionId)),
    hasMoreMessages: snapshot.messages?.has_more ?? false,
    inFlightTurn:
      // Truthy guard (not `=== null`): an older engine can OMIT the field
      // entirely (undefined), which must default the same way as an explicit
      // null — reading `.turn_id` off undefined would throw.
      snapshot.in_flight_turn
        ? {
            turnId: snapshot.in_flight_turn.turn_id,
            assistantText: snapshot.in_flight_turn.assistant_text ?? '',
            thinkingText: snapshot.in_flight_turn.thinking_text ?? '',
            runningTools: (snapshot.in_flight_turn.running_tools ?? []).map((tool) => ({
              toolCallId: tool.tool_call_id,
              name: tool.name,
              args: tool.args,
              description: tool.description,
              lastProgress: tool.last_progress,
            })),
            promptId: snapshot.in_flight_turn.current_prompt_id,
            startedAtMs: snapshot.in_flight_turn.current_turn_started_at,
          }
        : null,
    turnUsages: (snapshot.turn_usages ?? []).map((entry) => ({
      promptId: entry.promptId,
      inputTokens: entry.inputTokens,
      outputTokens: entry.outputTokens,
      cacheReadTokens: entry.cacheReadTokens,
      cacheCreationTokens: entry.cacheCreationTokens,
      models: entry.models ?? [],
    })),
    subagents: (snapshot.subagents ?? []).map((row) => toAppTaskFromWire(row)),
    pendingApprovals: (snapshot.pending_approvals ?? []).map((approval) =>
      toAppPendingApproval(approval, sessionId),
    ),
    pendingQuestions: (snapshot.pending_questions ?? []).map((question) => ({
      questionId: question.question_id,
      sessionId: question.session_id ?? sessionId,
      turnId: question.turn_id,
      toolCallId: question.tool_call_id,
      questions: normalizeSnapshotQuestionItems(question.questions),
      createdAt: question.created_at,
    })),
  };
}

/** Normalize one `session.snapshot` pending-question row's items onto the
 *  app `QuestionItem` shape. Two dialects reach this path: the rust engine's
 *  klient camelCase items (`multiSelect`/`allowOther`, from the interaction
 *  kernel's projection) and the TS daemon's snake_case rows
 *  (`multi_select`/`allow_other`). Each flag reads both spellings; other
 *  fields pass through untouched (the item shape is otherwise shared).
 *
 *  Item and option ids are minted when the row carries none (the rust gate
 *  transparently forwards the AskUserQuestion tool input, which has no ids)
 *  using the SAME scheme as the live `question.ask` bridge — item `q<idx>`,
 *  option the label — so a restored card submits the same ids a live card
 *  would, and the answer string the engine feeds the model stays the option
 *  label. Without unique ids every option's id is undefined and selecting
 *  one selects them all. */
function normalizeSnapshotQuestionItems(items: unknown): AppQuestionRequest['questions'] {
  if (!Array.isArray(items)) return [];
  return items.map((raw, itemIdx) => {
    const item = (raw ?? {}) as Record<string, unknown>;
    const multiSelect = item['multiSelect'] ?? item['multi_select'];
    const allowOther = item['allowOther'] ?? item['allow_other'];
    const rawOptions = Array.isArray(item['options']) ? item['options'] : [];
    const options = rawOptions.map((rawOption, optIdx) => {
      const option = (rawOption ?? {}) as Record<string, unknown>;
      const label = typeof option['label'] === 'string' ? option['label'] : '';
      return {
        ...option,
        // Same mint as the live question.ask bridge: the label IS the id, so
        // the answered id renders as the chosen label.
        id: typeof option['id'] === 'string' && option['id'].length > 0 ? option['id'] : label || `opt-${itemIdx}-${optIdx}`,
      };
    });
    return {
      ...item,
      ...(Array.isArray(item['options']) ? { options } : {}),
      ...(multiSelect !== undefined ? { multiSelect } : {}),
      ...(allowOther !== undefined ? { allowOther } : {}),
      ...(typeof item['id'] === 'string' && item['id'].length > 0 ? {} : { id: `q${itemIdx}` }),
    } as AppQuestionRequest['questions'][number];
  });
}

/** Map one `session.snapshot` pending-approval row (`RpcApprovalRequest`)
 *  onto the app ask shape — the same card the `approval.ask` reverse-RPC
 *  produces, so the resize/resync path reuses the existing
 *  `approvalRequested` rendering instead of duplicating it. */
export function toAppPendingApproval(
  approval: RpcApprovalRequest,
  sessionId: string,
): AppApprovalRequest {
  // The engine approval-rule facet (B9) — klient camelCase spelling,
  // snake_case tolerated for symmetry with the other dual reads; a
  // non-string (e.g. JSON null) reads as absent. Mapped only when the
  // row carries one.
  const facet: unknown = approval.approvalRule ?? approval.approval_rule;
  // The requesting member's identity (supervision B1) — present only when
  // the row attributes the ask to a registered member agent.
  const agentId: unknown = approval.agent_id;
  return {
    approvalId: approval.approval_id,
    sessionId: approval.session_id ?? sessionId,
    turnId: approval.turn_id,
    toolCallId: approval.tool_call_id ?? '',
    toolName: approval.tool_name,
    action: approval.action,
    display: approval.tool_input_display,
    ...(typeof facet === 'string' ? { approvalRule: facet } : {}),
    ...(typeof agentId === 'string' ? { agentId } : {}),
    // Honest expiry (D-13): the rust gate snapshot carries no `expires_at`
    // — the field stays absent instead of a fabricated empty string.
    createdAt: approval.created_at,
  };
}

/** Rebuild the approval cards from the gate's read face (ADR-011 e): one
 *  `approvalRequested` AppEvent per pending-approval row of a
 *  `session.snapshot`. Used after (re)subscribe so a reconnected/fresh
 *  client restores its `approvalsBySession` even though the pre-disconnect
 *  `approval.ask` notification died with the connection. The cards are
 *  deliberately NOT parked as answerable reverse-RPC asks — deciding one
 *  goes through `approval.decide`, which is exactly where the engine's
 *  gate resolves. */
export function pendingApprovalEventsFromSnapshot(
  snapshot: Pick<RpcSnapshot, 'pending_approvals'>,
  sessionId: string,
): Array<Extract<AppEvent, { type: 'approvalRequested' }>> {
  return (snapshot.pending_approvals ?? []).map((row) => ({
    type: 'approvalRequested',
    sessionId,
    approval: toAppPendingApproval(row, sessionId),
  }));
}

function toAppWorkspace(workspace: RpcWorkspace): AppWorkspace {
  return {
    id: workspace.id,
    root: workspace.root,
    name: workspace.name,
    isGitRepo: workspace.isGitRepo ?? false,
    branch: workspace.branch,
    lastOpenedAt: workspace.lastOpenedAt,
    sessionCount: workspace.sessionCount ?? 0,
    trusted: workspace.trusted ?? false,
  };
}

/** Map a `model.list` wire row (snake_case) to the app shape. The id is
 *  the wire `id` — the canonical config key (the model alias, ADR-003)
 *  — so every write (set default model, switch session model, delete)
 *  addresses models by config key. Engines that predate the field fall
 *  back to the composed `${provider}/${model}` display key. */
export function toAppModel(model: RpcModel): AppModel {
  return {
    id: model.id ?? `${model.provider}/${model.model}`,
    provider: model.provider,
    model: model.model,
    displayName: model.display_name,
    maxContextSize: model.max_context_size ?? 0,
    capabilities: model.capabilities,
    supportEfforts: model.support_efforts,
    defaultEffort: model.default_effort,
  };
}

/** Map a `model.listProviders` wire row (snake_case) to the app shape. The
 *  engine does not expose `api_key_display`; `AppProvider.apiKeyDisplay`
 *  stays undefined on the rust path. */
export function toAppProvider(provider: RpcProvider): AppProvider {
  return {
    id: provider.id,
    type: provider.type,
    baseUrl: provider.base_url,
    defaultModel: provider.default_model,
    hasApiKey: provider.has_api_key ?? false,
    status: provider.status ?? 'unconfigured',
    models: provider.models,
  };
}

/** Map a `providerDiscovery.listCatalog` wire row (snake_case) to the app
 *  catalog shape. The browse wire spells the model context `context_length`
 *  (the directory's limit.context) — the app shape calls it `maxContextSize`
 *  (the boundary mapping; the spelling is never "corrected" on the wire). A
 *  model without a declared limit maps to 0 (the engine's import stores
 *  the same honest zero). The pruned wire carries no guessed/rejected/
 *  reject_reason facts, so those stay undefined here — never fabricated —
 *  and `wireType`/`envKey` degrade to null when the directory does not
 *  declare them. The capability facts pass through untouched — never
 *  fabricated; `reasoning` defaults to false only when the engine omits
 *  it. */
export function toAppCatalogProvider(wire: RpcCatalogProvider): AppCatalogProvider {
  return {
    id: wire.id,
    name: wire.name,
    wireType: wire.wire_type ?? null,
    needsBaseUrl: wire.needs_base_url ?? false,
    envKey: wire.env_key ?? null,
    models: (wire.models ?? []).map((model) => ({
      id: model.id,
      name: model.name,
      maxContextSize: model.context_length ?? 0,
      capabilities: model.capabilities,
      reasoning: model.reasoning ?? false,
    })),
  };
}

/** Map a `providerDiscovery.refreshProviderModels` result (snake_case wire
 *  per the klient refreshProviderModelsResponseSchema) to the app
 *  `ProviderRefreshResult` shape — the same boundary mapping the v1 client's
 *  `toProviderRefreshResult` applies. */
export function toAppProviderRefreshResult(
  result: RpcProviderRefreshResult,
): ProviderRefreshResult {
  return {
    changed: (result.changed ?? []).map((entry) => ({
      providerId: entry.provider_id,
      providerName: entry.provider_name,
      added: entry.added,
      removed: entry.removed,
    })),
    unchanged: result.unchanged ?? [],
    failed: result.failed ?? [],
  };
}

/** Map an `agent.listProfiles` / `agent.getProfile` row (snake_case wire)
 * to the app `AppAgentProfile` shape — the same boundary mapping the v1
 * client's `toAppProfile` applies to the REST rows. Exported for unit
 * testing over a stubbed `call` (the WsRpcApi class builds its own
 * transport). */
export function toAppAgentProfile(agent: RpcAgentProfile): AppAgentProfile {
  return {
    name: agent.name,
    description: agent.description,
    source: agent.source ?? 'user',
    builtin: agent.builtin ?? false,
    essential: agent.essential ?? false,
    enabled: agent.enabled ?? true,
    hasOverride: agent.has_override ?? false,
    filePath: agent.file_path,
    defaultModel: agent.default_model,
    tools: agent.tools,
    disallowedTools: agent.disallowed_tools,
    capabilitiesRequired: agent.capabilities_required,
    subagents: agent.subagents,
    suppressedCandidates: agent.suppressed_candidates?.map((candidate) => ({
      source: candidate.source,
      priority: candidate.priority,
      reason: candidate.reason,
    })),
    whenToUse: agent.when_to_use,
    systemPromptTemplate: agent.system_prompt_template,
    effectiveSystemPrompt: agent.effective_system_prompt,
  };
}

/**
 * Map a failed agent-profile write/lifecycle RPC onto the shared
 * `DaemonApiError` so the panel's existing toast path receives the engine's
 * own `msg` verbatim: the -32001 not-found and -32602 refusal faces
 * (essential locks, name validation, merge guards) ride the JSON-RPC error
 * code, which the REST path carries as the envelope `code`. Anything that is
 * not a JSON-RPC error (transport failure, programming error) passes through
 * untouched.
 */
function agentProfileRpcError(method: string, error: unknown): unknown {
  if (error instanceof JsonRpcCallError) {
    return new DaemonApiError({
      code: error.code,
      msg: error.message,
      requestId: method,
    });
  }
  return error;
}

function toAppToolDescriptor(tool: RpcToolDescriptor): AppToolDescriptor {
  return {
    name: tool.name,
    description: tool.description ?? '',
    source: tool.source ?? 'builtin',
    mcpServerId: tool.mcpServerId,
  };
}

function toAppSkill(skill: RpcSkill): AppSkill {
  return {
    name: skill.name,
    description: skill.description ?? '',
    source: skill.source ?? 'builtin',
    // Management fields (B8): pass through verbatim when the engine sent
    // them; absent `enabled` is NOT a false (older backends have no
    // per-skill disable state — treat those rows as enabled).
    ...(skill.path !== undefined ? { path: skill.path } : {}),
    ...(skill.dir !== undefined ? { dir: skill.dir } : {}),
    ...(skill.enabled !== undefined ? { enabled: skill.enabled } : {}),
  };
}

/**
 * Fetch the workspace skill list over one `skill.list` call (P7d): the rust
 * engine's catalog is workspace-scoped — its dispatch ignores the session
 * id — so the workspace view reads the same route the session view uses.
 * Exported for unit testing over a stubbed `call` (the WsRpcApi class
 * builds its own transport).
 */
export async function fetchWorkspaceSkills(
  call: (method: 'skill.list', params: Record<string, unknown>) => Promise<unknown>,
  workspaceId: string,
): Promise<AppSkill[]> {
  const skills = await call('skill.list', { workspaceId });
  return (Array.isArray(skills) ? skills : []).map(toAppSkill);
}

/** Map the direct AgentTask serialization (the element shape task.list and
 * task.listWorkspace serve) onto the AppTask row: `stopped` becomes the
 * app's `cancelled`, the kind union collapses onto the app's bash/subagent/
 * tool vocabulary, and `detached` rides as runInBackground. The rust row
 * records no separate start instant — a background task starts when it
 * registers, so `created_at` doubles as `startedAt` (the dock's running
 * elapsed timer keys on it). */
function toAppTaskFromAgentTask(task: RpcWorkspaceTask): AppTask {
  return {
    id: task.id,
    sessionId: task.session_id,
    kind: wireTaskKindToApp(task.kind.kind),
    description: task.description,
    status: wireTaskStatusToApp(task.status.status),
    command: task.kind.kind === 'process' ? task.kind.command : undefined,
    createdAt: task.created_at,
    startedAt: task.created_at,
    completedAt: task.ended_at ?? undefined,
    outputPreview: task.output,
    // The summary view's `output_bytes` (the full length) wins when the
    // engine truncated; otherwise the preview's own length is the truth.
    outputBytes: task.output_bytes ?? task.output.length,
    agentId: task.kind.kind === 'agent' ? task.kind.agentId : undefined,
    subagentType: task.kind.kind === 'agent' ? task.kind.subagentType : undefined,
    parentToolCallId: task.kind.kind === 'agent' ? task.kind.parentToolCallId : undefined,
    swarmIndex: task.kind.kind === 'agent' ? task.kind.swarmIndex : undefined,
    model: task.kind.kind === 'agent' ? task.kind.model : undefined,
    runInBackground: task.detached,
  };
}

/** Map the `task.detach` BackgroundTaskInfo snapshot onto the AppTask row:
 * `killed` becomes the app's `cancelled`, and the epoch-ms instants become
 * ISO strings (the AppTask timestamp spelling). */
function toAppTaskFromBackgroundInfo(sessionId: string, info: RpcDetachedTaskInfo): AppTask {
  const startedAt = Number.isFinite(info.startedAt)
    ? new Date(info.startedAt).toISOString()
    : '';
  return {
    id: info.taskId,
    sessionId,
    kind: wireTaskKindToApp(info.kind),
    description: info.description,
    status: info.status === 'killed' ? 'cancelled' : info.status,
    command: info.kind === 'process' ? info.command : undefined,
    createdAt: startedAt,
    startedAt,
    completedAt:
      info.endedAt !== null && info.endedAt !== undefined && Number.isFinite(info.endedAt)
        ? new Date(info.endedAt).toISOString()
        : undefined,
    agentId: info.kind === 'agent' ? info.agentId : undefined,
    subagentType: info.kind === 'agent' ? info.subagentType : undefined,
    // A detach answer describes a task the manager now runs in the
    // background — that is the whole point of the conversion.
    runInBackground: true,
  };
}

function wireTaskKindToApp(kind: 'process' | 'agent' | 'question'): AppTask['kind'] {
  if (kind === 'process') return 'bash';
  if (kind === 'agent') return 'subagent';
  return 'tool';
}

function wireTaskStatusToApp(status: 'running' | 'completed' | 'failed' | 'stopped'): AppTaskStatus {
  if (status === 'stopped') return 'cancelled';
  return status;
}

function toAppCronTask(task: RpcCronTask): AppCronTask {
  return {
    id: task.id,
    cron: task.cron,
    prompt: task.prompt,
    recurring: task.recurring,
    timezone: task.timezone,
    stale: task.stale,
    createdAt: task.createdAt,
    lastFiredAt: task.lastFiredAt,
    nextFireAt: task.nextFireAt,
  };
}

/** Map the shared `{rules: [...]}` CRUD result onto the app rows; a null /
 *  absent list degrades to the honest empty list. */
function toAppPermissionRules(result: RpcPermissionRuleList | null): AppPermissionRule[] {
  return (result?.rules ?? []).map((rule) => ({
    decision: rule.decision,
    scope: rule.scope,
    pattern: rule.pattern,
    ...(rule.reason !== undefined ? { reason: rule.reason } : {}),
  }));
}

function toFsEntry(entry: RpcFsEntry): FsEntry {
  const kind: FsKind =
    entry.kind === 'file' ||
    entry.kind === 'dir' ||
    entry.kind === 'directory' ||
    entry.kind === 'symlink'
      ? entry.kind === 'dir'
        ? 'directory'
        : entry.kind
      : 'file';
  return {
    path: entry.path,
    name: entry.name,
    kind,
    size: entry.size,
    modifiedAt: entry.modifiedAt ?? '',
    etag: entry.etag,
    mime: entry.mime,
    languageId: entry.languageId,
    isBinary: entry.isBinary,
    isSymlinkTo: entry.isSymlinkTo,
    gitStatus: entry.gitStatus,
    childCount: entry.childCount,
  };
}