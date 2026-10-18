//! The default-base system prompt template (the TS v2 `system.md` mirror).
//!
//! [`SYSTEM_PROMPT_TEMPLATE`] is a verbatim mirror of the TS kernel's base
//! template (`packages/agent-core-v2/src/app/agentProfileCatalog/system.md`)
//! with the placeholder syntax converted from `${var}` to the rust engine's
//! `{{var}}` convention. The mirror is documented divergence: the two files
//! will drift over time, and byte-equality is impossible by construction
//! (the placeholder syntax differs). When editing the v2 template, mirror
//! the change here (and vice versa) — the header of each file points at the
//! other.
//!
//! Rendering is two-pass, matching the session's prompt lifecycle:
//!
//! 1. [`render_fixed_part`] substitutes the creation-time variables (host
//!    identity, role text, OS/shell, `now`, `cwd`). The result is stable for
//!    an agent's whole life — these are the values the template itself
//!    describes as captured at session start.
//! 2. [`apply_workspace_part`] substitutes the workspace-derived variables
//!    (directory tree, trust-gated `AGENTS.md`). The session's refresh path
//!    can re-run this pass against a fresh snapshot.
//!
//! The skills listing is rendered by [`render_skills_section`] (design 13
//! §3.5), the additional-dirs section by [`render_additional_dirs_section`]
//! (plan 2026-10-10-sandbox-path-permission D5), and the plugin
//! contributions by [`render_plugin_sections`]; all are evaluated at prompt
//! assembly — the values a new session's agents capture (existing sessions
//! keep their assembly-time snapshot, the `refresh_plugin_consumers`
//! documented difference).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::plugin_runtime::PluginSystemPromptSection;
use crate::profile::render_system_prompt;
use crate::skill::{SkillCatalog, SkillDef, SkillSource};

/// The base template — see the [module docs](self) for the mirror policy.
const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("system_prompt.md");

/// The default host identity (the TS `DEFAULT_PRODUCT_NAME`).
const DEFAULT_PRODUCT_NAME: &str = "Mirri Code CLI";

/// The default reply-style guide (the TS `DEFAULT_REPLY_STYLE_GUIDE`).
const DEFAULT_REPLY_STYLE_GUIDE: &str = "Your text replies render as Markdown in the user's terminal. Use light Markdown that reads well there: short paragraphs, `-` bullets for lists, backticks for code, commands, paths, and identifiers, and fenced blocks for multi-line code. Keep structure shallow — avoid deep nesting, large tables, and heavy headings in ordinary replies. Do not use emoji unless the user does first or asks for it. Default to prose; reach for a list only when the content is genuinely a set of items or steps. When you point to a specific code location, cite it as `path/to/file.ts:42` — a precise, consistent reference the user can navigate to.";

/// The Windows shell notes (the TS `WINDOWS_NOTES`), shown on Windows only.
const WINDOWS_NOTES: &str = "IMPORTANT: You are on Windows. The Bash tool runs through Git Bash, so use Unix shell syntax inside Bash commands — `/dev/null` not `NUL`, and forward slashes in paths. For file operations, always prefer the built-in tools (Read, Write, Edit, Glob, Grep) over Bash — they work reliably across all platforms.";

/// The creation-time variables of one agent's default-base prompt.
pub(crate) struct SystemPromptFixedVars {
    /// Host identity line ("You are …, an interactive general AI agent").
    pub product_name: String,
    /// Role text for the addressed profile (empty for the plain default).
    pub role_additional: String,
    /// The `now` timestamp rendered into the Date and Time section.
    pub now: String,
    /// The working directory (the session's workspace root).
    pub cwd: String,
}

impl SystemPromptFixedVars {
    /// The default fixed variables: the standard host identity, no role
    /// text, and the given creation-time facts.
    pub(crate) fn new(now: String, cwd: String) -> Self {
        Self {
            product_name: DEFAULT_PRODUCT_NAME.to_string(),
            role_additional: String::new(),
            now,
            cwd,
        }
    }
}

