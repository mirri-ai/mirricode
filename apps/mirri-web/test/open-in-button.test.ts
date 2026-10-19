// test/open-in-button.test.ts
//
// OpenInButton logic (open-in-apps Task 4). The repo has no
// @vue/test-utils, so the component's decision logic is a named pure
// export (`buildOpenInViewModel`) and tested directly, following the
// TurnUsagePill / UsageStatsBar precedent:
// - renders nothing when no apps are detected,
// - the collapsed button shows the last-used app's extracted logo
//   (or the folder icon when the last target has no logo, external-link
//   when there is no last target),
// - quick-open (button body) resolves to the last target, falling back
//   to the first detected app,
// - choosing an entry resolves to that entry's appId,
// - no work directory disables the button.

import { describe, expect, it } from 'vitest';

import {
  buildOpenInViewModel,
  type DetectedApp,
} from '../src/components/chat/OpenInButton.vue';

const APPS: DetectedApp[] = [
  { appId: 'finder', name: 'Finder' },
  { appId: 'zed', name: 'Zed', iconDataUrl: 'data:image/png;base64,AAA' },
];

describe('buildOpenInViewModel', () => {
  it('should render nothing when no apps are detected', () => {
    expect(buildOpenInViewModel([], 'zed', true).visible).toBe(false);
  });

  it('should render even when no last target is known', () => {
    expect(buildOpenInViewModel(APPS, null, true).visible).toBe(true);
  });

  it('should show the last-used app icon when collapsed', () => {
    const vm = buildOpenInViewModel(APPS, 'zed', true);
    expect(vm.visible).toBe(true);
    expect(vm.buttonIcon).toBe('data:image/png;base64,AAA');
  });

  it('should expose the target app when a last target matches', () => {
    expect(buildOpenInViewModel(APPS, 'zed', true).target).toEqual(APPS[1]);
  });

  it('should expose a null target when no last target is known', () => {
    expect(buildOpenInViewModel(APPS, null, true).target).toBeNull();
  });

  it('should expose a null target when the last target matches no detected app', () => {
    expect(buildOpenInViewModel(APPS, 'unknown-app', true).target).toBeNull();
  });

  it('should show the folder icon when the last target has no extracted logo', () => {
    const vm = buildOpenInViewModel(APPS, 'finder', true);
    expect(vm.buttonIcon).toBe(null);
    expect(vm.buttonIconKind).toBe('folder');
  });

  it('should show the external-link icon when there is no last target', () => {
    const vm = buildOpenInViewModel(APPS, null, true);
    expect(vm.buttonIcon).toBe(null);
    expect(vm.buttonIconKind).toBe('external-link');
  });

  it('should resolve the quick-open target to the last target when set', () => {
    expect(buildOpenInViewModel(APPS, 'zed', true).quickOpenAppId).toBe('zed');
  });

  it('should fall back to the first detected app on quick-open when no last target', () => {
    expect(buildOpenInViewModel(APPS, null, true).quickOpenAppId).toBe('finder');
  });

  it('should fall back to the first detected app on quick-open when the last target no longer matches any detected app', () => {
    expect(
      buildOpenInViewModel(APPS, 'unknown-app', true).quickOpenAppId,
    ).toBe('finder');
  });

  it('should disable when there is no work directory', () => {
    expect(buildOpenInViewModel(APPS, 'zed', false).disabled).toBe(true);
    expect(buildOpenInViewModel(APPS, 'zed', true).disabled).toBe(false);
  });
});
