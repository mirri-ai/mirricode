/**
 * Single source of truth for the embedded rust engine's `event.*`
 * notification surface — the 48 projected methods of the shared event
 * projection (`rust/crates/mirri-core/src/session/event_projection.rs`:
 * `PROJECTED_EVENT_METHODS` ∪ `SERVER_PROJECTED_EVENT_METHODS`), each with
 * the zod contract its full notification `params` must satisfy.
 *
 * Every notification the engine emits is a flat envelope — `sessionId`,
 * `volatile`, `timestamp` always; `seq` + `epoch` if and only if the event
 * is durable — with the type-specific payload fields merged in after the
 * envelope keys (design `.tmp/d7-design.md` §1). The branch schemas below
 * therefore validate the FULL `params` object: `EVENT_ENVELOPE_SCHEMA`
 * extended with the branch's payload fields, all strict (unknown keys fail
 * — a renamed or stray field is drift, never silently stripped).
 *
 * Two branch families, mirroring the design §2 table:
 *
 * - **mapped** (33): branches whose type name exists in
 *   `@mirri-ai/protocol`'s event schemas. The branch is the protocol's
 *   exported zod schema (`.omit` of the in-band `type` literal — the
 *   notification method is the discriminator) with an enumerated,
 *   documented adaptation set (string wire turn ids, absent `origin`,
 *   optional `turnId`, the wire-only `agentId`,
 *   the `blocked` stop reason, the relaxed background-task process info,
 *   the optional wire `taskId` on shell output). Drift detection stays
 *   real: every other protocol field is inherited verbatim, so a protocol
 *   evolution (a new required field, a renamed one) fails CI until the
 *   projection catches up.
 * - **extension** (15): rust-only types with no protocol counterpart; the
 *   branch schemas are written once here. The three `event.workspace.*`
 *   branches are server-scoped lifecycle events (W2): their envelope
 *   relaxes `sessionId` to a plain string (the engine synthesizes the
 *   empty string — no session is involved) and they are always
 *   `volatile: true` (fire-and-forget; no per-session cursor exists).
 *
 * There is **no skip bucket and no passthrough**: an unknown `event.*`
 * method must hard-fail consumers, so the vocabulary can never silently
 * drift between the rust projection and this table (the NAPI
 * `embeddedEventMethods` export reconciles the two — see
 * `rust-engine-contract-e2e.test.ts`).
 *
 * This module is data, not logic: schemas, method names, and the
 * mapped/extension classification are declarative; the sweep logic lives
 * in the runner (`test/rust-engine-contract-e2e.test.ts`).
 */

import { z } from 'zod';

import {
  agentBackgroundTaskInfoSchema,
  agentPhaseSchema,
  agentStatusUpdatedEventSchema,
  assistantDeltaEventSchema,
  backgroundTaskStartedEventSchema,
  backgroundTaskTerminatedEventSchema,
  compactionBlockedEventSchema,
  compactionCancelledEventSchema,
  compactionCompletedEventSchema,
  compactionStartedEventSchema,
  cronFiredEventSchema,
  goalBudgetReportSchema,
  goalSnapshotSchema,
  goalUpdatedEventSchema,
  hookResultEventSchema,
  isoDateTimeSchema,
  mirriErrorPayloadSchema,
  mcpServerStatusEventSchema,
  mcpServerStatusPayloadSchema,
  pluginCommandActivatedEventSchema,
  processBackgroundTaskInfoSchema,
  promptOriginSchema,
  questionBackgroundTaskInfoSchema,
  sessionMetaUpdatedEventSchema,
  shellOutputEventSchema,
  shellStartedEventSchema,
  skillActivatedEventSchema,
  subagentCompletedEventSchema,
  subagentFailedEventSchema,
  subagentSpawnedEventSchema,
  subagentStartedEventSchema,
  subagentSuspendedEventSchema,
  thinkingDeltaEventSchema,
  toolCallDeltaEventSchema,
  toolCallStartedEventSchema,
  toolProgressEventSchema,
  toolResultEventSchema,
  turnEndedEventSchema,
  turnStartedEventSchema,
  turnStepCompletedEventSchema,
  turnStepInterruptedEventSchema,
  turnStepRetryingEventSchema,
  turnStepStartedEventSchema,
} from '@mirri-ai/protocol';

