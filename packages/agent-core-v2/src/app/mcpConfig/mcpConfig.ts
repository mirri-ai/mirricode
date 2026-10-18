/**
 * `mcpConfig` domain — the layered MCP config read/write surface types and
 * the `IMcpConfigService` contract (the engine side of the `mcpConfigService`
 * klient contract: `mcp.config.list` / `set` / `remove` / `setToolFilter`,
 * `docs/specs/mcp-architecture.md` §4.1).
 *
 * Reads present the stored text verbatim — `${VAR}` environment references
 * are preserved and never resolved on this surface (expansion only happens
 * at connect time, adjudication A5).
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';

import type { McpServerConfig, McpStoredServerConfig } from '#/mcpCore/config-schema';

/** Write/read scope: the user file, or the workspace's project layer. */
export type McpConfigScope = 'user' | 'workspace';

/** Which config layer an entry comes from in a layered view. */
export type McpConfigSource = 'user' | 'projectRoot' | 'project' | 'plugin';

/**
 * The tool-filter patch `setToolFilter` applies to one entry: a present
 * field replaces the stored one, an absent field clears it (the filter
 * object is the entry's complete new filter state).
 */
export interface McpToolFilterPatch {
  readonly enabledTools?: readonly string[];
  readonly disabledTools?: readonly string[];
}

/** The per-server effective state a layered view reports. */
export interface McpEffectiveConfig {
  readonly enabled: boolean;
  readonly enabledTools?: readonly string[];
  readonly disabledTools?: readonly string[];
  readonly startupTimeoutMs?: number;
  readonly toolTimeoutMs?: number;
}

/** One layered-view entry: the stored config plus the effective state. */
export interface McpConfigEntry {
  readonly name: string;
  readonly source: McpConfigSource;
  /**
   * Content-addressed revision of the source layer file (FNV-1a 64 over the
   * raw bytes, hex; `"absent"` when no layer file backs the entry) — the
   * optimistic-lock token writes side-carry as `expectedRevision` (F-3).
   */
  readonly revision: string;
  /** Present on plugin-sourced entries (runtime name `plugin-<id>:<name>`). */
  readonly runtimeName?: string;
  /**
   * The stored config verbatim — `${VAR}` references preserved (A5). May be
   * a filter-only override entry (no transport) when the source layer holds
   * an override whose connection family stays with the shadowed layer.
   */
  readonly config: McpStoredServerConfig;
  readonly effective: McpEffectiveConfig;
}

/**
 * The `mcp.config.test` arguments: `config` for a direct test of a
 * not-yet-saved entry, or `scope` + `name` to test a persisted entry
 * (resolved from the addressed layer and expanded per A5).
 */
export interface McpConfigTestArgs {
  readonly scope?: McpConfigScope;
  readonly name?: string;
  readonly config?: McpServerConfig;
}

/** The `mcp.config.test` result — the test-connection backend's answer. */
export interface McpConfigTestResult {
  readonly status: 'ok' | 'error';
  readonly toolCount: number;
  readonly error?: string;
}

export interface IMcpConfigService {
  readonly _serviceBrand: undefined;

  /**
   * Layered view: no scope = the merged view (each entry annotated with its
   * source layer); `user` = the user file as stored; `workspace` = the
   * project-root + project layers merged. The project layers appear only
   * while a trusted workspace is open (the loader's trust gate — the view
   * mirrors what the engine would actually load).
   */
  list(scope?: McpConfigScope): Promise<readonly McpConfigEntry[]>;

  /**
   * Validate and upsert one server entry: `user` targets the user file,
   * `workspace` the project layer only (the project root's `.mcp.json` is a
   * read-only git-shared asset, adjudication A2). The stored value is the
   * caller's raw config, `${VAR}` references included.
   */
  set(scope: McpConfigScope, name: string, config: McpServerConfig): Promise<void>;

  /** Remove one server entry (idempotent: removing an absent name succeeds). */
  remove(scope: McpConfigScope, name: string): Promise<void>;

  /**
   * Apply a tool-filter patch to the highest-precedence layer defining the
   * name (project → project root → user); a name no layer defines answers
   * `mcp.server_not_found`.
   */
  setToolFilter(name: string, filter: McpToolFilterPatch): Promise<void>;

  /**
   * Fresh-connect test of one config on a scratch manager (the
   * management UI's "test connection" backend): a direct `config`
   * (`${VAR}` references expanded here, A5 — expansion happens at test
   * time only) or a persisted `scope` + `name` resolved from the
   * addressed layer with the loader's semantics. The scratch connection
   * runs under a `__test_<uuid>` runtime name and is removed afterwards;
   * the stored enablement toggle is bypassed so a disabled entry still
   * gets a real connectivity answer.
   */
  test(args: McpConfigTestArgs): Promise<McpConfigTestResult>;
}

export const IMcpConfigService: ServiceIdentifier<IMcpConfigService> =
  createDecorator<IMcpConfigService>('mcpConfigService');
