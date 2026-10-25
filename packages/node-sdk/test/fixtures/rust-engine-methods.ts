/**
 * Single source of truth for the embedded rust engine's full dotted-method
 * surface (198 methods — 96 M5-3b entries plus the five B1 session
 * additions: `session.get` / `session.countActive` / `session.createChild`
 * / `session.registerAgent` / `session.restore`, and the five B2 gap
 * implementations: `agent.getEffectiveThinkingLevel` / `agent.listCommands`
 * / `agent.runCommand` / `task.stop` / `agent.activity`, and the B3 gap
 * implementation `plugin.checkUpdates`, and the eight B4-a interaction
 * rows: `approval.decide` / `approval.listPending` / `question.answer` /
 * `question.dismiss` / `question.listPending` /
 * `interaction.listPending` / `interaction.respond` /
 * `interaction.isRecentlyResolved`, and the six B4-b workspace rows:
 * `workspace.list` / `workspace.createOrTouch` / `workspace.update` /
 * `workspace.delete` / `workspace.get` / `workspace.handlerFor`, and the
 * twenty-one B5-a rows (thirteen env reads, five flag reads, two fs rows,
 * providerDiscovery.refreshProviderModels), and the fourteen B5-b rows
 * (five config section methods, five model record/catalog methods, four
 * provider CRUD rows), the B5-c `model.generate` deep method, the
 * eight B5-d auth rows (the oauthService flow surface plus the
 * authSummaryService reads), and the four batch-3 P1 MCP config rows
 * (`mcp.config.list` / `set` / `remove` / `setToolFilter`), and the two
 * batch-4 P2 MCP pool rows (`mcp.config.test` / `mcp.status`), and the two
 * T2 user-tool management rows (`tool.listUserTools` /
 * `tool.setUserToolEnabled`), and the P6 B4a
 * `providerDiscovery.listCatalog` browse row, and the P7 B4b
 * `providerDiscovery.importCatalog` import row, and the M6 `mcp.sync`
 * idempotent reconcile row, and the W8 `workspace.setTrust` trust
 * management row, and the two collaboration mode-gate rows
 * (`collaboration.enter` / `collaboration.exit` — registered by the
 * collaboration batch, fixture entries backfilled by B8 to close the
 * drift), and the two B8 skills management rows (`skill.getContent` /
 * `skill.setEnabled`), and the two B6 cron management rows
 * (`cron.delete` / `cron.trigger`), and the B7 task-overview row
 * (`task.listWorkspace`), and the ADR-010 `skill.reload` row (the manual
 * skill-catalog reload face),
 * per the rust-v2-method-parity program).
 *
 * Every method carries a contract-e2e `mode`:
 *
 * - `schema`   — the response is checkable against a klient zod schema.
 *                Since M0.2 the runner validates the response against the
 *                klient contract table (`KLIENT_RESPONSE_SCHEMAS` in
 *                `rust-engine-contract-e2e.test.ts`); methods without a
 *                klient schema yet (no MCP/warning/skill contracts in
 *                klient) are documented gaps still guarded by the explicit
 *                855f57ef regression assertions.
 * - `snapshot` — the response shape is locked: the runner normalizes the
 *                response to a shape description (type names, key sets,
 *                element shapes — never concrete values or lengths) and
 *                compares it against the committed
 *                `rust-engine-contract-snapshots.json`, so any wire-shape
 *                drift during the RPC backend convergence trips the test.
 * - `error`    — the call must reject with a structured engine error: the
 *                runner asserts the rejection carries the expected
 *                `domainCode` (and optional `expectCode`) across the NAPI
 *                error channel (M-2; the retired/unimplemented methods the
 *                engine answers honestly since M1.2-b).
 * - `skip`     — excluded from the sweep; `reason` states why (side
 *                effects, pre-existing state requirements, flows).
 *
 * This module is data, not logic: classification, params and reasons are
 * declarative fields; the generic sweep logic lives in the runner
 * (`test/rust-engine-contract-e2e.test.ts`).
 */

/** How the contract-e2e runner treats a method. */
export type RustEngineContractMode = 'schema' | 'snapshot' | 'error' | 'skip';

/**
 * Placeholder the runner substitutes with the id of the session created
 * during test setup (used by methods that address a live session).
 */
export const SETUP_SESSION_ID = '$setupSessionId';

/**
 * Placeholder the runner substitutes with the id of the engine's registered
 * workspace (resolved through `workspace.list` during setup — the mock
 * engine's single registration is the cwd-derived key; W6).
 */
export const SETUP_WORKSPACE_ID = '$setupWorkspaceId';

/** The default (first) agent of the setup session. */
export const MAIN_AGENT_ID = 'main';

export interface RustEngineMethodFixtureBase {
  /** Dotted method name exactly as dispatched by the embedded engine. */
  readonly method: string;
  /**
   * Minimal viable params (`{}` omitted when the method takes none).
   * `${SETUP_SESSION_ID}` values are replaced with the setup session id by
   * the runner.
   */
  readonly params?: Readonly<Record<string, unknown>>;
}

/** Response is (or will be) validated against a klient zod schema. */
export interface RustEngineSchemaFixture extends RustEngineMethodFixtureBase {
  readonly mode: 'schema';
  /** Classification rationale. */
  readonly note: string;
}

/** Response shape is locked against the committed snapshot file. */
export interface RustEngineSnapshotFixture extends RustEngineMethodFixtureBase {
  readonly mode: 'snapshot';
  /** Classification rationale. */
  readonly note: string;
}

/**
 * The call must reject with a structured engine error (M-2): the runner
 * asserts the rejection decodes on the NAPI error channel
 * (`#/rust-engine/error-channel`) with the expected `domainCode` — and the
 * optional `expectCode` JSON-RPC code — so the M1.2-b honest-error wire
 * shape is guarded across the NAPI boundary, not only in Rust-side tests.
 */
export interface RustEngineErrorFixture extends RustEngineMethodFixtureBase {
  readonly mode: 'error';
  /** The domain code the rejection must carry (e.g. "not_implemented"). */
  readonly expectDomainCode: string;
  /** The JSON-RPC code the rejection must carry (default: none asserted). */
  readonly expectCode?: number;
  /** Classification rationale. */
  readonly note: string;
}

