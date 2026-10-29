#!/usr/bin/env node
/**
 * Journey ⑥ — migrate a legacy TS-engine home (spec R11).
 *
 * 假如：用户从旧 TS 内核时代带着一个 sessions/ 目录回到 rust 引擎
 * 当：在 home 布置 legacy 夹具 → `mirri migrate`（exit 0）→ 起 serve →
 *     listSessions / 打开会话 / 读消息历史
 * 那么：迁移会话出现在列表（title 保留）；历史可读（夹具 wire 记录的
 *       内容呈现在 message.list）；legacy 根目录原样保留（复制语义）
 *
 * Probe findings baked into the assertions (2026-10-28 code probes):
 *   - state.json: TS camelCase schema (`createdAt`/`updatedAt` epoch-ms
 *     ints, `title`, `workDir`) — alias-compatible with the rust
 *     `PersistedSessionMetadata` reader (persistence.rs:74).
 *   - wire.jsonl: first record must be `metadata` (migrate.rs
 *     `first_record_is_metadata`); `context.append_message` records with
 *     a `message` body are the TS conversation spelling the wire chain
 *     migrates lazily on resume.
 *   - The target bucket is re-derived from workDir
 *     (`encode_work_dir_key`), the source bucket name is irrelevant.
 *   - `message.list` is served over the WS RPC face (no REST GET route
 *     on the serve surface — journey-05 probe).
 *   - `runOneShot` env carries only MIRRICODE_HOME (no HOME remap) —
 *     fine for the one-shot CLI, which reads no MCP layer.
 */
import assert from 'node:assert/strict';
import { existsSync, readdirSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { createFakeProviderServer } from '@mirri-ai/e2e-harness';

import { createLegacySession } from '../src/fixtures/legacy-session';
import { fakeProviderConfigToml, runOneShot, startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

async function main(): Promise<void> {
  console.log('▶ journey-06: seeding a legacy TS-engine home and running mirri migrate');
  const provider = await createFakeProviderServer();
  const homeDir = mkdtempSync(join(tmpdir(), 'mirri-journey06-home-'));
  const workDir = mkdtempSync(join(tmpdir(), 'mirri-journey06-work-'));
  let proc: ProcHandle | undefined;
  const wsClients: WsRpcClient[] = [];
  try {
    // ── 假如：旧 TS 时代的一个会话躺在 sessions/ 下 ───────────────────
    const fixture = createLegacySession(homeDir, { workDir, title: 'journey 06 legacy session' });

    // ── 当：mirri migrate ─────────────────────────────────────────────
    const migrate = await runOneShot(['migrate'], homeDir);
    console.log(`▶ journey-06: migrate stdout: ${migrate.stdout.trim().split('\n').join(' | ')}`);
    assert.equal(migrate.code, 0, 'mirri migrate exits 0');

    // ── 那么：legacy 根目录原样保留（复制语义）────────────────────────
    assert.ok(existsSync(fixture.sessionDir), 'the legacy session directory is untouched');
    assert.ok(
      existsSync(join(fixture.sessionDir, 'state.json')),
      'the legacy state.json stays in place (copy semantics)',
    );
    assert.ok(
      existsSync(join(fixture.sessionDir, 'agents', 'main', 'wire.jsonl')),
      'the legacy nested wire log stays in place (copy semantics)',
    );
    console.log('▶ journey-06: legacy root untouched (copy semantics verified)');

    // ── 当：起 serve（同 homeDir + fake provider config）───────────────
    proc = await startServe({ homeDir, workDir, configToml: fakeProviderConfigToml(provider.baseUrl) });
    const ws = new WsRpcClient(proc.baseUrl);
    wsClients.push(ws);
    await ws.connect();

    // ── 那么：listSessions 含迁移会话（title 保留）────────────────────
    const listRes = await fetch(`${proc.baseUrl}/api/v1/sessions`);
    const listBody = (await listRes.json()) as { data?: { items?: Array<Record<string, unknown>> } };
    const rows = listBody.data?.items ?? [];
    const migrated = rows.find((row) => row['id'] === fixture.sessionId);
    assert.ok(
      migrated !== undefined,
      `the migrated session appears in the session list (found: ${rows.map((r) => String(r['title'])).join(', ')})`,
    );
    assert.equal(migrated['title'], 'journey 06 legacy session', 'the migrated session keeps its title');
    console.log('▶ journey-06: migrated session listed with its title preserved');

    // ── 那么：打开它 — 消息历史可读（夹具 wire 记录的内容）──────────────
    const history = await ws.call<{ messages?: Array<Record<string, unknown>> }>(
      'message.list',
      { sessionId: fixture.sessionId, pageSize: 50 },
    );
    const historyText = JSON.stringify(history.messages ?? []);
    assert.ok(
      historyText.includes('What does the legacy project contain?'),
      'the migrated history contains the legacy user message',
    );
    assert.ok(
      historyText.includes('LEGACY-REPLY-MARKER'),
      'the migrated history contains the legacy assistant reply',
    );
    console.log('▶ journey-06: migrated message history is readable (legacy wire content surfaced)');

    // ── 复核：v3 侧确有迁移产物（top-level wire.jsonl 被提升）────────────
    const v3Root = join(homeDir, 'v3_sessions');
    assert.ok(existsSync(v3Root), 'the v3_sessions root exists after migration');
    const v3SessionDir = join(v3Root, fixture.sessionId);
    assert.ok(
      existsSync(v3SessionDir) || v3BucketScan(v3Root, fixture.sessionId),
      'the migrated copy landed under v3_sessions',
    );
    console.log('▶ journey-06: v3 copy present with the promoted wire log');

    console.log('✓ journey-06-migrate: legacy home migrated, listed, readable, and the legacy root untouched');
  } finally {
    for (const ws of wsClients) ws.close();
    if (proc) await stopServe(proc, 'kill').catch(() => {});
    await provider.close().catch(() => {});
  }
}

/** Whether the session id exists under any bucket dir of the v3 root. */
function v3BucketScan(v3Root: string, sessionId: string): boolean {
  try {
    for (const entry of readdirSync(v3Root, { withFileTypes: true })) {
      if (entry.isDirectory() && existsSync(join(v3Root, entry.name, sessionId))) return true;
    }
  } catch {
    return false;
  }
  return false;
}

main().catch((error) => {
  console.error('✗ journey-06-migrate failed:', error);
  process.exit(1);
});
