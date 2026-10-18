/**
 * `agentProfileCatalog` domain — the management data plane over the profile
 * catalog: the App-scope `IAgentProfileManagementService` (R5-2).
 *
 * The read surface projects the same merged view the runtime agent tool and
 * the swarm scheduler consume (`IAgentProfileRegistry` through a session
 * catalog projection, or the direct builtin + user-directory scan when no
 * workspace is known) into wire-shaped rows, so every consumer — the REST
 * `/agents` routes, the SDK (`SDKRpcClientV2` through the klient facade), and
 * through them the terminal `/agents` view — reads one source. Rows use the
 * shared snake_case REST spelling, field-compatible with the protocol
 * `ProfileEntry` (the engine never emits the protocol-only optional fields
 * `extends` / `capabilities`).
 *
 * The write surface is the enable/disable toggle channel: a full `.md`
 * rewrite under the user agents root (`serializeAgentMd` + write + legacy
 * `.yaml`/`.yml` cleanup + `IUserAgentProfileLoader.reload()`), the same
 * implementation the REST `/agents/{name}:enable|:disable` endpoint applies.
 *
 * Workspace context: the v2 read surface is workspace-dependent (project
 * agent files live under the workspace root), but the service is App-scope
 * (klient global calls carry no workspace scope). Callers may therefore pass
 * an explicit `workspaceRoot`; without one the service resolves any open
 * workspace handler (the kap-server precedent) and falls back to the global
 * sources (builtin + user directory) when none exists.
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';

/** Where a row's winning definition came from (wire spelling). */
export type AgentProfileEntrySource =
  | 'builtin'
  | 'user'
  | 'project'
  | 'extra'
  | 'plugin'
  | 'explicit';

/** A same-name candidate that lost the merge (wire spelling). */
export interface AgentProfileEntrySuppressedCandidate {
  readonly source: AgentProfileEntrySource;
  readonly priority: number;
  /**
   * Why the candidate lost. Kept compile-time consistent with the protocol
   * zod enum (`suppressedCandidateSchema`) and the session catalog's
   * producer union — `builtin-override-required` has no producer since D14
   * but stays a wire-tolerance value on all three sides.
   */
  readonly reason: 'priority' | 'builtin-override-required' | 'disabled';
}

/** One listed profile row — snake_case wire shape (see the module header). */
export interface AgentProfileEntry {
  readonly name: string;
  readonly description?: string;
  readonly source: AgentProfileEntrySource;
  readonly builtin: boolean;
  readonly essential: boolean;
  readonly enabled: boolean;
  readonly has_override: boolean;
  readonly file_path?: string;
  readonly default_model?: string;
  readonly capabilities_required?: readonly string[];
  readonly tools?: readonly string[];
  readonly disallowed_tools?: readonly string[];
  readonly subagents?: readonly string[];
  readonly when_to_use?: string;
  readonly system_prompt_template?: string;
  readonly effective_system_prompt?: string;
  readonly suppressed_candidates?: readonly AgentProfileEntrySuppressedCandidate[];
  readonly prompt_vars?: Readonly<Record<string, string>>;
}

/** One agent file the discovery pass skipped (a parse failure). */
export interface SkippedAgentProfileFile {
  readonly path: string;
  readonly reason: string;
}

/** The `list` result: every registered row plus the discovery skips. */
export interface AgentProfileEntriesView {
  readonly entries: readonly AgentProfileEntry[];
  readonly skipped: readonly SkippedAgentProfileFile[];
}

export interface IAgentProfileManagementService {
  readonly _serviceBrand: undefined;

  /**
   * The merged profile view as wire rows. `workspaceRoot` (optional)
   * materializes the workspace the project-level rows resolve against;
   * without it the first open workspace handler wins, and with no workspace
   * at all the global sources (builtin + user directory) are scanned
   * directly.
   */
  list(workspaceRoot?: string): Promise<AgentProfileEntriesView>;

  /**
   * The enable/disable write channel: a full `.md` rewrite of the profile's
   * user file (serializeAgentMd + write to the user agents root + legacy
   * `.yaml`/`.yml` cleanup + loader reload), mirroring the REST
   * `/agents/{name}:enable|:disable` endpoint exactly. Rejects with
   * `agent_profile.not_found` for an unknown name and
   * `agent_profile.essential` when disabling the essential profile.
   */
  setEnabled(name: string, enabled: boolean, workspaceRoot?: string): Promise<void>;

  /**
   * Re-run the user agent-profile loader's discovery on any open workspace
   * handler — the post-write sync step the REST CRUD routes apply after
   * their own file writes.
   */
  reload(): Promise<void>;
}

export const IAgentProfileManagementService: ServiceIdentifier<IAgentProfileManagementService> =
  createDecorator<IAgentProfileManagementService>('agentProfileManagementService');