/**
 * The notification envelope every branch carries (design §1): `sessionId`,
 * `volatile`, and an RFC 3339 `timestamp` are always present; `seq` (the
 * per-session durable position) and `epoch` (the emitting server instance)
 * are present if and only if the event is durable (`volatile === false`).
 * That presence/absence cross-rule is asserted by the runner — optionality
 * here only models that both spellings parse.
 *
 * ## seq/epoch restart semantics (T5, documented contract)
 *
 * The durable seq's uniqueness domain is the `(epoch, seq)` pair, not `seq`
 * alone: `epoch` is the emitting server instance's id, and a change of
 * epoch means the per-session seq counter restarted. Concretely —
 *
 * - A same-process close→resume continues from the in-memory watermark
 *   (the stream stays monotonic under one epoch).
 * - A process restart (a new server instance) restarts the per-session
 *   seq at 1 under a new epoch — the epoch change is the consumer's
 *   seq-reset signal, never to be mistaken for a gap. Persisting the
 *   watermark across restarts would take a new wire domain (recorded as a
 *   followup candidate); consumers that care about cross-restart
 *   continuity key on `(epoch, seq)`.
 */
export const EVENT_ENVELOPE_SCHEMA = z.strictObject({
  sessionId: z.string().min(1),
  // Relay seq is stamped increment-before-use, so the first durable event
  // of a session carries 1 (never 0).
  seq: z.number().int().positive().optional(),
  epoch: z.string().optional(),
  volatile: z.boolean(),
  timestamp: isoDateTimeSchema,
});

/**
 * The thirty-three `event.*` methods whose type name exists in the protocol's event
 * schemas — their branches derive from the protocol exports (see the
 * module doc).
 */
export const MAPPED_EVENT_METHODS = [
  'event.agent.status.updated',
  'event.assistant.delta',
  'event.background.task.started',
  'event.background.task.terminated',
  'event.compaction.blocked',
  'event.compaction.cancelled',
  'event.compaction.completed',
  'event.compaction.started',
  'event.cron.fired',
  'event.goal.updated',
  'event.hook.result',
  'event.mcp.server.status',
  'event.plugin_command.activated',
  'event.session.meta.updated',
  'event.shell.output',
  'event.shell.started',
  'event.skill.activated',
  'event.subagent.completed',
  'event.subagent.failed',
  'event.subagent.spawned',
  'event.subagent.started',
  'event.subagent.suspended',
  'event.thinking.delta',
  'event.tool.call.delta',
  'event.tool.call.started',
  'event.tool.progress',
  'event.tool.result',
  'event.turn.ended',
  'event.turn.started',
  'event.turn.step.completed',
  'event.turn.step.interrupted',
  'event.turn.step.retrying',
  'event.turn.step.started',
] as const;

/**
 * The fifteen `event.*` methods with no protocol counterpart — rust-only types
 * whose branch schemas are hand-written in this table.
 */
export const EXTENSION_EVENT_METHODS = [
  'event.agent.created',
  'event.agent.removed',
  'event.approval.requested',
  'event.approval.resolved',
  'event.message.received',
  'event.question.answered',
  'event.question.dismissed',
  'event.question.requested',
  'event.session.deleted',
  'event.session.model_switched',
  'event.session.updated',
  'event.session.work_changed',
  'event.shell.completed',
  'event.workspace.created',
  'event.workspace.deleted',
  'event.workspace.updated',
] as const;

/** Every projected event method (48 = 33 mapped + 15 extension). */
export type RustEngineEventMethod =
  | (typeof MAPPED_EVENT_METHODS)[number]
  | (typeof EXTENSION_EVENT_METHODS)[number];

/** The closed 48-method vocabulary, as one list. */
export const EVENT_METHODS: readonly RustEngineEventMethod[] = [
  ...MAPPED_EVENT_METHODS,
  ...EXTENSION_EVENT_METHODS,
];

/**
 * The rust wire's process-kind background task info, narrowed from the
 * protocol's `processBackgroundTaskInfoSchema`: the engine never tracks the
 * child process's pid, the command is absent only for legacy persisted tasks
 * (which are terminal after a restart and never emit live), and `exitCode` is
 * stamped only on completed process tasks — the protocol's required
 * `command`/`pid` fields and the `number | null` exitCode all relax to
 * optional on the rust wire. Every other protocol field (the kind
 * discriminator, the status vocabulary, the agent/question members) is
 * inherited verbatim.
 */
