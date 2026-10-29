// apps/mirri-web/src/lib/inlineReferences.ts
// Pure TS — no Vue, no side effects. Inline skill/command reference helpers
// (skills domain US3.6 / ADR-004): word-start trigger-token extraction for the
// `/` and `$` menus, pre-submit reference validation, and the chip-split plan
// used by the Composer's overlay chips and the ChatPane user bubble.
//
// The link FORM itself (parsing, formatting, whitespace collapse) lives in the
// shared `@mirri-ai/protocol` package (parseInlineReferences & friends) — the
// web client and the v2 engine import the same single source, per ADR-004.

import {
  collapseInlineWhitespace,
  formatSkillReferenceLink,
  parseInlineReferences,
  SKILL_TRIGGER_CHARS,
  type InlineReference,
} from '@mirri-ai/protocol';

/** The `/` menu invocation char. */
export const SLASH_TRIGGER_CHAR = '/';

/**
 * Word characters for trigger scanning: anything that is NOT whitespace.
 * Mirrors the mention-menu token walk (a "word" ends at whitespace), which
 * also covers the full-width ideographic space (U+3000) via `\s`.
 */
const WORD_SPLIT = /\s/;

/**
 * Walk backwards from the caret to the start of the current word.
 * Returns the word's start offset (0 when the caret's text begins the string).
 */
function wordStartBefore(text: string, caret: number): number {
  let start = caret - 1;
  while (start >= 0 && !WORD_SPLIT.test(text[start]!)) start--;
  return start + 1;
}

export interface TriggerToken {
  /** Offset of the trigger char (word start). */
  start: number;
  /** Caret position (exclusive end of the token). */
  end: number;
  /** Text between the trigger char and the caret — the filter query. */
  query: string;
}

/**
 * Extract the trigger token under the caret for a single-char trigger family
 * (`$`/`¥`/`￥` or `/`). The trigger only fires at a WORD START — position 0 or
 * right after whitespace — so a `/` typed inside a path/URL or a `$` inside a
 * word never opens a menu (US3.6 anti-interruption rule). The query may not
 * contain whitespace (typing a space ends the trigger, the menu closes).
 */
function extractTriggerToken(
  text: string,
  caret: number,
  isTrigger: (ch: string) => boolean,
): TriggerToken | null {
  if (caret <= 0 || caret > text.length) return null;
  const start = wordStartBefore(text, caret);
  if (start >= caret) return null;
  if (!isTrigger(text[start]!)) return null;
  const query = text.slice(start + 1, caret);
  if (/\s/.test(query)) return null;
  return { start, end: caret, query };
}

/**
 * Skill-trigger token (`$`/`¥`/`￥`, US3.4) at the caret — word-start gated.
 */
export function extractSkillTriggerToken(text: string, caret: number): TriggerToken | null {
  return extractTriggerToken(text, caret, (ch) =>
    (SKILL_TRIGGER_CHARS as readonly string[]).includes(ch),
  );
}

/**
 * Slash-command token (`/`, US3.1) at the caret — word-start gated. Only the
 * trigger char itself differs from {@link extractSkillTriggerToken}; callers
 * then filter against the full slash item list (commands + skills).
 */
export function extractSlashTriggerToken(text: string, caret: number): TriggerToken | null {
  return extractTriggerToken(text, caret, (ch) => ch === SLASH_TRIGGER_CHAR);
}

/**
 * The inline reference inserted by the menus: a COMPACT token `[$name]` plus a
 * trailing space. The composer stores only the short token (a full link with
 * an absolute path is ~100 chars of invisible text under the chip — it wraps
 * the line, traps the caret, and truncates the chip); the submit path expands
 * every compact token back into the full `[$name](target)` link so the wire
 * format (ADR-004, history, engine parsing) is unchanged.
 */
export function buildInlineReferenceInsert(display: string, _target: string): string {
  return `[${SKILL_TRIGGER_CHARS[0]}${display}] `;
}

