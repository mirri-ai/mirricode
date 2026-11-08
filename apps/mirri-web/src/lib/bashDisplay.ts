// apps/mirri-web/src/lib/bashDisplay.ts
// Display-only normalization of a Bash tool call's command for the collapsed
// tool-call row. Nothing here changes what the engine runs, what is stored, or
// what the expanded body / copy action shows — the original command text is
// always the source of truth and stays reachable verbatim.
//
// Why this exists: models habitually prefix commands with
// `cd <absolute path> &&` even though Bash already runs in the session's
// working directory (`rust/crates/mirri-core/src/tools/bash.rs` — "The command
// runs in the session's working directory unless 'cwd' is given"). Rendered
// verbatim in a one-line summary, that prefix is pure noise: it is the same on
// every row, so a group of consecutive calls looks identical and the part that
// actually distinguishes them is pushed off the right edge.
//
// So the prefix is collapsed:
//   - path === the session workspace  → a workspace badge (folder + dir name);
//     the command reads from its first real token.
//   - <workspace>/.worktrees/<name>   → a worktree badge (branch glyph + the
//     checkout name) — the conventional in-repo worktree directory.
//   - any other directory inside the workspace → a subdir badge showing the
//     workspace-relative path.
//   - any other absolute/`~` directory → `cd …/a_folder/ &&`, last segment kept.
//   - anything else (relative paths, a mid-command `cd`, subshells) → untouched,
//     because compressing those would misreport where the command runs.
//   - a trailing `# comment` is stripped from the collapsed text only (the
//     expanded body / copy path keep it verbatim).
//
// Judgement is deliberately strict: a non-matching directory is never shown as
// the workspace badge. Showing one path segment too many is a cosmetic cost;
// showing the wrong directory is a correctness bug.

/** The `cd` prefix split off a Bash command, for row rendering. */
export interface BashPrefix {
  /** `workspace`: exactly the session's working directory (badge).
   *  `worktree`: `<root>/.worktrees/<name>` — an isolated checkout inside the
   *    workspace (community convention, not a git standard); the label is the
   *    checkout name, usually the branch.
   *  `subdir`: another directory inside the workspace; the label is the path
   *    relative to the workspace root.
   *  `other`: a directory outside the workspace (compressed `…/dir`). */
  kind: 'workspace' | 'worktree' | 'subdir' | 'other';
  /** Directory name / relative path for the row, e.g. `mirricode`. */
  label: string;
  /** The directory exactly as the model wrote it, quotes stripped. */
  path: string;
}

/** A Bash command split into an optional directory prefix + the command text. */
export interface BashCommandDisplay {
  prefix?: BashPrefix;
  /** Command text after the prefix (clipped for the collapsed row). */
  text: string;
}

/** Visible width of a collapsed Bash summary. Wide enough that the `cd` prefix
    is no longer the whole row, short enough to stay one line in the reading
    column. */
export const BASH_SUMMARY_MAX = 160;
/** Share of the summary budget kept at the head — the verb (`pnpm`, `git`) and
    its subcommand live there and carry the most meaning. */
const HEAD_SHARE = 0.6;

// `cd [|pushd] <path>` optionally followed by `&& | || | ; | newline` and the
// rest of the command. `[^"]` / `[^']` covers quoted paths with spaces; the
// bare branch stops at shell metacharacters so `cd /a && b` cannot swallow the
// separator. A bare newline is a command separator in every shell we run
// (Unix `sh -c` and Windows Git Bash `bash -c` both treat it as `;`), so
// `\r?\n` is accepted too — CRLF from Windows-authored command strings
// normalizes the same way. Anchored end-to-end: a `cd` that is not the whole
// head (a subshell, a mid-command `cd`) is not matched at all, which is the
// intent.
const CD_PREFIX =
  /^(?:cd|pushd)\s+(?:"([^"]*)"|'([^']*)'|([^\s&;|<>()]+))(?:\s*(?:&&|\|\||;|\r?\n)\s*([\s\S]*))?$/;

/** Trim, collapse internal whitespace (a multi-line command must stay on one
    row), and clip to `max` by keeping BOTH ends.
    The tail matters as much as the head here: for Bash the distinguishing
    detail (a filter, a path, an argument) usually sits at the end. */
export function clipBashSummary(text: string, max: number = BASH_SUMMARY_MAX): string {
  const s = text.replaceAll(/\s+/g, ' ').trim();
  if (s.length <= max) return s;
  const head = Math.ceil((max - 1) * HEAD_SHARE);
  const tail = max - 1 - head;
  return `${s.slice(0, head)}…${s.slice(s.length - tail)}`;
}

/** Strip a trailing shell `# comment` from a command, for the collapsed row.
    Display-only: the expanded body / copy path keep the original text.
    Conservative by design:
    - `#` only counts as a comment start at the start of the string or after
      space/tab (so `foo#bar` survives) and outside quotes (so `"#42"` survives);
    - only stripped when nothing but the comment follows to the end of the
      string — a `#` with real commands on later lines is left alone, because
      cutting to the end of the string would eat them;
    - a command that is nothing but a comment is kept whole (the comment is
      the only content there is). */
function stripTrailingComment(text: string): string {
  let inSingle = false;
  let inDouble = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i]!;
    if (inSingle) {
      if (c === "'") inSingle = false;
      continue;
    }
    if (inDouble) {
      if (c === '\\') i += 1;
      else if (c === '"') inDouble = false;
      continue;
    }
    if (c === '\\') {
      i += 1;
      continue;
    }
    if (c === "'") inSingle = true;
    else if (c === '"') inDouble = true;
    else if (c === '#' && (i === 0 || text[i - 1] === ' ' || text[i - 1] === '\t')) {
      if (text.slice(i).includes('\n')) break;
      const stripped = text.slice(0, i).trimEnd();
      return stripped === '' ? text : stripped;
    }
  }
  return text;
}

