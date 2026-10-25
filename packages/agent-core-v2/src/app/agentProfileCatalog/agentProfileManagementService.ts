/**
 * `agentProfileCatalog` domain — `IAgentProfileManagementService`
 * implementation (R5-2).
 *
 * Ports the kap-server `/agents` route layer's profile read projection and
 * enable/disable write channel into the engine, so the REST routes, the SDK
 * (`SDKRpcClientV2` over the klient facade), and the terminal `/agents` view
 * consume one source:
 *
 * - `list` is the ported `buildProfileEntries`: the catalog projection over
 *   the App-scope registry (workspace-tagged entries filtered to the
 *   resolved handler, suppressed candidates and discovery skips included),
 *   with the zero-workspace fallback scanning builtin profiles and the user
 *   agents directory directly.
 * - `setEnabled` is the ported enable/disable branch of the REST action
 *   endpoint: the same serializeAgentMd full rewrite into the user agents
 *   root, the same legacy `.yaml`/`.yml` cleanup, the same loader reload.
 * - `reload` is the ported `reloadUserProfileLoader`, kept public for the
 *   REST CRUD routes' own file writes.
 *
 * Bound at App scope, constructed on first resolution.
 */

import { dump } from 'js-yaml';
import { join } from 'node:path';

import { Disposable } from '#/_base/di/lifecycle';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import type { IWorkspaceScopeHandle } from '#/_base/di/scope';
import { ILogService } from '#/_base/log/log';
import { Error2 } from '#/_base/errors/errors';
import {
  DEFAULT_AGENT_PROFILE_NAME,
  type AgentProfile,
} from '#/app/agentProfileCatalog/agentProfileCatalog';
import { IAgentProfileRegistry } from '#/app/agentProfileCatalog/agentProfileRegistry';
import {
  BUILTIN_AGENT_PROFILE_SOURCE_ID,
  IBuiltinAgentProfileLoader,
} from '#/app/agentProfileCatalog/builtinAgentProfileLoader';
import { renderPromptTemplate, SYSTEM_PROMPT_TEMPLATE } from '#/app/agentProfileCatalog/profile-shared';
import {
  AgentProfileManagementErrors,
} from '#/app/agentProfileCatalog/agentProfileManagementErrors';
import {
  IAgentProfileManagementService,
  type AgentProfileEntriesView,
  type AgentProfileEntry,
  type AgentProfileEntrySource,
  type AgentProfileEntrySuppressedCandidate,
  type SkippedAgentProfileFile,
} from '#/app/agentProfileCatalog/agentProfileManagement';
import { IBootstrapService, resolveMirriHome } from '#/app/bootstrap/bootstrap';
import { IWorkspaceService } from '#/app/workspace/workspace';
import { IWorkspaceLifecycleService } from '#/app/workspaceLifecycle/workspaceLifecycle';
import { IHostFileSystem, type HostDirEntry } from '#/os/interface/hostFileSystem';
import { HostFsError, OsFsErrors } from '#/os/interface/hostFsErrors';
import { IUserAgentProfileLoader } from '#/workspace/workspaceAgentProfileLoader/userAgentProfileLoader';
import { parseAgentFileText } from '@mirri-ai/agent-profile';
import { SessionAgentProfileCatalogService } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService';
import type { AgentProfileInspection } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog';

// ---------------------------------------------------------------------------
// Pure helpers (exported for the REST route layer's own CRUD writes — the
// same serialization the toggle channel applies)
// ---------------------------------------------------------------------------

/** The user agents root under the (resolved) mirri home. */
export function userAgentsDir(homeDir: string): string {
  return join(resolveMirriHome(homeDir), 'agents');
}

/** The `.md` file path of the named profile under the user agents root. */
export function agentFilePath(homeDir: string, name: string): string {
  return join(userAgentsDir(homeDir), `${name}.md`);
}

/**
 * Read `<brandHome>/SYSTEM.md` as the default-base override (D21): the raw
 * text when the file exists and is non-empty (whitespace-only counts as
 * empty — the `loadSystemMdProfile` trim check), `undefined` otherwise. A
 * MISSING file is the default state and returns silently (the loader's
 * `isFilePath` contract); only a real read failure (or a non-regular file)
 * degrades to `undefined` with a warn — a transient fs error never poisons
 * the listing (the same degradation the loader applies).
 */
