/**
 * Rust-engine SDK client — an `SDKRpcClientBase` backed by the native
 * `@mirri-ai/mirri-node` NAPI binding (the Rust engine at
 * `rust/crates/mirri-node/`), instead of the TS agent-core cores.
 *
 * Every CoreAPI call is forwarded to the embedded Rust engine as one
 * JSON-RPC-style frame: `engine.callMethod(method, params)`. The engine is a
 * native module (a compiled `.node` cdylib) loaded defensively at module
 * scope: when the binding is unavailable (module not installed, or no
 * prebuilt for the platform), the client still constructs, and every RPC
 * call fails with a clear `not_implemented` error naming the missing module.
 *
 * The engine's push channels are wired on creation (see
 * {@link SDKRpcClientRust.attachEngineForwarders}): `engine.onEvent`
 * notifications are translated into v1 `Event`s and dispatched through the
 * base class's {@link SDKRpcClientBase.receiveEvent} (the same sink the v1
 * core and the v2 session wirings use, so TUI listeners registered via
 * `client.onEvent` / `Session.onEvent` receive them), and
 * `engine.onReverseRpc` requests (`approval.ask` / `question.ask`) are
 * bridged into the base class's `requestApproval` / `requestQuestion`
 * handlers, with every answer written back through
 * `engine.respondReverseRpc` / `engine.respondReverseRpcError` so the
 * engine's parked call always resolves.
 *
 * Engine instances are created lazily on the first RPC call and released by
 * {@link SDKRpcClientRust.close}, which disposes the engine (cancelling all
 * in-flight work) and drops the memoized handles.
 */

import { createRequire } from 'node:module';

import {
  ensureConfigFile,
  ErrorCodes,
  getRootLogger,
  log,
  MirriError,
  noopTelemetryClient,
  resolveConfigPath,
  resolveLoggingConfig,
  resolveMirriHome,
  type CoreAPI,
  type RPCCallOptions,
  type RPCMethods,
  type TelemetryClient,
} from '@mirri-ai/agent-core';
import { assertMirriHostIdentity } from '@mirri-ai/kosong';

import { MirriHarness } from '#/mirri-harness';
import { SDKRpcClientBase } from '#/rpc';
import { decodeRustEngineError } from '#/rust-engine/error-channel';
import {
  approvalResponseToWire,
  isRecord,
  parseRustEventNotification,
  sdkApprovalRequestFromWire,
  sdkQuestionRequestFromWire,
  translateRustEvent,
  translateSkillCatalogChanged,
  type RustEngineEventNotification,
  type RustEngineReverseRpcRequest,
} from '#/rust-engine/translate';
import type {
  AgentProfileListResult,
  AgentProfileSetEnabledResult,
  ListSessionsOptions,
  MirriHarnessOptions,
  MirriHostIdentity,
  ProfileEntry,
  RetentionDeleteResult,
  RetentionListEntry,
  RetentionListResult,
  RetentionListView,
  SessionSummary,
  SkillReloadSummary,
  SkippedAgentProfileFile,
} from '#/types';

/**
 * One engine instance as exported by the compiled `mirri-node` cdylib
 * (napi-derive renames `create_engine` → `createEngine`, `call_method` →
 * `callMethod`, `on_event` → `onEvent`, ...; see
 * `rust/crates/mirri-node/src/napi_exports.rs`).
 *
 * RPC calls (`callMethod`, `respondReverseRpc`, `respondReverseRpcError`,
 * `dispose`) are async: the native side returns a Promise and never blocks
 * the Node.js event loop. `onEvent` / `onReverseRpc` register callbacks
 * invoked on the Node.js main thread via a ThreadsafeFunction.
 */
export interface MirriEngineNative {
  /** Forward one RPC method call; resolves with the engine's JSON result. */
  callMethod(method: string, params: unknown): Promise<unknown>;
  /**
   * Subscribe to engine events. The callback uses Node.js error-first
   * convention: `(err, event)` — `err` is always `null`, `event` is a
   * `{ method, params }` object. Supports one subscription per engine.
   */
  onEvent(
    callback: (
      err: null,
      event: { method: string; params: unknown },
    ) => void,
  ): void;
  /**
   * Register the reverse-RPC handler. Same error-first convention:
   * `(err, request)` — `err` is always `null`, `request` is
   * `{ id, method, params }`.
   */
  onReverseRpc(
    callback: (
      err: null,
      request: { id: number; method: string; params: unknown },
    ) => void,
  ): void;
  /** Answer a pending reverse RPC request with a successful result. */
  respondReverseRpc(id: number, result: unknown): Promise<void>;
  /** Answer a pending reverse RPC request with an error message. */
  respondReverseRpcError(id: number, error: string): Promise<void>;
  /** Release the engine, cancelling all in-flight work. Idempotent. */
  dispose(): Promise<void>;
}

