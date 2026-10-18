/**
 * `mcpConfig` domain — the mcp.json file format: raw read/write primitives
 * behind the layered MCP config surface.
 *
 * Pure functions over an injected `IHostFileSystem` (no scoped state):
 * reads return the stored text verbatim — `${VAR}` references and external
 * field forms untouched (expansion only happens at connect time, adjudication
 * A5) — and writes preserve every key the writer did not address (unknown
 * top-level keys, sibling entries). Stored-form validation proves a value is
 * a legal server config without URL-format checks (the loader expands
 * references before its own stricter parse, so a stored `${VAR}` url must
 * validate — mirroring mirri-mcp's serde validation). A missing file reads
 * as an empty `mcpServers` envelope; a corrupt one (unparseable, non-object
 * root, non-object envelope) answers `config.invalid` — the loader's
 * semantics, so the management view surfaces exactly what the engine sees.
 */

import { dirname, join } from 'pathe';
import { z } from 'zod';

import { resolveMirriHome } from '#/app/bootstrap/bootstrap';
import { atomicWrite } from '#/_base/utils/fs';
import { Error2, ErrorCodes } from '#/errors';
import {
  McpServerHttpConfigSchema,
  McpServerSseConfigSchema,
  McpServerStdioConfigSchema,
  inferTransportTag,
  type McpServerConfig,
} from '#/mcpCore/config-schema';
import { HostFsError, OsFsErrors } from '#/os/interface/hostFsErrors';
import type { IHostFileSystem } from '#/os/interface/hostFileSystem';

import type { McpEffectiveConfig, McpToolFilterPatch } from './mcpConfig';

/** The user-layer mcp.json path under the given resolved mirri home (`bootstrap.homeDir`). */
export function userMcpJsonPath(homeDir: string): string {
  return join(resolveMirriHome(homeDir), 'mcp.json');
}

/**
 * Read one config layer as stored: the raw JSON root with `${VAR}`
 * references and unknown top-level keys preserved. A missing or blank file
 * reads as an empty `mcpServers` envelope.
 */
export async function readMcpJsonRoot(
  fs: IHostFileSystem,
  path: string,
): Promise<Record<string, unknown>> {
  let text: string;
  try {
    text = await fs.readText(path);
  } catch (error: unknown) {
    if (isFileNotFound(error)) return { mcpServers: {} };
    throw configReadError(path, error);
  }

  if (text.trim().length === 0) return { mcpServers: {} };

  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch (error: unknown) {
    throw new Error2(
      ErrorCodes.CONFIG_INVALID,
      `Invalid JSON in ${path}: ${describeError(error)}`,
      { cause: error },
    );
  }

  if (typeof data !== 'object' || data === null || Array.isArray(data)) {
    throw new Error2(ErrorCodes.CONFIG_INVALID, `MCP config ${path} is not a JSON object`);
  }
  return data as Record<string, unknown>;
}

/**
 * Content-addressed revision of one config layer file (FNV-1a 64 over the
 * raw file bytes, hex; `"absent"` when missing) — the TS mirror of
 * `mirri_mcp::config::layer_revision`, the optimistic-lock version source
 * of the `mcp.config.*` surface (F-3, ADR-005/ADR-008). Content-addressed
 * on purpose: mtime would churn on any touch even when the content is
 * unchanged; for identical bytes the hash is byte-identical to rust's.
 */
export async function readLayerRevision(fs: IHostFileSystem, path: string): Promise<string> {
  let bytes: Uint8Array;
  try {
    bytes = await fs.readBytes(path);
  } catch (error: unknown) {
    if (isFileNotFound(error)) return 'absent';
    throw configReadError(path, error);
  }
  // FNV-1a 64 over the raw bytes — the exact constants the rust mirror uses.
  let hash = 0xcbf29ce484222325n;
  const prime = 0x100000001b3n;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = (hash * prime) & 0xffff_ffff_ffff_ffffn;
  }
  return hash.toString(16).padStart(16, '0');
}

/**
 * Read the raw `mcpServers` entries of one layer, verbatim — no schema
 * validation, no normalization, `${VAR}` references preserved.
 */
