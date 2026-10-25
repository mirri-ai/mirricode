/**
 * Wire-translation layer for the Rust engine bridge (`SDKRpcClientRust`).
 *
 * The NAPI binding (`@mirri-ai/mirri-node`, `rust/crates/mirri-node`) delivers
 * two push channels, both in the engine's own wire dialect:
 *
 * - Events (`engine.onEvent`): every notification is a
 *   `{ method, params }` object. The method is an `event.<type>` string and
 *   `params` is a flat envelope `{ sessionId, volatile, timestamp,
 *   [seq, epoch], ...payloadFields }` (seq/epoch only on durable events),
 *   produced by the shared event projection in
 *   `rust/crates/mirri-core/src/session/event_projection.rs` — the same
 *   authority mirri-server uses. The envelope's reserved keys are stripped
 *   to form the payload; the dotted type name after the `event.` prefix is
 *   the protocol event type where one exists.
 * - Reverse RPC (`engine.onReverseRpc`): `{ id, method, params }` requests.
 *   The engine issues `approval.ask` (the permission gate, see
 *   `rust/crates/mirri-core/src/turn_flow/hooks.rs`) and `question.ask` (the
 *   `AskUserQuestion` tool, see `rust/crates/mirri-core/src/tools/ask_user.rs`).
 *
 * This module translates both channels into the SDK's v1 shapes:
 * `translateRustEvent` → `Event` (the process is best-effort — notifications
 * without a v1 counterpart are dropped, never thrown), and the reverse-RPC
 * helpers map the snake_case wire payloads onto the camelCase
 * `ApprovalRequest` / `QuestionRequest` the base class's handlers consume.
 */

import {
  ErrorCodes,
  makeErrorPayload,
  type ApprovalRequest,
  type ApprovalResponse,
  type BackgroundTaskStartedEvent,
  type CompactionResult,
  type CronFiredEvent,
  type Event,
  type GoalUpdatedEvent,
  type McpServerStatusEvent,
  type MirriErrorCode,
  type QuestionItem,
  type QuestionRequest,
  type SkillActivatedEvent,
  type SubagentCompletedEvent,
  type ToolInputDisplay,
  type ToolUpdate,
} from '@mirri-ai/agent-core';
import type { AgentPhase, UsageStatus } from '@mirri-ai/protocol';

/** The engine's main agent id (mirrors the TS SDK's `MAIN_AGENT_ID`). */
const MAIN_AGENT_ID = 'main';

/**
 * The structured `agent.status.updated` wire payload (CR4) in the v1 field
 * spellings: `agentId` and `phase` are the engine's only required fields,
 * every other one is optional and entirely absent from the JSON when
 * unknown (the projection's serde `skip_serializing_if` guarantees no null
 * placeholders). The shape is trusted, not re-validated here — the fixture
 * contract `test/fixtures/rust-engine-events.ts` pins it end to end.
 */
interface WireAgentStatusPayload {
  readonly phase?: AgentPhase;
  readonly model?: string;
  readonly thinkingEffort?: string;
  readonly contextTokens?: number;
  readonly maxContextTokens?: number;
  readonly contextUsage?: number;
  readonly planMode?: boolean;
  readonly swarmMode?: boolean;
  readonly usage?: UsageStatus;
}

/**
 * Synthetic turn id for translated events: the engine's turn ids are
 * `turn-<uuid>` strings by design, while every v1 turn event requires a
 * number — and the volatile events (`assistant.delta`, `thinking.delta`,
 * the `tool.*` stream) carry no turn id at all, so per-turn ids cannot be
 * reconstructed. The TUI treats the id as an opaque routing key, so a single
 * stable synthetic value keeps a turn's streamed text, tool cards, and
 * terminal state on one turn context.
 */
const SYNTHETIC_TURN_ID = 0;

/** One notification as delivered by `MirriEngineNative.onEvent`. */
export interface RustEngineEventNotification {
  readonly method: string;
  readonly params: unknown;
}

/** The parsed, validated `params` of an engine `event` notification. */
export interface ParsedRustEvent {
  readonly type: string;
  readonly sessionId: string;
  readonly payload: Record<string, unknown>;
}

/** One reverse-RPC request as delivered by `MirriEngineNative.onReverseRpc`. */
export interface RustEngineReverseRpcRequest {
  readonly id: number;
  readonly method: string;
  readonly params: unknown;
}

/** The `skill.catalogChanged` trigger vocabulary (ADR-010). */
const SKILL_CATALOG_TRIGGERS = new Set(['file', 'config', 'plugin', 'manual']);

/**
 * Translate the app-level `skill.catalogChanged` notification params (the
 * orchestrator's camelCase wire payload, ADR-010 Task 7) into the v1
 * `Event` — the one engine notification without the `event.` prefix this
 * client consumes. Returns `undefined` for anything malformed (the same
 * best-effort contract as {@link translateRustEvent}).
 */
export function translateSkillCatalogChanged(params: unknown): Event | undefined {
  if (!isRecord(params)) return undefined;
  const workspaceId = params['workspaceId'];
  if (typeof workspaceId !== 'string' || workspaceId.length === 0) return undefined;
  const added = stringList(params['added']);
  const removed = stringList(params['removed']);
  const updated = stringList(params['updated']);
  if (added === undefined || removed === undefined || updated === undefined) return undefined;
  const trigger = params['trigger'];
  if (typeof trigger !== 'string' || !SKILL_CATALOG_TRIGGERS.has(trigger)) return undefined;
  // The v1 `Event` envelope requires `agentId`/`sessionId`; the notification
  // is workspace-scoped with no session anywhere, so the main-agent spelling
  // and the empty session id carry it — the same no-session convention the
  // `event.workspace.*` projection uses.
  return {
    type: 'skill.catalogChanged',
    workspaceId,
    added,
    removed,
    updated,
    trigger: trigger as 'file' | 'config' | 'plugin' | 'manual',
    agentId: MAIN_AGENT_ID,
    sessionId: '',
  };
}

/** Validate an array of strings (`undefined` for anything else). */
function stringList(raw: unknown): readonly string[] | undefined {
  if (!Array.isArray(raw)) return undefined;
  return raw.every((entry) => typeof entry === 'string') ? (raw as readonly string[]) : undefined;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * Validate an engine event notification and pull out its discriminated
 * fields. The method must be an `event.<type>` string and `params` a flat
 * envelope whose reserved keys (`sessionId`, `volatile`, `timestamp`, `seq`,
 * `epoch`) are stripped to form the payload. Returns `undefined` for
 * anything that is not a well-formed engine event notification (other
 * methods, missing fields).
 */
export function parseRustEventNotification(
  notification: RustEngineEventNotification,
): ParsedRustEvent | undefined {
  if (!notification.method.startsWith('event.')) return undefined;
  const type = notification.method.slice('event.'.length);
  if (type.length === 0) return undefined;
  if (!isRecord(notification.params)) return undefined;
  const sessionId = notification.params['sessionId'];
  if (typeof sessionId !== 'string') return undefined;
  const payload: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(notification.params)) {
    if (RESERVED_ENVELOPE_KEYS.has(key)) continue;
    payload[key] = value;
  }
  return { type, sessionId, payload };
}

/** Envelope keys the new dialect reserves for itself (never payload fields). */
const RESERVED_ENVELOPE_KEYS = new Set(['sessionId', 'volatile', 'timestamp', 'seq', 'epoch']);

