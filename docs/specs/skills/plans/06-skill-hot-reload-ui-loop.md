# Skill Hot-Reload UI Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the skill-catalog hot-reload user-facing loop — the `skillHotReload` flag defaults ON, the web client subscribes to `skill.catalogChanged` and refreshes its skill caches + shows a non-blocking toast, the skills settings panel gains a rescan button with in-place result and watcher status, and the ACP translation layer projects the event as a session notification.

**Architecture:** Five independent cuts: (1) rust engine flips the `skillHotReload` FlagDef default to `true` and its dependents' tests; (2) the web ws client subscribes via `skill.subscribe` after handshake and routes the event through the any-event path into an `AppEvent`; (3) the web client state invalidates the per-session/per-workspace skill caches (event-driven, reusing the existing lazy-load functions) and pushes an info toast; (4) SkillsPanel gains the rescan button (`skill.reload` RPC) with in-place result + watcher status; (5) `mirri-acp`'s EventTranslator projects the event as an `agent_message_chunk` session update. Spec: `docs/specs/skills-requirements.md` US7.2/SDR-010, `docs/specs/skills-architecture.md` ADR-013, current-state evidence `docs/specs/skills-context.md` §11.2.

**Tech Stack:** Rust (mirri-embedded, mirri-acp, tokio), Vue 3 + TypeScript + vitest (apps/mirri-web).

## Global Constraints

- `skillHotReload` OFF path stays reachable: env override + config `[experimental]` explicit false remain (ADR-013 decision 1) — never remove the layers.
- Web subscription failure is a silent degradation for old backends (`skill.subscribe` → method-not-found → swallow); it must never break the connection or surface an error toast (ADR-013 decision 2).
- Cache invalidation scope: ALL cached session keys of the event's workspace + the workspace key; only when at least one cache key was already populated (ADR-013 decision 3). Never fetch for a workspace the UI never pulled.
- Reuse `loadSkillsForSession` / `loadSkillsForWorkspace` for every refetch — no new fetch surface, no polling, no menu-open refetch (ADR-013 key rulings).
- Tests use Given-When-Then naming (`should ... when ...`); prefer adding to existing test files over creating new ones.
- Rust: no new workspace deps. Web: no new npm deps.
- Commit messages: no co-author attribution, no agent identity reveal.
- Verification commands: rust — `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded -p mirri-acp` (background, ≥20 min timeout); web — `pnpm --filter @mirri-ai/mirri-web run typecheck` + `pnpm --filter @mirri-ai/mirri-web exec vitest run <file>`.

---

### Task 1: Rust — `skillHotReload` default ON

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs` (~line 172-179, the FlagDef registration)
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs` (the tests that seal the default at ~26501 and ~35593)

**Interfaces:**
- Consumes: `mirri_core::FlagDef { id, default, description, title, surface, env_name }`
- Produces: `SKILL_HOT_RELOAD_FLAG` registered with `default: true`; flag description says ON by default with the off-paths documented.

- [ ] **Step 1: Flip the registration**

In `rust/crates/mirri-embedded/src/embedded_server/mod.rs`, find the FlagDef registration for `crate::SKILL_HOT_RELOAD_FLAG` (the block containing `default: false` and the description starting "Mount per-workspace skill directory watchers"). Change `default: false` to `default: true` and rewrite the description to:

```rust
        description: "Mount per-workspace skill directory watchers that hot-reload the skill catalog on file changes (ADR-010). On by default (ADR-013); disable via the experimental flags layer (env override or config [experimental] explicit false) for network-home or restricted environments. The manual reload (skill.reload RPC) stays available either way.".to_string(),
```

- [ ] **Step 2: Fix the default-sealing tests**

In `rust/crates/mirri-embedded/src/embedded_server/tests.rs`:

- **`should_answer_the_env_override_chain_when_flag_methods_called` (~26448)**: the `"skillHotReload": false` at ~26496 is the `flag.snapshot` EXPECTED assertion (this test's server has NO skillHotReload config layer — it seals the registered default). After the flip: change the snapshot assertion to `"skillHotReload": true`, change the `flag.enabledIds` assertion at ~26508 from `json!(["test.feature"])` to `json!(["skillHotReload", "test.feature"])`, and rewrite the stale comment at ~26505-26507 ("skillHotReload is default-off while the feature settles") to state the new default (ADR-013 decision 1). The `MIRRICODE_EXPERIMENTAL_FLAG` env is already cleared at ~26462-26465 — keep that (it guards against a dev machine's global-flag env making the test flaky).
- **~35593** (`[experimental]\nskillHotReload = false` in a config string): keep as-is — it pins the config-layer OFF path, which ADR-013 preserves.
- **~42190** (e2e helper inserting `skillHotReload = true`): the surrounding comment (~42181-42184) claims opt-in is required because the flag is default-off — now stale. Keep the explicit `true` insert (explicit beats implicit in tests) but rewrite the comment to "explicitly pinned (the flag's default is on since ADR-013; the e2e helper keeps the pin so a default flip can never silently change e2e watcher behavior)".
- **Audit beyond the grep hits**: the watcher lazy-mounts on every RPC dispatch and the watcher task compresses sub-400ms timing margins (the comment at ~35586-35590 documents this). Search `grep -n "quiet\|debounce\|400" rust/crates/mirri-embedded/src/embedded_server/tests.rs | head -40` and check timing-sensitive tests near skill reload / session flows for watcher-induced churn; where a test needs determinism (no background watcher rescan racing assertions), add explicit `skillHotReload = false` via the same experimental-layer pattern used at 35593 — do NOT weaken assertions.

- [ ] **Step 3: Add the default-flip regression test**

Append to `rust/crates/mirri-embedded/src/embedded_server/tests.rs` (near the other skill-watcher tests, search `should_start_the_skill_directory_watcher`):

```rust
/// Given a fresh server with NO experimental config layer, when the flag
/// registry resolves `skillHotReload`, then the default is enabled — the
/// hot-reload watcher mounts without any user opt-in (ADR-013 decision 1).
#[tokio::test]
async fn should_default_the_skill_hot_reload_flag_to_enabled() {
    // Guard the global flag env exactly like the env-override-chain test
    // (~26462-26465): a dev machine's MIRRICODE_EXPERIMENTAL_FLAG would
    // otherwise flip this default-pin test.
    unsafe {
        std::env::remove_var("MIRRICODE_EXPERIMENTAL_FLAG");
        std::env::remove_var("MIRRICODE_EXPERIMENTAL_SKILL_HOT_RELOAD");
    }
    let (server, _tmp) = test_server(None);
    assert!(
        server.flag_registry().enabled(crate::SKILL_HOT_RELOAD_FLAG),
        "skillHotReload must default to enabled (ADR-013)"
    );
}
```

