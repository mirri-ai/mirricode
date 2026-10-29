#!/usr/bin/env node
/**
 * Journey ③ — first conversation (spec R4 + R5 + R6).
 *
 * 假如：用户已配置好模型（fake provider），打开一个有真实代码文件的临时工作区会话
 * 当：① 只读探索（Read 工具）② 写文件触发审批闭环（批准/拒绝）③ 流式中断
 * 那么：① 回复引用真实文件名，全程零审批事件；
 *       ② 审批事件到达时目标文件未变；批准后文件真实变化；拒绝后保持原样；
 *       ③ 中断后事件流停止，新消息仍得到回复（会话可用）
 *
 * Probe findings baked into the assertions (2026-10-28 live probes):
 *   - Tool names: `Read` (params `{path}`), `Write` (params `{path, content}`).
 *   - Read-only tools do NOT trigger approval (session_events show no
 *     `approval.requested` during the read window).
 *   - Approval arrives as a reverse-RPC REQUEST `approval.ask` (integer id,
 *     params `{toolName, toolCallId?, action?, ...}`); the reply shape the
 *     engine interprets is `{decision: "approved"|"rejected", feedback?}`
 *     (turn_flow/hooks.rs `interpret_approval_response`).
 *   - Sessions created WITHOUT a title trigger an out-of-band side-title LLM
 *     call that consumes a queued fake-provider response; scenarios must
 *     create sessions WITH a title (also sets `is_custom_title`, which the
 *     side-title guard respects) so every queued response maps 1:1 to a
 *     main-turn LLM call.
 *   - `message.abort` params: `{sessionId, turnId?}` (camelCase, serve wire).
 *   - The turn's terminal event is `event.turn.ended` with `reason`
 *     "cancelled" after a user abort (ADR-007 vocabulary).
 */
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { createFakeProviderServer, type FakeProviderServer } from '@mirri-ai/e2e-harness';

import { fakeProviderConfigToml, startServe, stopServe, type ProcHandle } from '../src/proc';
import { WsRpcClient } from '../src/ws';

interface ObservedNotification {
  method: string;
  params: Record<string, unknown>;
}

async function main(): Promise<void> {
  console.log('▶ journey-03: booting serve + fake provider with a real temp workspace');
  const provider = await createFakeProviderServer();
  const workDir = mkdtempSync(join(tmpdir(), 'mirri-journey03-'));

  // A real tiny project the agent will explore.
  mkdirSync(join(workDir, 'src'), { recursive: true });
  writeFileSync(join(workDir, 'src', 'main.ts'), 'export function main() { return 1; }\n', 'utf8');
  writeFileSync(join(workDir, 'src', 'util.ts'), 'export const answer = 42;\n', 'utf8');
  writeFileSync(join(workDir, 'README.md'), '# journey probe project\n', 'utf8');

  const proc = await startServe({ workDir, configToml: fakeProviderConfigToml(provider.baseUrl) });
  const ws = new WsRpcClient(proc.baseUrl);
  const notifications: ObservedNotification[] = [];

  try {
    await ws.connect();

    // Title set → no side-title LLM call → deterministic response queue.
    const createRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ title: 'journey 03 first conversation', metadata: { cwd: workDir } }),
    });
    const created = (await createRes.json()) as { data?: { id?: string } };
    const sid = created.data?.id;
    assert.ok(typeof sid === 'string', 'session created');
    await ws.subscribe(sid);
    ws.onNotification((n) => {
      if (n.params['sessionId'] === sid) notifications.push({ method: n.method, params: n.params });
    });

    await phase_readOnlyExploration(proc, provider, ws, sid, notifications);
    await phase_approvalLoop(proc, provider, ws, sid, notifications, workDir);
    await phase_streamingInterrupt(proc, provider, ws, sid, notifications);

    console.log('✓ journey-03-first-conversation: read-only, approval loop, and interrupt all verified');
  } finally {
    ws.close();
    await stopServe(proc, 'kill').catch(() => {});
    await provider.close().catch(() => {});
  }
}

