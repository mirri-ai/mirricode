import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { ContextMessage } from '#/agent/contextMemory/types';
import { IAgentContextMemoryService, IAgentProfileService, IAgentToolRegistryService } from '#/index';
import type { ExecutableTool } from '#/tool/toolContract';

import { createTestAgent, type TestAgentContext } from '../../harness';

/**
 * G1 gap: when a provider stream breaks mid-tool-call (finishReason paused /
 * other / truncated) but the response carries tool-call declarations, v2 must
 * record each as `tool.call` + `tool.result` (synthetic error) instead of
 * silently dropping them or (worse) executing truncated calls.
 */
describe('unexecuted tool-call recording on stream break', () => {
  let ctx: TestAgentContext;
  let context: IAgentContextMemoryService;

  beforeEach(() => {
    ctx = createTestAgent();
    context = ctx.get(IAgentContextMemoryService);
  });

  afterEach(async () => {
    await ctx.dispose();
  });

  const UNEXECUTED_OUTPUT =
    'This tool call was not executed: the model response ended before tool execution could start ' +
    '(the provider stream was interrupted). Do not assume the tool ran — ' +
    're-issue the call if it is still needed.';

  function findToolResultMessages(messages: readonly ContextMessage[]) {
    return messages.filter((m) => m.role === 'tool');
  }

  function textOf(content: readonly unknown[]): string {
    const part = content[0] as { text?: string } | undefined;
    return part?.text ?? '';
  }

  it('should record tool.call + tool.result with synthetic error when stream breaks with paused finish and tool calls', async () => {
    ctx.mockNextProviderResponse({
      parts: [
        { type: 'text', text: 'I will call two tools.' },
        { type: 'function', id: 'call_1', name: 'Read', arguments: '{"path":"/a.txt"}' },
        { type: 'function', id: 'call_2', name: 'Bash', arguments: '{"command":"ls"}' },
      ],
      finishReason: 'paused',
    });

    await ctx.rpc.prompt({ input: [{ type: 'text', text: 'Go' }] });
    await ctx.untilTurnEnd();

    const messages = context.get();
    const toolResults = findToolResultMessages(messages);

    // Each unexecuted call must have a corresponding tool.result with the synthetic error
    expect(toolResults).toHaveLength(2);
    expect(toolResults[0]!.toolCallId).toBe('call_1');
    expect(toolResults[0]!.isError).toBe(true);
    expect(textOf(toolResults[0]!.content)).toBe(UNEXECUTED_OUTPUT);

    expect(toolResults[1]!.toolCallId).toBe('call_2');
    expect(toolResults[1]!.isError).toBe(true);
    expect(textOf(toolResults[1]!.content)).toBe(UNEXECUTED_OUTPUT);
  });

  it('should sanitize unparseable arguments to empty object when stream breaks mid-arguments', async () => {
    ctx.mockNextProviderResponse({
      parts: [
        { type: 'function', id: 'call_trunc', name: 'Write', arguments: '{"path":"/out' },
      ],
      finishReason: 'other',
    });

    await ctx.rpc.prompt({ input: [{ type: 'text', text: 'Go' }] });
    await ctx.untilTurnEnd();

    const messages = context.get();
    const assistantWithCalls = messages.filter(
      (m) => m.role === 'assistant' && m.toolCalls.length > 0,
    );
    const toolResults = findToolResultMessages(messages);

    expect(assistantWithCalls).toHaveLength(1);
    expect(assistantWithCalls[0]!.toolCalls).toHaveLength(1);
    // Arguments should be sanitized (parsed as {} since the JSON was truncated)
    const args = JSON.parse(assistantWithCalls[0]!.toolCalls[0]!.arguments ?? 'null');
    expect(args).toEqual({});

    expect(toolResults).toHaveLength(1);
    expect(toolResults[0]!.toolCallId).toBe('call_trunc');
    expect(toolResults[0]!.isError).toBe(true);
    expect(textOf(toolResults[0]!.content)).toBe(UNEXECUTED_OUTPUT);
  });

  it('should record unexecuted calls when finishReason is truncated (max_tokens equivalent)', async () => {
    ctx.mockNextProviderResponse({
      parts: [
        { type: 'function', id: 'call_max', name: 'Bash', arguments: '{"command":"rm -rf /"}' },
      ],
      finishReason: 'truncated',
    });

    await ctx.rpc.prompt({ input: [{ type: 'text', text: 'Go' }] });
    await ctx.untilTurnEnd();

    const messages = context.get();
    const toolResults = findToolResultMessages(messages);

    expect(toolResults).toHaveLength(1);
    expect(toolResults[0]!.toolCallId).toBe('call_max');
    expect(toolResults[0]!.isError).toBe(true);
    expect(textOf(toolResults[0]!.content)).toBe(UNEXECUTED_OUTPUT);
  });

  it('should NOT record unexecuted calls when finishReason is tool_calls (normal tool step)', async () => {
    // Register a tool with resolveExecution so the executor can run it
    // without needing an approval gate (auto-approve).
    const lookupTool: ExecutableTool = {
      name: 'Lookup',
      description: 'Look up a short test value.',
      parameters: {
        type: 'object',
        properties: { query: { type: 'string' } },
        required: ['query'],
        additionalProperties: false,
      },
      resolveExecution: () => ({
        approvalRule: 'Lookup',
        execute: async () => ({ output: 'lookup-result' }),
      }),
    };
    const profile = ctx.get(IAgentProfileService);
    const toolRegistry = ctx.get(IAgentToolRegistryService);
    toolRegistry.register(lookupTool);
    profile.update({ activeToolNames: ['Lookup'] });

    const lookupCall = {
      type: 'function' as const,
      id: 'call_lookup',
      name: 'Lookup',
      arguments: '{"query":"moon"}',
    };

    ctx.mockNextResponse({ type: 'text', text: 'I will look it up.' }, lookupCall);
    ctx.mockNextResponse({ type: 'text', text: 'The result is lookup-result.' });

    await ctx.rpc.prompt({ input: [{ type: 'text', text: 'Look up moon' }] });
    // Approve the tool call
    await ctx.untilApproval(true);
    await ctx.untilTurnEnd();

    const messages = context.get();
    const toolResults = findToolResultMessages(messages);

    // Tool was actually executed (not recorded as unexecuted)
    expect(toolResults.length).toBeGreaterThanOrEqual(1);
    expect(toolResults[0]!.toolCallId).toBe('call_lookup');
    // Real tool execution produces the actual result, not the unexecuted message
    expect(textOf(toolResults[0]!.content)).toBe('lookup-result');
    expect(toolResults[0]!.isError).toBeFalsy();
  });

  it('should persist the unexecuted-call feedback in context memory for future turns', async () => {
    ctx.mockNextProviderResponse({
      parts: [
        { type: 'function', id: 'call_retry', name: 'Read', arguments: '{"path":"/x.txt"}' },
      ],
      finishReason: 'paused',
    });

    await ctx.rpc.prompt({ input: [{ type: 'text', text: 'Go' }] });
    await ctx.untilTurnEnd();

    // The unexecuted tool results should be in context memory so future turns
    // (or subsequent steps in the same turn) can see them
    const messages = context.get();
    const toolResults = findToolResultMessages(messages);

    expect(toolResults.length).toBeGreaterThanOrEqual(1);
    // The content must contain the re-issue guidance
    expect(textOf(toolResults[0]!.content)).toContain('not executed');
    expect(textOf(toolResults[0]!.content)).toContain('re-issue');
  });
});
