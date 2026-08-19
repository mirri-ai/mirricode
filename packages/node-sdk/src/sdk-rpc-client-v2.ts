/**
 * v2 wiring MVP — an `SDKRpcClientBase` backed by the agent-core-v2 engine
 * (DI × Scope) instead of the v1 `MirriCore` RPC pair. The engine is
 * bootstrapped in-process and reached through the klient facade over the
 * memory transport, so every call crosses the same contract validation and
 * JSON round-trip as the networked transports.
 *
 * Migration model: the base class still carries the v1 method surface. Any
 * method not yet overridden here falls through to `getRpc()`, which fails
 * loudly with `not_implemented` — migrated methods are the ones overridden
 * below. Once every method is migrated, the v1 `getRpc()` dependency (and
 * the v1 core) goes away entirely.
 *
 * Migrated so far (MVP):
 * - `createSession` / `resumeSession` → the workspace handler chain
 *   (`IWorkspaceLifecycleService.handlerFor` →
 *   `ISessionLifecycleService.create` / `resumeSessionById`, both reached
 *   through {@link engineAccessor} because the klient facade takes neither
 *   an explicit session id nor the resume options), with the v1
 *   `SessionSummary` / `SessionMeta` shapes restored by the pure mapping
 *   layer in `src/v2/session-mapper.ts`. `createSession`'s `model` /
 *   `thinking` / `permission` options materialize the main agent with the
 *   requested binding (v1 applies them eagerly at creation); `resumeSession`
 *   serves the full v1 per-agents snapshot — live slices from the restored
 *   agent scope + the klient facade, `replay` / `toolStore` folded from each
 *   agent's `wire.jsonl` via `src/v2/resume-replay.ts` —
 * - `closeSession` → `klient.session(id).close()`; `renameSession` → the
 *   klient session facade's `setTitle` (live session first, resume-rename-
 *   close for a closed one); `listSessions` → `klient.global.sessions.list`
 *   filtered by the workDir's workspace-id set, with the v1 summary shape
 *   restored.
 * - `prompt` / `steer` / `cancel` / `setModel` / `setPlanMode` /
 *   `setPermission` / `getPlan` / `clearPlan` / `getContext` / `getUsage` /
 *   `runShellCommand` / `cancelShellCommand` → the
 *   `klient.session(id).agent(id)` facade; `getStatus` → the same six-slice
 *   aggregate the base class builds, re-read from the profile / permission /
 *   swarm services plus the facade (context/plan/usage).
 * - `setThinking` → the agent scope's `IAgentProfileService.setThinking`
 *   with v1's blank-effort `session_thinking_empty` rejection; `compact` →
 *   the agent scope's `IAgentFullCompactionService.begin` (manual source,
 *   optional instruction); `cancelCompaction` / `undoHistory` → the agent
 *   scope's `IAgentRPCService` (the v2 RPC surface's own cancel/undo).
 * - `requestApproval` / `requestQuestion` / `onEvent` / `receiveEvent` → the
 *   base class registries, driven by the per-live-session wiring
 *   (`src/v2/session-wiring.ts`): the wiring subscribes every live agent's
 *   `IEventBus` (v1 `DomainEvent` → v1 `Event` through
 *   `src/v2/event-mapper.ts`) and bridges the session interaction kernel's
 *   pending approvals / questions / user-tool calls into the base class's
 *   own handler methods. The one v1-visible fact on the process-global
 *   `IEventService` (`session.meta.updated`) is forwarded from a constructor
 *   subscription.
 * - `activateSkill` → the agent scope's `IAgentSkillService.activate`
 *   (deliberately NOT the RPC service's fire-and-forget variant, which would
 *   swallow v1's synchronous rejections) plus v1's main-only metadata update;
 *   `activatePluginCommand` → `IAgentRPCService` through the agent scope;
 *   `generateAgentsMd` → `ISessionInitService` through the session scope;
 *   `listSkills` → the session scope's `ISessionSkillCatalog`;
 *   `startBtw` → the session scope's `ISessionBtwService`;
 *   `setSwarmMode` / `swarm` → the agent scope's `IAgentSwarmService`.
 * - `forkSession` → the handler chain's `ISessionLifecycleService.fork`
 *   (through `handlerForSession`, since the klient facade fork takes no
 *   explicit target id); `addAdditionalDir` → the session's workspace
 *   handler (`IWorkspaceDirs.addDir`); `exportSession` → the app scope's
 *   `ISessionExportService` (engine port of v1's export), with the v2
 *   manifest's heritage `kimiCodeVersion` field re-spelled to v1's
 *   `mirriCodeVersion`.
 * - `createGoal` / `getGoal` / `pauseGoal` / `resumeGoal` / `cancelGoal` →
 *   `IAgentGoalService` through the agent scope; `getCronTasks` →
 *   `ISessionCronService` through the session scope with the v1 snapshot
 *   shape restored; `listBackgroundTasks` / `getBackgroundTaskOutput` → the
 *   `klient.session(id).agent(id)` facade; `stopBackgroundTask` /
 *   `detachBackgroundTask` → `IAgentTaskService` through the agent scope
 *   (the facade's no-reason stop substitutes a user-cancellation reason v1
 *   never records).
 * - `listPlugins` / `installPlugin` / `setPluginEnabled` /
 *   `setPluginMcpServerEnabled` / `removePlugin` / `reloadPlugins` /
 *   `getPluginInfo` / `listPluginCommands` (+ the session-id-less
 *   `listPluginCommandsGlobal`) → `klient.global.plugins.*`. The wire types
 *   are field-identical between the engines, so no mapping layer is needed.
 * - `listMcpServers` / `getMcpStartupMetrics` / `reconnectMcpServer` →
 *   the seeded `ISessionMcpHandle.connectionManager` through the session
 *   scope (one shared manager per workspace handler); the v2 `McpServerEntry`
 *   is field-identical with v1's `McpServerInfo`.
 * - `getConfig` / `setConfig` / `removeProvider` / `getConfigDiagnostics` /
 *   `getExperimentalFeatures` → `klient.global.config.*` and
 *   `klient.global.flags.list()`, with the v1 wire shapes restored by the
 *   pure mapping layer in `src/v2/config-mapper.ts`: `getConfig` reads the
 *   resolved view (and honors v1's `reload` request), `setConfig` replays
 *   v1's whole-document merge patch one domain at a time, and
 *   `removeProvider` replays v1's full provider cascade — planned from
 *   `inspect().userValue` so the write reflects the user's document rather
 *   than the env-overlaid runtime view — as one atomic section replace.
 * - `reloadSession` → `klient.global.config.reload()` +
 *   `klient.global.plugins.reload()` followed by a close-then-resume through
 *   `engineAccessor` (the facade's resume takes no explicit session id),
 *   guarded by an `IAgentActivityView` busy-turn check;
 *   `getSessionWarnings` → the profile's cached `agentsMdWarning` with the
 *   engine's `prepareSystemPromptContext` as the recompute fallback (no v2
 *   service owns the aggregate);
 *   `waitForBackgroundTasksOnPrint` / `handlePrintMainTurnCompleted` → the
 *   agent scope's `IPrintModeService`, the engine's own port of v1's print
 *   policy (exit / drain / steer, ceiling and turn cap included), which also
 *   owns the steer state v1 kept on the Session object.
 * - `updateSessionMetadata` → the klient session facade's `update`, reading
 *   the current `custom` document first so the patch merges into it (the
 *   facade merges at the top level only, which would otherwise replace the
 *   whole document); `clearContext` → the agent scope's
 *   `IAgentPromptService.clear()` (no klient agent facade exposes it).
 *
 * Not migrated yet (fall through to the loud `getRpc()` failure):
 * `createSessionWithKaos` / `resumeSessionWithKaos` (base-class kaos-
 * ignoring degradation deliberately kept — the v2 engine has no kaos
 * injection point), `toolCall` (keeps the base class's "not supported"
 * answer, which the interaction bridge relies on).
 */
import { readdir } from 'node:fs/promises';
import { join, resolve, win32 } from 'node:path';

import {
  ensureConfigFile,
  ErrorCodes,
  MirriError,
  noopTelemetryClient,
  type AgentContextData,
} from '@mirri-ai/agent-core';
import { encodeWorkDirKey } from '@mirri-ai/agent-core-v2/_base/utils/workdir-slug';
import {
  applyPromptMetadataUpdate,
  type AgentCommandInfo,
  bootstrap,
  closeSessionById,
  DEFAULT_AGENT_PROFILE_NAME,
  ensureMainAgent,
  ensureMirriHome,
  followWorkspaceHandlers,
  getLiveSessionById,
  handlerForSession,
  IAgentActivityView,
  IAgentCommandService,
  IAgentFullCompactionService,
  IAgentGoalService,
  IAgentLifecycleService,
  IAgentPermissionModeService,
  IAgentPermissionRulesService,
  IAgentProfileService,
  IAgentPromptService,
  IAgentRPCService,
  IAgentSkillService,
  IAgentSwarmService,
  IAgentTaskService,
  IBootstrapService,
  IConfigService,
  IEventService,
  IHostEnvironment,
  IHostFileSystem,
  IModelService,
  IPrintModeService,
  IProviderService,
  ISessionBtwService,
  ISessionContext,
  ISessionCronService,
  ISessionExportService,
  ISessionIndex,
  ISessionInitService,
  ISessionLifecycleService,
  ISessionMcpHandle,
  ISessionMetadata,
  ISessionSkillCatalog,
  ISessionWorkspaceContext,
  IWorkspaceAliases,
  IWorkspaceDirs,
  IWorkspaceLifecycleService,
  logSeed,
  MAIN_AGENT_ID,
  prepareSystemPromptContext,
  ProfileError,
  ProfileErrors,
  promptMetadataTextFromSkill,
  resumeSessionById,
  resolveConfigPath,
  resolveLoggingConfig,
  resolveMirriHome,
  sessionDirOf,
  summarizeSkill,
  workspacePersistenceScope,
  type IAgentScopeHandle,
  type IDisposable,
  type ISessionScopeHandle,
  type Scope,
  type ServicesAccessor,
} from '@mirri-ai/agent-core-v2';
import type { AgentHandle, Klient } from '@mirri-ai/klient';
import { createKlient } from '@mirri-ai/klient/memory';
import {
  assertMirriHostIdentity,
  createMirriDefaultHeaders,
  type MirriHostIdentity,
} from '@mirri-ai/mirri-code-oauth';

