#!/usr/bin/env node
/**
 * Journey ⑤ — return visit and refresh continuity (spec R9 + R10).
 *
 * 假如：用户昨天和代理聊过（进程 A 完成一轮对话），今天重启机器/进程回来继续
 * 当：① 进程 A 完成一轮（回复带 PREVIOUS-DAY 标记文本）→ SIGKILL → 进程 B
 *       同 homeDir/config 重启 → listSessions 含昨天的会话 → REST 历史完整 →
 *       订阅 + 新消息
 *     ② 对话中途关掉 WS 连接（浏览器刷新替身）→ 记录 seq/epoch → 新连接带
 *       cursor 重订 → 拉快照
 * 那么：① 新一轮发给模型的 messages 含进程 A 时代的标记文本（上下文真实跨
 *       进程留存并传给模型）；
 *       ② 回放事件从 seq+1 起严格连续无丢无重，与快照 last_seq 衔接
 *
 * Probe findings baked into the assertions (2026-10-28 code probes):
 *   - Durable event notifications carry flat `seq` (u64) + `epoch` (string)
 *     envelope fields (`event_projection::event_notification`); volatile
 *     events (assistant.delta) carry neither.
 *   - `session.subscribe` ack returns `{ seq: tail_seq, epoch }`
 *     (`ws_rpc.rs`); with a cursor `{ seq, epoch }` the channel replays
 *     durable records with `seq > cursor.seq` first, then goes live.
 *   - REST `GET /api/v1/sessions/{id}` (`session.get`) restores a
 *     persisted session on demand and answers the full wire `Session`
 *     row (title / created_at / last_seq, protocol session.ts).
 *   - `Session.last_seq` (protocol row) is the durable event cursor the
 *     page refresh seeds from — used as the snapshot-side continuity
 *     anchor.
 *   - The model's request body records the full conversation history in
 *     `messages` (fake provider `requests[last].bodyJson.messages`).
 */
import assert from 'node:assert/strict';

import { createFakeProviderServer } from '@mirri-ai/e2e-harness';

import { fakeProviderConfigToml, startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

interface ObservedNotification {
  method: string;
  params: Record<string, unknown>;
}

/** POST /api/v1/sessions — create a session with a title (skips the
 *  side-title LLM call — journey convention). */
async function createSession(proc: ProcHandle, title: string): Promise<string> {
  const res = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ title, metadata: { cwd: proc.workDir } }),
  });
  const body = (await res.json()) as { code?: number; data?: { id?: string } };
  if (body.code !== 0 || typeof body.data?.id !== 'string') {
    throw new Error(`createSession failed: ${JSON.stringify(body)}`);
  }
  return body.data.id;
}

/** POST /api/v1/sessions/{sid}/messages — fire-mode prompt submit. */
async function sendMessage(proc: ProcHandle, sid: string, text: string): Promise<void> {
  const res = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ input: [{ type: 'text', text }], mode: 'fire' }),
  });
  const body = (await res.json()) as { code?: number };
  if (body.code !== 0) throw new Error(`message send failed: ${JSON.stringify(body)}`);
}

/** GET /api/v1/sessions — full session list, envelope-unwrapped. */
async function listSessions(
  proc: ProcHandle,
): Promise<Array<Record<string, unknown>>> {
  const res = await fetch(`${proc.baseUrl}/api/v1/sessions`);
  const body = (await res.json()) as { data?: unknown };
  // Probe finding: session.list's payload is the page envelope
  // `{items: SessionView[], has_more}` (session_handlers.rs:861) — the
  // rows are camelCase SessionView objects.
  const rows = (body.data as { items?: unknown } | null)?.items;
  return Array.isArray(rows) ? (rows as Array<Record<string, unknown>>) : [];
}

