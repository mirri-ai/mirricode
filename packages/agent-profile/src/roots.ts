/**
 * Agent profile directory resolution — where to look for agent files.
 *
 * Mirrors the directory conventions of other config domains (skills, themes,
 * mcp): bootstrap resolves `homeDir`, this module only knows about the
 * `agents/` subdirectory under it. Project roots walk upward to find `.git`.
 */

import { dirname, isAbsolute, join, resolve } from 'pathe';

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

export interface ProjectAgentRootCandidates {
  readonly projectRoot: string;
  readonly candidates: readonly string[];
}

/**
 * The project root plus every candidate agent directory under it, without
 * probing existence — callers (e.g. watchers or UI surfaces) use this to
 * monitor candidate paths that do not exist yet.
 */
export async function projectAgentRootCandidates(
  fs: ProfileFs,
  workDir: string,
): Promise<ProjectAgentRootCandidates> {
  const projectRoot = await findProjectRoot(fs, workDir);
  return {
    projectRoot,
    candidates: [...PROJECT_BRAND_DIRS, ...PROJECT_GENERIC_DIRS].map((dir) =>
      join(projectRoot, dir),
    ),
  };
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
    } catch (error) {
      if (fs.isUnavailable?.(error)) throw error;
      // .git not found or unreadable — continue upward
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
    // `realpath` first: it canonicalizes the path and surfaces fs-outage
    // errors (propagated via the isUnavailable hook) before any stat.
    const resolved = (await fs.realpath(dir)).replaceAll('\\', '/');
    const stat = await fs.stat(resolved);
    if (!stat.isDirectory) return false;
    if (!out.some((root) => root.path === resolved)) {
      out.push({ path: resolved, source });
    }
    return true;
  } catch (error) {
    if (fs.isUnavailable?.(error)) throw error;
    if (fs.isNotFound?.(error)) return false;
    warn?.(`Skipping unreadable agent root ${dir}`, error);
    return false;
  }
}

function resolveAgentPath(dir: string, projectRoot: string, osHomeDir: string): string {
  if (dir === '~') return osHomeDir;
  if (dir.startsWith('~/')) return join(osHomeDir, dir.slice(2));
  if (isAbsolute(dir)) return dir;
  return join(projectRoot, dir);
}