/** Excluded from the sweep; `reason` is mandatory. */
export interface RustEngineSkipFixture extends RustEngineMethodFixtureBase {
  readonly mode: 'skip';
  /** Why the method cannot be exercised in the contract sweep. */
  readonly reason: string;
}

export type RustEngineMethodFixture =
  | RustEngineSchemaFixture
  | RustEngineSnapshotFixture
  | RustEngineErrorFixture
  | RustEngineSkipFixture;

/**
 * All 198 embedded dotted methods, exactly one entry each (asserted by the
 * contract e2e test itself).
 */
export const RUST_ENGINE_METHODS: readonly RustEngineMethodFixture[] = [
  // --- agent domain ------------------------------------------------------

  // Mutating agent setters that stay skipped: each one changes persistent
  // per-agent state the sweep cannot meaningfully exercise (tool set,
  // registry). The M3-c batch 2 setters (model/thinking/permission) ARE
  // swept in snapshot mode: the mutations are self-contained per-session
  // and the result shapes are part of the klient contract.
  {
    method: 'agent.activateSkill',
    mode: 'skip',
    reason:
      'it requires an installed workspace skill by name and injects its SKILL.md content into the agent context (mutating)',
  },
  {
    method: 'agent.activity',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the klient agentActivityView.state contract (agentActivityStateSchema) — the engine-folded activity snapshot; idle shape under the mock engine (no turn in flight during the sweep, so the optional turn/lastTurn keys are absent)',
  },
  {
    method: 'agent.createProfile',
    mode: 'skip',
    reason:
      'it writes a profile file under the engine home (side-effect class; the P6 profile CRUD round trip — create/get/update/delete, the essential/builtin refusals, and the restart persistence — is asserted in the rust embedded tests)',
  },
  {
    method: 'agent.deleteProfile',
    mode: 'skip',
    reason:
      'it deletes a profile file under the engine home (side-effect class; the P6 profile CRUD round trip is asserted in the rust embedded tests)',
  },
  {
    method: 'agent.disableProfile',
    mode: 'skip',
    reason:
      'it writes the file-level enabled=false flag through the .md write channel (side-effect class; the disable/enable byte round trip is asserted in the rust embedded tests)',
  },
  {
    method: 'agent.enableProfile',
    mode: 'skip',
    reason:
      'it rewrites a profile file under the engine home (side-effect class; the no-op and re-enable semantics are asserted in the rust embedded tests)',
  },
  {
    method: 'agent.getProfile',
    mode: 'skip',
    reason:
      'it addresses a profile by name (read-only in isolation, but its meaningful shape needs the create step asserted in the rust embedded tests)',
  },
  {
    method: 'agent.listProfiles',
    mode: 'skip',
    reason:
      'it reads the profile catalog whose rows depend on the engine home contents (the builtin-row and round-trip shapes are asserted in the rust embedded tests)',
  },
  {
    method: 'agent.resetProfile',
    mode: 'skip',
    reason:
      'it deletes a built-in override profile file under the engine home (side-effect class; the reset refusals and built-in restoration are asserted in the rust embedded tests)',
  },
  {
    method: 'agent.updateProfile',
    mode: 'skip',
    reason:
      'it writes a profile file under the engine home (side-effect class; the P6 profile CRUD round trip is asserted in the rust embedded tests)',
  },
  {
    method: 'agent.getConfig',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the addressed agent’s resolved configuration',
  },
  {
    method: 'agent.getContext',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: context-memory stats (message count, used tokens)',
  },
  {
    method: 'agent.getEffectiveThinkingLevel',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the klient agentProfile plain-string contract — the rust ThinkingEffort spelling, "none" when unset',
  },
  {
    method: 'agent.getModel',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the agent’s model id as a plain wire string',
  },
  {
    method: 'agent.getPermission',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the agent’s permission mode and rules',
  },
  {
    method: 'agent.getUsage',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: accumulated token usage broken down by model',
  },
  {
    method: 'agent.listCommands',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the klient AgentCommandInfo[] contract — plugin-registered slash commands (empty under the mock engine, which attaches no plugin manager)',
  },
  {
    method: 'agent.runCommand',
    mode: 'skip',
    reason:
      'it executes a plugin slash command (side effect; requires a command registered by an installed plugin — the unknown-name -32001 branch is asserted in the rust embedded tests)',
  },
  {
    method: 'agent.registerTool',
    mode: 'skip',
    reason:
      'it mutates the agent’s tool registry (and the embedded engine only acknowledges user tools without an executable backing)',
  },
  {
    method: 'agent.setActiveTools',
    mode: 'skip',
    reason: 'it mutates the agent’s active tool set',
  },
  {
    method: 'agent.setModel',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, model: 'contract-e2e-model' },
    note: 'applies the model at runtime and echoes the klient setModelResultSchema: {model, providerName?}',
  },
  {
    method: 'agent.setPermission',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, mode: 'manual' },
    note: 'applies the permission mode at runtime and answers the klient noResult spelling: JSON null',
  },
  {
    method: 'agent.setThinking',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, thinking: 'high' },
    note: 'applies the thinking effort at runtime and answers the klient noResult spelling: JSON null',
  },
  {
    method: 'agent.unregisterTool',
    mode: 'skip',
    reason: 'it mutates the agent’s tool registry',
  },

  // --- approval domain ---------------------------------------------------

  {
    method: 'approval.decide',
    mode: 'skip',
    reason:
      'it resolves an in-flight pending approval by request id (requires a blocked tool call; the B4-a SRV-wire shape and both resolution channels are asserted in the rust embedded tests)',
  },
  {
    method: 'approval.listPending',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the klient approvalRequest[] bare array (empty — no approval is pending during the sweep)',
  },
  {
    method: 'approval.listRules',
    mode: 'snapshot',
    note: 'read-only: the B9 user-level permission rule read face — the [permission] rules of the shared config snapshot ({rules: [...]}; empty under the sweep — the runner seeds no rules)',
  },
  {
    method: 'approval.addRule',
    mode: 'skip',
    reason:
      'it mutates the user’s config.toml [[permission.rules]] and pushes the new rule set into every live agent (the write pipeline — idempotent duplicate, conflicting-decision, and unknown-pair removal semantics — is asserted in the rust embedded tests, plans/batches B9)',
  },
  {
    method: 'approval.removeRule',
    mode: 'skip',
    reason:
      'it mutates the user’s config.toml [[permission.rules]] (see approval.addRule)',
  },

  // --- btw / shell domain ------------------------------------------------

  {
    method: 'btw.start',
    mode: 'skip',
    reason:
      'it forks a real btw side-channel agent (a session side effect; the fork semantics — inherited context, vetoed tools, the {agent_id} wire result — are asserted in the rust embedded tests, P11a F-P8-1)',
  },
  {
    method: 'shell.cancel',
    mode: 'skip',
    reason:
      'it cancels a running foreground shell command (requires a live command id; the cancel path and the unknown-id no-op are asserted in the rust embedded tests — P3i landed the shell runner)',
  },
  {
    method: 'shell.run',
    mode: 'skip',
    reason:
      'it executes a shell command (side effects); the embedded engine answers with a not-supported error result',
  },

  // --- config / core domain ----------------------------------------------

  {
    method: 'config.diagnostics',
    mode: 'snapshot',
    note: 'read-only: config warnings (currently always empty)',
  },
  {
    method: 'config.get',
    mode: 'snapshot',
    note: 'read-only: the resolved config wire shape (provider api keys redacted)',
  },
  {
    method: 'config.removeModel',
    mode: 'skip',
    reason: 'it destructively edits the user’s config.toml',
  },
  {
    method: 'config.removeProvider',
    mode: 'skip',
    reason: 'it destructively edits the user’s config.toml',
  },
  {
    method: 'config.set',
    mode: 'skip',
    reason:
      'it deep-merges a patch into the user’s config.toml and reloads the in-memory config',
  },
  {
    method: 'core.echo',
    mode: 'schema',
    params: { text: 'drill echo' },
    note: 'M5 drill: echoes the text param back as {echo}; serve wire rides the engine fall-through',
  },
  {
    method: 'core.experimentalFeatures',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare array (the client calls .map on it)',
  },
  {
    method: 'core.info',
    mode: 'snapshot',
    note: 'read-only: version + engine identity',
  },

  // --- cron domain ---------------------------------------------------------

  {
    method: 'cron.list',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the session’s cron tasks',
  },
  {
    method: 'cron.delete',
    mode: 'skip',
    reason: 'it mutates the session’s cron task set',
  },
  {
    method: 'cron.trigger',
    mode: 'skip',
    reason: 'it injects a prompt and runs an engine turn in the session',
  },

  // --- goal domain ---------------------------------------------------------

  {
    method: 'goal.cancel',
    mode: 'skip',
    reason: 'it mutates the session’s goal state',
  },
  {
    method: 'goal.create',
    mode: 'skip',
    reason: 'it mutates the session’s goal state',
  },
  {
    method: 'goal.get',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the session’s goal snapshot (null when no goal exists)',
  },
  {
    method: 'goal.pause',
    mode: 'skip',
    reason: 'it mutates the session’s goal state',
  },
  {
    method: 'goal.resume',
    mode: 'skip',
    reason: 'it mutates the session’s goal state',
  },

  // --- interaction domain --------------------------------------------------

  {
    method: 'interaction.isRecentlyResolved',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, interactionId: '00000000-0000-0000-0000-000000000000' },
    note: 'read-only: the plain-boolean recently-resolved check over the session gates’ resolved rings (false — the id was never resolved)',
  },
  {
    method: 'interaction.listPending',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the klient interaction[] envelope (empty — nothing is pending during the sweep)',
  },
  {
    method: 'interaction.respond',
    mode: 'skip',
    reason:
      'it responds to an in-flight pending interaction by id (requires a blocked ask; the approval/question routing branches are asserted in the rust embedded tests)',
  },

  // --- mcp domain ----------------------------------------------------------
  // No MCP manager is wired into the mock-llm engine, so list/state reads
  // return their empty deterministic shapes. Retired methods and the
  // no-manager branches answer with an explicit `not_implemented` error
  // (asserted in `error` mode across the NAPI boundary since M-2); methods
  // that would spawn or mutate a real MCP manager remain skipped.
  // The P1 config surface reads the real config layers (the mock engine
  // runs without an injected home, so the user layer resolves to the
  // machine's real `~/.mirri-code/mcp.json`); its writes stay skipped —
  // they are asserted against injected temp layers in the rust embedded
  // tests. The one write exercised here is the F-3 conflict rejection
  // (`mcp.config.set` in error mode): the stale `expectedRevision` never
  // matches, so the engine answers `mcp.config.conflict` without touching
  // any file.

  {
    method: 'mcp.config.list',
    mode: 'schema',
    note: 'P1 config surface: the merged layered view — the klient mcpConfigService.list entry schema (config verbatim per A5)',
  },
  {
    method: 'mcp.config.preview',
    mode: 'schema',
    params: {
      config: {
        command: 'echo',
        args: ['${MIRRI_CONTRACT_E2E_PREVIEW_UNDEF}'],
      },
    },
    note: 'F-4 resolved-value preview: the adjudication A5 sole sanctioned outlet for resolved values — a direct unsaved config answers the expanded values plus the undefined-variable names (klient mcpConfigPreviewResultSchema), and nothing is cached, emitted, logged, or persisted',
  },
  {
    method: 'mcp.config.remove',
    mode: 'skip',
    reason:
      'it writes the addressed config layer — the mock engine runs without an injected home, so a user-scope call would edit the machine\'s real ~/.mirri-code/mcp.json (the write path is asserted in the rust embedded tests)',
  },
  {
    method: 'mcp.config.setToolFilter',
    mode: 'skip',
    reason:
      'it writes a tool filter into the layer owning the server — the mock engine runs without an injected home, so the write would target the machine\'s real config files (the patch path is asserted in the rust embedded tests)',
  },
  {
    method: 'mcp.config.test',
    mode: 'schema',
    params: { config: { command: 'definitely-not-a-real-mcp-server-xyz', args: [] } },
    note: 'P2 pool surface: the fresh-connect test runs on a scratch manager independent of the engine pools — a bogus command answers the error-result shape (klient mcpTestResultSchema), and no connection leaks into mcp.list',
  },
  {
    method: 'mcp.config.set',
    mode: 'error',
    params: {
      scope: 'user',
      name: 'never-stored-xyz',
      expectedRevision: '0000000000000000',
      config: { transport: 'stdio', command: 'node' },
    },
    expectDomainCode: 'mcp.config.conflict',
    note: 'F-3 optimistic lock: the expectedRevision can never match the addressed layer\'s current revision in the mock engine, so the write answers the mcp.config.conflict domain error before any file is touched (the stale-revision flows are asserted in the rust embedded tests)',
  },
  {
    method: 'mcp.disable',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v1-era session-level toggle); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.disableTool',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v1-era session-level toggle); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.enable',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v1-era session-level toggle); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.enableTool',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v1-era session-level toggle); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.connect',
    mode: 'skip',
    reason: 'it spawns a real MCP server connection (external process)',
  },
  {
    method: 'mcp.global.create',
    mode: 'skip',
    reason: 'it spawns a real MCP server connection (external process)',
  },
  {
    method: 'mcp.global.delete',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v2 has no global MCP RPC); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.disableTool',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'no MCP manager is wired into the mock-llm engine, so the no-manager branch answers an explicit not_implemented error (M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.enableTool',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'no MCP manager is wired into the mock-llm engine, so the no-manager branch answers an explicit not_implemented error (M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.list',
    mode: 'snapshot',
    note: 'read-only: registered MCP servers in the McpServerInfo[] wire shape',
  },
  {
    method: 'mcp.global.reload',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v2 has no global MCP RPC); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.testConnect',
    mode: 'skip',
    reason: 'it performs a live connection attempt to an external server',
  },
  {
    method: 'mcp.global.toggle',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v2 has no global MCP RPC); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.global.toggleState',
    mode: 'snapshot',
    note: 'read-only: disabled MCP servers/tools state',
  },
  {
    method: 'mcp.global.tools',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare GlobalMcpToolInfo[] array',
  },
  {
    method: 'mcp.global.update',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'retired in the RPC contract (v2 has no global MCP RPC); the engine answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.list',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare McpServerInfo[] array',
  },
  {
    method: 'mcp.reconnect',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'no MCP manager is wired into the mock-llm engine, so the no-manager branch answers an explicit not_implemented error (M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'mcp.startupMetrics',
    mode: 'snapshot',
    note: 'read-only: MCP startup metrics (static object without a manager)',
  },
  {
    method: 'mcp.status',
    mode: 'schema',
    note: 'P2 pool surface: the no-workspace global view — the mock engine attaches no pools, so the answer is the empty array (klient mcpServerInfo[] shape)',
  },
  {
    method: 'mcp.sync',
    mode: 'schema',
    note: 'F-6 idempotent reconcile: the mock engine attaches no pools, so the kernel applies nothing — the all-zero summary (klient mcpSyncResultSchema), the steady-state answer of an already-converged config',
  },
  {
    method: 'mcp.tools',
    mode: 'schema',
    params: { name: 'demo' },
    note: 'M1 tool view: the single-server listing with per-tool filter state — the mock engine attaches no pools, so an unknown name answers the explicit connected=false shape with an empty tools array (klient mcpToolsResultSchema)',
  },
  {
    method: 'mcp.toggleState',
    mode: 'snapshot',
    note: 'read-only: disabled MCP servers/tools state',
  },

  // --- message domain --------------------------------------------------------

  {
    method: 'message.abort',
    mode: 'skip',
    reason: 'it cancels in-flight fire-mode turns (requires an active turn)',
  },
  {
    method: 'message.send',
    mode: 'skip',
    reason:
      'it runs a full agent turn (LLM traffic, streaming events, persistent history); covered by rust-engine-e2e.test.ts instead',
  },
  {
    method: 'message.steer',
    mode: 'skip',
    reason: 'it steers an in-flight turn (requires an active turn)',
  },

  // --- model domain -----------------------------------------------------------

  {
    method: 'model.list',
    mode: 'snapshot',
    note: 'read-only: configured model catalog (default config in the mock engine)',
  },
  {
    method: 'model.listProviders',
    mode: 'snapshot',
    note: 'read-only: configured provider list',
  },

  // --- plan domain --------------------------------------------------------------

  {
    method: 'plan.cancel',
    mode: 'skip',
    reason: 'it mutates the session’s plan state',
  },
  {
    method: 'plan.clear',
    mode: 'skip',
    reason: 'it mutates the session’s plan state',
  },
  {
    method: 'plan.enter',
    mode: 'skip',
    reason: 'it mutates the session’s plan state',
  },
  {
    method: 'plan.status',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the session’s plan-mode state',
  },

  // --- plugin domain ---------------------------------------------------------------
  // No plugin manager is wired into the mock-llm engine: list/commands read
  // their empty deterministic shapes, while install/remove/enable methods
  // mutate plugin state or hit the network and are skipped.

  {
    method: 'plugin.activateCommand',
    mode: 'skip',
    reason:
      'it executes a plugin command (requires an installed plugin with commands)',
  },
  {
    method: 'plugin.checkUpdates',
    mode: 'schema',
    note: 'read-only: the klient pluginUpdateStatusSchema[] contract — github-sourced update statuses (empty array under the mock engine, which attaches no plugin manager; the per-plugin resolution branches are asserted in the rust embedded/plugin-host tests with injected stub resolvers)',
  },
  {
    method: 'plugin.commands',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare PluginCommandDef[] array',
  },
  {
    method: 'plugin.info',
    mode: 'skip',
    reason:
      'it requires an installed plugin id to inspect (the handler 404s otherwise)',
  },
  {
    method: 'plugin.install',
    mode: 'skip',
    reason: 'it installs a plugin from an external source (network/filesystem)',
  },
  {
    method: 'plugin.list',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare PluginInfo[] array',
  },
  {
    method: 'plugin.reload',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a ReloadSummary object {added, removed, errors}',
  },
  {
    method: 'plugin.remove',
    mode: 'skip',
    reason: 'it destructively removes an installed plugin',
  },
  {
    method: 'plugin.setEnabled',
    mode: 'skip',
    reason: 'it mutates plugin enablement state',
  },
  {
    method: 'plugin.setMcpServerEnabled',
    mode: 'skip',
    reason: 'it mutates plugin MCP server enablement',
  },

  // --- question domain ----------------------------------------------------

  {
    method: 'question.answer',
    mode: 'skip',
    reason:
      'it answers an in-flight pending question by request id (requires a blocked AskUserQuestion; the B4-a tolerant answer union is asserted in the rust embedded tests)',
  },
  {
    method: 'question.dismiss',
    mode: 'skip',
    reason:
      'it dismisses an in-flight pending question by request id (requires a blocked AskUserQuestion; the dismissed-tool-error path is asserted in the rust embedded tests)',
  },
  {
    method: 'question.listPending',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the klient questionRequest[] bare array (empty — no question is pending during the sweep)',
  },

  // --- session domain -----------------------------------------------------------------
  // Reads snapshot against the runner-created setup session; lifecycle
  // mutations (create/close/archive/fork/rename/…) are skipped because they
  // change persistent state or need pre-existing state.

  {
    method: 'session.addAdditionalDir',
    mode: 'skip',
    reason:
      'it resolves and persists a workspace additional directory (mutating: it writes <projectRoot>/.mirri-code/local.toml — the TS semantics, P11a F-P8-2; the resolution/persistence wire shape is asserted in the rust embedded tests)',
  },
  {
    method: 'session.archive',
    mode: 'skip',
    reason: 'it closes/archives the addressed session (mutating)',
  },
  {
    method: 'session.beginCompaction',
    mode: 'skip',
    reason: 'it launches a background compaction task (mutating)',
  },
  {
    method: 'session.cancelCompaction',
    mode: 'skip',
    reason: 'it mutates compaction state',
  },
  {
    method: 'session.clearContext',
    mode: 'skip',
    reason: 'it destructively wipes the agent’s context memory',
  },
  {
    method: 'session.close',
    mode: 'skip',
    reason: 'it closes the addressed session (mutating)',
  },
  {
    method: 'session.countActive',
    mode: 'snapshot',
    note: 'read-only: the bare live-session count (the engine binds one workspace; an omitted filter counts it)',
  },
  {
    method: 'session.create',
    mode: 'skip',
    reason:
      'it creates new session state on every call (mutating); its wire view is already asserted by rust-engine-e2e.test.ts and by the runner setup',
  },
  {
    method: 'session.createChild',
    mode: 'snapshot',
    params: { sourceSessionId: SETUP_SESSION_ID },
    note: 'self-contained mutation: the klient sessionLifecycle handle shape {id, kind} of the child forked from the setup session',
  },
  {
    method: 'session.export',
    mode: 'skip',
    reason: 'it writes a zip archive under the user’s export root (filesystem side effect)',
  },
  {
    method: 'session.fork',
    mode: 'skip',
    reason: 'it creates a new session (mutating)',
  },
  {
    method: 'session.generateAgentsMd',
    mode: 'error',
    expectDomainCode: 'not_implemented',
    expectCode: -32603,
    note: 'the embedded engine has no subagent-spawn facility to author AGENTS.md; it answers with an explicit not_implemented error (P9 cleared, M-2 asserted across the NAPI boundary)',
  },
  {
    method: 'session.get',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the session summary view plus the agent list (the same wire shape mirri-server serves)',
  },
  {
    method: 'session.getHistory',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the addressed agent\'s full context history as a bare ContextMessage array (the P5d ACP session/load replay read face; empty for the fresh setup session)',
  },
  {
    method: 'session.getMeta',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: the session’s metadata (id, title, agents, tags)',
  },
  {
    method: 'session.handlePrintMainTurnCompleted',
    mode: 'snapshot',
    params: { sessionId: SETUP_SESSION_ID },
    note: 'read-only decision on the fresh setup session (no background tasks → steer resolves to finish): the B1 T8 print-mode RPC answers { decision } deterministically',
  },
  {
    method: 'session.list',
    mode: 'snapshot',
    note: 'read-only: the W5 page envelope {items, has_more} — the kap-server list route’s filter face (multi-workspace merged listing, keyset cursors, exclude_empty); the snapshot pins the envelope shape',
  },
  {
    method: 'session.reload',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only for a live session: the embedded engine returns the wire view without reloading',
  },
  {
    method: 'session.registerAgent',
    mode: 'snapshot',
    params: { sessionId: SETUP_SESSION_ID, agentId: 'contract-e2e-agent' },
    note: 'self-contained mutation: the klient sessionMetadata noResult (JSON null) after registering a second agent into the setup session',
  },
  {
    method: 'session.rename',
    mode: 'skip',
    reason: 'it mutates the session’s metadata',
  },
  {
    method: 'session.restore',
    mode: 'error',
    params: { session_id: SETUP_SESSION_ID },
    expectDomainCode: 'session.already_exists',
    expectCode: -32603,
    note: 'the B1 EMB backfill: archive is close-with-files-kept, so restore is the resume path — a live (not archived) session answers session.already_exists (the happy path is exercised cross-backend in the rust test harness)',
  },
  {
    method: 'session.resume',
    mode: 'skip',
    reason:
      'it replays a persisted closed session (requires pre-existing closed-session state the runner does not create)',
  },
  {
    method: 'session.search',
    mode: 'snapshot',
    params: { query: 'no-such-term-in-the-fresh-setup-session' },
    note: 'read-only: the P10 global literal search read face (title + user/assistant text, NFKC + case folded, cursor paginated) — the empty hit page for a query the fresh setup session does not match',
  },
  {
    method: 'session.undo',
    mode: 'skip',
    reason: 'it destructively removes agent history messages',
  },
  {
    method: 'session.updateMeta',
    mode: 'skip',
    reason: 'it mutates the session’s metadata',
  },
  {
    method: 'session.waitForBackgroundTasksOnPrint',
    mode: 'snapshot',
    params: { sessionId: SETUP_SESSION_ID },
    note: 'the B1 T8 print-drain RPC (was not_implemented pre-B1): on the fresh setup session (no background tasks) it settles immediately with { waited, remaining } — the bounded-drain wait face of the print mode',
  },
  {
    method: 'session.warnings',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: '855f57ef regression group: TS wire contract is a bare SessionWarning[] array',
  },

  // --- skill domain --------------------------------------------------------------------

  {
    method: 'skill.list',
    mode: 'schema',
    note: '855f57ef regression group: TS wire contract is a bare SkillSummary[] array (the TUI startup crash)',
  },

  {
    method: 'skill.getContent',
    mode: 'error',
    expectDomainCode: 'skill.not_found',
    expectCode: -32001,
    params: { name: 'no-such-skill-in-the-fresh-setup' },
    note: 'B8 management read face: unknown names answer skill.not_found (the {name, content} happy-path shape stays pinned by the rust embedded tests; error/skip mode is the only self-consistent shape here — the METHODS_WITHOUT_KLIENT_SCHEMA set only admits schema-mode methods)',
  },

  {
    method: 'skill.setEnabled',
    mode: 'skip',
    reason: 'it mutates the user’s config.toml disabled_skills set',
  },

  {
    method: 'skill.reload',
    mode: 'schema',
    params: {},
    note: 'ADR-010 Task 6 manual catalog-reload face: {workspaces: [{workspace_id, added, removed, updated, watching}]} — the per-workspace diff counts of one serialized rescan sweep plus the watcher status; idempotent (a second call reports all-zero buckets)',
  },

  // --- collaboration domain (mode gate) --------------------------------------------------

  {
    method: 'collaboration.enter',
    mode: 'skip',
    reason: 'it mutates the session’s collaboration mode',
  },
  {
    method: 'collaboration.exit',
    mode: 'skip',
    reason: 'it mutates the session’s collaboration mode',
  },

  // --- swarm domain ----------------------------------------------------------------------

  {
    method: 'swarm.enter',
    mode: 'skip',
    reason: 'it mutates the session’s swarm mode',
  },
  {
    method: 'swarm.exit',
    mode: 'skip',
    reason: 'it mutates the session’s swarm mode',
  },
  {
    method: 'swarm.get',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID },
    note: 'read-only: whether swarm mode is active (plain boolean wire type)',
  },

  // --- task domain -------------------------------------------------------------------------

  {
    method: 'task.cancel',
    mode: 'skip',
    reason: 'it stops a background task (requires a live task id)',
  },
  {
    method: 'task.detach',
    mode: 'skip',
    reason:
      'it converts a running foreground shell command into a background task (requires a live task id; the detach path and the unknown-id null answer are asserted in the rust embedded tests — P3i landed the shell runner)',
  },
  {
    method: 'task.list',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: '855f57ef regression group: TS wire contract is a bare AgentTask[] array (M3-c batch 8 aligned the element shape on both backends)',
  },
  {
    method: 'task.output',
    mode: 'skip',
    reason: 'it reads a background task’s output (requires a live task id)',
  },
  {
    method: 'task.stop',
    mode: 'skip',
    reason:
      'it stops a background task (mutating; requires a live task id — the stopped-task wire shape and the unknown-id branch are asserted in the rust embedded tests)',
  },
  {
    method: 'task.listWorkspace',
    mode: 'skip',
    reason:
      'it aggregates every live session\u2019s tasks across the registered workspaces (the element shape — the direct AgentTask serialization task.list serves — and the scope/filter/empty branches are asserted in the rust embedded b7 tests and the cross-backend contract suite)',
  },

  // --- tool domain ---------------------------------------------------------------------------

  {
    method: 'tool.list',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the addressed agent’s registered tool definitions (bare array)',
  },
  {
    method: 'tool.listUserTools',
    mode: 'snapshot',
    params: { session_id: SETUP_SESSION_ID, agent_id: MAIN_AGENT_ID },
    note: 'read-only: the T2 user-tool management read face — every registered user tool with its registration payload and current visibility (bare array; empty under the sweep — the runner registers no user tools)',
  },
  {
    method: 'tool.setUserToolEnabled',
    mode: 'skip',
    reason:
      'it toggles a registered user tool’s visibility (mutating; the round trip — list → disable → resume-visible → re-enable — is asserted in the rust embedded tests, plans/07 T2)',
  },

  // --- workspace domain -------------------------------------------------------------------------
  // B4-b: the four SRV-wire mirrors plus the two domain-shaped gap rows.
  // All six shapes drift from the klient workspacesContract (id/cwd list
  // elements, RFC3339 string timestamps instead of epoch ms, extra
  // sessionCount), so they are snapshot-locked rather than klient-schema
  // validated — the drift is recorded in the authoritative TSV row notes.

  {
    method: 'workspace.createOrTouch',
    mode: 'skip',
    reason:
      'mutating: the real registry semantics (root validation, lexical normalization, D8 merge-write, register+build, lifecycle event) are asserted in the rust embedded tests and the W2 contract path',
  },
  {
    method: 'workspace.delete',
    mode: 'skip',
    reason:
      'mutating (W6 face): params {workspaceId, mode: "purge" | "remove"} (mode required) → {ok, mode, workspaceId, deletedSessions}; the mock engine is a single-workspace poisoned registry whose bound workspace is the pinned default — delete refuses it, so no result can be produced there without rebuilding the mock engine on `assemble`. The wire face and the rust-side purge/remove/tombstone semantics are pinned in the rust embedded tests and the cross-backend contract harness.',
  },
  {
    method: 'workspace.get',
    mode: 'snapshot',
    params: { workspaceId: 'no-such-ws' },
    note: 'read-only: the unknown-id miss is JSON null (the klient maybe semantics); the domain shape keeps RFC3339 string timestamps + sessionCount',
  },
  {
    method: 'workspace.handlerFor',
    mode: 'snapshot',
    params: { root: '/anywhere' },
    note: 'read-only: an unknown reference is JSON null (the klient maybe semantics); a registered reference resolves to the {id, kind} scope handle (klient handleWireSchema loose shape)',
  },
  {
    method: 'workspace.list',
    mode: 'snapshot',
    note: 'read-only: the bare WorkspaceView[] array — the W2 registry listing, recency-ordered with per-workspace sessionCount (the full info shape ca148592 serves — {id, root, name, createdAt, lastOpenedAt, sessionCount}; klient WorkspaceInfo field set is an M3 parked topic)',
  },
  {
    method: 'workspace.update',
    mode: 'snapshot',
    params: { workspaceId: SETUP_WORKSPACE_ID, name: 'contract-e2e' },
    note: 'mutating but reversible: the W6 real rename — the renamed WorkspaceView (id/root/name/createdAt/lastOpenedAt/sessionCount/trusted, RFC3339 string timestamps; W8 added the live trust verdict) written through the index + the D8 merged registry write under the isolated temp home',
  },
  {
    method: 'workspace.setTrust',
    mode: 'snapshot',
    params: { workspaceId: SETUP_WORKSPACE_ID, trusted: true },
    note: 'benign no-op on the sweep engine: the W8 trust management face answers the TS v2 trust face {trusted} (the kap-server workspaceTrustResponseSchema) — the setup workspace is the pinned default (trusted by construction), so the grant is the idempotent already-in-state arm that writes nothing; the real grant/revoke linkage (project-layer connect / immediate disconnect + the AGENTS.md gate) is asserted in the rust embedded tests and the cross-backend contract harness',
  },

  // --- env domain (B5-a: the klient bootstrapService zero-arg reads) ------

  {
    method: 'env.arch',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the rust std::env::consts::ARCH spelling (live host fact)',
  },
  {
    method: 'env.blobsDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the klient-layout <mirri home>/blobs derivation (live)',
  },
  {
    method: 'env.cacheDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the klient-layout <mirri home>/cache derivation (live)',
  },
  {
    method: 'env.clientIdentity',
    mode: 'schema',
    note: 'read-only: the klient MirriHostIdentity camelCase shape — the EMB engine states its own identity (no host channel; TSV note records the drift); userAgentSuffix omitted (no honest source)',
  },
  {
    method: 'env.clientVersion',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the engine crate semver (kept alongside clientIdentity on the wire)',
  },
  {
    method: 'env.configPath',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the loaded config.toml path, or the klient-layout <home>/config.toml when the config fell back to defaults',
  },
  {
    method: 'env.cwd',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the live process working directory',
  },
  {
    method: 'env.homeDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the Mirri home the composition root resolved (injected home / MIRRICODE_HOME / $HOME/.mirri-code)',
  },
  {
    method: 'env.logsDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the klient-layout <mirri home>/logs derivation (live)',
  },
  {
    method: 'env.osHomeDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the OS user home ($HOME / $USERPROFILE)',
  },
  {
    method: 'env.platform',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the rust std::env::consts::OS spelling (macos where the TS host says darwin; TSV note records the drift)',
  },
  {
    method: 'env.sessionsDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the session manager\'s actual persistence root (the honest read; injected home <home>/sessions, else the workspace-local <root>/.mirri/sessions — TSV note records the divergence)',
  },
  {
    method: 'env.storeDir',
    mode: 'schema',
    note: 'read-only: the klient bootstrapService stringRead — the klient-layout <mirri home>/store derivation (live)',
  },

  // --- flag domain (B5-a: the klient flagService reads) ---------------------

  {
    method: 'flag.enabled',
    mode: 'schema',
    params: { id: 'never-registered' },
    note: 'read-only: the klient flagService boolean — unregistered ids answer false under the empty EMB registry (the env chain never promotes them); the registered-flag env-override chain is asserted in the rust embedded tests',
  },
  {
    method: 'flag.enabledIds',
    mode: 'schema',
    note: 'read-only: the klient flagService string[] — the real registered set (empty under the mock engine, which registers no flags)',
  },
  {
    method: 'flag.explain',
    mode: 'schema',
    params: { id: 'never-registered' },
    note: 'read-only: the klient maybe(experimentalFeatureStateSchema) — an unknown id is JSON null (the klient maybe semantics); the rich-shape fields (env-override source chain) are asserted in the rust embedded tests',
  },
  {
    method: 'flag.explainAll',
    mode: 'schema',
    note: 'read-only: the klient experimentalFeatureStateSchema[] — the real registered set (expected-vacuous under the mock engine, which registers no flags)',
  },
  {
    method: 'flag.snapshot',
    mode: 'schema',
    note: 'read-only: the klient flagService record (id → bool) — the real registered set ({} under the mock engine, which registers no flags)',
  },

  // --- fs domain (B5-a: the klient hostFolderBrowser) ------------------------

  {
    method: 'fs.browse',
    mode: 'schema',
    note: 'read-only: the klient fsBrowseResponseSchema — the directories under the OS user home (the optional path defaults to home); entries list directories only (the is_dir literal), files are not part of the wire face',
  },
  {
    method: 'fs.home',
    mode: 'schema',
    note: 'read-only: the klient fsHomeResponseSchema — the OS user home plus the honestly empty recent_roots (no recent-workspace history store; TSV note records the adjudication)',
  },

  // --- providerDiscovery domain (B5-a + P6 B4a) ----------------------------

  {
    method: 'providerDiscovery.refreshProviderModels',
    mode: 'skip',
    reason:
      'it is the remote provider-model refresh (side-effect class; the current EMB answer is the honest empty success {changed: [], unchanged: [], failed: []} because no remote discovery capability is wired — the three-part shape and the invalid-scope branch are asserted in the rust embedded tests)',
  },
  {
    method: 'providerDiscovery.listCatalog',
    mode: 'schema',
    note: 'read-only: the klient providerCatalogEntrySchema browse rows — models.dev directory facts from the in-memory/disk cache (zero network); the empty array is the honest not-yet-refreshed signal under the mock engine (no catalog loaded); needs_base_url mirrors the TS resolveModelsDevImport derivation, wire_type/env_key/context_length omitted when undeclared',
  },
  {
    method: 'providerDiscovery.importCatalog',
    mode: 'error',
    params: { catalogId: 'nonexistent-directory-entry' },
    expectDomainCode: 'provider.not_found',
    expectCode: -32001,
    note: 'write: the models.dev directory import — under the mock engine no directory is loaded (no refresh ran), so the import answers the provider.not_found domain error instead of fetching implicitly; the happy path (provider record + minimal aliases in one config transaction, provider.already_exists on a taken id) is asserted in the rust embedded tests',
  },

  // --- config domain (B5-b: the klient configService section methods) ------

  {
    method: 'config.getAll',
    mode: 'schema',
    note: 'read-only: the klient configService record of every top-level config section (camelCase; the union of the env-applied wire projection and the raw document\'s unmodeled sections — TSV note records the no-registry/no-memory DRIFT)',
  },
  {
    method: 'config.inspect',
    mode: 'schema',
    params: { key: 'thinking' },
    note: 'read-only: the klient configInspectValueSchema layered snapshot — only the value/userValue layers exist on the rust engine (no section registry, no memory layer); apiKey values redacted like the Settled config.get wire',
  },
  {
    method: 'config.reload',
    mode: 'skip',
    reason:
      'it re-reads config.toml from disk into the engine\'s in-memory snapshot (side-effect class; the noResult shape and the reload visibility are asserted in the rust embedded tests)',
  },
  {
    method: 'config.replace',
    mode: 'skip',
    reason:
      'it replaces (or clears) a config.toml section and persists (side-effect class; the null-means-clear semantics, the invalid memory-target rejection, and the invalid-section validation are asserted in the rust embedded tests)',
  },
  {
    method: 'config.replaceSections',
    mode: 'skip',
    reason:
      'it batch-replaces config.toml sections and persists (side-effect class; the getAll-visibility round trip is asserted in the rust embedded tests)',
  },

  // --- model domain (B5-b: klient modelService records + modelResolver) ----

  {
    method: 'model.get',
    mode: 'schema',
    params: { id: 'no-such-model-alias' },
    note: 'read-only: the klient maybe(modelConfigSchema) — an unknown id is JSON null (the klient maybe semantics); the passthrough-key round trip and the alias-trio rejection DRIFT are asserted in the rust embedded tests',
  },
  {
    method: 'model.set',
    mode: 'skip',
    reason:
      'it replaces a [models.<id>] record in config.toml (side-effect class; the full-record replace with passthrough keys is asserted in the rust embedded tests)',
  },
  {
    method: 'model.delete',
    mode: 'skip',
    reason:
      'it removes a [models.<id>] record from config.toml (side-effect class; the defaultModel clearing is asserted in the rust embedded tests)',
  },
  {
    method: 'model.getProvider',
    mode: 'error',
    expectDomainCode: 'provider.not_found',
    expectCode: -32001,
    params: { providerId: 'no-such-provider' },
    note: 'the TS ModelCatalogErrors code for an unknown provider id (provider.no-such-provider does not exist) — the connected-provider catalog-item shape is asserted in the rust embedded tests',
  },
  {
    method: 'model.setDefaultModel',
    mode: 'skip',
    reason:
      'it persists the global defaultModel pointer to config.toml (side-effect class; the setDefaultModelResponseSchema shape and the model.not_found branch are asserted in the rust embedded tests)',
  },
  {
    method: 'model.generate',
    mode: 'error',
    expectDomainCode: 'model.not_found',
    expectCode: -32001,
    params: {
      modelId: 'no-such-model-alias',
      systemPrompt: 'contract-e2e',
      messages: [{ role: 'user', content: 'hi' }],
    },
    note: 'the only streaming contract (klient modelResolver.generate) — under the mock engine no config.toml holds the alias, so the sweep exercises the unknown-model branch; the part→usage→finish chunk sequence, the requestId echo, and the aggregated response shape (requestId/finishReason/usage/message) are asserted in the rust embedded tests (wiremock SSE), and chunk delivery stays best-effort (the aggregated response is authoritative)',
  },

  // --- provider domain (B5-b: the klient providerService CRUD) -------------

  {
    method: 'provider.get',
    mode: 'schema',
    params: { name: 'no-such-provider' },
    note: 'read-only: the klient maybe(providerConfigSchema) — an unknown id is JSON null (the klient maybe semantics)',
  },
  {
    method: 'provider.list',
    mode: 'schema',
    note: 'read-only: the klient record(id → providerConfigSchema) over the engine\'s [providers] registry (empty under the mock engine when no config.toml holds providers; apiKey values redacted like the Settled config.get wire)',
  },
  {
    method: 'provider.set',
    mode: 'skip',
    reason:
      'it replaces a [providers.<id>] record in config.toml (side-effect class; the round trip is asserted in the rust embedded tests)',
  },
  {
    method: 'provider.delete',
    mode: 'skip',
    reason:
      'it removes a [providers.<id>] record from config.toml (side-effect class; the defaultProvider clearing is asserted in the rust embedded tests)',
  },
  // --- session-retention domain (Task 9: the management handlers) --
  // The six retention management methods serve their real shapes now
  // (the handlers land in Task 9 over the session index, the retention
  // engine, and the unified delete executor).
  {
    method: 'retention.list',
    mode: 'schema',
    params: { include: 'archived' },
    note: 'session-retention management view (rust-first wire): the archived/compressed view rows over the session index + disk shape verdicts (schema-only: the sweep home has no archived sessions, so the items list is empty — the shape guard is the envelope)',
  },
  {
    method: 'retention.stats',
    mode: 'schema',
    params: {},
    note: 'session-retention management counters: the header counters plus the four per-tier delete estimates (schema-only: an empty home answers zero counts)',
  },
  {
    method: 'retention.compressNow',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: 'session-retention immediate compression over the core compress path (schema-only: the sweep session is not archived, so the in-lock revalidation answers the benign no-action ok)',
  },
  {
    method: 'retention.delete',
    mode: 'schema',
    params: { session_id: SETUP_SESSION_ID },
    note: 'session-retention single-session delete through the unified executor (schema-only: deleting the live sweep session is not exercised — the shape guard is the ok/reclaimed_bytes envelope; the destructive path is covered by the rust embedded tests)',
  },
  {
    method: 'retention.deleteTiered',
    mode: 'schema',
    params: { older_than_days: 90 },
    note: 'session-retention tier-button batch delete with per-row outcome reporting (schema-only: an empty compressed set answers the empty batch envelope)',
  },
  {
    method: 'retention.getPolicy',
    mode: 'schema',
    params: {},
    note: 'rust-first wire (no klient contract): the resolved [retention] knobs {enabled, archive_grace_days, interval_hours} — a config snapshot read, final shape since the wire registration',
  },
];