import { MirriAuthFacade } from '#/auth';
import { MirriHarness } from '#/mirri-harness';
import {
  SDKRpcClientBase,
  type ActivatePluginCommandRpcInput,
  type ActivateSkillRpcInput,
  type ListCommandsRpcInput,
  type ReconnectMcpServerRpcInput,
  type ReloadSessionRpcInput,
  type RunCommandRpcInput,
  type SessionIdRpcInput,
  type SessionPromptRpcInput,
  type SetSessionModelRpcInput,
  type SetSessionModelRpcResult,
  type SetSessionPermissionRpcInput,
  type SetSessionPlanModeRpcInput,
  type SetSessionSwarmModeRpcInput,
  type SetSessionThinkingRpcInput,
  type UpdateSessionMetadataRpcInput,
} from '#/rpc';
import type {
  AddAdditionalDirInput,
  AddAdditionalDirResult,
  BackgroundTaskInfo,
  CompactOptions,
  ConfigDiagnostics,
  CreateGoalInput,
  CreateSessionOptions,
  ExperimentalFeatureState,
  ExportSessionInput,
  ExportSessionResult,
  ForkSessionInput,
  GetConfigOptions,
  GetCronTasksResult,
  GoalSnapshot,
  GoalToolResult,
  ListSessionsOptions,
  McpServerInfo,
  McpStartupMetrics,
  MirriConfig,
  MirriConfigPatch,
  MirriHarnessOptions,
  OAuthRefreshOutcome,
  PluginCommandDef,
  PluginInfo,
  PluginSummary,
  ReloadSummary,
  RenameSessionInput,
  ResumeSessionInput,
  ResumedAgentState,
  ResumedSessionSummary,
  SessionPlan,
  SessionStatus,
  SessionSummary,
  SessionUsage,
  SkillSummary,
  TelemetryClient,
} from '#/types';
import {
  diagnosticsToConfigDiagnostics,
  planProviderRemoval,
  resolvedConfigToMirriConfig,
} from '#/v2/config-mapper';
import { translateGlobalEvent } from '#/v2/event-mapper';
import { foldAgentWireReplay } from '#/v2/resume-replay';
import {
  v2MetaToSessionMeta,
  v2SummaryToSessionSummary,
} from '#/v2/session-mapper';
import { SessionEventWiring } from '#/v2/session-wiring';

export interface SDKRpcClientV2Options {
  readonly homeDir?: string;
  readonly configPath?: string;
  readonly identity?: MirriHostIdentity;
  /**
   * Explicit skill directories for this process (v1's SDK `skillDirs` /
   * the CLI's `--skills-dir`): when non-empty, default user / project skill
   * discovery is skipped and these directories serve as the user skill
   * source. Passed into the engine through `BootstrapInput.args.skillDirs`.
   */
  readonly skillDirs?: readonly string[];
  readonly telemetry?: TelemetryClient;
  readonly onOAuthRefresh?: (outcome: OAuthRefreshOutcome) => void;
  /**
   * Host UI mode (`'print'` for `mirri -p`, `'cli'` for the TUI, ...).
   * Accepted for option parity with the v1 client; the v2 engine applies
   * print-mode config defaults itself, so nothing is forwarded here.
   */
  readonly uiMode?: string;
}

export class SDKRpcClientV2 extends SDKRpcClientBase {
  readonly homeDir: string;
  readonly configPath: string;
  readonly identity: MirriHostIdentity | undefined;
  readonly telemetry: TelemetryClient;
  readonly auth: MirriAuthFacade;
  readonly klient: Klient;

  private readonly app: Scope;
  /**
   * The model/provider registries (`IModelService` / `IProviderService`)
   * share the config service's ready trap: their reads are synchronous over
   * state that only exists after hydration. Every agent-side model
   * operation (profile bind, `setModel`, capability reads) flows through
   * them, so agent-interaction overrides await this before touching a
   * profile.
   */
  private readonly modelReady: Promise<void>;
  /**
   * The engine's config hydration. The klient config facade is a thin
   * forwarder onto `IConfigService`, whose reads are synchronous over state
   * that only exists after the initial load, so every config entry point
   * awaits this before its first facade call.
   */
  private readonly configReady: Promise<void>;
  /**
   * Per-live-session event/interaction wirings (`src/v2/session-wiring.ts`):
   * created when a session materializes through this client (create /
   * resume), dropped on close (ours or the engine's). Each wiring feeds the
   * base class's event listeners from the session's per-agent event buses
   * and bridges its pending approvals / questions / user-tool calls to the
   * registered handlers.
   */
  private readonly sessionWirings = new Map<string, SessionEventWiring>();
  /** App-scope subscriptions (global event forwarding, close tracking), disposed in {@link close}. */
  private readonly appSubscriptions: IDisposable[] = [];

  constructor(options: SDKRpcClientV2Options = {}) {
    super();
    this.identity =
      options.identity === undefined ? undefined : assertMirriHostIdentity(options.identity);
    this.homeDir = resolveMirriHome(options.homeDir);
    this.configPath = resolveConfigPath({
      homeDir: this.homeDir,
      configPath: options.configPath,
    });
    ensureMirriHome(this.homeDir);
    this.telemetry = options.telemetry ?? noopTelemetryClient;
    this.auth = new MirriAuthFacade({
      homeDir: this.homeDir,
      configPath: this.configPath,
      identity: this.identity,
      onRefresh: options.onOAuthRefresh,
    });

    // The v2 engine requires a host identity — every process names its host,
    // and a fabricated identity would silently misreport the host upstream.
    // The v1 `MirriHostIdentity` carries no `platform` field (the v1 headers
    // hard-code one), so the SDK states its own platform slot here; a host
    // that wants X-Msh-Platform parity with its v1 identity can extend the
    // options later.
    const identity = assertMirriHostIdentity(this.identity);
    const { app } = bootstrap(
      {
        homeDir: this.homeDir,
        configPath: this.configPath,
        clientIdentity: {
          productName: identity.userAgentProduct,
          version: identity.version,
          platform: 'mirri_code_sdk',
          userAgentSuffix: identity.userAgentSuffix,
        },
        args: {
          // Host identity headers for the engine's outbound requests (model,
          // WebSearch, registry refresh). Without them the managed vendors
          // go out with the SDK's default User-Agent and no X-Msh-* at all.
          requestHeaders: createMirriDefaultHeaders({ homeDir: this.homeDir, ...identity }),
          // `--skills-dir` (v1 parity): explicit skill dirs replace default
          // user / project discovery for every session this client hosts.
          skillDirs: options.skillDirs,
        },
      },
      [...logSeed(resolveLoggingConfig({ homeDir: this.homeDir, env: process.env }))],
    );
    this.app = app;
    this.klient = createKlient({ scope: app });
    this.configReady = app.accessor.get(IConfigService).ready;
    this.modelReady = Promise.all([
      app.accessor.get(IConfigService).ready,
      app.accessor.get(IModelService).ready,
      app.accessor.get(IProviderService).ready,
    ]).then(() => undefined);
    this.appSubscriptions.push(
      // v1's stream carries `session.meta.updated` (the prompt metadata
      // path) — the one v1-visible fact the v2 engine publishes on the
      // process-global IEventService rather than a per-agent bus. Every
      // other global-bus type is a daemon/WS-edge event the in-process v1
      // client never saw, so the translation filters down to that single
      // type.
      app.accessor.get(IEventService).subscribe((event) => {
        const translated = translateGlobalEvent(event);
        if (translated !== undefined) this.receiveEvent(translated);
      }),
      // A session closed without going through this client (an engine-
      // initiated close) drops its wiring with the scope. Close events fire
      // per workspace handler, so follow every handler — present and future
      // — through the App-scope registry.
      followWorkspaceHandlers(app.accessor, (service) =>
        service.onDidCloseSession((closed) => {
          this.unwireSession(closed.sessionId);
        }),
      ),
    );
  }

  async ensureConfigFile(): Promise<void> {
    await ensureConfigFile(this.configPath);
  }

