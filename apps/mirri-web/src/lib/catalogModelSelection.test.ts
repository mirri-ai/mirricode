// apps/mirri-web/src/lib/catalogModelSelection.test.ts
// Catalog-import model checklist specs (B8 US-5): default = everything
// checked (one-click import unchanged), toggling marks raw model ids as
// excluded, the import summary intersects the exclusion set with the
// provider's model list, and the in-list search is display-only — it
// never mutates the exclusion state.

import { describe, expect, it } from 'vitest';

import {
  catalogImportSummary,
  filterCatalogModels,
  initCatalogModelSelection,
  toggleAllVisibleCatalogModels,
  toggleCatalogModel,
} from './catalogModelSelection';

describe('initCatalogModelSelection', () => {
  it('should default to everything checked when a provider is selected', () => {
    const state = initCatalogModelSelection(['imp-large', 'imp-small']);
    expect(state.excluded).toEqual(new Set());
    expect(catalogImportSummary(state, ['imp-large', 'imp-small']).selected).toBe(2);
  });
});

describe('toggleCatalogModel', () => {
  it('should mark a model as excluded when it is checked off', () => {
    const state = toggleCatalogModel(initCatalogModelSelection(['a', 'b']), 'a');
    expect(state.excluded).toEqual(new Set(['a']));
  });

  it('should restore a model when the same id is toggled twice', () => {
    const state = toggleCatalogModel(
      toggleCatalogModel(initCatalogModelSelection(['a', 'b']), 'a'),
      'a',
    );
    expect(state.excluded).toEqual(new Set());
  });
});

describe('catalogImportSummary', () => {
  it('should count only the intersection when excluded ids come from the model list', () => {
    const state = toggleCatalogModel(initCatalogModelSelection(['a', 'b', 'c']), 'b');
    const summary = catalogImportSummary(state, ['a', 'b', 'c']);
    expect(summary).toEqual({ selected: 2, total: 3, excludeModelIds: ['b'] });
  });

  it('should drop stale exclusions when the provider is switched and the state re-initialized', () => {
    const stale = toggleCatalogModel(initCatalogModelSelection(['old-x']), 'old-x');
    const state = initCatalogModelSelection(['a', 'b']);
    const summary = catalogImportSummary(state, ['a', 'b']);
    expect(summary).toEqual({ selected: 2, total: 2, excludeModelIds: [] });
    expect(stale.excluded).toEqual(new Set(['old-x']));
  });

  it('should ignore exclusions that are not in the current provider list', () => {
    const state = toggleCatalogModel(initCatalogModelSelection([]), 'ghost');
    const summary = catalogImportSummary(state, ['a', 'b']);
    expect(summary).toEqual({ selected: 2, total: 2, excludeModelIds: [] });
  });
});

describe('toggleAllVisibleCatalogModels', () => {
  const all = ['a', 'b', 'c'];

  it('should exclude every visible model when everything is currently checked', () => {
    const state = toggleAllVisibleCatalogModels(initCatalogModelSelection(all), all);
    expect(catalogImportSummary(state, all).selected).toBe(0);
    expect(state.excluded).toEqual(new Set(['a', 'b', 'c']));
  });

  it('should re-check only the visible models when they are all excluded', () => {
    const none = toggleAllVisibleCatalogModels(initCatalogModelSelection(all), all);
    const state = toggleAllVisibleCatalogModels(none, all);
    expect(catalogImportSummary(state, all).selected).toBe(3);
  });

  it('should scope the toggle to the visible subset when a search is active', () => {
    // Given everything is checked and the search narrows the list to `b`
    // When the user hits the toggle over that visible subset
    const state = toggleAllVisibleCatalogModels(initCatalogModelSelection(all), ['b']);
    // Then only `b` is excluded — the hidden `a`/`c` stay checked
    expect(catalogImportSummary(state, all)).toEqual({
      selected: 2,
      total: 3,
      excludeModelIds: ['b'],
    });
  });

  it('should check the visible subset when only part of it is excluded', () => {
    // Given `b` is excluded and the visible subset is `b`/`c`
    const partly = toggleCatalogModel(initCatalogModelSelection(all), 'b');
    // When the user hits the toggle (mixed state → check all visible)
    const state = toggleAllVisibleCatalogModels(partly, ['b', 'c']);
    // Then every visible model is checked again
    expect(catalogImportSummary(state, all).selected).toBe(3);
  });

  it('should report zero selected in the summary when everything is excluded', () => {
    const state = toggleAllVisibleCatalogModels(initCatalogModelSelection(all), all);
    expect(catalogImportSummary(state, all)).toEqual({
      selected: 0,
      total: 3,
      excludeModelIds: ['a', 'b', 'c'],
    });
  });
});

describe('filterCatalogModels', () => {  const models = [
    { id: 'imp-large', name: 'Import Large' },
    { id: 'imp-small' },
    { id: 'imp-raw', name: 'raw model' },
  ];

  it('should return every model when the search is empty', () => {
    expect(filterCatalogModels(models, '')).toEqual(models);
    expect(filterCatalogModels(models, '   ')).toEqual(models);
  });

  it('should match by id case-insensitively when the search hits the id', () => {
    expect(filterCatalogModels(models, 'IMP-SMALL').map((m) => m.id)).toEqual(['imp-small']);
  });

  it('should match by name case-insensitively when the search hits the name', () => {
    expect(filterCatalogModels(models, 'large').map((m) => m.id)).toEqual(['imp-large']);
  });

  it('should keep the exclusion state untouched when the search filters models', () => {
    // Given the user checked off imp-large and searches a term that hides it
    const state = toggleCatalogModel(initCatalogModelSelection([]), 'imp-large');

    // When the list is filtered by a non-matching term, then cleared
    const filtered = filterCatalogModels(models, 'small');
    expect(filtered.map((m) => m.id)).toEqual(['imp-small']);
    const restored = filterCatalogModels(models, '');

    // Then the exclusion survives — the search is display-only
    expect(restored.map((m) => m.id)).toEqual(['imp-large', 'imp-small', 'imp-raw']);
    expect(state.excluded).toEqual(new Set(['imp-large']));
  });
});
