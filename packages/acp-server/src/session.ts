/**
 * ACP session (v2) — drives a single main agent over one ACP `sessionId`,
 * through the `Klient` facade. `start.ts` creates the klient over the
 * in-memory transport; everything below goes through facade calls and typed
 * klient events, so swapping the transport (http / ipc) requires no change
 * here.
 *
 * `prompt` submits the user input via `agent.prompt(...)` and translates the
 * agent's scoped event stream — subscribed once per session in `init()`,
 * before the first prompt — into ACP `session/update` notifications via the
 * helpers in `./events-map`. The promise settles on the `turn.ended` event; a
 * submission that launches no turn (busy / hook-blocked / not runnable)
 * settles gracefully with `end_turn`, mirroring the engine's `PromptHandle`
 * behavior.
 *
 * KLIENT GAPS (all reported; each marked `KLIENT-GAP` inline):
 *  - no session skill-listing surface (`session.skills`) → engine skills are
 *    never advertised as slash commands; only host-provided skill aliases
 *    reach the client palette (see `resolveCommands`).
 *  - the `activateSkill` RPC fires the skill turn without reporting its turn
 *    id (`IAgentSkillService.activate`'s `Turn` is discarded by the wire, and
 *    the facade types the RPC like `prompt`), so a skill launch adopts the
 *    next turn-scoped event instead of keying on the returned id (see
 *    `driveSkillActivation` / `dispatchTurnEvent`).
 *  - no `agent.compact` / `agent.getMcpServers` → `/compact` and `/mcp`
 *    answer with an explanatory notice (see `./builtin-commands`).
 *  - no `tool.call.delta` / `tool.progress` / `compaction.*` agent events →
 *    streaming-args deltas and compaction progress never surface as ACP
 *    notifications (the tool card is created on `tool.call.started` with the
 *    full stringified args and finalized at `tool.result`).
 */

import type {
  AvailableCommand,
  ContentBlock,
  PromptResponse,
  SessionConfigOption,
  SessionModeState,
  SessionNotification,
  ToolCallLocation,
} from '@agentclientprotocol/sdk';
import { RequestError } from '@agentclientprotocol/sdk';
import type { ContextMessage, SkillSummary } from '@mirri-ai/agent-core-v2';
import type {
  AgentEventPayloads,
  AgentHandle,
  ContentPart,
  IDisposable,
  Klient,
  PromptLaunchResult,
  SessionEventPayloads,
  SessionHandle,
} from '@mirri-ai/klient';
import type {
  ToolCallStartedEvent,
  ToolInputDisplay,
  ToolResultEvent,
} from '@mirri-ai/protocol';

import type { AcpClient } from './acp-client';
import type { AcpTerminalCreatedEvent, IAcpConnection } from './acp-fs';
import {
  ACP_BUILTIN_SLASH_COMMAND_NAMES,
  ACP_BUILTIN_SLASH_COMMANDS,
  type AcpBuiltinSlashCommandName,
  runBuiltinSlashCommand,
} from './builtin-commands';
import { buildSessionConfigOptions } from './config-options';
import { acpBlocksToContentParts, compressPromptImageParts } from './convert';
import {
  acpToolCallId,
  assistantDeltaToSessionUpdate,
  availableCommandsUpdateNotification,
  configOptionUpdateNotification,
  currentModeUpdateNotification,
  planFromDisplayBlock,
  sessionInfoUpdateNotification,
  thinkingDeltaToSessionUpdate,
  toolCallLocations,
  toolCallStartToSessionUpdate,
  toolResultToSessionUpdate,
  turnEndReasonToStopReason,
  usageUpdateNotification,
  isAuthError,
} from './events-map';
import { AcpInteractionBridge } from './interaction-bridge';
import { log } from './log';
import { projectModelCatalog } from './model-catalog';
import { ACP_MODES, type AcpModeId, acpModeToToggles, DEFAULT_MODE_ID } from './modes';
import { projectHistoryToSessionUpdates } from './replay';
import { detectSlashIntent, buildAcpSkillSlashCommands } from './slash';

/** Leading text of the first text block, if any (used for slash detection). */
function leadingText(blocks: readonly ContentBlock[]): string | undefined {
  const first = blocks[0];
  if (first !== undefined && first.type === 'text') return first.text;
  return undefined;
}

/**
 * The engine's wire code for "another turn is active". A plain
 * `agent.prompt` while busy is QUEUED by the engine (the RPC returns
 * `undefined`, indistinguishable from a hook-blocked launch), so this code
 * only reaches us from `agent.activateSkill`, which rejects instead.
 */
const TURN_AGENT_BUSY_CODE = 'turn.agent_busy';

/**
 * Map a prompt-launch rejection (from `agent.prompt` / `agent.activateSkill`)
 * to the JSON-RPC error the client sees.
 *
 *  - Auth-coded failures surface as `auth_required` so the client drives its
 *    re-auth flow (same mapping as the `turn.ended` auth path).
 *  - `turn.agent_busy` maps to `invalidRequest` (-32600), matching the legacy
 *    adapter's busy-prompt semantics.
 *  - Everything else becomes a fixed-message `internalError` (-32603): the
 *    raw engine message and stack are logged server-side but NEVER cross the
 *    wire, so internal details cannot leak into the JSON-RPC channel.
 */
