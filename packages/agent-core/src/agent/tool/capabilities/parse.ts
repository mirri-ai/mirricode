import { load as loadYaml } from 'js-yaml';
import { IntegrationsConfigSchema, type IntegrationsConfig } from './types';

export interface ParseResult {
  readonly config: IntegrationsConfig;
  readonly warnings: readonly string[];
}

/**
 * Parse a `.mirri-code/integrations.yaml` document string.
 *
 * Missing content is treated as an empty config (no integrations). Malformed
 * YAML or schema violations return an empty config with a warning; the caller
 * is expected to surface warnings but keep going. Never throws for user-authored
 * content — only invariants like a non-string input trigger throws.
 */
export function parseIntegrationsYaml(content: string | undefined): ParseResult {
  if (content === undefined || content.trim().length === 0) {
    return { config: { integrations: {} }, warnings: [] };
  }
  let parsed: unknown;
  try {
    parsed = loadYaml(content);
  } catch (error) {
    return {
      config: { integrations: {} },
      warnings: [
        `Failed to parse integrations.yaml: ${error instanceof Error ? error.message : String(error)}`,
      ],
    };
  }
  if (parsed === null || parsed === undefined) {
    return { config: { integrations: {} }, warnings: [] };
  }
  const result = IntegrationsConfigSchema.safeParse(parsed);
  if (!result.success) {
    const issues = result.error.issues
      .map((i) => {
        const path = i.path.length > 0 ? i.path.join('.') : '(root)';
        return `${path}: ${i.message}`;
      })
      .join('; ');
    return {
      config: { integrations: {} },
      warnings: [`integrations.yaml schema violation: ${issues}`],
    };
  }
  return { config: result.data, warnings: [] };
}
