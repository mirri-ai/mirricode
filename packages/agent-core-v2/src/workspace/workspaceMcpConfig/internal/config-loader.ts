/**
 * `workspaceMcpConfig` domain — MCP JSON config discovery and loading.
 *
 * Resolves the three MCP config files for a cwd (user `mcp.json` under the
 * kimi home, project-root `.mcp.json` — the root discovered through the
 * `git` domain's work-tree probe — and `.mirri-code/mcp.json` under the cwd)
 * and loads them with user < project-root < project precedence, normalizing
 * relative stdio `cwd` entries against the project-root file's directory.
 * `includeProject: false` skips the two project-level files and loads the
 * user file only — the workspace-trust gate: the project files ship with
 * the checkout, so an untrusted workspace must never see them. All
 * filesystem access goes through the os `IHostFileSystem`, supplied by
 * the caller. Pure functions — no scoped state.
 *
 * Two loading shapes are exported: `loadMcpServers` expands
 * `${VAR}` / `${env:VAR}` environment references in every string value
 * before schema validation (undefined variables resolve to the empty
 * string); `loadSourceMcpServers` returns the same discovery/merge but with
 * references left verbatim for the Settings UI.
 */

import { dirname, isAbsolute, join, normalize, resolve } from 'pathe';

import { findGitWorkTree } from '#/app/git/workTree';
import { resolveMirriHome } from '#/app/bootstrap/bootstrap';
import { OsFsErrors, HostFsError } from '#/os/interface/hostFsErrors';
import type { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { McpServerConfigSchema, type McpServerConfig } from '#/mcpCore/config-schema';
import { ErrorCodes, Error2 } from '#/errors';
import { z } from 'zod';

import { expandEnvVars, type EnvLookup } from './env-expand';

const McpJsonFileSchema = z.object({
  mcpServers: z.record(z.string(), McpServerConfigSchema).default({}),
});

export interface McpJsonPaths {
  readonly user: string;
  readonly projectRoot: string;
  readonly project: string;
}

export interface ResolveMcpJsonPathsInput {
  readonly fs: IHostFileSystem;
  readonly cwd: string;
  readonly homeDir?: string;
}

export async function resolveMcpJsonPaths(input: ResolveMcpJsonPathsInput): Promise<McpJsonPaths> {
  const start = normalize(input.cwd);
  const projectRoot = (await findGitWorkTree(input.fs, start))?.root ?? start;

  return {
    user: join(resolveMirriHome(input.homeDir), 'mcp.json'),
    projectRoot: join(projectRoot, '.mcp.json'),
    project: join(input.cwd, '.mirri-code', 'mcp.json'),
  };
}

export interface LoadMcpServersInput {
  readonly fs: IHostFileSystem;
  readonly cwd: string;
  readonly homeDir?: string;
  readonly includeProject?: boolean;
  /**
   * Overrides the environment-variable lookup used to expand `${VAR}` /
   * `${env:VAR}` references in config strings. Defaults to `process.env`.
   * Injectable for deterministic tests.
   */
  readonly envLookup?: EnvLookup;
}

export async function loadMcpServers(
  input: LoadMcpServersInput,
): Promise<Record<string, McpServerConfig>> {
  const paths = await resolveMcpJsonPaths(input);
  if (input.includeProject === false) {
    return readMcpJson(input.fs, paths.user, { envLookup: input.envLookup });
  }
  const [user, projectRoot, project] = await Promise.all([
    readMcpJson(input.fs, paths.user, { envLookup: input.envLookup }),
    readMcpJson(input.fs, paths.projectRoot, {
      stdioCwdBase: dirname(paths.projectRoot),
      envLookup: input.envLookup,
    }),
    readMcpJson(input.fs, paths.project, { envLookup: input.envLookup }),
  ]);
  return { ...user, ...projectRoot, ...project };
}

/**
 * Load discovery, merge and normalization identical to `loadMcpServers`, but
 * **skips environment-variable expansion**: `${VAR}` / `${env:VAR}`
 * references are left verbatim so the Settings UI can display and edit the
 * literal config the user wrote (and save it back unchanged).
 */
export async function loadSourceMcpServers(
  input: LoadMcpServersInput,
): Promise<Record<string, McpServerConfig>> {
  const paths = await resolveMcpJsonPaths(input);
  if (input.includeProject === false) {
    return readMcpJson(input.fs, paths.user, { expand: false });
  }
  const [user, projectRoot, project] = await Promise.all([
    readMcpJson(input.fs, paths.user, { expand: false }),
    readMcpJson(input.fs, paths.projectRoot, { stdioCwdBase: dirname(paths.projectRoot), expand: false }),
    readMcpJson(input.fs, paths.project, { expand: false }),
  ]);
  return { ...user, ...projectRoot, ...project };
}

interface ReadMcpJsonOptions {
  readonly stdioCwdBase?: string;
  readonly envLookup?: EnvLookup;
  /**
   * Whether to expand `${VAR}` / `${env:VAR}` references before schema
   * validation. Defaults to `true`; `loadSourceMcpServers` passes `false` to
   * preserve the literal config text.
   */
  readonly expand?: boolean;
}

async function readMcpJson(
  fs: IHostFileSystem,
  filePath: string,
  options: ReadMcpJsonOptions = {},
): Promise<Record<string, McpServerConfig>> {
  let text: string;
  try {
    text = await fs.readText(filePath);
  } catch (error: unknown) {
    if (isFileNotFound(error)) return {};
    throw new Error2(ErrorCodes.CONFIG_INVALID, `Failed to read ${filePath}: ${describeError(error)}`, {
      cause: error,
    });
  }

  if (text.trim().length === 0) return {};

  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch (error: unknown) {
    throw new Error2(ErrorCodes.CONFIG_INVALID, `Invalid JSON in ${filePath}: ${describeError(error)}`, {
      cause: error,
    });
  }

  try {
    const expanded = options.expand === false ? data : expandEnvVars(data, options.envLookup);
    return normalizeMcpServers(McpJsonFileSchema.parse(expanded).mcpServers, options);
  } catch (error: unknown) {
    throw new Error2(ErrorCodes.CONFIG_INVALID, `Invalid MCP server config in ${filePath}: ${describeError(error)}`, {
      cause: error,
    });
  }
}

function normalizeMcpServers(
  servers: Record<string, McpServerConfig>,
  options: ReadMcpJsonOptions,
): Record<string, McpServerConfig> {
  const stdioCwdBase = options.stdioCwdBase;
  if (stdioCwdBase === undefined) return servers;

  return Object.fromEntries(
    Object.entries(servers).map(([name, config]) => [name, normalizeStdioCwd(config, stdioCwdBase)]),
  );
}

function normalizeStdioCwd(config: McpServerConfig, cwdBase: string): McpServerConfig {
  if (config.transport !== 'stdio') return config;
  const cwd = config.cwd === undefined ? cwdBase : resolvePath(cwdBase, config.cwd);
  return { ...config, cwd };
}

function resolvePath(base: string, value: string): string {
  return isAbsolute(value) ? normalize(value) : resolve(base, value);
}

function isFileNotFound(error: unknown): boolean {
  return error instanceof HostFsError && error.code === OsFsErrors.codes.OS_FS_NOT_FOUND;
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
