/**
 * Inline skill/command reference links (glossary SK7, skills domain US3.6 / ADR-004).
 *
 * A message body is the single source of truth for inline activation: a
 * reference is an inline markdown link whose link text starts with a skill
 * trigger symbol (`$` and full-width variants) and whose target is either a
 * `SKILL.md` file path (skill), a `builtin://` pseudo path (builtin skill),
 * or a `command:<name>` pseudo-target (command). Plain markdown links never
 * activate; the trigger prefix is the machine-checkable part of the
 * activation judgment.
 *
 * Pure functions only — shared verbatim between the web client (chip overlay
 * rendering, pre-send validation) and the v2 engine (submit-entry parsing).
 * Server-side parsing is authoritative; client parsing is a UX pre-pass.
 */

/** Trigger symbols that mark an inline link as an activatable reference. */
export const SKILL_TRIGGER_CHARS = ['$', '¥', '￥'] as const;

/** Pseudo-target prefix marking an inline reference as a slash command. */
export const COMMAND_TARGET_PREFIX = 'command:';

/** Pseudo-target prefix identifying a builtin skill reference (ADR-007 D4). */
export const BUILTIN_TARGET_PREFIX = 'builtin://';

/** The stale `/SKILL.md` suffix historical messages carry on builtin targets. */
const BUILTIN_STALE_SUFFIX = /\/SKILL\.md$/i;

export type InlineReferenceKind = 'skill' | 'command';

export interface InlineReference {
  /** Reference kind: skill (SKILL.md target) or command (command: target). */
  kind: InlineReferenceKind;
  /** Display name without the trigger char (e.g. `alpha` for `[$alpha]`). */
  display: string;
  /**
   * Skill: absolute path to the skill's SKILL.md, or the `builtin://<name>`
   * pseudo path (the resolution authority — which file the link points at is
   * which skill gets activated; builtin targets are normalized to the clean
   * `builtin://` form). Command: `command:<canonical command name>`.
   */
  target: string;
  /** Start offset of the full link in the original text. */
  start: number;
  /** End offset (exclusive) of the full link in the original text. */
  end: number;
}

export interface ParsedInlineReferences {
  /** Text with each reference replaced by its bare display name. */
  body: string;
  /** References in order of appearance. */
  references: InlineReference[];
}

const triggerAlternation = SKILL_TRIGGER_CHARS.map((c) => c.replaceAll(/[$¥￥]/g, (m) => `\\${m}`)).join('');
// `[$name](target)` — link text MUST start with a trigger char; target is a
// non-empty, non-whitespace run without `)` (markdown link target shape).
const referenceRegex = new RegExp(
  `\\[([${triggerAlternation}][^\\]\\n]*)\\]\\(([^)\\s]+)\\)`,
  'g',
);

function escapeForDisplay(name: string): string {
  return name.replaceAll(/[[\]]/g, (m) => `\\${m}`);
}

/**
 * Format a reference as the canonical inline link form. Inverse of the
 * parsing step for a single reference.
 */
export function formatSkillReferenceLink(display: string, target: string): string {
  const trigger = SKILL_TRIGGER_CHARS[0];
  const text = escapeForDisplay(display);
  return `[${trigger}${text}](${target})`;
}

function isCommandTarget(target: string): boolean {
  return target.startsWith(COMMAND_TARGET_PREFIX);
}

/**
 * A target is a skill reference when it is a SKILL.md file path (case-
 * insensitive extension, the historical authority) OR a `builtin://` pseudo
 * path (the builtin chip target form, ADR-007 D4).
 */
function isSkillTarget(target: string): boolean {
  return (
    target.startsWith(BUILTIN_TARGET_PREFIX) || target.toLowerCase().endsWith('skill.md')
  );
}

/**
 * Normalize a builtin target: strip the stale `/SKILL.md` suffix (case-
 * insensitive) that pre-fix insertion sites persisted on `builtin://` pseudo
 * paths, so classification and resolution see the clean `builtin://name`
 * form. Non-builtin targets pass through untouched. Only the exact trailing
 * suffix strips — legitimate slash-shaped builtin names (e.g. the sub-skill
 * family's `builtin://sub-skill/review`) survive verbatim.
 */
function normalizeBuiltinTarget(target: string): string {
  if (!target.startsWith(BUILTIN_TARGET_PREFIX)) return target;
  return target.replace(BUILTIN_STALE_SUFFIX, '');
}

/**
 * Parse all activatable inline references from a message text.
 *
 * Links without a trigger prefix are left untouched (plain markdown). A
 * trigger link whose target is neither a SKILL.md path nor a `command:`
 * pseudo-target is also left untouched (invalid target = not activatable).
 */
export function parseInlineReferences(text: string): ParsedInlineReferences {
  const references: InlineReference[] = [];
  const replacements: Array<{ start: number; end: number; display: string }> = [];

  for (const match of text.matchAll(referenceRegex)) {
    const [full, linkText, target] = match;
    const trigger = linkText;
    if (full === undefined || trigger === undefined || target === undefined) continue;
    const start = match.index;
    const end = start + full.length;
    const display = trigger.slice(1).trim();
    if (!display) continue;

    const kind: InlineReferenceKind | undefined = isCommandTarget(target)
      ? 'command'
      : isSkillTarget(target)
        ? 'skill'
        : undefined;
    if (!kind) continue;

    // Builtin targets normalize to the clean `builtin://name` form (the
    // stale `/SKILL.md` suffix of historical messages strips here, so
    // `parsed.target` and downstream resolution never see the dirty shape).
    const normalizedTarget = normalizeBuiltinTarget(target);
    references.push({ kind, display, target: normalizedTarget, start, end });
    // Body replacement keeps the bracketed form `[name]`: readable plain text,
    // and re-parsing the body yields a trigger-less link (idempotent).
    replacements.push({ start, end, display: `[${display}]` });
  }

  let body = '';
  let cursor = 0;
  for (const { start, end, display } of replacements) {
    body += text.slice(cursor, start) + display;
    cursor = end;
  }
  body += text.slice(cursor);

  return { body, references };
}

/**
 * Collapse consecutive whitespace *between two adjacent references* into a
 * single separator space (cosmetic normalization only — never touches spaces
 * elsewhere in the text). Also trims leading/trailing whitespace.
 */
export function collapseInlineWhitespace(text: string): string {
  const gap = new RegExp(
    `(\\]\\([^)\\s]+\\))\\s{2,}(?=\\[[${triggerAlternation}])`,
    'g',
  );
  return text.replace(gap, '$1 ').trim();
}
