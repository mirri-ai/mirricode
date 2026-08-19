/**
 * `command` domain — `IAgentCommandService` contract.
 *
 * Engine-side registry of contributed commands: `register` hooks a
 * `CommandContribution` (last-wins on same-name registration), `list` exposes
 * the current catalog as `AgentCommandInfo`, and `run` invokes a contribution
 * with its raw arguments. `onDidChange` fires (a single untyped event) whenever
 * a command is registered, replaced, or removed. Bound at Agent scope, created
 * together with the scope.
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';
import type { Event } from '#/_base/event';
import type { IDisposable } from '#/_base/di/lifecycle';
import type { CommandContribution } from './commandContribution';

export interface AgentCommandInfo {
  readonly name: string;
  readonly description?: string | undefined;
  /** Identifier of the provider that registered the command (`'engine'` today; plugin/skill providers carry their own). */
  readonly source: string;
}

export interface IAgentCommandService {
  readonly _serviceBrand: undefined;

  readonly onDidChange: Event<void>;

  list(): readonly AgentCommandInfo[];
  run(name: string, args?: string): Promise<void>;
  /**
   * Registers a command contribution. `source` identifies the registering
   * provider (defaults to `'engine'`); plugins, skills, or SDK hosts pass
   * their own identifier so `list()` can attribute commands.
   */
  register(contribution: CommandContribution, source?: string | undefined): IDisposable;
}

export const IAgentCommandService =
  createDecorator<IAgentCommandService>('agentCommandService');