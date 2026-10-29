/**
 * `WsRpcClient` integration tests — unified JSON-RPC 2.0 endpoint (ADR-001).
 *
 * Each case boots a REAL `mirri serve` process (bypass auth) with a fake
 * OpenAI-compatible provider from `@mirri-ai/e2e-harness`, then drives the
 * `/api/v1/rpc` WebSocket: handshake, RPC round-trip, subscribe, prompt →
 * streamed engine events.
 */
import { afterEach, describe, expect, it } from 'vitest';
import { createFakeProviderServer, type FakeProviderServer } from '@mirri-ai/e2e-harness';

import { fakeProviderConfigToml, startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

const procs: ProcHandle[] = [];
const providers: FakeProviderServer[] = [];

afterEach(async () => {
  for (const proc of procs.splice(0)) {
    await stopServe(proc, 'kill').catch(() => {});
  }
  for (const provider of providers.splice(0)) {
    await provider.close().catch(() => {});
  }
});

/** Boot serve + fake provider, and arm one scripted model reply. */
async function bootWithFakeProvider(): Promise<ProcHandle> {
  const provider = await createFakeProviderServer();
  providers.push(provider);
  const proc = await startServe({
    configToml: fakeProviderConfigToml(provider.baseUrl),
  });
  procs.push(proc);
  return proc;
}

describe('WsRpcClient (unified json-rpc endpoint)', () => {
  it('should complete the handshake and round-trip a trivial rpc call', async () => {
    // 假如：一个已就绪的 serve 进程（bypass 鉴权）
    // 当：客户端连接统一 RPC 端点并发送 ping 请求
    // 那么：握手完成（server_hello → client_hello → accepted），ping 得到响应
    const proc = await bootWithFakeProvider();
    const client = new WsRpcClient(proc.baseUrl);
    try {
      await client.connect();
      // `ping` is answered with a pong NOTIFICATION, not a response — a
      // fire-mode `message.send` to the fake provider is the trivial
      // request/response round-trip instead.
      providers[0]?.nextText('OK');
      const sessionRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
      });
      const session = (await sessionRes.json()) as { data?: { id?: string } };
      const sid = session.data?.id;
      expect(typeof sid).toBe('string');
      const sent = await client.call<{ ok?: boolean; turn_id?: string }>('message.send', {
        sessionId: sid,
        input: [{ type: 'text', text: 'Say OK' }],
        mode: 'fire',
      });
      expect(sent.ok).toBe(true);
      expect(typeof sent.turn_id).toBe('string');
    } finally {
      client.close();
    }
  }, 60_000);

  it('should subscribe and receive engine events over the unified rpc endpoint', async () => {
    // 假如：已配置好 fake provider（脚本化回复 "OK"）并建立了 RPC 订阅
    // 当：用户通过 REST 提交一条消息
    // 那么：依次收到 event.message.received、event.assistant.delta（流式增量）
    //       与 event.turn.ended（回合完成）通知
    const proc = await bootWithFakeProvider();
    providers[0]?.nextText('OK');
    const client = new WsRpcClient(proc.baseUrl);
    try {
      await client.connect();

      const sessionRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
      });
      expect(sessionRes.status).toBe(200);
      const session = (await sessionRes.json()) as { data?: { id?: string } };
      const sid = session.data?.id;
      expect(typeof sid).toBe('string');

      await client.subscribe(sid!);

      const seenEvents: string[] = [];
      client.onNotification((n) => {
        if (n.params['sessionId'] === sid) seenEvents.push(n.method);
      });

      const submitRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          input: [{ type: 'text', text: 'Say OK' }],
          mode: 'fire',
        }),
      });
      expect(submitRes.status).toBe(200);
      const submit = (await submitRes.json()) as { data?: { turn_id?: string } };
      expect(typeof submit.data?.turn_id).toBe('string');

      const endedParams = await client.waitForEvent(sid!, 'event.turn.ended', 60_000);

      expect(endedParams['sessionId']).toBe(sid);
      expect(typeof endedParams['seq']).toBe('number');
      expect(seenEvents).toContain('event.message.received');
      expect(seenEvents).toContain('event.assistant.delta');
      expect(seenEvents).toContain('event.turn.ended');
    } finally {
      client.close();
    }
  }, 120_000);

  it('should buffer events received before a waiter is registered', async () => {
    // 假如：一次回合在注册等待者之前就已结束（事件先到）
    // 当：随后才调用 waitForEvent('event.turn.ended')
    // 那么：事件从队列中命中，不因晚注册而丢失
    const proc = await bootWithFakeProvider();
    providers[0]?.nextText('OK');
    const client = new WsRpcClient(proc.baseUrl);
    try {
      await client.connect();

      const sessionRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ metadata: { cwd: proc.workDir } }),
      });
      const session = (await sessionRes.json()) as { data?: { id?: string } };
      const sid = session.data?.id;
      expect(typeof sid).toBe('string');

      await client.subscribe(sid!);

      const submitRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          input: [{ type: 'text', text: 'Say OK' }],
          mode: 'fire',
        }),
      });
      expect(submitRes.status).toBe(200);

      // Poll REST until the session is quiescent — the turn ended while no
      // waiter was registered.
      const deadline = Date.now() + 60_000;
      for (;;) {
        const res = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}`);
        const body = (await res.json()) as { data?: { busy?: boolean } };
        if (body.data?.busy === false) break;
        if (Date.now() > deadline) throw new Error('session never reached quiescence');
        await new Promise((r) => setTimeout(r, 250));
      }

      // The queued event.turn.ended must still resolve the late waiter.
      const params = await client.waitForEvent(sid!, 'event.turn.ended', 5_000);
      expect(params['sessionId']).toBe(sid);
    } finally {
      client.close();
    }
  }, 120_000);
});
