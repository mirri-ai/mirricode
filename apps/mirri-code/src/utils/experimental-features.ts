import type {
  ExperimentalFeatureState,
  ExperimentalFlagMap,
} from '@mirri-ai/mirri-code-sdk';

export function experimentalFeatureMap(
  features: readonly Pick<ExperimentalFeatureState, 'id' | 'enabled'>[],
): ExperimentalFlagMap {
  return Object.fromEntries(features.map((feature) => [feature.id, feature.enabled]));
}
