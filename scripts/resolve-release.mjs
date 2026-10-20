#!/usr/bin/env node
// Resolves whether a release should be published by comparing the release
// anchor package version against the latest release tag (docs/specs/
// release-architecture.md "发布触发链").
//
// Trigger rule: publish iff anchorVersion semver-gt the latest v tag.
// Cold start: no comparable tag -> do NOT publish (the first release is
// bootstrapped by a maintainer-created tag, per the architecture doc).
//
// CLI usage (release.yml):
//   RELEASE_COMMIT=<sha> LATEST_TAG=v1.11.0 node scripts/resolve-release.mjs
// Writes should_publish / version / tag to $GITHUB_OUTPUT when set.
//
// The anchor version is read from the COMMITTED revision, not the working
// tree: the Release job's earlier `changesets/action` step runs
// `pnpm run version:release`, which applies every pending changeset to the
// checkout it shares with this step. Reading the file back would then report
// the version of the *next* release PR — the one no human has merged yet —
// and publish a release whose CHANGELOG entry does not exist on main.

import { execFileSync } from 'node:child_process';
import { appendFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const repoDir = resolve(import.meta.dirname, '..');

function parseSemver(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)/.exec(version ?? '');
  return match ? [Number(match[1]), Number(match[2]), Number(match[3])] : null;
}

function semverGt(a, b) {
  const pa = parseSemver(a);
  const pb = parseSemver(b);
  if (!pa || !pb) return false;
  for (let i = 0; i < 3; i += 1) {
    if (pa[i] !== pb[i]) return pa[i] > pb[i];
  }
  return false;
}

function tagVersion(tag) {
  if (typeof tag !== 'string' || tag.length === 0) return null;
  if (tag.startsWith('v')) return tag.slice(1);
  const scoped = /^@mirri-ai\/mirri-code@(.+)$/.exec(tag);
  if (scoped) return scoped[1];
  return null;
}

export function resolveRelease({ anchorVersion, latestTag }) {
  // Trigger rule is purely version comparison: publish iff anchorVersion
  // semver-gt the latest tag (docs/specs/release-architecture.md "发布触发链").
  // An `isReleasePR` field is accepted by callers for interface stability but
  // intentionally not gated here — the workflow-level gate lives in release.yml.
  const tag = `v${anchorVersion}`;
  const latest = tagVersion(latestTag);
  const shouldPublish = latest !== null && semverGt(anchorVersion, latest);
  return { shouldPublish, version: anchorVersion, tag };
}

/// The anchor version as committed at `commit` (`HEAD` when blank).
///
/// Read through git rather than from the file so a working tree that an
/// earlier workflow step has already mutated (changesets/action applies the
/// pending changesets in place) cannot stand in for what main actually
/// contains.
export function readCommittedAnchorVersion(commit = '', dir = repoDir) {
  const rev = commit.trim().length > 0 ? commit.trim() : 'HEAD';
  const source = execFileSync(
    'git',
    ['show', `${rev}:packages/release/package.json`],
    { cwd: dir, encoding: 'utf-8' },
  );
  return JSON.parse(source).version;
}

async function main() {
  const anchorVersion =
    process.env['ANCHOR_VERSION'] ??
    readCommittedAnchorVersion(process.env['RELEASE_COMMIT'] ?? '');
  const latestTag = process.env['LATEST_TAG']?.trim() || null;
  const isReleasePR = process.env['IS_RELEASE_PR'] === 'true';
  const result = resolveRelease({ anchorVersion, latestTag, isReleasePR });
  const githubOutput = process.env['GITHUB_OUTPUT'];
  const lines = [
    `should_publish=${String(result.shouldPublish)}`,
    `version=${result.version}`,
    `tag=${result.tag}`,
  ].join('\n');
  if (githubOutput !== undefined) {
    await appendFile(githubOutput, `${lines}\n`);
  }
  console.log(lines);
}

if (resolve(process.argv[1]) === import.meta.filename) {
  await main();
}
