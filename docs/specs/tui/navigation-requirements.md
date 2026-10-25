# TUI Navigation (Sessions, Background, Agents, Goals) — Requirements

> Domain of `tui-requirements.md` (overview). Covers: sessions · background tasks & agents · goals. Parity rows: D, E, F in `docs/specs/tui-context.md`.

## 1. Sessions

### 1.1 Lifecycle

- Create a fresh session (`/new`), fork the current one (`/fork [turn]`), rename (`/title`), archive management (`/retention`), export to markdown / debug zip
- Resume through a searchable picker (`/sessions`): session cards show title, relative time, id, directory, last prompt; the current session is marked; compacted sessions are badged; the picker can scope to this directory or all
- Sessions **waiting for user input surface at the top** of the picker (borrowed)
- Session titles auto-generate after the first turn (borrowed); a manual title always wins
- Startup: continue the most recent session in this directory; or pick a specific id; or open the picker. A wrong-directory resume explains what to do (with a copyable resume command) instead of failing silently

### 1.2 Switching

- Switching sessions rebuilds the whole screen: transcript (via replay, bounded), todos, background markers
- The previous session keeps running server-side; nothing is lost by switching

## 2. Background tasks & agents

Two kinds of concurrency exist and must never be conflated in the user's mind:

- **Tasks**: shell/process work detached from the conversation (Ctrl-B detaches the running foreground command into one)
- **Agents**: background sub-agents working on delegated jobs

### 2.1 Visibility

- Footer badges show both counts separately: `[3 tasks running] · [2 agents running]`
- Sub-agent activity renders inside the parent agent card in the transcript: phases, its tool calls (collapsed when many), token/error tails
- A swarm (multi-agent) run renders a per-agent progress grid: name, description, progress bar, state (waiting/thinking/tools/done/failed/aborted)

### 2.2 The task browser (`/tasks`)

- A full-screen browser over background work: list of tasks with filter (all/active), per-task output tail, stop with inline confirmation
- Selecting a task opens a full output viewer; both views refresh live
- Works even when no conversation session is active (with a clear empty state)

## 3. Goals

- **Lifecycle**: create (with a one-time permission prompt when starting under manual/yolo), status, pause, resume, cancel, replace
- **Queue semantics**: queued goals wait for the active goal to complete, fail, or be canceled before starting, in order; replace terminates the active goal and starts the new one immediately; cancel acts on one goal at a time (the active one, or a queued one chosen from the manager)
- **Queue manager**: list, edit, reorder, delete; the queue persists on disk and survives restarts
- **Visibility**: the footer shows the active goal's state, elapsed time, and turn count while running; completion renders a summary card (reason + stats); the next queued goal auto-starts when one completes
- The goal's stop conditions live in its objective text (natural language), enforced by the agent itself

## 4. Domain acceptance

- Full session journey: create → work → switch away → resume (replay) → fork → title → export, all keyboard-only
- Awaiting-input sessions appear above finished ones in the picker
- Detached tasks and background agents show as two distinct badge counts; stopping one never touches the other kind
- Goal lifecycle: create (permission prompt when applicable) → pause → resume → queue a follow-up → complete (auto-advance to the queued one) → cancel a queued item
- The task browser stops a task with confirmation and shows its final output
