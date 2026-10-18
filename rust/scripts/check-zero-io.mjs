#!/usr/bin/env node
// Zero-IO gate for mirri-core (plans/07 T13d, rulings J1/J2).
//
// Enforces the "direction A" zero-IO contract (rust/AGENTS.md 契约 6):
// mirri-core production code must not perform direct fs/process IO — it goes
// through injected mirri-os traits (FileSystem / SyncFs / PtyProvider /
// OsProcess). The gate scans mirri-core/src (excluding `#[cfg(test)]` mod
// blocks and their external `mod tests;` files) for banned direct-IO call
// patterns, and checks the dependency tree for forbidden network/DB crates.
//
// Banned source patterns (J2 ruled OUT of this list: `tokio::spawn` — the
// remaining spawn sites are task scheduling, not IO):
//   std::fs::, use std::fs, tokio::fs::, std::process::, tokio::process::,
//   Command::new, OpenOptions, File::open, File::create, std::os::unix::fs::
// plus implicit Path IO (F-T13-2): `.exists()` / `.canonicalize()` /
// `.metadata()` on std::path::Path are real std::fs IO disguised as
// innocuous method calls. Shapes are pinned to the repo's actual usage:
// zero-argument call form only (injected trait probes always pass an
// argument, e.g. `fs.exists(&p)`); `try_exists()` has no dot-anchored
// `exists` and never matches; `session.metadata().await` (SessionScope
// accessor, not Path IO) is excluded by the `.await` lookahead.
//
// Escape hatch: append `// zero-io-allow: <reason>` to a line — or, when the
// line opens a block (ends in `{`), place it on the first line inside the
// block: rustfmt relocates trailing comments past a `{` there, so the
// same-line form cannot survive `cargo fmt`.
//
// Dependency tree (J1 whitelist ruling):
//   - rusqlite: FAIL (regression lockout)
//   - reqwest: FAIL unless its only immediate dependent is the whitelisted
//     carrier `jsonschema` (jsonschema's default `resolve-http` feature pulls
//     reqwest for remote $ref resolution — a known, rustls-backed workspace
//     condition, see ROADMAP §2.1). A direct reqwest dependency on mirri-core
//     or any new carrier fails the gate.
//   - nix / portable-pty (via mirri-os), tokio fs feature (via mirri-os +
//     mirri-persistence): whitelist, reported as INFO.
//
// Usage:
//   node rust/scripts/check-zero-io.mjs            # gate: exit 1 on violation
//   node rust/scripts/check-zero-io.mjs --report  # progress dashboard:
//                                                  # counts only, always exit 0
//   node rust/scripts/check-zero-io.mjs --selftest  # adversarial regression
//                                                   # fixtures (F-T13-1/M6)
//
// The gate runs the self-test suite silently on every invocation too: the
// stripper/masker is the enforcement mechanism itself, so a regression in it
// fails the gate before any scan happens.

import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const rustDir = join(import.meta.dirname, '..');
const coreSrcDir = join(rustDir, 'crates', 'mirri-core', 'src');
const reportMode = process.argv.includes('--report');

// --- source scan -----------------------------------------------------------

// Word-boundary anchors keep `ForegroundShellCommand::new` (an internal type
// constructor) from tripping the `Command::new` rule, and similarly for
// `*File::open`-shaped identifiers.
const BANNED_PATTERNS = [
  { name: 'std::fs::', re: /\bstd::fs::/ },
  { name: 'use std::fs', re: /\buse\s+std::fs\b/ },
  { name: 'tokio::fs::', re: /\btokio::fs::/ },
  { name: 'std::process::', re: /\bstd::process::/ },
  { name: 'tokio::process::', re: /\btokio::process::/ },
  { name: 'Command::new', re: /\bCommand::new/ },
  { name: 'OpenOptions', re: /\bOpenOptions\b/ },
  { name: 'File::open', re: /\bFile::open/ },
  { name: 'File::create', re: /\bFile::create/ },
  { name: 'std::os::unix::fs::', re: /\bstd::os::unix::fs::/ },
  { name: 'Path::exists()', re: /\.\s*exists\s*\(\s*\)/ },
  { name: 'Path::canonicalize()', re: /\.\s*canonicalize\s*\(\s*\)/ },
  { name: 'Path::metadata()', re: /\.\s*metadata\s*\(\s*\)(?!\s*\.\s*await)/ },
];

