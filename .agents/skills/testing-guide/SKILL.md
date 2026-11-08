---
name: testing-guide
description: Use before running, writing, or debugging tests in this repo — the local execution ladder (which command to run for which change), the test iron rules (T0–T3/R layering, the eight isolation contracts, R-layer attribution), how to judge a red full-workspace run without mislabelling it environmental (including the case where failures scatter across different tests), the difference between R-layer classification and contention isolation, the local-tier to CI-job map, and the traps that make suites fail for reasons unrelated to the code. Load it when a full-suite run goes red, before writing a new test, when touching the RPC registry or its pinned surface counts, or when a loopback mock server returns an unexpected 502.
---

# Testing Guide

Procedural companion to two always-on contracts:

- `pnpm gate` is the single quality entrypoint (`scripts/gate.mjs`). Run it after every edit; it takes ~3 s and compiles nothing.
- The test iron rules below are **mechanically enforced** by `rust/scripts/check-test-hygiene.mjs` and `rust/scripts/check-zero-io.mjs`, both inside `pnpm gate`. The text here tells you what the gate will reject so you do not have to learn it one failed iteration at a time.

Design source of truth for layering and isolation: `docs/specs/rust-core-design/16-test-execution-strategy.md` §4. This file is the operational summary; if the two disagree, the design document and the gate script win.

## 1. Execution ladder — which command for which change

`pnpm gate` compiles nothing and runs no tests (five static guards, ~3 s). The next tier that actually runs tests is `gate:full` at ~8 min. So the daily loop is **cargo against one crate or one test target**, not a choice between the two extremes.

| Intent | Command | Cost |
|---|---|---|
| After every edit | `pnpm gate` | ~3 s, no compile, no tests |
| Changed rust production code (cross-crate surface) | `pnpm gate:rust` | minutes (includes clippy) |
| **Daily main path: changed one crate** | `cargo test -p <crate> --lib` | seconds to minutes |
| Changed a file under `tests/` | `cargo test -p <crate> --test <bin>` | same |
| Pinning down a single test | `cargo test -p <crate> --lib <substring>` | fastest |
| Batch close-out / before opening a PR | `pnpm gate:full` | ~8 min (warm cache) |
| Touched the R layer (`#[ignore]`) | `pnpm gate:real` | 1–2 min |
| Changed dependencies / added a crate | `cargo deny --manifest-path rust/Cargo.toml check` | minutes (**in no gate tier**; needs `cargo install cargo-deny`) |
| Touched platform-specific code | `cargo xwin check --target x86_64-pc-windows-msvc` | minutes (**in no gate tier**; needs `cargo install cargo-xwin`) |

`gate` exit codes: `0` all green; `2` passed with known failures (`KNOWN_FAILURES` in `scripts/gate.mjs`); `1` new failure (fast-fail abort). `KNOWN_FAILURES` is debt, not a licence — every entry carries an owner and a `TODO(remove)`.

## 2. Test classification maps to test targets, not to comments

`// layer: T<n>` is **review-compliance metadata**. It is only checked for presence within a file's first 20 lines by `check-test-hygiene.mjs`. **No runner consumes it.** Do not write a script that parses layer tags to select tests — that path costs maintenance and inevitably drifts.

The classification axis cargo actually consumes is the test target itself, and the mapping is zero-maintenance:

| Layer | Physical location | Command |
|---|---|---|
| T0 unit | `#[cfg(test)]` inside `src/` | `cargo test -p <crate> --lib` |
| T1/T2 integration | `tests/*.rs` | `cargo test -p <crate> --test <file>` |
| T3/R real-environment | `#[ignore]`-gated (either location) | `pnpm gate:real` or `-- --ignored` |

Current `tests/` targets (re-derive with `ls rust/crates/<crate>/tests/*.rs` when this looks stale):

```
mirri-acp: conformance desktop_share_e2e e2e_mock
mirri-cli: cli_tests
mirri-collaboration: collaboration_tests
mirri-core: session_group terminal_misc_group tools_scope_group
mirri-embedded: e2e_real_llm   ← only this one; the bulk lives in src/ #[cfg(test)] (use --lib)
mirri-kosong: providers_group
mirri-mcp: mcp_group
mirri-os: all_group
mirri-persistence: persistence_group
mirri-plugin-host: plugin_group
mirri-proto: proto_group
mirri-remote: remote_adapter_tests
mirri-server: server_group
mirri-test-harness: cross_backend_contract harness_tests mock_llm_tests
```

