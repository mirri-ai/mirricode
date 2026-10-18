/**
 * `workspaceAgentProfileLoader` domain — `IUserAgentProfileLoader` implementation.
 *
 * Discovers user agent profiles through `bootstrap` home paths and `hostFs`,
 * reports skipped files through `log`, and appends the `<home>/SYSTEM.md`
 * prompt-override profile (synthesized against the builtin default from the
 * App builtin loader) after the scanned profiles so it wins same-name
 * collisions within this contribution. The user roots are global os
 * directories, but per-workspace registration keeps every contribution
 * flowing through the same workspace-tagged lane. Bound at Workspace scope.
 *
 * The user agent roots are watched through `hostFsWatch` and reloaded
 * debounced, so an external edit of a user agent file re-registers this
 * contribution without any mutation request — the fix for the registry
 * snapshot only refreshing after a route-level mutation. (Mirrors the
 * project-root watch in `workspaceAgentProfileLoaderService`.)
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import { TimeoutTimer } from '#/_base/utils/timer';
import { subtreeWatchFilter } from '#/_base/utils/paths';
import { dirname } from 'pathe';
import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import type { AgentProfile } from '#/app/agentProfileCatalog/agentProfileCatalog';
import { IBuiltinAgentProfileLoader } from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IHostFsWatchService } from '#/os/interface/hostFsWatch';
import { IWorkspaceContext } from '#/workspace/workspaceContext/workspaceContext';

import { discoverAgentFiles, userAgentRootCandidates, userAgentRoots } from '@mirri-ai/agent-profile';

import { AgentProfileLoaderBase } from './internal/agentProfileLoader';
import {
  AGENT_PROFILE_SOURCE_PRIORITY,
  type AgentProfileContribution,
} from '#/app/agentProfileCatalog/agentProfileContribution';
import { profilesFromDiscovery } from './internal/agentProfileFromFile';
import { loadSystemMdProfile } from './internal/systemFile';
import { IUserAgentProfileLoader } from './userAgentProfileLoader';
import { toProfileFs } from './internal/profileFs';

const WATCH_DEBOUNCE_MS = 200;

export class UserAgentProfileLoaderService
  extends AgentProfileLoaderBase
  implements IUserAgentProfileLoader
{
  declare readonly _serviceBrand: undefined;

  protected readonly sourceId = 'user';
  protected readonly priority = AGENT_PROFILE_SOURCE_PRIORITY.user;

  private readonly watchDebounce = this._register(new TimeoutTimer());
  private readonly watchReady: Promise<void>;

  private defaultProfile: AgentProfile;

  constructor(
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IHostFileSystem private readonly fs: IHostFileSystem,
    @ILogService log: ILogService,
    @IBuiltinAgentProfileLoader private readonly builtin: IBuiltinAgentProfileLoader,
    @IAgentProfileRegistry registry: IAgentProfileRegistry,
    @IWorkspaceContext private readonly workspace: IWorkspaceContext,
    @IHostFsWatchService private readonly fsWatch: IHostFsWatchService,
  ) {
    super(registry, log);
    this.defaultProfile = builtin.getDefault();
    this.watchReady = this.watchUserAgentRoots();
    this.start();
  }

  protected override get workspaceKey(): string {
    return this.workspace.workspaceId;
  }

  getDefaultProfile(): AgentProfile {
    return this.defaultProfile;
  }

  protected async load(): Promise<AgentProfileContribution> {
    await this.watchReady;
    const profileFs = toProfileFs(this.fs);
    const roots = await userAgentRoots(
      profileFs,
      this.bootstrap.homeDir,
      this.bootstrap.osHomeDir,
      (message, error) => {
        this.log.warn(message, error);
      },
    );
    const systemMd = await loadSystemMdProfile(
      this.fs,
      this.bootstrap.homeDir,
      this.builtin.getDefault(),
      (message) => this.log.warn(message),
    );
    this.defaultProfile = systemMd ?? this.builtin.getDefault();
    const contribution = profilesFromDiscovery(
      await discoverAgentFiles({
        fs: profileFs,
        roots,
        warn: (message) => this.log.warn(message),
      }),
      (context) => this.defaultProfile.systemPrompt(context),
    );
    if (systemMd === undefined) return contribution;
    return { ...contribution, profiles: [...contribution.profiles, systemMd] };
  }

  /**
   * Watch the user agent-root candidates and reload debounced on change.
   *
   * Chokidar (the `hostFsWatch` backend) cannot watch a path that does not
   * exist yet — no events fire for it (verified empirically) — so each
   * candidate is watched through an existing ancestor with a subtree filter,
   * the same pattern `workspaceAgentProfileLoaderService` uses for project
   * roots:
   * - Brand root `<home>/agents`: the mirri home always exists; watching it
   *   with a filter narrowed to the agents subtree also catches the agents
   *   directory being created later.
   * - Generic root `<osHomeDir>/.agents/agents`: only consulted when the
   *   brand root is absent. Watching the real OS home would be far too
   *   heavy, so its `.agents` parent is watched only when it already
   *   exists; a from-scratch creation of the whole chain is picked up by
   *   the next explicit reload (any mutation request reloads the loader).
   */
  private async watchUserAgentRoots(): Promise<void> {
    const { candidates } = userAgentRootCandidates(this.bootstrap.homeDir, this.bootstrap.osHomeDir);
    const [brandRoot, genericRoot] = candidates;
    if (brandRoot !== undefined) {
      this.watchCandidateThroughAncestor(brandRoot, this.bootstrap.homeDir);
    }
    if (genericRoot !== undefined) {
      const genericParent = dirname(genericRoot);
      try {
        const stat = await this.fs.stat(genericParent);
        if (stat.isDirectory) this.watchCandidateThroughAncestor(genericRoot, genericParent);
      } catch {
        // `.agents` does not exist yet — nothing to watch; see the doc note.
      }
    }
  }

  private watchCandidateThroughAncestor(candidate: string, ancestor: string): void {
    const handle = this.fsWatch.watch(ancestor, {
      ignored: subtreeWatchFilter(ancestor, [candidate]),
    });
    this._register(handle);
    this._register(
      handle.onDidChange(() => {
        this.watchDebounce.cancelAndSet(() => {
          void this.reload().catch((error) => {
            this.log.warn(`agent profile loader "user" reload failed: ${String(error)}`);
          });
        }, WATCH_DEBOUNCE_MS);
      }),
    );
  }
}

registerScopedService(
  LifecycleScope.Workspace,
  IUserAgentProfileLoader,
  UserAgentProfileLoaderService,
  ScopeActivation.OnScopeCreated,
  'workspaceAgentProfileLoader',
);
