/**
 * Parameter alias converter.
 *
 * Maps common parameter name aliases to their canonical names defined in the
 * tool's JSON Schema. This addresses the well-known LLM "parameter name
 * hallucination" problem where the model uses a more common parameter name
 * (e.g. "offset") instead of the exact name in the schema (e.g. "line_offset").
 *
 * The converter is schema-driven: it only remaps keys that are NOT present in
 * the schema's properties but have a known alias that IS present. This ensures
 * we never overwrite a valid parameter.
 *
 * When both the alias and its canonical name appear in args, the canonical
 * name always wins — the model explicitly used the schema-defined parameter
 * name, so its value is authoritative. The alias is silently dropped.
 *
 * Aliases are scoped to specific tool names (with `'*'` wildcard support) to
 * prevent semantic mismatches. The alias data itself lives in
 * `parameter-alias-config.ts` so this converter stays stateless and testable.
 *
 * Must run before NumericStringConverter so that renamed keys also get type
 * conversion (e.g. `{ "offset": "42" }` → `{ "line_offset": "42" }` →
 * `{ "line_offset": 42 }`).
 */

import type { ToolArgConverter, ToolArgConverterContext } from '../types';
import { TOOL_ALIAS_CONFIG } from './parameter-alias-config';

/**
 * Build the tool-scoped lookup map from the config data.
 *
 * Tool-specific entries take precedence over the `'*'` wildcard. The result
 * is computed once at module load and shared across all converter instances.
 */
const TOOL_ALIAS_MAP: ReadonlyMap<string, ReadonlyMap<string, string>> = (() => {
  const map = new Map<string, Map<string, string>>();
  for (const entry of TOOL_ALIAS_CONFIG) {
    const inner = new Map<string, string>();
    for (const alias of entry.aliases) {
      inner.set(alias.alias, alias.canonical);
    }
    map.set(entry.tool, inner);
  }
  return map;
})();

const GLOBAL_ALIAS_MAP: ReadonlyMap<string, string> = TOOL_ALIAS_MAP.get('*') ?? new Map();

/**
 * Look up the alias→canonical map for a given tool.
 *
 * Falls back to the `'*'` wildcard entry when no tool-specific aliases exist.
 * Returns an empty map when neither is registered.
 */
function aliasMapForTool(toolName: string): ReadonlyMap<string, string> {
  return TOOL_ALIAS_MAP.get(toolName) ?? GLOBAL_ALIAS_MAP;
}

/**
 * Extract the set of property names defined in a JSON Schema's `properties`.
 */
function schemaPropertyNames(schema: unknown): Set<string> {
  if (typeof schema !== 'object' || schema === null) return new Set();
  const properties = (schema as Record<string, unknown>)['properties'];
  if (typeof properties !== 'object' || properties === null) return new Set();
  return new Set(Object.keys(properties));
}

/**
 * Converter that renames aliased parameter keys to their canonical schema names.
 */
export class ParameterAliasConverter implements ToolArgConverter {
  readonly name = 'parameter-alias';

  canConvert(ctx: ToolArgConverterContext): boolean {
    if (typeof ctx.args !== 'object' || ctx.args === null) return false;

    const aliasMap = aliasMapForTool(ctx.toolName);
    if (aliasMap.size === 0) return false;

    const propNames = schemaPropertyNames(ctx.toolParameters);
    if (propNames.size === 0) return false;

    const args = ctx.args as Record<string, unknown>;

    for (const key of Object.keys(args)) {
      if (propNames.has(key)) continue;
      const canonical = aliasMap.get(key);
      if (canonical !== undefined && propNames.has(canonical)) {
        return true;
      }
    }

    return false;
  }

  convert(ctx: ToolArgConverterContext): unknown {
    if (typeof ctx.args !== 'object' || ctx.args === null) return ctx.args;

    const aliasMap = aliasMapForTool(ctx.toolName);
    if (aliasMap.size === 0) return ctx.args;

    const propNames = schemaPropertyNames(ctx.toolParameters);
    const sourceKeys = Object.keys(ctx.args as Record<string, unknown>);

    // Pre-scan: collect the set of canonical names that already appear as
    // literal keys in the args. When the canonical name is present, the
    // alias is dropped — the canonical value is authoritative regardless of
    // whether the values match (the model explicitly used the schema-defined
    // parameter name, so its intent is clear).
    const presentCanonicalNames = new Set<string>();
    for (const key of sourceKeys) {
      if (propNames.has(key)) presentCanonicalNames.add(key);
    }

    const result: Record<string, unknown> = {};
    for (const key of sourceKeys) {
      const value = (ctx.args as Record<string, unknown>)[key]!;
      if (!propNames.has(key)) {
        const canonical = aliasMap.get(key);
        if (canonical !== undefined && propNames.has(canonical)) {
          if (!presentCanonicalNames.has(canonical)) {
            // Canonical name not in args — remap the alias to it.
            result[canonical] = value;
          }
          // Canonical name already in args — drop the alias. The canonical
          // value is authoritative; the alias is a hallucination residual.
          continue;
        }
      }
      result[key] = value;
    }

    return result;
  }
}