export async function readSystemMdOverride(
  fs: IHostFileSystem,
  homeDir: string,
  warn: (message: string) => void,
): Promise<string | undefined> {
  const path = join(resolveMirriHome(homeDir), 'SYSTEM.md');
  let text: string | undefined;
  try {
    const stat = await fs.stat(path);
    if (!stat.isFile) return undefined;
    text = await fs.readText(path);
  } catch (error) {
    // ENOENT (and "a path component is not a directory") is the absent-file
    // default — silent `undefined`, never a warn: every GET/PUT on the
    // agents surface lists profiles, so a missing-override warn would fire
    // on each healthy request.
    if (
      error instanceof HostFsError &&
      (error.code === OsFsErrors.codes.OS_FS_NOT_FOUND ||
        error.code === OsFsErrors.codes.OS_FS_NOT_DIRECTORY)
    ) {
      return undefined;
    }
    warn(`agent SYSTEM.md override read failed: ${String(error)} [${path}]`);
    return undefined;
  }
  return text.trim().length === 0 ? undefined : text;
}

/**
 * Stitch the SYSTEM.md override disclosure onto the essential `agent` row
 * (D21 / US-A8): `base_prompt_override` always (boolean), the raw
 * `base_prompt_template` when the override exists, and the builtin default
 * template's raw text under `base_prompt_default_template` (the settings
 * editor's seed when no override exists — sourced from the same
 * `system.md` asset the renderer uses, so the wire always matches the
 * default the engine actually assembles). Other rows pass through
 * untouched — the disclosure is the default base's concern, exactly like
 * the rust engine's wire rows.
 */
function decorateEssentialRow(
  entries: readonly AgentProfileEntry[],
  systemMd: string | undefined,
): readonly AgentProfileEntry[] {
  return entries.map((entry) =>
    entry.name !== DEFAULT_AGENT_PROFILE_NAME
      ? entry
      : {
          ...entry,
          base_prompt_override: systemMd !== undefined,
          base_prompt_default_template: DEFAULT_BASE_PROMPT_TEMPLATE,
          ...(systemMd !== undefined ? { base_prompt_template: systemMd } : {}),
        },
  );
}

/**
 * The builtin default base prompt template's RAW text (`system.md`
 * verbatim, `${placeholders}` intact) — re-exported from profile-shared so
 * the wire row (the settings editor's no-override seed) and the render
 * (`renderSystemPrompt`) always carry the same template.
 */
export const DEFAULT_BASE_PROMPT_TEMPLATE: string = SYSTEM_PROMPT_TEMPLATE;

const LEGACY_EXTENSIONS = ['.yaml', '.yml'] as const;

/**
 * Remove any legacy `.yaml`/`.yml` files with the same base name as `name`,
 * but only when the new-format `.md` file already exists. The `.md`-exists
 * guard catches callers that accidentally pass a name before writing the
 * primary file — a warn log is emitted for that case.
 */
export async function removeLegacyAgentFiles(
  fs: IHostFileSystem,
  homeDir: string,
  name: string,
  log?: (message: string) => void,
): Promise<void> {
  const dir = userAgentsDir(homeDir);
  const mdPath = join(dir, `${name}.md`);
  try {
    await fs.stat(mdPath);
  } catch {
    log?.(
      `removeLegacyFile: "${name}" has no .md file — skipping legacy cleanup (caller may have an out-of-order write)`,
    );
    return;
  }
  for (const ext of LEGACY_EXTENSIONS) {
    const legacyPath = join(dir, `${name}${ext}`);
    try {
      await fs.remove(legacyPath);
    } catch {
      // File does not exist or cannot be removed — non-fatal
    }
  }
}

