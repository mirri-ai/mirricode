/**
 * Tool-scoped parameter alias definitions.
 *
 * Each entry maps a hallucinated parameter name (the key the LLM actually
 * sent) to the canonical parameter name defined in the tool's JSON Schema.
 *
 * Structure:
 * - Outer key: tool name, or `'*'` for a wildcard that applies to all tools.
 * - Inner map: alias → canonical name.
 *
 * A tool-specific entry takes precedence over the `'*'` wildcard.
 *
 * Safety is enforced by the converter at runtime: an alias is only remapped
 * when (a) the alias key is NOT already a valid schema property, and (b) the
 * canonical name IS a valid schema property. So adding an entry here can never
 * corrupt a tool call that already uses correct parameter names.
 *
 * Add entries here as new LLM parameter-name hallucination patterns are
 * discovered — scope to the specific tool when the alias is tool-specific,
 * or use `'*'` when the alias is universally safe.
 */
export interface AliasEntry {
  readonly alias: string;
  readonly canonical: string;
}

export interface ToolAliasConfig {
  readonly tool: string;
  readonly aliases: readonly AliasEntry[];
}

export const TOOL_ALIAS_CONFIG: readonly ToolAliasConfig[] = [
  {
    tool: 'Read',
    aliases: [
      { alias: 'offset', canonical: 'line_offset' },
      { alias: 'limit', canonical: 'n_lines' },
    ],
  },
];
