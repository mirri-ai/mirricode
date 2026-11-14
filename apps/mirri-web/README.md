# Mirri Web

A browser client for Mirri Code — the peer of the desktop shells. Vue 3 + Vite +
TypeScript, talking to the local **rust engine** over REST + WebSocket. (The
terminal client this README used to name as its peer, `apps/mirri-code`, was
retired on 2026-10-28; see `docs/specs/ts-v1-engine-retirement-requirements.md`.)

---

## Quick start

```bash
# 1) Against a REAL server (the server must be running and reachable)
WEB_PORT=5197 MIRRICODE_SERVER_URL=http://192.168.97.91:58627 pnpm -C apps/mirri-web run dev
#   …or from the repo root:  pnpm dev:web   (uses the defaults below)

# 2) Offline / no server — a stub that fakes the server API + event stream
pnpm -C apps/mirri-web run dev:stub      # then run dev in another shell

# checks
pnpm -C apps/mirri-web run typecheck     # vue-tsc --noEmit
pnpm -C apps/mirri-web run test          # vitest (pure logic only)
pnpm -C apps/mirri-web run build         # vite build
```

### How it connects to the server

The browser cannot reach the server cross-origin (no CORS), so Vite **same-origin
proxies** `/api/v1` (HTTP + WS) to the server (`vite.config.ts`):

| env var           | default                  | meaning                                  |
| ----------------- | ------------------------ | ---------------------------------------- |
| `WEB_PORT`        | `5175`                   | port the dev server listens on           |
| `MIRRICODE_SERVER_URL` | `http://127.0.0.1:58627`  | where `/api/v1` (and `/api/v1/ws`) is forwarded |

> Behind a corporate HTTP proxy, also set `NO_PROXY=<server-host>` (for example,
> `NO_PROXY=127.0.0.1,localhost`) so the proxy forward reaches the server directly.

---

## Architecture

A strict one-direction data flow; components never touch the network or the
reducer — they consume computed view props and call actions.

```
server (REST + WS)
  └─ src/api/daemon/client.ts      REST adapter  (envelope → AppX types)
  └─ src/api/daemon/ws.ts          WS frames → classify → projector/reducer
       └─ agentEventProjector.ts   RAW agent-core events → AppEvent[]
       └─ eventReducer.ts          AppEvent[] → state
  └─ src/composables/useMirriWebClient.ts   the ONLY place that imports api + state;
                                           exposes computed view props + actions
  └─ src/components/*.vue          render props, emit intents (no api access)
```

> The directory name `src/api/daemon/` is historical and kept to minimise
> diff churn; conceptually it is the **server** adapter.

- **Adapter** (`src/api/`): wire types are snake_case; `AppX` types are camelCase.
  `config.ts` builds `/api/v1` URLs.
- **Event projector** (`agentEventProjector.ts`): the server streams **raw
  engine events** (no `event.` prefix; the "agent-core" in the sketch above is
  the historical TypeScript engine name, kept as a label for that event family).
  `classifyFrame` routes raw vs protocol (`event.*`) frames; the projector
  converts them to `AppEvent`s.
- **i18n** (`src/i18n/`): vue-i18n, en/zh, per-namespace flat camelCase keys.
  Detect order: `localStorage('mirri-locale')` → `navigator.language` → `en`.
---

## Server contract — non-obvious notes

The server's wire protocol has a few things that will bite you if forgotten:

- **Envelope:** every response is `{ code, msg, data, request_id }` and the HTTP
  status is **always 200** — check `code` (0 = ok), not the status.
- **Prompts require five fields.** `POST /sessions/{id}/prompts` must carry
  `{ content, model, thinking, permission_mode, plan_mode }`. The web fills these
  from settings (model ← session/`default_model`, thinking/permission/plan ← the
  StatusLine controls). Sending only `{ content }` → `40001 model …`.
- **Creating a session needs a *registered* workspace.** `workspace_id` must be a
  `wd_<slug>_<hash>` id that exists in the server's registry. Sessions get one
  auto-assigned by cwd, but it isn't *registered* until you `POST /workspaces
  { root }` (idempotent). The web registers on demand before `createSession`
  (otherwise: `workspace not found: wd_…`).
- **Persisted sessions are directly promptable** — selecting an old session and
  sending a message just works; there is **no `:activate` step**.
- **Workspaces** = real folders. `GET/POST/PATCH/DELETE /workspaces`,
  `GET /fs:browse?path=`, `GET /fs:home` back the rail + folder picker.

## Release & deployment

Mirri Web is **not published as a standalone package**. It is embedded into the
rust `mirri` binary as the built-in web UI.

### Current release flow

1. **Develop** — `pnpm dev:web` (or `pnpm -C apps/mirri-web run dev`), against
   the rust engine.
2. **Build** — `pnpm -C apps/mirri-web run build` produces `apps/mirri-web/dist`.
3. **Stage** — `node rust/scripts/stage-web.mjs` copies `apps/mirri-web/dist`
   into `rust/crates/mirri-server/web-dist`. `cargo build -p mirri-cli` with its
   default `embedded-web` feature embeds that directory via rust-embed. The release
   packaging (`rust/scripts/release-package.mjs`) runs the staging step itself, so
   the `full` variant always carries the SPA; the `lite` variant
   (`--no-default-features`) embeds nothing.
4. **Ship** — the release workflow uploads the resulting binaries as
   `mirri-code-<target>.zip` to the GitHub Release. There is no npm publish step
   (retired), and no separate web package version: the UI follows the product
   version in `packages/release`.

### Suggested improvements

- **Keep the current coupling for now.** Because Mirri Code is primarily a local
  CLI/server product, bundling the web UI into the CLI package keeps installs
  self-contained and avoids cross-origin/CORS complexity.
- **Add an independent web-deploy workflow only when needed.** If a public
  standalone web deployment is required later, create
  `.github/workflows/web-deploy.yml` that builds `apps/mirri-web` and uploads
  `dist/` to the chosen static host (S3/CloudFront, Cloudflare Pages, Vercel,
  etc.). Until then, do not maintain a separate deploy target.
- **Keep versioning owned by the CLI release.** `apps/mirri-web/package.json`
  remains internal workspace metadata; do not surface it as a separate user
  version unless the web app becomes an independently published product.
- **Ensure the web build is exercised in CI.** The root `build` script already
  builds every workspace, so `pnpm run build` in CI covers `apps/mirri-web`.
  Keep it that way; do not bypass the web build in release pipelines.
