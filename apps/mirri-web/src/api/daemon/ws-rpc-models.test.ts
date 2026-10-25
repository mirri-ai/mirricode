// apps/mirri-web/src/api/daemon/ws-rpc-models.test.ts
// Unit tests for the WsRpcApi model/provider wire → app mapping (B1). The
// rust engine sends `model.list` / `model.listProviders` rows in the klient
// snake_case shape (`modelCatalogItemSchema` / `providerCatalogItemSchema`),
// each model row carrying `id` — the canonical `[models.<id>]` config key
// (the model alias, ADR-003) the adapter uses as the app id.

import { describe, expect, it, vi } from 'vitest';

import {
  toAppAgentProfile,
  toAppCatalogProvider,
  toAppMessage,
  toAppModel,
  toAppProvider,
  toAppProviderRefreshResult,
  toAppSnapshot,
  WsRpcApi,
} from './ws-rpc-api';
import type {
  RpcCatalogProvider,
  RpcModel,
  RpcProvider,
  RpcProviderRefreshResult,
} from './ws-rpc-api';
import type { AppAddProviderModelInput } from '../types';

/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

describe('toAppModel (rust model.list wire → app shape)', () => {
  it('should carry the wire id (the config key alias) through as the app id', () => {
    // Given: rust wire 行（snake_case），行携带 `id` = `[models.<id>]`
    // 配置键（ADR-003——别名 key 是模型传递链路的唯一标识）
    const wire: RpcModel = {
      id: 'github-copilot/claude-opus-4.8',
      provider: 'github-copilot',
      model: 'claude-opus-4.8',
      display_name: 'Claude Opus 4.8',
      max_context_size: 200000,
    };
    // When
    const app = toAppModel(wire);
    // Then the app id is the wire id verbatim — every write path
    // (switch model, set default, delete) addresses models by config key
    expect(app.id).toBe('github-copilot/claude-opus-4.8');
    expect(app.displayName).toBe('Claude Opus 4.8');
    expect(app.maxContextSize).toBe(200000);
  });

  it('should carry a short-key alias id through (never a composed id)', () => {
    // Given a wire row whose config alias key is a short name (`fast`)
    // naming rakuten's DeepSeek — the composed `provider/model` spelling
    // resolves to nothing in the engine's alias registry, so the app id
    // must stay the config key the engine can actually resolve
    const wire: RpcModel = {
      id: 'fast',
      provider: 'rakuten',
      model: 'DeepSeek-V4-Flash',
      max_context_size: 716800,
    };
    // When
    const app = toAppModel(wire);
    // Then the id is the alias key, so a session switched to it resolves
    // through the config instead of dead-ending on a wrong provider
    expect(app.id).toBe('fast');
  });

  it('should fall back to the composed provider/model id when the wire carries no id', () => {
    // Given a wire row from an engine that predates the `id` field
    // (the TS daemon's klient-shaped rows) — a display key only
    const wire = {
      provider: 'openai',
      model: 'gpt-5.2',
      max_context_size: 0,
    } as unknown as RpcModel;
    // When
    const app = toAppModel(wire);
    // Then the mapping still succeeds with the composed display key
    expect(app.id).toBe('openai/gpt-5.2');
  });

  it('should fall back to the raw model name when display_name is absent', () => {
    // Given a wire row without the optional display_name
    const wire: RpcModel = {
      id: 'openai/gpt-5.2',
      provider: 'openai',
      model: 'gpt-5.2',
      max_context_size: 0,
    };
    // When
    const app = toAppModel(wire);
    // Then the display name stays undefined (the UI renders
    // `displayName ?? model`) and the support fields degrade honestly
    expect(app.id).toBe('openai/gpt-5.2');
    expect(app.displayName).toBeUndefined();
    expect(app.maxContextSize).toBe(0);
    expect(app.supportEfforts).toBeUndefined();
  });
});

describe('toAppProvider (rust model.listProviders wire → app shape)', () => {
  it('should map snake_case provider wire fields to AppProvider', () => {
    // Given: rust wire 行（snake_case）
    const wire: RpcProvider = {
      id: 'anthropic',
      type: 'anthropic',
      base_url: 'https://gateway.example.test/v1',
      default_model: 'anthropic/claude-sonnet-4',
      has_api_key: true,
      status: 'connected',
      models: ['anthropic/claude-sonnet-4'],
    };
    // When
    const provider = toAppProvider(wire);
    // Then
    expect(provider.hasApiKey).toBe(true);
    expect(provider.baseUrl).toBe('https://gateway.example.test/v1');
    expect(provider.defaultModel).toBe('anthropic/claude-sonnet-4');
    expect(provider.status).toBe('connected');
    expect(provider.id).toBe('anthropic');
  });

  it('should default has_api_key to false when absent or false', () => {
    // Given a wire row without has_api_key (e.g. unconfigured provider)
    const wire: RpcProvider = {
      id: 'moonshot',
      type: 'moonshot',
      has_api_key: false,
      status: 'unconfigured',
    };
    // When
    const provider = toAppProvider(wire);
    // Then
    expect(provider.hasApiKey).toBe(false);
    expect(provider.baseUrl).toBeUndefined();
  });
});

