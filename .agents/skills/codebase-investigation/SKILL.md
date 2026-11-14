---
name: codebase-investigation
description: Use when investigating where something lives, how a flow works, or what a change will break in this repo — before reaching for grep/Read. Covers the CodeGraph index (build, worktree handling, query shapes), the code-vs-spec split (Markdown is NOT in the graph), the multi-projection contract chains where the graph has no edges, and the drift traps that make a naive search answer wrong.
---

# Codebase Investigation

This repo is large enough that grep-first investigation is the slow path: ~1,170 indexable files, ~463 Rust files / 481k lines, 34 domains of spec documents at 242k words. This guide tells you which instrument answers which question.

## 1. The instrument table — pick before you search

| Question | Instrument | Why not the others |
|---|---|---|
| Where is symbol X / who calls it / what breaks if I change it | **CodeGraph** (`codegraph_explore`, `callers`, `impact`) | grep cannot give call paths or blast radius |
| Which file implements feature Y | **CodeGraph explore** with a natural-language question | One call returns verbatim source + relationships |
| What did we decide about Z, and is it still standing | **`docs/specs/`** — the `*-requirements.md` → SDR ledger | CodeGraph does not index Markdown at all |
| How do the two sides of a wire contract stay in sync | **The contract-chain table in §4** — then read the anchor file | No graph edge exists across the language boundary |
| Exact string, log message, config key, test name | **Grep** | CodeGraph indexes symbols, not string literals |
| File by name pattern | **Glob** | — |

**The hard boundary:** CodeGraph's extension map contains `.rs`, `.ts`, `.vue` and ~40 other code extensions — **not `.md`**. A `codegraph explore "approval requirements"` returns Rust symbols named after approval; it will never surface `docs/specs/approval-requirements.md`. Spec questions go to the file system, not to the graph.

## 2. CodeGraph: the operational facts for THIS repo

Installed at `~/.local/bin/codegraph` (v1.6.2+). The index lives in `.codegraph/` at the repo root — gitignored, ~145 MB, machine-local.

| Fact | Value |
|---|---|
| Full index cost | ~15–20 s cold; 34,782 nodes / 117,775 edges |
| Query cost after that | seconds; no re-index needed |
| Languages linked | Rust, TypeScript, Vue — including **trait → impl dynamic dispatch** (`AppendLogStore` ↔ `JsonlAppendLog`) |
| Languages NOT linked | Markdown, TOML, JSON, shell, YAML |

### Build / refresh

```bash
codegraph init --yes .   # first time, or to rebuild from scratch
codegraph sync .         # after pulling or after a large edit
codegraph status .       # is the index current? is it the right tree?
```

`codegraph sync` is cheap — run it after a pull, not after every edit. Stale results show up as "symbol not found" for code you just wrote.

### The worktree trap — read this before trusting any result

`.worktrees/` is the default place for parallel work in this repo, and `.codegraph/` resolution **walks up to the nearest parent index**. A query run from `.worktrees/<name>/` therefore silently answers from the **main checkout's** tree — a different branch. Symbols that exist only in your worktree are invisible, and symbols you deleted still appear.

This repo's CodeGraph version detects the situation and warns:

> ⚠ This CodeGraph index belongs to a different git working tree.

When you work inside a worktree, build a worktree-local index once:

```bash
cd .worktrees/<name> && codegraph init --yes .
```

Do not "fix" a stale answer by re-running the same query from the worktree root — that is the borrowed index again.

### Query shapes that pay off in this repo

| Intent | Call |
|---|---|
| Understand a flow | `codegraph explore "how wire log ops are recorded and replayed"` |
| Pin one symbol + its callers | `codegraph node <SymbolName> --include-code` |
| Who calls this (dedupes overloads) | `codegraph callers <symbol>` — narrow with `--file` when it reports "N distinct definitions" |
| Blast radius before an edit | `codegraph impact <symbol> --depth 2` |
| Tests that must run | `codegraph affected <changed-file>` |

Ask in natural language ("how does X handle Y"), not single generic words — `explore "agent"` returns noise, `explore "how a subagent is dispatched and resumed"` returns the flow.

## 3. Specs are the other half of the truth

CodeGraph answers "what does the code do". It cannot answer "what did we decide, and why". That lives in `docs/specs/`, governed by `docs/specs/AGENTS.md`:

