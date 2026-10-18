/**
 * `mcpConfig` domain — `IMcpConfigService` implementation.
 *
 * The App-scope config read/write surface shared by every workspace: reads
 * walk the same three layers the loader discovers (`resolveMcpJsonPaths`:
 * user `mcp.json`, project-root `.mcp.json`, `.mirri-code/mcp.json` — the
 * project-root file reached through the git work-tree probe, exactly the
 * loader's discovery), with later layers replacing same-name entries at
 * their first-appearance position. Writes are thin adapters over the
 * mcp.json file functions: `user` targets the user file, `workspace` the
 * project layer only (the project root's Claude-compatible file is a
 * read-only git-shared asset, adjudication A2), and the unified
 * `setToolFilter` patch targets the highest-precedence layer defining the
 * name. Plugin-bundled servers (`plugin-<id>:<name>` runtime names) are
 * read-only here: `set`/`remove` reject them (A1), and their tool filters
 * write the plugin user state and broadcast the plugin-contribution change
 * so the MCP layer re-resolves live (the plugin-status batch 3c surface).
 * The scope-less view appends plugin entries under their runtime-name slots.
 *
 * The workspace context resolves through the lifecycle's first materialized
 * handler (materializing the first known workspace when none is open, the
 * kap-server precedent); the project layers are gated by workspace trust —
 * the loader's gate — so the view mirrors what the engine would actually
 * load. Writes trigger no manual eventing: every workspace handler's
 * `workspaceMcpConfig` service watches these files and reloads on change
 * ("route writes, watcher reloads"). Bound at App scope, constructed on
 * first resolution.
 */

import { randomUUID } from 'node:crypto';
import { dirname } from 'pathe';

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';

import { Error2, ErrorCodes } from '#/errors';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IPluginService } from '#/app/plugin/plugin';
import { IWorkspaceService } from '#/app/workspace/workspace';
import { IWorkspaceLifecycleService } from '#/app/workspaceLifecycle/workspaceLifecycle';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IWorkspaceContext } from '#/workspace/workspaceContext/workspaceContext';
import { IWorkspaceTrust } from '#/workspace/workspaceTrust/workspaceTrust';
import { McpConnectionManager } from '#/mcpCore/connection-manager';
import { McpServerConfigSchema } from '#/mcpCore/config-schema';
import { expandEnvVars } from '#/workspace/workspaceMcpConfig/internal/env-expand';
import {
  readMcpJson,
  resolveMcpJsonPaths,
} from '#/workspace/workspaceMcpConfig/internal/config-loader';
import type { PluginMcpServerEntry } from '#/app/plugin/types';
import type { McpServerConfig } from '#/mcpCore/config-schema';

import {
  IMcpConfigService,
  type McpConfigEntry,
  type McpConfigScope,
  type McpConfigSource,
  type McpConfigTestArgs,
  type McpConfigTestResult,
  type McpEffectiveConfig,
  type McpToolFilterPatch,
} from './mcpConfig';
import {
  effectiveMcpConfig,
  presentMcpConfigEntry,
  readLayerRevision,
  readMcpJsonEntries,
  removeMcpServerEntry,
  setMcpServerToolFilter,
  upsertMcpServerEntry,
  userMcpJsonPath,
} from './mcpJsonFile';

const PLUGIN_PREFIX = 'plugin-';

interface ConfigLayer {
  readonly source: McpConfigSource;
  readonly path: string;
}

interface WorkspaceView {
  readonly cwd: string;
  readonly trusted: boolean;
}

export class McpConfigService implements IMcpConfigService {
  declare readonly _serviceBrand: undefined;

  constructor(
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IHostFileSystem private readonly fs: IHostFileSystem,
    @IPluginService private readonly plugins: IPluginService,
    @IWorkspaceLifecycleService private readonly lifecycle: IWorkspaceLifecycleService,
    @IWorkspaceService private readonly workspaceService: IWorkspaceService,
  ) {}

  async list(scope?: McpConfigScope): Promise<readonly McpConfigEntry[]> {
    const layers = await this.resolveLayers(scope);

    // A later layer replaces an earlier same-name entry at its
    // first-appearance position (the loader's per-server merge).
    const entries: McpConfigEntry[] = [];
    const index = new Map<string, number>();
    for (const layer of layers) {
      const servers = await readMcpJsonEntries(this.fs, layer.path);
      // F-3: the revision of the file defining these entries — the
      // optimistic-lock token write calls side-carry.
      const revision = await readLayerRevision(this.fs, layer.path);
      for (const [name, raw] of Object.entries(servers)) {
        const entry: McpConfigEntry = {
          name,
          source: layer.source,
          revision,
          config: presentMcpConfigEntry(raw),
          effective: effectiveMcpConfig(raw),
        };
        const slot = index.get(name);
        if (slot === undefined) {
          index.set(name, entries.length);
          entries.push(entry);
        } else {
          entries[slot] = entry;
        }
      }
    }
    // The scope-less merged view appends the plugin-contributed servers
    // under their runtime-name slots (the `plugin-` prefix is reserved, so
    // no config-file entry can collide, A1); a per-server-disabled server
    // still lists so the view can re-enable it.
    if (scope === undefined) {
      for (const pluginEntry of await this.plugins.listMcpServerEntries()) {
        entries.push({
          name: pluginEntry.name,
          runtimeName: pluginEntry.runtimeName,
          source: 'plugin',
          // No config-layer file backs a plugin-bundled entry (its
          // connection definition is plugin-owned, A1), so it has no
          // writable-layer revision to lock on.
          revision: 'absent',
          config: presentMcpConfigEntry(pluginEntry.config),
          effective: effectivePluginEntryView(pluginEntry),
        });
      }
    }
    return entries;
  }

