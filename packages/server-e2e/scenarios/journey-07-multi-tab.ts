#!/usr/bin/env node
/**
 * Journey ⑦ — multi-tab simultaneous observation (spec R12).
 *
 * 假如：用户在两个浏览器标签页里打开同一个会话
 * 当：两个 WsRpcClient 都订阅同一 session → 通过 REST（或任一连接）发
 *     一条消息
 * 那么：两个连接都收到相同的事件序列（相同 method 序列逐一对齐；到
 *       达时机可以不同）
 *
 * Probe findings baked into the assertions (2026-10-28 code probes):
 *   - The event channel fans each live record out to EVERY subscriber
 *     once (`ws_rpc.rs` broadcast — `should_fan_events_out_to_each_
 *     subscriber_once`), so both subscriptions observe the same stream.
 *   - Session-scoped events carry `params.sessionId`; both connections
 *     filter on the same id.
 */
import assert from 'node:assert/strict';

import { createFakeProviderServer } from '@mirri-ai/e2e-harness';

import { fakeProviderConfigToml, startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

interface ObservedNotification {
  method: string;
  params: Record<string, unknown>;
}

async function main(): Promise<void> {
  console.log('▶ journey-07: booting serve + fake provider for a two-tab session');
  const provider = await createFakeProviderServer();
  let proc: ProcHandle | undefined;
  const wsClients: WsRpcClient[] = [];
  try {
    proc = await startServe({ configToml: fakeProviderConfigToml(provider.baseUrl) });

    // 建会话（title on create — journey convention）
    const createRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ title: 'journey 07 multi tab', metadata: { cwd: proc.workDir } }),
    });
    const created = (await createRes.json()) as { data?: { id?: string } };
    const sid = created.data?.id;
    assert.ok(typeof sid === 'string', 'session created');

    // ── 当：两个"标签页"（WsRpcClient）都订阅同一会话 ──────────────────
    const wsTabA = new WsRpcClient(proc.baseUrl);
    const wsTabB = new WsRpcClient(proc.baseUrl);
    wsClients.push(wsTabA, wsTabB);
    await wsTabA.connect();
    await wsTabB.connect();
    await wsTabA.subscribe(sid);
    await wsTabB.subscribe(sid);
    console.log('▶ journey-07: two tabs subscribed to the same session');

    const eventsA: ObservedNotification[] = [];
    const eventsB: ObservedNotification[] = [];
    wsTabA.onNotification((n) => {
      if (n.params['sessionId'] === sid) eventsA.push({ method: n.method, params: n.params });
    });
    wsTabB.onNotification((n) => {
      if (n.params['sessionId'] === sid) eventsB.push({ method: n.method, params: n.params });
    });

    // ── 当：通过 REST 发消息（替身一段带标记的回复）─────────────────────
    provider.nextText('MULTI_TAB_REPLY_MARKER both tabs see this reply.');
    const sendRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ input: [{ type: 'text', text: 'Hello from tab A.' }], mode: 'fire' }),
    });
    const sendBody = (await sendRes.json()) as { code?: number };
    assert.equal(sendBody.code, 0, 'message send succeeds');

    // 等两路都收到 turn 结束
    const [endedA, endedB] = await Promise.all([
      wsTabA.waitForEvent(sid, 'event.turn.ended', 60_000),
      wsTabB.waitForEvent(sid, 'event.turn.ended', 60_000),
    ]);
    assert.equal(endedA['reason'], 'completed', 'tab A observed the turn end');
    assert.equal(endedB['reason'], 'completed', 'tab B observed the turn end');

    // ── 那么：两连接收到相同的事件序列（method 序列逐一对齐）──────────
    const methodsA = eventsA.map((n) => n.method);
    const methodsB = eventsB.map((n) => n.method);
    assert.ok(methodsA.length > 0, 'tab A received session events');
    assert.ok(methodsB.length > 0, 'tab B received session events');
    assert.deepEqual(
      methodsB, methodsA,
      `both tabs received the same event method sequence (A: ${methodsA.join(',')})`,
    );
    console.log(`▶ journey-07: both tabs saw the same ${methodsA.length}-event sequence`);

    // payload 对齐抽查：turn.ended 与 assistant 回复文本在两路一致
    const endedAParams = eventsA.find((n) => n.method === 'event.turn.ended')?.params;
    const endedBParams = eventsB.find((n) => n.method === 'event.turn.ended')?.params;
    assert.equal(endedAParams?.['reason'], endedBParams?.['reason'], 'the turn outcome matches on both tabs');
    for (const [label, events] of [['A', eventsA], ['B', eventsB]] as const) {
      const deltas = events
        .filter((n) => n.method === 'event.assistant.delta')
        .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
        .join('');
      assert.ok(
        deltas.includes('MULTI_TAB_REPLY_MARKER'),
        `tab ${label} rendered the same streamed reply text`,
      );
    }
    console.log('▶ journey-07: both tabs rendered the same reply text');

    console.log('✓ journey-07-multi-tab: both connections observed identical event sequences');
  } finally {
    for (const ws of wsClients) ws.close();
    if (proc) await stopServe(proc, 'kill').catch(() => {});
    await provider.close().catch(() => {});
  }
}

main().catch((error) => {
  console.error('✗ journey-07-multi-tab failed:', error);
  process.exit(1);
});
