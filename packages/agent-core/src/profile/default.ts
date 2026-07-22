import agentYaml from './default/agent.yaml?raw';
import coderYaml from './default/coder.yaml?raw';
import exploreYaml from './default/explore.yaml?raw';
import initMd from './default/init.md?raw';
import planYaml from './default/plan.yaml?raw';
import systemMd from './default/system.md?raw';
import { loadAgentProfilesFromSources } from './load';

// Keyed by the source path the profile loader expects: profile YAML files
// plus any file referenced through `systemPromptPath`.
const PROFILE_SOURCES: Record<string, string> = {
  'profile/default/agent.yaml': agentYaml,
  'profile/default/coder.yaml': coderYaml,
  'profile/default/explore.yaml': exploreYaml,
  'profile/default/plan.yaml': planYaml,
  'profile/default/system.md': systemMd,
};

const DEFAULT_PROFILE_PATHS = ['agent.yaml', 'coder.yaml', 'explore.yaml', 'plan.yaml'].map(
  (file) => `profile/default/${file}`,
);

export const DEFAULT_INIT_PROMPT = initMd;

export const DEFAULT_AGENT_PROFILES = loadAgentProfilesFromSources(
  DEFAULT_PROFILE_PATHS,
  PROFILE_SOURCES,
);

/**
 * Expose the raw built-in profile sources so the ProfileRegistry can
 * re-resolve them together with custom profiles in a single pass —
 * preserving systemPromptPath, promptVars, subagents, and extends.
 */
export const DEFAULT_PROFILE_SOURCES = PROFILE_SOURCES;
export const DEFAULT_PROFILE_FILE_PATHS = DEFAULT_PROFILE_PATHS;
