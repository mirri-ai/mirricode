import { mkdir, readFile, rm } from 'node:fs/promises';
import { join, dirname, extname } from 'pathe';

import { dump as dumpYaml, load as loadYaml } from 'js-yaml';

import { Disposable, InstantiationType, registerSingleton } from '../../di';
import { ICoreProcessService } from '../coreProcess/coreProcess';
import { IEventService } from '../event/event';
import { atomicWrite } from '../../utils/fs';
import { parseFrontmatter } from '../../skill/parser';
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

const DEFAULT_AGENT_FILE_EXTENSION = '.yaml';

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
    const filePath = join(this.homeDir, 'agents', `${data.name}${DEFAULT_AGENT_FILE_EXTENSION}`);
    const content = serializeAgentFile(raw, filePath);
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
      // Detect existing override format; default to .md for new overrides.
      const overridePath = await findOverrideFile(this.homeDir, name);
      const filePath = overridePath ?? join(this.homeDir, 'agents', `${name}${DEFAULT_AGENT_FILE_EXTENSION}`);

      // If an override already exists, merge with it; otherwise build from scratch
      let existingRaw: Record<string, unknown> = { name };
      if (overridePath !== undefined) {
        try {
          const existingContent = await readFile(filePath, 'utf-8');
          existingRaw = parseAgentFileContent(existingContent, filePath);
        } catch {
          // No existing override — start fresh
        }
      }
      const updatedRaw = mergeRawProfile(existingRaw, data);
      const content = serializeAgentFile(updatedRaw, filePath);

      await mkdir(dirname(filePath), { recursive: true });
      await atomicWrite(filePath, content);
      await removeStaleFormatFiles(dirname(filePath), name, extname(filePath));
      await this.registry.reload();
      this.publishProfilesChanged();

      const updated = this.registry.getEntry(name);
      if (updated === undefined) {
        throw new Error(`Failed to reload built-in profile "${name}"`);
      }
      return toProfileEntry(updated);
    }

    // Custom profile — update its file directly, preserving the original format
    const filePath = entry.filePath;
    if (filePath === undefined) {
      throw new Error(`No file path for profile "${name}"`);
    }

    const existingContent = await readFile(filePath, 'utf-8');
    const existingRaw = parseAgentFileContent(existingContent, filePath);
    const updatedRaw = mergeRawProfile(existingRaw, data);
    const content = serializeAgentFile(updatedRaw, filePath);

    await atomicWrite(filePath, content);
    await removeStaleFormatFiles(dirname(filePath), name, extname(filePath));
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

/**
 * Serialize a raw agent profile to file content. The format is determined by
 * the file extension: `.md` produces frontmatter + body (Markdown); `.yaml`/
 * `.yml` produces pure YAML. When writing `.md`, the `systemPromptTemplate`
 * field is extracted into the body and omitted from the frontmatter.
 */
function serializeAgentFile(
  raw: Record<string, unknown>,
  filePath: string,
): string {
  if (filePath.endsWith('.md')) {
    return serializeMarkdownAgentFile(raw);
  }
  return dumpYaml(raw);
}

/**
 * Serialize a raw agent profile as a Markdown document with YAML frontmatter.
 * The `systemPromptTemplate` field is extracted into the body (the Markdown
 * content after the closing `---` fence) rather than being embedded in the
 * frontmatter YAML, keeping it readable for users editing the `.md` file.
 */
function serializeMarkdownAgentFile(raw: Record<string, unknown>): string {
  const { systemPromptTemplate, ...frontmatterData } = raw;
  const body = typeof systemPromptTemplate === 'string' ? systemPromptTemplate : '';
  const yamlText = dumpYaml(frontmatterData, { lineWidth: -1 }).trimEnd();
  if (body.length === 0) {
    return `---\n${yamlText}\n---\n`;
  }
  return `---\n${yamlText}\n---\n${body}`;
}

/**
 * Parse agent file content into a raw object. For `.md` files, the frontmatter
 * is parsed as YAML and the body is used as `systemPromptTemplate`. For `.yaml`/
 * `.yml` files, the entire content is parsed as YAML.
 */
function parseAgentFileContent(
  content: string,
  filePath: string,
): Record<string, unknown> {
  if (filePath.endsWith('.md')) {
    const parsed = parseFrontmatter(content);
    if (parsed.data === null || typeof parsed.data !== 'object' || Array.isArray(parsed.data)) {
      throw new Error(`Invalid frontmatter in ${filePath}`);
    }
    const data = { ...(parsed.data as Record<string, unknown>) };
    const body = parsed.body.trim();
    if (body.length > 0 && data['systemPromptTemplate'] === undefined) {
      data['systemPromptTemplate'] = body;
    }
    return data;
  }
  return loadYaml(content) as Record<string, unknown>;
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
