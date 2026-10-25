# Rust TUI — Current-State Context

> Snapshot: 2026-10-24. This file inventories what exists today; it holds no decisions or goals (those live in `tui/tui-requirements.md` (overview) / `tui-architecture.md`).

## Environment facts

- Rust toolchain: rustc/cargo 1.95.0; workspace edition 2024 (`rust/Cargo.toml`).
- `rust/crates/` contains 17 crates; no TUI crate exists yet. `mirri-cli` is a non-interactive CLI (`mirri` binary).
- The TS TUI lives at `apps/mirri-code/src/tui/` (184 TS files; `tui-state.ts` reducer-style state, controllers, components under `components/{chrome,dialogs,editor,media,messages,panes}`) — the parity reference for the Rust TUI.
- Existing test infrastructure reusable by the TUI: `rust/crates/mirri-test-harness` (MirriHarness trait, HTTP/WS client, mock LLM factory), contract tests at `tests/cross_backend_contract.rs`.
- Engine event vocabulary single source: `rust/crates/mirri-core/src/session/event_projection.rs`; the client translation layer for the TS side is `packages/node-sdk/src/rust-engine/translate.ts`.
- Command registry (TS): `apps/mirri-code/src/tui/commands/registry.ts` — builtin slash commands with availability gating and priority ordering (row-level list in matrix G below).

## Registry data for selected dependencies (verified 2026-10-24 via crates.io/GitHub API)

| Crate | Version | Recent downloads | Last update |
|---|---|---|---|
| ratatui | 0.30.2 | ~23.5M | 2026-06 |
| ratatui-crossterm | 0.1.2 | (backend split crate) | 2026-06 |
| crossterm | 0.29.0 | ~53.5M | 2025-04 |
| ratatui-textarea | 0.9.3 | ~621k | 2026-10 |
| insta | 1.49.0 | ~34.5M | 2026-10 |
| vt100 | 0.18.1 | ~7.1M | 2026-10 |
| tui-textarea | 0.7.0 | ~722k | 2024-10 |
| tuirealm | 4.1.0 | ~44k | 2026-05 |

> `tui-textarea` (dormant) and `tuirealm` are listed for comparison only; the selected editor widget is `ratatui-textarea` — see `docs/specs/tui-architecture.md` D5.

ratatui 0.30 split backends into `ratatui-crossterm` / `ratatui-termion` / `ratatui-termwiz` and widgets/macros into separate crates.

## Parity capability matrix (authoritative — re-audit when the TS TUI changes)

> The authoritative definition of "parity" referenced by `tui/tui-requirements.md` §3. Full row-level inventory taken from the TS TUI source on 2026-10-24 (184-file audit). Re-audit whenever the TS TUI gains or loses a capability.

### A. Input & editor

| Capability | TS source |
|---|---|
| Readline multi-line editor (wrap, history ↑↓ mode-filtered, persisted) | `components/editor/custom-editor.ts` |
| Bash mode `!` (prompt/border/history separation, backspace-exit, paste auto-enter) | same |
| Tab completion: slash commands (2-line wrapped descriptions), paths, `@dir/` mentions | `editor/file-mention-provider.ts` |
| Argument ghost hints after slash commands | same |
| Paste markers `[paste #N]` + Ctrl-V expand; image/video paste placeholders | same |
| Full Ctrl-shortcut set (send, cancel ladder, steer Ctrl-S, detach Ctrl-B, plan Shift-Tab, expand Ctrl-O, todo Ctrl-T, external editor Ctrl-G, undo) | `controllers/editor-keyboard.ts` |
| Exit confirmation windows for Ctrl-C/Ctrl-D | same |
| Double-Esc undo selector (600ms window) | same |

### B. Conversation rendering

| Capability | TS source |
|---|---|
| Assistant markdown + syntax highlight, streaming (coalesced flush) | `components/messages/assistant-message.ts`, `controllers/streaming-ui.ts` |
| Thinking: collapsed dim preview, expandable, empty suppressed | `components/messages/thinking.ts` |
| Tool cards: state glyph, per-tool header wording, numeric chip, collapsible preview (code/diff/命令 live output), truncated tail | `components/messages/tool-call.ts` |
| Read/Agent grouping; step summaries for older steps | `messages/read-group.ts`, `agent-group.ts`, `step-summary.ts` |
| Diff: clustered LCS, gutter, context elision, streaming suppression, expand cap | `components/media/diff-preview.ts` |
| Images: Kitty/iTerm2 inline (≤12 rows, ≤40 cols), placeholder fallback | `components/media/image-thumbnail.ts` |
| Transcript windowing (recent turns kept; hysteresis trim; recent-only expand) | `mirri-tui.ts` constants |
| State markers: compaction (cancel, token before/after), model-switch divider, goal markers, swarm markers, MCP/skill/plugin cards, cron, error/warning notices | `components/messages/*` |
| Session replay (bounded turn limit, input blocked during replay) | `controllers/session-replay.ts` |

