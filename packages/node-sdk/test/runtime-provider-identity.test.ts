import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import type { MirriConfig } from '@mirri-ai/agent-core';
import { createMirriDefaultHeaders, MIRRICODE_PLATFORM } from '@mirri-ai/mirri-code-oauth';

import { ProviderManager } from '../../agent-core/src/session/provider-manager';
import { SDKRpcClient } from '#/index';
import { TEST_IDENTITY } from './test-identity';

const tempDirs: string[] = [];

function resolveRuntimeProvider(options: {
  readonly config: MirriConfig;
  readonly model?: string;
  readonly mirriRequestHeaders?: Record<string, string>;
}) {
  const manager = new ProviderManager({
    config: options.config,
    mirriRequestHeaders: options.mirriRequestHeaders,
  });
  const model = options.model ?? options.config.defaultModel;
  if (model === undefined) {
    throw new Error('No model selected');
  }
  return manager.resolveProviderConfig(model);
}

async function makeTempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mirri-sdk-provider-identity-'));
  tempDirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

describe('runtime provider identity headers', () => {
  it('preserves the host user agent suffix in SDK RPC headers', async () => {
    const homeDir = await makeTempDir();
    const client = new SDKRpcClient({
      homeDir,
      identity: {
        ...TEST_IDENTITY,
        userAgentSuffix: 'web-runtime',
      },
    });
    const core = client.core as unknown as {
      readonly mirriRequestHeaders?: Record<string, string>;
    };

    try {
      expect(core.mirriRequestHeaders).toMatchObject({
        'User-Agent': 'mirri-code-cli/0.0.0-test (web-runtime)',
        'X-Msh-Version': '0.0.0-test',
      });
    } finally {
      await client.close();
    }
  });

  it('adds mirri-code-cli User-Agent and complete X-Msh headers to the default Mirri provider', async () => {
    const homeDir = await makeTempDir();
    const mirriRequestHeaders = createMirriDefaultHeaders({ homeDir, ...TEST_IDENTITY });
    const resolved = resolveRuntimeProvider({
      config: {
        defaultModel: 'kimi-model',
        providers: {
          kimi: {
            type: 'openai',
            apiKey: 'test-key',
          },
        },
        models: {
          'kimi-model': {
            provider: 'kimi',
            model: 'kimi-model',
            maxContextSize: 1000,
          },
        },
      },
      mirriRequestHeaders,
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: expect.objectContaining({
        'User-Agent': 'mirri-code-cli/0.0.0-test',
        'X-Msh-Platform': MIRRICODE_PLATFORM,
        'X-Msh-Version': '0.0.0-test',
        'X-Msh-Device-Name': expect.any(String),
        'X-Msh-Device-Model': expect.any(String),
        'X-Msh-Os-Version': expect.any(String),
        'X-Msh-Device-Id': expect.stringMatching(/^[0-9a-f-]+$/),
      }),
    });
  });

  it('lets Mirri provider customHeaders override default identity headers', async () => {
    const homeDir = await makeTempDir();
    const mirriRequestHeaders = createMirriDefaultHeaders({ homeDir, ...TEST_IDENTITY });
    const config: MirriConfig = {
      providers: {
        kimi: {
          type: 'openai',
          apiKey: 'test-key',
          customHeaders: {
            'User-Agent': 'Custom/1',
            'X-Msh-Version': 'override-version',
          },
        },
      },
      defaultProvider: 'kimi',
      defaultModel: 'kimi-model',
      models: {
        'kimi-model': {
          provider: 'kimi',
          model: 'kimi-model',
          maxContextSize: 1000,
        },
      },
    };

    const resolved = resolveRuntimeProvider({
      config,
      mirriRequestHeaders,
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      defaultHeaders: expect.objectContaining({
        'User-Agent': 'Custom/1',
        'X-Msh-Version': 'override-version',
        'X-Msh-Platform': MIRRICODE_PLATFORM,
      }),
    });
  });

  it('applies only the User-Agent (no device identity headers) to non-Mirri providers', async () => {
    const homeDir = await makeTempDir();
    const mirriRequestHeaders = createMirriDefaultHeaders({ homeDir, ...TEST_IDENTITY });
    const config: MirriConfig = {
      providers: {
        openai: {
          type: 'openai',
          baseUrl: 'https://example.test/v1',
          apiKey: 'sk-test',
        },
      },
      defaultProvider: 'openai',
      defaultModel: 'gpt-test',
      models: {
        'gpt-test': {
          provider: 'openai',
          model: 'gpt-test',
          maxContextSize: 1000,
        },
      },
    };

    const resolved = resolveRuntimeProvider({
      config,
      mirriRequestHeaders,
    });

    expect(resolved.provider).toMatchObject({
      type: 'openai',
      model: 'gpt-test',
      defaultHeaders: {
        'User-Agent': `mirri-code-cli/${TEST_IDENTITY.version}`,
      },
    });
    // Device identity headers (`X-Msh-*`) stay Mirri-only — must not leak to
    // third-party providers.
    const headers = (resolved.provider as { defaultHeaders?: Record<string, string> })
      .defaultHeaders;
    expect(headers).toBeDefined();
    expect(headers).not.toHaveProperty('X-Msh-Platform');
  });
});