describe('toAppCatalogProvider (rust providerDiscovery.listCatalog wire → app shape)', () => {
  it('should map the pruned browse row to the app catalog shape', () => {
    // Given: rust wire 行（snake_case，pruned browse 形状）
    const wire: RpcCatalogProvider = {
      id: 'import-ai',
      name: 'Import AI',
      wire_type: 'openai',
      needs_base_url: false,
      env_key: 'IMPORT_AI_API_KEY',
      models: [
        { id: 'imp-large', name: 'Imp Large', context_length: 200000 },
        { id: 'imp-small', name: 'Imp Small', context_length: 32000 },
      ],
    };
    // When
    const provider = toAppCatalogProvider(wire);
    // Then
    expect(provider.id).toBe('import-ai');
    expect(provider.name).toBe('Import AI');
    expect(provider.wireType).toBe('openai');
    expect(provider.needsBaseUrl).toBe(false);
    expect(provider.envKey).toBe('IMPORT_AI_API_KEY');
    expect(provider.models).toHaveLength(2);
    // The wire's `context_length` maps to the app `maxContextSize` — the
    // boundary mapping (the wire spelling is the browse surface's own).
    expect(provider.models[0]).toEqual({
      id: 'imp-large',
      name: 'Imp Large',
      maxContextSize: 200000,
      reasoning: false,
    });
  });

  it('should degrade omitted wire facts to null/zero, never fabricate them', () => {
    // Given a wire row whose optional facts the directory does not declare
    // (the pruned shape omits — never nulls — and carries no
    // guessed/rejected/reject_reason at all)
    const wire: RpcCatalogProvider = {
      id: 'wiremock-ai',
      name: 'WireMock AI',
      needs_base_url: true,
      models: [{ id: 'wm-small', name: 'WM Small' }],
    };
    // When
    const provider = toAppCatalogProvider(wire);
    // Then: wireType/envKey degrade to null, a model without limit.context
    // maps maxContextSize to 0, and the rejected/guessed facts stay
    // undefined (the dialog branches on `=== true` so an undefined is the
    // honest "the engine does not know")
    expect(provider.wireType).toBeNull();
    expect(provider.envKey).toBeNull();
    expect(provider.needsBaseUrl).toBe(true);
    expect(provider.models[0]?.maxContextSize).toBe(0);
    expect(provider.models[0]?.reasoning).toBe(false);
    expect(provider.rejected).toBeUndefined();
    expect(provider.guessed).toBeUndefined();
    expect(provider.rejectReason).toBeUndefined();
  });

  it('should pass the directory capability facts through when the engine emits them', () => {
    // Given a rust wire row whose models carry the directory-derived
    // capability facts (ADR-004)
    const wire: RpcCatalogProvider = {
      id: 'import-ai',
      name: 'Import AI',
      needs_base_url: false,
      models: [
        {
          id: 'imp-large',
          name: 'Imp Large',
          context_length: 200000,
          capabilities: ['image_in', 'thinking', 'tool_use'],
          reasoning: true,
        },
        { id: 'imp-small', name: 'Imp Small', context_length: 32000 },
      ],
    };
    // When
    const provider = toAppCatalogProvider(wire);
    // Then: the facts pass through untouched; a row without them keeps
    // reasoning false and capabilities undefined (never fabricated)
    expect(provider.models[0]).toEqual({
      id: 'imp-large',
      name: 'Imp Large',
      maxContextSize: 200000,
      capabilities: ['image_in', 'thinking', 'tool_use'],
      reasoning: true,
    });
    expect(provider.models[1]).toEqual({
      id: 'imp-small',
      name: 'Imp Small',
      maxContextSize: 32000,
      reasoning: false,
    });
  });
});

describe('toAppProviderRefreshResult (rust providerDiscovery.refreshProviderModels wire → app shape)', () => {
  it('should map the snake_case refresh result to ProviderRefreshResult', () => {
    // Given: rust wire 行（snake_case，klient refreshProviderModelsResponseSchema）
    const wire: RpcProviderRefreshResult = {
      changed: [
        { provider_id: 'anthropic', provider_name: 'Anthropic', added: 3, removed: 1 },
        { provider_id: 'openai', provider_name: 'OpenAI', added: 0, removed: 0 },
      ],
      unchanged: ['moonshot'],
      failed: [{ provider: 'google', reason: 'no api key configured' }],
    };
    // When
    const result = toAppProviderRefreshResult(wire);
    // Then
    expect(result).toEqual({
      changed: [
        { providerId: 'anthropic', providerName: 'Anthropic', added: 3, removed: 1 },
        { providerId: 'openai', providerName: 'OpenAI', added: 0, removed: 0 },
      ],
      unchanged: ['moonshot'],
      failed: [{ provider: 'google', reason: 'no api key configured' }],
    });
  });
});

describe('WsRpcApi provider refresh (providerDiscovery.refreshProviderModels)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  const wireResult: RpcProviderRefreshResult = {
    changed: [{ provider_id: 'anthropic', provider_name: 'Anthropic', added: 3, removed: 1 }],
    unchanged: ['moonshot'],
    failed: [{ provider: 'google', reason: 'no api key configured' }],
  };

  it('should call refreshProviderModels with the addressed providerId and return the mapped result when refreshProvider is invoked', async () => {
    // Given a transport mock resolving a per-provider refresh result
    const { api, call } = createApi();
    call.mockResolvedValue(wireResult);
    // When refreshProvider is called for one configured provider
    const result = await api.refreshProvider('anthropic');
    // Then the RPC is addressed by providerId and the result reaches the caller
    expect(call).toHaveBeenCalledWith('providerDiscovery.refreshProviderModels', {
      providerId: 'anthropic',
    });
    expect(result).toEqual({
      changed: [
        { providerId: 'anthropic', providerName: 'Anthropic', added: 3, removed: 1 },
      ],
      unchanged: ['moonshot'],
      failed: [{ provider: 'google', reason: 'no api key configured' }],
    });
  });

  it('should call refreshProviderModels with the scope-wide all options when refreshAllProviders is invoked', async () => {
    // Given a transport mock resolving a scope-wide refresh result
    const { api, call } = createApi();
    call.mockResolvedValue(wireResult);
    // When refreshAllProviders is invoked
    const result = await api.refreshAllProviders();
    // Then the RPC is the `{ scope: 'all' }` refresh-all form and the result
    // reaches the caller
    expect(call).toHaveBeenCalledWith('providerDiscovery.refreshProviderModels', {
      scope: 'all',
    });
    expect(result).toEqual({
      changed: [
        { providerId: 'anthropic', providerName: 'Anthropic', added: 3, removed: 1 },
      ],
      unchanged: ['moonshot'],
      failed: [{ provider: 'google', reason: 'no api key configured' }],
    });
  });

  it('should propagate the RPC rejection (e.g. provider.not_found) to the caller', async () => {
    // Given a transport mock rejecting the scoped refresh
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('provider.not_found'));
    // When refreshProvider targets an unknown provider
    await expect(api.refreshProvider('ghost')).rejects.toThrow('provider.not_found');
    // Then the engine error reaches the caller untouched (no swallow, no wrap)
    expect(call).toHaveBeenCalledWith('providerDiscovery.refreshProviderModels', {
      providerId: 'ghost',
    });
  });
});

