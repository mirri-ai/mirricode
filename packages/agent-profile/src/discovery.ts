/**
 * Agent profile file discovery — recursive directory walk.
 *
 * Walks each root directory (max depth 8), parses agent files, and resolves
 * same-name conflicts. `.md` wins over `.yaml`/`.yml` in the same directory
 * (alphabetical sort puts `.md` first). Invalid files are collected into
 * `skipped` so one bad file does not zero the whole pass.
 */

import { join } from 'pathe';

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
      const reason = error instanceof Error ? error.message : String(error);
      skipped.push({ path: filePath, reason });
      warnCapped(filePath, `Skipping invalid agent file at ${filePath}: ${reason}`, error);
    }
  }

  async function walk(dirPath: string, root: AgentFileRoot, depth: number): Promise<void> {
    if (depth > MAX_AGENT_SCAN_DEPTH) return;

    let entries: readonly { name: string; isFile: boolean; isDirectory: boolean }[];
    try {
      entries = (await options.fs.readdir(dirPath))
        .map((e) => ({ name: e.name, isFile: e.isFile, isDirectory: e.isDirectory }))
        .toSorted((a, b) => a.name.localeCompare(b.name));
    } catch {
      if (depth > 0) return;
      return;
    }

    for (const entry of entries) {
      if (entry.name.startsWith('.') || entry.name === 'node_modules') continue;
      const entryPath = join(dirPath, entry.name);
      try {
        if (entry.isDirectory) {
          await walk(entryPath, root, depth + 1);
          continue;
        }
        if (!entry.isFile) continue;
        if (!isAgentFileName(entry.name)) continue;
        await parseAndRegister(entryPath, root);
      } catch (error) {
        warnCapped(entryPath, `Skipping unreadable agent path ${entryPath}: ${errorMessage(error)}`, error);
      }
    }
  }

  for (const root of options.roots) {
    try {
      await walk(root.path, root, 0);
    } catch (error) {
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
