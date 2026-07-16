import { execSync, spawnSync } from 'node:child_process';

import {
  createMirriHarness,
  flushDiagnosticLogsSync,
  log,
  type MirriHarness,
  type TelemetryClient,
} from '@mirri-ai/mirri-code-sdk';
import {
  setCrashPhase,
  setTelemetryContext,
  shutdownTelemetry,
  track,
  withTelemetryContext,
} from '@mirri-ai/mirri-telemetry';

import { CLI_SHUTDOWN_TIMEOUT_MS, CLI_UI_MODE } from '#/constant/app';
import type { TuiConfig } from '#/tui/config';
import { loadTuiConfig, TuiConfigParseError } from '#/tui/config';
import { CHROME_GUTTER } from '#/tui/constant/rendering';
import { MirriTUI } from '#/tui/index';
import { currentTheme, getColorPalette } from '#/tui/theme';
import { combineStartupNotice } from '#/tui/utils/startup';
import { toTerminalHyperlink } from '#/utils/terminal-hyperlink';
import { restoreTerminalModes } from '#/utils/terminal-restore';

import type { CLIOptions } from './options';
import { createCliTelemetryBootstrap, initializeCliTelemetry } from './telemetry';
import { createMirriCodeHostIdentity } from './version';

export async function runShell(
  opts: CLIOptions,
  version: string,
): Promise<void> {
  const startedAt = Date.now();
  const configStartedAt = startedAt;
  let tuiConfig: TuiConfig;
  let configWarning: string | undefined;
  try {
    tuiConfig = await loadTuiConfig();
  } catch (error) {
    if (!(error instanceof TuiConfigParseError)) throw error;
    tuiConfig = error.fallback;
    configWarning = error.message;
  }

  // Initialise the global Theme singleton before pi-tui grabs stdin.
  const palette = await getColorPalette(tuiConfig.theme);
  currentTheme.setPalette(palette);

  const workDir = process.cwd();
  const telemetryBootstrap = createCliTelemetryBootstrap();
  const telemetryClient: TelemetryClient = {
    track,
    withContext: withTelemetryContext,
    setContext: setTelemetryContext,
  };
  const harness = createMirriHarness({
    homeDir: telemetryBootstrap.homeDir,
    identity: createMirriCodeHostIdentity(version),
    skillDirs: opts.skillsDirs,
    telemetry: telemetryClient,
    onOAuthRefresh: (outcome) => {
      if (outcome.success) {
        track('oauth_refresh', { outcome: 'success' });
        return;
      }
      track('oauth_refresh', {
        outcome: 'error',
        reason: outcome.reason,
      });
    },
    sessionStartedProperties: { yolo: opts.yolo, auto: opts.auto, plan: opts.plan, afk: false },
  });
  log.info('mirri-code starting', {
    version,
    uiMode: CLI_UI_MODE,
    nodeVersion: process.version,
    platform: `${process.platform}/${process.arch}`,
    workDir,
  });

  await harness.ensureConfigFile();
  const config = await harness.getConfig();
  for (const warning of (await harness.getConfigDiagnostics()).warnings) {
    configWarning = combineStartupNotice(configWarning, warning);
  }
  const configMs = Date.now() - configStartedAt;
  const tui = new MirriTUI(harness, {
    cliOptions: opts,
    additionalDirs: opts.addDirs?.length ? opts.addDirs : undefined,
    tuiConfig,
    version,
    workDir,
    startupNotice: configWarning,
  });

  initializeCliTelemetry({
    harness,
    bootstrap: telemetryBootstrap,
    config,
    version,
    uiMode: CLI_UI_MODE,
  });
  setCrashPhase('runtime');

  const trackLifecycleForSession = (
    sessionId: string,
    event: string,
    properties?: Parameters<MirriHarness['track']>[1],
  ) => {
    if (sessionId.length === 0) {
      harness.track(event, properties);
      return;
    }
    withTelemetryContext({ sessionId }).track(event, properties);
  };
  const trackLifecycle = (event: string, properties?: Parameters<MirriHarness['track']>[1]) => {
    trackLifecycleForSession(tui.getCurrentSessionId(), event, properties);
  };

  let savedStty: string | undefined;
  try {
    // stty operates on the terminal behind stdin, so stdin must be the TTY —
    // piping /dev/null (ignore) makes stty fail with "not a tty".
    const saved = execSync('stty -g', {
      encoding: 'utf8',
      stdio: ['inherit', 'pipe', 'ignore'],
    });
    savedStty = typeof saved === 'string' ? saved.trim() : undefined;
    execSync('stty -ixon', { stdio: ['inherit', 'ignore', 'ignore'] });
  } catch {
    /* ignore */
  }
  const restoreStty = (): void => {
    if (savedStty === undefined) return;
    const args = savedStty.split(/\s+/).filter((arg) => arg.length > 0);
    if (args.length === 0) return;
    spawnSync('stty', args, { stdio: ['inherit', 'ignore', 'ignore'] });
  };

  // If we crash without going through MirriTUI.stop(), the terminal is left in
  // raw mode with a hidden cursor and XON/XOFF flow control disabled. Restore
  // both before exiting so the user's shell is usable afterwards.
  const emergencyExit = (exitCode: number): void => {
    // The crash log above is only enqueued into the async sink; flush it
    // synchronously or the `process.exit()` below would drop the one line that
    // explains why we crashed. Best-effort: an exit path must never throw.
    try {
      flushDiagnosticLogsSync();
    } catch {
      /* ignore */
    }
    restoreTerminalModes();
    restoreStty();
    process.exit(exitCode);
  };
  const onUncaughtException = (error: unknown): void => {
    try {
      log.error('uncaughtException, restoring terminal and exiting', { error: String(error) });
    } catch {
      /* ignore */
    }
    emergencyExit(1);
  };
  const onUnhandledRejection = (reason: unknown): void => {
    try {
      log.error('unhandledRejection, restoring terminal and exiting', { reason: String(reason) });
    } catch {
      /* ignore */
    }
    emergencyExit(1);
  };
  process.on('uncaughtException', onUncaughtException);
  process.on('unhandledRejection', onUnhandledRejection);
  // Remove the crash handlers once the TUI exits cleanly so repeated runShell()
  // calls in the same process (e.g. tests) don't accumulate process listeners.
  const removeCrashHandlers = (): void => {
    process.off('uncaughtException', onUncaughtException);
    process.off('unhandledRejection', onUnhandledRejection);
  };

  tui.onExit = async (exitCode = 0) => {
    const sessionId = tui.getCurrentSessionId();
    const hasContent = tui.hasSessionContent();
    setCrashPhase('shutdown');
    trackLifecycle('exit', { duration_ms: Date.now() - startedAt });
    await shutdownTelemetry({ timeoutMs: CLI_SHUTDOWN_TIMEOUT_MS });
    const gutter = ' '.repeat(CHROME_GUTTER);
    process.stdout.write(`${gutter}Bye!\n`);
    const hints: string[] = [];
    if (sessionId !== '' && hasContent) {
      hints.push(`${gutter}To resume this session: mirri -r ${sessionId}`);
    }
    if (tui.exitOpenUrl !== undefined) {
      hints.push(`${gutter}open ${toTerminalHyperlink(tui.exitOpenUrl, tui.exitOpenUrl)}`);
    }
    if (hints.length > 0) {
      process.stderr.write(`\n${hints.join('\n')}\n`);
    }
    removeCrashHandlers();
    restoreStty();
    process.exit(exitCode);
  };
  try {
    const initStartedAt = Date.now();
    await tui.start();
    const initMs = Date.now() - initStartedAt;
    const startupSessionId = tui.getCurrentSessionId();
    const mcpMs = await tui.getStartupMcpMs();
    trackLifecycleForSession(startupSessionId, 'startup_perf', {
      duration_ms: Date.now() - startedAt,
      config_ms: configMs,
      init_ms: initMs,
      mcp_ms: mcpMs,
    });
  } catch (error) {
    removeCrashHandlers();
    setCrashPhase('shutdown');
    trackLifecycle('exit', { duration_ms: Date.now() - startedAt });
    await shutdownTelemetry({ timeoutMs: CLI_SHUTDOWN_TIMEOUT_MS });
    await harness.close();
    throw error;
  }
}
