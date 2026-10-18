import { describe, expect, it } from 'vitest';
import type { ChatTurn, ToolCall, TurnBlock } from '../src/types';
// Raw SFC source, so the fork-at-turn button's placement can be asserted
// without mounting the component (the test suite has no component runner).
import ChatPaneSource from '../src/components/chat/ChatPane.vue?raw';
import {
  assistantRenderBlocks,
  assistantTurnRenderPlan,
  forkAnchorOf,
  formatDuration,
  formatTokens,
  groupAssistantRuns,
  rendersToolCard,
  renderBlockKey,
  runFinalText,
  toolStackPosition,
  turnBlocks,
  turnFinalText,
  turnToMarkdown,
} from '../src/components/chatTurnRendering';

function tool(id: string, over: Partial<ToolCall> = {}): ToolCall {
  return { id, name: 'read', arg: `· ${id}.ts`, status: 'ok', ...over };
}

function toolBlock(id: string, over: Partial<ToolCall> = {}): Extract<TurnBlock, { kind: 'tool' }> {
  return { kind: 'tool', tool: tool(id, over) };
}

function assistantTurn(blocks: TurnBlock[], over: Partial<ChatTurn> = {}): ChatTurn {
  return { id: 't1', role: 'assistant', no: 1, text: '', blocks, ...over };
}

function userTurn(id = 'u1'): ChatTurn {
  return { id, role: 'user', no: 1, text: 'question' };
}

function textBlock(text: string): Extract<TurnBlock, { kind: 'text' }> {
  return { kind: 'text', text };
}

function notificationBlock(): Extract<TurnBlock, { kind: 'notification' }> {
  return {
    kind: 'notification',
    text: 'background task done',
    notification: { taskId: 'task-1', status: 'completed' },
  };
}

/** An empty run built inline for runFinalText unit tests. */
function runOf(turns: ChatTurn[]): { turns: ChatTurn[]; startIndex: number; endIndex: number } {
  return { turns, startIndex: 0, endIndex: turns.length - 1 };
}

describe('formatTokens', () => {
  it('keeps counts under 1024 verbatim and uses 1024-based k / M units', () => {
    expect(formatTokens(0)).toBe('0');
    expect(formatTokens(999)).toBe('999');
    expect(formatTokens(1000)).toBe('1000');
    expect(formatTokens(1500)).toBe('1.5k');
    expect(formatTokens(1_000_000)).toBe('977k');
    expect(formatTokens(2_500_000)).toBe('2.4M');
  });
});

describe('formatDuration', () => {
  it('switches units at the 1s and 1m boundaries', () => {
    expect(formatDuration(999)).toBe('999ms');
    expect(formatDuration(1000)).toBe('1.0s');
    expect(formatDuration(59_999)).toBe('60.0s');
    expect(formatDuration(60_000)).toBe('1m0.0s');
    expect(formatDuration(90_500)).toBe('1m30.5s');
  });
});

describe('turnBlocks', () => {
  it('returns the ordered blocks as-is when present', () => {
    const blocks: TurnBlock[] = [{ kind: 'text', text: 'hi' }];
    expect(turnBlocks(assistantTurn(blocks))).toBe(blocks);
  });

  it('falls back to thinking -> text -> tools order when blocks are absent', () => {
    const turn: ChatTurn = {
      id: 't1',
      role: 'assistant',
      no: 1,
      text: 'answer',
      thinking: 'plan',
      tools: [tool('a')],
    };
    expect(turnBlocks(turn)).toEqual([
      { kind: 'thinking', thinking: 'plan' },
      { kind: 'text', text: 'answer' },
      { kind: 'tool', tool: tool('a') },
    ]);
  });
});

describe('rendersToolCard', () => {
  it('hides the card only for a successful tool that carries inline media', () => {
    expect(rendersToolCard(toolBlock('a'))).toBe(true);
    expect(rendersToolCard(toolBlock('r', { status: 'running' }))).toBe(true);
    expect(
      rendersToolCard(toolBlock('m', { status: 'ok', media: { kind: 'image', url: 'x' } })),
    ).toBe(false);
    // media but errored -> still rendered as a card
    expect(
      rendersToolCard(toolBlock('e', { status: 'error', media: { kind: 'image', url: 'x' } })),
    ).toBe(true);
  });
});

