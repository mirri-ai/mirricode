# TUI Interaction & Terminal Integration — Architecture

> Domain architecture of `tui-architecture.md` (cross-domain). Requirement source: `docs/specs/tui/interaction-requirements.md`. Parity row: A, I in `docs/specs/tui-context.md`.

## 1. State slice

```rust
// app.rs slice
struct EditorState {
    mode: InputMode,            // Prompt | Bash
    draft: DraftBuffer,         // textarea content + paste markers + media placeholders
    history: InputHistory,       // persisted, mode-filtered
    ghost_hint: Option<String>, // slash argument placeholder
}
struct InteractionState {
    editor: EditorState,
    queue: VecDeque<QueuedMessage>,
    cancel_ladder: CancelLadder,     // which Ctrl-C stage applies
    exit_confirm: Option<ExitKind>,  // Ctrl-C / Ctrl-D window with deadline (injected clock)
    external_editor: ExternalEditorState,
}
```

- Paste markers and media placeholders are spans in the draft, not separate state; the editor view resolves them.
- Exit-confirmation windows use the injected `Clock` (hard rule 1): deadline comparison is against an injectable now.

## 2. Keymap architecture

- `input/keymap.rs`: a declarative table of (context, key) → `Msg`. Contexts: `Editor`, `Dialog`, `PanelScroll`, `Global`. Priority: Dialog > Panel > Editor > Global.
- The Ctrl-C cancel ladder is a reducer state machine, not keymap logic: each `CancelRequested` msg advances one stage; the stage list mirrors `interaction-requirements.md` §3 order.
- `/help` content is **generated from the same keymap table** at build time — drift is structurally impossible (requirement: zero drift).

## 3. Editing (ratatui-textarea integration)

- `ratatui-textarea` provides the buffer/caret/undo core; readline semantics wrap it: history navigation swaps the buffer (saving the in-progress draft separately), mode filtering partitions one persisted history store.
- Bash mode is a presentation + history filter + submission router: a bash submission becomes a `RunShell` command instead of `SendMessage`.
- Completion: three providers (slash registry, path walker, `@` mention) behind one `Completer` trait; the active provider is chosen by trigger char and mode. Slash entries carry availability computed from state (idle checks), matching dispatch gating.
- Paste handling: bracketed paste from crossterm; oversized pastes collapse to markers via a size threshold; Ctrl-V on a marker span expands it. Media paste reads the clipboard once per hint lifetime (footer hint state lives in chrome domain).
- External editor: suspends rendering, spawns `$VISUAL`/`$EDITOR` via a `ProcessRunner` injected through `TerminalIo`-adjacent ports, restores on exit; the temp file round-trip is the contract.

## 4. Queue and steer

- Queued submissions produce `EnqueueMessage`; the queue panel view reads `queue` directly.
- `Steer` msg: reducer merges draft + queue into one `SteerCommand` (sent via `EngineClient::steer`); rejected for bash drafts (reducer returns a notice msg).
- After turn end or compaction end, the reducer drains the queue into ordered `SendMessage` commands.

## 5. Notifications

- `notify.rs`: an outgoing port (`NotificationSink` trait) with a production impl writing OSC 9 / BEL (with multiplexer passthrough detection) and a test sink recording events.
- Dedup by event key; gating by focus condition (tracked via terminal focus events when available) + config toggle; fired from the reducer as a `Command`, never as a side effect inside `update` (hard rule 1).

## 6. Terminal hygiene

- `term/mod.rs` owns enter/leave of raw mode and the panic hook installation at startup; every exit path funnels through one `restore()` (idempotent).
- Catchable signals (SIGTERM/SIGINT/SIGHUP) are routed into the loop as msgs, not acted on in handlers — the handler only enqueues.
- OSC 133 emission happens in the draw path around transcript region boundaries; the vt100 test parser asserts they are present but invisible.
- Size guard: a `MinSize` check in the view root renders the blocking notice below 80×24.

## 7. Test anchors (cross-domain layers in `tui-architecture.md` §4)

| Requirement | Layer | Anchor |
|---|---|---|
| Cancel ladder order | L1 | reducer given `CancelRequested` at each ladder state |
| Exit confirmation windows | L1 | injected clock: inside/outside window |
| Keymap table dispatch (every §3 binding routes to its domain handler) | L1 | table-driven dispatch test over the keymap table; per-key behavioral anchors live in their owning domains (steer→§4, detach→navigation, expand→conversation) |
| Queue/steer semantics | L1 + L3 | reducer; loop-level: queued msg sends after turn-end event |
| Queue release after compaction | L1 + L3 | reducer on compaction-completed event; loop-level scripted compaction |
| Notification firing (turn/approval/question, once each) | L1 | sink recording per event kind |
| Help/keymap zero-drift | L2 | generated help snapshot |
| Editor views (modes, ghost hints, markers) | L2 | 80-col + 40-col snapshots |
| Paste/IME/media flows | L3 | event-injected through TerminalIo |
| External editor round-trip | L3 | fake ProcessRunner |
| Terminal restoration on all catchable exits | L4 | exit-code + vt100 screen residue assertions |
| OSC 133 markers present, no artifacts | L4 | vt100 parser check |
