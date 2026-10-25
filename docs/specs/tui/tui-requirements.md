# Rust TUI — Domain Requirements Overview

> Status: approved design baseline. This is the entry point of the TUI requirements suite. Each functional domain has its own `*-requirements.md` in this directory, paired with an architecture doc; shared definitions live in `docs/specs/glossary.md`; the authoritative parity capability matrix and research evidence live in `docs/specs/tui-context.md` (one level up); roadmap/status in `docs/specs/tui-ROADMAP.md`.

## 1. Product goal

A Rust-native terminal interface for Mirri Code, at capability parity with the current TypeScript TUI (minus explicitly deferred items), that a developer runs all day inside a workspace without leaving the keyboard. It replaces the TS TUI as the primary terminal client of the rust engine.

**Positioning**: a power-user terminal client — keyboard-first, information-dense but calm, where reviewing what the agent did (diffs, tool output, thinking) is a first-class flow, not an afterthought.

## 2. Users and primary scenarios

- **P1 — daily driver**: long-running session, watches streaming output, answers approvals, queues follow-ups mid-stream, detaches long tasks to background, returns hours later to a resumable session.
- **P2 — reviewer**: joins a session mainly to review what changed (diffs, plan, todos) and steer direction.
- **P3 — first-timer**: opens the tool with no model configured; must reach a working state through in-product guidance.

## 3. Scope sources and domain map

Product scope has three sources, in priority order: (1) the **parity set** (capability matrix in `docs/specs/tui-context.md`, a living inventory re-audited when the TS TUI changes), (2) **borrowed best practice** from competitor research (evidence pack in `docs/specs/tui-context.md`), (3) **deliberate simplifications** (SDR-002 in this file, never implicit).

| Domain doc | Contents | Requirement domains |
|---|---|---|
| `interaction-requirements.md` | Editor, input modes, shortcuts, queue/steer, notifications, terminal hygiene | Input & editor · Terminal integration |
| `conversation-requirements.md` | Transcript, streaming, markdown/diff/media rendering, thinking, tool cards, replay | Conversation rendering |
| `trust-requirements.md` | Approvals, agent questions, permission modes, plan mode | Approvals & questions |
| `navigation-requirements.md` | Sessions, background tasks, sub-agents/swarm, goals | Sessions · Background tasks & agents · Goals |
| `command-requirements.md` | Slash commands, settings, chrome (welcome/footer/todo/themes), first-run | Slash commands & settings · Chrome & personalization · First-run & connection |

Non-functional experience requirements (N1–N7 below) apply across all domains.

## 4. Cross-domain non-functional requirements

| # | Requirement |
|---|---|
| N1 | Responsive feel: keystrokes echo without perceptible lag under streaming; render work is coalesced; a long session does not degrade scrolling/input |
| N2 | Calm rendering: no flicker on streaming updates; diff previews suppress transient artifacts mid-stream |
| N3 | Keyboard-only reachability of every capability; visual focus is always obvious |
| N4 | Discoverability: every command is reachable through `/help` and completion; hints rotate in the footer |
| N5 | Consistency: dialogs share one layout language (header/body/footer, Esc-cancels, numbered choices); new dialogs must reuse it |
| N6 | Single-language UI copy (English), matching the TS TUI's current state; localization is a future decision |
| N7 | Testability: "automated tests green" must be a reliable predictor of "works for the user" (mechanism in `tui-architecture.md` at the specs root) |

## 5. Product-level acceptance

- The five core journeys pass end-to-end via keyboard only: start-unconfigured → configure → converse; converse with streaming + approvals; queue/steer mid-stream; detach → background → reattach/stop; switch sessions with replay.
- Every capability in the parity matrix (`docs/specs/tui-context.md`) is reachable and functional — matrix sweep is the release gate for parity.
- All dialog flows are single-modal, queue-coordinated, and never stack.
- Terminal restoration verified on all catchable exit paths; no artifacts remain.
- N1–N6 verified per their definitions; N7 against the architecture test layering.

## 6. Decision records

### SDR-001 (2026-10-24): Rust TUI baseline

- Context: terminal client needs to move to the rust engine stack.
- Decision: build `mirri-tui` as a rust crate at capability parity with the TS TUI, launched via `mirri tui`.
- Rejected: extending the TS TUI (does not serve the rust-first direction); a web-only client (terminal is a primary surface for the target user).
- Impact: all domains; root architecture doc ADR-001/002/003.

### SDR-002 (2026-10-24): Scope cut — first release

- Context: full parity plus every competitor highlight exceeds one release.
- Decision: defer (a) alternate-screen full mode, (b) advanced diff side-panel/IDE-grade diff viewer, (c) localization, (d) mouse-first interaction, (e) leader-key/command-palette discoverability model (keep `/help` + completion as the discovery surface).
- Rejected: cutting deeper (breaks "replace the TS TUI" positioning); cutting nothing (unshippable).
- Impact: `interaction-requirements.md`, `conversation-requirements.md`, N6; deferred items are open decisions, revisit at post-parity review.

### SDR-003 (2026-10-24): Editor is readline-style, not vim-modal

- Context: early drafts said "vim-style editor"; capability inventory shows the TS TUI editor is readline-style, and competitor research shows the winning interaction model for this product category is low-friction input (steer, queue, bash mode) with vim reachable via the external editor.
- Decision: readline-style multi-line editor with the full shortcut set; vim users use Ctrl-G external editor.
- Rejected: vim modal editor (higher build cost, against the parity target and category practice); single-line input (inferior for long prompts).
- Impact: `interaction-requirements.md`; root architecture module layout.

### SDR-004 (2026-10-24): Requirements suite split by functional domain

- Context: a single requirements file could not carry the full product definition (parity set spans 10 functional domains; ~40 slash commands; 20+ dialogs) at reviewable depth; the first-cut single file was judged too thin against the product's size.
- Decision: split requirements and architecture into per-domain documents under `docs/specs/tui/`, with this overview as the entry point carrying goal, users, NFRs, product-level acceptance, and the SDR log; matrix rows in `docs/specs/tui-context.md` carry per-domain row IDs (A–J), each row mapping to exactly one requirement domain (domains cover multiple rows).
- Rejected: one monolithic file (unreviewable, merges unrelated domains); per-capability files (too granular, cross-domain flows fragment).
- Impact: this suite layout; `docs/specs/tui-context.md` domain IDs.
