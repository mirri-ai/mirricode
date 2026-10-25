// apps/mirri-web/src/lib/memberBlocks.test.ts
import { describe, expect, it } from 'vitest';
import { memberBlocksFromLive, memberBlocksFromMessages, settledNarrativeBlocks } from './memberBlocks';
import type { AppMessage } from '../api/types';

describe('memberBlocksFromLive — the MemberFocus narrative contract', () => {
  it('should render streamed text as the trailing text block', () => {
    expect(memberBlocksFromLive('hello', [], undefined)).toEqual([{ kind: 'text', text: 'hello' }]);
  });

  it('should render thinking first, tool cards next, streamed text last (ADR-009 live order)', () => {
    const blocks = memberBlocksFromLive(
      'summary',
      [{ id: 'tc-1', name: 'Read', arg: 'a.ts', status: 'ok', outputLines: ['body'] }],
      'reasoning',
    );
    expect(blocks.map((b) => b.kind)).toEqual(['thinking', 'tool', 'text']);
  });

  it('should return an empty list when the live member produced nothing yet', () => {
    // An empty list (not undefined) is the honest "no narrative" state the
    // settled fallback treats as authoritative when the journal is empty.
    expect(memberBlocksFromLive('', [], '')).toEqual([]);
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
