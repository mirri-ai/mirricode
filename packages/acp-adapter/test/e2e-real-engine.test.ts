/**
 * ACP e2e tests driven by a REAL v1 engine + FakeProviderServer.
 *
 * Unlike the existing `e2e-happy-path.test.ts` which uses a scripted
 * fake session, these tests boot a real `createMirriHarness` through
 * `V1EngineFixture`, wire it into an `AcpServer`, and drive the full
 * ACP protocol over an in-memory NDJSON pipe. The fake provider
 * returns scripted LLM responses via HTTP SSE.
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  AgentSideConnection,
  ClientSideConnection,
  ndJsonStream,
  type Client,
  type ContentBlock,
  type ReadTextFileRequest,
  type ReadTextFileResponse,
  type RequestPermissionRequest,
  type RequestPermissionResponse,
  type SessionNotification,
  type WriteTextFileRequest,
  type WriteTextFileResponse,
} from '@agentclientprotocol/sdk';

import {
  type EngineFixture,
  type FakeProviderServer,
  createFakeProviderServer,
  createV1EngineFixture,
  makeTempDir,
  removeTempDir,
} from '@mirri-ai/e2e-harness';

import { AcpServer } from '../src/server';
import { AUTHED_STATUS } from './_helpers/harness-stubs';

class CollectingClient implements Client {
  readonly updates: SessionNotification[] = [];
  readonly permissionRequests: RequestPermissionRequest[] = [];

  get promptUpdates(): readonly SessionNotification[] {
    return this.updates.filter(
      (n) =>
        (n.update as { sessionUpdate?: string }).sessionUpdate !==
        'available_commands_update',
    );
  }

  async requestPermission(p: RequestPermissionRequest): Promise<RequestPermissionResponse> {
    this.permissionRequests.push(p);
    return { outcome: { outcome: 'selected', optionId: 'approve' } };
  }
  async sessionUpdate(n: SessionNotification): Promise<void> {
    this.updates.push(n);
  }
  async writeTextFile(_p: WriteTextFileRequest): Promise<WriteTextFileResponse> {
    throw new Error('writeTextFile not implemented in e2e test client');
  }
  async readTextFile(_p: ReadTextFileRequest): Promise<ReadTextFileResponse> {
    throw new Error('readTextFile not implemented in e2e test client');
  }
}

function makeInMemoryStreamPair() {
  const clientToAgent = new TransformStream<Uint8Array, Uint8Array>();
  const agentToClient = new TransformStream<Uint8Array, Uint8Array>();
  return {
    agentStream: ndJsonStream(agentToClient.writable, clientToAgent.readable),
    clientStream: ndJsonStream(clientToAgent.writable, agentToClient.readable),
  };
}

const textBlock = (text: string): ContentBlock => ({ type: 'text', text });

/**
 * Wrap a V1EngineFixture's harness so `auth.status()` always returns
 * AUTHED_STATUS — the real harness has no OAuth token for the fake
 * provider, but ACP's `newSession` gates on auth.
 */
function withAuthedStub(fixture: EngineFixture): EngineFixture['rawHarness'] {
  const harness = fixture.rawHarness;
  return new Proxy(harness, {
    get(target, prop, receiver) {
      if (prop === 'auth') {
        // MirriAuthFacade's methods live on the class prototype, so spreading
        // the instance would drop them all. Override just `status` via a
        // Proxy and forward everything else to the real facade.
        return new Proxy(target.auth, {
          get(authTarget, authProp) {
            if (authProp === 'status') {
              return async () => AUTHED_STATUS;
            }
            return Reflect.get(authTarget, authProp);
          },
        });
      }
      return Reflect.get(target, prop, receiver);
    },
  }) as EngineFixture['rawHarness'];
}

