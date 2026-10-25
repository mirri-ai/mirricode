# Rust TUI — Cross-Domain Architecture

> Status: approved design baseline (Elm architecture on ratatui; four-layer test architecture). Domain-specific technical designs live in `docs/specs/tui/*-architecture.md`; requirements live in `docs/specs/tui/tui-requirements.md` (overview) and its domain files; current-state inventory in `docs/specs/tui-context.md`.

## 1. Overview and decisions

| # | Decision | Rationale (registry evidence snapshot: `docs/specs/tui-context.md`) |
|---|---|---|
| D1 | New workspace crate `rust/crates/mirri-tui`, launched by the `mirri tui` subcommand | Keeps CLI and TUI decoupled; testable in isolation |
| D2 | **Elm architecture**: `update(state, msg) -> (state, commands)` + `view(state) -> widget tree` | Reducers are pure functions, unit-testable; the TS TUI state model translates rather than being reinvented; validated by peer ratatui products |
| D3 | **TUI is an RPC client** of `mirri-server` via the existing transport (HTTP + WebSocket), peer of web/ACP | Reuses event projection, multi-session, remote capability; no parallel client contract |
| D4 | Rendering: **ratatui** with the `ratatui-crossterm` backend split | De-facto community standard, actively maintained — see the context registry snapshot |
| D5 | Multi-line editor: **ratatui-textarea** | Maintained under the ratatui organization, actively updated; supersedes the dormant `tui-textarea` |
| D6 | Snapshots: **insta**; terminal emulation for tests: **vt100** | Both actively maintained, widely adopted |
| D7 | Requirements/architecture split by functional domain under `docs/specs/tui/` (SDR-004) | Reviewable per-domain docs; this file keeps only cross-domain architecture |

Rejected alternatives (recorded for posterity):

- **tuirealm (declarative framework)** — small ecosystem, single maintainer; disproportionate risk for a full-parity frontend.
- **In-process direct engine calls** — bypasses the shared client contract; duplicates event projection and multi-client sync.
- **Raw imperative rendering without a state layer** — state synchronization across dialogs/streams/views fails at this complexity.
- **tui-rs** — unmaintained since 2023; community migrated to ratatui.
- **iced / termwiz as primary** — iced is a GUI framework; ecosystem resources concentrate on crossterm.

## 2. Module layout

```
rust/crates/mirri-tui/src/
├── app.rs        # App state root: sessions, conversation, editor draft, dialog stack
├── msg.rs        # Message vocabulary; engine event -> Msg translation (4th consumer of event projection)
├── update.rs     # Pure reducer: msg -> state change + optional engine command
├── view/         # Pure view functions: state -> widget tree
├── input/        # Keymaps, readline editing, history, completion, paste handling
├── client/       # EngineClient trait impls (RPC over mirri-transport)
├── term/         # TerminalIo trait + crossterm impl
└── main_loop.rs  # tokio select: input events / RPC events / tick -> redraw
```

Domain mapping: input+terminal hygiene → `input/`+`term/`; conversation rendering & replay → `view/`+`update.rs` transcript slice; trust modality → reverse-RPC queue in `app.rs`+`msg.rs`; navigation (sessions/tasks/goals) → engine commands via `client/`; command/settings/chrome → `view/` chrome module. Per-domain details: `docs/specs/tui/*-architecture.md`.

## 3. Testability architecture — the two hard rules

The product requirement "tests green ⇒ works" (N7 in `docs/specs/tui/tui-requirements.md`) is achieved by architecture, not by test volume. Two rules are **load-bearing and non-negotiable** for all future contributors:

1. **The reducer and view layers perform zero IO and use zero real time.** All waiting goes through injected clocks or paused async time. A reducer's inputs and outputs are only `Msg`/`State`/`Command` values. Violating this makes tests green while real terminals hang.
2. **All engine interaction goes through the `EngineClient` trait; all terminal interaction goes through the `TerminalIo` trait.** UI-logic tests never touch real transports; the production RPC/reconnect logic is tested separately at layer L3.

The only untestable code is the composition of these two trait implementations with the real event loop — kept under a thin `main` composition (<300 lines target).

## 4. Four-layer test architecture

Each layer covers a distinct failure surface; all four green is the formal basis for N7:

