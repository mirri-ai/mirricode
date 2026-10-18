/**
 * Kap server telemetry composition tests — boot the real App scope and storage,
 * then verify config gating, host-appender preservation, cloud delivery wiring,
 * and owned shutdown. The outbound cloud fetch is stubbed at the network
 * boundary.
 */

import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  bootstrap,
  type ITelemetryAppender,
  ITelemetryService,
  logSeed,
  resolveConfigPath,
  resolveLoggingConfig,
  type Scope,
  type ScopeSeed,
  TelemetryService,
} from '@mirri-ai/agent-core-v2';
import { readMirriDeviceId } from '@mirri-ai/kosong';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { initializeServerTelemetry, shutdownServerTelemetry } from '../src/services/telemetry';

describe('server telemetry', () => {
  let home: string | undefined;
  let core: Scope | undefined;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-telemetry-'));
  });

  afterEach(async () => {
    vi.unstubAllGlobals();
    core?.dispose();
    core = undefined;
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true });
      home = undefined;
    }
  });

  async function bootCore(
    toml?: string,
    env?: NodeJS.ProcessEnv,
    seeds: ScopeSeed = [],
  ): Promise<Scope> {
    const resolvedEnv = env ?? {
      ...process.env,
      MIRRI_DISABLE_TELEMETRY: undefined,
    };
    if (toml !== undefined) {
      await writeFile(join(home as string, 'config.toml'), toml, 'utf-8');
    }
    const { app } = bootstrap(
      {
        homeDir: home as string,
        configPath: resolveConfigPath({ homeDir: home as string }),
        env: resolvedEnv,
        clientIdentity: {
          productName: 'test-product',
          version: '0.0.0-test',
          platform: 'test_platform',
        },
      },
      [
        ...logSeed(resolveLoggingConfig({ homeDir: home as string, env: resolvedEnv })),
        ...seeds,
      ],
    );
    core = app;
    return app;
  }

  it('attaches the cloud appender by default and persists the device id', async () => {
    const app = await bootCore();
    const telemetry = await initializeServerTelemetry(app, home as string);
    expect(telemetry.appender).toBeDefined();
    expect(readMirriDeviceId(home as string)).not.toBeNull();
    await shutdownServerTelemetry(telemetry);
  });

  it('keeps host delivery independent of the server-owned cloud appender lifecycle', async () => {
    const cloudFetch = vi.fn(async () => new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', cloudFetch);
    const hostEvents: string[] = [];
    const hostAppender: ITelemetryAppender = {
      track: (event) => hostEvents.push(event),
    };
    const hostTelemetry = new TelemetryService();
    hostTelemetry.addAppender(hostAppender);
    const app = await bootCore(undefined, undefined, [[ITelemetryService, hostTelemetry]]);
    const telemetry = await initializeServerTelemetry(app, home as string);
    const service = app.accessor.get(ITelemetryService);

    service.track('server_probe');

    expect(hostEvents).toEqual(['server_probe']);

    await shutdownServerTelemetry(telemetry);
    service.track('host_after_server_shutdown');
    await service.flush();

    expect(cloudFetch).toHaveBeenCalledOnce();
    expect(hostEvents).toEqual(['server_probe', 'host_after_server_shutdown']);
  });

  it('returns at the deadline when cloud delivery never settles', async () => {
    // A cloud fetch that never resolves — the server telemetry shutdown must
    // still return at the deadline instead of hanging (the OAuth token channel
    // that used to supply this stall was retired with managed login; the
    // network boundary is the hold-up now).
    const cloudFetch = vi.fn(() => new Promise(() => {}));
    vi.stubGlobal('fetch', cloudFetch);
    const app = await bootCore();
    const telemetry = await initializeServerTelemetry(app, home as string);
    app.accessor.get(ITelemetryService).track('server_probe');

    await expect(shutdownServerTelemetry(telemetry, Date.now())).resolves.toBeUndefined();
  });

  it('uses the default provider api key as the telemetry bearer credential', async () => {
    const cloudFetch = vi.fn(
      async (_url: string, _init?: RequestInit) => new Response(null, { status: 204 }),
    );
    vi.stubGlobal('fetch', cloudFetch);
    const app = await bootCore(
      [
        'default_model = "x"',
        '',
        '[providers.x]',
        'type = "kimi"',
        'api_key = "sk-telemetry"',
      ].join('\n'),
    );
    const telemetry = await initializeServerTelemetry(app, home as string);
    app.accessor.get(ITelemetryService).track('server_probe');
    await shutdownServerTelemetry(telemetry);

    const calls = vi.mocked(cloudFetch).mock.calls;
    const init = calls[0]?.[1] as RequestInit | undefined;
    const headers = (init?.headers as Record<string, string> | undefined) ?? {};
    expect(headers['Authorization']).toBe('Bearer sk-telemetry');
  });

  it('keeps the null appender when config sets telemetry = false', async () => {
    const app = await bootCore('telemetry = false\n');
    const telemetry = await initializeServerTelemetry(app, home as string);
    expect(telemetry.appender).toBeUndefined();
    await shutdownServerTelemetry(telemetry);
  });

  it.each(['1', 'true', 't', 'yes', 'y', ' TRUE '])(
    'keeps the null appender when MIRRI_DISABLE_TELEMETRY=%s',
    async (value) => {
      const app = await bootCore(undefined, {
        ...process.env,
        MIRRI_DISABLE_TELEMETRY: value,
      });
      const telemetry = await initializeServerTelemetry(app, home as string);
      expect(telemetry.appender).toBeUndefined();
      expect(readMirriDeviceId(home as string)).toBeNull();
      await shutdownServerTelemetry(telemetry);
    },
  );
});
