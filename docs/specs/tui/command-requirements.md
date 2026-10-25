# TUI Command & Settings Surface; Chrome; First-Run — Requirements

> Domain of `tui-requirements.md` (overview). Covers: slash commands & settings · chrome & personalization · first-run & connection. Parity rows: G, H, J in `docs/specs/tui-context.md`.

## 1. Slash commands

- The full builtin set of the TS TUI is the parity baseline: the row-level list (command, aliases, availability) lives in `docs/specs/tui-context.md` matrix G and is authoritative
- Skills and plugins extend the same surface (namespaced), sharing completion, help, and availability gating
- Commands declare **availability**: idle-only commands are excluded from completion while the agent works and reply with guidance if invoked
- Unknown `/<word>` input routes to the model as a normal message (with the documented exception of the hidden easter egg)
- Argument completion for commands that take paths, ids, or choices

## 2. Settings surface

- `/settings` aggregates all setting panels; each panel is also directly reachable by its command
- **Model & effort**: provider-tabbed model selector; switching models mid-conversation warns that conversation cache will be lost and offers session-only vs persistent choice (borrowed); thinking effort is chosen alongside the model
- **Providers**: add (catalog → credentials → API-key entry in-dialog), delete with inline confirmation, background refresh reporting "+N models"
- **Appearance**: theme selector; external editor selector
- **Behavior**: permission mode, experiments, plugins/MCP, additional workspace directories (`/add-dir` with this-session/remember choice)
- **Info**: usage (tokens, context window, plan quota), status, version, help

## 3. Chrome

### 3.1 Welcome panel

- On start: logo, "Welcome to Mirri Code!", directory/session/model/version rows, MCP summary line
- Degrades to a single line on narrow terminals; rainbow variant easter egg
- When no model is configured: explicit guidance ("Run /provider to get started", model shows "not set")

### 3.2 Footer (two lines, the single status surface)

- Mode badges (auto/yolo/plan/swarm, warning-colored), model + thinking effort, goal badge (state · elapsed · turns), background counts (tasks vs agents), truncated cwd, git branch/dirty/PR badge, context usage % and cache/in/out readout, rotating weighted hints, transient hints (exit confirmations, clipboard image, queue notes) that auto-expire

### 3.3 Panels above the editor

- **Todo panel**: the agent's live task list; collapsed to a smart ≤5-line summary (in-progress first, recent done, oldest pending) with a "+N more" line; Ctrl-T expands
- **Queue panel**: pending messages with edit/steer hints (§2 of `interaction-requirements.md`)
- **Side-question panel** (`/btw <agent> <prompt>`): a detached Q&A thread above the editor — scrollable when long, closable with Esc, follow-ups allowed while idle; running questions block new ones with a visible state

### 3.4 Personalization

- Themes: dark, light, `auto` (follows terminal background live; falls back to dark when the terminal cannot report it), plus custom theme files with a documented schema; invalid values fall back safely
- Remote tips banner: display-once/cooldown semantics; absence of the source is invisible to the user
- `/dance` easter egg: rainbow animation (explicitly not acceptance-gated)

## 4. First-run & connection

- **Sessionless start**: the product opens without a session, login, or model; the welcome panel guides configuration; the first prompt lazily creates the session
- Adding a provider flows entirely in-dialog (catalog → credentials); model list refreshes in the background with status-line reporting
- **Engine disconnection**: automatic reconnection with a visible "reconnecting" state; if bounded retries fail, an explicit error and a clean exit (terminal restored)
- Startup diagnostics (config warnings, terminal-compat warnings) surface as notices, never blocking the editor

## 5. Domain acceptance

- Every command in matrix G (per its row: name/alias/availability) executes its documented effect; idle-only commands correctly gate mid-stream
- All settings panels round-trip: change → persists → survives restart (`/reload` applies runtime settings without exit)
- Footer and welcome render every element of §3.1–3.2 including degradation on narrow widths and auto theme fallback
- First-run journey from bare state to first assistant reply completes keyboard-only, including provider/API-key entry
- Kill the engine process: the TUI shows reconnecting, then the failure error, then exits with a clean terminal
