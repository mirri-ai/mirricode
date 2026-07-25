import { z } from 'zod';

import {
  ErrorCode,
  addProviderRequestSchema,
  getProviderResponseSchema,
  listCatalogProvidersResponseSchema,
  listModelsResponseSchema,
  listProvidersResponseSchema,
  refreshProviderModelsResponseSchema,
  setDefaultModelResponseSchema,
} from '@mirri-ai/protocol';
import { CatalogUnavailableError, IModelCatalogService, ModelNotFoundError, ProviderNotFoundError, type IInstantiationService } from '@mirri-ai/agent-core';

import { errEnvelope, okEnvelope } from '../envelope';
import { defineRoute } from '../middleware/defineRoute';
import { parseActionSuffix } from './action-suffix';

interface ModelCatalogRouteHost {
  get(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> } | undefined,
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
  delete(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> } | undefined,
    handler: (
      req: { id: string; params: unknown },
      reply: { send(payload: unknown): unknown },
    ) => Promise<void> | void,
  ): unknown;
}

const providerIdParamSchema = z.object({
  provider_id: z.string().min(1),
});

const modelActionTailParamSchema = z.object({
  tail: z.string().min(1),
});

const providerActionTailParamSchema = z.object({
  tail: z.string().min(1),
});

const providerCollectionActionParamSchema = z.object({
  action: z.string().min(1),
});

