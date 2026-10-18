/**
 * `agentRPCService` — the per-agent RPC surface. Mirrors the `AgentAPI`
 * subset of `agent-core-v2/agent/rpc/core-api.ts`; every method takes one
 * payload object. Only the methods still implemented by the engine's RPC
 * facade live here — the domain services the facade calls directly
 * (shellCommand / profile / usage / plan / task) have their own contracts in
 * `agent/services.ts`, reusing the payload/result schemas below.
 * `PromptPayload.input` mirrors the `PromptPart` subset of `ContentPart`
 * (text / image_url / video_url) from `agent-core-v2/kosong/contract/message.ts`.
 * Task wire shapes mirror the `TaskInfo` union in `protocol/src/events.ts`.
 */

import { z } from 'zod';

import { maybe, noResult } from '../helpers.js';
import type { ServiceContract } from '../types.js';

// ── prompt parts ────────────────────────────────────────────────────────────

const textPartSchema = z.object({
  type: z.literal('text'),
  text: z.string(),
});

const imageUrlPartSchema = z.object({
  type: z.literal('image_url'),
  imageUrl: z.object({ url: z.string(), id: z.string().optional() }),
});

const videoUrlPartSchema = z.object({
  type: z.literal('video_url'),
  videoUrl: z.object({ url: z.string(), id: z.string().optional() }),
});

/** `PromptPart = Extract<ContentPart, { type: 'text' | 'image_url' | 'video_url' }>`. */
export const promptPartSchema = z.discriminatedUnion('type', [
  textPartSchema,
  imageUrlPartSchema,
  videoUrlPartSchema,
]);

// ── payloads / results ──────────────────────────────────────────────────────

export const emptyPayloadSchema = z.object({});

export const promptPayloadSchema = z.object({
  input: z.array(promptPartSchema),
  // Mirrors `PromptPayload.disabledTools` in the engine (client-managed
  // session denylist, full-replace).
  disabledTools: z.array(z.string()).optional(),
});

/** Same shape as `SteerPayload` in the engine. */
export const steerPayloadSchema = z.object({
  input: z.array(promptPartSchema),
});

/** Same shape as `ActivateSkillPayload` in the engine. */
export const activateSkillPayloadSchema = z.object({
  name: z.string(),
  args: z.string().optional(),
});

/**
 * Same shape as `UndoHistoryPayload` in the engine (`count` user messages
 * to undo; the result is how many were removed). B1 of the
 * rust-v2-method-parity program: AgentAPI parity backfill.
 */
export const undoHistoryPayloadSchema = z.object({
  count: z.number(),
});

/** Same shape as `RunCommandPayload` in the engine. */
export const runCommandPayloadSchema = z.object({
  name: z.string(),
  args: z.string().optional(),
});

/**
 * Same shape as `ActivatePluginCommandPayload` in the engine. B1 of the
 * rust-v2-method-parity program: AgentAPI parity backfill.
 */
export const activatePluginCommandPayloadSchema = z.object({
  pluginId: z.string(),
  commandName: z.string(),
  args: z.string().optional(),
});

/**
 * `ToolSource` (agent-core-v2 `toolContract`) — where a tool is
 * contributed from.
 */
export const toolSourceSchema = z.enum(['builtin', 'user', 'mcp']);

/**
 * `ToolInfo` (agent-core-v2 `toolContract`) — the element of AgentAPI
 * `getTools`. B1 of the rust-v2-method-parity program: AgentAPI parity
 * backfill.
 */
export const toolInfoSchema = z.object({
  name: z.string(),
  description: z.string(),
  parameters: z.record(z.string(), z.unknown()).optional(),
  source: toolSourceSchema,
  disclosure: z.enum(['inline', 'deferred']).optional(),
  info: z.record(z.string(), z.unknown()).optional(),
});

/** Same shape as `AgentCommandInfo` in the engine. */
export const agentCommandInfoSchema = z.object({
  name: z.string(),
  description: z.string().optional(),
  source: z.string(),
});

export const promptLaunchResultSchema = z.object({
  turn_id: z.number(),
});

export const cancelPayloadSchema = z.object({
  turnId: z.number().optional(),
});

export const runShellCommandPayloadSchema = z.object({
  command: z.string(),
  commandId: z.string().optional(),
});

export const shellCommandResultSchema = z.object({
  stdout: z.string(),
  stderr: z.string(),
  isError: z.boolean().optional(),
  backgrounded: z.boolean().optional(),
});

export const cancelShellCommandPayloadSchema = z.object({
  commandId: z.string(),
});

export const setModelPayloadSchema = z.object({
  model: z.string(),
});

export const setModelResultSchema = z.object({
  model: z.string(),
  providerName: z.string().optional(),
});

export const permissionModeSchema = z.enum(['manual', 'yolo', 'auto']);

export const setPermissionPayloadSchema = z.object({
  mode: permissionModeSchema,
});