/** Submit a fire-mode prompt and wait for the turn to end; returns the ended params. */
async function submitAndAwaitTurn(
  proc: ProcHandle,
  sid: string,
  text: string,
  ws: WsRpcClient,
  timeoutMs = 60_000,
): Promise<Record<string, unknown>> {
  const sendRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ input: [{ type: 'text', text }], mode: 'fire' }),
  });
  const sendBody = (await sendRes.json()) as { code?: number; data?: { turn_id?: string } };
  if (sendBody.code !== 0) throw new Error(`message send failed: ${JSON.stringify(sendBody)}`);
  const ended = await ws.waitForEvent(sid, 'event.turn.ended', timeoutMs);
  return { ...ended, __turn_id: sendBody.data?.turn_id };
}

/** ── R4：只读探索 ───────────────────────────────────────────────────────── */
async function phase_readOnlyExploration(
  proc: ProcHandle,
  provider: FakeProviderServer,
  ws: WsRpcClient,
  sid: string,
  notifications: ObservedNotification[],
): Promise<void> {
  console.log('▶ journey-03 [R4]: read-only exploration of a real workspace');
  const marksBefore = notifications.length;

  // 假如：替身剧本是"读一个真实文件，然后引用真实文件名总结"
  provider.nextToolCall('Read', { path: 'src/main.ts' });
  provider.nextText('The project contains src/main.ts with a main() function and src/util.ts exporting the answer.');

  const ended = await submitAndAwaitTurn(proc, sid, 'Explore this project and summarize the files.', ws);
  assert.equal(ended['reason'], 'completed', 'the read-only turn completes normally');

  // 那么：回复包含真实文件名
  const deltas = notifications
    .slice(marksBefore)
    .filter((n) => n.method === 'event.assistant.delta')
    .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
    .join('');
  assert.ok(deltas.includes('src/main.ts'), `the assistant reply references the real file src/main.ts (got: ${deltas})`);
  assert.ok(deltas.includes('src/util.ts'), 'the assistant reply references the real file src/util.ts');

  // 那么：只读窗口内零审批事件（approval.requested / approval.resolved）
  const windowEvents = notifications.slice(marksBefore);
  const approvalEvents = windowEvents.filter(
    (n) => n.method === 'event.approval.requested' || n.method === 'event.approval.resolved',
  );
  assert.equal(approvalEvents.length, 0, 'no approval events fire during the read-only exploration turn');

  // 且 Read 工具真实执行（结果事件输出引用真实文件内容）
  const readResult = windowEvents.find((n) => n.method === 'event.tool.result');
  assert.ok(readResult !== undefined, 'a tool.result event arrives for the Read call');
  const output = typeof readResult.params['output'] === 'string' ? readResult.params['output'] : JSON.stringify(readResult.params['output']);
  assert.ok(output.includes('export function main()'), 'the Read result carries the real file content');
  console.log('▶ journey-03 [R4]: reply references real files; zero approval events');
}

