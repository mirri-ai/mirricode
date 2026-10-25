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
 * **Data plane (R5-2)**: the read view and the enable/disable write channel
 * are the engine's App-scope `IAgentProfileManagementService` — the same
 * service the SDK v2 client reaches through the klient wire surface, so the
 * REST routes, the web settings page, and the terminal `/agents` view read
 * and write one source. The service serves snake_case wire rows (the
 * protocol REST `ProfileEntry` shape) directly.
 *
 * **v2 profile model** (differs from v1): v1 stores profiles as `.yaml` files
 * managed by `IProfileService`; v2 discovers profiles from `.md` files
 * (YAML frontmatter + Markdown body) via the profile registry / catalog
 * extension point. CRUD write is an EDGE concern: POST/PUT/DELETE/reset
 * write `.md` files directly under `<home>/agents/` (through the engine's
 * exported pure helpers), then call the service's `reload()` to re-scan.
 * Enable/disable delegates to the service's `setEnabled` write channel
 * wholesale.
 *
 * **Essential flag**: only the `agent` profile (DEFAULT_AGENT_PROFILE_NAME) is
 * essential and cannot be modified, disabled, or deleted.
 *
 * **Enabled flag**: v2 has no `disabled_agents` config concept. Enable/disable
 * is expressed via the `enabled` field in the `.md` frontmatter. All profiles
 * are enabled by default.
 *
 * **Has override**: when a user `.md` file overrides a builtin profile (by
 * virtue of existing — no declaration key, adjudication D14), `has_override`
 * is `true`; the write channel never emits an `override` frontmatter key.
 *
 * **Error mapping**:
 *   - unknown profile            → envelope `code: 40418 agent_profile.not_found`
 *   - duplicate name             → envelope `code: 40419 agent_profile.already_exists`
 *   - deleting a builtin         → envelope `code: 41306 agent_profile.builtin_readonly`
 *   - modifying an essential     → envelope `code: 41307 agent_profile.essential`
 */

import {
  agentFilePath,
  composeOntoBasePrompt,
  DEFAULT_AGENT_PROFILE_NAME,
  ErrorCodes,
  IAgentProfileManagementService,
  IBootstrapService,
  IHostFileSystem,
  ILogService,
  isError2,
  isComposedBody,
  removeLegacyAgentFiles,
  resolveMirriHome,
  serializeAgentMd,
  userAgentsDir,
  type Scope,
} from '@mirri-ai/agent-core-v2';
import {
  createProfileRequestSchema,
  listProfilesResponseSchema,
  profileEntrySchema,
  updateProfileRequestSchema,
} from '@mirri-ai/protocol';
import {
  AGENT_NAME_MAX_LENGTH,
  AGENT_NAME_PATTERN,
  parseAgentFileText,
} from '@mirri-ai/agent-profile';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
import { z } from 'zod';

import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { ErrorCode } from '../protocol/error-codes';
import { parseActionSuffix } from './action-suffix';

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

/**
 * Validate a profile name before it is used to build a file path. Names are
 * concatenated into `join(agentsDir, `${name}.md`)`, so a non-kebab payload
 * such as `../evil` is a path-traversal write primitive. The pattern mirrors
 * the file-format contract (`AGENT_NAME_PATTERN`, also enforced by
 * `parseAgentFileText` on every discovered file): kebab-case segments of
 * `[a-z0-9]`. The length cap (`AGENT_NAME_MAX_LENGTH`) keeps the resulting
 * filename far from filesystem limits — without it an over-long name fails at
 * the write with a 500 (`ENAMETOOLONG`) instead of a clean 400.
 */
const NAME_PATTERN_MESSAGE =
  'name must be kebab-case (lowercase alphanumeric segments separated by single hyphens, e.g. "code-reviewer")';

const NAME_LENGTH_MESSAGE = `name must be at most ${AGENT_NAME_MAX_LENGTH} characters`;

