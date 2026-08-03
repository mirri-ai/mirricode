/**
 * `subagent` domain — subagent config-section schema, env binding, and
 * timeout / model resolution.
 *
 * Owns the `[subagent]` configuration section (`timeout_ms` on disk) together
 * with the `MIRRI_SUBAGENT_TIMEOUT_MS` env override (precedence: env >
 * config.toml > 2h default). While
 * the env var is set, `stripEnvBoundFields` restores the env-free raw value
 * before persistence, so the override never leaks into `config.toml`. Per-run
 * timeouts resolve through `resolveSubagentTimeoutMs`, and the timeout
 * message renders with `formatSubagentTimeoutDescription`.
 *
 * The model half of the spawn binding is the secondary model (the
 * `[secondary_model]` section on disk): when its
 * experiment is enabled and the model is set, newly spawned subagents bind to
 * it by default instead of inheriting the caller's model, and the
 * `Agent`/`AgentSwarm` tools let the parent model pick per spawn via their
 * `model` parameter. When unset, spawning behavior is unchanged (subagents
 * inherit the caller's model). A recipe with patch fields binds the
 * synthesized derived entry (`SECONDARY_DERIVED_MODEL_ID`); a pointer-only
 * recipe binds the pointed entry directly. `default_effort` is passed as the
 * explicit subagent thinking; without it the subagent resolves thinking
 * naturally (global thinking config → the bound model's default effort)
 * rather than inheriting the caller's level. Both tools resolve spawn
 * bindings through `resolveSubagentBinding`, advertise the pair via
 * `buildSubagentModelDescriptions`, and wrap spawn failures with
 * `wrapSubagentModelError`. The `model` parameter is now always advertised
 * (accepting arbitrary model aliases plus the primary/secondary keywords), so
 * `stripSubagentModelParameter` is no longer called by the tools at runtime
 * but remains exported as a general-purpose utility. Self-registered at module
 * load via `registerConfigSection`.
 */

import { z } from 'zod';

import { Error2, ErrorCodes, isError2 } from '#/errors';
import type { AgentModelPreference } from '#/app/agentProfileCatalog/agentProfileCatalog';
import { isPlainObject } from '#/app/config/toml';
import type { IFlagService } from '#/app/flag/flag';
import type { IModelCatalog, Model } from '#/kosong/model/catalog';
import type { ModelRecord } from '#/kosong/model/model';
import {
  SECONDARY_MODEL_ENV,
  SECONDARY_MODEL_SECTION,
} from '#/app/kosongConfig/configSection';
import {
  SECONDARY_DERIVED_MODEL_ID,
  secondaryModelPatch,
} from '#/app/kosongConfig/secondaryModelOverlay';
import { type SecondaryModelConfig } from '#/app/kosongConfig/configSection';
import {
  type EnvBindings,
  envBindings,
  stripEnvBoundFields,
  type IConfigService,
} from '#/app/config/config';
import { registerConfigSection } from '#/app/config/configSectionContributions';

import { SECONDARY_MODEL_FLAG_ID } from './flag';

export const SUBAGENT_SECTION = 'subagent';

export const SubagentConfigSchema = z.object({
  timeoutMs: z.number().int().min(0).optional(),
});

export type SubagentConfig = z.infer<typeof SubagentConfigSchema>;

export const DEFAULT_SUBAGENT_TIMEOUT_MS = 2 * 60 * 60 * 1000;

export const SUBAGENT_TIMEOUT_ENV = 'MIRRI_SUBAGENT_TIMEOUT_MS';

function parseTimeoutMsEnv(raw: string): number | undefined {
  const parsed = Number(raw);
  return Number.isInteger(parsed) && parsed >= 1 ? parsed : undefined;
}

export const subagentEnvBindings: EnvBindings<SubagentConfig> = envBindings(
  SubagentConfigSchema,
  {
    timeoutMs: { env: SUBAGENT_TIMEOUT_ENV, parse: parseTimeoutMsEnv },
  },
);

export const stripSubagentEnv = stripEnvBoundFields(subagentEnvBindings);

registerConfigSection(SUBAGENT_SECTION, SubagentConfigSchema, {
  defaultValue: { timeoutMs: DEFAULT_SUBAGENT_TIMEOUT_MS },
  env: subagentEnvBindings,
  stripEnv: stripSubagentEnv,
});

