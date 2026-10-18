// apps/mirri-web/test/ws-rpc-api-tasks.test.ts
// Given the rust engine's B7 workspace task faces (`task.listWorkspace` —
// the live-session union answering the direct AgentTask serialization,
// and `task.detach` with the toolCallId addressing of a model Bash tool
// call answering the BackgroundTaskInfo snapshot), the WsRpcApi adapters
// must call exactly the authoritative wire methods and translate the wire
// shapes onto the AppTask rows the Tasks overview panel consumes. The v2
// REST adapter answers honest errors instead of degrading silently.

import { beforeEach, describe, expect, it, vi } from 'vitest';

import { DaemonMirriWebApi } from '../src/api/daemon/client';
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

describe('WsRpcApi.listWorkspaceTasks', () => {
  it('should map task.listWorkspace rows onto AppTask rows carrying sessionId when the engine answers the live-session union', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'task-a',
        session_id: 's-1',
        description: 'npm run watch',
        kind: { kind: 'process', command: 'npm run watch' },
        status: { status: 'running' },
        created_at: '2026-11-01T00:00:00Z',
        ended_at: null,
        output: 'building…',
        detached: true,
      },
    ]);

    const tasks = await api.listWorkspaceTasks({ activeOnly: true });

    expect(call).toHaveBeenCalledWith('task.listWorkspace', { activeOnly: true });
    expect(tasks).toHaveLength(1);
    expect(tasks[0]).toMatchObject({
      id: 'task-a',
      sessionId: 's-1',
      kind: 'bash',
      status: 'running',
      command: 'npm run watch',
      createdAt: '2026-11-01T00:00:00Z',
      outputPreview: 'building…',
      runInBackground: true,
    });
  });

  it('should translate the terminal statuses and the agent-kind discriminator when rows settle or track subagents', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'task-stopped',
        session_id: 's-1',
        description: 'stopped run',
        kind: { kind: 'process' },
        status: { status: 'stopped' },
        created_at: '2026-11-01T00:00:00Z',
        ended_at: '2026-11-01T00:01:00Z',
        output: '',
        detached: true,
      },
      {
        id: 'task-completed',
        session_id: 's-1',
        description: 'clean build',
        kind: { kind: 'process', command: 'pnpm build' },
        status: { status: 'completed', detail: { exit_code: 0 } },
        created_at: '2026-11-01T00:02:00Z',
        ended_at: '2026-11-01T00:03:00Z',
        output: 'done',
        detached: true,
      },
      {
        id: 'task-agent',
        session_id: 's-2',
        description: 'explore the tree',
        kind: { kind: 'agent', agentId: 'ag-7', subagentType: 'explore', model: 'expert' },
        status: { status: 'failed', detail: 'provider 500' },
        created_at: '2026-11-01T00:04:00Z',
        ended_at: '2026-11-01T00:05:00Z',
        output: '',
        detached: false,
      },
    ]);

    const tasks = await api.listWorkspaceTasks();

    expect(call).toHaveBeenCalledWith('task.listWorkspace', {});
    expect(tasks.map((task) => [task.id, task.status])).toEqual([
      ['task-stopped', 'cancelled'],
      ['task-completed', 'completed'],
      ['task-agent', 'failed'],
    ]);
    expect(tasks[0]).toMatchObject({ completedAt: '2026-11-01T00:01:00Z' });
    expect(tasks[2]).toMatchObject({
      kind: 'subagent',
      agentId: 'ag-7',
      subagentType: 'explore',
      model: 'expert',
      runInBackground: false,
    });
  });

  it('should pass the workspaceId scope through to the wire', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.listWorkspaceTasks({ workspaceId: 'ws-1', activeOnly: false });

    expect(call).toHaveBeenCalledWith('task.listWorkspace', { workspaceId: 'ws-1', activeOnly: false });
  });

  it('should answer an empty list when the engine has no live sessions', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const tasks = await api.listWorkspaceTasks();

    expect(tasks).toEqual([]);
  });
});

describe('WsRpcApi.listTasks', () => {
  // The rust `task.list` answers the SAME direct AgentTask serialization
  // `task.listWorkspace` serves (snake_case fields, tagged `status`/`kind`
  // enums — session_handlers.rs "TSV row 80"). The session dock's task rows
  // ride this adapter, so a v2-shaped mapper here leaks raw enum objects
  // into the UI (the kind badge renders `{"kind":"process",…}` JSON and
  // every status comparison misses, painting settled tasks as failed).
  it('should map task.list rows onto flat AppTask rows when the engine answers the AgentTask serialization', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'task-run',
        session_id: 's-1',
        description: 'pnpm watch',
        kind: { kind: 'process', command: 'pnpm watch' },
        status: { status: 'running' },
        created_at: '2026-11-01T00:00:00Z',
        ended_at: null,
        output: 'building…',
        detached: true,
      },
      {
        id: 'task-ok',
        session_id: 's-1',
        description: 'run tests',
        kind: { kind: 'process', command: 'pnpm test' },
        status: { status: 'completed', detail: { exit_code: 0 } },
        created_at: '2026-11-01T00:01:00Z',
        ended_at: '2026-11-01T00:02:00Z',
        output: 'all green',
        detached: true,
      },
    ]);

    const tasks = await api.listTasks('s-1');

    expect(call).toHaveBeenCalledWith('task.list', { sessionId: 's-1' });
    expect(tasks).toHaveLength(2);
    expect(tasks[0]).toMatchObject({
      id: 'task-run',
      sessionId: 's-1',
      kind: 'bash',
      status: 'running',
      command: 'pnpm watch',
      startedAt: '2026-11-01T00:00:00Z',
      runInBackground: true,
    });
    expect(tasks[1]).toMatchObject({
      id: 'task-ok',
      kind: 'bash',
      status: 'completed',
      command: 'pnpm test',
      completedAt: '2026-11-01T00:02:00Z',
      outputPreview: 'all green',
    });
  });

  it('should keep the agent-kind linkage fields the dock swarm grouping keys on when rows track subagents', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'task-agent',
        session_id: 's-1',
        description: 'explore the codebase',
        kind: {
          kind: 'agent',
          agentId: 'ag-9',
          subagentType: 'explore',
          parentToolCallId: 'toolu-3',
          swarmIndex: 1,
          model: 'fast',
        },
        status: { status: 'failed', detail: 'tool error' },
        created_at: '2026-11-01T00:00:00Z',
        ended_at: '2026-11-01T00:05:00Z',
        output: '',
        detached: true,
      },
    ]);

    const tasks = await api.listTasks('s-1');

    expect(tasks[0]).toMatchObject({
      id: 'task-agent',
      kind: 'subagent',
      status: 'failed',
      agentId: 'ag-9',
      subagentType: 'explore',
      parentToolCallId: 'toolu-3',
      swarmIndex: 1,
      model: 'fast',
      runInBackground: true,
    });
  });
});

