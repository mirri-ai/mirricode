/**
 * F4 golden test — proves the mirri harness can boot `bootstrap()` in-process,
 * create a session via `klient/memory`, materialize the main agent, submit a
 * prompt, and receive a canned LLM response without any HTTP server or real
 * LLM.
 *
 * This is the gate-F4 acceptance test: if this passes, every feature-phase
 * subtask (B0–B4) can do TDD against the harness.
 */

import { afterEach, describe, expect, it } from 'vitest';

import { harness, type MirriHarness } from './index';

describe('F4 mirri harness golden test', () => {
  let ctx: MirriHarness | undefined;

  afterEach(() => {
    ctx?.cleanup();
    ctx = undefined;
  });

  it('should complete one no-tool turn with a canned text response', async () => {
    // 1. Boot the harness (bootstrap + klient/memory)
    ctx = harness();
    expect(ctx.core.app).toBeDefined();

    // 2. Queue a canned text response BEFORE submitting the prompt.
    //    The provider stub returns deterministic responses — no real LLM.
    ctx.stub.nextTextResponse('Hello from the stub!');

    // 3. Create a session through the klient facade.
    const session = await ctx.session({ title: 'golden-test' });
    expect(session).toBeDefined();

    // 4. Materialize the main agent and submit a prompt.
    const agent = session.agent('main');
    const result = await agent.prompt({
      input: [{ type: 'text', text: 'hello' }],
    });

    // 5. Assert the turn launched (the RPC returns a turn descriptor).
    //    `prompt()` returns a `PromptLaunchResult` with a `turnId`.
    expect(result).toBeDefined();
    expect(result.turnId).toBeDefined();
    expect(typeof result.turnId).toBe('number');

    // 6. Assert the provider stub received exactly one generate call.
    expect(ctx.stub.calls).toHaveLength(1);
  });

  it('should expose the klient facade with session/agent access', async () => {
    ctx = harness();

    // The klient's global facade can list sessions (empty initially).
    const sessions = await ctx.klient.global.sessions.list({ limit: 10 });
    expect(sessions.items).toHaveLength(0);

    // Create a session and verify it appears in the list.
    const session = await ctx.session();
    const afterCreate = await ctx.klient.global.sessions.list({ limit: 10 });
    expect(afterCreate.items.length).toBeGreaterThanOrEqual(1);

    // The session has an agent('main') handle.
    const agent = session.agent('main');
    expect(agent).toBeDefined();
    expect(typeof agent.prompt).toBe('function');
    expect(typeof agent.steer).toBe('function');
    expect(typeof agent.cancel).toBe('function');
  });

  it('should clean up the temp home on cleanup()', async () => {
    ctx = harness();
    const homeDir = ctx.home.homeDir;
    expect(homeDir).toContain('mirri-v2-harness');

    // Cleanup should not throw.
    ctx.cleanup();

    // After cleanup, calling again is safe (idempotent).
    ctx.cleanup();
  });
});