export function resolveSubagentTimeoutMs(config: IConfigService): number {
  return (
    config.get<SubagentConfig | undefined>(SUBAGENT_SECTION)?.timeoutMs ??
    DEFAULT_SUBAGENT_TIMEOUT_MS
  );
}

export type SubagentModelChoice = AgentModelPreference;

export function resolveSecondaryModel(
  config: IConfigService,
  flags: IFlagService,
): SecondaryModelConfig | undefined {
  if (!flags.enabled(SECONDARY_MODEL_FLAG_ID)) return undefined;
  return config.get<SecondaryModelConfig | undefined>(SECONDARY_MODEL_SECTION);
}

/**
 * Resolve the model binding for a newly spawned subagent.
 *
 * Three resolution tiers (first match wins):
 * 1. **Explicit alias** — `requested` is a non-empty string that is NOT
 *    `"primary"` or `"secondary"`: resolve it through `IModelCatalog` and
 *    return the catalog id + the alias model's `defaultEffort`. If the
 *    catalog cannot find the alias, returns `undefined` so the caller can
 *    surface an invalid-alias error to the LLM instead of silently falling
 *    back.
 * 2. **Primary/secondary preference** — `requested` is `"primary"` or
 *    `"secondary"` (or `undefined` with a configured secondary model):
 *    the existing `resolveSubagentBinding` primary/secondary logic.
 * 3. **Fallback** — inherit the caller's model alias and thinking level.
 *
 * When `catalog` is omitted, alias resolution (tier 1) is skipped and only
 * primary/secondary/fallback are available. This preserves backward
 * compatibility for call sites that have not been wired up yet.
 */
export function resolveSubagentBinding(
  config: IConfigService,
  flags: IFlagService,
  own: { modelAlias: string; thinkingLevel: string },
  requested?: string,
  catalog?: IModelCatalog,
): { model: string; thinking?: string } | undefined {
  // Tier 1: explicit model alias (not a primary/secondary keyword)
  if (
    requested !== undefined &&
    requested.length > 0 &&
    requested !== 'primary' &&
    requested !== 'secondary'
  ) {
    if (catalog === undefined) {
      // Without a catalog we cannot validate the alias; fall through to
      // primary/secondary resolution so that existing call sites that
      // haven't been wired up yet still work correctly.
      return { model: own.modelAlias, thinking: own.thinkingLevel };
    }
    const resolvedModel = catalog.getById(requested);
    if (resolvedModel === undefined) return undefined;
    return {
      model: resolvedModel.id,
      thinking: resolvedModel.defaultEffort,
    };
  }

  // Tier 2: primary/secondary preference
  const secondary = resolveSecondaryModel(config, flags);
  const preference: AgentModelPreference | undefined =
    requested === 'primary' || requested === 'secondary' ? requested : undefined;
  if (preference !== 'primary' && secondary?.model !== undefined) {
    return {
      model:
        secondaryModelPatch(secondary) === undefined
          ? secondary.model
          : SECONDARY_DERIVED_MODEL_ID,
      thinking: secondary.defaultEffort,
    };
  }

  // Tier 3: inherit caller
  return { model: own.modelAlias, thinking: own.thinkingLevel };
}

/**
 * Build the "Available models" description section injected into the
 * `Agent` / `AgentSwarm` tool descriptions. Includes:
 * - The primary/secondary pair (when a secondary model is configured).
 * - Curated model aliases from the model records (models with a `displayName`).
 *
 * When `catalog` is provided, each curated model's resolved capabilities
 * (vision, video, audio, tool-use) are appended so the LLM can pick a
 * subagent model that supports the input modality it needs.
 *
 * Returns `undefined` when there is nothing to display (no secondary model
 * and no curated catalog entries).
 */
