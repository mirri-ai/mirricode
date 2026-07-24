/**
 * LoopHooks contract — every phase hook is observed and asserted by its
 * publicly visible effect (events, transcript writes, TurnResult), not
 * by inspecting the loop's call graph.
 */

import { inputTotal, type Message } from '@mirri-ai/kosong';
import { describe, expect, it, vi } from 'vitest';

import type { LoopHooks, ExecutableToolResult, ToolExecution, PreLlmRequestData, PostLlmRequestData } from '../../src/loop/index';
import { PathSecurityError } from '../../src/tools/policies/path-access';
import { makeEndTurnResponse, makeToolCall, makeToolUseResponse } from './fixtures/fake-llm';
import { runTurn, runTurnExpectingThrow } from './fixtures/helpers';
import { EchoTool, FailingTool, type EchoInput } from './fixtures/tools';

function expectTextOutput(output: unknown): string {
  expect(typeof output).toBe('string');
  return output as string;
}

describe('runTurn — beforeStep hook', () => {
  it('passes through when the hook returns undefined', async () => {
    let calls = 0;
    const hooks: LoopHooks = {
      beforeStep: async () => {
        calls += 1;
      },
    };
    const { result } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
    expect(calls).toBe(1);
  });

  it('throws and prevents the LLM call when the hook blocks', async () => {
    const hooks: LoopHooks = {
      beforeStep: async () => ({ block: true, reason: 'policy says no' }),
    };
    const { error, llm, context, sink } = await runTurnExpectingThrow({
      hooks,
      responses: [makeEndTurnResponse('never')],
    });

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe('policy says no');
    expect(llm.callCount).toBe(0);
    expect(context.stepBegins().length).toBe(0);
    // turn.interrupted{reason:'error'} fires before the throw
    const interrupted = sink.byType('turn.interrupted');
    expect(interrupted[0]?.reason).toBe('error');
  });

  it('rethrows non-abort hook exceptions as loop errors', async () => {
    const hooks: LoopHooks = {
      beforeStep: async () => {
        throw new Error('hook crashed');
      },
    };
    const { error, llm } = await runTurnExpectingThrow({
      hooks,
      responses: [makeEndTurnResponse('never')],
    });
    expect((error as Error).message).toBe('hook crashed');
    expect(llm.callCount).toBe(0);
  });

  it('receives turnId, stepNumber, signal, and LLM', async () => {
    const ctxs: Array<{
      turnId: string;
      stepNumber: number;
      signal: AbortSignal;
      modelName: string;
      chat: unknown;
    }> = [];
    const hooks: LoopHooks = {
      beforeStep: async (ctx) => {
        ctxs.push({
          turnId: ctx.turnId,
          stepNumber: ctx.stepNumber,
          signal: ctx.signal,
          modelName: ctx.llm.modelName,
          chat: ctx.llm.chat,
        });
      },
    };
    await runTurn({
      hooks,
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: '1' }, 'a')]),
        makeEndTurnResponse('done'),
      ],
      tools: [new EchoTool()],
      turnId: 'turn-foo',
    });
    expect(ctxs).toEqual([
      {
        turnId: 'turn-foo',
        stepNumber: 1,
        signal: expect.any(AbortSignal) as AbortSignal,
        modelName: 'fake-model',
        chat: expect.any(Function) as unknown,
      },
      {
        turnId: 'turn-foo',
        stepNumber: 2,
        signal: expect.any(AbortSignal) as AbortSignal,
        modelName: 'fake-model',
        chat: expect.any(Function) as unknown,
      },
    ]);
  });
});

describe('runTurn — afterStep hook', () => {
  it('runs after step.end and observes the step result', async () => {
    const captured: Array<{
      stopReason: string;
      stepNumber: number;
      usageInput: number;
      usageOutput: number;
    }> = [];
    const hooks: LoopHooks = {
      afterStep: async (ctx) => {
        captured.push({
          stopReason: ctx.stopReason,
          stepNumber: ctx.stepNumber,
          usageInput: inputTotal(ctx.usage),
          usageOutput: ctx.usage.output,
        });
      },
    };
    const { sink } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok', { inputOther: 1, output: 2 })],
    });
    expect(captured).toEqual([
      {
        stopReason: 'end_turn',
        stepNumber: 1,
        usageInput: 1,
        usageOutput: 2,
      },
    ]);
    // step.end has fired
    expect(sink.count('step.end')).toBe(1);
  });

  it('errors thrown by afterStep are swallowed (turn still completes)', async () => {
    const afterStep = vi.fn(async () => {
      throw new Error('observer crashed');
    });
    const { result } = await runTurn({
      hooks: { afterStep },
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
    expect(afterStep).toHaveBeenCalled();
  });
});