  async set(scope: McpConfigScope, name: string, config: McpServerConfig): Promise<void> {
    assertWritableServerName(name, 'mcp.config.set');
    const path = await this.writableScopePath(scope);
    await upsertMcpServerEntry(this.fs, path, name, config);
  }

  async remove(scope: McpConfigScope, name: string): Promise<void> {
    assertWritableServerName(name, 'mcp.config.remove');
    const path = await this.writableScopePath(scope);
    await removeMcpServerEntry(this.fs, path, name);
  }

  async setToolFilter(name: string, filter: McpToolFilterPatch): Promise<void> {
    if (name.startsWith(PLUGIN_PREFIX)) {
      // Plugin-bundled server: the patch writes the plugin user state (A1 —
      // the connection definition stays manifest-owned); the per-server
      // mutation broadcasts the plugin-contribution change so the MCP layer
      // re-resolves live.
      const pluginEntries = await this.plugins.listMcpServerEntries();
      const target = pluginEntries.find((entry) => entry.runtimeName === name);
      if (target === undefined) {
        throw new Error2(
          ErrorCodes.MCP_SERVER_NOT_FOUND,
          `MCP server '${name}' is not defined by any installed plugin`,
        );
      }
      await this.plugins.setPluginMcpServerToolFilter({
        id: target.pluginId,
        server: target.name,
        filter,
      });
      return;
    }
    // Highest precedence first: project → project root → user.
    const layers = await this.resolveLayers();
    for (let i = layers.length - 1; i >= 0; i--) {
      const servers = await readMcpJsonEntries(this.fs, layers[i]!.path);
      if (Object.hasOwn(servers, name)) {
        await setMcpServerToolFilter(this.fs, layers[i]!.path, name, filter);
        return;
      }
    }
    throw new Error2(
      ErrorCodes.MCP_SERVER_NOT_FOUND,
      `MCP server '${name}' is not defined in any configuration layer`,
    );
  }

  async test(args: McpConfigTestArgs): Promise<McpConfigTestResult> {
    const config = await this.testTargetConfig(args);

    // A connectivity test ignores the stored enablement toggle: the manager
    // short-circuits disabled entries without connecting, which would
    // answer a misleading "success".
    const scratch = new McpConnectionManager();
    const tempName = `__test_${randomUUID()}`;
    try {
      await scratch.connect(tempName, { ...config, enabled: true });
      const entry = scratch.get(tempName);
      if (entry?.status === 'connected') {
        return { status: 'ok', toolCount: entry.toolCount };
      }
      return { status: 'error', toolCount: 0, error: entry?.error ?? 'connection failed' };
    } finally {
      // Remove (not just drop) so the scratch stdio child is reaped
      // deterministically.
      await scratch.remove(tempName);
    }
  }

  /**
   * Resolve the `mcp.config.test` target config: a direct `config`
   * (expanded here — expansion happens at test time only, adjudication A5)
   * or a persisted `scope` + `name` read through the addressed layer with
   * the loader's semantics (expansion and stdio `cwd` anchoring, exactly
   * as a real connect would), trust-gated like the list views. Within the
   * workspace scope the project layer wins over the read-only project root.
   */
  private async testTargetConfig(args: McpConfigTestArgs): Promise<McpServerConfig> {
    if (args.config !== undefined) {
      const expanded = expandEnvVars(args.config);
      const parsed = McpServerConfigSchema.safeParse(expanded);
      if (!parsed.success) {
        throw new Error2(
          ErrorCodes.VALIDATION_FAILED,
          `the test config is invalid: ${parsed.error.issues
            .map((issue) => `${issue.path.join('.')}: ${issue.message}`)
            .join('; ')}`,
        );
      }
      return parsed.data;
    }
    const scope = args.scope;
    const name = args.name;
    if (scope === undefined || name === undefined) {
      throw new Error2(
        ErrorCodes.VALIDATION_FAILED,
        'mcp.config.test takes a config, or a scope and name',
      );
    }

    // Highest precedence first within the scope (the setToolFilter search
    // order); a broken or missing layer file skips — the entry may live in
    // a later layer (the rust parity).
    const layers = await this.resolveLayers(scope);
    for (let i = layers.length - 1; i >= 0; i--) {
      const layer = layers[i]!;
      let servers: Record<string, McpServerConfig>;
      try {
        servers = await readMcpJson(this.fs, layer.path, {
          stdioCwdBase: layer.source === 'projectRoot' ? dirname(layer.path) : undefined,
        });
      } catch {
        continue;
      }
      if (Object.hasOwn(servers, name)) return servers[name]!;
    }
    throw new Error2(
      ErrorCodes.MCP_SERVER_NOT_FOUND,
      `MCP server '${name}' is not defined in the ${scope} configuration layer`,
    );
  }

