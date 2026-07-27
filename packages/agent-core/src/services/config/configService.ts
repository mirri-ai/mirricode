import { Disposable, InstantiationType, registerSingleton } from '../../di';
import type { MirriConfig, ProviderConfig } from '../../config';
import { isEnvRef } from '../../config/env-expand';
import type { ConfigResponse, PatchConfigRequest } from '@mirri-ai/protocol';

import { ICoreProcessService } from '../coreProcess/coreProcess';
import { IEventService } from '../event/event';
import { IConfigService } from './config';

export class ConfigService extends Disposable implements IConfigService {
  readonly _serviceBrand: undefined;

  constructor(
    @ICoreProcessService private readonly core: ICoreProcessService,
    @IEventService private readonly eventService: IEventService,
  ) {
    super();
  }

  async get(): Promise<ConfigResponse> {
    const config = await this.core.rpc.getMirriConfig({ reload: true });
    return toConfigResponse(config);
  }

  async set(patch: PatchConfigRequest): Promise<ConfigResponse> {
    const camelPatch = convertKeysSnakeToCamel(patch) as Record<string, unknown>;
    const updated = await this.core.rpc.setMirriConfig(camelPatch);
    const response = toConfigResponse(updated);

    this.eventService.publish({
      type: 'event.config.changed',
      agentId: 'main',
      sessionId: '__global__',
      changedFields: Object.keys(patch),
      config: response,
    });

    return response;
  }
}

function toConfigResponse(config: MirriConfig): ConfigResponse {
  const providers: Record<string, { type: string; base_url?: string; default_model?: string; has_api_key: boolean; api_key_display?: string | null }> = {};
  for (const [providerId, provider] of Object.entries(config.providers ?? {})) {
    providers[providerId] = {
      type: provider.type,
      base_url: provider.baseUrl,
      default_model: provider.defaultModel,
      has_api_key: hasProviderCredential(provider),
      api_key_display: apiKeyDisplay(provider.apiKey),
    };
  }

  return {
    providers,
    default_provider: config.defaultProvider,
    default_model: config.defaultModel,
    models: modelsToWire(config.models),
    thinking: config.thinking,
    plan_mode: config.planMode,
    yolo: config.yolo,
    default_permission_mode: config.defaultPermissionMode,
    default_plan_mode: config.defaultPlanMode,
    permission: config.permission,
    hooks: config.hooks,
    services: config.services,
    merge_all_available_skills: config.mergeAllAvailableSkills,
    extra_skill_dirs: config.extraSkillDirs,
    extra_agent_dirs: config.extraAgentDirs,
    disabled_agents: config.disabledAgents,
    loop_control: config.loopControl,
    background: config.background,
    experimental: config.experimental,
    telemetry: config.telemetry,
    raw: config.raw,
  };
}

function hasProviderCredential(provider: ProviderConfig): boolean {
  if (nonEmpty(provider.apiKey) !== undefined) return true;
  if (provider.oauth !== undefined) return true;
  return false;
}

/**
 * Safe display value for the UI: exposes the raw api_key string only when it
 * is a pure env-var reference (e.g. `${MY_KEY}`) — not a secret. Literal keys
 * return `null` so the UI never receives the actual secret value.
 */
function apiKeyDisplay(apiKey: string | undefined): string | null {
  if (apiKey === undefined) return null;
  return isEnvRef(apiKey) ? apiKey : null;
}

/** Converts the camelCase model alias map to snake_case wire objects so the
 *  REST response matches the WireModelAlias contract (and the Web mapper's
 *  expectations). Providers are already hand-converted above; models were
 *  previously passed through raw, leaving camelCase keys that the client read
 *  as undefined. */
function modelsToWire(
  models: Record<string, unknown> | undefined,
): Record<string, unknown> | undefined {
  if (models === undefined) return undefined;
  const out: Record<string, unknown> = {};
  for (const [id, alias] of Object.entries(models)) {
    out[id] = convertKeysCamelToSnake(alias);
  }
  return out;
}

function convertKeysCamelToSnake(obj: unknown): unknown {
  if (Array.isArray(obj)) {
    return obj.map(convertKeysCamelToSnake);
  }
  if (obj !== null && typeof obj === 'object') {
    const result: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(obj as Record<string, unknown>)) {
      result[camelToSnake(key)] = convertKeysCamelToSnake(value);
    }
    return result;
  }
  return obj;
}

function camelToSnake(str: string): string {
  return str.replaceAll(/([A-Z])/g, (_, ch: string) => `_${ch.toLowerCase()}`);
}

function nonEmpty(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  const trimmed = value.trim();
  return trimmed.length === 0 ? undefined : trimmed;
}

function convertKeysSnakeToCamel(obj: unknown): unknown {
  if (Array.isArray(obj)) {
    return obj.map(convertKeysSnakeToCamel);
  }
  if (obj !== null && typeof obj === 'object') {
    const result: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(obj)) {
      result[snakeToCamel(key)] = convertKeysSnakeToCamel(value);
    }
    return result;
  }
  return obj;
}

function snakeToCamel(str: string): string {
  return str.replaceAll(/_([a-z])/g, (_, ch: string) => ch.toUpperCase());
}

registerSingleton(IConfigService, ConfigService, InstantiationType.Delayed);