/**
 * Translate one engine event notification into a v1 `Event`, or `undefined`
 * when the engine's event has no v1 counterpart.
 *
 * Mapping table (engine `event.<type>` → v1 `Event` variant), kept in sync
 * with the projection vocabulary in
 * `rust/crates/mirri-core/src/session/event_projection.rs`:
 *
 * | engine event                                    | v1 event                    |
 * |-------------------------------------------------|-----------------------------|
 * | `turn.started { turnId }`                       | `turn.started` (origin `user`) |
 * | `turn.ended { reason }`                         | `turn.ended` (all reasons; `blocked` → `completed`, see below) |
 * | `assistant.delta { agentId, delta }`            | `assistant.delta`           |
 * | `thinking.delta { agentId, delta }`             | `thinking.delta`            |
 * | `tool.call.started { toolCallId, name, args }`  | `tool.call.started` (the wire `args` object passes through; before the field-level alignment the wire carried none and the streaming client accumulated them from `tool.call.delta`) |
 * | `tool.call.delta { agentId, toolCallId, … }`    | `tool.call.delta`           |
 * | `tool.progress { toolCallId, update }`          | `tool.progress`             |
 * | `tool.result { toolCallId, name, isError }`     | `tool.result` (output `''` — the wire carries none; `name` passes through so a mid-stream joiner still attributes the card; the TUI matches cards by toolCallId) |
 * | `agent.status.updated { agentId, phase, … }`   | `agent.status.updated` (the structured CR4 status bag — phase/model/thinkingEffort/context/usage — passes through field for field) |
 * | `hook.result { hookEvent, content, blocked? }`   | `hook.result`               |
 * | `compaction.started { trigger, instruction? }`  | `compaction.started`        |
 * | `compaction.completed { result }`                | `compaction.completed`      |
 * | `compaction.cancelled {}`                        | `compaction.cancelled`      |
 * | `compaction.blocked { turnId? }`                  | `compaction.blocked` (turnId omitted — the wire turn id is a `turn-<uuid>` string with no number mapping) |
 * | `background.task.started { info }`               | `background.task.started`   |
 * | `background.task.terminated { info }`             | `background.task.terminated` |
 * | `shell.output { commandId, update, taskId? }`   | `shell.output` (the wire-only `taskId` has no v1 field — the v1 client addresses the run by `commandId`) |
 * | `shell.started { commandId, taskId }`             | `shell.started`             |
 * | `subagent.spawned { subagentId, … }`               | `subagent.spawned` (the wire `parentToolCallId` is required for the v1 shape — an event without one is dropped) |
 * | `subagent.started/suspended/completed/failed`     | the same v1 variants        |
 * | `turn.step.started/completed { turnId, step, … }` | `turn.step.*` (the synthetic turn id — the wire now carries the real `turn-<uuid>` string, but the v1 events require a number) |
 * | `turn.step.retrying { turnId, step, attempts… }`  | `turn.step.retrying` (the synthetic turn id; errorName/errorMessage synthesized — the wire carries none) |
 * | `turn.step.interrupted { turnId, step, reason }` | `turn.step.interrupted` (the synthetic turn id) |
 * | `goal.updated { snapshot, change? }`              | `goal.updated` (the wire budget limits are folded into the v1 `GoalBudgetReport`) |
 * | `skill.activated { activationId, … }`             | `skill.activated`           |
 * | `plugin_command.activated { … }`                   | `plugin_command.activated` |
 * | `mcp.server.status { server }`                    | `mcp.server.status`        |
 * | `session.meta.updated { title?, patch? }`         | `session.meta.updated`     |
 * | `session.model_switched { from, to }`             | `session.meta.updated` (the durable mid-turn model switch, spec 11 §2.5 A2 — the v1 union has no first-class variant, so the aliases ride the free-form patch as `modelSwitched`) |
 * | `cron.fired { origin, prompt }`                   | `cron.fired`               |
 *
 * Id-mapping strategies:
 *
 * - **agentId**: the engine's wire events carry agent ids as strings — the
 *   main agent's events carry `"main"`; child agents carry their real id
 *   (`agent-<uuid>`). The nil uuid is only tolerated as the legacy spelling
 *   of `main` from older rust engines (`uuid_from_id("main")` on their side),
 *   so that one id also recovers to the SDK's `main`. Every other id passes
 *   through verbatim — child-agent / /btw routing keys off the real id.
 * - **turnId**: the engine's turn ids are `turn-<uuid>` strings by design,
 *   while every v1 turn event requires a number, and the volatile events
 *   (`assistant.delta`, `thinking.delta`, `tool.*`) carry no turn id at all.
 *   The TUI treats the id as an opaque routing key, so every translated
 *   event uses the same stable synthetic value — that keeps a turn's
 *   streamed text, tool cards, and terminal state on one turn context.
 *
 * Deliberately dropped (no v1 `Event` variant — the TUI learns these from
 * RPC responses or the reverse-RPC channel, the same as on the other
 * engines): `message.received` (no user-message variant; `PromptSubmittedEvent`
 * describes the client's own prompt, not an engine-side message), the
 * session lifecycle events (`session.deleted`, `session.updated`), the
 * workspace lifecycle events (`workspace.created`/`updated`/`deleted` — the
 * W2 server-scoped events: the SDK manages workspaces through RPC, so the
 * mirror notifications are deliberately dropped with explicit arms), the
 * agent membership events (`agent.created`, `agent.removed`), and the gate
 * mirror events (`approval.requested`/`resolved`,
 * `question.requested`/`answered`/`dismissed` — approvals and questions
 * ride the `approval.ask` / `question.ask` reverse-RPC channel), the
 * rust shell runner's terminal frame (`shell.completed` — the v1 dialect
 * closes a `!` run through the `runShellCommand` response and the
 * background-task lifecycle, with no completion event variant), and
 * unknown event types are dropped too — the engine's vocabulary grows
 * independently, and a not-yet-mapped event must never crash the event pump.
 */
