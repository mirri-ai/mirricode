// apps/mirri-web/src/lib/catalogModelSelection.ts
// Pure TS — no Vue. The catalog-import model checklist state machine
// (B8 US-5): the import dialog's model list is checked by default (an
// empty exclusion set), unchecking a model adds its raw model id to the
// exclusion list submitted as `excludeModelIds`, and the in-list search
// is display-only — it never mutates the exclusion state.

export interface CatalogModelSelectionState {
  /** Raw model ids the user unchecked — the import exclusion list. */
  excluded: ReadonlySet<string>;
}

/** Default = everything checked (one-click import unchanged). */
export function initCatalogModelSelection(_total: readonly string[]): CatalogModelSelectionState {
  return { excluded: new Set() };
}

export function toggleCatalogModel(
  state: CatalogModelSelectionState,
  modelId: string,
): CatalogModelSelectionState {
  const excluded = new Set(state.excluded);
  if (excluded.has(modelId)) excluded.delete(modelId);
  else excluded.add(modelId);
  return { excluded };
}

/** Toggle select-all ⇄ unselect-all over the currently VISIBLE models
 *  (search-filtered subset included): every visible model checked → all
 *  get excluded (the "unselect all, then re-check the few I want" flow);
 *  any visible model excluded → all visible models get re-checked. Ids
 *  outside the visible set are left untouched (same scope semantics as
 *  the provider-detail multi-select). */
export function toggleAllVisibleCatalogModels(
  state: CatalogModelSelectionState,
  visibleIds: readonly string[],
): CatalogModelSelectionState {
  const excluded = new Set(state.excluded);
  const allVisibleChecked = visibleIds.every((id) => !excluded.has(id));
  if (allVisibleChecked) {
    for (const id of visibleIds) excluded.add(id);
  } else {
    for (const id of visibleIds) excluded.delete(id);
  }
  return { excluded };
}

export function catalogImportSummary(
  state: CatalogModelSelectionState,
  total: readonly string[],
): { selected: number; total: number; excludeModelIds: string[] } {
  const excluded = new Set(state.excluded);
  const excludeModelIds = total.filter((id) => excluded.has(id));
  return { selected: total.length - excludeModelIds.length, total: total.length, excludeModelIds };
}

/** In-list search: case-insensitive substring match over id and name.
 *  Display-only — never touches the exclusion state (US-5 acceptance). */
export function filterCatalogModels<T extends { id: string; name?: string }>(
  models: readonly T[],
  search: string,
): T[] {
  const q = search.trim().toLowerCase();
  if (!q) return [...models];
  return models.filter(
    (m) => m.id.toLowerCase().includes(q) || (m.name?.toLowerCase().includes(q) ?? false),
  );
}
