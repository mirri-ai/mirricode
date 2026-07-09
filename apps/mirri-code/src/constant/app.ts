import { ErrorCodes } from '@mirri-ai/mirri-code-sdk';

export const PRODUCT_NAME = 'Mirri Code';
export const CLI_COMMAND_NAME = 'mirri';
export const PROCESS_NAME = 'mirri-code';

// Used in telemetry app names and HTTP User-Agent headers.
export const CLI_USER_AGENT_PRODUCT = 'mirri-code-cli';
export const CLI_UI_MODE = 'shell';
// Telemetry ui_mode for the `mirri web` / `mirri server run` host. Same product
// as the CLI (CLI_USER_AGENT_PRODUCT); the surface is distinguished by ui_mode.
export const WEB_UI_MODE = 'web';

// Give telemetry a short flush window without making CLI exit feel stuck.
export const CLI_SHUTDOWN_TIMEOUT_MS = 3000;

// Upper bound on headless (`mirri -p`) shutdown. A wedged cleanup step (e.g. a
// SessionEnd hook, an MCP shutdown, or a connection blackholed by a restrictive
// firewall) must not keep a completed run alive indefinitely — once this elapses
// we stop waiting on cleanup and let the run return.
export const PROMPT_CLEANUP_TIMEOUT_MS = 8000;

// Grace after a headless run has fully completed (turn done, cleanup attempted)
// before force-exiting. `mirri -p` otherwise relies on the event loop draining to
// exit; a stray ref'd handle (socket/timer/child) left over from the run would
// wedge it. The guard timer is unref'd, so a healthy run still exits naturally
// well before this fires.
export const HEADLESS_FORCE_EXIT_GRACE_MS = 2000;

// Max time to wait for buffered stdout/stderr to flush before arming the
// force-exit fallback. A slow/piped consumer's still-draining stdio is a
// legitimate ref'd handle — flushing first prevents the fallback from
// truncating completed output. Bounded so a permanently-stuck consumer can't
// re-introduce the hang.
export const HEADLESS_STDIO_DRAIN_TIMEOUT_MS = 10000;

// Published npm package name; this can differ from the executable command.
export const NPM_PACKAGE_NAME = '@mirri-ai/mirri-code';

// App-owned data paths. SDK/core runtime config is intentionally not routed here.
export const MIRRICODE_HOME_ENV = 'MIRRICODE_HOME';
export const MIRRICODE_DATA_DIR_NAME = '.mirri-code';
export const MIRRICODE_LOG_DIR_NAME = 'logs';
export const MIRRICODE_CACHE_DIR_NAME = 'cache';
export const MIRRICODE_UPDATE_DIR_NAME = 'updates';
export const MIRRICODE_BIN_DIR_NAME = 'bin';
export const MIRRICODE_UPDATE_STATE_FILE_NAME = 'latest.json';
export const MIRRICODE_UPDATE_INSTALL_STATE_FILE_NAME = 'install.json';
export const MIRRICODE_UPDATE_INSTALL_LOCK_FILE_NAME = 'install.lock';
export const MIRRICODE_UPDATE_ROLLOUT_LOG_FILE_NAME = 'rollout.log';
export const MIRRICODE_INPUT_HISTORY_DIR_NAME = 'user-history';
export const MIRRICODE_BANNER_DIR_NAME = 'banner';
export const MIRRICODE_BANNER_STATE_FILE_NAME = 'state.json';

// Managed Mirri auth provider key shared with OAuth/SDK config.
export const DEFAULT_OAUTH_PROVIDER_NAME = 'managed:mirri-code';

// SDK/core error code that tells the TUI to show a login-required startup
// notice. Derived from sdk's ErrorCodes so a future rename in core
// auto-propagates instead of silently breaking the startup recovery path.
export const OAUTH_LOGIN_REQUIRED_CODE = ErrorCodes.AUTH_LOGIN_REQUIRED;

export const FEEDBACK_ISSUE_URL = 'https://github.com/mirri-ai/mirricode/issues';

// Sent in the feedback `version` field so the backend can distinguish this
// TypeScript client from clients that send a bare version.
export const FEEDBACK_VERSION_PREFIX = 'mirri-code-';

// Telemetry event name; keep stable for dashboard queries.
export const FEEDBACK_TELEMETRY_EVENT = 'feedback_submitted';

// CDN source of truth: all version checks and native install scripts pull from here.
export const MIRRICODE_CDN_BASE = 'https://install.mirricode.com';
export const MIRRICODE_CDN_LATEST_URL = `${MIRRICODE_CDN_BASE}/latest`;
// Rollout manifest consumed by update checks; the plain-text `/latest` above
// stays unchanged forever — already-shipped clients hard-fail on non-semver
// bodies, and the CDN install scripts read it for fresh installs.
export const MIRRICODE_CDN_LATEST_JSON_URL = `${MIRRICODE_CDN_BASE}/latest.json`;
export const MIRRICODE_TIPS_BANNER_URL = 'https://cdn.kimi.com/mirri-code-tips/tips.json';
export const MIRRICODE_PLUGIN_MARKETPLACE_URL = `${MIRRICODE_CDN_BASE}/plugins/marketplace.json`;
export const MIRRICODE_PLUGIN_MARKETPLACE_URL_ENV = 'MIRRICODE_PLUGIN_MARKETPLACE_URL';
export const MIRRICODE_INSTALL_SH_URL = `${MIRRICODE_CDN_BASE}/install.sh`;
export const MIRRICODE_INSTALL_PS1_URL = `${MIRRICODE_CDN_BASE}/install.ps1`;

// Native install commands, split by platform. Use these for prompt copy and spawn calls only; do not assemble the strings elsewhere.
export const NATIVE_INSTALL_COMMAND_UNIX = `curl -fsSL ${MIRRICODE_INSTALL_SH_URL} | bash`;
export const NATIVE_INSTALL_COMMAND_WIN = `irm ${MIRRICODE_INSTALL_PS1_URL} | iex`;