const rustProcessBackgroundTaskInfoSchema = processBackgroundTaskInfoSchema.extend({
  command: z.string().optional(),
  pid: z.number().optional(),
  exitCode: z.number().nullable().optional(),
});

/** The rust wire's `BackgroundTaskInfo` — the protocol union with only the
 * process member's enumerated relaxations. */
const rustBackgroundTaskInfoSchema = z.discriminatedUnion('kind', [
  rustProcessBackgroundTaskInfoSchema,
  agentBackgroundTaskInfoSchema,
  questionBackgroundTaskInfoSchema,
]);

/** The wire `event.turn.ended` reason vocabulary: the protocol's
 * `turnEndReasonSchema` enum extended with `blocked` (the rust
 * `TurnStopReason::BlockedByHook` mapping; v2's `TurnEndReason` attests the
 * extension — design §0 #7).
 */
const turnEndedReasonSchema = z.enum([
  'completed',
  'cancelled',
  'failed',
  'filtered',
  'blocked',
]);

/**
 * The rust wire's goal budget: the protocol's full `GoalBudgetReport` OR
 * the engine's limits-only spelling (`{turns?, tokens?, wallClockMs?}`,
 * null when no budget is set) — the engine's snapshot projects the raw
 * limits and leaves the report's derived fields to consumers.
 */
const rustGoalBudgetSchema = z.union([
  goalBudgetReportSchema,
  z
    .object({
      turns: z.number().optional(),
      tokens: z.number().optional(),
      wallClockMs: z.number().optional(),
    })
    .nullable(),
]);

/** The rust wire's `GoalSnapshot` — the protocol schema with the budget
 * member's enumerated relaxation. */
const rustGoalSnapshotSchema = goalSnapshotSchema.extend({
  budget: rustGoalBudgetSchema,
});

/**
 * Every branch's full-params contract: the strict envelope extended with
 * the branch's payload fields (all strict — unknown keys fail). The record
 * is exhaustive over {@link RustEngineEventMethod}: a method added to the
 * lists above fails typecheck until its branch lands here, and the
 * contract suite reconciles the whole table against the rust projection's
 * `embeddedEventMethods` export.
 */