`test_server(None)` is the existing helper (returns `(EmbeddedServer, TempDir)`; the env-override-chain test at ~26456 shows the call shape). Note the method is `flag_registry()` (accessor), not a public field.

- [ ] **Step 4: Run the tests**

Run (background, ≥20 min): `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded skill_hot_reload skill_watcher`
Expected: PASS, and the previously-sealed default-off tests still pass with their explicit-false configs.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/mod.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs
git commit -m "feat: default skillHotReload flag on (ADR-013)"
```

---

### Task 2: Web — subscribe to `skill.catalogChanged` and route it as an AppEvent

**Files:**
- Modify: `apps/mirri-web/src/api/daemon/ws-rpc-api.ts` (constructor ~469-500; the any-event router at ~1206-1240)
- Modify: `apps/mirri-web/src/api/types.ts` (AppEvent union ~957; MirriEventHandlers unchanged)
- Modify: `apps/mirri-web/src/api/daemon/eventReducer.ts` (exhaustive switch ~1479-1490)
- Test: `apps/mirri-web/test/ws-rpc-event-routing.test.ts` (the `createApiWithAnyHandler` harness at ~355)

**Interfaces:**
- Consumes: `JsonRpcClient.call(method, params)` (throws `JsonRpcCallError` on a server error response — code `-32601` for method-not-found on old backends; the module-private constant `METHOD_NOT_FOUND_CODE` lives in `json-rpc-client.ts:151`, only `NOT_FOUND_CODE` is exported, so use the literal `-32601` with a comment); `this.client.onAnyEvent(...)` already wired in connectEvents; `JsonRpcClient` options `onStateChange(connected)` (fires on every completed handshake — reconnects included).
- Produces: new AppEvent `{ type: 'skillCatalogChanged'; workspaceId: string; added: readonly string[]; removed: readonly string[]; updated: readonly string[]; trigger: 'file' | 'config' | 'plugin' | 'manual' }` — the payload the server sends on `skill.catalogChanged` notifications (camelCase params: `workspaceId`, `added`, `removed`, `updated`, `trigger`).

- [ ] **Step 1: Add the AppEvent type**

In `apps/mirri-web/src/api/types.ts`, add to the `AppEvent` union (after the `workspaceDeleted` member, keeping the workspace-scoped group together):

```typescript
  | {
      type: 'skillCatalogChanged';
      workspaceId: string;
      added: readonly string[];
      removed: readonly string[];
      updated: readonly string[];
      trigger: 'file' | 'config' | 'plugin' | 'manual';
    }
```

Then in `apps/mirri-web/src/api/daemon/eventReducer.ts`, add the no-op case next to the workspace-lifecycle cases (~1479-1484 — "listed explicitly to keep the switch exhaustive"):

```typescript
    case 'skillCatalogChanged':
      // Handled before the reducer (the composable's onEvent intercept
      // refreshes the skill caches + pushes the toast); listed here to
      // keep the switch exhaustive.
      break;
```

- [ ] **Step 2: Subscribe on every completed handshake**

**Do NOT put the subscription in `connectEvents`** — that runs once per handler binding (`connectEventsIfNeeded` guards `if (eventConn !== null) return`), while reconnects go through `client.connect()` directly, and the server-side subscription is connection-local (the forwarder dies with the connection; ADR-013 decision 2 mandates re-subscribe on reconnect). Instead, subscribe from the constructor's `onStateChange(true)` hook, which fires on EVERY completed handshake (first connect and every reconnect — `json-rpc-client.ts:424` calls it from `completeHandshake`).

In `apps/mirri-web/src/api/daemon/ws-rpc-api.ts`, in the `JsonRpcClient` options inside the constructor (~475), extend the existing `onStateChange`:

```typescript
      onStateChange: (connected: boolean) => {
        this.handlers?.onConnectionChange(connected);
        this.terminalSinks?.onConnectionChange?.(connected);
        if (connected) this.subscribeSkillCatalog();
      },
```

And add the private method (near `installAskHandlers`):

```typescript
  /** ADR-013 decision 2: subscribe to the app-level skill-catalog change
   *  broadcast. Connection-local like the serve wire's mcp.subscribe; runs
   *  on every completed handshake (reconnects re-subscribe — the server's
   *  subscription dies with the connection). The server's handler is
   *  idempotent for repeat subscriptions on one connection (re-ack only).
   *  Old backends answer method-not-found — the silent degradation the
   *  spec mandates; any other failure is also non-fatal (the next
   *  reconnect retries). */
  private subscribeSkillCatalog(): void {
    void this.client
      .call('skill.subscribe', {})
      .catch((error: unknown) => {
        if (
          error instanceof JsonRpcCallError &&
          error.code === -32601 /* method-not-found: old backend */
        ) {
          return;
        }
        // Transport hiccup between handshake and subscribe — the next
        // reconnect's onStateChange(true) retries; never surface this.
      });
  }
```

- [ ] **Step 3: Route the event through the any-event path**

In the same file, the any-event handler (~1206) currently drops sessionless non-workspace events:

```typescript
      if (event.sessionId === undefined) {
        if (!event.type.startsWith('event.workspace.')) return;
      } else if (event.sessionId !== '') {
        this.lastSessionId = event.sessionId;
      }
```

Change to allow the skill catalog event through (the same exception class as `event.workspace.*` — a sessionless server-scoped notification, ADR-013 decision 2):

```typescript
      if (event.sessionId === undefined) {
        const isServerScoped =
          event.type.startsWith('event.workspace.') || event.type === 'skill.catalogChanged';
        if (!isServerScoped) return;
      } else if (event.sessionId !== '') {
        this.lastSessionId = event.sessionId;
      }
