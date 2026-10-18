// apps/mirri-web/test/delete-workspace-choice.test.ts
// Given the W6 three-choice workspace deletion (purge = delete the data and
// unregister; remove = unregister only, every byte preserved; cancel = Esc /
// overlay / the dismiss affordance), the useDeleteWorkspaceChoice singleton
// must resolve each path, supersede an older pending choice, and expose the
// pure view-builder the dialog renders (the session-count line above all —
// "3 sessions" must appear for a workspace holding three sessions).

import { describe, expect, it } from 'vitest';

import {
  buildDeleteWorkspaceDialogModel,
  useDeleteWorkspaceChoice,
  type WorkspaceDeleteTarget,
} from '../src/composables/useDeleteWorkspaceChoice';

const LABELS = {
  count: (n: number) => `${n} session${n === 1 ? '' : 's'}`,
  purge: 'Delete data and remove',
  remove: 'Remove from list only',
  cancel: 'Cancel',
  hint: 're-add the same path to restore',
};

function target(sessionCount = 3): WorkspaceDeleteTarget {
  return { id: 'wd_a', name: 'project', sessionCount };
}

describe('useDeleteWorkspaceChoice', () => {
  it('should resolve purge when the destructive choice is settled', async () => {
    const { confirmWorkspaceDeletion, settle } = useDeleteWorkspaceChoice();
    const pending = confirmWorkspaceDeletion(target());

    settle('purge');

    await expect(pending).resolves.toBe('purge');
  });

  it('should resolve remove when the data-preserving choice is settled', async () => {
    const { confirmWorkspaceDeletion, settle } = useDeleteWorkspaceChoice();
    const pending = confirmWorkspaceDeletion(target());

    settle('remove');

    await expect(pending).resolves.toBe('remove');
  });

  it('should resolve null when the dialog is cancelled (Esc / overlay / dismiss)', async () => {
    const { confirmWorkspaceDeletion, settle, current } = useDeleteWorkspaceChoice();
    const pending = confirmWorkspaceDeletion(target());
    expect(current.value).not.toBeNull();

    settle(null);

    await expect(pending).resolves.toBeNull();
    expect(current.value).toBeNull();
  });

  it('should supersede an older pending choice as cancelled before showing the new one', async () => {
    const { confirmWorkspaceDeletion, settle } = useDeleteWorkspaceChoice();
    const first = confirmWorkspaceDeletion(target(1));
    const second = confirmWorkspaceDeletion(target(2));

    await expect(first).resolves.toBeNull();
    settle('purge');
    await expect(second).resolves.toBe('purge');
  });

  it('should leave a no-op settle inert when nothing is pending', () => {
    const { settle, current } = useDeleteWorkspaceChoice();

    settle('purge');

    expect(current.value).toBeNull();
  });
});

describe('buildDeleteWorkspaceDialogModel', () => {
  it('should render the workspace session count in the count line', () => {
    const model = buildDeleteWorkspaceDialogModel(target(3), LABELS);

    expect(model.title).toBe('project');
    expect(model.countLine).toBe('3 sessions');
    expect(model.purgeLabel).toBe('Delete data and remove');
    expect(model.removeLabel).toBe('Remove from list only');
    expect(model.cancelLabel).toBe('Cancel');
    expect(model.hint).toBe('re-add the same path to restore');
  });

  it('should singularize the count line for one session and keep zero honest', () => {
    expect(buildDeleteWorkspaceDialogModel(target(1), LABELS).countLine).toBe('1 session');
    expect(buildDeleteWorkspaceDialogModel(target(0), LABELS).countLine).toBe('0 sessions');
  });

  it('should produce an empty closed model when no target is pending', () => {
    const model = buildDeleteWorkspaceDialogModel(null, LABELS);

    expect(model.title).toBe('');
    expect(model.countLine).toBe('');
  });
});
