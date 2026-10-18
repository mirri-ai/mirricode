// apps/mirri-web/src/composables/useDeleteWorkspaceChoice.ts
// Promise-based three-choice workspace deletion (W6): a module-level singleton
// holds the pending request (the useConfirmDialog pattern); the
// DeleteWorkspaceDialogHost (mounted once in App.vue) renders it. Callers
// `await confirmWorkspaceDeletion(ws)` from anywhere and resolve with the
// chosen mode ('purge' | 'remove') or null (cancel — Esc / overlay / the
// dismiss affordance).
import { ref } from 'vue';

import type { WorkspaceDeleteMode } from '../api/types';

/** What the caller wants the dialog to describe. */
export interface WorkspaceDeleteTarget {
  id: string;
  name: string;
  sessionCount: number;
}

/**
 * The dialog's view model (pure builder — the component is a thin render, so
 * the logic is unit-testable in the node test env without mounting). All
 * label strings are injected (the component passes the i18n spellings).
 */
export interface DeleteWorkspaceDialogModel {
  title: string;
  countLine: string;
  purgeLabel: string;
  removeLabel: string;
  cancelLabel: string;
  hint: string;
}

/** Build the dialog's view model for a target (`null` target = closed). */
export function buildDeleteWorkspaceDialogModel(
  target: WorkspaceDeleteTarget | null,
  labels: {
    count: (sessionCount: number) => string;
    purge: string;
    remove: string;
    cancel: string;
    hint: string;
  },
): DeleteWorkspaceDialogModel {
  return {
    title: target?.name ?? '',
    countLine: target === null ? '' : labels.count(target.sessionCount),
    purgeLabel: labels.purge,
    removeLabel: labels.remove,
    cancelLabel: labels.cancel,
    hint: labels.hint,
  };
}

/** The pending request: the target plus the promise's settle handles. */
interface DeleteChoiceRequest {
  target: WorkspaceDeleteTarget;
  resolve: (mode: WorkspaceDeleteMode | null) => void;
}

const current = ref<DeleteChoiceRequest | null>(null);

/** Resolve the pending choice (null = cancelled) and close the dialog. */
function settle(mode: WorkspaceDeleteMode | null): void {
  const req = current.value;
  if (!req) return;
  current.value = null;
  req.resolve(mode);
}

/**
 * Ask the user how to delete a workspace. Resolves with the chosen mode, or
 * `null` when the dialog was cancelled. If a choice is already open, the
 * older request resolves as cancelled before the new one shows (the
 * useConfirmDialog supersede rule).
 */
function confirmWorkspaceDeletion(
  target: WorkspaceDeleteTarget,
): Promise<WorkspaceDeleteMode | null> {
  if (current.value) settle(null);
  return new Promise<WorkspaceDeleteMode | null>((resolve) => {
    current.value = { target, resolve };
  });
}

export function useDeleteWorkspaceChoice(): {
  current: typeof current;
  settle: typeof settle;
  confirmWorkspaceDeletion: typeof confirmWorkspaceDeletion;
} {
  return { current, settle, confirmWorkspaceDeletion };
}
