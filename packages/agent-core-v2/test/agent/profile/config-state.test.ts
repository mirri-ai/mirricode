import { emptyUsage } from '#/kosong/contract/usage';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { IAgentLLMRequesterService } from '#/agent/llmRequester/llmRequester';
import { IAgentProfileService } from '#/agent/profile/profile';
import type { ModelRecord } from '#/kosong/model/model';
import {
  configServices,
  createTestAgent,
  llmGenerateServices,
  modelProviderOptionServices,
  telemetryServices,
  type TestAgentContext,
} from '../../harness';
import { recordingTelemetry, type TelemetryRecord } from '../../app/telemetry/stubs';

type TestMirriConfig = ReturnType<Parameters<typeof configServices>[0]>;
type TestProtocolModelConfig = NonNullable<TestMirriConfig['models']>[string] &
  Pick<ModelRecord, 'protocol'>;
type GenerateFn = Parameters<typeof llmGenerateServices>[0];

function defaultGenerate(): ReturnType<GenerateFn> {
  throw new Error('generate should not be called');
}

describe('ConfigState model capabilities', () => {
  let ctx: TestAgentContext;
  let profile: IAgentProfileService;
  let requester: IAgentLLMRequesterService;
  let kimiConfig: TestMirriConfig;
  let generate: GenerateFn;
  let records: TelemetryRecord[];

  beforeEach(() => {
    kimiConfig = {
      providers: {
        'test-provider': { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
      },
      models: {
        'mock-model': {
          provider: 'test-provider',
          model: 'mock-model',
          maxContextSize: 1_000_000,
          capabilities: [],
        },
      },
      defaultProvider: 'test-provider',
      defaultModel: 'mock-model',
    };
    generate = defaultGenerate;
    records = [];
    ctx = createTestAgent(
      configServices(() => kimiConfig),
      llmGenerateServices((...args) => generate(...args)),
      telemetryServices(recordingTelemetry(records)),
    );
    profile = ctx.get(IAgentProfileService);
    requester = ctx.get(IAgentLLMRequesterService);
  });

  afterEach(async () => {
    try {
      await ctx.expectResumeMatches();
    } finally {
      await ctx.dispose();
    }
  });

  it('computes provider and model capabilities from config metadata', () => {
    kimiConfig = {
      providers: {
        kimi: {
          type: 'kimi',
          apiKey: 'test-key',
          baseUrl: 'https://api.example.test/v1',
        },
      },
      models: {
        'mirri-code/kimi-for-coding': {
          provider: 'kimi',
          model: 'kimi-for-coding',
          maxContextSize: 1_000_000,
          supportEfforts: ['low', 'high'],
          capabilities: ['image_in', 'video_in', 'thinking', 'tool_use'],
        },
      },
    };

    profile.update({ modelAlias: 'mirri-code/kimi-for-coding' });

    expect(profile.getModel()).toBe('mirri-code/kimi-for-coding');
    expect(ctx.modelResolver.get('mirri-code/kimi-for-coding').name).toBe('kimi-for-coding');
    expect(profile.getModelCapabilities()).toMatchObject({
      image_in: true,
      video_in: true,
      audio_in: false,
      thinking: true,
      tool_use: true,
      max_context_tokens: 1_000_000,
    });
  });

  it('republishes the model status slice on demand', () => {
    kimiConfig = {
      providers: {
        kimi: {
          type: 'kimi',
          apiKey: 'test-key',
          baseUrl: 'https://api.example.test/v1',
        },
      },
      models: {
        'mirri-code/kimi-for-coding': {
          provider: 'kimi',
          model: 'kimi-for-coding',
          maxContextSize: 1_000_000,
          supportEfforts: ['low', 'high'],
        },
      },
    };
    profile.update({ modelAlias: 'mirri-code/kimi-for-coding' });
    const before = ctx.allEvents.filter((entry) => entry.event === 'agent.status.updated').length;

    profile.republishStatus();

    const statuses = ctx.allEvents.filter((entry) => entry.event === 'agent.status.updated');
    expect(statuses).toHaveLength(before + 1);
    expect(statuses.at(-1)?.args).toMatchObject({
      model: 'mirri-code/kimi-for-coding',
      maxContextTokens: 1_000_000,
    });
  });

  it('tracks thinking_toggle with the effort payload when effort changes', () => {
    kimiConfig = {
      providers: {
        kimi: {
          type: 'kimi',
          apiKey: 'test-key',
          baseUrl: 'https://api.example.test/v1',
        },
      },
      models: {
        'mirri-code/kimi-for-coding': {
          provider: 'kimi',
          model: 'kimi-for-coding',
          maxContextSize: 1_000_000,
          capabilities: ['thinking'],
          supportEfforts: ['low', 'high'],
        },
      },
    };
    profile.update({ modelAlias: 'mirri-code/kimi-for-coding' });
    profile.setThinking('off');
    records.length = 0;

    profile.setThinking('low');

    expect(records).toContainEqual({
      event: 'thinking_toggle',
      properties: { agent_id: 'main', enabled: true, effort: 'low', from: 'off' },
    });
  });

  it('does not infer Kimi capabilities from the provider catalogue', () => {
    kimiConfig = {
      providers: {
        kimi: {
          type: 'kimi',
          apiKey: 'test-key',
          baseUrl: 'https://api.example.test/v1',
        },
      },
      models: {
        'mirri-code': {
          provider: 'kimi',
          model: 'mirri-code',
          maxContextSize: 128_000,
        },
      },
    };

    profile.update({ modelAlias: 'mirri-code' });

    expect(profile.getModelCapabilities()).toMatchObject({
      image_in: false,
      video_in: false,
      audio_in: false,
      max_context_tokens: 128_000,
    });
  });

  it('uses model max output size as the LLM completion cap', async () => {
    let requestMaxTokens: unknown;
    kimiConfig = {
      providers: {
        deepseek: {
          type: 'openai',
          apiKey: 'test-key',
          baseUrl: 'https://api.deepseek.example/v1',
        },
      },
      models: {
        'deepseek/deepseek-v4-flash': {
          provider: 'deepseek',
          model: 'deepseek-v4-flash',
          maxContextSize: 1_000_000,
          maxOutputSize: 384_000,
        },
      },
    };
    generate = async (_provider, _systemPrompt, _tools, _history, _callbacks, options) => {
      // The per-turn completion budget arrives as a GenerateOptions intent
      // (the morph-era baked `modelParameters.max_tokens` is gone).
      requestMaxTokens = options?.maxCompletionTokens;
      return {
        id: 'response-1',
        message: { role: 'assistant', content: [], toolCalls: [] },
        usage: emptyUsage(),
        finishReason: 'completed',
        rawFinishReason: 'stop',
      };
    };

    profile.update({
      modelAlias: 'deepseek/deepseek-v4-flash',
      systemPrompt: 'system',
      thinkingLevel: 'off',
    });
    await requester.request({}, undefined, new AbortController().signal);

    expect(requestMaxTokens).toBe(384000);
  });
});

describe('ConfigState prompt cache hint', () => {
  let ctx: TestAgentContext;
  let profile: IAgentProfileService;
  let kimiConfig: TestMirriConfig;

  beforeEach(() => {
    kimiConfig = {
      providers: {
        kimi: {
          type: 'kimi',
          apiKey: 'test-key',
          baseUrl: 'https://api.example.test/v1',
        },
        'test-provider': { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
      },
      models: {
        'mirri-code': {
          provider: 'kimi',
          model: 'mirri-code',
          maxContextSize: 128_000,
        },
        'mock-model': {
          provider: 'test-provider',
          model: 'mock-model',
          maxContextSize: 1_000_000,
          capabilities: [],
        },
      },
      defaultProvider: 'kimi',
      defaultModel: 'mirri-code',
    };
    ctx = createTestAgent(
      configServices(() => kimiConfig),
      modelProviderOptionServices({ promptCacheKey: 'session-test' }),
    );
    profile = ctx.get(IAgentProfileService);
  });

  afterEach(async () => {
    try {
      await ctx.expectResumeMatches();
    } finally {
      await ctx.dispose();
    }
  });

  it('uses session id as a provider prompt cache hint without storing it on Agent', () => {
    profile.update({ modelAlias: 'mirri-code' });

    // Kimi is no longer a protocol: the vendor resolves to its `openai` base
    // while keeping `kimi` as the provider type.
    const model = ctx.modelResolver.get('mirri-code');
    expect(model.protocol).toBe('openai');
    expect(model.providerType).toBe('kimi');
    expect('sessionId' in ctx).toBe(false);
  });
});

describe('ConfigState thinking clamp for always-thinking models', () => {
  let ctx: TestAgentContext;
  let profile: IAgentProfileService;
  let requester: IAgentLLMRequesterService;
  let kimiConfig: TestMirriConfig;
  let capturedThinking: unknown;

  beforeEach(() => {
    kimiConfig = {
      providers: {
        kimi: { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
        'test-provider': { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
      },
      models: {
        'mock-model': {
          provider: 'test-provider',
          model: 'mock-model',
          maxContextSize: 1_000_000,
          capabilities: [],
        },
        'mirri-code/deep': {
          provider: 'kimi',
          model: 'kimi-deep-coder',
          maxContextSize: 128_000,
          capabilities: ['thinking', 'always_thinking', 'tool_use'],
          supportEfforts: ['low', 'high', 'max'],
        },
        'mirri-code/toggle': {
          provider: 'kimi',
          model: 'kimi-for-coding',
          maxContextSize: 128_000,
          capabilities: ['thinking'],
        },
        'mirri-code/custom': {
          provider: 'kimi',
          model: 'kimi-custom-coder',
          maxContextSize: 128_000,
          capabilities: ['thinking'],
          supportEfforts: ['low', 'medium', 'max'],
          defaultEffort: 'max',
        },
        'mirri-code/ultra': {
          provider: 'kimi',
          model: 'kimi-ultra',
          maxContextSize: 128_000,
          capabilities: ['thinking'],
          supportEfforts: ['low', 'high', 'ultra'],
          defaultEffort: 'ultra',
        },
        'mirri-code/compatible': {
          provider: 'kimi',
          protocol: 'anthropic',
          model: 'compatible-model',
          maxContextSize: 128_000,
          capabilities: ['thinking', 'always_thinking'],
          supportEfforts: ['max'],
          defaultEffort: 'max',
        } as TestProtocolModelConfig,
      },
      defaultProvider: 'kimi',
      defaultModel: 'mirri-code/deep',
    };
    capturedThinking = undefined;
    ctx = createTestAgent(
      configServices(() => kimiConfig),
      llmGenerateServices(async (_provider, _systemPrompt, _tools, _history, _callbacks, options) => {
        // The per-turn thinking intent (effort + keep) — the replacement for
        // the morph-era baked `_generationKwargs.extra_body.thinking`.
        capturedThinking = options?.thinking;
        return {
          id: 'response-1',
          message: { role: 'assistant', content: [], toolCalls: [] },
          usage: emptyUsage(),
          finishReason: 'completed',
          rawFinishReason: 'stop',
        };
      }),
    );
    profile = ctx.get(IAgentProfileService);
    requester = ctx.get(IAgentLLMRequesterService);
  });

  afterEach(async () => {
    try {
      await ctx.expectResumeMatches();
    } finally {
      await ctx.dispose();
    }
  });

  it('clamps thinkingLevel off to the configured effort', () => {
    profile.update({ modelAlias: 'mirri-code/deep', thinkingLevel: 'off' });

    expect(profile.data().thinkingLevel).toBe('high');
  });

  it('sends the clamped thinking effort in the per-turn intent after thinking was set off', async () => {
    profile.update({ modelAlias: 'mirri-code/deep', thinkingLevel: 'off' });

    await requester.request({}, undefined, new AbortController().signal);

    // The always-thinking clamp turns 'off' into the model default ('high');
    // encoding it as `extra_body.thinking: {type:'enabled'}` is the Kimi
    // dialect trait's job (`kimiOpenAITrait.withThinking`).
    expect(capturedThinking).toMatchObject({ effort: 'high' });
  });

  it('keeps thinking off working for toggleable models', () => {
    profile.update({ modelAlias: 'mirri-code/toggle', thinkingLevel: 'off' });

    expect(profile.data().thinkingLevel).toBe('off');
  });

  it('resolves an explicit on request to the model default effort', () => {
    profile.update({ modelAlias: 'mirri-code/custom', thinkingLevel: 'on' });

    expect(profile.data().thinkingLevel).toBe('max');
  });

  it('re-clamps when switching to an always-on model after thinking was off', () => {
    profile.update({ modelAlias: 'mirri-code/toggle', thinkingLevel: 'off' });
    expect(profile.data().thinkingLevel).toBe('off');

    profile.update({ modelAlias: 'mirri-code/deep' });
    expect(profile.data().thinkingLevel).toBe('high');
  });

  it('falls back to the target default when a model switch carries an unsupported effort', () => {
    profile.update({ modelAlias: 'mirri-code/ultra', thinkingLevel: 'ultra' });

    profile.update({ modelAlias: 'mirri-code/custom' });

    expect(profile.data().thinkingLevel).toBe('max');
  });

  it('projects an inherited concrete effort to on when switching to a boolean model', () => {
    profile.update({ modelAlias: 'mirri-code/ultra', thinkingLevel: 'ultra' });

    profile.update({ modelAlias: 'mirri-code/toggle' });

    expect(profile.data().thinkingLevel).toBe('on');
  });

  it('rejects an unsupported effort explicitly set on the current Kimi model', () => {
    profile.update({ modelAlias: 'mirri-code/custom' });

    expect(() => {
      profile.setThinking('ultra');
    }).toThrow(
      'Thinking effort "ultra" is not supported by model "mirri-code/custom". Supported efforts: off, low, medium, max.',
    );
  });

  it.each([
    [' HIGH ', 'high'],
    ['OFF', 'off'],
  ])('normalizes runtime effort %j to %s before validation', (input, expected) => {
    profile.update({ modelAlias: 'mirri-code/ultra' });

    profile.setThinking(input);

    expect(profile.data().thinkingLevel).toBe(expected);
  });

  it('uses the model default when the runtime effort is blank', () => {
    profile.update({ modelAlias: 'mirri-code/custom', thinkingLevel: 'low' });

    profile.setThinking('   ');

    expect(profile.data().thinkingLevel).toBe('max');
  });

  it('preserves unlisted efforts with a warning for Kimi-managed Anthropic models', () => {
    profile.update({ modelAlias: 'mirri-code/compatible', thinkingLevel: 'max' });

    expect(() => {
      profile.setThinking('high');
    }).not.toThrow();
    expect(profile.data().thinkingLevel).toBe('high');
    expect(ctx.allEvents).toContainEqual({
      type: '[rpc]',
      event: 'warning',
      args: {
        code: 'anthropic-thinking-effort-not-listed',
        message:
          'Thinking effort "high" is not listed for model "compatible-model" (known: max). The configured value will be sent unchanged to the Anthropic-compatible backend.',
      },
    });
  });

  it('clamps off to the model default for always-on models, on any transport', () => {
    // A model declared always-on never resolves to off: the clamp turns the
    // request into the model default ('max') instead of sending a dishonest
    // off upstream. (The always-on warning path remains as a defensive layer
    // for off values that bypass resolution.)
    profile.update({ modelAlias: 'mirri-code/compatible', thinkingLevel: 'max' });

    expect(() => {
      profile.setThinking('off');
    }).not.toThrow();
    expect(profile.data().thinkingLevel).toBe('max');
  });
});

describe('ConfigState.provider applies global MIRRICODE_MODEL_* request config', () => {
  let ctx: TestAgentContext | undefined;
  let profile: IAgentProfileService;
  let requester: IAgentLLMRequesterService;
  let kimiConfig: TestMirriConfig;
  let capturedProvider: unknown;
  let capturedOptions: Parameters<GenerateFn>[5];

  beforeEach(() => {
    kimiConfig = {
      providers: {
        kimi: { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
        'test-provider': { type: 'kimi', apiKey: 'test-key', baseUrl: 'https://api.example.test/v1' },
      },
      models: {
        'mock-model': {
          provider: 'test-provider',
          model: 'mock-model',
          maxContextSize: 1_000_000,
          capabilities: [],
        },
        'mirri-code': {
          provider: 'kimi',
          model: 'mirri-code',
          maxContextSize: 128_000,
          capabilities: ['thinking'],
        },
        'mirri-code-anthropic': {
          provider: 'kimi',
          protocol: 'anthropic',
          model: 'mirri-code-anthropic',
          maxContextSize: 128_000,
          capabilities: ['thinking'],
          supportEfforts: ['low', 'high'],
        } as TestProtocolModelConfig,
      },
      defaultProvider: 'kimi',
      defaultModel: 'mirri-code',
    };
    capturedProvider = undefined;
  });

  afterEach(async () => {
    try {
      await ctx?.expectResumeMatches();
    } finally {
      await ctx?.dispose();
      ctx = undefined;
      vi.unstubAllEnvs();
    }
  });

  function createAgentWithEnv(): void {
    ctx = createTestAgent(
      configServices(() => kimiConfig),
      llmGenerateServices(async (provider, _systemPrompt, _tools, _history, _callbacks, options) => {
        capturedProvider = provider;
        capturedOptions = options;
        return {
          id: 'response-1',
          message: { role: 'assistant', content: [], toolCalls: [] },
          usage: emptyUsage(),
          finishReason: 'completed',
          rawFinishReason: 'stop',
        };
      }),
    );
    profile = ctx.get(IAgentProfileService);
    requester = ctx.get(IAgentLLMRequesterService);
  }

  it('injects MIRRICODE_MODEL_TEMPERATURE into the per-turn sampling intent (the compaction request also uses)', async () => {
    vi.stubEnv('MIRRICODE_MODEL_TEMPERATURE', '0.3');
    createAgentWithEnv();

    profile.update({ modelAlias: 'mirri-code' });
    await requester.request({}, undefined, new AbortController().signal);

    // The env override lands in `modelOverrides.temperature`, which the
    // profile folds into the dialect-free sampling intent (the morph-era
    // baked `_generationKwargs.temperature` is gone); the Kimi dialect encodes
    // it as the wire `temperature` field.
    expect(capturedOptions?.sampling).toMatchObject({
      temperature: 0.3,
    });
  });

  it('injects MIRRICODE_MODEL_THINKING_KEEP into the per-turn thinking intent when thinking is on (so compaction keeps it)', async () => {
    vi.stubEnv('MIRRICODE_MODEL_THINKING_KEEP', 'all');
    createAgentWithEnv();

    profile.update({ modelAlias: 'mirri-code', thinkingLevel: 'high' });
    await requester.request({}, undefined, new AbortController().signal);

    // The model is boolean-thinking (no supportEfforts), so 'high' resolves
    // to 'on'; the env keep override rides the same thinking intent.
    expect(capturedOptions?.thinking).toMatchObject({ effort: 'on', keep: 'all' });
  });

  it('does NOT inject thinking.keep into the per-turn intent when thinking is off', async () => {
    vi.stubEnv('MIRRICODE_MODEL_THINKING_KEEP', 'all');
    createAgentWithEnv();

    profile.update({ modelAlias: 'mirri-code', thinkingLevel: 'off' });
    await requester.request({}, undefined, new AbortController().signal);

    expect(capturedOptions?.thinking?.effort).toBe('off');
    expect(capturedOptions?.thinking?.keep).toBeUndefined();
  });

  it('injects forced effort through the Anthropic protocol for a Kimi provider', async () => {
    vi.stubEnv('MIRRICODE_MODEL_THINKING_EFFORT', 'max');
    createAgentWithEnv();

    profile.update({ modelAlias: 'mirri-code-anthropic', thinkingLevel: 'high' });
    expect(profile.data().thinkingLevel).toBe('high');
    expect(profile.resolveModelContext().thinkingLevel).toBe('max');
    const statusEvent = ctx?.allEvents.findLast(
      (event) =>
        event.event === 'agent.status.updated' &&
        (event.args as { thinkingEffort?: unknown } | undefined)?.thinkingEffort !== undefined,
    );
    expect(statusEvent?.args).toMatchObject({
      model: 'mirri-code-anthropic',
      thinkingEffort: 'max',
    });

    await requester.request({}, undefined, new AbortController().signal);

    // The harness composes the real provider for a registered vendor: a Kimi
    // model on the Anthropic transport resolves to the anthropic base, and
    // the forced effort arrives as the per-turn thinking intent (the
    // morph-era baked `thinkingEffort` on the provider is gone).
    expect(capturedProvider).toMatchObject({ name: 'anthropic' });
    expect(capturedOptions?.thinking?.effort).toBe('max');
  });
});