  async close(): Promise<void> {
    for (const wiring of this.sessionWirings.values()) {
      wiring.dispose();
    }
    this.sessionWirings.clear();
    for (const subscription of this.appSubscriptions) {
      subscription.dispose();
    }
    this.appSubscriptions.length = 0;
    await this.klient.close();
    this.app.dispose();
  }

  /**
   * Escape hatch to the in-process engine's app-scope service accessor, for
   * SDK methods whose capability exists in agent-core-v2 but is not (yet)
   * exposed through the klient facade. This is a deliberate migration
   * pressure valve, not a new public API direction:
   * - it only exists because this client owns the bootstrapped `Scope` —
   *   there is nothing equivalent on a remote (ipc) transport, so anything
   *   built on it is in-process-only by construction;
   * - it resolves App-scope services only. Session/agent services need
   *   their own scope handles (via the lifecycle services), not this
   *   accessor;
   * - every use should name the klient facade method it stands in for, and
   *   move onto the facade once one exists. Remove when the migration ends.
   */
  get engineAccessor(): ServicesAccessor {
    return this.app.accessor;
  }

  // -----------------------------------------------------------------------
  // Config & experimental flags
  //
  // v1 serves the whole `config.toml` document as ONE zod-validated object;
  // v2 registers one config section per owning domain. These go through the
  // klient facade (`klient.global.config.*` / `klient.global.flags.*`) so
  // every call crosses the same contract validation and JSON round-trip as
  // the networked transports; the pure mapping in
  // `src/v2/config-mapper.ts` projects the per-domain view back onto the v1
  // wire shapes.
  //
  // The facade is a thin forwarder and does not await the engine's config
  // hydration, so each entry point awaits `configReady` first — the service's
  // reads are synchronous over state that only exists after the initial load.
  // -----------------------------------------------------------------------

  /**
   * `getAll()` is the effective view (file values + env overlays + section
   * defaults), which matches what v1's runtime config resolves to; `reload`
   * mirrors v1's re-read-from-disk option.
   */
  override async getConfig(input?: GetConfigOptions): Promise<MirriConfig> {
    await this.configReady;
    if (input?.reload === true) {
      await this.klient.global.config.reload();
    }
    return resolvedConfigToMirriConfig(await this.klient.global.config.getAll());
  }

  override async getConfigDiagnostics(): Promise<ConfigDiagnostics> {
    await this.configReady;
    return diagnosticsToConfigDiagnostics(await this.klient.global.config.diagnostics());
  }

  override async getExperimentalFeatures(): Promise<readonly ExperimentalFeatureState[]> {
    await this.configReady;
    return this.klient.global.flags.list();
  }

  /**
   * A v1 patch is one deep-merge over the whole document; v2 deep-merges per
   * domain with the same semantics, so the patch fans out one `config.set`
   * per top-level field. Undefined fields are absent from the v1 patch
   * schema (parse strips them), so they are skipped defensively.
   */
  override async setConfig(patch: MirriConfigPatch): Promise<MirriConfig> {
    await this.configReady;
    for (const [domain, value] of Object.entries(patch)) {
      if (value === undefined) continue;
      await this.klient.global.config.set({ domain, patch: value });
    }
    return this.getConfig();
  }

  /**
   * v1's removal cascades: the provider entry, every model pointing at it,
   * and the default pointers when they dangle. The engine's own provider
   * delete only clears the default-provider pointer, so the SDK replays the
   * full v1 cascade and persists it as ONE atomic multi-section replace —
   * the same single-write shape as v1, so a process exit can never leave the
   * file halfway cascaded.
   *
   * The cascade is planned from `inspect().userValue`, NOT the effective
   * value: the effective view folds in env overlays and section defaults,
   * which belong to the running process rather than the user's document.
   * The engine's `stripEnv` also scrubs env-derived provider entries on the
   * write path, so the two guards agree — planning from the user layer keeps
   * the intent explicit at this boundary instead of relying on that.
   */
  override async removeProvider(providerId: string): Promise<MirriConfig> {
    await this.configReady;
    const config = this.klient.global.config;
    const [providers, models, defaultModel, defaultProvider] = await Promise.all([
      config.inspect<Record<string, unknown>>('providers'),
      config.inspect<Record<string, Record<string, unknown>>>('models'),
      config.inspect<string>('defaultModel'),
      config.inspect<string>('defaultProvider'),
    ]);
    const plan = planProviderRemoval({
      providers: providers.userValue,
      models: models.userValue,
      defaultModel: defaultModel.userValue,
      defaultProvider: defaultProvider.userValue,
      providerId,
    });
    const sections: Record<string, unknown> = {
      providers: plan.providers,
      models: plan.models,
    };
    // `undefined` is the facade's clear marker (encoded as `null` on the wire).
    if (plan.clearDefaultModel) sections['defaultModel'] = undefined;
    if (plan.clearDefaultProvider) sections['defaultProvider'] = undefined;
    await config.replaceSections({ sections });
    return this.getConfig();
  }

  /**
   * The v2 engine's config service persists several sections in one atomic
   * document write, so hosts may stage a multi-section change (e.g. the
   * provider-refresh remove-then-restore) and commit it as a single write
   * that no process exit can leave halfway applied.
   */
  override supportsAtomicSectionReplace(): boolean {
    return true;
  }

  override async replaceConfigSections(sections: Record<string, unknown>): Promise<void> {
    await this.configReady;
    await this.klient.global.config.replaceSections({ sections });
  }

  protected getRpc(): Promise<never> {
    throw new MirriError(
      ErrorCodes.NOT_IMPLEMENTED,
      'This SDK method is not wired to agent-core-v2 yet.',
    );
  }

  // -----------------------------------------------------------------------
  // Session lifecycle
  //
  // The v2 engine splits what v1's SessionStore + in-memory session map did
  // across the app-scope `ISessionIndex` (persisted read model),
  // `IWorkspaceLifecycleService` (live workspace handlers and, under them,
  // the live session scopes), and the session-scope metadata/workspace
  // services. The klient facade covers listing and the metadata mutations
  // of a LIVE session; everything that needs an explicit session id or a
  // resume goes through the `engineAccessor` escape hatch (named per method
  // below).
  //
  // `createSessionWithKaos` / `resumeSessionWithKaos` are deliberately NOT
  // overridden: agent-core-v2 has no kaos injection point, so the base
  // class's degradation — ignore the kaos arguments and run a plain create /
  // resume — is the honest behavior, the same one every daemon-transport
  // client settles for.
  // -----------------------------------------------------------------------

  private liveSession(sessionId: string): ISessionScopeHandle | undefined {
    return getLiveSessionById(this.engineAccessor, sessionId);
  }

  /** v1's `requireSession` / store lookup failure shape. */
  private static sessionNotFound = (sessionId: string): MirriError =>
    new MirriError(ErrorCodes.SESSION_NOT_FOUND, `Session "${sessionId}" was not found`, {
      details: { sessionId },
    });

  /** The live session handle, or the error v1 raises for a non-active session. */
  private requireLiveSession(sessionId: string): ISessionScopeHandle {
    const handle = this.liveSession(sessionId);
    if (handle === undefined) throw SDKRpcClientV2.sessionNotFound(sessionId);
    return handle;
  }

  /**
   * Attach the event/interaction wiring to a freshly materialized session
   * (idempotent). Unwiring needs no call site of its own: every close path
   * goes through the engine's lifecycle close, whose `onDidCloseSession`
   * subscription (constructor) drops the wiring.
   */
  private wireSession(handle: ISessionScopeHandle): void {
    if (this.sessionWirings.has(handle.id)) return;
    this.sessionWirings.set(handle.id, new SessionEventWiring(handle, this));
  }

  private unwireSession(sessionId: string): void {
    const wiring = this.sessionWirings.get(sessionId);
    if (wiring === undefined) return;
    this.sessionWirings.delete(sessionId);
    wiring.dispose();
  }

  /**
   * The v1 summary of a live session, read from its own scope services (the
   * metadata document, the context's cwd/sessionDir, the workspace context's
   * additional dirs) rather than the index — no disk round-trip, and the
   * additional dirs only exist on the live session in both engines.
   */
  private async liveSessionSummary(handle: ISessionScopeHandle): Promise<SessionSummary> {
    const meta = await handle.accessor.get(ISessionMetadata).read();
    const ctx = handle.accessor.get(ISessionContext);
    const workspace = handle.accessor.get(ISessionWorkspaceContext);
    return {
      id: meta.id,
      title: meta.title,
      lastPrompt: meta.lastPrompt,
      workDir: ctx.cwd,
      sessionDir: ctx.sessionDir,
      createdAt: meta.createdAt,
      updatedAt: meta.updatedAt,
      archived: meta.archived,
      metadata: meta.custom as SessionSummary['metadata'],
      additionalDirs: workspace.additionalDirs,
    };
  }

