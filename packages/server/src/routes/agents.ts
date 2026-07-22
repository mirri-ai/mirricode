/**
 * `/agents` REST routes — CRUD for agent profiles.
 *
 *   GET    /agents                          data: {items: ProfileEntry[]}
 *   GET    /agents/{name}                   data: ProfileEntry
 *   POST   /agents                          data: ProfileEntry
 *   PUT    /agents/{name}                   data: ProfileEntry
 *   DELETE /agents/{name}
 *   POST   /agents/{name}:enable
 *   POST   /agents/{name}:disable
 *
 * Built-in profiles are read-only (reject PUT/DELETE). The essential `agent`
 * profile cannot be disabled.
 */

import {
  ErrorCode,
  createProfileRequestSchema,
  listProfilesResponseSchema,
  profileEntrySchema,
  updateProfileRequestSchema,
} from '@mirri-ai/protocol';
import { IProfileService, type IInstantiationService } from '@mirri-ai/agent-core';
import { z } from 'zod';

import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { parseActionSuffix } from './action-suffix';

/**
 * Convert a wire (snake_case) create/update request body to the
 * camelCase CreateProfileInput expected by ProfileService.
 */
function wireToCreateInput(body: Record<string, unknown>): {
  name: string;
  description?: string;
  extends?: string;
  defaultModel?: string;
  tools?: string[];
  systemPromptTemplate?: string;
  whenToUse?: string;
  promptVars?: Record<string, string>;
  capabilities?: string[];
  capabilitiesRequired?: string[];
} {
  return {
    name: body['name'] as string,
    description: body['description'] as string | undefined,
    extends: body['extends'] as string | undefined,
    defaultModel: body['default_model'] as string | undefined,
    tools: body['tools'] as string[] | undefined,
    systemPromptTemplate: body['system_prompt_template'] as string | undefined,
    whenToUse: body['when_to_use'] as string | undefined,
    promptVars: body['prompt_vars'] as Record<string, string> | undefined,
    capabilities: body['capabilities'] as string[] | undefined,
    capabilitiesRequired: body['capabilities_required'] as string[] | undefined,
  };
}

/**
 * Convert a ProfileEntry (camelCase) to a wire response (snake_case).
 */
function entryToWire(entry: {
  name: string;
  description?: string;
  source: string;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  filePath?: string;
  extends?: string;
  defaultModel?: string;
  tools?: readonly string[];
  whenToUse?: string;
  systemPromptTemplate?: string;
}) {
  return {
    name: entry.name,
    description: entry.description,
    source: entry.source,
    builtin: entry.builtin,
    essential: entry.essential,
    enabled: entry.enabled,
    file_path: entry.filePath,
    extends: entry.extends,
    default_model: entry.defaultModel,
    tools: entry.tools,
    when_to_use: entry.whenToUse,
    system_prompt_template: entry.systemPromptTemplate,
  };
}

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

const nameParamSchema = z.object({
  name: z.string().min(1),
});

export function registerAgentRoutes(
  app: AgentsRouteHost,
  ix: IInstantiationService,
): void {
  // GET /agents -----------------------------------------------------------
  const listRoute = defineRoute(
    {
      method: 'GET',
      path: '/agents',
      success: { data: listProfilesResponseSchema },
      errors: {},
      description: 'List all agent profiles (built-in + custom)',
      tags: ['agents'],
      operationId: 'listAgents',
    },
    async (req, reply) => {
      const items = await ix.invokeFunction((a) => a.get(IProfileService).listProfiles());
      reply.send(okEnvelope({ items: items.map(entryToWire) }, req.id));
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
      const entry = await ix.invokeFunction((a) => a.get(IProfileService).getProfile(name));
      if (entry === undefined) {
        reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_NOT_FOUND, `Agent profile "${name}" was not found`, req.id));
        return;
      }
      reply.send(okEnvelope(entryToWire(entry), req.id));
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
      },
      description: 'Create a custom agent profile',
      tags: ['agents'],
      operationId: 'createAgent',
    },
    async (req, reply) => {
      try {
        const entry = await ix.invokeFunction((a) =>
          a.get(IProfileService).createProfile(wireToCreateInput(req.body)),
        );
        reply.send(okEnvelope(entryToWire(entry), req.id));
      } catch (error) {
        const msg = error instanceof Error ? error.message : String(error);
        if (msg.includes('already exists')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_ALREADY_EXISTS, msg, req.id));
          return;
        }
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, msg, req.id));
      }
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
        [ErrorCode.AGENT_PROFILE_BUILTIN_READONLY]: {},
      },
      description: 'Update a custom agent profile',
      tags: ['agents'],
      operationId: 'updateAgent',
    },
    async (req, reply) => {
      try {
        const { name } = req.params;
        const entry = await ix.invokeFunction((a) =>
          a.get(IProfileService).updateProfile(name, wireToCreateInput(req.body)),
        );
        reply.send(okEnvelope(entryToWire(entry), req.id));
      } catch (error) {
        const msg = error instanceof Error ? error.message : String(error);
        if (msg.includes('not found')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_NOT_FOUND, msg, req.id));
          return;
        }
        if (msg.includes('read-only')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_BUILTIN_READONLY, msg, req.id));
          return;
        }
        reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, msg, req.id));
      }
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
      },
      description: 'Delete a custom agent profile',
      tags: ['agents'],
      operationId: 'deleteAgent',
    },
    async (req, reply) => {
      try {
        const { name } = req.params;
        await ix.invokeFunction((a) => a.get(IProfileService).deleteProfile(name));
        reply.send(okEnvelope({ deleted: true }, req.id));
      } catch (error) {
        const msg = error instanceof Error ? error.message : String(error);
        if (msg.includes('not found')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_NOT_FOUND, msg, req.id));
          return;
        }
        if (msg.includes('read-only')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_BUILTIN_READONLY, msg, req.id));
          return;
        }
        throw error;
      }
    },
  );
  app.delete(
    deleteRoute.path,
    deleteRoute.options,
    deleteRoute.handler as Parameters<AgentsRouteHost['delete']>[2],
  );

  // POST /agents/{name}:enable and :disable ----------------------------
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
      },
      description: 'Enable or disable an agent profile',
      tags: ['agents'],
      operationId: 'toggleAgent',
    },
    async (req, reply) => {
      try {
        const { tail } = req.params;
        const parsed = parseActionSuffix({
          tail,
          allowedActions: ['enable', 'disable'] as const,
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
        const service = ix.invokeFunction((a) => a.get(IProfileService));
        if (parsed.action === 'enable') {
          await service.enableProfile(name);
        } else {
          await service.disableProfile(name);
        }
        reply.send(okEnvelope({ ok: true }, req.id));
      } catch (error) {
        const msg = error instanceof Error ? error.message : String(error);
        if (msg.includes('not found')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_NOT_FOUND, msg, req.id));
          return;
        }
        if (msg.includes('essential')) {
          reply.send(errEnvelope(ErrorCode.AGENT_PROFILE_ESSENTIAL, msg, req.id));
          return;
        }
        throw error;
      }
    },
  );
  app.post(
    actionRoute.path,
    actionRoute.options,
    actionRoute.handler as Parameters<AgentsRouteHost['post']>[2],
  );
}
