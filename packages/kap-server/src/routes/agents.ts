/**
 * `/agents` REST routes — CRUD for agent profiles (v2 port).
 *
 * Mirrors the v1 server's wire contract
 * (`packages/server/src/routes/agents.ts`) path-for-path and schema-for-schema:
 *
 *   GET    /agents                          data: {items: ProfileEntry[]}
 *   GET    /agents/{name}                   data: ProfileEntry
 *   POST   /agents                          data: ProfileEntry
 *   PUT    /agents/{name}                   data: ProfileEntry
 *   DELETE /agents/{name}
 *   POST   /agents/{name}:enable
 *   POST   /agents/{name}:disable
 *   POST   /agents/{name}:reset
 *
 * **v2 profile model** (differs from v1):
 * - v1 stores profiles as `.yaml` files managed by `IProfileService`.
 * - v2 discovers profiles from `.md` files (YAML frontmatter + Markdown body)
 *   via `IUserAgentProfileLoader` / `IAgentProfileRegistry`. There is no
 *   `IProfileService` equivalent — the engine deliberately does not write config
 *   files. So CRUD write is an EDGE concern: routes write `.md` files directly
 *   under `<home>/agents/`, then call `IUserAgentProfileLoader.reload()` to
 *   re-scan.
 *
 * **Hybrid resolution**: The route resolves profiles from two sources:
 *   1. `IBuiltinAgentProfileLoader` (App-scope) — builtin profiles.
 *   2. Direct filesystem scan of `<home>/agents/*.md` — user/custom profiles.
 * This hybrid approach ensures the routes work regardless of workspace state.
 * After file writes, `IUserAgentProfileLoader.reload()` is called on any
 * available workspace so the engine's runtime view stays in sync.
 *
 * **Essential flag**: only the `agent` profile (DEFAULT_AGENT_PROFILE_NAME) is
 * essential and cannot be modified, disabled, or deleted.
 *
 * **Enabled flag**: v2 has no `disabled_agents` config concept. Enable/disable
 * is expressed via the `enabled` field in the `.md` frontmatter. All profiles
 * are enabled by default.
 *
 * **Has override**: when a user `.md` file overrides a builtin profile, the
 * `override` frontmatter field is set to `true` and `has_override` is `true`.
 *
 * **Error mapping**:
 *   - unknown profile            → envelope `code: 40418 agent_profile.not_found`
 *   - duplicate name             → envelope `code: 40419 agent_profile.already_exists`
 *   - deleting a builtin         → envelope `code: 41306 agent_profile.builtin_readonly`
 *   - modifying an essential     → envelope `code: 41307 agent_profile.essential`
 */

import {
  DEFAULT_AGENT_PROFILE_NAME,
  IBuiltinAgentProfileLoader,
  IBootstrapService,
  IHostFileSystem,
  IUserAgentProfileLoader,
  IWorkspaceLifecycleService,
  IWorkspaceService,
  parseAgentFileText,
  resolveMirriHome,
  type AgentProfile,
  type Scope,
} from '@mirri-ai/agent-core-v2';
import {
  createProfileRequestSchema,
  listProfilesResponseSchema,
  profileEntrySchema,
  updateProfileRequestSchema,
} from '@mirri-ai/protocol';
import { dump } from 'js-yaml';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { z } from 'zod';

import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { ErrorCode } from '../protocol/error-codes';
import { parseActionSuffix } from './action-suffix';

/** Inferred type for a parsed agent file — avoids importing an unexported symbol. */
type ParsedAgentFile = ReturnType<typeof parseAgentFileText>;

// ---------------------------------------------------------------------------
// Route host interface
// ---------------------------------------------------------------------------

