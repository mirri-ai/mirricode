#!/usr/bin/env node
// Spawn hygiene gate (batch A2 / #182): production rust code must not call
// `Command::new` / `CommandBuilder::new` directly. Every production spawn
// must go through `mirri_os::new_command` / `mirri_os::new_tokio_command`
// so Windows children get CREATE_NO_WINDOW and Unix children get their own
// process group.
//
// Scope:
//   - `rust/crates/**/src/**/*.rs` (production) — the first `#[cfg(test)]`
//     `mod` boundary in a file separates production code (scanned) from
//     test code (exempt — test spawns never run on user machines and some
//     deliberately bypass the helper to verify group-kill semantics).
//   - `rust/crates/**/tests/**` (integration test dirs) — exempt entirely.
//
// The test-boundary detection reuses the lexical stripping approach of
// rust/scripts/check-zero-io.mjs: comments and string literals are masked
// before matching `#[cfg(test)] mod`, so decoy text cannot hide production
// code from the scan.
//
// Exemptions live in WHITELIST below and are anchored to marker comments:
// a listed file is exempt only at raw spawn sites carrying the
// `check-spawn-usage whitelist` marker within a few lines, so a call and its
// justification can move together without the gate losing track of them.
//
// Exit code 0 = clean, 1 = unabsorbed raw spawns found.

import { readFileSync, readdirSync } from "node:fs";
import { join, relative } from "node:path";

// Usage: node scripts/check-spawn-usage.mjs [--root <dir>]
// The default root is the repository this script lives in; --root points
// the scan at a different tree (used by this script's own tests). `--root`
// as the last argument is treated as absent.
const rootArgIdx = process.argv.indexOf("--root");
const rootArg =
  rootArgIdx !== -1 && process.argv[rootArgIdx + 1] !== undefined
    ? process.argv[rootArgIdx + 1]
    : undefined;
const ROOT = rootArg ? resolve(rootArg) : resolve(import.meta.dirname, "..");
const RUST_CRATES = join(ROOT, "rust", "crates");

// file (relative to repo root, posix separators) -> exemption scope.
// A plain entry exempts ONLY raw spawn sites that carry the
// `check-spawn-usage whitelist` marker comment within MARKER_WINDOW lines —
// the exemption travels with its justification when line numbers drift.
// `wholeFile: true` exempts every raw spawn in the file. Every entry needs
// a reason; a marker comment in a file with no whitelist entry exempts
// nothing (the marker alone is not a license).
const MARKER = "check-spawn-usage whitelist";
const MARKER_WINDOW = 3;

const WHITELIST = [
  {
    file: "rust/crates/mirri-os/src/process.rs",
    wholeFile: true,
    reason:
      "the spawn-hygiene helpers themselves construct the raw Command",
  },
  {
    file: "rust/crates/mirri-os/src/pty.rs",
    reason:
      "portable-pty CommandBuilder spawns through the pty slave, not std Command",
  },
];

// Raw spawn constructors banned in production code. The lookbehind blocks
// embedded matches inside longer identifiers (`ForegroundShellCommand::new`)
// but allows the qualified forms this gate targets (`std::process::Command::new`).
const BANNED = [
  { name: "Command::new", re: /(?<!\w)Command::new\s*\(/ },
  { name: "CommandBuilder::new", re: /(?<!\w)CommandBuilder::new\s*\(/ },
];

function resolve(...parts) {
  // local stand-in for node:path.resolve to keep imports minimal
  let p = parts.join("/");
  return p;
}

function listRustFiles(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      listRustFiles(path, out);
    } else if (entry.isFile() && entry.name.endsWith(".rs")) {
      out.push(path);
    }
  }
  return out;
}

/**
 * Replace the content of comments and string literals with equal-length
 * whitespace (newlines preserved), so decoy `#[cfg(test)]` text inside a
 * comment/string cannot move the test boundary and prose mentioning
 * `Command::new` cannot trip the scan. Mirrors the state machine in
 * rust/scripts/check-zero-io.mjs.
 */
function maskCommentsAndStrings(text) {
  const out = text.split("");
  const isIdent = (c) => /[\w$]/.test(c ?? " ");
  const blank = (i) => {
    if (out[i] !== "\n") out[i] = " ";
  };
  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i];
    if (c === "/" && text[i + 1] === "/") {
      while (i < n && text[i] !== "\n") {
        blank(i);
        i++;
      }
    } else if (c === "/" && text[i + 1] === "*") {
      let depth = 0;
      while (i < n) {
        if (text[i] === "/" && text[i + 1] === "*") {
          blank(i);
          blank(i + 1);
          i += 2;
          depth++;
        } else if (text[i] === "*" && text[i + 1] === "/") {
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
      blank(i);
      i++;
      while (i < n && text[i] !== '"') {
        const step = text[i] === "\\" ? 2 : 1;
        for (let k = 0; k < step && i + k < n; k++) blank(i + k);
        i += step;
      }
      if (i < n) {
        blank(i);
        i++;
      }
    } else if (
      c === "r" &&
      (!isIdent(text[i - 1]) || (text[i - 1] === "b" && !isIdent(text[i - 2]))) &&
      /["#]/.test(text[i + 1] ?? "")
    ) {
      let hashes = 0;
      let j = i + 1;
      while (text[j] === "#") {
        hashes++;
        j++;
      }
      if (text[j] === '"') {
        const closing = '"' + "#".repeat(hashes);
        let end = j + 1;
        while (end < n && text.slice(end, end + closing.length) !== closing) end++;
        end = Math.min(end + closing.length, n);
        for (let k = i; k < end; k++) blank(k);
        i = end;
      } else {
        i++;
      }
    } else if (c === "'") {
      const rest = text.slice(i, i + 16);
      const m = rest.match(/^'(\\u\{[0-9a-fA-F_]{1,6}\}|\\.|[^\\'])'/);
      if (m) {
        for (let k = 0; k < m[0].length; k++) blank(i + k);
        i += m[0].length;
      } else {
        i++;
      }
    } else {
      i++;
    }
  }
  return out.join("");
}