/// Render the template's fixed part: the creation-time variables.
///
/// The workspace placeholders (`{{cwd_listing}}`, `{{agents_md}}`, the
/// `*_section` blocks) stay verbatim for the second pass.
pub(crate) fn render_fixed_part(vars: &SystemPromptFixedVars) -> String {
    let mut replacements = HashMap::new();
    replacements.insert("product_name".to_string(), vars.product_name.clone());
    replacements.insert("role_additional".to_string(), vars.role_additional.clone());
    replacements.insert(
        "reply_style_guide".to_string(),
        DEFAULT_REPLY_STYLE_GUIDE.to_string(),
    );
    replacements.insert("os".to_string(), os_kind());
    replacements.insert("shell".to_string(), shell_description());
    replacements.insert(
        "windows_notes".to_string(),
        if cfg!(windows) {
            format!("\n{WINDOWS_NOTES}\n")
        } else {
            String::new()
        },
    );
    replacements.insert("now".to_string(), vars.now.clone());
    replacements.insert("cwd".to_string(), vars.cwd.clone());
    render_system_prompt(SYSTEM_PROMPT_TEMPLATE, &Some(replacements))
}

/// The TS `ADDITIONAL_DIRS_SECTION_PROSE` — the lead-in of the
/// Additional Directories block (`profile-shared.ts`).
const ADDITIONAL_DIRS_SECTION_PROSE: &str = "The following directories have been added to the workspace. You can read, write, search, and glob files in these directories as part of your workspace scope.";

/// Render the template's `{{additional_dirs_section}}` slot from the
/// session's authorized additional dirs, mirroring the TS
/// `additional_dirs_section` (`profile-shared.ts:109-112`) and
/// `loadAdditionalDirsInfo` (`context.ts`) shapes: each dir becomes a
/// `### {dir}` heading plus its two-level tree (`list_directory`), sections
/// join with blank lines, and the whole block opens with the
/// `## Additional Directories` heading. An empty input renders empty; a
/// dir whose tree cannot be listed degrades to its heading alone (with a
/// warn) rather than failing the prompt.
fn render_additional_dirs_section(dirs: &[PathBuf]) -> String {
    if dirs.is_empty() {
        return String::new();
    }
    let mut sections = Vec::new();
    for dir in dirs {
        let heading = format!("### {}", dir.display());
        let listing = crate::context::dir_tree::list_directory(dir);
        if listing == "[not readable]" {
            tracing::warn!(
                dir = %dir.display(),
                "additional dir not readable; rendering its path only in the system prompt"
            );
            sections.push(heading);
        } else {
            sections.push(format!("{heading}\n{listing}"));
        }
    }
    format!(
        "\n\n## Additional Directories\n\n{ADDITIONAL_DIRS_SECTION_PROSE}\n\n{}\n\n",
        sections.join("\n\n")
    )
}

/// Substitute the workspace-derived variables into a fixed-part render.
///
/// `agents_md` is the already trust-gated merged content (the caller owns
/// the [`load_agents_md_for_trust`](crate::agents_md::load_agents_md_for_trust)
/// gate); `skills_section` is the model-visible skill listing rendered by
/// [`render_skills_section`]; `plugin_sections` is the already budget-capped
/// render of the plugin system-prompt contributions (see
/// [`render_plugin_sections`]); `additional_dirs` are the session's
/// authorized additional workspace directories, rendered into
/// `{{additional_dirs_section}}` (plan 2026-10-10-sandbox-path-permission
/// D5).
pub(crate) fn apply_workspace_part(
    fixed: &str,
    cwd_listing: &str,
    agents_md: &str,
    skills_section: &str,
    plugin_sections: &str,
    additional_dirs: &[PathBuf],
) -> String {
    let mut replacements = HashMap::new();
    replacements.insert("cwd_listing".to_string(), cwd_listing.to_string());
    replacements.insert("agents_md".to_string(), agents_md.to_string());
    replacements.insert(
        "additional_dirs_section".to_string(),
        render_additional_dirs_section(additional_dirs),
    );
    replacements.insert("skills_section".to_string(), skills_section.to_string());
    replacements.insert("plugin_sections".to_string(), plugin_sections.to_string());
    render_system_prompt(fixed, &Some(replacements))
}

