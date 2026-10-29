# ACP Desktop Runtime Sharing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ACP host probes for a live local desktop server at startup and connects as a remote client when found (falling back to the embedded engine otherwise), so IDE-driven sessions run inside the desktop engine and project live into the desktop UI.

**Architecture:** Probe layer in `mirri-acp` (state file → pid+healthz → token → `RemoteAdapter::connect` with a new subscribe policy); the AcpHandler's engine becomes a replaceable slot with touch-to-subscribe on the adapter; unregistered cwd is rejected via a workspace-list translation; list filtering carries US11.4 pinning in remote mode.

**Tech Stack:** Rust (tokio, axum client via reqwest or raw hyper — probe uses plain HTTP GET), `mirri-remote` `RemoteAdapter`, `mirri-acp` handler/transport.

**Design doc:** `docs/specs/rust-core-design/14-acp-desktop-runtime-sharing.md` (rev 4). All ADRs D1–D6a are decided — this plan implements them, it does not re-adjudicate.

## Global Constraints

- Rust workspace only: all code in `rust/crates/*`; no TS changes.
- Given-When-Then test naming (`should ... when ...` style used throughout the repo).
- `cargo test --manifest-path rust/Cargo.toml --workspace` must stay green (run affected crate only during tasks; full suite in Task 9).
- No `git push` / PR creation (local commits only, per delivery skill).
- Every new env var: `MIRRI_ACP_ENGINE`, reuse of `MIRRI_ACP_WORKSPACE_DIR`, existing `MIRRI_ACP_REMOTE_ENGINE`/`MIRRI_ACP_REMOTE_TOKEN` — exact spellings from the design doc.
- Wire methods referenced: `session.subscribe`, `session.unsubscribe`, `session.list`, `session.create`, `workspace.list` — exact server spellings.
- The probe must not scan ports; the only anchor is `<home>/server/desktop-state.json` (D1).
- `server_hello.version` gate: minimum = current major version of the server semver, extracted from the handshake params (D1; threshold constant defined in Task 2).

---

### Task 1: RemoteAdapter subscribe policy (SubscribePolicy::OwnSessionsOnly)

**Files:**
- Modify: `rust/crates/mirri-remote/src/lib.rs` (connect signature + auto-subscribe block, ~lines 185-346)
- Test: `rust/crates/mirri-remote/tests/remote_adapter_tests.rs`

**Interfaces:**
- Produces: `pub enum SubscribePolicy { AllSessions, OwnSessionsOnly }` and `RemoteAdapter::connect_with_policy(url: &str, auth_token: Option<&str>, reverse_rpc_allowlist: &[&str], policy: SubscribePolicy) -> Result<Arc<RemoteAdapter>, EngineError>`. Existing `connect(...)` becomes a thin wrapper passing `SubscribePolicy::AllSessions` (zero break for the 2 existing call sites: `mirri-acp/src/run.rs:76`, `mirri-node/src/napi_exports.rs:198`).
- Semantics (design D5): `OwnSessionsOnly` skips the connect-time `session.list` + per-session `session.subscribe` block entirely. Session-creating calls (`session.create`/`session.resume`/`session.fork`) still auto-subscribe before completing (existing `SESSION_CREATING_METHODS` mechanism unchanged).

- [ ] **Step 1: Write the failing test**

In `remote_adapter_tests.rs` — the existing harness is `TestServer::start(...)` and has neither session pre-seeding nor a subscribe spy, so this task **extends the harness first** (not a bare reference): add to the test module (a) `TestServer::with_sessions(&[&str])` — pre-seeds `session.list` responses with the given ids; (b) a `subscribe_calls: Mutex<Vec<String>>` recorder on TestServer fed by its `session.subscribe` handler; (c) the HTTP layer unchanged (WS fixtures only). These are test-local helpers owned by this task; Tasks 3/8's ACP-side fakes are separate (ProbeEngine captures in mirri-acp tests).

