// apps/mirri-web/src/lib/memberBlocks.test.ts
import { describe, expect, it } from 'vitest';
import {
  liveOpenSegmentId,
  liveStreamingView,
  liveTextTail,
  memberBlocksFromLive,
  memberBlocksFromMessages,
  settledNarrativeBlocks,
} from './memberBlocks';
import { renderBlocksFromBlocks } from '../components/chatTurnRendering';
import type { AgentStep, AppMessage, NarrativeSegment } from '../api/types';
import type { TurnBlock } from '../types';

function seg(overrides: Partial<NarrativeSegment> & Pick<NarrativeSegment, 'id' | 'kind' | 'text' | 'seq'>): NarrativeSegment {
  return { tailGuard: undefined, ...overrides };
}

function step(overrides: Partial<AgentStep> & Pick<AgentStep, 'id' | 'seq'>): AgentStep {
  return { name: 'Read', arg: 'a.ts', status: 'ok', outputLines: [], ...overrides };
}

describe('memberBlocksFromLive — interleaved live narrative (ADR-011 ①)', () => {
  it('should interleave segments and tool steps in arrival order when both are present', () => {
    // Given a live member whose narrative arrived thinking → tool → tool → text
    const blocks = memberBlocksFromLive(
      [
        seg({ id: 's0', kind: 'thinking', text: 'A', seq: 0 }),
        seg({ id: 's3', kind: 'text', text: 'D', seq: 3 }),
      ],
      [step({ id: 'tc-1', seq: 1 }), step({ id: 'tc-2', name: 'Bash', arg: 'ls', seq: 2 })],
    );

    // Then the block stream keeps true happening order
    expect(blocks.map((b) => b.kind)).toEqual(['thinking', 'tool', 'tool', 'text']);
    if (blocks[0]?.kind === 'thinking') expect(blocks[0].thinking).toBe('A');
    if (blocks[3]?.kind === 'text') expect(blocks[3].text).toBe('D');
  });

  it('should merge adjacent same-kind segments into one block when no tool separates them', () => {
    // Given two consecutive text segments with no tool between
    const blocks = memberBlocksFromLive(
      [
        seg({ id: 's0', kind: 'text', text: 'A', seq: 0 }),
        seg({ id: 's1', kind: 'text', text: 'B', seq: 1 }),
      ],
      [],
    );

    // Then they merge with the same "\n" join rule as the journal path
    expect(blocks).toEqual([{ kind: 'text', text: 'A\nB' }]);
  });

  it('should skip empty segments instead of emitting empty blocks', () => {
    // Given a segment whose accumulated text is still empty
    const blocks = memberBlocksFromLive(
      [seg({ id: 's0', kind: 'text', text: '', seq: 0 })],
      [],
    );

    // Then no block is emitted for it
    expect(blocks).toEqual([]);
  });

  it('should keep trailing whitespace within merged segments (no global trimEnd)', () => {
    // Given segment text with meaningful trailing whitespace (e.g. a fenced
    // block's indented wrapper line — same rule as the journal path)
    const blocks = memberBlocksFromLive([seg({ id: 's0', kind: 'text', text: 'A\n', seq: 0 })], []);

    // Then the trailing newline survives — no trimEnd is applied
    expect(blocks).toEqual([{ kind: 'text', text: 'A\n' }]);
  });

  it('should return only tool cards when the member has no segments (legacy engine / snapshot restore)', () => {
    // Given a running member with steps but no segments (pre-ADR-011 engine or
    // a snapshot-restored running row)
    // When normalized
    const blocks = memberBlocksFromLive(undefined, [step({ id: 'tc-1', seq: 0 })]);

    // Then only the tool card renders — the accepted degradation until settle
    expect(blocks.map((b) => b.kind)).toEqual(['tool']);
  });

  it('should return an empty list when the live member produced nothing yet', () => {
    // An empty list (not undefined) is the honest "no narrative" state the
    // settled fallback treats as authoritative when the journal is empty.
    expect(memberBlocksFromLive(undefined, undefined)).toEqual([]);
  });
});

