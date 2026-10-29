// apps/mirri-web/src/lib/modelSelection.test.ts
// Pure selection-state-machine specs (B8): the same semantics
// retentionManagement applies to retention rows, generalized for the
// provider-detail model list — select-all acts on the visible set only,
// a fully-selected visible set deselects, and ids outside the visible
// set are never touched.

import { describe, expect, it } from 'vitest';

import {
  initModelSelection,
  toggleAllVisibleModels,
  toggleModelSelection,
} from './modelSelection';

describe('initModelSelection', () => {
  it('should start with an empty selection when initialized', () => {
    expect(initModelSelection().ids).toEqual(new Set());
  });
});

describe('toggleModelSelection', () => {
  it('should add an id when it is unselected', () => {
    expect(toggleModelSelection({ ids: new Set() }, 'a').ids).toEqual(new Set(['a']));
  });

  it('should remove an id when it is already selected', () => {
    expect(toggleModelSelection({ ids: new Set(['a']) }, 'a').ids).toEqual(new Set());
  });
});

describe('toggleAllVisibleModels', () => {
  it('should select all visible ids when none are selected', () => {
    const next = toggleAllVisibleModels({ ids: new Set(['x']) }, ['a', 'b']);
    expect(next.ids).toEqual(new Set(['x', 'a', 'b']));
  });

  it('should select the missing ids when only some visible ids are selected', () => {
    const next = toggleAllVisibleModels({ ids: new Set(['a']) }, ['a', 'b']);
    expect(next.ids).toEqual(new Set(['a', 'b']));
  });

  it('should deselect the visible set when every visible id is selected', () => {
    const next = toggleAllVisibleModels({ ids: new Set(['a', 'b', 'x']) }, ['a', 'b']);
    expect(next.ids).toEqual(new Set(['x']));
  });

  it('should keep ids outside the visible set untouched', () => {
    const next = toggleAllVisibleModels({ ids: new Set(['x']) }, ['a']);
    expect(next.ids.has('x')).toBe(true);
  });

  it('should keep the selection empty when the visible set is empty', () => {
    const next = toggleAllVisibleModels({ ids: new Set(['x']) }, []);
    expect(next.ids).toEqual(new Set(['x']));
  });
});
