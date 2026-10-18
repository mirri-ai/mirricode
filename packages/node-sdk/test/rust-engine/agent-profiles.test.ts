import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { profileEntrySchema } from '@mirri-ai/protocol';

import { MirriError } from '@mirri-ai/agent-core';
import {
  __setMirriNodeBindingForTest,
  createMirriHarnessRust,
  SDKRpcClientRust,
  type MirriEngineNative,
  type MirriNodeBinding,
} from '#/sdk-rpc-client-rust';
import { RustEngineRpcError } from '#/rust-engine/error-channel';
import { SDKRpcClientBase } from '#/rpc';

/**
 * Agent profile data plane (R5-1) — the SDK surface over the embedded
 * engine's `agent.listProfiles` / `agent.enableProfile` /
 * `agent.disableProfile` RPC methods, driven through the same fake-binding
 * seam as `client-wiring.test.ts` (no compiled native module needed).
 */
class FakeEngine implements MirriEngineNative {
  readonly called: { method: string; params: unknown }[] = [];
  /** Scripted `callMethod` results keyed by wire method name. */
  readonly results = new Map<string, unknown>();
  /** Scripted `callMethod` rejections keyed by wire method name. */
  readonly rejections = new Map<string, Error>();

  async callMethod(method: string, params: unknown): Promise<unknown> {
    this.called.push({ method, params });
    const rejection = this.rejections.get(method);
    if (rejection !== undefined) throw rejection;
    return this.results.get(method) ?? {};
  }

  // The push channels are irrelevant to the data plane; record the
  // subscriptions so dispose stays honest if assertions need them.
  eventHandler: ((err: null, event: { method: string; params: unknown }) => void) | undefined;
  reverseRpcHandler:
    | ((err: null, request: { id: number; method: string; params: unknown }) => void)
    | undefined;

  onEvent(callback: (err: null, event: { method: string; params: unknown }) => void): void {
    this.eventHandler = callback;
  }

  onReverseRpc(
    callback: (err: null, request: { id: number; method: string; params: unknown }) => void,
  ): void {
    this.reverseRpcHandler = callback;
  }

  async respondReverseRpc(): Promise<void> {}

  async respondReverseRpcError(): Promise<void> {}

  async dispose(): Promise<void> {}
}

let fakeEngine: FakeEngine;
const tempDirs: string[] = [];

beforeEach(() => {
  fakeEngine = new FakeEngine();
  const binding: MirriNodeBinding = { createEngine: () => fakeEngine };
  __setMirriNodeBindingForTest(binding);
});

afterEach(async () => {
  __setMirriNodeBindingForTest(null);
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
  vi.restoreAllMocks();
});

async function makeTempHome(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-rust-profiles-'));
  tempDirs.push(dir);
  return dir;
}

function makeClient(homeDir: string): SDKRpcClientRust {
  return new SDKRpcClientRust({ homeDir, uiMode: 'cli' });
}

/** A client whose engine never answers the data plane — stands in for the v1 / pre-Task-32 v2 engines. */
class BareClient extends SDKRpcClientBase {
  protected getRpc(): Promise<never> {
    throw new Error('the agent profile data plane never reaches getRpc');
  }
}

/** One engine wire row, snake_case like the embedded engine serves it. */
const CODER_PROFILE_ROW = {
  name: 'coder',
  description: 'The coding agent',
  source: 'builtin',
  builtin: true,
  essential: false,
  enabled: true,
  has_override: true,
  file_path: '/home/.mirri-code/agents/coder.md',
  default_model: 'ultra',
  tools: ['Read'],
  suppressed_candidates: [
    { source: 'project', priority: 1, reason: 'priority' },
  ],
};