  /**
   * The `ResumedSessionSummary` of a just-materialized session, including
   * the per-agent snapshot v1 serves: the live slices are read from the
   * restored agent scope (profile / permission / swarm services and the
   * klient agent facade for context / plan / usage), while `replay` and
   * `toolStore` are folded from the agent's `wire.jsonl` by
   * {@link foldAgentWireReplay} (v2 has no replay builder of its own).
   * `warning` stays undefined — v2's resume has no migration-warning
   * channel.
   */
  private async resumedSessionSummary(handle: ISessionScopeHandle): Promise<ResumedSessionSummary> {
    const meta = await handle.accessor.get(ISessionMetadata).read();
    const agents: Record<string, ResumedAgentState> = {};
    // v1 resumes the main agent eagerly; materializing it here cold-restores
    // its wire into the scope (create-or-get) and applies the default
    // binding.
    const main = await this.materializeMainAgent(handle);
    agents[MAIN_AGENT_ID] = await this.resumedAgentState(handle, main, 'main');
    // v1's resume serves every persisted subagent's snapshot too; each is
    // best-effort (a subagent whose restore fails is left out, like v1).
    const agentsDir = join(handle.accessor.get(ISessionContext).sessionDir, 'agents');
    let subagentIds: readonly string[] = [];
    try {
      subagentIds = (await readdir(agentsDir, { withFileTypes: true }))
        .filter((entry) => entry.isDirectory() && entry.name !== MAIN_AGENT_ID)
        .map((entry) => entry.name);
    } catch {
      // No agents directory at all → the main agent is the whole roster.
    }
    for (const agentId of subagentIds) {
      try {
        // `create` is create-or-get and cold-restores the persisted wire.
        const agent = await handle.accessor.get(IAgentLifecycleService).create({ agentId });
        agents[agentId] = await this.resumedAgentState(handle, agent, 'sub');
      } catch {
        // Best-effort, same as v1.
      }
    }
    return {
      ...(await this.liveSessionSummary(handle)),
      sessionMetadata: v2MetaToSessionMeta(meta),
      agents,
      warning: undefined,
    };
  }

  /**
   * One agent's v1 `ResumedAgentState`. The scope reads mirror v1's
   * `resumeSessionResult` field-by-field; the casts only bridge the two
   * packages' type declarations (the wire shapes are the documented-identical
   * ports, same as the `getContext` / `getUsage` overrides). One deliberate
   * gap: `config.provider` is always undefined — v1 resolves the full
   * runtime `ProviderConfig` into the snapshot, agent-core-v2 has no
   * equivalent read, and the TUI only falls back to `provider?.model` when
   * `modelAlias` is unset.
   */
  private async resumedAgentState(
    session: ISessionScopeHandle,
    agent: IAgentScopeHandle,
    type: 'main' | 'sub',
  ): Promise<ResumedAgentState> {
    const facade = this.klient.session(session.id).agent(agent.id);
    const ctx = session.accessor.get(ISessionContext);
    const [context, plan, usage, background, folded] = await Promise.all([
      facade.getContext(),
      facade.getPlan(),
      facade.getUsage(),
      facade.getTasks({ activeOnly: false }),
      foldAgentWireReplay(join(ctx.sessionDir, 'agents', agent.id, 'wire.jsonl')),
    ]);
    const profile = agent.accessor.get(IAgentProfileService).data();
    return {
      type,
      config: {
        cwd: ctx.cwd,
        provider: undefined,
        modelAlias: profile.modelAlias,
        modelCapabilities: profile.modelCapabilities as ResumedAgentState['config']['modelCapabilities'],
        profileName: profile.profileName,
        thinkingEffort: profile.thinkingLevel,
        systemPrompt: profile.systemPrompt,
      },
      context: context as AgentContextData,
      replay: folded.replay,
      permission: {
        mode: agent.accessor.get(IAgentPermissionModeService).mode,
        rules: [...agent.accessor.get(IAgentPermissionRulesService).rules],
      } as ResumedAgentState['permission'],
      plan: plan as ResumedAgentState['plan'],
      swarmMode: agent.accessor.get(IAgentSwarmService).isActive,
      usage: usage as ResumedAgentState['usage'],
      tools: (await agent.accessor.get(IAgentRPCService).getTools({})) as ResumedAgentState['tools'],
      toolStore: folded.toolStore,
      background: background as ResumedAgentState['background'],
    };
  }

  /**
   * v1 rejects an empty workDir and bucket-filters by the normalized path;
   * the v2 index filters by workspace-id set instead. A session whose
   * workDir is unrecoverable (corrupt metadata, deleted workspace) cannot be
   * resumed on either engine, so it is dropped here too.
   */
  private async workspaceIdsFor(workDir: string): Promise<readonly string[]> {
    const workspaces = await this.klient.global.workspaces.list();
    const match = workspaces.find((workspace) => normalizeWorkDir(workspace.root) === workDir);
    if (match === undefined) return [encodeWorkDirKey(workDir)];
    return this.engineAccessor.get(IWorkspaceAliases).resolveAliasIds(match.id);
  }

  override async listSessions(input: ListSessionsOptions = {}): Promise<readonly SessionSummary[]> {
    // v1 rejects a blank workDir; the v2 index filters by workspace-id set.
    const workspaceIds =
      input.workDir === undefined
        ? undefined
        : await this.workspaceIdsFor(normalizeRequiredWorkDir('listSessions', input.workDir));
    const page = await this.klient.global.sessions.list({
      workspaceIds,
      sessionId: input.sessionId,
    });
    const bootstrapService = this.engineAccessor.get(IBootstrapService);
    const workspacesById = new Map(
      (await this.klient.global.workspaces.list()).map((workspace) => [workspace.id, workspace]),
    );
    const summaries: SessionSummary[] = [];
    for (const item of page.items) {
      const workDir = item.cwd ?? workspacesById.get(item.workspaceId)?.root;
      if (workDir === undefined) continue;
      summaries.push(
        v2SummaryToSessionSummary(item, {
          workDir,
          sessionDir: sessionDirOf(
            bootstrapService.homeDir,
            workspacePersistenceScope(bootstrapService.scope('sessions'), item.workspaceId),
            item.id,
          ),
        }),
      );
    }
    return summaries;
  }

  /**
   * v1 semantics: register the workDir as a workspace and create the session
   * (the handler's `ISessionLifecycleService.create` does both; the klient
   * facade wrapper is bypassed because it takes neither an explicit session
   * id nor caller-provided metadata). The `model` / `thinking` /
   * `permission` options are the main-agent configuration v1 applies eagerly
   * at creation: supplying any of them materializes the main agent here (v2
   * otherwise keeps it lazy) and binds the default profile with the
   * requested model/thinking.
   */
  override async createSession(input: CreateSessionOptions): Promise<SessionSummary> {
    const workDir = normalizeRequiredWorkDir('createSession', input.workDir);
    if (input.id !== undefined) {
      const existing =
        this.liveSession(input.id) ?? (await this.engineAccessor.get(ISessionIndex).get(input.id));
      if (existing !== undefined) {
        throw new MirriError(
          ErrorCodes.SESSION_ALREADY_EXISTS,
          `Session "${input.id}" already exists`,
        );
      }
    }
    const handler = await this.engineAccessor
      .get(IWorkspaceLifecycleService)
      .handlerFor({ root: workDir });
    const handle = await handler.accessor.get(ISessionLifecycleService).create({
      sessionId: input.id,
      workDir,
      additionalDirs: input.additionalDirs,
    });
    // Wired before the optional main-agent materialization so a profile-bind
    // warning (oversized AGENTS.md) reaches the listeners like v1's create.
    this.wireSession(handle);
    if (
      input.model !== undefined ||
      input.thinking !== undefined ||
      input.permission !== undefined
    ) {
      const agent = await this.materializeMainAgent(handle, {
        model: input.model,
        thinking: input.thinking,
      });
      if (input.permission !== undefined) {
        agent.accessor.get(IAgentPermissionModeService).setMode(input.permission);
      }
    }
    if (input.metadata !== undefined) {
      await this.klient.session(handle.id).update({ custom: { ...input.metadata } });
    }
    // v1 returns the caller's metadata verbatim on create (not the merged
    // custom map a later listing would report), so override it here too.
    return { ...(await this.liveSessionSummary(handle)), metadata: input.metadata };
  }

  /**
   * v1 renames through the live session when there is one and at the store
   * level otherwise. The v2 metadata service is session-scoped (and the
   * klient session facade 404s on a non-live session), so a closed session
   * is resumed, renamed, and closed again to land in the same state. The v2
   * `setTitle` does no validation, so v1's trim + empty-title rejection
   * lives here.
   */
  override async renameSession(input: RenameSessionInput): Promise<void> {
    const title = input.title.trim();
    if (title.length === 0) {
      throw new MirriError(ErrorCodes.SESSION_TITLE_EMPTY, 'Session title cannot be empty');
    }
    if (this.liveSession(input.id) !== undefined) {
      await this.klient.session(input.id).setTitle(title);
      return;
    }
    const handle = await resumeSessionById(this.engineAccessor, input.id);
    if (handle === undefined) throw SDKRpcClientV2.sessionNotFound(input.id);
    try {
      await this.klient.session(input.id).setTitle(title);
    } finally {
      await closeSessionById(this.engineAccessor, input.id);
    }
  }

  override async closeSession(input: SessionIdRpcInput): Promise<void> {
    this.unwireSession(input.sessionId);
    await this.klient.session(input.sessionId).close();
  }

  /**
   * Materializes the session through `engineAccessor`
   * (`resumeSessionById` through the handler chain; the facade only offers
   * `restore`, which would also clear the archived flag — v1's resume does
   * not).
   */
  override async resumeSession(input: ResumeSessionInput): Promise<ResumedSessionSummary> {
    const handle = await resumeSessionById(this.engineAccessor, input.id, {
      additionalDirs: input.additionalDirs,
    });
    if (handle === undefined) throw SDKRpcClientV2.sessionNotFound(input.id);
    this.wireSession(handle);
    return this.resumedSessionSummary(handle);
  }

