# Restore `model_alias` on `SubagentStart` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** v2's `SubagentStart` external-hook payload gains `model_alias` (the model the subagent run was bound to at spawn), aligned with v1.

**Architecture:** `AgentTaskStartHookContext` gains a required `modelAlias: string` field. `mirrorAgentRun` fills it by reading the child agent's `IAgentProfileService.getModel()` (same pattern as the existing `childContextTokens` helper); `SessionExternalHooksService.runSubagentStart` forwards it into the hook `inputData`, where the runner's `camelToSnake` turns it into `model_alias` on the wire.

**Tech Stack:** TypeScript, vitest, pnpm workspace. No new dependencies.

## Global Constraints

- Work inside `packages/agent-core-v2`.
- `AgentTaskStartHookContext.modelAlias` is **required** (`string`), never optional; it may be `''`.
- No new test files — extend `test/app/externalHooksRunner/integration.test.ts` only.
- Do not touch `HOOK_EVENT_TYPES`, `HookDef`, or the hooks config schema.
- Test naming: Given-When-Then style (`should … when …`).
- Typecheck: `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`. Tests: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run <file>`.

---

### Task 1: Transport contract — field, forward, and assertion

**Files:**
- Modify: `packages/agent-core-v2/src/session/subagent/subagent.ts` (`AgentTaskStartHookContext`, lines 35-39)
- Modify: `packages/agent-core-v2/src/session/externalHooks/externalHooksService.ts` (`runSubagentStart`, lines 96-107)
- Test: `packages/agent-core-v2/test/app/externalHooksRunner/integration.test.ts` (the `observes the agent-run hook slots…` test, lines 454-559)

**Interfaces:**
- Consumes: existing `AgentTaskStartHookContext` (`{ agentName, prompt, signal }`).
- Produces: `AgentTaskStartHookContext` gains `readonly modelAlias: string;`. This is the contract Task 2 build on.

- [ ] **Step 1: Write the failing assertion (extend the integration test)**

In `test/app/externalHooksRunner/integration.test.ts`, in the `observes the agent-run hook slots to fire SubagentStart and SubagentStop` test:

- [ ] Step 1a: extend the `onWillStartAgentTask.run({...})` call to pass `modelAlias`:

```typescript
      await subagents.hooks.onWillStartAgentTask.run({
        agentName: 'coder',
        prompt: 'Fix the bug',
        signal: new AbortController().signal,
        modelAlias: 'glm-5.2',
      });
```

- [ ] Step 1b. — update the `triggered` expectation to include the forwarded field:

```typescript
      expect(triggered).toEqual([
        {
          event: 'SubagentStart',
          matcherValue: 'coder',
          inputData: { agentName: 'coder', prompt: 'Fix the bug', modelAlias: 'glm-5.2' },
          signal: expect.any(AbortSignal),
        },
      ]);
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/externalHooksRunner/integration.test.ts -t "observes the agent-run hook slots"`
Expected: FAIL — `SubagentStart` fired with `inputData: { agentName: 'coder', prompt: 'Fix the bug' }` (no `modelAlias`), assertion mismatch. The call site also fails `tsc` (missing `modelAlias` on `AgentTaskStartHookContext`).

- [ ] **Step 3: Add the required field to the hook-context type**

In `packages/agent-core-v2/src/session/subagent/subagent.ts`:

```typescript
export interface AgentTaskStartHookContext {
  readonly agentName: string;
  readonly prompt: string;
  readonly signal: AbortSignal;
  readonly modelAlias: string;
}
```

- [ ] **Step 4: Forward the field in the session adapter**

In `packages/agent-core-v2/src/session/externalHooks/externalHooksService.ts`, `runSubagentStart`:

```typescript
  private async runSubagentStart(ctx: AgentTaskStartHookContext): Promise<void> {
    ctx.signal.throwIfAborted();
    await this.runner.trigger('SubagentStart', {
      matcherValue: ctx.agentName,
      signal: ctx.signal,
      inputData: {
        agentName: ctx.agentName,
        prompt: ctx.prompt,
        modelAlias: ctx.modelAlias,
      },
    });
    ctx.signal.throwIfAborted();
  }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/externalHooksRunner/integration.test.ts -t "observes the agent-run hook slots"`
Expected: PASS — the triggered event now carries `modelAlias: 'glm-5.2'` in `inputData`.

Also run the whole file to confirm no unrelated breakage:
`pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/externalHooksRunner/integration.test.ts`

- [ ] **Step 6: Typecheck**

Run: `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
Expected: PASS. (`tool.test.ts:326`, `sessionSwarm.test.ts:1317`, `sessionInit.test.ts:55` do not construct a hook context, so they compile unchanged.)