describe('WsRpcApi.listTasks summary view', () => {
  it('should forward activeOnly to the wire when the caller scopes to running rows', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([]);

    await api.listTasks('s-1', undefined, { activeOnly: true });

    expect(call).toHaveBeenCalledWith('task.list', { sessionId: 's-1', activeOnly: true });
  });

  it('should map the truncated-summary row onto the AppTask output fields when the engine caps the output', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue([
      {
        id: 'task-big',
        session_id: 's-1',
        description: 'build',
        kind: { kind: 'process', command: 'x'.repeat(300) },
        status: { status: 'running' },
        created_at: '2026-11-01T00:00:00Z',
        ended_at: null,
        output: 'x'.repeat(2048),
        output_truncated: true,
        output_bytes: 900_000,
        detached: true,
      },
    ]);

    const tasks = await api.listTasks('s-1');

    expect(call).toHaveBeenCalledWith('task.list', { sessionId: 's-1' });
    expect(tasks[0]).toMatchObject({
      id: 'task-big',
      outputPreview: 'x'.repeat(2048),
      outputBytes: 900_000,
    });
  });
});

describe('WsRpcApi.getTaskOutput', () => {
  it('should fetch one task tail through task.output without listing the session', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue('tail of the output');

    const preview = await api.getTaskOutput('s-1', 'task-1', 4096);

    expect(call).toHaveBeenCalledTimes(1);
    expect(call).toHaveBeenCalledWith('task.output', {
      sessionId: 's-1',
      taskId: 'task-1',
      tail: 4096,
    });
    expect(preview).toBe('tail of the output');
  });
});

describe('WsRpcApi.detachTask', () => {
  it('should address task.detach by toolCallId and map the converted BackgroundTaskInfo onto AppTask when the engine converts the run', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      kind: 'process',
      command: 'sleep 30',
      taskId: 'bash-9',
      description: 'sleep 30',
      status: 'running',
      startedAt: 1_730_000_000_000,
      endedAt: null,
    });

    const task = await api.detachTask('s-1', { toolCallId: 'toolu-1' });

    expect(call).toHaveBeenCalledWith('task.detach', { sessionId: 's-1', toolCallId: 'toolu-1' });
    expect(task).not.toBeNull();
    expect(task).toMatchObject({
      id: 'bash-9',
      sessionId: 's-1',
      kind: 'bash',
      status: 'running',
      command: 'sleep 30',
      description: 'sleep 30',
    });
  });

  it('should keep the taskId addressing for shell-detach parity and map the killed status onto cancelled', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      kind: 'process',
      taskId: 'bash-1',
      description: 'watch',
      status: 'killed',
      startedAt: 1_730_000_000_000,
      endedAt: 1_730_000_009_000,
    });

    const task = await api.detachTask('s-1', { taskId: 'bash-1' });

    expect(call).toHaveBeenCalledWith('task.detach', { sessionId: 's-1', taskId: 'bash-1' });
    expect(task).toMatchObject({ id: 'bash-1', status: 'cancelled' });
  });

  it('should answer null when the detach finds nothing to convert', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue(null);

    const task = await api.detachTask('s-1', { toolCallId: 'toolu-gone' });

    expect(task).toBeNull();
  });
});

describe('DaemonMirriWebApi workspace task degradation', () => {
  function createRestApi(): DaemonMirriWebApi {
    return new DaemonMirriWebApi({
      serverHttpUrl: 'http://daemon.test',
      clientId: 'web_test',
      clientName: 'test',
      clientVersion: '0.0.0',
      clientUiMode: 'test',
    });
  }

  it('should reject listWorkspaceTasks with an honest error on the v2 REST adapter', async () => {
    const api = createRestApi();

    await expect(api.listWorkspaceTasks()).rejects.toThrow(/v3/);
  });

  it('should reject detachTask with an honest error on the v2 REST adapter', async () => {
    const api = createRestApi();

    await expect(api.detachTask('s-1', { toolCallId: 'toolu-1' })).rejects.toThrow(/v3/);
  });
});