export async function readMcpJsonEntries(
  fs: IHostFileSystem,
  path: string,
): Promise<Record<string, unknown>> {
  const root = await readMcpJsonRoot(fs, path);
  return readEntriesOfRoot(root, path);
}

/**
 * Validate a wire-provided server config (any external transport form) and
 * upsert it as `mcpServers.<name>` unchanged — the stored form is the
 * caller's raw value, `${VAR}` references included. The file is created with
 * the envelope (parent directories included) when missing.
 */
export async function upsertMcpServerEntry(
  fs: IHostFileSystem,
  path: string,
  name: string,
  config: unknown,
): Promise<void> {
  if (typeof config !== 'object' || config === null || Array.isArray(config)) {
    throw new Error2(
      ErrorCodes.VALIDATION_FAILED,
      `Invalid MCP server config for '${name}': expected an object`,
    );
  }
  const parsed = StoredMcpServerConfigSchema.safeParse(config);
  if (!parsed.success) {
    throw new Error2(
      ErrorCodes.VALIDATION_FAILED,
      `Invalid MCP server config for '${name}': ${describeZodIssues(parsed.error.issues)}`,
    );
  }

  const root = await readMcpJsonRoot(fs, path);
  writableServersOfRoot(root, path)[name] = config;
  await writeMcpJsonRoot(fs, path, root);
}

/**
 * Remove `mcpServers.<name>`. Removing an absent name (or a missing file)
 * is an idempotent success; returns whether an entry was removed.
 */
export async function removeMcpServerEntry(
  fs: IHostFileSystem,
  path: string,
  name: string,
): Promise<boolean> {
  const root = await readMcpJsonRoot(fs, path);
  const servers = writableServersOfRoot(root, path);
  if (!Object.hasOwn(servers, name)) return false;
  delete servers[name];
  await writeMcpJsonRoot(fs, path, root);
  return true;
}

/**
 * Apply a tool-filter patch to `mcpServers.<name>` in place (present fields
 * replace, absent fields clear). Fails naming the server when the entry is
 * absent — the caller resolves the owning layer first. Skips the write when
 * the patch changes nothing (no mtime churn, no watcher reload).
 */
export async function setMcpServerToolFilter(
  fs: IHostFileSystem,
  path: string,
  name: string,
  patch: McpToolFilterPatch,
): Promise<boolean> {
  const root = await readMcpJsonRoot(fs, path);
  const servers = writableServersOfRoot(root, path);
  const entry = servers[name];
  if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
    throw new Error2(
      ErrorCodes.MCP_SERVER_NOT_FOUND,
      `MCP server '${name}' not found in ${path}`,
    );
  }

  const previous = entry as Record<string, unknown>;
  const next: Record<string, unknown> = { ...previous };
  if (patch.enabledTools === undefined) {
    delete next['enabledTools'];
  } else {
    next['enabledTools'] = [...patch.enabledTools];
  }
  if (patch.disabledTools === undefined) {
    delete next['disabledTools'];
  } else {
    next['disabledTools'] = [...patch.disabledTools];
  }

  if (stableStringify(previous) === stableStringify(next)) return false;

  servers[name] = next;
  await writeMcpJsonRoot(fs, path, root);
  return true;
}

/**
 * Write one config layer atomically (temp file + rename, the token-store
 * pattern — a crash mid-write never truncates the file), pretty JSON with a
 * trailing newline, creating parent directories when missing. Unknown
 * top-level keys of the root are preserved by the caller holding the root
 * object. Node-local like every consumer of the mirri home.
 */
export async function writeMcpJsonRoot(
  fs: IHostFileSystem,
  path: string,
  root: Record<string, unknown>,
): Promise<void> {
  await fs.mkdir(dirname(path), { recursive: true });
  await atomicWrite(path, `${JSON.stringify(root, null, 2)}\n`);
}

// ---------------------------------------------------------------------------
// Presentation views (the `list` projections)
// ---------------------------------------------------------------------------