describe('liveOpenSegmentId — the streaming block anchor (ADR-011 ③)', () => {
  it('should return the last segment id as the open segment', () => {
    // Given segments [a, b]
    const segments = [seg({ id: 'a', kind: 'text', text: 'A', seq: 0 }), seg({ id: 'b', kind: 'text', text: 'B', seq: 1 })];

    // When asked for the open segment
    // Then the last one wins — after normalization its mapped block is the
    // streaming tail
    expect(liveOpenSegmentId(segments)).toBe('b');
  });

  it('should return undefined when the member has no segments', () => {
    // Given no segments at all (member produced nothing / legacy engine)
    // Then there is no streaming anchor — no block claims streaming
    expect(liveOpenSegmentId(undefined)).toBeUndefined();
    expect(liveOpenSegmentId([])).toBeUndefined();
  });
});

describe('liveTextTail — the activity-line derivation (ADR-011 ②)', () => {
  it('should return the last text segment text when segments end in a text segment', () => {
    // Given a member whose latest segment is text
    const segments = [
      seg({ id: 'a', kind: 'thinking', text: 'planning', seq: 0 }),
      seg({ id: 'b', kind: 'text', text: 'line one\nCalling Read: a.ts', seq: 1 }),
    ];

    // Then the tail is that segment's accumulated text — the latest line is
    // what the activity row shows
    expect(liveTextTail(segments)).toBe('line one\nCalling Read: a.ts');
  });

  it('should return undefined when the last segment is a thinking segment', () => {
    // Given a member whose last arrival was reasoning, not output
    const segments = [
      seg({ id: 'a', kind: 'text', text: 'earlier output', seq: 0 }),
      seg({ id: 'b', kind: 'thinking', text: 'still reasoning', seq: 1 }),
    ];

    // Then there is no text tail — the last TEXT segment is not the last
    // segment, and the retired single-field accumulation would have shown the
    // stale earlier output as if it were current
    expect(liveTextTail(segments)).toBeUndefined();
  });

  it('should return undefined when the last text segment is still empty', () => {
    // Given a segment that opened but has no accumulated text yet
    const segments = [seg({ id: 'a', kind: 'text', text: '', seq: 0 })];

    // Then there is nothing to show — an empty tail is not a tail
    expect(liveTextTail(segments)).toBeUndefined();
  });

  it('should return undefined when the member has no segments', () => {
    // Given a legacy-engine / snapshot-restored member with no segments
    // Then no tail is derived — the caller falls back to the legacy field
    expect(liveTextTail(undefined)).toBeUndefined();
    expect(liveTextTail([])).toBeUndefined();
  });
});

describe('liveStreamingView — the MemberFocus streaming decision (ADR-011 ③)', () => {
  it('should mark the last non-tool block as streaming when the member is live with an open segment', () => {
    // Given a live member whose narrative ends in a growing text block
    const segments: NarrativeSegment[] = [
      seg({ id: 's0', kind: 'text', text: 'Working.', seq: 0 }),
    ];
    const steps: AgentStep[] = [];

    // Then the last block (the open segment's mapped block) streams
    const rendered = renderBlocksFromBlocks(memberBlocksFromLive(segments, steps));
    const view = liveStreamingView(segments, true, rendered);
    expect(view(rendered.at(-1)!)).toBe(true);
  });

  it('should finalize a text block once a tool step grows past it (no streaming flip)', () => {
    // Given a live member whose text block was overtaken by a later tool step
    const segments: NarrativeSegment[] = [
      seg({ id: 's0', kind: 'text', text: 'Done explaining.', seq: 0 }),
    ];
    const steps: AgentStep[] = [step({ id: 'tc-1', seq: 1 })];

    // When checking the text block (now non-last)
    const rendered = renderBlocksFromBlocks(memberBlocksFromLive(segments, steps));
    const view = liveStreamingView(segments, true, rendered);
    const textBlock = rendered.find((b) => b.kind === 'text')!;

    // Then it is final on the spot — the streaming flag does not flip it back
    expect(view(textBlock)).toBe(false);
    // And the trailing tool block never claims streaming
    expect(view(rendered.at(-1)!)).toBe(false);
  });

  it('should stream again when a new segment opens after the tools (last block re-enters streaming)', () => {
    // Given tools that overtook the first text, then a newly opened segment
    const segments: NarrativeSegment[] = [
      seg({ id: 's0', kind: 'text', text: 'First.', seq: 0 }),
      seg({ id: 's2', kind: 'text', text: 'Second.', seq: 2 }),
    ];
    const steps: AgentStep[] = [step({ id: 'tc-1', seq: 1 })];

    const rendered = renderBlocksFromBlocks(memberBlocksFromLive(segments, steps));
    const view = liveStreamingView(segments, true, rendered);
    // First text block is final; the new tail streams again
    expect(view(rendered.find((b) => b.kind === 'text')!)).toBe(false);
    expect(view(rendered.at(-1)!)).toBe(true);
  });

  it('should mark nothing as streaming when the member has no segments or is not live', () => {
    // Given a legacy member (no segments — tool cards only)
    const steps: AgentStep[] = [step({ id: 'tc-1', seq: 0 })];
    const legacyRendered = renderBlocksFromBlocks(memberBlocksFromLive(undefined, steps));
    expect(liveStreamingView(undefined, true, legacyRendered)(legacyRendered.at(-1)!)).toBe(false);

    // And a live member with segments, viewed after settle (isLive=false)
    const segments: NarrativeSegment[] = [seg({ id: 's0', kind: 'text', text: 'x', seq: 0 })];
    const settledRendered = renderBlocksFromBlocks(memberBlocksFromLive(segments, []));
    expect(liveStreamingView(segments, false, settledRendered)(settledRendered[0]!)).toBe(false);
  });
});

