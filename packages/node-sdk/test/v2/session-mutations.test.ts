/**
 * The two v1-contract session mutations that the v2 client now serves:
 * `updateSessionMetadata` and `clearContext`.
 *
 * Both exist in v1's CoreAPI (`agent-core/src/rpc/core-api.ts`) but carry no
 * `MirriHarness` / `Session` entry point in this repository, so the tests
 * drive the RPC client directly — the same surface the base class exposes to
 * SDK hosts that do wire them.
 */
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { SDKRpcClientV2 } from '#/sdk-rpc-client-v2';

const clients: SDKRpcClientV2[] = [];

afterEach(async () => {
  await Promise.allSettled(clients.splice(0).map((client) => client.close()));
});

function createClient(): SDKRpcClientV2 {
  const client = new SDKRpcClientV2({
    homeDir: mkdtempSync(join(tmpdir(), 'mirri-sdk-session-')),
    identity: { userAgentProduct: 'mirri-sdk-test', version: '0.0.0' },
    uiMode: 'cli',
  });
  clients.push(client);
  return client;
}

describe('SDKRpcClientV2 session mutations', () => {
  it('should merge the patch into session metadata when updating a live session', async () => {
    const client = createClient();
    const workDir = mkdtempSync(join(tmpdir(), 'mirri-work-'));
    const created = await client.createSession({ workDir, metadata: { keep: 'me' } });

    await client.updateSessionMetadata({
      sessionId: created.id,
      metadata: { added: 'value' },
    });

    const [summary] = await client.listSessions({ workDir });
    // The patch merges: the pre-existing key survives alongside the new one.
    expect(summary?.metadata).toMatchObject({ keep: 'me', added: 'value' });
  });

  it('should reject a metadata update for a session that is not live', async () => {
    const client = createClient();

    await expect(
      client.updateSessionMetadata({ sessionId: 'never-created', metadata: { a: 1 } }),
    ).rejects.toThrow();
  });

  it('should clear the agent context without ending the session', async () => {
    const client = createClient();
    const workDir = mkdtempSync(join(tmpdir(), 'mirri-work-'));
    const created = await client.createSession({ workDir });

    await expect(client.clearContext({ sessionId: created.id })).resolves.toBeUndefined();

    // The session itself survives the context clear.
    const [summary] = await client.listSessions({ workDir });
    expect(summary?.id).toBe(created.id);
  });

  it('should reject a context clear for a session that is not live', async () => {
    const client = createClient();

    await expect(client.clearContext({ sessionId: 'never-created' })).rejects.toThrow();
  });
});
