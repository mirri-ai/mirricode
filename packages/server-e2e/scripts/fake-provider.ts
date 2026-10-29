#!/usr/bin/env node
/**
 * Standalone fake LLM provider for server-e2e runs (docker or local).
 *
 * Serves an OpenAI-compatible SSE endpoint scripted through the shared
 * e2e-harness fake provider. Each completion returns a fixed one-word
 * reply ("OK") by default, so wire-level scenarios get deterministic
 * assistant turns without a real provider. The queue is re-armed from a
 * poll loop (the fake provider has no request hook), so long scenario
 * runs never hit the "no response queued" 500.
 *
 * Prints the container-reachable base URL on stdout, e.g.:
 *   FAKE_PROVIDER_URL=http://192.168.1.10:54321
 */
import { createFakeProviderServer } from '@mirri-ai/e2e-harness';

async function main(): Promise<void> {
  // Bind all interfaces: docker runtimes (colima/docker-desktop) reach the
  // host-side provider through the host LAN address, not 127.0.0.1.
  const server = await createFakeProviderServer({ host: '0.0.0.0' });
  const reply = process.argv[2] ?? 'OK';
  server.nextText(reply);

  const port = new URL(server.baseUrl).port;

  // For docker runtimes, containers reach the host through the special
  // host.docker.internal DNS (the VM's gateway address) — the host LAN
  // address is often blocked by the macOS application firewall. Print
  // both; docker:e2e consumes the host.docker.internal form.
  console.log(`FAKE_PROVIDER_URL=http://host.docker.internal:${port}`);
  console.log(`FAKE_PROVIDER_URL_LAN=http://192.168.5.45:${port}`);
  console.log(`(scripted reply: "${reply}") — Ctrl-C to stop`);

  // Re-arm the scripted reply as requests are consumed (500 ms poll).
  let seen = server.requests.length;
  setInterval(() => {
    if (server.requests.length > seen) {
      seen = server.requests.length;
      server.nextText(reply);
    }
  }, 500).unref();

  await new Promise<never>(() => {
    /* run until killed */
  });
}

void main().catch((error) => {
  console.error(error);
  process.exit(1);
});