describe('toolStackPosition', () => {
  it('marks a lone tool single and otherwise reports first/middle/last', () => {
    expect(toolStackPosition(0, 1)).toBe('single');
    expect(toolStackPosition(0, 0)).toBe('single');
    expect(toolStackPosition(0, 3)).toBe('first');
    expect(toolStackPosition(1, 3)).toBe('middle');
    expect(toolStackPosition(2, 3)).toBe('last');
  });
});

describe('assistantRenderBlocks', () => {
  it('groups consecutive renderable tools into one tool-stack', () => {
    const rendered = assistantRenderBlocks(assistantTurn([toolBlock('a'), toolBlock('b')]));
    expect(rendered).toHaveLength(1);
    expect(rendered[0]).toMatchObject({ kind: 'tool-stack' });
    if (rendered[0]?.kind === 'tool-stack') {
      expect(rendered[0].tools.map((t) => t.tool.id)).toEqual(['a', 'b']);
      expect(rendered[0].tools.map((t) => t.sourceIndex)).toEqual([0, 1]);
    }
  });

  it('renders a lone tool as a standalone tool, not a stack', () => {
    const rendered = assistantRenderBlocks(assistantTurn([toolBlock('a')]));
    expect(rendered).toEqual([{ kind: 'tool', tool: tool('a'), sourceIndex: 0 }]);
  });

  it('breaks the stack when a non-tool block interrupts the run', () => {
    const rendered = assistantRenderBlocks(
      assistantTurn([toolBlock('a'), { kind: 'text', text: 'x' }, toolBlock('b')]),
    );
    expect(rendered.map((b) => b.kind)).toEqual(['tool', 'text', 'tool']);
  });

  it('breaks the stack when a media tool (no card) interrupts the run', () => {
    const rendered = assistantRenderBlocks(
      assistantTurn([
        toolBlock('a'),
        toolBlock('b'),
        toolBlock('c', { status: 'ok', media: { kind: 'image', url: 'x' } }),
      ]),
    );
    expect(rendered.map((b) => b.kind)).toEqual(['tool-stack', 'tool']);
    if (rendered[0]?.kind === 'tool-stack') {
      expect(rendered[0].tools.map((t) => t.tool.id)).toEqual(['a', 'b']);
    }
  });

  it('preserves thinking/text order with their source indexes', () => {
    const rendered = assistantRenderBlocks(
      assistantTurn([
        { kind: 'thinking', thinking: 'plan' },
        { kind: 'text', text: 'answer' },
      ]),
    );
    expect(rendered).toEqual([
      { kind: 'thinking', thinking: 'plan', sourceIndex: 0 },
      { kind: 'text', text: 'answer', sourceIndex: 1 },
    ]);
  });
});

describe('turnFinalText', () => {
  it('joins only the text blocks, dropping thinking and tools', () => {
    const turn = assistantTurn([
      { kind: 'thinking', thinking: 'plan' },
      { kind: 'text', text: 'first' },
      toolBlock('a'),
      { kind: 'text', text: 'second' },
    ]);
    expect(turnFinalText(turn)).toBe('first\n\nsecond');
  });
});

describe('turnToMarkdown', () => {
  it('renders thinking as a quote, text verbatim, and tool output as a fenced block', () => {
    const turn = assistantTurn([
      { kind: 'thinking', thinking: 'line1\nline2' },
      { kind: 'text', text: 'hello' },
      toolBlock('a', { name: 'bash', output: ['out1', 'out2'] }),
    ]);
    expect(turnToMarkdown(turn)).toBe(
      ['> **Thinking**\n> line1\n> line2', 'hello', '```\n[bash]\nout1\nout2\n```'].join('\n\n'),
    );
  });
});