describe('AcpServer e2e with real v1 engine', () => {
  let fakeProvider: FakeProviderServer;
  let fixture: EngineFixture;
  let workDir: string;
  const tempDirs: string[] = [];

  beforeEach(async () => {
    fakeProvider = await createFakeProviderServer();
    const homeDir = await makeTempDir();
    workDir = await makeTempDir();
    tempDirs.push(homeDir, workDir);
    fixture = await createV1EngineFixture({ fakeProvider, homeDir });
  });

  afterEach(async () => {
    await fixture.close();
    await fakeProvider.close();
    for (const dir of tempDirs.splice(0)) {
      await removeTempDir(dir);
    }
  });

  it('should drive initialize → newSession → prompt(end_turn) with real engine', async () => {
    fakeProvider.nextText('Hello from real engine');

    const authedHarness = withAuthedStub(fixture);
    const { agentStream, clientStream } = makeInMemoryStreamPair();
    new AgentSideConnection((c) => new AcpServer(authedHarness, c), agentStream);
    const collecting = new CollectingClient();
    const client = new ClientSideConnection(() => collecting, clientStream);

    // 1. initialize
    const init = await client.initialize({
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });
    expect(init.agentCapabilities?.mcpCapabilities?.http).toBe(true);

    // 2. session/new
    const newRes = await client.newSession({ cwd: workDir, mcpServers: [] });
    expect(newRes.sessionId).toBeTruthy();

    // 3. session/prompt
    const promptRes = await client.prompt({
      sessionId: newRes.sessionId,
      prompt: [textBlock('hello')],
    });
    expect(promptRes.stopReason).toBe('end_turn');

    // Allow stream to flush
    await new Promise((resolve) => setTimeout(resolve, 50));

    // Should have received agent_message_chunk updates
    const chunks = collecting.promptUpdates.filter(
      (n) =>
        (n.update as { sessionUpdate?: string }).sessionUpdate ===
        'agent_message_chunk',
    );
    expect(chunks.length).toBeGreaterThanOrEqual(1);

    // At least one chunk should contain the fake provider's text
    const texts = chunks.map(
      (n) => (n.update as { content?: { text?: string } }).content?.text ?? '',
    );
    expect(texts.join('')).toContain('Hello from real engine');

    // Verify the fake provider received a real HTTP request
    expect(fakeProvider.requests).toHaveLength(1);
  });

  it('should handle tool call with approval through ACP protocol', async () => {
    // First LLM call: model requests a tool call
    fakeProvider.nextToolCall('bash', { command: 'echo hello' });
    // Second LLM call: model produces final text after tool result
    fakeProvider.nextText('The command ran successfully');

    const authedHarness = withAuthedStub(fixture);
    const { agentStream, clientStream } = makeInMemoryStreamPair();
    new AgentSideConnection((c) => new AcpServer(authedHarness, c), agentStream);
    const collecting = new CollectingClient();
    const client = new ClientSideConnection(() => collecting, clientStream);

    await client.initialize({
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });
    const newRes = await client.newSession({ cwd: workDir, mcpServers: [] });

    const promptRes = await client.prompt({
      sessionId: newRes.sessionId,
      prompt: [textBlock('run echo hello')],
    });

    // The tool call should have triggered a permission request
    // (auto-approved by CollectingClient.requestPermission → 'allow')
    // The prompt should complete
    expect(promptRes.stopReason).toBe('end_turn');

    // Two LLM calls: first for tool_call, second for final text
    expect(fakeProvider.requests).toHaveLength(2);
    const firstBody = fakeProvider.requests[0]?.bodyJson as { tools?: unknown[] };
    expect(firstBody.tools).toBeDefined();
    expect(firstBody.tools!.length).toBeGreaterThan(0);
  });

  it('should support multi-turn conversation through ACP', async () => {
    fakeProvider.nextText('first response');
    fakeProvider.nextText('second response');

    const authedHarness = withAuthedStub(fixture);
    const { agentStream, clientStream } = makeInMemoryStreamPair();
    new AgentSideConnection((c) => new AcpServer(authedHarness, c), agentStream);
    const collecting = new CollectingClient();
    const client = new ClientSideConnection(() => collecting, clientStream);

    await client.initialize({
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });
    const newRes = await client.newSession({ cwd: workDir, mcpServers: [] });

    // First turn
    const res1 = await client.prompt({
      sessionId: newRes.sessionId,
      prompt: [textBlock('first question')],
    });
    expect(res1.stopReason).toBe('end_turn');

    // Second turn
    collecting.updates.length = 0;
    const res2 = await client.prompt({
      sessionId: newRes.sessionId,
      prompt: [textBlock('second question')],
    });
    expect(res2.stopReason).toBe('end_turn');

    expect(fakeProvider.requests).toHaveLength(2);
  });

  it('should stream multiple chunks through ACP session updates', async () => {
    fakeProvider.nextThinkThenText('Let me think...', 'Here is my answer');

    const authedHarness = withAuthedStub(fixture);
    const { agentStream, clientStream } = makeInMemoryStreamPair();
    new AgentSideConnection((c) => new AcpServer(authedHarness, c), agentStream);
    const collecting = new CollectingClient();
    const client = new ClientSideConnection(() => collecting, clientStream);

    await client.initialize({
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });
    const newRes = await client.newSession({ cwd: workDir, mcpServers: [] });

    await client.prompt({
      sessionId: newRes.sessionId,
      prompt: [textBlock('think and answer')],
    });

    await new Promise((resolve) => setTimeout(resolve, 50));

    // Should have received at least one text chunk
    const textChunks = collecting.promptUpdates.filter(
      (n) =>
        (n.update as { sessionUpdate?: string }).sessionUpdate ===
          'agent_message_chunk' &&
        (n.update as { content?: { type?: string } }).content?.type === 'text',
    );
    expect(textChunks.length).toBeGreaterThanOrEqual(1);

    const fullText = textChunks
      .map((n) => (n.update as { content?: { text?: string } }).content?.text ?? '')
      .join('');
    expect(fullText).toContain('Here is my answer');
  });

  it('should return model catalog in initialize response', async () => {
    const authedHarness = withAuthedStub(fixture);
    const { agentStream, clientStream } = makeInMemoryStreamPair();
    new AgentSideConnection((c) => new AcpServer(authedHarness, c), agentStream);
    const collecting = new CollectingClient();
    const client = new ClientSideConnection(() => collecting, clientStream);

    const init = await client.initialize({
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false } },
    });

    // Initialize should succeed and advertise capabilities
    expect(init.protocolVersion).toBeDefined();
    expect(init.agentCapabilities).toBeDefined();
    expect(init.agentCapabilities?.promptCapabilities?.image).toBe(true);
  });
});
