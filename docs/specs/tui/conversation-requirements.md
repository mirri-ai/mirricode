# TUI Conversation — Requirements

> Domain of `tui-requirements.md` (overview). Covers: conversation rendering. Parity row: B in `docs/specs/tui-context.md`.

## 1. What the transcript is for

The transcript is the product's main reading surface: it must let the daily driver scan hours of agent work quickly, and let the reviewer audit what actually happened. Dense but calm — anything repetitive or low-signal collapses; anything risky or informative stays visible.

## 2. Message rendering

### 2.1 User and assistant messages

- User submissions render with a distinct bullet; echoed shell commands render with `$ cmd`
- Assistant text renders as markdown with syntax-highlighted code blocks; streaming text updates without flicker (coalesced refresh) and finalizes into the settled form
- An assistant message that is still arriving is visually distinguishable (transient style) from a finalized one

### 2.2 Thinking

- Live: an inline spinner with a short preview tail
- Settled: dim italic block, collapsed to a preview (≤2 lines); expandable with Ctrl-O; empty thinking renders nothing

### 2.3 Tool cards

Every tool call renders as a card:

- **State glyph**: running / done / error / truncated
- **Tool-specific headline**: e.g. "Ran a command", "Wrote file", "Searched codebase"
- **Numeric chip** summarizing the outcome: `+N -M` for edits, `N lines` for reads, `N matches` for searches, `N results` for web fetches; tools from connected servers show their origin
- **Collapsible preview body**: full code with line numbers for writes; clustered diff for edits; command + live output for bash; plan box for plan exits; per-tool summaries for reads/searches; long raw outputs show a tail view with expand
- **Streaming previews**: while the arguments are still arriving, the card shows what it can (the file being edited, the command being typed) — never a blank wait

### 2.4 Grouping and folding

- Repeated low-signal calls within a turn (several reads, repeated thinking rounds) collapse into a group or summary line ("… thought 5 times, called 50 tools")
- The transcript keeps a bounded window of recent turns; older turns roll off; only recent turns respond to expand (a global Ctrl-O toggle)

### 2.5 Diff and media

- Diffs: colored add/remove, line gutters, unchanged-context elision with counts, streaming-safe (no red-wall flicker while the edit streams in)
- Images: inline via terminal image protocols where supported (bounded size); otherwise a text placeholder. Video/media inputs show placeholders only
- The expand interaction (Ctrl-O) and its cap are shared with thinking/tool bodies

## 3. Session state markers

The transcript also narrates lifecycle, each as a distinct marker/card:

- Compaction: a running card (cancellable) that completes showing token before/after and a summary
- Model switch: divider with the new model
- Goal lifecycle: active/paused/blocked markers and a completion card
- Swarm enter/exit markers; MCP server connections (with tool counts), skill/plugin activations; cron reminders; error/warning notices
- **No silent degradation**: an engine event that cannot be rendered produces an explicit placeholder card — a dropped message is a defect

## 4. Replay

- Resuming/switching to a session rebuilds the visible transcript from history, bounded to the most recent turns, plus the todo state and background task markers
- Input is blocked with a visible replaying state until playback finishes

## 5. Live activity strip

- While the agent works, a slim activity area shows the phase (waiting / thinking / composing / running tools) with a spinner and a rotating tip
- When idle, it takes no space

## 6. Domain acceptance

- A mixed turn (text + thinking + tools + diff + image) renders per §2 with correct collapse states before and after Ctrl-O
- The activity strip shows each live phase (waiting/thinking/composing/tools) while active and takes no space when idle
- Streaming a large edit produces no visible flicker and shows progressive previews
- Every session state marker from §3 can be produced by its engine event and renders (or explicitly placeholders when unknown)
- Replay restores transcript + todos + background markers and unblocks input after finish
