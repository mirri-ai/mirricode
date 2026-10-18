import { createMirriDeviceId } from '@mirri-ai/kosong';
import {
  loadRuntimeConfigSafe,
  resolveConfigPath,
  resolveMirriHome,
  type MirriConfig,
  type MirriHarness,
  type TelemetryClient,
} from '@mirri-ai/mirri-code-sdk';
import {
  initializeTelemetry,
  setTelemetryContext,
  track,
  withTelemetryContext,
} from '@mirri-ai/mirri-telemetry';

import { CLI_USER_AGENT_PRODUCT, WEB_UI_MODE } from '#/constant/app';

export interface CliTelemetryBootstrap {
  readonly homeDir: string;
  readonly deviceId: string;
  readonly firstLaunch: boolean;
}

export interface InitializeCliTelemetryOptions {
  readonly harness: MirriHarness;
  readonly bootstrap: CliTelemetryBootstrap;
  readonly config: TelemetryConfigSlice;
  readonly version: string;
  readonly uiMode: string;
  readonly model?: string;
  readonly sessionId?: string;
}

/** Config slice the telemetry bootstrap reads: the toggle, the default model, and the api key. */
export type TelemetryConfigSlice = {
  readonly defaultModel?: string;
  readonly defaultProvider?: string;
  readonly providers?: MirriConfig['providers'];
  readonly telemetry?: boolean;
};

export function createCliTelemetryBootstrap(): CliTelemetryBootstrap {
  let firstLaunch = false;
  const homeDir = resolveMirriHome();
  const deviceId = createMirriDeviceId(homeDir, {
    onFirstLaunch: () => {
      firstLaunch = true;
    },
  });
  return { homeDir, deviceId, firstLaunch };
}

export function initializeCliTelemetry(options: InitializeCliTelemetryOptions): void {
  initializeTelemetry({
    homeDir: options.harness.homeDir,
    deviceId: options.bootstrap.deviceId,
    enabled: options.config.telemetry !== false,
    appName: CLI_USER_AGENT_PRODUCT,
    version: options.version,
    uiMode: options.uiMode,
    model: options.model ?? options.config.defaultModel,
    sessionId: options.sessionId,
    // API-key-only telemetry auth: the configured default provider's key is
    // sent as the bearer credential; no managed token channel exists anymore.
    getAccessToken: async () => resolveTelemetryAccessToken(options.config),
  });
  if (options.bootstrap.firstLaunch) {
    options.harness.track('first_launch');
  }
}

export interface InitializeServerTelemetryOptions {
  readonly version: string;
}

/**
 * Bootstrap telemetry for the `mirri web` / `mirri server run` host.
 *
 * Mirrors {@link initializeCliTelemetry}: mints the device id, reads config to
 * honor the `telemetry` toggle and pick up the default model, attaches the
 * sink with `ui_mode = "web"`, and returns a {@link TelemetryClient} the
 * caller hands to `startServer` via `coreProcessOptions.telemetry`. That wires
 * the same real client into `MirriCore`, so agent-core events emitted inside the
 * server process (`mcp_connected`, `session_load_failed`, plan-mode / cron
 * events, …) actually leave the process carrying the enriched context
 * (`app_name` / `version` / `ui_mode` / `model` / platform fields).
 *
 * The returned client wraps the `@mirri-ai/mirri-telemetry` module
 * functions, so the module-level `track` / `withTelemetryContext` (used to
 * fire the startup event) share the same underlying client + sink.
 */
export function initializeServerTelemetry(
  options: InitializeServerTelemetryOptions,
): TelemetryClient {
  const bootstrap = createCliTelemetryBootstrap();
  const configPath = resolveConfigPath({ homeDir: bootstrap.homeDir });
  const config = readServerTelemetryConfig(configPath);

  initializeTelemetry({
    homeDir: bootstrap.homeDir,
    deviceId: bootstrap.deviceId,
    enabled: config.telemetry !== false,
    appName: CLI_USER_AGENT_PRODUCT,
    version: options.version,
    uiMode: WEB_UI_MODE,
    model: config.defaultModel,
    getAccessToken: async () => resolveTelemetryAccessToken(config),
  });

  return {
    track,
    withContext: withTelemetryContext,
    setContext: setTelemetryContext,
  };
}

function readServerTelemetryConfig(configPath: string): TelemetryConfigSlice {
  try {
    const { config, fileError } = loadRuntimeConfigSafe(configPath);
    // A broken config fails the server on its own inside MirriCore; for
    // telemetry just degrade to "enabled, no model" so we never block startup.
    if (fileError !== undefined) return {};
    return config;
  } catch {
    return {};
  }
}

/**
 * The bearer credential for telemetry requests, resolved from the configured
 * default provider's API key. Managed login is retired, so the OAuth access
 * token channel is gone; telemetry attaches the api key when one is configured
 * and sends unauthenticated otherwise.
 */
function resolveTelemetryAccessToken(config: TelemetryConfigSlice): string | null {
  const providerName = config.defaultProvider ?? config.defaultModel?.split('/')[0];
  const apiKey = providerName === undefined ? undefined : config.providers?.[providerName]?.apiKey;
  return typeof apiKey === 'string' && apiKey.length > 0 ? apiKey : null;
}