/** A tool call in a scripted mock LLM response. */
export interface MockLlmToolCallSpec {
  name: string;
  id?: string;
  args: Record<string, unknown>;
}

/** A single scripted mock LLM response. */
export interface MockLlmResponseSpec {
  text?: string;
  toolCalls: MockLlmToolCallSpec[];
}

/** The module-level surface a `require('@mirri-ai/mirri-node')` returns. */
export interface MirriNodeBinding {
  /**
   * Create an engine instance rooted at the current working directory.
   * `homeDir` optionally injects an explicit Mirri home — the directory
   * holding `config.toml`, `profiles/`, `plugins/`, MCP config, and the
   * session persistence root — instead of the native side's env-based
   * resolution (`MIRRICODE_HOME`, then `$HOME/.mirri-code`); omitted keeps
   * the ambient behavior. `skillDirs` optionally carries explicit skill
   * directories (v1/v2 `skillDirs` parity): when non-empty they replace
   * the engine's default user/project skill discovery and serve as the
   * user skill source; omitted/empty keeps the ambient discovery.
   */
  createEngine(homeDir?: string, skillDirs?: readonly string[]): MirriEngineNative;
  /**
   * Create an engine backed by a scripted mock LLM (only available when the
   * native binary is built with the `mock-llm` feature). A "coder" profile
   * is registered so Agent/AgentSwarm tools are available.
   */
  createEngineWithMockLlm?(responses: string[], homeDir?: string): MirriEngineNative;
  /**
   * Create an engine with a scripted mock LLM that supports tool calls.
   * Responses are replayed in FIFO order across all agents.
   */
  createEngineWithScriptedMockLlm?(
    responses: MockLlmResponseSpec[],
    homeDir?: string,
  ): MirriEngineNative;
  /**
   * The embedded engine's dispatchable method names from the mirri-proto
   * RPC registry, sorted (only available when the native binary is built
   * with the `mock-llm` feature). M1.1 contract reconciliation export.
   */
  embeddedMethodNames?(): string[];
  /**
   * The embedded engine's projected `event.*` notification method names from
   * mirri-core's shared event projection, sorted (only available when the
   * native binary is built with the `mock-llm` feature). M4-b3 contract
   * reconciliation export.
   */
  embeddedEventMethods?(): string[];
  /**
   * Create an engine backed by a remote `mirri-server` instance speaking the
   * unified JSON-RPC 2.0 WebSocket protocol (`/api/v1/rpc`). `url` is the
   * WebSocket endpoint (e.g. `ws://127.0.0.1:58627/api/v1/rpc`); `authToken`
   * is offered as the `mirri-code.bearer.<token>` subprotocol when the
   * server requires authentication. Returns null when the binding does not
   * export the factory (an older native binary), and rejects when the
   * handshake fails.
   */
  createRemoteEngine?(url: string, authToken?: string): MirriEngineNative | null;
}

/**
 * npm name of the compiled native binding package (phase-3 design doc §2.9:
 * main package `@mirri-ai/mirri-node` plus per-platform
 * `@mirri-ai/mirri-node-<platform>-<arch>` optional dependencies).
 */
const MIRRI_NODE_BINDING_MODULE = '@mirri-ai/mirri-node';

const nodeRequire = createRequire(import.meta.url);

/**
 * The native binding module, or null when it cannot be loaded (not installed,
 * or no prebuilt for this platform). Loaded once at module scope following
 * the CLI's `clipboard-native.ts` defensive-load pattern, so an unavailable
 * binding degrades to loud per-call errors instead of failing at import time.
 */
let mirriNodeBinding: MirriNodeBinding | null = loadMirriNodeBinding();

function loadMirriNodeBinding(): MirriNodeBinding | null {
  try {
    return nodeRequire(MIRRI_NODE_BINDING_MODULE) as MirriNodeBinding;
  } catch {
    return null;
  }
}

/**
 * Test seam: replace the module-scope binding with a fake, so unit tests can
 * drive the client's engine wiring without a compiled native module. Mirrors
 * agent-core's `__resetRootLoggerForTest` convention.
 */
export function __setMirriNodeBindingForTest(
  binding: MirriNodeBinding | null,
): void {
  mirriNodeBinding = binding;
}

