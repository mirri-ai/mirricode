/**
 * `workspaceAgentProfileLoader` domain — `IWorkspaceAgentProfileLoader` implementation.
 *
 * Discovers the workspace's agent files (`.mirri-code/agents`, `.agents/agents`
 * under the project root, resolved through `workspaceContext` and `hostFs`)
 * and registers them via the shared loader skeleton. `${base_prompt}` is
 * backed by the user loader's effective default profile. Watches the project
 * agent-root candidates through `hostFsWatch` (watched whether or not they
 * exist yet) and reloads debounced, so a project agent-file change
 * re-registers this contribution only. Bound at Workspace scope: the scan is
 * per handler and the registration dies with it.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import { TimeoutTimer } from '#/_base/utils/timer';
import { subtreeWatchFilter } from '#/_base/utils/paths';
import { discoverAgentFiles, projectAgentRootCandidates, projectAgentRoots } from '@mirri-ai/agent-profile';
import { AgentProfileLoaderBase } from '#/workspace/workspaceAgentProfileLoader/internal/agentProfileLoader';
import {
  AGENT_PROFILE_SOURCE_PRIORITY,
  type AgentProfileContribution,
} from '#/app/agentProfileCatalog/agentProfileContribution';
import { profilesFromDiscovery } from './internal/agentProfileFromFile';
import { toProfileFs } from './internal/profileFs';
import { IUserAgentProfileLoader } from '#/workspace/workspaceAgentProfileLoader/userAgentProfileLoader';
import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IHostFsWatchService } from '#/os/interface/hostFsWatch';
import { IWorkspaceContext } from '#/workspace/workspaceContext/workspaceContext';

import { IWorkspaceAgentProfileLoader } from './workspaceAgentProfileLoader';

const WATCH_DEBOUNCE_MS = 200;

export class WorkspaceAgentProfileLoaderService
  extends AgentProfileLoaderBase
  implements IWorkspaceAgentProfileLoader
{
  declare readonly _serviceBrand: undefined;

  protected readonly sourceId = 'workspace';
  protected readonly priority = AGENT_PROFILE_SOURCE_PRIORITY.workspace;

  private readonly watchDebounce = this._register(new TimeoutTimer());
  private readonly watchReady: Promise<void>;

  constructor(
    @IWorkspaceContext private readonly workspace: IWorkspaceContext,
    @IHostFileSystem private readonly fs: IHostFileSystem,
    @ILogService log: ILogService,
    @IUserAgentProfileLoader private readonly user: IUserAgentProfileLoader,
    @IHostFsWatchService private readonly fsWatch: IHostFsWatchService,
    @IAgentProfileRegistry registry: IAgentProfileRegistry,
  ) {
    super(registry, log);
    this.watchReady = this.watchProjectAgentRoots();
    this.start();
  }

  protected override get workspaceKey(): string {
    return this.workspace.workspaceId;
  }

  protected async load(): Promise<AgentProfileContribution> {
    await this.watchReady;
    const profileFs = toProfileFs(this.fs);
    const roots = await projectAgentRoots(profileFs, this.workspace.cwd, (message, error) => {
      this.log.warn(message, error);
    });
    return profilesFromDiscovery(
      await discoverAgentFiles({
        fs: profileFs,
        roots,
        warn: (message) => this.log.warn(message),
      }),
      (context) => this.user.getDefaultProfile().systemPrompt(context),
    );
  }

  private async watchProjectAgentRoots(): Promise<void> {
    const profileFs = toProfileFs(this.fs);
    const { projectRoot, candidates } = await projectAgentRootCandidates(
      profileFs,
      this.workspace.cwd,
    );
    const handle = this.fsWatch.watch(projectRoot, {
      ignored: subtreeWatchFilter(projectRoot, candidates),
    });
    this._register(handle);
    this._register(
      handle.onDidChange(() => {
        this.watchDebounce.cancelAndSet(() => {
          void this.reload().catch((error) => {
            this.log.warn(`agent profile loader "workspace" reload failed: ${String(error)}`);
          });
        }, WATCH_DEBOUNCE_MS);
      }),
    );
  }
}

registerScopedService(
  LifecycleScope.Workspace,
  IWorkspaceAgentProfileLoader,
  WorkspaceAgentProfileLoaderService,
  ScopeActivation.OnScopeCreated,
  'workspaceAgentProfileLoader',
);
