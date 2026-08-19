import chalk from 'chalk';

import { splitTokenFragment } from '#/cli/sub/web/access-urls';
import { formatReadyBanner, startServerForeground } from '#/cli/sub/web/run';
import { DEFAULT_SERVER_ORIGIN, parseServerOptions } from '#/cli/sub/web/shared';
import { tryResolveServerToken } from '#/cli/sub/web/shared';
import { darkColors } from '#/tui/theme/colors';
import { openUrl } from '#/utils/open-url';
import { getDataDir } from '#/utils/paths';

import { ChoicePickerComponent } from '../components/dialogs/choice-picker';
import { NO_ACTIVE_SESSION_MESSAGE } from '../constant/mirri-tui';
import { formatErrorMessage } from '../utils/event-payload';
import type { SlashCommandHost } from './dispatch';

const WEB_CONFIRM = 'confirm';
const WEB_CANCEL = 'cancel';

/** How long to wait for a running server's /meta before starting a new one. */
const REUSE_PROBE_TIMEOUT_MS = 2_000;

/**
 * `/web` — hand the current session off to the browser.
 *
 * The web UI is served exclusively by the v2 engine (`mirri web` / kap-server),
 * so this command starts that server and opens the active session in it. If a
 * v2 server is already running on the default origin it is reused; otherwise
 * the TUI shuts down and the server takes over this terminal in the foreground
 * (`Ctrl+C` stops it). A confirmation step spells out the consequences and only
 * proceeds when the user presses Enter on Continue.
 */
export async function handleWebCommand(host: SlashCommandHost, _args: string): Promise<void> {
  const session = host.session;
  if (session === undefined) {
    host.showError(NO_ACTIVE_SESSION_MESSAGE);
    return;
  }
  const sessionId = session.id;

  const confirmed = await new Promise<boolean>((resolve) => {
    const picker = new ChoicePickerComponent({
      title: 'Open current session in the Web UI?',
      hint: '↑↓ navigate · Enter select · Esc cancel',
      options: [
        {
          value: WEB_CONFIRM,
          label: 'Continue',
          description:
            'Start the Mirri web server (v2) in the foreground (this terminal stays attached; Ctrl+C stops it) and open this session in your default browser.',
        },
        {
          value: WEB_CANCEL,
          label: 'Cancel',
          description: 'Stay in the terminal UI.',
        },
      ],
      onSelect: (value) => {
        resolve(value === WEB_CONFIRM);
      },
      onCancel: () => {
        resolve(false);
      },
    });
    host.mountEditorReplacement(picker);
  });
  host.restoreEditor();
  if (!confirmed) return;

  // A v2 server that is already running (e.g. the desktop's or a `mirri web`
  // process) can serve the web UI right away — reuse it instead of failing to
  // bind its port. Only a `backend: "v2"` server is eligible: the v1 engine no
  // longer hosts the web UI.
  const reused = await findRunningV2Server();
  if (reused !== undefined) {
    await openAndExit(host, sessionId, reused.origin, tryResolveServerToken(getDataDir()));
    return;
  }

  // No v2 server is running: shut the TUI down and let the server take over
  // this terminal in the foreground (the registered task runs after teardown,
  // where `process.exit` would normally happen). The deep link is opened from
  // the ready hook, once the server is actually listening, and the terminal
  // shows the same ready banner as `mirri web` plus the session deep link.
  host.showStatus('Starting Mirri web server and opening web UI…');
  host.setExitForegroundTask(async () => {
    const runOptions = parseServerOptions({});
    try {
      await startServerForeground(runOptions, {
        onReady: (origin) => {
          // Resolve the token here (after the server is listening) for the
          // first-boot reason: a fresh server writes `server.token` on first
          // boot, so reading it beforehand would miss it and the browser would
          // hit the auth gate.
          const token = tryResolveServerToken(getDataDir());
          const url = webSessionUrl(origin, sessionId, token);
          process.stdout.write(
            formatReadyBanner(origin, runOptions.host, { token }),
          );
          process.stdout.write(`\n  ${sessionLine(url)}\n`);
          openUrl(url);
        },
      });
    } catch (error) {
      process.stderr.write(`Failed to start server: ${formatErrorMessage(error)}\n`);
      process.exit(1);
    }
  });
  await host.stop();
}

/** Probe the default origin for a healthy v2 server (backend reported by
 *  `/api/v1/meta`). Returns the origin to reuse, or `undefined`. */
async function findRunningV2Server(): Promise<{ origin: string } | undefined> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REUSE_PROBE_TIMEOUT_MS);
    const res = await fetch(`${DEFAULT_SERVER_ORIGIN}/api/v1/meta`, {
      signal: controller.signal,
    });
    clearTimeout(timer);
    if (!res.ok) return undefined;
    const body = (await res.json()) as { code?: number; data?: { backend?: string } };
    if (body.code !== 0 || body.data?.backend !== 'v2') return undefined;
    return { origin: DEFAULT_SERVER_ORIGIN };
  } catch {
    return undefined;
  }
}

/** Styled `Session:` line for the foreground handoff; the token fragment is
 * dimmed like in the ready banner so the host/path stands out. */
function sessionLine(url: string): string {
  const label = (text: string): string => chalk.bold.hex(darkColors.textDim)(text);
  const accent = (text: string): string => chalk.hex(darkColors.accent)(text);
  const dim = (text: string): string => chalk.hex(darkColors.textDim)(text);
  const [base, frag] = splitTokenFragment(url);
  return `${label('Session:  ')}${accent(base)}${frag === '' ? '' : dim(frag)}`;
}

/**
 * Open the session deep link in the browser, record it for the exit hints,
 * and shut the TUI down. Used when a server is already running out of
 * process, so exit frees the terminal.
 */
function openAndExit(
  host: SlashCommandHost,
  sessionId: string,
  origin: string,
  token: string | undefined,
): Promise<void> {
  const url = webSessionUrl(origin, sessionId, token);
  host.showStatus(`open ${url}`, 'success');
  if (token !== undefined) {
    host.showStatus(`Token:    ${token}`, 'success');
  }
  openUrl(url);
  host.setExitOpenUrl(url);
  return host.stop();
}

/**
 * Build the deep-link URL the web UI recognises for a session. When a token is
 * known it rides in the `#token=` fragment (never sent to the server, so never
 * logged), so the browser authenticates on load just like `mirri web`.
 */
export function webSessionUrl(origin: string, sessionId: string, token?: string): string {
  const base = `${origin.replace(/\/+$/, '')}/sessions/${encodeURIComponent(sessionId)}`;
  return token === undefined ? base : `${base}#token=${token}`;
}