// Line-level escape hatch: `// zero-io-allow: <reason>` (reason required).
const ALLOW_RE = /\/\/\s*zero-io-allow:\s*\S/;

function listRustFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...listRustFiles(path));
    } else if (entry.isFile() && entry.name.endsWith('.rs')) {
      out.push(path);
    }
  }
  return out;
}

/**
 * Find the end of a `mod NAME { ... }` body that opens at `openBrace`,
 * skipping braces inside strings / char literals / comments. Returns the index
 * just past the matching `}`.
 */
function findModBodyEnd(text, openBrace) {
  let i = openBrace + 1;
  while (i < text.length) {
    const c = text[i];
    if (c === '/' && text[i + 1] === '/') {
      while (i < text.length && text[i] !== '\n') i++;
    } else if (c === '/' && text[i + 1] === '*') {
      i += 2;
      while (i < text.length && !(text[i] === '*' && text[i + 1] === '/')) i++;
      i += 2;
    } else if (c === '"') {
      // normal string literal
      i++;
      while (i < text.length && text[i] !== '"') i += text[i] === '\\' ? 2 : 1;
      i++;
    } else if (c === 'r' && !/[\w$]/.test(text[i - 1] ?? ' ') && /["#]/.test(text[i + 1] ?? '')) {
      // raw string: r"…" / r#"…"# / r##"…"## — closing needs the same hash count
      let hashes = 0;
      let j = i + 1;
      while (text[j] === '#') {
        hashes++;
        j++;
      }
      if (text[j] === '"') {
        i = j + 1;
        const closing = '"' + '#'.repeat(hashes);
        while (i < text.length && text.slice(i, i + closing.length) !== closing) i++;
        i += closing.length;
      } else {
        i++; // plain identifier starting with r — not a raw string
      }
    } else if (c === "'") {
      // char literal ('x', '\n', '\u{1F600}') vs lifetime ('static) — a char
      // literal is `'` + (unicode escape | escape | single char) + `'`.
      const rest = text.slice(i, i + 16);
      const m = rest.match(/^'(\\u\{[0-9a-fA-F_]{1,6}\}|\\.|[^\\'])'/);
      if (m) {
        i += m[0].length;
      } else {
        i++; // lifetime — skip the quote, keep scanning
      }
    } else if (c === '{') {
      // nested brace (inner mod / impl block) — find *its* end recursively
      i = findModBodyEnd(text, i);
    } else if (c === '}') {
      return i + 1;
    } else {
      i++;
    }
  }
  return text.length; // unterminated — be lenient
}

/**
 * Replace the *content* of comments and string literals with equal-length
 * whitespace (newlines preserved), so that decoy `#[cfg(test)] mod … {`
 * text inside a comment or string cannot trigger test-block stripping
 * (F-T13-1), and banned-pattern scanning ignores comment/doc/string prose
 * that merely *discusses* e.g. `std::fs::`. Uses the same lexical state
 * machine as `findModBodyEnd` (line comment / nested block comment / string
 * / raw string / char-vs-lifetime). Because the mask is equal-length and
 * newline-preserving, every index and file:line report stays aligned with
 * the original file.
 */
