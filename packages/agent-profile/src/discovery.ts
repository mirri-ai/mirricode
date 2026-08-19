/**
 * Agent profile file discovery — recursive directory walk.
 *
 * Walks each root directory (max depth 8), parses agent files, and resolves
 * same-name conflicts. `.md` wins over `.yaml`/`.yml` in the same directory
 * (alphabetical sort puts `.md` first). Invalid files are collected into
 * `skipped` so one bad file does not zero the whole pass.
 */

import { join } from 'pathe';

import { AgentFileParseError } from './errors';
import type { ProfileFs } from './fs';
import { parseAgentFileText } from './parse';
import {
  AGENT_FILE_EXTENSIONS,
  type AgentFileDef,
  type AgentFileDiscoveryResult,
  type AgentFileRoot,
  type SkippedAgentFile,
} from './schema';

const MAX_AGENT_SCAN_DEPTH = 8;
const MAX_SKIP_WARNINGS = 5;

export interface DiscoverAgentFilesWarn {
  (message: string, error?: unknown): void;
}

export interface DiscoverAgentFilesOptions {
  readonly fs: ProfileFs;
  readonly roots: readonly AgentFileRoot[];
  readonly warn?: DiscoverAgentFilesWarn;
}

export async function discoverAgentFiles(
  options: DiscoverAgentFilesOptions,
): Promise<AgentFileDiscoveryResult> {
  const byName = new Map<string, AgentFileDef>();
  const skipped: SkippedAgentFile[] = [];

  // Hook presence selects the error mode: a fs that provides classifications
  // (v2 hostFs) gets strict semantics — unavailable propagates, missing is
  // "nothing here", unexpected errors fail the scan; a bare fs (v1) keeps the
  // absorbed behavior: unreadable entries become warnings only.
  const strict = options.fs.isUnavailable !== undefined || options.fs.isNotFound !== undefined;

  let emittedWarnings = 0;
  let suppressedWarnings = 0;
  const suppressedSubjects: string[] = [];
  const warnCapped = (subject: string, message: string, error?: unknown): void => {
    if (emittedWarnings < MAX_SKIP_WARNINGS) {
      emittedWarnings += 1;
      options.warn?.(message, error);
    } else {
      suppressedWarnings += 1;
      if (suppressedSubjects.length < 3) suppressedSubjects.push(subject);
    }
  };

  async function parseAndRegister(filePath: string, root: AgentFileRoot): Promise<void> {
    try {
      const text = await options.fs.readText(filePath);
      const agent = parseAgentFileText({ path: filePath, text, source: root.source });
      const existing = byName.get(agent.name);
      if (existing === undefined) {
        byName.set(agent.name, agent);
      } else if (isSameDirDuplicate(existing.filePath ?? '', filePath)) {
        warnCapped(
          filePath,
          `Duplicate agent file for "${agent.name}": both ${existing.filePath} and ${filePath} exist in the same directory; using ${existing.filePath}`,
        );
      }
    } catch (error) {
      if (options.fs.isUnavailable?.(error)) throw error;
      if (error instanceof AgentFileParseError) {
        skipped.push({ path: filePath, reason: error.message });
        warnCapped(filePath, `Skipping invalid agent file at ${filePath}: ${error.message}`, error);
        return;
      }
      // parse errors only make `skipped`; unexpected failures are warnings
      // under strict mode (classification hooks present), and everything is
      // collected on a bare v1 fs so one broken file cannot hide.
      if (!strict) {
        const reason = error instanceof Error ? error.message : String(error);
        skipped.push({ path: filePath, reason });
        warnCapped(filePath, `Skipping invalid agent file at ${filePath}: ${reason}`, error);
        return;
      }
      warnCapped(filePath, `Skipping agent file at ${filePath} due to unexpected error`, error);
    }
  }

  async function walk(dirPath: string, root: AgentFileRoot, depth: number): Promise<void> {
    if (depth > MAX_AGENT_SCAN_DEPTH) return;

    let names: readonly string[];
    try {
      names = (await options.fs.readdir(dirPath)).map((e) => e.name).toSorted();
    } catch (error) {
      if (strict) {
        if (options.fs.isNotFound?.(error)) return;           // missing dir = nothing here
        if (options.fs.isUnavailable?.(error)) throw error;  // fs outage propagates
        if (depth > 0) {
          warnCapped(dirPath, `Skipping unreadable directory ${dirPath}: ${errorMessage(error)}`, error);
          return;
        }
        throw error;
      }
      // v1 semantics: absorb
      return;
    }

    for (const name of names) {
      if (name.startsWith('.') || name === 'node_modules') continue;
      const entryPath = join(dirPath, name);
      try {
        if (await isDirectoryPath(options.fs, entryPath, strict)) {
          await walk(entryPath, root, depth + 1);
          continue;
        }
        if (!isAgentFileName(name) || !(await isFilePath(options.fs, entryPath, strict))) continue;
        await parseAndRegister(entryPath, root);
      } catch (error) {
        if (strict && options.fs.isUnavailable?.(error)) throw error;
        warnCapped(entryPath, `Skipping unreadable agent path ${entryPath}: ${errorMessage(error)}`, error);
      }
    }
  }

  for (const root of options.roots) {
    try {
      await walk(root.path, root, 0);
    } catch (error) {
      if (strict && options.fs.isUnavailable?.(error)) throw error;
      warnCapped(root.path, `Skipping unreadable agent root ${root.path}: ${errorMessage(error)}`, error);
    }
  }

  if (suppressedWarnings > 0) {
    const examples = suppressedSubjects.map((s) => `"${s}"`).join(', ');
    options.warn?.(
      `Suppressed ${suppressedWarnings} further agent-discovery skip warnings (e.g. ${examples}); fix or remove the offending files/directories to silence them`,
    );
  }

  return {
    agents: [...byName.values()].toSorted((a, b) => a.name.localeCompare(b.name)),
    skipped,
    scannedRoots: options.roots.map((root) => root.path),
  };
}

export function isAgentFileName(name: string): boolean {
  return AGENT_FILE_EXTENSIONS.some((ext) => name.endsWith(ext));
}

function isSameDirDuplicate(pathA: string, pathB: string): boolean {
  const dirA = pathA.slice(0, Math.max(pathA.lastIndexOf('/'), pathA.lastIndexOf('\\')));
  const dirB = pathB.slice(0, Math.max(pathB.lastIndexOf('/'), pathB.lastIndexOf('\\')));
  return dirA === dirB && pathA !== pathB;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function isDirectoryPath(fs: ProfileFs, path: string, strict: boolean): Promise<boolean> {
  try {
    const resolved = await fs.realpath(path);
    return (await fs.stat(resolved)).isDirectory;
  } catch (error) {
    if (strict) {
      if (fs.isNotFound?.(error)) return false;
      throw error;
    }
    return false;
  }
}

async function isFilePath(fs: ProfileFs, path: string, strict: boolean): Promise<boolean> {
  try {
    const resolved = await fs.realpath(path);
    return (await fs.stat(resolved)).isFile;
  } catch (error) {
    if (strict) {
      if (fs.isNotFound?.(error)) return false;
      throw error;
    }
    return false;
  }
}
