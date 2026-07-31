import { describe, expect, it } from 'vitest';

import { effectiveModelAlias } from '#/config/model';
import type { ModelAlias } from '#/config/schema';

function alias(overrides?: Partial<ModelAlias>): ModelAlias {
  return {
    provider: 'managed:mirri-code',
    model: 'kimi-k2',
    maxContextSize: 262144,
    capabilities: ['thinking'],
    supportEfforts: ['low', 'high', 'max'],
    defaultEffort: 'max',
    ...overrides,
  };
}

describe('effectiveModelAlias', () => {
  it('returns the alias unchanged when no adjustments are needed', () => {
    const model = alias();

    expect(effectiveModelAlias(model)).toEqual(model);
  });

  it('drops an incompatible defaultEffort when supportEfforts does not include it', () => {
    const model = alias({ supportEfforts: ['low', 'high'], defaultEffort: 'max' });

    expect(effectiveModelAlias(model).defaultEffort).toBeUndefined();
  });

  it('keeps a defaultEffort that is valid within supportEfforts', () => {
    const model = alias({ supportEfforts: ['low', 'high'], defaultEffort: 'high' });

    expect(effectiveModelAlias(model).defaultEffort).toBe('high');
  });
});
