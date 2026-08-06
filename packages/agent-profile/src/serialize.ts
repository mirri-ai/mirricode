/**
 * Agent profile file serialization — always outputs `.md` format.
 *
 * Builds YAML frontmatter from the structured fields and places `prompt` as
 * the Markdown body (after the closing `---` fence). This is the inverse of
 * `parseAgentFileText` for `.md` files: a round-trip (parse → serialize →
 * parse) preserves all fields.
 */

import { serializeFrontmatter } from './frontmatter';
import type { AgentFileDef } from './schema';

export function serializeAgentFile(def: AgentFileDef): string {
  const data: Record<string, unknown> = { name: def.name };
  if (def.description !== undefined) data['description'] = def.description;
  if (def.whenToUse !== undefined) data['whenToUse'] = def.whenToUse;
  if (def.defaultModel !== undefined) data['defaultModel'] = def.defaultModel;
  if (def.tools !== undefined) data['tools'] = [...def.tools];
  if (def.subagents !== undefined) data['subagents'] = [...def.subagents];
  if (def.capabilitiesRequired !== undefined) {
    data['capabilitiesRequired'] = [...def.capabilitiesRequired];
  }
  return serializeFrontmatter(data, def.prompt);
}
