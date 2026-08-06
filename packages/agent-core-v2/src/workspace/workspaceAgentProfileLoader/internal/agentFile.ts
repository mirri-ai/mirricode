/**
 * `workspaceAgentProfileLoader` domain — agent-file parsing primitives.
 *
 * Re-exported from `@mirri-ai/agent-profile` — the unified parse/serialize
 * implementation shared by agent-core and agent-core-v2.
 */

export {
  parseAgentFileText,
  serializeAgentFile,
  AgentFileParseError,
} from '@mirri-ai/agent-profile';

export type { ParseAgentFileOptions } from '@mirri-ai/agent-profile';
