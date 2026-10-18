/**
 * Browser e2e: supervision delegation acceptance (Phase 1 + Phase 2).
 *
 * Real web UI + REAL rust engine (`mirri serve` — the stack the web targets
 * in production), only the LLM mocked (FakeProviderServer). Drives a REAL
 * Chromium against the PRODUCTION web build served by the rust engine and
 * asserts what a USER sees for the supervision domain:
 *
 *   Phase 1 (web unification):
 *   - a running/completed AgentSwarm renders as ONE collapsed delegation
 *     card (never auto-expanded, never a monitoring console) with an N/M
 *     chip and an Open button — no percentages, no ETA
 *   - Open reveals the supervision panel (member selector for N>1) and the
 *     member focus view
 *   - a single Agent delegation skips the selector (cardinality is data)
 *
 *   Phase 2 (engine read faces):
 *   - after a page refresh the delegation card STILL opens (snapshot roster)
 *     with both members and correct terminal states
 *   - selecting a finished member shows its work log narrative (the
 *     message.list agentId read face), not a blank panel
 *   - an approval requested by a member agent carries the sub-agent badge;
 *     a main-agent approval does not
 *   - stopping a running member from the focus view converges its state in
 *     the panel (phase 'failed', stop button gone) and in the card (chip
 *     settles, red breathing border for the failed delegation)
 */

import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer as createNetServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { chromium, type Locator } from 'playwright';
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';

import { createFakeProviderServer, type FakeProviderServer } from '../../../e2e-harness/src/fake-provider-server';

const REPO_ROOT = resolve(fileURLToPath(new URL('../../../../', import.meta.url)));
const RUST_BIN = join(REPO_ROOT, 'rust', 'target', 'debug', 'mirri');

/** The PRODUCTION web build — must exist (`pnpm --filter @mirri-ai/mirri-web build`). */
const WEB_DIST = fileURLToPath(
  new URL('../../../../apps/mirri-web/dist', import.meta.url),
);

const browserE2eAvailable =
  existsSync(WEB_DIST) && existsSync(chromium.executablePath()) && existsSync(RUST_BIN);

interface AppStack {
  child: ChildProcess;
  baseUrl: string;
  fakeProvider: FakeProviderServer;
  homeDir: string;
  workDir: string;
}

let browser: Awaited<ReturnType<typeof chromium.launch>>;
let page: Awaited<ReturnType<typeof browser.newPage>>;
const stacks: AppStack[] = [];

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const srv = createNetServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      const port = typeof addr === 'object' && addr !== null ? addr.port : undefined;
      srv.close(() => (port === undefined ? reject(new Error('no free port')) : resolvePort(port)));
    });
  });
}

