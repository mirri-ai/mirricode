// apps/mirri-web/test/ws-rpc-api-agent-profiles.test.ts
// Given the rust engine's agent-profile surface (`agent.listProfiles`
// answering the R3-6 {items, skipped} envelope and `agent.getProfile` the
// full ProfileEntry row), the WsRpcApi adapters must thread the real wire
// shapes back to the v1 AppAgentProfile/AppAgentProfileList shapes the
// Settings panel consumes — the six-value source enum, the Task-19/20
// detail fields (disallowed_tools, capabilities_required, subagents,
// suppressed_candidates), and the discovery pass's skipped files instead
// of the historical hardcoded empty list.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { JsonRpcCallError } from '../src/api/daemon/json-rpc-client';
import { WsRpcApi, toAppAgentProfile } from '../src/api/daemon/ws-rpc-api';
import { DaemonApiError } from '../src/api/errors';
import type { AppAgentProfile } from '../src/api/types';

/** The JsonRpcClient.call signature the adapter under test relies on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;

/** Build a WsRpcApi whose transport is a mocked `client.call`. */
function createApi(): { api: WsRpcApi; call: ReturnType<typeof vi.fn<RpcCall>> } {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>();
  (api as unknown as { client: { call: RpcCall } }).client = { call };
  return { api, call };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi agent profiles', () => {
  it('should map the full wire row when a profile answers agent.getProfile', () => {
    const mapped = toAppAgentProfile({
      name: 'guardian',
      description: 'Guards.',
      source: 'project',
      builtin: false,
      essential: false,
      enabled: false,
      has_override: true,
      file_path: '/ws/.mirri-code/agents/guardian.md',
      default_model: 'claude-sonnet-4-20250514',
      tools: ['Read'],
      disallowed_tools: ['Bash'],
      capabilities_required: ['code.read'],
      subagents: ['coder'],
      suppressed_candidates: [{ source: 'user', priority: 10, reason: 'disabled' }],
      when_to_use: 'When guarding.',
      system_prompt_template: 'You guard.',
      prompt_form: 'replacement',
      effective_system_prompt: 'You guard hard.',
    });

    const expected: AppAgentProfile = {
      name: 'guardian',
      description: 'Guards.',
      source: 'project',
      builtin: false,
      essential: false,
      enabled: false,
      hasOverride: true,
      filePath: '/ws/.mirri-code/agents/guardian.md',
      defaultModel: 'claude-sonnet-4-20250514',
      tools: ['Read'],
      disallowedTools: ['Bash'],
      capabilitiesRequired: ['code.read'],
      subagents: ['coder'],
      suppressedCandidates: [{ source: 'user', priority: 10, reason: 'disabled' }],
      whenToUse: 'When guarding.',
      systemPromptTemplate: 'You guard.',
      promptForm: 'replacement',
      effectiveSystemPrompt: 'You guard hard.',
    };
    expect(mapped).toEqual(expected);
  });

  it('should leave the optional detail fields absent when the wire row omits them', () => {
    const mapped = toAppAgentProfile({
      name: 'scout',
      source: 'builtin',
      builtin: true,
      essential: false,
      enabled: true,
      has_override: false,
    });
    expect(mapped.disallowedTools).toBeUndefined();
    expect(mapped.capabilitiesRequired).toBeUndefined();
    expect(mapped.subagents).toBeUndefined();
    expect(mapped.suppressedCandidates).toBeUndefined();
  });

  it('should thread the wire skipped files when agent.listProfiles answers the envelope', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      items: [
        {
          name: 'scout',
          source: 'user',
          builtin: false,
          essential: false,
          enabled: true,
          has_override: false,
        },
      ],
      skipped: [{ path: '/home/agents/broken.md', reason: 'parse failed' }],
    });

    const listed = await api.listAgents();

    expect(call).toHaveBeenCalledWith('agent.listProfiles');
    expect(listed.profiles.map((profile) => profile.name)).toEqual(['scout']);
    expect(listed.skippedAgentFiles).toEqual([
      { path: '/home/agents/broken.md', reason: 'parse failed' },
    ]);
  });

  it('should fall back to an empty skipped list when the wire response omits it', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      items: [
        {
          name: 'scout',
          source: 'user',
          builtin: false,
          essential: false,
          enabled: true,
          has_override: false,
        },
      ],
    });

    const listed = await api.listAgents();

    expect(listed.skippedAgentFiles).toEqual([]);
    expect(listed.profiles).toHaveLength(1);
  });
});

