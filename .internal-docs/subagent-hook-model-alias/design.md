# Restore `model_alias` on the `SubagentStart` External Hook — Design

> **Status**: Draft (brainstorming output, pending user review)
> **Date**: 2026-08-03
> **Related**: `.internal-docs/agent-core-v2/design.md` (v1/v2), user-facing `docs/{en,zh}/customization/hooks.md`

## 1. Background

The external hooks contract (`[[hooks]]`) lets users configure shell commands that run on lifecycle events. `SubagentStart` fires when one agent dispatches an agent run (subagent).

v1 (`packages/agent-core`) sends a `model_alias` field in the `SubagentStart` payload — the model the subagent run was launched with. v2 (`packages/agent-core-v2`) dropped it: the `SubagentStart` payload only carries `agent_name` / `prompt`. This is one of several payload fields lost in the v1→v2 port (same survey also lists `Stop`'s `last_assistant_message`, `PreCompact`'s `context_window_size`, etc.).

## 2. Goal & Non-Goals

**Goal**: v2's `SubagentStart` payload gains `model_alias`, semantically aligned with v1.

**Non-goals**:
- No other event changes. `SubagentStop` stays as-is (YAGNI).
- No schema changes: `HOOK_EVENT_TYPES`, `HookDef`, and the `[[hooks]]` config section are untouched. This is a payload-only, backward-compatible extension.

## 3. Payload Contract

`SubagentStart` stdin data becomes:

```json
{
  "hook_event_name": "SubagentStart",
  "session_id": "...",
  "agent_name": "...",
  "prompt": "...",
  "model_alias": "glm-5.2"
}
```

- **Field name**: `model_alias`. `inputData` keys are camelCase→snake_case-converted automatically in the hook runner (`app/externalHooksRunner/runner.ts` `camelToSnake`), so the code carries `modelAlias` and the wire carries `model_alias` — identical to v1's shape.
- **Value semantics**: the model the subagent run was **bound to at spawn** — i.e. the `binding.model` result of `resolveSubagentBinding` (LLM `args.model` → profile `defaultModel`/preference → inherit caller). This mirrors v1, where `modelAlias = options.model ?? profile.defaultModel ?? parent.config.modelAlias` was snapshotted when the child was configured.
- **Type**: `string`, always present. All three resolution tiers of `resolveSubagentBinding` fall back to a concrete model; `IAgentProfileService.getModel()` returns `modelAlias ?? ''` and never throws, so the field serializes as a string (possibly `''`).

### Semantics boundary

`model_alias` describes the model the run **started with**. It does not promise that every LLM request inside the run uses it — a run can rotate models mid-turn. v1 had the identical boundary: `SubagentStart` is a start-time event evaluated before the body runs.

## 4. Data Flow (after)

```
agentTool / agentSwarmTool / sessionSwarmService / sessionInitService
    │  resolveSubagentBinding() → binding.model; lifecycle.create(binding)   // model decided here
    ▼
subagents.run(...)
    ▼
mirrorAgentRun(requester, run, { profileName, prompt, signal })
    │  modelAlias = agentLifecycle.get(run.agentId)
    │                ?.accessor.get(IAgentProfileService)?.getModel() ?? ''
    │  subagents.hooks.onWillStartAgentTask.run({ agentName, prompt, signal, modelAlias })
    ▼
SessionExternalHooksService.runSubagentStart
    │  runner.trigger('SubagentStart',
    │      { matcherValue: agentName, inputData: { agentName, prompt, modelAlias } })
    ▼
[[hooks]] SubagentStart command receives model_alias
```

All four spawn paths funnel through `mirrorAgentRun`, so this single read site covers every path — including swarm and `/init`, which never pass through the tool-level `Agent`/`AgentSwarm` code.

## 5. Design Decisions

### 5.1 Read the model at the mirror (chosen)

`mirrorAgentRun` reads the child's bound model via `IAgentProfileService.getModel()` right where the `onWillStartAgentTask` hook fires.

Why this site:
- The binding is decided **before** the run is launched (tools call `resolveSubagentBinding` → `lifecycle.create` → `subagents.run` → `mirrorAgentRun`), so the read always sees the final model.
- Matches the existing `childContextTokens` pattern in the same file (reads child `IAgentContextSizeService` the same way) — no new dependency threading.
- Zero changes to the four spawn call sites.

Rejected alternatives:
- **Per-call-site plumbing** (`MirrorAgentRunOptions.model` filled by each tool): four sites to keep in sync; a future spawn entry can silently drop the field.
- **Lookup inside `SessionExternalHooksService`**: breaks that adapter's "observe-only" charter (`externalHooks.ts` declares it observes slot hosts and only translates), and it would need the child agent id, which the hook context does not carry.

### 5.2 `getModel()` safety

`getModel()` is `return this.modelAlias ?? ''` (`agent/profile/profileService.ts:447`) — no throw, no undefined. The read uses optional chaining like `childContextTokens`, and any anomaly yields `''` rather than failing the hook run.

## 6. Files Changed

| File | Change |
|------|--------|
| `src/session/subagent/subagent.ts` | `AgentTaskStartHookContext` gains `readonly modelAlias: string;` |
| `src/session/subagent/mirrorAgentRun.ts` | Fill `modelAlias` at the `onWillStartAgentTask.run` call from child `IAgentProfileService.getModel()` |
| `src/session/externalHooks/externalHooksService.ts` | `runSubagentStart` inputData adds `modelAlias: ctx.modelAlias` |
| Config / types | untouched — payload-only extension |

## 7. Testing

Follows the repo rule: don't add new test files; extend existing ones.

- `test/app/externalHooksRunner/integration.test.ts` — extend existing `SubagentStart` cases (the slot-observer test at `:454`, the `agent_name` payload test at `:907`, and the dispatch test at `:976`): assert `model_alias` appears in the hook JSON when the hook context carries it.
- Stub updates for the newly-required field so the build stays green: `test/session/swarm/sessionSwarm.test.ts:1317`, `test/session/sessionInit/sessionInit.test.ts:55`, `test/tool/tool.test.ts:326` all build `AgentTaskStartHookContext`.
- `mirrorAgentRun`'s child-read plumbing is exercised indirectly by the integration test's `onWillStartAgentTask.run` payload; no dedicated unit for the getter.

## 8. Verification

- `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
- `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/externalHooksRunner/integration.test.ts test/session/swarm/sessionSwarm.test.ts`

## 9. Compatibility & Release

- Backward compatible: adding a payload key cannot break existing hook consumers.
- Change level: `minor` for `@mirri-ai/agent-core-v2`.
- A changeset is generated at PR time per the `gen-changesets` skill (not part of this document).