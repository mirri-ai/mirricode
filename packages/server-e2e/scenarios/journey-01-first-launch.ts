#!/usr/bin/env node
/**
 * Journey ① — first launch on a brand-new machine (spec R1).
 *
 * 假如：一台全新机器（空 MIRRICODE_HOME、无 config.toml、无任何用户数据）
 * 当：用户第一次启动 `mirri serve`
 * 那么：healthz 就绪；home 目录自举（v3_sessions / logs）；meta 报告 rust v3 后端；零崩溃
 *
 * Standalone executable scenario (ADR-002): boots its own serve process
 * through the orchestrator, asserts on user-perceivable outcomes only
 * (readiness, directory bootstrap, backend identity), and exits 0/1.
 */
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

import { startServe, stopServe } from '../src/proc';

async function main(): Promise<void> {
  console.log('▶ journey-01: booting a fresh serve process (empty home, no config)');
  const proc = await startServe({});

  try {
    // ── healthz 就绪 ─────────────────────────────────────────────────────
    const healthz = await fetch(`${proc.baseUrl}/api/v1/healthz`);
    assert.equal(healthz.status, 200, 'healthz answers 200');
    const healthzBody = (await healthz.json()) as { data?: { ok?: boolean } };
    assert.equal(healthzBody.data?.ok, true, 'healthz envelope reports ok:true');
    console.log('▶ journey-01: healthz ok');

    // ── home 目录自举 ────────────────────────────────────────────────────
    assert.equal(
      existsSync(join(proc.homeDir, 'v3_sessions')),
      true,
      'v3_sessions directory is auto-created under the home dir',
    );
    assert.equal(
      existsSync(join(proc.homeDir, 'logs')),
      true,
      'logs directory is auto-created under the home dir',
    );
    console.log('▶ journey-01: home directories bootstrapped (v3_sessions, logs)');

    // ── meta 后端标识 ────────────────────────────────────────────────────
    const meta = await fetch(`${proc.baseUrl}/api/v1/meta`);
    assert.equal(meta.status, 200, 'meta answers 200');
    const metaBody = (await meta.json()) as {
      data?: { backend?: string; server_version?: string };
    };
    assert.equal(metaBody.data?.backend, 'v3', 'meta reports backend v3 (rust engine)');
    console.log(`▶ journey-01: meta backend=v3 (server ${metaBody.data?.server_version ?? '?'})`);

    // ── 零崩溃：进程仍然存活 ──────────────────────────────────────────────
    const secondProbe = await fetch(`${proc.baseUrl}/api/v1/healthz`);
    assert.equal(secondProbe.status, 200, 'healthz still answers after the probes (no crash)');
    assert.equal(proc.child.exitCode, null, 'serve process has not exited');

    console.log('✓ journey-01-first-launch: fresh boot is ready, bootstrapped, and stable');
  } finally {
    await stopServe(proc, 'kill').catch(() => {});
  }
}

main().catch((error) => {
  console.error('✗ journey-01-first-launch failed:', error);
  process.exit(1);
});