export function buildSubagentModelDescriptions(
  config: IConfigService,
  flags: IFlagService,
  callerModelAlias: string | undefined,
  modelRecords?: Readonly<Record<string, ModelRecord>>,
  catalog?: IModelCatalog,
): string | undefined {
  const secondaryModel = resolveSecondaryModel(config, flags)?.model;
  const hasSecondary = secondaryModel !== undefined && callerModelAlias !== undefined;

  const curated = modelRecords !== undefined ? buildCuratedAliases(modelRecords, catalog) : [];
  if (!hasSecondary && curated.length === 0) return undefined;

  const lines: string[] = ['Available models (pass via model):'];
  if (hasSecondary) {
    lines.push(
      `- secondary: ${secondaryModel} (default) — the configured secondary model; prefer it for routine subagent tasks`,
      `- primary: ${callerModelAlias} — the main model you are running on; use it for hard, quality-sensitive subagent tasks`,
    );
  }
  for (const entry of curated) {
    const ctxKb = Math.round(entry.maxContextSize / 1024);
    const suffix = ctxKb > 0 ? `, ${ctxKb}k context` : '';
    const capHint = entry.capabilities !== undefined ? `, ${formatCapabilityHint(entry.capabilities)}` : '';
    lines.push(`- ${entry.alias} — ${entry.description}${suffix}${capHint}`);
  }
  return lines.join('\n');
}

export interface CuratedModelAlias {
  readonly id: string;
  readonly alias: string;
  readonly description: string;
  readonly maxContextSize: number;
  /**
   * Resolved model capabilities (image_in, video_in, audio_in, tool_use).
   * `undefined` when no catalog is available to resolve them; otherwise a
   * partial record of the capabilities that are `true` (only non-default
   * capabilities are listed to keep the description concise).
   */
  readonly capabilities?: Readonly<Record<string, boolean>>;
}

/**
 * Build the curated model alias list from model records.
 *
 * A model is "curated" when it has a non-empty `description` — the
 * human-authored purpose string the LLM reads to pick a model. Models with
 * no description are hidden from the LLM entirely.
 *
 * Each curated model is listed under its `id` (the `[models.<id>]` config
 * key), which is also the value the LLM uses to reference the model — there
 * is no separate alias list. The `alias` field on the wire type carries the
 * same value as `id` for display.
 *
 * When `catalog` is provided, each curated model's resolved capabilities are
 * attached so the LLM can pick a vision-capable or tool-capable model.
 */
export function buildCuratedAliases(
  models: Readonly<Record<string, ModelRecord>>,
  catalog?: IModelCatalog,
): CuratedModelAlias[] {
  const result: CuratedModelAlias[] = [];
  for (const [id, record] of Object.entries(models)) {
    // `description` is the sole gate for LLM visibility: it is the
    // human-authored purpose string the LLM reads to pick a model. A model
    // with no description is not curated and is hidden — `displayName` is a
    // UI label, not an LLM-facing purpose, so it never substitutes for
    // description here.
    const description = nonEmpty(record.description);
    if (description === undefined) continue;
    // The LLM references a model by its `modelId` (the `[models.<id>]` config
    // key) — there is no separate alias list. `alias` and `id` are the same
    // value; the `alias` field is kept on the wire type for display.
    const maxContextSize = record.maxContextSize ?? 0;
    const capabilities = resolveAliasCapabilities(id, catalog);
    result.push({ id, alias: id, description, maxContextSize, capabilities });
  }
  return result;
}

/**
 * Resolve the notable capabilities of a curated model for hint display.
   * Returns only capabilities that are `true` (image_in, video_in, audio_in,
   * tool_use, thinking) so the hint stays concise. Returns `undefined` when
   * the catalog has no entry for the model or the model has no notable caps.
 */
function resolveAliasCapabilities(
  id: string,
  catalog?: IModelCatalog,
): Readonly<Record<string, boolean>> | undefined {
  if (catalog === undefined) return undefined;
  let model: Model;
  try {
    model = catalog.get(id);
  } catch {
    return undefined;
  }
  const caps = model.capabilities;
  const notable: Record<string, boolean> = {};
  if (caps['image_in']) notable['image_in'] = true;
  if (caps['video_in']) notable['video_in'] = true;
  if (caps['audio_in']) notable['audio_in'] = true;
  if (caps['tool_use']) notable['tool_use'] = true;
  if (caps['thinking']) notable['thinking'] = true;
  return Object.keys(notable).length > 0 ? notable : undefined;
}

/**
 * Human-readable label for a capability flag, used in model descriptions.
 */
const CAPABILITY_LABELS: Readonly<Record<string, string>> = {
  image_in: 'vision',
  video_in: 'video',
  audio_in: 'audio',
  tool_use: 'tool-use',
  thinking: 'thinking',
};

