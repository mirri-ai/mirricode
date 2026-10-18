// apps/mirri-web/src/api/daemon/ws-rpc-event-bridge.ts
// Pure routing/translation helpers for the WsRpcApi event channel — the
// bridge between the rust engine's unified RPC wire and the web AppEvent
// pipeline. Kept free of transport state so every rule is unit-testable.
//
// The engine sends events in the direct form: the JSON-RPC notification
// method IS the `event.<type>` name (mirri-server `ws_rpc.rs` frames the
// shared core projection verbatim). Three frame families meet here:
//   1) raw agent-core (no prefix):  assistant.delta{delta:'…'}
//   2) prefixed agent-core:         event.assistant.delta{delta:'…'}
//   3) genuine protocol events:     event.approval.requested, …
// `classifyFrame` decides the path; the projector's switch only knows
// unprefixed names, and `toAppEvent` only knows prefixed protocol names.

import { classifyFrame } from './agentEventProjector';
import type { AgentProjector, ProjectMeta } from './agentEventProjector';
import type { RpcEvent } from './json-rpc-client';
import { toAppEvent } from './mappers';
import type { WireEvent } from './wire';
import type {
  AppApprovalRequest,
  AppEvent,
  AppQuestionRequest,
  ApprovalResponse,
  QuestionResponse,
} from '../types';

// ---------------------------------------------------------------------------
// Event routing
// ---------------------------------------------------------------------------

/**
 * Route one engine event through the shared classification into AppEvents.
 *
 * Returns the events the UI should receive (possibly none). Never throws:
 * a malformed protocol payload is dropped so one bad frame cannot take down
 * the event pump.
 */
export function routeRpcEventToAppEvents(
  projector: AgentProjector,
  event: RpcEvent,
): AppEvent[] {
  // A sessionless frame is unroutable EXCEPT the server-scoped workspace
  // lifecycle events (`event.workspace.*`): they carry the synthesized
  // empty session id — the W2 unseal routes them through the same
  // toAppEvent path the session-scoped protocol events take, so the
  // multi-client workspace mirror reaches applyWorkspaceEvent.
  if (event.sessionId === undefined && !event.type.startsWith('event.workspace.')) return [];
  const sessionId = event.sessionId ?? '';
  const decision = classifyFrame(event.type, event.payload);
  if (decision.route === 'ignore') return [];
  if (decision.route === 'protocol') {
    // The thin `event.question.requested` notification carries only a summary
    // string — the full questions ride the `question.ask` reverse RPC. Fold
    // the summary into a single-question card so other clients (which never
    // see the reverse ask) can still render and resolve it.
    if (event.type === 'event.question.requested') {
      const payload = asRecord(event.payload);
      const question = typeof payload?.['question'] === 'string' ? payload['question'] : '';
      const requestId = typeof payload?.['requestId'] === 'string' ? payload['requestId'] : undefined;
      if (question === '' || requestId === undefined) return [];
      const request = questionRequestFromAskParams(
        { request_id: requestId, questions: [{ question }] },
        sessionId,
      );
      return request === null
        ? []
        : [{ type: 'questionRequested', sessionId, question: request }];
    }
    try {
      return [toAppEvent(rpcEventAsWireEvent(event))];
    } catch {
      return [];
    }
  }
  const meta: ProjectMeta | undefined =
    event.payload !== null && typeof event.payload === 'object' && 'offset' in (event.payload as Record<string, unknown>)
      ? { offset: (event.payload as Record<string, unknown>)['offset'] as number }
      : undefined;
  return projector.project(decision.agentType, event.payload, sessionId, meta);
}

/** Re-shape one RpcEvent as the WireEvent form `toAppEvent` consumes. */
function rpcEventAsWireEvent(event: RpcEvent): WireEvent {
  return {
    type: event.type,
    seq: event.seq ?? 0,
    session_id: event.sessionId ?? '',
    timestamp: new Date().toISOString(),
    payload: (event.payload ?? {}) as never,
  } as unknown as WireEvent;
}

// ---------------------------------------------------------------------------
// approval.ask / question.ask reverse-RPC translation
// ---------------------------------------------------------------------------

/**
 * Map the engine's `approval.ask` params onto a full approval request.
 *
 * Wire shape (mirri-core `turn_flow/hooks.rs`): `{ tool_name, tool_call_id,
 * action, display, reason, request_id }`. The minted `request_id` doubles as
 * the UI's approval id; it is what `approval.decide` accepts (first
 * resolution wins between the reverse-RPC reply and the decide RPC).
 * Returns `null` for a malformed payload.
 */
export function approvalRequestFromAskParams(
  params: unknown,
  sessionId: string,
): AppApprovalRequest | null {
  const p = asRecord(params);
  if (p === null) return null;
  const toolName = asString(p['tool_name']);
  const toolCallId = asString(p['tool_call_id']);
  const action = asString(p['action']);
  if (toolName === null || toolCallId === null || action === null) return null;
  const now = new Date().toISOString();
  // Honest expiry (D-13): the engine derives none — only a wire-carried
  // `expires_at` passes through, otherwise the field stays absent and no
  // countdown is rendered.
  const expiresAt = asString(p['expires_at']);
  // The engine approval-rule facet (B9): present only when the tool reported
  // one — the "always allow" upgrade persists exactly this pattern.
  const approvalRule = asString(p['approval_rule']);
  // The requesting member's identity (supervision B1): present only when the
  // flow runs as a member — the full agent id (`agent-<uuid>`), the same key
  // the subagents roster uses.
  const agentId = asString(p['agent_id']);
  return {
    approvalId: asString(p['request_id']) ?? toolCallId,
    sessionId,
    toolCallId,
    toolName,
    action,
    display: p['display'] ?? p['tool_input_display'] ?? null,
    ...(expiresAt !== null ? { expiresAt } : {}),
    ...(approvalRule !== null ? { approvalRule } : {}),
    ...(agentId !== null ? { agentId } : {}),
    createdAt: now,
  };
}