### C. Approvals & questions

| Capability | TS source |
|---|---|
| Approval panel: tool-specific headline, display blocks (dedup, cap 5), numbered choices, feedback input | `components/dialogs/approval-panel.ts` |
| Approve once / for-session / reject / reject-with-feedback; for-session auto-apply | `reverse-rpc/approval/*` |
| Dangerous command detection (8 pattern classes) | `reverse-rpc/approval/adapter.ts` |
| Full-screen diff/file preview (Ctrl-E) | `components/dialogs/approval-preview.ts` |
| Single-modal coordination + queued requests (approval/question mutex) | `reverse-rpc/modal-coordinator.ts` |
| Question dialog: multi-question, single/multi/Other, review-before-submit | `components/dialogs/question-dialog.ts` |
| Terminal notification per approval/question/turn (focus-conditioned) | `utils/terminal-notification.ts` |

### D. Sessions

| Capability | TS source |
|---|---|
| Create/resume(picker)/fork/title/retention/export-md/export-debug-zip | `commands/session.ts`, `retention.ts`, `prompts.ts` |
| Startup flags (`--continue`, `--session`, picker mode), wrong-directory guidance | `mirri-tui.ts init()` |
| Replay-driven switching (transcript/todo/background rebuild) | `controllers/session-replay.ts` |

### E. Background tasks & agents

| Capability | TS source |
|---|---|
| Ctrl-B detach; footer badges split tasks/agents | `controllers/editor-keyboard.ts`, `chrome/footer.ts` |
| `/tasks` browser: filter, output tail, stop with confirm, full viewer, 1s refresh | `controllers/tasks-browser.ts`, `components/dialogs/tasks-browser.ts`, `task-output-viewer.ts` |
| Sub-agent cards (phases, sub-tools, token/error); swarm progress grid | `messages/agent-group.ts`, `agent-swarm-progress.ts`, `controllers/subagent-event-handler.ts` |

### F. Goals

| Capability | TS source |
|---|---|
| Lifecycle: create/permission-prompt/status/pause/resume/cancel/replace | `commands/goal.ts`, `dialogs/goal-start-permission-prompt.ts` |
| Queue: append/reorder/edit/delete/persist; auto-advance | `goal-queue-store.ts`, `controllers/session-event-handler.ts` |
| Footer badge (state, elapsed, turns); completion card; markers | `chrome/footer.ts`, `messages/goal-*` |

### G. Slash commands & settings

> Row-level builtin command list (authoritative; verified against `apps/mirri-code/src/tui/commands/registry.ts` + dispatch handlers, 2026-10-24):

| Command (aliases) | Effect | Availability |
|---|---|---|
| `/new` (`clear`) | start fresh session | idle-only |
| `/sessions` (`resume`) | session picker | always |
| `/retention` (`archive`) | archived-session restore/delete | always |
| `/fork [turn]` | fork session copy and switch | idle-only |
| `/title <t>` (`rename`) | set/show session title | idle-only |
| `/compact [instruction]` | compact context | idle-only |
| `/undo` | withdraw last prompt (picker; also double-Esc) | idle-only |
| `/exit` (`quit`,`q`) | exit | always |
| `/version` | version info | always |
| `/plan [on\|off\|clear]` | plan mode toggle / clear | idle-only (clear) |
| `/permission` | permission mode picker | always |
| `/yolo` (`yes`) / `/auto` | permission mode toggles | always |
| `/model [alias]` | model picker (provider tabs, cache warning) | idle-only |
| `/effort` (`thinking`) | thinking effort picker | idle-only |
| `/provider` (`providers`) | provider manager (add/del/refresh, API key) | always |
| `/agents` (`agent`) | agent profiles toggle | idle-only |
| `/btw <agent> <prompt>` | side-question panel | always |
| `/swarm [on\|off] \| <task>` | swarm mode / run | idle-only |
| `/tasks` (`task`) | background task browser | always |
| `/settings` (`config`) | settings hub | always |
| `/theme [name]` | theme select/apply | always |
| `/editor [cmd]` | external editor select | always |
| `/experiments` (`experimental`) | experiment flags | idle-only |
| `/reload` | reload runtime + UI config | idle-only |
| `/reload-tui` | reload UI config only | always |
| `/goal [status\|<objective>\|pause\|resume\|cancel\|replace <o>\|next <o>\|next manage]` | goal lifecycle + queue | idle-only (create/replace) |
| `/skills [reload]` | skill catalog reload/status | always |
| `/plugins [reload]` | plugin/MCP manager | always |
| `/mcp` | MCP status panel | always |
| `/help` (`h`,`?`) | shortcuts + commands | always |
| `/usage` | tokens/context/quota panel | always |
| `/status` | session/runtime status | always |
| `/feedback` | open issue tracker | always |
| `/copy` | copy last assistant message | always |
| `/export-md [path]` (`export`) | export session markdown | always |
| `/export-debug-zip` | export debug zip | always |
| `/add-dir [list\|<path>]` | extra workspace dirs | idle-only |
| `/init` | generate AGENTS.md via model turn | idle-only |
| `/web` | open session in web UI (confirm) | always |

