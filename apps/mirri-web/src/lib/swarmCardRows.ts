// apps/mirri-web/src/lib/swarmCardRows.ts
// Build the accordion row model for the AgentSwarm inline tool card. Pure
// function of live members (AppTask store, real-time phase) and the parsed
// `<agent_swarm_result>` payload (terminal result) — kept in plain TS so it can
// be unit-tested without mounting the component.

import type { AgentStep, AppSubagentPhase, NarrativeSegment } from '../api/types';
import type { SwarmMember } from '../composables/swarmGroups';
import type { SwarmResult, SwarmResultSubagent } from './parseSwarmResult';
import { suspendedReasonText } from './suspendedReason';
import { toolLabel, toolSummary } from './toolMeta';

export interface SwarmCardRow {
  id: string;
  name: string;
  activity: string;
  phase: AppSubagentPhase;
  body: string;
  model?: string;
  /** The member's collapsed-row timeline (ADR-012 ①), passed straight through
   *  from the live member. Absent on result-only rows: the parsed
   *  `<agent_swarm_result>` payload carries no steps or segments, so such a row
   *  has nothing to derive from and falls back to its single activity line. */
  timeline?: SwarmTimelineView;
}

function lastNonEmptyLine(text: string | undefined): string {
  if (!text) return '';
  return text.split('\n').map((l) => l.trimEnd()).filter(Boolean).at(-1) ?? '';
}

/** The mirror of `lastNonEmptyLine` (ADR-012 ②): a text segment's first line is
 *  its timeline identity — a stable summary that a streaming segment never
 *  changes, unlike a "latest line" tail. */
function firstNonEmptyLine(text: string | undefined): string {
  if (!text) return '';
  return text.split('\n').map((l) => l.trimEnd()).find(Boolean) ?? '';
}

// ---------------------------------------------------------------------------
// Collapsed-row lightweight timeline (ADR-012 ①)
// ---------------------------------------------------------------------------
// The collapsed member row no longer shows one replacing activity line: it
// shows the tail of the member's recent activity as a short element sequence
// derived from the same `steps` / `segments` the detail panel consumes. This is
// a projection of existing structured facts — no new event, no new reducer
// field (ADR-012 ②) — and it stays inside the dialog area's red line: tool
// elements carry a `toolSummary` one-line arg digest (the same digest the
// header ToolRow shows) but never tool output, and reasoning elements never
// carry reasoning text.

/** A timeline element's summary length cap — a display constant, not a
 *  progress signal (SDR-003): long first lines are clipped so one element
 *  cannot crowd the row. */
export const TIMELINE_SUMMARY_MAX = 48;

/** The collapsed row's tail window (ADR-012 ①: fixed upper bound 5). */
const TIMELINE_WINDOW = 5;

export interface SwarmTimelineElement {
  readonly kind: 'tool' | 'text' | 'thinking';
  /** Render key — `step.id` / `segment.id`. */
  readonly id: string;
  /** Arrival order (ADR-011 ①'s shared counter): the merge/sort key. */
  readonly seq: number;
  /** Tool elements only — drives the row's status colouring. */
  readonly status?: 'ok' | 'running' | 'error';
  /** Tool elements only — the raw tool name, so the row renders its localized
   *  label through `toolLabel` at render time (ADR-012 ②: the pure layer
   *  carries no i18n — the same rule the thinking element follows by leaving
   *  its label to the component). Storing a resolved label here would also
   *  freeze a stale translation into the memo, which is keyed on the step array
   *  and never re-derives on a locale switch. Text / thinking elements carry
   *  none. */
  readonly name?: string;
  /** One-line digest: `toolSummary(name, arg)` for tools, the segment's first
   *  non-empty line for text, `''` for thinking (the component renders its own
   *  localized label off `kind` — the pure layer carries no i18n). */
  readonly summary: string;
  /** Text elements only: whether the element's first line was clipped by
   *  `TIMELINE_SUMMARY_MAX`. Always false for tool / thinking elements — their
   *  summaries are never length-clipped here (a tool digest is already clipped
   *  by `toolSummary`, and a thinking element carries no text at all). */
  readonly truncated: boolean;
}

