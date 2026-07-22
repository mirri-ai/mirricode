import { promises as fs } from 'node:fs';
import { dirname, join } from 'pathe';

import { load as loadYaml } from 'js-yaml';

import { RawAgentProfileSchema, type RawAgentProfile } from './types';

export type AgentProfileSource = 'user' | 'project' | 'extra';

export interface AgentProfileRoot {
  path: string;
  source: AgentProfileSource;
}

export interface DiscoveredProfile {
  name: string;
  source: AgentProfileSource;
  path: string;
  raw: RawAgentProfile;
}

export interface AgentProfilePathContext {
  readonly userHomeDir: string;
  readonly brandHomeDir: string;
  readonly workDir: string;
}

export async function resolveAgentProfileRoots(opts: {
  paths: AgentProfilePathContext;
  extraDirs?: readonly string[];
}): Promise<readonly AgentProfileRoot[]> {
  const roots: AgentProfileRoot[] = [];
  const { brandHomeDir, workDir } = opts.paths;

  // Project: <workDir>/.mirri-code/agents
  const projectAgentsDir = join(workDir, '.mirri-code', 'agents');
  if (await isDir(projectAgentsDir)) {
    roots.push({ path: projectAgentsDir, source: 'project' });
  }

  // User brand: <brandHomeDir>/agents
  const userAgentsDir = join(brandHomeDir, 'agents');
  if (await isDir(userAgentsDir)) {
    roots.push({ path: userAgentsDir, source: 'user' });
  }

  // Extra dirs
  for (const dir of opts.extraDirs ?? []) {
    if (await isDir(dir)) {
      roots.push({ path: dir, source: 'extra' });
    }
  }

  return roots;
}

export async function discoverAgentProfiles(
  roots: readonly AgentProfileRoot[],
): Promise<readonly DiscoveredProfile[]> {
  const byName = new Map<string, DiscoveredProfile>();

  // Roots are ordered by priority: project > user > extra
  for (const root of roots) {
    const entries = await readYamlFiles(root.path);
    for (const { filePath, content } of entries) {
      const raw = await parseAgentProfileFile(content, filePath);
      if (raw === undefined) continue;
      if (!byName.has(raw.name)) {
        byName.set(raw.name, {
          name: raw.name,
          source: root.source,
          path: filePath,
          raw,
        });
      }
    }
  }

  return [...byName.values()];
}

async function isDir(p: string): Promise<boolean> {
  try {
    const stat = await fs.stat(p);
    return stat.isDirectory();
  } catch {
    return false;
  }
}

async function readYamlFiles(
  dir: string,
): Promise<readonly { filePath: string; content: string }[]> {
  let entries: string[];
  try {
    entries = await fs.readdir(dir);
  } catch {
    return [];
  }

  const yamlFiles = entries.filter((f) => f.endsWith('.yaml') || f.endsWith('.yml'));
  const results: { filePath: string; content: string }[] = [];

  for (const file of yamlFiles) {
    const filePath = join(dir, file);
    try {
      const content = await fs.readFile(filePath, 'utf-8');
      results.push({ filePath, content });
    } catch {
      // Skip unreadable files
    }
  }

  return results;
}

/**
 * Parse a YAML profile file and resolve its systemPromptPath relative
 * to the file's directory. Returns undefined if parsing or validation fails.
 */
async function parseAgentProfileFile(
  content: string,
  filePath: string,
): Promise<RawAgentProfile | undefined> {
  let parsed: unknown;
  try {
    parsed = loadYaml(content);
  } catch {
    return undefined;
  }

  const result = RawAgentProfileSchema.safeParse(parsed);
  if (!result.success) {
    return undefined;
  }

  const raw = result.data;

  // Resolve systemPromptPath relative to the profile file's directory
  if (raw.systemPromptPath !== undefined) {
    try {
      const templatePath = join(dirname(filePath), raw.systemPromptPath);
      raw.systemPromptTemplate = await fs.readFile(templatePath, 'utf-8');
    } catch {
      // Template file not found — leave systemPromptTemplate unset
    }
  }

  return raw;
}
