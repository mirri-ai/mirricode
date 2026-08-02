import { createDecorator } from "#/_base/di/instantiation";

export type SwarmModeTrigger = 'manual' | 'task' | 'tool';

export interface IAgentSwarmService {
  readonly _serviceBrand: undefined;

  readonly isActive: boolean;
  enter(trigger: SwarmModeTrigger): void;
  /** Restore swarm mode from a persisted trigger without dispatching Ops or injecting reminders. */
  restoreEnter(trigger: SwarmModeTrigger): void;
  exit(): void;
}

export const IAgentSwarmService = createDecorator<IAgentSwarmService>('agentSwarmService');