/** Poll /api/v1/healthz until the envelope reports ok:true. */
async function waitForHealthz(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown = undefined;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/api/v1/healthz`);
      if (res.ok) {
        const body = (await res.json()) as { data?: { ok?: boolean } };
        if (body.data?.ok === true) return;
      }
    } catch (error) {
      lastError = error;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `rust engine on :${port} not ready within ${timeoutMs}ms` +
      (lastError !== undefined
        ? ` (last error: ${lastError instanceof Error ? lastError.message : JSON.stringify(lastError)})`
        : ''),
  );
}

/**
 * Boot the REAL rust engine (`mirri serve`) with the PRODUCTION web build,
 * an isolated MIRRICODE_HOME and the fake LLM — the exact production stack
 * the supervision Phase 2 read faces were implemented against (P2-T1/T2/T3
 * live in rust/crates/mirri-server + mirri-core). Only the LLM API is
 * mocked. The seed format mirrors the snapshot-contract e2e (config.toml
 * top-level `default_model` first — TOML top-level keys must precede table
 * sections for /auth ready).
 */
async function bootApp(permissionMode: 'auto' | 'manual'): Promise<AppStack> {
  const fakeProvider = await createFakeProviderServer();
  const homeDir = await mkdtemp(join(tmpdir(), 'mirri-supervision-home-'));
  const workDir = await mkdtemp(join(tmpdir(), 'mirri-supervision-work-'));
  // The member subagent reads this file so its work log has a tool call.
  await writeFile(join(workDir, 'note.txt'), 'acceptance note\n', 'utf8');
  const config = `
default_model = "test/model"
default_permission_mode = "${permissionMode}"

[providers.test]
type = "openai"
api_key = "sk-test"
base_url = "${fakeProvider.baseUrl}"

[models."test/model"]
provider = "test"
model = "gpt-test"
max_context_size = 128000
`;
  await writeFile(join(homeDir, 'config.toml'), config, 'utf8');

  const port = await freePort();
  const child = spawn(
    RUST_BIN,
    [
      'serve',
      '--host', '127.0.0.1',
      '--port', String(port),
      '--workspace', workDir,
      '--web-dir', WEB_DIST,
      '--log-level', 'warn',
      // Loopback open mode — matches the snapshot-contract e2e; no token
      // handshake to carry through the browser.
      '--dangerous-bypass-auth',
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        MIRRICODE_HOME: homeDir,
        OPENAI_API_KEY: 'dummy',
        OPENAI_BASE_URL: fakeProvider.baseUrl,
      },
    },
  );
  let stderr = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString()).slice(-4000);
  });
  try {
    await waitForHealthz(port, 30_000);
  } catch (error) {
    child.kill('SIGKILL');
    throw new Error(`rust engine failed to boot: ${stderr}\n${String(error)}`, { cause: error });
  }
  const stack: AppStack = {
    child,
    baseUrl: `http://127.0.0.1:${port}`,
    fakeProvider,
    homeDir,
    workDir,
  };
  stacks.push(stack);
  return stack;
}

/** Politely stop the rust engine via /api/v1/shutdown, falling back to SIGKILL. */
async function stopRustServer(stack: AppStack): Promise<void> {
  try {
    await fetch(`${stack.baseUrl}/api/v1/shutdown`, {
      method: 'POST',
      signal: AbortSignal.timeout(5000),
    });
    await Promise.race([
      new Promise<void>((r) => stack.child.once('exit', () => r())),
      new Promise<void>((r) => setTimeout(r, 5000)),
    ]);
  } catch {
    // not running — fine
  }
  if (stack.child.exitCode === null) stack.child.kill('SIGKILL');
}

/**
 * The web client's permission mode is a LOCAL setting (localStorage key
 * `mirri-web.permission`, defaulting to `manual` in a fresh profile) — it is
 * carried per-prompt and is what actually gates approvals and the swarm
 * enable confirmation. Seed it per test via an init script so each test
 * controls its own permission mode deterministically.
 */
async function newPage(permissionMode: 'auto' | 'manual'): Promise<void> {
  page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  await page.addInitScript(
    (mode: string) => {
      try {
        globalThis.localStorage.setItem('mirri-web.permission', mode);
      } catch { /* storage unavailable — test will surface the fallout */ }
    },
    permissionMode,
  );
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      // eslint-disable-next-line no-console
      console.log('[browser-console-error]', msg.text());
    }
  });
}

async function openApp(stack: AppStack): Promise<void> {
  // Loopback open mode — no token to carry, the app boots straight in.
  await page.goto(`${stack.baseUrl}/`, { waitUntil: 'networkidle' });
  await page.waitForTimeout(2000);

  // Dismiss the first-run onboarding overlay.
  const getStarted = page.getByText('Get started', { exact: true });
  if (await getStarted.count() > 0) {
    await getStarted.click();
    await page.waitForTimeout(1000);
  }

  // Create a workspace pointing at the isolated work dir.
  const newWorkspace = page.getByText('New workspace', { exact: true });
  if (await newWorkspace.count() > 0) {
    await newWorkspace.click();
    await page.waitForTimeout(1200);
    const pathInput = page.locator('input[type="text"]').first();
    if (await pathInput.count() > 0) {
      await pathInput.fill(stack.workDir);
      await page.keyboard.press('Enter');
      await page.waitForTimeout(1200);
    }
    const openThisFolder = page.getByText('Open this folder', { exact: true });
    if (await openThisFolder.count() > 0) {
      await openThisFolder.click();
      await page.waitForTimeout(1500);
    }
  }
}

