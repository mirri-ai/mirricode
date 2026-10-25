# TUI Conversation — Architecture

> Domain architecture of `tui-architecture.md` (cross-domain). Requirement source: `docs/specs/tui/conversation-requirements.md`. Parity row: B in `docs/specs/tui-context.md`.

## 1. State slice

```rust
// app.rs slice
struct TranscriptState {
    turns: VecDeque<Turn>,      // bounded window (keep recent N turns, hysteresis trim)
    live: LivePhase,             // Waiting | Thinking | Composing | Tool — activity strip source
    replay: ReplayState,        // Idle | Running(progress)
}
struct Turn {
    steps: Vec<Step>,           // recent M steps; older collapsed to StepSummary
}
enum Step { Text{buf}, Thinking{buf}, Tool{call, state}, Grouped{kind, items}, Marker(SessionMarker) }
```

- `Step::Text`/`Thinking`/`Tool` buffers are **delta-accumulating**: engine deltas append; a coalescing flush (tick-driven, no sleeping inside the reducer) materializes a new immutable render model.
- Step collapse (read groups, agent groups, step summaries) is a pure projection of `steps` computed at view time or on finalize — never mutated in place during streaming.

## 2. Rendering strategy

- `view/transcript.rs` renders top-down from `TranscriptState`; each step type maps to a render component (pure functions, no IO).
- Streaming calm (N2): partial text/thinking/tool bodies render from the accumulated buffer at flush boundaries; diff/tool previews suppress transient artifacts by rendering only committed clusters (a step's `is_incomplete` flag defers full diff coloring).
- Tool card shapes: a `ToolCardModel` derived from the call (state glyph, headline word map, chip computation per tool family, preview body selection). Chip math is pure and unit-tested per tool family (Edit `+N -M`, Read `N lines`, …).
- Markdown: a staged pipeline — engine text → markdown AST → styled lines, with a render cache keyed by (content hash, width) so finalized steps are O(1) on redraw; code highlighting maps language → theme colors.
- Diff: LCS-based cluster diff producing (meta, gutter, added, removed) styled lines; unchanged context elision with counts; cap + expand flag shared with Ctrl-O.
- Images: capability detection (Kitty/iTerm2) at startup, injected as a `GraphicsCapability` value; renders inline glyphs or placeholder spans accordingly — capability is a value, never probed mid-view (hard rule 1).

## 3. Windowing and memory

- Constants: keep recent N turns (15) with hysteresis (+5) before trimming oldest whole turns; per-turn recent M steps (30) with older steps folded into `StepSummary`; expand honored only for the recent E turns (3) — values live in one place (`transcript_consts.rs`), mirrored from the TS TUI.
- Render caches bounded by the window; nothing outside the window is rendered.

## 4. Session markers

- `SessionMarker` enum covers: compaction (running/cancelled/complete with token delta), model switch divider, goal lifecycle, swarm enter/exit, MCP connect/fail, skill/plugin activation, cron fired, error/warning notice.
- `msg.rs` translation: each engine `event.*` maps to a marker-producing `Msg` or a step mutation. **Unmapped engine events produce an explicit placeholder marker** — the translation layer is total (no event is dropped; unknown events render as "unsupported event" cards per the no-silent-degradation requirement).

## 5. Replay

- Replay is a reducer mode: `ReplayRunning` blocks editor submission (a notice explains why) and consumes a bounded replay event stream (engine-provided history projection) through the **same translation pipeline** as live events — one code path, so replay fidelity equals live fidelity by construction.
- Replay end emits `ReplayFinished`, unblocking input.

## 6. Test anchors

| Requirement | Layer | Anchor |
|---|---|---|
| Markdown/code/diff/tool card shapes | L2 | per-step-type snapshots (80/40 col), incl. streaming and finalized forms |
| Collapse/grouping rules | L1 + L2 | projection given N reads/thinking rounds; snapshots of group forms |
| Chip math per tool family | L1 | pure chip computation table tests |
| Diff clustering + streaming suppression | L1 + L2 | cluster diff unit tests on crafted inputs; incomplete-step snapshots |
| Windowing constants | L1 | trim/hysteresis boundary tests |
| Marker totality (no dropped events) | L1 | exhaustive event→msg translation test enumerating the projection vocabulary (guards rust/AGENTS.md contract 3 for this consumer) |
| Replay pipeline | L3 | scripted history through the loop; blocked input during, unblocked after |
| Activity strip phases (present while active, no space when idle) | L1 + L2 | reducer phase transitions; snapshots per phase |
| Image capability fallback | L2 | snapshots for inline vs placeholder given injected capability |