| File | Holds | Read it when |
|---|---|---|
| `{domain}-requirements.md` | User stories (US), acceptance criteria, and the **SDR** decision ledger | "What is the required behavior" / "was this decided" |
| `{domain}-architecture.md` | Technical shape and the **ADR** decision ledger | "How is it built" / "why this mechanism" |
| `{domain}-context.md` | Current-state inventory with a snapshot date | "What exists today" (drifts — check the date) |
| `{domain}-ROADMAP.md` | Open items and status | "What is still open" |
| `glossary.md` | Canonical definitions of cross-domain terms | Any term that spans domains |

### The SDR/ADR numbering trap

**SDR and ADR numbers are domain-local, not global.** `SDR-001` exists in 31 different requirements files; `ADR-001` in 30 architecture files. 266 SDRs and 289 ADRs total.

A bare `grep SDR-001 docs/specs/` returns 31 unrelated decisions. Always qualify with the domain — `approval/SDR-001` (question-interaction ownership) is a different decision from `session-management/SDR-001`. When citing one, write the qualified form.

## 4. The multi-projection chains — where the graph has no edges

Several contracts here are "one source of truth, N projections, miss one and the feature dies silently". `rust/AGENTS.md` enumerates them as hard rules. CodeGraph cannot help: the projections are linked by **string equality**, not by import, so there is no edge to traverse. Use the table, then read the anchor file.

### 4.1 Event vocabulary (47 canonical names)

| Role | File |
|---|---|
| **Source of truth** | `rust/crates/mirri-core/src/session/event_projection.rs` (`event.*` methods) |
| Web projection | `apps/mirri-web/src/api/daemon/mappers.ts` (`toAppEvent` switch) |
| Web reducer | `apps/mirri-web/src/api/daemon/eventReducer.ts` |
| Embedded server relay | `rust/crates/mirri-embedded/src/embedded_server/event_relay.rs` |
| ACP translation | `rust/crates/mirri-acp/src/acp_handler.rs` |
| TS wire types (local re-implementation) | `packages/protocol/src/events.ts` |

An event missing from `mappers.ts`'s switch falls into the `default:` branch and becomes `{ type: 'unknown', raw }` — the web UI silently does nothing. As of the last measurement, 27 of the 47 names had a `case` arm; the remainder are either handled by the projector or genuinely unprojected. **When adding an event, diff the projection source against `mappers.ts` — do not assume.**

This chain is also written down as contract #3 in `rust/AGENTS.md` — that is the normative statement; the table above is the same list in working order.

### 4.2 RPC method surface (293 methods)

| Role | File |
|---|---|
| **Machine-readable source of truth** | `rust/crates/mirri-proto/src/rpc/authoritative-methods.tsv` (13 columns, 293 rows) |
| Rust registry | `rust/crates/mirri-proto/src/rpc/registry.rs` (`METHODS`) |
| Parity gate | same file, test `should_agree_with_the_authoritative_method_table` |
| Web client | `apps/mirri-web/src/api/daemon/ws-rpc-api.ts` |
| NAPI exports | `rust/crates/mirri-node/src/napi_exports.rs` |

The TSV ⇄ `METHODS` pair is already enforced by a test — that is the model to copy for any new contract you need to pin. When you change a method name, all five links move together or the parity test goes red.

### 4.3 Wire log state domains

Any new session state domain (goal / cron / permission / plan / tool set) needs **both** a write side (`record_op`) and a reducer recovery path. A reducer branch with no producer is dead code — the feature does not exist. Anchor: `rust/crates/mirri-core/src/scope/session_scope.rs` (`SessionWireLog`, `WireOpRecorder`).

## 5. Traps

- Grepping for a symbol when the question is structural ("who calls this") — the graph answers in one call; grep needs a manual trace.
- Querying CodeGraph for anything in `docs/specs/` — Markdown is not indexed.
- Trusting `codegraph status` after a branch switch without reading whether the tree matches — see the worktree trap.
- Searching `docs/specs` for an unqualified `SDR-###` — 31 false positives.
- Assuming an event reaches the web because it exists in `event_projection.rs` — check `mappers.ts` (§4.1).
- Re-indexing the whole repo because one query looked wrong — `sync` is usually enough; a wrong *tree* is the worktree trap, not staleness.
