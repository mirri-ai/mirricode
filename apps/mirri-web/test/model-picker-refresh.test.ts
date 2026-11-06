// apps/mirri-web/test/model-picker-refresh.test.ts
// Pure-logic tests for the model-picker open flow's availability policy
// (`src/lib/modelPickerRefresh.ts`): the picker opens on the cached list
// and the network refresh must neither block it nor have its failures
// swallowed (#183/#185). Given-When-Then.

import { describe, expect, it } from 'vitest';

import { modelPickerUnavailable } from '../src/lib/modelPickerRefresh';

describe('modelPickerUnavailable', () => {
  it('given a cached list, when the local load fails, then the picker stays available on the cache', () => {
    expect(modelPickerUnavailable(true, 3)).toBe(false);
  });

  it('given no cache and a failed local load, when the picker opens, then it reports unavailable', () => {
    expect(modelPickerUnavailable(true, 0)).toBe(true);
  });

  it('given a healthy local load, when the picker opens, then it never reports unavailable', () => {
    expect(modelPickerUnavailable(false, 0)).toBe(false);
  });
});