/** Match a compact token `[$name]` (trigger-prefixed bracket form, no target). */
const compactTokenRegex = /\[([$¥￥][^\]\n]+)\]/g;

export interface CompactToken {
  /** Display name without the trigger char. */
  display: string;
  start: number;
  end: number;
}

/**
 * Scan the composer text for compact tokens `[$name]`. These exist only while
 * composing — the submit path expands them into full reference links.
 */
export function parseCompactTokens(text: string): CompactToken[] {
  const tokens: CompactToken[] = [];
  for (const match of text.matchAll(compactTokenRegex)) {
    const [full, name] = match;
    if (full === undefined || name === undefined) continue;
    tokens.push({ display: name.slice(1), start: match.index, end: match.index + full.length });
  }
  return tokens;
}

/**
 * Expand every compact token into the full reference link, resolving the
 * target from the live skill list by NAME (case-insensitive). Command-looking
 * names (starting with `/`) expand to `command:<name>`; skills whose name is
 * absent from the list are left as-is (the pre-submit validation bar flags
 * them — the user removes or sends anyway, and an unresolved token then
 * degrades to visible text rather than a broken link).
 */
export function expandCompactTokens(
  text: string,
  skills: ReadonlyArray<{ name: string; path?: string }>,
): string {
  const byName = new Map(skills.map((s) => [s.name.toLowerCase(), s]));
  // Full links already carry their target — their `[$name]` head must NOT be
  // re-expanded (the compact regex matches inside `[$name](target)`).
  const linkSpans = parseInlineReferences(text).references.map((r) => [r.start, r.end] as const);
  let out = '';
  let cursor = 0;
  for (const token of parseCompactTokens(text)) {
    if (token.start < cursor) continue; // overlapping (shouldn't happen)
    if (linkSpans.some(([s, e]) => token.start >= s && token.end <= e)) continue; // inside a full link
    out += text.slice(cursor, token.start);
    const skill = byName.get(token.display.toLowerCase());
    if (skill?.path !== undefined) {
      out += formatSkillReferenceLink(token.display, skill.path);
    } else if (token.display.startsWith('/')) {
      out += formatSkillReferenceLink(token.display, `command:${token.display}`);
    } else {
      out += text.slice(token.start, token.end); // unresolved — leave visible
    }
    cursor = token.end;
  }
  return out + text.slice(cursor);
}

// ---------------------------------------------------------------------------
// Pre-submit validation (M4, US3.6 "失效引用不静默失败")
// ---------------------------------------------------------------------------

export type InvalidReferenceReason = 'missing' | 'disabled';

/** Build the lookup maps shared by the text-scan and node-attrs validators. */
function buildSkillLookupMaps(skills: ReadonlyArray<{ name: string; path?: string; enabled?: boolean }>): {
  pathMap: Map<string, { enabled?: boolean }>;
  nameMap: Map<string, { enabled?: boolean }>;
  hasPaths: boolean;
} {
  const pathMap = new Map<string, { enabled?: boolean }>();
  const nameMap = new Map<string, { enabled?: boolean }>();
  let hasPaths = false;
  for (const skill of skills) {
    if (skill.path) {
      hasPaths = true;
      pathMap.set(skill.path, { enabled: skill.enabled });
    }
    nameMap.set(skill.name.toLowerCase(), { enabled: skill.enabled });
  }
  return { pathMap, nameMap, hasPaths };
}

/**
 * Validate structural chip attrs (ADR-009): the pre-submit warning checks
 * ONLY the chips the user inserted (skillMention nodes) — pasted or typed
 * text never enters the warning path. Command chips never block (established
 * semantics); a targetless chip (legacy compact-token draft) resolves by
 * NAME; a chipped target resolves by path. Backends reporting no path on any
 * skill disable path checking entirely.
 */
