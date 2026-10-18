// apps/mirri-web/test/model-capabilities.test.ts
// G-25 capability-badge rendering data for the Add-Provider dialog (catalog
// model preview + custom editor). The dialog's hardcoded `·thinking` suffix
// and disappearing non-preset capabilities are replaced by one pure mapping:
// capabilities (facts) -> badge rows ({ key, label, disabled }) where labels
// are i18n message keys (never raw English strings) and preset capabilities
// are interactive chips while anything else (e.g. `video_in`) stays visible
// as a disabled badge.

import { describe, expect, it } from 'vitest';
import {
  capabilityLabelKey,
  CAPABILITY_PRESETS,
  modelCapabilityBadges,
  type ModelCapabilityBadge,
} from '../src/lib/modelCapabilities';

describe('capabilityLabelKey', () => {
  it('should map known capability tags to settings.models.capability.* i18n keys (no raw strings)', () => {
    expect(capabilityLabelKey('thinking')).toBe('settings.models.capability.thinking');
    expect(capabilityLabelKey('image_in')).toBe('settings.models.capability.imageIn');
    expect(capabilityLabelKey('tool_use')).toBe('settings.models.capability.toolUse');
    expect(capabilityLabelKey('video_in')).toBe('settings.models.capability.videoIn');
    expect(capabilityLabelKey('audio_in')).toBe('settings.models.capability.audioIn');
    expect(capabilityLabelKey('dynamically_loaded_tools')).toBe(
      'settings.models.capability.dynamicTools',
    );
  });

  it('should return a generic capability key for unknown tags instead of a raw string', () => {
    expect(capabilityLabelKey('custom_thing')).toBe('settings.models.capability.unknown');
  });
});

describe('modelCapabilityBadges', () => {
  it('should render preset capabilities as enabled badges when the model declares them', () => {
    const badges = modelCapabilityBadges(['image_in', 'thinking', 'tool_use'], false);
    expect(badges).toEqual([
      { key: 'image_in', label: 'settings.models.capability.imageIn', disabled: false },
      { key: 'thinking', label: 'settings.models.capability.thinking', disabled: false },
      { key: 'tool_use', label: 'settings.models.capability.toolUse', disabled: false },
    ]);
  });

  it('should render non-preset capabilities (e.g. video_in) as disabled badges instead of dropping them', () => {
    const badges = modelCapabilityBadges(['image_in', 'video_in'], false);
    expect(badges).toEqual([
      { key: 'image_in', label: 'settings.models.capability.imageIn', disabled: false },
      { key: 'video_in', label: 'settings.models.capability.videoIn', disabled: true },
    ]);
  });

  it('should keep unknown capability tags visible via the generic label', () => {
    const badges = modelCapabilityBadges(['mystery_cap'], false);
    expect(badges).toEqual([
      { key: 'mystery_cap', label: 'settings.models.capability.unknown', disabled: true },
    ]);
  });

  it('should emit a thinking badge when the reasoning fact is true even if capabilities omit it', () => {
    const badges = modelCapabilityBadges([], true);
    expect(badges).toEqual([
      { key: 'thinking', label: 'settings.models.capability.thinking', disabled: false },
    ]);
  });

  it('should deduplicate thinking when both the reasoning fact and the capabilities array declare it', () => {
    const badges = modelCapabilityBadges(['thinking', 'tool_use'], true);
    const thinking = badges.filter((b: ModelCapabilityBadge) => b.key === 'thinking');
    expect(thinking).toHaveLength(1);
    expect(thinking[0]?.disabled).toBe(false);
  });

  it('should return an empty list when the model declares no capabilities and no reasoning', () => {
    expect(modelCapabilityBadges(undefined, false)).toEqual([]);
    expect(modelCapabilityBadges([], false)).toEqual([]);
  });

  it('should keep the preset order stable regardless of input order', () => {
    const badges = modelCapabilityBadges(['tool_use', 'image_in', 'thinking'], false);
    expect(badges.map((b) => b.key)).toEqual(['image_in', 'thinking', 'tool_use']);
  });
});

describe('CAPABILITY_PRESETS', () => {
  it('should list the interactive presets the custom editor offers', () => {
    expect(CAPABILITY_PRESETS).toEqual(['image_in', 'thinking', 'tool_use']);
  });
});
