#!/usr/bin/env node
// Model-hardcoding guard (spec 11 §1.4 A7 — source half; plan 11 Step 10.2).
//
// Scans the rust workspace's PRODUCTION source paths (crates/*/src) for
// hardcoded concrete model names (`claude-sonnet-4` / `claude-haiku-4` —
// the names the T1 forensics found silently bound). The runtime-assembly
// half of A7 is carried by the T1/T2 tests; this script pins the source
// half: after the fail-loud kernel, no production code path may spell a
// concrete model name the user never configured.
//
// Exclusions (spec 11 plan Step 10.2, T1 review I-2 ruling):
//   - `#[cfg(test)] mod … { … }` inline blocks — stripped with a
//     comment/string-aware brace balancer, so test fixtures do not trip.
//   - External test modules (`#[cfg(test)] mod name;`), `tests.rs` /
//     `test_*.rs` basenames, and `fixtures`/`testdata` path segments.
//   - Comment spans (line `//`, doc `///` / `//!`, block `/* */`) — doc
//     example literals are prose, not assembly paths (M-4 list:
//     config/mod.rs:11/:148, profile.rs:99/:222, anthropic.rs:57,
//     model_catalog.rs:26).
//   - `image_pipeline.rs`'s `VISION_MARKERS` array — the vision-capability
//     name heuristic (includes "claude-sonnet-4"/"claude-opus-4"/"gemini"
//     etc.). It never participates in model adjudication, so it is not an
//     R4 violation; registered here as the A7 scan's explicit exclusion.
//
// Usage:
//   node rust/scripts/check-model-hardcoding.mjs   # exit 1 on violation
//
// Not yet wired into CI — run manually (recorded as a post-T10 item).

import { readdirSync, readFileSync } from 'node:fs';
import { join, basename, dirname } from 'node:path';

const rustDir = join(import.meta.dirname, '..');
const cratesSrcDir = join(rustDir, 'crates');

// The concrete model names this guard pins (extend the list as new
// forensic families surface). Matched with a trailing boundary so
// `claude-sonnet-4-20250514` and bare `claude-sonnet-4` both hit, while
// `claude-sonnet-40`-shaped future names would need their own entry.
const BANNED_MODELS = [
  { name: 'claude-sonnet-4', re: /\bclaude-sonnet-4\b/ },
  { name: 'claude-haiku-4', re: /\bclaude-haiku-4\b/ },
];

const isIdent = (c) => /[\w$]/.test(c ?? ' ');

/**
 * Blank comment spans (line `//`, doc, nested block) with spaces, keeping
 * string / raw-string / char contents INTACT — unlike the zero-io gate this
 * scan targets string literals, so strings must survive the mask. The state
 * machine still tracks strings so an in-string `//` (URLs) is not mistaken
 * for a comment. Newlines are preserved so line numbers stay aligned.
 */
function maskComments(text) {
  const out = text.split('');
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
      // normal string literal — skip its content unblanked
      i++;
      while (i < n && text[i] !== '"') i += text[i] === '\\' ? 2 : 1;
      i++;
    } else if (
      c === 'r' &&
      !isIdent(text[i - 1]) &&
      /["#]/.test(text[i + 1] ?? '')
    ) {
      // raw string r"…" / r#"…"# — skip unblanked
      let hashes = 0;
      let j = i + 1;
      while (text[j] === '#') {
        hashes++;
        j++;
      }
      if (text[j] === '"') {
        const closing = '"' + '#'.repeat(hashes);
        let end = j + 1;
        while (end < n && text.slice(end, end + closing.length) !== closing) {
          end++;
        }
        i = Math.min(end + closing.length, n);
      } else {
        i++; // plain identifier starting with r
      }
    } else if (c === "'") {
      // char literal vs lifetime — skip a char literal, keep scanning on a
      // lifetime
      const rest = text.slice(i, i + 16);
      const m = rest.match(/^'(\\u\{[0-9a-fA-F_]{1,6}\}|\\.|[^\\'])'/);
      i = m ? i + m[0].length : i + 1;
    } else {
      i++;
    }
  }
  return out.join('');
}

/**
 * Blank BOTH comments and string literals — used only to locate
 * `#[cfg(test)] mod … {` spans and balance their braces safely (braces or
 * decoy attributes inside strings/comments cannot mislead the cut).
 */