describe('renderBlockKey', () => {
  it('derives stable keys per block kind', () => {
    expect(renderBlockKey({ kind: 'text', text: 'x', sourceIndex: 2 }, 0)).toBe('text-2');
    expect(renderBlockKey({ kind: 'tool', tool: tool('a'), sourceIndex: 3 }, 0)).toBe('a');
    expect(
      renderBlockKey({ kind: 'tool-stack', tools: [{ tool: tool('a'), sourceIndex: 5 }] }, 0),
    ).toBe('tool-stack-5');
  });
});

describe('fork-at-turn button placement', () => {
  // The per-turn key (`turn-<n>`, the fork anchor) is stamped on assistant
  // rows by the rust engine's message read faces — the button must live on
  // the assistant run-end footer next to the copy button, never on the user
  // message (user rows carry no anchor, so a user-side gate can never pass).
  const footerIdx = ChatPaneSource.indexOf('class="a-msg-ft"');
  const userMetaIdx = ChatPaneSource.indexOf('class="u-meta"');
  const forkIdx = ChatPaneSource.indexOf("t('conversation.forkAtTurnTooltip')");
  const assistantCopyIdx = ChatPaneSource.indexOf('@click="copyAssistantRun(ti)"');
  const userCopyIdx = ChatPaneSource.indexOf('@click.stop="copyUserMessage(turn)"');

  it('should place the fork action inside the assistant run-end footer, next to the copy button', () => {
    expect(footerIdx).toBeGreaterThan(-1);
    expect(forkIdx).toBeGreaterThan(footerIdx);
    expect(forkIdx).toBeLessThan(assistantCopyIdx);
  });

  it('should not place the fork action on the user message row', () => {
    expect(ChatPaneSource.match(/forkAtTurnTooltip/g)).toHaveLength(1);
    expect(forkIdx).toBeGreaterThan(userCopyIdx);
    expect(userCopyIdx).toBeGreaterThan(userMetaIdx);
  });

  it('should gate the fork action on a resolvable per-turn anchor key', () => {
    const forkButton = ChatPaneSource.slice(forkIdx - 200, forkIdx);
    expect(forkButton).toContain('forkAnchorOf(turn.promptId) !== undefined');
  });
});

describe('forkAnchorOf', () => {
  it('should resolve the numeric anchor from a durable turn key', () => {
    expect(forkAnchorOf('turn-1')).toBe(1);
    expect(forkAnchorOf('turn-42')).toBe(42);
  });

  it('should reject the live correlation key, aborted brackets, and missing keys', () => {
    expect(forkAnchorOf('turn-0192aaaa-bbbb-cccc-dddd-eeeeffff0000')).toBeUndefined();
    expect(forkAnchorOf('aborted-3')).toBeUndefined();
    expect(forkAnchorOf(undefined)).toBeUndefined();
    expect(forkAnchorOf('turn-x')).toBeUndefined();
  });
});