export function registerModelCatalogRoutes(
  app: ModelCatalogRouteHost,
  ix: IInstantiationService,
): void {
  const listModelsRoute = defineRoute(
    {
      method: 'GET',
      path: '/models',
      success: { data: listModelsResponseSchema },
      description: 'List configured model aliases',
      tags: ['models'],
    },
    async (req, reply) => {
      const items = await ix.invokeFunction((a) =>
        a.get(IModelCatalogService).listModels(),
      );
      reply.send(okEnvelope({ items }, req.id));
    },
  );
  app.get(
    listModelsRoute.path,
    listModelsRoute.options,
    listModelsRoute.handler as Parameters<ModelCatalogRouteHost['get']>[2],
  );

  const setDefaultModelRoute = defineRoute(
    {
      method: 'POST',
      path: '/models/{tail}',
      params: modelActionTailParamSchema,
      success: { data: setDefaultModelResponseSchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.MODEL_NOT_FOUND]: {},
      },
      description: 'Set the global default model alias',
      tags: ['models'],
      operationId: 'setDefaultModel',
    },
    async (req, reply) => {
      try {
        const { tail } = req.params;
        const parsed = parseActionSuffix({
          tail,
          allowedActions: ['set_default', 'delete'] as const,
          resourceLabel: 'model',
        });
        if (parsed.kind !== 'action') {
          const message = parsed.kind === 'invalid'
            ? parsed.reason
            : `unsupported action: ${tail}`;
          reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, message, req.id));
          return;
        }
        if (parsed.action === 'delete') {
          await ix.invokeFunction((a) =>
            a.get(IModelCatalogService).removeModel(parsed.id),
          );
          reply.send(okEnvelope({ deleted: true }, req.id));
          return;
        }
        const result = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).setDefaultModel(parsed.id),
        );
        reply.send(okEnvelope(result, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.post(
    setDefaultModelRoute.path,
    setDefaultModelRoute.options,
    setDefaultModelRoute.handler as Parameters<ModelCatalogRouteHost['post']>[2],
  );

  const listProvidersRoute = defineRoute(
    {
      method: 'GET',
      path: '/providers',
      success: { data: listProvidersResponseSchema },
      description: 'List configured providers',
      tags: ['providers'],
    },
    async (req, reply) => {
      const items = await ix.invokeFunction((a) =>
        a.get(IModelCatalogService).listProviders(),
      );
      reply.send(okEnvelope({ items }, req.id));
    },
  );
  app.get(
    listProvidersRoute.path,
    listProvidersRoute.options,
    listProvidersRoute.handler as Parameters<ModelCatalogRouteHost['get']>[2],
  );

  // Collection-level refresh actions share a single `/providers:<action>` route
  // because Fastify's router collapses every one-segment `/providers:xxx` path
  // onto one parameter slot (so `/providers:refresh` and `/providers:refresh_oauth`
  // cannot be registered separately). We dispatch on the captured action instead.
  const refreshProvidersRoute = defineRoute(
    {
      method: 'POST',
      path: '/providers:action',
      params: providerCollectionActionParamSchema,
      success: { data: refreshProviderModelsResponseSchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
      },
      description:
        'Refresh provider model metadata. Use `:refresh` for all providers or `:refresh_oauth` for OAuth-backed providers only.',
      tags: ['providers'],
      operationId: 'refreshProviders',
    },
    async (req, reply) => {
      const raw = req.params.action;
      const action = raw.startsWith(':') ? raw.slice(1) : raw;
      if (action === 'refresh_oauth') {
        const result = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).refreshOAuthProviderModels(),
        );
        reply.send(okEnvelope(result, req.id));
        return;
      }
      if (action === 'refresh') {
        const result = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).refreshProviderModels({ scope: 'all' }),
        );
        reply.send(okEnvelope(result, req.id));
        return;
      }
      reply.send(
        errEnvelope(ErrorCode.VALIDATION_FAILED, `unsupported action: ${raw}`, req.id),
      );
    },
  );
  app.post(
    refreshProvidersRoute.path,
    refreshProvidersRoute.options,
    refreshProvidersRoute.handler as Parameters<ModelCatalogRouteHost['post']>[2],
  );

  const addProviderRoute = defineRoute(
    {
      method: 'POST',
      path: '/providers',
      body: addProviderRequestSchema,
      success: { data: getProviderResponseSchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.PROVIDER_NOT_FOUND]: {},
      },
      description: 'Add a new provider configuration',
      tags: ['providers'],
      operationId: 'addProvider',
    },
    async (req, reply) => {
      try {
        const body = req.body as z.infer<typeof addProviderRequestSchema>;
        const provider = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).addProvider({
            type: body.type,
            apiKey: body.api_key,
            baseUrl: body.base_url,
            defaultModel: body.default_model,
          }),
        );
        reply.send(okEnvelope(provider, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.post(
    addProviderRoute.path,
    addProviderRoute.options,
    addProviderRoute.handler as Parameters<ModelCatalogRouteHost['post']>[2],
  );

  const refreshProviderRoute = defineRoute(
    {
      method: 'POST',
      path: '/providers/{tail}',
      params: providerActionTailParamSchema,
      success: { data: refreshProviderModelsResponseSchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.PROVIDER_NOT_FOUND]: {},
      },
      description: 'Refresh model metadata for a single provider',
      tags: ['providers'],
      operationId: 'refreshProvider',
    },
    async (req, reply) => {
      try {
        const { tail } = req.params;
        const parsed = parseActionSuffix({
          tail,
          allowedActions: ['refresh'] as const,
          resourceLabel: 'provider',
        });
        if (parsed.kind !== 'action') {
          const message = parsed.kind === 'invalid'
            ? parsed.reason
            : `unsupported action: ${tail}`;
          reply.send(errEnvelope(ErrorCode.VALIDATION_FAILED, message, req.id));
          return;
        }
        const result = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).refreshProviderModels({ providerId: parsed.id }),
        );
        reply.send(okEnvelope(result, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.post(
    refreshProviderRoute.path,
    refreshProviderRoute.options,
    refreshProviderRoute.handler as Parameters<ModelCatalogRouteHost['post']>[2],
  );

  const getProviderRoute = defineRoute(
    {
      method: 'GET',
      path: '/providers/{provider_id}',
      params: providerIdParamSchema,
      success: { data: getProviderResponseSchema },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.PROVIDER_NOT_FOUND]: {},
      },
      description: 'Get a configured provider by ID',
      tags: ['providers'],
    },
    async (req, reply) => {
      try {
        const { provider_id } = req.params;
        const provider = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).getProvider(provider_id),
        );
        reply.send(okEnvelope(provider, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.get(
    getProviderRoute.path,
    getProviderRoute.options,
    getProviderRoute.handler as Parameters<ModelCatalogRouteHost['get']>[2],
  );

  const deleteProviderRoute = defineRoute(
    {
      method: 'DELETE',
      path: '/providers/{provider_id}',
      params: providerIdParamSchema,
      success: { data: z.object({ deleted: z.boolean() }) },
      errors: {
        [ErrorCode.PROVIDER_NOT_FOUND]: {},
      },
      description: 'Delete a provider and its associated models',
      tags: ['providers'],
      operationId: 'deleteProvider',
    },
    async (req, reply) => {
      try {
        const { provider_id } = req.params;
        await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).removeProvider(provider_id),
        );
        reply.send(okEnvelope({ deleted: true }, req.id));
      } catch (error) {
        sendMappedError(reply, req.id, error);
      }
    },
  );
  app.delete(
    deleteProviderRoute.path,
    deleteProviderRoute.options,
    deleteProviderRoute.handler as Parameters<ModelCatalogRouteHost['delete']>[2],
  );

  const listCatalogProvidersRoute = defineRoute(
    {
      method: 'GET',
      path: '/catalog/providers',
      success: { data: listCatalogProvidersResponseSchema },
      errors: {
        [ErrorCode.CATALOG_UNAVAILABLE]: {},
      },
      description: 'Browse available providers from the remote model catalog',
      tags: ['catalog'],
      operationId: 'listCatalogProviders',
    },
    async (req, reply) => {
      try {
        const items = await ix.invokeFunction((a) =>
          a.get(IModelCatalogService).listCatalogProviders(),
        );
        reply.send(okEnvelope({ items }, req.id));
      } catch (error) {
        if (error instanceof CatalogUnavailableError) {
          reply.send(errEnvelope(ErrorCode.CATALOG_UNAVAILABLE, error.message, req.id));
          return;
        }
        throw error;
      }
    },
  );
  app.get(
    listCatalogProvidersRoute.path,
    listCatalogProvidersRoute.options,
    listCatalogProvidersRoute.handler as Parameters<ModelCatalogRouteHost['get']>[2],
  );
}

function sendMappedError(
  reply: { send(payload: unknown): unknown },
  requestId: string,
  err: unknown,
): void {
  if (err instanceof ProviderNotFoundError) {
    const error = err;
    reply.send(errEnvelope(ErrorCode.PROVIDER_NOT_FOUND, error.message, requestId));
    return;
  }
  if (err instanceof ModelNotFoundError) {
    const error = err;
    reply.send(errEnvelope(ErrorCode.MODEL_NOT_FOUND, error.message, requestId));
    return;
  }
  throw err;
}
