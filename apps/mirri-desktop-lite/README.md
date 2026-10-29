# Mirri Code Lite

The **Electrobun** desktop shell — a lightweight second shell coexisting with
the Electron shell (`apps/mirri-desktop`). Experimental; see
`docs/specs/desktop-lite-*.md` for the requirements/architecture and
`plan/electrobun-shell-poc-report.md` for the feasibility PoC.

## What it is

- Same web UI as the Electron shell, zero modifications (loads the
  daemon-served SPA; the 13-method bridge contract is implemented over
  Electrobun RPC with a compile-time drift gate)
- Own daemon on port **55828** and state dir `~/.mirri-code/server-lite/` —
  fully decoupled from the Electron shell's daemon (58827); the two shells
  never touch each other's daemon lifecycle
- Degraded capabilities (trial phase): dock badge / bounce, login-item,
  deep-link wake, notification click events — all return honest false values
  (the web UI degrades gracefully)

## Commands

```bash
pnpm run dev      # prepare + build assets + launch (dev mode)
pnpm run build    # same + stable .app bundle in build/stable-macos-arm64/
pnpm run typecheck
node scripts/selfcheck.mjs   # boot self-check: daemon provenance + bridge injection
```

Requirements: `hutch` on PATH (Hutch ≥ 0.28), `node`, macOS (arm64 tested).

Note: this app is **excluded from the pnpm workspace** (it manages its own
`node_modules` via Hutch; see `pnpm-workspace.yaml`) and from the root
tsconfig project set. Its deps are installed via `hutch install`.

## Layout

```
src/bun/          main process: window, bridge handlers, daemon, open-in
src/preload/      the injected window.mirriDesktop (esbuild → dist/preload.js)
src/mainview/     bundled self-check page (loaded when the daemon is down)
src/shared/       RPC schema derived from the web bridge contract
scripts/          build-preload.mjs, selfcheck.mjs
```

## Trial status / known gaps

- No code signing / notarization (Gatekeeper prompt on first stable launch —
  right-click → Open)
- Not in the release pipeline; local builds only
- macOS only