/**
 * Find the end of a `{ ... }` body that opens at `openBrace`, skipping
 * braces inside strings / char literals / comments.
 */
function findBlockEnd(text, openBrace) {
  let i = openBrace + 1;
  while (i < text.length) {
    const c = text[i];
    if (c === "/" && text[i + 1] === "/") {
      while (i < text.length && text[i] !== "\n") i++;
    } else if (c === "/" && text[i + 1] === "*") {
      i += 2;
      while (i < text.length && !(text[i] === "*" && text[i + 1] === "/")) i++;
      i += 2;
    } else if (c === '"') {
      i++;
      while (i < text.length && text[i] !== '"') i += text[i] === "\\" ? 2 : 1;
      i++;
    } else if (c === "'") {
      const rest = text.slice(i, i + 16);
      const m = rest.match(/^'(\\u\{[0-9a-fA-F_]{1,6}\}|\\.|[^\\'])'/);
      if (m) i += m[0].length;
      else i++;
    } else if (c === "{") {
      i = findBlockEnd(text, i);
    } else if (c === "}") {
      return i + 1;
    } else {
      i++;
    }
  }
  return text.length;
}

/**
 * Blank every line that belongs to a `#[cfg(test)] mod ... { ... }` block
 * (line numbers preserved), returning the masked text. `#[cfg(all(test,
// ...))]` shapes are covered too. `#[cfg(test)] mod name;` external-module
 * declarations are NOT followed into their files here — integration test
// dirs are exempted separately and unit-test external files are rare;
// if one appears, the whitelist is the escape hatch.
 */
function blankTestModules(text) {
  let masked = maskCommentsAndStrings(text);
  const cfgTestRe =
    /#\[[^\n]*\b(?:cfg\(\s*(?:all\(\s*)?test|cfg_attr\([^)]*,\s*test)\b[^\n]*\]\s*\n?\s*mod\s+(\w+)/g;
  let m;
  while ((m = cfgTestRe.exec(masked)) !== null) {
    const afterName = masked.slice(m.index + m[0].length);
    if (/^\s*\{/.test(afterName)) {
      const openBrace = m.index + m[0].length + afterName.match(/^\s*\{/)[0].length - 1;
      const end = findBlockEnd(masked, openBrace);
      const cut = (t) => {
        const newlines = (t.slice(m.index, end).match(/\n/g) ?? []).length;
        return t.slice(0, m.index) + "\n".repeat(newlines) + t.slice(end);
      };
      masked = cut(masked);
      cfgTestRe.lastIndex = m.index;
    }
  }
  return masked;
}

/**
 * Whether a raw call at `lineIdx` (0-based) has the whitelist marker
 * comment within MARKER_WINDOW lines. Searches the raw (unmasked) text:
 * the marker lives in a comment, which masking would blank out.
 */
function hasMarkerNearby(rawLines, lineIdx) {
  const from = Math.max(0, lineIdx - MARKER_WINDOW);
  const to = Math.min(rawLines.length, lineIdx + MARKER_WINDOW + 1);
  for (let k = from; k < to; k++) {
    if (rawLines[k].includes(MARKER)) return true;
  }
  return false;
}

let violations = 0;
for (const file of listRustFiles(RUST_CRATES)) {
  const rel = relative(ROOT, file).split("\\").join("/");
  // Integration / unit test dirs are exempt entirely.
  if (/(^|\/)tests\//.test(rel)) continue;

  const entry = WHITELIST.find((w) => w.file === rel);
  const exemptAll = entry?.wholeFile === true;

  const raw = readFileSync(file, "utf8");
  const masked = blankTestModules(raw);
  const maskedLines = masked.split("\n");
  const rawLines = raw.split("\n");

  for (let n = 0; n < maskedLines.length; n++) {
    for (const { name, re } of BANNED) {
      if (!re.test(maskedLines[n])) continue;
      if (exemptAll || (entry && hasMarkerNearby(rawLines, n))) continue;
      violations++;
      console.error(
        `${rel}:${n + 1}: raw ${name} in production code — use mirri_os::new_command / new_tokio_command`,
      );
      console.error(`    ${(rawLines[n] ?? "").trim()}`);
    }
  }
}

if (violations > 0) {
  console.error(`\ncheck-spawn-usage: ${violations} raw production spawn(s) found.`);
  console.error(
    "Production spawns must go through mirri_os::new_command / new_tokio_command " +
      "(Windows CREATE_NO_WINDOW, Unix process group). " +
      "If a site genuinely cannot, add a whitelist entry in scripts/check-spawn-usage.mjs " +
      "with a reason AND a `check-spawn-usage whitelist` marker comment within 3 lines of the call.",
  );
  process.exit(1);
}
console.log("check-spawn-usage: clean");
