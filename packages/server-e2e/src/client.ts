/**
 * `DaemonClient` — wire-level test client for the mirri-code server.
 *
 * Wraps the server's HTTP REST surface (`/api/v1/...`) plus the unified
 * JSON-RPC 2.0 WS endpoint (`/api/v1/rpc`) into a single, typed object
 * that scenarios can drive. Handles:
 *   - Envelope unwrap + typed REST helpers
 *   - JSON-RPC handshake (server_hello → client_hello → accepted)
 *   - `session.subscribe` / `session.unsubscribe`
 *   - `waitForEvent` / `waitForSessionBusy` convenience waits
 *
 * **What it is NOT**: a server bootstrap helper. Connect to a server process
 * that's already running at `baseUrl` (default `http://127.0.0.1:58627`).
 */
import type {
  ApprovalRequest,
  ApprovalResolveResult,
  ApprovalResponse,
  CloseTerminalResponse,
  CompactSessionRequest,
  CompactSessionResponse,
  CreateTerminalRequest,
  FileMeta,
  ForkSessionRequest,
  FsBrowseResponse,
  FsHomeResponse,
  ListTerminalsResponse,
  ListModelsResponse,
  ListPendingApprovalsResponse,
  ListPendingQuestionsResponse,
  ListProvidersResponse,
  Message,
  ProviderCatalogItem,
  PromptAbortResponse,
  PromptListResponse,
  PromptPermissionMode,
  PromptSubmission,
  PromptSteerResult,
  PromptSubmitResult,
  PromptThinking,
  QuestionRequest,
  QuestionResolveResult,
  QuestionResponse,
  SessionAbortResponse,
  SetDefaultModelResponse,
  Session,
  SessionChildCreate,
  SessionCreate,
  
  SessionUpdate,
  Terminal,
  UndoSessionRequest,
  UndoSessionResponse,
  Workspace,
  WorkspaceCreate,
  WorkspaceUpdate,
} from '@mirri-ai/protocol';

import { HttpClient } from './http.js';
import { DEFAULT_FRAME_TIMEOUT_MS, waitForSessionBusy } from './wait.js';
import { WsRpcClient } from './ws.js';

export interface DaemonClientOptions {
  /** Default `http://127.0.0.1:58627`. */
  baseUrl?: string;
  /** Default `/api/v1`. RPC WS endpoint is `${apiPrefix}/rpc`. */
  apiPrefix?: string;
  fetchImpl?: typeof fetch;
  logger?: (level: 'info' | 'warn' | 'error' | 'debug', msg: string, meta?: unknown) => void;
  /** Directory for JSONL trace events and generated HTML reports. */
  reportDir?: string;
}

export interface SubmitAndWaitOptions {
  /** Event name suffix — default `turn.ended` (full method `event.turn.ended`). */
  waitFor?: 'turn.ended' | 'turn.failed';
  /** Default 60s. */
  timeoutMs?: number;
}

type UploadFileData = Blob | ArrayBuffer | Uint8Array | string;

const DEFAULT_BASE_URL = 'http://127.0.0.1:58627';
const DEFAULT_API_PREFIX = '/api/v1';

/**
 * Per-request stateless session controls that the server REST surface
 * requires on every prompt submission. Scenarios that don't care about
 * these can leave them at the defaults; tests that exercise switching
 * model / thinking / permission / plan mode override only the field
 * they need.
 *
 * `model` matches what the existing server-e2e scenarios assume (the
 * default provider exposes `mirri-code/kimi-for-coding`).
 */
export const DEFAULT_PROMPT_CONTROLS = {
  model: 'mirri-code/kimi-for-coding',
  thinking: 'off' as PromptThinking,
  permission_mode: 'manual' as PromptPermissionMode,
  plan_mode: false,
} as const;

/**
 * Looser input shape for `submitPrompt` / `submitAndWait`. `content` is
 * required; the four stateless controls fall back to
 * `DEFAULT_PROMPT_CONTROLS` when omitted. `metadata` carries through
 * verbatim.
 */
export type PromptSubmitInput =
  Pick<PromptSubmission, 'content'>
  & Partial<Pick<PromptSubmission, 'metadata' | 'model' | 'thinking' | 'permission_mode' | 'plan_mode'>>;

function fillPromptDefaults(input: PromptSubmitInput): PromptSubmission {
  return { ...DEFAULT_PROMPT_CONTROLS, ...input };
}

export class DaemonClient {
  readonly baseUrl: string;
  readonly apiPrefix: string;
  readonly http: HttpClient;

  private readonly _logger: (
    level: 'info' | 'warn' | 'error' | 'debug',
    msg: string,
    meta?: unknown,
  ) => void;
  private readonly _reportDir: string | undefined;
  private _ws: WsRpcClient | null = null;

