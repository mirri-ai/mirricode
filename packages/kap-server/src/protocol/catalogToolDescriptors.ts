import type { ToolDescriptor } from './tool';

/**
 * Supplemental tool descriptors for the /tools/catalog endpoint.
 *
 * These tools are NOT in the agent-core-v2 contribution table because their
 * activation depends on runtime state (e.g. model capabilities). They are
 * still valid tool identifiers that users can reference in profile configs.
 *
 * If a tool migrates into the contribution table, remove it from here.
 */
export const SUPPLEMENTAL_TOOL_DESCRIPTORS: readonly ToolDescriptor[] = Object.freeze([
  {
    name: 'ReadMediaFile',
    description: 'Read an image or media file from the local filesystem.',
    input_schema: null,
    source: 'builtin',
  },
]);