export function translateRustEvent(
  notification: RustEngineEventNotification,
): Event | undefined {
  const parsed = parseRustEventNotification(notification);
  if (parsed === undefined) return undefined;
  const { type, sessionId, payload } = parsed;
  const agentId = agentIdFromWire(payload);
  switch (type) {
    case 'assistant.delta': {
      const delta = payload['delta'];
      if (typeof delta !== 'string' || delta.length === 0) return undefined;
      return {
        type: 'assistant.delta',
        turnId: SYNTHETIC_TURN_ID,
        delta,
        agentId,
        sessionId,
      };
    }
    case 'thinking.delta': {
      const delta = payload['delta'];
      if (typeof delta !== 'string' || delta.length === 0) return undefined;
      return {
        type: 'thinking.delta',
        turnId: SYNTHETIC_TURN_ID,
        delta,
        agentId,
        sessionId,
      };
    }
    case 'turn.started': {
      // The wire carries only the turn id — the v1 `origin` is required, so
      // the engine's turns (all dispatched from `message.send`) are declared
      // user-originated; nothing downstream consumes the origin today.
      return {
        type: 'turn.started',
        turnId: SYNTHETIC_TURN_ID,
        origin: { kind: 'user' },
        agentId,
        sessionId,
      };
    }
    case 'turn.ended': {
      const reason = turnEndReasonFromWire(payload['reason']);
      if (reason === 'failed') {
        // Rebuild the error rather than passing the wire object through: the
        // wire error carries a `retryable` key with no v1 counterpart. The
        // wire error code is preserved when it is a known SDK code so
        // consumers can branch on the real failure class.
        const wireError = payload['error'];
        const message =
          isRecord(wireError) &&
          typeof wireError['message'] === 'string' &&
          wireError['message'].length > 0
            ? wireError['message']
            : 'engine turn failed';
        const wireCode = isRecord(wireError) ? optionalString(wireError['code']) : undefined;
        const code =
          wireCode !== undefined && isSdkErrorCode(wireCode) ? wireCode : ErrorCodes.INTERNAL;
        return {
          type: 'turn.ended',
          turnId: SYNTHETIC_TURN_ID,
          reason: 'failed',
          error: makeErrorPayload(code, message),
          agentId,
          sessionId,
        };
      }
      return {
        type: 'turn.ended',
        turnId: SYNTHETIC_TURN_ID,
        reason,
        agentId,
        sessionId,
      };
    }
    case 'tool.call.started': {
      const toolCallId = payload['toolCallId'];
      const name = payload['name'];
      if (typeof toolCallId !== 'string' || toolCallId.length === 0) return undefined;
      if (typeof name !== 'string' || name.length === 0) return undefined;
      const args = isRecord(payload['args']) ? payload['args'] : undefined;
      return {
        type: 'tool.call.started',
        turnId: SYNTHETIC_TURN_ID,
        toolCallId,
        name,
        args,
        agentId,
        sessionId,
      };
    }
    case 'tool.call.delta': {
      const toolCallId = payload['toolCallId'];
      const argumentsPart = payload['argumentsPart'];
      if (typeof toolCallId !== 'string' || toolCallId.length === 0) return undefined;
      if (typeof argumentsPart !== 'string') return undefined;
      const name = optionalString(payload['name']);
      return {
        type: 'tool.call.delta',
        turnId: SYNTHETIC_TURN_ID,
        toolCallId,
        ...(name === undefined ? {} : { name }),
        argumentsPart,
        agentId,
        sessionId,
      };
    }
    case 'tool.progress': {
      const toolCallId = payload['toolCallId'];
      if (typeof toolCallId !== 'string' || toolCallId.length === 0) return undefined;
      const update = wireToolUpdate(payload['update']);
      if (update === undefined) return undefined;
      return {
        type: 'tool.progress',
        turnId: SYNTHETIC_TURN_ID,
        toolCallId,
        update,
        agentId,
        sessionId,
      };
    }
    case 'tool.result': {
      const toolCallId = payload['toolCallId'];
      if (typeof toolCallId !== 'string' || toolCallId.length === 0) return undefined;
      const name = optionalString(payload['name']);
      return {
        type: 'tool.result',
        turnId: SYNTHETIC_TURN_ID,
        toolCallId,
        // The wire event carries no output (the engine's projection keeps
        // the result content off the stream); an empty string is the safe
        // neutral rendering — the TUI matches the tool card by toolCallId.
        output: '',
        isError: payload['isError'] === true,
        // The wire's tool name rides along when the projection carried it,
        // so a consumer that missed the started event still attributes the
        // result to its tool.
        ...(name === undefined ? {} : { name }),
        agentId,
        sessionId,
      };
    }
    case 'agent.status.updated': {
      // The wire payload is the structured CR4 status bag: `agentId` +
      // `phase` always present, every other field optional and entirely
      // absent when unknown (the fixture contract
      // `test/fixtures/rust-engine-events.ts` pins the exact shapes).
      // Every field maps 1:1 onto the v1 `AgentStatusUpdatedEvent` —
      // direct passthrough, `undefined` fields stay naturally absent.
      const {
        phase,
        model,
        thinkingEffort,
        contextTokens,
        maxContextTokens,
        contextUsage,
        planMode,
        swarmMode,
        usage,
      } = payload as WireAgentStatusPayload;
      return {
        type: 'agent.status.updated',
        agentId,
        sessionId,
        phase,
        model,
        thinkingEffort,
        contextTokens,
        maxContextTokens,
        contextUsage,
        planMode,
        swarmMode,
        usage,
      };
    }
    case 'hook.result': {
      const hookEvent = payload['hookEvent'];
      const content = payload['content'];
      if (typeof hookEvent !== 'string' || hookEvent.length === 0) return undefined;
      if (typeof content !== 'string') return undefined;
      return {
        type: 'hook.result',
        turnId: SYNTHETIC_TURN_ID,
        hookEvent,
        content,
        ...(payload['blocked'] === true ? { blocked: true } : {}),
        agentId,
        sessionId,
      };
    }
    case 'compaction.started': {
      const trigger = payload['trigger'];
      if (trigger !== 'manual' && trigger !== 'auto') return undefined;
      const instruction = optionalString(payload['instruction']);
      return {
        type: 'compaction.started',
        trigger,
        ...(instruction === undefined ? {} : { instruction }),
        agentId,
        sessionId,
      };
    }
    case 'compaction.completed': {
      const result = wireCompactionResult(payload['result']);
      if (result === undefined) return undefined;
      return { type: 'compaction.completed', result, agentId, sessionId };
    }
    case 'compaction.cancelled': {
      return { type: 'compaction.cancelled', agentId, sessionId };
    }
    case 'compaction.blocked': {
      return { type: 'compaction.blocked', agentId, sessionId };
    }
    case 'background.task.started': {
      const info = wireBackgroundTaskInfo(payload['info']);
      if (info === undefined) return undefined;
      return { type: 'background.task.started', info, agentId, sessionId };
    }
    case 'background.task.terminated': {
      const info = wireBackgroundTaskInfo(payload['info']);
      if (info === undefined) return undefined;
      return { type: 'background.task.terminated', info, agentId, sessionId };
    }
    case 'shell.output': {
      // The live `!` chunk — the v1 variant routes by `commandId`; the
      // wire-only optional `taskId` (linkage to the background task the run
      // detached into) has no v1 field to land in.
      const commandId = payload['commandId'];
      if (typeof commandId !== 'string' || commandId.length === 0) return undefined;
      const update = wireToolUpdate(payload['update']);
      if (update === undefined) return undefined;
      return { type: 'shell.output', commandId, update, agentId, sessionId };
    }
    case 'shell.started': {
      const commandId = payload['commandId'];
      const taskId = payload['taskId'];
      if (typeof commandId !== 'string' || commandId.length === 0) return undefined;
      if (typeof taskId !== 'string' || taskId.length === 0) return undefined;
      return { type: 'shell.started', commandId, taskId, agentId, sessionId };
    }
    case 'subagent.spawned': {
      const subagentId = payload['subagentId'];
      const subagentName = payload['subagentName'];
      if (typeof subagentId !== 'string' || subagentId.length === 0) return undefined;
      if (typeof subagentName !== 'string' || subagentName.length === 0) return undefined;
      const parentToolCallId = optionalString(payload['parentToolCallId']);
      if (parentToolCallId === undefined) return undefined;
      return {
        type: 'subagent.spawned',
        subagentId,
        subagentName,
        parentToolCallId,
        ...(optionalString(payload['parentAgentId']) === undefined
          ? {}
          : { parentAgentId: payload['parentAgentId'] as string }),
        ...(optionalString(payload['description']) === undefined
          ? {}
          : { description: payload['description'] as string }),
        ...(typeof payload['swarmIndex'] === 'number' ? { swarmIndex: payload['swarmIndex'] } : {}),
        runInBackground: payload['runInBackground'] === true,
        ...(optionalString(payload['model']) === undefined
          ? {}
          : { model: payload['model'] as string }),
        agentId,
        sessionId,
      };
    }
    case 'subagent.started': {
      const subagentId = payload['subagentId'];
      if (typeof subagentId !== 'string' || subagentId.length === 0) return undefined;
      return { type: 'subagent.started', subagentId, agentId, sessionId };
    }
    case 'subagent.suspended': {
      const subagentId = payload['subagentId'];
      const reason = payload['reason'];
      if (typeof subagentId !== 'string' || subagentId.length === 0) return undefined;
      // ADR-011: the rust wire carries the reason as a structured OBJECT
      // (`{ kind: "approval" | "question" | "rate_limit", request_id?,
      // tool_call_id? }`) or a legacy string; the SDK event contract is
      // `reason: string`, so objects serialize verbatim to their JSON text.
      // Never drop the whole event because of the reason shape — downstream
      // (TUI `suspendedPhaseForReason`) parses both forms.
      let reasonText: string | undefined;
      if (typeof reason === 'string') {
        reasonText = reason.length > 0 ? reason : undefined;
      } else if (isRecord(reason)) {
        reasonText = JSON.stringify(reason) ?? undefined;
      }
      if (reasonText === undefined) return undefined;
      return { type: 'subagent.suspended', subagentId, reason: reasonText, agentId, sessionId };
    }
    case 'subagent.completed': {
      const subagentId = payload['subagentId'];
      const resultSummary = payload['resultSummary'];
      if (typeof subagentId !== 'string' || subagentId.length === 0) return undefined;
      if (typeof resultSummary !== 'string') return undefined;
      const usage = wireTokenUsage(payload['usage']);
      return {
        type: 'subagent.completed',
        subagentId,
        resultSummary,
        ...(usage === undefined ? {} : { usage }),
        agentId,
        sessionId,
      };
    }
    case 'subagent.failed': {
      const subagentId = payload['subagentId'];
      const error = payload['error'];
      if (typeof subagentId !== 'string' || subagentId.length === 0) return undefined;
      if (typeof error !== 'string' || error.length === 0) return undefined;
      return { type: 'subagent.failed', subagentId, error, agentId, sessionId };
    }
    case 'turn.step.started':
    case 'turn.step.completed': {
      // The wire step counter is the identity; the v1 events additionally
      // require the (synthetic) turn id. The completed branch's optional
      // usage rides along when the wire carries it.
      const step = payload['step'];
      if (typeof step !== 'number') return undefined;
      if (type === 'turn.step.started') {
        return { type: 'turn.step.started', turnId: SYNTHETIC_TURN_ID, step, agentId, sessionId };
      }
      const usage = wireTokenUsage(payload['usage']);
      return {
        type: 'turn.step.completed',
        turnId: SYNTHETIC_TURN_ID,
        step,
        ...(usage === undefined ? {} : { usage }),
        agentId,
        sessionId,
      };
    }
    case 'turn.step.retrying': {
      const step = payload['step'];
      const failedAttempt = payload['failedAttempt'];
      const nextAttempt = payload['nextAttempt'];
      const maxAttempts = payload['maxAttempts'];
      const delayMs = payload['delayMs'];
      if (typeof step !== 'number') return undefined;
      if (typeof failedAttempt !== 'number' || typeof nextAttempt !== 'number') return undefined;
      if (typeof maxAttempts !== 'number' || typeof delayMs !== 'number') return undefined;
      return {
        type: 'turn.step.retrying',
        turnId: SYNTHETIC_TURN_ID,
        step,
        failedAttempt,
        nextAttempt,
        maxAttempts,
        delayMs,
        errorName: 'StepRetry',
        errorMessage: optionalString(payload['message']) ?? 'step failed; retrying',
        agentId,
        sessionId,
      };
    }
    case 'turn.step.interrupted': {
      const step = payload['step'];
      const reason = payload['reason'];
      if (typeof step !== 'number') return undefined;
      if (typeof reason !== 'string' || reason.length === 0) return undefined;
      const message = optionalString(payload['message']);
      return {
        type: 'turn.step.interrupted',
        turnId: SYNTHETIC_TURN_ID,
        step,
        reason,
        ...(message === undefined ? {} : { message }),
        agentId,
        sessionId,
      };
    }
    case 'goal.updated': {
      // A malformed snapshot degrades to the null spelling (the goal is
      // gone / unparseable) rather than dropping the event — a `goal.updated`
      // the TUI misses would strand its goal card.
      const snapshot = wireGoalSnapshot(payload['snapshot']) ?? null;
      const change = wireGoalChange(payload['change']);
      return {
        type: 'goal.updated',
        snapshot,
        ...(change === undefined ? {} : { change }),
        agentId,
        sessionId,
      };
    }
    case 'skill.activated': {
      const activationId = payload['activationId'];
      const skillName = payload['skillName'];
      const trigger = payload['trigger'];
      if (typeof activationId !== 'string' || activationId.length === 0) return undefined;
      if (typeof skillName !== 'string' || skillName.length === 0) return undefined;
      if (
        trigger !== 'user-slash' && trigger !== 'user-inline' &&
        trigger !== 'model-tool' && trigger !== 'nested-skill'
      ) {
        return undefined;
      }
      const args = optionalString(payload['args']);
      const skillPath = optionalString(payload['skillPath']);
      const skillSource = wireSkillSource(payload['skillSource']);
      return {
        type: 'skill.activated',
        activationId,
        skillName,
        ...(args === undefined ? {} : { args }),
        trigger,
        ...(skillPath === undefined ? {} : { skillPath }),
        ...(skillSource === undefined ? {} : { skillSource }),
        agentId,
        sessionId,
      };
    }
    case 'plugin_command.activated': {
      const activationId = payload['activationId'];
      const pluginId = payload['pluginId'];
      const commandName = payload['commandName'];
      if (typeof activationId !== 'string' || activationId.length === 0) return undefined;
      if (typeof pluginId !== 'string' || pluginId.length === 0) return undefined;
      if (typeof commandName !== 'string' || commandName.length === 0) return undefined;
      const commandArgs = optionalString(payload['commandArgs']);
      return {
        type: 'plugin_command.activated',
        activationId,
        pluginId,
        commandName,
        ...(commandArgs === undefined ? {} : { commandArgs }),
        trigger: 'user-slash',
        agentId,
        sessionId,
      };
    }
    case 'mcp.server.status': {
      const server = wireMcpServerStatus(payload['server']);
      if (server === undefined) return undefined;
      return { type: 'mcp.server.status', server, agentId, sessionId };
    }
    case 'session.meta.updated': {
      const title = optionalString(payload['title']);
      const patch = wireMetaPatch(payload['patch']);
      return {
        type: 'session.meta.updated',
        ...(title === undefined ? {} : { title }),
        ...(patch === undefined ? {} : { patch }),
        agentId,
        sessionId,
      };
    }
    case 'session.model_switched': {
      // The durable mid-turn model switch (spec 11 §2.5 A2). The v1 `Event`
      // union is deliberately closed (an exhaustive-switch canary in
      // node-sdk's session-event-types.test.ts guards additions), so the
      // switch rides the free-form `session.meta.updated` patch as the
      // `modelSwitched` member — the same channel a real meta change uses,
      // and the channel the TUI's divider renders from. The aliases pass
      // through verbatim; the display-name mapping is a presentation-end
      // concern (M4 is not an identifier).
      const from = optionalString(payload['from']);
      const to = optionalString(payload['to']);
      if (from === undefined || to === undefined) return undefined;
      return {
        type: 'session.meta.updated',
        patch: { modelSwitched: { from, to } },
        agentId,
        sessionId,
      };
    }
    case 'cron.fired': {
      const origin = wireCronJobOrigin(payload['origin']);
      const prompt = payload['prompt'];
      if (origin === undefined) return undefined;
      if (typeof prompt !== 'string' || prompt.length === 0) return undefined;
      return { type: 'cron.fired', origin, prompt, agentId, sessionId };
    }
    // Workspace lifecycle events (the W2 server-scoped stream): no v1
    // `Event` counterpart — workspaces are managed through RPC, so the
    // mirror notifications are deliberately dropped with explicit arms
    // (contract 3 sync; an accidental future mapping must be a review
    // decision, not a default fall-through).
    case 'workspace.created':
    case 'workspace.updated':
    case 'workspace.deleted':
      return undefined;
    default:
      return undefined;
  }
}

