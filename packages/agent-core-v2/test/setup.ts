/**
 * Hermetic experimental-flag state for tests: scrub ambient
 * `MIRRICODE_EXPERIMENTAL_*` env vars inherited from the developer shell
 * (e.g. a globally exported `MIRRICODE_EXPERIMENTAL_FLAG=1`) so flag-driven
 * behavior — including tool schemas embedded in `llm.tools_snapshot`
 * snapshots — stays deterministic and matches CI. Tests opt into flags
 * explicitly via service overrides or `vi.stubEnv`.
 */
for (const key of Object.keys(process.env)) {
  if (key.startsWith('MIRRICODE_EXPERIMENTAL_')) {
    delete process.env[key];
  }
}

/**
 * Fail-fast on `[unexpected]` errors in tests.
 *
 * `safelyCallListener` swallows exceptions thrown by event-bus listeners and
 * routes them through `onUnexpectedError` — in production this keeps a single
 * misbehaving listener from crashing the process, but in tests it silently
 * hides real bugs (the test passes while `[unexpected]` noise floods stderr).
 *
 * The collector is installed at module-load time (not inside beforeEach) so it
 * is active from the very first line of every test file — including DI
 * container construction that happens before any beforeEach hook runs.
 *
 * Per-test attribution uses a watermark: beforeEach records the current error
 * count; afterEach checks whether any new entries were appended during the
 * test and fails if so.
 *
 * Tests that deliberately trigger onUnexpectedError (e.g. wire/telemetry
 * tests) install their own handler via setUnexpectedErrorHandler.  When they
 * call resetUnexpectedErrorHandler() in their afterEach, the default console
 * handler is restored rather than this collector, leaving subsequent tests
 * exposed.  The global beforeEach below re-installs the collector each time to
 * close that gap.
 *
 * Teardown-phase errors (e.g. chokidar watching a socket that was already
 * deleted) arrive after afterEach has already run; they are not attributable
 * to a single test and are best caught by grepping stderr in CI rather than
 * through per-test assertions.
 */
import { afterEach, beforeEach, expect } from 'vitest';
import { setUnexpectedErrorHandler } from '#/_base/errors/unexpectedError';

const collectedErrors: unknown[] = [];

// Install at module-load time so errors from DI construction (which runs
// before the first beforeEach) are captured from the start.
setUnexpectedErrorHandler((err) => {
  collectedErrors.push(err);
});

let watermark = 0;

beforeEach(() => {
  // Re-install in case a previous test's afterEach called
  // resetUnexpectedErrorHandler(), which would have replaced the collector
  // with the default console handler.
  setUnexpectedErrorHandler((err) => {
    collectedErrors.push(err);
  });
  watermark = collectedErrors.length;
});

afterEach(() => {
  const newErrors = collectedErrors.slice(watermark);
  if (newErrors.length > 0) {
    expect.fail(
      `Test produced ${newErrors.length} unexpected error(s):\n${newErrors.map((e) => String(e)).join('\n')}`,
    );
  }
});