describe('WsRpcApi agent profile writes', () => {
  /** A full wire row the create/update handlers answer with. */
  const wireRow = {
    name: 'scout',
    description: 'Scouts.',
    source: 'user',
    builtin: false,
    essential: false,
    enabled: true,
    has_override: false,
    file_path: '/home/.mirri-code/agents/scout.md',
    default_model: 'claude-sonnet-4-20250514',
    tools: ['Read'],
    disallowed_tools: ['Bash'],
    subagents: ['coder'],
    when_to_use: 'When scouting.',
    system_prompt_template: 'You scout.',
    effective_system_prompt: 'You scout hard.',
  };

  it('should send the snake_case create wire and map the returned row when createAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(wireRow);

    const created = await api.createAgent({
      name: 'scout',
      description: 'Scouts.',
      defaultModel: 'claude-sonnet-4-20250514',
      tools: ['Read'],
      disallowedTools: ['Bash'],
      subagents: ['coder'],
      whenToUse: 'When scouting.',
      systemPromptTemplate: 'You scout.',
    });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.createProfile', {
      name: 'scout',
      description: 'Scouts.',
      default_model: 'claude-sonnet-4-20250514',
      tools: ['Read'],
      disallowed_tools: ['Bash'],
      subagents: ['coder'],
      system_prompt_template: 'You scout.',
      when_to_use: 'When scouting.',
    });
    expect(created.name).toBe('scout');
    expect(created.defaultModel).toBe('claude-sonnet-4-20250514');
    expect(created.disallowedTools).toEqual(['Bash']);
    expect(created.subagents).toEqual(['coder']);
    expect(created.whenToUse).toBe('When scouting.');
    expect(created.systemPromptTemplate).toBe('You scout.');
  });

  it('should send the snake_case update fields beside the name when updateAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(wireRow);

    await api.updateAgent('scout', {
      description: 'Scouts harder.',
      defaultModel: 'claude-sonnet-4-20250514',
      tools: ['Read', 'Grep'],
      disallowedTools: [],
      subagents: ['coder'],
      whenToUse: 'When scouting.',
      systemPromptTemplate: 'You scout harder.',
    });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.updateProfile', {
      name: 'scout',
      description: 'Scouts harder.',
      default_model: 'claude-sonnet-4-20250514',
      tools: ['Read', 'Grep'],
      disallowed_tools: [],
      subagents: ['coder'],
      system_prompt_template: 'You scout harder.',
      when_to_use: 'When scouting.',
    });
  });

  it('should fire agent.deleteProfile without reading a row back when deleteAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true });

    await api.deleteAgent('scout');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.deleteProfile', { name: 'scout' });
  });

  it('should fire the bare-envelope lifecycle methods without reading a row back when enableAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.enableAgent('scout');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.enableProfile', { name: 'scout' });
  });

  it('should fire the bare-envelope lifecycle methods without reading a row back when disableAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.disableAgent('scout');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.disableProfile', { name: 'scout' });
  });

  it('should fire the bare-envelope lifecycle methods without reading a row back when resetAgent runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.resetAgent('coder');

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('agent.resetProfile', { name: 'coder' });
  });

  it('should surface the engine refusal code and msg through DaemonApiError when a write is rejected', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(
      new JsonRpcCallError({
        code: -32602,
        message: 'Cannot delete the essential profile "agent"',
      }),
    );

    const failure = await api.deleteAgent('agent').then(
      () => undefined,
      (error: unknown) => error,
    );

    expect(failure).toBeInstanceOf(DaemonApiError);
    expect(failure).toMatchObject({
      code: -32602,
      message: 'Cannot delete the essential profile "agent"',
    });
    // The panel's NOT_FOUND consumption pattern (getAgent) still sees the
    // raw JsonRpcCallError before this mapping — never double-wrapped.
    expect(failure).not.toBeInstanceOf(JsonRpcCallError);
  });

  it('should surface the not-found code through DaemonApiError when the engine answers -32001', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(
      new JsonRpcCallError({
        code: -32001,
        message: 'Agent profile "ghost" was not found',
      }),
    );

    const failure = await api.updateAgent('ghost', { description: 'x' }).then(
      () => undefined,
      (error: unknown) => error,
    );

    expect(failure).toBeInstanceOf(DaemonApiError);
    expect(failure).toMatchObject({
      code: -32001,
      message: 'Agent profile "ghost" was not found',
    });
  });
});
