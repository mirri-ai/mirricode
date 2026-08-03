/**
 * Tests for capability-gap hint injection (Step 2 + Step 3 + Step 4c of the
 * model-capability-awareness chain).
 *
 * Covers:
 * - When the main model lacks `image_in` and the prompt contains an image,
 *   a hint is injected telling the LLM to dispatch a vision-capable subagent.
 * - Same for `video_in` / video and `audio_in` / audio.
 * - When NO catalog model supports the missing capability, the hint is
 *   suppressed (don't tell the LLM to dispatch when no subagent could help).
 * - When the main model HAS the capability, no hint is injected.
 * - When the prompt has no modality content, no hint is injected.
 * - The hint only fires on `isNewTurn` (not on continuation steps).
 * - When a profile with a capable `defaultModel` exists, the hint names that
 *   profile (e.g. "dispatch the 'image-analyst' subagent").
 */

import { describe, expect, it } from 'vitest';

import { CapabilityGapInjection } from '#/agent/capabilityGap/capabilityGapInjection';
import type { IAgentContextInjectorService } from '#/agent/contextInjector/contextInjector';
import type { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import type { ContextMessage } from '#/agent/contextMemory/types';
import type { ModelCapability } from '#/kosong/contract/capability';
import type { ContentPart } from '#/kosong/contract/message';
import type { IModelCatalog, Model } from '#/kosong/model/catalog';
import type { IAgentProfileService } from '#/agent/profile/profile';
import type { AgentProfile } from '#/app/agentProfileCatalog/agentProfileCatalog';
import type { ISessionAgentProfileCatalog } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog';

// ── Stubs ────────────────────────────────────────────────────────────

function makeCapability(overrides: Partial<ModelCapability> = {}): ModelCapability {
  return {
    image_in: false,
    video_in: false,
    audio_in: false,
    thinking: false,
    tool_use: true,
    max_context_tokens: 128000,
    ...overrides,
  };
}

function textPart(text: string): ContentPart {
  return { type: 'text', text } as ContentPart;
}

function imagePart(): ContentPart {
  return { type: 'image_url', imageUrl: { url: 'data:image/png;base64,abc' } } as ContentPart;
}

function videoPart(): ContentPart {
  return { type: 'video_url', videoUrl: { url: 'data:video/mp4;base64,abc' } } as ContentPart;
}

function audioPart(): ContentPart {
  return { type: 'audio_url', audioUrl: { url: 'data:audio/wav;base64,abc' } } as ContentPart;
}

function userMessage(...content: ContentPart[]): ContextMessage {
  return {
    role: 'user',
    content,
    toolCalls: [],
  } as ContextMessage;
}

function makeContext(messages: readonly ContextMessage[]): IAgentContextMemoryService {
  return {
    _serviceBrand: undefined,
    get: () => messages,
    append: () => {},
    appendLoopEvent: () => {},
    clear: () => {},
    undo: () => ({ keptHead: [], dropped: [], droppedCount: 0 }),
    applyCompaction: () => ({ summary: '', prunedCount: 0 }),
  } as unknown as IAgentContextMemoryService;
}

function makeProfile(caps: ModelCapability): IAgentProfileService {
  return {
    _serviceBrand: undefined,
    getModelCapabilities: () => caps,
  } as unknown as IAgentProfileService;
}

function makeCatalog(models: Array<Partial<Model> & { id: string }>): IModelCatalog {
  const byId = new Map<string, Model>();
  for (const m of models) {
    byId.set(m.id, {
      id: m.id,
      name: m.name ?? m.id,
      protocol: m.protocol ?? 'openai',
      headers: {},
      capabilities: m.capabilities ?? makeCapability(),
      maxContextSize: m.maxContextSize ?? 128000,
      authProvider: { getAuth: async () => undefined },
      providerName: m.providerName ?? 'test',
      alwaysThinking: false,
    } as Model);
  }
  return {
    _serviceBrand: undefined,
    get: (id: string) => {
      const m = byId.get(id);
      if (m === undefined) throw new Error(`not found: ${id}`);
      return m;
    },
    getById: (modelId: string) => byId.get(modelId),
    getRequester: () => {
      throw new Error('not implemented');
    },
    inspect: () => [...byId.values()],
    ping: async () => {
      throw new Error('not implemented');
    },
    listModels: async () => [],
    listProviders: async () => [],
    getProvider: async () => {
      throw new Error('not implemented');
    },
    setDefaultModel: async () => {
      throw new Error('not implemented');
    },
  } as unknown as IModelCatalog;
}

function makeProfileCatalog(profiles: AgentProfile[]): ISessionAgentProfileCatalog {
  return {
    _serviceBrand: undefined,
    ready: Promise.resolve(),
    onDidChange: { register: () => ({ dispose: () => {} }) },
    get: (name: string) => profiles.find((p) => p.name === name),
    getDefault: () => profiles[0]!,
    list: () => profiles,
    inspect: () => undefined,
    load: async () => {},
    reload: async () => {},
  } as unknown as ISessionAgentProfileCatalog;
}

function makeProfileDef(
  name: string,
  defaultModel?: string,
  capabilitiesRequired?: readonly string[],
): AgentProfile {
  return {
    name,
    description: `${name} profile`,
    defaultModel,
    capabilitiesRequired,
    systemPrompt: () => '',
  } as AgentProfile;
}

/**
 * A fake injector that captures the registered provider so tests can invoke
 * it directly with a controlled `isNewTurn` flag.
 */
function makeInjector(): {
  injector: IAgentContextInjectorService;
  invoke: (isNewTurn: boolean) => string | readonly ContentPart[] | undefined;
} {
  let provider: ((ctx: { isNewTurn: boolean }) => unknown) | undefined;
  const invoke = (isNewTurn: boolean): string | readonly ContentPart[] | undefined => {
    if (provider === undefined) return undefined;
    return provider({ isNewTurn }) as string | readonly ContentPart[] | undefined;
  };
  const injector: IAgentContextInjectorService = {
    _serviceBrand: undefined,
    register: (_name: string, p: (ctx: { isNewTurn: boolean }) => unknown) => {
      provider = p;
      return { dispose: () => { provider = undefined; } };
    },
    injectAfterCompaction: async () => {},
  } as unknown as IAgentContextInjectorService;
  return { injector, invoke };
}

// Default empty profile catalog for tests that don't care about profiles.
const EMPTY_PROFILE_CATALOG = makeProfileCatalog([]);

// ── Tests ────────────────────────────────────────────────────────────

describe('CapabilityGapInjection', () => {
  it('should inject hint when model lacks image_in and prompt contains an image', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true);
    expect(typeof result).toBe('string');
    expect(result as string).toContain('does not support image input');
    expect(result as string).toContain('dispatch a subagent');
  });

  it('should inject hint for video gap when prompt contains a video', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ video_in: false }));
    const context = makeContext([userMessage(textPart('watch'), videoPart())]);
    const catalog = makeCatalog([
      { id: 'video-model', capabilities: makeCapability({ video_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true);
    expect(result as string).toContain('does not support video input');
    expect(result as string).toContain('dispatch a subagent');
  });

  it('should inject hint for audio gap when prompt contains audio', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ audio_in: false }));
    const context = makeContext([userMessage(textPart('listen'), audioPart())]);
    const catalog = makeCatalog([
      { id: 'audio-model', capabilities: makeCapability({ audio_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true);
    expect(result as string).toContain('does not support audio input');
    expect(result as string).toContain('dispatch a subagent');
  });

  it('should NOT inject hint when the model already supports the capability', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: true }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    expect(invoke(true)).toBeUndefined();
  });

  it('should NOT inject hint when prompt has no modality content', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('just text'))]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    expect(invoke(true)).toBeUndefined();
  });

  it('should NOT inject hint when no catalog model supports the missing capability (no feasible alternative)', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'text-model', capabilities: makeCapability({ image_in: false }) },
      { id: 'text-model-2', capabilities: makeCapability({ image_in: false }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    expect(invoke(true)).toBeUndefined();
  });

  it('should inject hint when at least one catalog model supports the capability (feasible alternative exists)', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'text-model', capabilities: makeCapability({ image_in: false }) },
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true);
    expect(typeof result).toBe('string');
    expect(result as string).toContain('dispatch a subagent');
  });

  it('should only fire on isNewTurn, not on continuation steps', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    expect(invoke(true)).toBeDefined();
    expect(invoke(false)).toBeUndefined();
  });

  it('should handle multiple modality gaps in one prompt', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false, video_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart(), videoPart())]);
    const catalog = makeCatalog([
      { id: 'multi-model', capabilities: makeCapability({ image_in: true, video_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true) as string;
    expect(result).toContain('image');
    expect(result).toContain('video');
  });

  it('should use the last user message (not earlier ones) for gap detection', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([
      userMessage(textPart('old'), imagePart()),
      userMessage(textPart('just text now')),
    ]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    expect(invoke(true)).toBeUndefined();
  });

  // ── profile-aware hints (Step 4c) ──────────────────────────────────

  it('should name the profile in the hint when a profile has a capable defaultModel', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', name: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);
    // A custom profile bound to the vision model.
    const profileCatalog = makeProfileCatalog([
      makeProfileDef('image-analyst', 'vision-model'),
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, profileCatalog);

    const result = invoke(true) as string;
    expect(result).toContain("'image-analyst'");
    expect(result).toContain('subagent_type');
  });

  it('should not name profiles whose defaultModel lacks the capability', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
      { id: 'text-model', capabilities: makeCapability({ image_in: false }) },
    ]);
    // image-analyst is bound to a text model — should NOT be named.
    // vision-helper is bound to vision-model — SHOULD be named.
    const profileCatalog = makeProfileCatalog([
      makeProfileDef('image-analyst', 'text-model'),
      makeProfileDef('vision-helper', 'vision-model'),
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, profileCatalog);

    const result = invoke(true) as string;
    expect(result).toContain("'vision-helper'");
    expect(result).not.toContain("'image-analyst'");
  });

  it('should fall back to generic hint when no profile has a capable defaultModel', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);
    // Profiles exist but none bound to a vision-capable model.
    const profileCatalog = makeProfileCatalog([
      makeProfileDef('text-worker', 'text-model'),
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, profileCatalog);

    const result = invoke(true) as string;
    expect(result).toContain('dispatch a subagent');
    expect(result).not.toContain("'text-worker'");
  });

  it('should ignore profiles without a defaultModel when searching for capable profiles', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);
    // Profile with no defaultModel — should not be named even though the
    // catalog has a capable model.
    const profileCatalog = makeProfileCatalog([
      makeProfileDef('no-model-profile', undefined),
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, profileCatalog);

    const result = invoke(true) as string;
    expect(result).toContain('dispatch a subagent');
    expect(result).not.toContain("'no-model-profile'");
  });

  // ── attachment-path hints (Issue 2) ────────────────────────────────

  it('should include the attachment file path in the hint when a compression caption is present', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    // Simulate a compression caption that embeds the original-image path.
    const caption = 'Image compressed to fit model limits: original 2054x1822 -> sent 2000x1774. ' +
      'The uncompressed original is saved at "/tmp/media-originals/abc123.png"; ' +
      'if you need fine detail, call ReadMediaFile on that path.';
    const context = makeContext([userMessage(textPart('look'), textPart(caption), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true) as string;
    expect(result).toContain('/tmp/media-originals/abc123.png');
    expect(result).toContain('ReadMediaFile');
  });

  it('should include attachment path when dispatching a named profile', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const caption = 'The uncompressed original is saved at "/home/user/sessions/img-xyz.png";';
    const context = makeContext([userMessage(textPart('look'), textPart(caption), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', name: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);
    const profileCatalog = makeProfileCatalog([
      makeProfileDef('image-analyst', 'vision-model'),
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, profileCatalog);

    const result = invoke(true) as string;
    expect(result).toContain("'image-analyst'");
    expect(result).toContain('/home/user/sessions/img-xyz.png');
  });

  it('should NOT include a path when no compression caption is present (small images)', () => {
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    // No compression caption — just image part, no saved path.
    const context = makeContext([userMessage(textPart('look'), imagePart())]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true) as string;
    expect(result).toContain('dispatch a subagent');
    expect(result).not.toContain('saved at');
    expect(result).not.toContain('ReadMediaFile on that path');
  });

  it('should detect image gap from a client-injected text notice even without an image_url part', () => {
    // Clients (Desktop / CLI) replace an inline image with a text notice when
    // the bound model lacks image input. The gap hint must still fire so the
    // LLM is told to dispatch a subagent — otherwise the model sees only a
    // text path and may reply "I don't see an image" instead of delegating.
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const notice =
      '[image omitted: current model has no image input]\n' +
      'The original image has been saved to: /home/user/.mirri-code/sessions/x/media-originals/abc.png\n' +
      'To analyze this image, try one of these approaches:\n' +
      '1. Check if a dedicated multimodal sub-agent is available (e.g. a media-reader profile).';
    const context = makeContext([userMessage(textPart('这个图是什么内容'), textPart(notice))]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true) as string;
    expect(result).toContain('does not support image input');
    expect(result).toContain('dispatch');
    // The path from the client notice must be extracted and surfaced so the
    // LLM can hand it to the subagent.
    expect(result).toContain('/home/user/.mirri-code/sessions/x/media-originals/abc.png');
    expect(result).toContain('ReadMediaFile');
  });

  it('should detect image gap from an "Attached file" notice for an image extension', () => {
    // kap-server replaces an unsupported-MIME image with a text notice of the
    // form: `Attached file "image.png" (image/png, 1234 bytes): /path — open it with the Read tool`
    // The gap hint must fire and surface that path.
    const { injector, invoke } = makeInjector();
    const profile = makeProfile(makeCapability({ image_in: false }));
    const notice = 'Attached file "image.png" (image/png, 1234 bytes): /tmp/attachments/image.png — open it with the Read tool';
    const context = makeContext([userMessage(textPart('what is this'), textPart(notice))]);
    const catalog = makeCatalog([
      { id: 'vision-model', capabilities: makeCapability({ image_in: true }) },
    ]);

    new CapabilityGapInjection(undefined, injector, context, profile, catalog, EMPTY_PROFILE_CATALOG);

    const result = invoke(true) as string;
    expect(result).toContain('does not support image input');
    expect(result).toContain('/tmp/attachments/image.png');
  });
});