describe('WsRpcApi listMessages (rust message.list wire → app shape)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  it('should carry the row provenance metadata through to the app message', async () => {
    // Given a transport answering a mixed history page — a real user row
    // and an injection row (the <tools_added> system reminder the engine
    // appends between turns)
    const { api, call } = createApi();
    call.mockResolvedValue({
      messages: [
        {
          id: 'msg-0',
          role: 'user',
          content: 'hello',
          agentId: 'main',
          metadata: { origin: { kind: 'user' } },
        },
        {
          id: 'msg-1',
          role: 'user',
          content: '<system-reminder><tools_added>…</tools_added></system-reminder>',
          agentId: 'main',
          metadata: { origin: { kind: 'injection' } },
        },
      ],
      hasMore: false,
    });

    // When listing the session's messages
    const page = await api.listMessages('session-1');

    // Then the provenance rides through (metadata.origin.kind) so the
    // transcript filter can hide the system-injected turn instead of
    // rendering it as real user input
    expect(page.items).toHaveLength(2);
    expect(page.items[0]?.metadata).toEqual({ origin: { kind: 'user' } });
    expect(page.items[1]?.metadata).toEqual({ origin: { kind: 'injection' } });
    expect(call).toHaveBeenCalledWith('message.list', { sessionId: 'session-1' });
  });

  it('should leave metadata undefined when the wire row carries none', async () => {
    // Given a transport answering a row without provenance (older engines
    // or assistant rows, which carry no origin)
    const { api, call } = createApi();
    call.mockResolvedValue({
      messages: [{ id: 'msg-0', role: 'assistant', content: 'hi', agentId: 'main' }],
      hasMore: false,
    });

    // When listing the session's messages
    const page = await api.listMessages('session-1');

    // Then the app message degrades honestly — no fabricated metadata
    expect(page.items[0]?.metadata).toBeUndefined();
  });
});

describe('WsRpcApi addProvider (createProvider alignment on the rust backend)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  /** A v2 `createProvider`-shaped input: explicit id, wire type, models
   *  (may be empty — the create accepts zero models). */
  const input = {
    id: 'acme',
    type: 'openai',
    apiKey: 'sk-test',
    baseUrl: 'https://api.example.test/v1',
    defaultModel: 'acme-large',
    models: [
      { model: 'acme-large', maxContextSize: 128000 },
      { model: 'acme-small', maxContextSize: 32000, displayName: 'Acme Small' },
    ],
  };

  it('should write the provider record and model aliases in one atomic config.set and seed the default model in a fresh environment', async () => {
    // Given a fresh environment (no providers, no defaultModel configured
    // anywhere) and a read-back provider row
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'config.get') return {};
      if (method === 'model.getProvider') {
        return {
          id: 'acme',
          type: 'openai',
          has_api_key: true,
          status: 'connected',
          default_model: 'acme/acme-large',
        };
      }
      return {};
    });

    // When addProvider runs the create flow
    const provider = await api.addProvider(input);

    // Then the write pattern is ONE config.set carrying both sections —
    // an atomic create (a failed second write can no longer strand a
    // provider with no models)
    expect(call).toHaveBeenNthCalledWith(1, 'config.get');
    expect(call).toHaveBeenNthCalledWith(2, 'config.set', {
      providers: {
        acme: {
          type: 'openai',
          apiKey: 'sk-test',
          baseUrl: 'https://api.example.test/v1',
          defaultModel: 'acme/acme-large',
        },
      },
      models: {
        'acme/acme-large': { provider: 'acme', model: 'acme-large', maxContextSize: 128000 },
        'acme/acme-small': {
          provider: 'acme',
          model: 'acme-small',
          maxContextSize: 32000,
          displayName: 'Acme Small',
        },
      },
      defaultModel: 'acme/acme-large',
    });
    // And the answer is the read-back provider row in the app shape
    expect(call).toHaveBeenNthCalledWith(3, 'model.getProvider', { providerId: 'acme' });
    expect(provider).toEqual({
      id: 'acme',
      type: 'openai',
      baseUrl: undefined,
      defaultModel: 'acme/acme-large',
      hasApiKey: true,
      status: 'connected',
      models: undefined,
    });
  });

  it('should reject a taken provider id before any write lands', async () => {
    // Given a config that already defines the addressed provider id
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'config.get') {
        return { providers: { acme: { type: 'openai' } } };
      }
      return {};
    });

    // When addProvider targets the taken id
    await expect(api.addProvider(input)).rejects.toThrow('provider acme already exists');

    // Then no config.set write ever fired (the guard runs before the writes)
    const setCalls = call.mock.calls.filter((invocation) => invocation[0] === 'config.set');
    expect(setCalls).toHaveLength(0);
  });

  it('should never move an existing defaultModel pointer', async () => {
    // Given an environment that already has a default model configured
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'config.get') return { defaultModel: 'other/first' };
      if (method === 'model.getProvider') {
        return { id: 'acme', type: 'openai', has_api_key: false, status: 'unconfigured' };
      }
      return {};
    });

    // When addProvider runs the create flow
    await api.addProvider(input);

    // Then the single config.set write carries no defaultModel key — an
    // existing pointer is never moved
    const setCalls = call.mock.calls.filter((invocation) => invocation[0] === 'config.set');
    expect(setCalls).toHaveLength(1);
    expect(setCalls[0]![1]).not.toHaveProperty('defaultModel');
  });

  it('should create a provider with zero models without writing any model aliases', async () => {
    // Given a fresh environment (no providers, no defaultModel) and a
    // create input whose models list is empty and no provider default
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'config.get') return {};
      if (method === 'model.getProvider') {
        return { id: 'empty', type: 'openai', has_api_key: true, status: 'connected' };
      }
      return {};
    });
    const emptyInput = {
      id: 'empty',
      type: 'openai',
      apiKey: 'sk-test',
      baseUrl: 'https://api.example.test/v1',
      models: [] as AppAddProviderModelInput[],
    };

    // When addProvider runs the create flow with the empty models list
    const provider = await api.addProvider(emptyInput);

    // Then one atomic config.set lands the provider record with no models
    // section and no seeded default (nothing to seed)
    expect(call).toHaveBeenNthCalledWith(2, 'config.set', {
      providers: {
        empty: {
          type: 'openai',
          apiKey: 'sk-test',
          baseUrl: 'https://api.example.test/v1',
        },
      },
      models: {},
    });
    expect(provider).toEqual({
      id: 'empty',
      type: 'openai',
      baseUrl: undefined,
      defaultModel: undefined,
      hasApiKey: true,
      status: 'connected',
      models: undefined,
    });
  });
});
// ---------------------------------------------------------------------------
// toAppSnapshot (rust session.snapshot wire → app shape)
// ---------------------------------------------------------------------------