/**
 * Returns the contract-violation message for `name` (pattern or length), or
 * `undefined` when the name is valid.
 */
function agentNameValidationError(name: string): string | undefined {
  if (!AGENT_NAME_PATTERN.test(name)) {
    return NAME_PATTERN_MESSAGE;
  }
  if (name.length > AGENT_NAME_MAX_LENGTH) {
    return NAME_LENGTH_MESSAGE;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Engine data plane + error mapping
// ---------------------------------------------------------------------------

/**
 * Map the management service's Error2 rejections onto the route layer's
 * envelope codes (engine `agent_profile.*` → REST 40418 / 41307, message
 * verbatim). Returns true when the reply was sent; unexpected errors are
 * left to the caller to rethrow so the global error handler keeps its
 * behavior.
 */
function sendAgentProfileError(
  reply: { send(payload: unknown): unknown },
  req: { id: string },
  error: unknown,
): boolean {
  if (isError2(error)) {
    if (error.code === ErrorCodes.AGENT_PROFILE_NOT_FOUND) {
      reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_NOT_FOUND, error.message, req.id));
      return true;
    }
    if (error.code === ErrorCodes.AGENT_PROFILE_ESSENTIAL) {
      reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_ESSENTIAL, error.message, req.id));
      return true;
    }
  }
  return false;
}

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

/**
 * Read the raw Markdown body of a profile `.md` file (the text after the
 * closing frontmatter fence), or `undefined` when the file does not exist
 * or does not parse. The write channel needs the raw body to discriminate a
 * hand-written full-replacement file from a composed one — the wire
 * template projects both as plain role text.
 */
async function readRawBody(fs: IHostFileSystem, filePath: string): Promise<string | undefined> {
  try {
    const text = await fs.readText(filePath);
    const parsed = parseAgentFileText({ path: filePath, source: 'user', text });
    return parsed.prompt;
  } catch {
    return undefined;
  }
}

/**
 * Whether the raw Markdown body of the profile's `.md` file is composed
 * onto the base reference (the two-shape discriminator, ADR-010 ①: the
 * bare-ref prefix, or exactly the bare ref). `false` when the file does
 * not exist — a builtin row without an override file has no body to
 * preserve.
 */
async function readRawBodyIsComposed(
  fs: IHostFileSystem,
  filePath: string,
): Promise<boolean> {
  const body = await readRawBody(fs, filePath);
  return body !== undefined && isComposedBody(body);
}