/**
 * The engine's RPC surface, keyed by the TS `CoreAPI` method name and valued
 * by the dotted wire method name sent in `callMethod`. The record is
 * exhaustive: adding a method to `CoreAPI` fails typecheck until it is mapped
 * here.
 *
 * The NAPI binding (`mirri-node`) drives the embedded engine
 * (`mirri-embedded::EmbeddedServer`), whose dispatch table accepts both
 * dotted names (`session.create`, `message.send`, …) and legacy TS camelCase
 * names (`createSession`, `prompt`, …). We use dotted names for clarity;
 * the embedded server's dispatch table is the authority on what is
 * implemented (see `rust/crates/mirri-embedded/src/embedded_server.rs`).
 */
const CORE_API_WIRE_METHODS = {
  // CoreAPI (session-independent)
  getCoreInfo: 'core.info',
  getExperimentalFeatures: 'core.experimentalFeatures',
  getMirriConfig: 'config.get',
  getConfigDiagnostics: 'config.diagnostics',
  setMirriConfig: 'config.set',
  removeMirriProvider: 'config.removeProvider',
  removeMirriModel: 'config.removeModel',
  createSession: 'session.create',
  closeSession: 'session.close',
  archiveSession: 'session.archive',
  resumeSession: 'session.resume',
  reloadSession: 'session.reload',
  forkSession: 'session.fork',
  listSessions: 'session.list',
  exportSession: 'session.export',
  listWorkspaceSkills: 'skill.list',
  // Plugin management — served by the embedded engine's plugin host
  // (mirri-plugin-host) when one is configured; without one the read
  // methods resolve to empty structures and mutating methods error.
  listPlugins: 'plugin.list',
  installPlugin: 'plugin.install',
  setPluginEnabled: 'plugin.setEnabled',
  setPluginMcpServerEnabled: 'plugin.setMcpServerEnabled',
  removePlugin: 'plugin.remove',
  reloadPlugins: 'plugin.reload',
  getPluginInfo: 'plugin.info',
  // MCP management — served by the embedded engine's MCP client
  // (mirri-mcp) when one is configured; without one the read methods
  // resolve to empty structures, `mcp.reconnect` errors explicitly, and
  // the retired global routes (`reload`/`update`/`delete`/`toggle`) answer
  // an explicit `not_implemented` error.
  listGlobalMcpServers: 'mcp.global.list',
  listGlobalMcpTools: 'mcp.global.tools',
  reloadGlobalMcp: 'mcp.global.reload',
  createGlobalMcpServer: 'mcp.global.create',
  updateGlobalMcpServer: 'mcp.global.update',
  deleteGlobalMcpServer: 'mcp.global.delete',
  toggleGlobalMcpServer: 'mcp.global.toggle',
  getGlobalMcpToggleState: 'mcp.global.toggleState',
  enableGlobalMcpTool: 'mcp.global.enableTool',
  disableGlobalMcpTool: 'mcp.global.disableTool',
  testConnectMcpServer: 'mcp.global.testConnect',
  connectGlobalMcpServer: 'mcp.global.connect',
  // Layered MCP config surface (`mcp.config.*`) — user/project scoped reads
  // and writes over the three config layers (A1/A2/A5), plus the P2 pool
  // surface: the fresh-connect test and the no-workspace global view.
  listMcpConfig: 'mcp.config.list',
  setMcpConfig: 'mcp.config.set',
  removeMcpConfig: 'mcp.config.remove',
  setMcpToolFilter: 'mcp.config.setToolFilter',
  testMcpConfig: 'mcp.config.test',
  getMcpStatus: 'mcp.status',
  // SessionAPI
  renameSession: 'session.rename',
  updateSessionMetadata: 'session.updateMeta',
  getSessionMetadata: 'session.getMeta',
  listSkills: 'skill.list',
  listPluginCommands: 'plugin.commands',
  listMcpServers: 'mcp.list',
  getMcpStartupMetrics: 'mcp.startupMetrics',
  reconnectMcpServer: 'mcp.reconnect',
  enableMcpServer: 'mcp.enable',
  disableMcpServer: 'mcp.disable',
  enableMcpTool: 'mcp.enableTool',
  disableMcpTool: 'mcp.disableTool',
  getMcpToggleState: 'mcp.toggleState',
  generateAgentsMd: 'session.generateAgentsMd',
  getSessionWarnings: 'session.warnings',
  waitForBackgroundTasksOnPrint: 'session.waitForBackgroundTasksOnPrint',
  handlePrintMainTurnCompleted: 'session.handlePrintMainTurnCompleted',
  addAdditionalDir: 'session.addAdditionalDir',
  // AgentAPI
  prompt: 'message.send',
  runShellCommand: 'shell.run',
  cancelShellCommand: 'shell.cancel',
  steer: 'message.steer',
  cancel: 'message.abort',
  undoHistory: 'session.undo',
  setThinking: 'agent.setThinking',
  setPermission: 'agent.setPermission',
  setModel: 'agent.setModel',
  getModel: 'agent.getModel',
  enterPlan: 'plan.enter',
  cancelPlan: 'plan.cancel',
  clearPlan: 'plan.clear',
  enterSwarm: 'swarm.enter',
  exitSwarm: 'swarm.exit',
  getSwarmMode: 'swarm.get',
  beginCompaction: 'session.beginCompaction',
  cancelCompaction: 'session.cancelCompaction',
  registerTool: 'agent.registerTool',
  unregisterTool: 'agent.unregisterTool',
  setActiveTools: 'agent.setActiveTools',
  stopBackground: 'task.cancel',
  detachBackground: 'task.detach',
  clearContext: 'session.clearContext',
  activateSkill: 'agent.activateSkill',
  activatePluginCommand: 'plugin.activateCommand',
  startBtw: 'btw.start',
  createGoal: 'goal.create',
  getGoal: 'goal.get',
  pauseGoal: 'goal.pause',
  resumeGoal: 'goal.resume',
  cancelGoal: 'goal.cancel',
  getBackgroundOutput: 'task.output',
  getContext: 'agent.getContext',
  getConfig: 'agent.getConfig',
  getPermission: 'agent.getPermission',
  getPlan: 'plan.status',
  getUsage: 'agent.getUsage',
  getTools: 'tool.list',
  getBackground: 'task.list',
  getCronTasks: 'cron.list',
} as const satisfies Record<keyof CoreAPI, string>;

