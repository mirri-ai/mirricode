import { type RunningServer, type ServerStartOptions, startServer } from '../../src/start';

export type { RunningServer } from '../../src/start';

/**
 * Start the server and wait for background startup work (workspace catalog
 * reconcile, session index prewarm) to finish before returning. Use this in
 * tests that assert on workspace/session state right after startup — it ensures
 * `healthz` returns `code:0` by the time your test body runs.
 *
 * Tests that specifically exercise the two-state readiness signal should call
 * `startServer()` directly and interact with `serverReadyPromise` themselves.
 */
export async function startReadyServer(opts: ServerStartOptions): Promise<RunningServer> {
  const server = await startServer(opts);
  await server.serverReadyPromise;
  return server;
}
