// apps/mirri-web/src/components/chat/tool-calls/toolRegistry.ts
import type { Component } from 'vue';
import type { ToolCall } from '../../../types';
import { normalizeToolName } from '../../../lib/toolMeta';
import AskUserTool from './AskUserTool.vue';
import DelegationCard from './DelegationCard.vue';
import EditTool from './EditTool.vue';
import GenericTool from './GenericTool.vue';
import MediaTool from './MediaTool.vue';

type ToolRenderer = Component;

/** Pick the renderer for a tool call. */
export function resolveToolRenderer(tool: ToolCall): ToolRenderer {
  if (tool.media && tool.status === 'ok') return MediaTool;
  const name = normalizeToolName(tool.name);
  if (name === 'edit' || name === 'write' || name === 'multi_edit') return EditTool;
  // NOTE: normalizeToolName() folds `agent`/`subagent` into the canonical
  // `task` kind (see lib/toolMeta.ts NAME_ALIASES). Both the single `Agent`
  // dispatch and `AgentSwarm` render through the SAME DelegationCard —
  // cardinality is data, not a different paradigm (supervision SDR-001).
  if (name === 'task' || name === 'agentswarm') return DelegationCard;
  if (name === 'askuserquestion') return AskUserTool;
  return GenericTool;
}
