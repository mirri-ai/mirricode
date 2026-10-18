/**
 * Browser e2e: real web UI + real kap-server, only LLM mocked.
 *
 * Drives a REAL Chromium (via the `playwright` library) against the REAL
 * built web app served by a REAL kap-server:
 *
 *   Node test process
 *     ├─ boot kap-server (webAssetsDir=apps/mirri-web/dist, config→fake LLM)
 *     ├─ playwright chromium.launch()
 *     ├─ page.goto(http://127.0.0.1:<kapPort>)   ← real built SPA, same origin
 *     ├─ type message, submit, open Sub Agent dock panel
 *     └─ assert what a USER sees: WITHOUT any refresh, once the subagent
 *        finishes, the dock row flips from "Running" to `done` (stop button
 *        gone) — the reported bug is that it never leaves "Running".
 *
 * The ONLY mock is the LLM API (FakeProviderServer). Everything else is the
 * real production stack: kap-server, engine, real web build, real browser.
 */

import { existsSync } from 'node:fs';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { chromium } from 'playwright';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { startReadyServer, type RunningServer } from '../helpers/startReadyServer';

import { createFakeProviderServer, type FakeProviderServer } from '../../../e2e-harness/src/fake-provider-server';

/** The PRODUCTION web build — must exist (`pnpm --filter @mirri-ai/mirri-web build`). */
const WEB_DIST = fileURLToPath(
  new URL('../../../../apps/mirri-web/dist', import.meta.url),
);

// Real-browser test: needs both the built web app and a Playwright Chromium.
// Skip (not fail) when either is missing — e.g. the CI unit-test job, which
// does not build the web app nor install browsers.
const browserE2eAvailable = existsSync(WEB_DIST) && existsSync(chromium.executablePath());

const TEST_HOST_IDENTITY = {
  productName: 'test-host',
  version: '0.0.0-test',
  platform: 'test_platform',
} as const;