```

Then, still inside the any-event handler, intercept the skill catalog event AFTER `dropParkedAskForResolution(event)` (~1216) and BEFORE `routeRpcEventToAppEvents` (~1217) — the projector's vocabulary does not know it, and the workspace-lifecycle precedent handles sessionless server-scoped frames at the bridge (ADR-013 decision 2):

```typescript
      // ADR-013 decision 2: skill.catalogChanged is projected at the bridge
      // (the workspace-lifecycle precedent — sessionless server-scoped
      // frames bypass the agent projector entirely).
      if (event.type === 'skill.catalogChanged') {
        const fields = (event.payload ?? {}) as Record<string, unknown>;
        const strings = (v: unknown): readonly string[] =>
          Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
        const workspaceId = typeof fields['workspaceId'] === 'string' ? fields['workspaceId'] : '';
        const trigger = ['file', 'config', 'plugin', 'manual'].includes(
          String(fields['trigger']),
        )
          ? (String(fields['trigger']) as 'file' | 'config' | 'plugin' | 'manual')
          : 'file';
        if (workspaceId !== '') {
          handlers.onEvent(
            {
              type: 'skillCatalogChanged',
              workspaceId,
              added: strings(fields['added']),
              removed: strings(fields['removed']),
              updated: strings(fields['updated']),
              trigger,
            },
            { sessionId: '', seq: 0 },
          );
        }
        return;
      }
```

(`JsonRpcCallError` is already imported in this file, ~116 — verify and keep.)

**ADR-013 decision 7 deviation note (recorded here, ruled at plan review)**: the plan consumes the payload with inline narrowing instead of adding a zod schema, following the workspace-lifecycle precedent for sessionless server-scoped frames (also narrowed inline at the bridge). The ADR's "按需补 zod schema" is read as: a schema is required only when a second consumer needs validation — the toast and cache-refresh paths both read the already-narrowed AppEvent.

- [ ] **Step 4: Write the tests**

In `apps/mirri-web/test/ws-rpc-event-routing.test.ts` — use the existing `createApiWithAnyHandler` harness (~355: mocked client + `connectEvents` + captured anyHandler). Add a describe block:

```typescript
describe('WsRpcApi skill catalog event routing', () => {
  it('should deliver a skill.catalogChanged notification to the UI as a skillCatalogChanged AppEvent', () => {
    const { handlers, anyHandler } = createApiWithAnyHandler();
    anyHandler({
      type: 'skill.catalogChanged',
      payload: { workspaceId: 'ws-1', added: ['alpha'], removed: [], updated: ['beta'], trigger: 'file' },
    });
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        type: 'skillCatalogChanged',
        workspaceId: 'ws-1',
        added: ['alpha'],
        updated: ['beta'],
        trigger: 'file',
      }),
      expect.anything(),
    );
  });

  it('should still update the last-session pointer for session events while routing the sessionless catalog event', () => {
    const { handlers, anyHandler } = createApiWithAnyHandler();
    anyHandler(rpcEvent('event.turn.started', { turnId: 1 }, 'sess-1', 1));
    anyHandler({ type: 'skill.catalogChanged', payload: { workspaceId: 'ws-1', trigger: 'file' } });
    expect(handlers.onEvent).toHaveBeenCalledTimes(2);
  });
});
```

(Copy the `rpcEvent` helper's usage from the neighboring workspace-routing test — it is file-local. `onEvent`'s second arg is `{ sessionId: '', seq: 0 }`.)

For the handshake subscription, add to `apps/mirri-web/src/api/daemon/json-rpc-client.test.ts` (the MockWebSocket harness — or, simpler, assert via the `ws-rpc-event-routing.test.ts` harness by mocking the mocked client's `call`): in the existing routing harness the mocked `client.call` records calls — after `connectEvents`, simulate `onStateChange(true)` and assert `client.call` was invoked with `('skill.subscribe', {})`; then make `call` reject with a `JsonRpcCallError`-shaped error (code -32601) and assert `handlers.onError` was NOT called (the silent degradation).

- [ ] **Step 5: Run the tests**

Run: `pnpm --filter @mirri-ai/mirri-web exec vitest run test/ws-rpc-event-routing.test.ts src/api/daemon/json-rpc-client.test.ts`
Expected: PASS (new + existing).

- [ ] **Step 6: Commit**

```bash
git add apps/mirri-web/src/api/daemon/ws-rpc-api.ts apps/mirri-web/src/api/types.ts apps/mirri-web/src/api/daemon/eventReducer.ts apps/mirri-web/test/ws-rpc-event-routing.test.ts apps/mirri-web/src/api/daemon/json-rpc-client.test.ts
git commit -m "feat(web): subscribe to skill.catalogChanged and project it as an AppEvent (ADR-013)"
```

---

### Task 3: Web — cache invalidation + non-blocking toast on the event

**Files:**
- Modify: `apps/mirri-web/src/composables/useMirriWebClient.ts` (the `connectEventsIfNeeded` onEvent handler ~1196-1268; the `workspaceIdForSession` helper ~2574)
- Modify: `apps/mirri-web/src/i18n/locales/zh/settings.ts` + `apps/mirri-web/src/i18n/locales/en/settings.ts` (new keys)
- Test: add to an existing composable test file if one covers `useMirriWebClient` wiring; otherwise the pure invalidation logic goes in a small exported pure function tested in the file it lives in (see Step 3).

**Interfaces:**
- Consumes: `skillCatalogChanged` AppEvent (Task 2); `modelProvider.loadSkillsForSession(sessionId)` / `modelProvider.loadSkillsForWorkspace(workspaceId)` (existing); `modelProvider.skillsBySession.value: Record<string, AppSkill[]>` / `skillsByWorkspace.value` (existing); `rawState.sessions: AppSession[]` (existing, has `.id`, `.workspaceId?`, `.cwd`); `workspaceIdForSession(s)` (existing ~2574); `pushWarning(warning: AppWarning)` (existing ~1457).
- Produces: exported pure function `skillCacheKeysToRefresh(workspaceId: string, sessions: ReadonlyArray<{id: string; workspaceId?: string; cwd: string}>, workspaces: ReadonlyArray<{id: string; root: string}>, cachedSessions: ReadonlySet<string>, cachedWorkspaces: ReadonlySet<string>): { sessions: string[]; workspace: boolean }` — the invalidation mapping (ADR-013 decision 3), testable without Vue. The `workspaces` array resolves a session's workspace by cwd (`workspaceIdForSession`'s fallback semantics).

- [ ] **Step 1: Write the failing test for the pure mapping**

Create the pure function in `apps/mirri-web/src/lib/skillsHotReload.ts` (new small file — pure logic, no Vue, the `skillsManagement.ts` precedent):

```typescript
// apps/mirri-web/src/lib/skillsHotReload.ts
// ADR-013 decision 3: the workspaceId → cache-key invalidation mapping for
// the skill-catalog change event. Pure — tested without Vue.

