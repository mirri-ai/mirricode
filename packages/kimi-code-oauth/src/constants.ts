import type { OAuthFlowConfig } from './types';

export const DEFAULT_MIRRICODE_OAUTH_HOST = 'https://auth.kimi.com';

export const MIRRICODE_FLOW_CONFIG: OAuthFlowConfig = {
  name: 'mirri-code',
  oauthHost:
    process.env['MIRRICODE_OAUTH_HOST'] ??
    process.env['MIRRI_OAUTH_HOST'] ??
    DEFAULT_MIRRICODE_OAUTH_HOST,
  clientId: '17e5f671-d194-4dfb-9706-5516cb48c098',
};