/**
 * The nil uuid — the legacy wire spelling of the main agent's id from rust
 * engines that emitted `uuid_from_id("main")` (now "main" verbatim; see
 * `rust/crates/mirri-core/src/session/events.rs`). Kept only so events
 * journaled by those older engines still resolve to the SDK's `main`.
 */
const NIL_AGENT_UUID = '00000000-0000-0000-0000-000000000000';

/**
 * Resolve the v1 `agentId` from an event payload: the main agent (`"main"`,
 * the legacy nil uuid, or absent) maps onto the SDK's `main`; any other
 * agent id passes through verbatim so child-agent / /btw routing keys off
 * the engine's real id.
 */
function agentIdFromWire(payload: Record<string, unknown>): string {
  const agentId = payload['agentId'];
  if (typeof agentId !== 'string' || agentId.length === 0 || agentId === NIL_AGENT_UUID) {
    return MAIN_AGENT_ID;
  }
  return agentId;
}

/**
 * Map the wire turn-end reason onto the v1 `TurnEndReason`. `completed`,
 * `cancelled`, `failed` and `filtered` map identity. `blocked` (a hook
 * blocked the prompt — the turn is over without fulfilling the request)
 * maps onto `completed`: the v1 vocabulary has no blocked variant, the
 * TUI must still finalize the turn (queue drain, footer state), and the
 * block reason itself rides the translated `hook.result` event. An unknown
 * future reason degrades the same way rather than dropping the terminal
 * event (a dropped `turn.ended` would strand the client's turn state).
 */
