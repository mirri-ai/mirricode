import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import type { AgentStep, AppSubagentPhase, AppTask, NarrativeSegment } from '../src/api/types';
import { i18n } from '../src/i18n';
import type { SwarmMember } from '../src/composables/swarmGroups';
import { swarmMembersByToolCall } from '../src/composables/swarmGroups';
import type { SwarmResult } from '../src/lib/parseSwarmResult';
import type { SwarmTimelineView } from '../src/lib/swarmCardRows';
import {
  TIMELINE_SUMMARY_MAX,
  buildMemberTimeline,
  buildSwarmCardRows,
  buildTimelineChips,
  swarmMemberActivity,
} from '../src/lib/swarmCardRows';
import { toolSummary } from '../src/lib/toolMeta';

function member(
  id: string,
  name: string,
  opts: {
    phase?: AppSubagentPhase;
    /** The segment-derived text tail (ADR-011 ②) — what swarmGroups fills
     *  from liveTextTail(task.segments). */
    text?: string;
    outputLines?: string[];
    summary?: string;
    suspendedReason?: string;
    model?: string;
    /** The collapsed-row timeline (ADR-012 ①) — what swarmGroups derives with
     *  buildMemberTimeline. Omitted where the member has no source. */
    timeline?: SwarmTimelineView;
  } = {},
): SwarmMember {
  return {
    id,
    name,
    phase: opts.phase ?? 'working',
    text: opts.text,
    outputLines: opts.outputLines,
    summary: opts.summary,
    suspendedReason: opts.suspendedReason,
    model: opts.model,
    timeline: opts.timeline,
    swarmIndex: 0,
  };
}

function result(subagents: SwarmResult['subagents']): SwarmResult {
  return {
    summary: `${subagents.length}`,
    completed: subagents.filter((s) => s.outcome === 'completed').length,
    failed: subagents.filter((s) => s.outcome === 'failed').length,
    aborted: subagents.filter((s) => s.outcome === 'aborted').length,
    total: subagents.length,
    subagents,
  };
}

describe('swarmMemberActivity', () => {
  it('prefers the segment-derived text tail over outputLines and summary', () => {
    const m = member('a', '子任务', {
      text: 'line 1\nline 2',
      outputLines: ['tool call output'],
      summary: 'final summary',
    });
    expect(swarmMemberActivity(m)).toBe('line 2');
  });

  it('falls back to the last outputLines entry when no text is streaming', () => {
    const m = member('a', '子任务', { outputLines: ['one', 'two'], summary: 'summary' });
    expect(swarmMemberActivity(m)).toBe('two');
  });

  it('falls back to summary', () => {
    expect(swarmMemberActivity(member('a', '子任务', { summary: 'sum' }))).toBe('sum');
  });

  // 事件驱动的 awaiting（ADR-008 ①）：等待用户决策的成员活动行固定为「等待
  // 审批中」文案，不再以其最后一行活动为主；P1 推导已删（ADR-008 ④）。
  it('shows the awaiting label as activity when a member awaits via the event phase', () => {
    const m = member('a', '子任务', { phase: 'awaiting', text: 'Calling Edit: x.ts' });
    expect(swarmMemberActivity(m, '等待审批中')).toBe('等待审批中');
  });
});

