/**
 * Provider stub — a canned-response LLM provider for the mirri TDD harness.
 *
 * Wraps the existing `createScriptedGenerate` from the agent-core-v2 test
 * harness, adding a simpler declarative API for the mirri feature-phase
 * tests. Tests call `stub.nextTextResponse(text)` or
 * `stub.nextToolCallResponse(...)` to queue deterministic LLM responses.
 *
 * The stub plugs into v2's `IProtocolAdapterRegistry` via a
 * `ScopeSeed` — pass `stub.seed` as an extra seed to `bootstrap()`.
 */

import type { ServiceIdentifier } from '#/_base/di/instantiation';
import type { StreamedMessagePart } from '#/kosong/contract/message';
import { type FinishReason } from '#/kosong/contract/provider';

import { createScriptedGenerate } from '../harness/scripted-generate';
import {
  IProtocolAdapterRegistry,
  ProtocolAdapterRegistry,
} from '#/kosong/provider/protocolAdapterRegistry';
import { hasProviderDefinition } from '#/kosong/provider/providerDefinition';
import type { ProtocolAdapterConfig } from '#/kosong/protocol/protocol';
import type { ChatProvider, GenerateOptions, StreamedMessage } from '#/kosong/contract/provider';
import type { generate as kosongGenerate } from '#/kosong/contract/generate';

type GenerateFn = typeof kosongGenerate;

/**
 * A provider stub that queues deterministic LLM responses.
 *
 * Usage:
 * ```
 * const stub = createProviderStub();
 * stub.nextTextResponse('Hello from the stub!');
 * // ... later, after the engine calls the LLM ...
 * expect(stub.calls).toHaveLength(1);
 * ```
 */
export interface ProviderStub {
  /** Queue a simple text response (no tool calls). */
  nextTextResponse(text: string, options?: StubResponseOptions): void;

  /** Queue a response that includes tool-call declarations. */
  nextToolCallResponse(
    toolCalls: readonly ToolCallDeclaration[],
    options?: StubResponseOptions,
  ): void;

  /** Queue a raw response with arbitrary `StreamedMessagePart`s. */
  nextRawResponse(parts: readonly StreamedMessagePart[], options?: StubResponseOptions): void;

  /** All LLM generate calls received so far. */
  readonly calls: ReturnType<typeof createScriptedGenerate>['calls'];

  /**
   * A `ScopeSeed` entry that registers this stub as the
   * `IProtocolAdapterRegistry`. Include in `bootstrap()` extra seeds:
   * `bootstrap(input, [stub.seed])`.
   */
  readonly seed: readonly [ServiceIdentifier<unknown>, unknown];
}

export interface StubResponseOptions {
  readonly finishReason?: FinishReason;
  readonly rawFinishReason?: string;
  readonly traceId?: string;
}

export interface ToolCallDeclaration {
  readonly id: string;
  readonly name: string;
  readonly args: string;
}

export function createProviderStub(): ProviderStub {
  const scripted = createScriptedGenerate();

  function nextTextResponse(text: string, options?: StubResponseOptions): void {
    scripted.mockNextProviderResponse({
      parts: [{ type: 'text', text }],
      finishReason: options?.finishReason,
      rawFinishReason: options?.rawFinishReason,
      traceId: options?.traceId,
    });
  }

  function nextToolCallResponse(
    toolCalls: readonly ToolCallDeclaration[],
    options?: StubResponseOptions,
  ): void {
    const parts: StreamedMessagePart[] = [
      // Include a minimal text part so the message isn't empty
      { type: 'text', text: '' },
      ...toolCalls.map((tc) => ({
        type: 'tool_call' as const,
        id: tc.id,
        name: tc.name,
        args: tc.args,
      })),
    ];
    scripted.mockNextProviderResponse({
      parts,
      finishReason: options?.finishReason ?? 'tool_calls',
      rawFinishReason: options?.rawFinishReason,
      traceId: options?.traceId,
    });
  }

  function nextRawResponse(
    parts: readonly StreamedMessagePart[],
    options?: StubResponseOptions,
  ): void {
    scripted.mockNextProviderResponse({
      parts: structuredClone(parts),
      finishReason: options?.finishReason,
      rawFinishReason: options?.rawFinishReason,
      traceId: options?.traceId,
    });
  }

  const protocolRegistry = createGenerateBackedProtocolRegistry(scripted.generate);

  return {
    nextTextResponse,
    nextToolCallResponse,
    nextRawResponse,
    calls: scripted.calls,
    seed: [IProtocolAdapterRegistry as ServiceIdentifier<unknown>, protocolRegistry],
  };
}