export function findInvalidSkillMentionAttrs(
  attrs: ReadonlyArray<{ display: string; target: string; kind: 'skill' | 'command' }>,
  skills: ReadonlyArray<{ name: string; path?: string; enabled?: boolean }>,
): Array<{ attrs: { display: string; target: string; kind: 'skill' | 'command' }; reason: InvalidReferenceReason }> {
  const { pathMap, nameMap, hasPaths } = buildSkillLookupMaps(skills);
  if (!hasPaths) return []; // path authority unavailable — nothing to check
  const issues: Array<{ attrs: { display: string; target: string; kind: 'skill' | 'command' }; reason: InvalidReferenceReason }> = [];
  for (const a of attrs) {
    if (a.kind === 'command') continue; // commands never block the send
    const hit = a.target ? pathMap.get(a.target) : nameMap.get(a.display.toLowerCase());
    if (hit === undefined) issues.push({ attrs: a, reason: 'missing' });
    else if (hit.enabled === false) issues.push({ attrs: a, reason: 'disabled' });
  }
  return issues;
}

// ---------------------------------------------------------------------------
// Chip split plan (M3) — shared by the Composer overlay and the user bubble
// ---------------------------------------------------------------------------

export type InlineSegment =
  | { kind: 'plain'; text: string }
  | {
      kind: 'skill' | 'command';
      display: string;
      target: string;
      start: number;
      end: number;
    }
  // E9 user-bubble echo: a serialized file/folder reference token (`@path`).
  // `refKind` tells the chip which glyph to paint and whether it opens the
  // file preview (file) or is inert (folder); `target` is the raw path (the
  // fileRef regex fallback form carries no metadata beyond the path).
  | {
      kind: 'fileRef';
      refKind: 'file' | 'folder';
      display: string;
      target: string;
      start: number;
      end: number;
      /** M4 (US9.4): metadata-driven chips know the engine truncated the
       *  file at its line budget — the chip paints the「已截取」mark. Absent
       *  on regex-fallback chips (no metadata source). */
      truncated?: boolean;
    };