describe('buildSwarmCardRows', () => {
  it('builds rows from live members when no parsed result exists', () => {
    const rows = buildSwarmCardRows(
      [member('a', '子任务 A', { text: 'streaming' })],
      null,
    );
    expect(rows).toEqual([{ id: 'a', name: '子任务 A', activity: 'streaming', phase: 'working', body: 'streaming' }]);
  });

  // 事件驱动的 awaiting（ADR-008 ①/④）：成员行以等待文案为活动行，且不再
  // 携带任何推导标记。
  it('carries the awaiting activity label onto member rows with no derived marker', () => {
    const rows = buildSwarmCardRows(
      [member('a1', '子任务 A', { phase: 'awaiting', text: 'streaming' })],
      null,
      '等待审批中',
    );
    expect(rows[0]).toMatchObject({
      phase: 'awaiting',
      activity: '等待审批中',
    });
    expect(rows[0]).not.toHaveProperty('awaitingApproval');
  });

  // ADR-012 ③：等待不是时间线的断点——活动行让位给等待文案，但成员已发生的
  // 活动元素仍在行上（组件渲染时整体弱化，视觉冻结不改变语义）。
  it('should keep the timeline elements on an awaiting row whose activity is the awaiting label', () => {
    // Given a member that suspended on an approval after two tool calls
    const timeline = buildMemberTimeline(
      [
        { id: 'tc-1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: [], seq: 0 },
        { id: 'tc-2', name: 'Edit', arg: 'a.ts', status: 'running', outputLines: [], seq: 1 },
      ],
      undefined,
    );
    const rows = buildSwarmCardRows(
      [member('a1', '子任务 A', { phase: 'awaiting', timeline })],
      null,
      '等待审批中',
    );

    // Then the activity line is the awaiting label and the timeline survives
    expect(rows[0]?.activity).toBe('等待审批中');
    expect(rows[0]?.timeline?.elements.map((e) => e.id)).toEqual(['tc-1', 'tc-2']);
    expect(rows[0]?.timeline?.elements.at(-1)?.status).toBe('running');
  });

  it('builds rows from result subagents when no members are present', () => {
    const rows = buildSwarmCardRows(
      [],
      result([
        { outcome: 'completed', item: 'A', body: 'A body' },
        { outcome: 'failed', item: 'B', body: 'B body' },
      ]),
    );
    expect(rows.map((r) => r.name)).toEqual(['A', 'B']);
    expect(rows.map((r) => r.phase)).toEqual(['completed', 'failed']);
  });

  it('appends result-only aborted not_started rows on top of live members', () => {
    const rows = buildSwarmCardRows(
      [
        member('a1', '子任务 A', { phase: 'completed' }),
        member('a2', '子任务 B', { phase: 'working' }),
      ],
      result([
        { outcome: 'completed', item: 'A', agentId: 'a1', body: 'A body' },
        { outcome: 'completed', item: 'B', agentId: 'a2', body: 'B body' },
        { outcome: 'aborted', item: 'C', state: 'not_started', body: 'C never started' },
      ]),
    );
    expect(rows.map((r) => r.id)).toEqual(['a1', 'a2', 'C']);
    // 用户取消分档（ADR-008 ③）：aborted 呈现为「已中止」，不再混入 failed。
    expect(rows[2]?.phase).toBe('aborted');
    expect(rows[2]?.body).toBe('C never started');
  });

  it('does not duplicate a result row that a live member already covers', () => {
    const rows = buildSwarmCardRows(
      [member('a1', '子任务 A', { phase: 'failed' })],
      result([{ outcome: 'aborted', item: 'A', agentId: 'a1', body: 'A body' }]),
    );
    expect(rows.map((r) => r.id)).toEqual(['a1']);
    expect(rows[0]?.phase).toBe('failed');
  });

  it('matches by item substring when agent ids disagree', () => {
    const rows = buildSwarmCardRows(
      [member('a1', 'find unused exports in src', { phase: 'completed' })],
      result([{ outcome: 'aborted', item: 'unused exports', state: 'not_started', body: 'x' }]),
    );
    expect(rows.map((r) => r.id)).toEqual(['a1']);
  });

  it('carries the resolved model onto progress rows', () => {
    const rows = buildSwarmCardRows(
      [member('a1', '子任务 A', { phase: 'working', model: 'claude-sonnet' })],
      undefined,
    );
    expect(rows[0]).toMatchObject({ id: 'a1', model: 'claude-sonnet' });
  });

  it('leaves model undefined on rows without a resolved model', () => {
    const rows = buildSwarmCardRows([member('a1', '子任务 A', { phase: 'working' })], undefined);
    expect(rows[0]?.model).toBeUndefined();
  });

  // ADR-012 ①/③：成员行的折叠态时间线由成员搬运，不做二次派生——行本身
  // 不持有任何 steps/segments，透传保证同一视图对象（引用稳定）。
  it('should pass the member timeline through onto its row when the member has one', () => {
    // Given a live member carrying a derived timeline
    const timeline = buildMemberTimeline(
      [{ id: 'tc-1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: [], seq: 0 }],
      [{ id: 'seg-0', kind: 'text', text: 'Reading a.ts', seq: 1 }],
    );
    const rows = buildSwarmCardRows([member('a1', '子任务 A', { timeline })], null);

    // Then the row carries that exact view object (no copy, no re-derivation)
    expect(rows[0]?.timeline).toBe(timeline);
  });

  it('should leave the timeline undefined on result-only rows', () => {
    // Given only a parsed result (post-refresh) — the payload carries no steps
    // or segments, so there is nothing to derive a timeline from
    const rows = buildSwarmCardRows(
      [],
      result([{ outcome: 'completed', item: 'A', body: 'A body' }]),
    );

    // Then the row has no timeline and the whole-row shape stays minimal
    expect(rows[0]).toEqual({
      id: 'A',
      name: 'A',
      activity: 'A body',
      phase: 'completed',
      body: 'A body',
    });
    expect(rows[0]).not.toHaveProperty('timeline');
  });
});

describe('activity line derived from narrative segments (ADR-011 ②)', () => {
  function subagentTask(segments?: AppTask['segments'], outputLines?: string[]): AppTask {
    return {
      id: 'a1',
      sessionId: 's1',
      kind: 'subagent',
      description: '子任务 A',
      status: 'running',
      createdAt: '2026-01-01T00:00:00.000Z',
      parentToolCallId: 'tc-swarm',
      segments,
      outputLines,
    };
  }

  it('should show the last line of the last text segment as the row activity when segments exist', () => {
    // Given a live member whose newest segment is text
    const members = swarmMembersByToolCall([
      subagentTask([
        { id: 'a1-seg-0', kind: 'thinking', text: 'planning', seq: 0 },
        { id: 'a1-seg-1', kind: 'text', text: 'Reading src/a.ts\nCalling Grep: liveTextTail', seq: 1 },
      ]),
    ]).get('tc-swarm');

    // Then the activity row shows the derived tail's last line
    const rows = buildSwarmCardRows(members ?? [], null);
    expect(rows[0]?.activity).toBe('Calling Grep: liveTextTail');
  });

  it('should show no derived activity when the member is only reasoning at the tail', () => {
    // Given a member whose latest arrival is a thinking segment after some text
    const members = swarmMembersByToolCall([
      subagentTask([
        { id: 'a1-seg-0', kind: 'text', text: 'Earliest output', seq: 0 },
        { id: 'a1-seg-1', kind: 'thinking', text: 'Reconsidering the approach', seq: 1 },
      ]),
    ]).get('tc-swarm');

    // Then the earlier text segment is not passed off as the current activity —
    // the row falls back through outputLines / summary instead
    const rows = buildSwarmCardRows(members ?? [], null);
    expect(rows[0]?.activity).toBe('');
  });

  it('should fall back through outputLines and summary when the tail segment carries no text', () => {
    // Given a member mid-run whose tail is reasoning and whose legacy flat
    // fields are gone (ADR-011 ⑥ retirement) — the derived text tail is
    // undefined, so the downstream chain must fall back honestly
    const members = swarmMembersByToolCall([
      subagentTask(
        [{ id: 'a1-seg-0', kind: 'thinking', text: 'Reconsidering', seq: 0 }],
        ['progress line one', 'progress line two'],
      ),
    ]).get('tc-swarm');

    // Then the activity row falls back to the last output line (the summary
    // would follow if outputLines were empty too)
    const rows = buildSwarmCardRows(members ?? [], null);
    expect(rows[0]?.activity).toBe('progress line two');
    expect(rows[0]?.body).toBe('progress line one\nprogress line two');
  });

  it('should fall back to the summary when the tail segment carries no text and no output lines exist', () => {
    // Given a member whose only narrative is a reasoning segment — no text
    // tail, no progress lines, only the settled summary
    const members = swarmMembersByToolCall([
      subagentTask([{ id: 'a1-seg-0', kind: 'thinking', text: 'Reconsidering', seq: 0 }]),
    ]).get('tc-swarm');

    // Then the summary carries the activity row (the last honest fallback)
    const withSummary = members?.map((m) => ({ ...m, summary: 'final summary' })) ?? [];
    const rows = buildSwarmCardRows(withSummary, null);
    expect(rows[0]?.activity).toBe('final summary');
  });
});

describe('buildMemberTimeline — collapsed-row lightweight timeline (ADR-012 ①)', () => {
  it('should collapse a multi-line command digest to a single line (SDR-010 red line)', () => {
    // Given a Bash step whose argument JSON embeds a multi-line command
    const s = step({ id: 'call_Bash', name: 'Bash', arg: JSON.stringify({ command: 'cd /tmp\necho SECRET_TOKEN=example\nexit 1' }) });
    // When the timeline derives its tool element summary
    const view = buildMemberTimeline([s], undefined);
    const summary = view.elements[0]?.summary ?? '';
    // Then it stays a single line — the data layer guarantees the
    // one-line invariant, not the component's CSS. The leading `cd` now
    // folds into a directory prefix (newline is a command separator), so
    // the digest leads with the compressed directory + the real command.
    expect(summary).not.toContain('\n');
    expect(summary).toContain('…/tmp/ && echo SECRET_TOKEN=example exit 1');
  });

  // toolSummary resolves a couple of labels through the global i18n instance,
  // so pin the locale (the same rule as test/notification-logic.test.ts).
  i18n.global.locale.value = 'en';

  function step(overrides: Partial<AgentStep> & Pick<AgentStep, 'id' | 'seq'>): AgentStep {
    return { name: 'Read', arg: 'a.ts', status: 'ok', outputLines: [], ...overrides };
  }

  function seg(
    overrides: Partial<NarrativeSegment> & Pick<NarrativeSegment, 'id' | 'kind' | 'seq'>,
  ): NarrativeSegment {
    return { text: '', ...overrides };
  }

  it('should interleave text, tool and thinking elements in arrival order when all three arrive out of order', () => {
    // Given a member whose narrative and tools arrive as seq 0..3 with the
    // arrays unsorted (the projector appends per kind, not globally)
    const steps = [step({ id: 'tc-b', seq: 1, name: 'Grep', arg: 'pattern' })];
    const segments = [
      seg({ id: 'seg-text', kind: 'text', text: 'Reading the fixture', seq: 2 }),
      seg({ id: 'seg-think', kind: 'thinking', text: 'internal plan', seq: 0 }),
    ];

    // Then the timeline is one arrival-ordered sequence across both sources
    const view = buildMemberTimeline(steps, segments);
    expect(view.elements.map((e) => [e.kind, e.id])).toEqual([
      ['thinking', 'seg-think'],
      ['tool', 'tc-b'],
      ['text', 'seg-text'],
    ]);
    expect(view.headTruncated).toBe(false);
  });

  it('should break seq ties by kind then id when a snapshot merge hands over equal seqs', () => {
    // Given a step and a segment stamped with the same seq (defensive: the
    // projector's counter never does this, a merged snapshot might)
    const steps = [step({ id: 'b-step', seq: 7 }), step({ id: 'a-step', seq: 7 })];
    const segments = [seg({ id: 'seg', kind: 'thinking', seq: 7 })];

    // Then the order is deterministic: kind first ('thinking' < 'tool'), then id
    const view = buildMemberTimeline(steps, segments);
    expect(view.elements.map((e) => e.id)).toEqual(['seg', 'a-step', 'b-step']);
  });

  it('should keep the tail five elements and flag the discarded head when six arrive', () => {
    // Given six activity elements
    const segments = Array.from({ length: 6 }, (_, i) =>
      seg({ id: `seg-${i}`, kind: 'text', text: `line ${i}`, seq: i }),
    );

    // Then only the newest five survive and the head truncation is flagged
    const view = buildMemberTimeline(undefined, segments);
    expect(view.elements.map((e) => e.id)).toEqual(['seg-1', 'seg-2', 'seg-3', 'seg-4', 'seg-5']);
    expect(view.headTruncated).toBe(true);
  });

  it('should not flag the head when exactly five elements arrive', () => {
    // Given exactly the window's worth of activity
    const segments = Array.from({ length: 5 }, (_, i) =>
      seg({ id: `seg-${i}`, kind: 'text', text: `line ${i}`, seq: i }),
    );

    // Then nothing was discarded
    const view = buildMemberTimeline(undefined, segments);
    expect(view.elements).toHaveLength(5);
    expect(view.headTruncated).toBe(false);
  });

  it('should return an empty view when the member has neither steps nor segments', () => {
    // Given a member with no live timeline data (result-only row, not started)
    // Then the timeline is empty and nothing is flagged
    expect(buildMemberTimeline(undefined, undefined)).toEqual({ elements: [], headTruncated: false });
    expect(buildMemberTimeline([], [])).toEqual({ elements: [], headTruncated: false });
    // The shared empty view is frozen (ADR-012 ③): one constant backs every
    // empty member, so an in-place consumer must not be able to corrupt it.
    const empty = buildMemberTimeline(undefined, undefined);
    expect(Object.isFrozen(empty)).toBe(true);
    expect(Object.isFrozen(empty.elements)).toBe(true);
  });

  it('should clip an over-long first line and flag the element as truncated', () => {
    // Given a text segment whose first line exceeds the display cap
    const long = 'x'.repeat(TIMELINE_SUMMARY_MAX + 20);
    const view = buildMemberTimeline(
      undefined,
      [seg({ id: 'seg-0', kind: 'text', text: `${long}\nsecond line`, seq: 0 })],
    );

    // Then the summary is capped with an ellipsis and marked truncated
    const el = view.elements[0];
    expect(el?.truncated).toBe(true);
    expect(el?.summary).toHaveLength(TIMELINE_SUMMARY_MAX);
    expect(el?.summary.endsWith('…')).toBe(true);
  });

  it('should carry no reasoning text when the element stands for a thinking segment', () => {
    // Given a thinking segment carrying real reasoning text
    const view = buildMemberTimeline(
      undefined,
      [seg({ id: 'seg-0', kind: 'thinking', text: 'secret chain of thought', seq: 0 })],
    );

    // Then the element is a kind signal only — the component renders the label
    expect(view.elements[0]).toEqual({
      kind: 'thinking',
      id: 'seg-0',
      seq: 0,
      summary: '',
      truncated: false,
    });
  });

  it('should keep a text element summary identical when the same segment keeps streaming', () => {
    // Given a text segment that later accumulates more text (a new array, the
    // way the projector re-mints it per delta)
    const first = buildMemberTimeline(
      undefined,
      [seg({ id: 'seg-0', kind: 'text', text: 'Opening line\nmore', seq: 0 })],
    );
    const grown = buildMemberTimeline(
      undefined,
      [seg({ id: 'seg-0', kind: 'text', text: 'Opening line\nmore\nand more', seq: 0 })],
    );

    // Then the element's summary is the segment's identity (its first line) and
    // never chases the streaming tail
    expect(grown.elements[0]?.summary).toBe(first.elements[0]?.summary);
    expect(grown.elements[0]?.summary).toBe('Opening line');
  });

  it('should return the same view object when called twice with the same inputs', () => {
    // Given a member's stable steps/segments array pair (ADR-011 ④
    // reference-stability guarantee)
    const steps = [step({ id: 'tc-1', seq: 0 })];
    const segments = [seg({ id: 'seg-0', kind: 'text', text: 'line', seq: 1 })];

    // Then re-derivation reuses the cached view, so the card sees no change
    expect(buildMemberTimeline(steps, segments)).toBe(buildMemberTimeline(steps, segments));
  });

  it('should re-derive when the step array is mutated in place at the same reference', () => {
    // Given the projector's in-place mutation path: a step array whose
    // reference survives while an element is appended
    const steps = [step({ id: 'tc-1', seq: 0 })];
    const segments: NarrativeSegment[] = [];
    const before = buildMemberTimeline(steps, segments);

    steps.push(step({ id: 'tc-2', seq: 1, name: 'Bash', arg: 'ls' }));
    const after = buildMemberTimeline(steps, segments);

    // Then the appended step is visible — the memo is keyed on references but
    // validated against the derived window, so a stale view never sticks
    expect(before.elements.map((e) => e.id)).toEqual(['tc-1']);
    expect(after.elements.map((e) => e.id)).toEqual(['tc-1', 'tc-2']);
  });

  // 红线回归（SDR-010 / ADR-012 ②）：工具输出与完整参数禁入折叠态。摘要必须
  // 与卡头 ToolRow 的 arg 摘要同型——单行、非完整 arg、绝不含 output 行。
  it('should never leak tool output or the full argument into a tool element summary', () => {
    // Given a Bash step whose JSON arg is far longer than the digest cap and
    // whose output lines carry sensitive content (the two things the dialog
    // area must never show)
    const longCommand = `cat /etc/secret-token-${'x'.repeat(200)}.conf`;
    const sensitiveOutput = 'SECRET_TOKEN=abc123';
    const steps = [
      step({
        id: 'tc-bash',
        seq: 0,
        name: 'Bash',
        arg: JSON.stringify({ command: longCommand, description: 'read the token' }),
        outputLines: [sensitiveOutput, 'more secret output'],
      }),
    ];

    // Then the summary is exactly the header digest: one line, clipped short of
    // the full arg, and free of every output line
    const el = buildMemberTimeline(steps, undefined).elements[0];
    expect(el?.summary).toBe(toolSummary('Bash', steps[0]!.arg));
    expect(el?.summary).not.toContain('\n');
    expect(el?.summary).not.toContain(sensitiveOutput);
    expect(el?.summary).not.toContain('more secret output');
    expect(el?.summary).not.toBe(longCommand);
    expect(el?.summary).not.toContain('read the token');
    expect(el?.summary.length).toBeLessThan(longCommand.length);
    // Status passes through so the row can colour the element
    expect(el?.status).toBe('ok');
  });
});

describe('buildTimelineChips — collapsed-row chip projection (ADR-012 ①②)', () => {
  // toolLabel resolves through the global i18n instance (same rule as above).
  i18n.global.locale.value = 'en';

  const view = (elements: SwarmTimelineView['elements'], headTruncated = false): SwarmTimelineView => ({
    elements,
    headTruncated,
  });

  it('should print the localized tool label followed by the argument digest when a tool element is projected', () => {
    // Given a member whose timeline holds one settled Read call
    const steps = [{ id: 'tc-1', name: 'Bash', arg: JSON.stringify({ command: 'ls -la' }), status: 'ok' as const, outputLines: [], seq: 0 }];

    // Then the chip reads like the header tool row: label + one-line digest
    const chips = buildTimelineChips(buildMemberTimeline(steps, undefined), 'Thinking');
    expect(chips.chips).toHaveLength(1);
    expect(chips.chips[0]?.text).toBe('Run ls -la');
  });

  it('should show the bare tool label when the call carries no argument worth digesting', () => {
    // Given a tool call whose arg is an empty object (toolSummary yields '')
    const steps = [{ id: 'tc-1', name: 'Read', arg: '{}', status: 'ok' as const, outputLines: [], seq: 0 }];

    // Then the chip is just the label — no dangling separator
    const chips = buildTimelineChips(buildMemberTimeline(steps, undefined), 'Thinking');
    expect(chips.chips[0]?.text).toBe('Read');
  });

  it('should render the localized thinking label when an element stands for a reasoning segment', () => {
    // Given a thinking segment (its text never leaves the detail panel)
    const segments = [{ id: 'seg-0', kind: 'thinking' as const, text: 'secret plan', seq: 0 }];

    // Then the chip is the injected label, not the segment text
    const chips = buildTimelineChips(buildMemberTimeline(undefined, segments), '思考中');
    expect(chips.chips[0]?.text).toBe('思考中');
    expect(chips.chips[0]?.text).not.toContain('secret plan');
  });

  it('should print the segment first line when a text element is projected', () => {
    // Given a text segment whose body has several lines
    const segments = [{ id: 'seg-0', kind: 'text' as const, text: 'Opening line\nstreaming tail', seq: 0 }];

    // Then the chip carries the stable first line, never the moving tail
    const chips = buildTimelineChips(buildMemberTimeline(undefined, segments), 'Thinking');
    expect(chips.chips[0]?.text).toBe('Opening line');
  });

  it('should colour tools by status and only the tail narrative element as the current activity', () => {
    // Given a settled tool, a failed tool, a running tool, then a live text tail
    const steps = [
      { id: 'tc-ok', name: 'Read', arg: 'a.ts', status: 'ok' as const, outputLines: [], seq: 0 },
      { id: 'tc-err', name: 'Bash', arg: 'boom', status: 'error' as const, outputLines: [], seq: 1 },
      { id: 'tc-run', name: 'Bash', arg: 'build', status: 'running' as const, outputLines: [], seq: 2 },
    ];
    const segments = [{ id: 'seg-0', kind: 'text' as const, text: 'Now writing', seq: 3 }];

    // Then each tool keeps its own status colour — a running tool reads as live
    // work (accent), a failed one as the anomaly, a settled one recedes even
    // though it is not the tail — and the newest narrative element is the
    // current activity (emphasis colour)
    const chips = buildTimelineChips(buildMemberTimeline(steps, segments), 'Thinking');
    expect(chips.chips.map((c) => [c.id, c.tone])).toEqual([
      ['tc-ok', 'done'],
      ['tc-err', 'err'],
      ['tc-run', 'run'],
      ['seg-0', 'active'],
    ]);
  });

  it('should leave a settled tail tool muted instead of highlighting it as the current activity', () => {
    // Given a member whose newest activity is an already-finished tool (the
    // common between-tool state)
    const steps = [{ id: 'tc-ok', name: 'Read', arg: 'a.ts', status: 'ok' as const, outputLines: [], seq: 0 }];

    // Then the settled tool recedes (ADR-012 ①: 已完成的弱化) — status colouring
    // outranks tail emphasis, so nothing pretends to be live
    const chips = buildTimelineChips(buildMemberTimeline(steps, undefined), 'Thinking');
    expect(chips.chips[0]?.tone).toBe('done');
  });

  it('should emphasise only the newest narrative element when a member has several text segments', () => {
    // Given two text segments with no tool activity between them
    const segments = [
      { id: 'seg-0', kind: 'text' as const, text: 'first thought', seq: 0 },
      { id: 'seg-1', kind: 'text' as const, text: 'second thought', seq: 1 },
    ];

    // Then the tail carries the emphasis and the earlier one recedes
    const chips = buildTimelineChips(buildMemberTimeline(undefined, segments), 'Thinking');
    expect(chips.chips.map((c) => c.tone)).toEqual(['done', 'active']);
  });

  it('should carry the head-truncation flag through when the window dropped earlier elements', () => {
    // Given six activities (the window keeps the tail five and flags the head)
    const segments = Array.from({ length: 6 }, (_, i) => ({
      id: `seg-${i}`,
      kind: 'text' as const,
      text: `line ${i}`,
      seq: i,
    }));

    // Then the row knows to render its leading ellipsis marker
    const chips = buildTimelineChips(buildMemberTimeline(undefined, segments), 'Thinking');
    expect(chips.headTruncated).toBe(true);
    expect(chips.chips).toHaveLength(5);
  });

  it('should produce no chips and no marker when the row has no timeline at all', () => {
    // Given a result-only row (no steps/segments to derive from)
    // Then the row keeps its single activity line and the projection stays empty
    expect(buildTimelineChips(undefined, 'Thinking')).toEqual({ headTruncated: false, chips: [] });
    expect(buildTimelineChips(view([]), 'Thinking')).toEqual({ headTruncated: false, chips: [] });
  });
});

// ---------------------------------------------------------------------------
// DelegationCard member-row rendering contract (ADR-012 ①②)
// ---------------------------------------------------------------------------
// The repo has no @vue/test-utils, so the row's rendering rules are pinned
// against the SFC source (the recovery-notice-card.test.ts precedent). These
// assertions guard the three branches a refactor could silently collapse:
// awaiting/suspended rows keep BOTH the waiting copy and a dimmed timeline,
// timeline rows replace the single activity line, and timeline-less rows keep
// it. The visual result itself is verified in the browser (Task 4).
describe('DelegationCard member-row timeline template', () => {
  const source = readFileSync(
    join(import.meta.dirname, '../src/components/chat/tool-calls/DelegationCard.vue'),
    'utf8',
  );

  it('should keep the waiting copy AND the dimmed timeline when a row is awaiting or suspended', () => {
    // Given the frozen branch (ADR-008 ② visibility: the waiting copy is the
    // one thing the user must act on, so it can never be replaced)
    expect(source).toContain("const frozen = row.phase === 'awaiting' || row.phase === 'suspended';");
    expect(source).toMatch(
      /<template v-if="frozen">[\s\S]*?<span class="dc-row-act">\{\{ row\.activity \}\}<\/span>[\s\S]*?<span v-if="chips" class="dc-tl"[\s\S]*?<\/template>/,
    );
    // And the parked timeline reads dimmer than the live one
    expect(source).toMatch(/\.dc-row\.frozen \.dc-tl \{\s*opacity: 0\.55;/);
  });

  it('should replace the single activity line with the timeline when the row has timeline chips', () => {
    // Given a row with derived chips — the timeline takes the activity slot
    expect(source).toMatch(/<span v-else-if="chips" class="dc-tl">/);
    // And the single-line summary remains the fallback for timeline-less rows
    // (result-only after a refresh, a member that never started)
    expect(source).toMatch(/<span v-else class="dc-row-act">\{\{ row\.activity \}\}<\/span>/);
  });

  it('should render the head ellipsis and per-element tone classes when the window was truncated', () => {
    // Given a truncated window: a leading ellipsis marker plus tone-classed chips
    expect(source).toContain('<span v-if="chips.headTruncated" class="dc-tl-more">…</span>');
    expect(source).toContain(':class="`tl-${c.tone}`"');
    // Every tone the projection can emit has a rule (no invisible chip)
    for (const tone of ['active', 'run', 'err', 'done']) {
      expect(source, `missing colour rule for the "${tone}" tone`).toContain(`.dc-tl-el.tl-${tone} {`);
    }
  });

  it('should keep the timeline on one clipped, non-scrolling line when the row is narrow', () => {
    // Given the row's height must never grow: no wrap, no scrollbar
    const rule = source.match(/\.dc-tl \{[\s\S]*?\}/)?.[0] ?? '';
    expect(rule).toContain('overflow: hidden;');
    expect(rule).toContain('white-space: nowrap;');
    expect(rule).not.toContain('overflow-x: auto');
    expect(rule, 'the timeline must not wrap onto a second line').not.toContain('flex-wrap');
  });
});