async function sendComposer(text: string): Promise<void> {
  const composer = page.locator('textarea').last();
  await composer.fill(text);
  await composer.press('Enter');
}

/** Poll until the locator's element count reaches the expected value. */
async function waitForCount(
  page: Awaited<ReturnType<typeof browser.newPage>>,
  locator: Locator,
  expected: number,
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if ((await locator.count()) === expected) return;
    if (Date.now() > deadline) {
      throw new Error(`waitForCount: expected ${expected}, got ${await locator.count()}`);
    }
    await page.waitForTimeout(200);
  }
}

/** Poll until the locator's trimmed text equals the expected value. */
async function waitForText(locator: Locator, expected: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const text = ((await locator.textContent()) ?? '').trim();
    if (text === expected) return;
    if (Date.now() > deadline) {
      await dumpDiagnostics(locator.page(), `waitForText "${expected}" (got "${text}")`);
      throw new Error(`waitForText: expected "${expected}", got "${text}"`);
    }
    await locator.page().waitForTimeout(200);
  }
}

/**
 * The transcript text EXCLUDING the delegation-card subtree. SDR-007
 * sanctions the card's collapsed member rows (identity · status · one-line
 * activity, which falls back to the member's last output line — e.g.
 * "Calling Read: note.txt") inside the conversation, so the leak guard must
 * assert the transcript PROPER: the member's tool call may never render as
 * tool rows outside the card.
 */
/**
 * Minimal structural DOM types for the in-browser `evaluate` callback below —
 * kap-server is a Node package with no DOM lib, so the browser snippet must
 * not reference `document`/`HTMLElement` directly (they would fail typecheck).
 */
interface BrowserNode {
  cloneNode(deep: boolean): BrowserNode;
  querySelectorAll(selector: string): { remove(): void }[];
  textContent: string | null;
}

async function chatTranscriptText(p: Awaited<ReturnType<typeof browser.newPage>>): Promise<string> {
  return p.evaluate(() => {
    const doc = (globalThis as { document?: { querySelector(sel: string): BrowserNode | null } }).document;
    const chat = doc?.querySelector('.chat');
    if (!chat) return '';
    const clone = chat.cloneNode(true);
    for (const el of clone.querySelectorAll('.dc')) el.remove();
    return clone.textContent ?? '';
  });
}

/** Failure forensics: what the LLM saw, what the server tracks, what the page shows. */
async function dumpDiagnostics(p: Awaited<ReturnType<typeof browser.newPage>>, stage: string): Promise<void> {
  // eslint-disable-next-line no-console
  console.log(`[diag] ${stage}`);
  try {
    // eslint-disable-next-line no-console
    console.log('[diag] url:', p.url());
  } catch { /* best-effort */ }
  try {
    await p.screenshot({
      path: join(process.cwd(), '../../.tmp', `supervision-e2e-${stage.replace(/\W+/g, '-')}.png`),
      timeout: 10_000,
    });
  } catch (error) {
    // eslint-disable-next-line no-console
    console.log('[diag] screenshot failed:', error);
  }
  try {
    const cardText = (await p.locator('.dc').first().textContent({ timeout: 3000 })) ?? '';
    // eslint-disable-next-line no-console
    console.log('[diag] card text:', cardText.replace(/\s+/g, ' ').slice(0, 300));
  } catch { /* absent card is itself a diagnostic */ }
  try {
    const panelText = (await p.locator('.sp').first().textContent({ timeout: 3000 })) ?? '';
    // eslint-disable-next-line no-console
    console.log('[diag] panel text:', panelText.replace(/\s+/g, ' ').slice(0, 300));
  } catch { /* absent panel is fine */ }
  try {
    const bodyText = (await p.locator('body').textContent({ timeout: 3000 })) ?? '';
    // eslint-disable-next-line no-console
    console.log('[diag] body text:', bodyText.replace(/\s+/g, ' ').slice(0, 600));
  } catch { /* page may be hung */ }
  try {
    const stack = stacks.at(-1);
    if (stack) {
      // Request fingerprints: whose turn was each LLM call (main prompts vs
      // member prompts vs tool-result echoes).
      const fingerprints = stack.fakeProvider.requests.map((r) => {
        const b = r.bodyText;
        const marks: string[] = [];
        if (b.includes('hello')) marks.push('main-hello');
        if (b.includes('run the supervision acceptance batch')) marks.push('main-swarm');
        if (b.includes('P: alpha')) marks.push('member-alpha');
        if (b.includes('P: beta')) marks.push('member-beta');
        if (b.includes('note.txt')) marks.push('read-echo');
        if (marks.length === 0) marks.push('other');
        return marks.join('+');
      });
      // eslint-disable-next-line no-console
      console.log('[diag] request fingerprints:', JSON.stringify(fingerprints));
      const sessionsRes = await fetch(`${stack.baseUrl}/api/v1/sessions`);
      const sessionsJson = (await sessionsRes.json()) as { data?: unknown };
      const sessionsData = Array.isArray(sessionsJson.data) ? sessionsJson.data : [];
      const first = sessionsData[0] as { id?: string } | undefined;
      // eslint-disable-next-line no-console
      console.log('[diag] session:', first?.id);
      if (first?.id !== undefined) {
        const res = await fetch(`${stack.baseUrl}/api/v1/sessions/${first.id}/tasks`);
        // eslint-disable-next-line no-console
        console.log('[diag] server tasks:', JSON.stringify(await res.json()).slice(0, 2000));
      }
    }
  } catch (error) {
    // eslint-disable-next-line no-console
    console.log('[diag] dump failed:', error);
  }
}