/// The model-visible skill listing rendered into the template's
/// `{{skills_section}}` slot — the port of the TS
/// `InMemorySkillCatalog.getModelSkillListing`
/// (`skillCatalog/registry.ts`, design 13 §3.5).
///
/// Rendering rules (TS-aligned): the first line is the replacement
/// declaration `DISREGARD any earlier skill listings. Current available
/// skills:`; entries group by source — Project / User / Extra (the plugin
/// wire source) / Built-in, in that order, name-sorted within a group; only
/// model-invocable entries render (`disable_model_invocation` and
/// `is_sub_skill` entries are filtered out); each entry is
/// `- name: description` (description truncated at 247 chars + `...`, total
/// ≤ 250 — the TS `LISTING_DESC_MAX` cut, char-boundary per ADR-B5/T5)
/// followed by a `  Path:` line (the builtin pseudo path verbatim). An
/// empty post-filter listing renders the empty string — no declaration
/// line, no group headings (the empty-catalog behavior is unchanged).
///
/// Divergence from the TS embedding (disclosed, ADR-B4-adjacent): the TS
/// `profile-shared.ts` wraps this listing in a `# Skills` heading plus
/// section prose; the rust side renders the bare listing — the same
/// simplification the existing [`render_plugin_sections`] makes for its
/// TS `# Plugin Instructions` wrapper.
///
/// The caller is the prompt assembly path: the section is evaluated when
/// an agent's default-base prompt is built (new-session-effective; existing
/// sessions keep their assembly-time snapshot, matching the
/// plugin-sections refresh policy).
pub(crate) fn render_skills_section(catalog: &SkillCatalog) -> String {
    /// The TS `LISTING_DESC_MAX` — the description cut budget.
    const LISTING_DESC_MAX: usize = 250;

    let skills = catalog.list();
    let invocable: Vec<&SkillDef> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && !skill.is_sub_skill)
        .collect();
    if invocable.is_empty() {
        return String::new();
    }
    let mut lines =
        vec!["DISREGARD any earlier skill listings. Current available skills:".to_string()];
    // The TS `SOURCE_GROUPS` order: project, user, extra (the plugin wire
    // source), builtin — the catalog's list() is already name-sorted, so the
    // per-group filter preserves the within-group order.
    let groups = [
        (SkillSource::Project, "Project"),
        (SkillSource::User, "User"),
        (SkillSource::Plugin, "Extra"),
        (SkillSource::Builtin, "Built-in"),
    ];
    for (source, label) in groups {
        let group = invocable.iter().filter(|skill| skill.source == source);
        let mut pushed_heading = false;
        for skill in group {
            if !pushed_heading {
                lines.push(format!("### {label}"));
                pushed_heading = true;
            }
            lines.push(format!(
                "- {}: {}",
                skill.name,
                truncate_listing_description(&skill.description, LISTING_DESC_MAX)
            ));
            lines.push(format!("  Path: {}", skill.path.display()));
        }
    }
    lines.join("\n")
}

/// Truncate a listing description at the TS `truncate` cut
/// (`registry.ts`): values at or under `max` pass through; longer values
/// are cut at `max - 3` chars and suffixed with `...` (total ≤ `max`).
/// Char-boundary arithmetic, not grapheme-aware — the disclosed T5
/// simplification of the TS `Intl.Segmenter` behavior.
fn truncate_listing_description(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let cut: String = value.chars().take(max - 3).collect();
    format!("{cut}...")
}

/// The aggregate budget for plugin system-prompt contributions — the TS
/// `PLUGIN_SECTIONS_MAX_BYTES` (`profileService.ts` 64 KiB).
pub const PLUGIN_SECTIONS_MAX_BYTES: usize = 64 * 1024;

/// Render the plugin system-prompt contributions into the template's
/// `{{plugin_sections}}` slot, mirroring the TS `resolvePluginSections`
/// (`profileService.ts`): each section becomes a
/// `<!-- From: plugin {pluginId} -->\n{content}` block, blocks join with a
/// blank line, and the aggregate is budget-capped at
/// [`PLUGIN_SECTIONS_MAX_BYTES`] — an overflowing section is skipped with a
/// warn (the TS budget-skip + warning-event behavior, minus the event).
pub(crate) fn render_plugin_sections(sections: &[PluginSystemPromptSection]) -> String {
    let mut parts = Vec::new();
    let mut total_bytes = 0usize;
    for section in sections {
        let block = format!(
            "<!-- From: plugin {} -->\n{}",
            section.plugin_id, section.content
        );
        let bytes = block.len();
        if total_bytes + bytes > PLUGIN_SECTIONS_MAX_BYTES {
            tracing::warn!(
                plugin = %section.plugin_id,
                "plugin system-prompt contribution skipped: the aggregate 64 KB budget is exhausted"
            );
            continue;
        }
        total_bytes += bytes;
        parts.push(block);
    }
    parts.join("\n\n")
}

