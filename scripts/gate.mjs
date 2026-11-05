#!/usr/bin/env node
// Unified quality gate entrypoint — fast-fail, lightest first.
//
// Usage:
//   node scripts/gate.mjs            # fast gates only (no compile, no tests)  ~10s
//   node scripts/gate.mjs --full     # fast gates + typecheck + lint + clippy + ALL tests
//   node scripts/gate.mjs --rust     # fast gates + clippy (no TS, no tests)
//   node scripts/gate.mjs --list     # list gate definitions and exit
//
// Design (docs/specs/rust-core-design/16-test-execution-strategy.md follow-up):
//   stages run lightest-first so a 0.1s failure never waits behind a 7min
//   test run. Each stage is a child process; the first failure aborts the
//   run with a clear stage label and the wall time it took.
//
// Known-failure ledger: pre-existing failures on `latest` (cron-batch
// leftovers) are recorded in KNOWN_FAILURES with an owner note; they are
// still REPORTED (and counted in the exit summary) but do not block
// earlier stages from running, so the ledger is visible debt, not silent
// rot. A stage whose failure is *listed* exits the run with the special
// code 2 so callers can distinguish "new breakage" (1) from "known debt".

import { spawnSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';

const ROOT = new URL('..', import.meta.url).pathname;

// --- stage definitions (lightest first — fast-fail ordering) -------------------
// Each: { name, cmd (string[]), cwd, knownFailure?: 'note' }
const STAGES = [
  {
    name: 'service-naming',
    desc: 'service naming conventions',
    cmd: ['node', 'scripts/check-service-naming.mjs'],
  },
  {
    name: 'sherif',
    desc: 'monorepo dependency rules',
    cmd: [pnpmBin(), 'exec', 'sherif'],
  },
  {
    name: 'rust-zero-io',
    desc: 'mirri-core direct-IO ban',
    cmd: ['node', 'rust/scripts/check-zero-io.mjs'],
  },
  {
    name: 'rust-spawn',
    desc: 'raw Command::new ban (mirri_os wrappers required)',
    cmd: ['node', 'scripts/check-spawn-usage.mjs'],
  },
  {
    name: 'rust-test-hygiene',
    desc: 'env-lock / sleep / layer-tag ratchet (16-design §4.5)',
    cmd: ['node', 'rust/scripts/check-test-hygiene.mjs'],
  },
];

// Heavier stages, only with flags:
const RUST_STAGES = [
  {
    name: 'rust-clippy',
    desc: 'cargo clippy --workspace --all-targets -D warnings',
    cmd: ['cargo', 'clippy', '--manifest-path', 'rust/Cargo.toml', '--workspace', '--all-targets', '--', '-D', 'warnings'],
    // minutes on cold cache — run in the background if you are impatient
  },
];

const FULL_STAGES = [
  {
    name: 'ts-typecheck',
    desc: 'build packages + typecheck web/desktop',
    cmd: [pnpmBin(), 'run', 'typecheck'],
  },
  {
    name: 'oxlint',
    desc: 'oxlint --type-aware',
    cmd: [pnpmBin(), 'run', 'lint'],
  },
  {
    name: 'rust-clippy',
    desc: 'cargo clippy --workspace --all-targets -D warnings',
    cmd: ['cargo', 'clippy', '--manifest-path', 'rust/Cargo.toml', '--workspace', '--all-targets', '--', '-D', 'warnings'],
  },
  {
    name: 'rust-tests',
    desc: 'cargo test --workspace --locked (the heaviest — always last)',
    cmd: ['cargo', 'test', '--manifest-path', 'rust/Cargo.toml', '--workspace', '--locked'],
    note: 'expect ~7min warm cache; run with --full only',
  },
  {
    name: 'ts-tests',
    desc: 'web + desktop vitest (MIRRICODE_E2E_SKIP_RUST=1)',
    cmd: [pnpmBin(), 'run', 'test'],
  },
];

// --- arg parsing ----------------------------------------------------------------
const args = new Set(process.argv.slice(2));
if (args.has('--list') || args.has('-l')) {
  console.log('gate stages (execution order, fast-fail):');
  for (const s of [...STAGES, { name: '— rust-only:', cmd: [] }, ...RUST_STAGES, { name: '— full-only:', cmd: [] }, ...FULL_STAGES]) {
    if (s.desc === undefined && s.note === undefined) { console.log(`\n${s.name}`); continue; }
    console.log(`  ${s.name.padEnd(20)} ${s.desc}${s.note ? `  (${s.note})` : ''}${s.knownFailure ? `  [KNOWN-FAILURE: ${s.knownFailure}]` : ''}`);
  }
  process.exit(0);
}
const MODE = args.has('--full') ? 'full' : args.has('--rust') ? 'rust' : 'fast';

// --- runner ----------------------------------------------------------------------
function pnpmBin() {
  return process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm';
}

function runStage(stage) {
  const t0 = performance.now();
  const res = spawnSync(stage.cmd[0], stage.cmd.slice(1), {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, FORCE_COLOR: '0' },
  });
  const ms = performance.now() - t0;
  return { ...stage, ok: res.status === 0, ms, output: res };
}

const failures = [];
const known = [];
const queue = MODE === 'fast' ? STAGES : MODE === 'rust' ? [...STAGES, ...RUST_STAGES] : [...STAGES, ...FULL_STAGES];

console.log(`gate: ${queue.length} stage(s), mode=${MODE} (fast < rust < full; tests run last)\n`);

for (const stage of queue) {
  process.stdout.write(`▸ ${stage.name.padEnd(20)} ${stage.desc} ... `);
  const r = runStage(stage);
  const sec = (r.ms / 1000).toFixed(1);
  if (r.ok) {
    console.log(`ok (${sec}s)`);
    continue;
  }
  if (stage.knownFailure) {
    console.log(`KNOWN-FAILURE (${sec}s) — ${stage.knownFailure}`);
    known.push(stage.name);
    continue; // known debt: report but keep going — visibility without blocking
  }
  console.log(`FAILED (${sec}s)`);
  const out = (r.output.stdout?.toString() ?? '') + (r.output.stderr?.toString() ?? '');
  // tail of the child output — enough to act on without flooding the log
  const lines = out.trimEnd().split('\n');
  console.log(lines.slice(-15).map((l) => `    ${l}`).join('\n'));
  failures.push(stage.name);
  console.log(`\ngate: aborting at "${stage.name}" (fast-fail). Fix it, then re-run.`);
  break;
}

// --- summary ----------------------------------------------------------------------
console.log('\n' + '─'.repeat(60));
if (failures.length > 0) {
  console.log(`gate: FAIL — new failure in: ${failures.join(', ')}`);
  process.exit(1);
}
if (known.length > 0) {
  console.log(`gate: PASS with ${known.length} known-failure(s): ${known.join(', ')} — see KNOWN_FAILURES in scripts/gate.mjs`);
  process.exit(2);
}
console.log(`gate: PASS — all ${queue.length} stage(s) green (mode=${MODE})`);
