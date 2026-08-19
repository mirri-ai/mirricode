import { createDecorator } from "#/_base/di/instantiation";
import type { SkillActivationOrigin } from '#/agent/contextMemory/types';
import type { Turn } from '#/agent/loop/loop';
import type { PromptHandle } from '#/agent/prompt/prompt';

export interface SkillActivationInput {
  readonly name: string;
  readonly args?: string;
}

export interface IAgentSkillService {
  readonly _serviceBrand: undefined;

  activate(input: SkillActivationInput): Promise<Turn>;
  /**
   * Queue a skill activation as a raw user-slash intent and return
   * immediately. Validation (skill exists, user-activatable) stays
   * synchronous; the rendered prompt, `skill.activate` fact, and telemetry
   * only happen when the queued prompt materializes (dequeue time), so the
   * caller is never blocked behind a running turn.
   */
  enqueueDeferred(input: SkillActivationInput): Promise<PromptHandle>;
  recordModelToolActivation(origin: SkillActivationOrigin): void;
}

export const IAgentSkillService =
  createDecorator<IAgentSkillService>('agentSkillService');