describe('SDKRpcClientRust agent profile data plane', () => {
  it('given the rust engine, when listAgentProfiles, then it calls agent.listProfiles and unwraps the {items, skipped} envelope with protocol-shaped rows', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('agent.listProfiles', {
        items: [CODER_PROFILE_ROW],
        skipped: [{ path: '/ws/agents/broken.md', reason: 'frontmatter parse failed' }],
      });

      const result = await client.listAgentProfiles();

      expect(fakeEngine.called).toContainEqual({
        method: 'agent.listProfiles',
        params: {},
      });
      expect(result.items).toHaveLength(1);
      expect(result.items[0]).toMatchObject({
        name: 'coder',
        default_model: 'ultra',
        has_override: true,
      });
      expect(result.skipped).toEqual([
        { path: '/ws/agents/broken.md', reason: 'frontmatter parse failed' },
      ]);
      // The row shape is the shared protocol REST contract — every listed
      // row must survive `profileEntrySchema.parse` untouched.
      for (const item of result.items) {
        expect(profileEntrySchema.parse(item)).toEqual(item);
      }
    } finally {
      await client.close();
    }
  });

  it('given the engine answers with a malformed list envelope, when listAgentProfiles, then it degrades to empty rows instead of throwing', async () => {
    const client = makeClient(await makeTempHome());
    try {
      // A non-object answer (e.g. an error-shaped payload that does not
      // reject at the transport layer) and an envelope whose `items`/
      // `skipped` are not arrays must both degrade to empty rows — the
      // same convention `listSessions` applies.
      fakeEngine.results.set('agent.listProfiles', null);
      await expect(client.listAgentProfiles()).resolves.toEqual({
        items: [],
        skipped: [],
      });

      fakeEngine.results.set('agent.listProfiles', {
        items: 'not-an-array',
        skipped: { path: '/ws/agents/broken.md' },
      });
      await expect(client.listAgentProfiles()).resolves.toEqual({
        items: [],
        skipped: [],
      });
    } finally {
      await client.close();
    }
  });

  it('given the rust engine, when setAgentProfileEnabled(name, true), then it calls agent.enableProfile with the name', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('agent.enableProfile', { ok: true });

      const result = await client.setAgentProfileEnabled('coder', true);

      expect(fakeEngine.called).toContainEqual({
        method: 'agent.enableProfile',
        params: { name: 'coder' },
      });
      expect(result).toEqual({ ok: true });
    } finally {
      await client.close();
    }
  });

  it('given the rust engine, when setAgentProfileEnabled(name, false), then it calls agent.disableProfile with the name', async () => {
    const client = makeClient(await makeTempHome());
    try {
      fakeEngine.results.set('agent.disableProfile', { ok: true });

      const result = await client.setAgentProfileEnabled('coder', false);

      expect(fakeEngine.called).toContainEqual({
        method: 'agent.disableProfile',
        params: { name: 'coder' },
      });
      expect(result).toEqual({ ok: true });
    } finally {
      await client.close();
    }
  });

  it('given the engine refuses disabling the essential profile, when setAgentProfileEnabled("agent", false), then the decoded engine error reaches the caller', async () => {
    const client = makeClient(await makeTempHome());
    try {
      // The exact native-layer rejection shape: the engine's TS-exact
      // refusal text plus the structured `[mirri-rpc-error …]` suffix the
      // NAPI layer appends (invalid params → JSON-RPC -32602).
      fakeEngine.rejections.set(
        'agent.disableProfile',
        new Error(
          'Cannot disable the essential profile "agent" [mirri-rpc-error {"code":-32602}]',
        ),
      );

      const failure = client.setAgentProfileEnabled('agent', false);

      await expect(failure).rejects.toBeInstanceOf(RustEngineRpcError);
      await expect(failure).rejects.toMatchObject({
        message: 'Cannot disable the essential profile "agent"',
        code: -32602,
      });
    } finally {
      await client.close();
    }
  });

  it('given a client whose engine lacks the data plane, when listAgentProfiles / setAgentProfileEnabled, then both reject with NOT_IMPLEMENTED', async () => {
    const client = new BareClient();

    await expect(client.listAgentProfiles()).rejects.toMatchObject({
      name: 'MirriError',
      code: 'not_implemented',
    } satisfies Partial<MirriError>);
    await expect(client.setAgentProfileEnabled('coder', true)).rejects.toMatchObject({
      name: 'MirriError',
      code: 'not_implemented',
    } satisfies Partial<MirriError>);
  });

  it('given the rust harness, when listAgentProfiles / setAgentProfileEnabled, then the harness delegates to the engine data plane', async () => {
    const harness = createMirriHarnessRust({
      homeDir: await makeTempHome(),
      uiMode: 'cli',
    });
    try {
      fakeEngine.results.set('agent.listProfiles', { items: [], skipped: [] });
      fakeEngine.results.set('agent.disableProfile', { ok: true });

      const listed = await harness.listAgentProfiles();
      await harness.setAgentProfileEnabled('coder', false);

      expect(listed).toEqual({ items: [], skipped: [] });
      expect(fakeEngine.called).toContainEqual({
        method: 'agent.listProfiles',
        params: {},
      });
      expect(fakeEngine.called).toContainEqual({
        method: 'agent.disableProfile',
        params: { name: 'coder' },
      });
    } finally {
      await harness.close();
    }
  });
});
