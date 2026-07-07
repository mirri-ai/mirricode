import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { createKimiHarness } from '@mirri-ai/mirri-code-sdk';

import { smokeIdentityFromEnv } from './runtime-smoke-helpers';

async function main(): Promise<void> {
  const homeDir = await mkdtemp(join(tmpdir(), 'kimi-harness-config-home-'));
  const harness = createKimiHarness({ homeDir, identity: smokeIdentityFromEnv() });

  const initial = await harness.getConfig();
  if (Object.keys(initial.providers).length > 0) {
    throw new Error('expected empty providers for a fresh config home');
  }

  await harness.setConfig({
    defaultModel: 'mirri-code/kimi-for-coding',
    thinking: { enabled: true },
    defaultPermissionMode: 'manual',
    defaultPlanMode: false,
    providers: {
      'managed:mirri-code': {
        type: 'openai',
        baseUrl: 'https://api.kimi.com/coding/v1',
        apiKey: '',
        oauth: { storage: 'file', key: 'oauth/mirri-code' },
      },
    },
    models: {
      'mirri-code/kimi-for-coding': {
        provider: 'managed:mirri-code',
        model: 'kimi-for-coding',
        maxContextSize: 262144,
        capabilities: ['image_in', 'thinking', 'video_in'],
        displayName: 'Kimi for Coding',
      },
    },
    loopControl: {
      maxRetriesPerStep: 3,
      maxRalphIterations: 0,
      reservedContextSize: 50000,
      compactionTriggerRatio: 0.85,
    },
    services: {
      moonshotSearch: {
        baseUrl: 'https://api.kimi.com/coding/v1/search',
        apiKey: '',
        oauth: { storage: 'file', key: 'oauth/mirri-code' },
      },
      moonshotFetch: {
        baseUrl: 'https://api.kimi.com/coding/v1/fetch',
        apiKey: '',
        oauth: { storage: 'file', key: 'oauth/mirri-code' },
      },
    },
  });

  const configPath = join(homeDir, 'config.toml');
  const text = await readFile(configPath, 'utf-8');
  for (const expected of [
    'default_model = "mirri-code/kimi-for-coding"',
    'default_permission_mode = "manual"',
    '[providers."managed:mirri-code"]',
    '[providers."managed:mirri-code".oauth]',
    '[models."mirri-code/kimi-for-coding"]',
    '[services.moonshot_search]',
  ]) {
    if (!text.includes(expected)) {
      throw new Error(`missing ${expected} in written config`);
    }
  }

  const reloaded = await harness.getConfig({ reload: true });
  if (reloaded.defaultModel !== 'mirri-code/kimi-for-coding') {
    throw new Error('reloaded config did not preserve defaultModel');
  }
  if (reloaded.providers['managed:mirri-code']?.oauth?.key !== 'oauth/mirri-code') {
    throw new Error('reloaded config did not preserve provider oauth');
  }

  process.stdout.write(`config: ${configPath}\n`);
  process.stdout.write('ok\n');
}

try {
  await main();
} catch (error: unknown) {
  console.error(error);
  process.exitCode = 1;
}