function maskCommentsAndStrings(text) {
  const fully = maskComments(text);
  const out = fully.split('');
  const blank = (i) => {
    if (out[i] !== '\n') out[i] = ' ';
  };
  let i = 0;
  const n = fully.length;
  while (i < n) {
    const c = fully[i];
    if (c === '"') {
      blank(i);
      i++;
      while (i < n && fully[i] !== '"') {
        const step = fully[i] === '\\' ? 2 : 1;
        for (let k = 0; k < step && i + k < n; k++) blank(i + k);
        i += step;
      }
      if (i < n) {
        blank(i);
        i++;
      }
    } else if (
      c === 'r' &&
      !isIdent(fully[i - 1]) &&
      /["#]/.test(fully[i + 1] ?? '')
    ) {
      let hashes = 0;
      let j = i + 1;
      while (fully[j] === '#') {
        hashes++;
        j++;
      }
      if (fully[j] === '"') {
        const closing = '"' + '#'.repeat(hashes);
        let end = j + 1;
        while (end < n && fully.slice(end, end + closing.length) !== closing) {
          end++;
        }
        end = Math.min(end + closing.length, n);
        for (let k = i; k < end; k++) blank(k);
        i = end;
      } else {
        i++;
      }
    } else {
      i++;
    }
  }
  return out.join('');
}

/** Find the end of a `mod NAME { … }` body opening at `openBrace`. */
function findModBodyEnd(text, openBrace) {
  let i = openBrace + 1;
  let depth = 0;
  while (i < text.length) {
    const c = text[i];
    if (c === '{') {
      depth++;
    } else if (c === '}') {
      if (depth === 0) return i + 1;
      depth--;
    }
    i++;
  }
  return text.length; // unterminated — be lenient
}

/**
 * Strip `#[cfg(test)] mod … { … }` blocks AND `#[cfg(test)] fn … { … }`
 * bodies (newlines preserved so line numbers stay aligned). Returns the
 * stripped text plus the names of external test modules (`#[cfg(test)]
 * mod name;`) whose files must be excluded from the scan entirely.
 */
function stripTestModules(rawText) {
  const externalMods = [];
  let masked = maskCommentsAndStrings(rawText);
  let text = rawText;
  // `#[cfg(test)] (mod|fn) …` / `#[cfg(all(test, …))]` / `#[cfg_attr(…, test)]`
  const cfgTestRe =
    /#\[[^\n]*\b(?:cfg\(\s*(?:all\(\s*)?test|cfg_attr\([^)]*,\s*test)\b[^\n]*\]\s*\n?\s*(?:mod|fn)\s+(\w+)/g;
  let match;
  while ((match = cfgTestRe.exec(masked)) !== null) {
    const rest = masked.slice(match.index + match[0].length);
    if (/^\s*;/.test(rest)) {
      // `#[cfg(test)] mod name;` — external file (name.rs or name/mod.rs)
      externalMods.push(match[1]);
    } else {
      // The block body: for `mod` it opens right after the name; for
      // `fn` it opens after the (brace-free) parameter list and return
      // type — in both cases the FIRST `{` after the signature is the
      // body's opening brace.
      const rel = rest.search(/\{/);
      if (rel === -1) continue;
      const openBrace = match.index + match[0].length + rel;
      const end = findModBodyEnd(masked, openBrace);
      const cut = (t) => {
        const newlines = (t.slice(match.index, end).match(/\n/g) ?? []).length;
        return t.slice(0, match.index) + '\n'.repeat(newlines) + t.slice(end);
      };
      masked = cut(masked);
      text = cut(text);
      cfgTestRe.lastIndex = match.index;
    }
  }
  return { strippedText: text, externalMods };
}

/**
 * Blank the `VISION_MARKERS` array span (the image pipeline's vision
 * capability heuristic — the T1-review-I-2 sanctioned exclusion). Returns
 * the text with the array contents blanked; a no-op for other files. The
 * bracket that opens is the one after the `=` of the const declaration —
 * NOT the `&[&str]` type slice earlier on the line.
 */
function maskVisionMarkers(text) {
  const decl = text.match(/\bconst\s+VISION_MARKERS\b[^=]*=/);
  if (!decl) return text;
  const openBracket = text.indexOf('[', decl.index + decl[0].length);
  if (openBracket === -1) return text;
  let depth = 0;
  for (let i = openBracket; i < text.length; i++) {
    if (text[i] === '[') depth++;
    if (text[i] === ']') {
      depth--;
      if (depth === 0) {
        const out = text.split('');
        for (let k = openBracket; k <= i; k++) {
          if (out[k] !== '\n') out[k] = ' ';
        }
        return out.join('');
      }
    }
  }
  return text;
}

function listRustFiles(dir, externalModNames, dirModNames) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return []; // missing dir (crate without a src/) — nothing to scan
  }
  const out = [];
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...listRustFiles(path, externalModNames, dirModNames));
    } else if (entry.isFile() && entry.name.endsWith('.rs')) {
      const base = entry.name.replace(/\.rs$/, '');
      const isTestFile =
        base === 'tests' ||
        base.startsWith('test_') ||
        externalModNames.has(base) ||
        (dirModNames.has(path) && base === 'mod');
      const isFixture = /(^|\/)(fixtures|testdata)(\/|$)/.test(path);
      if (!isTestFile && !isFixture) {
        out.push(path);
      }
    }
  }
  return out;
}

