// apps/mirri-web/src/lib/modelSelection.ts
// Pure TS — no Vue. The provider-detail model-list selection state
// machine (B8): the same semantics retentionManagement applies to
// retention rows, generalized for the settings model table. The
// component (ProviderDetail.vue) stays a thin renderer over these.

export interface ModelSelectionState {
  ids: ReadonlySet<string>;
}

export function initModelSelection(): ModelSelectionState {
  return { ids: new Set() };
}

/** Toggle one model row's checkbox. */
export function toggleModelSelection(state: ModelSelectionState, id: string): ModelSelectionState {
  const ids = new Set(state.ids);
  if (ids.has(id)) ids.delete(id);
  else ids.add(id);
  return { ids };
}

/** Toggle select-all over the currently visible rows: a fully selected
 *  visible set deselects; ids outside the visible set are untouched. */
export function toggleAllVisibleModels(
  state: ModelSelectionState,
  visibleIds: readonly string[],
): ModelSelectionState {
  const allVisibleSelected = visibleIds.length > 0 && visibleIds.every((id) => state.ids.has(id));
  const ids = new Set(state.ids);
  if (allVisibleSelected) {
    for (const id of visibleIds) ids.delete(id);
  } else {
    for (const id of visibleIds) ids.add(id);
  }
  return { ids };
}