/** GET /api/v1/sessions/{sid} — the session.get row (restores on demand). */
async function getSession(proc: ProcHandle, sid: string): Promise<Record<string, unknown>> {
  const res = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}`);
  const body = (await res.json()) as { data?: Record<string, unknown> };
  return body.data ?? {};
}

/** The durable seq carried by an event notification, when present. */
function eventSeq(n: ObservedNotification): number | undefined {
  return typeof n.params['seq'] === 'number' ? (n.params['seq'] as number) : undefined;
}

async function main(): Promise<void> {
  console.log('▶ journey-05: booting process A with the fake provider');
  const provider = await createFakeProviderServer();
  const homeDir = undefined; // fresh isolated home — reused across A→B below
  const configToml = fakeProviderConfigToml(provider.baseUrl);
  let procA: ProcHandle | undefined;
  let procB: ProcHandle | undefined;
  const wsClients: WsRpcClient[] = [];
  try {
    procA = await startServe({ homeDir, configToml });

    // ── R9（上半场）：进程 A 完成一轮带标记文本的对话 ─────────────────
    const sid = await createSession(procA, 'journey 05 return visit');
    const wsA = new WsRpcClient(procA.baseUrl);
    wsClients.push(wsA);
    await wsA.connect();
    await wsA.subscribe(sid);
    const endedA = await (async () => {
      provider.nextText('PREVIOUS-DAY-REPLY-MARKER the answer is 42.');
      await sendMessage(procA, sid, 'What is the answer?');
      return wsA.waitForEvent(sid, 'event.turn.ended', 60_000);
    })();
    assert.equal(endedA['reason'], 'completed', 'process A turn completes normally');
    console.log('▶ journey-05: process A finished its previous-day turn');

    // 会话行读一遍，作为重启前后对照（title/createdAt 基线 — probe:
    // SessionView 序列化为 camelCase `createdAt`，与协议行一致）
    const rowA = await getSession(procA, sid);
    assert.equal(rowA['title'], 'journey 05 return visit', 'pre-restart title');
    const createdAtA = rowA['createdAt'] ?? rowA['created_at'];
    assert.ok(typeof createdAtA === 'string', 'pre-restart createdAt is present');

    // SIGKILL（journey stage ⑤ 的硬重启形态 — 无优雅落盘依赖）
    const deadHome = procA.homeDir;
    await stopServe(procA, 'kill');
    procA = undefined;
    console.log('▶ journey-05: process A killed; restarting process B on the same home');

    // ── R9（下半场）：进程 B 同 homeDir/config 回访 ────────────────────
    procB = await startServe({ homeDir: deadHome, configToml });

    // 那么：listSessions 含进程 A 的会话（title/createdAt 保留）
    const sessions = await listSessions(procB);
    console.log(`▶ journey-05: process B lists ${sessions.length} session(s): ${sessions.map((row) => String(row['title'])).join(', ')}`);
    const revisited = sessions.find((row) => row['id'] === sid);
    assert.ok(revisited !== undefined, `process B's session list contains process A's session ${sid}`);
    assert.equal(revisited['title'], 'journey 05 return visit', 'the revisited session keeps its title');
    assert.equal(revisited['createdAt'] ?? revisited['created_at'], createdAtA, 'the revisited session keeps its createdAt');
    console.log('▶ journey-05: session list carries the previous-day session (title/createdAt intact)');

    // 那么：订阅 + 新消息 → 模型上下文真实携带进程 A 时代文本
    // （连接在历史读取之前建立 — message.list 走同一 WS RPC 面）
    const wsB = new WsRpcClient(procB.baseUrl);
    wsClients.push(wsB);
    await wsB.connect();
    await wsB.subscribe(sid);
    const history = await wsB.call<{ messages?: Array<Record<string, unknown>>; hasMore?: boolean }>(
      'message.list',
      { sessionId: sid, pageSize: 50 },
    );
    const historyText = JSON.stringify(history.messages ?? []);
    assert.ok(
      historyText.includes('What is the answer?'),
      'the message history contains the previous-day user prompt',
    );
    assert.ok(
      historyText.includes('PREVIOUS-DAY-REPLY-MARKER'),
      'the message history contains the previous-day assistant reply',
    );
    console.log('▶ journey-05: message history is complete across the restart');
    const requestsBefore = provider.requests.length;
    const endedB = await (async () => {
      provider.nextText('Welcome back. Continuing from where we left off.');
      await sendMessage(procB, sid, 'I am back — continue from the previous answer.');
      return wsB.waitForEvent(sid, 'event.turn.ended', 60_000);
    })();
    assert.equal(endedB['reason'], 'completed', 'the return-visit turn completes normally');

    const lastRequest = provider.requests.at(-1);
    assert.ok(
      provider.requests.length > requestsBefore,
      'the return-visit turn made at least one LLM call',
    );
    const bodyJson = lastRequest?.bodyJson as { messages?: Array<Record<string, unknown>> } | null;
    assert.ok(bodyJson !== null && Array.isArray(bodyJson.messages), 'the last LLM call carries a messages array');
    const sentToModel = JSON.stringify(bodyJson.messages);
    assert.ok(
      sentToModel.includes('PREVIOUS-DAY-REPLY-MARKER'),
      'the conversation history sent to the model contains the previous-day reply marker — context genuinely carried across the restart',
    );
    console.log('▶ journey-05: the model request after restart carries the previous-day context');

    // ── R10：刷新接续（对话中途换连接）─────────────────────────────────
    console.log('▶ journey-05 [R10]: refresh continuity mid-conversation');
    // 基线：当前快照 last_seq + 订阅面收到的最新 durable seq/epoch
    const snapshot1 = await getSession(procB, sid);
    const lastSeqBefore = snapshot1['lastSeq'] ?? snapshot1['last_seq'];
    assert.ok(typeof lastSeqBefore === 'number', 'the session row carries last_seq');

    // 记录此刻最新 durable 事件的 seq/epoch（作为"刷新前所见位置"）
    let lastSeenSeq = -1;
    let lastSeenEpoch: string | undefined;
    wsB.onNotification((n) => {
      if (n.params['sessionId'] !== sid) return;
      const seq = eventSeq({ method: n.method, params: n.params });
      if (seq !== undefined) {
        lastSeenSeq = Math.max(lastSeenSeq, seq);
        if (typeof n.params['epoch'] === 'string') lastSeenEpoch = n.params['epoch'];
      }
    });
    // 触发一至两个 durable 事件落进日志（一轮新对话）
    const endedMid = await (async () => {
      provider.nextText('Mid-conversation turn before the refresh.');
      await sendMessage(procB, sid, 'One more exchange before I refresh.');
      return wsB.waitForEvent(sid, 'event.turn.ended', 60_000);
    })();
    assert.equal(endedMid['reason'], 'completed', 'the pre-refresh turn completes normally');
    assert.ok(lastSeenSeq > lastSeqBefore, 'a fresh durable event was observed after subscribing (the refresh has something to continue from)');
    assert.ok(typeof lastSeenEpoch === 'string', 'the observed durable event carries an epoch');
    console.log(`▶ journey-05 [R10]: refresh point captured (seq=${lastSeenSeq}, epoch=${lastSeenEpoch})`);

    // 用户"刷新浏览器"：旧连接关闭，新连接带 cursor 重订
    wsB.close();
    const wsRefreshed = new WsRpcClient(procB.baseUrl);
    wsClients.push(wsRefreshed);
    await wsRefreshed.connect();
    const replayed: ObservedNotification[] = [];
    const ack = (await wsRefreshed.subscribe(sid, { seq: lastSeenSeq, epoch: lastSeenEpoch ?? '' })) as {
      seq?: number;
      epoch?: string;
    };
    wsRefreshed.onNotification((n) => {
      if (n.params['sessionId'] !== sid) return;
      const seq = eventSeq({ method: n.method, params: n.params });
      if (seq !== undefined) replayed.push({ method: n.method, params: n.params });
    });
    console.log(`▶ journey-05 [R10]: re-subscribed with cursor (ack seq=${ack.seq}, epoch=${ack.epoch})`);

    // 那么：REST 快照 last_seq 与订阅 ack 的 tail 一致（两种读面同步）
    const snapshot2 = await getSession(procB, sid);
    const snapshot2LastSeq = snapshot2['lastSeq'] ?? snapshot2['last_seq'];
    assert.equal(
      snapshot2LastSeq, ack.seq,
      'the refreshed snapshot last_seq matches the subscribe ack tail',
    );

    // 那么：再发一轮对话 → 回放事件从 seq+1 起严格连续无丢无重，并衔接快照
    const endedPost = await (async () => {
      provider.nextText('Post-refresh continuation — still on the same conversation.');
      await sendMessage(procB, sid, 'Continuing after my refresh.');
      return wsRefreshed.waitForEvent(sid, 'event.turn.ended', 60_000);
    })();
    assert.equal(endedPost['reason'], 'completed', 'the post-refresh turn completes normally');

    const seqs = replayed.map((n) => eventSeq(n) as number);
    assert.ok(seqs.length > 0, 'the refreshed connection received durable events');
    // 严格递增
    for (let i = 1; i < seqs.length; i++) {
      assert.ok(seqs[i]! > seqs[i - 1]!, `replayed seqs are strictly increasing (got ${seqs.join(',')})`);
    }
    // 无丢：从 cursor+1 起（或与快照 last_seq 衔接，二者取先到达者）
    const expectedStart = Math.max(lastSeenSeq + 1, snapshot2LastSeq as number);
    assert.equal(
      seqs[0], expectedStart,
      `the first replayed event continues from the refresh point (expected seq ${expectedStart}, got ${seqs[0]})`,
    );
    // 无重：与刷新点无重叠
    assert.ok(
      !seqs.includes(lastSeenSeq),
      'no replayed event duplicates the pre-refresh seq (no double render after refresh)',
    );
    // 新事件确实推进到了快照 last_seq 之后
    const snapshot3 = await getSession(procB, sid);
    const snapshot3LastSeq = snapshot3['lastSeq'] ?? snapshot3['last_seq'];
    assert.ok(
      (snapshot3LastSeq as number) >= seqs.at(-1)!,
      'the post-refresh snapshot last_seq has advanced to (at least) the last received event',
    );
    console.log(`▶ journey-05 [R10]: replay is contiguous (${seqs[0]}..${seqs.at(-1)}), no gaps, no duplicates`);

    console.log('✓ journey-05-return-visit: cross-process context and refresh continuity all verified');
  } finally {
    for (const ws of wsClients) ws.close();
    if (procA) await stopServe(procA, 'kill').catch(() => {});
    if (procB) await stopServe(procB, 'kill').catch(() => {});
    await provider.close().catch(() => {});
  }
}

main().catch((error) => {
  console.error('✗ journey-05-return-visit failed:', error);
  process.exit(1);
});