/**
 * Build the `RPCMethods<CoreAPI>` facade over one engine instance: every
 * method forwards its payload to `engine.callMethod(wireName, payload)` and
 * resolves with the engine's result. The RPC `options` argument is accepted
 * for signature parity with the base class and ignored — the binding has no
 * abort channel yet.
 *
 * Every session-addressed call also reports its session id through
 * `onSessionAddressed`: the engine's reverse-RPC requests carry no session id
 * on the wire, so this out-of-band knowledge is what lets the client route
 * `approval.ask` / `question.ask` to the right session's handlers. Session
 * ids are captured from the payload (`sessionId`, used by every
 * session-scoped method) and — for `session.create` / `session.resume`,
 * whose payloads carry no session id — from the returned session view's
 * `id`.
 */
function toRpcMethods(
  engine: MirriEngineNative,
  onSessionAddressed: (sessionId: string) => void,
): RPCMethods<CoreAPI> {
  const methods = {} as Record<keyof CoreAPI, unknown>;
  for (const methodName of Object.keys(CORE_API_WIRE_METHODS) as (keyof CoreAPI)[]) {
    const wireMethodName = CORE_API_WIRE_METHODS[methodName];
    methods[methodName] = async (payload: unknown, _options?: RPCCallOptions): Promise<unknown> => {
      if (isRecord(payload)) {
        const sessionId = payload['sessionId'];
        if (typeof sessionId === 'string' && sessionId.length > 0) {
          onSessionAddressed(sessionId);
        }
      }
      const result = await engine.callMethod(wireMethodName, payload).catch(
        (error: unknown) => {
          // The native layer carries the engine's JSON-RPC code and domain
          // code as a structured suffix on the error message (M-3); decode it
          // so every SDK call rejects with a `RustEngineRpcError` carrying
          // numeric `code` + `domainCode` — the same fields the JSON-RPC
          // transport exposes. Non-engine failures pass through unchanged.
          throw decodeRustEngineError(error);
        },
      );
      if (
        (methodName === 'createSession' || methodName === 'resumeSession') &&
        isRecord(result)
      ) {
        const id = result['id'];
        if (typeof id === 'string' && id.length > 0) {
          onSessionAddressed(id);
        }
      }
      // `btw.start` rides the serve wire, whose pinned shape (the kap-server
      // REST `startBtwSessionResponseSchema`, consumed identically by the
      // web WS client) wraps the new agent's id in `{ agent_id }`. The TS
      // `CoreAPI` contract promises a bare string, so unwrap here — without
      // this the id leaks to callers as an object and every later
      // `message.send` carries a map in `agentId`, which the engine rejects
      // as `invalid type: map, expected a string`.
      if (methodName === 'startBtw' && isRecord(result)) {
        const agentId = result['agent_id'] ?? result['agentId'];
        if (typeof agentId === 'string') {
          return agentId;
        }
      }
      return result;
    };
  }
  return methods as RPCMethods<CoreAPI>;
}