| Capability | TS source |
|---|---|
| Skill/plugin commands share the slash surface (namespaced; unknown `/word` → model message) | `commands/plugin-commands.ts`, `resolve.ts`, `parse.ts` |
| `/settings` aggregation; selector dialogs | `components/dialogs/*-selector.ts` |
| Model switch cache-warning + session-only choice | `dialogs/model-selector.ts`, `tabbed-model-selector.ts` |
| Help panel (shortcuts + commands) | `components/dialogs/help-panel.ts` |

### H. Chrome & personalization

| Capability | TS source |
|---|---|
| Welcome panel (logo, info rows, narrow degrade, unauthored guidance) | `components/chrome/welcome.ts` |
| Footer: badges/model/goal/bg counts/cwd/git/context-usage/hints/transients | `components/chrome/footer.ts` |
| Todo panel (smart collapse ≤5, expand) | `components/chrome/todo-panel.ts` |
| Spinners (moon/braille) + activity pane + loading tips | `chrome/moon-loader.ts`, `panes/activity-pane.ts` |
| Queue panel (edit hint, steer hint) | `panes/queue-pane.ts` |
| BTW side-question panel | `controllers/btw-panel.ts`, `panes/btw-panel.ts` |
| Banner (remote tips, once/cooldown, safe-fail) | `banner/*` |
| Themes: dark/light/auto (live terminal tracking), custom JSON loader + schema | `theme/*` |
| `/dance` easter egg (hidden; not acceptance-gated — see `tui/command-requirements.md` §3.4) | `easter-eggs/dance.ts` |
| Clipboard-image footer hint (once per image, debounce) | `controllers/clipboard-image-hint.ts` |

### I. Terminal integration

| Capability | TS source |
|---|---|
| OSC 9/9;4 notifications (focus-gated, tmux passthrough, bell fallback) | `utils/terminal-notification.ts` |
| Graduated Ctrl-C cancel ladder | `controllers/editor-keyboard.ts` |
| Clean shutdown (resume hint) + emergency terminal restore | `mirri-tui.ts stop()` |

### J. First-run & connection

| Capability | TS source |
|---|---|
| Sessionless start; lazy session create on first prompt | `mirri-tui.ts init()`, `cli/run-shell.ts` |
| Provider add flow (catalog → credentials → API-key dialog) | `dialogs/provider-manager.ts`, `api-key-input-dialog.ts` |
| Background model refresh (+N models status; failure notice) | `controllers/auth-flow.ts` |
| Engine reconnect UX (visible state; bounded retry; clean exit) | transport client layer |
| Startup diagnostics (config/terminal-compat notices) | `mirri-tui.ts init()` |

## Competitor research notes (2026-10-24, `../opencode` and `../kimi-code`)

> Evidence pack behind the "borrowed best practice" column of `docs/specs/tui/tui-requirements.md` §3. Facts vs judgments as reported by the research agents.

### opencode (v2.0.30) — borrowed patterns

| Pattern | Where the requirement lands |
|---|---|
| "Awaiting input" sessions surface at top of session picker | `tui/navigation-requirements.md` §1.1 |
| Approvals as a decision flow (inline options, expandable diff, reject-with-reason) | `tui/trust-requirements.md` §3 |
| Consistent dialog language (shared header/body/footer, numeric-key choices) | `tui/tui-requirements.md` N5 |
| Leader-key + command palette discoverability | not adopted in first release (see SDR-002 item (e)) |

Not adopted: three-pane layout with session tabs (scope); web-tree-sitter runtime downloads (operational risk); custom UI framework (the ratified stack is in `tui-architecture.md` — see its rejected-alternatives list).

### kimi-code (sibling product) — borrowed patterns

| Pattern | Where the requirement lands |
|---|---|
| Session auto-title after first turn | `tui/navigation-requirements.md` §1.1 |
| Model-switch prompt-cache loss warning | `tui/command-requirements.md` §2 |
| Background badge split (tasks vs agents) | already parity |
| Steer (Ctrl-S) + queue semantics | already parity |

Observed divergence: kimi-code has device-code OAuth login in-transcript, video input, alt-screen mode, tower orchestration — not first-release scope (see SDR-002 in `tui/tui-requirements.md`).
