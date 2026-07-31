import { describe, expect, it } from 'vitest';

import { parseConfigString, configToTomlData } from '#/config/toml';

describe('legacy model overrides migration', () => {
  it('collapses a legacy overrides sub-object into top-level fields on load', () => {
    const config = parseConfigString(`
[providers.kimi]
type = "openai"
api_key = "sk-test"

[models."kimi/kimi-k2"]
provider = "kimi"
model = "kimi-k2"
max_context_size = 262144
support_efforts = ["low", "high", "max"]

[models."kimi/kimi-k2".overrides]
support_efforts = ["low", "high"]
default_effort = "high"
`);

    const alias = config.models?.['kimi/kimi-k2'];
    // overrides fields merged to top-level (override wins).
    expect(alias?.supportEfforts).toEqual(['low', 'high']);
    expect(alias?.defaultEffort).toBe('high');
    // Base field preserved.
    expect(alias?.maxContextSize).toBe(262144);
    // overrides sub-object is gone.
    expect((alias as Record<string, unknown> | undefined)?.['overrides']).toBeUndefined();
  });

  it('drops the overrides key when it is empty', () => {
    const config = parseConfigString(`
[providers.kimi]
type = "openai"
api_key = "sk-test"

[models."kimi/kimi-k2"]
provider = "kimi"
model = "kimi-k2"
max_context_size = 262144

[models."kimi/kimi-k2".overrides]
`);

    const alias = config.models?.['kimi/kimi-k2'];
    expect((alias as Record<string, unknown> | undefined)?.['overrides']).toBeUndefined();
    expect(alias?.maxContextSize).toBe(262144);
  });

  it('does not re-emit overrides when writing the migrated config back to TOML', () => {
    const config = parseConfigString(`
[providers.kimi]
type = "openai"
api_key = "sk-test"

[models."kimi/kimi-k2"]
provider = "kimi"
model = "kimi-k2"
max_context_size = 262144

[models."kimi/kimi-k2".overrides]
support_efforts = ["low", "high"]
default_effort = "high"
`);

    const data = configToTomlData(config);
    const models = data['models'] as Record<string, Record<string, unknown>>;
    const alias = models['kimi/kimi-k2'];
    expect(alias?.['overrides']).toBeUndefined();
    // Merged values are top-level now.
    expect(alias?.['support_efforts']).toEqual(['low', 'high']);
    expect(alias?.['default_effort']).toBe('high');
  });
});
