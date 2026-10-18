/**
 * `agentProfileCatalog` domain error codes — management-plane rejections
 * (the engine-side spelling of the REST envelope codes 40418 / 41307).
 */

import { registerErrorDomain, type ErrorDomain } from '#/_base/errors/codes';

export const AgentProfileManagementErrors = {
  codes: {
    AGENT_PROFILE_NOT_FOUND: 'agent_profile.not_found',
    AGENT_PROFILE_ESSENTIAL: 'agent_profile.essential',
  },
} as const satisfies ErrorDomain;

registerErrorDomain(AgentProfileManagementErrors);
