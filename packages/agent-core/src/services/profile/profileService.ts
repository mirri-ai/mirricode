import { mkdir, readFile, rm } from 'node:fs/promises';
import { join, dirname } from 'pathe';

import { dump as dumpYaml, load as loadYaml } from 'js-yaml';

import { Disposable, InstantiationType, registerSingleton } from '../../di';
import { ICoreProcessService } from '../coreProcess/coreProcess';
import { IEventService } from '../event/event';
import { atomicWrite } from '../../utils/fs';
import { ProfileRegistry, type ProfileRegistryEntry } from '../../profile/registry';
import { RawAgentProfileSchema } from '../../profile/types';
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

    const raw = buildRawProfile(data);
    const yamlContent = dumpYaml(raw);

    const filePath = join(this.homeDir, 'agents', `${data.name}.yaml`);
    await mkdir(dirname(filePath), { recursive: true });
    await atomicWrite(filePath, yamlContent);

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
      // Built-in profiles are overridden via a YAML file in ~/.mirricode/agents/
      const filePath = join(this.homeDir, 'agents', `${name}.yaml`);

      // If an override already exists, merge with it; otherwise build from scratch
      let existingRaw: Record<string, unknown> = { name };
      try {
        const existingContent = await readFile(filePath, 'utf-8');
        existingRaw = loadYaml(existingContent) as Record<string, unknown>;
      } catch {
        // No existing override — start fresh
      }
      const updatedRaw = mergeRawProfile(existingRaw, data);
      const yamlContent = dumpYaml(updatedRaw);

      await mkdir(dirname(filePath), { recursive: true });
      await atomicWrite(filePath, yamlContent);
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
    const existingRaw = RawAgentProfileSchema.parse(loadYaml(existingContent));
    const updatedRaw = mergeRawProfile(existingRaw, data);
    const yamlContent = dumpYaml(updatedRaw);

    await atomicWrite(filePath, yamlContent);
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

    const filePath = join(this.homeDir, 'agents', `${name}.yaml`);
    try {
      await rm(filePath);
    } catch (error) {
      if ((error as { code?: string }).code !== 'ENOENT') throw error;
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

function buildRawProfile(data: CreateProfileInput): Record<string, unknown> {
  const raw: Record<string, unknown> = { name: data.name };
  if (data.extends !== undefined) raw['extends'] = data.extends;
  if (data.description !== undefined) raw['description'] = data.description;
  if (data.defaultModel !== undefined) raw['defaultModel'] = data.defaultModel;
  if (data.tools !== undefined) raw['tools'] = [...data.tools];
  if (data.systemPromptTemplate !== undefined) raw['systemPromptTemplate'] = data.systemPromptTemplate;
  if (data.whenToUse !== undefined) raw['whenToUse'] = data.whenToUse;
  if (data.promptVars !== undefined) raw['promptVars'] = data.promptVars;
  if (data.capabilities !== undefined) raw['capabilities'] = [...data.capabilities];
  if (data.capabilitiesRequired !== undefined) raw['capabilitiesRequired'] = [...data.capabilitiesRequired];
  return raw;
}

function mergeRawProfile(
  existing: Record<string, unknown>,
  data: UpdateProfileInput,
): Record<string, unknown> {
  const merged = { ...existing };
  if (data.extends !== undefined) merged['extends'] = data.extends;
  if (data.description !== undefined) merged['description'] = data.description;
  if (data.defaultModel !== undefined) merged['defaultModel'] = data.defaultModel;
  if (data.tools !== undefined) merged['tools'] = [...data.tools];
  if (data.systemPromptTemplate !== undefined) merged['systemPromptTemplate'] = data.systemPromptTemplate;
  if (data.whenToUse !== undefined) merged['whenToUse'] = data.whenToUse;
  if (data.promptVars !== undefined) merged['promptVars'] = data.promptVars;
  if (data.capabilities !== undefined) merged['capabilities'] = [...data.capabilities];
  if (data.capabilitiesRequired !== undefined) merged['capabilitiesRequired'] = [...data.capabilitiesRequired];
  return merged;
}
