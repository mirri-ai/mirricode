//! Skills — discoverable, disk-backed agent capabilities.
//!
//! A skill is a directory containing a `SKILL.md` file (plus optional helper
//! scripts). [`SkillCatalog`] discovers skills under a root directory (e.g.
//! `.agents/skills/`), indexes them by their directory name, and serves their
//! content on demand.
//!
//! The skill name is the basename of the directory holding `SKILL.md`:
//!
//! ```text
//! .agents/skills/
//! ├── git-commit/          → skill "git-commit"
//! │   ├── SKILL.md
//! │   └── commit.sh
//! └── pdf/                 → skill "pdf"
//!     └── SKILL.md
//! ```
//!
//! Discovery is recursive, so nested categories are supported. A `description`
//! is read from the skill's YAML-ish frontmatter block when present:
//!
//! ```markdown
//! ---
//! description: Creates conventional commit messages.
//! ---
//! # Body...
//! ```
//!
//! When multiple discovery roots index the same skill name, the most recent
//! [`discover`](SkillCatalog::discover) call wins (e.g. a user skill shadows a
//! builtin of the same name).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use mirri_os::SyncFs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The builtin skill bundle — code-defined (not disk-discovered) skill
/// definitions with compile-time-embedded bodies (design 13 §3.1, ADR-B1).
pub mod builtin;

/// The filename every skill directory must contain.
pub const SKILL_MD_FILE: &str = "SKILL.md";

/// Where a skill definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Bundled with the agent runtime.
    Builtin,
    /// Installed by the current user (e.g. `~/.agents/skills/`).
    User,
    /// Checked into the project (e.g. `.agents/skills/`).
    Project,
    /// Provided by a plugin.
    Plugin,
}

/// A skill definition loaded from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDef {
    /// Skill name — the basename of its directory. Unique within a catalog.
    pub name: String,
    /// Human-readable description parsed from `SKILL.md` frontmatter
    /// (`description:` key); empty when the file has none.
    pub description: String,
    /// Directory containing this skill's `SKILL.md` and helper files.
    pub path: PathBuf,
    /// Source of this skill definition.
    pub source: SkillSource,
    /// The plugin attribution of a plugin-contributed skill — the TS
    /// `SkillDefinition.plugin` (`skillCatalog/types.ts`:
    /// `{ id, instructions? }`). `None` for non-plugin skills. Consumed at
    /// skill prompt rendering (item J): instructions are prepended as a
    /// `<kimi-plugin-instructions>` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<SkillPluginContext>,
    /// Model-invocation gate (TS `disableModelInvocation`, design
    /// `13-builtin-skills.md` §3.2 / ADR-B2): when `true`, the skill may
    /// only be triggered by the user (slash activation), never by the model
    /// through the Skill tool. Parsed from the frontmatter key
    /// `disable-model-invocation` (hyphen/underscore aliases), so user
    /// skills declaring it get the same gate as builtin skills.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_model_invocation: bool,
    /// Sub-skill marker (TS `isSubSkill`, design 13 §3.2 / ADR-B2): when
    /// `true`, the skill is a child of a `has-sub-skill` parent bundle and
    /// is filtered out of the model-visible skill listing. Deliberately
    /// never sent over the wire (TS drops it too — both ends agree). Parsed
    /// from the frontmatter key `is-sub-skill` (hyphen/underscore aliases).
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_sub_skill: bool,
}

/// Serde predicate shared by the defaulted boolean metadata fields — a
/// `false` flag is omitted from the payload (the `plugin` field's
/// `Option::is_none` precedent, applied to bools).
fn is_false(b: &bool) -> bool {
    !*b
}

/// The plugin attribution of a discovered plugin skill — the TS
/// `SkillPluginContext` (`skillCatalog/types.ts`: `{ id, instructions? }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillPluginContext {
    /// The contributing plugin's id.
    pub id: String,
    /// The manifest `skillInstructions` attached to the root (carried from
    /// parse time, item J).
    pub instructions: Option<String>,
}

impl std::fmt::Debug for SkillCatalog {
    /// Manual `Debug`: the injected [`SyncFs`] provider has no `Debug` impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillCatalog").finish_non_exhaustive()
    }
}

/// Errors produced by skill discovery.
#[derive(Debug, Error)]
pub enum SkillError {
    /// A directory in the discovery tree could not be scanned.
    #[error("skill discovery failed: {0}")]
    DiscoveryFailed(#[from] std::io::Error),
}

/// Skill catalog — discovers and indexes skills.
///
/// The catalog is workspace-scoped: one catalog per workspace indexes the
/// skills visible to every session in that workspace. All operations are
/// synchronous; the catalog is cheaply shareable behind an `Arc`.
///
/// All disk access goes through the injected [`SyncFs`] provider (zero-IO
/// T13b): [`SkillCatalog::new`] uses the engine default ([`LocalSyncFs`],
/// adjudication J3), and tests inject an in-memory double via
/// [`SkillCatalog::with_fs`].
pub struct SkillCatalog {
    skills: RwLock<HashMap<String, SkillDef>>,
    /// The builtin content table (design 13 §3.1, ADR-B1): embedded
    /// `SKILL.md` bodies keyed by skill name, populated by
    /// [`SkillCatalog::register_builtin`] and consulted by
    /// [`SkillCatalog::read_content`] for `Builtin`-source entries. The
    /// residue after a same-name user override is never hit (the entry's
    /// source is no longer `Builtin`, so reads fall back to disk).
    builtin_contents: RwLock<HashMap<String, &'static str>>,
    /// User-level disabled skill names (B8 D-32). Catalog-level state,
    /// deliberately *not* part of [`SkillDef`]'s serde shape: it is seeded
    /// from the user config's `disabled_skills` key at assembly time and
    /// resynced on config writes, not persisted per-skill.
    user_disabled: RwLock<HashSet<String>>,
    fs: Arc<dyn SyncFs>,
}

impl Default for SkillCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// The name-classified change set between two catalog snapshots (ADR-010
/// decision 4): every name that differs between the `before` and `after`
/// whole-table snapshots lands in exactly one bucket, at the listing-
/// visible field level (a body-only edit never appears — the listing is
/// the notification's content, and it did not change).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillCatalogDiff {
    /// Names present only in the after-snapshot.
    pub added: Vec<String>,
    /// Names present only in the before-snapshot.
    pub removed: Vec<String>,
    /// Names present in both whose definitions are not equal.
    pub updated: Vec<String>,
}

/// Diff two whole-catalog snapshots by name (ADR-010 decision 4): a name
/// only in `after` is added, only in `before` is removed, and one present in
/// both with an unequal [`SkillDef`] (all fields — the `PartialEq` derive
/// covers the listing-visible set) is updated. Every bucket is name-sorted
/// so the output is deterministic.
pub fn diff_skill_catalogs(
    before: &HashMap<String, SkillDef>,
    after: &HashMap<String, SkillDef>,
) -> SkillCatalogDiff {
    let mut diff = SkillCatalogDiff::default();
    for name in after.keys() {
        if !before.contains_key(name) {
            diff.added.push(name.clone());
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            diff.removed.push(name.clone());
        }
    }
    for (name, before_def) in before {
        if let Some(after_def) = after.get(name)
            && after_def != before_def
        {
            diff.updated.push(name.clone());
        }
    }
    diff.added.sort();
    diff.removed.sort();
    diff.updated.sort();
    diff
}

impl SkillCatalog {
    /// Create an empty skill catalog backed by the engine-default local
    /// filesystem ([`mirri_os::LocalSyncFs`]).
    pub fn new() -> Self {
        Self::with_fs(Arc::new(mirri_os::LocalSyncFs))
    }

