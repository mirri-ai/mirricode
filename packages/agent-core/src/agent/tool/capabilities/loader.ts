import { readFileSync } from 'node:fs';
import { join } from 'pathe';
import { parseIntegrationsYaml } from './parse';
import type { IntegrationSpec, IntegrationsConfig } from './types';

const INTEGRATIONS_FILENAME = 'integrations.yaml';

export interface IntegrationsLoadResult {
  readonly config: IntegrationsConfig;
  /** Absolute paths of the files that were read (in load order). */
  readonly sources: readonly string[];
  /** Non-fatal parse / schema warnings, one per source. */
  readonly warnings: readonly string[];
}

/**
 * Load and merge `integrations.yaml` from layered scopes:
 *
 *   1. `<userHome>/integrations.yaml`         (user-global)
 *   2. `<cwd>/.mirri-code/integrations.yaml`  (project-local, wins on key collision)
 *
 * Both files are optional. A missing file is skipped silently; a parse
 * failure is captured as a warning and the layer is treated as empty.
 * Later layers override the whole `integrations[serverName]` entry from
 * an earlier layer — this keeps semantics predictable for users who want
 * to redefine capabilities for a specific project.
 */
export function loadIntegrationsConfig(input: {
  readonly userHome?: string | undefined;
  readonly cwd?: string | undefined;
}): IntegrationsLoadResult {
  const paths: string[] = [];
  if (input.userHome !== undefined) {
    paths.push(join(input.userHome, INTEGRATIONS_FILENAME));
  }
  if (input.cwd !== undefined) {
    paths.push(join(input.cwd, '.mirri-code', INTEGRATIONS_FILENAME));
  }

  const merged: Record<string, IntegrationSpec> = {};
  const sources: string[] = [];
  const warnings: string[] = [];

  for (const path of paths) {
    let content: string;
    try {
      content = readFileSync(path, 'utf8');
    } catch (error) {
      const err = error as NodeJS.ErrnoException;
      if (err.code === 'ENOENT') continue;
      warnings.push(`Failed to read ${path}: ${err.message}`);
      continue;
    }
    const parsed = parseIntegrationsYaml(content);
    for (const w of parsed.warnings) warnings.push(`${path}: ${w}`);
    sources.push(path);
    for (const [name, spec] of Object.entries(parsed.config.integrations)) {
      merged[name] = spec;
    }
  }

  return {
    config: { integrations: merged },
    sources,
    warnings,
  };
}