export interface SwarmTimelineView {
  /** The tail window, arrival-ordered. Readonly: the view is shared through
   *  the memo, so a consumer that reorders or trims it in place would corrupt
   *  every later derivation. */
  readonly elements: ReadonlyArray<SwarmTimelineElement>;
  /** True when the window dropped earlier elements (the row renders a leading
   *  ellipsis). */
  readonly headTruncated: boolean;
}

function clipSummary(line: string): { summary: string; truncated: boolean } {
  if (line.length <= TIMELINE_SUMMARY_MAX) return { summary: line, truncated: false };
  return { summary: line.slice(0, TIMELINE_SUMMARY_MAX - 1) + '…', truncated: true };
}

/** Arrival order, with equal-seq ties broken by kind then id so the projection
 *  is deterministic (two entries never share a seq in practice — the projector
 *  stamps one monotonic counter — but a snapshot merge must not reorder the
 *  row). */
function compareElements(a: SwarmTimelineElement, b: SwarmTimelineElement): number {
  if (a.seq !== b.seq) return a.seq - b.seq;
  if (a.kind !== b.kind) return a.kind < b.kind ? -1 : 1;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

// Reference stability (ADR-012 ③ / ADR-011 ④): an unchanged member keeps the
// SAME view object across re-derivations, so the card does not re-render on
// every poll write-back. Keyed on the member's own array reference, holding the
// last derived window — and validated against a signature of that window,
// because the event projector mutates step arrays in place (push a new step,
// flip a step's status): a pure reference key would freeze the row on its
// first derivation.
//
// The key is `steps` when the member has any (its array survives both the
// projector's in-place appends and the 1Hz snapshot merge's
// `steps: live.steps`), otherwise `segments` — a member with no tool activity
// has no stable array, so it re-derives per streamed delta; the first-line
// element rule keeps the derived window identical, only the object is fresh.
interface TimelineMemoEntry {
  signature: string;
  view: SwarmTimelineView;
}

const TIMELINE_BY_STEPS = new WeakMap<readonly AgentStep[], TimelineMemoEntry>();
const TIMELINE_BY_SEGMENTS = new WeakMap<readonly NarrativeSegment[], TimelineMemoEntry>();

/** A member with neither source has an empty timeline — a shared, frozen
 *  constant (the type layer already forbids mutation; freezing pins the same
 *  invariant at runtime for the one view that every empty member shares). */
const EMPTY_TIMELINE_VIEW: SwarmTimelineView = Object.freeze({
  elements: Object.freeze<SwarmTimelineElement[]>([]),
  headTruncated: false,
});

/** Reuse the cached view when the freshly derived window is unchanged. */
function memoized<T extends object>(
  memo: WeakMap<T, TimelineMemoEntry>,
  key: T,
  elements: ReadonlyArray<SwarmTimelineElement>,
  headTruncated: boolean,
): SwarmTimelineView {
  const signature = timelineSignature(elements, headTruncated);
  const cached = memo.get(key);
  if (cached && cached.signature === signature) return cached.view;
  const view: SwarmTimelineView = { elements, headTruncated };
  memo.set(key, { signature, view });
  return view;
}

function timelineSignature(
  elements: ReadonlyArray<SwarmTimelineElement>,
  headTruncated: boolean,
): string {
  const parts = elements.map(
    (e) =>
      `${e.kind}\u0001${e.id}\u0001${e.seq}\u0001${e.status ?? ''}\u0001${e.name ?? ''}\u0001${e.truncated ? 1 : 0}\u0001${e.summary}`,
  );
  parts.push(headTruncated ? 'head:1' : 'head:0');
  return parts.join('\u0002');
}

/**
 * Derive a member's collapsed-row timeline (ADR-012 ①): `steps` and `segments`
 * merged into ONE arrival-ordered element list, then cut to the tail window.
 * Pure in the value sense — the same derived window comes back as the same
 * object reference (memoized per member).
 */
export function buildMemberTimeline(
  steps: AgentStep[] | undefined,
  segments: NarrativeSegment[] | undefined,
): SwarmTimelineView {
  if (!steps?.length && !segments?.length) return EMPTY_TIMELINE_VIEW;

  const merged: SwarmTimelineElement[] = [];
  for (const step of steps ?? []) {
    merged.push({
      kind: 'tool',
      id: step.id,
      seq: step.seq,
      status: step.status,
      name: step.name,
      // Single-line invariant (SDR-010 / ADR-012 ②): `toolSummary` only trims
      // and clips — a multi-line command would otherwise carry embedded
      // newlines into a one-line row, so collapse whitespace here in the data
      // layer instead of relying on the component's CSS.
      summary: toolSummary(step.name, step.arg).replaceAll(/\s+/g, ' '),
      truncated: false,
    });
  }
  for (const seg of segments ?? []) {
    if (seg.kind === 'thinking') {
      // Reasoning text never reaches the dialog area (SDR-002 / ADR-012 ②):
      // the element is a kind signal, the label is the component's i18n.
      merged.push({ kind: 'thinking', id: seg.id, seq: seg.seq, summary: '', truncated: false });
      continue;
    }
    const line = firstNonEmptyLine(seg.text);
    // A text segment that has opened but accumulated nothing yet has no line
    // to show — emit no element rather than a blank chip.
    if (line === '') continue;
    const clipped = clipSummary(line);
    merged.push({
      kind: 'text',
      id: seg.id,
      seq: seg.seq,
      summary: clipped.summary,
      truncated: clipped.truncated,
    });
  }
  merged.sort(compareElements);

  const headTruncated = merged.length > TIMELINE_WINDOW;
  const elements = headTruncated ? merged.slice(-TIMELINE_WINDOW) : merged;

  // Tool steps carry the stable array (the projector appends in place, the
  // snapshot merge passes `live.steps` through), so they make the better memo
  // key; a segment-only member falls back to its segment array.
  return steps?.length
    ? memoized(TIMELINE_BY_STEPS, steps, elements, headTruncated)
    : memoized(TIMELINE_BY_SEGMENTS, segments ?? [], elements, headTruncated);
}

export function swarmMemberActivity(member: SwarmMember, awaitingLabel = ''): string {
  // 事件驱动的 awaiting（ADR-008 ①，结构化 approval/question 挂起）：等待
  // 用户决策的活动行固定为等待文案；backoff（限流退避）不是等用户，活动行
  // 回落到原因/进度。
  if (member.phase === 'awaiting') return awaitingLabel;
  // member.text 是段派生尾部（ADR-011 ②）：尾部是推理段时为 undefined，
  // 活动行回落 outputLines / summary。
  return (
    suspendedReasonText(member.suspendedReason) ||
    lastNonEmptyLine(member.text) ||
    lastNonEmptyLine(member.outputLines?.join('\n')) ||
    (member.summary ?? '')
  );
}

// ---------------------------------------------------------------------------
// Collapsed-row timeline chips (ADR-012 ①, component-facing view model)
// ---------------------------------------------------------------------------
// The row renders one chip per timeline element. This projection is what lets
// each chip print without the component holding per-kind branching: a chip
// carries its display text and its tone (which colour class it takes). Pure in
// the value sense and locale-free — the thinking label arrives as an argument,
// exactly as `swarmMemberActivity` takes `awaitingLabel` (the pure layer never
// owns i18n).

/** Chip tone — maps 1:1 onto the row's colour rules (ADR-012 ①②). Tools
 *  colour by status (`run` accent / `err` danger / `done` muted); narrative
 *  elements colour by position (`active` = the window tail, the current
 *  activity, in the emphasis colour). */
export type TimelineChipTone = 'done' | 'active' | 'run' | 'err';

export interface TimelineChip {
  /** Render key — the element's own id (ADR-012 ③). */
  readonly id: string;
  readonly text: string;
  readonly tone: TimelineChipTone;
}

export interface TimelineChipsView {
  /** True when the window dropped earlier elements — the row prefixes the
   *  chip list with an ellipsis marker. */
  readonly headTruncated: boolean;
  readonly chips: ReadonlyArray<TimelineChip>;
}

/**
 * Project a member's timeline into the row's printable chips (ADR-012 ①②).
 *
 * - tool → `toolLabel(name)` + the argument digest ("Read src/a.ts"), or the
 *   bare label when the digest is empty (`toolSummary` returns '' for an
 *   arg-less call).
 * - text → the segment's first-line summary, as derived.
 * - thinking → `thinkingLabel`; the segment's text never leaves L1.
 *
 * Colour, and the one place the two colour rules in ADR-012 ①② collide:
 * "the current element takes the emphasis colour" and "tools colour by status"
 * both want the tail element whenever the member's newest activity IS a tool —
 * and that is the usual case. Status wins for tools (running → accent, error →
 * danger, settled → the muted default), because accent in the dark theme is a
 * far stronger "this is happening NOW" signal than the plain text colour, and
 * a settled tool must read as settled even at the tail (ADR-012 ①: 已完成的
 * 弱化). Tail emphasis therefore applies to the narrative elements, which
 * carry no status of their own: the newest text / thinking chip is the current
 * activity, every other narrative chip recedes.
 */
export function buildTimelineChips(
  view: SwarmTimelineView | undefined,
  thinkingLabel: string,
): TimelineChipsView {
  const elements = view?.elements ?? [];
  const tailId = elements.at(-1)?.id;
  const chips = elements.map((el): TimelineChip => {
    const text =
      el.kind === 'thinking'
        ? thinkingLabel
        : el.kind === 'text'
          ? el.summary
          : [toolLabel(el.name ?? ''), el.summary].filter(Boolean).join(' ');
    const tone: TimelineChipTone =
      el.kind === 'tool'
        ? el.status === 'running'
          ? 'run'
          : el.status === 'error'
            ? 'err'
            : 'done'
        : el.id === tailId
          ? 'active'
          : 'done';
    return { id: el.id, text, tone };
  });
  return { headTruncated: view?.headTruncated ?? false, chips };
}

function swarmMemberBody(member: SwarmMember): string {
  // 结构化 reason 是标记不是散文（suspendedReasonText 返回 ''），body 回落到
  // 段派生尾部——绝不把对象插值为 "[object Object]"。
  const reason = suspendedReasonText(member.suspendedReason);
  if (reason) return reason;
  if (member.text) return member.text;
  if (member.outputLines && member.outputLines.length > 0) return member.outputLines.join('\n');
  return member.summary ?? '';
}

function outcomeToPhase(outcome: string): AppSubagentPhase {
  if (outcome === 'completed') return 'completed';
  if (outcome === 'failed') return 'failed';
  // 用户取消分档（ADR-008 ③）：aborted 不再混入 failed。
  if (outcome === 'aborted') return 'aborted';
  return 'working';
}

function resultRow(sub: SwarmResultSubagent, index: number): SwarmCardRow {
  return {
    id: sub.agentId ?? sub.item ?? `result-${index}`,
    name: sub.item ?? `subagent ${index + 1}`,
    activity: sub.body.split('\n')[0] ?? '',
    phase: outcomeToPhase(sub.outcome),
    body: sub.body,
  };
}

/**
 * Whether a live member already accounts for a result subagent. Members may
 * come from the projector (task id / description) while the result references
 * agent_id / item; the two ids don't always match, so also treat item ⊆
 * description as a match.
 */
function memberCoversResult(member: SwarmMember, sub: SwarmResultSubagent): boolean {
  if (sub.agentId && member.id === sub.agentId) return true;
  if (sub.item && member.name.includes(sub.item)) return true;
  return false;
}

/**
 * Merge the live members with the agent_swarm_result payload into one row list.
 *
 * - Members are authoritative while present (real-time phase + streamed text).
 * - When a parsed result is also present, append result rows that no member
 *   covers — e.g. interrupted swarms emit `state="not_started"` /
 *   `outcome="aborted"` entries for items that never spawned a task, which
 *   would otherwise be invisible until a refresh dropped the live tasks.
 * - When no members are present (post-refresh), fall back to result-only rows.
 */
export function buildSwarmCardRows(
  members: SwarmMember[],
  result: SwarmResult | null,
  // 等待用户（approval/question，事件驱动）成员的活动行文案——组件以 i18n
  // 标签传入；纯 TS 层不做本地化。
  awaitingLabel = '',
): SwarmCardRow[] {
  const memberRows = members.map((m) => ({
    id: m.id,
    name: m.name,
    activity: swarmMemberActivity(m, awaitingLabel),
    phase: m.phase,
    body: swarmMemberBody(m),
    model: m.model,
    timeline: m.timeline,
  }));
  if (!result) return memberRows;

  const resultOnly = result.subagents
    .filter(
      (sub) =>
        (sub.outcome === 'aborted' || sub.state === 'not_started') &&
        !members.some((m) => memberCoversResult(m, sub)),
    )
    .map((sub, i) => resultRow(sub, i));

  return memberRows.length > 0 ? [...memberRows, ...resultOnly] : result.subagents.map((s, i) => resultRow(s, i));
}
