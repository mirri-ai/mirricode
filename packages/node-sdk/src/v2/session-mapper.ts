/**
 * v2 → v1 session shape mapping — pure functions projecting the
 * agent-core-v2 session shapes onto the v1 SDK wire shapes.
 *
 * Two gaps are bridged here:
 * - the v2 `SessionSummary` (session index read model) carries no filesystem
 *   facts, so the v1 `SessionSummary`'s `workDir` / `sessionDir` /
 *   `additionalDirs` come in as pre-resolved `SessionSummaryFacts` supplied
 *   by the caller (derived from the session context and workspace catalog);
 * - the v2 `SessionMeta` document keeps epoch-ms timestamps and a `cwd` field
 *   where the v1 `SessionMeta` keeps ISO strings and `workDir`.
 * Everything else is a field-level port (the wire vocabularies were inherited
 * from v1 — see `agent-core-v2/src/session/sessionMetadata/`).
 */
import type {
  AgentMeta as V1AgentMeta,
  SessionMeta as V1SessionMeta,
} from '@mirri-ai/agent-core';
import type {
  AgentMeta as V2AgentMeta,
  SessionMeta as V2SessionMeta,
  SessionSummary as V2SessionSummary,
} from '@mirri-ai/agent-core-v2';

import type { JsonObject, SessionSummary } from '#/types';

/** v1 summary fields the v2 index summary does not carry, resolved by the caller. */
export interface SessionSummaryFacts {
  readonly workDir: string;
  readonly sessionDir: string;
  readonly additionalDirs?: readonly string[];
}

/**
 * Project a v2 session-index summary onto the v1 `SessionSummary` shape,
 * merging in the filesystem facts the index intentionally does not carry.
 * The v1 `metadata` (a bare `JsonObject`) maps from v2's `custom` document.
 */
export function v2SummaryToSessionSummary(
  summary: V2SessionSummary,
  facts: SessionSummaryFacts,
): SessionSummary {
  return {
    id: summary.id,
    title: summary.title,
    lastPrompt: summary.lastPrompt,
    workDir: facts.workDir,
    sessionDir: facts.sessionDir,
    createdAt: summary.createdAt,
    updatedAt: summary.updatedAt,
    archived: summary.archived,
    metadata: summary.custom as JsonObject | undefined,
    additionalDirs: facts.additionalDirs,
  };
}

/**
 * v1's `SessionMeta` types `title` / `isCustomTitle` / `custom` as required;
 * a v2 document that never set them reports the same neutral defaults v1's
 * session constructor produces (`''` / `false` / not persisted). Timestamps
 * convert epoch ms → ISO.
 */
export function v2MetaToSessionMeta(meta: V2SessionMeta): V1SessionMeta {
  return {
    createdAt: new Date(meta.createdAt).toISOString(),
    updatedAt: new Date(meta.updatedAt).toISOString(),
    title: meta.title ?? '',
    isCustomTitle: meta.isCustomTitle ?? false,
    lastPrompt: meta.lastPrompt,
    forkedFrom: meta.forkedFrom,
    workDir: meta.cwd,
    agents: v2AgentsToV1(meta.agents ?? {}),
    custom: (meta.custom ?? {}) as Record<string, unknown>,
  };
}

/**
 * v1's `AgentMeta` requires `homedir`, `type`, and `parentAgentId`; v2's
 * agent meta leaves fields unset where v1 persisted explicit defaults.
 */
function v2AgentsToV1(agents: Readonly<Record<string, V2AgentMeta>>): Record<string, V1AgentMeta> {
  const mapped: Record<string, V1AgentMeta> = {};
  for (const [agentId, agent] of Object.entries(agents)) {
    mapped[agentId] = {
      homedir: agent.homedir ?? '',
      // v2 registers every agent with a type; fallbacks cover documents
      // written before that registration existed.
      type: agent.type ?? (agentId === 'main' ? 'main' : 'sub'),
      // v1 persists an explicit null for a parentless agent where v2 leaves
      // the field unset.
      parentAgentId: agent.parentAgentId ?? null,
      swarmItem: agent.swarmItem,
    };
  }
  return mapped;
}

/**
 * The v2 `SessionMeta` timestamps are epoch-ms numbers; v1 uses ISO strings.
 * Exposed for the v1 meta round-trip when a v1-shaped document is written
 * back through the v2 engine (the reverse direction of {@link v2MetaToSessionMeta}).
 */
export function isoToEpochMs(value: string): number {
  return new Date(value).getTime();
}