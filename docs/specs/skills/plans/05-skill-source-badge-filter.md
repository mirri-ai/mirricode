# Skill Source Badge & Source Filter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each skill's source (builtin/project/user/extra/plugin-id) as a badge in the `/` slash menu and `$` skill trigger menu, and let the existing filter match canonical source tokens and plugin ids.

**Architecture:** The rust engine's `skill.list` WS-RPC projection gains an optional `plugin` field (single projection point: `dispatch_list_skills`). The web frontend threads `source`/`plugin` through the `SlashCommand` item model (currently dropped in `buildSlashItems`), renders a small badge in the name column of `SlashMenu.vue`, and extends `filterCommands` match keys. Spec: `docs/specs/skills-requirements.md` US3.8/SDR-009, `docs/specs/skills-architecture.md` ADR-012.

**Tech Stack:** Rust (mirri-embedded, serde_json), Vue 3 + TypeScript + vitest (apps/mirri-web).

## Global Constraints

- `source` wire values stay exactly `builtin` / `user` / `project` / `extra` — no enum extension (ADR-012).
- `plugin` is additive and optional; old clients ignore it, absent field means degradation to「额外」badge.
- Badge text reuses existing i18n keys `settings.skillsSourceBuiltin/Project/User/Extra` — zero new keys, do NOT change key values (the keys are also consumed by SkillsPanel).
- Filter matches canonical source tokens (`project` etc.) and plugin ids, never localized text.
- Tests use Given-When-Then naming (`should ... when ...`).
- Rust: no new workspace deps. Frontend: no new npm deps.
- Commit messages: no co-author attribution, no agent identity reveal.

---

### Task 1: Rust — `skill.list` plugin field (TDD)

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` (~line 4960, the `json!({...})` entry inside `dispatch_list_skills`)
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs` (append near the existing plugin tests ~line 18260)

**Interfaces:**
- Consumes: `SkillDef.plugin: Option<SkillPluginContext>` where `SkillPluginContext { id: String, instructions: Option<String> }` (rust/crates/mirri-core/src/skill.rs:109-115)
- Produces: `skill.list` entries optionally carry `"plugin": "<plugin-id>"` (string). Absent for non-plugin skills and plugin skills without context.

- [ ] **Step 1: Write the failing test**

Append to `rust/crates/mirri-embedded/src/embedded_server/tests.rs` (after the `should_inject_plugin_session_start_reminder_once_on_first_user_turn` test, which shows the full plugin-manager + `discover_plugin` + `EmbeddedServer::with_providers` setup pattern — copy that setup verbatim, replacing the assertion section):

```rust
/// Given a server with an installed plugin whose skills are discovered with
/// attribution, when `skill.list` answers, then each plugin skill entry
/// carries its plugin id in the additive `plugin` field while non-plugin
/// entries omit the field entirely (ADR-012).
#[tokio::test]
async fn should_list_plugin_skills_with_their_plugin_id() {
    // Given a server with an installed plugin whose skills are discovered
    let tmp = tempfile::tempdir().expect("tempdir");
    let source_dir = tempfile::tempdir().expect("plugin source dir");
    write_runtime_plugin_source(&source_dir);
    let plugins_dir = tempfile::tempdir().expect("plugins dir");
    let manager = mirri_plugin_host::PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&format!("local:{}", source_dir.path().display()))
        .await
        .expect("plugin install");

    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("plugin-list-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    for root in manager.plugin_skill_roots() {
        workspace
            .skills()
            .discover_plugin(
                &root.path,
                &mirri_core::skill::SkillPluginContext {
                    id: root.plugin.id.clone(),
                    instructions: root.plugin.instructions.clone(),
                },
            )
            .unwrap();
    }
    let session_manager = Arc::new(SessionManager::new(workspace));
    let server = EmbeddedServer::with_providers(
        session_manager,
        None,
        None,
        None,
        None,
        test_server_config(),
        None,
    )
    .with_plugin_manager(Arc::new(manager));

    // When skill.list answers
    let listed = server
        .call_method("skill.list", json!({}))
        .await
        .expect("skill.list");
    let entries = listed.as_array().expect("bare array wire shape");

    // Then a plugin skill entry carries its plugin id…
    let greeter = entries
        .iter()
        .find(|e| e["name"] == json!("greeter"))
        .expect("the plugin skill row");
    assert_eq!(greeter["source"], json!("extra"));
    assert_eq!(greeter["plugin"], json!("demo-plugin"));

    // …and non-plugin entries omit the field entirely
    for entry in entries.iter().filter(|e| e["name"] != json!("greeter")) {
        assert!(
            entry.get("plugin").is_none(),
            "non-plugin rows must omit the plugin field: {entry}"
        );
    }
}
```