    /// Create an empty skill catalog backed by an explicit [`SyncFs`]
    /// provider.
    pub fn with_fs(fs: Arc<dyn SyncFs>) -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            builtin_contents: RwLock::new(HashMap::new()),
            user_disabled: RwLock::new(HashSet::new()),
            fs,
        }
    }

    /// Register builtin skill definitions (design 13 §3.1): each entry
    /// lands in the skills map and its embedded body in the builtin
    /// content table, so [`Self::read_content`] serves it without touching
    /// the disk (the pseudo `builtin://` path holds no file).
    ///
    /// The composition root calls this first in the discovery replay —
    /// right after [`Self::clear`], before every other channel — so the
    /// catalog's later-wins rule makes the bundle the lowest-priority
    /// source (the TS `builtin(0)` floor, ADR-B3). Later registrations of
    /// the same name win, mirroring [`Self::discover`].
    pub fn register_builtin(&self, skills: impl IntoIterator<Item = builtin::BuiltinSkillDef>) {
        let mut map = self
            .skills
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut contents = self
            .builtin_contents
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for entry in skills {
            if let Some(previous) = map.get(&entry.def.name)
                && previous.source != entry.def.source
            {
                tracing::warn!(
                    skill = %entry.def.name,
                    previous_source = ?previous.source,
                    source = ?entry.def.source,
                    "skill shadowed by a later discovery"
                );
            }
            contents.insert(entry.def.name.clone(), entry.content);
            map.insert(entry.def.name.clone(), entry.def);
        }
    }

    /// Discover skills from a directory (e.g. `.agents/skills/`).
    ///
    /// Recursively scans `dir`; every directory containing a `SKILL.md` file
    /// becomes a skill named after that directory. Returns the number of
    /// skill directories processed. Skills that already have the same entry
    /// are overwritten — later discoveries win, so a user skill shadows a
    /// builtin of the same name.
    ///
    /// A non-existent `dir` is not an error (skill directories are optional)
    /// and yields `Ok(0)`.
    pub fn discover(&self, dir: &Path, source: SkillSource) -> Result<usize, SkillError> {
        self.discover_impl(dir, source, None)
    }

    /// Discover a plugin skill root with its plugin attribution (item J —
    /// the TS `pluginSkillRoots` → `discoverFileSkills` channel).
    ///
    /// Equivalent to [`Self::discover`] with [`SkillSource::Plugin`], except
    /// every indexed skill carries the root's plugin context (`{id,
    /// instructions}`), so skill-prompt rendering can prepend the
    /// `skillInstructions` and the session-start lookup can resolve by
    /// plugin id.
    pub fn discover_plugin(
        &self,
        dir: &Path,
        plugin: &SkillPluginContext,
    ) -> Result<usize, SkillError> {
        self.discover_impl(dir, SkillSource::Plugin, Some(plugin.clone()))
    }

    fn discover_impl(
        &self,
        dir: &Path,
        source: SkillSource,
        plugin: Option<SkillPluginContext>,
    ) -> Result<usize, SkillError> {
        let fs = self.fs.as_ref();
        if !fs.exists(dir) {
            tracing::debug!(path = %dir.display(), "skill directory absent; skipping discovery");
            return Ok(0);
        }
        let skill_dirs = find_skill_directories(dir, fs)?;
        let mut skills = self
            .skills
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for skill_dir in &skill_dirs {
            let Some(name) = skill_dir.file_name().and_then(|name| name.to_str()) else {
                tracing::warn!(path = %skill_dir.display(), "skill directory has no valid name; skipping");
                continue;
            };
            let meta = read_frontmatter(&skill_dir.join(SKILL_MD_FILE), fs);
            if let Some(previous) = skills.get(name)
                && previous.source != source
            {
                tracing::warn!(
                    skill = name,
                    previous_source = ?previous.source,
                    source = ?source,
                    "skill shadowed by a later discovery"
                );
            }
            skills.insert(
                name.to_string(),
                SkillDef {
                    name: name.to_string(),
                    description: meta.description,
                    path: skill_dir.clone(),
                    source,
                    plugin: plugin.clone(),
                    disable_model_invocation: meta.disable_model_invocation,
                    is_sub_skill: meta.is_sub_skill,
                },
            );
        }
        Ok(skill_dirs.len())
    }

    /// Remove every discovered skill from the catalog.
    ///
    /// Composition roots use this when explicit skill directories replace
    /// the default discovery: the default project/user roots were already
    /// scanned by [`WorkspaceScope::new`](crate::scope::WorkspaceScope::new),
    /// so the catalog is reset before the explicit roots are discovered
    /// (the v1/v2 SDK `skillDirs` contract — non-empty dirs *replace*, not
    /// extend, the default user/project skill set).
    pub fn clear(&self) {
        self.skills
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Clone the whole current skills table (ADR-010 decision 3): the
    /// pre-rescan snapshot a caller hands to
    /// [`diff_skill_catalogs`](crate::skill::diff_skill_catalogs) to gate
    /// the change notification.
    pub fn snapshot(&self) -> HashMap<String, SkillDef> {
        self.skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Clone the whole current builtin-content table — the scratch-build
    /// counterpart of [`Self::snapshot`]: the rescan orchestration replays
    /// discovery onto a scratch catalog, then hands BOTH tables to
    /// [`Self::replace_all`] so a stale builtin body never survives the
    /// swap (a builtin dropped by the replay must not keep serving its old
    /// content through `read_content`).
    pub fn snapshot_builtin_contents(&self) -> HashMap<String, &'static str> {
        self.builtin_contents
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Atomically replace the whole skills table and the builtin content
    /// table in single write locks (ADR-010 decision 3): the rescan
    /// orchestration builds the fresh tables off to the side (a scratch
    /// catalog) and swaps them in here, so readers never observe a
    /// mid-rescan empty catalog or a transient not-found — the write lock
    /// covers only the swap instant. The user-disabled set is deliberately
    /// NOT part of the swap: it is runtime toggle state (seeded from
    /// config, mutated through [`Self::set_enabled`]), not discovered
    /// content, so it survives the replacement untouched.
    pub fn replace_all(
        &self,
        skills: HashMap<String, SkillDef>,
        builtin_contents: HashMap<String, &'static str>,
    ) {
        *self
            .skills
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = skills;
        *self
            .builtin_contents
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = builtin_contents;
    }

    /// List all discovered skills, sorted by name for deterministic ordering.
    pub fn list(&self) -> Vec<SkillDef> {
        let mut skills: Vec<SkillDef> = self
            .skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<SkillDef> {
        self.skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    /// Read the `SKILL.md` content for a skill.
    ///
    /// `Builtin`-source entries are served from the builtin content table
    /// (the compile-time-embedded bodies — design 13 §3.1); every other
    /// source — and a builtin shadowed by a later same-name discovery, or a
    /// table miss — reads the `SKILL.md` next to the skill's `path`. Returns
    /// `None` when the skill is unknown or the content cannot be read (a
    /// warning is logged in the latter case). Reads go through the
    /// catalog's injected [`SyncFs`] provider.
    pub fn read_content(&self, name: &str) -> Option<String> {
        let skill = self.get(name)?;
        if skill.source == SkillSource::Builtin
            && let Some(content) = self
                .builtin_contents
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(name)
        {
            return Some((*content).to_string());
        }
        let path = skill.path.join(SKILL_MD_FILE);
        match self.fs.read_text(&path) {
            Ok(content) => Some(content),
            Err(error) => {
                tracing::warn!(skill = name, path = %path.display(), %error, "failed to read SKILL.md");
                None
            }
        }
    }

    /// Resolve a plugin-contributed skill by `(plugin_id, name)` — the TS
    /// `SkillCatalog.getPluginSkill` (`byPluginAndName` index).
    ///
    /// Returns `None` when no skill of that name carries the plugin's
    /// attribution. Skills registered without a plugin context never match.
    pub fn get_plugin_skill(&self, plugin_id: &str, name: &str) -> Option<SkillDef> {
        let skills = self
            .skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        skills.get(name).cloned().filter(|skill| {
            skill
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.id == plugin_id)
        })
    }

    /// Resolve a skill reference — a bare skill name or a plugin-qualified
    /// `plugin_id:skill_name` form (SDR-002 / ADR-001), the reference shape
    /// the model and the user-slash activation face may use to disambiguate
    /// a plugin-contributed skill.
    ///
    /// Resolution order: the reference is first tried as an exact bare name
    /// ([`Self::get`]) — existing semantics unchanged, so a skill whose
    /// directory name literally contains a colon is still found bare-first.
    /// Only on a bare miss and when the reference contains a `:` is it split
    /// once on the first colon into `(plugin_id, skill_name)` and handed to
    /// [`Self::get_plugin_skill`]; the plugin id segment is lower-cased
    /// before the lookup (the plugin id charset contract is lowercase —
    /// defensive normalization, e.g. `Demo:greeter` matches plugin `demo`).
    /// Empty segments never match, and there is no prefix-stripping
    /// fallback: an unknown plugin prefix stays a miss even when a bare
    /// skill of the trailing name exists.
    pub fn resolve(&self, reference: &str) -> Option<SkillDef> {
        if let Some(skill) = self.get(reference) {
            return Some(skill);
        }
        let (plugin_id, name) = reference.split_once(':')?;
        if plugin_id.is_empty() || name.is_empty() {
            return None;
        }
        self.get_plugin_skill(&plugin_id.to_lowercase(), name)
    }

    /// Resolve a skill by its `SKILL.md` FILE path — the message-inline
    /// reference resolution authority (SDR-005 / ADR-004: "链接指向哪个技能文件
    /// 就激活哪个副本"). A reference target is the absolute `SKILL.md` path
    /// (`<dir>/SKILL.md`); this matches a skill whose directory is exactly that
    /// path's parent.
    ///
    /// `builtin://` pseudo-paths (ADR-007 D4) resolve by EXACT pseudo-path
    /// equality against registered skills — builtin skills are registered with
    /// `path: PathBuf::from("builtin://<name>")`, so the target's `builtin://`
    /// prefix + name is the whole authority; there is no file on disk behind
    /// it. The dirty historical `builtin://<name>/SKILL.md` form (persisted by
    /// pre-fix insertion sites) is tolerated here too: the parser normally
    /// normalizes at admission time, but replay-time journal targets may skip
    /// the parser, so reusing the parser's normalization at resolve time
    /// (`normalize_builtin_target` — one consistent point, case-insensitive)
    /// keeps both faces clean. Unknown builtin pseudo-paths fail soft (`None`)
    /// exactly like unknown disk paths.
    ///
    /// Return `None` when no registered skill matches (a missing/unknown
    /// skill — the caller decides the fail-soft handling). For *disk* targets
    /// only a *file*-shaped target (basename exactly `SKILL.md`) is
    /// considered; any other basename never matches.
    pub fn resolve_by_skill_md_path(&self, target: &str) -> Option<SkillDef> {
        let skills = self
            .skills
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if target.starts_with(super::skill_reference::BUILTIN_TARGET_PREFIX) {
            // Builtin pseudo-path resolution: exact equality on the registered
            // pseudo path (builtin skills are registered with
            // `path: PathBuf::from("builtin://<name>")`). Normalizing here too
            // tolerates replay-time journal targets that skip the parser.
            let pseudo_path = super::skill_reference::normalize_builtin_target(target);
            return skills
                .values()
                .find(|&skill| skill.path.as_os_str() == pseudo_path.as_str())
                .cloned();
        }
        let path = Path::new(target);
        if path.file_name().and_then(|n| n.to_str()) != Some(SKILL_MD_FILE) {
            return None;
        }
        let dir = path.parent()?;
        skills.values().find(|&skill| skill.path == dir).cloned()
    }

    /// Render a skill's prompt the way the TS `renderSkillPrompt(skill, '')`
    /// does (item J): the raw `SKILL.md` body, prefixed with the plugin's
    /// `<kimi-plugin-instructions>` block when the skill carries a plugin
    /// attribution with non-empty `skillInstructions`.
    ///
    /// Returns `None` when the skill is unknown or its content cannot be
    /// read (the same failure surface as [`Self::read_content`]). The plugin
    /// id is XML-attribute escaped, matching the TS `escapeXmlAttr` usage.
    pub fn render_skill_prompt(&self, name: &str) -> Option<String> {
        let content = self.read_content(name)?;
        let skill = self.get(name)?;
        let instructions = skill
            .plugin
            .as_ref()
            .and_then(|plugin| plugin.instructions.as_deref())
            .filter(|instructions| !instructions.trim().is_empty());
        match instructions {
            Some(instructions) => {
                let plugin_id = skill
                    .plugin
                    .as_ref()
                    .map(|plugin| escape_xml_attr(&plugin.id))
                    .unwrap_or_default();
                Some(format!(
                    "<kimi-plugin-instructions plugin=\"{plugin_id}\">\n{instructions}\n</kimi-plugin-instructions>\n\n{content}"
                ))
            }
            None => Some(content),
        }
    }

    /// Port of the TS `renderUserSlashSkillPrompt` (agent-core-v2
    /// `agent/skill/prompt.ts`): the prompt the user-slash activation
    /// turn's user message carries. The opening line plus a
    /// `<kimi-skill-loaded>` block whose attributes carry the activation's
    /// identity (the same fields the web transcript's activation chip and
    /// compaction's user-slash keep rule consume); the body is
    /// [`Self::render_skill_prompt`]'s output (plugin instructions prepend
    /// included). Returns `None` when the skill is unknown or its content
    /// cannot be read (the same failure surface as [`Self::render_skill_prompt`]).
    pub fn render_user_slash_skill_prompt(&self, name: &str, args: &str) -> Option<String> {
        let skill = self.get(name)?;
        let content = self.render_skill_prompt(name)?;
        let source = skill_source_wire(skill.source);
        Some(format!(
            "{}<kimi-skill-loaded name=\"{}\" trigger=\"{}\" source=\"{}\" \
             dir=\"{}\" args=\"{}\">\n{}\n</kimi-skill-loaded>",
            user_activation_preamble(&skill.name),
            escape_xml_attr(&skill.name),
            escape_xml_attr("user-slash"),
            escape_xml_attr(source),
            escape_xml_attr(&skill.path.display().to_string()),
            escape_xml_attr(args),
            content,
        ))
    }

    /// Render a user-inline (SDR-005 / US3.6) skill activation prompt — the
    /// same `<kimi-skill-loaded>` wrap as the slash path but with
    /// `trigger="user-inline"` and no args (an inline reference carries no
    /// argument string). The reading line matches the TS user-slash family's
    /// declaration wording for inline activations (US3.6 "声明语与用户激活一致"),
    /// shared with [`Self::render_user_slash_skill_prompt`].
    pub fn render_user_inline_skill_prompt(&self, name: &str) -> Option<String> {
        let skill = self.get(name)?;
        let content = self.render_skill_prompt(name)?;
        let source = skill_source_wire(skill.source);
        Some(format!(
            "{}<kimi-skill-loaded name=\"{}\" trigger=\"{}\" source=\"{}\" dir=\"{}\">\n{}\n</kimi-skill-loaded>",
            user_activation_preamble(&skill.name),
            escape_xml_attr(&skill.name),
            escape_xml_attr("user-inline"),
            escape_xml_attr(source),
            escape_xml_attr(&skill.path.display().to_string()),
            content,
        ))
    }

    /// Replace the whole user-level disabled set (B8 D-32).
    ///
    /// Used at workspace assembly (seeding from the user config's
    /// `disabled_skills` key) and by the config-write resync hook — both are
    /// whole-set replacements, so the set is never incrementally merged.
    pub fn set_user_disabled(&self, names: Vec<String>) {
        *self
            .user_disabled
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = names.into_iter().collect();
    }

    /// Enable or disable a single skill by name (user-level).
    ///
    /// Returns whether the name is registered in this catalog (`false` for
    /// unknown names, in which case nothing changes).
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if self.get(name).is_none() {
            return false;
        }
        let mut disabled = self
            .user_disabled
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_string());
        }
        true
    }

    /// Whether the named skill is currently disabled at the user level.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.user_disabled
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(name)
    }
}