  // ---------------------------------------------------------------------------
  // Layer resolution
  // ---------------------------------------------------------------------------

  /**
   * The layers a view or write addresses, in merge order (user → project
   * root → project). The project layers appear only while a trusted
   * workspace is open — the loader's trust gate, so the view mirrors what
   * the engine would actually load.
   */
  private async resolveLayers(scope?: McpConfigScope): Promise<readonly ConfigLayer[]> {
    const userLayer: ConfigLayer = {
      source: 'user',
      path: userMcpJsonPath(this.bootstrap.homeDir),
    };
    if (scope === 'user') return [userLayer];

    const workspace = await this.resolveWorkspaceView();
    if (workspace === undefined || !workspace.trusted) {
      return scope === 'workspace' ? [] : [userLayer];
    }

    const paths = await resolveMcpJsonPaths({
      fs: this.fs,
      cwd: workspace.cwd,
      homeDir: this.bootstrap.homeDir,
    });
    const projectLayers: readonly ConfigLayer[] = [
      { source: 'projectRoot', path: paths.projectRoot },
      { source: 'project', path: paths.project },
    ];
    return scope === 'workspace' ? projectLayers : [userLayer, ...projectLayers];
  }

  /** The write target for a scope: `user` → the user file, `workspace` → the project layer (A2). */
  private async writableScopePath(scope: McpConfigScope): Promise<string> {
    if (scope === 'user') return userMcpJsonPath(this.bootstrap.homeDir);

    const workspace = await this.resolveWorkspaceView();
    if (workspace === undefined || !workspace.trusted) {
      throw new Error2(
        ErrorCodes.VALIDATION_FAILED,
        "scope 'workspace' requires an open, trusted workspace",
      );
    }
    const paths = await resolveMcpJsonPaths({
      fs: this.fs,
      cwd: workspace.cwd,
      homeDir: this.bootstrap.homeDir,
    });
    return paths.project;
  }

  /**
   * The first open workspace's view, materializing the first known
   * workspace when none is open (the kap-server `mcpGlobal` precedent —
   * global surfaces operate on the first workspace).
   */
  private async resolveWorkspaceView(): Promise<WorkspaceView | undefined> {
    let handle = this.lifecycle.handlers.list()[0];
    if (handle === undefined) {
      const first = (await this.workspaceService.list())[0];
      if (first === undefined) return undefined;
      handle = await this.lifecycle.handlerFor({ workspaceId: first.id, root: first.root });
    }
    return {
      cwd: handle.accessor.get(IWorkspaceContext).cwd,
      trusted: handle.accessor.get(IWorkspaceTrust).isTrusted(),
    };
  }
}

/** Reject writes to plugin-bundled servers (A1: the connection definition is plugin-owned). */
function assertWritableServerName(name: string, method: string): void {
  if (name.startsWith(PLUGIN_PREFIX)) {
    throw new Error2(
      ErrorCodes.VALIDATION_FAILED,
      `'${name}' is a plugin-bundled MCP server; plugin connection definitions are read-only (${method}, adjudication A1)`,
    );
  }
}

/**
 * The effective view for a plugin entry: the manifest-derived state
 * overlaid with the recorded per-server user state — a recorded field
 * overrides, a recorded-absent tool filter clears (the record is the
 * complete user state, the rust list-view parity).
 */
function effectivePluginEntryView(entry: PluginMcpServerEntry): McpEffectiveConfig {
  const state = entry.state;
  if (state === undefined) return effectiveMcpConfig(entry.config);
  const effective: {
    enabled: boolean;
    enabledTools?: readonly string[];
    disabledTools?: readonly string[];
    startupTimeoutMs?: number;
    toolTimeoutMs?: number;
  } = { ...effectiveMcpConfig(entry.config), enabled: state.enabled };
  if (state.enabledTools === undefined) delete effective.enabledTools;
  else effective.enabledTools = [...state.enabledTools];
  if (state.disabledTools === undefined) delete effective.disabledTools;
  else effective.disabledTools = [...state.disabledTools];
  return effective;
}

registerScopedService(
  LifecycleScope.App,
  IMcpConfigService,
  McpConfigService,
  ScopeActivation.OnDemand,
  'mcpConfig',
);