describe.skipIf(!browserE2eAvailable)('supervision delegation acceptance (browser e2e)', () => {
  beforeAll(async () => {
    browser = await chromium.launch();
  });

  afterAll(async () => {
    await browser?.close();
  });

  afterEach(async () => {
    // Failure forensics: dump what the user was seeing before tearing down.
    if (page) {
      try {
        const current = page;
        const testId = expect.getState().currentTestName ?? 'unknown';
        await current.screenshot({
          path: join(process.cwd(), '../../.tmp', `supervision-e2e-${testId.replace(/\W+/g, '-')}.png`),
        });
      } catch { /* best-effort */ }
    }
    // Close the page BEFORE the servers: an open page keeps its WS/SSE
    // subscriptions in flight, which would hang the engine's shutdown
    // (known e2e-env pitfall — see reports/rust-v3-snapshot-rerender-findings.md §七).
    await page?.close();
    for (const stack of stacks.splice(0)) {
      await stopRustServer(stack);
      await stack.fakeProvider.close();
      await rm(stack.homeDir, { recursive: true, force: true });
      await rm(stack.workDir, { recursive: true, force: true });
    }
  }, 180_000);

  it('should keep the swarm as one collapsed card, open the panel, and survive a refresh with the roster and work log', async () => {
    await newPage('auto');
    const stack = await bootApp('auto');
    // Give the fake provider a small delay so the running window is observable.
    stack.fakeProvider.setStreamDelay(300);

    // LLM script: main greets → swarm batch (2 members) → both members read a
    // file then report → main finalizes. Two members race for the shared FIFO
    // queue, so both member turns of each round are identical. After the tool
    // round, a generous buffer of identical texts covers every remaining
    // consumer (each member's final turn, the post-completion report step, the
    // main agent's closing turn) — an unexpected consumer just gets a valid
    // text response instead of a 500 retry loop.
    stack.fakeProvider.nextText('Ready.');
    // The engine's out-of-band session-title generation (spec 09 / ADR-020)
    // fires one side LLM call once the FIRST user turn settles on an untitled
    // session — it shares this FIFO queue, so feed it a plain title text or
    // it would consume the AgentSwarm tool call below and shift the script.
    stack.fakeProvider.nextText('Supervision acceptance run');
    stack.fakeProvider.nextToolCall('AgentSwarm', {
      description: 'supervision acceptance batch',
      subagent_type: 'coder',
      // The 'coder' profile pins a real model — override with the fake alias
      // so members talk to the FakeProviderServer.
      model: 'test/model',
      prompt_template: 'P: {{item}}',
      items: ['alpha', 'beta'],
    });
    for (let i = 0; i < 2; i++) stack.fakeProvider.nextToolCall('Read', { file_path: 'note.txt' });
    for (let i = 0; i < 12; i++) stack.fakeProvider.nextText('Member finished its slice of work.');

    await openApp(stack);

    // First prompt creates the session.
    await sendComposer('hello');
    await page.getByText('Ready.', { exact: false }).first().waitFor({ state: 'visible', timeout: 60_000 });

    // Auto permission mode auto-approves the swarm start: the swarm-start
    // confirmation only asks under manual permission (approval-context §
    // "goal/swarm 启动确认" — 仅 manual 弹), and the engine's auto-mode approve
    // policy sits ahead of the swarm-ask policy, so the batch starts
    // immediately with no approval card.
    await sendComposer('/swarm run the supervision acceptance batch');

    // The delegation card appears — ONE card for the whole batch.
    const card = page.locator('.dc').first();
    await card.waitFor({ state: 'visible', timeout: 60_000 });

    // SDR-007 member rows: two collapsed rows (one per started member),
    // each with the member's identity + phase; the head keeps the N/M chip.
    await waitForCount(page, card.locator('.dc-row'), 2, 60_000);
    expect((await card.locator('.dc-row').first().textContent()) ?? '').toContain('coder');
    // The card body (prompt / result detail) is STILL not rendered while collapsed.
    expect(await card.locator('.dc-body').count()).toBe(0);
    // Honest progress: N/M chip, no percentages, no ETA anywhere in the card.
    await card.locator('.dc-chip').waitFor({ state: 'visible', timeout: 60_000 });
    const cardText = (await card.textContent()) ?? '';
    expect(cardText).not.toMatch(/%|ETA|eta/);

    // Clicking a member row opens the panel focused on THAT member (SDR-007).
    await card.locator('.dc-row').first().click();
    const panel = page.locator('.sp').first();
    await panel.waitFor({ state: 'visible', timeout: 30_000 });
    const focus = page.locator('.mf-body').first();
    await focus.waitFor({ state: 'visible', timeout: 30_000 });
    await focus.getByText('Member finished its slice of work.', { exact: false }).first()
      .waitFor({ state: 'visible', timeout: 60_000 });

    // While running, the member's Read activity streams to the panel only.
    // The leak negative is scoped to the transcript EXCLUDING the delegation
    // card (chatTranscriptText): the card's own member-row one-line activity
    // is sanctioned by SDR-007 and may legitimately echo the last output line.
    await waitForCount(page, page.locator('.sp').getByText('Calling Read'), 1, 60_000);
    const transcriptDuringRun = await chatTranscriptText(page);
    expect(transcriptDuringRun).not.toContain('Calling Read');
    expect(transcriptDuringRun).not.toContain('note.txt');

    // The batch converges: the card head reads 2 / 2.
    await waitForText(card.locator('.dc-chip'), '2 / 2', 90_000);

    // REFRESH — the card must stay collapsed and STILL openable (snapshot
    // roster), with both members and their terminal states.
    await page.goto(`${stack.baseUrl}/`, { waitUntil: 'networkidle' });
    await page.waitForTimeout(2500);

    // A fresh page lands on the empty state — re-select the session exactly
    // like a returning user would (click its row in the sidebar). The row now
    // carries the side-call-generated title ("Supervision acceptance run" —
    // the text scripted for the out-of-band title call), not the first
    // prompt's text.
    await page.locator('.se', { hasText: 'Supervision acceptance' }).first().click();
    await page.waitForTimeout(1500);

    // The restored COMPLETED run auto-collapsed on the fresh page — expand
    // the LAST run header (the first run was the "hello" greeting) so the
    // delegation card is reachable again.
    const runHeaderAfter = page.locator('button.a-run-hd[aria-expanded="false"]').last();
    await runHeaderAfter.waitFor({ state: 'visible', timeout: 30_000 });
    await runHeaderAfter.click();
    await page.waitForTimeout(500);

    const cardAfter = page.locator('.dc').first();
    try {
      await cardAfter.waitFor({ state: 'visible', timeout: 60_000 });
    } catch (error) {
      await dumpDiagnostics(page, 'refresh-no-card');
      throw error;
    }
    expect(await cardAfter.locator('.dc-body').count()).toBe(0);
    // The member rows survive the refresh with the result's data (SDR-007).
    await waitForCount(page, cardAfter.locator('.dc-row'), 2, 60_000);
    await cardAfter.locator('.dc-open').click();
    const panelAfter = page.locator('.sp').first();
    await panelAfter.waitFor({ state: 'visible', timeout: 30_000 });
    await waitForCount(page, panelAfter.locator('.sp-row'), 2, 30_000);

    // Selecting a FINISHED member shows its work log (message.list agentId
    // read face) — non-blank narrative with the tool call it made.
    await panelAfter.locator('.sp-row').first().click();
    const historyRows = page.locator('.mf-hist');
    await historyRows.first().waitFor({ state: 'visible', timeout: 30_000 });
    expect(await historyRows.count()).toBeGreaterThanOrEqual(2);
    const historyText = (await page.locator('.mf-progress, .mf-hist').allTextContents()).join('\n');
    expect(historyText).toContain('Calling Read');
    expect(historyText).toContain('Member finished its slice of work.');

    // Leak guard (SDR-002 / P2+1): the members called Read on note.txt — that
    // activity must exist ONLY in the panel's focus view, never as tool rows
    // in the main conversation column. Before the engine attributed tool
    // events, these frames leaked into the transcript and vanished on
    // refresh (context §8).
    const panelNoteHits = page.locator('.sp').getByText('note.txt');
    expect(await panelNoteHits.count()).toBeGreaterThan(0);
    // Same scoping as during the run: the negative targets the transcript
    // EXCLUDING the delegation card's sanctioned member rows.
    const transcriptAfterRefresh = await chatTranscriptText(page);
    expect(transcriptAfterRefresh).not.toContain('note.txt');
    expect(transcriptAfterRefresh).not.toContain('Calling Read');
  });

  it('should render a single Agent delegation without the member selector and with zero single-agent regression', async () => {
    await newPage('auto');
    const stack = await bootApp('auto');
    stack.fakeProvider.setStreamDelay(200);

    // LLM script: main delegates to ONE subagent; the member reports; the
    // main closes. Buffered identical texts absorb the member's report step
    // and the main's closing turn (see the swarm test note).
    stack.fakeProvider.nextToolCall('Agent', {
      prompt: 'Write a hello world function',
      description: 'Delegate one coding task',
      model: 'test/model',
    });
    for (let i = 0; i < 6; i++) stack.fakeProvider.nextText('The single member finished its task.');

    await openApp(stack);
    await sendComposer('delegate one task to a subagent');

    // ONE collapsed delegation card; the sole member's activity shows on the
    // head while running.
    const card = page.locator('.dc').first();
    await card.waitFor({ state: 'visible', timeout: 60_000 });
    expect(await card.locator('.dc-body').count()).toBe(0);
    // SDR-007: exactly ONE member row (the sole member), and the N/M chip is
    // N>1-only — absent here (cardinality is data, SDR-001).
    expect(await card.locator('.dc-row').count()).toBe(1);
    expect(await card.locator('.dc-chip').count()).toBe(0);

    // Click the row → focus view directly, NO member selector (N=1 skips it).
    await card.locator('.dc-row').first().click();
    const panel = page.locator('.sp').first();
    await panel.waitFor({ state: 'visible', timeout: 30_000 });
    expect(await panel.locator('.sp-row').count()).toBe(0);
    expect(await panel.locator('.sp-selector').count()).toBe(0);
    const focus = page.locator('.mf-body').first();
    await focus.waitFor({ state: 'visible', timeout: 30_000 });
    await focus.getByText('The single member finished its task.', { exact: false }).first()
      .waitFor({ state: 'visible', timeout: 60_000 });

    // Zero-regression guard: no approval cards anywhere in this session, and
    // no sub-agent badge text.
    expect(await page.locator('.appr').count()).toBe(0);
  });

  it('should badge a member-originated approval and leave main-agent approvals unbadged', async () => {
    await newPage('manual');
    const stack = await bootApp('manual');

    // LLM script: main runs a Bash command first (main-agent approval), then
    // delegates; the member writes a file (member-agent approval), then both
    // wrap up. Buffered identical texts absorb the report step and closing
    // turns (see the swarm test note).
    stack.fakeProvider.nextToolCall('Bash', { command: 'echo main-approval' });
    stack.fakeProvider.nextToolCall('Agent', {
      prompt: 'Write the output file',
      description: 'Delegate the file write',
      model: 'test/model',
    });
    stack.fakeProvider.nextToolCall('Write', {
      path: 'member-out.txt',
      content: 'written by the member',
    });
    for (let i = 0; i < 8; i++) stack.fakeProvider.nextText('All done.');

    await openApp(stack);
    await sendComposer('run a command then delegate a file write');

    // First approval: the MAIN agent's Bash — no sub-agent badge.
    const firstApproval = page.locator('.appr').first();
    await firstApproval.waitFor({ state: 'visible', timeout: 60_000 });
    expect((await firstApproval.locator('.ah').textContent()) ?? '').not.toContain('sub agent ·');
    await firstApproval.getByRole('button', { name: /^Approve/ }).first().click();
    await page.waitForTimeout(500);

    // The main then dispatches the member (possibly gated by another unbadged
    // approval), and the MEMBER's Write produces the badged approval. Approve
    // unbadged main-agent approvals until the member's shows up.
    let memberApproval: Locator | null = null;
    const badgeDeadline = Date.now() + 120_000;
    while (Date.now() < badgeDeadline) {
      const appr = page.locator('.appr').first();
      if ((await appr.count()) > 0) {
        const headerText = (await appr.locator('.ah').textContent()) ?? '';
        // eslint-disable-next-line no-console
        console.log('[badge-loop] approval header:', headerText.replace(/\s+/g, ' ').slice(0, 200));
        if (headerText.includes('sub agent ·')) {
          memberApproval = appr;
          break;
        }
        await appr.getByRole('button', { name: /^Approve/ }).first().click();
        await page.waitForTimeout(500);
      } else {
        await page.waitForTimeout(300);
      }
    }
    if (memberApproval === null) await dumpDiagnostics(page, 'badge-not-found');
    expect(memberApproval).not.toBeNull();
    await memberApproval!.getByRole('button', { name: /^Approve/ }).first().click();

    // The turn completes end to end.
    await page.getByText('All done.', { exact: false }).first().waitFor({ state: 'visible', timeout: 90_000 });
    // Prove the engine really ran main + member LLM turns.
    expect(stack.fakeProvider.requests.length).toBeGreaterThanOrEqual(3);
  });

  it('should converge a member stop across card and panel, with the stopped member aborted', async () => {
    await newPage('auto');
    const stack = await bootApp('auto');
    // A long per-response delay keeps the running window open so the member
    // can be stopped from the panel mid-flight (each scripted LLM response
    // streams for ~2s).
    stack.fakeProvider.setStreamDelay(2000);

    // Forensics: log the task.cancel request/response pair so a failed
    // convergence shows exactly what was addressed.
    const cancelCallIds = new Set<string>();
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload.includes('task.cancel')) {
          const match = /"id"\s*:\s*("?[\w-]+"?)/.exec(payload);
          if (match) cancelCallIds.add(match[1]!);
          // eslint-disable-next-line no-console
          console.log('[ws-sent]', payload.slice(0, 400));
        }
      });
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        const idMatch = /"id"\s*:\s*("?[\w-]+"?)/.exec(payload);
        if (idMatch && cancelCallIds.has(idMatch[1]!)) {
          // eslint-disable-next-line no-console
          console.log('[ws-recv]', payload.slice(0, 400));
        }
      });
    });

    // LLM script: main greets → 2-member swarm batch → members read a file
    // (several queued tool rounds so both stay 'working') → buffered texts
    // absorb the surviving member's final turns, the report step and the
    // main's closing turn (see the swarm test note).
    stack.fakeProvider.nextText('Ready.');
    // Feed the engine's out-of-band session-title side call (see the swarm
    // test above) so it does not consume the AgentSwarm tool call.
    stack.fakeProvider.nextText('Supervision stop acceptance');
    stack.fakeProvider.nextToolCall('AgentSwarm', {
      description: 'supervision stop acceptance batch',
      subagent_type: 'coder',
      model: 'test/model',
      prompt_template: 'P: {{item}}',
      items: ['alpha', 'beta'],
    });
    for (let i = 0; i < 4; i++) stack.fakeProvider.nextToolCall('Read', { file_path: 'note.txt' });
    for (let i = 0; i < 10; i++) stack.fakeProvider.nextText('Member finished its slice of work.');

    await openApp(stack);
    await sendComposer('hello');
    await page.getByText('Ready.', { exact: false }).first().waitFor({ state: 'visible', timeout: 60_000 });

    // Auto permission mode auto-approves the swarm start (manual-only
    // confirmation — see the swarm test above), so the batch starts
    // immediately with no approval card.
    await sendComposer('/swarm run the stop acceptance batch');

    // Open the panel WHILE the batch is running and select a member.
    const card = page.locator('.dc').first();
    await card.waitFor({ state: 'visible', timeout: 60_000 });
    // SDR-007: the member rows are visible on the card during the run too.
    await waitForCount(page, card.locator('.dc-row'), 2, 60_000);
    await card.locator('.dc-open').click();
    const panel = page.locator('.sp').first();
    await panel.waitFor({ state: 'visible', timeout: 30_000 });
    await waitForCount(page, panel.locator('.sp-row'), 2, 60_000);
    await panel.locator('.sp-row').first().click();

    // The selected member is running → its Stop button is present. Stop it
    // (US5.1: the user can call off a member from the focus view).
    const stop = page.locator('.mf-stop');
    await stop.waitFor({ state: 'visible', timeout: 30_000 });
    await stop.click();

    // Convergence in the panel: the member's phase lands on 'failed' and the
    // Stop button disappears (the member is no longer running).
    await panel
      .locator('.ui-badge')
      .filter({ hasText: 'failed' })
      .first()
      .waitFor({ state: 'visible', timeout: 90_000 });
    await page.waitForTimeout(500);
    expect(await page.locator('.mf-stop').count()).toBe(0);

    // Convergence in the card: the chip reaches 2 / 2 (the stopped member
    // counts as settled — completed / failed / aborted are all terminal), and
    // ADR-008 ③ the stopped member lands on the 'aborted' tier, NOT 'failed':
    // user cancellation is not an execution failure, so the delegation card
    // carries no red error border; the stopped member's row shows the aborted
    // phase instead (US5.1: the stop is user intent, honestly presented).
    await waitForText(card.locator('.dc-chip'), '2 / 2', 120_000);
    // Once the main turn completes, the assistant run auto-collapses to its
    // header and the card leaves the DOM — re-expand the fold whenever the
    // card goes missing mid-poll (the run head stays expanded afterwards).
    const settleDeadline = Date.now() + 60_000;
    for (;;) {
      if ((await card.count()) === 0) {
        const collapsedRun = page.locator('button.a-run-hd[aria-expanded="false"]').last();
        if (await collapsedRun.count() > 0) await collapsedRun.click().catch(() => {});
        await page.waitForTimeout(300);
        continue;
      }
      const abortedRows = await card
        .locator('.dc-mphase', { hasText: 'Aborted' })
        .count();
      if (abortedRows >= 1) break;
      if (Date.now() > settleDeadline) {
        await dumpDiagnostics(page, 'stop-no-aborted-row');
        throw new Error('expected the stopped member row to show the aborted phase');
      }
      await page.waitForTimeout(200);
    }
    // User cancellation is not an execution failure — no error aggregate.
    expect((await card.getAttribute('class')) ?? '').not.toContain('err');

    // The stop is scoped: the OTHER member runs its course untouched — its
    // phase lands on completed and the batch result reads
    // "completed: 1, aborted: 1" engine-side (US5.1: stopping one member
    // must not take down the rest of the batch).
    await panel.locator('.sp-row').nth(1).click();
    await panel
      .locator('.ui-badge')
      .filter({ hasText: 'completed' })
      .first()
      .waitFor({ state: 'visible', timeout: 120_000 });
  });
});
