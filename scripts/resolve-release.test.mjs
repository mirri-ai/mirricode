/* oxlint-disable typescript/no-floating-promises -- node:test 顶层同步回调的 test() 必然返回未处理的 Promise */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { readCommittedAnchorVersion, resolveRelease } from './resolve-release.mjs';

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

test('should report the committed anchor version when the working tree carries a newer one', () => {
  // Given a repo whose committed anchor is 1.11.0 and whose working tree has
  // been bumped to 2.0.0 — the state the Release job is in after
  // `changesets/action` runs `version:release` against the same checkout
  const repo = mkdtempSync(join(tmpdir(), 'resolve-release-'));
  const pkg = join(repo, 'packages', 'release');
  mkdirSync(pkg, { recursive: true });
  const write = (version) =>
    writeFileSync(
      join(pkg, 'package.json'),
      `${JSON.stringify({ name: '@mirri-ai/release', version }, null, 2)}\n`,
    );
  const git = (...args) =>
    execFileSync('git', args, { cwd: repo, encoding: 'utf-8', stdio: 'pipe' });
  write('1.11.0');
  git('init', '--quiet');
  git('add', '.');
  git(
    '-c',
    'user.email=test@example.test',
    '-c',
    'user.name=Resolve Release Test',
    'commit',
    '--quiet',
    '-m',
    'anchor 1.11.0',
  );
  write('2.0.0');

  // Then the committed 1.11.0 wins over the working tree's 2.0.0, so an
  // unmerged release PR cannot trigger a publish
  assert.equal(readCommittedAnchorVersion('HEAD', repo), '1.11.0');
});