```rust
#[tokio::test]
async fn should_not_subscribe_any_existing_session_when_connected_with_own_sessions_only_policy() {
    // Given a fake server with two already-persisted sessions and a spy
    // recording every `session.subscribe` call
    let (_server, url, token) = fake_server_with_sessions(&["sess-a", "sess-b"]).await;

    // When an adapter connects with the OwnSessionsOnly policy
    let adapter = RemoteAdapter::connect_with_policy(
        &url, Some(&token), &["approval"], SubscribePolicy::OwnSessionsOnly,
    ).await.expect("connect");

    // Then no session.subscribe call was recorded during the handshake
    assert!(server_subscribe_calls().await.is_empty(),
        "OwnSessionsOnly must not subscribe to existing sessions at connect time");

    // And a session-creating call through the adapter still subscribes (create → subscribe-before-complete)
    let _ = adapter.call_method("session.create", serde_json::json!({})).await;
    assert_eq!(server_subscribe_calls().await.len(), 1,
        "session.create must still auto-subscribe under OwnSessionsOnly");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-remote --test remote_adapter_tests should_not_subscribe`
Expected: FAIL — `connect_with_policy` / `SubscribePolicy` not found.

- [ ] **Step 3: Implement**

In `lib.rs`: add the `SubscribePolicy` enum (doc-comment citing design 14 §D5); refactor `connect` body into an internal `connect_inner(... , policy)`; the auto-subscribe block (`Ok(response) if response.error.is_none()` branch calling `send_subscribe` per listed session) is wrapped in `if policy == SubscribePolicy::AllSessions { ... }`; public `connect` delegates with `AllSessions`; new `connect_with_policy` exposes the policy. No other behavior change.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-remote --test remote_adapter_tests should_not_subscribe`
Expected: PASS. Also run the full remote adapter suite:
Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-remote`
Expected: all PASS (existing `connect` callers unchanged semantics).

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-remote/src/lib.rs rust/crates/mirri-remote/tests/remote_adapter_tests.rs
git commit -m "feat(remote): SubscribePolicy::OwnSessionsOnly skips connect-time mass subscribe (design 14 D5)"
```

---

### Task 2: Desktop probe layer in mirri-acp

**Files:**
- Create: `rust/crates/mirri-acp/src/desktop_probe.rs`
- Modify: `rust/crates/mirri-acp/src/lib.rs` (add `pub mod desktop_probe;`)
- Test: `rust/crates/mirri-acp/src/desktop_probe.rs` (unit tests in-file, following repo convention of in-module `#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing from Task 1 (pure probe).
- Produces:
  - `pub struct DesktopServerEndpoint { pub url: String, pub token: String }`
  - `pub async fn probe_desktop_server() -> Option<DesktopServerEndpoint>` — resolves the Mirri home (`mirri_core::config::mirri_home_dir()`; `None` on any failure), reads `<home>/server/desktop-state.json` (`{pid, port}` JSON, tolerant parse), checks pid liveness, then GET `http://127.0.0.1:{port}/api/v1/healthz` expecting HTTP 200 with `{"ok":true}` body, then reads `<home>/server.token`. Returns `None` on ANY failure (missing file, dead pid, refused port, non-200, missing/empty token) — no port scanning ever.
  - **Pid liveness (decided, D1)**: the rust workspace has no existing pid-liveness helper (verified — desktop's TS-side check does not port). Use the `sysinfo` crate if it is already in the workspace dependency graph (check `rust/Cargo.lock` first); otherwise use `libc::kill(pid, 0)` — `libc` is already a transitive dependency of several workspace crates; add it to `mirri-acp`'s Cargo.toml as a direct dep. Either path: return `false` on ESRCH, `true` on success, `false` on EPERM (a process we can't signal is not ours).
  - **Version gate (decided, D1)**: `pub fn version_satisfies(server_version: &str, min: &str) -> bool` — semver-major compare, unparseable → `false` (fail closed to embedded). The `min` value is `pub const MIN_SERVER_VERSION: &str = "2.0.0"` — a literal in `desktop_probe.rs`, bumped in the same commit whenever a wire change lands that the ACP host cannot tolerate. (A build-time cross-crate version macro was considered and rejected: mirri-acp cannot read mirri-server's CARGO_PKG_VERSION, and "same major as the CLI" drifts from "protocol features the host actually uses" — the literal is the honest floor.) The gate is enforced where the handshake completes (Task 4) via the `RemoteCaps.version` the adapter captured.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_unsatisfied_when_the_server_version_is_unparseable() {
        // Given a server hello version that is not semver
        // When the gate is evaluated
        // Then it fails closed (probe must fall back to embedded)
        assert!(!version_satisfies("not-semver", "1.0.0"));
    }

    #[test]
    fn should_report_satisfied_when_the_server_major_meets_the_minimum() {
        assert!(version_satisfies("1.4.2", "1.0.0"));
        assert!(!version_satisfies("0.9.9", "1.0.0"));
    }

    #[tokio::test]
    async fn should_return_none_when_the_desktop_state_file_is_missing() {
        // Given a temp home with no server/desktop-state.json
        // (set MIRRICODE_HOME to the tempdir for the test)
        // When probing
        // Then the result is None with no side effects (no port contact)
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MIRRICODE_HOME", home.path());
        let endpoint = probe_desktop_server().await;
        assert!(endpoint.is_none());
        std::env::remove_var("MIRRICODE_HOME");
    }
}
```

Note: env-var mutation in async tests can race — run these serially or use a serial-test guard; check whether `mirri-acp` tests already set `MIRRICODE_HOME` and follow their pattern.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-acp desktop_probe`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement** the probe as specified. HTTP client: check `rust/crates` for an existing lightweight HTTP client dependency used by `mirri-acp` or siblings (e.g. `reqwest` in any crate's Cargo.toml); reuse whatever exists; if none, add `reqwest` with `default-features = false, features = ["json"]` to `mirri-acp`'s Cargo.toml (matching the workspace's dependency style).

- [ ] **Step 4: Run to verify pass** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp desktop_probe`

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-acp/src/desktop_probe.rs rust/crates/mirri-acp/src/lib.rs rust/crates/mirri-acp/Cargo.toml
git commit -m "feat(acp): desktop server probe — state file, pid liveness, healthz, token (design 14 D1)"
```

---

### Task 3: touch-to-subscribe + unsubscribe in RemoteAdapter

**Files:**
- Modify: `rust/crates/mirri-remote/src/lib.rs` (call path + trigger set + unsubscribe method)
- Test: `rust/crates/mirri-remote/tests/remote_adapter_tests.rs`

**Interfaces:**
- Consumes: Task 1's `SubscribePolicy`.
- Produces:
  - Trigger-set change: `SESSION_CREATING_METHODS` gains `"forkSession"` (design D5 — the ACP host's actual fork call).
  - `call_method` touch-to-subscribe: when policy is `OwnSessionsOnly` AND the method is session-addressed (params carry a non-empty `session_id` or `sessionId`) AND the session is not in `subscribed_sessions`, the adapter sends `session.subscribe` (no cursor) before the original call — the subscribe result is awaited with the call's own timeout, and a subscribe *failure* is logged and swallowed (the original call proceeds; server answers not-found if the session truly doesn't exist — the subscribe must not turn a not-found into a transport error).
  - `pub async fn unsubscribe(&self, session_id: &str) -> Result<(), EngineError>` — sends `session.unsubscribe`, removes from `subscribed_sessions`, best-effort (errors surface to caller but don't poison the adapter).

- [ ] **Step 1: Failing tests** (fake-server harness, follow Task 1's fixture):

```rust
#[tokio::test]
async fn should_subscribe_on_first_touch_of_an_unsubscribed_session_under_own_sessions_only() {
    // Given an OwnSessionsOnly adapter and a persisted session "sess-old"
    // (never created through this adapter) that the fake server knows
    // When a session-addressed call touches it (e.g. session.get)
    // Then session.subscribe for sess-old is recorded BEFORE the call is served
    // And a second session-addressed call does not subscribe again
}

#[tokio::test]
async fn should_not_subscribe_on_touch_when_the_call_has_no_session_id() {
    // Given the same adapter
    // When a non-session-addressed call runs (e.g. session.list)
    // Then no subscribe is recorded
}

#[tokio::test]
async fn should_send_unsubscribe_and_clear_tracking_when_unsubscribe_is_called() {
    // Given an adapter subscribed to sess-a (via create)
    // When unsubscribe("sess-a") is called
    // Then the server sees session.unsubscribe with sessionId sess-a
    // And a subsequent session-addressed call to sess-a re-subscribes first
}

#[tokio::test]
async fn should_auto_subscribe_a_forked_session_created_via_forkSession_method() {
    // Given the adapter with a session
    // When call_method("forkSession", {session_id: "..."}) completes
    // Then the returned/new session id is subscribed before the response is delivered
}

#[tokio::test]
async fn should_hand_over_the_approval_route_when_a_second_subscriber_subscribes_the_same_session() {
    // Design 14 D5 / acceptance 8: the server-side "last subscriber wins" routing
    // Given adapter A (OwnSessionsOnly) subscribed to sess-1 via session.create,
    //       and adapter B connects and subscribes sess-1
    // When the engine-side approval.ask fires for sess-1 (fake server script:
    //       reverse RPC request issued after B's subscribe lands)
    // Then the ask routes to adapter B's reverse-RPC channel, and adapter A's
    //       reverse-RPC channel receives nothing (no double-ask)
    // And B answering it completes the approval round trip
}
```

This last test pins the server-side routing behavior the design relies on (`ws_rpc.rs` "last subscriber wins"): it must NOT regress when ACP hosts stop mass-subscribing.

- [ ] **Step 2: Verify failure** — `cargo test --manifest-path rust/Cargo.toml -p mirri-remote --test remote_adapter_tests touch_`
Expected: FAIL (methods/behavior absent).

- [ ] **Step 3: Implement** — in `call_method`'s dispatch path (where `SESSION_CREATING_METHODS` already triggers pre-completion subscribe), add the touch-to-subscribe branch; add `"forkSession"` to the trigger set; add `unsubscribe`. The `subscribed_sessions` set is the existing `Mutex<HashSet<String>>` — reuse it.

- [ ] **Step 4: Verify pass** — full `cargo test -p mirri-remote`.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-remote/src/lib.rs rust/crates/mirri-remote/tests/remote_adapter_tests.rs
git commit -m "feat(remote): touch-to-subscribe, forkSession trigger, unsubscribe API (design 14 D5)"
```

---

### Task 4: Engine slot + probe/re-probe/degrade wiring in run.rs

**Files:**
- Modify: `rust/crates/mirri-acp/src/run.rs` (engine resolution)
- Modify: `rust/crates/mirri-acp/src/acp_handler.rs` (replaceable engine slot)
- Test: `rust/crates/mirri-acp/src/run.rs` (in-module tests where feasible) + `rust/crates/mirri-acp/tests/` (new integration test file `desktop_share_tests.rs` if the fake-server harness needs to live at integration level — check where existing ACP integration tests with engines live and follow)

**Interfaces:**
- Consumes: Task 1 `connect_with_policy` + `SubscribePolicy`, Task 2 `probe_desktop_server`/`version_satisfies`, Task 3 `unsubscribe`.
- Produces:
  - In `acp_handler.rs`: `AcpHandler::replace_engine(&self, new: Arc<dyn EngineAdapter>)` — swaps the default-engine slot (interior-mutable `RwLock<Arc<dyn EngineAdapter>>` around the existing `engine` field), clears `session_engines` + `cwd_engines` + `engine_cwds` route caches, clears `wired_engines` (so `ensure_wired` re-runs on the new engine). All existing methods that read `self.engine` go through a `current_engine()` accessor. **Remote-mode handler has no factory** (existing `with_engine` constructor already is single-engine — keep that shape; the slot is the default-engine field).
  - In `run.rs`: the resolution chain (design §2):
    1. `MIRRI_ACP_REMOTE_ENGINE` set → existing forced-remote path (token: `MIRRI_ACP_REMOTE_TOKEN` else server.token file), `connect` (AllSessions policy — preserved behavior, byte-compatible).
    2. `MIRRI_ACP_ENGINE=embedded` → embedded path.
    3. `MIRRI_ACP_WORKSPACE_DIR` set → embedded path (test semaphore).
    4. Else → `probe_desktop_server()`: `Some(endpoint)` → `connect_with_policy(&endpoint.url, Some(&endpoint.token), &["approval","question"], OwnSessionsOnly)`; handshake `version` gate via `version_satisfies` (fail → embedded); connect/subscribe failure → embedded. `None` → embedded.
  - Connection-death re-probe: a `ConnectionDeathResponse` enum outcome `{ Reconnected(Arc<RemoteAdapter>), DegradedToEmbedded(Arc<dyn EngineAdapter>), StillDead }` produced by `handle_connection_death()` — wired where the RemoteAdapter surfaces transport errors. **Placement note for the implementer:** the AcpHandler today calls `engine.call_method` per request; the death signal arrives as `EngineError::Transport`/`NotConnected` from those calls. Implement the re-probe as: on a transport-class error from the remote engine, the handler (or a small supervisor task in run.rs holding the slot + a `RemoteAdapter` liveness flag) runs one re-probe; success → `replace_engine(new_remote)`, failure → build the embedded engine (with home injection **if** the ADR-019 batch has landed; otherwise today's `for_workspace_root(..., None, ...)` — the design doc lists them as independently deliverable) and `replace_engine(embedded)`; subsequent new-session calls then land on the embedded engine. In-flight turns are NOT migrated (they already failed with the transport error). Degrade happens once (a `degraded: AtomicBool` in the supervisor) — no re-probe loop after degrade.
- Test seam: extract the resolution into `pub(crate) fn resolve_engine_choice() -> EngineChoice` reading env vars only (pure, unit-testable), with the async connect/degrade in the supervisor.

- [ ] **Step 1: Failing tests**

```rust
// run.rs tests (pure env-var resolution):
#[test]
fn should_prefer_explicit_remote_engine_over_everything() {
    // Given MIRRI_ACP_REMOTE_ENGINE set AND MIRRI_ACP_ENGINE=embedded set
    // When resolving
    // Then the choice is ForcedRemote (priority 1 wins over 2)
}

#[test]
fn should_force_embedded_when_engine_env_says_so() {
    // Given MIRRI_ACP_ENGINE=embedded and no remote url
    // Then EmbeddedForced
}

#[test]
fn should_skip_probing_when_workspace_dir_is_set() {
    // Given MIRRI_ACP_WORKSPACE_DIR set (and no higher-priority vars)
    // Then EmbeddedForced (probe skipped entirely)
}

#[test]
fn should_probe_only_when_no_override_applies() {
    // Given no override vars
    // Then ProbeDesktop
}
```

(env-var tests: same serial-env caveat as Task 2; follow the repo's existing env-var test pattern.)

Integration test (in `tests/` with a fake WS server — reuse `mirri-remote`'s test harness pattern by spawning a real `mirri-server` instance? NO — keep it lighter: the full-chain e2e is Task 8; here unit-test `handle_connection_death` with injected probe/connect closures):

```rust
#[tokio::test]
async fn should_replace_the_engine_slot_and_clear_route_caches_when_reconnected() {
    // Given a handler with a remote engine (mock adapter that fails) and a
    // probe closure returning a fresh mock adapter
    // When handle_connection_death runs
    // Then the handler's current engine is the new adapter
    // And a previously remembered session routes to the new engine (route cache cleared)
}

#[tokio::test]
async fn should_probe_only_once_after_degrading_and_never_reprobe_again() {
    // Design 14 D3: degrade is one-way ("降级后不再自动再探测")
    // Given a handler whose remote engine died and whose probe closure
    //       ALWAYS fails (counting invocations), leading to degrade
    // When further transport errors occur (two more session-addressed calls)
    // Then the probe closure's invocation count is exactly 1 (the degrade attempt)
    // And the new-session calls were served by the embedded engine
}
```

- [ ] **Step 2: Verify failure** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp`
- [ ] **Step 3: Implement** per the interfaces above.
- [ ] **Step 4: Verify pass** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp` all green.
- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-acp/src/
git commit -m "feat(acp): probe/reprobe/degrade engine resolution + replaceable engine slot (design 14 D3)"
```

---

### Task 5: cwd → workspaceId routing (D2) + list pinning filter (D6)

**Files:**
- Modify: `rust/crates/mirri-acp/src/acp_handler.rs` (`session_new`/`session_load` engine-call params; `session_list` filtering)
- Test: `rust/crates/mirri-acp/src/acp_handler.rs` in-module tests (follow existing `ProbeEngine` pattern at :3953)

**Interfaces:**
- Consumes: Task 4's engine slot (`current_engine()` accessor).
- Produces (remote mode only — embed-mode unchanged):
  - `async fn translate_cwd_to_workspace_id(engine: &Arc<dyn EngineAdapter>, cwd: &Path) -> Result<String, (i64, String)>` — calls `workspace.list` (existing engine method), lexically resolves the cwd (`mirri_core::session::lexically_resolve`), matches each registered root by the same lexical form; hit → its id; miss → `Err` with message `"directory {cwd} is not a registered workspace of the desktop server; open it in the desktop app first"` (ACP-level error, mapped to the `session/new` JSON-RPC error response).
  - `session_new`/`session_load` remote path: before `session.create`, translate the request cwd and pass `workspaceId` in the create params (server's existing `workspace_id` field — `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` CreateSessionParams, alias `workspaceId`); `session/load` uses the same translation for `session.resume`'s params when the session is unknown locally.
  - `session_list` remote path: query the server, then filter items by `workspaceId == translate(host_start_root)`; on translation failure (host root unregistered) return the unfiltered result with a stderr warning (the IDE still deserves a list, not an error — mirror-reading of D6's "fallback" is deliberate: pinning is a presentation-scoping concern; log loud).
- Design note for the implementer: "remote mode" detection = the current engine slot holds a `RemoteAdapter`. Since `EngineAdapter` is a trait object, add `fn is_remote(&self) -> bool { false }` default method to `EngineAdapter` (mirri-transport) overridden in `RemoteAdapter` — small trait addition, no other implementors affected.

- [ ] **Step 1: Failing tests** (ProbeEngine gains `workspace_list_response: Mutex<Value>` + `created_params: Mutex<Vec<Value>>` capture):

```rust
#[tokio::test]
async fn should_route_session_new_to_the_registered_workspace_of_the_cwd() {
    // Given a remote-mode handler whose engine lists workspace wd-1 rooted at /lexically/resolved/proj
    // When session/new arrives with cwd /lexically/resolved/proj
    // Then the engine receives session.create with workspaceId wd-1
}

#[tokio::test]
async fn should_reject_session_new_when_the_cwd_is_not_a_registered_workspace() {
    // Given the same handler
    // When session/new arrives with cwd /nowhere
    // Then the response is a JSON-RPC error whose message mentions "not a registered workspace"
    // And session.create was never called
}

#[tokio::test]
async fn should_filter_session_list_to_the_host_root_workspace_in_remote_mode() {
    // Given the engine returns sessions from two workspaces
    // When session/list runs (remote mode, host root = /proj)
    // Then only the /proj workspace's sessions are returned
}

#[tokio::test]
async fn should_pass_the_mcp_servers_overlay_through_in_remote_mode() {
    // Design 14 D5a: overlay rides the existing params, no new mechanism
    // Given a remote-mode handler and a session/new request carrying mcpServers
    //       (use a minimal one-entry array, shape from acp_handler's overlay_entries)
    // When session/new routes to the registered workspace
    // Then the captured session.create params carry the mcpServers overlay entry
    // And workspaceId is still set (both fields survive the same call)
}
```

- [ ] **Step 2: Verify failure** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp`
- [ ] **Step 3: Implement** per interfaces.
- [ ] **Step 4: Verify pass** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp`.
- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-acp/src/acp_handler.rs rust/crates/mirri-transport/src/adapter.rs
git commit -m "feat(acp): cwd→workspaceId routing rejects unregistered roots; remote list pinning (design 14 D2/D6)"
```

---

### Task 6: session/close unsubscribe + stderr logging surface

**Files:**
- Modify: `rust/crates/mirri-acp/src/acp_handler.rs` (`session_close`)
- Test: in-module tests

**Interfaces:**
- Consumes: Task 3's `RemoteAdapter::unsubscribe`.
- Produces: remote-mode `session_close` calls the engine slot's unsubscribe before the engine `session.close` call — only when the slot is remote (`is_remote()`). Embedded mode untouched. Engine-form logging (probe outcome, degrade, translation misses, version-gate failure) goes through `tracing::warn!` on stderr (mirri-acp's logger already writes stderr — verify `run.rs`'s tracing init and follow it).

- [ ] **Step 1: Failing test** — `should_unsubscribe_from_the_remote_engine_on_session_close` (ProbeEngine records unsubscribe calls; assert order: unsubscribe before close).
- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Verify pass** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp`.
- [ ] **Step 5: Commit** — `feat(acp): unsubscribe from remote sessions on close (design 14 D5)`.

---

### Task 7: docs — reference updates

**Files:**
- Modify: `docs/zh/reference/mirri-acp.md`, `docs/en/reference/mirri-acp.md` (the two language twins; add the auto-probe behavior, `MIRRI_ACP_ENGINE`, the fallback chain, and the unregistered-cwd error in user language)
- Modify: `docs/zh/reference/mirri-command.md:109` + `docs/en/reference/mirri-command.md:111` (acp paragraph mentions auto-probe in one clause)
- Follow the `gen-docs`/`translate-docs` skill conventions if they apply to reference pages (check `.agents/skills/gen-docs/SKILL.md` scope first — reference docs are in scope).

- [ ] No failing-test step (docs): write the sections, verify the docs build if a docs build command exists (`docs/` is VitePress — `pnpm --filter docs build` or equivalent per `docs/AGENTS.md`), keep both languages in sync per `translate-docs` rules.
- [ ] Commit — `docs(acp): desktop runtime sharing — probe, fallback, switches, routing error`.

---

### Task 8: end-to-end integration test (real server, real probe files)

**Files:**
- Test: `rust/crates/mirri-acp/tests/desktop_share_e2e.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: a test that boots a real `mirri-server` (the repo's e2e precedent: check `mirri-server`'s own tests or `mirri-test-harness` for how a real server is spawned in-process — `mirri_server::serve` with an ephemeral port; follow `rust-e2e-ws-handshake` skill guidance if it applies), writes a fake `desktop-state.json` + `server.token` into a temp `MIRRICODE_HOME`, and drives the AcpHandler through the public `handle()` surface:
  1. probe finds the server, remote engine in slot;
  2. `session/new` with a registered cwd → session created on the server; events flow to the ACP update relay;
  3. `session/new` with unregistered cwd → error, no server-side session;
  4. no server-side revival of untouched persisted sessions (pre-seed a persisted session dir, assert it stays un-restored);
  5. kill the server → transport error on in-flight; next request re-probe fails → engine slot is embedded; a subsequent `session/new` succeeds locally (acceptance 3's startup-fallback twin: degrade path);
  5a. **no-desktop startup** (acceptance 3): fresh handler + fresh temp home with no desktop-state.json → embedded from the start, session/new succeeds, and the probe made zero HTTP requests (assert via the probe's injectable transport recorder — extend the Task 2 seam with a `#[cfg(test)]` probe-count hook);
  5b. **degrade one-way** (acceptance 3/D3): after step 5's degrade, restart the server → next session-addressed call still fails `NotConnected` (no re-probe), and a `session/new` lands on the embedded engine (probe count still 1);
  6. **re-probe success chain** (acceptance 6): a fresh handler + server A, kill A, start server B on the same recorded port, next request re-probes successfully → remote again, and a session-addressed call to a session created before the kill gets touch-to-subscribed on the NEW connection (server B must know the session — seed it via a shared home dir so restore-on-demand serves it);
  7. **explicit-env byte-compat** (acceptance 5): with `MIRRI_ACP_REMOTE_ENGINE` pointing at the server, assert connect succeeds and that the run path took the explicit branch (env resolution unit — plus run the existing remote-mode conformance suite as regression: `cargo test -p mirri-acp tests/conformance` if it exercises the remote path; otherwise assert via stderr log line);
  8. **probe side-effect freedom** (acceptance 4): in step 5a's fresh home, assert no `workspaces.json` was created and the probe attempted no TCP connections beyond the (absent) state-file read (transport recorder count 0).

- [ ] **Step 1: Write the test** following the skill `rust-e2e-ws-handshake` warnings (handshake timeouts under load — generous timeouts, per-file runs).
- [ ] **Step 2: Run** — `cargo test --manifest-path rust/Cargo.toml -p mirri-acp --test desktop_share_e2e -- --test-threads=1` — fix until green.
- [ ] **Step 3: Commit** — `test(acp): desktop runtime sharing e2e (probe/route/reject/no-revival/degrade)`.

---

### Task 9: full verification + self-review

- [ ] Self-review the whole diff (`git diff main...HEAD` in the worktree): debug prints, naming, comment accuracy (comments must cite design 14 ADRs where the mechanism is non-obvious), no stray files.
- [ ] `cargo test --manifest-path rust/Cargo.toml --workspace` (background task, ≥20min timeout — per repo AGENTS.md).
- [ ] `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings`.
- [ ] `cargo fmt --check` (or the repo's formatter invocation — follow `rust/AGENTS.md`).
- [ ] Fix all findings, re-run until green. No commit of broken code.

---

### Task 10: upstream write-back (design §6 duties) + changeset

- [ ] `docs/specs/session-persistence-architecture.md` ADR-019: append the revision record (remote-default rejection reopened by design 14 under the probe+fallback premise).
- [ ] `docs/specs/session-persistence-requirements.md` SDR-009: append matching revision note; US11.4 add the execution-position amendment (remote mode carried by ACP host read-side filter).
- [ ] Run the `gen-changesets` skill (`.agents/skills/gen-changesets/SKILL.md`): user-perceivable change (ACP sessions appear live in desktop) → `minor` against `@mirri-ai/release`.
- [ ] Commit — `docs(specs): ADR-019/SDR-009 revision records + US11.4 amendment` and the changeset in its own commit per the skill's rules.

---

## Execution Notes

- Task order: 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 strictly serial (each builds on the previous; Tasks 1 and 2 are the only parallelizable pair — but keep serial for review simplicity).
- Acceptance 8 (approval handover) is covered at TWO levels on purpose: adapter/server level in Task 3's last test (routing must not regress) — the full ACP↔web handover is a server-side routing fact once both subscribe, so the fake-server pin is the durable guard; do not skip it.
- Task 4 is the riskiest (handler refactor); if `replace_engine` + route-cache clearing turns out to conflict with `ensure_wired`'s pointer-keyed dedup in a way the plan didn't anticipate, STOP and report rather than improvising a different seam — that's a design-level question.
- The fake-server harness in `remote_adapter_tests.rs` is the template for Tasks 1/3's tests — read it fully before writing test 1.
- Env-var-mutating tests must be serialized (repo has no serial-test dep — use a single `#[test]` per env state or a mutex).
