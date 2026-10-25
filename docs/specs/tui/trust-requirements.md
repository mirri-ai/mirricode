# TUI Trust Surface (Approvals, Questions, Permission & Plan Modes) — Requirements

> Domain of `tui-requirements.md` (overview). Covers: approvals & agent questions. Parity row: C in `docs/specs/tui-context.md`.

## 1. Why this domain exists

Every moment the product asks the user to trust it — running a command, writing a file, following a plan — is a trust decision. This domain is designed as a **decision flow, not a blocking box** (borrowed from competitor research): the user must understand what is about to happen in one glance, decide in one gesture, and be able to say why they said no.

## 2. Permission modes (the policy layer)

| Mode | Meaning | Footer badge |
|---|---|---|
| Manual | every risky action is approved individually | (none) |
| Auto | agent acts fully autonomously, still may ask questions | `[auto]` (warning color) |
| YOLO | agent decides what needs asking; mostly hands-off | `[yolo]` (warning color) |

- Switchable at runtime (slash command and settings); plan mode is an orthogonal toggle shown as its own badge
- Mode applies to new actions immediately, in both directions (tighten/loosen)

## 3. The approval panel

When the engine requests approval, a single modal panel replaces the editor:

- **Headline matches the action**: "Run this command?", "Write this file?", "Apply these edits?", "Stop this task?", "Ready to build with this plan?", fallback "Approve <tool>?"
- **Evidence blocks**: the command with its working directory, the diff, the new file content, the URL, the search query — whichever applies, deduplicated, capped at five blocks; anything longer is expandable (Ctrl-O) or viewable full-screen (Ctrl-E)
- **Danger flag**: known destructive shell patterns get a red "Dangerous: …" marker above the block
- **Choices**: Approve once / Approve for this session / Reject / Reject with feedback; feedback opens an inline input; numbered keys and arrows select, Enter confirms, Esc rejects
- "Approve for this session" auto-applies to later requests for the same action in this session
- One notification fires when the panel appears

### 3.1 Never stacking

Approvals and agent questions are **mutually exclusive single modals with a queue**: a burst of requests waits in order; the user never faces overlapping panels. Concurrent identical-action requests benefit from the session-level approve automatically.

## 4. Agent questions

When the agent asks the user structured questions:

- Multi-question forms, single-choice and multi-choice, with an "Other" free-text option
- Numbered choices; Tab moves between questions; a review page ("Review your answer before submit") precedes submission
- Unanswered questions are flagged before submit; Esc dismisses with empty answers
- One notification fires when the dialog appears

## 5. Plan mode

- When on, the agent proposes a plan before acting; the plan renders as a full markdown box inside the approval flow ("Ready to build with this plan?")
- Plan approval offers the engine-provided choices; rejecting can be plain or with revision feedback
- Toggled via Shift-Tab or slash command; badge in the footer

## 6. Domain acceptance

- Each approval headline/block-set choice combination from §3 renders with its evidence and all four decisions work, including feedback capture
- "Approve for this session" suppresses a later identical request without any panel
- Two simultaneous engine requests present sequentially, never stacked
- The question dialog completes a multi-question flow with review, flags unanswered, and submits exact answers
- Danger flag fires for known destructive patterns and does not fire on benign look-alikes (e.g. `rm -rf` in a quoted string is judged by the same rules as the TS TUI)
- Every modal here is escapable and leaves the editor exactly as it was
