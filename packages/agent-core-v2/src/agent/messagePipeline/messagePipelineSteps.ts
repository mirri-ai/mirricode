/**
 * `messagePipeline` domain — builtin step registration.
 *
 * `AgentMessagePipelineStepsService` is an Agent-scope service whose sole
 * purpose is to register the two builtin `MessageProjectionStep`s into
 * `IMessagePipelineService` on construction:
 *
 *   1. `videoResolution` — delegates to `IAgentVideoResolverService.resolve`,
 *      rewriting `kimi-file://` video references to provider-acceptable parts.
 *   2. `mediaCapabilityAdaptation` — delegates to
 *      `projectMediaForModelCapability`, dropping media parts the bound model
 *      cannot accept and substituting a path-bearing placeholder.
 *
 * Registration order is the execution order — `videoResolution` runs before
 * `mediaCapabilityAdaptation` because video resolution may emit new media
 * parts (inline base64) that the capability projector must see. Both steps
 * apply to every `RequestProjection` (normal / strict / media-degraded /
 * media-stripped) and therefore do not override `appliesTo`.
 *
 * Splitting the steps into a dedicated service (rather than registering them
 * inside `AgentMessagePipelineService` itself) keeps the pipeline runner free
 * of domain dependencies — the pipeline only knows the `MessageProjectionStep`
 * interface, not about video resolution or capability adaptation.
 */

import { Disposable } from '#/_base/di/lifecycle';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { createDecorator } from '#/_base/di/instantiation';

import type { Message } from '#/kosong/contract/message';
import { isUnknownCapability } from '#/kosong/contract/capability';
import { mediaInputCapabilityOf, projectMediaForModelCapability } from '#/agent/media/mediaCapabilityProjector';
import { IAgentVideoResolverService } from '#/agent/media/videoResolver';

import { IMessagePipelineService, type MessageProjectionStep } from './messagePipeline';

export interface IAgentMessagePipelineStepsService {
  readonly _serviceBrand: undefined;
}

export const IAgentMessagePipelineStepsService =
  createDecorator<IAgentMessagePipelineStepsService>('agentMessagePipelineStepsService');

export class AgentMessagePipelineStepsService
  extends Disposable
  implements IAgentMessagePipelineStepsService
{
  readonly _serviceBrand: undefined = undefined;

  constructor(
    @IMessagePipelineService private readonly pipeline: IMessagePipelineService,
    @IAgentVideoResolverService private readonly videoResolver: IAgentVideoResolverService,
  ) {
    super();
    this._register(pipeline.register(videoResolutionStep(this.videoResolver)));
    this._register(pipeline.register(mediaCapabilityAdaptationStep));
  }
}

function videoResolutionStep(
  resolver: IAgentVideoResolverService,
): MessageProjectionStep {
  return {
    name: 'videoResolution',
    project: async (ctx) =>
      resolver.resolve(ctx.messages, ctx.requester, ctx.signal),
  };
}

const mediaCapabilityAdaptationStep: MessageProjectionStep = {
  name: 'mediaCapabilityAdaptation',
  project: (ctx) => {
    // When the model's capabilities are entirely unknown (the provider has
    // not catalogued the model), pass media through unchanged — let the
    // provider accept or reject it, rather than preemptively stripping.
    // This mirrors v1's `downgradeUnsupportedMedia` guard via
    // `isUnknownCapability`. Only when capabilities ARE known (and a
    // modality is explicitly false) do we strip and substitute a
    // path-bearing placeholder.
    if (isUnknownCapability(ctx.model.capabilities)) {
      return ctx.messages;
    }
    return projectMediaForModelCapability(
      ctx.messages,
      mediaInputCapabilityOf(ctx.model.capabilities),
    ) as readonly Message[];
  },
};

registerScopedService(
  LifecycleScope.Agent,
  IAgentMessagePipelineStepsService,
  AgentMessagePipelineStepsService,
  ScopeActivation.OnScopeCreated,
  'messagePipeline',
);