export const tokenUsageSchema = z.object({
  inputOther: z.number(),
  output: z.number(),
  inputCacheRead: z.number(),
  inputCacheCreation: z.number(),
});

export const usageStatusSchema = z.object({
  byModel: z.record(z.string(), tokenUsageSchema).optional(),
  currentTurn: tokenUsageSchema.optional(),
  total: tokenUsageSchema.optional(),
});

/**
 * `AgentContextData` — `history` items are full `ContextMessage`s (deep
 * `Message` / `Tool` / `PromptOrigin` unions); mirrored as `unknown` entries.
 */
export const agentContextDataSchema = z.object({
  history: z.array(z.unknown()),
  tokenCount: z.number(),
});

/** `PlanData = null | { id, content, path }` — null is JSON-representable. */
export const planDataSchema = z.union([
  z.null(),
  z.object({
    id: z.string(),
    content: z.string(),
    path: z.string(),
  }),
]);

export const cancelPlanPayloadSchema = z.object({
  id: z.string().optional(),
});

export const getTasksPayloadSchema = z.object({
  activeOnly: z.boolean().optional(),
  limit: z.number().optional(),
});

/** `TaskStatus` (mirri-core task.rs) — an adjacently-tagged enum object:
 * `{"status": "running"}`, `{"status": "completed", "detail": {"exit_code": 0}}`
 * (struct variant serializes its fields as a nested object),
 * `{"status": "failed", "detail": "…"}`, `{"status": "stopped"}`. */
const taskLifecycleStatusSchema = z.discriminatedUnion('status', [
  z.object({ status: z.literal('running') }),
  z.object({ status: z.literal('completed'), detail: z.object({ exit_code: z.number() }) }),
  z.object({ status: z.literal('failed'), detail: z.string() }),
  z.object({ status: z.literal('stopped') }),
]);

/** The `TaskKind` discriminator (mirri-core task.rs) — what kind of work a
 * background task tracks. Serialized as an internally-tagged object whose
 * fields follow the protocol union's camelCase spelling
 * (`{"kind":"process","command":"…"}`,
 * `{"kind":"agent","agentId":"…"}`,
 * `{"kind":"question","questionCount":2}`). */
export const agentTaskKindSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('process'), command: z.string().optional() }),
  z.object({
    kind: z.literal('agent'),
    agentId: z.string().optional(),
    subagentType: z.string().optional(),
  }),
  z.object({ kind: z.literal('question'), questionCount: z.number() }),
]);

/** `AgentTask` (mirri-core task.rs) serialization — the authoritative
 * `task.list` element shape served by both rust backends (TSV row 80):
 * snake_case keys, RFC3339 `created_at`, the tagged status object above.
 * `kind` (the task's union discriminator) and `ended_at` (RFC3339, `null`
 * while running) joined with the background.task live-event batch; both
 * stay optional so records persisted before the fields existed keep
 * parsing. */
export const agentTaskInfoSchema = z.object({
  id: z.string(),
  kind: agentTaskKindSchema.optional(),
  description: z.string(),
  status: taskLifecycleStatusSchema,
  created_at: z.string(),
  ended_at: z.string().nullable().optional(),
  output: z.string(),
  session_id: z.string(),
});

export const stopTaskPayloadSchema = z.object({
  taskId: z.string(),
  reason: z.string().optional(),
});

export const getTaskOutputPayloadSchema = z.object({
  taskId: z.string(),
  tail: z.number().optional(),
});

// ── contract ────────────────────────────────────────────────────────────────

export const agentRpcContract = {
  prompt: { input: z.tuple([promptPayloadSchema]), output: maybe(promptLaunchResultSchema) },
  steer: { input: z.tuple([steerPayloadSchema]), output: maybe(promptLaunchResultSchema) },
  activateSkill: {
    input: z.tuple([activateSkillPayloadSchema]),
    output: maybe(promptLaunchResultSchema),
  },
  listCommands: {
    input: z.tuple([emptyPayloadSchema]),
    output: z.array(agentCommandInfoSchema),
  },
  runCommand: {
    input: z.tuple([runCommandPayloadSchema]),
    output: noResult,
  },
  cancel: { input: z.tuple([cancelPayloadSchema]), output: noResult },
  undoHistory: { input: z.tuple([undoHistoryPayloadSchema]), output: z.number() },
  setPermission: { input: z.tuple([setPermissionPayloadSchema]), output: noResult },
  cancelCompaction: { input: z.tuple([emptyPayloadSchema]), output: noResult },
  activatePluginCommand: {
    input: z.tuple([activatePluginCommandPayloadSchema]),
    output: noResult,
  },
  getContext: { input: z.tuple([emptyPayloadSchema]), output: agentContextDataSchema },
  getTools: { input: z.tuple([emptyPayloadSchema]), output: z.array(toolInfoSchema) },
} satisfies ServiceContract;