describe('groupAssistantRuns', () => {
  it('should group consecutive assistant turns into one run', () => {
    const turns = [
      userTurn('u1'),
      assistantTurn([textBlock('a')], { id: 'a1' }),
      assistantTurn([textBlock('b')], { id: 'a2' }),
      userTurn('u2'),
    ];
    expect(groupAssistantRuns(turns)).toEqual([
      { turns: [turns[1], turns[2]], startIndex: 1, endIndex: 2 },
    ]);
  });

  it('should split runs when a user turn separates assistant turns', () => {
    const turns = [
      assistantTurn([textBlock('a')], { id: 'a1' }),
      userTurn('u1'),
      assistantTurn([textBlock('b')], { id: 'a2' }),
    ];
    const runs = groupAssistantRuns(turns);
    expect(runs).toHaveLength(2);
    expect(runs[0]).toMatchObject({ startIndex: 0, endIndex: 0 });
    expect(runs[1]).toMatchObject({ startIndex: 2, endIndex: 2 });
  });

  it('should treat divider roles (compaction, modelSwitch) as run splits', () => {
    const compaction: ChatTurn = { id: 'c1', role: 'compaction', no: 1, text: 'summary' };
    const modelSwitch: ChatTurn = { id: 'm1', role: 'modelSwitch', no: 2, text: '' };
    const turns = [
      assistantTurn([textBlock('a')], { id: 'a1' }),
      compaction,
      assistantTurn([textBlock('b')], { id: 'a2' }),
      modelSwitch,
      assistantTurn([textBlock('c')], { id: 'a3' }),
    ];
    const runs = groupAssistantRuns(turns);
    expect(runs.map((r) => [r.startIndex, r.endIndex])).toEqual([
      [0, 0],
      [2, 2],
      [4, 4],
    ]);
  });

  it('should close a trailing run at the end of the transcript', () => {
    const turns = [
      userTurn('u1'),
      assistantTurn([textBlock('a')], { id: 'a1' }),
      assistantTurn([textBlock('b')], { id: 'a2' }),
    ];
    expect(groupAssistantRuns(turns)).toEqual([
      { turns: [turns[1], turns[2]], startIndex: 1, endIndex: 2 },
    ]);
  });

  it('should return no runs when the transcript has no assistant turns', () => {
    expect(groupAssistantRuns([userTurn('u1')])).toEqual([]);
    expect(groupAssistantRuns([])).toEqual([]);
  });
});

describe('runFinalText', () => {
  it('should return the run-end terminal text when the run ends on a text block', () => {
    const run = runOf([
      assistantTurn([textBlock('intermediate')], { id: 'a1' }),
      assistantTurn(
        [{ kind: 'thinking', thinking: 'plan' }, toolBlock('x'), textBlock('final')],
        { id: 'a2' },
      ),
    ]);
    expect(runFinalText(run)).toBe('final');
  });

  it('should return empty when the run ends on a tool call', () => {
    const run = runOf([
      assistantTurn([textBlock('answer'), toolBlock('x')], { id: 'a1' }),
    ]);
    expect(runFinalText(run)).toBe('');
  });

  it('should not pull text from an earlier turn forward when the run ends on a tool call', () => {
    const run = runOf([
      assistantTurn([textBlock('earlier conclusion')], { id: 'a1' }),
      assistantTurn([toolBlock('x')], { id: 'a2' }),
    ]);
    expect(runFinalText(run)).toBe('');
  });

  it('should return the text before trailing notification blocks when the run ends on notifications', () => {
    // Historical replay shape (pre-core-fix): a background-task notification
    // folded into the turn tail after the final summary text.
    const run = runOf([
      assistantTurn([textBlock('final summary'), notificationBlock()], { id: 'a1' }),
    ]);
    expect(runFinalText(run)).toBe('final summary');
  });

  it('should return empty when a tool call follows the trailing notification blocks', () => {
    const run = runOf([
      assistantTurn([textBlock('final summary'), notificationBlock(), toolBlock('x')], { id: 'a1' }),
    ]);
    expect(runFinalText(run)).toBe('');
  });

  it('should return the run-end text when the run ends on a plain text block', () => {
    const run = runOf([
      assistantTurn([toolBlock('x'), textBlock('done')], { id: 'a1' }),
    ]);
    expect(runFinalText(run)).toBe('done');
  });

  it('should return empty for a run with no turns', () => {
    expect(runFinalText(runOf([]))).toBe('');
  });
});

