/**
 * Unit tests for LLM-aware model selection in subagent tools (B2-L2).
 *
 * Tests the pure functions in `configSection.ts` that implement the
 * two-tier model resolution (explicit alias → inherit caller),
 * invalid-alias error feedback, and curated catalog injection.
 */

import { describe, expect, it } from 'vitest';

import type { IModelCatalog, Model } from '#/kosong/model/catalog';
import type { ModelRecord } from '#/kosong/model/model';
import {
  buildCuratedAliases,
  buildSubagentModelDescriptions,
  resolveSubagentBinding,
  validateModelAlias,
  type CuratedModelAlias,
} from '#/session/subagent/configSection';

// ── Helpers ──────────────────────────────────────────────────────────

function makeCatalog(modelsById: Record<string, Partial<Model>>): IModelCatalog {
  return {
    _serviceBrand: undefined,
    get(id: string): Model {
      const entry = modelsById[id];
      if (entry === undefined) throw new Error(`Model "${id}" not found`);
      return {
        id,
        name: entry.name ?? id,
        protocol: entry.protocol ?? 'openai',
        baseUrl: entry.baseUrl,
        headers: entry.headers ?? {},
        capabilities: entry.capabilities ?? { max_context_tokens: 0 },
        maxContextSize: entry.maxContextSize ?? 128000,
        authProvider: entry.authProvider ?? { getAuth: async () => undefined },
        providerName: entry.providerName ?? 'test',
        alwaysThinking: false,
        ...entry,
      } as Model;
    },
    getById(modelId: string): Model | undefined {
      const entry = modelsById[modelId];
      if (entry === undefined) return undefined;
      return this.get(modelId);
    },
    getRequester: () => {
      throw new Error('not implemented in stub');
    },
    inspect: () => {
      throw new Error('not implemented in stub');
    },
    ping: async () => {
      throw new Error('not implemented in stub');
    },
    listModels: async () => [],
    listProviders: async () => [],
    getProvider: async () => {
      throw new Error('not implemented in stub');
    },
    setDefaultModel: async () => {
      throw new Error('not implemented in stub');
    },
    removeModel: async () => ({ deleted: true }),
  };
}

function makeModelRecords(
  entries: Record<string, Partial<ModelRecord>>,
): Record<string, ModelRecord> {
  const out: Record<string, ModelRecord> = {};
  for (const [id, record] of Object.entries(entries)) {
    out[id] = { ...record };
  }
  return out;
}

// ── resolveSubagentBinding ────────────────────────────────────────────

describe('resolveSubagentBinding', () => {
  const own = { modelAlias: 'provider/main-model', thinkingLevel: 'medium' };

  it('should inherit caller model when no model is requested', () => {
    const result = resolveSubagentBinding(own);
    expect(result).toEqual({ model: 'provider/main-model', thinking: 'medium' });
  });

  it('should resolve a valid model id through the catalog', () => {
    const catalog = makeCatalog({
      'provider/fast-model': {
        name: 'fast-model',
        maxContextSize: 64000,
        defaultEffort: 'low',
      },
    });
    const result = resolveSubagentBinding(own, 'provider/fast-model', catalog);
    expect(result).toEqual({ model: 'provider/fast-model', thinking: 'low' });
  });

  it('should resolve a model alias matching the catalog id directly', () => {
    const catalog = makeCatalog({
      'provider/big-model': {
        name: 'big-model',
        maxContextSize: 200000,
        defaultEffort: 'high',
      },
    });
    const result = resolveSubagentBinding(own, 'provider/big-model', catalog);
    expect(result).toEqual({ model: 'provider/big-model', thinking: 'high' });
  });

  it('should return undefined when an invalid alias is given', () => {
    const catalog = makeCatalog({});
    const result = resolveSubagentBinding(own, 'provider/nonexistent', catalog);
    expect(result).toBeUndefined();
  });

  it('should inherit caller binding when no catalog is provided and an alias is given', () => {
    // Without a catalog, we cannot validate aliases; fall back to inheriting
    const result = resolveSubagentBinding(own, 'some-alias');
    expect(result).toEqual({ model: 'provider/main-model', thinking: 'medium' });
  });

  it('should inherit caller binding when called without requested model or catalog', () => {
    // Legacy call sites that do not pass the catalog preserve the caller binding.
    const result = resolveSubagentBinding(own);
    expect(result).toEqual({ model: 'provider/main-model', thinking: 'medium' });
  });
});

// ── validateModelAlias ────────────────────────────────────────────────