export function mapPromptLaunchError(error: unknown, sessionId: string): RequestError {
  const code = (error as { readonly code?: unknown } | null | undefined)?.code;
  const message = error instanceof Error ? error.message : String(error);
  if (typeof code === 'string' && isAuthError({ code })) {
    log.warn('acp: prompt launch rejected with an auth error; mapping to auth_required', {
      sessionId,
      error: message,
    });
    return RequestError.authRequired(undefined, message);
  }
  if (code === TURN_AGENT_BUSY_CODE) {
    log.warn('acp: prompt rejected because another turn is active', { sessionId });
    return RequestError.invalidRequest({ code }, message);
  }
  log.error('acp: prompt launch failed', {
    sessionId,
    error: error instanceof Error ? { message: error.message, stack: error.stack } : String(error),
  });
  return RequestError.internalError(undefined, 'session prompt failed');
}

/** Per-turn settlement state for one in-flight `session/prompt`. */
interface HostSlashCommandsSnapshot {
  readonly commands: ReadonlyArray<AvailableCommand>;
  readonly skillCommandMap?: ReadonlyMap<string, string>;
}

interface ResolvedCommands {
  readonly commands: AvailableCommand[];
  readonly skillCommandMap: ReadonlyMap<string, string>;
}

interface TurnDriver {
  resolve(response: PromptResponse): void;
  reject(error: unknown): void;
  /**
   * Learned once the launch call (`agent.prompt` / `agent.activateSkill`)
   * resolves. Events carry a `turnId`, but until this is set no turn-scoped
   * event can be attributed to this prompt — and the turn may START (with a
   * fast model even END) before the launch round-trip delivers the id. Such
   * events are buffered in {@link early} instead of being dropped.
   */
  turnId?: number;
  /**
   * Adopt the first turn-scoped event's id instead of keying on the launch
   * result. Set for skill activations: the engine's `activateSkill` RPC
   * fires the turn without reporting its id (the RPC discards
   * `IAgentSkillService.activate`'s `Turn` — see `driveSkillActivation`).
   * Only one turn can be active (`assertNoActiveTurn`), so the first
   * turn-scoped event while this flag is set necessarily belongs to this
   * launch, and events buffered in {@link early} before the adoption
   * belong to the same (only) turn and replay on adoption.
   */
  adoptNextTurn?: boolean;
  settled: boolean;
  /**
   * Set when `cancel()` arrives while {@link turnId} is still unknown: the
   * launch handler re-issues a precisely-addressed cancel once the id lands,
   * and a no-launch outcome settles `cancelled` instead of `end_turn`.
   */
  cancelRequested?: boolean;
  /**
   * Turn-scoped events that arrived while `turnId` was still unknown. Once
   * the launch resolves, the entries matching the driver's turn are replayed
   * in arrival order; the rest (a still-draining prior turn) are dropped —
   * the same verdict the live path would have given.
   */
  early: Array<{ readonly turnId: number; readonly dispatch: () => void }>;
}

export class AcpSession {
  /** The klient facade this session was created from. */
  private readonly klient: Klient;
  private readonly session: SessionHandle;
  private readonly agent: AgentHandle;

  /** Currently-selected model id (bare, no suffix). Empty when unbound. */
  private currentModelId: string = '';
  /** The engine's current thinking level verbatim (`'off'`, `'on'`, or an effort). */
  private currentThinkingLevel: string = 'off';
  /** Current ACP mode. */
  private currentModeId: AcpModeId = DEFAULT_MODE_ID;
  /**
   * Session skill summaries. KLIENT-GAP: the facade exposes no
   * `session.skills` listing, so this stays `[]` and never feeds the command
   * palette — the flush host-provided skill aliases in `resolveCommands`.
   * Kept as the projection site so a future `session.skills` slot turns the
   * engine-skill arm back on by filling it (plus `refreshSkills` /
   * `skills.changed` plumbing).
   */
  private skills: readonly SkillSummary[] = [];
  /** The in-flight prompt's driver, if any. */
  private driver: TurnDriver | undefined;
  /**
   * Abort markers of prompts still in their pre-turn image-compression phase
   * (no turn launched yet, so `agent.cancel` has nothing to cancel).
   * `cancel()` flips every marker; the prompt settles with
   * `stopReason: 'cancelled'` once compression finishes instead of launching
   * a turn the client already asked to stop.
   */
  private readonly pendingPromptAborts = new Set<{ aborted: boolean }>();
  /** Session-level agent-event subscriptions, torn down by `dispose()`. */
  private readonly subscriptions: IDisposable[] = [];
  /**
   * Locations derived at `tool.call.started`, keyed by the ACP wire
   * toolCallId, re-attached to the terminal `tool_call_update`
   * (`tool.result` carries no args/display of its own).
   */
  private readonly toolLocations = new Map<string, ToolCallLocation[]>();
  /**
   * In-flight Bash tool calls awaiting terminal correlation, keyed by the ACP
   * wire toolCallId; the value is the model's `args.command`. Filled at
   * `tool.call.started`, consumed by {@link onTerminalCreated} (or dropped at
   * `tool.result`).
   */
  private readonly bashCallsAwaitingTerminal = new Map<string, string>();
  /**
   * Tool calls whose execution runs in a client terminal: ACP wire toolCallId
   * → terminalId. Their terminal `tool_call_update` carries a
   * `{type: 'terminal'}` content entry instead of the textual output (the
   * client already renders the bytes in the terminal pane — showing them in
   * the card too would duplicate them). The model still receives the full
   * captured output; only the client-facing card content is de-duplicated.
   */
  private readonly terminalBackedCalls = new Map<string, string>();
  /** Bridges engine approval / ask-user requests to the ACP client. */
  private readonly interactionBridge: AcpInteractionBridge;

