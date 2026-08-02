import { promises as fs } from 'node:fs';
import { dirname, join } from 'pathe';

import { load as loadYaml } from 'js-yaml';

import { parseFrontmatter } from '../skill/parser';
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

/**
 * Accepted agent-file extensions. `.md` (frontmatter + body) is the preferred
 * format; `.yml`/`.yaml` are supported for backward compatibility. The order
 * here defines the priority used by `agentFileExtensionPriority` when both
 * `.md` and `.yaml`/`.yml` exist for the same agent name in the same
 * directory.
 */
const AGENT_FILE_EXTENSIONS = ['.md', '.yaml', '.yml'] as const;

/**
 * Priority of each extension when resolving same-name conflicts in the same
 * directory. Lower number = higher priority. `.md` wins over `.yaml`, which
 * wins over `.yml`.
 */
const EXTENSION_PRIORITY: Record<string, number> = {
  '.md': 0,
  '.yaml': 1,
  '.yml': 2,
};

/**
 * Warn callback type for `discoverAgentProfiles`. Called when a duplicate
 * same-name agent file is found in the same directory.
 */
export type DiscoverAgentProfilesWarn = (message: string) => void;

export async function discoverAgentProfiles(
  roots: readonly AgentProfileRoot[],
  warn?: DiscoverAgentProfilesWarn,
): Promise<readonly DiscoveredProfile[]> {
  const byName = new Map<string, DiscoveredProfile>();

  // Roots are ordered by priority: project > user > extra
  for (const root of roots) {
    const entries = await readAgentFiles(root.path);
    for (const { filePath, content } of entries) {
      const raw = await parseAgentProfileFile(content, filePath);
      if (raw === undefined) continue;
      const existing = byName.get(raw.name);
      if (existing === undefined) {
        byName.set(raw.name, {
          name: raw.name,
          source: root.source,
          path: filePath,
          raw,
        });
      } else if (isSameDirectory(existing.path, filePath)) {
        // Same agent name, same directory, different file extension.
        // The file with the higher-priority extension (lower number) was
        // registered first (because readAgentFiles sorts by priority). Warn
        // the user so they can clean up the redundant file.
        warn?.(
          `Duplicate agent file for "${raw.name}": both ${existing.path} and ${filePath} exist in the same directory; using ${existing.path}`,
        );
      }
      // If the paths are in different directories, the first-wins rule
      // applies silently (existing stays, new is dropped).
    }
  }

  return [...byName.values()];
}

/**
 * Check whether two file paths reside in the same directory.
 */
function isSameDirectory(pathA: string, pathB: string): boolean {
  const dirA = pathA.slice(0, Math.max(pathA.lastIndexOf('/'), pathA.lastIndexOf('\\')));
  const dirB = pathB.slice(0, Math.max(pathB.lastIndexOf('/'), pathB.lastIndexOf('\\')));
  return dirA === dirB && pathA !== pathB;
}

async function isDir(p: string): Promise<boolean> {
  try {
    const stat = await fs.stat(p);
    return stat.isDirectory();
  } catch {
    return false;
  }
}

/**
 * Read all agent files (`.md`, `.yaml`, `.yml`) from a directory. Results are
 * sorted by extension priority (`.md` first, then `.yaml`, then `.yml`) so
 * that when two files share the same base name, the higher-priority format
 * is registered first and wins the name slot in `discoverAgentProfiles`.
 */
async function readAgentFiles(
  dir: string,
): Promise<readonly { filePath: string; content: string }[]> {
  let entries: string[];
  try {
    entries = await fs.readdir(dir);
  } catch {
    return [];
  }

  const agentFiles = entries
    .filter((f) => AGENT_FILE_EXTENSIONS.some((ext) => f.endsWith(ext)))
    .toSorted((a, b) => {
      const extA = AGENT_FILE_EXTENSIONS.find((ext) => a.endsWith(ext)) ?? '';
      const extB = AGENT_FILE_EXTENSIONS.find((ext) => b.endsWith(ext)) ?? '';
      const priorityA = EXTENSION_PRIORITY[extA] ?? 99;
      const priorityB = EXTENSION_PRIORITY[extB] ?? 99;
      if (priorityA !== priorityB) return priorityA - priorityB;
      return a.localeCompare(b);
    });
  const results: { filePath: string; content: string }[] = [];

  for (const file of agentFiles) {
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
 * Parse an agent profile file. For `.md` files, the content is split into
 * YAML frontmatter + Markdown body using {@link parseFrontmatter}; the body
 * becomes the `systemPromptTemplate`. For `.yaml`/`.yml` files, the entire
 * content is parsed as YAML (v1-compatible). Returns undefined if parsing or
 * validation fails.
 */
async function parseAgentProfileFile(
  content: string,
  filePath: string,
): Promise<RawAgentProfile | undefined> {
  let parsed: unknown;

  if (filePath.endsWith('.md')) {
    let frontmatter: unknown;
    try {
      const result = parseFrontmatter(content);
      frontmatter = result.data;
      const body = result.body.trim();
      if (frontmatter === null) return undefined;
      // Merge the body as systemPromptTemplate into the frontmatter data
      if (typeof frontmatter === 'object' && frontmatter !== null && !Array.isArray(frontmatter)) {
        const data = frontmatter as Record<string, unknown>;
        if (body.length > 0 && data['systemPromptTemplate'] === undefined) {
          data['systemPromptTemplate'] = body;
        }
        parsed = data;
      } else {
        return undefined;
      }
    } catch {
      return undefined;
    }
  } else {
    try {
      parsed = loadYaml(content);
    } catch {
      return undefined;
    }
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