describe('validateModelAlias', () => {
  const catalog = makeCatalog({
    'provider/main-model': { name: 'main-model', maxContextSize: 128000 },
    'provider/fast-model': {
      name: 'fast-model',
      maxContextSize: 64000,
    },
  });
  const records = makeModelRecords({
    'provider/main-model': {
      name: 'main-model',
      description: 'Main model',
      maxContextSize: 128000,
    },
    'provider/fast-model': {
      name: 'fast-model',
      description: 'Fast model',
      maxContextSize: 64000,
    },
  });

  it('should return empty string when no alias is given', () => {
    const result = validateModelAlias(undefined, catalog, records);
    expect(result).toBe('');
  });

  it('should resolve a valid model id', () => {
    const result = validateModelAlias('provider/fast-model', catalog, records);
    expect(result).toBe('provider/fast-model');
  });

  it('should resolve a model id directly', () => {
    const result = validateModelAlias('provider/main-model', catalog, records);
    expect(result).toBe('provider/main-model');
  });

  it('should return error object for an invalid alias with curated hint', () => {
    const result = validateModelAlias('provider/nonexistent', catalog, records);
    expect(result).toEqual({
      error: expect.stringContaining('Model "provider/nonexistent" is not configured'),
    });
    expect((result as { error: string }).error).toContain('Valid options:');
  });

  it('should return error object without curated hint when no curated models', () => {
    const emptyCatalog = makeCatalog({});
    const emptyRecords = makeModelRecords({});
    const result = validateModelAlias('nonexistent', emptyCatalog, emptyRecords);
    expect(result).toEqual({
      error: 'Model "nonexistent" is not configured. Leave the model parameter empty to use the default.',
    });
  });
});

// ── buildCuratedAliases ───────────────────────────────────────────────

describe('buildCuratedAliases', () => {
  it('should return empty array when no models have description', () => {
    const records = makeModelRecords({
      'provider/main': { name: 'main', displayName: 'Main', maxContextSize: 128000 },
    });
    expect(buildCuratedAliases(records)).toEqual([]);
  });

  it('should include models with description', () => {
    const records = makeModelRecords({
      'provider/main': {
        name: 'main',
        description: 'General-purpose coding model',
        maxContextSize: 128000,
      },
    });
    const result = buildCuratedAliases(records);
    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      id: 'provider/main',
      alias: 'provider/main',
      description: 'General-purpose coding model',
      maxContextSize: 128000,
    });
  });

  it('should use the model id as the LLM-facing alias', () => {
    // The LLM references a model by its modelId (the `[models.<id>]` config
    // key). There is no separate alias list — `alias` and `id` are the same.
    const records = makeModelRecords({
      'provider/fast': {
        name: 'fast',
        description: 'Fast model for quick tasks',
        maxContextSize: 64000,
      },
      'provider/custom': {
        description: 'Custom model',
        maxContextSize: 100000,
      },
    });
    const result = buildCuratedAliases(records);
    expect(result).toHaveLength(2);
    expect(result.find((m) => m.id === 'provider/fast')!.alias).toBe('provider/fast');
    expect(result.find((m) => m.id === 'provider/custom')!.alias).toBe('provider/custom');
  });

  it('should default maxContextSize to 0 when not set', () => {
    const records = makeModelRecords({
      'provider/unknown': {
        description: 'Unknown model',
      },
    });
    const result = buildCuratedAliases(records);
    expect(result).toHaveLength(1);
    expect(result[0]!.maxContextSize).toBe(0);
  });

  it('should prefer description over displayName when both are set', () => {
    const records = makeModelRecords({
      'provider/main': {
        displayName: 'Display Label',
        description: 'Human-readable purpose',
        maxContextSize: 128000,
      },
    });
    const result = buildCuratedAliases(records);
    expect(result).toHaveLength(1);
    expect(result[0]!.description).toBe('Human-readable purpose');
  });

  it('should hide models with empty description even when displayName is set', () => {
    // `description` is the sole gate for LLM visibility. An explicitly empty
    // description (e.g. `description = ""` in config.toml) means the user has
    // not provided an LLM-facing purpose, so the model is hidden regardless
    // of `displayName` — displayName is a UI label, not a substitute.
    const records = makeModelRecords({
      'provider/ultra': {
        displayName: 'Ultra Model',
        description: '',
        maxContextSize: 200000,
      },
    });
    expect(buildCuratedAliases(records)).toEqual([]);
  });

  it('should hide models with no description field even when displayName is set', () => {
    const records = makeModelRecords({
      'provider/copilot-ghost': {
        displayName: 'Copilot Ghost',
        maxContextSize: 200000,
      },
    });
    expect(buildCuratedAliases(records)).toEqual([]);
  });

  it('should hide models with neither description nor displayName', () => {
    const records = makeModelRecords({
      'provider/ghost': {
        name: 'ghost',
        maxContextSize: 128000,
      },
    });
    expect(buildCuratedAliases(records)).toEqual([]);
  });

  it('should hide models with empty description and no displayName', () => {
    const records = makeModelRecords({
      'provider/empty': {
        description: '',
        maxContextSize: 128000,
      },
    });
    expect(buildCuratedAliases(records)).toEqual([]);
  });
});

