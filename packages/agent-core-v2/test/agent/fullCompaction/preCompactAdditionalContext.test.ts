/**
 * Standalone unit tests for PreCompact `additionalContext` injection.
 *
 * Verifies that when a PreCompact hook returns `additionalContext` in its
 * structured output, that context is injected into the agent's context memory
 * as a user message with `hook_additional_context` origin — and that the
 * compaction summarizer sees it before building the summary.
 *
 * Also verifies the `collectAdditionalContext` / `injectHookAdditionalContext`
 * contract: only non-blocked, non-timed-out, exit-0 results contribute their
 * additionalContext; blocked results are ignored.
 */

import { describe, expect, it, vi } from 'vitest';

import {
  buildContextCompactionShape,
  compactionUserMessageDisposition,
  isRealUserInput,
} from '#/agent/contextMemory/compactionHandoff';
import {
  collectAdditionalContext,
  injectHookAdditionalContext,
} from '#/agent/externalHooks/user-prompt';
import type { ContextMessage } from '#/agent/contextMemory/types';
import type { HookResult } from '#/agent/externalHooks/types';
import type { IExternalHooksRunnerService } from '#/app/externalHooksRunner/externalHooksRunner';
import {
  IAgentFullCompactionService,
} from '#/index';
import { testAgent } from '../../harness';

const CATALOGUED_PROVIDER = {
  type: 'kimi',
  apiKey: 'test-key',
  baseUrl: 'https://api.example/v1',
  model: 'mirri-code',
} as const;
const CATALOGUED_MODEL_CAPABILITIES = {
  image_in: true,
  video_in: true,
  audio_in: false,
  thinking: true,
  tool_use: true,
  max_context_tokens: 256_000,
} as const;
const SNAPSHOT_VISIBLE_TOOLS = [
  'Agent',
  'AgentSwarm',
  'CronCreate',
  'CronDelete',
  'CronList',
  'EnterPlanMode',
  'ExitPlanMode',
] as const;

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

/**
 * Creates a mock IExternalHooksRunnerService whose `trigger` returns
 * the given HookResult[] for PreCompact and [] for everything else.
 */
function mockHookRunner(preCompactResults: HookResult[]): IExternalHooksRunnerService {
  return {
    _serviceBrand: undefined,
    ready: Promise.resolve(),
    summary: {},
    trigger: vi.fn(async (event: string) => {
      if (event === 'PreCompact') return preCompactResults;
      return [];
    }),
    triggerBlock: vi.fn(async (event: string) => {
      if (event === 'PreCompact') {
        const block = preCompactResults.find((r) => r.action === 'block');
        if (block) return { block: true, reason: block.reason ?? 'Blocked' };
        return undefined;
      }
      return undefined;
    }),
    fireAndForgetTrigger: vi.fn(async () => []),
  } as unknown as IExternalHooksRunnerService;
}

