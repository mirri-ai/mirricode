/**
 * `workspaceAgentProfileLoader` domain — agent-file model types.
 *
 * Shared types for the agent-file primitives: the parsed single-file
 * definition (`AgentFileDefinition`), scan roots (`AgentFileRoot`) tagged with
 * their source, and the discovery result carrying per-file skip diagnostics.
 * Pure data; no scoped state.
 */

import type { AgentModelPreference } from '#/app/agentProfileCatalog/agentProfileCatalog';
import type { SkippedAgentFile } from '#/app/agentProfileCatalog/agentProfileContribution';

export type { SkippedAgentFile } from '#/app/agentProfileCatalog/agentProfileContribution';

export type AgentFileSource = 'plugin' | 'project' | 'user' | 'extra' | 'explicit';

export interface AgentFileRoot {
  readonly path: string;
  readonly source: AgentFileSource;
}

export interface AgentFileDefinition {
  readonly name: string;
  readonly description: string;
  readonly whenToUse?: string;
  readonly override: boolean;
  readonly tools?: readonly string[];
  readonly disallowedTools?: readonly string[];
  readonly subagents?: readonly string[];
  readonly modelPreference?: AgentModelPreference;
  /** Explicit model alias overriding the global default when this profile is bound. */
  readonly defaultModel?: string;
  /** Capability tags required by this profile (e.g. `code.explore`, `code.read`). */
  readonly capabilitiesRequired?: readonly string[];
  /**
   * Optional named profile this profile extends (UI convenience only — v2
   * stores self-contained profiles, so this is carried for display/round-trip
   * rather than resolved as runtime inheritance).
   */
  readonly extends?: string;
  /**
   * Variables substituted into the prompt template (`${var}`). The v1 UI uses
   * `roleAdditional` here to inject a user-authored role preamble; v2 carries
   * the same map so the field round-trips through the web settings panel.
   */
  readonly promptVars?: Readonly<Record<string, string>>;
  readonly prompt: string;
  readonly path: string;
  readonly source: AgentFileSource;
}

export interface AgentFileDiscoveryResult {
  readonly agents: readonly AgentFileDefinition[];
  readonly skipped: readonly SkippedAgentFile[];
  readonly scannedRoots: readonly string[];
}