// ── buildSubagentModelDescriptions ────────────────────────────────────

describe('buildSubagentModelDescriptions', () => {
  it('should return undefined when no curated models are provided', () => {
    expect(buildSubagentModelDescriptions()).toBeUndefined();
    expect(buildSubagentModelDescriptions(undefined)).toBeUndefined();
  });

  it('should include curated model ids from model records', () => {
    const records = makeModelRecords({
      'provider/fast': {
        name: 'fast',
        description: 'Fast model for quick tasks',
        maxContextSize: 64000,
      },
    });
    const result = buildSubagentModelDescriptions(records);
    expect(result).toContain('Available models (pass via model):');
    expect(result).toContain('- provider/fast — Fast model for quick tasks, 63k context');
  });

  it('should render curated models when no catalog is provided', () => {
    const records = makeModelRecords({
      'provider/big': {
        description: 'Big context model',
        maxContextSize: 200000,
      },
    });
    const result = buildSubagentModelDescriptions(records);
    expect(result).toContain('- provider/big — Big context model, 195k context');
  });

  // ── capability-aware catalog (L2 enhancement) ──────────────────────

  it('should append vision capability hint when curated model supports image_in', () => {
    const records = makeModelRecords({
      'provider/vision': {
        description: 'Vision-capable model',
        maxContextSize: 128000,
      },
    });
    const catalog = makeCatalog({
      'provider/vision': {
        capabilities: { image_in: true, video_in: false, audio_in: false, thinking: false, tool_use: true, max_context_tokens: 128000 },
      },
    });
    const result = buildSubagentModelDescriptions(records, catalog);
    expect(result).toContain('- provider/vision — Vision-capable model, 125k context, supports vision, tool-use');
  });

  it('should append multiple capability hints when model supports several modalities', () => {
    const records = makeModelRecords({
      'provider/multi': {
        description: 'Multimodal model',
        maxContextSize: 200000,
      },
    });
    const catalog = makeCatalog({
      'provider/multi': {
        capabilities: { image_in: true, video_in: true, audio_in: true, thinking: false, tool_use: true, max_context_tokens: 200000 },
      },
    });
    const result = buildSubagentModelDescriptions(records, catalog);
    expect(result).toContain('supports vision, video, audio, tool-use');
  });

  it('should omit capability hint when model has no notable capabilities', () => {
    const records = makeModelRecords({
      'provider/text': {
        description: 'Text-only model',
        maxContextSize: 128000,
      },
    });
    const catalog = makeCatalog({
      'provider/text': {
        capabilities: { image_in: false, video_in: false, audio_in: false, thinking: false, tool_use: false, max_context_tokens: 128000 },
      },
    });
    const result = buildSubagentModelDescriptions(records, catalog);
    expect(result).toContain('- provider/text — Text-only model, 125k context');
    expect(result).not.toContain('supports');
  });

  it('should omit capability hint when no catalog is provided', () => {
    const records = makeModelRecords({
      'provider/vision': {
        description: 'Vision-capable model',
        maxContextSize: 128000,
      },
    });
    // No catalog argument — capabilities cannot be resolved.
    const result = buildSubagentModelDescriptions(records);
    expect(result).toContain('- provider/vision — Vision-capable model, 125k context');
    expect(result).not.toContain('supports');
  });

  it('should handle catalog missing a model entry gracefully', () => {
    const records = makeModelRecords({
      'provider/missing': {
        description: 'Missing-from-catalog model',
        maxContextSize: 128000,
      },
    });
    // Catalog does not contain 'provider/missing' — get() throws.
    const catalog = makeCatalog({});
    const result = buildSubagentModelDescriptions(records, catalog);
    expect(result).toContain('- provider/missing — Missing-from-catalog model, 125k context');
    expect(result).not.toContain('supports');
  });
});