/**
 * Process orchestrator tests — journey scenario process lifecycle (ADR-002).
 *
 * Each case boots a REAL `mirri serve` process with an isolated home dir,
 * which is exactly what the journey scenarios need: process birth/death is
 * the journey stage under test (spec R1/R9).
 */
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { fakeProviderConfigToml, stopServe, type ProcHandle } from '../src/proc';
import { startServe } from '../src/proc';

const procs: ProcHandle[] = [];

afterEach(async () => {
  for (const proc of procs.splice(0)) {
    await stopServe(proc, 'kill').catch(() => {});
  }
});

describe('process orchestrator (journey scenarios)', () => {
  it('should boot the binary from MIRRICODE_E2E_RUST_BIN when it is set', async () => {
    // 假如：docker（或本地）环境通过 MIRRICODE_E2E_RUST_BIN 显式指定了 rust 二进制路径
    // 当：编排器启动 serve 进程
    // 那么：启动的正是该环境变量指向的二进制（容器内 CARGO_TARGET_DIR 构建产物优先于本地路径）
    const { resolve } = await import('node:path');
    const { fileURLToPath } = await import('node:url');
    const repoRoot = resolve(fileURLToPath(new URL('../../../', import.meta.url)));
    const realBin = join(repoRoot, 'rust', 'target', 'debug', 'mirri');
    expect(existsSync(realBin)).toBe(true);

    const previous = process.env['MIRRICODE_E2E_RUST_BIN'];
    process.env['MIRRICODE_E2E_RUST_BIN'] = realBin;
    try {
      const proc = await startServe({});
      procs.push(proc);
      const healthz = await fetch(`http://127.0.0.1:${proc.port}/api/v1/healthz`);
      const body = (await healthz.json()) as { data?: { ok?: boolean } };
      expect(body.data?.ok).toBe(true);
    } finally {
      if (previous === undefined) {
        delete process.env['MIRRICODE_E2E_RUST_BIN'];
      } else {
        process.env['MIRRICODE_E2E_RUST_BIN'] = previous;
      }
    }
  }, 60_000);

  it('should boot a fresh serve process and create home directories', async () => {
    // 假如：一台全新机器（空 MIRRICODE_HOME、无 config.toml）
    // 当：用户第一次启动 mirri serve
    // 那么：healthz 就绪；home 目录自举；零崩溃
    const proc = await startServe({});
    procs.push(proc);

    const healthz = await fetch(`http://127.0.0.1:${proc.port}/api/v1/healthz`);
    expect(healthz.status).toBe(200);
    const body = (await healthz.json()) as { data?: { ok?: boolean } };
    expect(body.data?.ok).toBe(true);

    expect(existsSync(join(proc.homeDir, 'v3_sessions'))).toBe(true);
    expect(existsSync(join(proc.homeDir, 'logs'))).toBe(true);
  }, 60_000);

  it('should report backend v3 in meta for a fresh boot', async () => {
    const proc = await startServe({});
    procs.push(proc);

    const meta = await fetch(`http://127.0.0.1:${proc.port}/api/v1/meta`);
    const body = (await meta.json()) as { data?: { backend?: string } };
    expect(body.data?.backend).toBe('v3');
  }, 60_000);

  it('should write a config.toml when one is provided and accept model config', async () => {
    // 假如：用户已配置好模型（config.toml 指向 fake provider）
    // 当：启动 serve
    // 那么：createSession 不再被 no-model 拒绝
    const proc = await startServe({
      configToml: fakeProviderConfigToml('http://127.0.0.1:59999'),
    });
    procs.push(proc);

    const res = await fetch(`http://127.0.0.1:${proc.port}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
    });
    expect(res.status).toBe(200);
  }, 60_000);

  it('should kill the process immediately in kill mode', async () => {
    const proc = await startServe({});
    procs.push(proc);
    const pid = proc.child.pid;

    await stopServe(proc, 'kill');
    // SIGKILL: Node reports exitCode null + killed true; the OS-level check is the source of truth.
    expect(proc.child.killed).toBe(true);
    if (pid !== undefined) {
      let alive = true;
    try {
      process.kill(pid, 0);
    } catch {
      alive = false;
    }
    if (alive) {
      // PID reuse is theoretically possible; give it a moment then fail.
      await new Promise((r) => setTimeout(r, 500));
      expect(() => process.kill(pid, 0)).toThrow();
    }
    }
  }, 60_000);

  it('should exit cleanly via shutdown in graceful mode', async () => {
    const proc = await startServe({});
    procs.push(proc);

    const start = Date.now();
    await stopServe(proc, 'graceful');
    expect(proc.child.exitCode).toBe(0);
    expect(Date.now() - start).toBeLessThan(10_000);
  }, 60_000);

  it('should run a one-shot CLI command and capture its exit code', async () => {
    const { runOneShot } = await import('../src/proc');
    const homeDir = mkdtempSync(join(tmpdir(), 'mirri-oneshot-'));

    // `mirri doctor` is a harmless one-shot; exit 0 on a valid (or absent) config
    const result = await runOneShot(['doctor'], homeDir);
    expect(result.code).toBe(0);
    expect(result.stdout).toContain('Mirri doctor');
  }, 30_000);
});

// Keep execFileSync referenced for parity with tsx-run scenarios (no unused import lint)
void execFileSync;