function turnEndReasonFromWire(reason: unknown): 'completed' | 'cancelled' | 'failed' | 'filtered' {
  switch (reason) {
    case 'cancelled':
    case 'failed':
    case 'filtered':
      return reason;
    default:
      return 'completed';
  }
}

/** The `MirriErrorCode` strings the SDK recognizes on the wire error object. */
const SDK_ERROR_CODES = new Set<string>(Object.values(ErrorCodes));

/** Whether the wire error code is one of the SDK's `MirriErrorCode`s. */
function isSdkErrorCode(code: string): code is MirriErrorCode {
  return SDK_ERROR_CODES.has(code);
}

/** The update classes the v1 `ToolUpdate` union accepts. */
const TOOL_UPDATE_KINDS = new Set(['stdout', 'stderr', 'progress', 'status', 'custom']);

/**
 * Normalize the engine's `tool.progress` update onto the v1 `ToolUpdate`.
 * The wire shape is already camelCase and field-compatible (see
 * `rust/crates/mirri-core/src/tool.rs`), so the translation validates and
 * drops unknown shapes. Returns `undefined` for anything malformed.
 */
function wireToolUpdate(raw: unknown): ToolUpdate | undefined {
  if (!isRecord(raw)) return undefined;
  const kind = raw['kind'];
  if (typeof kind !== 'string' || !TOOL_UPDATE_KINDS.has(kind)) return undefined;
  const text = optionalString(raw['text']);
  const percent = typeof raw['percent'] === 'number' ? raw['percent'] : undefined;
  const customKind = optionalString(raw['customKind']);
  return {
    kind: kind as ToolUpdate['kind'],
    ...(text === undefined ? {} : { text }),
    ...(percent === undefined ? {} : { percent }),
    ...(customKind === undefined ? {} : { customKind }),
    ...('customData' in raw ? { customData: raw['customData'] } : {}),
  };
}

/**
 * Normalize the engine's compaction result onto the v1 `CompactionResult`.
 * The wire object is field-compatible (the engine's projection mirrors the
 * TS shape); returns `undefined` when required fields are missing or
 * mistyped.
 */
function wireCompactionResult(raw: unknown): CompactionResult | undefined {
  if (!isRecord(raw)) return undefined;
  const summary = raw['summary'];
  const compactedCount = raw['compactedCount'];
  const tokensBefore = raw['tokensBefore'];
  const tokensAfter = raw['tokensAfter'];
  if (typeof summary !== 'string') return undefined;
  if (typeof compactedCount !== 'number') return undefined;
  if (typeof tokensBefore !== 'number' || typeof tokensAfter !== 'number') return undefined;
  const droppedCount = raw['droppedCount'];
  return {
    summary,
    compactedCount,
    tokensBefore,
    tokensAfter,
    ...(typeof droppedCount === 'number' ? { droppedCount } : {}),
  };
}

/** The status strings the v1 `BackgroundTaskInfo` union accepts. */
const BACKGROUND_TASK_STATUSES = new Set([
  'running',
  'completed',
  'failed',
  'timed_out',
  'killed',
  'lost',
]);

/**
 * Normalize the engine's background-task snapshot onto the v1
 * `BackgroundTaskInfo` union. The wire is already a `kind`-tagged union
 * (see `rust/crates/mirri-core/src/task.rs`); the translation fills the
 * process kind's required fields the engine never tracks (`pid` → 0,
 * `command` → `''` when absent, `exitCode` → `null` when absent — the
 * TUI treats those as display-only). Returns `undefined` for anything
 * malformed.
 */