export class SDKRpcClientRust extends SDKRpcClientBase {
  readonly homeDir: string;
  readonly configPath: string;
  readonly identity: MirriHostIdentity | undefined;
  readonly telemetry: TelemetryClient;
  /**
   * The explicit skill directories from the harness options (v1/v2
   * `skillDirs` parity), threaded into the native engine at creation.
   * Empty keeps the engine's ambient user/project skill discovery.
   */
  private readonly skillDirs: string[];

  /**
   * Memoized engine-creation promise: the binding factory runs once, on the
   * first RPC call, and every `getRpc()` shares the same engine instance
   * until {@link close} releases it (a later call then creates a fresh one).
   */
  private enginePromise: Promise<MirriEngineNative> | undefined;
  /** Memoized `RPCMethods<CoreAPI>` facade over {@link enginePromise}'s engine. */
  private rpcPromise: Promise<RPCMethods<CoreAPI>> | undefined;
  /**
   * The session id of the engine's most recent activity — the last
   * session-addressed RPC payload / `session.create` / `session.resume`
   * result (see `toRpcMethods`), or the last engine event's session id.
   *
   * The embedded engine registers ONE server-wide reverse-RPC handler and its
   * `approval.ask` / `question.ask` requests carry no session id, so this
   * id — the session the engine is provably working on right now — is what
   * routes a reverse request to the right session's handlers. Cleared when
   * that session closes or the client closes.
   */
  private latestSessionId: string | undefined;

  constructor(options: MirriHarnessOptions) {
    super();
    this.identity =
      options.identity === undefined ? undefined : assertMirriHostIdentity(options.identity);
    this.homeDir = resolveMirriHome(options.homeDir);
    // v1/v2 `skillDirs` parity: explicit skill directories replace the
    // engine's default user/project skill discovery (see the binding's
    // `createEngine`). Copied once so later mutation of the caller's array
    // cannot retroactively change the engine's discovery set.
    this.skillDirs = options.skillDirs === undefined ? [] : [...options.skillDirs];
    this.configPath = resolveConfigPath({
      homeDir: this.homeDir,
      configPath: options.configPath,
    });
    this.telemetry = options.telemetry ?? noopTelemetryClient;

    void getRootLogger().configure(resolveLoggingConfig({ homeDir: this.homeDir }));
  }

  async ensureConfigFile(): Promise<void> {
    await ensureConfigFile(this.configPath);
  }

  /**
   * `session.list` rides the serve wire, whose rows are the engine's
   * `SessionView` (camelCase, ISO timestamps, `cwd`), while the v1
   * `SessionSummary` contract promises `workDir` + epoch-millisecond
   * timestamps. Map at this client boundary — the same adaptation the v2
   * client performs (`v2SummaryToSessionSummary`). Without it the TUI's
   * session picker reads `workDir: undefined` and crashes the render loop
   * (`homeAlias` → `startsWith` of undefined).
   *
   * `sessionDir` has no `SessionView` counterpart; the list path's consumers
   * only read `id` / `workDir` / timestamps, and `session.resume` supplies the
   * real per-session directory for the flows that need it.
   */
  override async listSessions(
    input: ListSessionsOptions = {},
  ): Promise<readonly SessionSummary[]> {
    const rows = (await super.listSessions(input)) as unknown;
    // W5: the engine's `session.list` answers the page envelope
    // (`{items, has_more}`) — unwrap the items (an older engine answering
    // the bare array still maps through the fallback).
    const items = Array.isArray(rows)
      ? rows
      : ((rows as Record<string, unknown> | null | undefined)?.['items'] as unknown);
    if (!Array.isArray(items)) return [];
    return items.map((row) => {
      const view = (row ?? {}) as Record<string, unknown>;
      const toMs = (value: unknown): number => {
        const parsed =
          typeof value === 'string' ? Date.parse(value) : typeof value === 'number' ? value : Number.NaN;
        return Number.isFinite(parsed) ? parsed : 0;
      };
      const title = typeof view['title'] === 'string' && view['title'].length > 0 ? view['title'] : undefined;
      const str = (value: unknown): string => (typeof value === 'string' ? value : '');
      return {
        id: str(view['id']),
        title,
        lastPrompt: typeof view['lastPrompt'] === 'string' && view['lastPrompt'].length > 0
          ? view['lastPrompt']
          : undefined,
        workDir: str(view['cwd']),
        sessionDir: '',
        createdAt: toMs(view['createdAt']),
        updatedAt: toMs(view['updatedAt']),
        archived: view['status'] === 'archived' ? true : undefined,
        // The retention compression marker (US2.1 第 4 条): the wire row's
        // `compressed` flag passes straight through — absent on older
        // engines (the consumers' "not compressed" default).
        compressed: view['compressed'] === true ? true : undefined,
        metadata: undefined,
        additionalDirs: undefined,
      } satisfies SessionSummary;
    });
  }