interface AgentsRouteHost {
  get(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  post(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; body: unknown; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  put(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; body: unknown; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
  delete(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (
      req: { id: string; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
}

// ---------------------------------------------------------------------------
// Param schemas
// ---------------------------------------------------------------------------

const nameParamSchema = z.object({
  name: z.string().min(1),
});

// ---------------------------------------------------------------------------
// Projection: v2 profile data → wire ProfileEntry (snake_case)
// ---------------------------------------------------------------------------

interface ProfileEntryWire {
  name: string;
  description?: string;
  source: 'builtin' | 'user' | 'project' | 'extra';
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  has_override: boolean;
  file_path?: string;
  extends?: string;
  default_model?: string;
  capabilities_required?: readonly string[];
  tools?: readonly string[];
  when_to_use?: string;
  system_prompt_template?: string;
  prompt_vars?: Record<string, string>;
}

/** A resolved profile ready for wire projection. */
interface ResolvedProfile {
  name: string;
  description?: string;
  source: 'builtin' | 'user' | 'project' | 'extra';
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  hasOverride: boolean;
  filePath?: string;
  extends?: string;
  whenToUse?: string;
  tools?: readonly string[];
  disallowedTools?: readonly string[];
  modelPreference?: 'primary' | 'secondary';
  systemPromptTemplate?: string;
  defaultModel?: string;
  capabilitiesRequired?: readonly string[];
  promptVars?: Record<string, string>;
}

function resolvedToWire(r: ResolvedProfile): ProfileEntryWire {
  return {
    name: r.name,
    description: r.description,
    source: r.source,
    builtin: r.builtin,
    essential: r.essential,
    enabled: r.enabled,
    has_override: r.hasOverride,
    file_path: r.filePath,
    extends: r.extends,
    default_model: r.defaultModel,
    capabilities_required: r.capabilitiesRequired,
    tools: r.tools,
    when_to_use: r.whenToUse,
    system_prompt_template: r.systemPromptTemplate,
    prompt_vars: r.promptVars,
  };
}

/** Create a ResolvedProfile from an ParsedAgentFile parsed from disk. */
function fileDefToResolved(
  def: ParsedAgentFile,
  filePath: string,
  isOverrideOfBuiltin: boolean,
): ResolvedProfile {
  // Read `enabled` from the raw frontmatter — parseAgentFileText doesn't
  // extract it, but the `enabled` field lives in the YAML frontmatter.
  // For now, default to true; the route reads enabled directly when needed.
  return {
    name: def.name,
    description: def.description,
    source: 'user',
    builtin: false,
    essential: def.name === DEFAULT_AGENT_PROFILE_NAME,
    enabled: true,
    hasOverride: isOverrideOfBuiltin || def.override,
    filePath,
    extends: def.extends,
    whenToUse: def.whenToUse,
    tools: def.tools,
    disallowedTools: def.disallowedTools,
    modelPreference: def.modelPreference,
    defaultModel: def.defaultModel,
    capabilitiesRequired: def.capabilitiesRequired,
    systemPromptTemplate: def.prompt,
    promptVars: def.promptVars,
  };
}

/** Create a ResolvedProfile from an AgentProfile (builtin). */
function builtinToResolved(profile: AgentProfile): ResolvedProfile {
  return {
    name: profile.name,
    description: profile.description,
    source: 'builtin',
    builtin: true,
    essential: profile.name === DEFAULT_AGENT_PROFILE_NAME,
    enabled: true,
    hasOverride: false,
    extends: profile.extends,
    whenToUse: profile.whenToUse,
    tools: profile.tools,
    disallowedTools: profile.disallowedTools,
    modelPreference: profile.modelPreference,
    defaultModel: profile.defaultModel,
    capabilitiesRequired: profile.capabilitiesRequired,
    promptVars: profile.promptVars ? { ...profile.promptVars } : undefined,
  };
}

// ---------------------------------------------------------------------------
// Helpers: resolve the user agents directory
// ---------------------------------------------------------------------------

function userAgentsDir(homeDir: string): string {
  return join(resolveMirriHome(homeDir), 'agents');
}

function agentFilePath(homeDir: string, name: string): string {
  return join(userAgentsDir(homeDir), `${name}.md`);
}

// ---------------------------------------------------------------------------
// Helpers: .md file serialization
// ---------------------------------------------------------------------------

/** Serialize an agent definition to the v2 `.md` frontmatter format. */
function serializeAgentMd(fields: {
  name: string;
  description?: string;
  whenToUse?: string;
  tools?: readonly string[];
  disallowedTools?: readonly string[];
  subagents?: readonly string[];
  modelPreference?: 'primary' | 'secondary';
  defaultModel?: string;
  capabilitiesRequired?: readonly string[];
  override?: boolean;
  enabled?: boolean;
  extends?: string;
  promptVars?: Record<string, string>;
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
  if (fields.subagents && fields.subagents.length > 0) frontmatter['subagents'] = [...fields.subagents];
  if (fields.modelPreference) frontmatter['model_preference'] = fields.modelPreference;
  if (fields.defaultModel) frontmatter['defaultModel'] = fields.defaultModel;
  if (fields.capabilitiesRequired && fields.capabilitiesRequired.length > 0)
    frontmatter['capabilitiesRequired'] = [...fields.capabilitiesRequired];
  if (fields.override) frontmatter['override'] = true;
  if (fields.enabled === false) frontmatter['enabled'] = false;
  if (fields.extends) frontmatter['extends'] = fields.extends;
  if (fields.promptVars && Object.keys(fields.promptVars).length > 0)
    frontmatter['promptVars'] = fields.promptVars;

  const yaml = dump(frontmatter, { lineWidth: -1, quotingType: '"' }).trim();
  // parseAgentFileText rejects files with an empty prompt body — always write
  // at least a minimal placeholder when no system prompt template is provided.
  const body = fields.systemPromptTemplate?.trim() || 'You are a helpful AI assistant.';
  return `---\n${yaml}\n---\n${body}`;
}

// ---------------------------------------------------------------------------
// Helpers: resolve workspace handle for workspace-scoped services
// ---------------------------------------------------------------------------

async function resolveWorkspaceHandle(
  core: Scope,
): Promise<import('@mirri-ai/agent-core-v2').IWorkspaceScopeHandle | undefined> {
  const lifecycle = core.accessor.get(IWorkspaceLifecycleService);
  const handlers = lifecycle.handlers.list();
  if (handlers.length > 0) return handlers[0];

  const workspaces = await core.accessor.get(IWorkspaceService).list();
  if (workspaces.length === 0) return undefined;

  const ws = workspaces[0]!;
  return lifecycle.handlerFor({ workspaceId: ws.id, root: ws.root });
}

// ---------------------------------------------------------------------------
// Helpers: reload user profile loader (after file writes)
// ---------------------------------------------------------------------------

async function reloadUserProfileLoader(core: Scope): Promise<void> {
  const handle = await resolveWorkspaceHandle(core);
  if (handle === undefined) return;
  const loader = handle.accessor.get(IUserAgentProfileLoader);
  await loader.reload();
}

// ---------------------------------------------------------------------------
// Helpers: build the complete profile list (hybrid: builtin + disk scan)
// ---------------------------------------------------------------------------

/**
 * Scan the user agents directory for `.md` files and parse them.
 * Returns a map of name → (ParsedAgentFile, filePath).
 */
async function scanUserAgentFiles(
  fs: IHostFileSystem,
  homeDir: string,
): Promise<Map<string, { def: ParsedAgentFile; filePath: string }>> {
  const result = new Map<string, { def: ParsedAgentFile; filePath: string }>();
  const agentsDir = userAgentsDir(homeDir);
  let entries: readonly import('@mirri-ai/agent-core-v2').HostDirEntry[];
  try {
    entries = await fs.readdir(agentsDir);
  } catch {
    return result;
  }
  for (const entry of entries) {
    if (entry.isDirectory) continue;
    if (!entry.name.endsWith('.md') && !entry.name.endsWith('.yml') && !entry.name.endsWith('.yaml')) continue;
    const filePath = join(agentsDir, entry.name);
    let text: string;
    try {
      text = await fs.readText(filePath);
    } catch {
      continue;
    }
    try {
      const def = parseAgentFileText({ path: filePath, source: 'user', text });
      result.set(def.name, { def, filePath });
    } catch {
      // Skip unparseable files
    }
  }
  return result;
}

/**
 * Build the complete merged profile list from both builtin profiles and
 * user .md files on disk. User files override builtins with the same name.
 */
async function buildFullProfileList(
  core: Scope,
): Promise<ResolvedProfile[]> {
  const builtinLoader = core.accessor.get(IBuiltinAgentProfileLoader);
  const bootstrap = core.accessor.get(IBootstrapService);
  const fs = core.accessor.get(IHostFileSystem);

  const builtinProfiles = builtinLoader.list();
  const userFiles = await scanUserAgentFiles(fs, bootstrap.homeDir);

  const result = new Map<string, ResolvedProfile>();

  // Add builtin profiles first
  for (const profile of builtinProfiles) {
    result.set(profile.name, builtinToResolved(profile));
  }

  // Add/override with user profiles
  const builtinNames = new Set(builtinProfiles.map((p) => p.name));
  for (const [name, { def, filePath }] of userFiles) {
    const isOverrideOfBuiltin = builtinNames.has(name);
    result.set(name, fileDefToResolved(def, filePath, isOverrideOfBuiltin));
  }

  return [...result.values()];
}

// ---------------------------------------------------------------------------
// Helpers: read the `enabled` field from an agent .md file's raw frontmatter
// ---------------------------------------------------------------------------

/**
 * Read the `enabled` boolean from the YAML frontmatter of an agent .md file.
 * Returns `undefined` if the file doesn't exist or has no `enabled` field.
 */
async function readEnabledFromFrontmatter(
  fs: IHostFileSystem,
  filePath: string,
): Promise<boolean | undefined> {
  let text: string;
  try {
    text = await fs.readText(filePath);
  } catch {
    return undefined;
  }
  // Quick YAML frontmatter parse: extract between --- markers
  const match = text.match(/^---\s*\n([\s\S]*?)\n---/);
  if (!match) return undefined;
  const yamlText = match[1]!;
  // Simple check for `enabled: false`
  if (/enabled:\s*false/i.test(yamlText)) return false;
  if (/enabled:\s*true/i.test(yamlText)) return true;
  return undefined;
}

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

export function registerAgentRoutes(app: AgentsRouteHost, core: Scope): void {
  // GET /agents -----------------------------------------------------------
  const listRoute = defineRoute(
    {
      method: 'GET',
      path: '/agents',
      success: { data: listProfilesResponseSchema },
      description: 'List all agent profiles (built-in + custom)',
      tags: ['agents'],
      operationId: 'listAgents',
    },
    async (req, reply) => {
      const resolved = await buildFullProfileList(core);
      // Enrich with enabled state from frontmatter
      const fs = core.accessor.get(IHostFileSystem);
      const items = await Promise.all(
        resolved.map(async (r) => {
          if (r.filePath !== undefined) {
            const enabled = await readEnabledFromFrontmatter(fs, r.filePath);
            if (enabled !== undefined) {
              return resolvedToWire({ ...r, enabled });
            }
          }
          return resolvedToWire(r);
        }),
      );
      reply.send(okEnvelope({ items }, req.id));
    },
  );
  app.get(
    listRoute.path,
    listRoute.options,
    listRoute.handler as Parameters<AgentsRouteHost['get']>[2],
  );

  // GET /agents/{name} ----------------------------------------------------
  const getRoute = defineRoute(
    {
      method: 'GET',
      path: '/agents/{name}',
      params: nameParamSchema,
      success: { data: profileEntrySchema },
      errors: {
        [ErrorCode.AGENT_PROFILE_NOT_FOUND]: {},
      },
      description: 'Get a single agent profile by name',
      tags: ['agents'],
      operationId: 'getAgent',
    },
    async (req, reply) => {
      const { name } = req.params;
      const resolved = await buildFullProfileList(core);
      const found = resolved.find((r) => r.name === name);
      if (found === undefined) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_NOT_FOUND,
            `Agent profile "${name}" was not found`,
            req.id,
          ),
        );
        return;
      }
      // Enrich with enabled state
      let enabled = found.enabled;
      if (found.filePath !== undefined) {
        const fs = core.accessor.get(IHostFileSystem);
        const enabledFromFm = await readEnabledFromFrontmatter(fs, found.filePath);
        if (enabledFromFm !== undefined) enabled = enabledFromFm;
      }
      reply.send(okEnvelope(resolvedToWire({ ...found, enabled }), req.id));
    },
  );
  app.get(
    getRoute.path,
    getRoute.options,
    getRoute.handler as Parameters<AgentsRouteHost['get']>[2],
  );

  // POST /agents ----------------------------------------------------------
  const createRoute = defineRoute(
    {
      method: 'POST',
      path: '/agents',
      body: createProfileRequestSchema,
      success: { data: profileEntrySchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.AGENT_PROFILE_ALREADY_EXISTS]: {},
        [ErrorCode.AGENT_PROFILE_ESSENTIAL]: {},
      },
      description: 'Create a custom agent profile',
      tags: ['agents'],
      operationId: 'createAgent',
    },
    async (req, reply) => {
      const body = req.body;
      const name = body['name'] as string;
      if (!name || name.length === 0) {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, 'name is required', req.id));
        return;
      }

      if (name === DEFAULT_AGENT_PROFILE_NAME) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ESSENTIAL,
            `Cannot create a profile with the reserved name "${DEFAULT_AGENT_PROFILE_NAME}"`,
            req.id,
          ),
        );
        return;
      }

