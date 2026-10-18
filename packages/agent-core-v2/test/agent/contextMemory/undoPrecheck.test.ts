import { describe, expect, it } from 'vitest';

import {
  computeUndoCut,
  contextUndo,
  isFullyUndoable,
} from '#/agent/contextMemory/contextOps';
import type { ContextMessage } from '#/agent/contextMemory/types';

function text(value: string): { type: 'text'; text: string } {
  return { type: 'text', text: value };
}

function user(origin?: ContextMessage['origin']): ContextMessage {
  return {
    role: 'user',
    content: [text('u')],
    toolCalls: [],
    ...(origin === undefined ? {} : { origin }),
  };
}

function assistant(): ContextMessage {
  return { role: 'assistant', content: [text('a')], toolCalls: [] };
}

function injection(): ContextMessage {
  return {
    role: 'user',
    content: [text('i')],
    toolCalls: [],
    origin: { kind: 'injection', variant: 'system_reminder' },
  };
}

function compaction(): ContextMessage {
  return {
    role: 'user',
    content: [text('sum')],
    toolCalls: [],
    origin: { kind: 'compaction_summary' },
  };
}

function inlineWrapper(activationId: string): ContextMessage {
  return {
    role: 'user',
    content: [text('User activated the skill.')],
    toolCalls: [],
    origin: {
      kind: 'skill_activation',
      activationId,
      skillName: 'review',
      trigger: 'user-inline',
    },
  };
}

const USER_ORIGIN: ContextMessage['origin'] = { kind: 'user' };

describe('computeUndoCut', () => {
  it('finds the cut for the last real user prompt', () => {
    const cut = computeUndoCut([user(USER_ORIGIN), assistant()], 1);
    expect(cut).toEqual({ cutIndex: 0, removedCount: 1, stoppedAtCompaction: false });
    expect(isFullyUndoable(cut, 1)).toBe(true);
  });

  it('skips trailing non-user messages while scanning', () => {
    const cut = computeUndoCut([user(USER_ORIGIN), assistant(), assistant()], 1);
    expect(cut.cutIndex).toBe(0);
    expect(isFullyUndoable(cut, 1)).toBe(true);
  });

  it('treats a user message without origin as a real prompt (legacy)', () => {
    const cut = computeUndoCut([user(), assistant()], 1);
    expect(cut.cutIndex).toBe(0);
    expect(isFullyUndoable(cut, 1)).toBe(true);
  });

  it('finds nothing when the history has no real user prompt', () => {
    const cut = computeUndoCut([], 1);
    expect(cut).toEqual({ cutIndex: -1, removedCount: 0, stoppedAtCompaction: false });
    expect(isFullyUndoable(cut, 1)).toBe(false);
  });

  it('skips injections without counting them', () => {
    const cut = computeUndoCut([injection(), assistant()], 1);
    expect(cut.cutIndex).toBe(-1);
    expect(isFullyUndoable(cut, 1)).toBe(false);
  });

  it('counts fewer prompts than requested as not fully undoable', () => {
    const history = [user(USER_ORIGIN), assistant(), user(USER_ORIGIN), assistant()];
    const cut = computeUndoCut(history, 3);
    expect(cut.removedCount).toBe(2);
    expect(isFullyUndoable(cut, 3)).toBe(false);
  });

  it('stops at a compaction summary', () => {
    const cut = computeUndoCut([user(USER_ORIGIN), compaction(), assistant()], 1);
    expect(cut).toEqual({ cutIndex: -1, removedCount: 0, stoppedAtCompaction: true });
    expect(isFullyUndoable(cut, 1)).toBe(false);
  });

  it('stops at a compaction summary even after counting some prompts', () => {
    const history = [user(USER_ORIGIN), compaction(), user(USER_ORIGIN), assistant()];
    const cut = computeUndoCut(history, 2);
    expect(cut.removedCount).toBe(1);
    expect(cut.stoppedAtCompaction).toBe(true);
    expect(isFullyUndoable(cut, 2)).toBe(false);
  });

  describe('when a user prompt carries inline skill references (ADR-004 decision 3)', () => {
    it('should not count a user-inline activation wrapper as an undo anchor', () => {
      // Given one body + two wrappers, when undoing one unit, the wrappers
      // must not inflate the anchor count.
      const history = [user(USER_ORIGIN), assistant(), inlineWrapper('m#0'), inlineWrapper('m#1'), user(USER_ORIGIN)];
      const cut = computeUndoCut(history, 2);
      expect(cut.removedCount).toBe(2);
      expect(cut.cutIndex).toBe(0);
    });

    it('should cut the inline wrappers together with their body as one undo unit', () => {
      // Given a turn materialized as 2 wrappers + 1 body + a reply, when
      // undoing once, the cut must absorb the wrappers ahead of the body.
      const history = [
        user(USER_ORIGIN),
        assistant(),
        inlineWrapper('m#0'),
        inlineWrapper('m#1'),
        user(USER_ORIGIN),
        assistant(),
      ];
      const cut = computeUndoCut(history, 1);
      expect(cut).toEqual({ cutIndex: 2, removedCount: 1, stoppedAtCompaction: false });
      expect(isFullyUndoable(cut, 1)).toBe(true);

      const next = contextUndo.apply(history, { count: 1 });
      expect(next).toEqual([user(USER_ORIGIN), assistant()]);
    });

    it('should absorb a wrapper that immediately precedes the cut body, and stop at other content', () => {
      // Given an assistant reply between a wrapper and the cut body (no
      // adjacency), when undoing the body, the wrapper must NOT be absorbed
      // — it belongs to an earlier turn's content.
      const history = [
        inlineWrapper('m#0'),
        assistant(),
        user(USER_ORIGIN),
        assistant(),
      ];
      const cut = computeUndoCut(history, 1);
      expect(cut.cutIndex).toBe(2);
      expect(cut.removedCount).toBe(1);
      expect(isFullyUndoable(cut, 1)).toBe(true);
      const next = contextUndo.apply(history, { count: 1 });
      expect(next).toEqual([inlineWrapper('m#0'), assistant()]);
    });

    it('should still count a user-slash skill activation as its own undo anchor', () => {
      const slashActivation: ContextMessage = {
        role: 'user',
        content: [text('/review')],
        toolCalls: [],
        origin: {
          kind: 'skill_activation',
          activationId: 'm#0',
          skillName: 'review',
          trigger: 'user-slash',
        },
      };
      const history = [user(USER_ORIGIN), assistant(), slashActivation, assistant()];
      const cut = computeUndoCut(history, 1);
      expect(cut).toEqual({ cutIndex: 2, removedCount: 1, stoppedAtCompaction: false });
    });
  });
});

describe('contextUndo op', () => {
  it('slices the history at the cut point, dropping post-cut injections too', () => {
    const state = [
      user(USER_ORIGIN),
      assistant(),
      user(USER_ORIGIN),
      injection(),
      assistant(),
    ];
    const next = contextUndo.apply(state, { count: 1 });
    expect(next).toEqual([user(USER_ORIGIN), assistant()]);
  });

  it('returns the same reference when not fully undoable', () => {
    const state = [user(USER_ORIGIN), compaction(), assistant()];
    expect(contextUndo.apply(state, { count: 1 })).toBe(state);
  });

  it.each([0, 0.5, Number.MAX_SAFE_INTEGER + 1])(
    'returns the same reference for invalid count %s',
    (count) => {
      const state = [user(USER_ORIGIN), assistant()];
      expect(contextUndo.apply(state, { count })).toBe(state);
    },
  );
});