/** The skill caches to refetch for one skillCatalogChanged event: every
 *  cached session key belonging to the event's workspace, plus the
 *  workspace key — but only keys already populated (the UI carried skill
 *  candidates for them; never fetch for a workspace the UI never pulled). */
export function skillCacheKeysToRefresh(
  workspaceId: string,
  sessions: ReadonlyArray<{ id: string; workspaceId?: string; cwd: string }>,
  workspaces: ReadonlyArray<{ id: string; root: string }>,
  cachedSessions: ReadonlySet<string>,
  cachedWorkspaces: ReadonlySet<string>,
): { sessions: string[]; workspace: boolean } {
  const rootsById = new Map(workspaces.map((w) => [w.id, w.root] as const));
  const belongs =
    workspaceId === '' ? () : false : (s: { workspaceId?: string; cwd: string }) =>
      s.workspaceId === workspaceId ||
      (s.workspaceId === undefined && rootsById.get(workspaceId) === s.cwd);
  const sessionsToRefresh = sessions
    .filter((s) => cachedSessions.has(s.id) && belongs(s))
    .map((s) => s.id);
  return { sessions: sessionsToRefresh, workspace: cachedWorkspaces.has(workspaceId) };
}
```

Test file: `apps/mirri-web/src/lib/skillsHotReload.test.ts` (the `skillsManagement` test precedent — colocated `*.test.ts`):

```typescript
import { describe, expect, it } from 'vitest';
import { skillCacheKeysToRefresh } from './skillsHotReload';

describe('skillCacheKeysToRefresh', () => {
  it('should refresh every cached session of the event workspace and the workspace key when caches were populated', () => {
    const result = skillCacheKeysToRefresh(
      'ws-1',
      [
        { id: 's-1', workspaceId: 'ws-1' },
        { id: 's-2', cwd: '/repo' },
        { id: 's-3', workspaceId: 'ws-2' },
        { id: 's-4', workspaceId: 'ws-1' },
      ],
      [{ id: 'ws-1', root: '/repo' }],
      new Set(['s-1', 's-2', 's-3', 's-4']),
      new Set(['ws-1', 'ws-2']),
    );
    expect(result).toEqual({ sessions: ['s-1', 's-2', 's-4'], workspace: true });
  });

  it('should not fetch anything when the UI never pulled the workspace caches', () => {
    const result = skillCacheKeysToRefresh(
      'ws-9',
      [{ id: 's-1', workspaceId: 'ws-9' }],
      [{ id: 'ws-9', root: '/x' }],
      new Set<string>(),
      new Set<string>(),
    );
    expect(result).toEqual({ sessions: [], workspace: false });
  });

  it('should skip cached sessions of other workspaces', () => {
    const result = skillCacheKeysToRefresh(
      'ws-2',
      [
        { id: 's-1', workspaceId: 'ws-1' },
        { id: 's-2', workspaceId: 'ws-2' },
      ],
      [],
      new Set(['s-1', 's-2']),
      new Set(['ws-2']),
    );
    expect(result).toEqual({ sessions: ['s-2'], workspace: true });
  });
});
```

- [ ] **Step 2: Run it red then green**

Run: `pnpm --filter @mirri-ai/mirri-web exec vitest run src/lib/skillsHotReload.test.ts`
Expected: FAIL (file missing) → implement → PASS.

- [ ] **Step 3: Wire the event in the app's onEvent handler**

In `apps/mirri-web/src/composables/useMirriWebClient.ts`, inside the `connectEventsIfNeeded` `onEvent(appEvent, meta)` handler (the same place `workspaceCreated` etc. are intercepted, ~1214-1220), add the skill catalog branch BEFORE the usage-frame drop logic:

```typescript
      // ADR-013 decisions 3-4: skill-catalog change — invalidate the skill
      // caches of the event's workspace (session keys + workspace key,
      // only where populated) and surface the non-blocking toast (the
      // presentation duty of US7.2's UI channel).
      if (appEvent.type === 'skillCatalogChanged') {
        const cachedSessions = new Set(
          Object.keys(modelProvider.skillsBySession.value),
        );
        const cachedWorkspaces = new Set(
          Object.keys(modelProvider.skillsByWorkspace.value),
        );
        const { sessions, workspace } = skillCacheKeysToRefresh(
          appEvent.workspaceId,
          rawState.sessions,
          rawState.workspaces,
          cachedSessions,
          cachedWorkspaces,
        );
        for (const sid of sessions) void modelProvider.loadSkillsForSession(sid);
        if (workspace) void modelProvider.loadSkillsForWorkspace(appEvent.workspaceId);
        // US7.2 requires the summary to carry counts AND skill names —
        // cap the name list at 5 per bucket (a big install event must not
        // flood the toast; the full names are in the skills panel).
        const nameList = [...appEvent.added, ...appEvent.removed, ...appEvent.updated];
        const names = nameList.slice(0, 5).join(', ');
        const namesSuffix =
          nameList.length > 5 ? ` +${nameList.length - 5}` : nameList.length > 0 ? '' : '';
        pushWarning({
          severity: 'info',
          title: i18n.global.t('settings.skillsUpdatedTitle'),
          message:
            i18n.global.t('settings.skillsUpdatedMessage', {
              added: appEvent.added.length,
              removed: appEvent.removed.length,
              updated: appEvent.updated.length,
            }) + (names !== '' ? ` — ${names}${namesSuffix}` : ''),
        });
        return;
      }
```

Import `skillCacheKeysToRefresh` from `#/lib/skillsHotReload` following the file's existing `#/` import convention (check neighboring imports; use the same form).

- [ ] **Step 4: i18n keys**

`apps/mirri-web/src/i18n/locales/zh/settings.ts` (Skills tab section):

```typescript
  skillsUpdatedTitle: '技能已更新',
  skillsUpdatedMessage: '新增 {added} · 移除 {removed} · 变更 {updated}',
  skillsReload: '重新扫描',
  skillsReloadResult: '重扫完成：新增 {added} · 移除 {removed} · 变更 {updated}',
  skillsReloadNoChange: '重扫完成：无变化',
  skillsReloadWatchingOff: '自动感知未启用（文件监听已停用，可检查 skillHotReload 配置）',
```

`apps/mirri-web/src/i18n/locales/en/settings.ts` (same section):