describe('runTurn — prepareToolExecution hook', () => {
  it('receives all tool calls from the same provider response', async () => {
    const observedBatches: string[][] = [];
    const hooks: LoopHooks = {
      prepareToolExecution: async (ctx) => {
        observedBatches.push(ctx.toolCalls?.map((toolCall) => toolCall.id) ?? []);
      },
    };

    await runTurn({
      hooks,
      tools: [new EchoTool()],
      responses: [
        makeToolUseResponse([
          makeToolCall('echo', { text: 'a' }, 'tc-a'),
          makeToolCall('echo', { text: 'b' }, 'tc-b'),
        ]),
        makeEndTurnResponse('done'),
      ],
    });

    expect(observedBatches).toEqual([
      ['tc-a', 'tc-b'],
      ['tc-a', 'tc-b'],
    ]);
  });

  it('block:true records an error result without invoking execute', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      prepareToolExecution: async () => ({ block: true, reason: 'forbidden' }),
    };
    const { sink, context } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    const result = sink.byType('tool.result')[0];
    expect(result?.result.isError).toBe(true);
    expect(result?.result.output).toContain('forbidden');
    expect(context.toolResults()[0]?.result.isError).toBe(true);
  });

  it('updatedArgs rewrites the args passed to execute', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      prepareToolExecution: async () => ({ updatedArgs: { text: 'rewritten' } }),
    };
    const { sink, context } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'original' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(1);
    expect(echo.calls[0]?.args).toEqual({ text: 'rewritten' });
    expect(sink.byType('tool.call')[0]?.args).toEqual({ text: 'rewritten' });
    expect(context.toolCalls()[0]?.args).toEqual({ text: 'rewritten' });
  });

  it('revalidates updatedArgs before execute', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      prepareToolExecution: async () => ({ updatedArgs: { text: 123 } }),
    };
    const { sink, context } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'original' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    expect(sink.byType('tool.result')[0]?.result.isError).toBe(true);
    expect(sink.byType('tool.result')[0]?.result.output).toContain(
      'after prepareToolExecution hook',
    );
    expect(context.toolResults()[0]?.result.isError).toBe(true);
  });

  it('hook throws non-abort -> records an error result, execute not called', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      prepareToolExecution: async () => {
        throw new Error('prepareToolExecution crashed');
      },
    };
    const { sink, result } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    const tr = sink.byType('tool.result')[0];
    expect(tr?.result.isError).toBe(true);
    expect(expectTextOutput(tr?.result.output).toLowerCase()).toContain('preparetoolexecution');
    // The turn still converges
    expect(result.stopReason).toBe('end_turn');
  });
});

describe('runTurn — rewriteToolInput hook', () => {
  it('rewrites tool args when the hook returns updatedArgs', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      rewriteToolInput: async () => ({ updatedArgs: { text: 'rewritten-by-hook' } }),
    };
    const { sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'original' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(1);
    expect(echo.calls[0]?.args).toEqual({ text: 'rewritten-by-hook' });
    expect(sink.byType('tool.call')[0]?.args).toEqual({ text: 'rewritten-by-hook' });
  });

  it('preserves original args when the hook returns undefined', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      rewriteToolInput: async () => undefined,
    };
    const { sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'keep-me' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(1);
    expect(echo.calls[0]?.args).toEqual({ text: 'keep-me' });
    expect(sink.byType('tool.call')[0]?.args).toEqual({ text: 'keep-me' });
  });

  it('fail-open: hook throw preserves original args', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      rewriteToolInput: async () => { throw new Error('boom'); },
    };
    const { sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'safe' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(1);
    expect(echo.calls[0]?.args).toEqual({ text: 'safe' });
  });

  it('revalidates rewritten args before execute', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      rewriteToolInput: async () => ({ updatedArgs: { text: 123 } }),
    };
    const { sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'original' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    expect(sink.byType('tool.result')[0]?.result.isError).toBe(true);
  });
});

describe('runTurn — authorizeToolExecution hook', () => {
  it('receives all tool calls from the same provider response', async () => {
    const observedBatches: string[][] = [];
    const hooks: LoopHooks = {
      authorizeToolExecution: async (ctx) => {
        observedBatches.push(ctx.toolCalls?.map((toolCall) => toolCall.id) ?? []);
      },
    };

    await runTurn({
      hooks,
      tools: [new EchoTool()],
      responses: [
        makeToolUseResponse([
          makeToolCall('echo', { text: 'a' }, 'tc-a'),
          makeToolCall('echo', { text: 'b' }, 'tc-b'),
        ]),
        makeEndTurnResponse('done'),
      ],
    });

    expect(observedBatches).toEqual([
      ['tc-a', 'tc-b'],
      ['tc-a', 'tc-b'],
    ]);
  });
});

