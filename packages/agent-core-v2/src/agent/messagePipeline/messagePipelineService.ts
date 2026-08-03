/**
 * `messagePipeline` domain — `IMessagePipelineService` implementation.
 *
 * Holds an ordered list of `MessageProjectionStep`s and runs them in
 * registration order. The service itself owns no state beyond the step list;
 * each step is responsible for its own caching / memoization (e.g.
 * `AgentVideoResolverService` memoizes uploads in `agentState`).
 *
 * Steps are contributed by their owning domains through `register()` on
 * construction — see `messagePipelineSteps.ts` for the two builtin steps
 * (`videoResolution`, `mediaCapabilityAdaptation`). A step's `appliesTo`
 * callback gates whether it runs under the current `RequestProjection`.
 *
 * Bound at Agent scope so each agent's pipeline is independent.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { toDisposable, type IDisposable } from '#/_base/di/lifecycle';

import type { Message } from '#/kosong/contract/message';
import type { Model } from '#/kosong/model/catalog';
import type { ModelRequester } from '#/kosong/model/modelRequester';

import {
  IMessagePipelineService,
  type MessageProjectionContext,
  type MessageProjectionStep,
  type RequestProjection,
} from './messagePipeline';

export class AgentMessagePipelineService implements IMessagePipelineService {
  declare readonly _serviceBrand: undefined;

  private readonly steps: MessageProjectionStep[] = [];

  register(step: MessageProjectionStep): IDisposable {
    this.steps.push(step);
    const steps = this.steps;
    return toDisposable(() => {
      const index = steps.indexOf(step);
      if (index !== -1) steps.splice(index, 1);
    });
  }

  async run(
    messages: readonly Message[],
    model: Model,
    requester: ModelRequester,
    projection: RequestProjection,
    signal: AbortSignal | undefined,
  ): Promise<readonly Message[]> {
    let current: readonly Message[] = messages;
    for (const step of this.steps) {
      if (step.appliesTo !== undefined && !step.appliesTo(projection)) continue;
      signal?.throwIfAborted();
      const context: MessageProjectionContext = {
        messages: current,
        model,
        requester,
        projection,
        signal,
      };
      const next = await step.project(context);
      if (next !== current) current = next;
    }
    return current;
  }
}

registerScopedService(
  LifecycleScope.Agent,
  IMessagePipelineService,
  AgentMessagePipelineService,
  ScopeActivation.OnScopeCreated,
  'messagePipeline',
);