```typescript
  skillsUpdatedTitle: 'Skills updated',
  skillsUpdatedMessage: '{added} added · {removed} removed · {updated} changed',
  skillsReload: 'Rescan',
  skillsReloadResult: 'Rescan finished: {added} added · {removed} removed · {updated} changed',
  skillsReloadNoChange: 'Rescan finished: no changes',
  skillsReloadWatchingOff: 'Auto refresh is off (file watching disabled; check the skillHotReload flag)',
```

(The zh/en key sets must stay in sync — the repo convention checks this.)

- [ ] **Step 5: Typecheck + tests**

Run: `pnpm --filter @mirri-ai/mirri-web run typecheck` and `pnpm --filter @mirri-ai/mirri-web exec vitest run src/lib/skillsHotReload.test.ts`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add apps/mirri-web/src/lib/skillsHotReload.ts apps/mirri-web/src/lib/skillsHotReload.test.ts apps/mirri-web/src/composables/useMirriWebClient.ts apps/mirri-web/src/i18n/locales/zh/settings.ts apps/mirri-web/src/i18n/locales/en/settings.ts
git commit -m "feat(web): refresh skill caches and toast on skill.catalogChanged (ADR-013)"
```

---

### Task 4: Web — SkillsPanel rescan button with in-place result and watcher status

**Files:**
- Modify: `apps/mirri-web/src/api/types.ts` (MirriWebApi interface — add `reloadSkills`)
- Modify: `apps/mirri-web/src/api/daemon/client.ts` (v1/v2 REST face — honest degradation)
- Modify: `apps/mirri-web/src/api/daemon/ws-rpc-api.ts` (v3 face — the RPC call, near `setSkillEnabled` ~1857)
- Modify: `apps/mirri-web/src/components/settings/SkillsPanel.vue` (toolbar button + result banner)

**Interfaces:**
- Consumes: `skill.reload` RPC — no params, returns `{ workspaces: Array<{ workspace_id: string, added: number, removed: number, updated: number, watching: boolean }> }` (snake_case wire; embedded handler `dispatch_skill_reload`, `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:5850`).
- Produces: `MirriWebApi.reloadSkills(): Promise<Array<{ workspaceId: string; added: number; removed: number; updated: number; watching: boolean }>>` (camelCase app shape; v3 only).

- [ ] **Step 1: API surface**

`apps/mirri-web/src/api/types.ts` — add next to `setSkillEnabled` (~1855), inside the v3-only management comment block:

```typescript
  /** Rescan every workspace's skill catalog through the engine's manual
   * reload RPC (ADR-013 decision 5). v3-only; older backends throw. */
  reloadSkills(): Promise<Array<{ workspaceId: string; added: number; removed: number; updated: number; watching: boolean }>>;
```

`apps/mirri-web/src/api/daemon/client.ts` — next to the existing v3-only honest degradations (search `requires the v3 (rust) backend`):

```typescript
  async reloadSkills(): Promise<Array<{ workspaceId: string; added: number; removed: number; updated: number; watching: boolean }>> {
    throw new Error('reloadSkills requires the v3 (rust) backend');
  }
```

`apps/mirri-web/src/api/daemon/ws-rpc-api.ts` — next to `setSkillEnabled` (~1857):

```typescript
  /** `skill.reload` — the manual rescan face (ADR-013 decision 5). The
   *  response's per-workspace counts and watcher status back the panel's
   *  in-place result; the button does NOT listen to the event channel for
   *  its own feedback (the RPC response is the feedback). */
  async reloadSkills(): Promise<Array<{ workspaceId: string; added: number; removed: number; updated: number; watching: boolean }>> {
    const result = await this.client.call<{
      workspaces?: Array<{
        workspace_id?: unknown;
        added?: unknown;
        removed?: unknown;
        updated?: unknown;
        watching?: unknown;
      }>;
    }>('skill.reload', {});
    return (result?.workspaces ?? []).map((w) => ({
      workspaceId: typeof w.workspace_id === 'string' ? w.workspace_id : '',
      added: typeof w.added === 'number' ? w.added : 0,
      removed: typeof w.removed === 'number' ? w.removed : 0,
      updated: typeof w.updated === 'number' ? w.updated : 0,
      watching: w.watching === true,
    }));
  }
```

- [ ] **Step 2: Panel button + result banner**

In `apps/mirri-web/src/components/settings/SkillsPanel.vue`:

Add state after the existing `togglingName` ref:

```typescript
// Manual rescan (ADR-013 decision 5) — in-place result + watcher status.
const rescanning = ref(false);
const rescanResult = ref<
  | { kind: 'changed'; added: number; removed: number; updated: number; watchingOff: boolean }
  | { kind: 'nochange'; watchingOff: boolean }
  | null
>(null);

async function onRescan(): Promise<void> {
  rescanning.value = true;
  rescanResult.value = null;
  error.value = '';
  try {
    const rows = await api.reloadSkills();
    const total = rows.reduce(
      (acc, r) => ({
        added: acc.added + r.added,
        removed: acc.removed + r.removed,
        updated: acc.updated + r.updated,
      }),
      { added: 0, removed: 0, updated: 0 },
    );
    const watchingOff = rows.length > 0 && rows.every((r) => !r.watching);
    rescanResult.value =
      total.added + total.removed + total.updated === 0
        ? { kind: 'nochange', watchingOff }
        : { kind: 'changed', ...total, watchingOff };
    // The panel's own list follows the rescan directly (the same lazy-load
    // face the auto-refresh path uses — but here unconditionally, the user
    // asked for it).
    await load();
    void refreshSkills();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    rescanning.value = false;
  }
}
```

(`refreshSkills` is already destructured from `useMirriWebClient()` at the top of the script — check; it refreshes the composer `/` menu caches.)

**Event-driven self-refresh (ADR-013 decision 3, last sentence)** — the panel holds its own list (it does NOT ride the composable caches), so it must listen for the catalog event itself. The panel's `useMirriWebClient()` destructure must also pull `onAppEvent` — check whether the composable already exposes a subscription hook (grep `subscribeAppEvent\|onAppEvent` in `useMirriWebClient.ts`); if none exists, wire it via the existing pattern: `useMirriWebClient()` returns the client object whose `connectEventsIfNeeded` pipeline is private — so instead, register a lightweight watcher in `SkillsPanel.vue` against the composable's exposed `skillCatalogEvents` — **if and only if** the composable exposes the raw event stream; otherwise (the likely case) add a small `watch`-able ref `lastSkillCatalogChange = ref<SkillCatalogChangedAppEvent | null>(null)` updated in the Task 3 onEvent intercept, and in `SkillsPanel.vue`:

```typescript
// ADR-013 decision 3: the panel self-fetches, so the auto-refresh path must
// reach it too — watch the last catalog change and reload when mounted.
const { /* existing destructure */ lastSkillCatalogChange } = useMirriWebClient();
watch(lastSkillCatalogChange, (e) => {
  if (e === null) return;
  const wid = activeWorkspaceId.value;
  if (wid === null || e.workspaceId === wid) void load();
});
```

In Task 3, add `lastSkillCatalogChange` (a `ref` holding the latest `skillCatalogChanged` event, set in the onEvent intercept right after the cache refresh loop) and export it from the composable's return object (next to `skills`/`refreshSkills`).

**Toolbar copy disambiguation (ADR-013 decision 5)** — the existing Refresh button (re-fetch only) must not read as a rescan twin. Change its label binding from `t('settings.skillsRefresh')` to a new key `settings.skillsRefetch` (zh `重新拉取` / en `Re-fetch list`), keeping `skillsRefresh` unused elsewhere (grep first — if other surfaces consume `skillsRefresh`, leave the key and only change the panel's binding).

Template — toolbar (after the existing Refresh button):

```html
      <Button
        v-if="isV3"
        variant="secondary"
        size="sm"
        :disabled="rescanning"
        @click="onRescan"
      >{{ rescanning ? t('settings.skillsReloading') : t('settings.skillsReload') }}</Button>