/// The human OS kind, mirroring the TS `osKind` values.
fn os_kind() -> String {
    match std::env::consts::OS {
        "macos" => "macOS".to_string(),
        "windows" => "Windows".to_string(),
        "linux" => "Linux".to_string(),
        other => other.to_string(),
    }
}

/// The shell line — "`/bin/bash` (`zsh`)" shaped like the TS context's
/// `${shellName} (\`${shellPath}\`)`.
fn shell_description() -> String {
    let path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "bash".to_string());
    format!("`{path}` (`{name}`)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_vars() -> SystemPromptFixedVars {
        SystemPromptFixedVars {
            product_name: DEFAULT_PRODUCT_NAME.to_string(),
            role_additional: String::new(),
            now: "2026-09-16T13:30:11.413Z".to_string(),
            cwd: "/tmp/test-workspace".to_string(),
        }
    }

    /// Given the fixed-part render, When inspecting the output, Then every
    /// fixed placeholder is substituted while the workspace placeholders
    /// stay verbatim for the second pass.
    #[test]
    fn should_substitute_the_fixed_part_and_keep_the_workspace_placeholders() {
        let fixed = render_fixed_part(&fixed_vars());

        assert!(fixed.contains("You are Mirri Code CLI,"));
        assert!(fixed.contains(&format!("running on **{}**", os_kind())));
        assert!(fixed.contains("/tmp/test-workspace"));
        assert!(fixed.contains("2026-09-16T13:30:11.413Z"));
        assert!(
            fixed.contains("{{cwd_listing}}"),
            "the workspace placeholders must survive the fixed pass"
        );
        assert!(fixed.contains("{{agents_md}}"));
    }

    /// Given a fixed-part render, When applying the workspace part, Then the
    /// directory tree and AGENTS.md content land inside their template
    /// sections and no placeholder remains.
    #[test]
    fn should_fill_the_workspace_part_without_leaving_placeholders() {
        let fixed = render_fixed_part(&fixed_vars());
        let rendered = apply_workspace_part(
            &fixed,
            "src/\n  main.rs",
            "PROJECT_RULES_MARKER",
            "",
            "",
            &[],
        );

        assert!(rendered.contains("src/\n  main.rs"));
        assert!(rendered.contains("PROJECT_RULES_MARKER"));
        assert!(
            !rendered.contains("{{"),
            "no placeholder may survive: {rendered}"
        );
    }

    /// Given a fixed-part render, When applying the workspace part, Then the
    /// template's section order holds — the language directive
    /// precedes the AGENTS.md section (the same order the TS prompt uses).
    #[test]
    fn should_place_the_language_directive_ahead_of_the_agents_md_section() {
        let fixed = render_fixed_part(&fixed_vars());
        let rendered = apply_workspace_part(&fixed, "tree", "AGENTS_MD_MARKER", "", "", &[]);

        let directive = rendered
            .find("Write in the user's language")
            .expect("directive present");
        let agents_md = rendered
            .find("AGENTS_MD_MARKER")
            .expect("AGENTS.md present");
        assert!(directive < agents_md);
    }

    /// Given plugin system-prompt contributions, When the workspace part is
    /// applied, Then each section renders as a `<!-- From: plugin {id} -->`
    /// block inside the template's plugin slot, joined with blank lines —
    /// the TS `resolvePluginSections` shape (item J).
    #[test]
    fn should_render_plugin_sections_when_contributions_exist() {
        let sections = vec![
            PluginSystemPromptSection {
                plugin_id: "alpha".to_string(),
                content: "First part.".to_string(),
            },
            PluginSystemPromptSection {
                plugin_id: "beta".to_string(),
                content: "Second part.".to_string(),
            },
        ];
        let rendered = render_plugin_sections(&sections);

        assert_eq!(
            rendered,
            "<!-- From: plugin alpha -->\nFirst part.\n\n<!-- From: plugin beta -->\nSecond part."
        );

        // The fixed part renders the slot content verbatim.
        let fixed = render_fixed_part(&fixed_vars());
        let full = apply_workspace_part(&fixed, "tree", "md", "", &rendered, &[]);
        assert!(full.contains("<!-- From: plugin alpha -->\nFirst part."));
        assert!(full.contains("<!-- From: plugin beta -->\nSecond part."));
        assert!(!full.contains("{{plugin_sections}}"));
    }

    /// Given plugin contributions whose aggregate exceeds the 64 KiB budget,
    /// When the sections render, Then the overflowing sections are skipped
    /// with a warn and the in-budget ones survive in order (the TS
    /// `PLUGIN_SECTIONS_MAX_BYTES` cap).
    #[test]
    fn should_skip_plugin_sections_when_aggregate_budget_is_exhausted() {
        let small = PluginSystemPromptSection {
            plugin_id: "small".to_string(),
            content: "x".repeat(1024),
        };
        let huge = PluginSystemPromptSection {
            plugin_id: "huge".to_string(),
            content: "x".repeat(PLUGIN_SECTIONS_MAX_BYTES),
        };
        let sections = vec![small.clone(), huge.clone(), small];

        let rendered = render_plugin_sections(&sections);

        assert!(rendered.contains("<!-- From: plugin small -->"));
        assert!(!rendered.contains("<!-- From: plugin huge -->"));
        assert_eq!(
            rendered,
            format!(
                "<!-- From: plugin small -->\n{}\n\n<!-- From: plugin small -->\n{}",
                "x".repeat(1024),
                "x".repeat(1024)
            )
        );
    }

    /// Given no plugin contributions, When the sections render, Then the
    /// slot stays empty.
    #[test]
    fn should_render_empty_plugin_sections_when_no_contributions_exist() {
        assert_eq!(render_plugin_sections(&[]), "");
    }

    // ===== model-visible skill listing (design 13 §3.5, M2 model plane) =====

    /// Write a single discovered skill directory `skills/<name>` carrying
    /// `content` as its `SKILL.md`.
    fn write_skill_md(name: &str, content: &str) -> tempfile::TempDir {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::skill::SKILL_MD_FILE), content).unwrap();
        temp
    }

    /// Write a single discovered skill directory `skills/<name>` with the
    /// given frontmatter description.
    fn write_skill(name: &str, description: &str) -> tempfile::TempDir {
        write_skill_md(
            name,
            &format!("---\ndescription: {description}\n---\n# {name}\n"),
        )
    }

    /// Given a catalog holding project, user, plugin (extra) and builtin
    /// skills, When the skills section renders, Then the first line is the
    /// replacement declaration and the entries group in the TS order —
    /// Project / User / Extra / Built-in — name-sorted within each group,
    /// each entry carrying the `- name: description` plus `Path:` lines
    /// (V4 — the grouping half).
    #[test]
    fn should_render_the_grouped_listing_when_project_user_extra_and_builtin_skills_exist() {
        let project = write_skill("zebra", "a project skill");
        let user = write_skill("alpha", "a user skill");
        let extra = write_skill("midway", "a plugin skill");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&project.path().join("skills"), SkillSource::Project)
            .unwrap();
        catalog
            .discover(&user.path().join("skills"), SkillSource::User)
            .unwrap();
        catalog
            .discover_plugin(
                &extra.path().join("skills"),
                &crate::skill::SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();
        catalog.register_builtin(crate::skill::builtin::builtin_skills());

        let section = render_skills_section(&catalog);

        // The first line is the replacement declaration, verbatim.
        assert!(
            section
                .starts_with("DISREGARD any earlier skill listings. Current available skills:\n"),
            "the declaration opens the section: {section}"
        );
        // The four groups render in the TS SOURCE_GROUPS order.
        let project_pos = section.find("### Project").expect("Project group");
        let user_pos = section.find("### User").expect("User group");
        let extra_pos = section.find("### Extra").expect("Extra group");
        let builtin_pos = section.find("### Built-in").expect("Built-in group");
        assert!(
            project_pos < user_pos && user_pos < extra_pos && extra_pos < builtin_pos,
            "group order Project < User < Extra < Built-in: {section}"
        );
        // Entry shape: `- name: description` plus the indented Path line;
        // the builtin pseudo path rides verbatim.
        assert!(section.contains("- zebra: a project skill"));
        assert!(section.contains("- alpha: a user skill"));
        assert!(section.contains("- midway: a plugin skill"));
        assert!(section.contains("- write-goal: "));
        assert!(section.contains("  Path: builtin://write-goal"));
        // The Built-in group holds exactly the two model-invocable builtins,
        // name-sorted (update-config before write-goal).
        let builtin_part = &section[builtin_pos + "### Built-in".len()..];
        let update_pos = builtin_part.find("- update-config: ").unwrap();
        let goal_pos = builtin_part.find("- write-goal: ").unwrap();
        assert!(
            update_pos < goal_pos,
            "within-group name sort: {builtin_part}"
        );
        assert!(
            !builtin_part.contains("### "),
            "no group heading after Built-in: {builtin_part}"
        );

        // The section lands in the template slot when the workspace part
        // applies (the pipeline wiring half).
        let fixed = render_fixed_part(&fixed_vars());
        let prompt = apply_workspace_part(&fixed, "tree", "md", &section, "", &[]);
        assert!(prompt.contains("DISREGARD any earlier skill listings."));
        assert!(!prompt.contains("{{skills_section}}"));
    }

    /// Given a catalog whose only entries are model-gated, When the skills
    /// section renders, Then the gated entries are filtered out —
    /// `disable_model_invocation` skills (mcp-config, custom-theme,
    /// import-from-cc-codex, the sub-skill family) and `is_sub_skill`
    /// entries never appear (V4 — the filtering half).
    #[test]
    fn should_filter_gated_entries_when_rendering_the_model_listing() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin(crate::skill::builtin::builtin_skills());

        let section = render_skills_section(&catalog);

        for gated in [
            "mcp-config",
            "custom-theme",
            "import-from-cc-codex",
            "sub-skill",
            "sub-skill.review",
            "sub-skill.consolidate",
        ] {
            assert!(
                !section.contains(gated),
                "{gated} must be filtered out of the model listing: {section}"
            );
        }
        assert!(section.contains("- update-config: "));
        assert!(section.contains("- write-goal: "));
    }

    /// Given a skill whose description exceeds 250 chars, When the listing
    /// renders, Then the description is cut at 247 chars and suffixed with
    /// `...` — the total description never exceeds 250 (the TS
    /// `LISTING_DESC_MAX` cut, char-boundary per T5).
    #[test]
    fn should_truncate_long_descriptions_with_an_ellipsis_when_rendering_the_listing() {
        let skill = write_skill("long", &"x".repeat(300));
        let catalog = SkillCatalog::new();
        catalog
            .discover(&skill.path().join("skills"), SkillSource::User)
            .unwrap();

        let section = render_skills_section(&catalog);

        let line = section
            .lines()
            .find(|line| line.starts_with("- long: "))
            .expect("the long-description entry renders");
        let description = line.strip_prefix("- long: ").unwrap();
        assert!(
            description.ends_with("..."),
            "cut ends with an ellipsis: {line}"
        );
        assert_eq!(description.chars().count(), 250, "the cut stays in budget");
        assert!(description.starts_with(&"x".repeat(247)));
    }

    /// Given a catalog whose every entry is filtered out, When the skills
    /// section renders, Then the section is the empty string — no
    /// declaration line, no group headings (the empty-catalog behavior is
    /// unchanged).
    #[test]
    fn should_render_an_empty_section_when_every_skill_is_filtered_out() {
        let gated = write_skill_md(
            "gated",
            "---\ndescription: never listed\ndisable-model-invocation: true\n---\n# Gated\n",
        );
        let catalog = SkillCatalog::new();
        catalog
            .discover(&gated.path().join("skills"), SkillSource::User)
            .unwrap();

        assert_eq!(render_skills_section(&catalog), "");
    }

    /// Given a rendered listing, When the catalog later changes, Then the
    /// already-rendered section is untouched — the assembly path bakes the
    /// string at prompt-build time (new sessions see the new state;
    /// existing prompts keep their snapshot, the plugin-sections policy).
    #[test]
    fn should_snapshot_the_listing_at_render_time_when_the_catalog_later_changes() {
        let early = write_skill("early", "the early skill");
        let catalog = SkillCatalog::new();
        catalog
            .discover(&early.path().join("skills"), SkillSource::User)
            .unwrap();

        let snapshot = render_skills_section(&catalog);

        let late = write_skill("late", "the late skill");
        catalog
            .discover(&late.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(
            !snapshot.contains("late"),
            "the rendered section is a snapshot, not a live view: {snapshot}"
        );
        assert!(render_skills_section(&catalog).contains("- late: "));
    }

    // -----------------------------------------------------------------------
    // Task 5 (plan 2026-10-10-sandbox-path-permission D5): the Path Sandbox
    // contract and the `{{additional_dirs_section}}` render.
    // -----------------------------------------------------------------------

    /// Given a fixed-part render and registered additional dirs, When the
    /// workspace part is applied, Then each dir renders as a
    /// `### {dir}` heading followed by its two-level tree inside a
    /// `## Additional Directories` block, in insertion order — the TS
    /// `profile-shared.ts` `additional_dirs_section` shape.
    #[test]
    fn should_render_the_additional_dirs_section_when_dirs_are_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let assets = tmp.path().join("assets");
        let docs = tmp.path().join("docs");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(assets.join("logo.png"), b"x").unwrap();
        std::fs::write(docs.join("readme.md"), b"x").unwrap();

        let fixed = render_fixed_part(&fixed_vars());
        let rendered = apply_workspace_part(
            &fixed,
            "tree",
            "md",
            "",
            "",
            &[assets.clone(), docs.clone()],
        );

        assert!(rendered.contains("## Additional Directories"));
        assert!(rendered.contains(
            "The following directories have been added to the workspace. You can read, write, search, and glob files in these directories as part of your workspace scope."
        ));
        let assets_heading = format!("### {}", assets.display());
        let docs_heading = format!("### {}", docs.display());
        assert!(
            rendered.contains(&assets_heading),
            "heading for the first dir"
        );
        assert!(
            rendered.contains(&docs_heading),
            "heading for the second dir"
        );
        assert!(
            rendered.find(&assets_heading).unwrap() < rendered.find(&docs_heading).unwrap(),
            "dirs render in insertion order"
        );
        assert!(
            rendered.contains("logo.png"),
            "the first dir's tree content rides below its heading"
        );
        assert!(
            rendered.contains("readme.md"),
            "the second dir's tree content rides below its heading"
        );
    }

    /// Given a fixed-part render and no additional dirs, When the workspace
    /// part is applied, Then the additional-dirs slot renders empty (the
    /// current behavior with no authorized dirs).
    #[test]
    fn should_render_an_empty_additional_dirs_section_when_no_dirs_are_registered() {
        let fixed = render_fixed_part(&fixed_vars());
        let rendered = apply_workspace_part(&fixed, "tree", "md", "", "", &[]);

        assert!(!rendered.contains("## Additional Directories"));
        assert!(!rendered.contains("### "));
    }

    /// Given an additional dir whose tree cannot be listed, When the
    /// workspace part is applied, Then the render degrades to the path
    /// heading alone (with a warn) instead of failing the whole prompt.
    #[test]
    fn should_degrade_an_unreadable_additional_dir_to_its_heading_only() {
        let missing = std::path::PathBuf::from("/definitely/not/a/real/dir-on-any-host");
        let fixed = render_fixed_part(&fixed_vars());
        let rendered =
            apply_workspace_part(&fixed, "tree", "md", "", "", std::slice::from_ref(&missing));

        assert!(rendered.contains(&format!("### {}", missing.display())));
        assert!(
            !rendered.contains("[not readable]"),
            "an unlistable dir shows its path only: {rendered}"
        );
    }

    /// Given the rendered template, When the prompt is inspected, Then it
    /// carries the static Path Sandbox contract — the sandbox scope
    /// declaration and the NEVER-bypass directive for denied paths.
    #[test]
    fn should_carry_the_path_sandbox_contract_in_the_rendered_prompt() {
        let fixed = render_fixed_part(&fixed_vars());
        let rendered = apply_workspace_part(&fixed, "tree", "md", "", "", &[]);

        assert!(rendered.contains("## Path Sandbox"));
        assert!(rendered.contains("File and search tools operate inside the workspace sandbox"));
        assert!(rendered.contains("NEVER attempt to access the path through any other means"));
    }
}
