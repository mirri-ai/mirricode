/**
 * `workspaceAgentProfileLoader` domain — agent-file model types.
 *
 * Shared types for the agent-file primitives: the parsed single-file
 * definition (`AgentFileDefinition`), scan roots (`AgentFileRoot`) tagged with
 * their source, and the discovery result carrying per-file skip diagnostics.
 * Pure data; no scoped state.
 */

import type { SkippedAgentFile } from '#/app/agentProfileCatalog/agentProfileContribution';

export type { SkippedAgentFile } from '#/app/agentProfileCatalog/agentProfileContribution';

// Re-export types from the unified @mirri-ai/agent-profile package.
// `AgentFileDefinition` is an alias for `AgentFileDef` to keep existing v2
// consumer code working without renaming.
export type {
  AgentFileDef as AgentFileDefinition,
  AgentFileSource,
  AgentFileRoot,
  AgentFileDiscoveryResult,
} from '@mirri-ai/agent-profile';