```

Add the `skillsReloading` i18n key too (zh `重扫中…` / en `Rescanning…`), and the `skillsRefetch` key (zh `重新拉取` / en `Re-fetch list`) — then change the existing Refresh button's binding from `t('settings.skillsRefresh')` to `t('settings.skillsRefetch')` (`skillsRefresh` is consumed only by this panel — verified by grep; leave the old key in place for any future consumer).

Result banner (after the `!isV3` Banner):

```html
    <Banner v-if="rescanResult?.kind === 'changed'" variant="info">
      {{ t('settings.skillsReloadResult', {
        added: rescanResult.added,
        removed: rescanResult.removed,
        updated: rescanResult.updated,
      }) }}
      <template v-if="rescanResult.watchingOff"> · {{ t('settings.skillsReloadWatchingOff') }}</template>
    </Banner>
    <Banner v-else-if="rescanResult?.kind === 'nochange'" variant="info">
      {{ t('settings.skillsReloadNoChange') }}
      <template v-if="rescanResult.watchingOff"> · {{ t('settings.skillsReloadWatchingOff') }}</template>
    </Banner>
```

Check `Banner.vue`'s slot support (it takes default slot text — the `skillsV3Only` usage above is the precedent).

- [ ] **Step 3: Typecheck**

Run: `pnpm --filter @mirri-ai/mirri-web run typecheck`
Expected: PASS (the panel wiring is template-level; the repo has no @vue/test-utils — template correctness rides on vue-tsc, the ADR-012 plan's precedent).

- [ ] **Step 4: Commit**

```bash
git add apps/mirri-web/src/api/types.ts apps/mirri-web/src/api/daemon/client.ts apps/mirri-web/src/api/daemon/ws-rpc-api.ts apps/mirri-web/src/components/settings/SkillsPanel.vue apps/mirri-web/src/i18n/locales/zh/settings.ts apps/mirri-web/src/i18n/locales/en/settings.ts
git commit -m "feat(web): skills panel rescan button with in-place result and watcher status (ADR-013)"
```

---

### Task 5: ACP — project `skill.catalogChanged` as a session notification

**Files:**
- Modify: `rust/crates/mirri-acp/src/acp_handler.rs` (the `EventTranslator::translate` match ~2780-2790)
- Modify: `rust/crates/mirri-remote/src/lib.rs` (`handle_notification` ~1048-1062; post-handshake setup for the `skill.subscribe` call)
- Test: `rust/crates/mirri-acp/src/acp_handler.rs` (the `#[cfg(test)]` mod at the bottom, near `should_translate_assistant_delta_into_agent_message_chunk_update` ~3420) + the mirri-remote notification-forwarding test module

**Interfaces:**
- Consumes: the engine's `skill.catalogChanged` broadcast arrives on the ACP engine-adapter event stream as a direct-form notification (method `skill.catalogChanged`, camelCase params `{workspaceId, added, removed, updated, trigger}`). **Two adapter modes, two paths**:
  - *Embedded mode* (in-process engine): the embedded server's per-subscriber forwarder already pushes `skill.catalogChanged` directly onto every `subscribe_events` receiver (`rust/crates/mirri-embedded/src/embedded_server/mod.rs:5616-5644`) — no extra subscription needed.
  - *Remote mode* (`RemoteAdapter`, the desktop-server probe path): the server only pushes to connections that called `skill.subscribe`, and `mirri-remote`'s `handle_notification` drops non-`event.*` methods (`rust/crates/mirri-remote/src/lib.rs:1048-1062`). Both must be extended (Step 2b).