// E9 file-reference echo: recognize the serialized `@path` wire text of a
// Composer fileMention chip so the user bubble re-renders it as a chip
// (US9.10). FOLDER first — a folder mention ends with `/` followed by
// whitespace/EOL; a file mention is any whitespace-free token. Both require a
// word-start `@` (position 0 or after whitespace), so emails
// (`user@host.com`) never match. Quote characters are excluded from the
// capture, so the quoted hand-typed form `@"a b"` degrades to plain text
// (a mention-menu path never contains quotes — quoting is not a round-trip
// form of the chip).
const folderMentionRegex = /(?:^|\s)@([^\s@"]+\/)(?=\s|$)/g;
const fileMentionRegex = /(?:^|\s)@([^\s@"]+)/g;

/**
 * Scan the serialized wire text for file/folder reference tokens (`@path`,
 * folders with a trailing `/` — the form serializeDocJsonWithFileMentions
 * emits). Returns chip spans for the E9 user-bubble echo.
 *
 * `metaRefs` (M5): when the message carries the send-time reference metadata,
 * a token renders as a chip ONLY if its path is in the metadata — a
 * hand-typed `@path` that was never chipped stays plain text. A metadata
 * folder ref also matches its trailing-slash spelling.
 */
function parseFileRefMentions(
  text: string,
  metaRefs?: ReadonlyArray<{ path: string; kind: 'file' | 'folder'; truncated?: boolean }>,
): Array<{
  kind: 'fileRef';
  refKind: 'file' | 'folder';
  display: string;
  target: string;
  start: number;
  end: number;
  truncated?: boolean;
}> {
  const spans: Array<{
    kind: 'fileRef';
    refKind: 'file' | 'folder';
    display: string;
    target: string;
    start: number;
    end: number;
    truncated?: boolean;
  }> = [];
  const seen = new Set<string>();
  const taken: Array<[number, number]> = [];
  // Metadata lookups: a token is a chip only when its path was chipped.
  // Folders are keyed by their trailing-slash wire spelling too, so both
  // the bare and the slashed form of the token resolve.
  const metaByPath = new Map<string, { kind: 'file' | 'folder'; truncated?: boolean }>();
  for (const ref of metaRefs ?? []) {
    metaByPath.set(ref.path, { kind: ref.kind, ...(ref.truncated ? { truncated: true } : undefined) });
    if (ref.kind === 'folder') metaByPath.set(`${ref.path}/`, { kind: 'folder' });
  }
  // Metadata verdict for a token path. When NO metadata exists (regex
  // fallback, reloaded history) every token is allowed with no truncated
  // state; with metadata, only chipped paths resolve — anything else is
  // hand-typed text that must stay plain.
  const metaAllows = (path: string): { kind: 'file' | 'folder'; truncated?: boolean } | undefined =>
    metaRefs === undefined ? { kind: 'file' } : metaByPath.get(path);
  // Folder pass first: `@src/utils/` must win over the file token that would
  // otherwise swallow the trailing slash.
  for (const match of text.matchAll(folderMentionRegex)) {
    const path = match[1]!;
    const meta = metaAllows(path);
    if (meta === undefined) continue; // hand-typed — not a chipped reference
    const at = match.index + match[0].length - 1 - path.length;
    spans.push({ kind: 'fileRef', refKind: 'folder', display: path, target: path, start: at, end: at + 1 + path.length, ...(meta.truncated ? { truncated: true } : undefined) });
    taken.push([at, at + 1 + path.length]);
    seen.add(path);
  }
  for (const match of text.matchAll(fileMentionRegex)) {
    const path = match[1]!;
    const at = match.index + match[0].length - 1 - path.length;
    if (taken.some(([s, e]) => at >= s && at < e)) continue; // inside a folder span
    if (seen.has(path)) continue; // same path already chip'd
    const meta = metaAllows(path);
    if (meta === undefined) continue; // hand-typed — not a chipped reference
    spans.push({ kind: 'fileRef', refKind: 'file', display: path, target: path, start: at, end: at + 1 + path.length, ...(meta.truncated ? { truncated: true } : undefined) });
  }
  return spans;
}

/** Bare-name marker left by the engine's activation rewrite (ADR-006). */
const activatedMarkerRegex = /\[([^\]\n]+)\]/g;

/** One activated skill recorded on the rewritten body message (ADR-006). */
export interface ActivatedSkillRef {
  readonly name: string;
  readonly target: string;
  readonly trigger: string;
}

/**
 * Split a message text into plain-text segments and reference segments in
 * order of appearance. Accepts BOTH forms: compact tokens `[$name]` (the
 * composer's current form) and full links `[$name](target)` (persisted
 * history / replayed messages). When `withSkills` is given (a message
 * rewritten by the engine's activation pass, ADR-006), bare-name markers
 * `[name]` matching an activated skill rebuild as skill chips; full links and
 * compact tokens take precedence (they carry their own target). The user
 * bubble renders this verbatim (never through Markdown — US3.6: plain parts
 * keep pre-wrap, reference spans render as chips); the Composer overlay mirror
 * uses the same plan to paint chips.
 *
 * `fileRefs` (M5, US9.10): the send-time reference metadata of the optimistic
 * echo. When present, `@path` tokens render as chips ONLY for paths in the
 * metadata (a hand-typed `@path` stays plain text) and truncated chips carry
 * the M4 mark. Absent (reloaded history) — every well-formed `@path` token
 * renders as a chip (regex fallback, previous behavior).
 */
export function splitMessageInlineReferences(
  text: string,
  withSkills?: readonly ActivatedSkillRef[],
  fileRefs?: ReadonlyArray<{ path: string; kind: 'file' | 'folder'; truncated?: boolean }>,
): InlineSegment[] {
  const segments: InlineSegment[] = [];
  let cursor = 0;
  const emit = (seg: InlineSegment): void => {
    segments.push(seg);
  };
  // Full links first (authoritative spans), then compact tokens outside them.
  const links = parseInlineReferences(text).references.map((r) => ({
    kind: r.kind,
    display: r.display,
    target: r.target,
    start: r.start,
    end: r.end,
  }));
  const tokens = parseCompactTokens(text)
    .filter((t) => !links.some((l) => t.start >= l.start && t.end <= l.end))
    .map((t) => ({
      kind: 'skill' as const,
      display: t.display,
      target: '', // compact: resolved at submit-time expansion
      start: t.start,
      end: t.end,
    }));
  // ADR-006: bare-name markers of activated skills rebuild as chips. Full
  // links and compact tokens take precedence (they carry their own target).
  const markers = withSkills
    ? [...text.matchAll(activatedMarkerRegex)]
        .filter(
          (m) =>
            !links.some((l) => m.index >= l.start && m.index + m[0].length <= l.end) &&
            !tokens.some((t) => m.index >= t.start && m.index + m[0].length <= t.end),
        )
        .filter((m) => withSkills.some((s) => s.name === (m[1] ?? '')))
        .map((m) => {
          const skill = withSkills.find((s) => s.name === (m[1] ?? ''))!;
          return {
            kind: 'skill' as const,
            display: m[1] ?? '',
            target: skill.target,
            start: m.index,
            end: m.index + m[0].length,
          };
        })
    : [];
  // E9: serialized file-reference tokens are also chips. With send-time
  // metadata (M5) only chipped paths become chips; without it (reloaded
  // history) every well-formed token does (regex fallback). Skill links are
  // authoritative spans — a fileRef match inside one (can't happen with the
  // whitespace-gated `@`, but guards the ordering) is dropped.
  const fileRefSpans = parseFileRefMentions(text, fileRefs).filter(
    (f) => !links.some((l) => f.start >= l.start && f.end <= l.end),
  );
  const all = [...links, ...tokens, ...markers, ...fileRefSpans].toSorted((a, b) => a.start - b.start);
  for (const ref of all) {
    if (ref.start > cursor) {
      emit({ kind: 'plain', text: text.slice(cursor, ref.start) });
    }
    emit(ref);
    cursor = ref.end;
  }
  if (cursor < text.length) {
    emit({ kind: 'plain', text: text.slice(cursor) });
  }
  return segments;
}

/**
 * The reference link span containing `caret`, or null when the caret is outside
 * every reference. Used by the Composer overlay for the chip-degrade rule: a
 * caret inside a link marks the chip as "being edited" so it can drop to plain
 * link styling (the re-parse simply stops returning the span when the link
 * text is altered — no error, per ADR-004 decision 5).
 */
export function extractInlineReferenceSpan(
  text: string,
  caret: number,
): InlineReference | null {
  return (
    parseInlineReferences(text).references.find(
      (reference) => caret > reference.start && caret < reference.end,
    ) ?? null
  );
}

/**
 * True when the text contains at least one activatable inline reference — the
 * Composer's "mixed message" test. Mixed messages go through the ordinary
 * submit path (payload text = raw text with links); only a message that is a
 * PURE command (starts with `/` and parses to a known command) takes the
 * command-intercept branch, which a mixed message never hits. Compact tokens
 * (`[$name]`, the composer form) count as references too.
 */
export function hasInlineReferences(text: string): boolean {
  return (
    parseInlineReferences(text).references.length > 0 || parseCompactTokens(text).length > 0
  );
}

/**
 * Normalize a message before send (M4): expand compact tokens to full links
 * (resolved from the live skill list) and collapse the whitespace between
 * adjacent references to a single separator space. The WIRE format is always
 * the full link (ADR-004) — the compact token is a composer-only ergonomics
 * form; engines and persisted history never see it.
 */
export function normalizeInlineReferencesForSubmit(
  text: string,
  skills: ReadonlyArray<{ name: string; path?: string }> = [],
): string {
  return collapseInlineWhitespace(expandCompactTokens(text, skills));
}