function wireBackgroundTaskInfo(raw: unknown): BackgroundTaskInfoValue | undefined {
  if (!isRecord(raw)) return undefined;
  const taskId = raw['taskId'];
  const description = raw['description'];
  const status = raw['status'];
  const startedAt = raw['startedAt'];
  const endedAt = raw['endedAt'];
  if (typeof taskId !== 'string' || taskId.length === 0) return undefined;
  if (typeof description !== 'string') return undefined;
  if (typeof status !== 'string' || !BACKGROUND_TASK_STATUSES.has(status)) return undefined;
  if (typeof startedAt !== 'number') return undefined;
  if (typeof endedAt !== 'number' && endedAt !== null) return undefined;
  const base = {
    taskId,
    description,
    status: status as BackgroundTaskInfoValue['status'],
    startedAt,
    endedAt,
    ...(optionalString(raw['stopReason']) === undefined
      ? {}
      : { stopReason: raw['stopReason'] as string }),
  };
  switch (raw['kind']) {
    case 'process':
      return {
        kind: 'process',
        ...base,
        command: typeof raw['command'] === 'string' ? raw['command'] : '',
        pid: typeof raw['pid'] === 'number' ? raw['pid'] : 0,
        exitCode: typeof raw['exitCode'] === 'number' ? raw['exitCode'] : null,
      };
    case 'agent': {
      const agentId = optionalString(raw['agentId']);
      const subagentType = optionalString(raw['subagentType']);
      return {
        kind: 'agent',
        ...base,
        ...(agentId === undefined ? {} : { agentId }),
        ...(subagentType === undefined ? {} : { subagentType }),
      };
    }
    case 'question': {
      const questionCount = raw['questionCount'];
      if (typeof questionCount !== 'number') return undefined;
      return { kind: 'question', ...base, questionCount };
    }
    default:
      return undefined;
  }
}

/** The v1 background-task info union, via the event type that carries it. */
type BackgroundTaskInfoValue = BackgroundTaskStartedEvent['info'];

/** The v1 `TokenUsage`, via the event type that carries it. */
type TokenUsageValue = NonNullable<SubagentCompletedEvent['usage']>;

/**
 * Normalize the engine's token-usage wire object (the shared projection's
 * `inputOther`/`output`/`inputCacheRead`/`inputCacheCreation` spelling)
 * onto the v1 `TokenUsage`. Returns `undefined` for anything malformed.
 */
function wireTokenUsage(raw: unknown): TokenUsageValue | undefined {
  if (!isRecord(raw)) return undefined;
  const inputOther = raw['inputOther'];
  const output = raw['output'];
  const inputCacheRead = raw['inputCacheRead'];
  const inputCacheCreation = raw['inputCacheCreation'];
  if (
    typeof inputOther !== 'number' ||
    typeof output !== 'number' ||
    typeof inputCacheRead !== 'number' ||
    typeof inputCacheCreation !== 'number'
  ) {
    return undefined;
  }
  return { inputOther, output, inputCacheRead, inputCacheCreation };
}

/** The v1 `GoalSnapshot`, via the event type that carries it. */
type GoalSnapshotValue = NonNullable<GoalUpdatedEvent['snapshot']>;

/**
 * Normalize the engine's goal snapshot onto the v1 `GoalSnapshot`. The
 * wire `status` vocabulary matches the protocol union (`complete`, not
 * the wire-log op spelling); the budget member accepts the engine's
 * limits-only spelling (`{turns?, tokens?, wallClockMs?}` — the derived
 * report fields default to null/false, the same degradation the web
 * projector applies). Returns `undefined` for a malformed object.
 */
function wireGoalSnapshot(raw: unknown): GoalSnapshotValue | undefined {
  if (!isRecord(raw)) return undefined;
  const goalId = raw['goalId'];
  const objective = raw['objective'];
  const status = raw['status'];
  if (typeof goalId !== 'string' || goalId.length === 0) return undefined;
  if (typeof objective !== 'string') return undefined;
  if (status !== 'active' && status !== 'paused' && status !== 'blocked' && status !== 'complete') {
    return undefined;
  }
  const turnsUsed = raw['turnsUsed'];
  const tokensUsed = raw['tokensUsed'];
  const wallClockMs = raw['wallClockMs'];
  if (
    typeof turnsUsed !== 'number' ||
    typeof tokensUsed !== 'number' ||
    typeof wallClockMs !== 'number'
  ) {
    return undefined;
  }
  const completionCriterion = optionalString(raw['completionCriterion']);
  const terminalReason = optionalString(raw['terminalReason']);
  const budgetRaw = isRecord(raw['budget']) ? raw['budget'] : undefined;
  const tokenBudget = typeof budgetRaw?.['tokens'] === 'number' ? budgetRaw['tokens'] : null;
  const turnBudget = typeof budgetRaw?.['turns'] === 'number' ? budgetRaw['turns'] : null;
  const wallClockBudgetMs =
    typeof budgetRaw?.['wallClockMs'] === 'number' ? budgetRaw['wallClockMs'] : null;
  return {
    goalId,
    objective,
    ...(completionCriterion === undefined ? {} : { completionCriterion }),
    status,
    turnsUsed,
    tokensUsed,
    wallClockMs,
    budget: {
      tokenBudget,
      turnBudget,
      wallClockBudgetMs,
      remainingTokens:
        tokenBudget === null ? null : Math.max(0, tokenBudget - tokensUsed),
      remainingTurns: turnBudget === null ? null : Math.max(0, turnBudget - turnsUsed),
      remainingWallClockMs: wallClockBudgetMs === null ? null : Math.max(0, wallClockBudgetMs - wallClockMs),
      tokenBudgetReached: tokenBudget !== null && tokensUsed >= tokenBudget,
      turnBudgetReached: turnBudget !== null && turnsUsed >= turnBudget,
      wallClockBudgetReached: wallClockBudgetMs !== null && wallClockMs >= wallClockBudgetMs,
      overBudget:
        (tokenBudget !== null && tokensUsed >= tokenBudget) ||
        (turnBudget !== null && turnsUsed >= turnBudget) ||
        (wallClockBudgetMs !== null && wallClockMs >= wallClockBudgetMs),
    },
    ...(terminalReason === undefined ? {} : { terminalReason }),
  };
}

/**
 * Normalize the engine's goal change onto the v1 `GoalChange`. Returns
 * `undefined` for a malformed object; the optional members stay absent
 * when the wire omits them.
 */
function wireGoalChange(raw: unknown): GoalUpdatedEvent['change'] | undefined {
  if (!isRecord(raw)) return undefined;
  const kind = raw['kind'];
  if (kind !== 'lifecycle' && kind !== 'completion') return undefined;
  const status = raw['status'];
  if (
    status !== undefined &&
    status !== 'active' &&
    status !== 'paused' &&
    status !== 'blocked' &&
    status !== 'complete'
  ) {
    return undefined;
  }
  const actor = raw['actor'];
  if (
    actor !== undefined &&
    actor !== 'user' &&
    actor !== 'model' &&
    actor !== 'runtime' &&
    actor !== 'system'
  ) {
    return undefined;
  }
  const statsRaw = isRecord(raw['stats']) ? raw['stats'] : undefined;
  const turnsUsed = statsRaw?.['turnsUsed'];
  const tokensUsed = statsRaw?.['tokensUsed'];
  const wallClockMs = statsRaw?.['wallClockMs'];
  const stats =
    statsRaw !== undefined &&
    typeof turnsUsed === 'number' &&
    typeof tokensUsed === 'number' &&
    typeof wallClockMs === 'number'
      ? { turnsUsed, tokensUsed, wallClockMs }
      : undefined;
  const reason = optionalString(raw['reason']);
  return {
    kind,
    ...(status === undefined ? {} : { status }),
    ...(reason === undefined ? {} : { reason }),
    ...(stats === undefined ? {} : { stats }),
    ...(actor === undefined ? {} : { actor }),
  };
}

