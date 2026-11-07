/* oxlint-disable typescript/no-floating-promises -- node:test 顶层同步回调的 test() 必然返回未处理的 Promise */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';

const SCRIPT = join(import.meta.dirname, 'check-test-hygiene.mjs');

/**
 * 构造一次性 fixture rust-root 并黑盒运行 gate。
 * `rootLayout`：相对 rust 根的路径 → 内容；`baseline`：可选的基线 JSON 对象。
 */
function runGate(rootLayout, { baseline } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'test-hygiene-'));
  for (const [rel, content] of Object.entries(rootLayout)) {
    const path = join(root, rel);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, content);
  }
  if (baseline !== undefined) {
    mkdirSync(join(root, 'scripts'), { recursive: true });
    writeFileSync(join(root, 'scripts', 'test-hygiene-baseline.json'), JSON.stringify(baseline));
  }
  const res = spawnSync(process.execPath, [SCRIPT, '--root', root], { encoding: 'utf-8' });
  return { status: res.status, stderr: res.stderr, stdout: res.stdout, root };
}

/** 从 stderr 提取 `file:line` 形式的违规行号列表。 */
function violationLines(stderr, file) {
  const lines = stderr.split('\n').filter((l) => l.startsWith('test-hygiene violation:'));
  return lines
    .filter((l) => l.includes(file))
    .map((l) => Number(l.split(`${file}:`)[1].split(' ')[0]));
}

// 一个 src 文件骨架：N 行生产代码 + 内联 #[cfg(test)] mod（内含 sleep）。
// sleep 行的真实文件行号 = prefix + 4（#[cfg(test)] 1 行 + mod 头 1 行 + fn 头 2 行）。
function srcFileWithInlineTestMod(prefixLines) {
  return (
    'fn prod() {}\n'.repeat(prefixLines) +
    '#[cfg(test)]\nmod tests {\n    fn helper() {\n' +
    '        tokio::time::sleep(std::time::Duration::from_millis(10)).await;\n' +
    '    }\n}\n'
  );
}

test('should report the real file line number for a sleep inside an inline cfg(test) mod', () => {
  const prefix = 10;
  const file = 'crates/demo/src/lib.rs';
  const { status, stderr } = runGate({ [file]: srcFileWithInlineTestMod(prefix) });
  // 真实行号：prefix(10) + #[cfg(test)](11) + mod(12) + fn(13) + sleep(14)
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [prefix + 4]);
});

test('should keep the real line number when production code above the test module grows', () => {
  const prefix = 120;
  const file = 'crates/demo/src/lib.rs';
  const { stderr } = runGate({ [file]: srcFileWithInlineTestMod(prefix) });
  assert.deepEqual(violationLines(stderr, file), [prefix + 4]);
});

test('should lock the off-by-one with a minimal fixture whose cfg(test) sits on line 1', () => {
  const file = 'crates/demo/src/lib.rs';
  // 行 1 = #[cfg(test)]，行 2 = mod 头，行 3 = fn 头，行 4 = sleep
  const { stderr } = runGate({
    [file]: '#[cfg(test)]\nmod tests {\n    fn helper() {\n        thread::sleep(std::time::Duration::from_millis(1));\n    }\n}\n',
  });
  assert.deepEqual(violationLines(stderr, file), [4]);
});

test('should compute line numbers independently per cfg(test) segment in one file', () => {
  const file = 'crates/demo/src/lib.rs';
  // 段 1：sleep 在行 3；段 2：sleep 在行 8
  const content = [
    '#[cfg(test)]',
    'mod a {',
    '    fn t() { thread::sleep(std::time::Duration::from_millis(1)); }',
    '}',
    '',
    '#[cfg(test)]',
    'mod b {',
    '    fn t() { thread::sleep(std::time::Duration::from_millis(1)); }',
    '}',
    '',
  ].join('\n');
  const { stderr } = runGate({ [file]: content });
  assert.deepEqual(violationLines(stderr, file), [3, 8]);
});

test('should still report a violation when the guard marker sits outside the segment', () => {
  const file = 'crates/demo/src/lib.rs';
  // test_support guard 标记在生产代码注释里（段外），sleep 在段首 15 行内
  const content = [
    '// uses test_support helpers below',
    'fn prod() {}',
    '#[cfg(test)]',
    'mod tests {',
    '    fn helper() {',
    '        tokio::time::sleep(std::time::Duration::from_millis(10)).await;',
    '    }',
    '}',
    '',
  ].join('\n');
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [6]);
});

