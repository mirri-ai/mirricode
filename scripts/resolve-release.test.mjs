/* oxlint-disable typescript/no-floating-promises -- node:test 顶层同步回调的 test() 必然返回未处理的 Promise */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { resolveRelease } from './resolve-release.mjs';

test('should publish when the anchor version advances past the latest v tag', () => {
  const result = resolveRelease({ anchorVersion: '1.12.0', latestTag: 'v1.11.0', isReleasePR: false });
  assert.deepEqual(result, { shouldPublish: true, version: '1.12.0', tag: 'v1.12.0' });
});

test('should not publish when the anchor version equals the latest v tag', () => {
  const result = resolveRelease({ anchorVersion: '1.11.0', latestTag: 'v1.11.0', isReleasePR: false });
  assert.deepEqual(result, { shouldPublish: false, version: '1.11.0', tag: 'v1.11.0' });
});

test('should not publish when there is no historical tag (cold start guard)', () => {
  const result = resolveRelease({ anchorVersion: '1.11.0', latestTag: null, isReleasePR: false });
  assert.deepEqual(result, { shouldPublish: false, version: '1.11.0', tag: 'v1.11.0' });
});

test('should fall back to comparing against legacy scoped tags when only they exist', () => {
  const result = resolveRelease({
    anchorVersion: '1.12.0',
    latestTag: '@mirri-ai/mirri-code@1.11.0',
    isReleasePR: false,
  });
  assert.deepEqual(result, { shouldPublish: true, version: '1.12.0', tag: 'v1.12.0' });
});

test('should not publish when the anchor version regresses below the latest tag', () => {
  const result = resolveRelease({ anchorVersion: '1.11.0', latestTag: 'v1.12.0', isReleasePR: false });
  assert.deepEqual(result, { shouldPublish: false, version: '1.11.0', tag: 'v1.11.0' });
});