  /**
   * v1's reload: re-read config and plugins from disk, then rebuild the   * session so the fresh state reaches a live run. The config / plugin
   * re-reads go through the klient facade (`config.reload` /
   * `plugins.reload`); the close-then-resume pair goes through
   * `engineAccessor`, since the facade's resume takes no explicit session id.
   *
   * A running turn blocks the reload — rebuilding the session under an
   * in-flight turn would strand it — so every live agent is checked through
   * `IAgentActivityView` first. A session that is not live is validated
   * against the persisted index so an unknown id still fails as
   * `session_not_found` rather than silently resuming nothing.
   *
   * `forcePluginSessionStartReminder` has no v2 channel: the engine owns
   * plugin session-start injection, so the flag is accepted and ignored.
   */
  override async reloadSession(input: ReloadSessionRpcInput): Promise<ResumedSessionSummary> {
    const { sessionId } = input;
    const live = this.liveSession(sessionId);
    if (live !== undefined) {
      for (const agent of live.accessor.get(IAgentLifecycleService).list()) {
        if (agent.accessor.get(IAgentActivityView).state().turn !== undefined) {
          throw new MirriError(
            ErrorCodes.TURN_AGENT_BUSY,
            `Session "${sessionId}" cannot be reloaded while a turn is running`,
            { details: { sessionId } },
          );
        }
      }
    } else if ((await this.engineAccessor.get(ISessionIndex).get(sessionId)) === undefined) {
      throw SDKRpcClientV2.sessionNotFound(sessionId);
    }
    await this.klient.global.config.reload();
    await this.klient.global.plugins.reload();
    if (live !== undefined) {
      await closeSessionById(this.engineAccessor, sessionId);
    }
    const handle = await resumeSessionById(this.engineAccessor, sessionId);
    if (handle === undefined) throw SDKRpcClientV2.sessionNotFound(sessionId);
    this.wireSession(handle);
    return this.resumedSessionSummary(handle);
  }

  /**
   * v1 merges the patch into the session's metadata document and writes it
   * back. The v2 metadata facade's `update` shallow-merges at the TOP level,
   * so passing `{ custom }` would REPLACE the whole custom document rather
   * than merge into it — the current value is read first and merged here so
   * unlisted keys survive, matching v1. (v1's `metadata` maps onto v2's
   * `custom`; see `src/v2/session-mapper.ts`. The engine keeps its per-agent
   * records outside `custom`, so v1's `agents` re-pin has no counterpart.)
   *
   * v1 routes the write through the live session and 404s on a closed one;
   * `requireLiveSession` mirrors that.
   */
  override async updateSessionMetadata(input: UpdateSessionMetadataRpcInput): Promise<void> {
    this.requireLiveSession(input.sessionId);
    const session = this.klient.session(input.sessionId);
    const current = await session.get();
    await session.update({ custom: { ...current.custom, ...input.metadata } });
  }

  /**
   * v1 forwards to the agent's own context clear. The v2 port is the agent
   * scope's `IAgentPromptService.clear()`; no klient agent facade exposes it
   * (the facade carries `clearPlan`, a different surface).
   */
  override async clearContext(input: SessionIdRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    agent.accessor.get(IAgentPromptService).clear();
  }

  // -----------------------------------------------------------------------
  // Fork / workspace dirs / export
  //
  // `forkSession` and `addAdditionalDir` both need the workspace handler the
  // v1 core reached through the SessionStore: the klient facade's fork takes
  // no explicit target id, and the add-dir surface lives on the handler
  // (`IWorkspaceDirs`, workspace scope). `exportSession` is the app scope's
  // `ISessionExportService` (the v2 port of v1's export), reached through
  // `engineAccessor`.
  // -----------------------------------------------------------------------

  /**
   * Through the handler chain's `ISessionLifecycleService.fork` — the klient
   * facade fork takes no explicit target id. Known gaps vs v1: the engine's
   * fork is unconditional — it never rejects an in-flight source turn (v1's
   * `SESSION_FORK_ACTIVE_TURN`) — and it always forks the main agent as the
   * source (v1 also forks the agent `interactiveAgentId` addresses; SDK hosts
   * only ever fork the main agent). The default title also differs by design
   * (v1: "New Session", v2: "Fork: <source>") — pass an explicit title for
   * identical results.
   */
  override async forkSession(input: ForkSessionInput): Promise<SessionSummary> {
    const forkHandler = await handlerForSession(this.engineAccessor, input.id);
    if (forkHandler === undefined) throw SDKRpcClientV2.sessionNotFound(input.id);
    const handle = await forkHandler.accessor.get(ISessionLifecycleService).fork({
      sourceSessionId: input.id,
      newSessionId: input.forkId,
      title: input.title,
      metadata: input.metadata,
    });
    this.wireSession(handle);
    return this.resumedSessionSummary(handle);
  }

  /**
   * Through the session's handler (`IWorkspaceDirs`, workspace scope) — the
   * workspace-level add-dir surface: `persist: true` (v1's default) appends
   * to the project-local `.mirri-code/local.toml`, `persist: false` joins the
   * handler's shared in-memory set. The set is shared by every session of the
   * workspace (a v1 `persist: false` dir was session-scoped and written into
   * session metadata to survive a resume; the v2 handler keeps it for every
   * session of the workspace until the process exits). Returns the same
   * `{additionalDirs, projectRoot, configPath, persisted}` shape as v1
   * (`WorkspaceAdditionalDirsResult` is field-identical with v1's
   * `AddAdditionalDirResult`).
   */
  override async addAdditionalDir(input: AddAdditionalDirInput): Promise<AddAdditionalDirResult> {
    const handle = this.requireLiveSession(input.id);
    return handle.accessor.get(IWorkspaceDirs).addDir({
      path: input.path,
      persist: input.persist,
    });
  }

  /**
   * Through `engineAccessor` (`ISessionExportService`, app scope) — the v2
   * port of v1's export: same payload fields, same zip writer layout, same
   * live-session flush before the read. Works on closed sessions on both
   * engines (v1 reads the store, v2 the index). Gaps vs v1, pinned in the
   * migration tracker: v2 additionally validates the host `version`
   * (blank → `session_export.missing_version` — v1 records it unchecked), the
   * manifest's activity timestamps come from v2's per-agent wire scan (v1
   * scans only the root `wire.jsonl`), and the zip ENTRY LIST is not part of
   * the parity surface (the engines lay their session directories out
   * differently by design). The returned manifest is remapped to the v1
   * contract's field names (v2's manifest still spells the version field
   * `kimiCodeVersion`, a heritage name; v1's is `mirriCodeVersion`).
   */
  override async exportSession(input: ExportSessionInput): Promise<ExportSessionResult> {
    const result = await this.engineAccessor.get(ISessionExportService).export({
      sessionId: input.id,
      outputPath: input.outputPath,
      includeGlobalLog: input.includeGlobalLog,
      version: input.version,
      installSource: input.installSource,
      shellEnv: input.shellEnv,
    });
    return {
      ...result,
      manifest: {
        ...result.manifest,
        mirriCodeVersion: result.manifest.kimiCodeVersion,
      },
    };
  }

  // -----------------------------------------------------------------------
  // Agent interaction
  //
  // v1 serves these from the session's eagerly-created main agent, already
  // configured with the model/thinking defaults. The v2 engine splits them
  // across the klient agent facade (model / permission / plan / context /
  // usage / cancel — validated contract calls) and the session scope, both
  // reached through the live session handle. Every override requires a live
  // session (`v1's requireSession`) and resolves the target agent from
  // `interactiveAgentId`: the main agent materializes on first use with the
  // default profile bound (v1's eager equivalent); any other agent must
  // already exist (`AGENT_NOT_FOUND`).
  // -----------------------------------------------------------------------

  /**
   * The session's materialized main agent with v1's eager default binding
   * applied: a freshly created agent whose profile is still unbound gets the
   * default profile + configured default model. A home with no configured
   * model leaves the agent unbound instead of failing — v1's model-less
   * session reads (`model: undefined`, `'off'` thinking, zero capabilities)
   * map onto the unbound state exactly.
   */
  private async materializeMainAgent(
    session: ISessionScopeHandle,
    binding?: { readonly model?: string; readonly thinking?: string },
  ): Promise<IAgentScopeHandle> {
    await this.modelReady;
    const agent = await ensureMainAgent(session);
    const profile = agent.accessor.get(IAgentProfileService);
    if (binding !== undefined || profile.data().profileName === undefined) {
      try {
        await profile.bind({
          profile: DEFAULT_AGENT_PROFILE_NAME,
          model: binding?.model,
          thinking: binding?.thinking,
        });
      } catch (error) {
        if (
          binding === undefined &&
          error instanceof ProfileError &&
          error.code === ProfileErrors.codes.MODEL_NOT_CONFIGURED
        ) {
          return agent;
        }
        throw error;
      }
    }
    return agent;
  }