- [ ] **Step 7: Commit**

```bash
git add packages/agent-core-v2/src/session/subagent/subagent.ts packages/agent-core-v2/src/session/externalHooks/externalHooksService.ts packages/agent-core-v2/test/app/externalHooksRunner/integration.test.ts
git commit -m "feat(agent-core-v2): forward modelAlias in SubagentStart external hook"
```

---

### Task 2: Feed the real bound model from `mirrorAgentRun`

**Files:**
- Modify: `packages/agent-core-v2/src/session/subagent/mirrorAgentRun.ts` (hook call at lines 131-146; add a helper next to `childContextTokens` at lines 188-194)
- No new test file — per spec, the read path is covered by the existing integration test's hand-fed `modelAlias` and by `typecheck`. `IAgentProfileService` is already imported at `mirrorAgentRun.ts:24`.

**Interfaces:**
- Consumes: `AgentTaskStartHookContext.modelAlias` from Task 1; `IAgentProfileService.getModel(): string` (returns `modelAlias ?? ''`, never throws).
- Produces: the `onWillStartAgentTask.run({...})` call now carries the child's bound model.

- [ ] **Step 1: Fill `modelAlias` at the hook call site**

In `packages/agent-core-v2/src/session/subagent/mirrorAgentRun.ts`, replace the hook invocation (lines 139-146):

```typescript
    try {
      await subagents?.hooks.onWillStartAgentTask.run({
        agentName: options.profileName,
        prompt: options.prompt,
        signal: options.signal,
        modelAlias: childModelAlias(agentLifecycle, run.agentId),
      });
    } catch (error) {
      cancelAndRethrow(error);
    }
```

- [ ] **Step 2: Add the `childModelAlias` helper next to `childContextTokens`**

In the same file, after the existing `childContextTokens` function:

```typescript
function childModelAlias(
  agentLifecycle: IAgentLifecycleService | undefined,
  agentId: string,
): string {
  if (agentLifecycle === undefined) return '';
  return agentLifecycle.get(agentId)?.accessor.get(IAgentProfileService)?.getModel() ?? '';
}
```

Note: `getModel()` returns `modelAlias ?? ''` and never throws (`src/agent/profile/profileService.ts:447`), so the helper yields `''` at worst.

- [ ] **Step 3: Typecheck**

Run: `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
Expected: PASS (imports and the new field already in place from Task 1).

- [ ] **Step 4: Run the downstream test suites that exercise spawn flows**

Run:
```bash
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/externalHooksRunner/integration.test.ts test/session/swarm/sessionSwarm.test.ts
```
Expected: PASS (the integration `SubagentStart` test now receives `modelAlias` from the real hook chain's per-call payload; `sessionSwarm` guards the spawn path compiles/runs unchanged).

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/session/subagent/mirrorAgentRun.ts
git commit -m "feat(agent-core-v2): report bound model in SubagentStart hook payload"
```

---

## Self-Review

- **Spec coverage** — spec §3 (payload `model_alias`, string, spawn-time value): Task 1 contract + Task 2 value source. §4 data flow: both tasks. §6 files: all three + test file. §7 verification: Task 1 Steps 6/7, Task 2 Steps 3/4. §9 (minor, backward-compatible): no impl step needed.
- **Placeholder scan** — no TBD/TODO; every step carries exact code/commands.
- **Type consistency** — the field is `modelAlias` everywhere (`subagent.ts` type, service DI, `mirrorAgentRun`, test payload); the wire name `model_alias` is produced by the runner's existing `camelToSnake`, no code references `model_alias` directly.