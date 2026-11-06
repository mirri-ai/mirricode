---
name: playwright-product-testing
description: Use when driving the mirricode web UI (apps/mirri-web) with Playwright for product-level testing or bug reproduction — spinning up an isolated rust engine + Vite dev server, bypassing server auth and the model-setup gate, seeding a fake provider, keeping a persistent browser profile across scripts, or simulating real IME composition via the CDP Input.imeSetComposition API. Covers the four environment gates (auth overlay, setup gate, onboarding dialog, ephemeral profiles) and the stale-prebundle trap that each cost a failed round during the 2026-10-31 IME Backspace investigation.
---

# Playwright Product Testing (Mirri Code Web UI)

How to spin up and drive the real Mirri Code web UI with Playwright — without
fighting the auth gate, the setup gate, or losing state between runs. Written
after the 2026-10-31 IME Backspace investigation, which wasted many rounds on
exactly the traps below.

## Why this exists

Product-level bugs (IME, focus, drag-drop, streaming UI) cannot be reproduced
with component or unit tests, and synthetic `dispatchEvent` calls do not
establish real browser state (e.g. IME composition). The environment has FOUR
gates that each cost a failed round if you don't know them:

1. **Server auth** — the engine requires a bearer token; a naive run gets the
   "Server token required" overlay on every fresh browser.
2. **Setup gate** — with no model provider configured, the web UI replaces the
   whole app with the "Add a model provider" onboarding page. `.ph` (Composer)
   never mounts.
3. **Onboarding dialog** — first visit to a healthy instance shows a
   theme/language "Get started" dialog that intercepts pointer events.
4. **Ephemeral browser profiles** — launching a fresh browser per script throws
   away localStorage (auth token), so gates 1–3 re-appear every single run.

## The recipe (one-time setup, ~30s)

### 1. Seed an isolated engine home + config

```bash
ROOT=$(pwd)
DIR=.tmp/pw-test            # scratch dir (gitignored) for this investigation
mkdir -p $DIR/home

cat > $DIR/home/config.toml <<EOF
default_model = "test/model"

[providers.test]
type = "openai"
api_key = "sk-test"
base_url = "http://127.0.0.1:9"

[models."test/model"]
provider = "test"
model = "gpt-test"
max_context_size = 128000
EOF
```

CRITICAL: `default_model` must be set. `/api/v1/auth` reports
`ready: true` only when BOTH a provider exists AND `default_model` is
non-null — missing it leaves the setup gate up even though providers_count=1.

If the test needs real model responses, point `base_url` at the fake provider
server from `@mirri-ai/e2e-harness` (`createFakeProviderServer()`) instead of
a dead port — see `apps/mirri-web/test/fork-streaming.e2e.test.ts` for the
full pattern.

### 2. Start the engine with auth bypassed

```bash
MIRRICODE_HOME="$ROOT/$DIR/home" rust/target/debug/mirri serve \
  --host 127.0.0.1 --port 58627 \
  --workspace "$ROOT" --log-level warn \
  --dangerous-bypass-auth > $DIR/engine.log 2>&1 &
```

`--dangerous-bypass-auth` (loopback only, trusted network) removes the token
overlay entirely. This mirrors what the repo's own e2e suites do
(`spawnRustServer` in fork-streaming.e2e.test.ts).

If the binary is missing: `cargo build -p mirri-cli` in `rust/`.

### 3. Start Vite dev server

```bash
cd apps/mirri-web
npx vite --port 58629 --strictPort --force > ../../$DIR/vite.log 2>&1 &
```

- `--force` matters when dependencies changed since the last run — stale
  prebundled deps silently serve OLD library code and send you chasing ghosts.
- Vite binds IPv6 `localhost` on macOS; connect to `http://localhost:PORT`,
  NOT `127.0.0.1` (connection refused).

### 4. Use ONE persistent browser profile across ALL scripts

```js
import { chromium } from 'playwright';

const browser = await chromium.launchPersistentContext('.tmp/pw-test/profile', {
  headless: false,
  channel: 'chrome',          // system Chrome; Playwright's bundled Chromium
                               // is NOT downloaded in this environment
});
const page = await browser.newPage();
```