/** The v1 `SkillSource` vocabulary (the wire carries the same strings). */
function wireSkillSource(raw: unknown): SkillActivatedEvent['skillSource'] | undefined {
  if (raw === 'project' || raw === 'user' || raw === 'extra' || raw === 'builtin') return raw;
  return undefined;
}

/**
 * Normalize the engine's MCP server status payload onto the v1
 * `McpServerStatusPayload`. Returns `undefined` for anything malformed.
 */
function wireMcpServerStatus(
  raw: unknown,
): NonNullable<McpServerStatusEvent['server']> | undefined {
  if (!isRecord(raw)) return undefined;
  const name = raw['name'];
  const transport = raw['transport'];
  const status = raw['status'];
  const toolCount = raw['toolCount'];
  if (typeof name !== 'string' || name.length === 0) return undefined;
  if (transport !== 'stdio' && transport !== 'http' && transport !== 'sse') return undefined;
  if (
    status !== 'pending' &&
    status !== 'connected' &&
    status !== 'failed' &&
    status !== 'disabled' &&
    status !== 'needs-auth'
  ) {
    return undefined;
  }
  if (typeof toolCount !== 'number') return undefined;
  const pool = raw['pool'];
  if (
    pool !== 'user' &&
    pool !== 'workspace' &&
    pool !== 'plugin' &&
    pool !== 'session'
  ) {
    return undefined;
  }
  const changeKindRaw = raw['changeKind'];
  if (
    changeKindRaw !== undefined &&
    changeKindRaw !== 'status' &&
    changeKindRaw !== 'added' &&
    changeKindRaw !== 'removed' &&
    changeKindRaw !== 'reconfigured' &&
    changeKindRaw !== 'field-change'
  ) {
    return undefined;
  }
  const error = optionalString(raw['error']);
  return {
    name,
    transport,
    status,
    toolCount,
    pool,
    ...(changeKindRaw === undefined ? {} : { changeKind: changeKindRaw }),
    ...(error === undefined ? {} : { error }),
  };
}

/**
 * Normalize the engine's session-metadata patch onto the v1
 * `SessionMetaUpdatedEvent.patch` (a free-form record). Returns
 * `undefined` for a non-object.
 */
function wireMetaPatch(raw: unknown): Record<string, unknown> | undefined {
  return isRecord(raw) ? { ...raw } : undefined;
}

/**
 * Normalize the engine's cron-job origin onto the v1 `CronJobOrigin`
 * (the wire spelling the `cron.fired` payload carries). Returns
 * `undefined` for anything malformed.
 */
function wireCronJobOrigin(raw: unknown): CronFiredEvent['origin'] | undefined {
  if (!isRecord(raw)) return undefined;
  if (raw['kind'] !== 'cron_job') return undefined;
  const jobId = raw['jobId'];
  const cron = raw['cron'];
  const recurring = raw['recurring'];
  const coalescedCount = raw['coalescedCount'];
  const stale = raw['stale'];
  if (typeof jobId !== 'string' || jobId.length === 0) return undefined;
  if (typeof cron !== 'string' || cron.length === 0) return undefined;
  if (typeof recurring !== 'boolean') return undefined;
  if (typeof coalescedCount !== 'number') return undefined;
  if (typeof stale !== 'boolean') return undefined;
  return { kind: 'cron_job', jobId, cron, recurring, coalescedCount, stale };
}

// ---------------------------------------------------------------------------
// Reverse RPC
// ---------------------------------------------------------------------------

/**
 * Map the engine's `approval.ask` params onto the SDK `ApprovalRequest`.
 *
 * Wire shape (see `turn_flow/hooks.rs`): `{ tool_name, tool_call_id, action,
 * display, reason }` — snake_case, with `display` the externally-tagged
 * serialization of `mirri_core::ToolInputDisplay`. The wire carries no turn
 * id, so `turnId` stays unset. Returns `undefined` when the params are not a
 * well-formed approval request.
 */
export function sdkApprovalRequestFromWire(params: unknown): ApprovalRequest | undefined {
  if (!isRecord(params)) return undefined;
  const toolCallId = params['tool_call_id'];
  const toolName = params['tool_name'];
  const action = params['action'];
  if (
    typeof toolCallId !== 'string' ||
    toolCallId.length === 0 ||
    typeof toolName !== 'string' ||
    toolName.length === 0 ||
    typeof action !== 'string'
  ) {
    return undefined;
  }
  return {
    toolCallId,
    toolName,
    action,
    display: normalizeWireDisplay(params['display']),
  };
}

/**
 * Map the SDK `ApprovalResponse` back onto the engine's `approval.ask`
 * result. The engine reads `decision` (`"approved"` / anything else) and
 * `feedback` (used as the block reason when not approved).
 */
export function approvalResponseToWire(response: ApprovalResponse): Record<string, unknown> {
  const wire: Record<string, unknown> = { decision: response.decision };
  if (response.feedback !== undefined) wire['feedback'] = response.feedback;
  if (response.selectedLabel !== undefined) wire['selected_label'] = response.selectedLabel;
  return wire;
}

/**
 * Map the engine's `question.ask` params onto the SDK `QuestionRequest`.
 *
 * Wire shape (the `AskUserQuestion` tool input): `{ questions: [{ question,
 * header?, options?: [{ label, description? }], multi_select? }] }` — already
 * camelCase-compatible, so the translation only renames `multi_select` and
 * defaults a missing `options` array to `[]`. The wire carries no turn/tool
 * call id, so those stay unset. Returns `undefined` for a malformed payload.
 */
export function sdkQuestionRequestFromWire(params: unknown): QuestionRequest | undefined {
  if (!isRecord(params)) return undefined;
  const rawQuestions = params['questions'];
  if (!Array.isArray(rawQuestions) || rawQuestions.length === 0) return undefined;
  const questions: QuestionItem[] = [];
  for (const raw of rawQuestions) {
    const item = questionItemFromWire(raw);
    if (item === undefined) return undefined;
    questions.push(item);
  }
  return { questions };
}

function questionItemFromWire(raw: unknown): QuestionItem | undefined {
  if (!isRecord(raw)) return undefined;
  const question = raw['question'];
  if (typeof question !== 'string' || question.length === 0) return undefined;

  const options: { label: string; description?: string }[] = [];
  if (raw['options'] !== undefined) {
    if (!Array.isArray(raw['options'])) return undefined;
    for (const rawOption of raw['options']) {
      if (!isRecord(rawOption)) return undefined;
      const label = rawOption['label'];
      if (typeof label !== 'string' || label.length === 0) return undefined;
      const description = optionalString(rawOption['description']);
      options.push(description === undefined ? { label } : { label, description });
    }
  }

  const header = optionalString(raw['header']);
  const multiSelect = typeof raw['multi_select'] === 'boolean' ? raw['multi_select'] : undefined;
  return {
    question,
    ...(header === undefined ? {} : { header }),
    options,
    ...(multiSelect === undefined ? {} : { multiSelect }),
  };
}

