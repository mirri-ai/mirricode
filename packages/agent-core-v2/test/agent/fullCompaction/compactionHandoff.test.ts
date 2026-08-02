/**
 * Pure unit tests for `compactionHandoff` and `PromptOrigin` disposition
 * — verifies head/tail selection, elision, compaction summary shape,
 * and that `hook_additional_context` origin messages are dropped during
 * compaction (they are transient context, not real user input).
 */

import { describe, expect, it } from 'vitest';

import {
  buildCompactionSummaryText,
  buildContextCompactionShape,
  collectCompactableUserMessages,
  compactionUserMessageDisposition,
  createCompactionElisionMessage,
  createCompactionSummaryMessage,
  isCompactionSummaryMessage,
  isRealUserInput,
  selectCompactionUserMessages,
  selectRecentUserMessages,
  COMPACTION_SUMMARY_PREFIX,
  COMPACT_USER_MESSAGE_MAX_TOKENS,
  COMPACT_USER_MESSAGE_HEAD_TOKENS,
} from '#/agent/contextMemory/compactionHandoff';
import type { ContextMessage, PromptOrigin } from '#/agent/contextMemory/types';

function userMessage(text: string, origin?: PromptOrigin): ContextMessage {
  return {
    role: 'user',
    content: [{ type: 'text', text }],
    toolCalls: [],
    origin,
  };
}

function assistantMessage(text: string): ContextMessage {
  return {
    role: 'assistant',
    content: [{ type: 'text', text }],
    toolCalls: [],
  };
}

