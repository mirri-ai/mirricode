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
 * The model half of a spawn binding is a two-tier resolution: an explicit
 * `model` parameter (an alias that must resolve through the model catalog or
 * the curated alias list) wins, otherwise the subagent inherits the caller's
 * model alias and thinking level. Both the `Agent`/`AgentSwarm` tools resolve
 * spawn bindings through `resolveSubagentBinding`, advertise the curated
 * model choices via `buildSubagentModelDescriptions`, and wrap spawn failures
 * with `wrapSubagentModelError`. The `model` parameter is always advertised,
 * accepting arbitrary configured model aliases. Self-registered at module
 * load via `registerConfigSection`.
 */

import { z } from 'zod';

import { Error2, ErrorCodes, isError2 } from '#/errors';
import type { IModelCatalog, Model } from '#/kosong/model/catalog';
import type { ModelRecord } from '#/kosong/model/model';
import {
  type EnvBindings,
  envBindings,
  stripEnvBoundFields,
  type IConfigService,
} from '#/app/config/config';
import { registerConfigSection } from '#/app/config/configSectionContributions';

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

/**
 * Resolve the model binding for a newly spawned subagent.
 *
 * Two resolution tiers (first match wins):
 * 1. **Explicit alias** — `requested` is a non-empty string: resolve it
 *    through `IModelCatalog` and return the catalog id + the alias model's
 *    `defaultEffort`. If the catalog cannot find the alias, returns
 *    `undefined` so the caller can surface an invalid-alias error to the
 *    LLM instead of silently falling back.
 * 2. **Fallback** — inherit the caller's model alias and thinking level.
 *
 * When `catalog` is omitted, alias resolution is skipped and the caller's
 * binding is inherited. This preserves backward compatibility for call sites
 * that have not been wired up yet.
 */
export function resolveSubagentBinding(
  own: { modelAlias: string; thinkingLevel: string },
  requested?: string,
  catalog?: IModelCatalog,
): { model: string; thinking?: string } | undefined {
  // Tier 1: explicit model alias
  if (requested !== undefined && requested.length > 0) {
    if (catalog === undefined) {
      // Without a catalog we cannot validate the alias; inherit the caller.
      return { model: own.modelAlias, thinking: own.thinkingLevel };
    }
    const resolvedModel = catalog.getById(requested);
    if (resolvedModel === undefined) return undefined;
    return {
      model: resolvedModel.id,
      thinking: resolvedModel.defaultEffort,
    };
  }

  // Tier 2: inherit caller
  return { model: own.modelAlias, thinking: own.thinkingLevel };
}

/**
 * Build the "Available models" description section injected into the
 * `Agent` / `AgentSwarm` tool descriptions: the curated model aliases from
 * the model records (models with a human-authored `description`).
 *
 * When `catalog` is provided, each curated model's resolved capabilities
 * (vision, video, audio, tool-use) are appended so the LLM can pick a
 * subagent model that supports the input modality it needs.
 *
 * Returns `undefined` when there are no curated catalog entries to display.
 */
export function buildSubagentModelDescriptions(
  modelRecords?: Readonly<Record<string, ModelRecord>>,
  catalog?: IModelCatalog,
): string | undefined {
  const curated = modelRecords !== undefined ? buildCuratedAliases(modelRecords, catalog) : [];
  if (curated.length === 0) return undefined;

  const lines: string[] = ['Available models (pass via model):'];
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
 * given).
 */
export function validateModelAlias(
  alias: string | undefined,
  catalog: IModelCatalog,
  modelRecords: Readonly<Record<string, ModelRecord>>,
): string | { error: string } {
  if (alias === undefined || alias.length === 0) return '';
  const resolvedModel = catalog.getById(alias);
  if (resolvedModel !== undefined) return resolvedModel.id;
  const curated = buildCuratedAliases(modelRecords);
  const validAliases = curated.map((m) => m.alias);
  const hint = validAliases.length > 0 ? ` Valid options: ${validAliases.join(', ')}.` : '';
  return { error: `Model "${alias}" is not configured.${hint} Leave the model parameter empty to use the default.` };
}

export function wrapSubagentModelError(
  error: unknown,
  boundModel: string,
  callerModelAlias: string | undefined,
): unknown {
  if (boundModel === callerModelAlias) return error;
  if (!isError2(error) || error.code !== ErrorCodes.CONFIG_INVALID) return error;
  if (error.details?.['model'] !== boundModel) return error;
  return new Error2(
    error.code,
    `${error.message} (model "${boundModel}" — check that it names a valid [models] entry)`,
    {
      cause: error,
      name: error.name,
      details: {
        ...error.details,
        model: boundModel,
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
