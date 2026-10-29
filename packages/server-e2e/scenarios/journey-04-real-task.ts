#!/usr/bin/env node
/**
 * Journey ④ — a real task loop and a subagent (spec R7 + R8).
 *
 * 假如：用户在一个最小 node 项目工作区里，让代理完成"写函数 → 跑测试 → 汇报"闭环
 * 当：① 多轮工具剧本：Edit 写入 kebab-case 函数 → Bash 跑 node --test → 汇总文本
 *     ② 子代理剧本：Agent 工具 + 子代理轮流式回复
 * 那么：① 函数文件真实落盘；测试命令真实成功；汇报引用真实输出片段；
 *       消息历史含完整工具记录；
 *       ② 事件序列 event.subagent.spawned → 子代理 event.assistant.delta →
 *       event.subagent.completed；主代理最终回复包含子代理结果
 *
 * Probe findings baked into the assertions (2026-10-28 code probes):
 *   - Edit tool params: `{path, old_string, new_string}` (exact-substring).
 *   - Bash tool params: `{command}` (foreground default 60s timeout).
 *   - Agent tool params: `{prompt, description}` (both required).
 *   - Subagent events: `event.subagent.spawned` (payload `subagentId`,
 *     `description`), subagent turns stream `event.assistant.delta` with
 *     `agentId` = the subagent's agent id, `event.subagent.completed` carries
 *     `resultSummary`.
 *   - The fake provider serves BOTH the main and the subagent LLM calls in
 *     arrival order — responses are queued per call.
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

const UTILS_ORIGINAL = 'export const existingValue = 1;\n';
// NOTE: the plan's "kebab-case function" is realized as a kebab-case NAMED
// artifact — JS identifiers cannot contain hyphens (a `journey-kebab-function`
// identifier is a syntax error), so the function is `journeyKebabFunction`
// living in the kebab-case test file `journey-utils.test.ts`, returning the
// kebab marker string.
const UTILS_NEW_FUNCTION = "export function journeyKebabFunction() {\n  return 'written-by-journey-04';\n}\n";

async function main(): Promise<void> {
  console.log('▶ journey-04: booting serve + fake provider with a minimal node project');
  const provider = await createFakeProviderServer();
  const workDir = mkdtempSync(join(tmpdir(), 'mirri-journey04-'));

  // A minimal node project: package.json + existing src/utils.ts + a test
  // that will only pass AFTER the agent adds the new function.
  mkdirSync(join(workDir, 'src'), { recursive: true });
  writeFileSync(
    join(workDir, 'package.json'),
    JSON.stringify({ name: 'journey-04-project', version: '1.0.0', type: 'module' }, null, 2) + '\n',
    'utf8',
  );
  writeFileSync(join(workDir, 'src', 'utils.ts'), UTILS_ORIGINAL, 'utf8');
  writeFileSync(
    join(workDir, 'src', 'journey-utils.test.ts'),
    [
      "import test from 'node:test';",
      "import assert from 'node:assert/strict';",
      "import { journeyKebabFunction } from './utils.ts';",
      '',
      "test('journeyKebabFunction returns the marker', () => {",
      "  assert.equal(journeyKebabFunction(), 'written-by-journey-04');",
      '});',
      '',
    ].join('\n'),
    'utf8',
  );

  const proc = await startServe({ workDir, configToml: fakeProviderConfigToml(provider.baseUrl) });
  const ws = new WsRpcClient(proc.baseUrl);
  const notifications: ObservedNotification[] = [];

  try {
    await ws.connect();
    const createRes = await fetch(`${proc.baseUrl}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ title: 'journey 04 real task', metadata: { cwd: workDir } }),
    });
    const created = (await createRes.json()) as { data?: { id?: string } };
    const sid = created.data?.id;
    assert.ok(typeof sid === 'string', 'session created');
    await ws.subscribe(sid);
    ws.onNotification((n) => {
      if (n.params['sessionId'] === sid) notifications.push({ method: n.method, params: n.params });
    });
    // The journey's user grants every tool approval this task asks for —
    // R7 is about the write-execute-report loop, not the approval gate
    // (journey-03 owns that). Answering via the same reverse-RPC face a
    // real client uses.
    ws.onRequest((req) => {
      if (req.method !== 'approval.ask') return;
      ws.respond(req.id, { decision: 'approved' });
    });

    await phase_realTaskLoop(proc, provider, ws, sid, notifications, workDir);
    await phase_subagent(proc, provider, ws, sid, notifications);

    console.log('✓ journey-04-real-task: write-execute-report loop and subagent flow all verified');
  } finally {
    ws.close();
    await stopServe(proc, 'kill').catch(() => {});
    await provider.close().catch(() => {});
  }
}

/** Submit a fire-mode prompt and wait for its turn to end. */
async function submitAndAwaitTurn(
  proc: ProcHandle,
  sid: string,
  text: string,
  ws: WsRpcClient,
): Promise<Record<string, unknown>> {
  const sendRes = await fetch(`${proc.baseUrl}/api/v1/sessions/${sid}/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ input: [{ type: 'text', text }], mode: 'fire' }),
  });
  const sendBody = (await sendRes.json()) as { code?: number };
  if (sendBody.code !== 0) throw new Error(`message send failed: ${JSON.stringify(sendBody)}`);
  return ws.waitForEvent(sid, 'event.turn.ended', 90_000);
}

/** ── R7：写-执行-汇报闭环 ───────────────────────────────────────────────── */
async function phase_realTaskLoop(
  proc: ProcHandle,
  provider: FakeProviderServer,
  ws: WsRpcClient,
  sid: string,
  notifications: ObservedNotification[],
  workDir: string,
): Promise<void> {
  console.log('▶ journey-04 [R7]: write-execute-report loop');
  const marksBefore = notifications.length;

  // —— 轮 1：Edit 把 kebab-case 函数追加进 src/utils.ts ——
  provider.nextToolCall('Edit', {
    path: 'src/utils.ts',
    old_string: UTILS_ORIGINAL,
    new_string: UTILS_ORIGINAL + UTILS_NEW_FUNCTION,
  });
  // —— 轮 2（同 turn 第二个 LLM 调用）：Bash 跑 node --test（必须真实成功）——
  provider.nextToolCall('Bash', { command: 'node --test src/journey-utils.test.ts' });
  // —— 轮 3：汇总文本（引用真实输出片段）——
  provider.nextText('Task complete: I added journeyKebabFunction to src/utils.ts and ran node --test src/journey-utils.test.ts — 1 test passed with exit code 0.');

  const ended = await submitAndAwaitTurn(proc, sid, 'Add the journey function to utils.ts, run the tests, and report.', ws);
  assert.equal(ended['reason'], 'completed', 'the multi-round task turn completes normally');
  const windowEvents = notifications.slice(marksBefore);

  // 那么：函数文件真实落盘且内容正确
  const utilsPath = join(workDir, 'src', 'utils.ts');
  assert.equal(existsSync(utilsPath), true, 'src/utils.ts still exists');
  const utilsContent = readFileSync(utilsPath, 'utf8');
  assert.ok(
    utilsContent.includes("journeyKebabFunction"),
    'the utils file on disk contains the new function',
  );
  assert.ok(
    utilsContent.includes("written-by-journey-04"),
    'the new function body carries the expected implementation',
  );
  console.log('▶ journey-04 [R7]: function written to disk');

  // 那么：Bash 真实执行且测试真实通过（tool.result 无错误 + 输出含通过标记）
  const bashResults = windowEvents.filter(
    (n) => n.method === 'event.tool.result' && n.params['name'] === 'Bash',
  );
  assert.ok(bashResults.length > 0, 'a Bash tool result event is recorded');
  const bashOutput = bashResults
    .map((n) => (typeof n.params['output'] === 'string' ? (n.params['output'] as string) : JSON.stringify(n.params['output'])))
    .join('\n');
  assert.equal(bashResults[0]?.params['isError'], false, 'the node --test run did not error');
  assert.ok(
    /pass 1|✔|1 passing/i.test(bashOutput),
    `the Bash output shows the test genuinely passed (got: ${bashOutput.slice(0, 200)})`,
  );
  console.log('▶ journey-04 [R7]: node --test genuinely ran and passed');

  // 那么：汇报文本引用真实执行结果片段
  const deltas = windowEvents
    .filter((n) => n.method === 'event.assistant.delta')
    .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
    .join('');
  assert.ok(deltas.includes('node --test'), 'the final summary quotes the real command node --test');
  assert.ok(/pass/i.test(deltas), 'the final summary reports the passing tests');
  console.log('▶ journey-04 [R7]: summary text quotes the real execution result');

  // 那么：消息历史含完整工具记录（REST message.list）
  const history = await ws.call<{ messages: Array<{ role?: string; content?: unknown }>; hasMore?: boolean }>(
    'message.list',
    { sessionId: sid, pageSize: 50 },
  );
  const historyText = JSON.stringify(history.messages);
  assert.ok(history.messages.length >= 3, 'the history contains the full exchange (user, tool records, summary)');
  assert.ok(historyText.includes('Edit'), 'the history records the Edit tool call');
  assert.ok(historyText.includes('Bash'), 'the history records the Bash tool call');
  console.log('▶ journey-04 [R7]: message history carries the full tool records');
}

/** ── R8：子代理 ─────────────────────────────────────────────────────────── */
async function phase_subagent(
  proc: ProcHandle,
  provider: FakeProviderServer,
  ws: WsRpcClient,
  sid: string,
  notifications: ObservedNotification[],
): Promise<void> {
  console.log('▶ journey-04 [R8]: subagent task with streamed subagent turn');
  const marksBefore = notifications.length;

  // 剧本（按 LLM 调用到达顺序入队 — probe finding: the default 'coder'
  // subagent makes TWO LLM calls of its own: its working turn, then a
  // settle/summary turn; the main agent's final reply is the FOURTH call):
  //   1. 主代理调用 Agent 工具（发起子代理）
  //   2. 子代理工作轮（流式文本）
  //   3. 子代理收尾轮（流式文本 = 子代理最终产出）
  //   4. 主代理拿到子代理结果后的最终回复
  provider.nextToolCall('Agent', {
    prompt: 'Research the project and find what journeyKebabFunction returns.',
    description: 'explore utils implementation',
  });
  provider.nextText('Researching the utils implementation now.');
  provider.nextText('SUBAGENT_FINDING: journeyKebabFunction returns written-by-journey-04.');
  provider.nextText('The subagent found that journeyKebabFunction returns written-by-journey-04. That is the full result.');

  const ended = await submitAndAwaitTurn(proc, sid, 'Use a subagent to find what journeyKebabFunction returns, then tell me.', ws);
  assert.equal(ended['reason'], 'completed', 'the subagent task turn completes normally');
  const windowEvents = notifications.slice(marksBefore);

  // 那么：事件序列 event.subagent.spawned → 子代理 delta → event.subagent.completed
  const spawned = windowEvents.find((n) => n.method === 'event.subagent.spawned');
  assert.ok(spawned !== undefined, 'event.subagent.spawned is observed');
  const subagentId = spawned.params['subagentId'];
  assert.ok(typeof subagentId === 'string' && subagentId.length > 0, 'the spawned event carries a subagent id');
  console.log(`▶ journey-04 [R8]: subagent spawned (${subagentId})`);

  const completed = windowEvents.find((n) => n.method === 'event.subagent.completed');
  assert.ok(completed !== undefined, 'event.subagent.completed is observed');
  assert.equal(completed.params['subagentId'], subagentId, 'the completed event matches the spawned subagent id');

  // 子代理的流式 delta（agentId = 子代理 id）
  const subagentDeltas = windowEvents.filter(
    (n) => n.method === 'event.assistant.delta' && n.params['agentId'] === subagentId,
  );
  assert.ok(subagentDeltas.length > 0, 'the subagent streams its own assistant deltas (its agent id)');
  const subagentText = subagentDeltas
    .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
    .join('');
  assert.ok(
    subagentText.includes('SUBAGENT_FINDING'),
    `the subagent's streamed reply carries its finding (got: ${subagentText.slice(0, 120)})`,
  );
  console.log('▶ journey-04 [R8]: subagent streamed its finding with its own agent id');

  // 顺序断言：spawned 在子代理 delta 之前，completed 在其后
  const idxSpawned = windowEvents.findIndex((n) => n.method === 'event.subagent.spawned');
  const idxFirstSubagentDelta = windowEvents.findIndex(
    (n) => n.method === 'event.assistant.delta' && n.params['agentId'] === subagentId,
  );
  const idxCompleted = windowEvents.findIndex((n) => n.method === 'event.subagent.completed');
  assert.ok(idxSpawned < idxFirstSubagentDelta, 'subagent.spawned precedes the subagent deltas');
  assert.ok(idxFirstSubagentDelta < idxCompleted, 'the subagent deltas precede subagent.completed');

  // 那么：主代理最终回复包含子代理结果
  const mainDeltas = windowEvents
    .filter(
      (n) =>
        n.method === 'event.assistant.delta' &&
        n.params['agentId'] !== subagentId &&
        typeof n.params['agentId'] === 'string' &&
        !String(n.params['agentId']).includes(String(subagentId)),
    )
    .map((n) => (typeof n.params['delta'] === 'string' ? (n.params['delta'] as string) : ''))
    .join('');
  assert.ok(
    mainDeltas.includes('written-by-journey-04'),
    `the main agent's final reply incorporates the subagent finding (got: ${mainDeltas.slice(0, 160)})`,
  );
  console.log('▶ journey-04 [R8]: main agent reply incorporates the subagent result');
}

main().catch((error) => {
  console.error('✗ journey-04-real-task failed:', error);
  process.exit(1);
});
