# Mirri Code Desktop

An Electron desktop client for Mirri Code (product name **Mirri Code Desktop**;
workspace package `@mirri-ai/mirri-desktop`). It is a thin **shell + process manager**
around the existing web UI (`apps/mirri-web`): it does not reimplement any UI or
backend, it just opens a native window onto the local Mirri server.

## How it works

The web UI cannot run on its own — it needs the Mirri Code **server** (REST + WS
under `/api/v1`). That server ships as a Rust `mirri` binary (built from
`rust/mirri-cli` with the web UI embedded via rust-embed).

On launch the app:

1. Runs the bundled `mirri` binary's `web` server, which reuses a live shared
   daemon if one is already running, or starts one — exactly the same
   `ensureDaemon` flow the CLI (`mirri web`) uses. The daemon binds the
   well-known port (`58627`) and writes `~/.mirri-code/server/lock`, so the CLI,
   the browser and the TUI all share the **same** server.
2. Reads that lock file for the real port and loads the web UI from the daemon's
   origin (e.g. `http://127.0.0.1:58627`) — same-origin, no CORS, no preload.

On quit the daemon is **left running**; it self-exits ~60s after the last client
disconnects, so closing the desktop app never tears down a server another client
is still using.

Key files:

- `src/main/index.ts` — app lifecycle orchestrator: single-instance lock, deep-link wiring, quit, and the ready sequence.
- `src/main/window/` — main-window creation + the server connect flow (`create.ts`), window-bounds persistence (`state.ts`).
- `src/main/server/` — run the bundled `mirri` binary, read the lock, confirm `/healthz` (`daemon.ts`); daemon state, shell-env probe, and token live in sibling files.
- `src/main/platform/` — the only module that reads `process.platform`; per-OS open-in launch commands live under `open-in/`.
- `src/main/deep-links/` — the startup queue/flush dispatch (`dispatch.ts`); `mirricode://` parsing lives one level up in `src/main/deep-link.ts`.
- `src/main/screens.ts` — loading/error screens shown while the daemon boots.
- `src/main/menu.ts` — native application menu.
- `src/main/server-binary.ts` — resolve the bundled `mirri` binary path (dev vs packaged).
- `src/main/ipc.ts` — IPC channel names + payload validation, shared by preload and main.
- `src/main/register-ipc.ts` — registers the validated IPC handlers on `ipcMain`.
- `src/main/preload.ts` — the sandboxed preload bridge exposing `window.mirriDesktop`.

## Develop

The dev build probes for a local `mirri` binary (see `server-binary.ts`;
`build/backend/<target>/`, then `rust/target/release|debug`). The minimal setup
is the web SPA (the server serves it via `--web-dir` in dev) plus a debug build
of the backend:

```bash
# one-time (rebuild when rust/mirri-cli or mirri-web change):
pnpm -C apps/mirri-web run build   # SPA for dev (`--web-dir`); required by build:backend too
cargo build -p mirri-cli           # debug build is enough for dev

# or, for the packaging-equivalent backend (same artifact CI stages — embeds the SPA):
pnpm -C apps/mirri-desktop run build:backend

# then run the desktop app (builds the main process, launches Electron):
pnpm -C apps/mirri-desktop run dev      # or: pnpm dev:desktop  (from repo root)
```

Checks:

```bash
pnpm -C apps/mirri-desktop run typecheck
```

## Package

`dist` builds the main process and runs electron-builder for the **current**
platform. Build the backend first with the packaging profile, then package —
`scripts/before-pack.cjs` stages the matching-platform Rust `mirri` binary from
`build/backend/<target>/` into the app's resources (`<resources>/bin/<target>/`).

```bash
pnpm -C apps/mirri-desktop run build:backend   # SPA + stage-web + cargo --profile dist
CSC_IDENTITY_AUTO_DISCOVERY=false pnpm -C apps/mirri-desktop run dist
# -> apps/mirri-desktop/dist-app/
```

> Do **not** rename a built `.app` bundle — renaming invalidates its code
> signature and macOS will report it as "damaged".

Cross-platform installers are produced in CI (`.github/workflows/desktop-build.yml`),
which builds the Rust backend on each platform runner (linux-x64 via
cargo-zigbuild/musl, the rest native) and packages there. The backend is a
native binary per platform, so each OS must be built on its own runner.

### macOS signing + notarization

An **unsigned** macOS build shows *"app is damaged and can't be opened"* once it
has been transferred to another Mac (Gatekeeper quarantine). To distribute it,
the app must be signed with a **Developer ID Application** certificate and
notarized by Apple. The config (`electron-builder.config.cjs`) applies the
hardened runtime + entitlements (`build/entitlements.mac.plist`) to the app and
the nested backend binary, and signing/notarization are environment-driven:

```bash
MIRRICODE_DESKTOP_NOTARIZE=true \
CSC_NAME="Developer ID Application: … (TEAMID)" \
APPLE_API_KEY=/path/AuthKey_XXX.p8 APPLE_API_KEY_ID=XXXX APPLE_API_ISSUER=…uuid… \
pnpm -C apps/mirri-desktop run dist
```

In CI, run the **desktop-build** workflow with `sign-macos: true`; it reuses the
same Apple secrets / keychain action as the TUI native build
(`APPLE_CERTIFICATE_P12`, `APPLE_NOTARIZATION_KEY_*`). The resulting `.dmg` opens
on any Mac without warnings.

> An `Apple Development` certificate is **not** enough — it can sign for your own
> machine but cannot be notarized. You need a `Developer ID Application` cert.

## v1 scope / not done yet

- **Auto-update**: not implemented (v2).
- **Windows / Linux signing**: unsigned in v1 (Windows shows a SmartScreen
  prompt). Only macOS is signed + notarized.
- **App icon**: builds ship the Mirri logo (sourced from the docs site art) on
  macOS, Windows, and Linux.
- **First launch may need network**: the bundled `mirri` binary shares the
  installed CLI's runtime (`~/.mirri-code` lock, config and auth), so the same
  shell/network environment applies.
