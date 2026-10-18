// apps/mirri-web/test/ws-rpc-permission-rules.test.ts
// Given the rust engine's user-level permission rule CRUD faces (B9:
// `approval.listRules` / `approval.addRule` / `approval.removeRule`), the
// WsRpcApi adapters must call exactly the authoritative wire methods with
// the handler's param spellings and map the shared `{rules: [...]}` result
// onto the AppPermissionRule rows the Settings rules panel consumes.

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

describe('WsRpcApi permission rules (B9)', () => {
  it('should list rules through approval.listRules and map the wire rows', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({
      rules: [
        { decision: 'deny', scope: 'user', pattern: 'Bash' },
        { decision: 'allow', scope: 'user', pattern: 'Write(/tmp/**)', reason: 'scratch space' },
      ],
    });

    const rules = await api.listPermissionRules();

    expect(call).toHaveBeenCalledWith('approval.listRules', {});
    expect(rules).toEqual([
      { decision: 'deny', scope: 'user', pattern: 'Bash' },
      { decision: 'allow', scope: 'user', pattern: 'Write(/tmp/**)', reason: 'scratch space' },
    ]);
  });

  it('should answer an empty list when the engine carries no rules', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ rules: [] });

    const rules = await api.listPermissionRules();

    expect(rules).toEqual([]);
  });

  it('should add a rule through approval.addRule and return the updated list', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ rules: [{ decision: 'allow', scope: 'user', pattern: 'Bash' }] });

    const rules = await api.addPermissionRule({ decision: 'allow', pattern: 'Bash' });

    expect(call).toHaveBeenCalledWith('approval.addRule', {
      rule: { decision: 'allow', pattern: 'Bash' },
    });
    expect(rules[0].pattern).toBe('Bash');
  });

  it('should send the optional reason on addRule when one is given', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ rules: [] });

    await api.addPermissionRule({ decision: 'ask', pattern: 'Read', reason: 'review reads' });

    expect(call).toHaveBeenCalledWith('approval.addRule', {
      rule: { decision: 'ask', pattern: 'Read', reason: 'review reads' },
    });
  });

  it('should remove a rule through approval.removeRule keyed by pattern+decision', async () => {
    const { api, call } = createApi();
    call.mockResolvedValue({ rules: [] });

    await api.removePermissionRule('Bash', 'allow');

    expect(call).toHaveBeenCalledWith('approval.removeRule', {
      pattern: 'Bash', decision: 'allow',
    });
  });

  it('should propagate a rejected call to the caller', async () => {
    const { api, call } = createApi();
    call.mockRejectedValue(new Error('conflicting rule'));

    await expect(api.addPermissionRule({ decision: 'allow', pattern: 'Bash' })).rejects.toThrow('conflicting rule');
  });
});
