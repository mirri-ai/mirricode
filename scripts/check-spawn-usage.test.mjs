/* oxlint-disable typescript/no-floating-promises -- node:test 顶层同步回调的 test() 必然返回未处理的 Promise */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';

const SCRIPT = join(import.meta.dirname, 'check-spawn-usage.mjs');

/** 构造一次性 fixture 仓库并黑盒运行 gate，返回退出码与 stderr。 */
function runGate(files) {
  const root = mkdtempSync(join(tmpdir(), 'spawn-usage-'));
  for (const [rel, content] of Object.entries(files)) {
    const path = join(root, rel);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, content);
  }
  const res = spawnSync(process.execPath, [SCRIPT, '--root', root], {
    encoding: 'utf-8',
  });
  return { status: res.status, stderr: res.stderr };
}

const MARKER_COMMENT = [
  '    // raw spawn allowed here: portable-pty spawns through the pty slave,',
  '    // not std Command (check-spawn-usage whitelist)',
].join('\n');

const RAW_CALL = '    let mut cmd = CommandBuilder::new(program);';

test('should pass when a whitelisted file carries the marker next to the raw call', () => {
  const { status } = runGate({
    'rust/crates/mirri-os/src/pty.rs': `fn open_pty() {\n${MARKER_COMMENT}\n${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 0);
});

test('should pass when line numbers above the exempted call drift', () => {
  const filler = 'fn pad() {}\n'.repeat(50);
  const { status } = runGate({
    'rust/crates/mirri-os/src/pty.rs': `${filler}fn open_pty() {\n${MARKER_COMMENT}\n${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 0);
});

test('should fail when the marker sits farther than 3 lines from the raw call', () => {
  const { status, stderr } = runGate({
    'rust/crates/mirri-os/src/pty.rs':
      'fn open_pty() {\n    // (check-spawn-usage whitelist)\n\n\n\n' + `${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 1);
  assert.match(stderr, /check-spawn-usage: 1 raw production spawn/);
});

test('should fail when an unlisted file carries the marker comment', () => {
  const { status } = runGate({
    'rust/crates/mirri-x/src/main.rs': `fn main() {\n${MARKER_COMMENT}\n${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 1);
});

test('should fail on a raw spawn with no marker at all', () => {
  const { status } = runGate({
    'rust/crates/mirri-x/src/main.rs': `fn main() {\n${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 1);
});

test('should pass for any raw spawn inside a whole-file whitelisted file', () => {
  const { status } = runGate({
    'rust/crates/mirri-os/src/process.rs':
      'pub fn helper() {\n    std::process::Command::new(prog)\n}\n',
  });
  assert.equal(status, 0);
});

test('should fail when a whitelisted file has a second raw call without the marker', () => {
  const { status, stderr } = runGate({
    'rust/crates/mirri-os/src/pty.rs':
      `fn open_pty() {\n${MARKER_COMMENT}\n${RAW_CALL}\n}\nfn helper() {\n${RAW_CALL}\n}\n`,
  });
  assert.equal(status, 1);
  assert.match(stderr, /check-spawn-usage: 1 raw production spawn/);
});

test('should ignore raw spawns inside #[cfg(test)] modules', () => {
  const { status } = runGate({
    'rust/crates/mirri-x/src/main.rs':
      '#[cfg(test)]\nmod tests {\n    fn t() {\n        std::process::Command::new("true");\n    }\n}\n',
  });
  assert.equal(status, 0);
});
