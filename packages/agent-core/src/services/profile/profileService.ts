import { mkdir, readFile, rm } from 'node:fs/promises';
import { join, dirname } from 'pathe';

import {
  parseAgentFileText,
  serializeAgentFile,
  type AgentFileDef,
} from '@mirri-ai/agent-profile';

import { Disposable, InstantiationType, registerSingleton } from '../../di';
import { ICoreProcessService } from '../coreProcess/coreProcess';
import { IEventService } from '../event/event';
import { atomicWrite } from '../../utils/fs';
import { ProfileRegistry, type ProfileRegistryEntry } from '../../profile/registry';
import {
  IProfileService,
  type CreateProfileInput,
  type ProfileEntry,
  type UpdateProfileInput,
} from './profile';

/**
 * Fields whose change in config.toml requires a profile registry reload.
 */
const PROFILE_RELEVANT_CONFIG_FIELDS = new Set(['disabled_agents', 'extra_agent_dirs']);

const ESSENTIAL_PROFILES = new Set(['agent']);

const DEFAULT_AGENT_FILE_EXTENSION = '.md';

export class ProfileService extends Disposable implements IProfileService {
  readonly _serviceBrand: undefined;

  constructor(
    @ICoreProcessService private readonly core: ICoreProcessService,
    @IEventService private readonly eventService: IEventService,
  ) {
    super();
    // Subscribe to config changes — reload the registry when profile-relevant
    // fields change (disabled_agents, extra_agent_dirs).
    this._register(
      this.eventService.onDidPublish((event) => {
        if (
          event.type === 'event.config.changed' &&
          event.changedFields.some((f) => PROFILE_RELEVANT_CONFIG_FIELDS.has(f))
        ) {
          void this.core.profileRegistry?.reload().then(() => {
            this.publishProfilesChanged();
          });
        }
      }),
    );
  }

  private get registry(): ProfileRegistry {
    const reg = this.core.profileRegistry;
    if (reg === undefined) {
      throw new Error('ProfileRegistry is not available on the core process');
    }
    return reg;
  }

  private get homeDir(): string {
    const dir = this.core.homeDir;
    if (dir === undefined) {
      throw new Error('homeDir is not available on the core process');
    }
    return dir;
  }

  async listProfiles(): Promise<readonly ProfileEntry[]> {
    const entries = this.registry.listEntries();
    return entries.map(toProfileEntry);
  }

  async getProfile(name: string): Promise<ProfileEntry | undefined> {
    const entry = this.registry.getEntry(name);
    return entry ? toProfileEntry(entry) : undefined;
  }

  async createProfile(data: CreateProfileInput): Promise<ProfileEntry> {
    const existing = this.registry.getEntry(data.name);
    if (existing !== undefined) {
      throw new Error(`Agent profile "${data.name}" already exists`);
    }

    if (!/^[a-z0-9-]+$/.test(data.name)) {
      throw new Error(`Agent profile name must be kebab-case (lowercase letters, digits, hyphens)`);
    }

    const def = buildAgentFileDef(data);
    const filePath = join(this.homeDir, 'agents', `${data.name}${DEFAULT_AGENT_FILE_EXTENSION}`);
    const content = serializeAgentFile(def);
    await mkdir(dirname(filePath), { recursive: true });
    await atomicWrite(filePath, content);
    await removeStaleFormatFiles(dirname(filePath), data.name, DEFAULT_AGENT_FILE_EXTENSION);

    await this.registry.reload();
    this.publishProfilesChanged();

    const entry = this.registry.getEntry(data.name);
    if (entry === undefined) {
      throw new Error(`Failed to load created profile "${data.name}"`);
    }
    return toProfileEntry(entry);
  }

