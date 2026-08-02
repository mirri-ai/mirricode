/**
 * `messagePipeline` domain — composition tests for `AgentMessagePipelineService`.
 *
 * These tests pin down the pipeline's execution contract:
 *   - steps run in registration order
 *   - a step whose `appliesTo` returns false is skipped for that projection
 *   - a no-op step (returns the input reference) does not break the chain
 *   - an empty pipeline returns the input reference unchanged
 *   - an aborted signal propagates between steps
 *
 * The two builtin steps (`videoResolution`, `mediaCapabilityAdaptation`) are
 * unit-tested through their owning modules (`videoResolver.test.ts`,
 * `mediaCapabilityProjector.test.ts`); this file tests the pipeline runner,
 * not the steps themselves.
 */

import { describe, expect, it } from 'vitest';

import { AgentMessagePipelineService } from '#/agent/messagePipeline/messagePipelineService';
import type { MessageProjectionStep, RequestProjection } from '#/agent/messagePipeline/messagePipeline';
import type { Message } from '#/kosong/contract/message';

function textMessage(text: string): Message {
  return {
    role: 'user',
    content: [{ type: 'text', text }],
    toolCalls: [],
  };
}

// A minimal stand-in for `Model` — the pipeline never inspects the model in
// these tests; it only forwards it to each step's project callback, which we
// control. Using `as` here keeps the test focused on pipeline composition
// rather than constructing a fully-valid Model.
const stubModel = { id: 'm', capabilities: { image_in: true, video_in: true, audio_in: true } } as unknown as Parameters<
  AgentMessagePipelineService['run']
>[1];

const stubRequester = {} as unknown as Parameters<AgentMessagePipelineService['run']>[2];

const ALL_PROJECTIONS: RequestProjection[] = [
  'normal',
  'strict',
  'media-degraded',
  'media-stripped',
];

describe('AgentMessagePipelineService', () => {
  it('returns the input reference unchanged when no steps are registered', async () => {
    const pipeline = new AgentMessagePipelineService();
    const messages: readonly Message[] = [textMessage('hello')];

    const result = await pipeline.run(messages, stubModel, stubRequester, 'normal', undefined);

    expect(result).toBe(messages);
  });

  it('runs steps in registration order, passing each step the output of the previous', async () => {
    const pipeline = new AgentMessagePipelineService();
    const calls: string[] = [];
    pipeline.register({
      name: 'first',
      project: (ctx) => {
        calls.push('first');
        return [...ctx.messages, textMessage('after-first')];
      },
    });
    pipeline.register({
      name: 'second',
      project: (ctx) => {
        calls.push('second');
        return [...ctx.messages, textMessage('after-second')];
      },
    });

    const result = await pipeline.run(
      [textMessage('start')],
      stubModel,
      stubRequester,
      'normal',
      undefined,
    );

    expect(calls).toEqual(['first', 'second']);
    expect(result.map((message) => (message.content[0] as { text: string }).text)).toEqual([
      'start',
      'after-first',
      'after-second',
    ]);
  });

  it('skips a step whose appliesTo returns false for the active projection', async () => {
    const pipeline = new AgentMessagePipelineService();
    const calls: string[] = [];
    pipeline.register({
      name: 'always',
      project: (ctx) => {
        calls.push('always');
        return ctx.messages;
      },
    });
    pipeline.register({
      name: 'skip-on-stripped',
      appliesTo: (projection) => projection !== 'media-stripped',
      project: (ctx) => {
        calls.push('conditional');
        return ctx.messages;
      },
    });

    await pipeline.run(
      [textMessage('x')],
      stubModel,
      stubRequester,
      'media-stripped',
      undefined,
    );
    expect(calls).toEqual(['always']);

    calls.length = 0;
    await pipeline.run([textMessage('x')], stubModel, stubRequester, 'normal', undefined);
    expect(calls).toEqual(['always', 'conditional']);
  });

  it('applies every step on every projection when appliesTo is omitted', async () => {
    const pipeline = new AgentMessagePipelineService();
    const seenProjections: RequestProjection[] = [];
    pipeline.register({
      name: 'observe',
      project: (ctx) => {
        seenProjections.push(ctx.projection);
        return ctx.messages;
      },
    });

    for (const projection of ALL_PROJECTIONS) {
      await pipeline.run([textMessage('x')], stubModel, stubRequester, projection, undefined);
    }
    expect(seenProjections).toEqual(ALL_PROJECTIONS);
  });

  it('supports async steps and preserves order across awaits', async () => {
    const pipeline = new AgentMessagePipelineService();
    const calls: string[] = [];
    pipeline.register({
      name: 'async-first',
      project: async (ctx) => {
        await Promise.resolve();
        calls.push('async-first');
        return [...ctx.messages, textMessage('a')];
      },
    });
    pipeline.register({
      name: 'sync-second',
      project: (ctx) => {
        calls.push('sync-second');
        return [...ctx.messages, textMessage('b')];
      },
    });

    const result = await pipeline.run(
      [textMessage('start')],
      stubModel,
      stubRequester,
      'normal',
      undefined,
    );

    expect(calls).toEqual(['async-first', 'sync-second']);
    expect(result.map((message) => (message.content[0] as { text: string }).text)).toEqual([
      'start',
      'a',
      'b',
    ]);
  });

  it('throws when the signal is already aborted before a step runs', async () => {
    const pipeline = new AgentMessagePipelineService();
    const controller = new AbortController();
    controller.abort();
    pipeline.register({
      name: 'never',
      project: () => {
        throw new Error('step should not run');
      },
    });

    await expect(
      pipeline.run(
        [textMessage('x')],
        stubModel,
        stubRequester,
        'normal',
        controller.signal,
      ),
    ).rejects.toThrow();
  });

  it('removes a step when its registration disposable is disposed', async () => {
    const pipeline = new AgentMessagePipelineService();
    const calls: string[] = [];
    const registration = pipeline.register({
      name: 'ephemeral',
      project: (ctx) => {
        calls.push('ephemeral');
        return ctx.messages;
      },
    });

    await pipeline.run([textMessage('x')], stubModel, stubRequester, 'normal', undefined);
    expect(calls).toEqual(['ephemeral']);

    registration.dispose();
    calls.length = 0;

    await pipeline.run([textMessage('x')], stubModel, stubRequester, 'normal', undefined);
    expect(calls).toEqual([]);
  });
});