  async close(): Promise<void> {
    try {
      await getRootLogger().flush();
    } catch {
      // never let logger flush block process exit
    }
    this.rpcPromise = undefined;
    const enginePromise = this.enginePromise;
    this.enginePromise = undefined;
    this.latestSessionId = undefined;
    if (enginePromise === undefined) return;
    const engine = await enginePromise.catch(() => undefined);
    await engine?.dispose();
  }

  // -----------------------------------------------------------------------
  // Agent profile data plane (R5-1) — the embedded engine's
  // `agent.listProfiles` / `agent.enableProfile` / `agent.disableProfile`
  // RPC methods. These are NOT part of the v1 `CoreAPI` contract, so they
  // stay out of `CORE_API_WIRE_METHODS` (an exhaustive map over
  // `keyof CoreAPI`); they ride the generic NAPI `callMethod` channel
  // directly, with the same structured-error decoding the CoreAPI facade
  // applies. Rows keep the engine's snake_case wire spelling verbatim —
  // the protocol REST `ProfileEntry` shape every consumer (web REST, the
  // terminal `/agents` view) reads.
  // -----------------------------------------------------------------------

  override async listAgentProfiles(): Promise<AgentProfileListResult> {
    const result = (await this.callEngineMethod('agent.listProfiles', {})) as unknown;
    // The `{items, skipped}` envelope (R3-6). A malformed answer degrades
    // to empty rows (the `listSessions` override's convention) instead of
    // throwing on a cast.
    const record = isRecord(result) ? result : {};
    return {
      items: Array.isArray(record['items']) ? (record['items'] as ProfileEntry[]) : [],
      skipped: Array.isArray(record['skipped'])
        ? (record['skipped'] as SkippedAgentProfileFile[])
        : [],
    };
  }

  override async setAgentProfileEnabled(
    name: string,
    enabled: boolean,
  ): Promise<AgentProfileSetEnabledResult> {
    const wireMethod = enabled ? 'agent.enableProfile' : 'agent.disableProfile';
    const result = (await this.callEngineMethod(wireMethod, { name })) as unknown;
    // `{ok: true}` bare envelope on the wire; failures reject instead.
    const record = isRecord(result) ? result : {};
    return { ok: record['ok'] === true };
  }

  // The session-retention data plane (rust-first `retention.*` rows — the
  // terminal's archived-session management): forwarded through the generic
  // engine RPC channel like the profile rows above. The wire rows keep the
  // protocol's snake_case `RetentionListEntry` shape verbatim.

  override async listRetentionSessions(
    view: RetentionListView,
  ): Promise<RetentionListResult> {
    const result = (await this.callEngineMethod('retention.list', { include: view })) as unknown;
    const record = isRecord(result) ? result : {};
    return {
      items: Array.isArray(record['items'])
        ? (record['items'] as RetentionListEntry[])
        : [],
    };
  }

  override async deleteRetentionSession(sessionId: string): Promise<RetentionDeleteResult> {
    const result = (await this.callEngineMethod('retention.delete', {
      session_id: sessionId,
    })) as unknown;
    const record = isRecord(result) ? result : {};
    return {
      ok: record['ok'] === true,
      reclaimed_bytes: typeof record['reclaimed_bytes'] === 'number'
        ? record['reclaimed_bytes']
        : 0,
    };
  }

  /**
   * Restore one archived session (`session.restore` — the resume path plus
   * the archive-flag clear; the rust handler decompresses a compressed wire
   * log before resuming). Serves the same wire face the web management
   * page's restore button drives.
   */
  override async restoreRetentionSession(sessionId: string): Promise<void> {
    await this.callEngineMethod('session.restore', { session_id: sessionId });
  }

