import { describe, expect, it } from 'vitest';
import { resolveModelDisplayName } from './modelDisplayName';
import type { AppModel } from '../api/types';

function model(partial: Partial<AppModel> & Pick<AppModel, 'id' | 'model'>): AppModel {
  return {
    provider: 'prov',
    maxContextSize: 128000,
    ...partial,
  };
}

describe('resolveModelDisplayName (model-switch divider label)', () => {
  it('should resolve the id-exact entry first when names collide across providers', () => {
    // Given two entries whose raw names collide but ids differ, and an
    // alias that spells one id
    const models = [
      model({ id: 'prov-a/small', model: 'small', displayName: 'Prov A Small' }),
      model({ id: 'prov-b/small', model: 'small', displayName: 'Prov B Small' }),
    ];
    // When resolving the id
    const label = resolveModelDisplayName(models, 'prov-b/small');
    // Then the id-exact entry wins — never the wrong provider's entry
    expect(label).toBe('Prov B Small');
  });

  it('should fall back to a raw-name match when the alias is not an id', () => {
    // Given an entry reachable only by its raw model name
    const models = [model({ id: 'kimi/alpha', model: 'moonshot-v1', displayName: 'Moonshot v1' })];
    // When resolving the raw name
    expect(resolveModelDisplayName(models, 'moonshot-v1')).toBe('Moonshot v1');
  });

  it('should prefer the displayName but degrade to the bare model name when absent', () => {
    // Given an entry without a displayName
    const models = [model({ id: 'x', model: 'bare-name' })];
    // When resolving its alias
    expect(resolveModelDisplayName(models, 'x')).toBe('bare-name');
  });

  it('should answer the alias verbatim when no models list or no entry matches', () => {
    // Given no list at all, a list without the alias, and empties
    expect(resolveModelDisplayName(undefined, 'model-a')).toBe('model-a');
    expect(resolveModelDisplayName([], 'model-a')).toBe('model-a');
    expect(
      resolveModelDisplayName([model({ id: 'other', model: 'other' })], 'model-a'),
    ).toBe('model-a');
  });

  it('should answer the em dash for a missing or empty alias', () => {
    // Given the divider's unknown-alias spelling
    expect(resolveModelDisplayName([], undefined)).toBe('—');
    expect(resolveModelDisplayName([], null)).toBe('—');
    expect(resolveModelDisplayName([], '')).toBe('—');
  });
});