describe('toAppSnapshot (rust session.snapshot wire → app shape)', () => {
  type SnapshotWire = Parameters<typeof toAppSnapshot>[0];

  const idleSnapshot: SnapshotWire = {
    as_of_seq: 7,
    epoch: 'epoch-1',
    session: {
      id: 'session-1',
      title: null,
      busy: false,
      agentCount: 1,
      model: null,
      permission: null,
      messageCount: 0,
    } as SnapshotWire['session'],
    messages: { items: [], has_more: false },
    in_flight_turn: null,
    pending_approvals: [],
    pending_questions: [],
  };

  it('should report no in-flight turn when the snapshot carries none', () => {
    // Given a snapshot of an idle session (in_flight_turn is null)
    // When the snapshot maps to the app shape
    const app = toAppSnapshot(idleSnapshot, 'session-1');

    // Then the in-flight turn is absent — no ghost half-finished reply
    expect(app.inFlightTurn).toBeNull();
    expect(app.asOfSeq).toBe(7);
    expect(app.epoch).toBe('epoch-1');
  });

  it('should map the in-flight turn with its running tools and string turn id', () => {
    // Given a snapshot of a mid-turn session: accumulated text, thinking,
    // one running tool with a last progress update, and a wire turn id
    const wire: SnapshotWire = {
      ...idleSnapshot,
      in_flight_turn: {
        turn_id: 'turn-abc123',
        assistant_text: 'hello ',
        thinking_text: 'pondering',
        running_tools: [
          {
            tool_call_id: 'call-1',
            name: 'Bash',
            args: { command: 'ls' },
            last_progress: { kind: 'stdout', text: 'file.txt' },
          },
        ],
      },
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the in-flight turn carries the turn id verbatim, the accumulated
    // text/thinking, and the running tool with its progress — ready for
    // the projector's seedInFlight
    expect(app.inFlightTurn).not.toBeNull();
    expect(app.inFlightTurn!.turnId).toBe('turn-abc123');
    expect(app.inFlightTurn!.assistantText).toBe('hello ');
    expect(app.inFlightTurn!.thinkingText).toBe('pondering');
    expect(app.inFlightTurn!.runningTools).toHaveLength(1);
    expect(app.inFlightTurn!.runningTools[0]).toMatchObject({
      toolCallId: 'call-1',
      name: 'Bash',
      args: { command: 'ls' },
      lastProgress: { kind: 'stdout', text: 'file.txt' },
    });
    expect(app.inFlightTurn!.promptId).toBeUndefined();
  });

  it('should map pending approvals and questions to their app shapes', () => {
    // Given a snapshot with one pending approval and one pending question
    const wire: SnapshotWire = {
      ...idleSnapshot,
      pending_approvals: [
        {
          approval_id: 'ap-1',
          session_id: 'session-1',
          tool_call_id: 'call-9',
          tool_name: 'Bash',
          action: 'execute Bash',
          tool_input_display: { kind: 'command', command: 'rm -rf /' },
          created_at: '2026-09-12T00:00:00Z',
        },
      ],
      pending_questions: [
        {
          question_id: 'q-1',
          session_id: 'session-1',
          questions: [{ id: 'i1', question: 'Proceed?', options: [{ id: 'a', label: 'Yes' }, { id: 'b', label: 'No' }] }],
          created_at: '2026-09-12T00:00:01Z',
        },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then both pending rows survive the mapping with their ids and
    // payloads — a mid-turn reopen can render and answer them
    expect(app.pendingApprovals).toHaveLength(1);
    expect(app.pendingApprovals[0]).toMatchObject({
      approvalId: 'ap-1',
      toolCallId: 'call-9',
      toolName: 'Bash',
      action: 'execute Bash',
      display: { kind: 'command', command: 'rm -rf /' },
    });
    expect(app.pendingQuestions).toHaveLength(1);
    expect(app.pendingQuestions[0]).toMatchObject({
      questionId: 'q-1',
      questions: [{ id: 'i1', question: 'Proceed?' }],
    });
  });

  it('should map the pending question rows through the protocol mapper so every option keeps a unique id', () => {
    // Given a snapshot whose pending_questions rows carry the v2 REST
    // questionRequestSchema shape — items with NO id, options with NO id,
    // snake_case multi_select (the rust gate transparently forwards the
    // AskUserQuestion tool input; only a snapshot-minted request id exists)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      pending_questions: [
        {
          question_id: 'q-1',
          session_id: 'session-1',
          turn_id: 4,
          tool_call_id: 'call-4',
          questions: [
            {
              question: 'Proceed?',
              multi_select: false,
              allow_other: true,
              options: [
                { label: 'Yes', description: 'go ahead' },
                { label: 'No' },
              ],
            },
          ],
          created_at: '2026-09-12T00:00:01Z',
        },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the pending question row survives with unique item and option
    // ids — without them every option's id is undefined and selecting one
    // selects them all (the session-switch restore regression)
    expect(app.pendingQuestions).toHaveLength(1);
    expect(app.pendingQuestions[0]).toMatchObject({
      questionId: 'q-1',
      sessionId: 'session-1',
      turnId: 4,
      toolCallId: 'call-4',
    });
    const item = app.pendingQuestions[0]!.questions[0]!;
    expect(item.multiSelect).toBe(false);
    expect(item.allowOther).toBe(true);
    expect(item.options).toHaveLength(2);
    expect(item.options[0]).toMatchObject({ label: 'Yes', description: 'go ahead' });
    expect(item.options[1]).toMatchObject({ label: 'No' });
    const [firstId, secondId] = [item.options[0]!.id, item.options[1]!.id];
    expect(firstId).toBeTruthy();
    expect(secondId).toBeTruthy();
    expect(firstId).not.toBe(secondId);
    expect(item.id).toBeTruthy();
  });

  it('should map the requesting member agent id on a pending approval row when the snapshot carries one', () => {
    // Given a snapshot whose pending approval row attributes the ask to a
    // member agent (supervision B1) — and one that does not
    const wire: SnapshotWire = {
      ...idleSnapshot,
      pending_approvals: [
        {
          approval_id: 'ap-m',
          session_id: 'session-1',
          tool_call_id: 'call-m',
          tool_name: 'Bash',
          action: 'execute Bash',
          tool_input_display: { kind: 'shell', command: 'ls' },
          agent_id: 'agent-33333333-3333-4333-8333-333333333333',
          created_at: '2026-09-14T00:00:00Z',
        },
        {
          approval_id: 'ap-0',
          session_id: 'session-1',
          tool_call_id: 'call-0',
          tool_name: 'Bash',
          action: 'execute Bash',
          tool_input_display: { kind: 'shell', command: 'pwd' },
          created_at: '2026-09-14T00:00:01Z',
        },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the member's row carries the full agent id (the roster key the
    // badge resolves against) and the unattributed row stays unattributed
    const byId = new Map(app.pendingApprovals.map((a) => [a.approvalId, a]));
    expect(byId.get('ap-m')?.agentId).toBe('agent-33333333-3333-4333-8333-333333333333');
    expect(byId.get('ap-0')?.agentId).toBeUndefined();
  });

  it('should map turn usages onto the snapshot when the engine sends them', () => {
    // Given a snapshot whose envelope carries the per-turn usage fold of a
    // completed turn (camelCase inner keys, matching the step-completed buckets)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      turn_usages: [
        {
          promptId: 'turn-1',
          inputTokens: 10,
          outputTokens: 5,
          cacheReadTokens: 1000,
          cacheCreationTokens: 200,
          models: ['kimi-k2'],
        },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the app snapshot carries the folded usage per turn, keyed by prompt id
    expect(app.turnUsages).toEqual([
      {
        promptId: 'turn-1',
        inputTokens: 10,
        outputTokens: 5,
        cacheReadTokens: 1000,
        cacheCreationTokens: 200,
        models: ['kimi-k2'],
      },
    ]);
  });

  it('should default turn usages to an empty array when an older engine omits the field', () => {
    // Given a snapshot from an older engine (no turn_usages field at all)
    // When the snapshot maps to the app shape
    const app = toAppSnapshot(idleSnapshot, 'session-1');

    // Then the app snapshot seeds an empty usage map — nothing to fold
    expect(app.turnUsages).toEqual([]);
  });

  it('should keep turn usages intact alongside a live in-flight turn when a snapshot carries both', () => {
    // Given a snapshot whose envelope carries BOTH a live in-flight turn
    // (a session reopened mid-run) and the folded usage of an earlier
    // completed turn — the composition the projector seeds together
    const wire: SnapshotWire = {
      ...idleSnapshot,
      in_flight_turn: {
        turn_id: 'turn-live',
        assistant_text: 'working ',
        thinking_text: '',
        running_tools: [],
      },
      turn_usages: [
        {
          promptId: 'turn-1',
          inputTokens: 10,
          outputTokens: 5,
          cacheReadTokens: 1000,
          cacheCreationTokens: 200,
          models: ['kimi-k2'],
        },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then BOTH survive the mapping: the in-flight turn renders live while
    // the completed turns' usage stays seedable — an in-flight turn must
    // never wipe the historical pills
    expect(app.inFlightTurn).not.toBeNull();
    expect(app.inFlightTurn!.turnId).toBe('turn-live');
    expect(app.turnUsages).toEqual([
      {
        promptId: 'turn-1',
        inputTokens: 10,
        outputTokens: 5,
        cacheReadTokens: 1000,
        cacheCreationTokens: 200,
        models: ['kimi-k2'],
      },
    ]);
  });

  it('should map current_turn_started_at onto the in-flight turn as startedAtMs', () => {
    // Given a snapshot of a mid-turn session whose in-flight turn carries
    // the engine's real start time (current_turn_started_at, epoch-ms)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      in_flight_turn: {
        turn_id: 'turn-live',
        assistant_text: 'working ',
        thinking_text: '',
        running_tools: [],
        current_prompt_id: 'turn-3',
        current_turn_started_at: 1750000000000,
      },
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the startedAtMs anchor rides the in-flight turn — the streaming
    // header ticks the "Working for X" elapsed from the server's anchor
    expect(app.inFlightTurn).not.toBeNull();
    expect(app.inFlightTurn!.promptId).toBe('turn-3');
    expect(app.inFlightTurn!.startedAtMs).toBe(1750000000000);
  });

  it('should leave startedAtMs absent when the in-flight turn carries no start stamp', () => {
    // Given a snapshot from an engine that predates the current_turn_started_at
    // field (or a log that cannot resolve the open turn)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      in_flight_turn: {
        turn_id: 'turn-live',
        assistant_text: 'working ',
        thinking_text: '',
        running_tools: [],
        current_prompt_id: 'turn-3',
      },
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the app shape stays honestly without an anchor — the header then
    // falls back to the bare "Working…" label instead of a fabricated elapsed
    expect(app.inFlightTurn).not.toBeNull();
    expect(app.inFlightTurn!.startedAtMs).toBeUndefined();
  });

  it('should map snapshot todos onto the app snapshot when present', () => {
    // Given a snapshot whose session envelope carries the folded todo list
    // (the rust engine's wire-folded state, kept in sync by restore/undo)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      todos: [
        { title: 'Implement G7', status: 'in_progress' },
        { title: 'Write tests', status: 'done' },
        { title: 'Commit', status: 'pending' },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then the app snapshot carries the todos with normalized statuses — a
    // refreshed client renders the panel from the snapshot instead of
    // deriving it from the (possibly compacted) message history
    expect(app.todos).toEqual([
      { title: 'Implement G7', status: 'in_progress' },
      { title: 'Write tests', status: 'done' },
      { title: 'Commit', status: 'pending' },
    ]);
  });

  it('should normalize completed-status todos to done when mapping snapshot todos', () => {
    // Given a snapshot whose todos carry the Claude-style 'completed' status
    // (the same dual-dialect the message-derivation helper tolerates)
    const wire: SnapshotWire = {
      ...idleSnapshot,
      todos: [
        { title: 'Legacy done', status: 'done' },
        { title: 'Claude done', status: 'completed' },
      ],
    };

    // When the snapshot maps to the app shape
    const app = toAppSnapshot(wire, 'session-1');

    // Then both spellings normalize to the app's 'done' status
    expect(app.todos).toEqual([
      { title: 'Legacy done', status: 'done' },
      { title: 'Claude done', status: 'done' },
    ]);
  });

  it('should keep todos undefined when the engine omits them', () => {
    // Given a snapshot from an older engine (no todos field at all)
    // When the snapshot maps to the app shape
    const app = toAppSnapshot(idleSnapshot, 'session-1');

    // Then the app snapshot carries no todos — the composable falls back to
    // the message-history derivation (absent ≠ empty: a snapshot that carries
    // an explicitly empty list must still report the empty list)
    expect(app.todos).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// toAppMessage (rust message row wire → app shape)
// ---------------------------------------------------------------------------

describe('toAppMessage (rust message row wire → app shape)', () => {
  type MessageWire = Parameters<typeof toAppMessage>[0];

  /** Minimal assistant row — the neighbor describes build their wires on
   *  this same shape (`id`, `role`, `content`, `agentId`). */
  const baseMessage: MessageWire = {
    id: 'msg-0',
    role: 'assistant',
    content: [{ type: 'text', text: 'hello' }],
    agentId: 'main',
  };

  it('should carry the prompt id on message rows when the engine stamps one', () => {
    // Given an assistant row stamped with the turn's prompt id (`turn-<n>`
    // from the usage fold)
    // When the row maps to the app shape
    const message = toAppMessage(
      { ...baseMessage, role: 'assistant', promptId: 'turn-1' },
      'session-1',
    );

    // Then the app row carries the prompt id verbatim — the transcript key
    // that links the row to its per-turn usage pill
    expect(message.promptId).toBe('turn-1');
  });

  it('should leave the prompt id undefined when the engine does not stamp one', () => {
    // Given an assistant row with no prompt id (older engine, or a row that
    // existed before the per-turn attribution landed)
    // When the row maps to the app shape
    const message = toAppMessage({ ...baseMessage, role: 'assistant' }, 'session-1');

    // Then the app row leaves the field unset — no fabricated attribution
    expect(message.promptId).toBeUndefined();
  });

  it('should carry the activation record on user rows when the engine stamps with_skills', () => {
    // Given a user row stamped with the admission-time activation list
    // (ADR-006 — the chip-rebuild record for reloaded sessions)
    // When the row maps to the app shape
    const message = toAppMessage(
      {
        ...baseMessage,
        role: 'user',
        with_skills: [
          { name: 'writing-plans', target: '/skills/writing-plans/SKILL.md', trigger: 'user-inline' },
        ],
      },
      'session-1',
    );

    // Then the app row carries the record so the bubble can rebuild [name]
    // markers as chips after a reload
    expect(message.withSkills).toEqual([
      { name: 'writing-plans', target: '/skills/writing-plans/SKILL.md', trigger: 'user-inline' },
    ]);
  });

  it('should leave the activation record undefined when the engine does not stamp one', () => {
    // Given a user row with no with_skills (older engine, or a message that
    // activated no skills)
    // When the row maps to the app shape
    const message = toAppMessage({ ...baseMessage, role: 'user' }, 'session-1');

    // Then the app row leaves the field unset — markers render as plain text
    expect(message.withSkills).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// rpcContentToAppContent (rust message row content → app content, R1)
// ---------------------------------------------------------------------------

describe('rpcContentToAppContent (rust message row content → app content)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  /** Map one wire row's `content` through the message.list read path. */
  async function listedContent(content: unknown) {
    const { api, call } = createApi();
    call.mockResolvedValue({
      messages: [{ id: 'msg-0', role: 'assistant', content, agentId: 'main' }],
      hasMore: false,
    });
    const page = await api.listMessages('session-1');
    return page.items[0]!.content;
  }

  it('should map protocol tool_use parts into toolUse app content when listing messages', async () => {
    // Given an assistant row whose content carries the protocol `tool_use`
    // part the rust projection emits (tool_call_id / tool_name / input)
    // When the wire row maps through message.list
    const content = await listedContent([
      { type: 'tool_use', tool_call_id: 'c1', tool_name: 'Bash', input: { command: 'pwd' } },
    ]);
    // Then the part becomes the app toolUse block with camelCase fields
    expect(content).toEqual([
      { type: 'toolUse', toolCallId: 'c1', toolName: 'Bash', input: { command: 'pwd' } },
    ]);
  });

  it('should map protocol tool_result parts into toolResult app content when listing messages', async () => {
    // Given a tool row carrying the protocol `tool_result` part (bound to
    // the originating call id, with the optional is_error flag)
    const content = await listedContent([
      { type: 'tool_result', tool_call_id: 'c1', output: 'out', is_error: true },
    ]);
    // When the wire row maps through message.list
    // Then the part becomes the app toolResult block with camelCase fields
    expect(content).toEqual([
      { type: 'toolResult', toolCallId: 'c1', output: 'out', isError: true },
    ]);
  });

  it('should map protocol thinking parts into thinking app content when listing messages', async () => {
    // Given an assistant row carrying the protocol `thinking` part (the
    // projection of the persisted `think` part)
    const content = await listedContent([{ type: 'thinking', thinking: 'reasoning' }]);
    // When the wire row maps through message.list
    // Then the part becomes the app thinking block verbatim
    expect(content).toEqual([{ type: 'thinking', thinking: 'reasoning' }]);
  });

  it('should map image parts with a url source into the app source shape when listing messages', async () => {
    // Given an image part in the protocol source shape ({kind:'url',url})
    const content = await listedContent([
      { type: 'image', source: { kind: 'url', url: 'https://x/a.png' } },
    ]);
    // When the wire row maps through message.list
    // Then the part becomes the app image block with the same source shape
    expect(content).toEqual([{ type: 'image', source: { kind: 'url', url: 'https://x/a.png' } }]);
  });

  it('should map protocol file parts into file app content when listing messages', async () => {
    // Given a file part with the protocol file_id/name/media_type/size fields
    const content = await listedContent([
      { type: 'file', file_id: 'f1', name: 'a.pdf', media_type: 'application/pdf', size: 10 },
    ]);
    // When the wire row maps through message.list
    // Then the part becomes the app file block with camelCase fields
    expect(content).toEqual([
      { type: 'file', fileId: 'f1', name: 'a.pdf', mediaType: 'application/pdf', size: 10 },
    ]);
  });

  it('should degrade inline data-only file parts instead of emitting a broken fileId when listing messages', async () => {
    // Given the context-memory inline file form (resolved bytes, no store
    // id — the server degrades it to `{data,...}` since the protocol file
    // part requires a file_id)
    const content = await listedContent([
      { type: 'file', data: 'aGVsbG8=', name: 'memo.txt', media_type: 'text/plain', size: 5 },
    ]);
    // When the wire row maps through message.list
    // Then the part degrades to unknown (raw preserved, no data dropped)
    // rather than emitting an app file block that cannot fetch by id
    expect(content).toEqual([
      { type: 'unknown', raw: { type: 'file', data: 'aGVsbG8=', name: 'memo.txt', media_type: 'text/plain', size: 5 } },
    ]);
  });

  it('should degrade unknown part types to unknown content without throwing when listing messages', async () => {
    // Given a row whose content mixes a protocol part with a part of no
    // known protocol type and a non-object element
    const content = await listedContent([{ type: 'text', text: 'see' }, { type: 'mystery', n: 1 }, 42]);
    // When the wire row maps through message.list
    // Then the unknown bits degrade to {type:'unknown'} (raw preserved) and
    // the mapping never throws (既有行为护栏)
    expect(content).toEqual([
      { type: 'text', text: 'see' },
      { type: 'unknown', raw: { type: 'mystery', n: 1 } },
      { type: 'unknown', raw: 42 },
    ]);
  });
});

describe('WsRpcApi listGlobalMcpServers (merged source view, PS1.1)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  /** Minimal complete config for a stdio server. */
  function stdioConfig(command: string) {
    return { transport: 'stdio' as const, command, args: [] };
  }

  it('should collapse same-name user and workspace pool entries into one card with the effective source when both layers define the name', async () => {
    // Given the no-shadowing pool view returns the same name from both
    // pools, and the layered config names the effective layer
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [
          { name: 'search', transport: 'stdio', status: 'connected', toolCount: 3, pool: 'workspace', source: 'project' },
          { name: 'search', transport: 'stdio', status: 'connected', toolCount: 3, pool: 'user', source: 'user' },
        ];
      }
      if (method === 'mcp.config.list') {
        return [
          { name: 'search', source: 'projectRoot', config: stdioConfig('uvx'), effective: { enabled: true } },
          { name: 'search', source: 'user', config: stdioConfig('npx'), effective: { enabled: true } },
        ];
      }
      return undefined;
    });
    // When the settings panel loads its merged list
    const servers = await api.listGlobalMcpServers();
    // Then one card per name remains, carrying the effective definition's
    // source layer and the shadowed pool provenance
    expect(servers).toHaveLength(1);
    expect(servers[0]!.name).toBe('search');
    expect(servers[0]!.source).toBe('projectRoot');
    expect(servers[0]!.shadowedPools).toEqual(['user']);
    // and the joined config is the effective (highest-precedence) layer's
    expect(servers[0]!.config?.command).toBe('uvx');
  });

  it('should keep distinct names as independent cards with their own source labels when no shadowing exists', async () => {
    // Given pools hold two distinct names from different layers
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [
          { name: 'graph', transport: 'stdio', status: 'connected', toolCount: 2, pool: 'user', source: 'user' },
          { name: 'docs', transport: 'http', status: 'pending', toolCount: 0, pool: 'workspace', source: 'project' },
        ];
      }
      if (method === 'mcp.config.list') {
        return [
          { name: 'graph', source: 'user', config: stdioConfig('codegraph'), effective: { enabled: true } },
          { name: 'docs', source: 'project', config: { transport: 'http' as const, url: 'https://docs.example.test/mcp' }, effective: { enabled: true } },
        ];
      }
      return undefined;
    });
    // When the settings panel loads its merged list
    const servers = await api.listGlobalMcpServers();
    // Then both names list once each, each labeled with its own layer,
    // and neither carries shadowed provenance
    expect(servers.map((s) => [s.name, s.source, s.shadowedPools])).toEqual([
      ['graph', 'user', undefined],
      ['docs', 'project', undefined],
    ]);
  });

  it('should list config-layer entries with no pool slot as disconnected cards with their source when the engine has not loaded a layer', async () => {
    // Given the pool view is empty but a user-layer config exists
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') return [];
      if (method === 'mcp.config.list') {
        return [
          { name: 'unloaded', source: 'user', config: stdioConfig('npx'), effective: { enabled: true } },
        ];
      }
      return undefined;
    });
    // When the settings panel loads its merged list
    const servers = await api.listGlobalMcpServers();
    // Then the entry still lists as an editable unloaded card, sourced
    expect(servers.map((s) => [s.name, s.source, s.status])).toEqual([
      ['unloaded', 'user', 'disconnected'],
    ]);
  });
});

describe('WsRpcApi listGlobalMcpServers (merged source view — shadowing direction)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  it('should keep the workspace entry as the effective card and mark the user pool as shadowed when the config view has no match', async () => {
    // Given both pools list the same name and the layered config view is
    // empty (no per-layer source to join) — the pool entries' own source
    // is the only precedence signal
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [
          { name: 'search', transport: 'stdio', status: 'connected', toolCount: 1, pool: 'workspace', source: 'project' },
          { name: 'search', transport: 'stdio', status: 'connected', toolCount: 1, pool: 'user', source: 'user' },
        ];
      }
      return undefined;
    });
    // When the settings panel loads its merged list
    const servers = await api.listGlobalMcpServers();
    // Then the project-layer (workspace pool) entry is the effective card
    // and the user pool is the shadowed provenance — never the reverse
    expect(servers).toHaveLength(1);
    expect(servers[0]!.source).toBe('project');
    expect(servers[0]!.shadowedPools).toEqual(['user']);
  });

  it('should join plugin runtime names between the pool view and the config view when a bundled server runs under its qualified name', async () => {
    // Given a plugin-bundled server listed under its plugin-qualified
    // runtime name in both the pool view and the config view
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [
          { name: 'plugin-datasource:data', transport: 'stdio', status: 'connected', toolCount: 5, pool: 'plugin', source: 'plugin' },
        ];
      }
      if (method === 'mcp.config.list') {
        return [
          { name: 'data', runtimeName: 'plugin-datasource:data', source: 'plugin', config: { transport: 'stdio' as const, command: 'node' }, effective: { enabled: true } },
        ];
      }
      return undefined;
    });
    // When the settings panel loads its merged list
    const servers = await api.listGlobalMcpServers();
    // Then the plugin card carries the plugin source label and its
    // bundled config joins by runtime name
    expect(servers).toHaveLength(1);
    expect(servers[0]!.name).toBe('plugin-datasource:data');
    expect(servers[0]!.source).toBe('plugin');
    expect(servers[0]!.config?.command).toBe('node');
  });
});

