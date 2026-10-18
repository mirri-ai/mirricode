// apps/mirri-web/test/member-blocks.test.ts
// Spec for the member narrative's content-block read faces (supervision
// ADR-009): the settled journal path and the live task path must both produce
// the main transcript's ordered block model (thinking / text / tool cards),
// with tool pairing, status and order preserved.
import { describe, expect, it } from 'vitest';
import { memberBlocksFromLive, memberBlocksFromMessages } from '../src/lib/memberBlocks';
import type { AppMessage, AppMessageContent, AgentStep } from '../src/api/types';

function message(role: AppMessage['role'], content: AppMessageContent[]): AppMessage {
  return {
    id: `msg_${Math.random().toString(36).slice(2)}`,
    sessionId: 'sess_1',
    role,
    content,
    createdAt: '2026-10-18T00:00:00.000Z',
  };
}

describe('memberBlocksFromMessages', () => {
  it('should produce thinking, text and tool blocks in happening order when the journal interleaves them', () => {
    // Given a member journal: thinking → text → tool call → result → final text.
    const messages: AppMessage[] = [
      message('assistant', [
        { type: 'thinking', thinking: 'Let me look at the repo first.' },
        { type: 'text', text: 'Investigating the panel rendering.' },
        { type: 'toolUse', toolCallId: 'tc_1', toolName: 'Read', input: { path: 'a.ts' } },
      ]),
      message('tool', [{ type: 'toolResult', toolCallId: 'tc_1', output: 'file body', isError: false }]),
      message('assistant', [{ type: 'text', text: 'The panel needs restructuring.' }]),
    ];

    const blocks = memberBlocksFromMessages(messages);

    // Then the block kinds follow the journal's true order.
    expect(blocks.map((b) => b.kind)).toEqual(['thinking', 'text', 'tool', 'text']);
  });

  it('should pair a tool result onto its tool card with status and output when the journal contains both', () => {
    const messages: AppMessage[] = [
      message('assistant', [{ type: 'toolUse', toolCallId: 'tc_1', toolName: 'Bash', input: 'cargo test' }]),
      message('tool', [{ type: 'toolResult', toolCallId: 'tc_1', output: 'test result: ok', isError: true }]),
    ];

    const [toolBlock] = memberBlocksFromMessages(messages);

    expect(toolBlock?.kind).toBe('tool');
    if (toolBlock?.kind === 'tool') {
      expect(toolBlock.tool.name).toBe('Bash');
      expect(toolBlock.tool.status).toBe('error');
      expect(toolBlock.tool.output).toEqual(['test result: ok']);
      expect(toolBlock.tool.arg).toBe('cargo test');
    }
  });

  it('should keep a tool card running when the journal ends before its result arrives', () => {
    const messages: AppMessage[] = [
      message('assistant', [{ type: 'toolUse', toolCallId: 'tc_1', toolName: 'Read', input: 'b.ts' }]),
    ];

    const [toolBlock] = memberBlocksFromMessages(messages);

    expect(toolBlock?.kind).toBe('tool');
    if (toolBlock?.kind === 'tool') expect(toolBlock.tool.status).toBe('running');
  });

  it('should drop an orphan tool result when the journal starts mid-flight without its call', () => {
    const messages: AppMessage[] = [
      message('tool', [{ type: 'toolResult', toolCallId: 'tc_missing', output: 'late output', isError: false }]),
    ];

    expect(memberBlocksFromMessages(messages)).toEqual([]);
  });

  it('should merge consecutive text segments into one block instead of fragmenting them', () => {
    const messages: AppMessage[] = [
      message('assistant', [{ type: 'text', text: 'Part one.' }]),
      message('assistant', [{ type: 'text', text: 'Part two.' }]),
    ];

    const blocks = memberBlocksFromMessages(messages);

    expect(blocks).toEqual([{ kind: 'text', text: 'Part one.\nPart two.' }]);
  });

  it('should keep toolUse progress lines as the running card output when present', () => {
    const messages: AppMessage[] = [
      message('assistant', [
        {
          type: 'toolUse',
          toolCallId: 'tc_1',
          toolName: 'Bash',
          input: 'sleep 1',
          outputLines: ['line 1', 'line 2'],
        },
      ]),
    ];

    const [toolBlock] = memberBlocksFromMessages(messages);

    expect(toolBlock?.kind).toBe('tool');
    expect(toolBlock?.kind === 'tool' && toolBlock.tool.output).toEqual(['line 1', 'line 2']);
  });
});

describe('memberBlocksFromLive', () => {
  function step(overrides: Partial<AgentStep> = {}): AgentStep {
    return {
      id: 'tc_1',
      name: 'Read',
      arg: 'a.ts',
      status: 'ok',
      outputLines: [],
      ...overrides,
    };
  }

  it('should render structured steps as tool cards followed by the streamed text tail', () => {
    const blocks = memberBlocksFromLive('Summarizing findings.', [
      step(),
      step({ id: 'tc_2', name: 'Bash', arg: 'ls', status: 'running', outputLines: ['a', 'b'] }),
    ]);

    expect(blocks.map((b) => b.kind)).toEqual(['tool', 'tool', 'text']);
    if (blocks[1]?.kind === 'tool') {
      expect(blocks[1].tool.status).toBe('running');
      expect(blocks[1].tool.output).toEqual(['a', 'b']);
    }
    if (blocks[2]?.kind === 'text') expect(blocks[2].text).toBe('Summarizing findings.');
  });

  it('should return only tool cards when the member has streamed no text yet', () => {
    const blocks = memberBlocksFromLive(undefined, [step()]);
    expect(blocks.map((b) => b.kind)).toEqual(['tool']);
  });

  it('should lead with the member reasoning thinking block when present', () => {
    const blocks = memberBlocksFromLive('Summary.', [step()], 'Pondering the approach.');
    expect(blocks[0]).toEqual({ kind: 'thinking', thinking: 'Pondering the approach.' });
    expect(blocks.at(-1)?.kind).toBe('text');
  });

  it('should return an empty stream when the live member has produced nothing', () => {
    expect(memberBlocksFromLive(undefined, undefined)).toEqual([]);
  });
});