/**
 * Normalize the engine's `ToolInputDisplay` serialization onto the SDK's
 * camelCase discriminated union (`{ kind: 'command', command: 'ls' }`).
 *
 * Two wire dialects are accepted, in precedence order:
 *
 * 1. **Internally tagged** (current engines, the protocol contract in
 *    `docs/specs/rust-core-design/01-protocol-unification.md` $defs):
 *    `{ "kind": "file_io", "path": ..., "operation": ... }` with snake_case
 *    variant names and optional keys omitted. Detected by the `kind` string.
 * 2. **Externally tagged** (older engines): a single PascalCase variant key
 *    plus a snake_case field object, e.g. `{ "FileIo": { "path": ... } }`.
 *
 * Every variant the SDK's `ToolInputDisplay` union can express is mapped;
 * anything else degrades to a `generic` display carrying the raw payload in
 * `detail`, so an approval prompt can never crash on an unknown display
 * shape. Fields the Rust variant cannot provide (e.g. `mode` of
 * `goal_start`) get a safe default.
 */
export function normalizeWireDisplay(raw: unknown): ToolInputDisplay {
  if (!isRecord(raw)) return genericDisplay(raw);
  if (typeof raw['kind'] === 'string') return internallyTaggedDisplay(raw);
  return externallyTaggedDisplay(raw);
}

/**
 * Parse the internally tagged dialect (`{ kind: ... }` + flat snake_case
 * fields). Wire keys that differ from the SDK union (`agent_name`,
 * `skill_name`, `query`/`scope`, `completion_criterion`) are renamed here;
 * fields the Rust variant omits but the union requires default to `''`.
 */
function internallyTaggedDisplay(raw: Record<string, unknown>): ToolInputDisplay {
  switch (raw['kind']) {
    case 'command':
      return { kind: 'command', command: stringField(raw, 'command') };
    case 'file_io': {
      const operation = stringField(raw, 'operation');
      return {
        kind: 'file_io',
        operation: FILE_IO_OPERATIONS.has(operation)
          ? (operation as 'read' | 'write' | 'edit' | 'glob' | 'grep')
          : 'write',
        path: stringField(raw, 'path'),
      };
    }
    case 'diff':
      return {
        kind: 'diff',
        path: stringField(raw, 'path'),
        before: stringField(raw, 'before'),
        after: stringField(raw, 'after'),
      };
    case 'search': {
      const scope = optionalString(raw['scope']);
      return {
        kind: 'search',
        query: stringField(raw, 'query'),
        ...(scope === undefined ? {} : { scope }),
      };
    }
    case 'url_fetch':
      return { kind: 'url_fetch', url: stringField(raw, 'url') };
    case 'agent_call': {
      const background = raw['background'];
      return {
        kind: 'agent_call',
        agent_name: stringField(raw, 'agent_name'),
        prompt: stringField(raw, 'prompt'),
        ...(typeof background === 'boolean' ? { background } : {}),
      };
    }
    case 'skill_call': {
      const args = optionalString(raw['args']);
      return {
        kind: 'skill_call',
        skill_name: stringField(raw, 'skill_name'),
        ...(args === undefined ? {} : { args }),
      };
    }
    // The rust wire sends bare `{"kind":"todo_list"}` with no items, but the
    // SDK union requires an `items` array — an honest generic beats a
    // fabricated empty todo list, so degrade instead of inventing fields.
    case 'todo_list':
      return genericDisplay(raw);
    // The rust wire sends bare `{"kind":"background_task"}` with no task id,
    // status, or description — the SDK union requires all three, so degrade
    // to a generic display rather than fabricate an empty task row.
    case 'background_task':
      return genericDisplay(raw);
    case 'task_stop':
      return {
        kind: 'task_stop',
        task_id: stringField(raw, 'task_id'),
        task_description: stringField(raw, 'task_description'),
      };
    case 'plan_review': {
      const path = optionalString(raw['path']);
      const options: { label: string; description: string }[] = [];
      if (Array.isArray(raw['options'])) {
        for (const rawOption of raw['options']) {
          if (!isRecord(rawOption)) continue;
          // The wire option's `description` is optional; the SDK union
          // requires it, so default to `''` when the engine omitted it.
          options.push({
            label: stringField(rawOption, 'label'),
            description: stringField(rawOption, 'description'),
          });
        }
      }
      return {
        kind: 'plan_review',
        plan: stringField(raw, 'plan'),
        ...(path === undefined ? {} : { path }),
        ...(options.length === 0 ? {} : { options }),
      };
    }
    case 'goal_start': {
      const completionCriterion = optionalString(raw['completion_criterion']);
      // The rust engine carries no permission mode into the display, and the
      // SDK union requires one. `manual` is the safe start menu (`auto` never
      // reaches this display, because it auto-approves without a prompt).
      return {
        kind: 'goal_start',
        objective: stringField(raw, 'objective'),
        ...(completionCriterion === undefined ? {} : { completionCriterion }),
        mode: 'manual',
      };
    }
    case 'generic':
      return { kind: 'generic', summary: stringField(raw, 'summary') };
    default:
      return genericDisplay(raw);
  }
}

/**
 * Parse the legacy externally tagged dialect (PascalCase variant key plus a
 * snake_case field object) that older engines serialized. Kept as a fallback
 * so displays from pre-protocol engines still translate.
 */
function externallyTaggedDisplay(raw: Record<string, unknown>): ToolInputDisplay {
  const entries = Object.entries(raw);
  if (entries.length !== 1) return genericDisplay(raw);
  const first = entries[0];
  if (first === undefined) return genericDisplay(raw);
  const [variant, fields] = first;
  if (!isRecord(fields)) return genericDisplay(raw);
  switch (variant) {
    case 'Command':
      return { kind: 'command', command: stringField(fields, 'command') };
    case 'FileIo': {
      const operation = stringField(fields, 'operation');
      return {
        kind: 'file_io',
        operation: FILE_IO_OPERATIONS.has(operation)
          ? (operation as 'read' | 'write' | 'edit' | 'glob' | 'grep')
          : 'write',
        path: stringField(fields, 'path'),
      };
    }
    case 'Diff':
      return {
        kind: 'diff',
        path: stringField(fields, 'path'),
        before: stringField(fields, 'before'),
        after: stringField(fields, 'after'),
      };
    case 'Search': {
      const scope = optionalString(fields['path']);
      return {
        kind: 'search',
        query: stringField(fields, 'pattern'),
        ...(scope === undefined ? {} : { scope }),
      };
    }
    case 'UrlFetch':
      return { kind: 'url_fetch', url: stringField(fields, 'url') };
    case 'AgentCall':
      return {
        kind: 'agent_call',
        agent_name: stringField(fields, 'agent_type'),
        prompt: stringField(fields, 'prompt'),
      };
    case 'SkillCall': {
      const args = optionalString(fields['args']);
      return {
        kind: 'skill_call',
        skill_name: stringField(fields, 'skill'),
        ...(args === undefined ? {} : { args }),
      };
    }
    case 'TaskStop':
      return {
        kind: 'task_stop',
        task_id: stringField(fields, 'task_id'),
        task_description: stringField(fields, 'task_description'),
      };
    default:
      return genericDisplay(raw);
  }
}

/** The operations the SDK `file_io` display union accepts. */
const FILE_IO_OPERATIONS = new Set(['read', 'write', 'edit', 'glob', 'grep']);

/** The typed field value, or `''` when the wire omitted it. */
function stringField(fields: Record<string, unknown>, key: string): string {
  const value = fields[key];
  return typeof value === 'string' ? value : '';
}

function optionalString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function genericDisplay(raw: unknown): ToolInputDisplay {
  return { kind: 'generic', summary: 'Tool call', detail: raw };
}