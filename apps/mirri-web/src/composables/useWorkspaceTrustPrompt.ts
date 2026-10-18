// apps/mirri-web/src/composables/useWorkspaceTrustPrompt.ts
// Promise-based workspace trust confirmation (W8, SDR-007): a module-level
// singleton holds the pending request (the useDeleteWorkspaceChoice
// pattern); the TrustWorkspaceDialogHost (mounted once in App.vue) renders
// it. The confirmation is NON-BLOCKING by design — 选中即用 (SDR-007: the
// workspace is selected and usable the moment the user picks it; the
// confirmation only asks whether to ALSO trust it now). Callers
// `await confirmWorkspaceTrust(ws)` from anywhere and resolve with the
// user's choice: `true` (信任此工作区), `false` (暂不 — Esc / overlay /
// the dismiss affordance all count: the workspace stays usable and the
// untrusted state stays honestly visible).
import { ref } from 'vue';

import type { AppWorkspace } from '../api/types';

/** What the caller wants the dialog to describe. */
export interface WorkspaceTrustTarget {
  id: string;
  name: string;
  root: string;
}

/**
 * The dialog's view model (pure builder — the component is a thin render,
 * so the logic is unit-testable in the node test env without mounting). All
 * label strings are injected (the component passes the i18n spellings).
 */
export interface TrustWorkspaceDialogModel {
  title: string;
  rootLine: string;
  trustLabel: string;
  laterLabel: string;
  hint: string;
}

/** Build the dialog's view model for a target (`null` target = closed). */
export function buildTrustWorkspaceDialogModel(
  target: WorkspaceTrustTarget | null,
  labels: {
    trust: string;
    later: string;
    hint: string;
  },
): TrustWorkspaceDialogModel {
  return {
    title: target?.name ?? '',
    rootLine: target?.root ?? '',
    trustLabel: labels.trust,
    laterLabel: labels.later,
    hint: labels.hint,
  };
}

/** The pending request: the target plus the promise's settle handles. */
interface TrustChoiceRequest {
  target: WorkspaceTrustTarget;
  resolve: (trusted: boolean) => void;
}

const current = ref<TrustChoiceRequest | null>(null);

/** Resolve the pending choice (`false` = 暂不) and close the dialog. */
function settle(trusted: boolean): void {
  const req = current.value;
  if (!req) return;
  current.value = null;
  req.resolve(trusted);
}

/**
 * Ask the user whether to trust a workspace (SDR-007's two actions).
 * Resolves `true` (信任此工作区) or `false` (暂不 — also the Esc/overlay
 * dismiss). If a choice is already open, the older request resolves as
 * 暂不 before the new one shows (the useDeleteWorkspaceChoice supersede
 * rule).
 */
export function confirmWorkspaceTrust(target: WorkspaceTrustTarget): Promise<boolean> {
  if (current.value) settle(false);
  return new Promise<boolean>((resolve) => {
    current.value = { target, resolve };
  });
}

export function useWorkspaceTrustPrompt(): {
  current: typeof current;
  settle: typeof settle;
  confirmWorkspaceTrust: typeof confirmWorkspaceTrust;
} {
  return { current, settle, confirmWorkspaceTrust };
}

/** Whether the workspace should trigger the confirmation when selected. */
export function isWorkspaceTrustPromptNeeded(workspace: AppWorkspace): boolean {
  // The TS REST backend maps `trusted: true` (no trust column), so only
  // the rust backend's honest untrusted rows ever prompt.
  return !workspace.trusted;
}
