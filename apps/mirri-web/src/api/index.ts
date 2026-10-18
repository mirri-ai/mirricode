// apps/mirri-web/src/api/index.ts
// Singleton factory for the MirriWebApi client.
//
// The web app talks to one of two transport backends:
//   - DaemonMirriWebApi  — the v1/v2 REST+WS dual-channel adapter (legacy)
//   - WsRpcApi            — the v3 (Rust core) single-channel JSON-RPC adapter
//
// Backend selection happens at boot via `/meta.backend`:
//   "v3" → WsRpcApi; anything else → DaemonMirriWebApi.
//
// The compile-time env `VITE_MIRRICODE_BACKEND` can force the choice:
//   "ts"   → always DaemonMirriWebApi (skip the /meta round-trip)
//   "v3"   → always WsRpcApi
//   "auto" (default) → detect via /meta

import { buildRestUrl, readMirriApiConfig } from './config';
import type { MirriWebApi } from './types';
import { DaemonMirriWebApi } from './daemon/client';
import { WsRpcApi, buildRpcWsUrl } from './daemon/ws-rpc-api';
import { getCredential } from './daemon/serverAuth';

let singleton: MirriWebApi | undefined;
let initPromise: Promise<MirriWebApi> | null = null;

/**
 * Pre-initialise the singleton by detecting the backend via `/meta`.
 * Must be called (and awaited) before `getMirriWebApi()` on first use.
 * Safe to call multiple times — concurrent callers share one init.
 */
export async function initMirriWebApi(): Promise<MirriWebApi> {
  if (singleton) return singleton;
  if (initPromise) return initPromise;
  initPromise = doInit();
  try {
    return await initPromise;
  } finally {
    initPromise = null;
  }
}

async function doInit(): Promise<MirriWebApi> {
  const forced = import.meta.env.VITE_MIRRICODE_BACKEND;
  const config = readMirriApiConfig();

  // Force paths skip the /meta round-trip.
  if (forced === 'ts') {
    singleton = new DaemonMirriWebApi(config);
    return singleton;
  }
  if (forced === 'v3' || forced === 'rust') {
    singleton = createRustApi(config.serverHttpUrl);
    return singleton;
  }

  // Auto-detect via /meta.
  try {
    const metaUrl = buildRestUrl(config.serverHttpUrl, '/meta');
    const res = await fetch(metaUrl, { headers: authHeaders() });
    if (res.ok) {
      // The daemon wraps /meta in a `{ code, msg, data }` envelope — when it
      // does, the backend marker lives at `data.backend`. (The v3 server
      // sends the envelope; bare shapes are tolerated for older/other
      // engines.)
      const meta = (await res.json()) as {
        backend?: string;
        data?: { backend?: string };
      };
      const backend = meta.data?.backend ?? meta.backend;
      if (backend === 'v3') {
        singleton = createRustApi(config.serverHttpUrl);
        return singleton;
      }
    }
  } catch {
    // /meta unreachable or malformed — fall through to the default transport.
  }

  singleton = new DaemonMirriWebApi(config);
  return singleton;
}

function createRustApi(origin: string): MirriWebApi {
  const url = buildRpcWsUrl(origin);
  const token = getCredential();
  // WsRpcApi implements Partial<MirriWebApi>; the cast is intentional —
  // missing methods will surface as runtime errors only if called, and the
  // migration is incremental (methods are added over time).
  return new WsRpcApi(url, origin, token) as unknown as MirriWebApi;
}

function authHeaders(): Record<string, string> {
  const token = getCredential();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

/**
 * Synchronous accessor. Returns the already-initialised singleton, or creates
 * a default DaemonMirriWebApi if initMirriWebApi() has not been called yet
 * (preserves the pre-migration boot path for code paths that don't await).
 */
export function getMirriWebApi(): MirriWebApi {
  singleton ??= new DaemonMirriWebApi(readMirriApiConfig());
  return singleton;
}