  /** The target agent's live scope handle (see the section header). */
  private async agentScope(sessionId: string): Promise<IAgentScopeHandle> {
    const session = this.requireLiveSession(sessionId);
    const agentId = this.interactiveAgentId;
    if (agentId === MAIN_AGENT_ID) return this.materializeMainAgent(session);
    const agent = session.accessor.get(IAgentLifecycleService).get(agentId);
    if (agent === undefined) {
      throw new MirriError(ErrorCodes.AGENT_NOT_FOUND, `Agent "${agentId}" was not found`);
    }
    return agent;
  }

  /**
   * The klient agent facade for the target agent. The scope is resolved
   * first so the main agent exists and carries its default binding before
   * the facade call crosses the channel.
   */
  private async agentFacade(sessionId: string): Promise<AgentHandle> {
    await this.agentScope(sessionId);
    return this.klient.session(sessionId).agent(this.interactiveAgentId);
  }

  // -----------------------------------------------------------------------
  // Print-mode policy (`mirri -p`)
  //
  // The v2 engine owns this policy itself: `IPrintModeService` (agent scope)
  // is the port of v1's `Session.waitForBackgroundTasksOnPrint` /
  // `handlePrintMainTurnCompleted`, carrying the same exit/drain/steer modes,
  // the same suppress-then-wait drain loop, and the same ceiling / turn-cap
  // bounds. It also holds the steer deadline and turn counter, which v1 kept
  // as Session fields — so these overrides delegate rather than re-deriving
  // the policy from config on the SDK side, and a print run's steer state
  // lives with the agent it belongs to instead of in a client-side map.
  // No klient facade exposes the print policy, hence the agent scope.
  // -----------------------------------------------------------------------