  constructor(
    private readonly conn: AcpClient,
    klient: Klient,
    readonly sessionId: string,
    private readonly acpConnection: IAcpConnection,
    /**
     * Whether the client advertised `elicitation.form` at `initialize` —
     * forwarded to the interaction bridge's ask-user routing.
     */
    elicitationForm: boolean,
    /**
     * Resolve the session's media-originals dir for prompt-image compression
     * (`sessionMediaOriginalsDir(sessionDir)` when the live session scope is
     * reachable). Undefined / returning undefined → `persistOriginalImage`'s
     * shared temp-dir fallback applies.
     */
    private readonly resolveOriginalsDir?: (sessionId: string) => string | undefined,
    private readonly hostCommands:
      | ReadonlyArray<AvailableCommand>
      | HostSlashCommandsSnapshot = [],
  ) {
    this.klient = klient;
    this.session = klient.session(sessionId);
    // `main` is auto-materialized by the transport's scope resolution on the
    // first call — no explicit agent bootstrap is needed here.
    this.agent = this.session.agent('main');
    this.interactionBridge = new AcpInteractionBridge(conn, this.session, sessionId, elicitationForm);
  }

  /**
   * Subscribe the agent event stream and seed the config state. Must be
   * awaited before the first `prompt` so no early turn events are missed.
   *
   * KLIENT-GAP(event surface): the facade streams no `tool.call.delta` /
   * `tool.progress` / `compaction.*` events, and there is no `session.skills`
   * listing or `skills.changed` event — those subscriptions from the
   * original port are dropped (see the module doc), so `skills` stays `[]`.
   */
  async init(): Promise<void> {
    const events = this.agent.events;
    this.subscriptions.push(
      events.on('assistant.delta', (event) => {
        this.dispatchTurnEvent(event.turnId, () => {
          this.onAssistantDelta(event);
        });
      }),
      events.on('thinking.delta', (event) => {
        this.dispatchTurnEvent(event.turnId, () => {
          this.onThinkingDelta(event);
        });
      }),
      events.on('tool.call.started', (event) => {
        this.dispatchTurnEvent(event.turnId, () => {
          this.onToolCallStarted(event);
        });
      }),
      events.on('tool.result', (event) => {
        this.dispatchTurnEvent(event.turnId, () => {
          this.onToolResult(event);
        });
      }),
      events.on('turn.ended', (event) => {
        this.dispatchTurnEvent(event.turnId, () => {
          this.onTurnEnded(event);
        });
      }),
    );
    // Session-scope stream: title changes surface as `session_info_update`.
    this.subscriptions.push(
      this.session.events.on('metadata.changed', (event) => {
        this.onMetadataChanged(event);
      }),
    );
    // Terminal correlation: the ACP-backed process runner (same process,
    // App-scope holder) announces every terminal it creates so this session
    // can attach it to the matching in-flight Bash tool call.
    const unsubscribeTerminal = this.acpConnection.onTerminalCreated((event) => {
      this.onTerminalCreated(event);
    });
    this.subscriptions.push({ dispose: unsubscribeTerminal });
    try {
      this.currentModelId = await this.agent.getModel();
      this.currentThinkingLevel = await this.agent.getThinking();
    } catch (error) {
      // Keep the unbound defaults — configOptions stays honest.
      log.warn('acp: could not seed model/thinking state', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  /**
   * Tear down per-session resources. Settles an in-flight prompt as
   * cancelled, stops forwarding approval / ask-user requests to the client,
   * and detaches the event subscriptions. Idempotent.
   */
  dispose(): void {
    this.cancel();
    const driver = this.driver;
    if (driver !== undefined) {
      // Never leave the JSON-RPC `session/prompt` hanging after teardown.
      this.settleDriver(driver, () => {
        driver.resolve({ stopReason: 'cancelled' });
      });
    }
    this.interactionBridge.dispose();
    for (const subscription of this.subscriptions.splice(0)) {
      subscription.dispose();
    }
  }

  /**
   * Replay the main agent's persisted context history as an ordered batch of
   * `session/update` notifications. Used by `session/load` so the client
   * re-renders prior turns before the response settles. Awaits every push for
   * ordering — replay is a one-shot batch, not a live stream.
   */
  async replayHistory(): Promise<void> {
    let messages: readonly ContextMessage[];
    try {
      // `history` items cross the wire as JSON-cloned `ContextMessage`s (the
      // facade types them via the engine RPC signature).
      messages = (await this.agent.getContext()).history;
    } catch (error) {
      log.warn('acp: replayHistory could not read context memory', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
      return;
    }
    const updates = projectHistoryToSessionUpdates(this.sessionId, messages);
    for (const update of updates) {
      try {
        await this.conn.sessionUpdate(update);
      } catch (error) {
        // A single transient push failure must not truncate the whole replay;
        // log and continue so the rest of the history still lands.
        log.warn('acp: replayHistory failed to push a session/update; continuing', {
          sessionId: this.sessionId,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    }
  }

  /**
   * Resolve the current command catalog. Builtins always win, followed by
   * engine skills and then host-provided commands; duplicate names are
   * dropped. Host aliases only participate in skill activation when that
   * alias is also present in the advertised command list.
   *
   * The engine-skill arm (`buildAcpSkillSlashCommands`) is inert in this
   * port: `this.skills` stays `[]` because the facade exposes no
   * `session.skills` listing (KLIENT-GAP). Skill activation therefore only
   * reaches reachable skills whose aliases the host supplied via
   * `SlashCommandsSnapshot.skillCommandMap`.
   */
  private resolveCommands(): ResolvedCommands {
    const skillSnapshot = buildAcpSkillSlashCommands(this.skills);
    const hostSnapshot = Array.isArray(this.hostCommands)
      ? { commands: this.hostCommands, skillCommandMap: new Map<string, string>() }
      : (this.hostCommands as HostSlashCommandsSnapshot);
    const commands: AvailableCommand[] = [];
    const names = new Set<string>();
    for (const command of [
      ...ACP_BUILTIN_SLASH_COMMANDS,
      ...skillSnapshot.commands,
      ...hostSnapshot.commands,
    ]) {
      if (names.has(command.name)) continue;
      names.add(command.name);
      commands.push(command);
    }
    const commandMap = new Map(skillSnapshot.commandMap);
    for (const [alias, skillName] of hostSnapshot.skillCommandMap ?? []) {
      if (ACP_BUILTIN_SLASH_COMMAND_NAMES.has(alias)) continue;
      if (!names.has(alias) || commandMap.has(alias)) continue;
      commandMap.set(alias, skillName);
    }
    return { commands, skillCommandMap: commandMap };
  }

  /** Return the same merged command palette that is advertised to the client. */
  availableCommands(): AvailableCommand[] {
    return this.resolveCommands().commands;
  }

  /** Push the current `available_commands_update` to the client. */
  async emitAvailableCommandsUpdate(): Promise<void> {
    try {
      const { commands } = this.resolveCommands();
      await this.conn.sessionUpdate(availableCommandsUpdateNotification(this.sessionId, commands));
    } catch (error) {
      log.warn('acp: failed to push available_commands_update', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  async prompt(blocks: readonly ContentBlock[]): Promise<PromptResponse> {
    // Slash-intent detection: builtin commands execute locally without an LLM
    // turn; skills activate through the engine (rendered skill prompt driven
    // as a normal turn); unknown slash commands are answered locally with an
    // "unknown command" notice (never sent to the model); non-slash input goes
    // to the model as-is. Skill activation only exists for aliases the host
    // supplied (no `session.skills` listing — see `resolveCommands`).
    const text = leadingText(blocks);
    if (text !== undefined) {
      const commands = this.resolveCommands();
      const intent = detectSlashIntent(text, commands.skillCommandMap);
      if (intent.kind === 'builtin') {
        return this.driveBuiltinCommand(intent.name, intent.args, commands.commands);
      }
      if (intent.kind === 'skill') {
        return this.driveSkillActivation(intent.skillName, intent.args);
      }
      if (intent.kind === 'unknown') {
        return this.driveUnknownCommand(intent.name);
      }
    }

    const content = await this.preparePromptContent(blocks);
    if (content === undefined) {
      // Cancelled while compressing (see `cancel()`): settle without
      // launching a turn.
      return { stopReason: 'cancelled' };
    }
    return this.driveTurn(content);
  }

  /**
   * Convert the ACP blocks to engine content parts and run the input-stage
   * image compression (see `compressPromptImageParts`). Compression happens
   * before any turn exists, so honor a `session/cancel` that arrives during
   * it: `cancel()` flips the marker and this returns `undefined` rather than
   * launching a turn the client already asked to stop. Returns the compressed
   * parts otherwise.
   */
  private async preparePromptContent(
    blocks: readonly ContentBlock[],
  ): Promise<readonly ContentPart[] | undefined> {
    const pending = { aborted: false };
    this.pendingPromptAborts.add(pending);
    let content: readonly ContentPart[];
    try {
      content = await compressPromptImageParts(acpBlocksToContentParts(blocks), {
        originalsDir: this.resolveOriginalsDir?.(this.sessionId),
      });
    } finally {
      this.pendingPromptAborts.delete(pending);
    }
    return pending.aborted ? undefined : content;
  }

  /**
   * Answer an unknown slash command locally (no LLM turn): push the notice as
   * one `agent_message_chunk`, then settle the prompt with `end_turn`. The
   * wording mirrors the legacy adapter's `runUnknownSlashCommand`.
   */
  private async driveUnknownCommand(name: string): Promise<PromptResponse> {
    await this.conn.sessionUpdate({
      sessionId: this.sessionId,
      update: {
        sessionUpdate: 'agent_message_chunk',
        content: {
          type: 'text',
          text: `Unknown ACP command: /${name}. Use /help to see available commands.`,
        },
      },
    });
    return { stopReason: 'end_turn' };
  }

  /**
   * Activate a skill through the engine (`IAgentSkillService.activate` behind
   * the klient facade): the engine renders the skill prompt (content + args)
   * and drives it as a normal turn, so the turn events stream and settle
   * exactly like a plain prompt. Empty args go over as `undefined`, matching
   * the other consumers.
   *
   * KLIENT-GAP(launch): the `activateSkill` RPC does not report the launched
   * turn id (it fires `IAgentSkillService.activate` fire-and-forget), so the
   * driver cannot key on the launch result. `adoptNextTurn` makes the first
   * turn-scoped event adopt the driver instead — events still stream and
   * `turn.ended` still settles, exactly like the id-returning path.
   */
  private driveSkillActivation(skillName: string, args: string): Promise<PromptResponse> {
    this.assertNoActiveTurn();
    return this.driveLaunch(
      this.agent.activateSkill({ name: skillName, args: args.length > 0 ? args : undefined }),
      { adoptNextTurn: true },
    );
  }

  /**
   * Execute an ACP builtin slash command locally: render its text from live
   * klient/engine state (no LLM turn), push it as one `agent_message_chunk`,
   * then settle the prompt with `end_turn`. The chunk push is awaited so it
   * lands before the prompt response.
   */
  private async driveBuiltinCommand(
    name: AcpBuiltinSlashCommandName,
    args: string,
    availableCommands: readonly AvailableCommand[],
  ): Promise<PromptResponse> {
    let text: string;
    try {
      text = await runBuiltinSlashCommand(
        name,
        {
          klient: this.klient,
          session: this.session,
          agent: this.agent,
          sessionId: this.sessionId,
          modelId: this.currentModelId,
          thinkingEnabled: this.currentThinkingLevel !== 'off',
          modeId: this.currentModeId,
          availableCommands,
        },
        args,
      );
    } catch (error) {
      log.warn('acp: builtin slash command failed', {
        sessionId: this.sessionId,
        command: name,
        error: error instanceof Error ? error.message : String(error),
      });
      text = `/${name} failed: ${error instanceof Error ? error.message : String(error)}`;
    }
    await this.conn.sessionUpdate({
      sessionId: this.sessionId,
      update: {
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text },
      },
    });
    return { stopReason: 'end_turn' };
  }

  /**
   * Reject a second model-bound prompt while a turn is in flight. The engine
   * QUEUES a plain `agent.prompt` submitted during an active turn (the launch
   * resolves `undefined`, indistinguishable from a hook-blocked launch), and
   * tracking that queued turn would overwrite the only in-flight driver — the
   * first prompt would never settle and both turns' events would go
   * unattributed. The legacy adapter rejected this case (`turn.agent_busy` →
   * -32600); reject locally instead, synchronously with driver assignment so
   * two concurrent prompts cannot race past the check.
   */
  private assertNoActiveTurn(): void {
    if (this.driver !== undefined && !this.driver.settled) {
      throw RequestError.invalidRequest(
        { code: TURN_AGENT_BUSY_CODE },
        'another turn is already in progress',
      );
    }
  }

  /**
   * Submit the prompt and drive the turn to completion: `agent.prompt()`
   * returns the launched turn id, which the session-level event handlers use
   * to attribute events to this driver. Settles on `turn.ended`; a no-launch
   * result (hook-blocked / not runnable) settles with `end_turn`.
   */
  private driveTurn(input: readonly ContentPart[]): Promise<PromptResponse> {
    this.assertNoActiveTurn();
    return this.driveLaunch(this.agent.prompt({ input }));
  }

  /**
   * Shared turn settlement for every launch path (`agent.prompt`,
   * `agent.activateSkill`): the returned turn id attributes subsequent events
   * to this driver; `undefined` means no turn launched (hook-blocked / not
   * runnable — the busy case never gets this far, see
   * {@link assertNoActiveTurn}), so the prompt settles gracefully with
   * `end_turn`.
   *
   * With `options.adoptNextTurn` (skill activation — see
   * {@link driveSkillActivation}), a `undefined` launch result is NOT a
   * no-launch: the engine fired the turn without reporting its id, so the
   * driver stays pending and adopts the first turn-scoped event.
   */
  private driveLaunch(
    launch: Promise<PromptLaunchResult>,
    options?: { readonly adoptNextTurn?: boolean },
  ): Promise<PromptResponse> {
    return new Promise<PromptResponse>((resolve, reject) => {
      const driver: TurnDriver = {
        resolve,
        reject,
        adoptNextTurn: options?.adoptNextTurn,
        settled: false,
        early: [],
      };
      this.driver = driver;
      launch.then(
        (launched) => {
          if (driver.settled) return;
          if (launched === undefined) {
            if (driver.adoptNextTurn === true) {
              // Skill activation: the turn fired without an id on the RPC.
              // Keep the driver pending — `dispatchTurnEvent` adopts the
              // first turn-scoped event and replays everything buffered
              // since the launch. (A never-starting activation would stall
              // until a `session/cancel` or dispose settles it.)
              return;
            }
            // No turn will emit `turn.ended`, so settle gracefully. The engine
            // publishes a `prompt.completed` with reason 'blocked' for the
            // hook-blocked case; the wire carries no blocking message to
            // surface, matching the old `PromptHandle`-based behavior.
            this.settleDriver(driver, () => {
              resolve({ stopReason: driver.cancelRequested === true ? 'cancelled' : 'end_turn' });
            });
            return;
          }
          driver.turnId = launched.turn_id;
          if (driver.cancelRequested === true) {
            // A cancel arrived before the id was known (see `cancel()`): the
            // unaddressed cancel may have predated the turn's activation, so
            // re-issue it now precisely addressed. Idempotent.
            void this.agent.cancel({ turnId: launched.turn_id }).catch((error) => {
              log.warn('acp: deferred cancel failed', {
                sessionId: this.sessionId,
                error: error instanceof Error ? error.message : String(error),
              });
            });
          }
          // Replay the events the turn emitted before its id arrived (a fast
          // turn can outrun the launch round-trip — `activateSkill` returns
          // only after the prompt-metadata update).
          for (const early of driver.early.splice(0)) {
            if (early.turnId === driver.turnId) early.dispatch();
          }
        },
        (error) => {
          this.settleDriver(driver, () => {
            reject(mapPromptLaunchError(error, this.sessionId));
          });
        },
      );
    });
  }

  /**
   * Route a turn-scoped event: buffer it while the in-flight prompt's turn id
   * is still unknown (see {@link TurnDriver.early}), otherwise dispatch live.
   *
   * For an adopting skill driver (`adoptNextTurn`), the FIRST event assigns
   * the id: only one turn can be active (`assertNoActiveTurn`), so that event
   * — and everything buffered before the adoption — belongs to this launch.
   */
  private dispatchTurnEvent(turnId: number, dispatch: () => void): void {
    const driver = this.driver;
    if (driver !== undefined && !driver.settled && driver.turnId === undefined) {
      if (driver.adoptNextTurn === true) {
        // Adopt: this event's turn is the skill activation's turn (see
        // `driveSkillActivation`). Replay the events buffered since the launch
        // round-trip, then dispatch this one — arrival order is preserved.
        driver.turnId = turnId;
        driver.adoptNextTurn = false;
        for (const early of driver.early.splice(0)) {
          early.dispatch();
        }
        dispatch();
        return;
      }
      driver.early.push({ turnId, dispatch });
      return;
    }
    dispatch();
  }

  /**
   * Settle the driver exactly once and detach it from the session so later
   * events of its turn are ignored.
   */
  private settleDriver(driver: TurnDriver, action: () => void): void {
    if (driver.settled) return;
    driver.settled = true;
    if (this.driver === driver) this.driver = undefined;
    action();
  }

  /** The active driver, but only for events of ITS turn. */
  private driverFor(turnId: number): TurnDriver | undefined {
    const driver = this.driver;
    if (driver === undefined || driver.turnId === undefined || driver.turnId !== turnId) {
      return undefined;
    }
    return driver;
  }

  private onAssistantDelta(event: AgentEventPayloads['assistant.delta']): void {
    if (this.driverFor(event.turnId) === undefined) return;
    this.emit(assistantDeltaToSessionUpdate(this.sessionId, event));
  }

  private onThinkingDelta(event: AgentEventPayloads['thinking.delta']): void {
    if (this.driverFor(event.turnId) === undefined) return;
    this.emit(thinkingDeltaToSessionUpdate(this.sessionId, event));
  }

  private onToolCallStarted(event: AgentEventPayloads['tool.call.started']): void {
    if (this.driverFor(event.turnId) === undefined) return;
    // The klient payload mirrors `ToolCallStartedEvent` (`args` / `display`
    // arrive as `unknown` — cast at this seam).
    const mapped = event as unknown as ToolCallStartedEvent;
    const key = acpToolCallId(event.turnId, event.toolCallId);
    const locations = toolCallLocations(mapped.name, mapped.args, mapped.display);
    if (locations !== undefined) {
      this.toolLocations.set(key, locations);
    }
    if (mapped.name === 'Bash') {
      const command = (mapped.args as { command?: unknown } | undefined)?.command;
      if (typeof command === 'string') {
        this.bashCallsAwaitingTerminal.set(key, command);
      }
    }
    // No streaming-args deltas exist on this facade (KLIENT-GAP: no
    // `tool.call.delta` event), so the wire `tool_call` is always created
    // here with the full stringified args.
    this.emit(toolCallStartToSessionUpdate(this.sessionId, mapped));
    if (event.display !== undefined) {
      this.emit(
        planFromDisplayBlock(this.sessionId, event.turnId, event.display as ToolInputDisplay),
      );
    }
  }

  private onToolResult(event: AgentEventPayloads['tool.result']): void {
    if (this.driverFor(event.turnId) === undefined) return;
    const key = acpToolCallId(event.turnId, event.toolCallId);
    const locations = this.toolLocations.get(key);
    this.toolLocations.delete(key);
    this.bashCallsAwaitingTerminal.delete(key);
    const terminalId = this.terminalBackedCalls.get(key);
    this.terminalBackedCalls.delete(key);
    if (terminalId !== undefined) {
      // Terminal-backed call: the client already renders the output bytes in
      // the terminal pane, so the card gets the terminal embed instead of a
      // textual copy. The full output still reached the model (and the
      // persisted wire record) untouched.
      this.emit({
        sessionId: this.sessionId,
        update: {
          sessionUpdate: 'tool_call_update',
          toolCallId: key,
          status: event.isError === true ? 'failed' : 'completed',
          content: [{ type: 'terminal', terminalId }],
          locations,
        },
      });
      return;
    }
    this.emit(
      toolResultToSessionUpdate(this.sessionId, event as unknown as ToolResultEvent, locations),
    );
  }

  /**
   * Correlate a freshly-created client terminal with the in-flight Bash tool
   * call whose command it runs, then attach a `{type: 'terminal'}` content
   * entry to that call's card. Match key: the runner reports the full shell
   * invocation (`cd <cwd> && <command>`), which ends with the model's
   * `args.command`. Terminals with no matching call (e.g. a subagent's —
   * this session only follows the main agent's events) stay unattached.
   */
  private onTerminalCreated(event: AcpTerminalCreatedEvent): void {
    if (event.sessionId !== this.sessionId) return;
    for (const [key, command] of this.bashCallsAwaitingTerminal) {
      if (command.length === 0 || !event.shellCommand.endsWith(command)) continue;
      this.bashCallsAwaitingTerminal.delete(key);
      this.terminalBackedCalls.set(key, event.terminalId);
      this.emit({
        sessionId: this.sessionId,
        update: {
          sessionUpdate: 'tool_call_update',
          toolCallId: key,
          content: [{ type: 'terminal', terminalId: event.terminalId }],
        },
      });
      return;
    }
  }

  private onTurnEnded(event: AgentEventPayloads['turn.ended']): void {
    const driver = this.driverFor(event.turnId);
    if (driver === undefined) return;
    const error = event.error as { readonly code: string; readonly message?: string } | undefined;
    this.settleDriver(driver, () => {
      // Auth failures must surface as a JSON-RPC `auth_required` error
      // so the client triggers its re-auth flow, not a silent `end_turn`.
      if (event.reason === 'failed' && isAuthError(error)) {
        driver.reject(RequestError.authRequired(undefined, error?.message));
        return;
      }
      driver.resolve({ stopReason: turnEndReasonToStopReason(event.reason, error) });
    });
    void this.emitUsageUpdate();
  }

  /**
   * Push a one-shot `usage_update` after a turn settles: `used` = the agent's
   * current context token count, `size` = the bound model's max context size
   * from the catalog. Skipped while no catalog model matches the bound id —
   * there is nothing honest to report. `cost` stays omitted (the engine has
   * no cost data).
   */
  private async emitUsageUpdate(): Promise<void> {
    try {
      const size = (await this.klient.global.kosong.listModels()).find(
        (item) => item.model === this.currentModelId,
      )?.max_context_size;
      if (size === undefined) return;
      const context = await this.agent.getContext();
      this.emit(usageUpdateNotification(this.sessionId, context.tokenCount, size));
    } catch (error) {
      log.warn('acp: failed to push usage_update', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  private onMetadataChanged(event: SessionEventPayloads['metadata.changed']): void {
    if (!event.changed.includes('title')) return;
    void this.emitSessionInfoUpdate();
  }

  /** Push a `session_info_update` with the current title (best-effort). */
  private async emitSessionInfoUpdate(): Promise<void> {
    try {
      const meta = await this.session.get();
      this.emit(sessionInfoUpdateNotification(this.sessionId, meta.title ?? null));
    } catch (error) {
      log.warn('acp: failed to push session_info_update', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  /** Push a `session/update` notification (best-effort, never throws). */
  private emit(notification: SessionNotification | null): void {
    if (notification === null) return;
    void this.conn.sessionUpdate(notification).catch((error) => {
      log.warn('acp: failed to push session/update', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    });
  }

  /**
   * Cancel the in-flight turn, if any, and abort every prompt still in its
   * pre-turn compression phase (those settle as cancelled without launching —
   * see {@link preparePromptContent}). Idempotent.
   */
  cancel(): void {
    for (const pending of this.pendingPromptAborts) {
      pending.aborted = true;
    }
    const driver = this.driver;
    if (driver === undefined || driver.settled) return;
    const turnId = driver.turnId;
    if (turnId === undefined) {
      // The launch round-trip has not returned the turn id yet. The engine's
      // cancel payload makes turnId optional — an empty call cancels whatever
      // turn is active (the same contract kap-server's cancel route relies
      // on) — and concurrent prompts are rejected, so the active turn can only
      // be this driver's. Flag the driver too: when the id lands, the launch
      // handler re-issues a precisely-addressed cancel, and a no-launch
      // outcome settles `cancelled` instead of `end_turn`.
      driver.cancelRequested = true;
      void this.agent.cancel().catch((error) => {
        log.warn('acp: cancel (unaddressed) failed', {
          sessionId: this.sessionId,
          error: error instanceof Error ? error.message : String(error),
        });
      });
      return;
    }
    void this.agent.cancel({ turnId }).catch((error) => {
      log.warn('acp: cancel failed', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    });
  }

  /**
   * Build the current `configOptions` snapshot (model + thinking + mode).
   * The thinking toggle only appears when the bound model's catalog row is
   * thinking-capable (see `buildSessionConfigOptions`).
   */
  async configOptions(): Promise<SessionConfigOption[]> {
    const models = projectModelCatalog(await this.klient.global.kosong.listModels());
    return buildSessionConfigOptions(
      models,
      this.currentModelId,
      this.currentThinkingLevel,
      this.currentModeId,
    );
  }

  /**
   * The first-class `modes` ({@link SessionModeState}) snapshot for the
   * `session/new` / `session/load` / `session/resume` responses — the same
   * taxonomy the `mode` config-option arm projects.
   */
  modeState(): SessionModeState {
    return { currentModeId: this.currentModeId, availableModes: [...ACP_MODES] };
  }

  /**
   * Switch the active model.
   *
   * Legacy clients may merge the thinking flag into the model id as
   * `"<id>,thinking"` (mirrors the Python ref's `_ModelIDConv.from_acp_model_id`
   * and the legacy adapter): the merged form splits into `setModel(<bare id>)`
   * plus `setThinking(<the NEW model's default effort>)`. The asymmetry is
   * load-bearing — a bare id does NOT turn thinking off (model and thinking
   * stay orthogonal; disabling thinking requires the `thinking` config option
   * with value `'off'`). Both entry points (`session/set_model` and the
   * `model` arm of `session/set_config_option`) funnel here.
   */
  async setModel(id: string): Promise<void> {
    const suffix = ',thinking';
    const hasSuffix = id.endsWith(suffix);
    const baseId = hasSuffix ? id.slice(0, -suffix.length) : id;
    await this.agent.setModel(baseId);
    // Update BEFORE resolving the on-effort so a merged `,thinking` switch
    // picks the NEW model's default level, not the old one's.
    this.currentModelId = baseId;
    if (hasSuffix) {
      const models = projectModelCatalog(await this.klient.global.kosong.listModels());
      const level = models.find((model) => model.id === baseId)?.defaultThinkingEffort ?? 'on';
      await this.agent.setThinking(level);
      this.currentThinkingLevel = level;
    }
    await this.emitConfigOptionUpdate();
  }

  /**
   * Switch the thinking level. The value is validated against the current
   * model's declared capability: with `supportEfforts` the allowed set is
   * `'off'` plus every declared effort (an `always_thinking` model drops
   * `'off'`); without them it stays the boolean `'off'` / `'on'` pair. Legacy
   * boolean clients sending `'on'` to an effort-granular model map to the
   * model's default effort (`AcpModelEntry.defaultThinkingEffort`). Returns
   * `false` for a value the model cannot take — the caller maps that to ACP
   * `invalid_params`.
   */
  async setThinking(value: string): Promise<boolean> {
    const models = projectModelCatalog(await this.klient.global.kosong.listModels());
    const entry = models.find((model) => model.id === this.currentModelId);
    const efforts = entry?.supportEfforts;
    const alwaysThinking = entry?.alwaysThinking === true;
    const allowed =
      efforts !== undefined
        ? alwaysThinking
          ? efforts
          : ['off', ...efforts]
        : alwaysThinking
          ? ['on']
          : ['off', 'on'];
    let level: string;
    if (allowed.includes(value)) {
      level = value;
    } else if (value === 'on' && efforts !== undefined) {
      level = entry?.defaultThinkingEffort ?? 'on';
    } else {
      return false;
    }
    await this.agent.setThinking(level);
    this.currentThinkingLevel = level;
    await this.emitConfigOptionUpdate();
    return true;
  }

  /** Switch the ACP mode (plan mode + permission mode). */
  async setMode(id: AcpModeId): Promise<void> {
    const { plan, permission } = acpModeToToggles(id);
    if (plan) {
      await this.agent.enterPlan();
    } else {
      // KLIENT-GAP(plan): `exitPlan` (`planService.exit()`) is not on the
      // klient surface; `cancelPlan` (`planModeCancel`) has the identical
      // state effect (see `agent/plan/planOps.ts`) — only the persisted op
      // name differs.
      await this.agent.cancelPlan();
    }
    await this.agent.setPermission(permission);
    this.currentModeId = id;
    // Both notifications fire: `current_mode_update` serves clients reading
    // the first-class `modes` state, `config_option_update` serves clients
    // reading the `mode` config-option arm. (Engine-side mode changes are not
    // observable — klient exposes no permission/plan change event.)
    this.emit(currentModeUpdateNotification(this.sessionId, id));
    await this.emitConfigOptionUpdate();
  }

  /** Push a fresh `config_option_update` to the client. */
  private async emitConfigOptionUpdate(): Promise<void> {
    try {
      await this.conn.sessionUpdate(
        configOptionUpdateNotification(this.sessionId, await this.configOptions()),
      );
    } catch (error) {
      log.warn('acp: failed to push config_option_update', {
        sessionId: this.sessionId,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }
}