| Layer | Tests | Tools | Failure surface covered |
|---|---|---|---|
| **L1** | Reducer unit tests: pure `update` behavior (approval flow, dialog stack, mode switching, event translation) | standard `#[test]` | business logic errors |
| **L2** | View snapshots: `view(state)` rendered via ratatui `TestBackend` at standard and narrow (40-col) widths | insta + TestBackend | layout breakage, truncation, wrap errors |
| **L3** | In-process integration, two forms: (a) full Elm loop driven by scripted input **events** (injected via `TerminalIo`) and a mock `EngineClient`; (b) the **production** RPC `EngineClient` against a scripted fake server, covering reconnection and disconnect handling | vt100 screen-grid assertions; mock `EngineClient` backed by mirri-test-harness mock LLM | loop bugs, deadlocks, dropped frames, ANSI-level defects; production client reconnection logic |
| **L4** | End-to-end: real server + mock LLM, real binary, scripted full user journeys; screen assertions + clean-exit verification | mirri-test-harness + vt100, feature-gated `e2e` | process boundaries: startup, handshake, real crossterm sequences, terminal restoration |

Rules for test content:

- L3/L4 assertions reference **only visible screen text** (via the vt100 screen grid), never internal state fields; state assertions belong to L1.
- Byte-level ANSI parsing is exercised only at L4 (real process over a pty); L3 injects at the event level through `TerminalIo`.
- The main loop is exposed as an injectable `run(...)` taking input/output channels — tests construct it directly instead of spawning processes.
- Test names follow Given-When-Then.
- Snapshot files are committed; changes reviewed via `cargo insta review`.
- L4 cases must additionally assert the TUI process exits cleanly with the terminal restored (exit code + no alternate-screen residue) — the terminal-hygiene requirement in `interaction-requirements.md` §5.

## 5. Commands and CI

```bash
cargo test -p mirri-tui                        # L1+L2 (seconds; run frequently)
cargo test -p mirri-tui --test integration    # L3 (mock engine + production client vs fake server)
cargo test -p mirri-tui --test e2e --features e2e   # L4 (real server; minutes)
cargo clippy -p mirri-tui --all-targets -- -D warnings
```

L4 is feature-gated so the default `cargo test --workspace` run is not slowed. CI runs the same commands as the rust workspace jobs.

## 6. Event flow

```
crossterm input ──┐
                   ├─ tokio select ──▶ translate ──▶ update() ──▶ view() ──▶ draw
engine WS events ──┘ (client task)      to Msg      reducer      pure      TerminalIo
```

- Engine events are consumed from the existing event projection; the client task owns reconnection (bounded retries, then explicit error and clean exit, per `command-requirements.md` §4) and translates events to `Msg` values. Translation failures surface as explicit placeholder messages (conversation domain: no silent degradation), never drops. `msg.rs` is the **fourth consumer** of the event projection (rust/AGENTS.md contract 3).
- Panics are caught by a hook installed at startup that restores the terminal before propagating.

## 7. Decision records

### ADR-001 (2026-10-24): Elm architecture on ratatui

- Context: full TS-TUI parity frontend needs clear unidirectional data flow; third-party declarative frameworks are too small to bet on.
- Decision: ratatui + ratatui-crossterm + self-built Elm state layer (update/view split, message vocabulary).
- Rejected: tuirealm (ecosystem risk), raw imperative rendering (state sync at scale).
- Impact: module layout in §2; test layering in §4.

### ADR-002 (2026-10-24): Four-layer test architecture as the N7 mechanism

- Context: "automated tests green ⇒ all features work" is a property that must be architected; TUIs fail in four distinct places (logic, layout, loop, process boundary).
- Decision: the two hard rules (§3) + four layers (§4); UI code is only accepted behind the `EngineClient`/`TerminalIo` traits.
- Rejected: widget-structure assertions instead of screen-text assertions; testing only through real terminals (flaky, slow).
- Impact: every future mirri-tui contribution must state which layer covers it before review.

### ADR-003 (2026-10-24): L3 dual form — event-level injection and production-client testing

- Context: the main loop cannot consume raw key bytes portably (no public byte-stream parser API in crossterm), and the production RPC/reconnect logic must be exercised without paying the L4 cost of real processes.
- Decision: L3 runs in two forms — (a) the Elm loop driven by input events injected via `TerminalIo` plus a mock `EngineClient`; (b) the production RPC client against a scripted fake server that simulates disconnect/reconnect. Byte-level ANSI parsing is verified only at L4.
- Rejected: byte-level injection at L3 (not portable); leaving reconnection coverage to L4 (slow, feature-gated, weakens N7).
- Impact: §4 L3 row; the `EngineClient` production implementation must be constructible against an arbitrary server endpoint in tests.

### ADR-004 (2026-10-24): Per-domain architecture docs

- Context: SDR-004 split requirements by functional domain; a single architecture file cannot carry ten domains at design depth.
- Decision: this file keeps cross-domain architecture (stack, Elm, test layers, event flow); each functional domain gets `docs/specs/tui/<domain>-architecture.md` holding its technical shapes (state slices, message families, rendering strategy, test anchors).
- Rejected: one monolithic architecture file (same reviewability problem as requirements); architecture merged into requirement files (violates the governance's technical/product language layering).
- Impact: `docs/specs/tui/` suite layout.
