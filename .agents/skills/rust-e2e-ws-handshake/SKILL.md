---
name: rust-e2e-ws-handshake
description: Pitfalls when writing or debugging mirri-web e2e tests that spawn the real rust engine and speak WebSocket. Use when a vitest e2e flakes only under full-suite load, when a WS handshake frame (server_hello) "times out" but works when the file runs alone, when reading vitest output that shows "N skipped" after a suite failure, or when writing a new e2e helper that connects to `ws://.../api/v1/rpc` or `/api/v1/ws`.
---

# Rust E2E WebSocket Handshake Pitfalls

Hard-won lessons from the 2026-10-26 snapshot-contract flake. Each one
cost hours to diagnose because the observable symptoms all point the wrong
way.

## Pitfall 1: `server_hello` listener attached after `await open`

The bug that started all this. The `ws` library, when the client event
loop is busy, may deliver the 101 upgrade response and the server's first
frame in ONE socket data event — and then emits `'open'` and `'message'`
in the SAME synchronous stack.

```ts
// WRONG — loses the frame under load
await new Promise<void>((resolve, reject) => {
  ws.once('open', () => resolve());
  ws.once('error', reject);
});
ws.on('message', handler); // ← runs one microtask too late: 'message'
                           //   was already emitted inside the open event
```

The `await open` continuation (a microtask) runs only after the entire
data event finishes — by which time the frame was emitted to a socket
with no listener. **The frame is not slow, it is gone.** No timeout ever
recovers it.

```ts
// RIGHT — attach the listener before awaiting anything
const helloReceived = new Promise<void>((resolve, reject) => {
  /* timer + ws.on('message', ...) + ws.once('error', ...) */
});
await new Promise<void>((resolve, reject) => {
  ws.once('open', () => resolve());
  ws.once('error', reject);
});
await helloReceived;
```

Reference implementations in the repo:
- correct helper: `apps/mirri-web/test/backend-switch.e2e.test.ts` (`readFirstWsFrame` — attaches `on('message')` synchronously in the Promise constructor)
- corrected client: `apps/mirri-web/test/snapshot-contract.e2e.test.ts` (`RpcClient.connect`)
- product code is safe by construction: `apps/mirri-web/src/api/daemon/json-rpc-client.ts` sets `onmessage` immediately after constructing the socket

## Pitfall 2: widening the timeout does not fix frame loss

When a handshake frame never arrives, raising 10s to 30s only makes the
failure slower to report. Before touching timeouts, ask: **is the frame
late, or is it lost?** A lost frame is a listener-ordering bug (Pitfall 1)
or a dropped connection — distinguish by checking whether the socket
`close`d or whether the server actually sent (rust log level below
`--log-level warn` hides it).

## Pitfall 3: vitest reports a failed `beforeAll` as "N skipped"

When a suite's `beforeAll` throws, vitest marks the suite's tests
`skipped` and fails the file. Output like:

```
test/foo.e2e.test.ts (2 tests | 2 skipped) 30852ms
FAIL test/foo.e2e.test.ts > <suite name>
Error: server_hello not received within 30s
```

means the suite RAN (the 30s+ duration is the beforeAll), not that it
was skipped via `describe.skipIf`. Do not confuse it with the
`MIRRICODE_E2E_SKIP_RUST=1` skip (those suites show ~0ms duration).

## Pitfall 4: "works alone, fails in full suite" = event-loop starvation races

The rust engine is innocent until proven guilty. Under full-suite
parallelism the Node worker's event loop is saturated (transform/import
times in the vitest header are the tell), which changes client-side
timing: coalesced socket reads, delayed microtasks. Reproduce
deterministically without the engine by blocking the event loop:

```ts
const connecting = RpcClient.connect(url);   // start connect
const t0 = Date.now();
while (Date.now() - t0 < 300) { /* spin */ } // force coalesced delivery
const client = await connecting;
```

The regression test in `snapshot-contract.e2e.test.ts`
("should complete the handshake when the 101 response and server_hello
arrive in one data event") uses exactly this with a mock `ws` server —
copy that pattern instead of rebuilding the engine.

## Checklist for new engine-spawning e2e helpers

- [ ] Spawn `rust/target/debug/mirri serve` with `--dangerous-bypass-auth` and an isolated `MIRRICODE_HOME` (copy `spawnRustServer` in snapshot-contract or fork-streaming).
- [ ] Poll `/api/v1/healthz` before connecting (boot readiness), but do NOT treat healthz green as handshake green.
- [ ] Attach every `ws.on('message')` listener synchronously before the first `await`.
- [ ] Stop the engine via `POST /api/v1/shutdown` in `afterAll` BEFORE closing the fake provider (an in-flight SSE stream blocks fastify's close), with SIGKILL fallback.
- [ ] Guard the handshake with a regression test using the spin-block pattern above, not a real engine.