describe('WsRpcApi listGlobalMcpTools (full tool view, SDR-S005)', () => {
  /** Build a WsRpcApi whose transport is a mocked `client.call`. */
  function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    return { api, call };
  }

  it('should include disabled tools with their enabled flag when the server advertises them with enabled false', async () => {
    // Given one server advertising three tools, one carrying enabled:false
    const { api, call } = createApi();
    call.mockImplementation(async (method: string) => {
      if (method === 'mcp.status') {
        return [{ name: 'atlas', transport: 'stdio', status: 'connected', toolCount: 3, pool: 'user', source: 'user' }];
      }
      if (method === 'mcp.tools') {
        return {
          connected: true,
          tools: [
            { name: 'search', description: 'find', enabled: true },
            { name: 'delete', description: 'destroy', enabled: false },
            { name: 'read', description: 'peek', enabled: true },
          ],
        };
      }
      return undefined;
    });
    // When the settings panel loads the tool list
    const tools = await api.listGlobalMcpTools();
    // Then all three tools are listed — a disabled tool stays visible with
    // its effective enabled verdict, so its switch can be flipped back on
    // (SDR-S005 PS3.2)
    expect(tools.map((t) => [t.name, t.mcpServerId, t.enabled])).toEqual([
      ['search', 'atlas', true],
      ['delete', 'atlas', false],
      ['read', 'atlas', true],
    ]);
  });
});

