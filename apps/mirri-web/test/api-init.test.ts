// apps/mirri-web/test/api-init.test.ts
// Focused coverage of the boot-time backend auto-detect in `src/api/index.ts`.
//
// The daemon wraps `GET /api/v1/meta` in a `{code,msg,data}` envelope — the
// v3 (Rust core) engine reports `backend: "v3"` INSIDE `data`, so a top-level
// read never detects it. These tests pin the envelope-aware lookup
// (`data.backend` first, flat `backend` as fallback) and the transport each
// shape selects.
//
// The api module caches a singleton, so every case gets a fresh module
// registry via `vi.resetModules()` + dynamic `import()` — and the transport
// classes are re-imported from the same fresh registry so `instanceof` holds.

import { afterEach, describe, expect, it, vi } from 'vitest';

function envelope(data: unknown): Response {
  return new Response(JSON.stringify({ code: 0, msg: 'success', data, request_id: 'r1' }), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

async function loadApi() {
  const { initMirriWebApi } = await import('../src/api');
  const { WsRpcApi } = await import('../src/api/daemon/ws-rpc-api');
  const { DaemonMirriWebApi } = await import('../src/api/daemon/client');
  return { initMirriWebApi, WsRpcApi, DaemonMirriWebApi };
}

describe('initMirriWebApi backend auto-detect', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.resetModules();
  });

  it('should select the v3 transport when /meta reports backend v3 inside the data envelope', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        envelope({ version: '0.1.0', serverId: 'v3-srv', backend: 'v3', capabilities: {} }),
      ),
    );
    const { initMirriWebApi, WsRpcApi } = await loadApi();
    const api = await initMirriWebApi();
    expect(api).toBeInstanceOf(WsRpcApi);
  });

  it('should select the v3 transport when /meta reports a flat top-level backend of v3', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({ backend: 'v3', serverId: 'v3-srv', capabilities: {} }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      ),
    );
    const { initMirriWebApi, WsRpcApi } = await loadApi();
    const api = await initMirriWebApi();
    expect(api).toBeInstanceOf(WsRpcApi);
  });

  it('should select the daemon transport when /meta reports backend v2 inside the data envelope', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        envelope({ server_version: '0.4.0', server_id: 'v2-srv', backend: 'v2', capabilities: {} }),
      ),
    );
    const { initMirriWebApi, DaemonMirriWebApi } = await loadApi();
    const api = await initMirriWebApi();
    expect(api).toBeInstanceOf(DaemonMirriWebApi);
  });

  it('should select the daemon transport when /meta is unreachable', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('connection refused')));
    const { initMirriWebApi, DaemonMirriWebApi } = await loadApi();
    const api = await initMirriWebApi();
    expect(api).toBeInstanceOf(DaemonMirriWebApi);
  });
});