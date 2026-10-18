// apps/mirri-web/test/ws-rpc-api-cron.test.ts
// Given the rust engine's cron management faces (B6: `cron.list`,
// `cron.delete`, `cron.trigger`), the WsRpcApi adapters must call exactly the
// authoritative wire methods with camelCase ids and translate the wire shapes
// back to the AppCronTask rows the Settings cron panel consumes. `cron.list`
// answers the `{tasks: [...]}` envelope (CronTasksResult), and timestamps
// stay epoch-ms end to end (no unit conversion — the panel formats them).

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

describe('WsRpcApi cron management', () => {
  it('Given the engine answers cron.list with the {tasks: [...]} envelope, When the list is fetched, Then the rows map onto AppCronTask', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        {
          id: 'cron-1', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
          timezone: 'Asia/Shanghai', stale: false,
          createdAt: 1_700_000_000_000, lastFiredAt: null, nextFireAt: 1_700_000_300_000,
        },
      ],
    });

    const tasks = await api.listCronTasks('s-1');

    expect(call).toHaveBeenCalledWith('cron.list', { sessionId: 's-1' });
    expect(tasks).toEqual([
      {
        id: 'cron-1', cron: '*/5 * * * *', prompt: 'tick', recurring: true,
        timezone: 'Asia/Shanghai', stale: false,
        createdAt: 1_700_000_000_000, lastFiredAt: null, nextFireAt: 1_700_000_300_000,
      },
    ]);
  });

  it('Given the engine answers cron.list with an empty {tasks: []} envelope, When the list is fetched, Then the panel gets an empty list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ tasks: [] });

    const tasks = await api.listCronTasks('s-1');

    expect(call).toHaveBeenCalledWith('cron.list', { sessionId: 's-1' });
    expect(tasks).toEqual([]);
  });

  it('Given cron.list answers a null result, When the list is fetched, Then the panel gets an empty list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const tasks = await api.listCronTasks('s-1');

    expect(tasks).toEqual([]);
  });

  it('should tolerate absent optional keys when the row is a fired one-shot', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      tasks: [
        { id: 'cron-2', cron: '0 9 1 1 *', prompt: 'once', recurring: false,
          stale: false, createdAt: 1_700_000_000_000 },
      ],
    });

    const tasks = await api.listCronTasks('s-1');

    expect(tasks[0]).toMatchObject({ id: 'cron-2', recurring: false });
    expect(tasks[0].lastFiredAt).toBeUndefined();
    expect(tasks[0].nextFireAt).toBeUndefined();
  });

  it('should call cron.delete with the camelCase ids and map the wire ack', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true, task_id: 'cron-1' });

    const ack = await api.deleteCronTask('s-1', 'cron-1');

    expect(call).toHaveBeenCalledWith('cron.delete', { sessionId: 's-1', taskId: 'cron-1' });
    expect(ack).toEqual({ deleted: true, taskId: 'cron-1' });
  });

  it('should call cron.trigger with the camelCase ids and map the wire ack', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ triggered: true, task_id: 'cron-1' });

    const ack = await api.triggerCronTask('s-1', 'cron-1');

    expect(call).toHaveBeenCalledWith('cron.trigger', { sessionId: 's-1', taskId: 'cron-1' });
    expect(ack).toEqual({ triggered: true, taskId: 'cron-1' });
  });

  it('should fall back to the requested id when the ack omits task_id', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: true });

    const ack = await api.deleteCronTask('s-1', 'cron-9');

    expect(ack).toEqual({ deleted: true, taskId: 'cron-9' });
  });

  it('should propagate a rejected cron.list call to the panel', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('session not found'));

    await expect(api.listCronTasks('s-1')).rejects.toThrow('session not found');
  });

  it('should reject when the delete ack does not confirm the removal', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ deleted: false, task_id: 'cron-1' });

    await expect(api.deleteCronTask('s-1', 'cron-1')).rejects.toThrow('did not confirm');
  });

  it('should reject when the trigger ack does not confirm the run', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ triggered: false, task_id: 'cron-1' });

    await expect(api.triggerCronTask('s-1', 'cron-1')).rejects.toThrow('did not confirm');
  });
});