function formatCapabilityHint(caps: Readonly<Record<string, boolean>>): string {
  const labels: string[] = [];
  for (const key of Object.keys(caps)) {
    const label = CAPABILITY_LABELS[key];
    if (label !== undefined) labels.push(label);
  }
  return labels.length > 0 ? `supports ${labels.join(', ')}` : '';
}

/**
 * Validate that a model id the LLM supplied resolves through the catalog.
 * Returns the resolved model id on success, or an error message string on
 * failure. Returns `undefined` when there is nothing to validate (no id
 * given or the id is a primary/secondary keyword handled elsewhere).
 */
export function validateModelAlias(
  alias: string | undefined,
  catalog: IModelCatalog,
  modelRecords: Readonly<Record<string, ModelRecord>>,
): string | { error: string } {
  if (alias === undefined || alias.length === 0) return alias ?? '';
  if (alias === 'primary' || alias === 'secondary') return alias;
  const resolvedModel = catalog.getById(alias);
  if (resolvedModel !== undefined) return resolvedModel.id;
  const curated = buildCuratedAliases(modelRecords);
  const validAliases = curated.map((m) => m.alias);
  const hint = validAliases.length > 0 ? ` Valid options: ${validAliases.join(', ')}.` : '';
  return { error: `Model "${alias}" is not configured.${hint} Leave the model parameter empty to use the default.` };
}

/**
 * Strip the `model` property from a subagent collaboration tool's advertised
 * JSON schema. While the `secondary-model` experiment is off the parameter is
 * a silent no-op, so the schema the model sees (and the args validator
 * compiled from the same advertised schema) drops it entirely — the
 * secondary-model concept never enters the prompt, and a stray `model`
 * argument is rejected instead of silently inheriting the caller's model.
 * Returns the input unchanged when there is no `model` property; otherwise a
 * shallow copy — the input is never mutated, so callers can keep both
 * variants as shared constants.
 */
export function stripSubagentModelParameter(
  parameters: Record<string, unknown>,
): Record<string, unknown> {
  const properties = parameters['properties'];
  if (!isPlainObject(properties) || !('model' in properties)) return parameters;
  const nextProperties = { ...properties };
  delete nextProperties['model'];
  const next: Record<string, unknown> = { ...parameters, properties: nextProperties };
  const required = parameters['required'];
  if (Array.isArray(required) && required.includes('model')) {
    next['required'] = required.filter((entry) => entry !== 'model');
  }
  return next;
}

export function wrapSubagentModelError(
  error: unknown,
  boundModel: string,
  callerModelAlias: string | undefined,
): unknown {
  if (boundModel === callerModelAlias) return error;
  if (!isError2(error) || error.code !== ErrorCodes.CONFIG_INVALID) return error;
  if (error.details?.['model'] !== boundModel) return error;
  const displayModel =
    boundModel === SECONDARY_DERIVED_MODEL_ID
      ? `the derived entry "${SECONDARY_DERIVED_MODEL_ID}"`
      : `"${boundModel}"`;
  return new Error2(
    error.code,
    `${error.message} (secondary model ${displayModel} comes from [secondary_model].model / ${SECONDARY_MODEL_ENV} — check that it names a valid [models] entry)`,
    {
      cause: error,
      name: error.name,
      details: {
        ...error.details,
        secondaryModel: boundModel,
        secondaryModelConfig: {
          section: 'secondaryModel.model',
          environment: SECONDARY_MODEL_ENV,
        },
      },
    },
  );
}

export function formatSubagentTimeoutDescription(ms: number): string {
  if (ms % (60 * 60 * 1000) === 0) {
    const h = ms / (60 * 60 * 1000);
    return `${h} hour${h === 1 ? '' : 's'}`;
  }
  if (ms % (60 * 1000) === 0) {
    const m = ms / (60 * 1000);
    return `${m} minute${m === 1 ? '' : 's'}`;
  }
  if (ms % 1000 === 0) {
    const s = ms / 1000;
    return `${s} second${s === 1 ? '' : 's'}`;
  }
  return `${ms} ms`;
}

function nonEmpty(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() !== '' ? value.trim() : undefined;
}