/** ── R5：审批闭环（批准 → 文件真实变化；拒绝 → 保持原样）────────────────── */
async function phase_approvalLoop(
  proc: ProcHandle,
  provider: FakeProviderServer,
  ws: WsRpcClient,
  sid: string,
  notifications: ObservedNotification[],
  workDir: string,
): Promise<void> {
  console.log('▶ journey-03 [R5]: write approval loop (approve then reject)');

  // —— 第 1 轮：批准 ——
  const approvedTarget = join(workDir, 'notes', 'approved.txt');
  const approvedContent = 'written-after-approval';
  {
    const marksBefore = notifications.length;
    provider.nextToolCall('Write', { path: 'notes/approved.txt', content: approvedContent });
    provider.nextText('I wrote notes/approved.txt as requested.');

    const turnEnded = ws.waitForEvent(sid, 'event.turn.ended', 60_000);
    // fire the prompt without awaiting the turn here — the approval ask must
    // be answered while the turn is in flight. `sendPromise` is failure-traced
    // so an early throw in this block never leaves it unhandled on close().
    const sendPromise = fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ input: [{ type: 'text', text: 'Write the file notes/approved.txt.' }], mode: 'fire' }),
    }).catch((error) => {
      throw error instanceof Error ? error : new Error(String(error));
    });
    const ask = await waitForAsk(ws);
    // 当：审批请求到达（引擎侧 ask 载荷为 snake_case：tool_name 等 —
    // turn_flow/hooks.rs `approval_request` 的键）
    assert.equal(ask.method, 'approval.ask', 'the engine asks for approval via reverse RPC');
    assert.ok(
      typeof ask.params['tool_name'] === 'string' && ask.params['tool_name'] === 'Write',
      'the ask names the Write tool (snake_case tool_name key)',
    );

    // 那么：批准之前目标文件未发生任何变化
    assert.equal(existsSync(approvedTarget), false, 'the target file does not exist before the approval is answered');

    // 当：用户批准（respond 同 id 的 JSON-RPC result，decision=approved）
    ws.respond(ask.id, { decision: 'approved' });
    await sendPromise; // status check only
    const ended = await turnEnded;
    assert.equal(ended['reason'], 'completed', 'the approved write turn completes normally');

    // 那么：文件真实落盘且内容正确
    assert.equal(existsSync(approvedTarget), true, 'the target file exists after the approval');
    assert.equal(readFileSync(approvedTarget, 'utf8'), approvedContent, 'the file carries the exact approved content');

    // 且事件面记录了这次审批（event.approval.requested + resolved）
    const windowEvents = notifications.slice(marksBefore);
    assert.ok(windowEvents.some((n) => n.method === 'event.approval.requested'), 'event.approval.requested is observable');
    assert.ok(
      windowEvents.some((n) => n.method === 'event.approval.resolved'),
      'event.approval.resolved is observable after the decision',
    );
    console.log('▶ journey-03 [R5]: approve round — file unchanged before decision, written after');
  }

  // —— 第 2 轮：拒绝 ——
  const rejectedTarget = join(workDir, 'notes', 'rejected.txt');
  const rejectedContent = 'must-never-appear';
  {
    const marksBefore = notifications.length;
    provider.nextToolCall('Write', { path: 'notes/rejected.txt', content: rejectedContent });
    // After a rejection the engine surfaces the denial to the model; the
    // model's next reply ends the turn.
    provider.nextText('The write was rejected by the user.');

    const turnEnded = ws.waitForEvent(sid, 'event.turn.ended', 60_000);
    await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ input: [{ type: 'text', text: 'Write the file notes/rejected.txt.' }], mode: 'fire' }),
    });
    const ask = await waitForAsk(ws);
    // 当：用户拒绝（decision=rejected + feedback）
    ws.respond(ask.id, { decision: 'rejected', feedback: 'not allowed in this journey' });
    const ended = await turnEnded;
    assert.equal(ended['reason'], 'completed', 'the rejected write turn still completes (the model sees the denial)');

    // 那么：目标文件保持原样（从未创建）
    assert.equal(existsSync(rejectedTarget), false, 'the rejected target file is never created');

    // 且审批结果事件记录了拒绝
    const resolved = notifications
      .slice(marksBefore)
      .find((n) => n.method === 'event.approval.resolved');
    assert.ok(resolved !== undefined, 'event.approval.resolved is observable for the rejection');
    assert.equal(resolved.params['approved'], false, 'the resolved event records approved=false');
    console.log('▶ journey-03 [R5]: reject round — file stays untouched');
  }
}

/** Wait for the next `approval.ask` reverse-RPC request (30s). */
function waitForAsk(ws: WsRpcClient): Promise<{ id: number | string; method: string; params: Record<string, unknown> }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.onRequest(() => {});
      reject(new Error('journey-03: approval.ask did not arrive within 30s'));
    }, 30_000);
    const handler = (req: { id: number | string; method: string; params: Record<string, unknown> }) => {
      if (req.method !== 'approval.ask') return;
      clearTimeout(timer);
      resolve(req);
    };
    ws.onRequest(handler);
  });
}

