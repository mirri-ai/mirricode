/**
 * Standalone unit tests for the compaction core flow:
 *   1. Compaction preserves handoff summary
 *   2. Compaction prunes old messages
 *   3. PreCompact hook `additionalContext` is injected into context
 *      before compaction and pruned after
 *
 * These tests do NOT use the F4 harness — they operate on the
 * pure-function layer (`buildContextCompactionShape`,
 * `buildCompactionSummaryText`, `injectHookAdditionalContext`,
 * `isHookAdditionalContext`) and simulate the compaction pipeline
 * by composing those functions the same way the production code does.
 */

import { describe, expect, it } from 'vitest';

import {
  buildCompactionSummaryText,
  buildContextCompactionShape,
  compactionUserMessageDisposition,
  createCompactionSummaryMessage,
  isRealUserInput,
} from '#/agent/contextMemory/compactionHandoff';
import type { ContextMessage } from '#/agent/contextMemory/types';
import {
  collectAdditionalContext,
  injectHookAdditionalContext,
} from '#/agent/externalHooks/user-prompt';
import type { HookResult } from '#/agent/externalHooks/types';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function userMessage(text: string, origin?: ContextMessage['origin']): ContextMessage {
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

function toolResultMessage(text: string, toolCallId: string): ContextMessage {
  return {
    role: 'tool',
    content: [{ type: 'text', text }],
    toolCalls: [],
    toolCallId,
  };
}

/** Minimal mock of IAgentContextMemoryService — only `append` and `get`. */
function createMockContext(initial?: ContextMessage[]) {
  const history: ContextMessage[] = initial ? [...initial] : [];
  return {
    get: () => history as readonly ContextMessage[],
    append(...messages: readonly ContextMessage[]) {
      history.push(...messages);
    },
    history,
  };
}

/** Simulates the full-compaction pipeline at the function level:
 *  1. Capture original history
 *  2. Run PreCompact hook (inject additionalContext)
 *  3. Collect hook-injected messages
 *  4. Build compaction shape (prune + summary)
 *  5. Return the final shape
 */
function simulateCompactionPipeline(
  initialHistory: readonly ContextMessage[],
  hookResults: readonly HookResult[] | undefined,
) {
  // Step 1 — capture original history (mirrors fullCompactionService line 598)
  const originalHistory = [...initialHistory];
  const tokensBefore = roughTokenCount(initialHistory);

  // Step 2 — inject hook additionalContext (mirrors runPreCompact)
  const ctx = createMockContext([...initialHistory]);
  injectHookAdditionalContext(ctx, hookResults, 'PreCompact');

  // Step 3 — collect hook-injected messages (mirrors fullCompactionService lines 611-614)
  const currentHistory = ctx.get();
  const hookInjectedMessages = currentHistory.slice(originalHistory.length).filter(
    isHookAdditionalContext,
  );

  // Step 4 — build history that the compaction LLM sees
  // (mirrors fullCompactionService line 633-636)
  const historyForModel: readonly ContextMessage[] = [
    ...stripDynamicToolContextStub(originalHistory),
    ...hookInjectedMessages,
  ];

  // Step 5 — compaction LLM would produce a summary; simulate it
  const summary = 'Simulated compaction summary of the conversation.';

  // Step 6 — apply compaction shape
  const shape = buildContextCompactionShape(historyForModel, {
    summary,
    compactedCount: historyForModel.length,
    tokensBefore,
  });

  return {
    historyForModel,
    hookInjectedMessages,
    shape,
    summary,
    contextAfterCompaction: shape.messages,
  };
}

/** Stub for stripDynamicToolContext — passes through unchanged for unit tests. */
function stripDynamicToolContextStub<T>(messages: readonly T[]): T[] {
  return [...messages];
}

/** Very rough token count for test purposes (char / 4). */
function roughTokenCount(messages: readonly ContextMessage[]): number {
  let total = 0;
  for (const msg of messages) {
    for (const part of msg.content) {
      if (part.type === 'text') total += Math.ceil(part.text.length / 4);
    }
  }
  return total;
}

// ---------------------------------------------------------------------------
// 1. Compaction preserves handoff summary
// ---------------------------------------------------------------------------

describe('Compaction preserves handoff summary', () => {
  it('should produce a compaction summary with the handoff prefix at the end of context', () => {
    const summary = 'The user discussed testing strategies for the new module.';
    const handoff = buildCompactionSummaryText(summary);

    expect(handoff).toContain('The conversation so far has been compacted');
    expect(handoff).toContain(summary);
  });

  it('should include the compaction summary as the last message in the compacted shape', () => {
    const history: ContextMessage[] = [
      userMessage('first question', { kind: 'user' }),
      assistantMessage('first answer'),
      userMessage('second question', { kind: 'user' }),
      assistantMessage('second answer'),
    ];

    const shape = buildContextCompactionShape(history, {
      summary: 'User asked two questions about testing.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    const lastMessage = shape.messages.at(-1)!;
    expect(lastMessage.origin?.kind).toBe('compaction_summary');
    const text = (lastMessage.content[0] as { type: 'text'; text: string }).text;
    expect(text).toContain('User asked two questions about testing.');
  });

  it('should preserve the contextSummary field in the compaction result', () => {
    const history: ContextMessage[] = [
      userMessage('question', { kind: 'user' }),
      assistantMessage('answer'),
    ];

    const customContextSummary = 'Custom context summary override';
    const shape = buildContextCompactionShape(history, {
      summary: 'Raw summary text',
      contextSummary: customContextSummary,
      compactedCount: history.length,
      tokensBefore: 50,
    });

    expect(shape.contextSummary).toBe(customContextSummary);
    // The summary message at the end should also use contextSummary
    const lastMessage = shape.messages.at(-1)!;
    const text = (lastMessage.content[0] as { type: 'text'; text: string }).text;
    expect(text).toContain(customContextSummary);
  });

  it('should default contextSummary to the raw summary when no explicit contextSummary is provided', () => {
    const summary = 'Summary without explicit contextSummary.';
    const shape = buildContextCompactionShape([], {
      summary,
      compactedCount: 0,
      tokensBefore: 0,
    });

    // buildContextCompactionShape defaults contextSummary to input.summary
    // when contextSummary is not explicitly provided.
    // The caller (fullCompactionService) passes contextSummary: buildCompactionSummaryText(summary)
    // to get the handoff prefix.
    expect(shape.contextSummary).toBe(summary);
  });

  it('should use the handoff prefix when the caller passes buildCompactionSummaryText as contextSummary', () => {
    const summary = 'Summary with handoff prefix.';
    const contextSummary = buildCompactionSummaryText(summary);
    const shape = buildContextCompactionShape([], {
      summary,
      contextSummary,
      compactedCount: 0,
      tokensBefore: 0,
    });

    expect(shape.contextSummary).toContain('The conversation so far has been compacted');
    expect(shape.contextSummary).toContain(summary);
  });
});

// ---------------------------------------------------------------------------
// 2. Compaction prunes old messages
// ---------------------------------------------------------------------------

describe('Compaction prunes old messages', () => {
  it('should produce fewer messages than the original history', () => {
    const history: ContextMessage[] = [
      userMessage('old question 1', { kind: 'user' }),
      assistantMessage('old answer 1'),
      userMessage('old question 2', { kind: 'user' }),
      assistantMessage('old answer 2'),
      userMessage('recent question', { kind: 'user' }),
      assistantMessage('recent answer'),
    ];

    const shape = buildContextCompactionShape(history, {
      summary: 'Summarized old questions.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    // The compacted shape should have fewer messages than the original 6
    // (at minimum: a summary message, plus any kept user messages)
    expect(shape.messages.length).toBeLessThan(history.length);
    // Summary message should always be present
    expect(shape.messages.some((m) => m.origin?.kind === 'compaction_summary')).toBe(true);
  });

  it('should exclude assistant messages from the compacted shape (only user + summary remain)', () => {
    const history: ContextMessage[] = [
      userMessage('q1', { kind: 'user' }),
      assistantMessage('a1'),
      userMessage('q2', { kind: 'user' }),
      assistantMessage('a2'),
      userMessage('q3', { kind: 'user' }),
      assistantMessage('a3'),
    ];

    const shape = buildContextCompactionShape(history, {
      summary: 'Summarized.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    // After compaction, no assistant messages should remain
    // (only user messages + compaction_summary + possible injection/elision messages)
    const assistantMessages = shape.messages.filter((m) => m.role === 'assistant');
    expect(assistantMessages).toHaveLength(0);
  });

  it('should drop tool-result messages during compaction', () => {
    const history: ContextMessage[] = [
      userMessage('run tool', { kind: 'user' }),
      assistantMessage(''),
      toolResultMessage('output', 'call_1'),
      userMessage('follow-up', { kind: 'user' }),
      assistantMessage('done'),
    ];

    const shape = buildContextCompactionShape(history, {
      summary: 'Summarized tool run.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    const toolMessages = shape.messages.filter((m) => m.role === 'tool');
    expect(toolMessages).toHaveLength(0);
  });

  it('should keep at least one real user message in the compacted shape when history has user input', () => {
    const history: ContextMessage[] = [
      userMessage('important question', { kind: 'user' }),
      assistantMessage('answer'),
    ];

    const shape = buildContextCompactionShape(history, {
      summary: 'Summary.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    const realUserMessages = shape.messages.filter(
      (m) => m.role === 'user' && isRealUserInput(m),
    );
    expect(realUserMessages.length).toBeGreaterThan(0);
  });

  it('should include elision marker when user messages exceed the token budget', () => {
    // Create many large user messages to exceed the 20k token budget
    const largeText = 'x'.repeat(8_000);
    const history: ContextMessage[] = Array.from({ length: 10 }, (_, i) => {
      const msgs: ContextMessage[] = [
        userMessage(`question ${i}: ${largeText}`, { kind: 'user' }),
        assistantMessage(`answer ${i}`),
      ];
      return msgs;
    }).flat();

    const shape = buildContextCompactionShape(history, {
      summary: 'Summarized long conversation.',
      compactedCount: history.length,
      tokensBefore: roughTokenCount(history),
    });

    // Should have an elision marker message
    const elisionMessages = shape.messages.filter(
      (m) => m.origin?.kind === 'injection' && m.origin.variant === 'compaction_elision',
    );
    expect(elisionMessages.length).toBe(1);
  });
});

// ---------------------------------------------------------------------------
// 3. PreCompact hook additionalContext injection
// ---------------------------------------------------------------------------

describe('PreCompact hook additionalContext injection', () => {
  it('should inject additionalContext from PreCompact hook results as a hook_additional_context message', () => {
    const ctx = createMockContext();
    const results: HookResult[] = [
      { action: 'allow', exitCode: 0, additionalContext: 'extra context from hook' },
    ];

    injectHookAdditionalContext(ctx, results, 'PreCompact');

    expect(ctx.history).toHaveLength(1);
    expect(ctx.history[0]!.role).toBe('user');
    expect(ctx.history[0]!.origin).toEqual({
      kind: 'hook_additional_context',
      event: 'PreCompact',
    });
    expect((ctx.history[0]!.content[0] as { type: 'text'; text: string }).text).toBe(
      'extra context from hook',
    );
  });

  it('should be a no-op when hook results have no additionalContext', () => {
    const ctx = createMockContext();
    const results: HookResult[] = [
      { action: 'allow', exitCode: 0, message: 'ok' },
    ];

    injectHookAdditionalContext(ctx, results, 'PreCompact');
    expect(ctx.history).toHaveLength(0);
  });

  it('should be a no-op when hook results are undefined or empty', () => {
    const ctx1 = createMockContext();
    injectHookAdditionalContext(ctx1, undefined, 'PreCompact');
    expect(ctx1.history).toHaveLength(0);

    const ctx2 = createMockContext();
    injectHookAdditionalContext(ctx2, [], 'PreCompact');
    expect(ctx2.history).toHaveLength(0);
  });

  it('should ignore additionalContext from blocked hook results', () => {
    const results: HookResult[] = [
      { action: 'block', exitCode: 2, additionalContext: 'blocked context', reason: 'denied' },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should ignore additionalContext from timed-out hook results', () => {
    const results: HookResult[] = [
      { action: 'allow', timedOut: true, additionalContext: 'timed out context' },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should ignore additionalContext from non-zero exit-code hook results', () => {
    const results: HookResult[] = [
      { action: 'allow', exitCode: 1, additionalContext: 'error context' },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should join multiple additionalContext values from multiple valid hook results', () => {
    const results: HookResult[] = [
      { action: 'allow', exitCode: 0, additionalContext: 'context A' },
      { action: 'allow', exitCode: 0, additionalContext: 'context B' },
    ];
    expect(collectAdditionalContext(results)).toBe('context A\ncontext B');
  });

  it('should make hook_additional_context messages visible in the context that gets compacted', () => {
    const initialHistory: ContextMessage[] = [
      userMessage('original question', { kind: 'user' }),
      assistantMessage('original answer'),
      userMessage('recent question', { kind: 'user' }),
      assistantMessage('recent answer'),
    ];

    const hookResults: HookResult[] = [
      { action: 'allow', exitCode: 0, additionalContext: 'injected-by-precompact-hook' },
    ];

    const { historyForModel, hookInjectedMessages } = simulateCompactionPipeline(
      initialHistory,
      hookResults,
    );

    // The hook-injected message should be captured
    expect(hookInjectedMessages).toHaveLength(1);
    expect(hookInjectedMessages[0]!.origin?.kind).toBe('hook_additional_context');

    // The history sent to the compaction LLM should include the injected context
    const hasInjected = historyForModel.some(
      (m) =>
        m.origin?.kind === 'hook_additional_context' &&
        (m.content[0] as { type: 'text'; text: string }).text.includes('injected-by-precompact-hook'),
    );
    expect(hasInjected).toBe(true);
  });

  it('should prune hook_additional_context messages from the compacted result', () => {
    const initialHistory: ContextMessage[] = [
      userMessage('question', { kind: 'user' }),
      assistantMessage('answer'),
      userMessage('recent question', { kind: 'user' }),
      assistantMessage('recent answer'),
    ];

    const hookResults: HookResult[] = [
      { action: 'allow', exitCode: 0, additionalContext: 'extra hook info' },
    ];

    const { contextAfterCompaction } = simulateCompactionPipeline(
      initialHistory,
      hookResults,
    );

    // After compaction, no hook_additional_context messages should remain
    // (they have 'drop' disposition in compactionUserMessageDisposition)
    const hookMessages = contextAfterCompaction.filter(
      (m) => m.origin?.kind === 'hook_additional_context',
    );
    expect(hookMessages).toHaveLength(0);
  });

  it('should not inject additionalContext when no hook results are produced', () => {
    const initialHistory: ContextMessage[] = [
      userMessage('question', { kind: 'user' }),
      assistantMessage('answer'),
    ];

    const { hookInjectedMessages, historyForModel } = simulateCompactionPipeline(
      initialHistory,
      undefined,
    );

    expect(hookInjectedMessages).toHaveLength(0);
    const hookMessages = historyForModel.filter(
      (m) => m.origin?.kind === 'hook_additional_context',
    );
    expect(hookMessages).toHaveLength(0);
  });

  it('should give hook_additional_context messages a "drop" disposition during compaction', () => {
    expect(
      compactionUserMessageDisposition({ kind: 'hook_additional_context', event: 'PreCompact' }),
    ).toBe('drop');
  });

  it('should not treat hook_additional_context messages as real user input', () => {
    const msg = userMessage('hook context', {
      kind: 'hook_additional_context',
      event: 'PreCompact',
    });
    expect(isRealUserInput(msg)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// End-to-end pipeline: additionalContext flows through compaction correctly
// ---------------------------------------------------------------------------

describe('Compaction pipeline: additionalContext integration', () => {
  it('should include additionalContext in the compaction LLM input but not in the final context', () => {
    const history: ContextMessage[] = [
      userMessage('first', { kind: 'user' }),
      assistantMessage('first-reply'),
      userMessage('second', { kind: 'user' }),
      assistantMessage('second-reply'),
      userMessage('third', { kind: 'user' }),
      assistantMessage('third-reply'),
    ];

    const hookResults: HookResult[] = [
      {
        action: 'allow',
        exitCode: 0,
        additionalContext: 'Remember: the user prefers concise answers',
        structuredOutput: true,
      },
    ];

    const { historyForModel, contextAfterCompaction } = simulateCompactionPipeline(
      history,
      hookResults,
    );

    // 1. additionalContext IS visible to the compaction LLM
    const llmSeesAdditionalContext = historyForModel.some(
      (m) =>
        m.origin?.kind === 'hook_additional_context' &&
        (m.content[0] as { type: 'text'; text: string }).text.includes(
          'the user prefers concise answers',
        ),
    );
    expect(llmSeesAdditionalContext).toBe(true);

    // 2. additionalContext is NOT in the final compacted context
    const finalHasAdditionalContext = contextAfterCompaction.some(
      (m) => m.origin?.kind === 'hook_additional_context',
    );
    expect(finalHasAdditionalContext).toBe(false);

    // 3. The handoff summary IS in the final context
    const lastMessage = contextAfterCompaction.at(-1)!;
    expect(lastMessage.origin?.kind).toBe('compaction_summary');
  });

  it('should not affect compaction when hook results have no additionalContext', () => {
    const history: ContextMessage[] = [
      userMessage('q', { kind: 'user' }),
      assistantMessage('a'),
      userMessage('q2', { kind: 'user' }),
      assistantMessage('a2'),
    ];

    const hookResults: HookResult[] = [
      { action: 'allow', exitCode: 0, message: 'ok' },
    ];

    const { historyForModel, contextAfterCompaction } = simulateCompactionPipeline(
      history,
      hookResults,
    );

    // No hook-injected messages in the compaction LLM input
    expect(
      historyForModel.every((m) => m.origin?.kind !== 'hook_additional_context'),
    ).toBe(true);

    // No hook-injected messages in the final context
    expect(
      contextAfterCompaction.every((m) => m.origin?.kind !== 'hook_additional_context'),
    ).toBe(true);

    // Summary is still present
    expect(
      contextAfterCompaction.some((m) => m.origin?.kind === 'compaction_summary'),
    ).toBe(true);
  });

  it('should handle multiple hook results with mixed additionalContext', () => {
    const history: ContextMessage[] = [
      userMessage('q1', { kind: 'user' }),
      assistantMessage('a1'),
      userMessage('q2', { kind: 'user' }),
      assistantMessage('a2'),
    ];

    const hookResults: HookResult[] = [
      { action: 'allow', exitCode: 0, additionalContext: 'context from hook A' },
      { action: 'allow', exitCode: 0, message: 'no additional context here' },
      { action: 'allow', exitCode: 0, additionalContext: 'context from hook C' },
      { action: 'block', exitCode: 2, additionalContext: 'blocked context', reason: 'denied' },
    ];

    const { hookInjectedMessages, historyForModel } = simulateCompactionPipeline(
      history,
      hookResults,
    );

    // Only one injected message (all valid additionalContext is merged into one)
    expect(hookInjectedMessages).toHaveLength(1);
    const injectedText = (hookInjectedMessages[0]!.content[0] as { type: 'text'; text: string }).text;
    expect(injectedText).toContain('context from hook A');
    expect(injectedText).toContain('context from hook C');
    expect(injectedText).not.toContain('blocked context');

    // The compaction LLM input should include the merged additionalContext
    const llmSeesContext = historyForModel.some(
      (m) =>
        m.origin?.kind === 'hook_additional_context' &&
        (m.content[0] as { type: 'text'; text: string }).text.includes('context from hook A'),
    );
    expect(llmSeesContext).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// historySafeToCompact parity (mirrors fullCompactionService.ts:831-840)
// ---------------------------------------------------------------------------

describe('historySafeToCompact with hook_additional_context', () => {
  it('should allow compaction when only hook_additional_context messages were appended after original', () => {
    // This mirrors the production check in fullCompactionService.ts:831-840
    const original: ContextMessage[] = [
      userMessage('q', { kind: 'user' }),
      assistantMessage('a'),
    ];
    const hookMessage: ContextMessage = userMessage('extra', {
      kind: 'hook_additional_context',
      event: 'PreCompact',
    });
    const current: ContextMessage[] = [...original, hookMessage];

    // current >= original length ✓
    expect(current.length).toBeGreaterThanOrEqual(original.length);
    // original prefix matches ✓
    expect(original.every((msg, i) => msg === current[i])).toBe(true);
    // appended messages are either real user input or hook_additional_context ✓
    const appended = current.slice(original.length);
    expect(appended.every((m) => isRealUserInput(m) || isHookAdditionalContext(m))).toBe(true);
  });

  it('should reject compaction when a non-hook, non-user message was appended after original', () => {
    const original: ContextMessage[] = [
      userMessage('q', { kind: 'user' }),
      assistantMessage('a'),
    ];
    const appendedAssistant: ContextMessage = assistantMessage('unexpected');
    const current: ContextMessage[] = [...original, appendedAssistant];

    // current >= original length ✓
    expect(current.length).toBeGreaterThanOrEqual(original.length);
    // But appended assistant message is neither real user input nor hook_additional_context
    const appended = current.slice(original.length);
    expect(appended.every((m) => isRealUserInput(m) || isHookAdditionalContext(m))).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// isHookAdditionalContext (mirrors fullCompactionService.ts:842-844)
// ---------------------------------------------------------------------------

function isHookAdditionalContext(message: ContextMessage): boolean {
  return message.role === 'user' && message.origin?.kind === 'hook_additional_context';
}

describe('isHookAdditionalContext', () => {
  it('should identify hook_additional_context messages', () => {
    const msg = userMessage('extra', { kind: 'hook_additional_context', event: 'PreCompact' });
    expect(isHookAdditionalContext(msg)).toBe(true);
  });

  it('should reject regular user messages', () => {
    const msg = userMessage('normal', { kind: 'user' });
    expect(isHookAdditionalContext(msg)).toBe(false);
  });

  it('should reject assistant messages', () => {
    const msg = assistantMessage('reply');
    expect(isHookAdditionalContext(msg)).toBe(false);
  });

  it('should reject compaction_summary messages', () => {
    const msg = createCompactionSummaryMessage('summary text');
    expect(isHookAdditionalContext(msg)).toBe(false);
  });
});