describe.skipIf(!browserE2eAvailable)('subagent completed state in the real web UI (browser e2e)', () => {
  let fakeProvider: FakeProviderServer;
  let server: RunningServer;
  let baseUrl: string;
  let homeDir: string;
  let browser: Awaited<ReturnType<typeof chromium.launch>>;
  let page: Awaited<ReturnType<typeof browser.newPage>>;

  beforeAll(async () => {
    fakeProvider = await createFakeProviderServer();
    homeDir = await mkdtemp(join(tmpdir(), 'mirri-browser-e2e-'));
    const config = `
default_model = "fake-model"

[providers.local]
type = "openai"
api_key = "sk-test"
base_url = "${fakeProvider.baseUrl}/v1"

[models.fake-model]
provider = "local"
model = "fake-model"
max_context_size = 262144
`;
    await writeFile(join(homeDir, 'config.toml'), config, 'utf8');

    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir,
      logLevel: 'silent',
      webAssetsDir: WEB_DIST,
    });
    baseUrl = `http://127.0.0.1:${server.port}`;

    browser = await chromium.launch();
    page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  });

  afterAll(async () => {
    await page?.close();
    await browser?.close();
    try { await server?.close(); } catch { /* best-effort */ }
    await fakeProvider?.close();
    if (homeDir) await rm(homeDir, { recursive: true, force: true });
  });

  it('shows the finished subagent as completed in the dock without a refresh', async () => {
      // LLM script: main → Agent tool (BACKGROUND: only background subagents
      // live in the dock — the pill is `v-if="subagentTasks.length > 0"` over
      // background-only tasks) → the member's final answer → main's closing
      // text (distinctive so the turn's completion is assertable).
      fakeProvider.nextToolCall('Agent', {
        prompt: 'Write a hello world function and report completion',
        description: 'Delegate coding task to subagent',
        run_in_background: true,
      });
      fakeProvider.nextText('I have created the hello world function.');
      for (let i = 0; i < 8; i++) {
        fakeProvider.nextText('The subagent completed the task.');
      }

    // 1. Open the real web app, carrying the server token via #token fragment.
    const token = server.authTokenService.getToken();
    await page.goto(`${baseUrl}/#token=${token}`, { waitUntil: 'networkidle' });
    await page.waitForTimeout(2000);

    // 2. Dismiss the first-run "Welcome to Mirri Web" onboarding overlay.
    const getStarted = page.getByText('Get started', { exact: true });
    if (await getStarted.count() > 0) {
      await getStarted.click();
      await page.waitForTimeout(1000);
    }

    // 3. If no workspace, create one ("New workspace" button in the sidebar).
    const newWorkspace = page.getByText('New workspace', { exact: true });
    if (await newWorkspace.count() > 0) {
      const workDirPath = await mkdtemp(join(tmpdir(), 'mirri-browser-work-'));
      await newWorkspace.click();
      await page.waitForTimeout(1200);
      const pathInput = page.locator('input[type="text"]').first();
      if (await pathInput.count() > 0) {
        await pathInput.fill(workDirPath);
        await page.keyboard.press('Enter');
        await page.waitForTimeout(1200);
      }
      const openThisFolder = page.getByText('Open this folder', { exact: true });
      if (await openThisFolder.count() > 0) {
        await openThisFolder.click();
        await page.waitForTimeout(1500);
      }
    }

    // 4. Send a message that delegates to a subagent.
    const composer = page.locator('textarea').last();
    await composer.fill('delegate a coding task to a subagent');
    await composer.press('Enter');

    // 5. The delegation completes end to end: the main agent's closing text
    //    appears only after the Agent tool returned, and the sole member's
    //    answer rides the collapsed delegation card's head (the supervision
    //    UI replaced the old raw "status: completed" result text).
    await page
      .locator('text=/The subagent completed the task/')
      .first()
      .waitFor({ state: 'visible', timeout: 120_000 });
    // A completed assistant run auto-collapses to its "Worked for …" header
    // (the conversation-view auto-collapse), so the delegation card hides
    // inside the fold — expand the run before asserting the card.
    const runHeader = page.locator('button.a-run-hd[aria-expanded="false"]').first();
    await runHeader.waitFor({ state: 'visible', timeout: 30_000 });
    await runHeader.click();
    const delegationCard = page.locator('.dc').first();
    await delegationCard.waitFor({ state: 'visible', timeout: 60_000 });
    expect(
      ((await delegationCard.textContent()) ?? '').includes('I have created the hello world function.'),
    ).toBe(true);

    // 6. Open the Sub Agent dock panel. WITHOUT any refresh the row must show
    //    the task as `done` — the reported bug is that it stays "Running".
    const pill = page.getByText('Sub Agent', { exact: false }).first();
    await pill.waitFor({ state: 'visible', timeout: 30_000 });
    await pill.click();
    const row = page.locator('.taskspane .tp-row').first();
    await row.waitFor({ state: 'visible', timeout: 30_000 });

    // Manual poll for the `done` class (100ms interval, 30s budget).
    let rowClass: string | null = null;
    const classPollDeadline = Date.now() + 30_000;
    while (Date.now() < classPollDeadline) {
      rowClass = await row.getAttribute('class');
      if (rowClass?.includes('done')) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    // eslint-disable-next-line no-console
    console.log(
      '[browser-e2e] row class=', rowClass,
      '| text=', (await row.textContent())?.replace(/\s+/g, ' ').trim(),
      '| stopBtn=', await row.locator('.tp-stop').count(),
      '| rows=', await page.locator('.taskspane .tp-row').count(),
    );
    expect(rowClass ?? '').toContain('done');

    // The stop button is gone once the subagent completed.
    await row.locator('.tp-stop').waitFor({ state: 'detached', timeout: 30_000 });

    // 7. Prove the engine really ran: main + subagent + main LLM calls.
    expect(fakeProvider.requests.length).toBeGreaterThanOrEqual(3);
  });
});