describe('compactionHandoff', () => {
  describe('buildCompactionSummaryText', () => {
    it('should prepend the summary prefix to a non-empty summary', () => {
      const result = buildCompactionSummaryText('The user discussed caching.');
      expect(result).toBe(`${COMPACTION_SUMMARY_PREFIX}\nThe user discussed caching.`);
    });

    it('should use placeholder when summary is empty', () => {
      const result = buildCompactionSummaryText('');
      expect(result).toBe(`${COMPACTION_SUMMARY_PREFIX}\n(no summary available)`);
    });

    it('should trim the summary before joining', () => {
      const result = buildCompactionSummaryText('  padded  ');
      expect(result).toBe(`${COMPACTION_SUMMARY_PREFIX}\npadded`);
    });
  });

  describe('createCompactionSummaryMessage', () => {
    it('should create a user message with compaction_summary origin', () => {
      const msg = createCompactionSummaryMessage('summary text');
      expect(msg.role).toBe('user');
      expect(msg.content).toEqual([{ type: 'text', text: 'summary text' }]);
      expect(msg.origin).toEqual({ kind: 'compaction_summary' });
    });
  });

  describe('createCompactionElisionMessage', () => {
    it('should create a user message with injection origin and compaction_elision variant', () => {
      const msg = createCompactionElisionMessage(5000);
      expect(msg.role).toBe('user');
      expect(msg.origin).toEqual({ kind: 'injection', variant: 'compaction_elision' });
      expect(msg.content[0]).toMatchObject({ type: 'text' });
      expect((msg.content[0] as { type: 'text'; text: string }).text).toContain('5000');
    });
  });

  describe('isCompactionSummaryMessage', () => {
    it('should return true for compaction_summary origin', () => {
      expect(isCompactionSummaryMessage(createCompactionSummaryMessage('x'))).toBe(true);
    });

    it('should return false for regular user message', () => {
      expect(isCompactionSummaryMessage(userMessage('x'))).toBe(false);
    });

    it('should return false for message with no origin', () => {
      expect(isCompactionSummaryMessage(userMessage('x', undefined))).toBe(false);
    });
  });

  describe('compactionUserMessageDisposition', () => {
    it('should keep user origin', () => {
      expect(compactionUserMessageDisposition({ kind: 'user' })).toBe('keep');
    });

    it('should keep user-slash trigger in skill_activation', () => {
      expect(
        compactionUserMessageDisposition({ kind: 'skill_activation', trigger: 'user-slash' } as PromptOrigin),
      ).toBe('keep');
    });

    it('should drop model-tool trigger in skill_activation', () => {
      expect(
        compactionUserMessageDisposition({ kind: 'skill_activation', trigger: 'model-tool' } as PromptOrigin),
      ).toBe('drop');
    });

    it('should drop injection, shell_command, compaction_summary, system_trigger', () => {
      const dropKinds: PromptOrigin[] = [
        { kind: 'injection', variant: 'test' },
        { kind: 'shell_command', phase: 'input' },
        { kind: 'compaction_summary' },
        { kind: 'system_trigger', name: 'auto_compact' },
      ];
      for (const origin of dropKinds) {
        expect(compactionUserMessageDisposition(origin)).toBe('drop');
      }
    });

    it('should drop hook_result origin', () => {
      expect(
        compactionUserMessageDisposition({ kind: 'hook_result', event: 'UserPromptSubmit' }),
      ).toBe('drop');
    });

    it('should drop hook_additional_context origin', () => {
      expect(
        compactionUserMessageDisposition({ kind: 'hook_additional_context', event: 'PreCompact' }),
      ).toBe('drop');
    });

    it('should drop retry origin', () => {
      expect(compactionUserMessageDisposition({ kind: 'retry' })).toBe('drop');
    });

    it('should keep undefined origin', () => {
      expect(compactionUserMessageDisposition(undefined)).toBe('keep');
    });
  });

  describe('isRealUserInput', () => {
    it('should return true for user-role messages with keep disposition', () => {
      expect(isRealUserInput(userMessage('hello', { kind: 'user' }))).toBe(true);
    });

    it('should return false for user-role messages with drop disposition', () => {
      expect(
        isRealUserInput(userMessage('injected', { kind: 'hook_additional_context', event: 'PreCompact' })),
      ).toBe(false);
    });

    it('should return false for assistant-role messages', () => {
      expect(isRealUserInput(assistantMessage('response'))).toBe(false);
    });

    it('should return true for user-role with no origin (defaults to keep)', () => {
      expect(isRealUserInput(userMessage('no origin'))).toBe(true);
    });
  });

  describe('collectCompactableUserMessages', () => {
    it('should include real user input but exclude compaction summaries and hook additional context', () => {
      const messages: ContextMessage[] = [
        userMessage('user one', { kind: 'user' }),
        assistantMessage('assistant one'),
        userMessage('injected context', { kind: 'hook_additional_context', event: 'PreCompact' }),
        createCompactionSummaryMessage('previous compaction'),
        userMessage('user two', { kind: 'user' }),
      ];
      const result = collectCompactableUserMessages(messages);
      expect(result).toHaveLength(2);
      expect(result[0]!.content[0]).toMatchObject({ text: 'user one' });
      expect(result[1]!.content[0]).toMatchObject({ text: 'user two' });
    });

    it('should exclude hook_result origin messages', () => {
      const messages: ContextMessage[] = [
        userMessage('user prompt', { kind: 'user' }),
        userMessage('hook message', { kind: 'hook_result', event: 'UserPromptSubmit' }),
      ];
      const result = collectCompactableUserMessages(messages);
      expect(result).toHaveLength(1);
      expect(result[0]!.content[0]).toMatchObject({ text: 'user prompt' });
    });
  });

  describe('buildContextCompactionShape', () => {
    it('should build the head/tail/elision shape with a compaction summary at the end', () => {
      const history: ContextMessage[] = [
        userMessage('old user', { kind: 'user' }),
        assistantMessage('old assistant'),
        userMessage('recent user', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Summary of old conversation.',
        compactedCount: history.length,
        tokensBefore: 100,
      });

      expect(shape.summary).toBe('Summary of old conversation.');
      expect(shape.contextSummary).toContain('Summary of old conversation.');
      expect(shape.compactedCount).toBe(3);
      expect(shape.tokensBefore).toBe(100);
      // Last message should be the compaction summary
      const lastMsg = shape.messages.at(-1)!;
      expect(lastMsg.origin).toEqual({ kind: 'compaction_summary' });
    });

    it('should preserve handoff by including kept user messages before the summary', () => {
      const history: ContextMessage[] = [
        userMessage('first user', { kind: 'user' }),
        assistantMessage('first assistant'),
        userMessage('second user', { kind: 'user' }),
        assistantMessage('second assistant'),
        userMessage('third user', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Conversation summary.',
        compactedCount: history.length,
        tokensBefore: 200,
      });

      // All messages before the summary should be kept user messages
      const messagesBeforeSummary = shape.messages.slice(0, -1);
      for (const msg of messagesBeforeSummary) {
        expect(msg.role).toBe('user');
        expect(isRealUserInput(msg)).toBe(true);
      }
    });

    it('should use legacy tail shape when legacyTail is true', () => {
      const history: ContextMessage[] = [
        userMessage('kept user', { kind: 'user' }),
        assistantMessage('kept assistant'),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Legacy summary.',
        compactedCount: 1,
        tokensBefore: 50,
        legacyTail: true,
      });

      // Legacy shape: summary message + messages after compactedCount
      expect(shape.messages[0]!.origin).toEqual({ kind: 'compaction_summary' });
    });

    it('should not include hook_additional_context messages in the compaction shape', () => {
      const history: ContextMessage[] = [
        userMessage('real user', { kind: 'user' }),
        userMessage('additional ctx', { kind: 'hook_additional_context', event: 'PreCompact' }),
        userMessage('another real user', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Summary with hook context.',
        compactedCount: history.length,
        tokensBefore: 100,
      });

      const nonSummaryMessages = shape.messages.filter(
        (msg) => msg.origin?.kind !== 'compaction_summary' && msg.origin?.kind !== 'injection',
      );
      for (const msg of nonSummaryMessages) {
        expect(msg.origin?.kind).not.toBe('hook_additional_context');
      }
    });
  });

  describe('selectRecentUserMessages', () => {
    it('should select the most recent messages within token budget', () => {
      const messages = [
        userMessage('oldest'),
        userMessage('middle'),
        userMessage('newest'),
      ];
      const selected = selectRecentUserMessages(messages, 100_000);
      expect(selected).toHaveLength(3);
    });

    it('should select only recent messages when budget is tight', () => {
      const messages = [
        userMessage('oldest message with some content'),
        userMessage('middle message with some content'),
        userMessage('newest'),
      ];
      const selected = selectRecentUserMessages(messages, 10);
      expect(selected.length).toBeLessThan(3);
      if (selected.length > 0) {
        expect(selected.at(-1)!.content[0]).toMatchObject({ text: 'newest' });
      }
    });
  });

  describe('selectCompactionUserMessages', () => {
    it('should return all messages with no elision when total fits within budget', () => {
      const messages = [
        userMessage('msg one'),
        userMessage('msg two'),
      ];
      const selection = selectCompactionUserMessages(messages, 100_000, 2_000);
      expect(selection.elided).toBe(false);
      expect(selection.head).toHaveLength(0);
      expect(selection.tail).toHaveLength(2);
    });

    it('should split into head and tail with elision when total exceeds budget', () => {
      const messages = Array.from({ length: 50 }, (_, i) =>
        userMessage(`user message ${i} with enough text to consume tokens`),
      );
      const selection = selectCompactionUserMessages(messages, 500, 100);
      expect(selection.elided).toBe(true);
      expect(selection.head.length).toBeGreaterThan(0);
      expect(selection.tail.length).toBeGreaterThan(0);
      expect(selection.omittedTokens).toBeGreaterThan(0);
    });
  });

  describe('pruning behavior during compaction', () => {
    it('should prune old messages: compaction replaces full history with summary + kept user messages', () => {
      const history: ContextMessage[] = [
        userMessage('very old user 1', { kind: 'user' }),
        assistantMessage('very old assistant 1'),
        userMessage('very old user 2', { kind: 'user' }),
        assistantMessage('very old assistant 2'),
        userMessage('recent user 1', { kind: 'user' }),
        assistantMessage('recent assistant 1'),
        userMessage('recent user 2', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Summarized old conversation.',
        compactedCount: history.length,
        tokensBefore: 500,
      });

      // After compaction, the result should be much shorter than the original
      expect(shape.messages.length).toBeLessThan(history.length);
      // The last message is always the compaction summary
      expect(shape.messages.at(-1)!.origin).toEqual({ kind: 'compaction_summary' });
      // No assistant messages remain (only user messages + summary)
      const assistantMessages = shape.messages.filter((msg) => msg.role === 'assistant');
      expect(assistantMessages).toHaveLength(0);
    });

    it('should produce a handoff summary that preserves essential conversation context', () => {
      const history: ContextMessage[] = [
        userMessage('user question about auth', { kind: 'user' }),
        assistantMessage('auth explanation'),
        userMessage('user follow-up about tokens', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'User asked about authentication and token management.',
        compactedCount: history.length,
        tokensBefore: 200,
      });

      // The context summary should contain the summary text
      expect(shape.contextSummary).toContain('authentication and token management');
      // The shape should have a valid token count after compaction
      expect(shape.tokensAfter).toBeGreaterThan(0);
    });
  });
});