  constructor(opts: DaemonClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? DEFAULT_BASE_URL).replace(/\/+$/, '');
    this.apiPrefix = opts.apiPrefix ?? DEFAULT_API_PREFIX;
    this._logger = opts.logger ?? noopLogger;
    this._reportDir = opts.reportDir;
    this.http = new HttpClient({
      baseUrl: this.baseUrl,
      apiPrefix: this.apiPrefix,
      fetchImpl: opts.fetchImpl ?? fetch,
      reportDir: this._reportDir,
    });
  }

  // ── Probes + model catalog ─────────────────────────────────────────────
  listModels(): Promise<ListModelsResponse> {
    return this.http.listModels();
  }
  setDefaultModel(modelId: string): Promise<SetDefaultModelResponse> {
    return this.http.setDefaultModel(modelId);
  }
  listProviders(): Promise<ListProvidersResponse> {
    return this.http.listProviders();
  }
  getProvider(providerId: string): Promise<ProviderCatalogItem> {
    return this.http.getProvider(providerId);
  }

  // ── HTTP convenience surface ────────────────────────────────────────────
  createSession(body: SessionCreate): Promise<Session> {
    return this.http.createSession(body);
  }
  getSession(sid: string): Promise<Session> {
    return this.http.getSession(sid);
  }
  listSessions(
    query?: { page_size?: number; before_id?: string; after_id?: string; workspace_id?: string },
  ): Promise<{ items: Session[]; has_more: boolean }> {
    return this.http.listSessions(query);
  }
  updateSession(sid: string, body: SessionUpdate): Promise<Session> {
    return this.http.updateSession(sid, body);
  }
  forkSession(sid: string, body: ForkSessionRequest = {}): Promise<Session> {
    return this.http.forkSession(sid, body);
  }
  compactSession(
    sid: string,
    body: CompactSessionRequest = {},
  ): Promise<CompactSessionResponse> {
    return this.http.compactSession(sid, body);
  }
  undoSession(
    sid: string,
    body: UndoSessionRequest = { count: 1 },
  ): Promise<UndoSessionResponse> {
    return this.http.undoSession(sid, body);
  }
  archiveSession(sid: string): Promise<{ archived: true }> {
    return this.http.archiveSession(sid);
  }
  listChildren(
    sid: string,
    query?: { page_size?: number; before_id?: string; after_id?: string; busy?: boolean },
  ): Promise<{ items: Session[]; has_more: boolean }> {
    return this.http.listChildren(sid, query);
  }
  createChild(sid: string, body: SessionChildCreate = {}): Promise<Session> {
    return this.http.createChild(sid, body);
  }

  // ── Terminals ──────────────────────────────────────────────────────────
  listTerminals(sid: string): Promise<ListTerminalsResponse> {
    return this.http.listTerminals(sid);
  }
  createTerminal(
    sid: string,
    body: CreateTerminalRequest = {},
  ): Promise<Terminal> {
    return this.http.createTerminal(sid, body);
  }
  getTerminal(sid: string, terminalId: string): Promise<Terminal> {
    return this.http.getTerminal(sid, terminalId);
  }
  closeTerminal(
    sid: string,
    terminalId: string,
  ): Promise<CloseTerminalResponse> {
    return this.http.closeTerminal(sid, terminalId);
  }

  // ── Workspaces + folder picker ──────────────────────────────────────────
  listWorkspaces(): Promise<{ items: Workspace[] }> {
    return this.http.listWorkspaces();
  }
  createWorkspace(body: WorkspaceCreate): Promise<Workspace> {
    return this.http.createWorkspace(body);
  }
  updateWorkspace(workspaceId: string, body: WorkspaceUpdate): Promise<Workspace> {
    return this.http.updateWorkspace(workspaceId, body);
  }
  deleteWorkspace(workspaceId: string): Promise<{ deleted: true }> {
    return this.http.deleteWorkspace(workspaceId);
  }
  fsBrowse(path?: string): Promise<FsBrowseResponse> {
    return this.http.fsBrowse(path);
  }
  fsHome(): Promise<FsHomeResponse> {
    return this.http.fsHome();
  }
  uploadFile(input: {
    name: string;
    data: UploadFileData;
    mediaType?: string;
    expiresInSec?: number;
  }): Promise<FileMeta> {
    return this.http.uploadFile(input);
  }
  deleteFile(fileId: string): Promise<{ deleted: true }> {
    return this.http.deleteFile(fileId);
  }
  listMessages(
    sid: string,
    query?: { page_size?: number; before_id?: string; after_id?: string; role?: string },
  ): Promise<{ items: Message[]; has_more: boolean }> {
    return this.http.listMessages(sid, query);
  }
  submitPrompt(sid: string, input: PromptSubmitInput): Promise<PromptSubmitResult> {
    return this.http.submitPrompt(sid, fillPromptDefaults(input));
  }
  /**
   * Stateful-session submit — sends `body` to `POST /sessions/{sid}/prompts`
   * verbatim, with NO default controls injected. Pair with
   * `updateSession(sid, {agent_config: {...}})` (or `submitPrompt` with the
   * legacy default-filled path) to first establish session state, then
   * exercise the "content-only prompt inherits session state" contract.
   */
  submitPromptStateful(
    sid: string,
    body: PromptSubmission,
  ): Promise<PromptSubmitResult> {
    return this.http.submitPrompt(sid, body);
  }
  listPrompts(sid: string): Promise<PromptListResponse> {
    return this.http.listPrompts(sid);
  }
  steerPrompt(sid: string, pid: string): Promise<PromptSteerResult> {
    return this.http.steerPrompt(sid, pid);
  }
  steerPrompts(sid: string, promptIds: readonly string[]): Promise<PromptSteerResult> {
    return this.http.steerPrompts(sid, promptIds);
  }
  abortPrompt(sid: string, pid: string): Promise<PromptAbortResponse> {
    return this.http.abortPrompt(sid, pid);
  }
  abortSession(sid: string): Promise<SessionAbortResponse> {
    return this.http.abortSession(sid);
  }
  resolveApproval(
    sid: string,
    aid: string,
    body: ApprovalResponse,
  ): Promise<ApprovalResolveResult> {
    return this.http.resolveApproval(sid, aid, body);
  }
  listPendingApprovals(sid: string): Promise<ListPendingApprovalsResponse> {
    return this.http.listPendingApprovals(sid);
  }
  resolveQuestion(
    sid: string,
    qid: string,
    body: QuestionResponse,
  ): Promise<QuestionResolveResult> {
    return this.http.resolveQuestion(sid, qid, body);
  }
  listPendingQuestions(sid: string): Promise<ListPendingQuestionsResponse> {
    return this.http.listPendingQuestions(sid);
  }
  dismissQuestion(
    sid: string,
    qid: string,
  ): Promise<{ dismissed: true; dismissed_at: string }> {
    return this.http.dismissQuestion(sid, qid);
  }

  // ── WS lifecycle ────────────────────────────────────────────────────────
  /**
   * Open the unified JSON-RPC WS connection (`/api/v1/rpc`) and complete the
   * `server_hello` / `client_hello` handshake.
   */
  async connect(): Promise<void> {
    if (this._ws) return;
    const ws = new WsRpcClient(this.baseUrl);
    this._ws = ws;
    try {
      await ws.connect();
    } catch (error) {
      this._ws = null;
      throw error;
    }
    this._logger('debug', 'ws-rpc: handshake complete');
  }

  /** `session.subscribe` RPC. */
  subscribe(sid: string): Promise<unknown> {
    return this._requireWs().subscribe(sid);
  }

  /** `session.unsubscribe` RPC. */
  unsubscribe(sid: string): Promise<unknown> {
    return this._requireWs().unsubscribe(sid);
  }

  /** Cancel in-flight turns of a session over the WS RPC connection. */
  interrupt(sid: string): Promise<unknown> {
    return this._requireWs().call('message.abort', { sessionId: sid });
  }

  /** Close the socket. Idempotent. */
  close(): void {
    if (this._ws) {
      this._ws.close();
      this._ws = null;
    }
  }

  // ── WS observation ──────────────────────────────────────────────────────
  /** Observe every incoming JSON-RPC notification. Single slot. */
  onNotification(handler: (n: { method: string; params: Record<string, unknown> }) => void): void {
    this._requireWs().onNotification(handler);
  }

  /** Observe server→client reverse-RPC requests (approval.ask etc.). Single slot. */
  onRequest(
    handler: (req: { id: number | string; method: string; params: Record<string, unknown> }) => void,
  ): void {
    this._requireWs().onRequest(handler);
  }

  /** Send a raw JSON-RPC request over the WS connection. */
  call<T>(method: string, params: object): Promise<T> {
    return this._requireWs().call<T>(method, params);
  }

  /**
   * Wait for the next `event.<name>` notification of the session. Events
   * received before the wait starts are buffered, not dropped.
   */
  waitForEvent(
    sid: string,
    type: string,
    opts?: { timeoutMs?: number },
  ): Promise<Record<string, unknown>> {
    return this._requireWs().waitForEvent(sid, type, opts?.timeoutMs ?? DEFAULT_FRAME_TIMEOUT_MS);
  }

  /** Poll `/sessions/{sid}` until its aggregate work flag reaches `busy`. */
  waitForSessionBusy(
    sid: string,
    busy: boolean,
    opts?: { timeoutMs?: number; pollMs?: number },
  ): Promise<Session> {
    return waitForSessionBusy(this.http, sid, busy, opts);
  }

  // ── Reverse RPC (approval + question) ───────────────────────────────────
  /**
   * Install a handler invoked on every `event.approval.requested` event.
   * The handler's return value is POSTed to `/sessions/{sid}/approvals/{aid}`.
   */
  onApprovalRequested(
    handler: (req: ApprovalRequest) => Promise<ApprovalResponse> | ApprovalResponse,
  ): void {
    const ws = this._requireWs();
    ws.onNotification((n) => {
      if (n.method !== 'event.approval.requested') return;
      const payload = n.params as unknown as ApprovalRequest;
      const sessionId = (n.params as { sessionId?: string }).sessionId;
      const aid = (n.params as Record<string, unknown>)['approvalId'] as string | undefined;
      if (!sessionId || !aid) {
        this._logger('warn', 'approval.requested missing sessionId/approvalId', n.params);
        return;
      }
      Promise.resolve()
        .then(async () => {
          const response = await handler(payload);
          await this.http.resolveApproval(sessionId, aid, response);
        })
        .catch((error) => {
          this._logger('warn', 'approval resolve failed', { err: String(error), sessionId, aid });
        });
    });
  }

  /**
   * Install a handler invoked on every `event.question.requested` event.
   */
  onQuestionAsked(
    handler: (req: QuestionRequest) => Promise<QuestionResponse> | QuestionResponse,
  ): void {
    const ws = this._requireWs();
    ws.onNotification((n) => {
      if (n.method !== 'event.question.requested') return;
      const payload = n.params as unknown as QuestionRequest;
      const sessionId = (n.params as { sessionId?: string }).sessionId;
      const qid = (n.params as Record<string, unknown>)['questionId'] as string | undefined;
      if (!sessionId || !qid) {
        this._logger('warn', 'question.requested missing sessionId/questionId', n.params);
        return;
      }
      Promise.resolve()
        .then(async () => {
          const response = await handler(payload);
          await this.http.resolveQuestion(sessionId, qid, response);
        })
        .catch((error) => {
          this._logger('warn', 'question resolve failed', { err: String(error), sessionId, qid });
        });
    });
  }

  // ── High-level convenience ──────────────────────────────────────────────
  /**
   * Submit a prompt and wait for its terminal event (`event.turn.ended` by
   * default). Returns `prompt_id` and the matching event params.
   */
  async submitAndWait(
    sid: string,
    input: PromptSubmitInput,
    opts?: SubmitAndWaitOptions,
  ): Promise<{ prompt_id: string; user_message_id: string; finalEvent: Record<string, unknown> }> {
    const timeoutMs = opts?.timeoutMs ?? DEFAULT_FRAME_TIMEOUT_MS;

    // POST the prompt FIRST; the WS layer buffers every event from the
    // moment we connect, so events that arrive between this POST and the
    // wait below are matched from the queue, not dropped.
    const submit = await this.http.submitPrompt(sid, fillPromptDefaults(input));

    const waitFor = opts?.waitFor ?? 'turn.ended';
    const finalEvent = await this._requireWs().waitForEvent(sid, `event.${waitFor}`, timeoutMs);
    return { prompt_id: submit.prompt_id, user_message_id: submit.user_message_id, finalEvent };
  }

  /**
   * Stateful-session companion to `submitAndWait` — POSTs `body` verbatim
   * (NO default controls injected), then waits for the terminal event.
   */
  async submitAndWaitStateful(
    sid: string,
    body: PromptSubmission,
    opts?: SubmitAndWaitOptions,
  ): Promise<{ prompt_id: string; user_message_id: string; finalEvent: Record<string, unknown> }> {
    const timeoutMs = opts?.timeoutMs ?? DEFAULT_FRAME_TIMEOUT_MS;
    const submit = await this.http.submitPrompt(sid, body);
    const waitFor = opts?.waitFor ?? 'turn.ended';
    const finalEvent = await this._requireWs().waitForEvent(sid, `event.${waitFor}`, timeoutMs);
    return { prompt_id: submit.prompt_id, user_message_id: submit.user_message_id, finalEvent };
  }

  // ── internals ───────────────────────────────────────────────────────────
  private _requireWs(): WsRpcClient {
    if (!this._ws) {
      throw new Error('ws not connected — call `await client.connect()` first');
    }
    return this._ws;
  }
}

function noopLogger(): void {
  // intentionally blank
}
