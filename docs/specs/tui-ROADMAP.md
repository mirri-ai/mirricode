# Rust TUI ROADMAP

> Status/progress home for the Rust TUI domain. Requirements suite: `tui/` directory (overview `tui/tui-requirements.md`); architecture: `tui-architecture.md` + `tui/*-architecture.md`; current state: `tui-context.md`.

## Phases

| Phase | Scope | Requirement domains | Status |
|---|---|---|---|
| P0 | Design baseline (requirements + architecture approved) | all | done (2026-10-24) |
| P1 | Crate scaffold, TerminalIo/EngineClient traits, Elm skeleton, L1/L2 harness | cross-domain (`tui-architecture.md`) | not started |
| P2 | Core loop: session list/create/switch, conversation rendering, streaming output (scripted input injection — the editor lands in P3) | conversation, navigation §1–2 | not started |
| P3 | Readline editor (history, completion, bash mode, queue/steer), slash commands | interaction §1–3, command §1–2 | not started |
| P4 | Approval panel + questions + modal queue, plan mode, diff display, background tasks browser | trust, interaction §4, navigation §3 | not started |
| P5 | Goals, BTW panel, swarm grid, themes, notifications, first-run & connection (sessionless start, provider guidance, engine reconnection), terminal hygiene hardening (panic hook, all exit paths) | navigation §4, command §3–4, interaction §5 | not started |
| P6 | L4 e2e journeys, parity sweep against the capability matrix (`tui-context.md`) | all | not started |

## Open items

- Parity capability matrix (`tui-context.md`): row-level inventory in place (A–J); re-audit whenever the TS TUI changes.
- SDR-002 deferred items — (a) alternate-screen full mode, (b) advanced diff side-panel, (c) localization, (d) mouse-first interaction, (e) leader-key/command-palette discoverability: revisit at post-P6 review.
- Migration/retirement decision for the TS TUI after parity: out of scope until post-P6 review.
