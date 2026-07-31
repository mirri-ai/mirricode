import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { MirriCore } from '../../src/rpc/core-impl';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

async function makeHome(configToml: string): Promise<string> {
  const home = await mkdtemp(path.join(tmpdir(), 'mirri-deny-'));
  tempDirs.push(home);
  const { writeFile } = await import('node:fs/promises');
  await writeFile(path.join(home, 'config.toml'), configToml, 'utf-8');
  return home;
}

function makeCore(home: string): MirriCore {
  return new MirriCore(async () => ({}) as never, { homeDir: home });
}

const CONFIG_WITH_MODELS = `
default_model = "kimi/alpha"

[providers.kimi]
type = "openai"
api_key = "sk-test"

[models."kimi/alpha"]
provider = "kimi"
model = "alpha"
max_context_size = 128000

[models."kimi/beta"]
provider = "kimi"
model = "beta"
max_context_size = 200000
`;

describe('MirriCore removeMirriModel denylist', () => {
  it('records the deleted model id on the provider removedModelIds', async () => {
    const home = await makeHome(CONFIG_WITH_MODELS);
    const core = makeCore(home);

    await core.removeMirriModel({ modelId: 'kimi/alpha' });

    const config = await core.getMirriConfig({});
    expect(config.providers['kimi']?.removedModelIds).toEqual(['alpha']);
    // Model alias is gone.
    expect(config.models?.['kimi/alpha']).toBeUndefined();
    // Other model untouched.
    expect(config.models?.['kimi/beta']).toBeDefined();
  });

  it('does not duplicate an id already on the denylist', async () => {
    const home = await makeHome(CONFIG_WITH_MODELS);
    const core = makeCore(home);

    await core.removeMirriModel({ modelId: 'kimi/alpha' });
    // Re-add the alias manually, then delete again.
    await core.setMirriConfig({
      models: {
        'kimi/alpha': { provider: 'kimi', model: 'alpha', maxContextSize: 128000 },
      },
    });
    await core.removeMirriModel({ modelId: 'kimi/alpha' });

    const config = await core.getMirriConfig({});
    expect(config.providers['kimi']?.removedModelIds).toEqual(['alpha']);
  });

  it('clears the id from the denylist when the alias is re-added', async () => {
    const home = await makeHome(CONFIG_WITH_MODELS);
    const core = makeCore(home);

    await core.removeMirriModel({ modelId: 'kimi/alpha' });
    expect((await core.getMirriConfig({})).providers['kimi']?.removedModelIds).toEqual([
      'alpha',
    ]);

    // Re-add the alias via setMirriConfig.
    await core.setMirriConfig({
      models: {
        'kimi/alpha': { provider: 'kimi', model: 'alpha', maxContextSize: 256000 },
      },
    });

    const config = await core.getMirriConfig({});
    expect(config.providers['kimi']?.removedModelIds).toBeUndefined();
    expect(config.models?.['kimi/alpha']).toMatchObject({ model: 'alpha', maxContextSize: 256000 });
  });
});
