import { promises as fs } from 'node:fs';
import { join } from 'pathe';

import {
  discoverAgentFiles,
  type AgentFileDef,
  type AgentFileRoot as PkgAgentFileRoot,
  type ProfileFs,
} from '@mirri-ai/agent-profile';

import type { RawAgentProfile } from './types';

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

/**
 * Node.js fs.promises adapter for {@link ProfileFs}.
 */
const profileFs: ProfileFs = {
  readText: (p) => fs.readFile(p, 'utf-8'),
  readdir: async (p) => {
    const entries = await fs.readdir(p, { withFileTypes: true });
    return entries.map((e) => ({ name: e.name, isFile: e.isFile(), isDirectory: e.isDirectory() }));
  },
  realpath: (p) => fs.realpath(p),
  stat: async (p) => {
    const s = await fs.stat(p);
    return { isFile: s.isFile(), isDirectory: s.isDirectory() };
  },
};

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
 * Warn callback type for `discoverAgentProfiles`. Called when a duplicate
 * same-name agent file is found in the same directory.
 */
export type DiscoverAgentProfilesWarn = (message: string) => void;

export async function discoverAgentProfiles(
  roots: readonly AgentProfileRoot[],
  warn?: DiscoverAgentProfilesWarn,
): Promise<readonly DiscoveredProfile[]> {
  // Adapt v1 AgentProfileRoot[] to package AgentFileRoot[]
  const pkgRoots: PkgAgentFileRoot[] = roots.map((r) => ({
    path: r.path,
    source: r.source,
  }));

  const result = await discoverAgentFiles({
    fs: profileFs,
    roots: pkgRoots,
    warn: warn ? (msg: string) => warn(msg) : undefined,
  });

  return result.agents.map((def) => ({
    name: def.name,
    source: (def.source ?? 'extra') as AgentProfileSource,
    path: def.filePath ?? '',
    raw: agentFileDefToRawAgentProfile(def),
  }));
}

/** Default prompt value that the package assigns when none is declared in a YAML file. */
const PACKAGE_DEFAULT_PROMPT = 'You are a helpful assistant.';

/**
 * Map a package {@link AgentFileDef} to a v1 {@link RawAgentProfile}.
 *
 * Only fields with meaningful (non-default, non-empty) values are included so
 * that the registry's partial-merge spread (`{ ...builtin, ...override }`)
 * preserves built-in fields the override didn't explicitly set.
 */
function agentFileDefToRawAgentProfile(def: AgentFileDef): RawAgentProfile {
  const raw: RawAgentProfile = { name: def.name };
  if (def.description !== undefined && def.description !== '') raw.description = def.description;
  // Only surface systemPromptTemplate when the user explicitly set it — the
  // package defaults `prompt` to PACKAGE_DEFAULT_PROMPT for YAML files that
  // don't declare one; including that default would overwrite the built-in's
  // prompt during the registry partial-merge.
  if (def.prompt !== '' && def.prompt !== PACKAGE_DEFAULT_PROMPT) {
    raw.systemPromptTemplate = def.prompt;
  }
  if (def.whenToUse !== undefined) raw.whenToUse = def.whenToUse;
  if (def.defaultModel !== undefined) raw.defaultModel = def.defaultModel;
  if (def.tools !== undefined && def.tools.length > 0) raw.tools = [...def.tools];
  if (def.capabilitiesRequired !== undefined && def.capabilitiesRequired.length > 0) {
    raw.capabilitiesRequired = [...def.capabilitiesRequired];
  }
  if (def.subagents !== undefined && def.subagents.length > 0) {
    raw.subagents = Object.fromEntries(def.subagents.map((s) => [s, {}]));
  }
  return raw;
}

async function isDir(p: string): Promise<boolean> {
  try {
    const stat = await fs.stat(p);
    return stat.isDirectory();
  } catch {
    return false;
  }
}