- `channel: 'chrome'` — required. `npx playwright install` was never run;
  the cache only has unrelated builds.
- Keep the profile under `.tmp/` (gitignored).
- The FIRST run still shows the onboarding dialog — dismiss it once and it
  never comes back:

```js
const overlay = page.locator('.ui-dialog__overlay');
if (await overlay.count()) {
  await overlay.locator('button', { hasText: /get started/i }).click();
}
```

## Simulating real IME composition (the hard part)

Synthetic `new CompositionEvent(...)` dispatches do NOT put the browser into a
real composition state — ProseMirror/TipTap behave differently under real
composition, and several bugs (including the 2026-10 Backspace regression)
only reproduce with a real one. Use the CDP composition API:

```js
const cdp = await page.context().newCDPSession(page);

// Start/update a composition (like typing pinyin "nihao")
await cdp.send('Input.imeSetComposition', {
  text: 'nihao',
  selectionStart: 5,
  selectionEnd: 5,
  selection: { focusX: 0, focusY: 0, anchorX: 0, anchorY: 0 },
});

// Backspace inside the composition (a REAL key event)
await page.keyboard.press('Backspace');

// Real IMEs re-set the composition with the shortened text after Backspace
await cdp.send('Input.imeSetComposition', {
  text: 'niha',
  selectionStart: 4, selectionEnd: 4,
  selection: { focusX: 0, focusY: 0, anchorX: 0, anchorY: 0 },
});
```

### Reading the verdict

Instrument the editor before acting, then inspect:

```js
await page.evaluate(() => {
  const box = document.querySelector('.ph');   // the TipTap contenteditable
  window.__ev = [];
  for (const ev of ['compositionstart', 'compositionupdate', 'compositionend',
                    'beforeinput', 'input', 'keydown']) {
    box.addEventListener(ev, (e) => window.__ev.push(
      e.type + (e.data ? ':' + e.data : '') +
      (e.isComposing ? '[ic]' : '') +
      (e instanceof InputEvent ? '<' + e.inputType + '>' : '')));
  }
});
```

A healthy composition under Backspace shows ONE `compositionstart` and
successive `compositionupdate`s. A broken one shows repeated
`compositionstart` events (composition being killed and restarted) with the
old pinyin committed as plain text.

Useful extras:

- To grab the ProseMirror `EditorView` from the page: add (temporarily, in
  the app's `editorProps.handleKeyDown: (view, event) => ...`) a line storing
  `window.__view = view`. Then `view.composing`, `view.input.compositionNode`,
  and monkey-patched `view.dispatch` reveal everything PM does.
- A plain `contenteditable` div appended to the page makes a perfect CONTROL
  GROUP to decide whether misbehavior is the app's editor or the
  browser/CDP layer.
- `e.isComposing` on `beforeinput` is `false` for `deleteContentBackward`
  even mid-composition (Chromium behavior) — don't treat that alone as proof
  of a broken composition.

## Cleanup

Kill engine + Vite when done, or they linger as background listeners:

```bash
lsof -nP -iTCP:58629 -sTCP:LISTEN   # find the vite PID
kill <pid>
```

Scratch artifacts live entirely under `.tmp/` and are gitignored — safe to
leave, cheap to delete.

## Gotcha log (things that each cost a failed round)

| Symptom | Cause | Fix |
|---|---|---|
| `ERR_CONNECTION_REFUSED` on 127.0.0.1 | Vite binds IPv6 localhost | use `http://localhost:PORT` |
| Auth overlay every run | fresh browser profile | `launchPersistentContext` |
| "Executable doesn't exist" crash | no bundled Chromium | `channel: 'chrome'` |
| Composer never appears | no provider / no default_model | seed config.toml (step 1) |
| Click times out on `.ph` | onboarding dialog intercepts | dismiss once (step 4) |
| Fix works in unit test, not in browser | Vite stale prebundle | `vite --force` |
| Composition bug won't reproduce | synthetic events ≠ real composition | CDP `Input.imeSetComposition` |
