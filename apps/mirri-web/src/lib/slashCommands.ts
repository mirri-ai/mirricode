// apps/mirri-web/src/lib/slashCommands.ts
// Pure TS — no Vue, no side effects. Slash-command metadata + parsers.

export interface SlashCommand {
  name: string;
  /**
   * Description text. For built-in commands this is an i18n KEY (resolve with
   * t(desc)); for skills (`isSkill`) it is the skill's RAW description, rendered
   * verbatim.
   */
  desc: string;
  /**
   * True for a session skill (not a built-in command). Selecting one activates
   * the skill instead of running an app command, and its `desc` is raw text.
   */
  isSkill?: boolean;
  /**
   * Selecting the item should leave the command in the composer so the user can
   * type the message/argument that follows it.
   */
  acceptsInput?: boolean;
  /**
   * Skill source wire value (`builtin` | `user` | `project` | `extra`) —
   * carried for skills so the menu can badge the row and the filter can
   * match the canonical source token (US3.8/ADR-012). Absent on commands.
   */
  source?: string;
  /**
   * Contributing plugin id for plugin-sourced skills; absent when the
   * engine did not send one (degrade to the generic extra badge).
   */
  plugin?: string;
}

export const SLASH_COMMANDS: SlashCommand[] = [
  { name: '/new',        desc: 'commands.new.desc' },
  { name: '/clear',      desc: 'commands.clear.desc' },
  { name: '/plan',       desc: 'commands.plan.desc' },
  { name: '/swarm',      desc: 'commands.swarm.desc', acceptsInput: true },
  { name: '/goal',       desc: 'commands.goal.desc', acceptsInput: true },
  { name: '/btw',        desc: 'commands.btw.desc', acceptsInput: true },
  { name: '/auto',       desc: 'commands.auto.desc' },
  { name: '/yolo',       desc: 'commands.yolo.desc' },
  { name: '/thinking',   desc: 'commands.thinking.desc' },
  { name: '/compact',    desc: 'commands.compact.desc', acceptsInput: true },
  { name: '/undo',       desc: 'commands.undo.desc' },
  { name: '/fork',       desc: 'commands.fork.desc' },
  { name: '/status',     desc: 'commands.status.desc' },
  { name: '/cron',       desc: 'commands.cron.desc' },
  { name: '/tasks',      desc: 'commands.tasks.desc' },
];

/**
 * Parse a slash command from the start of the input string.
 * Returns { cmd, arg } if input starts with `/` at line start (no leading whitespace),
 * otherwise returns null.
 *
 * Examples:
 *   "/help"         -> { cmd: "/help", arg: "" }
 *   "/new session"  -> { cmd: "/new", arg: "session" }
 *   "hello /help"   -> null (slash not at line start)
 *   "  /help"       -> null (leading whitespace)
 */
export function parseSlash(input: string): { cmd: string; arg: string } | null {
  if (!input.startsWith('/')) return null;
  // Must start exactly at position 0 (no leading spaces)
  const spaceIdx = input.indexOf(' ');
  if (spaceIdx === -1) {
    return { cmd: input, arg: '' };
  }
  return {
    cmd: input.slice(0, spaceIdx),
    arg: input.slice(spaceIdx + 1),
  };
}

/** The prefix marking a slash item as a skill activation (`/skill:<name>`). */
export const SKILL_COMMAND_PREFIX = 'skill:';

/**
 * Strip the `skill:` prefix from a slash-command name (with or without the
 * leading `/`), returning the bare skill name. Non-prefixed input is returned
 * unchanged.
 */
export function stripSkillPrefix(name: string): string {
  return name.startsWith(SKILL_COMMAND_PREFIX) ? name.slice(SKILL_COMMAND_PREFIX.length) : name;
}

/**
 * Return the display name of a slash item. For skill items this is the bare
 * name — the canonical command name (`/skill:<name>`, or `/<name>` for
 * builtin-sourced skills) with the leading slash and `skill:` prefix stripped
 * (`/skill:deploy` -> `deploy`). Non-skill items return `item.name` unchanged.
 */
