import type { ContentPart, Message } from '#/kosong/contract/message';

import type { WithSkillsEntry } from '#/agent/contextMemory/protocolMessage';
import type { AgentTaskStatus } from '#/agent/task/task';

export type SkillSource = 'project' | 'user' | 'extra' | 'builtin';

export interface UserPromptOrigin {
  readonly kind: 'user';
}

export const USER_PROMPT_ORIGIN: UserPromptOrigin = { kind: 'user' };

/** How a skill activation was triggered (SDR-005 four-state family). */
export type SkillActivationTrigger = 'user-slash' | 'user-inline' | 'model-tool' | 'nested-skill';
/** User-initiated triggers: user-slash and user-inline (message inline references). */
export function isUserActivationTrigger(trigger: SkillActivationTrigger): boolean {
  return trigger === 'user-slash' || trigger === 'user-inline';
}

/**
 * Standalone user-activation triggers — the user activation that opens its
 * own turn (`user-slash`: one message, one turn). A `user-inline` wrapper is
 * a same-turn companion of its body message (ADR-004 decision 3), never a
 * turn opener or an undo anchor on its own.
 */
export function isStandaloneUserActivationTrigger(trigger: SkillActivationTrigger): boolean {
  return trigger === 'user-slash';
}

export interface SkillActivationOrigin {
  readonly kind: 'skill_activation';
  readonly activationId: string;
  readonly skillName: string;
  readonly skillArgs?: string | undefined;
  readonly trigger: SkillActivationTrigger;
  readonly skillType?: string | undefined;
  readonly skillPath?: string | undefined;
  readonly skillSource?: SkillSource | undefined;
}

export interface PluginCommandOrigin {
  readonly kind: 'plugin_command';
  readonly activationId: string;
  readonly pluginId: string;
  readonly commandName: string;
  readonly commandArgs?: string | undefined;
  readonly trigger: 'user-slash';
}

export interface InjectionOrigin {
  readonly kind: 'injection';
  readonly variant: string;
  readonly ownerPromptId?: string;
}

export interface ShellCommandOrigin {
  readonly kind: 'shell_command';
  readonly phase: 'input' | 'output';
  readonly isError?: boolean;
}

export interface CompactionSummaryOrigin {
  readonly kind: 'compaction_summary';
}

export interface SystemTriggerOrigin {
  readonly kind: 'system_trigger';
  readonly name: string;
}

export interface TaskOrigin {
  readonly kind: 'background_task';
  readonly taskId: string;
  readonly status: AgentTaskStatus;
  readonly notificationId: string;
}

export interface CronJobOrigin {
  readonly kind: 'cron_job';
  readonly jobId: string;
  readonly cron: string;
  readonly recurring: boolean;
  readonly coalescedCount: number;
  readonly stale: boolean;
}

export interface CronMissedOrigin {
  readonly kind: 'cron_missed';
  readonly count: number;
}

export interface HookResultOrigin {
  readonly kind: 'hook_result';
  readonly event: string;
  readonly blocked?: boolean;
}

export interface HookAdditionalContextOrigin {
  readonly kind: 'hook_additional_context';
  readonly event: string;
}

export interface RetryOrigin {
  readonly kind: 'retry';
  readonly trigger?: string;
}

export type PromptOrigin =
  | UserPromptOrigin
  | SkillActivationOrigin
  | PluginCommandOrigin
  | InjectionOrigin
  | ShellCommandOrigin
  | CompactionSummaryOrigin
  | SystemTriggerOrigin
  | TaskOrigin
  | CronJobOrigin
  | CronMissedOrigin
  | HookResultOrigin
  | HookAdditionalContextOrigin
  | RetryOrigin;

export type ContextMessage = Message & {
  readonly id?: string;
  readonly providerMessageId?: string;
  readonly origin?: PromptOrigin | undefined;
  readonly isError?: boolean;
  readonly note?: string;
  /**
   * ADR-006: activation record stamped by the prompt scheduler on a user
   * message whose inline skill references were rewritten (body replaced with
   * `[display]` markers). Pure product record — never enters model context,
   * never drives activation. `Message` (kosong contract) itself stays clean:
   * the model-boundary projector copies only known fields.
   */
  readonly with_skills?: WithSkillsEntry[];
};

export interface UserMessageRecord {
  content: readonly ContentPart[];
  origin: PromptOrigin;
}

export interface SystemReminderRecord {
  content: string;
  origin: PromptOrigin;
}

export interface AgentContextData {
  history: readonly ContextMessage[];
  tokenCount: number;
}