/** Strip trailing slashes so `/a/b/` and `/a/b` compare equal. `.` stays `.`
    ("here"), the empty string stays empty. */
function normalizePath(p: string): string {
  const s = p.trim().replace(/\/+$/, '');
  return s === '' && p.trim() !== '' ? '/' : s;
}

/** Last path segment, or the whole string when it has none (`/`). */
function lastSegment(p: string): string {
  const parts = p.split('/').filter(Boolean);
  return parts.at(-1) ?? p;
}

/** Only absolute paths and `~/…` are compressed. A relative path (`..`,
    `build`) has no meaningful "last segment" to show and no way to prove it is
    the workspace, so it is left exactly as written. */
function isCollapsible(path: string): boolean {
  return path.startsWith('/') || path.startsWith('~/') || path === '~' || path === '.';
}

/** True when `path` provably IS the session workspace. Strict by design. */
function isWorkspace(path: string, workspaceRoot: string | undefined): boolean {
  const p = normalizePath(path);
  if (p === '.') return true;
  const root = workspaceRoot ? normalizePath(workspaceRoot) : '';
  if (!root || !root.startsWith('/')) return false;
  if (p === root) return true;
  // `~/x/y` is accepted only when it is an exact suffix of the known absolute
  // root (`/Users/me/x/y`) — that identifies the directory without guessing the
  // user's home path.
  if (p.startsWith('~/')) {
    const rest = p.slice(2);
    return rest.length > 0 && root.endsWith(`/${rest}`);
  }
  return false;
}

/** Classify a collapsible `cd` target against the session workspace.
    Strict by design: only provable matches get a specific badge — an
    in-workspace path becomes a `subdir` (shown relative to the root), the
    conventional `<root>/.worktrees/<name>` checkout becomes a `worktree`,
    everything else stays `other` with only its last segment. */
function classifyPrefix(path: string, workspaceRoot: string | undefined): BashPrefix {
  const p = normalizePath(path);
  if (isWorkspace(path, workspaceRoot)) {
    const root = workspaceRoot ? normalizePath(workspaceRoot) : '';
    return { kind: 'workspace', label: lastSegment(root || p), path };
  }
  const root = workspaceRoot ? normalizePath(workspaceRoot) : '';
  if (root && root.startsWith('/') && p.startsWith(`${root}/`)) {
    const rel = p.slice(root.length + 1);
    if (/^\.worktrees\/[^/]+$/.test(rel)) {
      return { kind: 'worktree', label: rel.slice('.worktrees/'.length), path };
    }
    return { kind: 'subdir', label: rel, path };
  }
  return { kind: 'other', label: lastSegment(p), path };
}

/**
 * Split a Bash command into `{ prefix, text }` for the collapsed row.
 *
 * @param full when true, return the command verbatim with no prefix — the
 *   expanded card body and the copy path must show exactly what ran.
 */
export function summarizeBashCommand(
  command: string,
  workspaceRoot: string | undefined,
  options: { full?: boolean; max?: number } = {},
): BashCommandDisplay {
  const raw = (command ?? '').trim();
  if (options.full) return { text: raw };

  const stripped = stripTrailingComment(raw);
  const m = CD_PREFIX.exec(stripped);
  if (!m) return { text: clipBashSummary(stripped, options.max) };

  const path = (m[1] ?? m[2] ?? m[3] ?? '').trim();
  if (!isCollapsible(path)) return { text: clipBashSummary(raw, options.max) };

  const rest = (m[4] ?? '').trim();
  const prefix = classifyPrefix(path, workspaceRoot);

  return { prefix, text: clipBashSummary(rest, options.max) };
}

/** The prefix as inline text, for summaries that cannot render a badge (the
    single-line digests built by `toolSummary`). The workspace badge form is
    only used by the tool row itself. */
export function bashPrefixText(prefix: BashPrefix): string {
  switch (prefix.kind) {
    case 'workspace':
      return `${prefix.label}/ && `;
    case 'worktree':
    case 'subdir':
      return `${prefix.label} && `;
    case 'other':
      return `…/${prefix.label}/ && `;
  }
}

/** Parse a tool `arg` into the Bash command it carries. `arg` is normally the
    JSON-stringified tool input; a plain string is accepted as the command. */
export function bashCommandOf(arg: string): string | undefined {
  const s = (arg ?? '').trim();
  if (!s) return undefined;
  if (!s.startsWith('{')) return s;
  try {
    const d: unknown = JSON.parse(s);
    if (!d || typeof d !== 'object' || Array.isArray(d)) return undefined;
    const r = d as Record<string, unknown>;
    const cmd = [r.command, r.cmd, r.script].find((v) => typeof v === 'string' && v.length > 0);
    return typeof cmd === 'string' ? cmd : undefined;
  } catch {
    return undefined;
  }
}

/** The remaining Bash input fields (everything but the command itself),
    pretty-printed — shown under the full command on hover so `cwd`, `timeout`,
    `run_in_background` etc. stay reachable without the JSON wrapper burying the
    command. */
export function bashExtraFieldsOf(arg: string): string | undefined {
  const s = (arg ?? '').trim();
  if (!s.startsWith('{')) return undefined;
  try {
    const d: unknown = JSON.parse(s);
    if (!d || typeof d !== 'object' || Array.isArray(d)) return undefined;
    const { command: _c, cmd: _cmd, script: _sc, ...rest } = d as Record<string, unknown>;
    if (Object.keys(rest).length === 0) return undefined;
    return JSON.stringify(rest, null, 2);
  } catch {
    return undefined;
  }
}
