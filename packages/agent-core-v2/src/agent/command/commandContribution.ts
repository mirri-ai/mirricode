/**
 * `command` domain — contribution-side contract for engine commands.
 *
 * A `CommandContribution` is the seam through which providers (engine
 * builtins, and later plugins, skills, or SDK consumers) contribute named
 * commands. Each contribution's `run` receives a `CommandRunContext` carrying
 * the raw argument string and a `get` accessor that resolves DI services from
 * the executing agent's scope.
 */

import type { ServiceIdentifier } from '#/_base/di/instantiation';

export interface CommandRunContext {
  /** Raw argument string as typed after the command name (unparsed). */
  readonly args: string;

  /** Resolves any service registered in the executing agent's DI scope. */
  get<T>(id: ServiceIdentifier<T>): T;
}

export interface CommandContribution {
  readonly name: string;
  readonly description?: string | undefined;
  run(ctx: CommandRunContext): void | Promise<void>;
}