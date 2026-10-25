# TUI Command & Settings Surface; Chrome; First-Run — Architecture

> Domain architecture of `tui-architecture.md` (cross-domain). Requirement source: `docs/specs/tui/command-requirements.md`. Parity rows: G, H, J in `docs/specs/tui-context.md`.

## 1. State slice

```rust
// app.rs slice
struct CommandState {
    registry: SlashRegistry,       // builtin + skill/plugin (namespaced), availability, aliases
    dialog: Option<SettingsDialog>, // any settings picker replaces the editor (mount/restore)
}
struct ChromeState {
    welcome: WelcomeInfo,
    footer_hints: HintCarousel,      // weighted rotation, tick-driven
    transient: OptionDeque<TransientHint>, // auto-expiring (injected clock)
    theme: ThemeState,               // resolved palette + auto tracking
    banner: Option<Banner>,
    clipboard_hint: ClipboardHint,
}
struct ConnectionState {
    engine: EnginePhase,             // Connected | Reconnecting{attempt} | Failed
    provider_refresh: RefreshState,
}
```

## 2. Slash command registry

- `SlashRegistry` is a declarative table: name, aliases, availability predicate (over state), handler `Msg` factory. Builtin entries are static; skill/plugin entries merge at load (and on catalog-change events) with namespacing.
- Dispatch mirrors the TS pipeline: parse → resolve (builtin/skill/plugin/message) → availability check (busy commands reply with guidance) → execute. Unknown `/word` routes to the model; the easter egg claims its documented name before that fallback.
- **The registry is the single source** for: completion list, `/help` command section, and dispatch — three consumers, one table (drift impossible).

## 3. Settings dialogs

- All pickers are one component family (`dialog/`) per the shared layout language (N5): header/body/footer, numbered choices, Esc-cancels, searchable where large.
- Model/effort switching: the cache-loss warning is a confirm sub-dialog derived from engine-provided cache state; the session-only vs persistent choice maps to a runtime config command vs a persisted config write (`EngineClient::set_config` vs a persisted store write).
- Provider add flow: catalog list → credential form → API-key entry (secret input widget; never echoed, never logged); delete requires inline confirmation.
- Config I/O (config.toml / tui.toml equivalents) goes through the engine/config ports — never direct file IO in the TUI crate (zero-IO discipline alignment; hard rule 1).

## 4. Chrome

- Welcome/footer/panels are pure views over `ChromeState` + sibling slices (mode badges from trust; counts from navigation; goal from navigation; usage from engine events).
- Theme: `auto` resolves through an injected `BackgroundProbe` (OSC 11 with timeout, run once at startup + on terminal-change events, never inside views); **resolution failure falls back to dark**; custom themes load from the data directory via the config port with documented schema, invalid values dropped safely.
- Banner fetch is a command (async, timeout-bounded, via injected HTTP port through the engine/config surface) — failure is invisible (None); display state (once/cooldown) is persisted via the config port.
- Hint carousel and transients advance on ticks with the injected clock.
- Clipboard hint: focus events re-arm a probe command; state dedups per image hash; hint auto-expires.

## 5. First-run & connection

- Sessionless bootstrap: the loop starts with no session and no model; welcome guidance is static chrome; the first provider/model configuration happens through the settings dialogs above.
- Engine connection: the `client` task owns the state machine — Connected → (drop) → Reconnecting (bounded backoff) → Failed → clean exit with restored terminal (interaction domain §6 hygiene). Reconnect state surfaces as a visible chrome element and blocks submissions with a notice.
- Provider/model refresh runs as a background command through `EngineClient`; results update the registry state and a status-line notice (+N models / failure).

## 6. Test anchors

| Requirement | Layer | Anchor |
|---|---|---|
| Registry-driven completion/help/dispatch consistency | L1 + L2 | table-driven dispatch tests; generated help snapshot |
| Availability gating mid-stream | L1 | busy-state invocation replies guidance |
| Every settings dialog round-trip | L3 | scripted dialog journeys with mock engine (incl. provider add, secret never echoed — screen-text assertion only) |
| Model switch cache warning + choices | L1 + L2 | reducer + snapshots |
| Theme auto fallback / custom load / invalid values | L1 + L2 | injected probe (fail→dark); invalid-theme snapshot |
| Footer/welcome element coverage + narrow degrade | L2 | 80/40-col snapshots with populated state |
| Banner failure invisibility | L3 | unreachable source → no visible change |
| First-run journey | L4 | bare state → provider → model → first reply (e2e) |
| Engine kill → reconnect → failure exit | L3(b) + L4 | scripted fake-server disconnect; real-process kill |