  /**
   * The manual skill-catalog reload face (ADR-010 Task 6), the
   * `listAgentProfiles` pattern: NOT part of the v1 `CoreAPI` contract,
   * so it rides the generic NAPI `callMethod` channel directly. The
   * engine's snake_case wire spelling (`workspaces[].workspace_id`) is
   * kept verbatim.
   */
  override async reloadSkills(): Promise<SkillReloadSummary> {
    const result = (await this.callEngineMethod('skill.reload', {})) as unknown;
    const record = isRecord(result) ? result : {};
    const rows = Array.isArray(record['workspaces']) ? (record['workspaces'] as unknown[]) : [];
    return {
      workspaces: rows.map((row) => {
        const entry = isRecord(row) ? row : {};
        return {
          workspace_id: typeof entry['workspace_id'] === 'string' ? entry['workspace_id'] : '',
          added: typeof entry['added'] === 'number' ? entry['added'] : 0,
          removed: typeof entry['removed'] === 'number' ? entry['removed'] : 0,
          updated: typeof entry['updated'] === 'number' ? entry['updated'] : 0,
          watching: entry['watching'] === true,
        };
      }),
    };
  }

  /**
   * One generic engine RPC outside the v1 `CoreAPI` contract: forwarded
   * through the memoized engine (the same instance `getRpc()` drives, so
   * the forwarders and the close path apply), with rejections decoded off
   * the NAPI error channel exactly like the CoreAPI facade (`toRpcMethods`)
   * — engine failures surface as `RustEngineRpcError` with the numeric
   * JSON-RPC `code` + `domainCode`.
   */
  private async callEngineMethod(method: string, params: unknown): Promise<unknown> {
    const engine = await (this.enginePromise ??= this.createEngine());
    return engine.callMethod(method, params).catch((error: unknown) => {
      throw decodeRustEngineError(error);
    });
  }

  protected getRpc(): Promise<RPCMethods<CoreAPI>> {
    return (this.rpcPromise ??= this.buildRpc());
  }

  private async buildRpc(): Promise<RPCMethods<CoreAPI>> {
    const engine = await (this.enginePromise ??= this.createEngine());
    return toRpcMethods(engine, (sessionId) => {
      this.latestSessionId = sessionId;
    });
  }

  private async createEngine(): Promise<MirriEngineNative> {
    if (mirriNodeBinding === null) {
      throw new MirriError(
        ErrorCodes.NOT_IMPLEMENTED,
        `The Rust engine binding (npm package "${MIRRI_NODE_BINDING_MODULE}") is not available. ` +
          'Install it or build the mirri-node crate (rust/crates/mirri-node) so the native ' +
          'module can be loaded; the TS engines remain available through the other clients.',
      );
    }
    // Thread the harness home into the native engine (I-1): without it the
    // engine resolves config/profiles/plugins/MCP and persists sessions
    // against the ambient environment (`~/.mirri-code`, `<cwd>/.mirri`),
    // which makes session listings machine-dependent. The explicit skill
    // dirs ride along (v1/v2 `skillDirs` parity): the engine replaces its
    // default user/project skill discovery with them when non-empty.
    const engine = mirriNodeBinding.createEngine(this.homeDir, this.skillDirs);
    this.attachEngineForwarders(engine);
    return engine;
  }

  /**
   * Subscribe to the engine's push channels. Called once per engine instance
   * (inside {@link createEngine}, which is memoized), so every engine gets
   * exactly one event subscription and one reverse-RPC handler.
   *
   * - `onEvent` → {@link handleEngineEvent}: engine wire events are
   *   translated into v1 `Event`s and pushed through the base class's
   *   `receiveEvent` — the same sink the v1 core and the v2 session wirings
   *   use, so everything registered through `client.onEvent` /
   *   `Session.onEvent` (the TUI's streaming, tool, and status renderers)
   *   sees the engine's stream.
   * - `onReverseRpc` → {@link handleEngineReverseRpc}: engine-initiated
   *   `approval.ask` / `question.ask` requests are bridged into the base
   *   class's `requestApproval` / `requestQuestion` handler registries and
   *   answered on the same engine instance.
   */
  private attachEngineForwarders(engine: MirriEngineNative): void {
    engine.onEvent((_err, notification) => {
      try {
        this.handleEngineEvent(notification);
      } catch (error) {
        // A translation or listener failure must never take down the event
        // pump: drop the offending notification and keep streaming.
        log.error('rust engine event handling failed', { error });
      }
    });
    engine.onReverseRpc((_err, request) => {
      void this.handleEngineReverseRpc(engine, request);
    });
  }