// --- agent profile RPC mapping (base-prompt override fields, D21/US-A8) ---

describe('toAppAgentProfile (agent.*Profile wire row → app shape)', () => {
  const baseRow = {
    name: 'agent',
    source: 'builtin' as const,
    builtin: true,
    essential: true,
    enabled: true,
    has_override: false,
  };

  it('should carry the override disclosure when SYSTEM.md exists', () => {
    // Given an essential row whose RPC response carries the override fields
    const row = {
      ...baseRow,
      base_prompt_override: true,
      base_prompt_template: 'CUSTOM_LAYER ${base_prompt}',
    };

    // When the row maps onto the app shape
    const app = toAppAgentProfile(row);

    // Then both fields survive the mapping
    expect(app.basePromptOverride).toBe(true);
    expect(app.basePromptTemplate).toBe('CUSTOM_LAYER ${base_prompt}');
  });

  it('should keep the template absent when no override exists', () => {
    const app = toAppAgentProfile({ ...baseRow, base_prompt_override: false });

    expect(app.basePromptOverride).toBe(false);
    expect(app.basePromptTemplate).toBeUndefined();
  });

  it('should map the default template seed when no override exists', () => {
    // Given the essential row's RPC response carrying the builtin default
    // template's raw text (placeholders intact)
    const app = toAppAgentProfile({
      ...baseRow,
      base_prompt_override: false,
      base_prompt_default_template: 'You are … {{cwd}}',
    });

    // Then the seed survives the mapping
    expect(app.basePromptOverride).toBe(false);
    expect(app.basePromptTemplate).toBeUndefined();
    expect(app.basePromptDefaultTemplate).toBe('You are … {{cwd}}');
  });

  it('should map the default template alongside an active override', () => {
    const app = toAppAgentProfile({
      ...baseRow,
      base_prompt_override: true,
      base_prompt_template: 'CUSTOM_LAYER ${base_prompt}',
      base_prompt_default_template: 'You are … {{cwd}}',
    });

    expect(app.basePromptOverride).toBe(true);
    expect(app.basePromptTemplate).toBe('CUSTOM_LAYER ${base_prompt}');
    expect(app.basePromptDefaultTemplate).toBe('You are … {{cwd}}');
  });

  it('should send the override body on the update wire when provided', async () => {
    // Given a WsRpcApi whose transport captures the agent.updateProfile payload
    const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
    const call = vi.fn<RpcCall>();
    (api as unknown as { client: { call: RpcCall } }).client = { call };
    call.mockResolvedValue({ ...baseRow });

    // When the essential row is updated with an override body
    await api.updateAgent('agent', { basePromptTemplate: 'WRITTEN ${base_prompt}' });

    // Then the wire carries the snake_case field
    expect(call).toHaveBeenCalledWith('agent.updateProfile', {
      name: 'agent',
      base_prompt_template: 'WRITTEN ${base_prompt}',
    });
  });
});