/** Serialize an agent definition to the v2 `.md` frontmatter format. */
export function serializeAgentMd(fields: {
  name: string;
  description?: string;
  whenToUse?: string;
  tools?: readonly string[];
  disallowedTools?: readonly string[];
  subagents?: readonly string[];
  defaultModel?: string;
  capabilitiesRequired?: readonly string[];
  enabled?: boolean;
  systemPromptTemplate?: string;
}): string {
  const frontmatter: Record<string, unknown> = {
    name: fields.name,
    description: fields.description ?? '',
  };
  if (fields.whenToUse) frontmatter['whenToUse'] = fields.whenToUse;
  if (fields.tools && fields.tools.length > 0) frontmatter['tools'] = [...fields.tools];
  if (fields.disallowedTools && fields.disallowedTools.length > 0)
    frontmatter['disallowedTools'] = [...fields.disallowedTools];
  if (fields.subagents && fields.subagents.length > 0)
    frontmatter['subagents'] = [...fields.subagents];
  if (fields.defaultModel) frontmatter['defaultModel'] = fields.defaultModel;
  if (fields.capabilitiesRequired && fields.capabilitiesRequired.length > 0)
    frontmatter['capabilitiesRequired'] = [...fields.capabilitiesRequired];
  // D14 (ADR-006): no `override` declaration key is ever emitted — a
  // same-name file overrides by virtue of existing.
  if (fields.enabled === false) frontmatter['enabled'] = false;

  const yaml = dump(frontmatter, { lineWidth: -1, quotingType: '"' }).trim();
  // parseAgentFileText rejects files with an empty prompt body — the
  // fallback body is the bare base-prompt reference (SDR-014 ②③: the
  // placeholder text is retired from every TS write channel; a profile
  // without a role of its own rides the shared base).
  const body = fields.systemPromptTemplate?.trim() || BUILTIN_BASE_PROMPT_REF;
  return `---\n${yaml}\n---\n${body}`;
}

/**
 * The base-prompt reference token a composed profile body opens with.
 * `renderPromptTemplate` binds it lazily at runtime to the effective
 * default profile's prompt, so the base stays in sync with product
 * updates instead of being snapshotted into the file.
 */
const BUILTIN_BASE_PROMPT_REF = '${base_prompt}';

/** Wrap a role text into a composed override body (base reference + text). */
export function builtinOverrideBody(roleText: string): string {
  return `${BUILTIN_BASE_PROMPT_REF}\n\n${roleText}`;
}

/**
 * Wrap a plain role text into a composed override body (the base
 * reference followed by the text). An already-composed text (starting
 * with the bare base reference, per ADR-010 ①) passes through verbatim —
 * the write channels' single composition discriminator for custom rows
 * and builtin overrides alike (SDR-014 ②: they are isomorphic). A
 * template that merely mentions the token mid-text does not start with
 * it and is a full replacement (ADR-010 ①: never wrapped).
 */
export function composeOntoBasePrompt(text: string): string {
  return isComposedBody(text) ? text : builtinOverrideBody(text);
}

/**
 * Whether a body is composed onto the base reference — the ADR-010 ①
 * two-shape discriminator, aligned with the rust `is_composed_body`:
 * a body that starts with the bare `${base_prompt}` reference (followed
 * by anything) is composed by convention; one that merely mentions the
 * token mid-text is a full replacement and must never be double-wrapped.
 */
export function isComposedBody(body: string): boolean {
  return body.startsWith(BUILTIN_BASE_PROMPT_REF);
}

/**
 * Whether a body opens with the base-prompt reference — the write
 * channels' composition discriminator (ADR-010 ①). Kept as the alias of
 * {@link isComposedBody} for the route layer's existing imports.
 */
export function opensWithBasePromptRef(body: string): boolean {
  return isComposedBody(body);
}

/**
 * Strip the leading `${base_prompt}` reference and the separator
 * whitespace after it — the editable role-text projection (ADR-010 ②),
 * aligned with the rust `strip_base_prompt_ref`: strip the bare
 * reference prefix, then trim the leading whitespace/newlines. The
 * `${base_prompt}\n\nRole` and `${base_prompt}\nRole` forms both yield
 * `Role`; the bare reference (or whitespace-only remainder) yields the
 * empty string; a body without the exact prefix passes through verbatim
 * (a hand-written template that merely mentions the token is not
 * stripped).
 */
export function stripBuiltinBasePromptRef(body: string): string {
  return isComposedBody(body) ? body.slice(BUILTIN_BASE_PROMPT_REF.length).trimStart() : body;
}