// ---------------------------------------------------------------------------
// Protocol-registry adapter — mirrors the existing harness pattern from
// test/harness/agent.ts but standalone so this module has no dependency on
// the existing test harness.
// ---------------------------------------------------------------------------

function createGenerateBackedProtocolRegistry(generate: GenerateFn): IProtocolAdapterRegistry {
  const real = new ProtocolAdapterRegistry();
  return {
    _serviceBrand: undefined,
    supportedProtocols: () => real.supportedProtocols(),
    resolveAdapterIdentity: (protocol, providerType) =>
      real.resolveAdapterIdentity(protocol, providerType),
    resolveProviderBaseId: (protocol, providerType) =>
      real.resolveProviderBaseId(protocol, providerType),
    resolveCapability: (protocol, modelName, providerType) =>
      real.resolveCapability(protocol, modelName, providerType),
    explainCapability: (protocol, modelName, providerType) =>
      real.explainCapability(protocol, modelName, providerType),
    createChatProvider: (input: ProtocolAdapterConfig) => {
      if (input.providerType !== undefined && hasProviderDefinition(input.providerType)) {
        return replaceProviderGenerate(real.createChatProvider(input), generate);
      }
      return new GenerateBackedChatProvider(input, generate);
    },
  } as IProtocolAdapterRegistry;
}

/**
 * Replace `generate` on a composed provider while delegating everything else.
 */
function replaceProviderGenerate(provider: ChatProvider, generate: GenerateFn): ChatProvider {
  const replaced: ChatProvider = {
    get name() { return provider.name; },
    get modelName() { return provider.modelName; },
    get thinkingEffort() { return provider.thinkingEffort; },
    get maxCompletionTokens() { return provider.maxCompletionTokens; },
    generate: (systemPrompt, tools, history, options) =>
      generateBackedResponse(provider, generate, systemPrompt, tools, history, options),
  };
  if (provider.uploadVideo !== undefined) {
    replaced.uploadVideo = (input, options) => provider.uploadVideo!(input, options);
  }
  return replaced;
}

/**
 * Simple `ChatProvider` that delegates to the scripted `generate`.
 */
class GenerateBackedChatProvider implements ChatProvider {
  constructor(
    private readonly config: ProtocolAdapterConfig,
    private readonly generate: GenerateFn,
  ) {}

  get name(): string { return this.config.model; }
  get modelName(): string { return this.config.model; }
  get thinkingEffort(): string | undefined { return undefined; }
  get maxCompletionTokens(): number | undefined { return undefined; }

  generate(
    systemPrompt: string,
    tools: Parameters<GenerateFn>[1],
    history: Parameters<GenerateFn>[2],
    options: GenerateOptions,
  ): Promise<StreamedMessage> {
    return this.generate(this.config, systemPrompt, tools, history, undefined, options);
  }
}

/**
 * Route a composed provider's `generate` through the scripted function,
 * passing the composed provider as the `chat` argument.
 */
async function generateBackedResponse(
  provider: ChatProvider,
  generate: GenerateFn,
  systemPrompt: string,
  tools: Parameters<GenerateFn>[1],
  history: Parameters<GenerateFn>[2],
  options: GenerateOptions,
): Promise<StreamedMessage> {
  const config: ProtocolAdapterConfig = {
    protocol: 'openai',
    model: provider.modelName,
  };
  return generate(config, systemPrompt, tools, history, undefined, options);
}
