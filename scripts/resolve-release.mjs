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
//   ANCHOR_VERSION=1.12.0 LATEST_TAG=v1.11.0 node scripts/resolve-release.mjs
// Writes should_publish / version / tag to $GITHUB_OUTPUT when set.

import { appendFile, readFile } from 'node:fs/promises';
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

async function main() {
  const anchorVersion =
    process.env['ANCHOR_VERSION'] ??
    JSON.parse(await readFile(resolve(repoDir, 'packages/release/package.json'), 'utf-8'))
      .version;
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
