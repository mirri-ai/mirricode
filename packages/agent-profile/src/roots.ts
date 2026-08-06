/**
 * Agent profile directory resolution — where to look for agent files.
 *
 * Mirrors the directory conventions of other config domains (skills, themes,
 * mcp): bootstrap resolves `homeDir`, this module only knows about the
 * `agents/` subdirectory under it. Project roots walk upward to find `.git`.
 */

import { dirname, join, resolve } from 'pathe';

import type { AgentFileRoot, AgentFileSource } from './schema';
import type { ProfileFs } from './fs';

const USER_BRAND_DIRS = ['agents'] as const;
const USER_GENERIC_DIRS = ['.agents/agents'] as const;
const PROJECT_BRAND_DIRS = ['.mirri-code/agents'] as const;
const PROJECT_GENERIC_DIRS = ['.agents/agents'] as const;

export interface AgentRootWarn {
  (message: string, error?: unknown): void;
}

export async function userAgentRoots(
  fs: ProfileFs,
  homeDir: string,
  osHomeDir: string,
  warn?: AgentRootWarn,
): Promise<readonly AgentFileRoot[]> {
  const roots: AgentFileRoot[] = [];
  await pushFirstExisting(fs, roots, USER_BRAND_DIRS, homeDir, 'user', warn);
  await pushFirstExisting(fs, roots, USER_GENERIC_DIRS, osHomeDir, 'user', warn);
  return roots;
}

export async function projectAgentRoots(
  fs: ProfileFs,
  workDir: string,
  warn?: AgentRootWarn,
): Promise<readonly AgentFileRoot[]> {
  const projectRoot = await findProjectRoot(fs, workDir);
  const roots: AgentFileRoot[] = [];
  await pushFirstExisting(fs, roots, PROJECT_BRAND_DIRS, projectRoot, 'project', warn);
  await pushFirstExisting(fs, roots, PROJECT_GENERIC_DIRS, projectRoot, 'project', warn);
  return roots;
}

export async function configuredAgentRoots(
  fs: ProfileFs,
  dirs: readonly string[],
  workDir: string,
  osHomeDir: string,
  source: AgentFileSource,
  warn?: AgentRootWarn,
): Promise<readonly AgentFileRoot[]> {
  const projectRoot = await findProjectRoot(fs, workDir);
  const roots: AgentFileRoot[] = [];
  for (const dir of dirs) {
    const resolved = resolveAgentPath(dir, projectRoot, osHomeDir);
    await pushExistingRoot(fs, roots, resolved, source, warn);
  }
  return roots;
}

async function findProjectRoot(
  fs: ProfileFs,
  workDir: string,
): Promise<string> {
  let current = resolve(workDir);
  while (true) {
    try {
      const stat = await fs.stat(join(current, '.git'));
      if (stat.isDirectory || stat.isFile) return current;
    } catch {
      // .git not found — continue upward
    }
    const parent = dirname(current);
    if (parent === current) return resolve(workDir);
    current = parent;
  }
}

async function pushFirstExisting(
  fs: ProfileFs,
  out: AgentFileRoot[],
  dirs: readonly string[],
  base: string,
  source: AgentFileSource,
  warn?: AgentRootWarn,
): Promise<void> {
  for (const dir of dirs) {
    if (await pushExistingRoot(fs, out, join(base, dir), source, warn)) return;
  }
}

async function pushExistingRoot(
  fs: ProfileFs,
  out: AgentFileRoot[],
  dir: string,
  source: AgentFileSource,
  warn?: AgentRootWarn,
): Promise<boolean> {
  try {
    const stat = await fs.stat(dir);
    if (!stat.isDirectory) return false;
    const resolved = (await fs.realpath(dir)).replaceAll('\\', '/');
    if (!out.some((root) => root.path === resolved)) {
      out.push({ path: resolved, source });
    }
    return true;
  } catch (error) {
    warn?.(`Skipping unreadable agent root ${dir}`, error);
    return false;
  }
}

function resolveAgentPath(dir: string, projectRoot: string, osHomeDir: string): string {
  if (dir.startsWith('~/')) return join(osHomeDir, dir.slice(2));
  if (dir.startsWith('/')) return dir;
  return join(projectRoot, dir);
}