// --- scan ------------------------------------------------------------------

const violations = [];
let scannedFiles = 0;
let strippedInlineBlocks = 0;
let excludedTestFiles = 0;

const cratesDir = join(cratesSrcDir);
for (const crate of readdirSync(cratesDir, { withFileTypes: true })) {
  if (!crate.isDirectory()) continue;
  const srcDir = join(cratesDir, crate.name, 'src');

  // First pass over this crate's .rs files: collect the external
  // `#[cfg(test)] mod name;` declarations so their target files (name.rs
  // or name/mod.rs next to the declaring file) are excluded.
  const externalModNames = new Set();
  const dirModNames = new Set();
  const allRsFiles = listRustFiles(srcDir, new Set(), new Set());
  for (const file of allRsFiles) {
    const { externalMods } = stripTestModules(readFileSync(file, 'utf8'));
    for (const name of externalMods) {
      externalModNames.add(name);
      dirModNames.add(join(dirname(file), `${name}.rs`));
      dirModNames.add(join(dirname(file), name, 'mod.rs'));
    }
  }

  // Second pass: the scan proper (listRustFiles tolerates crates without
  // a src/ dir by returning an empty list).
  const files = listRustFiles(srcDir, externalModNames, dirModNames);
  for (const file of files) {
    const raw = readFileSync(file, 'utf8');
    const { strippedText, externalMods } = stripTestModules(raw);
    strippedInlineBlocks += externalMods.length;
    if (strippedText !== raw) strippedInlineBlocks += 1; // at least one inline cut
    let masked = maskComments(strippedText);
    if (basename(file) === 'image_pipeline.rs') {
      masked = maskVisionMarkers(masked);
    }
    scannedFiles++;
    const lines = masked.split('\n');
    const rawLines = strippedText.split('\n');
    for (let n = 0; n < lines.length; n++) {
      for (const model of BANNED_MODELS) {
        if (model.re.test(lines[n])) {
          violations.push({
            file: file.replace(join(rustDir, ''), ''),
            line: n + 1,
            model: model.name,
            text: (rawLines[n] ?? '').trim(),
          });
        }
      }
    }
  }
  excludedTestFiles += allRsFiles.length - files.length;
}

if (violations.length > 0) {
  console.error(
    `model-hardcoding guard: ${violations.length} hardcoded model name(s) in production paths:`,
  );
  for (const v of violations) {
    console.error(`  ${v.file}:${v.line}  [${v.model}]  ${v.text}`);
  }
  console.error(
    '\nProduction code must not spell a concrete model name the user never',
  );
  console.error(
    'configured (spec 11 R4/A7). Resolve the alias through the config',
  );
  console.error('registry, or move the literal into a #[cfg(test)] fixture.');
  process.exit(1);
}

console.log(
  `model-hardcoding guard: clean — ${scannedFiles} production files scanned,` +
    ` 0 hits (${strippedInlineBlocks} inline test block group(s) stripped,` +
    ` ${excludedTestFiles} external/standalone test file(s) excluded).`,
);
