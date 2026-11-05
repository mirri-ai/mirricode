import { describe, expect, it } from 'vitest';

import { describeScheduledTaskApproval } from './scheduledTaskApproval';

const t = (key: string, params?: Record<string, unknown>): string =>
  params === undefined ? key : `${key}:${JSON.stringify(params)}`;

const base = {
  kind: 'scheduled_task' as const,
  cron: '15 10 * * 1-5',
  timezone: 'Asia/Tokyo',
  workspaceId: 'wd_demo',
  prompt: 'review the pickup tickets',
  recurring: true,
};

describe('describeScheduledTaskApproval', () => {
  it('should name the workspace by its display name when one is known', () => {
    const view = describeScheduledTaskApproval(base, t, { workspaceName: 'flat35' });
    expect(view.workspace).toBe('flat35');
  });

  it('should never show a bare workspace id when no display name is known', () => {
    const view = describeScheduledTaskApproval(base, t, {});
    expect(view.workspace).toBe('approval.scheduledTask.workspaceUnknown');
  });

  it('should show the workspace default model when the agent left the model unset', () => {
    const view = describeScheduledTaskApproval(base, t, {});
    expect(view.model).toBe('approval.scheduledTask.modelDefault');
  });

  it('should show the chosen model verbatim when the agent set one', () => {
    const view = describeScheduledTaskApproval({ ...base, model: 'claude-sonnet-4' }, t, {});
    expect(view.model).toBe('claude-sonnet-4');
  });

  it('should explain what manual mode means for the user when the run needs approvals', () => {
    const view = describeScheduledTaskApproval({ ...base, permissionMode: 'manual' }, t, {});
    expect(view.permission).toBe('approval.scheduledTask.permission_manual');
  });

  it('should describe a one-shot task as a single run', () => {
    const view = describeScheduledTaskApproval({ ...base, recurring: false }, t, {});
    expect(view.repeat).toBe('approval.scheduledTask.repeatOnce');
  });

  it('should show the run cap when a recurring task declares max fires', () => {
    const view = describeScheduledTaskApproval({ ...base, maxFires: 5 }, t, {});
    expect(view.repeat).toBe('approval.scheduledTask.repeatCapped:{"n":5}');
  });

  it('should fall back to the raw expression when the schedule has no friendly reading', () => {
    const view = describeScheduledTaskApproval({ ...base, cron: '7 3 1 */2 *' }, t, {});
    expect(view.schedule).toBe('7 3 1 */2 *');
  });

  it('should use the task title as the heading and fall back to the prompt first line', () => {
    expect(describeScheduledTaskApproval({ ...base, title: 'Pick Up Review' }, t, {}).heading).toBe('Pick Up Review');
    expect(describeScheduledTaskApproval({ ...base, prompt: 'line one\nline two' }, t, {}).heading).toBe('line one');
  });
});