/// Recursively collect every directory under `root` that contains a
/// `SKILL.md` file, deterministically sorted by path.
///
/// Directory scanning goes through the injected [`SyncFs`] provider; the
/// `is_dir`/`is_file` entry flags follow symlinks (like `Path::is_dir` /
/// `Path::is_file`), matching the pre-trait discovery behavior.
fn find_skill_directories(root: &Path, fs: &dyn SyncFs) -> Result<Vec<PathBuf>, SkillError> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let mut subdirs = Vec::new();
        let mut has_skill_md = false;
        for entry in fs
            .iterdir(&dir)
            .map_err(|err| SkillError::DiscoveryFailed(std::io::Error::other(format!("{err:#}"))))?
        {
            if entry.is_dir {
                subdirs.push(PathBuf::from(entry.path));
            }
            if entry.is_file && entry.name == SKILL_MD_FILE {
                has_skill_md = true;
            }
        }
        if has_skill_md {
            found.push(dir);
        }
        pending.extend(subdirs);
    }
    found.sort();
    Ok(found)
}

/// Read a skill's `SKILL.md` frontmatter metadata (the discovery-path
/// consumption of [`parse_skill_metadata`]). Reads go through the injected
/// [`SyncFs`] provider; a missing/unreadable file yields all-empty
/// metadata (a warning is logged).
fn read_frontmatter(skill_md_path: &Path, fs: &dyn SyncFs) -> SkillFrontmatter {
    match fs.read_text(skill_md_path) {
        Ok(content) => parse_skill_metadata(&content),
        Err(error) => {
            tracing::warn!(path = %skill_md_path.display(), %error, "failed to read SKILL.md while indexing");
            SkillFrontmatter::default()
        }
    }
}

