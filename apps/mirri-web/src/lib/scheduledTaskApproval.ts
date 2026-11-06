// apps/mirri-web/src/lib/scheduledTaskApproval.ts
// Turn a scheduled-task creation approval into the plain-language rows the
// chat approval card shows (ADR-022). The card is the user's ONE decision, so
// every parameter is spelled out in words — never a bare id or a raw field.

import type { ApprovalBlock } from '../types';
import { humanizeCron } from './cronHumanize';
import { zoneOffsetLabel } from './cronManagement';

type Translator = (key: string, params?: Record<string, unknown>) => string;
type ScheduledTaskBlock = Extract<ApprovalBlock, { kind: 'scheduled_task' }>;

export interface ScheduledTaskApprovalView {
  /** The task's name: its title, else the instruction's first line. */
  heading: string;
  /** The humanized schedule (the raw expression when it has no friendly reading). */
  schedule: string;
  /** The zone's short offset label, or null when the schedule is zone-less. */
  zone: string | null;
  /** The workspace's display name — never a bare id. */
  workspace: string;
  /** The model, or the workspace-default wording. */
  model: string;
  /** What the run's permission mode means for the user, in words. */
  permission: string;
  /** Repeat wording: once / unlimited / capped. */
  repeat: string;
}

export function describeScheduledTaskApproval(
  block: ScheduledTaskBlock,
  t: Translator,
  context: { workspaceName?: string },
): ScheduledTaskApprovalView {
  const expression = block.cron.trim();
  const human = humanizeCron(expression, t);
  const firstLine = block.prompt.split('\n')[0]?.trim() ?? '';
  const zone = block.timezone?.trim() ?? '';
  const mode = block.permissionMode?.trim() ?? '';
  return {
    heading: block.title?.trim() || firstLine,
    schedule: human,
    zone: zone === '' ? null : zoneOffsetLabel(Date.now(), zone),
    workspace: context.workspaceName?.trim() || t('approval.scheduledTask.workspaceUnknown'),
    model: block.model?.trim() || t('approval.scheduledTask.modelDefault'),
    permission: t(`approval.scheduledTask.permission_${mode === '' ? 'manual' : mode}`),
    repeat: !block.recurring
      ? t('approval.scheduledTask.repeatOnce')
      : block.maxFires !== undefined
        ? t('approval.scheduledTask.repeatCapped', { n: block.maxFires })
        : t('approval.scheduledTask.repeatUnlimited'),
  };
}