  /**
   * Dispatch one engine event notification: keep the latest-session tracking
   * current, then translate and forward the event to the SDK listeners.
   * Notification parsing and translation are defensive (see
   * `#/rust-engine/translate`), so malformed notifications are dropped.
   *
   * The app-level `skill.catalogChanged` notification (ADR-010 Task 7) is
   * the one non-`event.*` notification this client consumes: it carries no
   * session envelope (the payload's `workspaceId` scopes it), so it skips
   * the session-tracking update and rides its own camelCase translation.
   */
  private handleEngineEvent(notification: RustEngineEventNotification): void {
    if (notification.method === 'skill.catalogChanged') {
      const event = translateSkillCatalogChanged(notification.params);
      if (event !== undefined) this.receiveEvent(event);
      return;
    }
    const parsed = parseRustEventNotification(notification);
    if (parsed === undefined) return;
    // `event.session.deleted` ends the session, so that spelling retires the
    // tracking.
    if (parsed.type === 'session.deleted') {
      if (this.latestSessionId === parsed.sessionId) this.latestSessionId = undefined;
    } else {
      this.latestSessionId = parsed.sessionId;
    }
    const event = translateRustEvent(notification);
    if (event !== undefined) this.receiveEvent(event);
  }

  /**
   * Answer one engine reverse-RPC request through the base class's handler
   * registries.
   *
   * Every request is ALWAYS answered — success or error: the engine parks the
   * underlying call on a one-shot channel (see `EngineBridge`), so an
   * unanswered request would stall the tool (or the whole turn) forever.
   *
   * - `approval.ask` → `requestApproval`; the response is written back in
   *   the wire shape the permission gate reads (`decision` + `feedback`).
   * - `question.ask` → `requestQuestion`; the handler's result (or `null`
   *   for a dismissed / unhandled question) is written back verbatim — the
   *   engine serializes it as the tool result.
   * - Unknown methods → an error response naming the method, so a future
   *   engine reverse method degrades to a clear engine-side failure instead
   *   of a hang.
   */
  private async handleEngineReverseRpc(
    engine: MirriEngineNative,
    request: RustEngineReverseRpcRequest,
  ): Promise<void> {
    try {
      const sessionId = this.latestSessionId;
      if (sessionId === undefined) {
        await engine.respondReverseRpcError(
          request.id,
          'engine reverse RPC arrived before any session was addressed by this client',
        );
        return;
      }
      const agentId = this.interactiveAgentId;
      switch (request.method) {
        case 'approval.ask': {
          const approval = sdkApprovalRequestFromWire(request.params);
          if (approval === undefined) {
            await engine.respondReverseRpcError(request.id, 'invalid approval.ask params');
            return;
          }
          const response = await this.requestApproval({
            ...approval,
            sessionId,
            agentId,
          });
          await engine.respondReverseRpc(request.id, approvalResponseToWire(response));
          return;
        }
        case 'question.ask': {
          const question = sdkQuestionRequestFromWire(request.params);
          if (question === undefined) {
            await engine.respondReverseRpcError(request.id, 'invalid question.ask params');
            return;
          }
          const result = await this.requestQuestion({
            ...question,
            sessionId,
            agentId,
          });
          await engine.respondReverseRpc(request.id, result ?? null);
          return;
        }
        default:
          await engine.respondReverseRpcError(
            request.id,
            `unsupported engine reverse RPC method: ${request.method}`,
          );
      }
    } catch (error) {
      // A handler failure must still resolve the engine's parked call.
      await engine
        .respondReverseRpcError(request.id, errorMessage(error))
        .catch(() => undefined);
    }
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function createMirriHarnessRust(options: MirriHarnessOptions): MirriHarness {
  const rpc = new SDKRpcClientRust(options);
  return new MirriHarness(rpc, {
    identity: rpc.identity,
    uiMode: options.uiMode,
    homeDir: rpc.homeDir,
    configPath: rpc.configPath,
    telemetry: rpc.telemetry,
    ensureConfigFile: () => rpc.ensureConfigFile(),
    onClose: () => rpc.close(),
    // The Rust engine has no ingestion-limit equivalent of v1's core-owned
    // `imageLimits`; leave it undefined so ingestion falls back to
    // env/built-in defaults like daemon-client hosts (same as the v2 client).
    sessionStartedProperties: options.sessionStartedProperties,
  });
}