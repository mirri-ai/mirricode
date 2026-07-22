import type { MirriConfig } from '../config';
import type { Logger } from '../logging';
import { DEFAULT_PROFILE_FILE_PATHS, DEFAULT_PROFILE_SOURCES } from './default';
import { finalizeRawAgentProfileSource } from './load';
import { resolveAgentProfiles } from './resolve';
import {
  discoverAgentProfiles,
  resolveAgentProfileRoots,
  type AgentProfileSource,
  type DiscoveredProfile,
} from './scanner';
import type { RawAgentProfile, ResolvedAgentProfile } from './types';

export interface ProfileRegistryEntry {
  profile: ResolvedAgentProfile;
  source: AgentProfileSource | 'builtin';
  filePath?: string;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  /** True when a built-in profile has a user/project override file. */
  hasOverride: boolean;
  /** Raw promptVars from the YAML (pre-resolution), exposed for UI editing. */
  readonly promptVars?: Record<string, string>;
  /** Raw systemPromptTemplate from the YAML (pre-resolution), exposed for UI editing. */
  readonly systemPromptTemplate?: string;
}

const ESSENTIAL_PROFILES = new Set(['agent']);

export class ProfileRegistry {
  private entries: Map<string, ProfileRegistryEntry> = new Map();
  private mergedCache: Record<string, ResolvedAgentProfile> = {};

  constructor(
    private readonly configProvider: () => MirriConfig,
    private readonly envPaths: { brandHomeDir: string; workDir: string; userHomeDir: string },
    private readonly log?: Logger,
  ) {}

  async reload(): Promise<void> {
    const config = this.configProvider();
    const extraDirs = config.extraAgentDirs;
    const disabledAgents = new Set(config.disabledAgents ?? []);

    // 1. Discover custom profiles from filesystem
    const roots = await resolveAgentProfileRoots({
      paths: this.envPaths,
      extraDirs,
    });
    const discovered = await discoverAgentProfiles(roots);
    const customByName = new Map<string, DiscoveredProfile>();
    for (const d of discovered) {
      customByName.set(d.name, d);
    }

    // 2. Build the full set of raw profiles:
    //    - Built-in profiles from their embedded YAML sources (preserves systemPromptPath, promptVars, subagents, extends)
    //    - Custom profiles from disk (already have systemPromptTemplate resolved by scanner)
    //    - Custom profile with same name as built-in → replaces the built-in raw
    const allRaw: RawAgentProfile[] = [];

    for (const profilePath of DEFAULT_PROFILE_FILE_PATHS) {
      const raw = finalizeRawAgentProfileSource(
        DEFAULT_PROFILE_SOURCES[profilePath]!,
        profilePath,
        DEFAULT_PROFILE_SOURCES,
      );
      // Check if a custom profile overrides this built-in
      const customOverride = customByName.get(raw.name);
      if (customOverride !== undefined && !ESSENTIAL_PROFILES.has(raw.name)) {
        // Partial-merge: custom YAML only overrides fields it declares.
        // Built-in fields not present in the custom YAML are preserved.
        // Essential profiles (agent) are protected from override.
        allRaw.push({ ...raw, ...customOverride.raw });
      } else {
        allRaw.push(raw);
      }
    }

    // Add custom profiles that are not built-in overrides
    const builtinNames = new Set(DEFAULT_PROFILE_FILE_PATHS.map((p) => {
      const raw = finalizeRawAgentProfileSource(
        DEFAULT_PROFILE_SOURCES[p]!,
        p,
        DEFAULT_PROFILE_SOURCES,
      );
      return raw.name;
    }));
    for (const [name, d] of customByName) {
      if (!builtinNames.has(name)) {
        allRaw.push(d.raw);
      }
    }

    // 3. Resolve all together (handles extends, cycle detection, subagent linking)
    const resolved = resolveAgentProfiles(allRaw);

    // 4. Build entries with source/essential/enabled metadata
    //    Also keep raw promptVars/systemPromptTemplate for UI editing.
    const rawByName = new Map<string, RawAgentProfile>();
    for (const raw of allRaw) {
      if (raw.name) rawByName.set(raw.name, raw);
    }

    this.entries.clear();
    for (const [name, profile] of Object.entries(resolved)) {
      const isBuiltin = builtinNames.has(name);
      const custom = customByName.get(name);
      const essential = ESSENTIAL_PROFILES.has(name);
      const disabled = disabledAgents.has(name) && !essential;
      const raw = rawByName.get(name);

      this.entries.set(name, {
        profile,
        source: custom?.source ?? 'builtin',
        filePath: custom?.path,
        builtin: isBuiltin,
        essential,
        enabled: !disabled,
        hasOverride: isBuiltin && custom !== undefined,
        promptVars: raw?.promptVars,
        systemPromptTemplate: raw?.systemPromptTemplate,
      });
    }

    // 5. Build merged cache (only enabled profiles)
    this.mergedCache = {};
    for (const [name, entry] of this.entries) {
      if (entry.enabled) {
        this.mergedCache[name] = entry.profile;
      }
    }
  }

  getMergedProfiles(): Record<string, ResolvedAgentProfile> {
    return this.mergedCache;
  }

  listEntries(): ProfileRegistryEntry[] {
    return [...this.entries.values()];
  }

  getProfile(name: string): ResolvedAgentProfile | undefined {
    const entry = this.entries.get(name);
    return entry?.enabled ? entry.profile : undefined;
  }

  getEntry(name: string): ProfileRegistryEntry | undefined {
    return this.entries.get(name);
  }

  getMergedSubagents(parentProfileName?: string): Record<string, ResolvedAgentProfile> {
    const parent = parentProfileName
      ? this.mergedCache[parentProfileName]
      : this.mergedCache['agent'];
    return parent?.subagents ?? {};
  }

  /**
   * Returns all subagents available for dispatch right now: declared
   * subagents from the parent profile (filtered by enabled) PLUS all
   * other enabled non-essential profiles (custom agents). The `agent`
   * profile itself is never included.
   */
  getAvailableSubagents(parentProfileName?: string): Record<string, ResolvedAgentProfile> {
    const parent = parentProfileName
      ? this.mergedCache[parentProfileName]
      : this.mergedCache['agent'];
    const declared = parent?.subagents ?? {};

    // Start with declared subagents (filtered by enabled)
    const result: Record<string, ResolvedAgentProfile> = {};
    for (const [name, profile] of Object.entries(declared)) {
      const entry = this.entries.get(name);
      if (entry === undefined || entry.enabled) {
        result[name] = profile;
      }
    }

    // Add all other enabled non-essential profiles (custom agents)
    for (const [name, entry] of this.entries) {
      if (entry.essential) continue;
      if (result[name] !== undefined) continue;
      if (!entry.enabled) continue;
      result[name] = entry.profile;
    }

    return result;
  }
}