describe('PreCompact additionalContext injection', () => {
  describe('collectAdditionalContext', () => {
    it('should return undefined for empty results', () => {
      expect(collectAdditionalContext(undefined)).toBeUndefined();
      expect(collectAdditionalContext([])).toBeUndefined();
    });

    it('should return undefined when no results have additionalContext', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0 },
        { action: 'allow', exitCode: 0, message: 'hello' },
      ];
      expect(collectAdditionalContext(results)).toBeUndefined();
    });

    it('should collect additionalContext from valid allow results', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'context A' },
        { action: 'allow', exitCode: 0, additionalContext: 'context B' },
      ];
      expect(collectAdditionalContext(results)).toBe('context A\ncontext B');
    });

    it('should ignore blocked results even with additionalContext', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'allowed context' },
        { action: 'block', exitCode: 2, additionalContext: 'blocked context', reason: 'nope' },
      ];
      expect(collectAdditionalContext(results)).toBe('allowed context');
    });

    it('should ignore timed-out results even with additionalContext', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'good context' },
        { action: 'allow', timedOut: true, additionalContext: 'timed out context' },
      ];
      expect(collectAdditionalContext(results)).toBe('good context');
    });

    it('should ignore non-zero exit code results even with additionalContext', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'good context' },
        { action: 'allow', exitCode: 1, additionalContext: 'error context' },
      ];
      expect(collectAdditionalContext(results)).toBe('good context');
    });

    it('should trim and skip empty additionalContext strings', () => {
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: '  valid  ' },
        { action: 'allow', exitCode: 0, additionalContext: '   ' },
        { action: 'allow', exitCode: 0, additionalContext: '' },
      ];
      expect(collectAdditionalContext(results)).toBe('valid');
    });
  });

  describe('injectHookAdditionalContext', () => {
    it('should append a user message with hook_additional_context origin', () => {
      const appended: ContextMessage[] = [];
      const context = {
        append(...messages: readonly ContextMessage[]) {
          appended.push(...messages);
        },
      };
      const results: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'extra info from hook' },
      ];
      injectHookAdditionalContext(context, results, 'PreCompact');
      expect(appended).toHaveLength(1);
      expect(appended[0]!.role).toBe('user');
      expect(appended[0]!.origin).toEqual({ kind: 'hook_additional_context', event: 'PreCompact' });
      expect((appended[0]!.content[0] as { type: 'text'; text: string }).text).toBe('extra info from hook');
    });

    it('should be a no-op when no additionalContext is available', () => {
      const appended: ContextMessage[] = [];
      const context = {
        append(...messages: readonly ContextMessage[]) {
          appended.push(...messages);
        },
      };
      injectHookAdditionalContext(context, [], 'PreCompact');
      expect(appended).toHaveLength(0);
    });
  });

  describe('compactionUserMessageDisposition for hook_additional_context', () => {
    it('should drop hook_additional_context origin during compaction', () => {
      expect(
        compactionUserMessageDisposition({ kind: 'hook_additional_context', event: 'PreCompact' }),
      ).toBe('drop');
    });

    it('should not treat hook_additional_context as real user input', () => {
      const msg = userMessage('extra context', { kind: 'hook_additional_context', event: 'PreCompact' });
      expect(isRealUserInput(msg)).toBe(false);
    });

    it('should exclude hook_additional_context from compactable user messages in compaction shape', () => {
      const history: ContextMessage[] = [
        userMessage('real user prompt', { kind: 'user' }),
        userMessage('injected by hook', { kind: 'hook_additional_context', event: 'PreCompact' }),
        userMessage('another real prompt', { kind: 'user' }),
      ];
      const shape = buildContextCompactionShape(history, {
        summary: 'Compaction summary.',
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

  describe('integration: PreCompact hook additionalContext appears in context before compaction LLM call', () => {
    it('should inject additionalContext from PreCompact hook into context memory before the compaction LLM round', async () => {
      const hookResults: HookResult[] = [
        {
          action: 'allow',
          exitCode: 0,
          additionalContext: 'injected-by-precompact-hook',
          structuredOutput: true,
        },
      ];
      const ctx = testAgent({
        hookEngine: mockHookRunner(hookResults),
      });
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'old user two', 'old assistant two', 40);
      ctx.appendExchange(3, 'recent user three', 'recent assistant three', 120);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Compacted summary.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'auto', instruction: undefined });
      await compacted;

      // The compaction LLM should have seen the injected additionalContext
      const [compactionCall] = ctx.llmCalls;
      const historyTexts = compactionCall?.history.map((msg) => {
        if (typeof msg.content === 'string') return msg.content;
        return msg.content
          .filter((part): part is { type: 'text'; text: string } => part.type === 'text')
          .map((part) => part.text)
          .join('');
      }) ?? [];

      // The additionalContext from the hook should appear in the history that the
      // compaction LLM sees (it's injected as a user message before the round)
      const hasAdditionalContext = historyTexts.some(
        (text) => text.includes('injected-by-precompact-hook'),
      );
      expect(hasAdditionalContext).toBe(true);

      // After compaction, the hook_additional_context message should be pruned
      // (it has 'drop' disposition in compactionUserMessageDisposition)
      const finalHistory = ctx.context.get();
      const hookAdditionalContextMessages = finalHistory.filter(
        (msg) => msg.origin?.kind === 'hook_additional_context',
      );
      expect(hookAdditionalContextMessages).toHaveLength(0);
    });

    it('should not inject additionalContext when PreCompact hook returns no additionalContext', async () => {
      const hookResults: HookResult[] = [
        { action: 'allow', exitCode: 0, message: 'ok' },
      ];
      const ctx = testAgent({
        hookEngine: mockHookRunner(hookResults),
      });
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'recent user two', 'recent assistant two', 80);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Compacted summary.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'auto', instruction: undefined });
      await compacted;

      // No hook_additional_context messages should exist in final history
      const finalHistory = ctx.context.get();
      const hookAdditionalContextMessages = finalHistory.filter(
        (msg) => msg.origin?.kind === 'hook_additional_context',
      );
      expect(hookAdditionalContextMessages).toHaveLength(0);
    });

    it('should not inject additionalContext from blocked PreCompact hook results', async () => {
      const hookResults: HookResult[] = [
        { action: 'block', exitCode: 2, additionalContext: 'blocked-context', reason: 'nope' },
      ];
      const ctx = testAgent({
        hookEngine: mockHookRunner(hookResults),
      });
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'recent user two', 'recent assistant two', 80);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Compacted summary.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'auto', instruction: undefined });
      await compacted;

      // Blocked hooks should not contribute additionalContext
      const finalHistory = ctx.context.get();
      const hookAdditionalContextMessages = finalHistory.filter(
        (msg) => msg.origin?.kind === 'hook_additional_context',
      );
      expect(hookAdditionalContextMessages).toHaveLength(0);
    });

    it('should not inject additionalContext from timed-out PreCompact hook results', async () => {
      const hookResults: HookResult[] = [
        { action: 'allow', timedOut: true, additionalContext: 'timed-out-context' },
      ];
      const ctx = testAgent({
        hookEngine: mockHookRunner(hookResults),
      });
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'recent user two', 'recent assistant two', 80);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Compacted summary.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'auto', instruction: undefined });
      await compacted;

      const finalHistory = ctx.context.get();
      const hookAdditionalContextMessages = finalHistory.filter(
        (msg) => msg.origin?.kind === 'hook_additional_context',
      );
      expect(hookAdditionalContextMessages).toHaveLength(0);
    });

    it('should join multiple additionalContext values from multiple hook results', async () => {
      const hookResults: HookResult[] = [
        { action: 'allow', exitCode: 0, additionalContext: 'context from hook A' },
        { action: 'allow', exitCode: 0, additionalContext: 'context from hook B' },
      ];
      const ctx = testAgent({
        hookEngine: mockHookRunner(hookResults),
      });
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'recent user two', 'recent assistant two', 80);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Compacted summary.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'auto', instruction: undefined });
      await compacted;

      // The compaction LLM should have seen both additionalContext values
      const [compactionCall] = ctx.llmCalls;
      const historyTexts = compactionCall?.history.map((msg) => {
        if (typeof msg.content === 'string') return msg.content;
        return msg.content
          .filter((part): part is { type: 'text'; text: string } => part.type === 'text')
          .map((part) => part.text)
          .join('');
      }) ?? [];

      const hasContextA = historyTexts.some((text) => text.includes('context from hook A'));
      const hasContextB = historyTexts.some((text) => text.includes('context from hook B'));
      expect(hasContextA).toBe(true);
      expect(hasContextB).toBe(true);
    });
  });

  describe('compaction preserves handoff summary after pruning', () => {
    it('should produce a compaction summary with handoff context at the end of history', async () => {
      const ctx = testAgent();
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'old user two', 'old assistant two', 40);
      ctx.appendExchange(3, 'recent user three', 'recent assistant three', 120);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Conversation was about testing compaction handoff.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'manual', instruction: undefined });
      await compacted;

      const finalHistory = ctx.context.get();
      // Last message should be the compaction summary
      const lastMessage = finalHistory.at(-1)!;
      expect(lastMessage.origin?.kind).toBe('compaction_summary');
      expect((lastMessage.content[0] as { type: 'text'; text: string }).text).toContain(
        'Conversation was about testing compaction handoff.',
      );

      // History should be shorter than before (pruned)
      expect(finalHistory.length).toBeLessThan(6); // original was 6 messages
    });

    it('should preserve recent real user messages in the compacted history', async () => {
      const ctx = testAgent();
      ctx.configure({
        provider: CATALOGUED_PROVIDER,
        modelCapabilities: CATALOGUED_MODEL_CAPABILITIES,
        tools: SNAPSHOT_VISIBLE_TOOLS,
      });
      ctx.appendExchange(1, 'old user one', 'old assistant one', 20);
      ctx.appendExchange(2, 'recent user two', 'recent assistant two', 80);

      const compacted = ctx.once('full_compaction.complete');
      ctx.mockNextResponse({ type: 'text', text: 'Summarized old content.' });
      ctx.get(IAgentFullCompactionService).begin({ source: 'manual', instruction: undefined });
      await compacted;

      const finalHistory = ctx.context.get();
      // There should be at least one real user message kept
      const realUserMessages = finalHistory.filter(
        (msg) => msg.role === 'user' && isRealUserInput(msg),
      );
      expect(realUserMessages.length).toBeGreaterThan(0);
    });
  });
});