## 3. Iron rules

Violating these means rework. They are enforced by the gate; read them so you do not have to be told twice.

### 3.1 Layering

| Layer | Name | Location | Responsibility | Parallel-safe | Speed budget |
|---|---|---|---|---|---|
| T0 | unit | `#[cfg(test)]` in `src/` | Pure logic: reducers, parsers, pure functions. No IO, no real time, no env reads/writes | inherently | ≤5 s |
| T1 | component integration | `tests/` | Collaboration inside one crate, using injected fakes (fake FS / paused clock / `MockLlm`) | must be | ≤5 s |
| T2 | service integration | `tests/` (server, embedded) | Boots a real service instance (random port + private HOME), verifies protocol behaviour | must be | ≤30 s |
| T3/R | real-environment | `#[ignore]`-gated | Tests whose **outcome depends on the real environment**: real wall-clock windows, real subprocess sampling, performance probes, real-link e2e | not in the default pool; CI R profile pins `test-threads = 2` | no budget (not in the default gate) |

- **Layer rule**: a test's layer is decided by **what real resource it needs**. Fakes suffice → T0/T1; needs a real process or port → T2; needs the real external network → T3. T3 never runs in `cargo test --workspace`; only via `--ignored` or a dedicated command.
- **Execution dimension (R/D)**, from `docs/superpowers/specs/2026-11-06-rust-test-execution-optimization-design.md` §3: ask "would the result change on a machine 10× slower?" If it would (or might) → R layer (`#[ignore]`); if not → D layer. A real subprocess alone is not sufficient for R — the assertion must depend on a real wall-clock window, an unobservable real-world check, or an external service (a subprocess test with a condition-driven assertion and a wide backstop stays in D). A `// hygiene-allow: real-...` reason prefix **forces** R attribution and is mechanically enforced by test-hygiene rule R.
- **Declared layer**: every test module (`mod` or file) must carry a `// layer: T<n>` tag in its header. A new test without one does not pass review. Pre-existing gaps are baselined and backfilled in R3 migration batches.

### 3.2 Isolation contract (eight obligations)

