// apps/mirri-web/test/ws-rpc-session-search.test.ts
// Given the rust engine's `session.search` RPC (the P10 global literal
// read face — snake_case wire: query / container.session_id / role /
// page_size / page_token, hits ordered time-desc with ~80-char snippet
// windows), the WsRpcApi adapter must send exactly that wire spelling and
// map the snake_case page onto the app's camelCase vocabulary — optional
// turn / next_page_token keys only when the engine carries them.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';

/** The JsonRpcClient.call signature the adapters under test rely on. */
type RpcCall = (method: string, params?: unknown) => Promise<unknown>;
type EventHandler = (event: unknown) => void;

function createApi(): {
  api: WsRpcApi;
  call: ReturnType<typeof vi.fn<RpcCall>>;
} {
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const call = vi.fn<RpcCall>().mockResolvedValue(undefined);
  (api as unknown as { client: unknown }).client = {
    call,
    on: vi.fn<(type: string, handler: EventHandler) => () => void>(),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  return { api, call };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('WsRpcApi searchSessions', () => {
  it('should send the snake_case wire params when the search runs', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ hits: [], mode: 'literal' });

    await api.searchSessions({ query: 'needle' });

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('session.search', { query: 'needle' });
  });

  it('should carry the optional filters in the wire spelling when they are set', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ hits: [], mode: 'literal' });

    await api.searchSessions({
      query: 'needle',
      sessionId: 's-1',
      role: 'assistant',
      pageSize: 50,
      pageToken: 'cursor',
    });

    expect(call).toHaveBeenCalledWith('session.search', {
      query: 'needle',
      container: { session_id: 's-1' },
      role: 'assistant',
      page_size: 50,
      page_token: 'cursor',
    });
  });

  it('should map the snake_case page onto the app vocabulary when hits return', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      hits: [
        {
          session_id: 's-1',
          workspace_id: 'ws-1',
          session_title: 'Deploy notes',
          agent_id: 'main',
          role: 'user',
          snippet: '…the needle…',
          time: 1735689600000,
          turn: 2,
        },
        {
          // A title-role hit: no turn ordinal, empty agent id.
          session_id: 's-2',
          workspace_id: 'ws-1',
          session_title: 'The runbook',
          agent_id: '',
          role: 'title',
          snippet: 'The runbook',
          time: 1735689600001,
        },
      ],
      mode: 'literal',
      next_page_token: 'next-cursor',
    });

    const page = await api.searchSessions({ query: 'needle' });

    expect(page).toEqual({
      hits: [
        {
          sessionId: 's-1',
          workspaceId: 'ws-1',
          sessionTitle: 'Deploy notes',
          agentId: 'main',
          role: 'user',
          snippet: '…the needle…',
          time: 1735689600000,
          turn: 2,
        },
        {
          sessionId: 's-2',
          workspaceId: 'ws-1',
          sessionTitle: 'The runbook',
          agentId: '',
          role: 'title',
          snippet: 'The runbook',
          time: 1735689600001,
        },
      ],
      mode: 'literal',
      nextPageToken: 'next-cursor',
    });
  });

  it('should omit the optional keys when the final page returns', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      hits: [
        {
          session_id: 's-1',
          workspace_id: 'ws-1',
          session_title: 'Deploy notes',
          agent_id: 'main',
          role: 'user',
          snippet: '…the needle…',
          time: 1735689600000,
        },
      ],
      mode: 'literal',
    });

    const page = await api.searchSessions({ query: 'needle' });

    expect(page.hits[0]).not.toHaveProperty('turn');
    expect(page).not.toHaveProperty('nextPageToken');
  });

  it('should propagate the engine error when the fingerprint mismatches', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('page_token fingerprint mismatch'));

    await expect(api.searchSessions({ query: 'other', pageToken: 'stale' })).rejects.toThrow(
      'page_token fingerprint mismatch',
    );
  });
});
