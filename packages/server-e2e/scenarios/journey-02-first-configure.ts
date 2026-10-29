#!/usr/bin/env node
/**
 * Journey ② — first configure (spec R2 + R3).
 *
 * 假如：用户第一次启动 mirri serve（无 config.toml），尚未配置任何模型
 * 当：① 用户尝试创建会话 → ② 用户按引导写入 config.toml（进程不重启）
 * 那么：① 用户得到含 default_model 引导词的可自助错误；
 *       ② 同一进程（pid 不变）、同一会话再发消息得到模型替身回复文本
 *
 * Probe findings baked into the assertions (2026-10-28 live probes):
 *   - With no config, `POST /api/v1/sessions` answers HTTP 200 with an
 *     ENVELOPE error body (code -32602, domainCode request.invalid) whose
 *     msg says "no model configured: set default_model in config.toml...".
 *     The user-facing failure is on session CREATION, not submitPrompt.
 *   - config.toml written AFTER boot is hot-reloaded (200ms debounce
 *     watcher, `spawn_config_watchers` in mirri-cli) — same pid, no restart.
 *   - A post-boot config cannot rely on `providers.<id>.api_key`: the
 *     boot-time credential migration (bootstrap.rs) already ran. The
 *     `[providers.<id>.env]` sub-table (`OPENAI_API_KEY`) is resolved at
 *     LLM-factory call time and works for hot-reloaded configs.
 */
import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { createFakeProviderServer, type FakeProviderServer } from '@mirri-ai/e2e-harness';

import { startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

/** Config text placing the fake provider behind `fake/model` with the API
 *  key in the provider `env` sub-table — the form a hot-reloaded (post-boot)
 *  config can use, since boot-time credential migration has already run. */
function hotReloadableFakeProviderConfigToml(fakeUrl: string): string {
  return `default_model = "fake/model"

[providers.fake]
type = "openai"
base_url = "${fakeUrl}"

[providers.fake.env]
OPENAI_API_KEY = "sk-fake"

[models."fake/model"]
provider = "fake"
model = "fake-model"
max_context_size = 128000
`;
}

async function main(): Promise<void> {
  console.log('▶ journey-02: booting fresh serve (no config) + fake provider');
  const provider = await createFakeProviderServer();
  let proc: ProcHandle | undefined;
  const wsClients: WsRpcClient[] = [];
  try {
    proc = await startServe({});
    const pid = proc.child.pid;
    assert.ok(pid !== undefined, 'serve process has a pid');

    // ── R2：空配置下的可自助引导 ────────────────────────────────────────
    const createRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
    });
    const createBody = (await createRes.json()) as {
      code?: number;
      msg?: string;
      domainCode?: string;
    };
    assert.ok(createBody.msg !== undefined, 'no-config session creation fails with an error message');
    assert.notEqual(createBody.code, 0, 'no-config session creation does not succeed');
    assert.match(
      createBody.msg,
      /default_model/,
      'the error the user sees carries default_model guidance text',
    );
    console.log(`▶ journey-02: R2 no-config error: ${createBody.msg}`);

    // ── R3：写入 config.toml（热加载，不重启进程）────────────────────────
    writeFileSync(
      join(proc.homeDir, 'config.toml'),
      hotReloadableFakeProviderConfigToml(provider.baseUrl),
      'utf8',
    );
    console.log('▶ journey-02: config.toml written after boot (hot reload)');

    // Poll until the hot-reload watcher applies the config (debounce + fs
    // event latency) — the createSession call that failed above now succeeds.
    const reloadDeadline = Date.now() + 15_000;
    let sid: string | undefined;
    for (;;) {
      const res = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
      });
      const body = (await res.json()) as { code?: number; data?: { id?: string } };
      if (res.status === 200 && body.code === 0 && typeof body.data?.id === 'string') {
        sid = body.data.id;
        break;
      }
      if (Date.now() > reloadDeadline) {
        throw new Error(`config hot-reload never made session creation succeed: ${JSON.stringify(body)}`);
      }
      await new Promise((r) => setTimeout(r, 300));
    }
    console.log(`▶ journey-02: session ${sid} created after hot reload`);

    // 关键断言：config 写入到生效之间无进程重启（spawn 的 pid 前后一致）
    assert.equal(proc.child.pid, pid, 'the serve process pid is unchanged (no restart between config write and effect)');
    assert.equal(proc.child.exitCode, null, 'the serve process is still alive');

    // ── R3：同一会话发消息 → 得到替身回复 ────────────────────────────────
    const ws = new WsRpcClient(proc.baseUrl);
    wsClients.push(ws);
    await ws.connect();
    await ws.subscribe(sid);
    // Re-arm BEFORE submit: the fake provider 500s on an empty queue.
    provider.nextText('JOURNEY_TWO_REPLY_FROM_FAKE_MODEL');

    const sendRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ input: [{ type: 'text', text: 'hello' }], mode: 'fire' }),
    });
    const sendBody = (await sendRes.json()) as { code?: number; data?: { turn_id?: string } };
    assert.equal(sendBody.code, 0, 'message send succeeds on the configured session');
    assert.ok(typeof sendBody.data?.turn_id === 'string', 'send returns a turn id');
    console.log(`▶ journey-02: prompt submitted (turn ${sendBody.data?.turn_id})`);

    // The reply text the user sees: collect assistant deltas until the turn ends.
    let replyText = '';
    ws.onNotification((n) => {
      if (n.method === 'event.assistant.delta' && n.params['sessionId'] === sid) {
        replyText += typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : '';
      }
    });
    const ended = await ws.waitForEvent(sid, 'event.turn.ended', 60_000);
    assert.equal(ended['reason'], 'completed', 'the turn completes normally');
    assert.equal(
      replyText,
      'JOURNEY_TWO_REPLY_FROM_FAKE_MODEL',
      'the same session now receives the fake provider reply text',
    );

    // 进程仍然活着，pid 依然一致（全程无重启）
    assert.equal(proc.child.pid, pid, 'the serve pid is unchanged end-to-end');

    console.log('✓ journey-02-first-configure: guidance error, hot reload, same-session reply — all verified');
  } finally {
    for (const ws of wsClients) ws.close();
    if (proc) await stopServe(proc, 'kill').catch(() => {});
    await closeProvider(provider);
  }
}

function closeProvider(provider: FakeProviderServer): Promise<void> {
  return provider.close().catch(() => undefined);
}

main().catch((error) => {
  console.error('✗ journey-02-first-configure failed:', error);
  process.exit(1);
});
