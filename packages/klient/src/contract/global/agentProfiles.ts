/**
 * `agentProfileManagementService` — the profile data plane over the agent
 * profile catalog (R5-2): the merged wire-row view (`list`), the
 * enable/disable toggle write channel (`setEnabled`), and the post-write
 * loader sync (`reload`). Mirrors
 * `agent-core-v2/app/agentProfileCatalog/agentProfileManagement.ts`; rows
 * reuse the shared protocol REST contract (`profileEntrySchema`,
 * snake_case) so every consumer (web REST, the terminal `/agents` view, the
 * embedded rust engine) reads identical rows.
 */

import { z } from 'zod';

import { profileEntrySchema } from '@mirri-ai/protocol';
import type { AgentProfileEntriesView } from '@mirri-ai/agent-core-v2/app/agentProfileCatalog/agentProfileManagement';

import { noResult } from '../helpers.js';
import type { ServiceContract } from '../types.js';

export const skippedAgentProfileFileSchema = z.object({
  path: z.string(),
  reason: z.string(),
});

export const agentProfileEntriesViewSchema = z.object({
  entries: z.array(profileEntrySchema),
  skipped: z.array(skippedAgentProfileFileSchema),
});

export const agentProfileManagementContract = {
  list: {
    input: z.tuple([z.string().optional()]),
    output: agentProfileEntriesViewSchema,
  },
  setEnabled: {
    input: z.tuple([z.string(), z.boolean(), z.string().optional()]),
    output: noResult,
  },
  reload: { input: z.tuple([]), output: noResult },
} satisfies ServiceContract;

// Compile-time parity: the contract's `list` output must validate everything
// the engine's `AgentProfileEntriesView` can produce (the engine row is the
// snake_case wire spelling; the protocol-only optional fields are absent).
const _engineViewParity: z.ZodType<AgentProfileEntriesView> = agentProfileEntriesViewSchema;