test('should report real line numbers for an external cfg(test) mod file', () => {
  const main = [
    'fn prod() {}',
    '',
    '#[cfg(test)]',
    'mod tests;',
    '',
  ].join('\n');
  // modFileFor：lib.rs 的子模块解析到 src/tests.rs
  const modFile = [
    'fn helper() {}',
    '',
    'pub fn t() {',
    '    std::env::set_var("K", "V");',
    '}',
  ].join('\n');
  const { stderr } = runGate({
    'crates/demo/src/lib.rs': main,
    'crates/demo/src/tests.rs': modFile,
  });
  assert.deepEqual(violationLines(stderr, 'crates/demo/src/tests.rs'), [4]);
});

test('should not report a sleep when a guard context exists on the same line', () => {
  const file = 'crates/demo/tests/integration.rs';
  const content = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    wait_for(|| true); // wraps tokio::time::sleep internally',
    '}',
    '',
  ].join('\n');
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 0);
  assert.equal(violationLines(stderr, file).length, 0);
});

test('should not report a call annotated with a hygiene-allow comment', () => {
  const file = 'crates/demo/tests/integration.rs';
  const content = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    thread::sleep(std::time::Duration::from_millis(1)); // hygiene-allow: startup ordering',
    '}',
    '',
  ].join('\n');
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 0);
  assert.equal(violationLines(stderr, file).length, 0);
});

test('should exit 0 when a poll-step baseline entry covers a sleep, and 1 when a legacy entry no longer does', () => {
  const file = 'crates/demo/tests/integration.rs';
  const content = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    thread::sleep(std::time::Duration::from_millis(1));',
    '}',
    '',
  ].join('\n');
  // poll-step（合规豁免语义不变）：基线覆盖 → exit 0
  const pollStepBaseline = {
    violations: {
      'crates/demo/tests/integration.rs:4': {
        type: 'sleep',
        reason: 'poll-step: deadline-bounded polling step (sanctioned form, 16-design §4.2 rule 4)',
      },
    },
  };
  const ok = runGate({ [file]: content }, { baseline: pollStepBaseline });
  assert.equal(ok.status, 0, ok.stderr);

  // 零容忍（批 6a）：legacy reason 的 sleep 基线条目不再视为已覆盖 → exit 1
  const legacyBaseline = {
    violations: {
      'crates/demo/tests/integration.rs:4': { type: 'sleep', reason: 'legacy: pending R2/R4 migration' },
    },
  };
  const legacy = runGate({ [file]: content }, { baseline: legacyBaseline });
  assert.equal(legacy.status, 1, legacy.stdout);
  assert.ok(legacy.stderr.includes('integration.rs:4'), legacy.stderr);
  assert.ok(legacy.stderr.includes('zero-tolerance'), legacy.stderr);

  // 新增一行未被基线覆盖的 sleep（行 5）→ 必须报新违规
  const freshContent = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    thread::sleep(std::time::Duration::from_millis(1));',
    '    thread::sleep(std::time::Duration::from_millis(2));',
    '}',
    '',
  ].join('\n');
  const fresh = runGate({ [file]: freshContent }, { baseline: pollStepBaseline });
  assert.equal(fresh.status, 1);
  assert.ok(fresh.stderr.includes('integration.rs:5'), fresh.stderr);
});

test('should write the baseline under the --root tree only (never the real repo baseline)', async () => {
  const file = 'crates/demo/tests/integration.rs';
  const content = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    thread::sleep(std::time::Duration::from_millis(1));',
    '}',
    '',
  ].join('\n');
  const root = mkdtempSync(join(tmpdir(), 'test-hygiene-'));
  const cratePath = join(root, file);
  mkdirSync(dirname(cratePath), { recursive: true });
  writeFileSync(cratePath, content);
  const res = spawnSync(process.execPath, [SCRIPT, '--root', root, '--write-baseline'], {
    encoding: 'utf-8',
  });
  assert.equal(res.status, 0, res.stderr);
  const { readFileSync: readFile } = await import('node:fs');
  const written = JSON.parse(readFile(join(root, 'scripts', 'test-hygiene-baseline.json'), 'utf-8'));
  assert.ok(written.violations['crates/demo/tests/integration.rs:4']);
  // 真实仓库基线不含 fixture 条目
  const real = JSON.parse(readFile(join(import.meta.dirname, 'test-hygiene-baseline.json'), 'utf-8'));
  assert.equal(real.violations['crates/demo/tests/integration.rs:4'], undefined);
});