      // Check if a profile with this name already exists (registry + disk)
      const resolved = await buildFullProfileList(core);
      if (resolved.some((r) => r.name === name)) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ALREADY_EXISTS,
            `Agent profile "${name}" already exists`,
            req.id,
          ),
        );
        return;
      }

      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const agentsDir = userAgentsDir(bootstrap.homeDir);
      await mkdir(agentsDir, { recursive: true });

      const filePath = agentFilePath(bootstrap.homeDir, name);
      const content = serializeAgentMd({
        name,
        description: body['description'] as string | undefined,
        whenToUse: body['when_to_use'] as string | undefined,
        tools: body['tools'] as string[] | undefined,
        defaultModel: body['default_model'] as string | undefined,
        capabilitiesRequired: body['capabilities_required'] as string[] | undefined,
        systemPromptTemplate: body['system_prompt_template'] as string | undefined,
        extends: body['extends'] as string | undefined,
        promptVars: body['prompt_vars'] as Record<string, string> | undefined,
      });

      await fs.writeText(filePath, content);
      await reloadUserProfileLoader(core);

      reply.send(
        okEnvelope(
          resolvedToWire({
            name,
            description: body['description'] as string | undefined,
            source: 'user',
            builtin: false,
            essential: false,
            enabled: true,
            hasOverride: false,
            filePath,
            extends: body['extends'] as string | undefined,
            whenToUse: body['when_to_use'] as string | undefined,
            tools: body['tools'] as string[] | undefined,
            systemPromptTemplate: body['system_prompt_template'] as string | undefined,
            defaultModel: body['default_model'] as string | undefined,
            capabilitiesRequired: body['capabilities_required'] as string[] | undefined,
            promptVars: body['prompt_vars'] as Record<string, string> | undefined,
          }),
          req.id,
        ),
      );
    },
  );
  app.post(
    createRoute.path,
    createRoute.options,
    createRoute.handler as Parameters<AgentsRouteHost['post']>[2],
  );

  // PUT /agents/{name} ----------------------------------------------------
  const updateRoute = defineRoute(
    {
      method: 'PUT',
      path: '/agents/{name}',
      body: updateProfileRequestSchema,
      params: nameParamSchema,
      success: { data: profileEntrySchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.AGENT_PROFILE_NOT_FOUND]: {},
        [ErrorCode.AGENT_PROFILE_ESSENTIAL]: {},
      },
      description: 'Update a custom or built-in agent profile',
      tags: ['agents'],
      operationId: 'updateAgent',
    },
    async (req, reply) => {
      const { name } = req.params;
      const body = req.body;

      if (name === DEFAULT_AGENT_PROFILE_NAME) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ESSENTIAL,
            `Cannot modify the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
            req.id,
          ),
        );
        return;
      }

      const resolved = await buildFullProfileList(core);
      const existing = resolved.find((r) => r.name === name);
      if (existing === undefined) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_NOT_FOUND,
            `Agent profile "${name}" was not found`,
            req.id,
          ),
        );
        return;
      }

      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const agentsDir = userAgentsDir(bootstrap.homeDir);
      await mkdir(agentsDir, { recursive: true });

      const filePath = agentFilePath(bootstrap.homeDir, name);

      // Merge: new fields override existing; keep existing for omitted fields
      const mergedDescription = ('description' in body && body['description'] !== undefined)
        ? (body['description'] as string)
        : existing.description ?? '';
      const mergedWhenToUse = ('when_to_use' in body && body['when_to_use'] !== undefined)
        ? (body['when_to_use'] as string)
        : existing.whenToUse;
      const mergedTools = ('tools' in body && body['tools'] !== undefined)
        ? (body['tools'] as string[])
        : existing.tools ? [...existing.tools] : undefined;
      const mergedDefaultModel = ('default_model' in body && body['default_model'] !== undefined)
        ? (body['default_model'] as string | undefined)
        : existing.defaultModel;
      const mergedCapabilitiesRequired =
        ('capabilities_required' in body && body['capabilities_required'] !== undefined)
          ? (body['capabilities_required'] as string[] | undefined)
          : existing.capabilitiesRequired
            ? [...existing.capabilitiesRequired]
            : undefined;
      const mergedPrompt = ('system_prompt_template' in body && body['system_prompt_template'] !== undefined)
        ? (body['system_prompt_template'] as string)
        : existing.systemPromptTemplate;
      const mergedExtends = ('extends' in body && body['extends'] !== undefined)
        ? (body['extends'] as string | undefined)
        : existing.extends;
      const mergedPromptVars = ('prompt_vars' in body && body['prompt_vars'] !== undefined)
        ? (body['prompt_vars'] as Record<string, string> | undefined)
        : existing.promptVars;

      // For builtin profiles being overridden, mark as override
      const isBuiltinOverride = existing.builtin;

      const content = serializeAgentMd({
        name,
        description: mergedDescription,
        whenToUse: mergedWhenToUse,
        tools: mergedTools,
        defaultModel: mergedDefaultModel,
        capabilitiesRequired: mergedCapabilitiesRequired,
        override: isBuiltinOverride,
        extends: mergedExtends,
        promptVars: mergedPromptVars,
        systemPromptTemplate: mergedPrompt,
      });

      await fs.writeText(filePath, content);
      await reloadUserProfileLoader(core);

      reply.send(
        okEnvelope(
          resolvedToWire({
            ...existing,
            description: mergedDescription,
            whenToUse: mergedWhenToUse,
            tools: mergedTools,
            defaultModel: mergedDefaultModel,
            capabilitiesRequired: mergedCapabilitiesRequired,
            extends: mergedExtends,
            promptVars: mergedPromptVars,
            systemPromptTemplate: mergedPrompt,
            source: isBuiltinOverride ? 'user' : existing.source,
            builtin: false,
            hasOverride: isBuiltinOverride,
            filePath,
          }),
          req.id,
        ),
      );
    },
  );
  app.put(
    updateRoute.path,
    updateRoute.options,
    updateRoute.handler as Parameters<AgentsRouteHost['put']>[2],
  );

  // DELETE /agents/{name} -------------------------------------------------
  const deleteRoute = defineRoute(
    {
      method: 'DELETE',
      path: '/agents/{name}',
      params: nameParamSchema,
      success: { data: z.object({ deleted: z.literal(true) }) },
      errors: {
        [ErrorCode.AGENT_PROFILE_NOT_FOUND]: {},
        [ErrorCode.AGENT_PROFILE_BUILTIN_READONLY]: {},
        [ErrorCode.AGENT_PROFILE_ESSENTIAL]: {},
      },
      description: 'Delete a custom agent profile',
      tags: ['agents'],
      operationId: 'deleteAgent',
    },
    async (req, reply) => {
      const { name } = req.params;

      if (name === DEFAULT_AGENT_PROFILE_NAME) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ESSENTIAL,
            `Cannot delete the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
            req.id,
          ),
        );
        return;
      }

      const resolved = await buildFullProfileList(core);
      const existing = resolved.find((r) => r.name === name);
      if (existing === undefined) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_NOT_FOUND,
            `Agent profile "${name}" was not found`,
            req.id,
          ),
        );
        return;
      }

      // Cannot delete builtin profiles (only their override file)
      if (existing.builtin && !existing.hasOverride) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_BUILTIN_READONLY,
            `Cannot delete built-in profile "${name}"`,
            req.id,
          ),
        );
        return;
      }

      // Delete the .md file
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = agentFilePath(bootstrap.homeDir, name);
      try {
        await fs.remove(filePath);
      } catch {
        // File may not exist
      }

      await reloadUserProfileLoader(core);
      reply.send(okEnvelope({ deleted: true }, req.id));
    },
  );
  app.delete(
    deleteRoute.path,
    deleteRoute.options,
    deleteRoute.handler as Parameters<AgentsRouteHost['delete']>[2],
  );

  // POST /agents/{name}:enable|disable|reset ------------------------------
  const actionRoute = defineRoute(
    {
      method: 'POST',
      path: '/agents/{tail}',
      params: z.object({ tail: z.string().min(1) }),
      success: { data: z.object({ ok: z.literal(true) }) },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.AGENT_PROFILE_NOT_FOUND]: {},
        [ErrorCode.AGENT_PROFILE_ESSENTIAL]: {},
        [ErrorCode.AGENT_PROFILE_BUILTIN_READONLY]: {},
      },
      description: 'Enable, disable, or reset an agent profile',
      tags: ['agents'],
      operationId: 'toggleAgent',
    },
    async (req, reply) => {
      const { tail } = req.params;
      const parsed = parseActionSuffix({
        tail,
        allowedActions: ['enable', 'disable', 'reset'] as const,
        resourceLabel: 'name',
      });
      if (parsed.kind === 'invalid') {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, parsed.reason, req.id));
        return;
      }
      if (parsed.kind === 'bare') {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, `unsupported action: ${tail}`, req.id));
        return;
      }
      const name = parsed.id;
      const action = parsed.action;

      if (name === DEFAULT_AGENT_PROFILE_NAME && action !== 'enable') {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ESSENTIAL,
            `Cannot ${action} the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
            req.id,
          ),
        );
        return;
      }

      const resolved = await buildFullProfileList(core);
      const existing = resolved.find((r) => r.name === name);
      if (existing === undefined) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_NOT_FOUND,
            `Agent profile "${name}" was not found`,
            req.id,
          ),
        );
        return;
      }

      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const agentsDir = userAgentsDir(bootstrap.homeDir);
      const filePath = agentFilePath(bootstrap.homeDir, name);

      if (action === 'reset') {
        // Reset = remove the override .md file for builtin profiles
        if (!existing.hasOverride) {
          reply.send(
            errEnvelope(
              ErrorCode.AGENT_PROFILE_BUILTIN_READONLY,
              `Can only reset built-in profiles with overrides`,
              req.id,
            ),
          );
          return;
        }
        try {
          await fs.remove(filePath);
        } catch {
          // File may not exist
        }
        await reloadUserProfileLoader(core);
        reply.send(okEnvelope({ ok: true }, req.id));
        return;
      }

      // enable / disable — write/rewrite the .md file with the enabled field
      await mkdir(agentsDir, { recursive: true });

      const isBuiltinOverride = existing.builtin;

      const content = serializeAgentMd({
        name: existing.name,
        description: existing.description ?? '',
        whenToUse: existing.whenToUse,
        tools: existing.tools ? [...existing.tools] : undefined,
        disallowedTools: existing.disallowedTools ? [...existing.disallowedTools] : undefined,
        defaultModel: existing.defaultModel,
        capabilitiesRequired: existing.capabilitiesRequired
          ? [...existing.capabilitiesRequired]
          : undefined,
        override: isBuiltinOverride,
        enabled: action === 'enable',
        extends: existing.extends,
        promptVars: existing.promptVars,
        systemPromptTemplate: existing.systemPromptTemplate,
      });

      await fs.writeText(filePath, content);
      await reloadUserProfileLoader(core);
      reply.send(okEnvelope({ ok: true }, req.id));
    },
  );
  app.post(
    actionRoute.path,
    actionRoute.options,
    actionRoute.handler as Parameters<AgentsRouteHost['post']>[2],
  );
}
