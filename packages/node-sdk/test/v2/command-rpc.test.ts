/**
 * The contributed-command RPC that the v2 client serves through
 * `listCommands` / `runCommand` — the SDK door onto the engine's
 * `IAgentCommandService` (agent scope), a v2-only seam with no v1
 * counterpart (the base class answers `not_implemented`).
 *
 * Like the session-mutation tests, these drive the RPC client directly
 * against the in-process engine. Contributions are registered by reaching
 * the live session's main-agent scope through the client's `engineAccessor`
 * escape hatch (`getLiveSessionById` + the agent lifecycle service), the
 * same route the "how to register a command" snippet in the original change
 * prescribes.
 */
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import {
  ensureMainAgent,
  getLiveSessionById,
  IAgentCommandService,
  IAgentLifecycleService,
  MAIN_AGENT_ID,
  type CommandContribution,
  type IAgentScopeHandle,
  type IDisposable,
  type ISessionScopeHandle,
} from '@mirri-ai/agent-core-v2';

import { SDKRpcClientV2 } from '#/sdk-rpc-client-v2';

const clients: SDKRpcClientV2[] = [];
/** Every contributed command registered in this file, disposed after each test. */
const commandRegistrations: IDisposable[] = [];

afterEach(async () => {
  for (const registration of commandRegistrations.splice(0)) {
    registration.dispose();
  }
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

/** Creates a session and returns its id (each caller gets its own workDir). */
async function createLiveSession(client: SDKRpcClientV2): Promise<{ sessionId: string }> {
  const workDir = mkdtempSync(join(tmpdir(), 'mirri-work-'));
  const created = await client.createSession({ workDir });
  return { sessionId: created.id };
}

/**
 * The live session's main agent scope handle. The engine materializes the
 * main agent lazily, so a bare `createSession` does not create it yet:
 * create-or-get through `ensureMainAgent` (the same helper the client's own
 * `agentScope` uses) instead of assuming presence.
 */
async function mainAgentScopeOf(
  client: SDKRpcClientV2,
  sessionId: string,
): Promise<IAgentScopeHandle> {
  const session: ISessionScopeHandle | undefined = getLiveSessionById(
    client.engineAccessor,
    sessionId,
  );
  if (session === undefined) {
    throw new Error(`expected a live session, got none for "${sessionId}"`);
  }
  return (
    session.accessor.get(IAgentLifecycleService).get(MAIN_AGENT_ID) ??
    (await ensureMainAgent(session))
  );
}

/**
 * Registers a contributed command on the session's main agent and returns
 * the registration handle. The handle is also tracked file-wide, so a test
 * that forgets to dispose it still cannot leak into a later test.
 */
async function contributeTestCommand(
  client: SDKRpcClientV2,
  sessionId: string,
  contribution: CommandContribution,
): Promise<IDisposable> {
  const agent = await mainAgentScopeOf(client, sessionId);
  const registration = agent.accessor.get(IAgentCommandService).register(contribution);
  commandRegistrations.push(registration);
  return registration;
}

describe('SDKRpcClientV2 contributed-command RPC', () => {
  it('should list no commands when nothing has been registered', async () => {
    const client = createClient();
    const { sessionId } = await createLiveSession(client);

    await expect(client.listCommands({ sessionId })).resolves.toEqual([]);
  });

  it('should list a registered command (name, description, source) and then drop it when its registration is disposed', async () => {
    const client = createClient();
    const { sessionId } = await createLiveSession(client);
    const registration = await contributeTestCommand(client, sessionId, {
      name: 'my-command',
      description: 'A test-only contributed command',
      run: () => {},
    });

    // The registered command shows up with every field the info contract carries.
    const listed = await client.listCommands({ sessionId });
    expect(listed).toEqual([
      { name: 'my-command', description: 'A test-only contributed command', source: 'engine' },
    ]);

    // Disposing the registration handle removes the command from the catalog.
    registration.dispose();
    const listedAfterDispose = await client.listCommands({ sessionId });
    expect(listedAfterDispose).toEqual([]);
  });

  it('should run a registered command passing the raw args through verbatim when the name matches', async () => {
    const client = createClient();
    const { sessionId } = await createLiveSession(client);
    const receivedArgs: string[] = [];
    await contributeTestCommand(client, sessionId, {
      name: 'record-args',
      run: (ctx) => {
        receivedArgs.push(ctx.args);
      },
    });

    await expect(
      client.runCommand({ sessionId, name: 'record-args', args: 'hello world' }),
    ).resolves.toBeUndefined();
    expect(receivedArgs).toEqual(['hello world']);

    // Omitting `args` hands the contribution the engine default (`''`).
    await client.runCommand({ sessionId, name: 'record-args' });
    expect(receivedArgs).toEqual(['hello world', '']);
  });

  it('should reject when the command name is not registered', async () => {
    const client = createClient();
    const { sessionId } = await createLiveSession(client);

    await expect(
      client.runCommand({ sessionId, name: 'never-registered-command' }),
    ).rejects.toThrow();
  });

  it('should reject listCommands and runCommand when the session does not exist', async () => {
    const client = createClient();

    await expect(client.listCommands({ sessionId: 'never-created' })).rejects.toThrow();
    await expect(
      client.runCommand({ sessionId: 'never-created', name: 'my-command' }),
    ).rejects.toThrow();
  });
});