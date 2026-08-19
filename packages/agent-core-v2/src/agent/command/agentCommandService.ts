/**
 * `command` domain — `IAgentCommandService` implementation.
 *
 * Backed by a name-keyed `Map`; a re-registration for an existing name
 * replaces the earlier entry (last-wins, mirroring the upstream collection
 * semantics). `register` returns an `IDisposable` whose `dispose` is guarded
 * by an identity check against the currently stored entry, so disposing a
 * stale handle never removes a later registration for the same name. `run`
 * hands each contribution a `get` that resolves services from the executing
 * agent's scope through a fresh `invokeFunction` per call — the accessor is
 * deliberately not captured, so an async contribution may call `ctx.get`
 * after its own `await` points. Bound at Agent scope with `OnScopeCreated`
 * activation: the nested RPC service (also Agent-scoped) depends on it, so
 * the scope would eagerly construct it either way.
 */

import { Emitter, type Event } from '#/_base/event';
import { Disposable, type IDisposable } from '#/_base/di/lifecycle';
import {
  IInstantiationService,
  type ServiceIdentifier,
} from '#/_base/di/instantiation';
import { ErrorCodes, Error2 } from '#/errors';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import {
  IAgentCommandService,
  type AgentCommandInfo,
} from './agentCommand';
import type { CommandContribution } from './commandContribution';

export class AgentCommandService extends Disposable implements IAgentCommandService {
  declare readonly _serviceBrand: undefined;

  private readonly _commands = new Map<string, { contribution: CommandContribution; source: string }>();
  private readonly _onDidChange = this._register(new Emitter<void>());
  readonly onDidChange: Event<void> = this._onDidChange.event;

  constructor(
    @IInstantiationService private readonly instantiation: IInstantiationService,
  ) {
    super();
  }

  register(contribution: CommandContribution, source = 'engine'): IDisposable {
    const entry = { contribution, source };
    this._commands.set(contribution.name, entry);
    this._onDidChange.fire();
    return {
      dispose: () => {
        if (this._commands.get(contribution.name) === entry) {
          this._commands.delete(contribution.name);
          this._onDidChange.fire();
        }
      },
    };
  }

  list(): readonly AgentCommandInfo[] {
    return [...this._commands.entries()].map(([name, entry]) => ({
      name,
      description: entry.contribution.description,
      source: entry.source,
    }));
  }

  async run(name: string, args = ''): Promise<void> {
    const entry = this._commands.get(name);
    if (entry === undefined) {
      throw new Error2(ErrorCodes.REQUEST_INVALID, `Command "${name}" was not found`);
    }
    // Each `get` runs its own `invokeFunction`, so the accessor never outlives
    // the synchronous call that creates it — async contributions may call
    // `ctx.get` after `await` without hitting "accessor is only valid during
    // the invocation of its target method".
    const get = <T>(id: ServiceIdentifier<T>): T =>
      this.instantiation.invokeFunction((accessor) => accessor.get(id));
    await entry.contribution.run({ args, get });
  }
}

registerScopedService(
  LifecycleScope.Agent,
  IAgentCommandService,
  AgentCommandService,
  ScopeActivation.OnScopeCreated,
  'command',
);