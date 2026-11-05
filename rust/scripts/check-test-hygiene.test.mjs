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

test('should exit 0 when every violation is baseline-covered, and 1 on a new one', () => {
  const file = 'crates/demo/tests/integration.rs';
  const content = [
    '// layer: T1',
    '#[test]',
    'fn waits() {',
    '    thread::sleep(std::time::Duration::from_millis(1));',
    '}',
    '',
  ].join('\n');
  const baseline = { violations: { 'crates/demo/tests/integration.rs:4': { type: 'sleep', reason: 'legacy: pending R2/R4 migration' } } };
  const ok = runGate({ [file]: content }, { baseline });
  assert.equal(ok.status, 0, ok.stderr);

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
  const fresh = runGate({ [file]: freshContent }, { baseline });
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
