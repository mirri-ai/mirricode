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
  skills: ReadonlyArray<{ name: string; description: string; source?: string; enabled?: boolean }> = [],
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
 * the raw description.
 */
export function buildSkillTriggerItems(
  skills: ReadonlyArray<{ name: string; description: string; source?: string; enabled?: boolean }> = [],
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
 * `/skill:deploy` (no regression); the better of the two scores wins.
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
        let score = matchScore(fullName, q);
        if (item.isSkill) {
          score = Math.max(score, matchScore(skillDisplayName(item).toLowerCase(), q));
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