export function registerAgentRoutes(app: AgentsRouteHost, core: Scope): void {
  const log = core.accessor.get(ILogService);
  const management = core.accessor.get(IAgentProfileManagementService);
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
      const view = await management.list();
      const items = view.entries;
      reply.send(okEnvelope({ items, skipped: view.skipped.length > 0 ? view.skipped : undefined }, req.id));
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
      const view = await management.list();
      const found = view.entries.find((r) => r.name === name);
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
      reply.send(okEnvelope(found, req.id));
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

      // Path-traversal guard: the name is later concatenated into a file path
      // (`join(agentsDir, `${name}.md`)`) — reject anything that is not a
      // kebab-case name before any filesystem access.
      const nameError = agentNameValidationError(name);
      if (nameError !== undefined) {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, nameError, req.id));
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

      // The .md file format requires a non-empty description
      // (`parseAgentFileText` rejects empty/whitespace strings), so reject it
      // up front instead of writing a file the loader silently skips — the
      // historical "invisible profile" bug where POST /agents succeeded but
      // GET /agents never listed the created profile.
      const description = body['description'] as string | undefined;
      if (description === undefined || description.trim() === '') {
        reply.send(
          errEnvelope(
            ErrorCode.VALIDATION_FAILED,
            'description is required and must be a non-empty string',
            req.id,
          ),
        );
        return;
      }

      // Check if a profile with this name already exists (registry + disk)
      const view = await management.list();
      if (view.entries.some((r) => r.name === name)) {
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
      // SDR-014 ②: a non-empty prompt wraps onto the base reference —
      // custom rows are isomorphic with builtin overrides; an
      // already-composed body passes through (no double wrap). An absent
      // or blank prompt falls to the serializer's bare base reference.
      const requestedPrompt = body['system_prompt_template'] as string | undefined;
      const composedPrompt =
        requestedPrompt !== undefined && requestedPrompt.trim() !== ''
          ? composeOntoBasePrompt(requestedPrompt)
          : undefined;
      const content = serializeAgentMd({
        name,
        description: body['description'] as string | undefined,
        whenToUse: body['when_to_use'] as string | undefined,
        tools: body['tools'] as string[] | undefined,
        disallowedTools: body['disallowed_tools'] as string[] | undefined,
        subagents: body['subagents'] as string[] | undefined,
        defaultModel: body['default_model'] as string | undefined,
        capabilitiesRequired: body['capabilities_required'] as string[] | undefined,
        systemPromptTemplate: composedPrompt,
      });

      await fs.writeText(filePath, content);
      await removeLegacyAgentFiles(fs, bootstrap.homeDir, name, (m) => log.warn(m));
      await management.reload();

      reply.send(
        okEnvelope(
          {
            name,
            description: body['description'] as string | undefined,
            source: 'user',
            builtin: false,
            essential: false,
            enabled: true,
            has_override: false,
            file_path: filePath,
            default_model: body['default_model'] as string | undefined,
            capabilities_required: body['capabilities_required'] as string[] | undefined,
            tools: body['tools'] as string[] | undefined,
            disallowed_tools: body['disallowed_tools'] as string[] | undefined,
            subagents: body['subagents'] as string[] | undefined,
            when_to_use: body['when_to_use'] as string | undefined,
            system_prompt_template: body['system_prompt_template'] as string | undefined,
          },
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

      // Path-traversal guard, same as POST: `name` is later concatenated into
      // `join(agentsDir, `${name}.md`)`. The registry lookup below would
      // already fail-safe with 404 (no parsed profile can carry such a name),
      // but the explicit 400 states the contract before any file write.
      const nameError = agentNameValidationError(name);
      if (nameError !== undefined) {
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, nameError, req.id));
        return;
      }

      // D21 / US-A8: the SYSTEM.md override write — a dedicated field on
      // the essential row only. Non-empty writes (or rewrites) the file;
      // `''` deletes it (reset to default). The write happens BEFORE and
      // independently of the row's own profile merge below (the rust
      // channel's order): an essential-row update may carry BOTH the
      // override and other editable fields, and both must apply.
      const overrideBody = body['base_prompt_template'] as string | undefined;
      if (overrideBody !== undefined) {
        if (name !== DEFAULT_AGENT_PROFILE_NAME) {
          reply.send(
            errEnvelope(
              ErrorCode.VALIDATION_FAILED,
              'base_prompt_template is only valid on the essential profile "agent"',
              req.id,
            ),
          );
          return;
        }
        const bootstrap0 = core.accessor.get(IBootstrapService);
        const fs0 = core.accessor.get(IHostFileSystem);
        const systemMdPath = join(resolveMirriHome(bootstrap0.homeDir), 'SYSTEM.md');
        try {
          if (overrideBody.trim() === '') {
            // The reset (US-A8): remove the file so the default base falls
            // back to the builtin template. A missing file is a legal no-op
            // (an idempotent reset).
            const stat = await fs0.stat(systemMdPath);
            if (stat.isFile) await fs0.remove(systemMdPath);
          } else {
            await fs0.writeText(systemMdPath, overrideBody);
          }
        } catch (error) {
          reply.send(
            errEnvelope(
              ErrorCode.VALIDATION_FAILED,
              `failed to write SYSTEM.md: ${String(error)}`,
              req.id,
            ),
          );
          return;
        }
        await management.reload();
      } else if (name === DEFAULT_AGENT_PROFILE_NAME) {
        // The essential row without the override field keeps the essential
        // refusal: the default base's editable surface is exactly the
        // SYSTEM.md override.
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_ESSENTIAL,
            `Cannot modify the essential profile "${DEFAULT_AGENT_PROFILE_NAME}"`,
            req.id,
          ),
        );
        return;
      }

      const view = await management.list();
      const existing = view.entries.find((r) => r.name === name);
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
      // The .md contract requires a non-empty description (parse-level
      // `requiredNonEmptyString`), so an update that would serialize an empty
      // one is rejected rather than silently coerced: an explicit empty
      // string in the body — or merging into a profile that has no
      // description — would otherwise write a file the loader skips, making
      // the profile disappear from every subsequent read.
      if (mergedDescription.trim() === '') {
        reply.send(
          errEnvelope(
            ErrorCode.VALIDATION_FAILED,
            'description must be a non-empty string',
            req.id,
          ),
        );
        return;
      }
      const mergedWhenToUse = ('when_to_use' in body && body['when_to_use'] !== undefined)
        ? (body['when_to_use'] as string)
        : existing.when_to_use;
      const mergedTools = ('tools' in body && body['tools'] !== undefined)
        ? (body['tools'] as string[])
        : existing.tools ? [...existing.tools] : undefined;
      const mergedDefaultModel = ('default_model' in body && body['default_model'] !== undefined)
        ? (body['default_model'] as string | undefined)
        : existing.default_model;
      const mergedCapabilitiesRequired =
        ('capabilities_required' in body && body['capabilities_required'] !== undefined)
          ? (body['capabilities_required'] as string[] | undefined)
          : existing.capabilities_required
            ? [...existing.capabilities_required]
            : undefined;
      const mergedPrompt = ('system_prompt_template' in body && body['system_prompt_template'] !== undefined)
        ? (body['system_prompt_template'] as string)
        : existing.system_prompt_template;
      const mergedSubagents = ('subagents' in body && body['subagents'] !== undefined)
        ? (body['subagents'] as string[])
        : existing.subagents ? [...existing.subagents] : undefined;
      const mergedDisallowedTools = ('disallowed_tools' in body && body['disallowed_tools'] !== undefined)
        ? (body['disallowed_tools'] as string[])
        : existing.disallowed_tools ? [...existing.disallowed_tools] : undefined;

      // D14: no override declaration key is emitted — the file overrides by
      // virtue of existing; the builtin rows are still distinguishable
      // through `has_override` on the wire.
      const isBuiltinOverride = existing.builtin;

      // SDR-014 ②④: the persisted prompt. A prompt the request or merge
      // carries composes onto the base reference — custom rows are
      // isomorphic with builtin overrides. But a hand-written
      // full-replacement body (no base reference) on a field-only update
      // keeps its replacement semantics verbatim: the wire template of a
      // composed row (stripped role text) is indistinguishable from a
      // replacement body, so the raw file body is read to discriminate.
      // An empty or absent prompt falls to the serializer's bare base
      // reference.
      const promptTouched =
        'system_prompt_template' in body && body['system_prompt_template'] !== undefined;
      const rawBodyIsComposed = await readRawBodyIsComposed(fs, filePath);
      let persistedPrompt: string | undefined;
      if (mergedPrompt !== undefined && mergedPrompt.trim() !== '' &&
          (promptTouched || rawBodyIsComposed || isBuiltinOverride)) {
        // A role text being saved (a composed file's or a builtin's role
        // text carried through the merge): compose it onto the base
        // reference — a builtin without an override keeps its own role
        // text riding the reference (SDR-014 ②).
        persistedPrompt = composeOntoBasePrompt(mergedPrompt);
      } else if (mergedPrompt !== undefined && mergedPrompt.trim() !== '') {
        // A full-replacement file whose prompt is not being touched:
        // preserve the raw body verbatim.
        persistedPrompt = await readRawBody(fs, filePath);
      } else {
        // An explicit clear, or a promptless row: the write lands the
        // bare base reference (undefined → the serializer's fallback).
        persistedPrompt = undefined;
      }

      const content = serializeAgentMd({
        name,
        description: mergedDescription,
        whenToUse: mergedWhenToUse,
        tools: mergedTools,
        disallowedTools: mergedDisallowedTools,
        defaultModel: mergedDefaultModel,
        capabilitiesRequired: mergedCapabilitiesRequired,
        enabled: existing.enabled !== false,
        subagents: mergedSubagents,
        systemPromptTemplate: persistedPrompt,
      });

      await fs.writeText(filePath, content);
      await removeLegacyAgentFiles(fs, bootstrap.homeDir, name, (m) => log.warn(m));
      await management.reload();

      // The fresh row off the reloaded loader — the same shape a GET
      // answers with, so the response never drifts from the persisted
      // state (the essential branch's pattern). `undefined` would mean the
      // write disappeared the profile (a description-empty guard miss) —
      // impossible by construction, so the fallback never carries a
      // half-valid row into a `profileEntrySchema` response.
      const refreshed = await management.list();
      const updated = refreshed.entries.find((r) => r.name === name);
      if (updated === undefined) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_NOT_FOUND,
            `Agent profile "${name}" was not found after update`,
            req.id,
          ),
        );
        return;
      }
      reply.send(okEnvelope(updated, req.id));
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

      const view = await management.list();
      const existing = view.entries.find((r) => r.name === name);
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
      if (existing.builtin && !existing.has_override) {
        reply.send(
          errEnvelope(
            ErrorCode.AGENT_PROFILE_BUILTIN_READONLY,
            `Cannot delete built-in profile "${name}"`,
            req.id,
          ),
        );
        return;
      }

      // Delete the .md file and any legacy .yaml/.yml files
      // No name-pattern guard is needed here: the profile was resolved from
      // the registry above, and every parsed profile name is kebab-case
      // (AGENT_NAME_PATTERN), so a traversal payload can never resolve to an
      // existing profile — the lookup above already failed with 404.
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);
      const filePath = agentFilePath(bootstrap.homeDir, name);
      try {
        await fs.remove(filePath);
      } catch {
        // File may not exist
      }
      await removeLegacyAgentFiles(fs, bootstrap.homeDir, name, (m) => log.warn(m));

      await management.reload();
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

      const view = await management.list();
      const existing = view.entries.find((r) => r.name === name);
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

      // No name-pattern guard is needed here: `name` was resolved from the
      // registry above and every parsed profile name is kebab-case
      // (AGENT_NAME_PATTERN), so a traversal payload never reaches this write
      // site — it fails with 404 at the lookup.
      const bootstrap = core.accessor.get(IBootstrapService);
      const fs = core.accessor.get(IHostFileSystem);

      if (action === 'reset') {
        // Reset = remove the override .md file for builtin profiles
        if (!existing.has_override) {
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
          await fs.remove(agentFilePath(bootstrap.homeDir, name));
        } catch {
          // File may not exist
        }
        await removeLegacyAgentFiles(fs, bootstrap.homeDir, name, (m) => log.warn(m));
        await management.reload();
        reply.send(okEnvelope({ ok: true }, req.id));
        return;
      }

      // enable / disable — the management service's write channel (the same
      // serializeAgentMd rewrite + agents-root write + legacy cleanup +
      // loader reload the REST endpoint applied in-process; the route's
      // essential / not-found guards above already ran, so the service's own
      // re-checks are the same answers).
      try {
        await management.setEnabled(name, action === 'enable');
      } catch (error) {
        if (sendAgentProfileError(reply, req, error)) return;
        throw error;
      }
      reply.send(okEnvelope({ ok: true }, req.id));
    },
  );
  app.post(
    actionRoute.path,
    actionRoute.options,
    actionRoute.handler as Parameters<AgentsRouteHost['post']>[2],
  );
}
