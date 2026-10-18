/**
 * Server telemetry bootstrap — wires agent-core-v2's `CloudAppender` into the
 * App-scoped `ITelemetryService` so engine events emitted inside the server
 * process (`session_started`, turn / tool / permission events, `image_compress`,
 * …) actually leave the process. Mirrors the v1 `mirri web` host
 * (`initializeServerTelemetry` in apps/mirri-code): same product app name, the
 * surface distinguished by `ui_mode = "web"`, the config `telemetry` toggle
 * honored at startup (a change takes effect on restart).
 */

import {
  type CloudAppender,
  createCloudAppender,
  IBootstrapService,
  IConfigService,
  type IDisposable,
  ITelemetryService,
  type Scope,
} from '@mirri-ai/agent-core-v2';
import { createMirriDeviceId } from '@mirri-ai/kosong';

// Same product as the CLI; the surface is distinguished by ui_mode (v1
// convention: CLI_USER_AGENT_PRODUCT / WEB_UI_MODE in apps/mirri-code).
const SERVER_TELEMETRY_APP_NAME = 'mirri-code-cli';
const SERVER_TELEMETRY_UI_MODE = 'web';
const TELEMETRY_DISABLE_ENV = 'MIRRI_DISABLE_TELEMETRY';
const TELEMETRY_DISABLE_ENV_VALUES = new Set(['1', 'true', 't', 'yes', 'y']);

/**
 * Cap on how long server close waits for its best-effort final flush. A wedged
 * telemetry endpoint must not hold shutdown hostage.
 */
const TELEMETRY_SHUTDOWN_TIMEOUT_MS = 3_000;

export interface ServerTelemetry {
  /** Present only when telemetry is enabled by both config and environment. */
  readonly appender?: CloudAppender;
  readonly registration?: IDisposable;
}

function isTelemetryDisabledByEnv(core: Scope): boolean {
  const value = core.accessor.get(IBootstrapService).getEnv(TELEMETRY_DISABLE_ENV);
  return value !== undefined && TELEMETRY_DISABLE_ENV_VALUES.has(value.trim().toLowerCase());
}

export async function initializeServerTelemetry(
  core: Scope,
  homeDir: string,
): Promise<ServerTelemetry> {
  const service = core.accessor.get(ITelemetryService);
  const config = core.accessor.get(IConfigService);
  await config.ready;
  const enabled = config.get('telemetry') !== false;
  if (!enabled || isTelemetryDisabledByEnv(core)) return {};

  const appender = createCloudAppender(core.accessor, {
    deviceId: createMirriDeviceId(homeDir),
    appName: SERVER_TELEMETRY_APP_NAME,
    uiMode: SERVER_TELEMETRY_UI_MODE,
    model: config.get<string>('defaultModel') ?? undefined,
    // API-key-only telemetry auth, mirroring the CLI host: the default
    // provider's configured key is the bearer credential; managed-login OAuth
    // is retired, so no token channel exists anymore.
    getAccessToken: async () => resolveTelemetryAccessToken(config),
  });
  const registration = service.addAppender(appender);
  try {
    // The server is long-lived: flush on a timer, not only at the threshold.
    appender.startPeriodicFlush();
  } catch (error) {
    registration.dispose();
    throw error;
  }
  return { appender, registration };
}

export async function shutdownServerTelemetry(
  telemetry: ServerTelemetry,
  deadlineMs = Date.now() + TELEMETRY_SHUTDOWN_TIMEOUT_MS,
): Promise<void> {
  telemetry.registration?.dispose();
  if (telemetry.appender === undefined) return;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      telemetry.appender.shutdown(),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, Math.max(0, deadlineMs - Date.now()));
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * The bearer credential for telemetry requests, resolved from the configured
 * default provider's API key. Managed login is retired, so the OAuth access
 * token channel is gone; telemetry attaches the api key when one is configured
 * and sends unauthenticated otherwise. Mirrors the CLI host's
 * `resolveTelemetryAccessToken` (apps/mirri-code).
 */
function resolveTelemetryAccessToken(config: IConfigService): string | null {
  const defaultModel = config.get<string>('defaultModel');
  const defaultProvider = config.get<string>('defaultProvider');
  const providerName = defaultProvider ?? defaultModel?.split('/')[0];
  if (providerName === undefined) return null;
  const providers = config.inspect<Record<string, { apiKey?: string }>>('providers').userValue;
  const apiKey = providers?.[providerName]?.apiKey;
  return typeof apiKey === 'string' && apiKey.length > 0 ? apiKey : null;
}
