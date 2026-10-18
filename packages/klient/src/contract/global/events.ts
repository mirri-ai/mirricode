/**
 * Klient-level global events — the public, typed, namespaced event surface.
 * Each registration binds a public event name to its underlying source (the
 * `IEventService` bus or one service's `onDid*` emitter) plus the zod schema
 * its payload must satisfy. Consumers never see the engine's `onDid`/`onWill`
 * naming; unknown bus event types are not forwarded.
 */

import { z } from 'zod';

import type { ConfigChangedEvent } from '@mirri-ai/agent-core-v2/app/config/config';
import type { ModelsChangedEvent } from '@mirri-ai/agent-core-v2/kosong/model/model';
import type { ProvidersChangedEvent } from '@mirri-ai/agent-core-v2/kosong/provider/provider';
import type { ReloadSummary } from '@mirri-ai/agent-core-v2/app/plugin/types';
import type { McpServerStatusPayload } from '@mirri-ai/protocol';
import { mcpServerStatusEventSchema } from '@mirri-ai/protocol';

import { stringDeltaSchema } from '../helpers.js';
import type { EventRegistration } from '../types.js';

/** Payload of `event.session.archived` on the global bus. */
export interface SessionArchivedPayload {
  readonly sessionId: string;
}

/** Payload of `session.meta.updated` on the global bus (`agent/rpc/prompt-metadata.ts`). */
export interface SessionMetaUpdatedPayload {
  readonly agentId: string;
  readonly sessionId: string;
  readonly title?: string;
  readonly patch: {
    readonly title?: string;
    readonly isCustomTitle?: boolean;
    readonly lastPrompt?: string;
    readonly model?: string;
    readonly thinking?: string;
  };
}

/** Payload of `event.model_catalog.changed` — the provider-model refresh result shape. */
export type CatalogChangedPayload = {
  readonly changed: Array<{
    readonly provider_id: string;
    readonly provider_name: string;
    readonly added: number;
    readonly removed: number;
  }>;
  readonly unchanged: readonly string[];
  readonly failed: ReadonlyArray<{ readonly provider: string; readonly reason: string }>;
};

/**
 * Payload of `event.mcp.server.status` on the global bus (M7, F-9): the
 * notification `params` — the server status object with its pool
 * attribution and (when the event reports a persisted change) the
 * change kind. Session-level consumers receive the same `server` object
 * nested under the event's envelope.
 */
export interface McpServerStatusEventPayload {
  readonly server: McpServerStatusPayload;
}

/** Public event name → payload type. Keys must stay in sync with `globalEvents`. */
export interface KlientEventPayloads {
  'config.changed': ConfigChangedEvent;
  'config.sectionChanged': ConfigChangedEvent;
  'kosong.providers.changed': ProvidersChangedEvent;
  'kosong.models.changed': ModelsChangedEvent;
  'plugins.reloaded': ReloadSummary;
  'session.archived': SessionArchivedPayload;
  'session.metaUpdated': SessionMetaUpdatedPayload;
  'kosong.changed': CatalogChangedPayload;
  'event.mcp.server.status': McpServerStatusEventPayload;
}

export type KlientEventName = keyof KlientEventPayloads;

const configChangedSchema = z.object({
  domain: z.string(),
  source: z.enum(['load', 'reload', 'set']),
  value: z.unknown(),
  previousValue: z.unknown(),
});

const reloadSummarySchema = z.object({
  added: z.array(z.string()),
  removed: z.array(z.string()),
  errors: z.array(z.object({ id: z.string(), message: z.string() })),
});

const sessionMetaUpdatedSchema = z.object({
  agentId: z.string(),
  sessionId: z.string(),
  title: z.string().optional(),
  patch: z.object({
    title: z.string().optional(),
    isCustomTitle: z.boolean().optional(),
    // The engine patch has never carried lastPrompt on this bus — the
    // TS projector fills it from its own state; the rust engine (P2)
    // carries model/thinking instead. All fields optional, mirroring
    // the session-level metadata patch contract.
    lastPrompt: z.string().optional(),
    model: z.string().optional(),
    thinking: z.string().optional(),
  }),
});

export const catalogChangedSchema = z.object({
  changed: z.array(
    z.object({
      provider_id: z.string(),
      provider_name: z.string(),
      added: z.number(),
      removed: z.number(),
    }),
  ),
  unchanged: z.array(z.string()),
  failed: z.array(z.object({ provider: z.string(), reason: z.string() })),
});

/** Public event name → source binding + payload schema. */
export const globalEvents = {
  'config.changed': {
    kind: 'emitter',
    service: 'configService',
    event: 'onDidChangeConfiguration',
    schema: configChangedSchema,
  },
  'config.sectionChanged': {
    kind: 'emitter',
    service: 'configService',
    event: 'onDidSectionChange',
    schema: configChangedSchema,
  },
  'kosong.providers.changed': {
    kind: 'emitter',
    service: 'providerService',
    event: 'onDidChangeProviders',
    schema: stringDeltaSchema,
  },
  'kosong.models.changed': {
    kind: 'emitter',
    service: 'modelService',
    event: 'onDidChangeModels',
    schema: stringDeltaSchema,
  },
  'plugins.reloaded': {
    kind: 'emitter',
    service: 'pluginService',
    event: 'onDidReload',
    schema: reloadSummarySchema,
  },
  'session.archived': {
    kind: 'bus',
    type: 'event.session.archived',
    schema: z.object({ sessionId: z.string() }),
  },
  'session.metaUpdated': {
    kind: 'bus',
    type: 'session.meta.updated',
    schema: sessionMetaUpdatedSchema,
  },
  'kosong.changed': {
    kind: 'bus',
    type: 'event.model_catalog.changed',
    schema: catalogChangedSchema,
  },
  'event.mcp.server.status': {
    kind: 'bus',
    type: 'event.mcp.server.status',
    // The protocol's event schema minus the in-band `type` literal — the
    // bus carries the payload unwrapped from `{ type, payload }`.
    schema: mcpServerStatusEventSchema.omit({ type: true }),
  },
} satisfies Record<KlientEventName, EventRegistration>;