/**
 * Present a stored entry for the wire: TS preprocess parity — infer the flat
 * `transport` tag when the stored entry omits it (`command` → `stdio`,
 * `url` → `http`); every other byte stays verbatim, `${VAR}` references
 * included (adjudication A5). A non-object entry presents as `{}` — a
 * broken entry still lists so the user can fix it.
 */
export function presentMcpConfigEntry(raw: unknown): McpServerConfig {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return {} as McpServerConfig;
  }
  return inferTransportTag(raw) as McpServerConfig;
}

/**
 * Derive the effective-state view from a stored entry: the parsed
 * server-level overrides (`enabled` defaults true, the tool filters and
 * timeouts present-only). An entry too broken to parse still lists — the
 * management view must show it so the user can fix it — and every field
 * this view carries is an optional override the broken entry does not
 * store, so the defaults are the honest answer (the connect path reports
 * the transport breakage itself).
 */
export function effectiveMcpConfig(raw: unknown): McpEffectiveConfig {
  const parsed = StoredMcpServerConfigSchema.safeParse(raw);
  if (!parsed.success) return { enabled: true };
  const { enabled, enabledTools, disabledTools, startupTimeoutMs, toolTimeoutMs } = parsed.data;
  return {
    enabled: enabled ?? true,
    enabledTools,
    disabledTools,
    startupTimeoutMs,
    toolTimeoutMs,
  };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Stored-form validation: the loader schema with URL-format checks relaxed
 * to plain strings — the loader expands `${VAR}` references before its own
 * parse, so the stored form must validate without them (mirroring
 * mirri-mcp's serde validation, which never checks URL formats).
 */
const StoredMcpServerConfigSchema = z.preprocess(
  inferTransportTag,
  z.discriminatedUnion('transport', [
    McpServerStdioConfigSchema,
    McpServerHttpConfigSchema.omit({ url: true }).extend({ url: z.string() }),
    McpServerSseConfigSchema.omit({ url: true }).extend({ url: z.string() }),
  ]),
);

function readEntriesOfRoot(
  root: Record<string, unknown>,
  path: string,
): Record<string, unknown> {
  const servers = root['mcpServers'];
  if (servers === undefined) return {};
  if (typeof servers !== 'object' || servers === null || Array.isArray(servers)) {
    throw new Error2(ErrorCodes.CONFIG_INVALID, `MCP config ${path}: mcpServers is not an object`);
  }
  return servers as Record<string, unknown>;
}

/**
 * The root's `mcpServers` envelope as a mutable record, normalized to a
 * null-prototype object when not one — writes of pathological names
 * (`__proto__`, `constructor`) become own properties instead of hitting
 * the prototype chain, and inherited members never leak into the merge.
 */
function writableServersOfRoot(
  root: Record<string, unknown>,
  _path: string,
): Record<string, unknown> {
  const servers = root['mcpServers'];
  const writable =
    typeof servers === 'object' && servers !== null && !Array.isArray(servers)
      ? servers
      : undefined;
  const normalized = Object.assign(Object.create(null), writable);
  root['mcpServers'] = normalized;
  return normalized;
}

function configReadError(path: string, error: unknown): Error2 {
  return new Error2(ErrorCodes.CONFIG_INVALID, `Failed to read ${path}: ${describeError(error)}`, {
    cause: error,
  });
}

function isFileNotFound(error: unknown): boolean {
  return error instanceof HostFsError && error.code === OsFsErrors.codes.OS_FS_NOT_FOUND;
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function describeZodIssues(issues: readonly z.ZodIssue[]): string {
  return issues.map((issue) => `${issue.path.join('.')}: ${issue.message}`).join('; ');
}

/** Key-order-insensitive JSON fingerprint (the watcher-reload comparison). */
function stableStringify(value: unknown): string {
  return JSON.stringify(sortKeysDeep(value));
}

function sortKeysDeep(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeysDeep);
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value)
        .toSorted(([a], [b]) => a.localeCompare(b))
        .map(([key, nested]) => [key, sortKeysDeep(nested)]),
    );
  }
  return value;
}