class DescribedEchoTool extends EchoTool {
  override resolveExecution(args: EchoInput) {
    const execution = super.resolveExecution(args);
    return { ...execution, description: `Echoing ${args.text}` };
  }
}

class CrashingDescriptionEchoTool extends EchoTool {
  override resolveExecution(_args: EchoInput): ToolExecution {
    throw new Error('description crashed');
  }
}

class PathSecurityEchoTool extends EchoTool {
  override resolveExecution(_args: EchoInput): ToolExecution {
    throw new PathSecurityError(
      'PATH_OUTSIDE_WORKSPACE',
      '../secret.txt',
      '/secret.txt',
      'path policy denied',
    );
  }
}

class ErrorExecutionEchoTool extends EchoTool {
  override resolveExecution(_args: EchoInput): ToolExecution {
    return { isError: true, output: 'resolved error' };
  }
}

describe('runTurn — tool-call display fields', () => {
  it('writes resolveExecution description onto appendToolCall after prepared args are known', async () => {
    const echo = new DescribedEchoTool();
    const hooks: LoopHooks = {
      prepareToolExecution: async () => ({ updatedArgs: { text: 'rewritten' } }),
    };
    const { context } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    const tcRow = context.toolCalls()[0];
    expect(tcRow?.description).toBe('Echoing rewritten');
  });

  it('resolveExecution failures record a tool error without running execute', async () => {
    const echo = new CrashingDescriptionEchoTool();
    const { context } = await runTurn({
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    expect(context.toolResults()[0]?.result.isError).toBe(true);
    expect(context.toolResults()[0]?.result.output).toContain('failed to resolve execution');
  });

  it('resolveExecution path security failures use the policy message directly', async () => {
    const echo = new PathSecurityEchoTool();
    const { context } = await runTurn({
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    expect(context.toolResults()[0]?.result).toEqual({
      isError: true,
      output: 'path policy denied',
    });
  });

  it('resolveExecution error result records a tool error without running execute', async () => {
    const echo = new ErrorExecutionEchoTool();
    const { context } = await runTurn({
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'hi' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(0);
    expect(context.toolResults()[0]?.result).toEqual({
      isError: true,
      output: 'resolved error',
    });
  });
});

describe('runTurn — finalizeToolResult hook', () => {
  it('returned result replaces the tool result before persistence', async () => {
    const echo = new EchoTool();
    const override: ExecutableToolResult = { output: 'redacted' };
    const hooks: LoopHooks = {
      finalizeToolResult: async () => override,
    };
    const { context, sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'secret' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(echo.calls.length).toBe(1);
    expect(context.toolResults()[0]?.result.output).toBe('redacted');
    expect(sink.byType('tool.result')[0]?.result.output).toBe('redacted');
  });

  it('hook throw on success records an error result and never persists raw output', async () => {
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      finalizeToolResult: async () => {
        throw new Error('finalizeToolResult crashed');
      },
    };
    const { context, sink } = await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: 'secret' }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    const persisted = context.toolResults()[0]?.result;
    expect(persisted?.isError).toBe(true);
    // Output never leaks the original tool content
    expect(persisted?.output).not.toContain('secret');
    // Live event also marked as error
    expect(sink.byType('tool.result')[0]?.result.isError).toBe(true);
  });

  it('execute failure passes the error result through finalizeToolResult', async () => {
    const fail = new FailingTool('execute crashed');
    let finalizeToolResultCalls = 0;
    const hooks: LoopHooks = {
      finalizeToolResult: async (ctx) => {
        finalizeToolResultCalls += 1;
        return ctx.result;
      },
    };
    const { sink } = await runTurn({
      hooks,
      tools: [fail],
      responses: [
        makeToolUseResponse([makeToolCall('fail', {}, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(finalizeToolResultCalls).toBe(1);
    const tr = sink.byType('tool.result')[0];
    expect(tr?.result.isError).toBe(true);
  });

  it('preflight errors pass through finalizeToolResult before persistence', async () => {
    let finalizeToolResultCalls = 0;
    const hooks: LoopHooks = {
      finalizeToolResult: async (ctx) => {
        finalizeToolResultCalls += 1;
        const output = expectTextOutput(ctx.result.output);
        return {
          output: `finalized: ${output}`,
          isError: ctx.result.isError,
        };
      },
    };
    const { sink, context } = await runTurn({
      hooks,
      tools: [],
      responses: [
        makeToolUseResponse([makeToolCall('ghost', { x: 1 }, 'tc-1')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(finalizeToolResultCalls).toBe(1);
    expect(context.toolResults()[0]?.result.output).toContain('finalized:');
    expect(sink.byType('tool.result')[0]?.result.output).toContain('finalized:');
  });
});

describe('runTurn — shouldContinueAfterStop hook', () => {
  it('continue:false breaks the turn at end_turn (default behaviour)', async () => {
    const shouldContinueAfterStop = vi.fn(async () => ({ continue: false }));
    const { result } = await runTurn({
      hooks: { shouldContinueAfterStop },
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
    expect(shouldContinueAfterStop).toHaveBeenCalledTimes(1);
  });

  it('continue:true allows another step after a non-tool stopReason', async () => {
    const shouldContinueAfterStop = vi
      .fn()
      .mockResolvedValueOnce({ continue: true })
      .mockResolvedValueOnce({ continue: false });
    const { result, llm } = await runTurn({
      hooks: { shouldContinueAfterStop },
      responses: [
        makeEndTurnResponse('first', { inputOther: 1, output: 1 }),
        makeEndTurnResponse('second', { inputOther: 2, output: 2 }),
      ],
    });
    expect(llm.callCount).toBe(2);
    expect(result.steps).toBe(2);
    expect(result.stopReason).toBe('end_turn');
  });

  it('returning undefined breaks the turn (default)', async () => {
    let calls = 0;
    const hooks: LoopHooks = {
      shouldContinueAfterStop: async () => {
        calls += 1;
      },
    };
    const { result } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
    expect(calls).toBe(1);
  });

  it('is NOT consulted between tool_use steps', async () => {
    const shouldContinueAfterStop = vi.fn(async () => ({ continue: false }));
    const echo = new EchoTool();
    await runTurn({
      hooks: { shouldContinueAfterStop },
      tools: [echo],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: '1' }, 'a')]),
        makeToolUseResponse([makeToolCall('echo', { text: '2' }, 'b')]),
        makeEndTurnResponse('done'),
      ],
    });
    // Hook is only consulted at the final non-tool step
    expect(shouldContinueAfterStop).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// preLlmRequest / postLlmRequest — fire-and-forget LLM observers.
// ---------------------------------------------------------------------------

describe('runTurn — preLlmRequest hook', () => {
  it('should fire before the LLM chat call when messages and tools are assembled', async () => {
    const callOrder: string[] = [];
    const preData: PreLlmRequestData[] = [];
    const hooks: LoopHooks = {
      preLlmRequest: (data) => {
        callOrder.push('preLlm');
        preData.push(data);
      },
    };
    // Use a custom LLM name so we can assert it in the payload.
    const { llm } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
      systemPrompt: 'system',
    });
    callOrder.push('llmChatDone');
    expect(llm.callCount).toBe(1);

    // preLlmRequest must have been called before llm.chat returned.
    expect(callOrder[0]).toBe('preLlm');
    expect(preData).toHaveLength(1);
    expect(preData[0]?.model).toBe('fake-model');
    expect(preData[0]?.turnId).toBe('turn-1');
    expect(preData[0]?.step).toBe(1);
    expect(preData[0]?.messageCount).toBeGreaterThanOrEqual(0);
    expect(preData[0]?.toolCount).toBe(0);
    expect(Array.isArray(preData[0]?.messages)).toBe(true);
  });

  it('should include serialized messages with media replaced by size_bytes', async () => {
    const messages: Message[] = [
      {
        role: 'user',
        content: [
          { type: 'text', text: 'look at this screenshot' },
          { type: 'image_url', imageUrl: { url: 'data:image/png;base64,' + 'A'.repeat(100) } },
        ],
        toolCalls: [],
      },
    ];
    const preData: PreLlmRequestData[] = [];
    const hooks: LoopHooks = {
      preLlmRequest: (data) => preData.push(data),
    };
    await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
      contextOptions: { messages },
    });
    expect(preData).toHaveLength(1);
    const msg = preData[0]?.messages[0];
    expect(msg?.role).toBe('user');
    expect(msg?.content).toHaveLength(2);
    expect(msg?.content[0]?.type).toBe('text');
    expect(msg?.content[0]?.text).toBe('look at this screenshot');
    // Media should be replaced by size_bytes marker.
    expect(msg?.content[1]?.type).toBe('image_url');
    expect(msg?.content[1]?.size_bytes).toBeGreaterThan(0);
    // The raw base64 URL must not be present.
    expect(msg?.content[1]?.text).toBeUndefined();
  });

  it('should include tool declarations from system message.tools', async () => {
    const messages: Message[] = [
      {
        role: 'system',
        content: [{ type: 'text', text: 'system prompt' }],
        toolCalls: [],
        tools: [
          {
            name: 'dynamic_tool',
            description: 'A tool loaded mid-conversation',
            parameters: { type: 'object', properties: { x: { type: 'number' } } },
          },
        ],
      },
    ];
    const preData: PreLlmRequestData[] = [];
    const hooks: LoopHooks = {
      preLlmRequest: (data) => preData.push(data),
    };
    await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
      contextOptions: { messages },
    });
    const sysMsg = preData[0]?.messages[0];
    expect(sysMsg?.tools).toHaveLength(1);
    expect(sysMsg?.tools?.[0]?.name).toBe('dynamic_tool');
    expect(sysMsg?.tools?.[0]?.description).toBe('A tool loaded mid-conversation');
    expect(sysMsg?.tools?.[0]?.parameters).toEqual({
      type: 'object',
      properties: { x: { type: 'number' } },
    });
  });

  it('should fire once per step in a multi-step turn', async () => {
    let preCount = 0;
    const hooks: LoopHooks = {
      preLlmRequest: () => { preCount += 1; },
    };
    await runTurn({
      hooks,
      tools: [new EchoTool()],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: '1' }, 'a')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(preCount).toBe(2);
  });

  it('should not block the turn when the hook throws', async () => {
    const hooks: LoopHooks = {
      preLlmRequest: () => { throw new Error('observer crashed'); },
    };
    const { result } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
  });
});

describe('runTurn — postLlmRequest hook', () => {
  it('should fire after the LLM response returns but before tool execution', async () => {
    const callOrder: string[] = [];
    const postData: PostLlmRequestData[] = [];
    const echo = new EchoTool();
    const hooks: LoopHooks = {
      postLlmRequest: (data) => {
        callOrder.push('postLlm');
        postData.push(data);
      },
    };
    await runTurn({
      hooks,
      tools: [echo],
      responses: [
        makeToolUseResponse(
          [makeToolCall('echo', { text: 'hi' }, 'tc-1')],
          { inputOther: 10, output: 20 },
        ),
        makeEndTurnResponse('done'),
      ],
    });

    // postLlmRequest fires before tool.result is recorded.
    // The tool call must have been registered (echo.calls has data).
    expect(echo.calls.length).toBe(1);
    expect(postData).toHaveLength(2); // one per step
    expect(callOrder[0]).toBe('postLlm');

    const first = postData[0]!;
    expect(first.model).toBe('fake-model');
    expect(first.turnId).toBe('turn-1');
    expect(first.step).toBe(1);
    expect(first.finishReason).toBe('tool_use');
    expect(first.usage.inputOther).toBe(10);
    expect(first.usage.output).toBe(20);
    expect(first.durationMs).toBeGreaterThanOrEqual(0);
    expect(first.toolCallCount).toBe(1);
  });

  it('should fire once per step in a multi-step turn', async () => {
    let postCount = 0;
    const hooks: LoopHooks = {
      postLlmRequest: () => { postCount += 1; },
    };
    await runTurn({
      hooks,
      tools: [new EchoTool()],
      responses: [
        makeToolUseResponse([makeToolCall('echo', { text: '1' }, 'a')]),
        makeEndTurnResponse('done'),
      ],
    });
    expect(postCount).toBe(2);
  });

  it('should not fire when the LLM call throws', async () => {
    let postCount = 0;
    const hooks: LoopHooks = {
      postLlmRequest: () => { postCount += 1; },
    };
    await runTurnExpectingThrow({
      hooks,
      responses: [makeEndTurnResponse('never')],
      llmThrowOnIndex: { index: 0, error: new Error('LLM exploded') },
    });
    expect(postCount).toBe(0);
  });

  it('should not block the turn when the hook throws', async () => {
    const hooks: LoopHooks = {
      postLlmRequest: () => { throw new Error('observer crashed'); },
    };
    const { result } = await runTurn({
      hooks,
      responses: [makeEndTurnResponse('ok')],
    });
    expect(result.stopReason).toBe('end_turn');
  });

  it('should report end_turn finish reason for a non-tool-use response', async () => {
    const postData: PostLlmRequestData[] = [];
    const hooks: LoopHooks = {
      postLlmRequest: (data) => postData.push(data),
    };
    await runTurn({
      hooks,
      responses: [makeEndTurnResponse('all done')],
    });
    expect(postData).toHaveLength(1);
    expect(postData[0]?.finishReason).toBe('end_turn');
    expect(postData[0]?.toolCallCount).toBe(0);
  });
});