Note: check `write_runtime_plugin_source` in the same file for the exact plugin id and skill name it writes (`demo-plugin` / `greeter` per the neighboring test's assertions); adjust the assertions if it differs. If `EmbeddedServer::with_providers` without providers does not support `skill.list` without a session, pass `Some(Arc::new(ScriptedLlmFactory::new(Arc::new(ScriptedLlm::new()))))` as the second argument like the neighbor test does.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded should_list_plugin_skills_with_their_plugin_id`
Expected: FAIL — `greeter["plugin"]` is `Null`, assert_eq fails (`json!(null)` vs `json!("demo-plugin")`).

- [ ] **Step 3: Implement the projection change**

In `dispatch_list_skills` (session_handlers.rs ~line 4942-4971), the entry construction currently reads:

```rust
// Read before the projection consumes the definition.
let disable_model_invocation = skill.disable_model_invocation;
```

Add after it:

```rust
// ADR-012: the additive plugin id for plugin-contributed skills —
// `source` stays `extra` (stable four-value contract); absent for
// non-plugin skills and plugin skills without context (frontend
// degrades to the generic 「额外」 badge).
let plugin_id = skill.plugin.as_ref().map(|ctx| ctx.id.clone());
```

And in the `json!({...})` entry, after `"enabled": enabled,` add (mirroring the existing `disable_model_invocation` optional-emit pattern):

```rust
if let Some(plugin_id) = plugin_id {
    entry["plugin"] = json!(plugin_id);
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded should_list_plugin_skills_with_their_plugin_id`
Expected: PASS

- [ ] **Step 5: Run the neighboring skill.list tests (regression)**

Run: `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded skill_list`
Expected: PASS (workspace routing tests that assert on `source` etc.)

- [ ] **Step 6: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs
git commit -m "feat(skill-list): carry plugin id on skill.list entries"
```

---

### Task 2: Web — model passthrough + filter match keys (TDD)

**Files:**
- Modify: `apps/mirri-web/src/api/types.ts` (~line 1124, `AppSkill`)
- Modify: `apps/mirri-web/src/api/daemon/ws-rpc-api.ts` (`RpcSkill` ~line 3885, `toAppSkill` ~line 4333)
- Modify: `apps/mirri-web/src/lib/slashCommands.ts` (`SlashCommand` ~line 4, `buildSlashItems` ~line 119)
- Test: `apps/mirri-web/src/lib/slashCommands.test.ts` (append)

**Interfaces:**
- Consumes: Task 1's wire shape — `skill.list` entries optionally carry `plugin: string`.
- Produces:
  - `interface AppSkill { ...; plugin?: string }`
  - `interface SlashCommand { ...; source?: string; plugin?: string }`
  - `buildSlashItems` output items carry `source`/`plugin` verbatim from the input skills.
  - `filterCommands` matches skills additionally against `item.source` and `item.plugin` (lowercased substring via the existing `matchScore` tiers).

- [ ] **Step 1: Write the failing tests**

Append to `apps/mirri-web/src/lib/slashCommands.test.ts`:

```ts
describe('buildSlashItems — source passthrough', () => {
  it('should carry source and plugin onto skill items when building slash items', () => {
    const items = buildSlashItems([
      { name: 'deploy', description: 'Deploy', source: 'builtin' },
      { name: 'greeter', description: 'Greets', source: 'extra', plugin: 'demo-plugin' },
      { name: 'my-tool', description: 'Mine', source: 'project' },
    ]);
    const deploy = items.find((i) => i.name === '/deploy');
    const greeter = items.find((i) => i.name === '/skill:greeter');
    const mine = items.find((i) => i.name === '/skill:my-tool');
    expect(deploy?.source).toBe('builtin');
    expect(deploy?.plugin).toBeUndefined();
    expect(greeter?.source).toBe('extra');
    expect(greeter?.plugin).toBe('demo-plugin');
    expect(mine?.source).toBe('project');
  });

  it('should omit source and plugin on built-in command items', () => {
    const items = buildSlashItems([{ name: 'deploy', description: 'Deploy', source: 'builtin' }]);
    const command = items.find((i) => i.name === '/new');
    expect(command?.source).toBeUndefined();
    expect(command?.plugin).toBeUndefined();
  });
});

describe('filterCommands — source and plugin match keys', () => {
  const skills = [
    { name: 'deploy', description: 'Deploy', source: 'builtin' },
    { name: 'lint-it', description: 'Lint', source: 'project' },
    { name: 'greeter', description: 'Greets', source: 'extra', plugin: 'demo-plugin' },
  ];

  it('should keep only project skills when the query is the canonical source token', () => {
    const items = filterCommands('project', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['lint-it']);
  });

  it('should keep only a plugin skills when the query is the plugin id', () => {
    const items = filterCommands('demo-plugin', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['greeter']);
  });

  it('should match a plugin id on a substring like skill names do', () => {
    const items = filterCommands('demo', buildSlashItems(skills));
    const names = items.filter((i) => i.isSkill).map((i) => skillDisplayName(i));
    expect(names).toEqual(['greeter']);
  });

  it('should not match localized source labels like 额外 when filtering', () => {
    const items = filterCommands('额外', buildSlashItems(skills));
    expect(items.filter((i) => i.isSkill)).toEqual([]);
  });

  it('should still clear the list when the query matches nothing', () => {
    const items = filterCommands('nomatch-zzz', buildSlashItems(skills));
    expect(items).toEqual([]);
  });
});
```

Check the test file's existing imports and extend them: `buildSlashItems`, `filterCommands`, `skillDisplayName` (some already imported).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd apps/mirri-web && pnpm vitest run src/lib/slashCommands.test.ts`
Expected: FAIL — `source`/`plugin` are `undefined` on items; the source-token filter cases return `[]` (no match).

- [ ] **Step 3: Implement**

`apps/mirri-web/src/api/types.ts` — `AppSkill` gains (after `source: string;`):

```ts
  /** Contributing plugin id for plugin-sourced skills; absent when the
   * engine did not send one (ADR-012 — degrade to the generic extra badge). */
  plugin?: string;
```

`apps/mirri-web/src/api/daemon/ws-rpc-api.ts` — `RpcSkill` gains (after `source: string;`):

```ts
  /** Contributing plugin id (ADR-012); absent on older engines. */
  plugin?: string;
```

`toAppSkill` gains in the passthrough block:

```ts
    ...(skill.plugin !== undefined ? { plugin: skill.plugin } : {}),
```

`apps/mirri-web/src/lib/slashCommands.ts` — `SlashCommand` gains:

```ts
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
```

`buildSlashItems` — extend the map callback (and its parameter type) to pass them through; the parameter type becomes:

```ts
  skills: ReadonlyArray<{
    name: string;
    description: string;
    source?: string;
    plugin?: string;
    enabled?: boolean;
  }> = [],
```

and the mapped item gains:

```ts
      source: s.source,
      plugin: s.plugin,
```

(both optional properties — direct assignment, no conditional spread, per repo convention).

`filterCommands` — extend the skill scoring block. Current code:

```ts
        let score = item.isSkill
          ? skillCommandScore(fullName, q)
          : matchScore(fullName, q);
        if (item.isSkill) {
          // The bare (prefix-stripped) name always matches at its regular
          // tier; the better of the two scores wins.
          score = Math.max(score, matchScore(skillDisplayName(item).toLowerCase(), q));
        }
```

becomes:

```ts
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd apps/mirri-web && pnpm vitest run src/lib/slashCommands.test.ts`
Expected: PASS (all, including the pre-existing namespace-demotion cases — the match-key extension only adds candidates via `Math.max`, never lowers an existing score).

- [ ] **Step 5: Run the menu integration tests (regression)**

Run: `cd apps/mirri-web && pnpm vitest run test/slash-menu.test.ts test/skill-trigger-menu.test.ts`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add apps/mirri-web/src/api/types.ts apps/mirri-web/src/api/daemon/ws-rpc-api.ts apps/mirri-web/src/lib/slashCommands.ts apps/mirri-web/src/lib/slashCommands.test.ts
git commit -m "feat(slash-menu): thread skill source and plugin id through item model and filter"
```

---

### Task 3: Web — badge rendering in SlashMenu.vue

**Files:**
- Modify: `apps/mirri-web/src/lib/slashCommands.ts` (add exported pure functions)
- Modify: `apps/mirri-web/src/components/chat/SlashMenu.vue` (template ~lines 106-111, script, styles)
- Test: `apps/mirri-web/src/lib/slashCommands.test.ts` (append)

**Testing strategy note:** this repo has **no `@vue/test-utils`** — the established pattern (`test/open-in-button.test.ts`, `test/turn-usage-pill.test.ts`) is to export the component's decision logic as named pure exports from a lib file (or the `.vue` script block) and test those directly. This task follows that pattern: the badge decisions are pure functions in `src/lib/slashCommands.ts`; the template wiring and CSS are verified by typecheck, the existing mount-free tests, and manual smoke.

**Interfaces:**
- Consumes: Task 2's `SlashCommand.source` / `SlashCommand.plugin`.
- Produces:
  - `skillSourceBadgeText(item: SlashCommand, t: (key: string) => string): string | undefined` — exported from `src/lib/slashCommands.ts`
  - `skillSourceBadgeVariant(source: string | undefined, plugin: string | undefined): 'info' | 'neutral' | 'success' | 'solid'` — exported from `src/lib/slashCommands.ts`
  - rendered badge in the `.slash-name` column for skill items only.

- [ ] **Step 1: Write the failing tests**

Append to `apps/mirri-web/src/lib/slashCommands.test.ts` (extend the import list from `'../src/lib/slashCommands'`… adjust to the file's actual relative import, which is `'./slashCommands'`):

```ts
import {
  skillSourceBadgeText,
  skillSourceBadgeVariant,
} from './slashCommands';

const T = (key: string): string => key; // identity — assert the KEY, not the label

const skillItem = (name: string, source?: string, plugin?: string): SlashCommand => ({
  name: source === 'builtin' ? `/${name}` : `/skill:${name}`,
  desc: 'A skill',
  isSkill: true,
  acceptsInput: true,
  source,
  plugin,
});

describe('skillSourceBadgeText', () => {
  it('should return the plugin id as the badge text when the skill carries a plugin', () => {
    expect(skillSourceBadgeText(skillItem('greeter', 'extra', 'demo-plugin'), T)).toBe('demo-plugin');
  });

  it('should return the localized key for each known source when no plugin id is present', () => {
    expect(skillSourceBadgeText(skillItem('deploy', 'builtin'), T)).toBe('settings.skillsSourceBuiltin');
    expect(skillSourceBadgeText(skillItem('lint-it', 'project'), T)).toBe('settings.skillsSourceProject');
    expect(skillSourceBadgeText(skillItem('mine', 'user'), T)).toBe('settings.skillsSourceUser');
    expect(skillSourceBadgeText(skillItem('thing', 'extra'), T)).toBe('settings.skillsSourceExtra');
  });

  it('should degrade to the extra label when a plugin skill has no plugin id', () => {
    expect(skillSourceBadgeText(skillItem('thing', 'extra'), T)).toBe('settings.skillsSourceExtra');
  });

  it('should render an unknown source verbatim instead of swallowing it', () => {
    expect(skillSourceBadgeText(skillItem('odd', 'future-scope'), T)).toBe('future-scope');
  });

  it('should return undefined for commands and skills with no source', () => {
    expect(skillSourceBadgeText({ name: '/new', desc: 'commands.new.desc' }, T)).toBeUndefined();
    expect(skillSourceBadgeText(skillItem('bare'), T)).toBeUndefined();
  });
});

describe('skillSourceBadgeVariant', () => {
  it('should map each known source to its SkillsPanel badge variant', () => {
    expect(skillSourceBadgeVariant('project', undefined)).toBe('info');
    expect(skillSourceBadgeVariant('user', undefined)).toBe('neutral');
    expect(skillSourceBadgeVariant('extra', undefined)).toBe('success');
    expect(skillSourceBadgeVariant('builtin', undefined)).toBe('solid');
  });

  it('should use the extra variant for plugin skills whether or not the plugin id is present', () => {
    expect(skillSourceBadgeVariant('extra', 'demo-plugin')).toBe('success');
  });

  it('should fall back to neutral for unknown sources', () => {
    expect(skillSourceBadgeVariant('future-scope', undefined)).toBe('neutral');
  });
});
```

Check the existing test file's import style (`import {...} from './slashCommands'` presumably) and merge the new imports into it rather than adding a second import statement.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd apps/mirri-web && pnpm vitest run src/lib/slashCommands.test.ts`
Expected: FAIL — `skillSourceBadgeText` / `skillSourceBadgeVariant` are not exported (import error or undefined).

- [ ] **Step 3: Implement the pure functions**

In `apps/mirri-web/src/lib/slashCommands.ts`, after `isBlankSkillDesc`:

```ts
/** Localized-label i18n key prefix for known skill sources (US3.8). */
const SKILL_SOURCE_LABEL_KEY_PREFIX = 'settings.skillsSource';

/**
 * Badge text for a skill row (US3.8): the plugin id when the engine sent
 * one, else the localized source label; `undefined` for commands and
 * skills without a source. Unknown sources render verbatim so new engine
 * scopes stay visible instead of being swallowed.
 */
export function skillSourceBadgeText(
  item: Pick<SlashCommand, 'isSkill' | 'source' | 'plugin'>,
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
  if (source === undefined) return 'neutral';
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
```

(`plugin` is in the signature for call-site symmetry; plugin skills are always `extra` on the wire, so `success` covers them — keep the parameter so future source values cannot silently mis-color a plugin row.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd apps/mirri-web && pnpm vitest run src/lib/slashCommands.test.ts`
Expected: PASS

- [ ] **Step 5: Wire the badge into the template**

In `SlashMenu.vue`:

`<script setup>` — import the two functions (add to the existing `slashCommands` import):

```ts
import {
  groupSlashItems,
  isBlankSkillDesc,
  skillDisplayName,
  skillSourceBadgeText,
  skillSourceBadgeVariant,
  type SlashCommand,
} from '../../lib/slashCommands';
```

Template — after the display name inside `.slash-name`:

```html
<span class="slash-name">
  <span v-if="row.item.isSkill" class="skill-icon" aria-hidden="true">
    <Icon name="skill" size="sm" />
  </span>
  {{ row.item.isSkill ? skillDisplayName(row.item) : row.item.name }}
  <span
    v-if="row.item.isSkill && skillSourceBadgeText(row.item, t) !== undefined"
    class="skill-source-badge"
    :class="`skill-source-badge-${skillSourceBadgeVariant(row.item.source, row.item.plugin)}`"
  >{{ skillSourceBadgeText(row.item, t) }}</span>
</span>
```

Styles (scoped, after the `.skill-icon` rules) — per-source variants per ADR-012 (`project=info / user=neutral / extra=success / builtin=solid`), a small pill at menu scale:

```css
/* Source badge (US3.8) — a small pill after the skill name. Variants mirror
   the SkillsPanel source-badge semantics (SOURCE_BADGE_VARIANT) at menu
   scale: same source, same color, wherever the user sees it. */
.skill-source-badge {
  display: inline-flex;
  align-items: center;
  padding: 0 5px;
  border-radius: var(--radius-full, 999px);
  font-size: var(--text-xs);
  line-height: 16px;
  flex-shrink: 0;
  max-width: 120px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.skill-source-badge-info {
  color: var(--color-info-text, var(--color-text-muted));
  background: var(--color-info-soft, var(--color-surface-sunken));
}
.skill-source-badge-neutral {
  color: var(--color-text-muted);
  background: var(--color-surface-sunken);
}
.skill-source-badge-success {
  color: var(--color-success-text, var(--color-text-muted));
  background: var(--color-success-soft, var(--color-surface-sunken));
}
.skill-source-badge-solid {
  color: var(--color-surface-raised);
  background: var(--color-text-muted);
}
```

Before writing the CSS, check the actual design-token names in the repo (`rg --files apps/mirri-web/src | xargs grep -l 'color-info'` or inspect `SkillsPanel.vue` / the `Badge` component's variant styles in `src/components/ui/Badge.vue` if present) and prefer the same token names the `Badge` component uses for `info` / `success` / `solid` variants — replace the `var(--fallback, …)` chains with the repo's real tokens. If a `Badge` UI component exists and fits inline, prefer `<Badge :variant="…" size="sm">` over hand-rolled CSS.

- [ ] **Step 6: Run typecheck + style check + regression tests**

Run: `cd apps/mirri-web && pnpm typecheck && pnpm check:style && pnpm vitest run src/lib/slashCommands.test.ts test/slash-menu.test.ts test/skill-trigger-menu.test.ts`
Expected: PASS (mount-free; the template wiring is covered by vue-tsc)

- [ ] **Step 7: Commit**

```bash
git add apps/mirri-web/src/lib/slashCommands.ts apps/mirri-web/src/lib/slashCommands.test.ts apps/mirri-web/src/components/chat/SlashMenu.vue
git commit -m "feat(slash-menu): badge skill rows with their source"
```

---

### Task 4: Docs — changeset + skills-ROADMAP note

**Files:**
- Create: `.changeset/<random-name>.md` (follow `.agents/skills/gen-changesets/SKILL.md` — minor bump on `@mirri-ai/release`, English entry)
- Modify: `docs/specs/skills-ROADMAP.md` (add the SDR-009 work item to the appropriate section)

**Interfaces:**
- Consumes: Tasks 1-3 complete.
- Produces: release documentation.

- [ ] **Step 1: Read the gen-changesets skill and generate the changeset**

Read `.agents/skills/gen-changesets/SKILL.md`, follow its rules (anchor `@mirri-ai/release`, `minor` bump — user-perceivable feature). Entry text (English):

```yaml
---
'@mirri-ai/release': minor
---

Skill source badges in the slash menu and skill trigger menu: every skill row now shows where it comes from (built-in, project, user, extra, or the contributing plugin's id), and menu filtering matches canonical source tokens and plugin ids alongside skill names.
```

- [ ] **Step 2: Add the ROADMAP entry**

Open `docs/specs/skills-ROADMAP.md`, find the section tracking US3.x items, add one line for SDR-009/US3.8 pointing at `docs/specs/skills/plans/05-skill-source-badge-filter.md` for status. Follow the file's existing entry format.

- [ ] **Step 3: Commit**

```bash
git add .changeset docs/specs/skills-ROADMAP.md
git commit -m "chore: changeset and roadmap entry for skill source badge"
```

---

## Final Verification (all tasks)

- [ ] `cd apps/mirri-web && pnpm typecheck && pnpm test` (full maintained TS suite)
- [ ] `cargo test --manifest-path rust/Cargo.toml -p mirri-embedded skill_list` plus the new plugin test (or the crate suite if fast enough; full workspace suite runs at Phase 4/5)
- [ ] Manual smoke (optional, if a dev server is available): `pnpm dev:v2`, type `/` and `$`, verify badges and filtering by `project` / plugin id.

## Self-Review Record

- **Spec coverage:** US3.8 badge items → Tasks 2-3; per-source visual variants (ADR-012 decision 3, `SOURCE_BADGE_VARIANT` semantics) → Task 3 Step 5; filter items → Task 2; wire `plugin` field → Task 1; degradation (no plugin id → 额外) → Task 3 Step 1 tests; commands without badge → Tasks 2-3 tests; unknown source verbatim → Task 3 Step 1 test (`future-scope` case). i18n zero-new-keys → Task 3 constraint (pure function returns the key; the component passes `t`).
- **Placeholder scan:** no TBD/TODO; Task 3 Step 5 contains an explicit token-name verification instruction (check real design tokens before writing CSS) — a verification step with concrete fallbacks, not a placeholder.
- **Type consistency:** `AppSkill.plugin?: string`, `RpcSkill.plugin?: string`, `SlashCommand.source?/plugin?: string`, `skillSourceBadgeText(item: Pick<SlashCommand, 'isSkill' | 'source' | 'plugin'>, t)`, `skillSourceBadgeVariant(source?: string, plugin?: string)` used consistently across Tasks 2-3.
- **Test strategy:** pure-function exports + direct tests (repo has no `@vue/test-utils`; follows the `open-in-button` / `turn-usage-pill` precedent). Template wiring covered by `vue-tsc` typecheck; visual smoke is manual.
