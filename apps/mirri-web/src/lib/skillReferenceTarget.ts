// apps/mirri-web/src/lib/skillReferenceTarget.ts
// Pure TS — no Vue. Maps a raw skill entry from the session skill list to the
// reference link target used by skillMention chips (ADR-004): disk skills
// resolve by their SKILL.md file path, builtin skills by their `builtin://`
// pseudo path (which IS the authority — never suffixed with /SKILL.md), and
// path-less entries fall back to the canonical command target.

export interface SkillLike {
  path?: string;
}

/** Builtin skills are identified by their `builtin://` pseudo path. */
const BUILTIN_PATH_PREFIX = 'builtin://';

/**
 * Resolve the chip target for a skill from the live session list.
 * `commandName` is the canonical slash-command name (`/name` or `/skill:name`)
 * used when the backend reports no path (older engines).
 */
export function skillReferenceTarget(skill: SkillLike | undefined, commandName: string): string {
  const rawPath = skill?.path;
  if (rawPath === undefined) return `command:${commandName}`;
  if (rawPath.toLowerCase().endsWith('skill.md')) return rawPath;
  if (rawPath.startsWith(BUILTIN_PATH_PREFIX)) return rawPath;
  return `${rawPath.replace(/\/+$/, '')}/SKILL.md`;
}