- Produces: an ACP `session/update` notification with `{"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "<summary line>"}}` (ADR-013 decision 6: the host's own notification surface renders it; a text chunk is the established non-interactive carrier).

- [ ] **Step 1: Write the failing test**

Append to the `#[cfg(test)]` mod (copy the `engine_event` helper pattern from `should_translate_thinking_delta_into_agent_message_chunk_update`):

```rust
    /// Given a skill-catalog change notification with no session id, when
    /// an ACP session is active, then the change is translated into an
    /// agent_message_chunk update addressed to the active session (the
    /// host's notification surface renders it; ADR-013 decision 6). With
    /// no active session the event drops (the translator's existing
    /// sessionless semantics — documented, not a silent failure).
    #[tokio::test]
    async fn should_translate_skill_catalog_changed_into_an_agent_message_chunk() {
        // Given the skill-catalog change notification (direct form, camelCase)
        let notification = JsonRpcNotification {
            method: "skill.catalogChanged".to_string(),
            params: json!({
                "workspaceId": "ws-1",
                "added": ["alpha"],
                "removed": [],
                "updated": ["beta"],
                "trigger": "file",
            }),
        };

        // When translated with an active session
        let mut translator = EventTranslator::default();
        translator.set_active_session("session-1");
        let update = translator.translate(&notification);

        // Then it becomes an agent_message_chunk carrying the summary line
        let update = update.expect("the catalog change must reach the host");
        assert_eq!(update.method, "session/update");
        assert_eq!(update.params["sessionId"], "session-1");
        assert_eq!(update.params["update"]["sessionUpdate"], "agent_message_chunk");
        let text = update.params["update"]["content"]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("1 added") && text.contains("1 changed"), "summary carries counts: {text}");
    }
```

Note: `EventTranslator::translate` currently requires `params.get("sessionId")` (line ~2783: `let session_id = params.get("sessionId").and_then(Value::as_str)?;`) — every event without one returns `None`. The `set_active_session` mechanism does not exist yet; Step 2 adds it. (Alternative rejected: wiring the skill broadcast into the handler's own task — the translator is where every engine event already flows, and the active-session pointer is the ACP-established way to address sessionless frames; the reverse-RPC bridge already uses `active_prompt_sessions` for exactly this.)

- [ ] **Step 2: Implement**

In `EventTranslator` (struct at ~2741), add:

```rust
struct EventTranslator {
    tool_calls: HashMap<String, TrackedToolCall>,
    /// The session a sessionless server-scoped event addresses (the
    /// skill-catalog change has no session id — ADR-013 decision 6 routes
    /// it to the active session the way the reverse-RPC bridge uses the
    /// active prompt session; `None` drops the event, the documented
    /// sessionless semantics).
    active_session: Option<String>,
}
```

Add the setter (and use it where the forwarder task is spawned — `ensure_update_forwarder` ~2041 — the handler's `active_prompt_sessions` last entry is the active session; read it there once per event, NOT once per wiring):

```rust
    fn set_active_session(&mut self, session_id: &str) {
        self.active_session = Some(session_id.to_string());
    }
```

Two changes in `translate` (top of the function):

**(a) Prefix tolerance** — the skill event arrives in DIRECT form (method `skill.catalogChanged`, no `event.` prefix) while every other engine event arrives `event.`-prefixed. Make the strip tolerant (~2781):

```rust
        let event_type = notification
            .method
            .strip_prefix("event.")
            .unwrap_or(notification.method.as_str());
```

(The embedded event stream carries only `event.*` methods plus the direct-form `skill.catalogChanged` — the tolerance adds no other pass-through. The remote stream's non-`event.*` drops happen earlier, in `mirri-remote`'s `handle_notification` — see Step 2b.)

**(b) Session extraction with the active-session fallback, debug-logged when dropped** (replaces the current hard `params.get("sessionId").and_then(Value::as_str)?`):

```rust
        let session_id = match params.get("sessionId").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => match self.active_session.as_deref() {
                // Only the skill-catalog event may ride the active-session
                // fallback (ADR-013 decision 6) — every other sessionless
                // event keeps the drop semantics, debug-logged per the
                // ADR's "丢弃并记 debug 日志" requirement.
                Some(id) if event_type == "skill.catalogChanged" => id.to_string(),
                _ => {
                    tracing::debug!(
                        method = %notification.method,
                        "dropping sessionless event with no active session"
                    );
                    return None;
                }
            },
        };
```

Add the match arm (before the `_ => return None`):

```rust
            // The app-level skill-catalog change (ADR-013 decision 6): the
            // host's notification surface renders the summary; an
            // agent_message_chunk is the established non-interactive text
            // carrier (no ACP update vocabulary exists for catalog state).
            "skill.catalogChanged" => {
                let count = |key: &str| {
                    params
                        .get(key)
                        .and_then(Value::as_array)
                        .map_or(0, std::vec::Vec::len)
                };
                let text = format!(
                    "Skills updated: {} added, {} removed, {} changed",
                    count("added"),
                    count("removed"),
                    count("updated")
                );
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
```

(The engine's diff gate never broadcasts empty diffs — the all-zero defensive branch is unnecessary; the format is honest if one ever arrives.)

**Wire the active-session pointer** — in `ensure_update_forwarder`'s spawned loop (~2041), before `translator.translate(&notification)`, read the active prompt session. This requires `active_prompt_sessions: Arc<Mutex<Vec<String>>>` to be cloned into the forwarder task (it is already cloned into the reverse-RPC bridge closure at ~2063; add the same clone for the forwarder):

```rust
                let active = active_prompts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last()
                    .cloned();
                if let Some(id) = active {
                    translator.set_active_session(&id);
                } else {
                    translator.clear_active_session();
                }
```

(Add the `clear_active_session` method setting `active_session = None`.)

**Add a drop-semantics test** (the no-active-session negative the plan asserted only in comments):

```rust
    /// Given a skill-catalog change with NO active session, when
    /// translated, then it drops (None) — the documented sessionless
    /// semantics, not a silent failure.
    #[tokio::test]
    async fn should_drop_the_skill_catalog_change_when_no_session_is_active() {
        let notification = JsonRpcNotification {
            method: "skill.catalogChanged".to_string(),
            params: json!({
                "workspaceId": "ws-1",
                "added": ["alpha"],
                "removed": [],
                "updated": [],
                "trigger": "file",
            }),
        };
        let update = EventTranslator::default().translate(&notification);
        assert!(update.is_none(), "no active session must drop the event");
    }
```

- [ ] **Step 2b: Remote mode — subscribe + forward in `mirri-remote`**

Two additions so the remote ACP host receives the event:

1. **Forward**: in `rust/crates/mirri-remote/src/lib.rs` `handle_notification` (~1048-1062), extend the event-forward arm:

```rust
        method if method == "event"
            || method.starts_with("event.")
            || method == "skill.catalogChanged" =>
        {
```

2. **Subscribe**: the adapter must call `skill.subscribe` after the handshake (the server pushes only to subscribed connections — `rust/crates/mirri-server/src/state.rs:322-375`). Find where the adapter's post-handshake setup runs (the `client_hello` send and its response handling; the same place `session.list` mass-subscribe happens for `SubscribePolicy::AllSessions`) and add a best-effort call after the handshake completes:

```rust
        // ADR-013 decision 6 (remote mode): the app-level skill-catalog
        // broadcast is connection-local on the serve wire — subscribe once
        // per connection. Old servers answer method-not-found; best-effort
        // only (the embedded mode needs none of this).
        let _ = self
            .call_raw("skill.subscribe", serde_json::json!({}))
            .await;
```

(Adapt `call_raw` to the adapter's internal request primitive — grep how `session.subscribe` is sent inside `mirri-remote/src/lib.rs` and mirror that call; a failure is logged at debug and swallowed.) Also add the `subscribe_skill_catalog_events` override note: the ACP handler consumes events via `subscribe_events`, so the RemoteAdapter needs no separate seam — the forwarded notification lands on the same event stream the ACP forwarder already drains. Add a test in `rust/crates/mirri-remote` mirroring an existing notification-forwarding test: a `skill.catalogChanged` notification frame received on the socket lands on the `subscribe_events` receiver.

- [ ] **Step 3: Run the tests**

Run (background, ≥20 min): `cargo test --manifest-path rust/Cargo.toml -p mirri-acp -p mirri-remote`
Expected: PASS — the new tests plus the whole translator suite (the `event.` prefix tolerance change must not break the existing `translate` tests; they all use `event.`-prefixed methods and the direct form only adds the pass-through).

- [ ] **Step 4: Commit**

```bash
git add rust/crates/mirri-acp/src/acp_handler.rs rust/crates/mirri-remote/src/lib.rs
git commit -m "feat(acp): project skill.catalogChanged as an agent_message_chunk session update (ADR-013)"
```

---

### Task 6: Changeset + final verification

**Files:**
- Create: `.changeset/<generated>.md` (via the gen-changesets skill rules — anchor `@mirri-ai/release`, minor bump, English changelog entry)
- Modify: `docs/specs/skills-ROADMAP.md` (G11 → mark implemented with the branch/commit pointer)

- [ ] **Step 1: Changeset**

Run the `gen-changesets` skill flow; the change is user-perceivable (skills hot-reload now works by default in web/desktop without restart): `minor` against `@mirri-ai/release`. Entry covers: skill hot-reload on by default; web/desktop auto-refresh skill lists + toast on catalog changes; skills settings rescan button; ACP hosts see skill-change notifications.

- [ ] **Step 2: ROADMAP G11 status**

Update the G11 row's 备注 column to note the implementation landed (branch + short hash), following the G9/G10 precedent phrasing.

- [ ] **Step 3: Full verification**

- `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded -p mirri-acp` (background, ≥20 min timeout)
- `cargo clippy --manifest-path rust/Cargo.toml -p mirri-embedded -p mirri-acp --all-targets -- -D warnings` (background)
- `pnpm --filter @mirri-ai/mirri-web run typecheck`
- `pnpm --filter @mirri-ai/mirri-web run test` (or the maintained subset first: `vitest run src/lib/skillsHotReload.test.ts src/api/daemon/ws-rpc-parts.test.ts`)

- [ ] **Step 4: Commit**

```bash
git add .changeset docs/specs/skills-ROADMAP.md
git commit -m "chore: changeset and roadmap for skill hot-reload UI loop"
```

---

## Parallelization

- Task 1 (rust embedded) is independent of Tasks 2-4 (web) and Task 5 (rust acp) — all three cut across disjoint files and can run as parallel sub-agents.
- Task 2 must precede Task 3 (the AppEvent type and the bridge routing are Task 3's input).
- Task 4 depends on nothing from Tasks 2-3 except the i18n keys created in Task 3 Step 4 (`skillsReload*`) — either run Task 4 after Task 3, or have Task 4 create the keys and Task 3 skip them. Recommended order: Task 1 ∥ (Task 2 → Task 3) ∥ Task 5, then Task 4, then Task 6.

## Self-Review Record (post plan-review revision)

- **Spec coverage:** ADR-013 decision 1 (default on) → Task 1; decision 2 (subscribe + bridge routing + AppEvent) → Task 2 (handshake-hook subscription survives reconnects; reducer exhaustiveness handled); decision 3 (cache invalidation scope, no-fetch-when-cold, panel self-refresh) → Task 3 (pure function + wiring) + Task 4 (panel watcher on `lastSkillCatalogChange`); decision 4 (non-blocking toast, counts AND skill names) → Task 3 Step 3-4; decision 5 (rescan button, in-place result, watcher status, RPC-response-not-event feedback, refresh/rescan copy disambiguation) → Task 4; decision 6 (ACP projection, embedded + remote modes, debug-log drop) → Task 5 (+ Step 2b remote subscribe/forward); decision 7 (zod按需) — inline narrowing with the workspace-lifecycle precedent, deviation recorded in Task 2. US7.2 acceptance: default-on → Task 1; 不刷新页面/不切换会话/不重启 → Task 3 event-driven invalidation; 未承载候选不发请求 → Task 3 pure function's populated-only gate; 重扫就地结果 + 监听失效声明 → Task 4 (watchingOff banner); 非阻断提示 → Task 3 toast (info severity, names capped at 5).
- **Placeholder scan:** no TBD/TODO; every code step carries the actual code; instructions that reference neighboring tests/helpers name them precisely (`test_server(None)` at ~26456, `createApiWithAnyHandler` at ~355, `rpcEvent` helper, the mirri-remote notification-forwarding test module).
- **Type consistency:** AppEvent `skillCatalogChanged` shape (Task 2) matches Task 3's consumer (`appEvent.workspaceId/added/removed/updated`); `reloadSkills()` return shape identical in types.ts / client.ts / ws-rpc-api.ts / SkillsPanel usage; `skillCacheKeysToRefresh` 5-param signature matches its test and call site; i18n keys `skillsUpdatedTitle/skillsUpdatedMessage/skillsReload/skillsReloading/skillsReloadResult/skillsReloadNoChange/skillsReloadWatchingOff/skillsRefetch` all created in Task 3/4 with zh+en pairs.
- **Plan-review findings addressed:** reconnect subscription loss (major 1) → onStateChange hook; reducer exhaustiveness (major 2) → eventReducer no-op case; wrong test file (major 3) → ws-rpc-event-routing.test.ts; Task 1 test misread (major 4) → assertion-rewrite instructions; panel event refresh (major 5) → lastSkillCatalogChange watcher; ACP remote mode (major 6) → Step 2b (RemoteAdapter subscribe + forward); refresh-button rename (minor 7) → skillsRefetch key; toast skill names (minor 8) → name list capped at 5; ACP debug log (minor 9) → translate drop branch; signature mismatch (minor 10) → 5-param Interfaces; METHOD_NOT_FOUND export (minor 11) → literal -32601 with comment; default-flip test blast radius (minor 12) → timing-audit step + explicit-false pattern; zod deviation (minor 13) → recorded deviation note; accessor/test-helper nits (14, 15, 16) → fixed inline.