describe('settle structural equivalence (ADR-011 / P6-4 acceptance anchor)', () => {
  it('should produce the same block structure from live segments+steps as from the equivalent settled journal', () => {
    // Given the same member process expressed two ways — live fields
    // (segments + steps) and the equivalent journal messages
    const segments: NarrativeSegment[] = [
      seg({ id: 's0', kind: 'thinking', text: 'Plan first.', seq: 0 }),
      seg({ id: 's1', kind: 'text', text: 'Reading the file.', seq: 1 }),
      seg({ id: 's4', kind: 'text', text: 'Now editing.', seq: 4 }),
    ];
    const steps: AgentStep[] = [
      step({ id: 'tc-1', name: 'Read', arg: 'a.ts', seq: 2, status: 'ok', outputLines: ['read body'] }),
      step({ id: 'tc-2', name: 'Edit', arg: 'a.ts old new', seq: 3, status: 'error', outputLines: ['boom'] }),
    ];
    const messages: AppMessage[] = [
      {
        id: 'm1',
        sessionId: 's1',
        role: 'assistant',
        createdAt: '2026-01-01T00:00:00Z',
        content: [
          { type: 'thinking', thinking: 'Plan first.' },
          { type: 'text', text: 'Reading the file.' },
          { type: 'toolUse', toolCallId: 'tc-1', toolName: 'Read', input: 'a.ts' },
        ],
      },
      {
        id: 'm2',
        sessionId: 's1',
        role: 'tool',
        createdAt: '2026-01-01T00:00:01Z',
        content: [{ type: 'toolResult', toolCallId: 'tc-1', output: 'read body', isError: false }],
      },
      {
        id: 'm3',
        sessionId: 's1',
        role: 'assistant',
        createdAt: '2026-01-01T00:00:02Z',
        content: [
          { type: 'toolUse', toolCallId: 'tc-2', toolName: 'Edit', input: 'a.ts old new' },
          { type: 'text', text: 'Now editing.' },
        ],
      },
      {
        id: 'm4',
        sessionId: 's1',
        role: 'tool',
        createdAt: '2026-01-01T00:00:03Z',
        content: [{ type: 'toolResult', toolCallId: 'tc-2', output: 'boom', isError: true }],
      },
    ];

    // When both normalizations run
    const live = memberBlocksFromLive(segments, steps);
    const settled = memberBlocksFromMessages(messages);

    // Then the block structure is equivalent — kind sequence, text content
    // and tool-card pairing all match, so settle swaps the source without a
    // visible re-layout
    const shape = (blocks: TurnBlock[]) =>
      blocks.map((b) => {
        if (b.kind === 'thinking') return { kind: b.kind, text: b.thinking };
        if (b.kind === 'text') return { kind: b.kind, text: b.text };
        if (b.kind === 'tool') return { kind: b.kind, tool: { id: b.tool.id, name: b.tool.name, arg: b.tool.arg, status: b.tool.status, output: b.tool.output } };
        throw new Error(`unexpected block kind: ${b.kind}`);
      });
    expect(shape(live)).toEqual(shape(settled));
  });
});