export const EVENT_BRANCHES: Readonly<Record<RustEngineEventMethod, z.ZodType>> = {
  // --- mapped branches (protocol schemas + enumerated adaptations) ------
  // Each is the protocol schema minus its in-band `type` literal (the
  // method is the discriminator), adapted to the embedded wire.

  // `turnStartedEventSchema` adapted: the wire turn id is the engine's
  // `turn-<uuid>` string, no origin exists on the variant, and the
  // wire-only `agentId` carries the turn-owning agent's string identity
  // (`"main"` / `"agent-<uuid>"` — never a UUID conversion of `"main"`)
  // so clients can route the opening bracket to the right agent.
  'event.turn.started': EVENT_ENVELOPE_SCHEMA.extend(
    turnStartedEventSchema
      .omit({ type: true })
      .extend({
        agentId: z.string(),
        turnId: z.string(),
        origin: promptOriginSchema.optional(),
      }).shape,
  ),
  // `turnEndedEventSchema` adapted: string turn id, the `blocked` reason,
  // and the wire-only `agentId` (required — both the completed and the
  // failed spelling carry the turn-owning agent's identity; the failed one
  // alongside the structured error object). The error object's `code` is
  // relaxed to a free string: the embedded engine reports the failure's
  // real domain code, which extends the protocol's `MirriErrorCode`
  // vocabulary with the rust-specific namespaces (`abort.*`,
  // `permission.*`, `hook.*`, `tool.*`) — a protocol-restricted enum would
  // force the projection back to a generic `internal`, hiding the failure
  // class from clients.
  'event.turn.ended': EVENT_ENVELOPE_SCHEMA.extend(
    turnEndedEventSchema
      .omit({ type: true, error: true })
      .extend({
        turnId: z.string(),
        reason: turnEndedReasonSchema,
        agentId: z.string(),
        error: mirriErrorPayloadSchema
          .extend({ code: z.string() })
          .optional(),
        // The embedded engine stamps the wire turn id (the small integer
        // the TS engine used) alongside the uuid `turnId`; clients address
        // turns by either.
        wireTurnId: z.number().optional(),
      }).shape,
  ),
  // `assistantDeltaEventSchema` adapted: the chunk carries no turn id, and
  // the session-scoped wire carries the agent's uuid.
  'event.assistant.delta': EVENT_ENVELOPE_SCHEMA.extend(
    assistantDeltaEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `thinkingDeltaEventSchema` adapted: the reasoning chunk carries no turn
  // id and the wire turn id is the string form, plus the agent's uuid —
  // the same adaptation the assistant delta branch applies.
  'event.thinking.delta': EVENT_ENVELOPE_SCHEMA.extend(
    thinkingDeltaEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `toolCallDeltaEventSchema` adapted: the streamed fragment carries no turn
  // id (the live callback carries none — same rule as the started/progress
  // branches) and the session-scoped wire carries the agent's uuid.
  'event.tool.call.delta': EVENT_ENVELOPE_SCHEMA.extend(
    toolCallDeltaEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `toolCallStartedEventSchema` adapted: optional turn id (the live
  // callback carries none) plus the wire-only `agentId` (the tool-calling
  // agent's identity — the same adaptation the delta branches apply; the
  // projection has stamped it since the wire alignment batch);
  // `args: z.unknown()` is inherited — the wire now carries the full parsed
  // argument object (field-level alignment with the TS emit), and the
  // unknown-key optionality keeps the branch tolerant of its absence.
  'event.tool.call.started': EVENT_ENVELOPE_SCHEMA.extend(
    toolCallStartedEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `toolProgressEventSchema` adapted: the same absent-turnId rule as the
  // started branch — the live callback carries no wire turn id — plus the
  // same wire-only `agentId` the started branch carries.
  'event.tool.progress': EVENT_ENVELOPE_SCHEMA.extend(
    toolProgressEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `toolResultEventSchema` adapted: optional turn id (`output:
  // z.unknown()` likewise tolerates its absence) and the wire-only
  // `agentId` (the started branch's rule — the result is attributed to the
  // same agent that opened the call); the optional `name` is inherited
  // from the protocol — the rust wire dialect carries it so a consumer
  // that missed the started event still attributes the result.
  'event.tool.result': EVENT_ENVELOPE_SCHEMA.extend(
    toolResultEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string().optional(),
        agentId: z.string(),
      }).shape,
  ),
  // `agentStatusUpdatedEventSchema` adapted: the wire adds the agent id and
  // tightens `phase` to required (the CR4 projection always carries
  // `agentId` + `phase`; every other field is optional and absent when
  // unknown). The wire-only `agentId` keys the per-agent frame.
  'event.agent.status.updated': EVENT_ENVELOPE_SCHEMA.extend(
    agentStatusUpdatedEventSchema
      .omit({ type: true })
      .extend({
        agentId: z.string(),
        phase: agentPhaseSchema,
      }).shape,
  ),
  // `compactionStartedEventSchema` adapted: the wire adds the wire-only
  // `agentId` (the compacting agent's identity) next to the protocol's
  // trigger enum and optional instruction.
  'event.compaction.started': EVENT_ENVELOPE_SCHEMA.extend(
    compactionStartedEventSchema
      .omit({ type: true })
      .extend({ agentId: z.string() }).shape,
  ),
  // `compactionBlockedEventSchema` adapted: the optional `turnId` stays
  // absent — the engine does not plumb the wire turn id down to the turn
  // loop (the protocol type marks it optional), so the branch tolerates
  // both a number and the absent spelling — plus the wire-only `agentId`
  // (the blocked turn's agent identity).
  'event.compaction.blocked': EVENT_ENVELOPE_SCHEMA.extend(
    compactionBlockedEventSchema
      .omit({ type: true })
      .extend({
        agentId: z.string(),
        turnId: z.union([z.number(), z.string()]).optional(),
      }).shape,
  ),
  // `hookResultEventSchema` adapted: the same absent-turnId rule as the
  // blocked compaction — the hook result is published from inside the turn
  // loop, which does not hold the wire `turn-<uuid>` (the protocol type
  // marks it optional; the branch tolerates a number, the string spelling,
  // and absence).
  'event.hook.result': EVENT_ENVELOPE_SCHEMA.extend(
    hookResultEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.union([z.number(), z.string()]).optional(),
      }).shape,
  ),
  // `compactionCancelledEventSchema` adapted: the wire adds the wire-only
  // `agentId` (the cancelling agent's identity) — the protocol type itself
  // carries no fields.
  'event.compaction.cancelled': EVENT_ENVELOPE_SCHEMA.extend(
    compactionCancelledEventSchema
      .omit({ type: true })
      .extend({ agentId: z.string() }).shape,
  ),
  // `compactionCompletedEventSchema` adapted: the wire-only `agentId` (the
  // compacting agent's identity) plus the protocol's result schema
  // inherited verbatim (the engine fills summary/compactedCount/
  // tokensBefore/tokensAfter/droppedCount; the kept* optionals stay
  // absent while the engine does not track them).
  'event.compaction.completed': EVENT_ENVELOPE_SCHEMA.extend(
    compactionCompletedEventSchema
      .omit({ type: true })
      .extend({ agentId: z.string() }).shape,
  ),
  // `backgroundTaskStartedEventSchema` adapted: the `info` union's process
  // member carries the enumerated rust relaxations (see
  // rustBackgroundTaskInfoSchema) — pid/command/exitCode optional on the
  // wire; the agent/question members are inherited verbatim.
  'event.background.task.started': EVENT_ENVELOPE_SCHEMA.extend(
    backgroundTaskStartedEventSchema
      .omit({ type: true })
      .extend({ info: rustBackgroundTaskInfoSchema }).shape,
  ),
  // `backgroundTaskTerminatedEventSchema` adapted: the same relaxed `info`
  // union — `exitCode` rides only the completed process spelling,
  // `stopReason` the failed one, `endedAt` every terminated task.
  'event.background.task.terminated': EVENT_ENVELOPE_SCHEMA.extend(
    backgroundTaskTerminatedEventSchema
      .omit({ type: true })
      .extend({ info: rustBackgroundTaskInfoSchema }).shape,
  ),
  // `shellOutputEventSchema` adapted: the protocol type predates the
  // rust shell runner's task linkage — the wire adds the optional
  // `taskId` (present when the run is linked to a background task,
  // absent for pure tool-stream output) while the protocol schema has no
  // `taskId` field at all.
  'event.shell.output': EVENT_ENVELOPE_SCHEMA.extend(
    shellOutputEventSchema
      .omit({ type: true })
      .extend({ taskId: z.string().optional() }).shape,
  ),
  // `shellStartedEventSchema` adapted: nothing — the wire payload is
  // exactly the protocol's command/task id pair (the started frame
  // always carries the task that owns the run).
  'event.shell.started': EVENT_ENVELOPE_SCHEMA.extend(
    shellStartedEventSchema.omit({ type: true }).shape,
  ),
  // `subagentSpawnedEventSchema` adapted: the rust host knows the parent
  // tool call id only when the spawn came from a tool call, so the
  // protocol's required `parentToolCallId` relaxes to optional on the
  // wire (absent — never a null placeholder — when the spawn had no
  // tool-call parent).
  'event.subagent.spawned': EVENT_ENVELOPE_SCHEMA.extend(
    subagentSpawnedEventSchema
      .omit({ type: true })
      .extend({ parentToolCallId: z.string().optional() }).shape,
  ),
  // `subagentStartedEventSchema` adapted: nothing — the wire payload is
  // exactly the child's agent id.
  'event.subagent.started': EVENT_ENVELOPE_SCHEMA.extend(
    subagentStartedEventSchema.omit({ type: true }).shape,
  ),
  // `subagentSuspendedEventSchema` adapted: nothing.
  'event.subagent.suspended': EVENT_ENVELOPE_SCHEMA.extend(
    subagentSuspendedEventSchema.omit({ type: true }).shape,
  ),
  // `subagentCompletedEventSchema` adapted: the wire always carries the
  // child's token usage (the engine extracts it from the turn result),
  // and the untracked `contextTokens` stays absent.
  'event.subagent.completed': EVENT_ENVELOPE_SCHEMA.extend(
    subagentCompletedEventSchema.omit({ type: true }).shape,
  ),
  // `subagentFailedEventSchema` adapted: nothing.
  'event.subagent.failed': EVENT_ENVELOPE_SCHEMA.extend(
    subagentFailedEventSchema.omit({ type: true }).shape,
  ),
  // `turnStepStartedEventSchema` adapted: the wire turn id is the
  // `turn-<uuid>` string the dispatch minted (the same key `turn.started`
  // carries — plumbed down through `TurnFlow::set_wire_turn_id`), so the
  // protocol's numeric `turnId` becomes the wire's string spelling.
  'event.turn.step.started': EVENT_ENVELOPE_SCHEMA.extend(
    turnStepStartedEventSchema.omit({ type: true }).extend({ turnId: z.string() }).shape,
  ),
  // `turnStepCompletedEventSchema` adapted: the string wire turn id (same
  // as the started branch); the step's usage is always present on the wire
  // while the latency/finish-reason extras stay absent (the engine does
  // not track them).
  'event.turn.step.completed': EVENT_ENVELOPE_SCHEMA.extend(
    turnStepCompletedEventSchema.omit({ type: true }).extend({ turnId: z.string() }).shape,
  ),
  // `turnStepInterruptedEventSchema` adapted: the live-event relay
  // captures the wire `turn-<uuid>` turn id at dispatch, so the
  // protocol's required numeric `turnId` becomes the string spelling
  // (present on every embedded path; the branch keeps it required now
  // that the dispatch always supplies one).
  'event.turn.step.interrupted': EVENT_ENVELOPE_SCHEMA.extend(
    turnStepInterruptedEventSchema
      .omit({ type: true })
      .extend({ turnId: z.string() }).shape,
  ),
  // `turnStepRetryingEventSchema` adapted: the same string wire turn id
  // as the interrupted branch, plus the retrying step is identified by its
  // attempt counters (the live channel carries no step number). The
  // protocol's `errorName`/`errorMessage` stay absent on the wire (the SDK
  // translation synthesizes them — the engine's retry backoff reports no
  // structured error), so both relax to optional.
  'event.turn.step.retrying': EVENT_ENVELOPE_SCHEMA.extend(
    turnStepRetryingEventSchema
      .omit({ type: true })
      .extend({
        turnId: z.string(),
        step: z.number().optional(),
        errorName: z.string().optional(),
        errorMessage: z.string().optional(),
      }).shape,
  ),
  // `goalUpdatedEventSchema` adapted: the engine's snapshot carries the
  // budget limits object (`{turns?, tokens?, wallClockMs?}` — null when
  // no budget is set) instead of the protocol's full `GoalBudgetReport`;
  // the report's derived fields (remaining*, overBudget) stay the
  // consumer's to derive from the limits.
  'event.goal.updated': EVENT_ENVELOPE_SCHEMA.extend(
    goalUpdatedEventSchema
      .omit({ type: true })
      .extend({ snapshot: rustGoalSnapshotSchema.nullable() }).shape,
  ),
  // `skillActivatedEventSchema` adapted: nothing — the wire payload is
  // exactly the activation identity the RPC handler mints.
  'event.skill.activated': EVENT_ENVELOPE_SCHEMA.extend(
    skillActivatedEventSchema.omit({ type: true }).shape,
  ),
  // `mcpServerStatusEventSchema` adapted: the transport enum widens with
  // `sse` (the rust connection manager still speaks the deprecated
  // SSE transport), everything else inherited verbatim.
  'event.mcp.server.status': EVENT_ENVELOPE_SCHEMA.extend(
    mcpServerStatusEventSchema.omit({ type: true }).extend({
      server: mcpServerStatusPayloadSchema.extend({
        transport: z.enum(['stdio', 'http', 'sse']),
      }),
    }).shape,
  ),
  // `sessionMetaUpdatedEventSchema` adapted: nothing — the wire payload
  // is the optional title plus the raw camelCase patch object.
  'event.session.meta.updated': EVENT_ENVELOPE_SCHEMA.extend(
    sessionMetaUpdatedEventSchema.omit({ type: true }).shape,
  ),
  // `pluginCommandActivatedEventSchema` adapted: nothing — the wire
  // payload is exactly the activation identity.
  'event.plugin_command.activated': EVENT_ENVELOPE_SCHEMA.extend(
    pluginCommandActivatedEventSchema.omit({ type: true }).shape,
  ),
  // `cronFiredEventSchema` adapted: nothing — the wire payload is the
  // cron-job origin (the neutral `coalescedCount: 1` / `stale: false` for
  // the rust one-fire-per-due-task pass) plus the prompt.
  'event.cron.fired': EVENT_ENVELOPE_SCHEMA.extend(
    cronFiredEventSchema.omit({ type: true }).shape,
  ),

  // --- extension branches (rust-only types, hand-written) --------------

  // The rust shell runner's terminal frame — the protocol models shell
  // lifecycle only through the started/output pair (the v1 daemon fired
  // completion through the tool result); `taskId` is optional the same
  // way the output branch's is.
  'event.shell.completed': EVENT_ENVELOPE_SCHEMA.extend({
    commandId: z.string(),
    isError: z.boolean(),
    taskId: z.string().optional(),
  }),
  'event.approval.requested': EVENT_ENVELOPE_SCHEMA.extend({
    agentId: z.string(),
    requestId: z.string(),
    toolName: z.string(),
  }),
  'event.approval.resolved': EVENT_ENVELOPE_SCHEMA.extend({
    requestId: z.string(),
    approved: z.boolean(),
  }),
  'event.question.requested': EVENT_ENVELOPE_SCHEMA.extend({
    agentId: z.string(),
    requestId: z.string(),
    question: z.string(),
  }),
  'event.question.answered': EVENT_ENVELOPE_SCHEMA.extend({
    requestId: z.string(),
    answer: z.string(),
  }),
  'event.question.dismissed': EVENT_ENVELOPE_SCHEMA.extend({
    requestId: z.string(),
  }),
  'event.message.received': EVENT_ENVELOPE_SCHEMA.extend({
    agentId: z.string(),
    role: z.string(),
    contentSummary: z.string(),
  }),
  'event.session.deleted': EVENT_ENVELOPE_SCHEMA.extend({
    reason: z.string(),
  }),
  // The session's work-facts tuple transitioned (US4.4/SDR-013): durable,
  // globally fanned out — list rows update without a per-session
  // subscription. snake_case payload, same vocabulary the TS v2
  // reference emits; last_turn_reason is absent before any turn.
  'event.session.work_changed': EVENT_ENVELOPE_SCHEMA.extend({
    busy: z.boolean(),
    main_turn_active: z.boolean(),
    pending_interaction: z.enum(['none', 'approval', 'question']),
    last_turn_reason: z.enum(['completed', 'cancelled', 'failed']).optional(),
  }),
  // The rust engine's mid-session model switch: from/to are model aliases.
  'event.session.model_switched': EVENT_ENVELOPE_SCHEMA.extend({
    from: z.string(),
    to: z.string(),
  }),
  'event.session.updated': EVENT_ENVELOPE_SCHEMA.extend({
    change: z.enum(['resumed', 'forked']),
    sourceSessionId: z.string().optional(),
    newSessionId: z.string().optional(),
  }),
  'event.agent.created': EVENT_ENVELOPE_SCHEMA.extend({
    agentId: z.string(),
  }),
  'event.agent.removed': EVENT_ENVELOPE_SCHEMA.extend({
    agentId: z.string(),
  }),
  // The three server-scoped workspace lifecycle branches (W2): the engine
  // synthesizes the empty `sessionId` (the `WireEventBase`-required field,
  // but no session is involved) and emits them as volatile (fire-and-forget
  // — no per-session seq/epoch exists for server-scope events). The payload
  // carries the identity snapshot `{id, root, name?}` (name absent when the
  // workspace has none) or, for the deleted event, the `workspaceId`/`root`
  // pair.
  'event.workspace.created': EVENT_ENVELOPE_SCHEMA.extend({
    sessionId: z.string(),
    workspace: z.object({
      id: z.string(),
      root: z.string(),
      name: z.string().optional(),
    }),
  }),
  'event.workspace.updated': EVENT_ENVELOPE_SCHEMA.extend({
    sessionId: z.string(),
    workspace: z.object({
      id: z.string(),
      root: z.string(),
      name: z.string().optional(),
    }),
  }),
  'event.workspace.deleted': EVENT_ENVELOPE_SCHEMA.extend({
    sessionId: z.string(),
    workspaceId: z.string(),
    root: z.string(),
  }),
};
