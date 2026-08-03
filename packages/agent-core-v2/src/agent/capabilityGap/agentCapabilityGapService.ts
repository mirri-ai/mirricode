/**
 * Agent-scope service that owns the capability-gap hint injection.
 *
 * Instantiated once per agent (via `registerScopedService` at
 * `LifecycleScope.Agent`). On construction it registers a
 * `ContextInjectionProvider` named `'capability_gap'` that, on every new
 * turn's first step, checks whether the user's prompt contains a modality
 * the bound model cannot handle (image / video / audio). When such a gap
 * exists AND at least one catalog model can cover it, the provider returns a
 * system reminder telling the LLM to dispatch a subagent.
 *
 * See `capabilityGapInjection.ts` for the hint logic.
 */

import { Disposable } from '#/_base/di/lifecycle';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { createDecorator } from '#/_base/di/instantiation';
import { IAgentContextInjectorService } from '#/agent/contextInjector/contextInjector';
import { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import { CapabilityGapInjection } from '#/agent/capabilityGap/capabilityGapInjection';
import { IAgentProfileService } from '#/agent/profile/profile';
import { IModelCatalog } from '#/kosong/model/catalog';
import { ISessionAgentProfileCatalog } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog';

/**
 * Agent-scope service marker — ensures the DI container instantiates
 * `AgentCapabilityGapService` once per agent, which in turn registers the
 * `'capability_gap'` context injection provider.
 */
export interface IAgentCapabilityGapService {
  readonly _serviceBrand: undefined;
}

export const IAgentCapabilityGapService = createDecorator<IAgentCapabilityGapService>(
  'agentCapabilityGapService',
);

export class AgentCapabilityGapService extends Disposable implements IAgentCapabilityGapService {
  readonly _serviceBrand: undefined = undefined;

  constructor(
    @IAgentContextInjectorService dynamicInjector: IAgentContextInjectorService,
    @IAgentContextMemoryService context: IAgentContextMemoryService,
    @IAgentProfileService profile: IAgentProfileService,
    @IModelCatalog modelCatalog: IModelCatalog,
    @ISessionAgentProfileCatalog profileCatalog: ISessionAgentProfileCatalog,
  ) {
    super();
    this._register(
      new CapabilityGapInjection(
        undefined,
        dynamicInjector,
        context,
        profile,
        modelCatalog,
        profileCatalog,
      ),
    );
  }
}

registerScopedService(
  LifecycleScope.Agent,
  IAgentCapabilityGapService,
  AgentCapabilityGapService,
  ScopeActivation.OnScopeCreated,
  'capabilityGap',
);
