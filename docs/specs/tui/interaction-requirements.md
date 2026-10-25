# TUI Interaction & Terminal Integration — Requirements

> Domain of `tui-requirements.md` (overview). Covers: input & editor · terminal integration. Parity rows: A, I in `docs/specs/tui-context.md`.

## 1. The input surface

The editor is the single place where the user talks to the product. It must feel instant, never lose text, and degrade gracefully when the user pushes it hard (long pastes, images, mid-stream submissions).

### 1.1 Editing model

- **Readline-style multi-line editor** (SDR-003): free multi-line text, wrapped display; Enter submits; Shift-Enter / Ctrl-J insert a newline
- Caret movement, selection, word-wise jumps, undo (Ctrl-/-Ctrl-_) behave like a conventional terminal editor — no surprise to muscle memory
- Input history: ↑/↓ browses past submissions, persisted across sessions
- Ghost hints: after a slash command, a dim placeholder shows the expected arguments

### 1.2 Two input modes

| Mode | Entry | Look | History |
|---|---|---|---|
| Prompt (`>`) | default | neutral border | all prompt submissions |
| Bash (`!`) | type `!` on an empty prompt (or paste `!`-leading text); leave by backspacing the `!` or submitting | `!` prompt + distinct border color | only bash submissions |

A bash submission runs as a shell command: the transcript shows a run card with the command, live output tail, elapsed timer, and a hint to detach it.

### 1.3 Completion and mentions

- `/` triggers slash-command completion with descriptions (wrapped to ≤2 lines); commands that are unavailable right now (streaming/compaction) are excluded
- Tab completes paths; `@` triggers file/directory mentions across workspace roots
- In bash mode, `/` falls back to path completion

### 1.4 Paste and media

- Large pastes collapse into `[paste #N]` markers; pressing Ctrl-V with the caret on a marker expands it in place; an empty prompt plus a paste replaces the buffer
- Pasting an image or video (Ctrl-V; Alt-V on Windows) inserts a `[image #N (W×H)]` placeholder; the media uploads in the background and never blocks typing
- When an image sits in the clipboard and the model supports images, the footer shows a one-time hint "Image in clipboard · Ctrl+V to paste" — once per image, not on every focus

### 1.5 External editor

- Ctrl-G opens `$VISUAL`/`$EDITOR` with the current draft in a temp file; accepting the editor result replaces the draft
- If neither variable is set, a friendly inline error explains how to configure one

## 2. Submitting while the agent is busy: queue vs steer

Two complementary paths when the agent is mid-response (definition of both terms in `docs/specs/glossary.md` — `steer`):

- **Queue**: pressing Enter mid-stream enqueues the message; the queue panel lists pending messages with `▶`; ↑ on an empty editor recalls the last queued message for editing; queued items send automatically when the current work finishes (or after compaction completes)
- **Steer (Ctrl-S)**: injects the current draft (plus any queued messages) immediately as a course correction; bash submissions cannot steer; an empty draft does nothing

## 3. Global shortcuts

| Key | Behavior |
|---|---|
| Enter / Shift-Enter | submit / newline |
| Ctrl-C | graduated cancel: interrupt in-flight work → close top panel → cancel compaction → stop the stream → clear the editor → (with confirmation window) exit |
| Ctrl-D | exit, with confirmation window when the editor is empty |
| Ctrl-S | steer |
| Ctrl-B | detach the foreground task to background |
| Shift-Tab | toggle plan mode |
| Ctrl-O | expand/collapse tool output, thinking, plan, compaction (recent turns only) |
| Ctrl-T | expand the todo panel (when it overflows) |
| Ctrl-G | external editor |
| Double-Esc (within ~600ms) | undo selector |
| ↑ / ↓ | history on non-empty editor; panel scroll / queue recall on empty |

- Every shortcut is listed in `/help`; help content and the actual bindings must never drift (single source)
- Ctrl-C/Ctrl-D exit confirmations show a transient footer hint and expire

## 4. Notifications

- Terminal notification fires once per: approval requested, agent question asked, turn completed
- Respects the focus condition (only when unfocused, by default) and a global on/off toggle
- Delivered as a desktop notification where the terminal supports it (including through terminal multiplexers), otherwise a bell

## 5. Terminal integration & hygiene

- Rendering targets the terminal's native scrollback (regular mode); alternate-screen full mode is deferred (SDR-002)
- Transcript region markers (OSC 133 — invisible markers that let external terminal tools recognize where each conversation region starts and ends) are emitted where the terminal supports them; they never leak visible artifacts
- **Terminal hygiene on catchable paths**: on every catchable termination path — normal exit, panic, catchable signals, failed engine reconnection — the terminal is restored to a usable state (no stuck alternate screen, raw-mode leaks, or ghost UI). SIGKILL-class kills are inherently out of reach
- Minimum size: below 80×24 a blocking "terminal too small" notice; above it, graceful degradation ordering (media placeholders before text)
- Clean shutdown prints the session resume hint ("run `mirri -r <id>`")

## 6. Domain acceptance

- All shortcuts from §3 behave per table, including the full Ctrl-C cancel ladder in order
- Queue and steer are distinct and both work mid-stream; queued messages survive compaction and send after it
- A turn completion, an approval request, and a question request each fire exactly one notification
- Pasted multi-line text, images, and mixed content never block input or lose text
- On each catchable exit path, an observer sees a clean prompt with no artifacts
- `/help` lists every binding in §3 with zero drift
