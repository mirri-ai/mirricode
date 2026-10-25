# TUI Navigation — Architecture

> Domain architecture of `tui-architecture.md` (cross-domain). Requirement source: `docs/specs/tui/navigation-requirements.md`. Parity rows: D, E, F in `docs/specs/tui-context.md`.

## 1. State slice

```rust
// app.rs slice
struct NavigationState {
    session: SessionMeta,                 // id, title (manual > auto), dir
    session_picker: Option<PickerState>,  // search, scope, page
    background: BackgroundState,          // tasks vs agents, split counts
    goals: GoalsState,                    // active + queue (persisted)
    swarm: Option<SwarmRun>,
}
struct GoalsState {
    active: Option<ActiveGoal>,           // state, started_at (injected clock), turns
    queue: Vec<QueuedGoal>,               // reorder/edit/delete; persisted via engine/store
}
```

## 2. Sessions

- All session operations are engine commands through `EngineClient` (create/fork/rename/list/resume/archive/export); the TUI never manages session storage itself.
- Session picker: paged search delegating to the engine's session list; cards derive from session metadata; **awaiting-input sessions sort to the top** as a stable secondary ordering (primary = search relevance).
- Auto-title: the engine emits a title event; the reducer applies it only when no manual title is set (manual wins, recorded in `SessionMeta`).
- Switch/resume triggers the conversation domain's replay pipeline (transcript/todo/background markers rebuild); wrong-directory resume renders an actionable error notice (copyable resume command via clipboard port).
- Sessionless start: `SessionMeta` is empty; the first submission creates the session lazily (a `Command` from the reducer, invisible to the user beyond the welcome panel updating).

## 3. Background: tasks vs agents (never conflated)

- `BackgroundState` holds two typed collections (shell tasks; agent tasks) with distinct sources (detach command vs sub-agent events); the footer badge renders counts from each separately.
- Task browser (`/tasks`): a full-screen view over `BackgroundState` with its own list/filter/output-tail sub-state; polling is tick-driven through `EngineClient` queries (no background thread in the reducer — hard rule 1); stop goes through an inline confirm sub-state.
- Sub-agent/swarm rendering lives in the conversation domain (agent groups, swarm grid); navigation only owns their **management** surfaces (browser, badges).

## 4. Goals

- Lifecycle commands (create/pause/resume/cancel/replace/queue ops) are engine commands; the engine owns semantics; the TUI owns presentation + the queue manager UI.
- Queue semantics enforced by the reducer presentation contract (waiting list order; auto-advance on completion event); persistence is delegated to the engine-side store (parity with the TS side's persisted goal-queue behavior — see `docs/specs/tui-context.md` matrix F).
- Elapsed time in the footer badge computes from the injected clock at tick boundaries — never wall-clock reads inside the view (hard rule 1).
- Auto-advance: a goal completion event with a non-empty queue triggers the engine's start-next command; the reducer only reflects the resulting events.

## 5. Test anchors

| Requirement | Layer | Anchor |
|---|---|---|
| Picker search/scope/paging + awaiting-input ordering | L1 + L2 | reducer + snapshots |
| Auto-title precedence (manual wins) | L1 | reducer given title event |
| Sessionless → lazy create on first submit | L1 + L3 | reducer; loop-level with mock engine |
| Switch → replay rebuild | L3 | scripted switch through the loop (conversation domain anchors cover fidelity) |
| Badge split (tasks vs agents) | L2 | footer snapshots with both counts present |
| Task browser flows (filter/tail/stop/viewer) | L3 | scripted browser journey |
| Goal lifecycle + queue reorder/edit/delete | L1 | reducer transitions |
| Auto-advance on completion | L1 + L3 | reducer; loop with scripted completion event |
| Elapsed-time badge | L1 | injected clock tick sequence |