/**
 * The wire `system_prompt_template` projection for a file body: the
 * stripped role text, with a strip-to-empty result mapped to `undefined`
 * (a bare-reference body carries no editable text — the rust wire row
 * does not emit the key, so the cross-engine snapshots agree).
 */
function fileBodyToWireTemplate(body: string): string | undefined {
  const stripped = stripBuiltinBasePromptRef(body);
  return stripped === '' ? undefined : stripped;
}

// ---------------------------------------------------------------------------
// Read projection internals
// ---------------------------------------------------------------------------

/** Map registry sourceId to wire source enum. */
const SOURCE_BY_SOURCE_ID: Readonly<Record<string, AgentProfileEntrySource>> = {
  builtin: 'builtin',
  plugin: 'plugin',
  user: 'user',
  extra: 'extra',
  workspace: 'project',
  explicit: 'explicit',
};

/** Inferred type for a parsed agent file — avoids importing an unexported symbol. */
type ParsedAgentFile = ReturnType<typeof parseAgentFileText>;

function catalogEntryToWire(
  profile: AgentProfile,
  inspection: AgentProfileInspection | undefined,
  builtinNames: ReadonlySet<string>,
): AgentProfileEntry {
  const sourceId = inspection?.sourceId ?? BUILTIN_AGENT_PROFILE_SOURCE_ID;
  const source = SOURCE_BY_SOURCE_ID[sourceId] ?? 'user';
  const builtin = sourceId === BUILTIN_AGENT_PROFILE_SOURCE_ID;
  const hasOverride = !builtin && builtinNames.has(profile.name);
  const template = profile.systemPromptTemplate;
  return {
    name: profile.name,
    description: profile.description,
    source,
    builtin,
    essential: profile.name === DEFAULT_AGENT_PROFILE_NAME,
    enabled: inspection?.disabled !== true,
    has_override: hasOverride,
    file_path: profile.filePath,
    default_model: profile.defaultModel,
    capabilities_required: profile.capabilitiesRequired,
    tools: profile.tools,
    disallowed_tools: profile.disallowedTools,
    subagents: profile.subagents,
    when_to_use: profile.whenToUse,
    // ADR-010 ②: the editable template is the file body with the base
    // reference stripped — for ALL file rows (a custom composed row
    // projects its role text like a builtin override does; a
    // bare-reference body strips to empty → undefined). Builtin rows keep
    // the code-defined role text verbatim.
    system_prompt_template: builtin
      ? template
      : template === undefined
        ? undefined
        : fileBodyToWireTemplate(template),
    // ADR-010 ① (US-A4 form disclosure): the prompt form of the row's
    // underlying body — a file body composed onto the bare base reference
    // (ref + separator, or exactly the bare ref) is composed, a mid-text
    // mention is a full replacement; builtin rows and rows without a body
    // of their own are composed by the A16 model (their role rides the
    // base — the rust wire row's no-prompt branch emits the same).
    prompt_form: builtin || template === undefined || isComposedBody(template)
      ? 'composed'
      : 'replacement',
    // D12 (ADR-007): every row with a system prompt carries the fully
    // rendered effective prompt — the file profile's renderer resolves
    // `${base_prompt}` against the effective default profile, so a
    // covered builtin shows the base + role merge and a custom row shows
    // its own template post-substitution.
    effective_system_prompt: profile.systemPrompt({}),
    suppressed_candidates: inspection?.suppressed.map(
      (s): AgentProfileEntrySuppressedCandidate => ({
        source: SOURCE_BY_SOURCE_ID[s.sourceId] ?? 'user',
        priority: s.priority,
        reason: s.reason,
      }),
    ),
    prompt_vars: builtin && profile.promptVars ? { ...profile.promptVars } : undefined,
  };
}