1. **Environment variables**: T0/T1 must not read or write process env. Code needing env semantics must go through injected resolution (the `resolve_*_from` pattern); tests pass values instead of mutating env. If a T2 test genuinely needs env (a real child process, say): (a) scope it to the child only (`Command::env`), and (b) if `set_var` is unavoidable, use the **workspace-unique** `env::lock()` guard over the minimal variable set.
2. **HOME / directories**: every filesystem footprint lands inside `tempfile::tempdir()` (or an equivalent throwaway dir). Never read or write anything under the real `~`. Code needing HOME semantics takes an injected `home_dir` or an isolated fake home.
3. **Ports**: every listener binds `127.0.0.1:0` for random assignment; hardcoded ports are forbidden. Client side must tolerate a port conflict with retry.
4. **Time**: T0/T1 must not really wait (`thread::sleep` / fixed `tokio::time::sleep`). Async tests default to `start_paused`; advance time with `tokio::time::advance`. Polling always uses a deadline-bounded `wait_for_*` helper (the prohibition targets a **bare fixed sleep inside test code**; the helper's internal ≤50 ms stepping is exempt). A bare fixed sleep is never a synchronisation primitive.
5. **Network**: default-gate tests must not touch the real internet. HTTP goes through wiremock or a local listener.
6. **Databases / locks**: test db files and lock files live in the tempdir.
7. **Working directory**: a test must not assume the CWD is writable, and must leave no trace in the real CWD.
8. **Locks and async**: `env::lock()` is a synchronous mutex — **never hold it across `.await`** (thread starvation deadlock on a multi-threaded runtime). Inside async context, finish set/restore before entering the async block, or move to subprocess-level injection (`Command::env`).

### 3.3 Mechanical guards and speed budget

- New tests declare a layer (`// layer: T<n>` module header). T3 must be `#[ignore]`.
- New tests must satisfy all eight isolation obligations before review.
- Needs env semantics → prefer injection; if `set_var` is truly required → `mirri-test-support::env::lock()` + minimal set + not across `await`.
- Existing tests that violate the contract migrate per ROADMAP in batches; `rust/scripts/test-hygiene-baseline.json` whitelists them and does not block new contributions. **The sleep rule is zero-tolerance with an allow-list (batches 6a/6b)**: a baseline sleep entry only counts as covered when its `reason` matches `^(poll-step|negative-wait|real-|D:)`. Everything else — missing reason, `legacy:`, the `Legacy:` variant, ad-hoc prose — is **not** coverage. `--write-baseline` re-anchoring writes an uncovered reason for any genuinely new entry (no matching old entry in the same file+type group), so re-anchoring cannot launder a new bare sleep: the next gate stays red until someone gives it a real reason.
  - **Known mechanism limit (do not assume it is airtight)**: the sleep guard-context check scans the 15 lines before the hit for a `wait_for` marker — if `wait_for` appears anywhere within 15 lines, a later bare sleep in the same test counts as inside a polling loop; an inline `// hygiene-allow: <any prose>` is also accepted (outside rule R's `real-` branch, the reason is not validated). Narrowing the window blocks more latent blind spots (an audit found 4 that way) but requires re-anchoring 150+ baseline entries — a separate batch.
- **Primary guard**: `node rust/scripts/check-test-hygiene.mjs` (hooked as `pnpm gate:rust-test-hygiene`, same tier as the zero-io guard). It scans test cfg bodies for bare `std::env::set_var`/`remove_var`, bare `thread::sleep`, missing `// layer:` tags in `tests/` files, and rule R's real-env signals. An inline `// hygiene-allow: <reason>` exempts a line (except rule R's `real-` window branch). clippy `disallowed-methods` cannot be scoped to test cfg globally (it would flag legitimate production `set_var`), so it stays a ROADMAP increment (SDR-016.3).

### 3.4 Rule R — the real-env attribution ratchet

Two signals in test code must be attributed to the R layer, enforced by test-hygiene:

- **Bare real subprocess spawn** (`std::process::Command` / `tokio::process::Command` / `Command::new`): the line must sit inside an `#[ignore]` test fn, or carry a line-level `// hygiene-allow: <reason>`. Helper fns cannot carry `#[ignore]`, so their spawn lines **stay visible by default** — only a line-level allow exempts them; an `#[ignore]` on the caller does **not** vouch for a helper's spawn.
- **`// hygiene-allow: real-...` semantic window**: must be inside an `#[ignore]` test fn. Windows inside helpers are not reported by the gate (callers are not inspected) — attribute by caller during audit. This branch does not accept `hygiene-allow` as an escape hatch: the reason prefix *is* the R signal.
- Wrapper spawns (`mirri_os` family) are not covered; audit them case by case.
- **R-layer suite (CI definition)**: the `#[ignore]` set **minus** `e2e_real_llm` (needs real LLM credentials, run manually) **minus** the G-30 placeholder stubs (`should_handle_create_rpc_independently` / `should_handle_plugins_rpc_independently`, deliberate panic TODOs excluded by test name).
- **Local entry**: `pnpm gate:real` = `cargo test -- --ignored` minus the two stubs (libtest `--skip`). `gate:real` does not exclude `e2e_real_llm` by name — it skips itself without credentials; if `OPENAI_API_KEY` is exported locally, `gate:real` will hit the real LLM (cost / hang risk — `unset OPENAI_API_KEY` first). Local R runs never use nextest: on macOS the per-exec tax measured 14×–80×+ ("80×+" is a lower bound from ">18 min unfinished", not a ceiling).
- **CI selection**: `rust-real-env` (non-blocking) plus nightly (required) run `cargo nextest run --workspace --locked --run-ignored only --no-fail-fast -E 'not (binary(e2e_real_llm) or test(should_handle_create_rpc_independently) or test(should_handle_plugins_rpc_independently))' --profile real`. `--no-fail-fast` carries weight: one red must list every failing test, since that list is the attribution input. Profile lives in `rust/.config/nextest.toml` (retries 1, slow-timeout 2m×3, `test-threads = 2`).
- **Concurrency exception**: `test-threads = 2` is **deliberately hardcoded** — the R layer is sensitive to real subprocess and port contention and does not follow nproc. This is the explicit exception to the D-layer policy of "do not hardcode concurrency; derive from nproc or measurements" (design §6; that section's `max-threads` is a typo — the real nextest key is `test-threads`; do not "fix" the config to match it). Treat the number as an empirical setting, not a proof — when a case still flakes at 2 threads, name the contention mechanism (§4 step 4) rather than lowering the count.
- **Runner adoption**: nextest is R-layer only. D-layer adoption must clear an A/B threshold (≤1.1×, `.github/workflows/rust-nextest-ab.yml`).
- **`#[ignore]` style**: R-layer tests use bare `#[ignore]` plus an adjacent comment `// R-layer: <reason> (design §3)`. Precedents (all in this shape): `mirri-acp/tests/conformance.rs`'s `should_not_spawn_the_real_homes_mcp_servers_when_the_conformance_child_creates_a_session` (pgrep-absent sampling), `mirri-embedded/src/embedded_server/tests.rs`'s `should_not_connect_plugin_mcp_servers_when_installed_until_reload` (plugin-absent sampling), `mirri-embedded/src/skill_watcher.rs`'s `should_mount_the_watcher_within_a_fixed_time_budget` (mount probe). `b1_print_mode_tests.rs`'s drain budget test was once on this list and has migrated to D-layer paused-clock (batch 5 dropped its `#[ignore]`). `e2e_real_llm.rs` uses bare `#[ignore]` plus a module header explaining why. `#[ignore = "reason"]` is accepted too, though not required.
- **Known constraint**: `#[ignore]` currently has two meanings (R layer vs TODO stub). A new TODO stub must choose one: join the R-layer exclusion list by the same naming convention, or implement it before landing (never leave a stub that can never run).
- **Config trap**: nextest only warns about unknown config keys (exit 0) — after editing `rust/.config/nextest.toml`, confirm stderr has no `ignoring unknown configuration key`. A profile with a misspelled key silently stops applying.
- **Speed budget**: per test ≤5 s (T0/T1), ≤30 s (T2); full workspace ≤7 min (warm cache). A test over budget must be split or dropped a layer (switch to fakes).
- Every test crate maintains a shared `tests/common/` module; new test scaffolding goes there, not duplicated per file.

## 4. Judging a red full-workspace run

A red test in the full workspace must be **isolated before it is characterised**. Isolating is necessary but **not sufficient** — the three steps below are the actual criterion, and stopping after step 1 is how a real, fixable cause gets mislabelled as environmental noise.

### Step 1 — isolate

```bash
cargo test -p <crate> --lib <test-name-substring>
```

### Step 2 — establish stability

Run the **same** command twice and compare the failing **sets**, not just the count:

```bash
cargo test -p <crate> --lib 2>&1 | sed -n '/^failures:$/,$p' | grep '^    ' | sort -u
```

- Failing set differs between runs → concurrency or load interference. First check whether **you** are running several full suites at once; that manufactures CPU contention and makes the failure set drift, which destroys every downstream inference. Machine-level interference is possible too (on macOS a runaway `fseventsd` slows everything down and amplifies races), but rule it in only after excluding your own concurrency.
- Failing set is identical → continue to step 3.

### Step 3 — look for a shared, named feature

If the failures share one concrete feature, that feature is the cause and it is worth chasing — it is **not** noise:

| Shared feature in the message | Likely cause | Fix |
|---|---|---|
| `HTTP 502 Bad Gateway`, 502 on `/v1`, `wiremock` never receives the request | A system HTTP proxy intercepts loopback. Mock servers bind `127.0.0.1:<random>`; a proxy without `127.0.0.1` in its bypass list answers instead of the mock | `NO_PROXY="127.0.0.1,localhost" no_proxy="127.0.0.1,localhost" <command>` |
| e2e timeouts landing on exact 30 s / 60 s / 90 s boundaries | The web e2e suites spawn `rust/target/debug/mirri`, which is stale relative to the rust sources you just changed | `cargo build -p mirri-cli` before running them |
| `models.dev directory has no such entry` | The directory is fetched over the network; offline it is empty until a refresh | Refresh the catalog, or accept these as offline-only failures |
| `Connection refused` where a 502 appears instead | Same proxy cause as row 1 — the proxy answers before the refusal can surface | Same `NO_PROXY` fix |

Only when the failures share no feature, and the set is unstable, is "environment" a legitimate conclusion. Even then: **do not report it as green.** Report it as unverified.

### Why step 2 and 3 both matter

Skipping step 2 hides "I ran three suites at once". Skipping step 3 turns a fixable proxy or stale-binary misconfiguration into unfalsifiable noise. A single test passing in isolation proves only that it does not depend on its neighbours — it says nothing about what made it fail in the full run.

### Step 4 — when the failures scatter across different tests

A different pattern: each round goes red, but **a different test each time**, and none of them reproduces locally (even repeated 10×, even under deliberate CPU oversubscription). Do not read that as "four broken tests". Read it as **one suite-level condition surfacing wherever it happens to land**, and work it as such:

1. **Confirm the scatter.** Collect the failing test names from several rounds (PR runs and recent `main` runs alike — a scatter that also hits `main` is not your PR's doing):

   ```bash
   gh run list --branch main --workflow CI --limit 20 --json databaseId,conclusion,displayTitle
   gh run view <run-id> --log-failed 2>&1 | grep -E "FAILED$|panicked at"
   ```

   No repeats across rounds → a shared condition, not per-test defects.

2. **Find what the scattered tests have in common**, not what is unique to each. The usual candidates: real wall-clock waits with tight deadlines, a mock or global shared between in-process servers, background tickers/sweepers that outlive the test's own scope, and real sockets that assume they win a port race.

3. **Check whether the test asserts the precondition it depends on.** A test that waits five seconds for a task to settle and then drops the result is asserting nothing — the test proceeds either way. When the leftover work then perturbs a shared counter, the failure message blames a product bug ("no duplicate") instead of the unsettled premise. This is the single most common reason a suite-level condition turns into a misleading red.

4. **Reproduce by contention, not by repetition.** Repetition on an idle machine proves nothing. Either oversubscribe the CPU, or run the whole module (not the single test) — a suite-level condition often only appears with its neighbours alive.

5. **Classify only after 1–4.** Then choose per test, with the trade-off stated below.

### R layer is a classification, not an isolation bucket

These are two different reasons a test runs outside the default pool, and they require different evidence:

| | Meaning | Evidence needed | Cost of getting it wrong |
|---|---|---|---|
| **R layer (classification)** | The test's **outcome legitimately depends on the real environment** — a real wall-clock window, a real subprocess, an external service. Deterministic isolation is not available at reasonable cost. | The assertion genuinely references real time / a real external condition | Correctly placed; runs in `rust-real-env` and nightly |
| **Isolation (execution strategy)** | The test is deterministic **in principle**, but is sensitive to parallelism or resource contention; running it in the main pool is unstable. | A mechanism explains the contention (shared mock, port race, global ticker) | Skipping the analysis hides a real defect |

The two are not exclusive, and the second is a legitimate strategy — R-layer cases **are** genuinely parallelism- and resource-sensitive, which is exactly why the R profile pins concurrency instead of following nproc. Today that pin is a single simple correspondence (`test-threads = 2` in `rust/.config/nextest.toml`), and it is stable for most of the suite. Treat that number as **an empirical setting, not a proof**: when a case still flakes at 2 threads, the contention mechanism is stronger than the pin, and the answer is to name the mechanism (step 2 above) — either fix it, or isolate the case further — not to lower the thread count again.

What this section exists to prevent: reaching for a layer move as the *first* response to a red suite. Moving a test into the R pool has a concrete consequence — `rust-real-env` is `continue-on-error: true` on pull requests, so a test that moves there **stops blocking PRs**. That is acceptable for a test whose outcome is genuinely environment-dependent. It is not acceptable as a way to quiet a test whose premise was simply never asserted, because the underlying defect stays and now only surfaces on nightly.

Rule of thumb: if you cannot name the mechanism in one sentence, you are not ready to classify — you are postponing.

## 5. Local tier to CI job

| CI job (`.github/workflows/ci.yml`) | Local equivalent | In a gate tier? |
|---|---|---|
| `rust-test` (full `cargo test --workspace`) | `pnpm gate:full` | ✅ |
| `rust-clippy` (inside `gate:rust`) | `pnpm gate:rust` | ✅ |
| `rust-zero-io-gate` | `pnpm gate` (`rust-zero-io` stage) | ✅ |
| `rust-test-hygiene-gate` | `pnpm gate` (`rust-test-hygiene` stage) | ✅ |
| `rust-real-env` (non-blocking) / `nightly-rust-real-env` (required) | `pnpm gate:real` | ✅ (opt-in tier) |
| `rust-deny` | `cargo deny --manifest-path rust/Cargo.toml check` | ❌ manual |
| `rust-agent-md-parity` | `cargo test -p mirri-core --test tools_scope_group agent_md_cross_engine_` and `agent_list_cross_engine_` | ❌ manual (when touching the `AGENTS.md` template or tool inventory) |
| Windows cross-compile | `cargo xwin check --target x86_64-pc-windows-msvc` | ❌ manual |

The last three rows are known gaps: changing dependencies, the `AGENTS.md` tool template, or platform-specific code requires running them by hand or the PR goes red. They stay out of `pnpm gate` because they need extra tooling (`cargo-deny` / `cargo-xwin`) that is not part of a default Rust toolchain; when not installed, CI is the authority.

## 6. Timeout discipline

- A normal test finishes in 5–6 minutes; 10 minutes is the ceiling. Past that, suspect **the test case itself or a bug in the implementation** (an infinite loop, a wait for an event that never arrives) — not that the test "genuinely needs that long".
- Investigate sceptically. Blindly raising a timeout only masks the problem.
- Standard practice: redirect test output to a file under `.tmp/`, check it every 1–2 minutes, and locate where it stalls — one expensive test, or a bug. Decide after locating the stall, not by widening the budget.
- Full workspace `cargo test` takes ~7 minutes wall-clock on a warm incremental cache (~6m44s, mostly compilation; the test bodies finish in seconds), and a follow-up `cargo clippy --workspace --all-targets -- -D warnings` adds ~45 s. A cold cache (`cargo clean`, CI, many changed crates) is significantly longer. Run both as background tasks with a ≥20-minute timeout; foreground shell calls cap at 300 s and get moved to the background mid-build.

## 7. Touching the RPC registry: the pinned-surface checklist

The RPC surface is pinned in **four** places. Adding or removing a method means updating all of them; CI reports them one at a time, which is how a change can go red two rounds in a row. Find them all up front:

```bash
grep -rn "methods_for(RpcBackend::\|all_routes().count()\|serve_surface.len()" rust/crates
```

| File | What to update |
|---|---|
| `rust/crates/mirri-proto/src/rpc/registry.rs` | The `METHODS` entry, plus the five pinned counts in `should_pin_the_method_count_quadruple` / `should_record_the_disposition_ledger` / `should_report_the_backend_current_views` (embedded, server, both, total, rust-first, settled, currently-dispatched) |
| `rust/crates/mirri-proto/src/rpc/authoritative-methods.tsv` | One row, and its **position must match `METHODS` element-for-element** — the parity test compares row order, so an appended-at-the-end row fails even when every field is right |
| `rust/crates/mirri-embedded/src/embedded_server/tests.rs` | The flattened route total, and the routed-vs-registry equality count |
| `rust/crates/mirri-test-harness/tests/cross_backend_contract.rs` | The serve-surface total (`methods_for(RpcBackend::Server)` count) |

Two rules that are easy to get wrong:

- **Pick the disposition deliberately.** `both(...)` means the method exists in the TS v2 contract too. A method only the rust engine serves (no klient counterpart) must use `both_kept(...)` — `RustFirst`. Using `both` for a rust-only method fails the v2 row-count assertion.
- **Re-anchor the hygiene baseline, never hand-merge it.** `rust/scripts/test-hygiene-baseline.json` is keyed by `file:line`, so inserting code into `tests.rs` shifts every later entry. Run `node rust/scripts/check-test-hygiene.mjs --write-baseline` (it prints `221 entries`, and re-anchoring alone must leave 0 new violations). Editing the JSON by hand gets the arithmetic wrong.

## 8. What belongs here versus in `AGENTS.md`

`rust/AGENTS.md` keeps only the always-on contract: the gate entrypoint, the one-line iron-rule summary, and a pointer here. Everything in this file is procedural — needed when you are about to run, write, fix, or audit tests, which is not every task. When a new testing lesson appears, add it here; only add to `AGENTS.md` if violating it costs rework **before** the gate can catch it.