describe('assistantTurnRenderPlan', () => {
  const noneExpanded: ReadonlySet<string> = new Set();
  const thinkingBlock: TurnBlock = { kind: 'thinking', thinking: 'plan' };

  it('should fold a completed run into header + final text and hide the rest', () => {
    const turns = [
      userTurn('u1'),
      // run-start turn carries thinking + intermediate text + a tool call
      assistantTurn([thinkingBlock, textBlock('mid'), toolBlock('x')], { id: 'a1' }),
      // run-end turn carries thinking + the terminal text
      assistantTurn([thinkingBlock, textBlock('final')], { id: 'a2' }),
      userTurn('u2'),
    ];
    const startPlan = assistantTurnRenderPlan(turns, 1, null, noneExpanded);
    const endPlan = assistantTurnRenderPlan(turns, 2, null, noneExpanded);
    expect(startPlan).toMatchObject({
      runKey: 'a1',
      isRunStart: true,
      isRunEnd: false,
      streaming: false,
      folded: true,
      renderBlocks: false,
      finalText: '',
      visible: true,
    });
    expect(endPlan).toMatchObject({
      runKey: 'a1',
      isRunStart: false,
      isRunEnd: true,
      streaming: false,
      folded: true,
      renderBlocks: false,
      finalText: 'final',
      visible: true,
    });
  });

  it('should hide the middle turns of a folded multi-turn run entirely', () => {
    const turns = [
      assistantTurn([textBlock('a')], { id: 'a1' }),
      assistantTurn([textBlock('b')], { id: 'a2' }),
      assistantTurn([textBlock('c')], { id: 'a3' }),
    ];
    const middle = assistantTurnRenderPlan(turns, 1, null, noneExpanded);
    expect(middle).toMatchObject({
      runKey: 'a1',
      folded: true,
      renderBlocks: false,
      finalText: '',
      visible: false,
    });
  });

  it('should never fold the streaming run, for every turn in it', () => {
    const turns = [
      assistantTurn([textBlock('a')], { id: 'a1' }),
      assistantTurn([textBlock('b')], { id: 'a2' }),
    ];
    const first = assistantTurnRenderPlan(turns, 0, 'a2', noneExpanded);
    const last = assistantTurnRenderPlan(turns, 1, 'a2', noneExpanded);
    expect(first).toMatchObject({ streaming: true, folded: false, renderBlocks: true, visible: true });
    expect(last).toMatchObject({ streaming: true, folded: false, renderBlocks: true, finalText: '', visible: true });
  });

  it('should expand a completed run whose key is in the expanded set', () => {
    const turns = [
      assistantTurn([toolBlock('x'), textBlock('final')], { id: 'a1' }),
    ];
    const plan = assistantTurnRenderPlan(turns, 0, null, new Set(['a1']));
    expect(plan).toMatchObject({
      folded: false,
      renderBlocks: true,
      // expanded runs render the terminal text inside the blocks — no duplicate
      finalText: '',
    });
  });

  it('should return null for a non-assistant turn', () => {
    expect(assistantTurnRenderPlan([userTurn('u1')], 0, null, noneExpanded)).toBeNull();
  });

  it('should carry the run-end duration and createdAt onto every plan of the run', () => {
    const turns = [
      assistantTurn([textBlock('a')], { id: 'a1' }),
      assistantTurn([textBlock('b')], { id: 'a2', durationMs: 1500, createdAt: '2025-01-01T10:00:00.000Z' }),
    ];
    const startPlan = assistantTurnRenderPlan(turns, 0, null, noneExpanded);
    const endPlan = assistantTurnRenderPlan(turns, 1, null, noneExpanded);
    expect(startPlan).toMatchObject({ durationMs: 1500, createdAt: '2025-01-01T10:00:00.000Z' });
    expect(endPlan).toMatchObject({ durationMs: 1500, createdAt: '2025-01-01T10:00:00.000Z' });
  });

  it('should work for turns built from aggregate fields only (no blocks)', () => {
    // Aggregate fallback order is thinking -> text -> tools, so the terminal
    // block is the tool: no final text may be pulled forward.
    const toolOnly: ChatTurn = { id: 'a1', role: 'assistant', no: 1, text: '', tools: [tool('x')] };
    const plan = assistantTurnRenderPlan([toolOnly], 0, null, noneExpanded);
    expect(plan).toMatchObject({ folded: true, renderBlocks: false, finalText: '' });

    // Aggregate text is the terminal block of the fallback blocks.
    const textOnly: ChatTurn = { id: 'a2', role: 'assistant', no: 2, text: 'done' };
    const plan2 = assistantTurnRenderPlan([textOnly], 0, null, noneExpanded);
    expect(plan2).toMatchObject({ folded: true, finalText: 'done' });
  });
});