/** Create a wire row from a ParsedAgentFile parsed from disk (fallback path). */
function fileDefToWire(
  def: ParsedAgentFile,
  filePath: string,
  isOverrideOfBuiltin: boolean,
  basePrompt?: (context: import('#/app/agentProfileCatalog/agentProfileCatalog').AgentProfileContext) => string,
): AgentProfileEntry {
  // The effective prompt for a file row: the template rendered with an
  // empty context, `${base_prompt}` (when present) bound to the effective
  // default profile's prompt — the same rendering path the runtime uses
  // (D12: every row with a system prompt carries it, not just builtins).
  const skillActive = def.tools === undefined || def.tools.includes('Skill');
  const effectiveSystemPrompt =
    basePrompt !== undefined
      ? renderPromptTemplate(def.prompt, {}, { skillActive }, basePrompt)
      : undefined;
  return {
    name: def.name,
    description: def.description,
    source: 'user',
    builtin: false,
    essential: def.name === DEFAULT_AGENT_PROFILE_NAME,
    enabled: def.enabled !== false,
    has_override: isOverrideOfBuiltin,
    file_path: filePath,
    default_model: def.defaultModel,
    capabilities_required: def.capabilitiesRequired,
    tools: def.tools,
    disallowed_tools: def.disallowedTools,
    subagents: def.subagents,
    when_to_use: def.whenToUse,
    // ADR-010 ②: the strip applies to ALL file rows on the fallback path
    // too — a custom composed body projects its role text, a bare
    // reference projects no template, a replacement body passes through.
    system_prompt_template: fileBodyToWireTemplate(def.prompt),
    // ADR-010 ① (US-A4 form disclosure): the file body's composition form
    // (two-shape discriminator — bare-ref prefix or bare-ref-only).
    prompt_form: isComposedBody(def.prompt) ? 'composed' : 'replacement',
    effective_system_prompt: effectiveSystemPrompt,
    // D16: a user file covering a builtin name displaces the builtin
    // definition into the row's suppressed candidates — the same shape
    // the catalog projection and the rust engine emit.
    suppressed_candidates: isOverrideOfBuiltin
      ? [{ source: 'builtin', priority: 0, reason: 'priority' }]
      : undefined,
  };
}

/** Create a wire row from an AgentProfile (builtin, fallback path). */
function builtinToWire(profile: AgentProfile): AgentProfileEntry {
  return {
    name: profile.name,
    description: profile.description,
    source: 'builtin',
    builtin: true,
    essential: profile.name === DEFAULT_AGENT_PROFILE_NAME,
    enabled: true,
    has_override: false,
    default_model: profile.defaultModel,
    capabilities_required: profile.capabilitiesRequired,
    tools: profile.tools,
    disallowed_tools: profile.disallowedTools,
    subagents: profile.subagents,
    when_to_use: profile.whenToUse,
    // Builtin prompts are code-defined `systemPrompt(context)` renderers. The
    // editable template is the profile's own role definition (undefined for
    // the default `agent`, whose prompt is the shared base itself); the
    // effective prompt is rendered with an empty context so the UI can show
    // the full behavior read-only. Context-dependent sections (cwd, agentsMd,
    // skills, …) render empty in that view.
    system_prompt_template: profile.systemPromptTemplate,
    // ADR-010 ① (US-A4 form disclosure): a builtin's code-defined role
    // rides the shared base by the A16 model — composed, never a full
    // replacement.
    prompt_form: 'composed' as const,
    effective_system_prompt: profile.systemPrompt({}),
    prompt_vars: profile.promptVars ? { ...profile.promptVars } : undefined,
  };
}

/**
 * Scan the user agents directory for `.md` files and parse them.
 * Returns a map of name → (ParsedAgentFile, filePath) and a list of skipped files.
 */