/**
 * Map the UI's approval decision onto the `approval.ask` response wire the
 * engine's permission gate reads: `decision` plus the optional feedback
 * field (mirri-core reads `feedback` as the block reason). `selected_label`
 * is deliberately NOT sent — the rust gate does not deserialize it (only the
 * v1 daemon mapper still carries it).
 */
export function approvalResponseToEngineWire(response: ApprovalResponse): Record<string, unknown> {
  const wire: Record<string, unknown> = { decision: response.decision };
  if (response.feedback !== undefined) wire['feedback'] = response.feedback;
  return wire;
}

/**
 * Registry of parked reverse-RPC ask responses, keyed by the minted request
 * id. Pure state holder (no transport) so the take-once semantics the
 * first-one-wins gate relies on are unit-testable in isolation.
 */
export class ParkedAskRegistry<TResponse> {
  private readonly asks = new Map<string, { respond: (response: TResponse) => void }>();

  /** Park the responder for one ask. */
  park(requestId: string, respond: (response: TResponse) => void): void {
    this.asks.set(requestId, { respond });
  }

  /** Take the responder for `requestId`, removing it (one ask answers once).
   *  Returns `undefined` when nothing is parked under that id. */
  take(requestId: string): ((response: TResponse) => void) | undefined {
    const parked = this.asks.get(requestId);
    if (parked === undefined) return undefined;
    this.asks.delete(requestId);
    return parked.respond;
  }

  /** Drop the parked responder for `requestId` — used when resolution
   *  (or expiry) arrives through another path and the engine's gate will
   *  never read our reply. Does NOT resolve or reject the parked ask. */
  drop(requestId: string): void {
    this.asks.delete(requestId);
  }

  /** Drop every parked responder (transport closed). */
  clear(): void {
    this.asks.clear();
  }
}

/**
 * Extract the request id from an approval/question resolution event payload.
 * Reads both spellings the wire carries today: the rust engine's camelCase
 * `requestId` (event_projection.rs) and the TS daemon's snake_case
 * `approval_id` / `question_id`. Returns `null` when no id is present.
 */
export function resolutionEventRequestId(payload: unknown): string | null {
  const p = asRecord(payload);
  if (p === null) return null;
  return asString(p['requestId']) ?? asString(p['approval_id']) ?? asString(p['question_id']);
}

/**
 * Map the engine's `question.ask` params (the AskUserQuestion tool input
 * plus the minted `request_id`) onto a question request. Item ids are
 * positionally stable (`q0`, `q1`, …) so the UI's answer map keys round-trip
 * through `questionResponseToEngineWire`. Returns `null` when the questions
 * array is missing, empty, or malformed.
 */
export function questionRequestFromAskParams(
  params: unknown,
  sessionId: string,
): AppQuestionRequest | null {
  const p = asRecord(params);
  if (p === null) return null;
  const rawQuestions = p['questions'];
  if (!Array.isArray(rawQuestions) || rawQuestions.length === 0) return null;
  const questions = [];
  for (let i = 0; i < rawQuestions.length; i++) {
    const raw = asRecord(rawQuestions[i]);
    if (raw === null) return null;
    const question = asString(raw['question']);
    if (question === null) return null;
    const options = Array.isArray(raw['options'])
      ? raw['options'].map((rawOption) => {
          const option = asRecord(rawOption);
          const label = option !== null ? asString(option['label']) : null;
          return {
            id: label ?? `opt-${i}`,
            label: label ?? '',
            ...(option !== null && asString(option['description']) !== null
              ? { description: asString(option['description']) ?? undefined }
              : {}),
          };
        })
      : [];
    questions.push({
      id: `q${i}`,
      question,
      ...(asString(raw['header']) !== null ? { header: asString(raw['header']) ?? undefined } : {}),
      options,
      // The engine always advertises the free-text Other option — matching
      // the tool's model-facing description and the TS engine's wire
      // behavior (allow_other: true on every question item).
      allowOther: true,
      ...(raw['multi_select'] === true ? { multiSelect: true } : {}),
    });
  }
  return {
    questionId: asString(p['request_id']) ?? 'question',
    sessionId,
    questions,
    createdAt: new Date().toISOString(),
  };
}

/**
 * Map the UI's question answers onto the `question.ask` response wire. The
 * engine stringifies the whole response as the tool result, so a flat
 * `answers: { questionId → text }` map renders exactly what the user chose.
 */
export function questionResponseToEngineWire(response: QuestionResponse): Record<string, unknown> {
  const answers: Record<string, string> = {};
  for (const [questionId, answer] of Object.entries(response.answers)) {
    answers[questionId] = questionAnswerText(answer);
  }
  return { answers };
}

/** Render one UI question answer as the text the tool result shows. */
function questionAnswerText(answer: QuestionResponse['answers'][string]): string {
  switch (answer.kind) {
    case 'single':
      return answer.optionId;
    case 'multi':
      return answer.optionIds.join(', ');
    case 'other':
      return answer.text;
    case 'multiWithOther':
      return [...answer.optionIds, answer.otherText].join(', ');
    case 'skipped':
      return '';
  }
}

// ---------------------------------------------------------------------------
// Small defensive helpers
// ---------------------------------------------------------------------------

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function asString(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}
