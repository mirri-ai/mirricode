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
//   - any other absolute/`~` directory → `cd …/a_folder/ &&`, last segment kept.
//   - anything else (relative paths, a mid-command `cd`, subshells) → untouched,
//     because compressing those would misreport where the command runs.
//
// Judgement is deliberately strict: a non-matching directory is never shown as
// the workspace badge. Showing one path segment too many is a cosmetic cost;
// showing the wrong directory is a correctness bug.

/** The `cd` prefix split off a Bash command, for row rendering. */
export interface BashPrefix {
  /** `workspace`: the path is the session's working directory (badge).
   *  `other`: a different directory (compressed `cd …/dir/ &&` text). */
  kind: 'workspace' | 'other';
  /** Directory name for the row, e.g. `mirricode`. */
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

  const m = CD_PREFIX.exec(raw);
  if (!m) return { text: clipBashSummary(raw, options.max) };

  const path = (m[1] ?? m[2] ?? m[3] ?? '').trim();
  if (!isCollapsible(path)) return { text: clipBashSummary(raw, options.max) };

  const rest = (m[4] ?? '').trim();
  const root = workspaceRoot ? normalizePath(workspaceRoot) : '';
  const prefix: BashPrefix = isWorkspace(path, workspaceRoot)
    ? {
        kind: 'workspace',
        // The badge names the directory the session lives in; fall back to the
        // cd path itself when the workspace root is unknown.
        label: lastSegment(root || normalizePath(path)),
        path,
      }
    : { kind: 'other', label: lastSegment(normalizePath(path)), path };

  return { prefix, text: clipBashSummary(rest, options.max) };
}

/** The prefix as inline text, for summaries that cannot render a badge (the
    single-line digests built by `toolSummary`). The workspace badge form is
    only used by the tool row itself. */
export function bashPrefixText(prefix: BashPrefix): string {
  return prefix.kind === 'workspace' ? `${prefix.label}/ && ` : `…/${prefix.label}/ && `;
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