function maskCommentsAndStrings(text) {
  const out = text.split('');
  const isIdent = (c) => /[\w$]/.test(c ?? ' ');
  const blank = (i) => {
    if (out[i] !== '\n') out[i] = ' ';
  };
  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i];
    if (c === '/' && text[i + 1] === '/') {
      while (i < n && text[i] !== '\n') {
        blank(i);
        i++;
      }
    } else if (c === '/' && text[i + 1] === '*') {
      // Rust block comments nest.
      let depth = 0;
      while (i < n) {
        if (text[i] === '/' && text[i + 1] === '*') {
          blank(i);
          blank(i + 1);
          i += 2;
          depth++;
        } else if (text[i] === '*' && text[i + 1] === '/') {
          blank(i);
          blank(i + 1);
          i += 2;
          depth--;
          if (depth === 0) break;
        } else {
          blank(i);
          i++;
        }
      }
    } else if (c === '"') {
      // normal string literal (also covers b"…" byte strings)
      blank(i);
      i++;
      while (i < n && text[i] !== '"') {
        const step = text[i] === '\\' ? 2 : 1;
        for (let k = 0; k < step && i + k < n; k++) blank(i + k);
        i += step;
      }
      if (i < n) {
        blank(i);
        i++;
      }
    } else if (
      c === 'r' &&
      (!isIdent(text[i - 1]) ||
        (text[i - 1] === 'b' && !isIdent(text[i - 2]))) &&
      /["#]/.test(text[i + 1] ?? '')
    ) {
      // raw string: r"…" / r#"…"# / r##"…"## (br#"…"# too) — closing needs
      // the same hash count
      let hashes = 0;
      let j = i + 1;
      while (text[j] === '#') {
        hashes++;
        j++;
      }
      if (text[j] === '"') {
        const closing = '"' + '#'.repeat(hashes);
        let end = j + 1;
        while (end < n && text.slice(end, end + closing.length) !== closing) end++;
        end = Math.min(end + closing.length, n);
        for (let k = i; k < end; k++) blank(k);
        i = end;
      } else {
        i++; // plain identifier starting with r — not a raw string
      }
    } else if (c === "'") {
      // char literal ('x', '\n', '\u{1F600}') vs lifetime ('static) — a char
      // literal is `'` + (unicode escape | escape | single char) + `'`.
      const rest = text.slice(i, i + 16);
      const m = rest.match(/^'(\\u\{[0-9a-fA-F_]{1,6}\}|\\.|[^\\'])'/);
      if (m) {
        for (let k = 0; k < m[0].length; k++) blank(i + k);
        i += m[0].length;
      } else {
        i++; // lifetime — keep scanning
      }
    } else {
      i++;
    }
  }
  return out.join('');
}

/**
 * Strip `#[cfg(test)] mod … { … }` blocks from a file's text. Also returns
 * the names of external test modules (`#[cfg(test)] mod name;`), whose files
 * live outside this text and must be excluded from the scan entirely.
 *
 * F-T13-1 fix: the cfg(test) match and brace balancing run on the masked
 * text (comments/strings blanked), so decoy `#[cfg(test)] mod … {` inside a
 * comment or string literal can no longer swallow the production code that
 * follows it. The same byte ranges are cut from the raw text (replaced by
 * newlines to keep line numbers), which keeps the `zero-io-allow` exemption
 * markers — comments — visible to pass 2.
 */
