/**
 * Scenario: per-agent prompt scheduling and launch-failure settlement.
 *
 * Exercises `IAgentPromptService` through DI with controlled context, loop,
 * wire, compaction, and tool-execution collaborators.
 * Run: `pnpm exec vitest run packages/agent-core-v2/test/agent/prompt/promptService.test.ts`.
 */

import { describe, expect, it, onTestFinished, vi } from 'vitest';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices } from '#/_base/di/test';
import { Event } from '#/_base/event';
import { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import type { ContextMessage } from '#/agent/contextMemory/types';
import { IAgentFullCompactionService } from '#/agent/fullCompaction/fullCompaction';
import { IAgentLoopService } from '#/agent/loop/loop';
import { IAgentPromptService } from '#/agent/prompt/prompt';
import { AgentPromptService } from '#/agent/prompt/promptService';
import { IInlineSkillAnalyzer, type InlineSkillReferenceAnalysis } from '#/agent/skill/skill';
import { IAgentSystemReminderService } from '#/agent/systemReminder/systemReminder';
import { AgentSystemReminderService } from '#/agent/systemReminder/systemReminderService';
import { IAgentToolExecutorService } from '#/agent/toolExecutor/toolExecutor';
import { IEventBus } from '#/app/event/eventBus';
import { EventBusService } from '#/app/event/eventBusService';
import { ErrorCodes, Error2 } from '#/errors';
import { createHooks } from '#/hooks';
import { IWireService } from '#/wire/wire';

import { stubContextMemory } from '../contextMemory/stubs';
import { stubLoopWithHooks, stubToolExecutor, stubWire } from '../loop/stubs';
import { registerStateServices } from '../../state/stubs';

function message(text: string): ContextMessage {
  return { role: 'user', content: [{ type: 'text', text }], toolCalls: [], origin: { kind: 'user' } };
}

function harness() {
  const disposables = new DisposableStore();
  onTestFinished(() => disposables.dispose());
  const context = stubContextMemory();
  const loop = stubLoopWithHooks({ pendingTurnResult: true });
  const fullCompaction = {
    _serviceBrand: undefined,
    compacting: null,
    begin: () => false,
    hooks: createHooks(['onWillCompact']),
    onDidFinishCompaction: Event.None,
  } as unknown as IAgentFullCompactionService;
  const ix = createServices(disposables, {
    strict: true, additionalServices: (reg) => {
      registerStateServices(reg);
      reg.defineInstance(IAgentContextMemoryService, context);
      reg.defineInstance(IAgentLoopService, loop);
      reg.defineInstance(IWireService, stubWire());
      reg.defineInstance(IAgentToolExecutorService, stubToolExecutor());
      reg.defineInstance(IAgentFullCompactionService, fullCompaction);
      reg.define(IEventBus, EventBusService);
      reg.define(IAgentSystemReminderService, AgentSystemReminderService);
      reg.define(IAgentPromptService, AgentPromptService);
    }
  });
  return { prompt: ix.get(IAgentPromptService), loop, context, fullCompaction, ix };
}

describe('AgentPromptService', () => {
  it('assigns stable identity and launches an idle prompt', async () => {
    const { prompt } = harness();
    const handle = await prompt.enqueue({ id: 'prompt-1', message: message('hello') });
    expect(handle.id).toBe('prompt-1');
    expect(handle.userMessageId).toBe('prompt-1');
    expect((await handle.launched)?.id).toBe(0);
  });

  it('keeps later prompts in FIFO order while active', async () => {
    const { prompt } = harness();
    await prompt.enqueue({ message: message('active') });
    const first = await prompt.enqueue({ message: message('one') });
    const second = await prompt.enqueue({ message: message('two') });
    expect(prompt.list().pending.map((item) => item.id)).toEqual([first.id, second.id]);
  });

  it('atomically rejects steer when any id is not pending', async () => {
    const { prompt } = harness();
    await prompt.enqueue({ message: message('active') });
    const queued = await prompt.enqueue({ message: message('one') });
    await expect(prompt.steer([queued.id, 'missing'])).rejects.toMatchObject({ code: 'prompt.not_found' });
    expect(prompt.list().pending.map((item) => item.id)).toEqual([queued.id]);
  });

  it('steers selected prompts in FIFO order', async () => {
    const { prompt, context, loop } = harness();
    const active = await prompt.enqueue({ message: message('active') });
    await active.launched;
    const one = await prompt.enqueue({ message: message('one') });
    const two = await prompt.enqueue({ message: message('two') });
    const handles = await prompt.steer([two.id, one.id]);
    expect(handles.map((item) => item.id)).toEqual([one.id, two.id]);
    loop.drainNextBatch(context);
  });

  it('aborts pending prompts and settles completion', async () => {
    const { prompt } = harness();
    await prompt.enqueue({ message: message('active') });
    const handle = await prompt.enqueue({ message: message('queued') });
    expect(prompt.abort(handle.id)).toBe(true);
    await expect(handle.completion).resolves.toMatchObject({ state: 'cancelled' });
    expect(prompt.list().pending).toEqual([]);
  });

  it('keeps injections outside the prompt queue', async () => {
    const { prompt } = harness();
    await prompt.inject({ ...message('system'), origin: { kind: 'injection', variant: 'test' } });
    expect(prompt.list()).toEqual({ active: undefined, pending: [] });
  });

  it('settles blocked prompts', async () => {
    const { prompt } = harness();
    prompt.hooks.onBeforeSubmitPrompt.register('block', async (ctx, next) => { ctx.block = true; await next(); });
    const handle = await prompt.enqueue({ message: message('blocked') });
    await expect(handle.completion).resolves.toMatchObject({ state: 'blocked' });
  });

  it('settles the prompt as failed when the loop throws on launch', async () => {
    const { prompt, loop } = harness();
    vi.spyOn(loop, 'enqueue').mockImplementation(() => {
      throw new Error2(ErrorCodes.TURN_AGENT_BUSY, 'Cannot launch a new turn while another turn is active');
    });
    const handle = await prompt.enqueue({ id: 'prompt-x', message: message('hello') });
    expect(handle.state).toBe('failed');
    await expect(handle.launched).resolves.toBeUndefined();
    await expect(handle.completion).resolves.toMatchObject({ state: 'failed', result: undefined });
    expect(prompt.list()).toEqual({ active: undefined, pending: [] });
  });

  it('replaces an unsupported prompt image with a text notice at the history funnel', async () => {
    const { prompt, context, loop } = harness();
    const avifUrl = `data:image/avif;base64,${Buffer.from([1, 2, 3]).toString('base64')}`;
    const handle = await prompt.enqueue({
      id: 'prompt-img',
      message: {
        role: 'user',
        content: [{ type: 'image_url', imageUrl: { url: avifUrl } }],
        toolCalls: [],
        origin: { kind: 'user' },
      },
    });
    await handle.launched;
    loop.drainNextBatch(context);

    const appended = context.get();
    expect(appended).toHaveLength(1);
    const parts = appended[0]!.content;
    expect(parts.some((part) => part.type === 'image_url')).toBe(false);
    expect(parts[0]).toMatchObject({ type: 'text' });
    expect((parts[0] as { text: string }).text).toContain('image/avif');
  });

  it('gates steered prompt images too', async () => {
    const { prompt, context, loop } = harness();
    const active = await prompt.enqueue({ message: message('active') });
    await active.launched;
    const avifUrl = `data:image/avif;base64,${Buffer.from([4, 5, 6]).toString('base64')}`;
    const queued = await prompt.enqueue({
      id: 'prompt-steer-img',
      message: {
        role: 'user',
        content: [{ type: 'image_url', imageUrl: { url: avifUrl } }],
        toolCalls: [],
        origin: { kind: 'user' },
      },
    });
    await prompt.steer([queued.id]);
    loop.drainNextBatch(context);

    const appended = context.get();
    const parts = appended.flatMap((entry) => (entry.content as { type: string; text?: string; imageUrl?: unknown }[]));
    expect(parts.some((part) => part.type === 'image_url')).toBe(false);
    expect(
      parts.some((part) => part.type === 'text' && (part.text ?? '').includes('image/avif')),
    ).toBe(true);
  });

  // ---------------------------------------------------------------------
  // Deferred materialization (queued intent is materialized only when it
  // becomes the active prompt)
  // ---------------------------------------------------------------------

  it('should defer the materializer until the prompt reaches the queue head when another turn is active', async () => {
    const { prompt } = harness();
    const active = await prompt.enqueue({ message: message('active') });
    await active.launched;

    const materialize = vi.fn().mockResolvedValue(message('deferred content'));
    const handle = await prompt.enqueue({ message: message('placeholder'), materialize });

    // Queued as intent only — the materializer must not run while the running
    // turn keeps the scheduler busy.
    expect(prompt.list().pending.map((item) => item.id)).toEqual([handle.id]);
    expect(materialize).not.toHaveBeenCalled();
  });

  it('should materialize an immediately-launching prompt before the loop receives it', async () => {
    const { prompt, context, loop } = harness();
    const materialize = vi.fn().mockResolvedValue(message('resolved content'));
    const handle = await prompt.enqueue({
      id: 'prompt-deferred-idle',
      message: message('placeholder'),
      materialize,
    });
    await handle.launched;

    expect(materialize).toHaveBeenCalledTimes(1);
    loop.drainNextBatch(context);
    const parts = context.get().flatMap((entry) => entry.content);
    expect(parts.some((part) => part.type === 'text' && part.text === 'resolved content')).toBe(true);
  });

  it('should settle the prompt as failed and continue draining when the materializer throws', async () => {
    const { prompt } = harness();
    const materialize = vi.fn().mockRejectedValue(new Error('skill removed while queued'));
    const handle = await prompt.enqueue({
      message: message('placeholder'),
      materialize,
    });

    await expect(handle.completion).resolves.toMatchObject({ state: 'failed' });
    await expect(handle.launched).resolves.toBeUndefined();
    expect(prompt.list()).toEqual({ active: undefined, pending: [] });
  });

  it('should cancel a pending deferred prompt without invoking its materializer', async () => {
    const { prompt } = harness();
    const active = await prompt.enqueue({ message: message('active') });
    await active.launched;

    const materialize = vi.fn().mockResolvedValue(message('deferred'));
    const handle = await prompt.enqueue({ message: message('placeholder'), materialize });

    expect(prompt.abort(handle.id)).toBe(true);
    await expect(handle.completion).resolves.toMatchObject({ state: 'cancelled' });
    expect(materialize).not.toHaveBeenCalled();
  });

  it('should materialize a deferred prompt before merging it into a steer', async () => {
    const { prompt, context, loop } = harness();
    const active = await prompt.enqueue({ message: message('active') });
    await active.launched;

    const materialize = vi.fn().mockResolvedValue(message('steer ready'));
    const handle = await prompt.enqueue({ message: message('placeholder'), materialize });

    await prompt.steer([handle.id]);
    loop.drainNextBatch(context);

    expect(materialize).toHaveBeenCalledTimes(1);
    const parts = context.get().flatMap((entry) => entry.content);
    expect(parts.some((part) => part.type === 'text' && part.text === 'steer ready')).toBe(true);
    expect(parts.some((part) => part.type === 'text' && part.text === 'placeholder')).toBe(false);
  });

  // ---------------------------------------------------------------------
  // Inline skill reference injection (US3.6 / ADR-004) — a user message with
  // inline references materializes as N activation wrappers + the body in
  // one turn, and the REST/edge analysis is exposed on the handle.
  // ---------------------------------------------------------------------

  describe('inline skill reference injection', () => {
    function inlineHarness(analysis: Partial<InlineSkillReferenceAnalysis> = {}) {
      const full: InlineSkillReferenceAnalysis = {
        body: 'please do the review',
        wrappers: [
          {
            skillName: 'review',
            target: '/skills/review/SKILL.md',
            materialize: vi.fn().mockResolvedValue({
              role: 'user',
              content: [{ type: 'text', text: 'User activated the skill "review".' }],
              toolCalls: [],
              origin: {
                kind: 'skill_activation',
                activationId: 'p1#0',
                skillName: 'review',
                trigger: 'user-inline',
              },
            }),
          },
        ],
        pendingCommands: [{ name: 'clear', display: '/clear', target: 'command:clear' }],
        skipped: [{ name: 'ghost', target: '/gone/SKILL.md', reason: 'skill_not_found' }],
        activatedSkills: [
          { name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' },
        ],
        ...analysis,
      };
      const h = harness();
      const analyzer = {
        _serviceBrand: undefined,
        analyzeInlineReferences: vi.fn().mockResolvedValue(full),
      };
      h.ix.set(IInlineSkillAnalyzer, analyzer);
      return { ...h, analysis: full, analyzer };
    }

    it('should append the activation wrappers before the body message in the same turn', async () => {
      const { prompt, context, loop, analysis } = inlineHarness();
      const handle = await prompt.enqueue({
        id: 'p1',
        message: message('please run [$review](/skills/review/SKILL.md) and review'),
      });
      // The edge analysis rides on the handle for the REST response.
      expect(handle.inline).toMatchObject({
        pendingCommands: [{ name: 'clear', display: '/clear', target: 'command:clear' }],
        skipped: [{ name: 'ghost', target: '/gone/SKILL.md' }],
      });
      await handle.launched;
      loop.drainNextBatch(context);

      const appended = context.get();
      expect(appended).toHaveLength(2);
      expect(appended[0]!.origin).toMatchObject({
        kind: 'skill_activation',
        trigger: 'user-inline',
        activationId: 'p1#0',
      });
      expect((appended[1]!.content[0] as { text: string }).text).toBe(analysis.body);
      // ADR-006: the rewritten body message carries the activation record.
      expect(appended[1]!.with_skills).toEqual([
        { name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' },
      ]);
    });

    it('should keep a plain message unchanged and attach no inline analysis when there is nothing to expose', async () => {
      const { prompt, context, loop, analysis } = inlineHarness({
        body: 'plain message',
        wrappers: [],
        pendingCommands: [],
        skipped: [],
      });
      const handle = await prompt.enqueue({ id: 'p2', message: message('plain message') });
      expect(handle.inline).toBeUndefined();
      await handle.launched;
      loop.drainNextBatch(context);
      const appended = context.get();
      expect(appended).toHaveLength(1);
      expect((appended[0]!.content[0] as { text: string }).text).toBe(analysis.body);
    });

    it('should not stamp with_skills on the body message when nothing activated', async () => {
      const { prompt, context, loop } = inlineHarness({
        body: 'plain message',
        wrappers: [],
        pendingCommands: [],
        skipped: [],
        activatedSkills: [],
      });
      const handle = await prompt.enqueue({ id: 'p2b', message: message('plain message') });
      await handle.launched;
      loop.drainNextBatch(context);
      const appended = context.get();
      expect(appended[0]!.with_skills).toBeUndefined();
    });

    it('should not analyze non-user-origin messages', async () => {
      const { prompt, context, loop } = inlineHarness();
      const handle = await prompt.enqueue({
        id: 'p3',
        message: { ...message('[$review](/skills/review/SKILL.md)'), origin: { kind: 'injection', variant: 'test' } },
      });
      expect(handle.inline).toBeUndefined();
      await handle.launched;
      loop.drainNextBatch(context);
      const appended = context.get();
      expect(appended).toHaveLength(1);
      expect((appended[0]!.content[0] as { text: string }).text).toBe('[$review](/skills/review/SKILL.md)');
    });

    it('should carry pending commands on the handle when the message is queued behind an active turn', async () => {
      // Given a busy scheduler (an active turn) and a queued user message
      // carrying an inline command reference, when the enqueue call returns,
      // the handle must already expose the parsed pending commands — the
      // pure parse at submission time needs no catalog. `skipped` stays
      // admission-time (empty here until the turn launches).
      const { prompt } = inlineHarness();
      const active = await prompt.enqueue({ message: message('active') });
      await active.launched;

      const handle = await prompt.enqueue({
        id: 'p-queued',
        message: message('run [$review](/skills/review/SKILL.md) and [$/clear](command:clear) please'),
      });

      expect(prompt.list().pending.map((item) => item.id)).toEqual([handle.id]);
      expect(handle.inline).toMatchObject({
        pendingCommands: [{ name: 'clear', display: '/clear', target: 'command:clear' }],
        skipped: [],
      });
    });

    it('should attach no pending commands at submission for a plain queued message', async () => {
      const { prompt } = inlineHarness();
      const active = await prompt.enqueue({ message: message('active') });
      await active.launched;

      const handle = await prompt.enqueue({ message: message('no references here') });
      expect(handle.inline).toBeUndefined();
    });
  });
});