export function skillDisplayName(item: SlashCommand): string {
  if (!item.isSkill) return item.name;
  return stripSkillPrefix(item.name.replace(/^\//, ''));
}

/**
 * True when a skill description carries no usable text: empty or
 * whitespace-only, or made solely of symbols/punctuation (no letter or number
 * in any script). Callers rendering the skill trigger menu can fall back to
 * the bare skill name when this is true.
 */
export function isBlankSkillDesc(desc: string): boolean {
  const trimmed = desc.trim();
  return trimmed === '' || /[\p{L}\p{N}]/u.test(trimmed) === false;
}

/** Localized-label i18n key prefix for known skill sources (US3.8). */
const SKILL_SOURCE_LABEL_KEY_PREFIX = 'settings.skillsSource';

/**
 * Badge text for a skill row (US3.8): the plugin id when the engine sent
 * one, else the localized source label; `undefined` for commands and
 * skills without a source. Unknown sources render verbatim so new engine
 * scopes stay visible instead of being swallowed.
 */
export function skillSourceBadgeText(
  item: SlashCommand,
  t: (key: string) => string,
): string | undefined {
  if (!item.isSkill || item.source === undefined) return undefined;
  if (item.plugin !== undefined) return item.plugin;
  const known = ['builtin', 'user', 'project', 'extra'] as const;
  if ((known as readonly string[]).includes(item.source)) {
    const key = item.source as (typeof known)[number];
    return t(`${SKILL_SOURCE_LABEL_KEY_PREFIX}${key.charAt(0).toUpperCase()}${key.slice(1)}`);
  }
  return item.source;
}

/**
 * Badge variant for a skill row (US3.8/ADR-012): mirrors the SkillsPanel
 * source-badge semantics (`SOURCE_BADGE_VARIANT`: project=info / user=
 * neutral / extra=success / builtin=solid) so the same source wears the
 * same color in the menu and the settings panel. Unknown sources fall
 * back to neutral; plugin skills wear the extra (success) variant.
 */
export function skillSourceBadgeVariant(
  source: string | undefined,
  plugin: string | undefined,
): 'info' | 'neutral' | 'success' | 'solid' {
  // A plugin-sourced row is `extra` on the wire — the plugin id in the
  // signature is a guard so future source values cannot silently mis-color
  // a plugin row, and success (the extra variant) is what the settings
  // panel shows for plugin skills.
  if (plugin !== undefined) return 'success';
  switch (source) {
    case 'project':
      return 'info';
    case 'user':
      return 'neutral';
    case 'extra':
      return 'success';
    case 'builtin':
      return 'solid';
    default:
      return 'neutral';
  }
}

/**
 * Build the full slash-item list: built-in commands followed by the session's
 * skills. Within the skills, builtin-sourced skills come before the others
 * (a stable sort, US3.1/US3.4: the platform's own skills surface first while
 * relative order inside each group is preserved). Non-builtin skills are shown
 * as `/skill:<skill-name>` so the user can tell them apart from built-in
 * commands (mirroring the TUI); builtin-sourced skills keep the bare
 * `/<skill-name>`. Skills carry their raw description and an `isSkill` flag so
 * the caller knows to activate rather than run a command.
 *
 * A skill the user disabled at the user level (`enabled === false`, B8) is
 * dropped by default — only an explicit false excludes a row; absent means
 * enabled (older backends have no per-skill disable state). Pass
 * `{ includeDisabled: true }` for the *recognition* face (the Composer's
 * submit gate): a disabled skill is still a known command there, so typing
 * its `/skill:<name>` routes to the activation path (which the server then
 * rejects with its "disabled by the user" error) instead of leaking the raw
 * command text into the conversation as a chat message.
 */
export function buildSlashItems(
  skills: ReadonlyArray<{
    name: string;
    description: string;
    source?: string;
    plugin?: string;
    enabled?: boolean;
  }> = [],
  options: { includeDisabled?: boolean } = {},
): SlashCommand[] {
  const skillItems: SlashCommand[] = skills
    .filter((s) => options.includeDisabled === true || s.enabled !== false)
    // Builtin-sourced skills first (US3.1/US3.4). `toSorted` is a stable sort,
    // so the relative order inside each group (builtin / non-builtin) is kept.
    .toSorted((a, b) => Number(b.source === 'builtin') - Number(a.source === 'builtin'))
    .map((s) => ({
      name: s.source === 'builtin' ? `/${s.name}` : `/${SKILL_COMMAND_PREFIX}${s.name}`,
      desc: s.description,
      isSkill: true,
      // Keep the selected skill in the composer so arguments can be appended.
      acceptsInput: true,
      // Source/plugin passthrough (US3.8/ADR-012): carried verbatim for the
      // menu badge and the source-token filter; absent on commands.
      source: s.source,
      plugin: s.plugin,
    }));
  return [...SLASH_COMMANDS, ...skillItems];
}

/**
 * Build ONLY the skill items (no built-in commands) for the skill trigger
 * menu. Shares {@link buildSlashItems}'s filtering semantics: a skill disabled
 * at the user level (`enabled === false`) is dropped by default; pass
 * `{ includeDisabled: true }` to keep it (the recognition face). Item names
 * stay canonical command names (`/${name}` for builtin source,
 * `/skill:${name}` otherwise) with `isSkill: true`, `acceptsInput: true` and
 * the raw description; `source`/`plugin` pass through verbatim (US3.8/
 * ADR-012) for the menu badge and the source-token filter.
 */
export function buildSkillTriggerItems(
  skills: ReadonlyArray<{ name: string; description: string; source?: string; plugin?: string; enabled?: boolean }> = [],
  options: { includeDisabled?: boolean } = {},
): SlashCommand[] {
  return buildSlashItems(skills, options).filter((item) => item.isSkill === true);
}

/**
 * Split slash items into commands and skills (by the `isSkill` flag),
 * preserving the input order within each group: commands first, then skills.
 */
export function groupSlashItems(
  items: SlashCommand[],
): { commands: SlashCommand[]; skills: SlashCommand[] } {
  const commands: SlashCommand[] = [];
  const skills: SlashCommand[] = [];
  for (const item of items) {
    if (item.isSkill) skills.push(item);
    else commands.push(item);
  }
  return { commands, skills };
}

/**
 * Rank one match key against the normalized query: 3 = exact, 2 = prefix
 * (startsWith), 1 = arbitrary substring, 0 = no match.
 */
function matchScore(key: string, q: string): number {
  if (key === q) return 3;
  if (key.startsWith(q)) return 2;
  if (key.includes(q)) return 1;
  return 0;
}

/**
 * Rank a skill's canonical command name (`skill:<bare>`) against a query,
 * demoting matches that fall entirely inside the `skill:` namespace
 * segment (ADR-003's user-facing rationale: `skill:` is a qualifier, not
 * part of the name — "write-a-skill" must outrank "write-tui" for the
 * query "skill", which a plain prefix match on `skill:write-tui` would
 * otherwise steal).
 *
 * A match is namespace-confined when the query contains no colon and ends
 * at or before the namespace separator (`skill:`.length): the query
 * "skill" or "sk" matches only the qualifier, while "skill:de" or "de"
 * spills past it into the name and keeps its regular score.
 */
const NAMESPACE_HIT_SCORE = 0.5;

function skillCommandScore(fullName: string, q: string): number {
  if (q.includes(':')) return matchScore(fullName, q);
  const score = matchScore(fullName, q);
  if (score === 0) return 0;
  // The demotion applies only to namespaced command names (`skill:<bare>`) —
  // a builtin skill's bare command name (`/<name>`) has no qualifier segment,
  // so its matches always keep their regular tier.
  if (!fullName.startsWith(SKILL_COMMAND_PREFIX)) return score;
  // Demote when the match never reaches past the `skill:` segment: the
  // matched region ends at or before the separator position.
  const separatorEnd = SKILL_COMMAND_PREFIX.length; // position after ':'
  const matchEnd = fullName.indexOf(q) + q.length;
  return matchEnd <= separatorEnd ? NAMESPACE_HIT_SCORE : score;
}

/**
 * Reorder slash items so every command precedes every skill, preserving the
 * order within each group. `filterCommands` applies this after score ranking so
 * the returned list order always equals the menu's visual group order (
 * commands first, ADR-003) and therefore the menu's index-based selection
 * semantics (highlighter / aria-selected / Enter / hover / scrollIntoView).
 */
function commandsBeforeSkills(items: SlashCommand[]): SlashCommand[] {
  const { commands, skills } = groupSlashItems(items);
  return [...commands, ...skills];
}

/**
 * Filter slash items by a query string. Matches are ranked so exact and prefix
 * matches come before arbitrary substring matches. If query is empty or just
 * "/", returns all items. Defaults to the built-in commands; pass a merged list
 * (see buildSlashItems) to include skills.
 *
 * Commands match against the canonical name. Skill items additionally match
 * their bare (prefix-stripped) name — `de` finds `/skill:deploy` at startsWith
 * level — while the canonical name still answers full queries like `skill:` or
 * `/skill:deploy` (no regression); the better of the two scores wins. A match
 * confined to the `skill:` qualifier segment (query "skill"/"sk", no colon)
 * is demoted below every bare-name tier, so name-bearing skills outrank the
 * namespace-only hits (see [`skillCommandScore`]).
 *
 * After score ranking the results are regrouped (command group first, then
 * skill group) so the returned order is stable regardless of which side scored
 * higher — the caller renders the menu in that exact order (ADR-003).
 */
export function filterCommands(
  query: string,
  items: SlashCommand[] = SLASH_COMMANDS,
): SlashCommand[] {
  const q = query.toLowerCase().trim().replace(/^\//, '');
  if (q === '') return commandsBeforeSkills(items);

  return commandsBeforeSkills(
    items
      .map((item, index) => {
        const fullName = item.name.toLowerCase().replace(/^\//, '');
        let score = item.isSkill
          ? skillCommandScore(fullName, q)
          : matchScore(fullName, q);
        if (item.isSkill) {
          // The bare (prefix-stripped) name always matches at its regular
          // tier; the better of the two scores wins.
          score = Math.max(score, matchScore(skillDisplayName(item).toLowerCase(), q));
          // Source filter (US3.8): the canonical source token and the plugin
          // id join the match keys — matched against wire values, never
          // localized text, so the same query hits the same skills in any
          // UI language.
          if (item.source !== undefined) {
            score = Math.max(score, matchScore(item.source.toLowerCase(), q));
          }
          if (item.plugin !== undefined) {
            score = Math.max(score, matchScore(item.plugin.toLowerCase(), q));
          }
        }
        return { item, index, score };
      })
      .filter(({ score }) => score > 0)
      .toSorted((a, b) => {
        if (a.score !== b.score) return b.score - a.score;
        return a.index - b.index;
      })
      .map(({ item }) => item),
  );
}

/**
 * Characters that start a skill trigger from the Composer (the ASCII `$` plus
 * the half-width and full-width Yen signs, which many IMEs type for `$`).
 */
export const SKILL_TRIGGER_STARTS = ['$', '¥', '￥'] as const;

/**
 * True when `text` starts a potential skill trigger: non-empty, the first
 * character is one of {@link SKILL_TRIGGER_STARTS}, and the text contains no
 * whitespace (any whitespace means a message is being typed, not a trigger —
 * `\s` covers the full-width ideographic space U+3000 that IMEs type too).
 */
export function isSkillTriggerStart(text: string): boolean {
  return (
    text.length > 0 &&
    SKILL_TRIGGER_STARTS.some((trigger) => trigger === text[0]) &&
    !/\s/.test(text)
  );
}
