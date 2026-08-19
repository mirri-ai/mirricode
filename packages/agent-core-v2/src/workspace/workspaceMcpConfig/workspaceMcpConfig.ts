/**
 * `workspaceMcpConfig` domain — Workspace-scoped MCP server-config owner
 * contract.
 *
 * Defines `IWorkspaceMcpConfigService`, the single source of truth for "which
 * MCP servers should this workspace run": it resolves the MCP config files
 * (user `mcp.json`, project-root `.mcp.json`, `.mirri-code/mcp.json`) and the
 * enabled plugins' contributions — on a name collision the file config wins —
 * with the two project-level files gated by `workspaceTrust` (an untrusted
 * workspace gets the user file and plugin contributions only), then tracks
 * both sources (fs watch on the config files,
 * `plugins.onDidReload`) and publishes the reconciled effective set as a
 * snapshot plus already-diffed change events. Consumers never read config
 * files, the plugin registry, or the `[mcp]` config section themselves: the
 * global timeout preferences are exposed here as {@link tunables} too, so the
 * connection side has exactly one configuration dependency. The domain holds
 * no connection state and never talks to an MCP server; writing config files
 * stays out of the engine. Bound at Workspace scope.
 *
 * The config surface is split into two views with different consumers:
 * `resolvedServers()` returns the merged config with `${VAR}` /
 * `${env:VAR}` references already expanded — the runtime connection side
 * consumes this, so spawned servers always receive real environment values.
 * `sourceServers()` returns the merged config with references left verbatim —
 * the Settings UI reads this to display and edit what the user actually wrote.
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';
import type { Event } from '#/_base/event';

import type { McpServerConfig } from '#/mcpCore/config-schema';

export interface McpServersChange {
  readonly upsert: Readonly<Record<string, McpServerConfig>>;
  readonly remove: readonly string[];
}

export interface McpTunables {
  readonly startupTimeoutMs?: number;
  readonly toolTimeoutMs?: number;
}

export interface IWorkspaceMcpConfigService {
  readonly _serviceBrand: undefined;

  readonly ready: Promise<void>;

  /**
   * The merged effective server set with `${VAR}` / `${env:VAR}` references
   * expanded against the process environment. Consumed by the runtime
   * connection side. Values in configs other than the resolved ones must
   * not be used to spawn anything.
   */
  resolvedServers(): Readonly<Record<string, McpServerConfig>>;

  /**
   * The merged server set as written in the config files — `${VAR}` /
   * `${env:VAR}` references left verbatim. Consumed by the Settings UI to
   * display and edit the user's literal config; never used to run servers.
   */
  sourceServers(): Readonly<Record<string, McpServerConfig>>;

  tunables(): McpTunables;

  readonly onDidChange: Event<McpServersChange>;
}

export const IWorkspaceMcpConfigService: ServiceIdentifier<IWorkspaceMcpConfigService> =
  createDecorator<IWorkspaceMcpConfigService>('workspaceMcpConfigService');
