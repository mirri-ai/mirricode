// apps/mirri-web/test/ws-rpc-api-recovery.test.ts
// Given the rust engine's interrupted-task recovery faces (ADR-004 pull
// model: `recovery.list`, `recovery.resume`, `recovery.resumeAll`,
// `recovery.dismiss`, `recovery.dismissAll`), the WsRpcApi adapters must call
// exactly the authoritative wire methods with snake_case params and map the
// wire rows onto the AppRecovery* shapes the notice card and session-tree
// badge consume. The list rows already arrive camelCase (engine-side
// serialization); `resume` passes the message-send fire result through
// verbatim (review finding 7 — no client-side turnId fabrication).

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';

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

describe('WsRpcApi interrupted-task recovery', () => {
  it('Given the engine answers recovery.list with interruption rows, When the list is fetched, Then the rows map onto AppRecoveryList', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      interruptions: [
        {
          sessionId: 's-1',
          workspaceKey: '/w/alpha',
          workspaceName: 'Alpha',
          title: 'Fix the login bug',
          interruptedAt: 1_700_000_000_000,
          cause: 'service_exit',
          pendingBackgroundHint: 'A background task may have finished',
        },
        {
          sessionId: 's-2',
          workspaceKey: '/w/beta',
          workspaceName: 'Beta',
          title: 'Write the report',
          interruptedAt: 1_700_000_100_000,
          cause: 'graceful_exit',
        },
      ],
      expiresAt: 1_700_000_500_000,
    });

    const list = await api.listRecovery!();

    expect(call).toHaveBeenCalledWith('recovery.list', {});
    expect(list).toEqual({
      interruptions: [
        {
          sessionId: 's-1',
          workspaceKey: '/w/alpha',
          workspaceName: 'Alpha',
          title: 'Fix the login bug',
          interruptedAt: 1_700_000_000_000,
          cause: 'service_exit',
          pendingBackgroundHint: 'A background task may have finished',
        },
        {
          sessionId: 's-2',
          workspaceKey: '/w/beta',
          workspaceName: 'Beta',
          title: 'Write the report',
          interruptedAt: 1_700_000_100_000,
          cause: 'graceful_exit',
        },
      ],
      expiresAt: 1_700_000_500_000,
    });
  });

  it('Given recovery.list answers null or omits rows, When the list is fetched, Then the caller gets an empty list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const list = await api.listRecovery!();

    expect(call).toHaveBeenCalledWith('recovery.list', {});
    expect(list).toEqual({ interruptions: [] });
  });

  it('Given a session to resume, When resumeInterrupted is called, Then recovery.resume goes out with the snake_case session_id and the fire result passes through verbatim', async () => {
    const { api, call } = createApi();
    const fireResult = { fired: true, prompt_id: 'p-9', queued: false };
    call.mockResolvedValue(fireResult);

    const result = await api.resumeInterrupted!('s-1');

    expect(call).toHaveBeenCalledWith('recovery.resume', { session_id: 's-1' });
    expect(result).toBe(fireResult);
  });

  it('Given multiple sessions to resume, When resumeAllInterrupted is called, Then recovery.resumeAll returns the accepted queued failed counts', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      accepted: 2,
      queued: 1,
      failed: [{ sessionId: 's-3', error: 'session not found' }],
    });

    const result = await api.resumeAllInterrupted!();

    expect(call).toHaveBeenCalledWith('recovery.resumeAll', {});
    expect(result).toEqual({
      accepted: 2,
      queued: 1,
      failed: [{ sessionId: 's-3', error: 'session not found' }],
    });
  });

  it('Given a session to dismiss, When dismissInterrupted is called, Then recovery.dismiss goes out with the snake_case session_id and resolves void', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.dismissInterrupted!('s-1');

    expect(call).toHaveBeenCalledWith('recovery.dismiss', { session_id: 's-1' });
  });

  it('Given every session to dismiss, When dismissAllInterrupted is called, Then recovery.dismissAll goes out with no params and resolves void', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ ok: true });

    await api.dismissAllInterrupted!();

    expect(call).toHaveBeenCalledWith('recovery.dismissAll', {});
  });

  it('Given the engine rejects a recovery call, When resumeInterrupted is called, Then the error propagates to the caller', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('session not found'));

    await expect(api.resumeInterrupted!('s-1')).rejects.toThrow('session not found');
  });
});