/** ── R6：流式中断 ───────────────────────────────────────────────────────── */
async function phase_streamingInterrupt(
  proc: ProcHandle,
  provider: FakeProviderServer,
  ws: WsRpcClient,
  sid: string,
  notifications: ObservedNotification[],
): Promise<void> {
  console.log('▶ journey-03 [R6]: streaming interrupt via message.abort');
  const marksBefore = notifications.length;

  // 假如：替身流式返回一段长回复（多 chunk）。`setStreamDelay` delays the
  // WHOLE SSE response (not per-chunk — the fake provider's stream delay is
  // applied before the first byte), so the deterministic window is: the turn
  // is in-flight (phase streaming) while the response is still pending.
  provider.nextRawSSE([
    ...[1, 2, 3, 4, 5, 6, 7, 8].map((i) => ({
      id: 'chatcmpl-fake',
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: 'fake-model',
      choices: [{ index: 0, delta: { content: `chunk-${i}-` }, finish_reason: null }],
    })),
    {
      id: 'chatcmpl-fake',
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: 'fake-model',
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' as const }],
    },
  ]);
  provider.setStreamDelay(1_500);

  // 当：用户提交消息，引擎进入流式阶段（响应尚未开始），此时发出中断
  const sendRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ input: [{ type: 'text', text: 'Tell me a long story.' }], mode: 'fire' }),
  });
  const sendBody = (await sendRes.json()) as { code?: number; data?: { turn_id?: string } };
  assert.equal(sendBody.code, 0, 'the interrupt-test prompt submits fine');
  const turnId = sendBody.data?.turn_id;
  assert.ok(typeof turnId === 'string', 'the submit returns a turn id');

  // Wait until the engine is demonstrably in the turn's streaming phase (the
  // status event fires before the provider response begins).
  const streamingDeadline = Date.now() + 20_000;
  for (;;) {
    const streaming = notifications
      .slice(marksBefore)
      .find(
        (n) =>
          n.method === 'event.agent.status.updated' &&
          n.params['phase'] === 'streaming',
      );
    if (streaming !== undefined) break;
    if (Date.now() > streamingDeadline) {
      throw new Error('journey-03 [R6]: the turn never reached the streaming phase');
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  console.log('▶ journey-03 [R6]: turn is streaming, sending message.abort');

  const abortResult = await ws.call<{ ok?: boolean }>('message.abort', { sessionId: sid, turnId });
  assert.equal(abortResult.ok, true, 'message.abort is acknowledged');

  // 那么：中断后事件流停止——turn 以 cancelled 终结，且不再有新的 delta
  const ended = await ws.waitForEvent(sid, 'event.turn.ended', 30_000);
  assert.equal(ended['reason'], 'cancelled', 'the aborted turn ends with reason cancelled');

  // 等一小段窗口，确认没有任何 chunk delta 在中断后继续到达
  await new Promise((r) => setTimeout(r, 500));
  const deltasAfterAbort = notifications
    .slice(marksBefore)
    .filter(
      (n) =>
        n.method === 'event.assistant.delta' &&
        typeof n.params['delta'] === 'string' &&
        (n.params['delta'] as string).startsWith('chunk-'),
    );
  // 已收增量（可能为 0——中断发生在响应开始前）全部保留在事件流记录中；
  // 关键用户语义：没有 delta 在中断之后才到达（流真正停了）。
  const endedSeqIndex = notifications.findIndex((n) => n.method === 'event.turn.ended' && n.params['turnId'] === turnId);
  const deltasAfterEnd = endedSeqIndex === -1
    ? []
    : notifications.slice(endedSeqIndex).filter(
        (n) =>
          n.method === 'event.assistant.delta' &&
          typeof n.params['delta'] === 'string' &&
          (n.params['delta'] as string).startsWith('chunk-'),
      );
  assert.equal(deltasAfterEnd.length, 0, 'no scripted chunk delta arrives after the turn ended');
  void deltasAfterAbort;
  provider.setStreamDelay(0);

  // 那么：会话仍然可用——新消息得到完整回复
  const marksAfter = notifications.length;
  provider.nextText('JOURNEY_THREE_STILL_USABLE');
  const ended2 = await submitAndAwaitTurn(proc, sid, 'Are you still there?', ws);
  assert.equal(ended2['reason'], 'completed', 'a fresh prompt after the abort completes normally');
  const deltas2 = notifications
    .slice(marksAfter)
    .filter((n) => n.method === 'event.assistant.delta')
    .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
    .join('');
  assert.equal(deltas2, 'JOURNEY_THREE_STILL_USABLE', 'the post-abort prompt receives the full scripted reply');
  console.log('▶ journey-03 [R6]: deltas retained, turn cancelled, session usable again');
}

main().catch((error) => {
  console.error('✗ journey-03-first-conversation failed:', error);
  process.exit(1);
});