  override async waitForBackgroundTasksOnPrint(input: SessionIdRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IPrintModeService).waitForBackgroundTasksOnPrint();
  }

  override async handlePrintMainTurnCompleted(
    input: SessionIdRpcInput,
  ): Promise<'finish' | 'continue'> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IPrintModeService).handlePrintMainTurnCompleted();
  }

  /**
   * v1's `Session.getSessionWarnings`, rebuilt over v2 primitives: no v2
   * service owns the warnings aggregate. The source is the profile's cached
   * `agentsMdWarning` (computed on every bind — v1's bootstrap-time cache),
   * recomputed through the engine's own `prepareSystemPromptContext` when the
   * cache is empty, mirroring v1's `computeAgentsMdWarning` fallback for
   * resumed sessions that skipped bootstrap. A recompute failure degrades to
   * "no warning" exactly like v1's caught-and-logged path.
   *
   * No divergence from the upstream reference is folded in here: this
   * repository's v1 `getSessionWarnings` emits the AGENTS.md warning ONLY,
   * so surfacing any other engine warning in this client would invent a
   * warning v1 never returned. Revisit when v1's contract grows.
   */
  override async getSessionWarnings(input: SessionIdRpcInput) {
    const agent = await this.agentScope(input.sessionId);
    let warning = agent.accessor.get(IAgentProfileService).getAgentsMdWarning();
    if (warning === undefined) {
      const session = this.requireLiveSession(input.sessionId);
      try {
        const prepared = await prepareSystemPromptContext(
          {
            fs: this.engineAccessor.get(IHostFileSystem),
            homeDir: this.engineAccessor.get(IHostEnvironment).homeDir,
          },
          session.accessor.get(ISessionContext).cwd,
          this.engineAccessor.get(IBootstrapService).homeDir,
          { additionalDirs: session.accessor.get(ISessionWorkspaceContext).additionalDirs },
        );
        warning = prepared.agentsMdWarning;
      } catch {
        // v1 logs and returns no warning rather than failing the caller.
        warning = undefined;
      }
    }
    return warning === undefined
      ? []
      : [{ code: 'agents-md-oversized', message: warning, severity: 'warning' as const }];
  }

  /** Facade (`agentProfileService.setModel`). */
  override async setModel(input: SetSessionModelRpcInput): Promise<SetSessionModelRpcResult> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.setModel(input.model);
  }

  override async setPermission(input: SetSessionPermissionRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.setPermission(input.mode);
  }

  /** v1 maps the toggle onto two RPCs (`enterPlan` / `cancelPlan`); so does v2. */
  override async setPlanMode(input: SetSessionPlanModeRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    if (!input.enabled) return agent.cancelPlan();
    return agent.enterPlan();
  }

  /**
   * Facade (`agentRPCService.getContext`). The v2 `AgentContextData` is the
   * same wire shape as v1's — the cast only bridges the two packages' type
   * declarations; the data itself crossed the same JSON boundary on both
   * sides.
   */
  override async getContext(input: SessionIdRpcInput): Promise<AgentContextData> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.getContext() as Promise<AgentContextData>;
  }

  override async getUsage(input: SessionIdRpcInput): Promise<SessionUsage> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.getUsage();
  }

  /**
   * The base class aggregates v1's per-agent `getConfig` / `getContext` /
   * `getPermission` / `getPlan` / `getSwarmMode` / `getUsage` RPCs. The v2
   * rebuild reads the same six slices: the profile's bound model alias and
   * resolved thinking level + capabilities (v1's agent `getConfig`), the
   * facade's context/plan/usage, and the permission-mode and swarm services.
   * `busy` stays `false` — the base class never computed it either.
   */
  override async getStatus(input: SessionIdRpcInput): Promise<SessionStatus> {
    const agent = await this.agentScope(input.sessionId);
    const facade = this.klient.session(input.sessionId).agent(this.interactiveAgentId);
    const [context, plan, usage] = await Promise.all([
      facade.getContext(),
      facade.getPlan(),
      facade.getUsage(),
    ]);
    const profile = agent.accessor.get(IAgentProfileService).data();
    const capability = profile.modelCapabilities;
    const maxContextTokens = capability.max_input_tokens ?? capability.max_context_tokens;
    const contextTokens = context.tokenCount;
    // Deliberately unclamped, same as the base class (>100% is the documented
    // overflow signal on this path).
    const contextUsage = maxContextTokens > 0 ? contextTokens / maxContextTokens : 0;
    const hasUsage =
      usage.byModel !== undefined || usage.total !== undefined || usage.currentTurn !== undefined;
    return {
      busy: false,
      model: profile.modelAlias,
      thinkingEffort: profile.thinkingLevel,
      permission: agent.accessor.get(IAgentPermissionModeService).mode,
      planMode: plan !== null,
      swarmMode: agent.accessor.get(IAgentSwarmService).isActive,
      contextTokens,
      maxContextTokens,
      contextUsage,
      usage: hasUsage ? usage : undefined,
    };
  }

  override async cancel(input: SessionIdRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.cancel();
  }

  /**
   * Session-level cron (v2 shape): the engine owns a single session-scoped
   * cron service that steers the main agent — the v1 per-agent manager's
   * main-agent port. Non-main interactive agents report an empty list (v1
   * returned `[]` for a subagent too — its cron manager is null). The v1
   * snapshot shape is restored field-by-field: `recurring` defaults to true,
   * `nextFireAt` comes from the same scheduler read v1 forwards.
   */
  override async getCronTasks(input: SessionIdRpcInput): Promise<GetCronTasksResult> {
    await this.agentScope(input.sessionId);
    if (this.interactiveAgentId !== MAIN_AGENT_ID) return { tasks: [] };
    const cron = this.requireLiveSession(input.sessionId).accessor.get(ISessionCronService);
    return {
      tasks: cron.list().map((task) => ({
        id: task.id,
        cron: task.cron,
        recurring: task.recurring !== false,
        createdAt: task.createdAt,
        lastFiredAt: task.lastFiredAt,
        nextFireAt: cron.getNextFireForTask(task.id),
      })),
    };
  }

  override async prompt(input: SessionPromptRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    await agent.prompt({ input: input.input });
  }

  override async steer(input: SessionPromptRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    await agent.steer({ input: input.input });
  }

  // -----------------------------------------------------------------------
  // Plan / thinking / compaction / undo / shell
  //
  // v1 serves these from the session's main agent (already configured at
  // create). The v2 engine splits them across the klient agent facade
  // (plan status / clear — validated contract calls), agent-scope services
  // the facade does not cover (thinking levels, full compaction, undo), and
  // the facade's shell command seam. Every override goes through the same
  // live-session + `interactiveAgentId` resolution as the rest of the agent
  // interaction section.
  // -----------------------------------------------------------------------

  /** Facade (`agentPlanService.status`). v2 `PlanData` is field-identical with v1's `SessionPlan`. */
  override async getPlan(input: SessionIdRpcInput): Promise<SessionPlan> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.getPlan();
  }

  /** Facade (`agentPlanService.clear`) — same unconditional clear as v1's `clearPlan`. */
  override async clearPlan(input: SessionIdRpcInput): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.clearPlan();
  }

  /**
   * Through the agent scope (`IAgentProfileService.setThinking`) — no klient
   * facade exists. v1's contract normalizes the effort (trim) and rejects a
   * blank one with `session_thinking_empty` before the RPC reaches the
   * profile; v2's service validates the resolved effort against the current
   * model's supported efforts (`model.config_invalid`) — the same two-stage
   * validation the v1 core applies.
   */
  override async setThinking(input: SetSessionThinkingRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    const effort = input.effort.trim();
    if (effort.length === 0) {
      throw new MirriError(
        ErrorCodes.SESSION_THINKING_EMPTY,
        'Session thinking effort cannot be empty',
      );
    }
    agent.accessor.get(IAgentProfileService).setThinking(effort);
  }

  /**
   * Through the agent scope (`IAgentFullCompactionService.begin`) — no klient
   * facade exists. Same semantics as v1's `beginCompaction`: a manual
   * compaction launches the summarizer immediately in the background, is a
   * silent no-op while one is already running, and rejects with
   * `compaction.unable` on an empty history or an active turn. The `begin`
   * result (boolean) is dropped — v1's RPC returns void.
   */
  override async compact(input: SessionIdRpcInput & CompactOptions): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    agent.accessor.get(IAgentFullCompactionService).begin({
      source: 'manual',
      instruction: input.instruction,
    });
  }

  /**
   * Through the agent scope (`IAgentRPCService.cancelCompaction`, the v2 RPC
   * surface's own cancel) — no klient facade exists. Aborts the in-flight
   * compaction; a no-op when idle, like v1.
   */
  override async cancelCompaction(input: SessionIdRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentRPCService).cancelCompaction({});
  }

  /**
   * Through the agent scope (`IAgentRPCService.undoHistory`, the v2 RPC
   * surface's own undo) — no klient facade exists; the returned count is
   * dropped (v1 returns void). Failure semantics differ by design: v2
   * prechecks and rejects atomically with `session.undo_unavailable`, while
   * v1 splices a partial suffix out of the live history and then throws
   * `request.invalid` — pinned in the parity KNOWN_DIFFS.
   */
  override async undoHistory(input: SessionIdRpcInput & { count: number }): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentRPCService).undoHistory({ count: input.count });
  }

  /**
   * Facade (`agentShellCommandService.run`) — the same builtin-Bash execution
   * and `shell_command`-origin history records as v1, with an identical
   * `{stdout, stderr, isError?, backgrounded?}` result shape. The `commandId`
   * event stream (`shell.output` / `shell.started` / `shell.completed`) is
   * engine-side on both; translating it into SDK events is the event batch's
   * job. Model-less gap, not pinned: v1's builtin tools only exist on a
   * profiled agent, so a model-less v1 session answers "Bash tool is not
   * available." where v2 runs the command.
   */
  override async runShellCommand(input: {
    sessionId: string;
    command: string;
    commandId?: string;
  }): Promise<{ stdout: string; stderr: string; isError?: boolean; backgrounded?: boolean }> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.runShellCommand({ command: input.command, commandId: input.commandId });
  }

  /** Facade (`agentShellCommandService.cancel`) — an unknown id is a silent no-op on both engines. */
  override async cancelShellCommand(input: { sessionId: string; commandId: string }): Promise<void> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.cancelShellCommand({ commandId: input.commandId });
  }

  // -----------------------------------------------------------------------
  // Skills / plugin commands / AGENTS.md / side questions / swarm
  // -----------------------------------------------------------------------

  /**
   * Through the session scope (`ISessionSkillCatalog`) — no klient facade
   * exists. Same merged view v1's `listSkills` serves (builtin + user +
   * project + plugin skills through the same `summarizeSkill` mapping), with
   * the same snapshot-vs-live caveat as `listPluginCommands`: v1 loads the
   * registry once at session creation while the v2 catalog re-merges on
   * source changes mid-session; the two agree for any session created after
   * the last skill change.
   */
  override async listSkills(input: SessionIdRpcInput): Promise<readonly SkillSummary[]> {
    const catalog = this.requireLiveSession(input.sessionId).accessor.get(ISessionSkillCatalog);
    await catalog.ready;
    return catalog.catalog.listSkills().map(summarizeSkill);
  }

  /**
   * Through the agent scope (`IAgentSkillService.activate`) — deliberately
   * NOT `IAgentRPCService.activateSkill`, whose fire-and-forget
   * (`void this.skills.activate(...)`) turns v1's synchronous rejections
   * (`skill.not_found` / `skill.type_unsupported`) into unhandled rejections.
   * The direct call keeps v1's semantics: validate first, then render the
   * skill prompt and launch a turn with it. v1's session layer then updates
   * title/lastPrompt for the MAIN agent only; replicated here over the
   * engine's shared metadata helpers. Busy-turn gap vs v1, pinned in the
   * migration tracker: v1 drops the activation into an error event while a
   * turn runs; v2's activate awaits the queued prompt's launch.
   */
  override async activateSkill(input: ActivateSkillRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentSkillService).activate({ name: input.name, args: input.args });
    if (this.interactiveAgentId === MAIN_AGENT_ID) {
      await this.updatePromptMetadata(input.sessionId, promptMetadataTextFromSkill(input));
    }
  }

  /**
   * Through the agent scope (`IAgentRPCService.activatePluginCommand`) — the
   * v2 RPC surface's own implementation: the same `request.invalid` rejection
   * text for an unknown command, the same argument expansion, the activation
   * event, the prompt enqueue, and the metadata update. Two gaps vs v1,
   * pinned in the migration tracker: v1 resolves the command against the
   * session's creation-time snapshot (v2 uses the app-global live view), and
   * v1 drops the activation while a turn runs where v2 queues it.
   */
  override async activatePluginCommand(input: ActivatePluginCommandRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentRPCService).activatePluginCommand({
      pluginId: input.pluginId,
      commandName: input.commandName,
      args: input.args,
    });
  }

  /**
   * Contributed commands are a v2-only seam (`IAgentCommandService`, agent
   * scope) — mapped 1:1 here.
   */
  override async listCommands(input: ListCommandsRpcInput): Promise<readonly AgentCommandInfo[]> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentCommandService).list();
  }

  override async runCommand(input: RunCommandRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentCommandService).run(input.name, input.args);
  }

  /**
   * Through the session scope (`ISessionInitService.generateAgentsMd`, the
   * engine's port of v1's `Session.generateAgentsMd`) — a session-level
   * operation pinned to the main agent on both engines, so
   * `interactiveAgentId` does not apply; the main agent is materialized first
   * (v1 creates it eagerly at createSession). The success path is a real
   * subagent LLM round (`/init` brief), so parity covers only the model-less
   * rejection: both engines fail with `session.init_failed`, with different
   * messages (v1 wraps the provider-resolution failure, v2 preflights the
   * missing binding).
   */
  override async generateAgentsMd(input: SessionIdRpcInput): Promise<void> {
    const session = this.requireLiveSession(input.sessionId);
    await this.materializeMainAgent(session);
    await session.accessor.get(ISessionInitService).generateAgentsMd();
  }

  /**
   * Through the session scope (`ISessionBtwService.start`) — no klient facade
   * exists. The v2 service is the port of v1's btw fork: same inherited
   * profile/context, same byte-identical side-question reminder, same
   * tool-call deny, and the same return (the forked child's agent id). The
   * main agent is materialized first — both engines fork it as the source,
   * and v2's fork throws on a missing source. Gaps vs v1: v2 always forks
   * MAIN where v1 forks the agent `interactiveAgentId` addresses (SDK hosts
   * only ever btw the main agent), and the v2 child is a regular persisted
   * agent where v1's is memory-only (no metadata).
   */
  override async startBtw(input: SessionIdRpcInput): Promise<string> {
    const session = this.requireLiveSession(input.sessionId);
    await this.materializeMainAgent(session);
    return session.accessor.get(ISessionBtwService).start();
  }

  /**
   * Through the agent scope (`IAgentSwarmService.enter` / `.exit`) — no
   * klient facade exists. The v2 service is the port of v1's `SwarmMode`:
   * enter is idempotent and injects the byte-identical enter reminder for
   * non-`tool` triggers, exit pops that reminder when it is the last message
   * (appending the exit reminder otherwise), and `task` / `tool` triggers
   * auto-exit on turn end. The base class's private enter/exit pair is
   * replaced wholesale; `swarm()` below recomposes it over this override.
   */
  override async setSwarmMode(input: SetSessionSwarmModeRpcInput): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    const swarm = agent.accessor.get(IAgentSwarmService);
    if (input.enabled) {
      swarm.enter(input.trigger);
      return;
    }
    swarm.exit();
  }

  /** v1's `swarm()` composition: enter with the one-shot `task` trigger, then prompt. */
  override async swarm(input: SessionPromptRpcInput): Promise<void> {
    await this.setSwarmMode({ sessionId: input.sessionId, enabled: true, trigger: 'task' });
    return this.prompt(input);
  }

  // -----------------------------------------------------------------------
  // Goal / background tasks
  //
  // The goal service is the v2 port of v1's `GoalMode` (same state machine,
  // same validations, same error codes), so the goal overrides are thin
  // forwards through the agent scope. The task manager moved from per-agent
  // (v1) to an agent-scope service with field-identical wire shapes: the
  // lists/reads ride the klient facade, while stop/detach go straight to the
  // service (the facade's no-reason stop stamps a user-cancellation reason
  // v1 never records).
  // -----------------------------------------------------------------------

  /**
   * Through the agent scope (`IAgentGoalService.createGoal`) — no klient
   * facade exists for the goal domain. Gap: v2 rejects every goal command on
   * a non-main agent (`goal.unsupported_agent`) where v1 keeps a `GoalMode`
   * on every agent; only reachable through a non-main `interactiveAgentId`.
   */
  override async createGoal(input: SessionIdRpcInput & CreateGoalInput): Promise<GoalSnapshot> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor
      .get(IAgentGoalService)
      .createGoal({ objective: input.objective, replace: input.replace });
  }

  override async getGoal(input: SessionIdRpcInput): Promise<GoalToolResult> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentGoalService).getGoal();
  }

  override async pauseGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentGoalService).pauseGoal();
  }

  override async resumeGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentGoalService).resumeGoal();
  }

  override async cancelGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentGoalService).cancelGoal();
  }

  /**
   * Facade (`agentTaskService.list`). The v2 `AgentTaskInfo` union is the
   * same wire shape as v1's `BackgroundTaskInfo` — the process / agent /
   * question kinds are field-identical — so the cast only bridges the two
   * packages' type declarations. One content gap, pinned in the parity
   * KNOWN_DIFFS: after a detach, v2 rewrites the reported `timeoutMs` to the
   * detach deadline where v1 keeps the foreground one.
   */
  override async listBackgroundTasks(
    input: SessionIdRpcInput & { activeOnly?: boolean; limit?: number },
  ): Promise<readonly BackgroundTaskInfo[]> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.getTasks({ activeOnly: input.activeOnly, limit: input.limit }) as Promise<
      readonly BackgroundTaskInfo[]
    >;
  }

  /**
   * Facade (`agentTaskService.readOutput`) — same unknown-id-returns-`''`
   * behavior and the same trailing-characters `tail` semantics as v1.
   */
  override async getBackgroundTaskOutput(
    input: SessionIdRpcInput & { taskId: string; tail?: number },
  ): Promise<string> {
    const agent = await this.agentFacade(input.sessionId);
    return agent.getTaskOutput({ taskId: input.taskId, tail: input.tail });
  }

  /**
   * Through the agent scope (`IAgentTaskService.stop`) — deliberately NOT the
   * facade's `stopTask`, whose no-reason path routes to `stopByUser` and
   * stamps a user-cancellation `stopReason` where v1's
   * `background.stop(taskId, reason)` records none. The direct call matches
   * v1 in both shapes (reason trimmed, blank → undefined). Timing gap: v1
   * fire-and-forgets the stop so its RPC returns before the kill settles; the
   * v2 service awaits the termination — a strictly stronger guarantee.
   */
  override async stopBackgroundTask(
    input: SessionIdRpcInput & { taskId: string; reason?: string },
  ): Promise<void> {
    const agent = await this.agentScope(input.sessionId);
    await agent.accessor.get(IAgentTaskService).stop(input.taskId, input.reason);
  }

  /**
   * Through the agent scope (`IAgentTaskService.detach`) — no klient facade
   * exists. Same semantics as v1's `background.detach`: releases the
   * foreground tool-call waiter, returns the live info (or the ghost / live
   * info for an already-terminal task, `undefined` for an unknown id).
   */
  override async detachBackgroundTask(
    input: SessionIdRpcInput & { taskId: string },
  ): Promise<BackgroundTaskInfo | undefined> {
    const agent = await this.agentScope(input.sessionId);
    return agent.accessor.get(IAgentTaskService).detach(input.taskId);
  }

  // -----------------------------------------------------------------------
  // Plugins
  //
  // The wire types are field-identical between the engines, so no mapping
  // layer is needed. Unlike the config domain, the v2 plugin service
  // serializes every read behind its own initial load, so there is no ready
  // trap here.
  // -----------------------------------------------------------------------

  override async listPlugins(): Promise<readonly PluginSummary[]> {
    return this.klient.global.plugins.list();
  }

  override async installPlugin(source: string): Promise<PluginSummary> {
    return this.klient.global.plugins.install(source);
  }

  override async setPluginEnabled(id: string, enabled: boolean): Promise<void> {
    await this.klient.global.plugins.setEnabled({ id, enabled });
  }

  override async setPluginMcpServerEnabled(
    id: string,
    server: string,
    enabled: boolean,
  ): Promise<void> {
    await this.klient.global.plugins.setMcpServerEnabled({ id, server, enabled });
  }

  override async removePlugin(id: string): Promise<void> {
    await this.klient.global.plugins.remove(id);
  }

  override async reloadPlugins(): Promise<ReloadSummary> {
    return this.klient.global.plugins.reload();
  }

  /**
   * App-global live view of the enabled plugin commands, no session required.
   * The v1 SDK contract answers from the session's creation-time snapshot;
   * the v2 engine only exposes the live view, so the sessionId is ignored in
   * {@link listPluginCommands}. The two agree for any session created after
   * the last plugin change.
   */
  override async listPluginCommands(input: SessionIdRpcInput): Promise<readonly PluginCommandDef[]> {
    void input;
    return this.listPluginCommandsGlobal();
  }

  /**
   * The session-id-less shape of {@link listPluginCommands}, for hosts with no
   * open session (v2-only addition; the v1 SDK client has no equivalent).
   */
  async listPluginCommandsGlobal(): Promise<readonly PluginCommandDef[]> {
    return this.klient.global.plugins.listCommands();
  }

  override async getPluginInfo(id: string): Promise<PluginInfo> {
    // v2's manifest hook-event union is a subset of v1's, so the v2
    // `PluginInfo` shape carries no event a v1 consumer cannot express — the
    // official API surfaces it directly.
    return this.klient.global.plugins.info(id);
  }

  // -----------------------------------------------------------------------
  // MCP servers (session-scoped connection manager)
  //
  // Through the session scope (the seeded `ISessionMcpHandle.connectionManager`
  // — the workspace handler's one shared manager). create/resume no longer
  // waits for MCP startup, so entries may still be pending: a live snapshot
  // of the connect progress, matching v1's post-`ready` reads. The v2
  // `McpServerEntry` is field-identical with v1's `McpServerInfo` (the cast
  // bridges the two packages' type declarations).
  // -----------------------------------------------------------------------

  override async listMcpServers(input: SessionIdRpcInput): Promise<readonly McpServerInfo[]> {
    const mcp = this.requireLiveSession(input.sessionId).accessor.get(ISessionMcpHandle);
    return mcp.connectionManager.list() as readonly McpServerInfo[];
  }

  override async getMcpStartupMetrics(input: SessionIdRpcInput): Promise<McpStartupMetrics> {
    const mcp = this.requireLiveSession(input.sessionId).accessor.get(ISessionMcpHandle);
    await mcp.ready;
    return { durationMs: mcp.connectionManager.initialLoadDurationMs() };
  }

  /**
   * Same direct `reconnect` as v1's session RPC — the v2 manager raises the
   * same `mcp.server_not_found` / `mcp.server_disabled` errors, and the tool
   * re-registration rides on the status listeners in both engines.
   */
  override async reconnectMcpServer(input: ReconnectMcpServerRpcInput): Promise<void> {
    const mcp = this.requireLiveSession(input.sessionId).accessor.get(ISessionMcpHandle);
    await mcp.connectionManager.reconnect(input.name);
  }

  /**
   * v1's `Session.updatePromptMetadata` — the title/lastPrompt refresh for a
   * just-activated skill: same merged metadata write and the same
   * `session.meta.updated` event the v1 path published, both through the
   * engine's shared helpers (`applyPromptMetadataUpdate`).
   */
  private async updatePromptMetadata(sessionId: string, text: string | undefined): Promise<void> {
    const session = this.requireLiveSession(sessionId);
    await applyPromptMetadataUpdate(
      {
        metadata: session.accessor.get(ISessionMetadata),
        eventService: this.engineAccessor.get(IEventService),
        sessionId,
      },
      text,
    );
  }
}