describe('memberBlocksFromMessages — settled journal normalization', () => {
  it('should pair a toolUse with its toolResult and keep happening order', () => {
    const messages: AppMessage[] = [
      {
        id: 'm1',
        sessionId: 's1',
        role: 'assistant',
        createdAt: '2026-01-01T00:00:00Z',
        content: [
          { type: 'text', text: 'before' },
          { type: 'toolUse', toolCallId: 'tc-1', toolName: 'Read', input: { path: 'a.ts' } },
        ],
      },
      {
        id: 'm2',
        sessionId: 's1',
        role: 'tool',
        createdAt: '2026-01-01T00:00:01Z',
        content: [{ type: 'toolResult', toolCallId: 'tc-1', output: 'file body', isError: false }],
      },
    ];
    const blocks = memberBlocksFromMessages(messages);
    expect(blocks).toHaveLength(2);
    expect(blocks[0]).toMatchObject({ kind: 'text', text: 'before' });
    expect(blocks[1]).toMatchObject({
      kind: 'tool',
      tool: { id: 'tc-1', name: 'Read', status: 'ok', output: ['file body'] },
    });
  });

  it('should drop an orphan toolResult with no open tool card', () => {
    const messages: AppMessage[] = [
      {
        id: 'm1',
        sessionId: 's1',
        role: 'tool',
        createdAt: '2026-01-01T00:00:00Z',
        content: [{ type: 'toolResult', toolCallId: 'ghost', output: 'x', isError: false }],
      },
    ];
    expect(memberBlocksFromMessages(messages)).toEqual([]);
  });

  it('should merge consecutive same-kind text segments', () => {
    const messages: AppMessage[] = [
      {
        id: 'm1',
        sessionId: 's1',
        role: 'assistant',
        createdAt: '2026-01-01T00:00:00Z',
        content: [
          { type: 'text', text: 'one' },
          { type: 'text', text: 'two' },
        ],
      },
    ];
    expect(memberBlocksFromMessages(messages)).toEqual([{ kind: 'text', text: 'one\ntwo' }]);
  });
});

describe('settledNarrativeBlocks — settled member narrative fallback (ADR-015 / RC-2)', () => {
  it('should render the journal blocks when a settled member has a non-empty history', () => {
    // Given a loaded non-empty journal and leftover live blocks
    const history = [{ kind: 'text' as const, text: 'journal narrative' }];
    const liveBlocks = [{ kind: 'text' as const, text: 'live narrative' }];

    // When the settled fallback decision runs
    // Then the journal is authoritative — the journal's blocks win
    expect(settledNarrativeBlocks(history, liveBlocks)).toBe(history);
  });

  it('should fall back to the live blocks while the journal read is still in flight', () => {
    // Given a still-loading journal (null) and leftover live blocks
    const liveBlocks = [{ kind: 'text' as const, text: 'live narrative' }];

    // When the settled fallback decision runs
    // Then the panel does not blank between the live stream and the journal
    // read — the last live blocks are kept
    expect(settledNarrativeBlocks(null, liveBlocks)).toBe(liveBlocks);
  });

  it('should render no narrative section when the journal is empty (authoritative empty)', () => {
    // Given a loaded journal with no blocks and leftover live blocks
    const liveBlocks = [{ kind: 'text' as const, text: 'live narrative' }];

    // When the settled fallback decision runs
    // Then the empty journal is authoritative — no narrative renders (an
    // empty journal means the member produced no narrative, not "still
    // loading")
    expect(settledNarrativeBlocks([], liveBlocks)).toBeNull();
  });

  it('should render no narrative section when the journal is in flight and no live blocks exist', () => {
    // Given a still-loading journal (null) and no live blocks either
    // When the settled fallback decision runs
    // Then the fallback has nothing to keep — no narrative section
    expect(settledNarrativeBlocks(null, [])).toBeNull();
  });
});