// --- 规则 R：real-env 归因 ------------------------------------------------------

/** 一个 tests/ 二进制 fixture：layer 标签 + 给定 fn 体。 */
function testBinary(body) {
  return ['// layer: T2', ...body, ''].join('\n');
}

test('should flag a raw subprocess spawn in a test fn without #[ignore]', () => {
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[test]',
    'fn spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [4]);
});

test('should not flag a raw subprocess spawn in an #[ignore]d test fn', () => {
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[test]',
    '#[ignore]',
    'fn spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
  ]);
  const { status } = runGate({ [file]: content });
  assert.equal(status, 0);
});

test('should attribute a spawn hit by fn body rather than by line window', () => {
  // fn a 带 #[ignore] 且只有一个短 fn 头；fn b 不带 ignore，命中行距离
  // fn a 的 #[ignore] 不足 15 行——旧的行窗口守卫会误放行，fn 归因必须拦下。
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[test]',
    '#[ignore]',
    'fn a() {',
    '    let _c = 1;',
    '}',
    '#[test]',
    'fn b() {',
    '    let _c = tokio::process::Command::new("echo");',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [9]);
});

test('should flag a real-time window in a test fn without #[ignore]', () => {
  const file = 'crates/demo/tests/window.rs';
  const content = testBinary([
    '#[test]',
    'fn samples_a_window() {',
    '    let _s = 0; // hygiene-allow: real-time: simulated work under test',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [4]);
});

test('should not flag a virtual-clock pacing window', () => {
  const file = 'crates/demo/tests/window.rs';
  const content = testBinary([
    '#[test]',
    'fn paces_a_scenario() {',
    '    let _s = 0; // hygiene-allow: scenario pacing (virtual clock — member work time)',
    '}',
  ]);
  const { status } = runGate({ [file]: content });
  assert.equal(status, 0);
});

test('should accept an #[ignore] with a reason string as R-layer attribution', () => {
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[test]',
    '#[ignore = "needs a real child process"]',
    'fn spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
  ]);
  const { status } = runGate({ [file]: content });
  assert.equal(status, 0);
});

test('should not treat doc-comment prose mentioning #[ignore] as attribution', () => {
  // 属性区间的判定只看代码（注释/字符串已被 mask）：文档注释里"提到"
  // #[ignore] 不算归因，否则一段说明文字就能悄悄放过裸 spawn。
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '/// Not `#[ignore]`d yet: the child is hermetic.',
    '#[test]',
    'fn spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [5]);
});

test('should accept a line-level hygiene-allow on a spawn in a plain test fn', () => {
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[test]',
    'fn spawns_a_hermetic_child() {',
    '    let _c = Command::new("echo"); // hygiene-allow: D: hermetic child',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 0, stderr);
});

test('should flag a spawn in a helper fn without a line-level hygiene-allow', () => {
  // helper 不能合法携带 #[ignore]，所以它没有归因能力：spawn 必须留在
  // 违规面上（或行级豁免并写明理由），由 caller 测试归因。
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    'fn helper_spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
    '#[test]',
    '#[ignore]',
    'fn spawns_a_child() {',
    '    helper_spawns_a_child();',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [3]);
});

test('should not let an #[ignore] on a non-test fn exempt a spawn', () => {
  // 非 test fn 上的 #[ignore] 是惰性的（编译期无效果），不构成归因。
  const file = 'crates/demo/tests/procs.rs';
  const content = testBinary([
    '#[ignore]',
    'fn helper_spawns_a_child() {',
    '    let _c = std::process::Command::new("echo");',
    '}',
  ]);
  const { status, stderr } = runGate({ [file]: content });
  assert.equal(status, 1);
  assert.deepEqual(violationLines(stderr, file), [4]);
});