  async updateProfile(name: string, data: UpdateProfileInput): Promise<ProfileEntry> {
    const entry = this.registry.getEntry(name);
    if (entry === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }
    if (entry.essential) {
      throw new Error(`Agent profile "${name}" is essential and cannot be modified`);
    }

    if (entry.builtin) {
      // Built-in profiles are overridden via a file in ~/.mirricode/agents/
      // Start from the built-in's own fields so they are preserved when the
      // override only specifies a subset (partial-merge).
      const builtInDef: AgentFileDef = {
        name,
        description: entry.profile.description ?? name,
        whenToUse: entry.profile.whenToUse,
        defaultModel: entry.profile.defaultModel,
        tools: entry.profile.tools?.length ? [...entry.profile.tools] : undefined,
        capabilitiesRequired: entry.profile.capabilitiesRequired?.length
          ? [...entry.profile.capabilitiesRequired]
          : undefined,
        prompt: entry.systemPromptTemplate || DEFAULT_PROMPT,
      };

      const overridePath = await findOverrideFile(this.homeDir, name);

      let existingDef: AgentFileDef;
      if (overridePath !== undefined) {
        try {
          const existingContent = await readFile(overridePath, 'utf-8');
          existingDef = parseAgentFileText({ path: overridePath, text: existingContent });
        } catch {
          existingDef = builtInDef;
        }
      } else {
        existingDef = builtInDef;
      }

      const updatedDef = mergeAgentFileDef(existingDef, data);
      const content = serializeAgentFile(updatedDef);
      const newFilePath = join(this.homeDir, 'agents', `${name}.md`);
      await mkdir(dirname(newFilePath), { recursive: true });
      await atomicWrite(newFilePath, content);
      await removeStaleFormatFiles(dirname(newFilePath), name, '.md');
      await this.registry.reload();
      this.publishProfilesChanged();

      const updated = this.registry.getEntry(name);
      if (updated === undefined) {
        throw new Error(`Failed to reload built-in profile "${name}"`);
      }
      return toProfileEntry(updated);
    }

    // Custom profile — update its file directly
    const filePath = entry.filePath;
    if (filePath === undefined) {
      throw new Error(`No file path for profile "${name}"`);
    }

    const existingContent = await readFile(filePath, 'utf-8');
    let existingDef: AgentFileDef;
    try {
      existingDef = parseAgentFileText({ path: filePath, text: existingContent });
    } catch {
      existingDef = {
        name,
        description: entry.profile.description ?? name,
        prompt: entry.systemPromptTemplate || DEFAULT_PROMPT,
      };
    }

    const updatedDef = mergeAgentFileDef(existingDef, data);
    const content = serializeAgentFile(updatedDef);

    // Always save as .md going forward; remove any stale format siblings
    const newFilePath = join(dirname(filePath), `${name}.md`);
    await mkdir(dirname(newFilePath), { recursive: true });
    await atomicWrite(newFilePath, content);
    // The entry.filePath may already point to .md after a prior save; always
    // nuke any .yaml/.yml leftovers so the scanner never sees stale data.
    await removeStaleFormatFiles(dirname(newFilePath), name, '.md');
    await this.registry.reload();
    this.publishProfilesChanged();

    const updated = this.registry.getEntry(name);
    if (updated === undefined) {
      throw new Error(`Failed to reload updated profile "${name}"`);
    }
    return toProfileEntry(updated);
  }

  async deleteProfile(name: string): Promise<void> {
    const entry = this.registry.getEntry(name);
    if (entry === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }
    if (entry.builtin) {
      throw new Error(`Built-in agent profile "${name}" is read-only`);
    }

    const filePath = entry.filePath;
    if (filePath === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }

    try {
      await rm(filePath);
    } catch (error) {
      if ((error as { code?: string }).code !== 'ENOENT') throw error;
    }
    await this.registry.reload();
    this.publishProfilesChanged();
  }

  async enableProfile(name: string): Promise<void> {
    const entry = this.registry.getEntry(name);
    if (entry === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }
    if (entry.enabled) return;

    await this.updateDisabledAgents((current) => current.filter((n) => n !== name));
  }

  async disableProfile(name: string): Promise<void> {
    const entry = this.registry.getEntry(name);
    if (entry === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }
    if (ESSENTIAL_PROFILES.has(name)) {
      throw new Error(`Agent profile "${name}" is essential and cannot be disabled`);
    }
    if (!entry.enabled) return;

    await this.updateDisabledAgents((current) =>
      current.includes(name) ? current : [...current, name],
    );
  }

  async resetBuiltinOverride(name: string): Promise<void> {
    const entry = this.registry.getEntry(name);
    if (entry === undefined) {
      throw new Error(`Agent profile "${name}" was not found`);
    }
    if (!entry.builtin) {
      throw new Error(`Agent profile "${name}" is not a built-in profile`);
    }

    const overridePath = await findOverrideFile(this.homeDir, name);
    if (overridePath !== undefined) {
      try {
        await rm(overridePath);
      } catch (error) {
        if ((error as { code?: string }).code !== 'ENOENT') throw error;
      }
    }

    await this.registry.reload();
    this.publishProfilesChanged();
  }