async function scanUserAgentFiles(
  fs: IHostFileSystem,
  homeDir: string,
): Promise<{
  files: Map<string, { def: ParsedAgentFile; filePath: string }>;
  skipped: SkippedAgentProfileFile[];
}> {
  const result = new Map<string, { def: ParsedAgentFile; filePath: string }>();
  const skipped: SkippedAgentProfileFile[] = [];
  const agentsDir = userAgentsDir(homeDir);
  let entries: readonly HostDirEntry[];
  try {
    entries = await fs.readdir(agentsDir);
  } catch {
    return { files: result, skipped };
  }
  // Sort so .md files come before .yaml/.yml for the same base name,
  // matching the unified @mirri-ai/agent-profile priority convention.
  const sorted = [...entries]
    .filter(
      (e) =>
        !e.isDirectory &&
        (e.name.endsWith('.md') || e.name.endsWith('.yaml') || e.name.endsWith('.yml')),
    )
    .toSorted((a, b) => {
      const baseA = a.name.replace(/\.(md|yaml|yml)$/, '');
      const baseB = b.name.replace(/\.(md|yaml|yml)$/, '');
      if (baseA !== baseB) return baseA.localeCompare(baseB);
      // Same base name: .md (index 0) sorts before .yaml (index 1) / .yml (index 2)
      const extA = (['.md', '.yaml', '.yml'] as const).findIndex((ext) => a.name.endsWith(ext));
      const extB = (['.md', '.yaml', '.yml'] as const).findIndex((ext) => b.name.endsWith(ext));
      return extA - extB;
    });
  for (const entry of sorted) {
    const filePath = join(agentsDir, entry.name);
    let text: string;
    try {
      text = await fs.readText(filePath);
    } catch {
      continue;
    }
    try {
      const def = parseAgentFileText({ path: filePath, source: 'user', text });
      // If already registered (e.g. .md won over .yaml for a same-name pair),
      // keep the first entry — the sort order guarantees .md comes first.
      if (!result.has(def.name)) {
        result.set(def.name, { def, filePath });
      }
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      skipped.push({ path: filePath, reason });
    }
  }
  return { files: result, skipped };
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

export class AgentProfileManagementService
  extends Disposable
  implements IAgentProfileManagementService
{
  declare readonly _serviceBrand: undefined;

  constructor(
    @ILogService private readonly log: ILogService,
    @IAgentProfileRegistry private readonly registry: IAgentProfileRegistry,
    @IBuiltinAgentProfileLoader private readonly builtin: IBuiltinAgentProfileLoader,
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IHostFileSystem private readonly fs: IHostFileSystem,
    @IWorkspaceService private readonly workspaceService: IWorkspaceService,
    @IWorkspaceLifecycleService private readonly workspaceLifecycle: IWorkspaceLifecycleService,
  ) {
    super();
  }

  async list(workspaceRoot?: string): Promise<AgentProfileEntriesView> {
    // D21 / US-A8: the SYSTEM.md override — read once per listing and
    // disclosed on the essential `agent` row (the same field shape the rust
    // engine emits). The other rows' `effective_system_prompt` values already
    // resolve through the SYSTEM.md-aware default (the loader registers
    // the override profile), so only the disclosure needs stitching here.
    const systemMd = await readSystemMdOverride(
      this.fs,
      this.bootstrap.homeDir,
      (message) => this.log.warn(message),
    );
    const handle = await this.resolveWorkspaceHandle(workspaceRoot);
    if (handle !== undefined) {
      const catalog = new SessionAgentProfileCatalogService(
        this.registry,
        { _serviceBrand: undefined, workspaceKey: handle.id },
        this.log,
      );
      try {
        const builtinNames = new Set(this.builtin.list().map((p) => p.name));
        const entries = catalog.listAll().map((profile) =>
          catalogEntryToWire(profile, catalog.inspect(profile.name), builtinNames),
        );
        const skipped: SkippedAgentProfileFile[] = [];
        const seen = new Set<string>();
        for (const entry of this.registry.entries()) {
          if (entry.workspaceKey !== undefined && entry.workspaceKey !== handle.id) continue;
          for (const skip of entry.contribution.skipped ?? []) {
            if (!seen.has(skip.path)) {
              seen.add(skip.path);
              skipped.push({ path: skip.path, reason: skip.reason });
            }
          }
        }
        return { entries: decorateEssentialRow(entries, systemMd), skipped };
      } finally {
        catalog.dispose();
      }
    }
    // No workspace yet — the loaders have never registered; fall back to the
    // direct scan of the global sources (builtin + user directory).
    const scan = await scanUserAgentFiles(this.fs, this.bootstrap.homeDir);
    const builtins = this.builtin.list();
    const result = new Map<string, AgentProfileEntry>();
    for (const profile of builtins) {
      result.set(profile.name, builtinToWire(profile));
    }
    const builtinNames = new Set(result.keys());
    // The `${base_prompt}` binder for the fallback's file rows — the builtin
    // default profile's prompt, the same binding the loader uses.
    const builtinDefault = builtins.find((p) => p.name === DEFAULT_AGENT_PROFILE_NAME);
    const basePrompt =
      builtinDefault !== undefined
        ? (context: import('#/app/agentProfileCatalog/agentProfileCatalog').AgentProfileContext) =>
            builtinDefault.systemPrompt(context)
        : undefined;
    for (const [name, { def, filePath }] of scan.files) {
      result.set(name, fileDefToWire(def, filePath, builtinNames.has(name), basePrompt));
    }
    return {
      entries: decorateEssentialRow([...result.values()], systemMd),
      skipped: [...scan.skipped],
    };
  }

  async setEnabled(name: string, enabled: boolean, workspaceRoot?: string): Promise<void> {
    // The essential guard precedes the lookup (the REST action endpoint's
    // order): disabling the default profile is refused even when the name
    // also resolves through a user file, while enabling it stays allowed.
    if (name === DEFAULT_AGENT_PROFILE_NAME && !enabled) {
      throw new Error2(
        AgentProfileManagementErrors.codes.AGENT_PROFILE_ESSENTIAL,
        `Cannot disable the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
      );
    }
    const view = await this.list(workspaceRoot);
    const existing = view.entries.find((r) => r.name === name);
    if (existing === undefined) {
      throw new Error2(
        AgentProfileManagementErrors.codes.AGENT_PROFILE_NOT_FOUND,
        `Agent profile "${name}" was not found`,
      );
    }

    const agentsDir = userAgentsDir(this.bootstrap.homeDir);
    await this.fs.mkdir(agentsDir, { recursive: true });

    // SDR-014 ②: a non-empty template is composed onto the base reference —
    // custom rows and builtin overrides are isomorphic, so toggling a
    // custom profile with a prompt never degrades it to a literal
    // replacement body. An already-composed template passes through (no
    // double wrap); an empty or absent one falls to the serializer's bare
    // base reference.
    const template = existing.system_prompt_template?.trim()
      ? composeOntoBasePrompt(existing.system_prompt_template)
      : undefined;
    const content = serializeAgentMd({
      name: existing.name,
      description: existing.description ?? '',
      whenToUse: existing.when_to_use,
      tools: existing.tools ? [...existing.tools] : undefined,
      disallowedTools: existing.disallowed_tools ? [...existing.disallowed_tools] : undefined,
      subagents: existing.subagents ? [...existing.subagents] : undefined,
      defaultModel: existing.default_model,
      capabilitiesRequired: existing.capabilities_required
        ? [...existing.capabilities_required]
        : undefined,
      enabled,
      systemPromptTemplate: template,
    });

    await this.fs.writeText(agentFilePath(this.bootstrap.homeDir, name), content);
    await removeLegacyAgentFiles(this.fs, this.bootstrap.homeDir, name, (m) =>
      this.log.warn(m),
    );
    await this.reload();
  }

  async reload(): Promise<void> {
    const handle = await this.resolveWorkspaceHandle(undefined);
    if (handle === undefined) return;
    await handle.accessor.get(IUserAgentProfileLoader).reload();
  }

  /**
   * The workspace the read projection resolves against: an explicit root
   * (create-or-get, so project-level rows materialize), else the first open
   * handler, else the first known workspace (materializing it), else
   * `undefined` — the zero-workspace fallback.
   */
  private async resolveWorkspaceHandle(
    workspaceRoot: string | undefined,
  ): Promise<IWorkspaceScopeHandle | undefined> {
    if (workspaceRoot !== undefined) {
      return this.workspaceLifecycle.handlerFor({ root: workspaceRoot });
    }
    const handlers = this.workspaceLifecycle.handlers.list();
    if (handlers.length > 0) return handlers[0];

    const workspaces = await this.workspaceService.list();
    if (workspaces.length === 0) return undefined;

    const ws = workspaces[0]!;
    return this.workspaceLifecycle.handlerFor({ workspaceId: ws.id, root: ws.root });
  }
}

registerScopedService(
  LifecycleScope.App,
  IAgentProfileManagementService,
  AgentProfileManagementService,
  ScopeActivation.OnDemand,
  'agentProfileCatalog',
);