/// The frontmatter-derived metadata a skill index consumes: the
/// `description` plus the two gated keys of design 13 §3.2 (ADR-B6).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SkillFrontmatter {
    description: String,
    disable_model_invocation: bool,
    is_sub_skill: bool,
}

/// Parse a `SKILL.md`'s frontmatter into the metadata the skill index
/// consumes — the shared path of disk discovery and builtin construction
/// (design 13 §3.1): the `description` plus the two gated keys.
///
/// The gated keys accept the hyphenated and underscored spellings (the TS
/// `METADATA_ALIASES` compatibility shape) and are `true` only for a
/// case-insensitive `true` value.
fn parse_skill_metadata(content: &str) -> SkillFrontmatter {
    let fields = parse_frontmatter(content);
    SkillFrontmatter {
        description: fields.get("description").cloned().unwrap_or_default(),
        disable_model_invocation: frontmatter_flag(
            &fields,
            "disable-model-invocation",
            "disable_model_invocation",
        ),
        is_sub_skill: frontmatter_flag(&fields, "is-sub-skill", "is_sub_skill"),
    }
}

/// Interpret a hyphen/underscore-aliased boolean frontmatter key: `true`
/// (case-insensitive) maps to `true`, anything else to `false`.
fn frontmatter_flag(fields: &HashMap<String, String>, hyphenated: &str, underscored: &str) -> bool {
    fields
        .get(hyphenated)
        .or_else(|| fields.get(underscored))
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Escape a value for use inside an XML/HTML attribute — the rust
/// counterpart of the TS `escapeXmlAttr` the skill registries use around
/// plugin ids (item J / I).
pub fn escape_xml_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The wire `source` attribute value for a skill definition (the TS
/// `SkillSource` wire spelling), shared by the slash and inline renderers.
fn skill_source_wire(source: SkillSource) -> &'static str {
    match source {
        SkillSource::Builtin => "builtin",
        SkillSource::User => "user",
        SkillSource::Project => "project",
        SkillSource::Plugin => "extra",
    }
}

/// The declaration line preceding a user-triggered `<kimi-skill-loaded>` wrap
/// (both slash and inline) — US5.1's "用户激活了…请遵循" wording, shared so
/// the inline path carries the same declaration semantics as user-slash.
fn user_activation_preamble(name: &str) -> String {
    format!(
        "User activated the skill \"{}\". Follow the loaded skill instructions.\n\n",
        escape_xml_attr(name)
    )
}

/// Parse `key: value` lines from a leading `---`-fenced frontmatter block.
///
/// Keys are lower-cased and trimmed; values are trimmed. Returns an empty map
/// when the document has no frontmatter. Only the first frontmatter block is
/// considered; it must end before the body begins (first line that is exactly
/// `---`).
fn parse_frontmatter(content: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let Some(rest) = content.strip_prefix("---") else {
        return fields;
    };
    // Accept both LF and CRLF after the opening fence.
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .unwrap_or(rest);
    let Some(end) = rest.find("\n---") else {
        return fields;
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        fields.insert(key.trim().to_lowercase(), value.trim().to_string());
    }
    fields
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;

    fn write_skill(root: &Path, relative_dir: &str, content: &str) {
        let dir = root.join(relative_dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_MD_FILE), content).unwrap();
    }

    /// A minimal disk-shaped `SkillDef` for snapshot/diff tests.
    fn skill_def(name: &str, description: &str) -> (String, SkillDef) {
        (
            name.to_string(),
            SkillDef {
                name: name.into(),
                description: description.into(),
                path: PathBuf::from(format!("/tmp/skills/{name}")),
                source: SkillSource::User,
                plugin: None,
                disable_model_invocation: false,
                is_sub_skill: false,
            },
        )
    }

    /// A minimal builtin registration helper for catalog tests: the standard
    /// `builtin://<name>` pseudo path + Builtin source (the shape the real
    /// `builtin::builtin_skills()` entries carry).
    fn builtin_entry(name: &str, pseudo_path: &str) -> builtin::BuiltinSkillDef {
        builtin::BuiltinSkillDef {
            def: SkillDef {
                name: name.into(),
                description: format!("{name} helper"),
                path: PathBuf::from(pseudo_path),
                source: SkillSource::Builtin,
                plugin: None,
                disable_model_invocation: false,
                is_sub_skill: false,
            },
            content: "always do the thing",
        }
    }

    #[test]
    fn should_index_skill_with_metadata_when_discovering_skill_directory() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greet", "# Greet\nbody");

        let catalog = SkillCatalog::new();
        let count = catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        assert_eq!(count, 1);
        let skill = catalog
            .get("greet")
            .expect("skill indexed by directory name");
        assert_eq!(skill.name, "greet");
        assert_eq!(skill.source, SkillSource::Project);
        assert_eq!(skill.path, temp.path().join("skills").join("greet"));
    }

    #[test]
    fn should_index_all_skills_when_discovering_nested_directories() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/git/commit", "# Commit\n");
        write_skill(temp.path(), "skills/pdf", "# Pdf\n");

        let catalog = SkillCatalog::new();
        let count = catalog
            .discover(&temp.path().join("skills"), SkillSource::Builtin)
            .unwrap();

        assert_eq!(count, 2);
        let names: Vec<String> = catalog.list().into_iter().map(|skill| skill.name).collect();
        assert_eq!(names, vec!["commit".to_string(), "pdf".to_string()]);
    }

    #[test]
    fn should_parse_description_when_discovering_skill_with_frontmatter() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/greet",
            "---\ndescription: Greets the user warmly.\nname: ignored-key\n---\n# Greet\nbody",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        let skill = catalog.get("greet").unwrap();
        assert_eq!(skill.description, "Greets the user warmly.");
    }

    #[test]
    fn should_parse_description_when_discovering_skill_with_crlf_frontmatter() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/greet",
            "---\r\ndescription: CRLF friendly.\r\n---\r\n# Greet\r\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert_eq!(catalog.get("greet").unwrap().description, "CRLF friendly.");
    }

    #[test]
    fn should_have_empty_description_when_discovering_skill_without_frontmatter() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/plain", "# No frontmatter here");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("plain").unwrap().description.is_empty());
    }

    #[test]
    fn should_have_empty_description_when_discovering_skill_without_description_key() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/other",
            "---\nname: only-a-name\n---\nbody",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("other").unwrap().description.is_empty());
    }

    // ===== builtin data plane (design 13 §3.2): SkillDef gated metadata ====

    /// Given a skill def with both gated-metadata flags set, when it
    /// roundtrips through serde, then both flags survive and are emitted
    /// (set flags are visible; only `false` is skipped).
    #[test]
    fn should_preserve_gated_metadata_when_serde_roundtripping_skill_def() {
        let def = SkillDef {
            name: "mcp-config".into(),
            description: "configures mcp".into(),
            path: PathBuf::from("builtin://mcp-config"),
            source: SkillSource::Builtin,
            plugin: None,
            disable_model_invocation: true,
            is_sub_skill: true,
        };

        let json = serde_json::to_string(&def).unwrap();
        let decoded: SkillDef = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, def);
        assert!(json.contains("\"disable_model_invocation\":true"));
        assert!(json.contains("\"is_sub_skill\":true"));
    }

    /// Given a legacy JSON payload without the gated-metadata keys, when it
    /// is decoded, then both flags default to `false` (old payloads are
    /// losslessly readable).
    #[test]
    fn should_default_gated_metadata_to_false_when_decoding_legacy_skill_def_payload() {
        let json = r#"{
            "name": "pdf",
            "description": "handles pdfs",
            "path": "/tmp/skills/pdf",
            "source": "project"
        }"#;

        let decoded: SkillDef = serde_json::from_str(json).unwrap();

        assert!(!decoded.disable_model_invocation);
        assert!(!decoded.is_sub_skill);
    }

    /// Given a skill whose frontmatter declares the hyphenated
    /// `disable-model-invocation: true`, when it is discovered, then the
    /// indexed def carries the model-invocation gate.
    #[test]
    fn should_disable_model_invocation_when_frontmatter_uses_the_hyphenated_key() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/gated",
            "---\ndescription: gated\ndisable-model-invocation: true\n---\n# Gated\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("gated").unwrap().disable_model_invocation);
        assert!(!catalog.get("gated").unwrap().is_sub_skill);
    }

    /// Given a skill whose frontmatter declares the underscored
    /// `disable_model_invocation: true`, when it is discovered, then the
    /// key is normalized and the gate applies (the TS alias spellings).
    #[test]
    fn should_disable_model_invocation_when_frontmatter_uses_the_underscored_key() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/gated",
            "---\ndescription: gated\ndisable_model_invocation: true\n---\n# Gated\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("gated").unwrap().disable_model_invocation);
    }

    /// Given a skill whose frontmatter value is `True` (any case), when it
    /// is discovered, then the gate applies — the value is matched
    /// case-insensitively.
    #[test]
    fn should_disable_model_invocation_when_frontmatter_value_is_true_in_any_case() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/gated",
            "---\ndescription: gated\ndisable-model-invocation: TRUE\n---\n# Gated\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("gated").unwrap().disable_model_invocation);
    }

    /// Given a skill whose frontmatter value is anything but `true`, when it
    /// is discovered, then the gate does not apply (only a case-insensitive
    /// `true` counts).
    #[test]
    fn should_not_disable_model_invocation_when_frontmatter_value_is_not_true() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/open",
            "---\ndescription: open\ndisable-model-invocation: false\n---\n# Open\n",
        );
        write_skill(
            temp.path(),
            "skills/flaky",
            "---\ndescription: flaky\ndisable-model-invocation: yes\n---\n# Flaky\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(!catalog.get("open").unwrap().disable_model_invocation);
        assert!(!catalog.get("flaky").unwrap().disable_model_invocation);
    }

    /// Given a skill whose frontmatter declares the hyphenated or
    /// underscored `is-sub-skill: true`, when it is discovered, then the
    /// indexed def carries the sub-skill marker (either alias spelling).
    #[test]
    fn should_mark_sub_skill_when_frontmatter_declares_it_with_either_alias() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/child-a",
            "---\ndescription: a\nis-sub-skill: true\n---\n# A\n",
        );
        write_skill(
            temp.path(),
            "skills/child-b",
            "---\ndescription: b\nis_sub_skill: TRUE\n---\n# B\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get("child-a").unwrap().is_sub_skill);
        assert!(catalog.get("child-b").unwrap().is_sub_skill);
        assert!(!catalog.get("child-a").unwrap().disable_model_invocation);
    }

    /// Given a skill whose frontmatter declares neither gated key, when it
    /// is discovered, then both flags stay `false`.
    #[test]
    fn should_keep_gated_metadata_false_when_frontmatter_has_no_gated_keys() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/plain",
            "---\ndescription: plain\n---\n# Plain\n",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        let skill = catalog.get("plain").unwrap();
        assert!(!skill.disable_model_invocation);
        assert!(!skill.is_sub_skill);
    }

    // ===== builtin content table (design 13 §3.1): register_builtin + read_content

    /// Given a catalog with the builtin bundle registered, when read_content
    /// is called for a builtin entry, then the embedded body is served from
    /// the content table — the `builtin://` pseudo path never touches the
    /// disk (ADR-B1).
    #[test]
    fn should_return_the_embedded_body_when_reading_content_of_a_builtin_entry() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin(builtin::builtin_skills());

        let mcp = builtin::builtin_skills()
            .into_iter()
            .find(|skill| skill.def.name == "mcp-config")
            .expect("mcp-config entry");
        assert_eq!(
            catalog.read_content("mcp-config").as_deref(),
            Some(mcp.content),
            "the embedded body comes back verbatim (no disk read)"
        );
    }

    /// Given a builtin entry shadowed by a same-name user skill, when
    /// read_content is called, then the user's disk body is served — the
    /// content-table residue is never hit once the entry's source is no
    /// longer Builtin (§3.1 shadowing consistency).
    #[test]
    fn should_fall_back_to_disk_when_a_builtin_entry_is_shadowed_by_a_user_skill() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/mcp-config",
            "---\ndescription: user override\n---\n# User mcp-config\n",
        );

        let catalog = SkillCatalog::new();
        catalog.register_builtin(builtin::builtin_skills());
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        let skill = catalog.get("mcp-config").unwrap();
        assert_eq!(
            skill.source,
            SkillSource::User,
            "the user skill shadows the builtin"
        );
        assert_eq!(
            catalog.read_content("mcp-config").as_deref(),
            Some("---\ndescription: user override\n---\n# User mcp-config\n"),
            "a shadowed builtin reads from disk, not the content table"
        );
    }

    /// Given the builtin bundle, when it is registered, then every entry
    /// lands in the skills map and its embedded body in the content table
    /// (register_builtin's two writes — §3.1).
    #[test]
    fn should_register_every_entry_and_body_when_registering_the_builtin_bundle() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin(builtin::builtin_skills());

        assert_eq!(catalog.list().len(), 8);
        for entry in builtin::builtin_skills() {
            assert_eq!(catalog.get(&entry.def.name).as_ref(), Some(&entry.def));
            assert_eq!(
                catalog.read_content(&entry.def.name).as_deref(),
                Some(entry.content),
                "{}: the body is served through the content table",
                entry.def.name
            );
        }
    }

    /// Given two builtin registrations of the same name, when they are
    /// registered in order, then the later registration wins for both the
    /// definition and the body — the later-wins rule of the catalog's other
    /// channels (the should_keep_latest_source_when_discovering_duplicate_skill_name
    /// precedent).
    #[test]
    fn should_keep_the_latest_registration_when_registering_duplicate_builtin_names() {
        fn entry(name: &str, description: &str, content: &'static str) -> builtin::BuiltinSkillDef {
            builtin::BuiltinSkillDef {
                def: SkillDef {
                    name: name.into(),
                    description: description.into(),
                    path: PathBuf::from(format!("builtin://{name}")),
                    source: SkillSource::Builtin,
                    plugin: None,
                    disable_model_invocation: false,
                    is_sub_skill: false,
                },
                content,
            }
        }

        let catalog = SkillCatalog::new();
        catalog.register_builtin([
            entry("greet", "first", "first body"),
            entry("greet", "second", "second body"),
        ]);

        let skill = catalog.get("greet").unwrap();
        assert_eq!(skill.description, "second");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(
            catalog.read_content("greet").as_deref(),
            Some("second body")
        );
    }

    #[test]
    fn should_keep_latest_source_when_discovering_duplicate_skill_name() {
        let builtin = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        write_skill(builtin.path(), "skills/prune", "# builtin");
        write_skill(user.path(), "skills/prune", "# user override");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&builtin.path().join("skills"), SkillSource::Builtin)
            .unwrap();
        catalog
            .discover(&user.path().join("skills"), SkillSource::User)
            .unwrap();

        let skill = catalog.get("prune").unwrap();
        assert_eq!(skill.source, SkillSource::User);
        assert_eq!(catalog.list().len(), 1);
    }

    #[test]
    fn should_return_zero_without_error_when_discovering_nonexistent_directory() {
        let catalog = SkillCatalog::new();

        let count = catalog
            .discover(Path::new("/definitely/not/here"), SkillSource::User)
            .unwrap();

        assert_eq!(count, 0);
        assert!(catalog.list().is_empty());
    }

    #[test]
    fn should_have_empty_catalog_when_cleared_after_discovery() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greet", "# Greet\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();
        assert_eq!(catalog.list().len(), 1);

        // When the catalog is cleared
        catalog.clear();

        // Then it is empty and a re-discovery works from scratch
        assert!(catalog.list().is_empty());
        assert!(catalog.get("greet").is_none());
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();
        assert_eq!(catalog.list().len(), 1);
    }

    #[test]
    fn should_return_zero_when_discovering_directory_without_skills() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("empty")).unwrap();
        fs::write(temp.path().join("empty/readme.txt"), "not a skill").unwrap();

        let catalog = SkillCatalog::new();
        let count = catalog
            .discover(&temp.path().join("empty"), SkillSource::User)
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn should_return_skill_md_body_when_reading_content_of_known_skill() {
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/summarize",
            "---\ndescription: Summarizes.\n---\n# Summarize\n\nDo the thing.",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Builtin)
            .unwrap();

        let content = catalog.read_content("summarize").expect("content readable");
        assert!(content.contains("# Summarize"));
        assert!(content.contains("Do the thing."));
    }

    #[test]
    fn should_return_none_when_reading_content_of_unknown_skill() {
        let catalog = SkillCatalog::new();

        assert!(catalog.read_content("nope").is_none());
    }

    #[test]
    fn should_preserve_source_and_sort_by_name_when_listing_catalog() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/zebra", "# zebra");
        write_skill(temp.path(), "skills/alpha", "# alpha");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Plugin)
            .unwrap();

        let listed = catalog.list();
        assert_eq!(
            listed
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zebra"]
        );
        assert!(
            listed
                .iter()
                .all(|skill| skill.source == SkillSource::Plugin)
        );
    }

    #[test]
    fn should_preserve_fields_when_serde_roundtripping_skill_def() {
        let def = SkillDef {
            name: "pdf".into(),
            description: "handles pdfs".into(),
            path: PathBuf::from("/tmp/skills/pdf"),
            source: SkillSource::Project,
            plugin: None,
            disable_model_invocation: false,
            is_sub_skill: false,
        };

        let json = serde_json::to_string(&def).unwrap();
        let decoded: SkillDef = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, def);
        // Tagged enum serializes as snake_case.
        assert!(json.contains("\"project\""));
        // The absent plugin attribution is omitted (serde skip_serializing_if).
        assert!(!json.contains("plugin"));
        // Defaulted-false gated metadata is omitted too (the plugin-field
        // precedent extended to bools — design 13 §3.2).
        assert!(!json.contains("disable_model_invocation"));
        assert!(!json.contains("is_sub_skill"));
    }

    // ===== item J — plugin attribution, plugin-scoped lookup, prompt render ==

    /// Given a plugin skill root discovered with its attribution, when the
    /// catalog is queried, then the skill carries the plugin context and
    /// resolves through the plugin-scoped lookup.
    #[test]
    fn should_attach_plugin_context_when_discovering_plugin_root() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: Some("Always be helpful.".into()),
                },
            )
            .unwrap();

        let skill = catalog.get("greeter").expect("indexed");
        assert_eq!(skill.source, SkillSource::Plugin);
        assert_eq!(skill.plugin.as_ref().map(|p| p.id.as_str()), Some("demo"));
        assert_eq!(
            skill
                .plugin
                .as_ref()
                .and_then(|p| p.instructions.as_deref()),
            Some("Always be helpful.")
        );
        assert_eq!(
            catalog.get_plugin_skill("demo", "greeter").map(|s| s.name),
            Some("greeter".into())
        );
        assert!(
            catalog.get_plugin_skill("other", "greeter").is_none(),
            "another plugin's attribution never matches"
        );
    }

    /// Given a non-plugin skill, when the plugin-scoped lookup is asked,
    /// then it never matches (no plugin attribution).
    #[test]
    fn should_not_match_plugin_lookup_without_plugin_attribution() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/plain", "# Plain\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        assert!(catalog.get_plugin_skill("demo", "plain").is_none());
    }

    /// Given a plugin skill with non-empty instructions, when the prompt is
    /// rendered, then the `<kimi-plugin-instructions>` block is prepended —
    /// the TS `renderSkillPrompt` shape (item J).
    #[test]
    fn should_prepend_plugin_instructions_when_rendering_plugin_skill_prompt() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nSay hi.");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: Some("Always be helpful.".into()),
                },
            )
            .unwrap();

        let prompt = catalog.render_skill_prompt("greeter").expect("rendered");
        assert!(
            prompt.starts_with(
                "<kimi-plugin-instructions plugin=\"demo\">\nAlways be helpful.\n</kimi-plugin-instructions>\n\n"
            ),
            "unexpected prompt: {prompt}"
        );
        assert!(prompt.contains("# Greeter"));
    }

    /// Given a plugin skill with blank instructions, when the prompt is
    /// rendered, then no instructions block is emitted (the TS
    /// `trim().length === 0` skip).
    #[test]
    fn should_skip_instructions_block_when_plugin_instructions_are_blank() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nSay hi.");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: Some("   ".into()),
                },
            )
            .unwrap();

        let prompt = catalog.render_skill_prompt("greeter").expect("rendered");
        assert!(!prompt.contains("kimi-plugin-instructions"));
        assert_eq!(prompt, "# Greeter\nSay hi.");
    }

    /// Given a non-plugin skill, when the prompt is rendered, then the raw
    /// body comes back untouched.
    #[test]
    fn should_render_raw_body_when_skill_has_no_plugin_attribution() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/plain", "# Plain\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        assert_eq!(
            catalog.render_skill_prompt("plain").as_deref(),
            Some("# Plain\nbody")
        );
    }

    /// Given an unknown skill, when the prompt is rendered, then `None` is
    /// returned.
    #[test]
    fn should_return_none_when_rendering_unknown_skill() {
        let catalog = SkillCatalog::new();
        assert!(catalog.render_skill_prompt("nope").is_none());
    }

    /// Given a plugin id with XML-special characters, when the prompt is
    /// rendered, then the attribute is escaped (the TS `escapeXmlAttr`).
    #[test]
    fn should_escape_plugin_id_when_rendering_instructions_block() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nSay hi.");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "weird\"name".into(),
                    instructions: Some("Do it.".into()),
                },
            )
            .unwrap();

        let prompt = catalog.render_skill_prompt("greeter").expect("rendered");
        assert!(
            prompt.starts_with("<kimi-plugin-instructions plugin=\"weird&quot;name\">"),
            "unexpected prompt: {prompt}"
        );
    }

    // ===== injected SyncFs (T-13b1-3: zero-IO state plane) =====

    /// In-memory [`mirri_os::SyncFs`] double serving a fixed directory tree,
    /// so discovery is verified without the real filesystem.
    #[derive(Default)]
    struct TreeFs {
        dirs: Vec<PathBuf>,
        files: HashMap<PathBuf, String>,
    }

    impl TreeFs {
        fn add_dir(&mut self, dir: &Path) {
            self.dirs.push(dir.to_path_buf());
        }

        fn add_file(&mut self, path: &Path, content: &str) {
            self.files.insert(path.to_path_buf(), content.to_string());
        }
    }

    impl mirri_os::SyncFs for TreeFs {
        fn exists(&self, path: &Path) -> bool {
            self.dirs.iter().any(|dir| dir == path) || self.files.contains_key(path)
        }

        fn iterdir(&self, dir: &Path) -> anyhow::Result<Vec<mirri_os::DirEntry>> {
            let mut entries: Vec<mirri_os::DirEntry> = self
                .dirs
                .iter()
                .filter(|path| path.parent() == Some(dir))
                .map(|path| mirri_os::DirEntry {
                    name: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path: path.to_string_lossy().into_owned(),
                    is_dir: true,
                    is_file: false,
                })
                .collect();
            entries.extend(
                self.files
                    .keys()
                    .filter(|path| path.parent() == Some(dir))
                    .map(|path| mirri_os::DirEntry {
                        name: path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: path.to_string_lossy().into_owned(),
                        is_dir: false,
                        is_file: true,
                    }),
            );
            Ok(entries)
        }

        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
        }
    }

    /// should discover skills through the injected fs with the shadow
    /// priority unchanged (T-13b1-3)
    #[test]
    fn should_discover_skills_through_injected_fs_with_shadow_priority_unchanged() {
        // Given a fake fs holding a builtin and a user skill of the same
        // name, plus one extra user skill
        let builtin_root = PathBuf::from("/virtual/builtin-skills");
        let user_root = PathBuf::from("/virtual/user-skills");
        let mut fs = TreeFs::default();
        fs.add_dir(&builtin_root);
        fs.add_dir(&builtin_root.join("prune"));
        fs.add_file(
            &builtin_root.join("prune").join(SKILL_MD_FILE),
            "---\ndescription: Builtin prune.\n---\n# Prune\n",
        );
        fs.add_dir(&user_root);
        fs.add_dir(&user_root.join("prune"));
        fs.add_file(
            &user_root.join("prune").join(SKILL_MD_FILE),
            "---\ndescription: User prune.\n---\n# Prune\n",
        );
        fs.add_dir(&user_root.join("extra"));
        fs.add_file(&user_root.join("extra").join(SKILL_MD_FILE), "# Extra\n");

        // When a catalog wired to the fake discovers the builtin root and
        // then the user root
        let catalog = SkillCatalog::with_fs(Arc::new(fs));
        assert_eq!(
            catalog
                .discover(&builtin_root, SkillSource::Builtin)
                .expect("builtin discovery"),
            1
        );
        assert_eq!(
            catalog
                .discover(&user_root, SkillSource::User)
                .expect("user discovery"),
            2
        );

        // Then the later user discovery shadows the builtin of the same name
        let prune = catalog.get("prune").expect("prune indexed");
        assert_eq!(prune.source, SkillSource::User);
        assert_eq!(prune.description, "User prune.");
        assert_eq!(prune.path, user_root.join("prune"));
        assert_eq!(catalog.list().len(), 2);
        // And the SKILL.md content also reads through the injected fs
        assert!(
            catalog
                .read_content("prune")
                .expect("content readable")
                .contains("User prune.")
        );
    }

    /// should skip discovery when the injected fs reports the root absent
    /// (T-13b1-3)
    #[test]
    fn should_skip_discovery_when_injected_fs_reports_root_absent() {
        // Given a catalog wired to an empty fake fs
        let catalog = SkillCatalog::with_fs(Arc::new(TreeFs::default()));

        // When a root is discovered that the fake does not hold
        let count = catalog
            .discover(Path::new("/virtual/nowhere"), SkillSource::User)
            .expect("absent root is not an error");

        // Then discovery yields zero skills
        assert_eq!(count, 0);
        assert!(catalog.list().is_empty());
    }

    /// Given a plugin skill, when a plugin-qualified reference (`plugin_id:
    /// skill_name`) is resolved, then the plugin's skill definition comes
    /// back (SDR-002 / ADR-001).
    #[test]
    fn should_resolve_plugin_skill_when_reference_is_plugin_qualified() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();

        let skill = catalog
            .resolve("demo:greeter")
            .expect("qualified reference resolves");
        assert_eq!(skill.name, "greeter");
        assert_eq!(skill.plugin.as_ref().map(|p| p.id.as_str()), Some("demo"));
    }

    /// Given a bare skill whose name literally contains a colon (so the
    /// reference also parses as a qualified form), when it is resolved, then
    /// the exact bare-name match wins over the plugin-scoped lookup.
    #[test]
    fn should_prefer_bare_name_when_reference_matches_a_known_skill_exactly() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/a:b", "# Colon-named\nbody");
        write_skill(temp.path(), "plugin-skills/b", "# Plugin b\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();
        catalog
            .discover_plugin(
                &temp.path().join("plugin-skills"),
                &SkillPluginContext {
                    id: "a".into(),
                    instructions: None,
                },
            )
            .unwrap();

        let skill = catalog.resolve("a:b").expect("exact bare match wins");
        assert_eq!(skill.name, "a:b");
        assert_eq!(skill.source, SkillSource::Project);
    }

    /// Given a plugin skill, when the qualified reference's prefix names an
    /// unknown plugin, then resolution fails — no prefix-stripping fallback
    /// to the bare name.
    #[test]
    fn should_return_none_when_the_plugin_prefix_is_unknown() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();

        assert!(
            catalog.resolve("other:greeter").is_none(),
            "an unknown plugin prefix must not fall back to the bare name"
        );
    }

    /// Given a plugin, when the qualified reference names a skill it did not
    /// contribute, then resolution fails.
    #[test]
    fn should_return_none_when_the_plugin_lacks_the_named_skill() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();

        assert!(catalog.resolve("demo:missing").is_none());
    }

    /// Given a non-plugin skill, when it is addressed through a qualified
    /// reference, then resolution fails (no plugin attribution).
    #[test]
    fn should_return_none_when_a_non_plugin_skill_is_addressed_qualified() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/plain", "# Plain\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        assert!(catalog.resolve("demo:plain").is_none());
    }

    /// Given a catalog, when a qualified reference has an empty plugin id
    /// or skill name segment, then resolution fails.
    #[test]
    fn should_return_none_when_reference_has_empty_segments() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();

        assert!(catalog.resolve(":greeter").is_none());
        assert!(catalog.resolve("demo:").is_none());
        assert!(catalog.resolve(":").is_none());
    }

    /// Given a plugin skill, when the qualified reference's plugin id
    /// segment differs only in case, then resolution still succeeds (the
    /// plugin id is defensively lower-cased — the id charset contract).
    #[test]
    fn should_normalize_plugin_id_case_when_resolving_a_qualified_reference() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greeter", "# Greeter\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: "demo".into(),
                    instructions: None,
                },
            )
            .unwrap();

        let skill = catalog
            .resolve("Demo:greeter")
            .expect("plugin id case is normalized");
        assert_eq!(skill.name, "greeter");
    }

    /// Given a catalog, when a reference without a colon does not match a
    /// bare skill, then resolution fails (no other fallback).
    #[test]
    fn should_return_none_when_a_bare_reference_is_unknown() {
        let catalog = SkillCatalog::new();
        assert!(catalog.resolve("nope").is_none());
    }

    /// Given a catalog with a discovered skill, when resolving by its SKILL.md
    /// FILE path, then the skill whose directory is the path's parent matches.
    #[test]
    fn should_resolve_skill_by_its_skill_md_file_path() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/alpha", "# Alpha\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        // When resolved by the SKILL.md file path (the inline reference target)
        let target = temp.path().join("skills").join("alpha").join("SKILL.md");
        let resolved = catalog.resolve_by_skill_md_path(&target.to_string_lossy());

        // Then the skill whose directory is the parent matches
        let skill = resolved.expect("resolve by SKILL.md path");
        assert_eq!(skill.name, "alpha");
        assert_eq!(skill.path, temp.path().join("skills").join("alpha"));
    }

    /// Given a catalog, when resolving by a directory path or a missing file,
    /// then resolution returns none (only the SKILL.md file shape matches).
    #[test]
    fn should_return_none_when_resolving_by_a_directory_or_missing_target() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/alpha", "# Alpha\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        // A directory path (not the SKILL.md file) never matches.
        assert!(
            catalog
                .resolve_by_skill_md_path(
                    &temp.path().join("skills").join("alpha").to_string_lossy()
                )
                .is_none()
        );
        // A missing file whose parent is a real skill dir still matches by path
        // shape (resolution is path-based, not existence-based).
        let missing = temp.path().join("skills").join("alpha").join("SKILL.md");
        assert!(
            catalog
                .resolve_by_skill_md_path(&missing.to_string_lossy())
                .is_some()
        );
    }

    // ===== builtin:// pseudo-path resolution (ADR-007 D4 prerequisite) =====

    /// Given a catalog with a registered builtin skill, when resolving by its
    /// `builtin://<name>` pseudo path, then the builtin skill matches (exact
    /// path equality — the builtin registration stamps the same pseudo path).
    #[test]
    fn should_resolve_builtin_skill_when_target_is_its_pseudo_path() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin([builtin_entry("update-config", "builtin://update-config")]);

        // When resolved by the exact pseudo path
        let resolved = catalog.resolve_by_skill_md_path("builtin://update-config");
        // Then the builtin skill matches
        let skill = resolved.expect("resolve by builtin pseudo path");
        assert_eq!(skill.name, "update-config");
        assert_eq!(skill.path, PathBuf::from("builtin://update-config"));
        assert_eq!(skill.source, SkillSource::Builtin);
    }

    /// Given a catalog with a registered builtin skill, when resolving the
    /// dirty historical `builtin://name/SKILL.md` form, then it still matches
    /// (replay-time journal targets may carry the stale suffix — the minimal
    /// consistent point tolerates it here, mirroring the parser normalization).
    #[test]
    fn should_resolve_builtin_skill_when_target_carries_the_stale_skill_md_suffix() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin([builtin_entry("update-config", "builtin://update-config")]);

        // When resolved by the dirty form persisted by pre-fix sites
        let resolved = catalog.resolve_by_skill_md_path("builtin://update-config/SKILL.md");
        // Then it still matches (suffix stripped before the equality check)
        let skill = resolved.expect("dirty builtin target resolves");
        assert_eq!(skill.name, "update-config");
    }

    /// Given a catalog with builtin skills, when resolving an unknown builtin
    /// pseudo path, then resolution is None (fail-soft — unknown builtins
    /// never activate).
    #[test]
    fn should_return_none_when_builtin_pseudo_path_is_unknown() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin([builtin_entry("update-config", "builtin://update-config")]);

        assert!(
            catalog
                .resolve_by_skill_md_path("builtin://write-goal")
                .is_none(),
            "unknown builtin must fail soft"
        );
        assert!(
            catalog
                .resolve_by_skill_md_path("builtin://update-config/SKILL.md/extra")
                .is_none(),
            "non-exact pseudo paths must not match"
        );
    }

    /// Given a catalog with both disk and builtin skills, when resolving a
    /// disk SKILL.md path, then the disk behavior is unchanged (builtin
    /// pseudo-paths never shadow real disk targets).
    #[test]
    fn should_keep_disk_resolution_unchanged_when_builtins_are_registered() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/alpha", "# Alpha\nbody");
        let catalog = SkillCatalog::new();
        catalog.register_builtin([builtin_entry("update-config", "builtin://update-config")]);
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        let target = temp.path().join("skills").join("alpha").join("SKILL.md");
        assert_eq!(
            catalog
                .resolve_by_skill_md_path(&target.to_string_lossy())
                .expect("disk target still resolves")
                .name,
            "alpha"
        );
    }

    // ===== user-level disabled set (B8 D-32: per-skill enablement) =====

    #[test]
    fn should_mark_skill_disabled_when_name_lands_in_the_user_disabled_set() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greet", "# Greet\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        // When the skill is disabled by name
        assert!(catalog.set_enabled("greet", false));

        // Then the catalog reports it disabled while the def stays listed
        assert!(catalog.is_disabled("greet"));
        assert!(catalog.get("greet").is_some());
        // And re-enabling clears the disabled state
        assert!(catalog.set_enabled("greet", true));
        assert!(!catalog.is_disabled("greet"));
    }

    #[test]
    fn should_return_registered_false_and_change_nothing_when_set_enabled_targets_an_unknown_name()
    {
        let catalog = SkillCatalog::new();

        // When an unknown name is toggled
        assert!(!catalog.set_enabled("nope", false));

        // Then the disabled set stays empty
        assert!(!catalog.is_disabled("nope"));
    }

    #[test]
    fn should_replace_the_whole_set_when_seeding_user_disabled_skills() {
        let catalog = SkillCatalog::new();
        catalog.set_user_disabled(vec!["a".into(), "b".into()]);
        catalog.set_user_disabled(vec!["c".into()]);

        // When seeded twice, Then only the latest set applies
        assert!(!catalog.is_disabled("a"));
        assert!(catalog.is_disabled("c"));
    }

    /// Given two catalog snapshots with added, removed and updated skills,
    /// when they are diffed, then each name lands in exactly its own bucket
    /// — a same-name entry with identical fields never enters `updated`
    /// (the listing-visible field level, ADR-010 decision 4).
    #[test]
    fn should_classify_names_into_their_buckets_when_diffing_two_catalog_snapshots() {
        let before = HashMap::from([
            skill_def("kept", "kept desc"),
            skill_def("changed", "old desc"),
            skill_def("gone", "gone desc"),
        ]);
        let after = HashMap::from([
            skill_def("kept", "kept desc"),
            skill_def("changed", "new desc"),
            skill_def("fresh", "fresh desc"),
        ]);

        let diff = diff_skill_catalogs(&before, &after);

        assert_eq!(diff, SkillCatalogDiff {
            added: vec!["fresh".into()],
            removed: vec!["gone".into()],
            updated: vec!["changed".into()],
        });
    }

    /// Given two identical snapshots, when they are diffed, then the diff is
    /// empty in every bucket (an empty change set gates the notification).
    #[test]
    fn should_return_an_empty_diff_when_snapshots_are_identical() {
        let snapshot = HashMap::from([skill_def("a", "same"), skill_def("b", "same")]);

        let diff = diff_skill_catalogs(&snapshot, &snapshot);

        assert_eq!(diff, SkillCatalogDiff::default());
    }

    /// Given a snapshot whose diff buckets hold multiple names, when the
    /// diff is computed, then every bucket is name-sorted (deterministic
    /// output — the wire payload and tests both depend on it).
    #[test]
    fn should_sort_every_bucket_when_diffing_unordered_snapshots() {
        let before = HashMap::from([
            skill_def("a-removed", ""),
            skill_def("c-removed", ""),
            skill_def("z-stale", ""),
            skill_def("m-stale", ""),
        ]);
        let after = HashMap::from([
            skill_def("z-added", ""),
            skill_def("m-added", ""),
            skill_def("z-stale", "changed description"),
            skill_def("m-stale", ""),
        ]);

        let diff = diff_skill_catalogs(&before, &after);

        assert_eq!(
            diff.added,
            vec!["m-added".to_string(), "z-added".to_string()]
        );
        assert_eq!(
            diff.removed,
            vec!["a-removed".to_string(), "c-removed".to_string()]
        );
        assert_eq!(diff.updated, vec!["z-stale".to_string()]);
    }

    /// Given a catalog replaced through `replace_all`, when a name is
    /// resolved, then the new table answers (the atomic whole-table swap —
    /// old entries vanish, new entries resolve) and the user-disabled set
    /// survives the swap untouched (it is deliberately NOT part of the
    /// swapped table).
    #[test]
    fn should_serve_the_new_table_and_keep_user_disabled_when_replace_all_swaps() {
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/old-skill", "# Old\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();
        catalog.set_user_disabled(vec!["old-skill".into()]);

        let replacement = HashMap::from([skill_def("new-skill", "replacement")]);
        catalog.replace_all(replacement, HashMap::new());

        assert!(catalog.get("old-skill").is_none(), "the old table is gone");
        assert!(
            catalog.resolve("new-skill").is_some(),
            "the new table resolves"
        );
        assert!(
            !catalog.is_disabled("new-skill"),
            "a name outside the disabled set stays enabled"
        );
        assert!(
            catalog.is_disabled("old-skill"),
            "the user-disabled set survives the swap"
        );
    }

    /// Given a builtin entry replaced away by `replace_all`, when its
    /// content is read, then the builtin content table swap applies too —
    /// a replaced-away builtin no longer serves its embedded body, and the
    /// swapped-in builtin body is served without touching the disk.
    #[test]
    fn should_swap_the_builtin_content_table_when_replace_all_swaps() {
        let catalog = SkillCatalog::new();
        catalog.register_builtin([builtin_entry("update-config", "builtin://update-config")]);

        let replacement = HashMap::from([skill_def("brand-new", "replaced builtin")]);
        // The swapped-in entry keeps the builtin source and pseudo path so the
        // content table lookup takes the embedded-body branch.
        let mut replacement = replacement;
        let mut def = replacement.get("brand-new").cloned().unwrap();
        def.source = SkillSource::Builtin;
        def.path = PathBuf::from("builtin://brand-new");
        replacement.insert("brand-new".to_string(), def);
        let contents = HashMap::from([("brand-new".to_string(), "brand-new body")]);
        catalog.replace_all(replacement, contents);

        assert!(catalog.get("update-config").is_none());
        let skill = catalog.get("brand-new").expect("swapped-in builtin");
        assert_eq!(skill.source, SkillSource::Builtin);
        assert_eq!(
            catalog.read_content("brand-new").as_deref(),
            Some("brand-new body"),
            "the swapped-in builtin body is served from the new content table"
        );
    }

    // ===== user-slash activation prompt (Task 2: v2 renderUserSlashSkillPrompt) ====

    #[test]
    fn should_render_the_user_slash_prompt_with_identity_attributes_when_args_present() {
        // Given a catalog with one discovered project skill
        let temp = TempDir::new().unwrap();
        write_skill(
            temp.path(),
            "skills/commit",
            "# Commit\nRun the commit flow.",
        );

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        // When the user-slash prompt is rendered with args
        let prompt = catalog
            .render_user_slash_skill_prompt("commit", "fix the bug")
            .expect("rendered prompt");

        // Then the shape is the v2 renderUserSlashSkillPrompt form
        assert!(
            prompt.starts_with(
                "User activated the skill \"commit\". Follow the loaded skill instructions.\n\n"
            ),
            "unexpected opening: {prompt}"
        );
        assert!(
            prompt.contains(
                "<kimi-skill-loaded name=\"commit\" trigger=\"user-slash\" source=\"project\""
            ),
            "missing identity attributes: {prompt}"
        );
        assert!(prompt.contains("# Commit\nRun the commit flow."));
        assert!(prompt.ends_with("</kimi-skill-loaded>"));
    }

    #[test]
    fn should_render_the_user_slash_prompt_with_empty_args_when_args_absent() {
        // Given the same catalog
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/commit", "# Commit\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::Project)
            .unwrap();

        // When rendered with empty args, the args attribute is the empty string
        let prompt = catalog
            .render_user_slash_skill_prompt("commit", "")
            .expect("rendered prompt");
        assert!(
            prompt.contains("args=\"\""),
            "empty args must still emit the attribute (v2 parity): {prompt}"
        );
    }

    #[test]
    fn should_escape_attribute_values_when_rendering_the_user_slash_prompt() {
        // Given a skill whose directory name is safe but whose args contain
        // characters that would break the attribute quoting
        let temp = TempDir::new().unwrap();
        write_skill(temp.path(), "skills/greet", "# Greet\nbody");

        let catalog = SkillCatalog::new();
        catalog
            .discover(&temp.path().join("skills"), SkillSource::User)
            .unwrap();

        // When rendered with hostile args
        let prompt = catalog
            .render_user_slash_skill_prompt("greet", "a\"b & <c>")
            .expect("rendered prompt");

        // Then the attribute value is XML-escaped
        assert!(
            prompt.contains("args=\"a&quot;b &amp; &lt;c&gt;\""),
            "args must be attribute-escaped: {prompt}"
        );
    }
}