export function createMirriHarnessV2(options: MirriHarnessOptions): MirriHarness {
  const rpc = new SDKRpcClientV2(options);
  return new MirriHarness(rpc, {
    identity: rpc.identity,
    uiMode: options.uiMode,
    homeDir: rpc.homeDir,
    configPath: rpc.configPath,
    auth: rpc.auth,
    telemetry: rpc.telemetry,
    ensureConfigFile: () => rpc.ensureConfigFile(),
    onClose: () => rpc.close(),
    // The v2 engine has no ingestion-limit equivalent of v1's core-owned
    // `imageLimits`; leave it undefined so ingestion falls back to
    // env/built-in defaults like daemon-client hosts.
    sessionStartedProperties: options.sessionStartedProperties,
  });
}

/** v1's `requiredWorkDir`: reject blank and normalize to the canonical spelling. */
function normalizeRequiredWorkDir(operation: string, workDir: string): string {
  if (typeof workDir !== 'string' || workDir.trim() === '') {
    throw new MirriError(ErrorCodes.REQUEST_WORK_DIR_REQUIRED, `${operation} requires workDir`);
  }
  return normalizeWorkDir(workDir);
}

/**
 * v1's canonical workDir spelling (mirror of the v1 core's
 * `normalizeWorkDir` in `agent-core/session/store/workdir-key`): Windows-
 * shaped paths resolve through `win32` and fold to forward slashes,
 * everything else resolves against the process cwd. The workspace registry
 * minted the session buckets from this same spelling, so re-spelling before
 * the lookup keeps the keys comparable across engines.
 */
function normalizeWorkDir(workDir: string): string {
  if (/^[A-Za-z]:[\\/]/.test(workDir) || /^[\\/]{2}[^\\/]+[\\/][^\\/]+/.test(workDir)) {
    return win32.resolve(workDir).replaceAll('\\', '/');
  }
  return resolve(workDir);
}