function stripTestModules(rawText) {
  const externalMods = [];
  let masked = maskCommentsAndStrings(rawText);
  let text = rawText;
  // `#[cfg(test)] mod …` / `#[cfg(all(test, …))]` / `#[cfg_attr(…, test)]`
  const cfgTestRe =
    /#\[[^\n]*\b(?:cfg\(\s*(?:all\(\s*)?test|cfg_attr\([^)]*,\s*test)\b[^\n]*\]\s*\n?\s*mod\s+(\w+)/g;
  let match;
  while ((match = cfgTestRe.exec(masked)) !== null) {
    const afterName = masked.slice(match.index + match[0].length);
    if (/^\s*;/.test(afterName)) {
      // `#[cfg(test)] mod name;` — external file (name.rs or name/mod.rs)
      externalMods.push(match[1]);
    } else if (/^\s*\{/.test(afterName)) {
      const openBrace = match.index + match[0].length + afterName.match(/^\s*\{/)[0].length - 1;
      const end = findModBodyEnd(masked, openBrace);
      const cut = (t) => {
        const newlines = (t.slice(match.index, end).match(/\n/g) ?? []).length;
        return t.slice(0, match.index) + '\n'.repeat(newlines) + t.slice(end);
      };
      masked = cut(masked);
      text = cut(text);
      cfgTestRe.lastIndex = match.index; // re-scan from the same offset
    }
  }
  return { maskedText: masked, rawText: text, externalMods };
}

/**
 * Scan already-stripped text (masked + raw, same line count) for banned
 * direct-IO patterns. The banned-pattern match runs on the *masked* line
 * (comments/strings blanked) so prose that merely discusses `std::fs::`
 * cannot trip it, while the `zero-io-allow` exemption marker — itself a
 * comment — is honoured from the *raw* line.
 */
function scanStrippedLines(maskedText, rawText) {
  const maskedLines = maskedText.split('\n');
  const rawLines = rawText.split('\n');
  const found = [];
  for (let n = 0; n < maskedLines.length; n++) {
    if (ALLOW_RE.test(rawLines[n] ?? '')) continue;
    // rustfmt relocates a trailing marker past a block-opening `{` to the
    // first line inside the block, so a marker is also honoured there — but
    // only for that exact shape: a line that does not open a block keeps the
    // marker anchored to itself, so an unrelated following marker cannot
    // silently exempt the preceding line.
    const opensBlock = (rawLines[n] ?? '').trimEnd().endsWith('{');
    if (opensBlock && ALLOW_RE.test(rawLines[n + 1] ?? '')) continue;
    for (const pattern of BANNED_PATTERNS) {
      if (pattern.re.test(maskedLines[n])) {
        found.push({ line: n + 1, pattern: pattern.name, text: (rawLines[n] ?? '').trim() });
      }
    }
  }
  return found;
}

/** Strip + scan one file's text (used by the self-test on inline fixtures). */
function findViolations(rawText) {
  const { maskedText, rawText: strippedRaw } = stripTestModules(rawText);
  return scanStrippedLines(maskedText, strippedRaw);
}

// --- self-test (F-T13-1 regression: review mutation M6 decoy shape) --------
//
// The guard is the enforcement mechanism for AGENTS.md 契约 6, so it carries
// its own adversarial fixtures: the exact decoy-comment + direct-IO shape
// that bypassed the pre-fix stripper (review mutation M6), decoy strings and
// raw strings, decoy + real cfg(test) mixing, comment/string prose that must
// NOT trip the banned patterns, the implicit Path IO shapes, and the
// exemption marker. `--selftest` prints each case and exits non-zero on
// failure; the gate itself runs the same suite silently before scanning.

const SELFTEST_CASES = [
  {
    name: 'M6 decoy comment must not mask a real violation',
    text: [
      'pub fn enabled() -> bool {',
      '    // see #[cfg(test)] mod examples {',
      '    // for the test-suite counterpart of this flag table.',
      '    let _ = std::fs::read_to_string("some-flag-file");',
      '    true',
      '}',
      '',
    ].join('\n'),
    expect: [{ line: 4, pattern: 'std::fs::' }],
  },
  {
    name: 'decoy string / raw string must not trigger stripping',
    text: [
      'const DECOY: &str = "#[cfg(test)] mod evil {";',
      'pub fn probe() {',
      '    let raw = r#"see #[cfg(test)] mod raw { here"#;',
      '    let _ = std::fs::read_to_string("x");',
      '}',
      '',
    ].join('\n'),
    expect: [{ line: 4, pattern: 'std::fs::' }],
  },
  {
    name: 'decoy comment + real cfg(test) block: strip exactly the real one',
    text: [
      '// decoy: see #[cfg(test)] mod examples {',
      'pub fn probe() {',
      '    let _ = std::fs::read_to_string("a");',
      '}',
      '',
      '#[cfg(test)]',
      'mod examples {',
      '    use std::fs::read_to_string;',
      '    fn helper() {',
      '        let _ = read_to_string("b");',
      '    }',
      '}',
      '',
      'pub fn after() {',
      '    let _ = std::process::Command::new("ls");',
      '}',
      '',
    ].join('\n'),
    expect: [
      { line: 3, pattern: 'std::fs::' },
      { line: 15, pattern: 'std::process::' },
      { line: 15, pattern: 'Command::new' },
    ],
  },
  {
    name: 'comment/doc/string prose discussing banned APIs must not trip',
    text: [
      '// production code must not call std::fs::read_to_string directly (zero-IO contract)',
      '/// Docs: `Command::new` and `OpenOptions` are banned in mirri-core.',
      'const NOTE: &str = "never use tokio::fs:: or std::process::Command::new here";',
      '',
    ].join('\n'),
    expect: [],
  },
  {
    name: 'implicit Path IO shapes vs injected-trait / accessor lookalikes',
    text: [
      'pub fn p(root: &std::path::Path) -> bool {',
      '    let _ = session.metadata().await; // SessionScope accessor, not Path IO',
      '    let _ = fs.exists(&root); // injected trait probe, with argument',
      '    let _ = fs.exists(root); // ditto, no reference — still an argument',
      '    let _ = root.try_exists(); // not dot-anchored `exists`',
      '    root.exists() && root.canonicalize().is_ok() && root.metadata().is_ok()',
      '}',
      '',
    ].join('\n'),
    expect: [
      { line: 6, pattern: 'Path::exists()' },
      { line: 6, pattern: 'Path::canonicalize()' },
      { line: 6, pattern: 'Path::metadata()' },
    ],
  },
  {
    name: 'zero-io-allow exemption marker is honoured on the raw line',
    text: [
      'pub fn probe() {',
      '    let _ = std::fs::read_to_string("x"); // zero-io-allow: baseline probe',
      '}',
      '',
    ].join('\n'),
    expect: [],
  },
  {
    name: 'marker rustfmt relocates into the block is honoured (block-opening line)',
    text: [
      'pub fn probe(p: &std::path::Path) {',
      '    if p.exists() {',
      '        // zero-io-allow: baseline Path IO, see F-T13-2',
      '        return;',
      '    }',
      '}',
      '',
    ].join('\n'),
    expect: [],
  },
  {
    name: 'a following marker must NOT exempt a non-block-opening line',
    text: [
      'pub fn probe(p: &std::path::Path) -> bool {',
      '    let _ = p.exists();',
      '    // zero-io-allow: marker belonging to a later line',
      '    p.metadata().is_ok()',
      '}',
      '',
    ].join('\n'),
    expect: [
      { line: 2, pattern: 'Path::exists()' },
      { line: 4, pattern: 'Path::metadata()' },
    ],
  },
];

function runSelfTest(verbose = false) {
  const failures = [];
  for (const testCase of SELFTEST_CASES) {
    const got = findViolations(testCase.text).map((v) => ({
      line: v.line,
      pattern: v.pattern,
    }));
    const want = testCase.expect;
    const same =
      got.length === want.length &&
      got.every((g, i) => g.line === want[i].line && g.pattern === want[i].pattern);
    if (verbose) {
      console.log(`${same ? 'ok  ' : 'FAIL'}  ${testCase.name}`);
      if (!same) {
        console.log(`      want: ${JSON.stringify(want)}`);
        console.log(`      got : ${JSON.stringify(got)}`);
      }
    }
    if (!same) failures.push(testCase.name);
  }
  return { passed: SELFTEST_CASES.length - failures.length, failed: failures.length, failures };
}

const selfTestMode = process.argv.includes('--selftest');
const selfTest = runSelfTest(selfTestMode || process.env.ZERO_IO_GATE_DEBUG === '1');
if (selfTestMode) {
  console.log(
    `zero-io self-test: ${selfTest.passed}/${SELFTEST_CASES.length} passed` +
      (selfTest.failed > 0 ? `, ${selfTest.failed} FAILED` : ''),
  );
  process.exit(selfTest.failed > 0 ? 1 : 0);
}
if (selfTest.failed > 0) {
  // Silent regression check on every gate run: the stripper/masker is the
  // enforcement mechanism itself, so a broken stripper must fail the gate.
  for (const name of selfTest.failures) console.error(`FAIL: self-test: ${name}`);
  console.error('zero-io gate: built-in self-test failed — stripper/masker regression');
  process.exit(1);
}

const scanFiles = listRustFiles(coreSrcDir);
// Pass 1: strip inline `#[cfg(test)]` blocks (comment/string-masked) and
// collect external test module declarations (`#[cfg(test)] mod name;`) —
// their files are excluded entirely, regardless of directory iteration order.
const strippedFiles = [];
const excluded = new Set();
for (const file of scanFiles) {
  const { maskedText, rawText, externalMods } = stripTestModules(readFileSync(file, 'utf8'));
  strippedFiles.push({ file, maskedText, rawText });
  for (const name of externalMods) {
    excluded.add(join(file, '..', `${name}.rs`));
    excluded.add(join(file, '..', name, 'mod.rs'));
  }
}

// Pass 2: scan the production text of every non-excluded file — banned
// patterns on the masked line, exemption markers on the raw line.
const violations = [];
const patternCounts = new Map(BANNED_PATTERNS.map((p) => [p.name, 0]));

for (const { file, maskedText, rawText } of strippedFiles) {
  if (excluded.has(file)) continue;
  for (const v of scanStrippedLines(maskedText, rawText)) {
    violations.push({ file: file.slice(rustDir.length + 1), ...v });
    patternCounts.set(v.pattern, patternCounts.get(v.pattern) + 1);
  }
}

// --- dependency tree check --------------------------------------------------

const FORBIDDEN_CRATES = ['rusqlite'];
// reqwest's whitelisted transitive carriers (J1 + audit: jsonschema's default
// `resolve-http` feature is the only in-tree source today).
const REQWEST_CARRIERS = new Set(['jsonschema']);
const WHITELIST_CRATES = ['nix', 'portable-pty'];

function runCargo(args) {
  return spawnSync('cargo', args, { cwd: rustDir, encoding: 'utf8' });
}

function parseTreeCrateNames(output) {
  const names = new Set();
  for (const line of output.split('\n')) {
    const m = line.match(/^(?:[^\w]\s*)?([\w-]+) v[\d.]/);
    if (m) names.add(m[1]);
  }
  return names;
}

function reqwestImmediateDependents() {
  const result = runCargo(['tree', '-p', 'mirri-core', '-i', 'reqwest']);
  if (result.status !== 0) return null; // reqwest not in tree
  const dependents = new Set();
  for (const line of result.stdout.split('\n').slice(1)) {
    // tree children of the root have no 4-char ancestor prefix
    const m = line.match(/^((?:[│ ]{4})*)(?:[├└])──\s+([\w-]+) v/);
    if (m && m[1].length === 0) dependents.add(m[2]);
  }
  return dependents;
}

let treeFailures = [];
const treeInfo = [];

const treeResult = runCargo(['tree', '-p', 'mirri-core', '--prefix', 'none']);
if (treeResult.status !== 0) {
  treeFailures.push(`cargo tree failed: ${treeResult.stderr.trim().split('\n')[0]}`);
} else {
  const crates = parseTreeCrateNames(treeResult.stdout);
  for (const crate of FORBIDDEN_CRATES) {
    if (crates.has(crate)) {
      treeFailures.push(`forbidden crate in mirri-core dependency tree: ${crate}`);
    }
  }
  for (const crate of WHITELIST_CRATES) {
    if (crates.has(crate)) {
      treeInfo.push(`whitelisted (J1): ${crate} present via mirri-os`);
    }
  }
  if (crates.has('reqwest')) {
    const dependents = reqwestImmediateDependents();
    const carriers = [...(dependents ?? [])].filter((d) => !REQWEST_CARRIERS.has(d));
    if (dependents === null || carriers.length > 0) {
      treeFailures.push(
        `reqwest reachable from mirri-core outside whitelisted carriers ${[...REQWEST_CARRIERS].join(',')}: ${carriers.join(', ') || 'direct'}`,
      );
    } else {
      treeInfo.push(
        `whitelisted (J1): reqwest present only via ${[...dependents].join(',')} (default resolve-http feature, rustls-backed — ROADMAP §2.1)`,
      );
    }
  }
  const featuresResult = runCargo([
    'tree',
    '-p',
    'mirri-core',
    '-e',
    'features',
    '--prefix',
    'none',
  ]);
  if (featuresResult.status === 0) {
    if (/^tokio feature "fs"/m.test(featuresResult.stdout)) {
      treeInfo.push('whitelisted (J1): tokio fs feature on (workspace tokio = "full"; via mirri-os + mirri-persistence)');
    }
  }
}

// --- output -----------------------------------------------------------------

const skippedExternal = strippedFiles.filter((f) => excluded.has(f.file)).length;
const scannedCount = scanFiles.length - skippedExternal;

if (reportMode) {
  console.log(
    `zero-io report — scanned ${scannedCount} files (${skippedExternal} external test-module file(s) excluded)`,
  );
  console.log(`banned-pattern hits: ${violations.length}`);
  for (const [pattern, count] of patternCounts) {
    if (count > 0) console.log(`  ${count}\t${pattern}`);
  }
  for (const info of treeInfo) console.log(`INFO: ${info}`);
  for (const failure of treeFailures) console.log(`FAIL: ${failure}`);
  process.exit(0);
}

for (const v of violations) {
  console.error(
    `zero-io violation: ${v.file}:${v.line} [${v.pattern}] ${v.text}\n` +
      `  -> route IO through mirri-os traits, or add a line-level exemption:  // zero-io-allow: <reason>`,
  );
}
for (const info of treeInfo) console.log(`INFO: ${info}`);
for (const failure of treeFailures) console.error(`FAIL: ${failure}`);

if (violations.length > 0 || treeFailures.length > 0) {
  console.error(
    `zero-io gate: ${violations.length} source violation(s), ${treeFailures.length} dependency-tree failure(s)`,
  );
  process.exit(1);
}
console.log(
  `zero-io gate ok: 0 hits in ${scannedCount} mirri-core src files (${skippedExternal} external test-module file(s) excluded); dependency tree clean`,
);