  /**
   * Update `disabled_agents` in config.toml via the core RPC, then reload
   * the registry so the change takes effect immediately.
   */
  private async updateDisabledAgents(
    compute: (current: readonly string[]) => readonly string[],
  ): Promise<void> {
    const config = await this.core.rpc.getMirriConfig({ reload: true });
    const currentDisabled = config.disabledAgents ?? [];
    const nextDisabled = compute(currentDisabled);

    // Only write if the list actually changed
    if (JSON.stringify([...currentDisabled].toSorted()) === JSON.stringify([...nextDisabled].toSorted())) {
      return;
    }

    await this.core.rpc.setMirriConfig({ disabledAgents: [...nextDisabled] });
    await this.core.profileRegistry?.reload();
    this.publishProfilesChanged();
  }

  private publishProfilesChanged(): void {
    this.eventService.publish({
      type: 'event.agent.profiles_changed',
      agentId: 'main',
      sessionId: '__global__',
    });
  }
}

registerSingleton(IProfileService, ProfileService, InstantiationType.Delayed);

function toProfileEntry(entry: ProfileRegistryEntry): ProfileEntry {
  return {
    name: entry.profile.name,
    description: entry.profile.description,
    source: entry.source,
    builtin: entry.builtin,
    essential: entry.essential,
    enabled: entry.enabled,
    hasOverride: entry.hasOverride,
    filePath: entry.filePath,
    extends: undefined,
    defaultModel: entry.profile.defaultModel,
    tools: entry.profile.tools,
    whenToUse: entry.profile.whenToUse,
    systemPromptTemplate: entry.systemPromptTemplate,
    promptVars: entry.promptVars,
  };
}

/** Default prompt used when none is provided — must match the package's default. */
const DEFAULT_PROMPT = 'You are a helpful assistant.';

/**
 * Build an {@link AgentFileDef} from a {@link CreateProfileInput}.
 */
function buildAgentFileDef(data: CreateProfileInput): AgentFileDef {
  return {
    name: data.name,
    description: data.description || data.name,
    whenToUse: data.whenToUse,
    defaultModel: data.defaultModel,
    tools: data.tools ? [...data.tools] : undefined,
    subagents: undefined,
    capabilitiesRequired: data.capabilitiesRequired ? [...data.capabilitiesRequired] : undefined,
    prompt: data.systemPromptTemplate || DEFAULT_PROMPT,
  };
}

/**
 * Merge update data into an existing {@link AgentFileDef}.
 * Uses `||` for string fields so that empty strings in `data` are treated
 * as "not provided" and the existing value is preserved.
 */
function mergeAgentFileDef(existing: AgentFileDef, data: UpdateProfileInput): AgentFileDef {
  return {
    name: existing.name,
    description: data.description || existing.description,
    whenToUse: data.whenToUse || existing.whenToUse,
    defaultModel: data.defaultModel ?? existing.defaultModel,
    tools: data.tools !== undefined ? [...data.tools] : existing.tools,
    subagents: existing.subagents,
    capabilitiesRequired: data.capabilitiesRequired !== undefined
      ? [...data.capabilitiesRequired]
      : existing.capabilitiesRequired,
    prompt: data.systemPromptTemplate || existing.prompt,
  };
}

/**
 * Find an existing override file for a built-in profile. Checks for both
 * `.md` and `.yaml`/`.yml` extensions. Returns the path of the first match,
 * or undefined if no override exists.
 */
async function findOverrideFile(homeDir: string, name: string): Promise<string | undefined> {
  const agentsDir = join(homeDir, 'agents');
  const candidates = [`${name}.md`, `${name}.yaml`, `${name}.yml`];
  for (const candidate of candidates) {
    const filePath = join(agentsDir, candidate);
    try {
      await readFile(filePath, 'utf-8');
      return filePath;
    } catch {
      // File doesn't exist — try next
    }
  }
  return undefined;
}

/**
 * All agent-file extensions recognized by the scanner. Used by
 * {@link removeStaleFormatFiles} to know which sibling files to clean up.
 */
const AGENT_FILE_EXTENSIONS_ALL = ['.md', '.yaml', '.yml'] as const;

/**
 * Remove same-name agent files with different extensions from the same
 * directory. After a save, only the file that was just written should
 * exist — any stale `.yaml`/`.yml`/`.md` leftovers from a previous format
 * are deleted so the scanner never reads outdated data.
 *
 * ENOENT is silently ignored (the stale file simply doesn't exist).
 */
async function removeStaleFormatFiles(
  dir: string,
  name: string,
  keepExt: string,
): Promise<void> {
  for (const ext of AGENT_FILE_EXTENSIONS_ALL) {
    if (ext === keepExt) continue;
    const filePath = join(dir, `${name}${ext}`);
    try {
      await rm(filePath);
    } catch (error) {
      if ((error as { code?: string }).code !== 'ENOENT') throw error;
    }
  }
}
