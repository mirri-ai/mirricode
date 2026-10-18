//! Builtin skill definitions — the six workflow skills bundled with the
//! runtime (design `docs/specs/rust-core-design/13-builtin-skills.md`
//! §3.1).
//!
//! The bodies are the TS v2 assets copied verbatim except the disclosed
//! adjustments of §3.7 (ADR-B5, completed by the M2 correction): brand
//! unification in import-from-cc-codex (every "Kimi" — including the
//! "Kimi-specific" compound — becomes "Mirri") and "Kimi-specific" →
//! "Mirri-specific" in mcp-config, plus the mcp-config authority pointer
//! rewritten to the official documentation (no internal repository paths in
//! model context).
//! Each `.md` is embedded at compile time via `include_str!` (ADR-B1) — the
//! body ships with the binary and evolves in lockstep with this code
//! ("the body IS the interface").
//!
//! Construction mirrors the TS wrappers (`write-goal.ts` … `sub-skill.ts`):
//! frontmatter is parsed for the description (and the frontmatter-declared
//! `disable-model-invocation` of import-from-cc-codex), then the source is
//! forced to [`SkillSource::Builtin`] and the pseudo `builtin://` path
//! stamped on — the TS literals verbatim, so the sub-skill family's pseudo
//! paths use slashes (`builtin://sub-skill/review`) even though the
//! registered names keep their dots (`sub-skill.review`). The gate-flag
//! override points match TS exactly: mcp-config / custom-theme / the
//! sub-skill family (parent + both children, the children additionally
//! carrying `is_sub_skill`) get `disable_model_invocation` set at the
//! registration layer; write-goal / update-config get neither.

use std::path::PathBuf;

use super::{SkillDef, SkillSource, parse_skill_metadata};

/// A builtin skill: its catalog definition plus the embedded `SKILL.md`
/// body (the `include_str!` `&'static str`) supplied alongside it — the
/// body travels with the definition so [`super::SkillCatalog`]'s builtin
/// content table can serve it through [`super::SkillCatalog::read_content`].
#[derive(Debug, Clone)]
pub struct BuiltinSkillDef {
    /// The catalog entry to register.
    pub def: SkillDef,
    /// The embedded `SKILL.md` body served for this skill.
    pub content: &'static str,
}

const WRITE_GOAL_BODY: &str = include_str!("builtin/write-goal.md");
const UPDATE_CONFIG_BODY: &str = include_str!("builtin/update-config.md");
const MCP_CONFIG_BODY: &str = include_str!("builtin/mcp-config.md");
const CUSTOM_THEME_BODY: &str = include_str!("builtin/custom-theme.md");
const IMPORT_FROM_CC_CODEX_BODY: &str = include_str!("builtin/import-from-cc-codex.md");
const SUB_SKILL_PARENT_BODY: &str = include_str!("builtin/sub-skill/SKILL.md");
const SUB_SKILL_REVIEW_BODY: &str = include_str!("builtin/sub-skill/review/SKILL.md");
const SUB_SKILL_CONSOLIDATE_BODY: &str = include_str!("builtin/sub-skill/consolidate/SKILL.md");

/// Build one builtin entry — the counterpart of the TS per-skill wrapper
/// (`parseSkillText` → override path/source + gate flags).
///
/// `pseudo_path` is the TS literal verbatim (`builtin://<name>` for the flat
/// skills, the slash-separated `builtin://sub-skill/<child>` form for the
/// sub-skill family — the registered names keep their dots).
/// `disable_model_invocation` / `is_sub_skill` are the registration-layer
/// overrides; a frontmatter declaration of either key (the
/// import-from-cc-codex `disable-model-invocation: true`) ORs in, matching
/// the TS `metadata` merge (`{...parsed.metadata, ...extraMetadata}`).
fn make(
    name: &str,
    pseudo_path: &str,
    content: &'static str,
    disable_model_invocation: bool,
    is_sub_skill: bool,
) -> BuiltinSkillDef {
    let meta = parse_skill_metadata(content);
    BuiltinSkillDef {
        def: SkillDef {
            name: name.to_string(),
            description: meta.description,
            path: PathBuf::from(pseudo_path),
            source: SkillSource::Builtin,
            plugin: None,
            disable_model_invocation: disable_model_invocation || meta.disable_model_invocation,
            is_sub_skill: is_sub_skill || meta.is_sub_skill,
        },
        content,
    }
}

/// The eight builtin skill entries (six skills; the sub-skill bundle
/// registers its parent plus both children), in the TS `BUILTIN_SKILLS`
/// order.
pub fn builtin_skills() -> Vec<BuiltinSkillDef> {
    vec![
        make(
            "mcp-config",
            "builtin://mcp-config",
            MCP_CONFIG_BODY,
            true,
            false,
        ),
        make(
            "import-from-cc-codex",
            "builtin://import-from-cc-codex",
            IMPORT_FROM_CC_CODEX_BODY,
            false,
            false,
        ),
        make(
            "update-config",
            "builtin://update-config",
            UPDATE_CONFIG_BODY,
            false,
            false,
        ),
        make(
            "custom-theme",
            "builtin://custom-theme",
            CUSTOM_THEME_BODY,
            true,
            false,
        ),
        make(
            "write-goal",
            "builtin://write-goal",
            WRITE_GOAL_BODY,
            false,
            false,
        ),
        make(
            "sub-skill",
            "builtin://sub-skill",
            SUB_SKILL_PARENT_BODY,
            true,
            false,
        ),
        make(
            "sub-skill.review",
            "builtin://sub-skill/review",
            SUB_SKILL_REVIEW_BODY,
            true,
            true,
        ),
        make(
            "sub-skill.consolidate",
            "builtin://sub-skill/consolidate",
            SUB_SKILL_CONSOLIDATE_BODY,
            true,
            true,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given the builtin bundle, when it is constructed, then all eight
    /// entries carry the builtin source, the TS-literal `builtin://` pseudo
    /// paths (slash-separated for the sub-skill family), unique names, and
    /// each embedded body is non-empty with its frontmatter description
    /// parsed.
    #[test]
    fn should_build_eight_builtin_entries_with_pseudo_paths_when_constructing_the_bundle() {
        let skills = builtin_skills();

        assert_eq!(skills.len(), 8, "six skills + two sub-skill children");
        let mut names: Vec<&str> = skills.iter().map(|s| s.def.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "custom-theme",
                "import-from-cc-codex",
                "mcp-config",
                "sub-skill",
                "sub-skill.consolidate",
                "sub-skill.review",
                "update-config",
                "write-goal",
            ]
        );
        // The TS `PSEUDO_PATH` literals verbatim — the sub-skill family's
        // pseudo paths are slash-separated even though the registered names
        // keep their dots (`sub-skill.ts` lines 39/46/53).
        let expected_paths = [
            ("custom-theme", "builtin://custom-theme"),
            ("import-from-cc-codex", "builtin://import-from-cc-codex"),
            ("mcp-config", "builtin://mcp-config"),
            ("sub-skill", "builtin://sub-skill"),
            ("sub-skill.consolidate", "builtin://sub-skill/consolidate"),
            ("sub-skill.review", "builtin://sub-skill/review"),
            ("update-config", "builtin://update-config"),
            ("write-goal", "builtin://write-goal"),
        ];
        for (name, expected_path) in expected_paths {
            let skill = skills
                .iter()
                .find(|s| s.def.name == name)
                .unwrap_or_else(|| panic!("{name} entry"));
            assert_eq!(skill.def.source, SkillSource::Builtin);
            assert_eq!(
                skill.def.path,
                PathBuf::from(expected_path),
                "{name}: the TS-literal pseudo path"
            );
            assert!(!skill.def.description.is_empty(), "{}", skill.def.name);
            assert!(!skill.content.is_empty(), "{}", skill.def.name);
        }
    }

    /// Given the builtin bundle, when the gate flags are read, then exactly
    /// the TS override points hold: mcp-config / custom-theme / the
    /// sub-skill family disable model invocation, import-from-cc-codex gets
    /// it from its frontmatter, write-goal / update-config are
    /// model-invocable, and only the sub-skill children carry the sub-skill
    /// marker.
    #[test]
    fn should_set_gate_flags_only_on_the_ts_override_points_when_constructing_the_bundle() {
        let flags: Vec<(String, bool, bool)> = builtin_skills()
            .into_iter()
            .map(|s| {
                (
                    s.def.name,
                    s.def.disable_model_invocation,
                    s.def.is_sub_skill,
                )
            })
            .collect();

        let expected = [
            ("mcp-config".to_string(), true, false),
            ("import-from-cc-codex".to_string(), true, false),
            ("update-config".to_string(), false, false),
            ("custom-theme".to_string(), true, false),
            ("write-goal".to_string(), false, false),
            ("sub-skill".to_string(), true, false),
            ("sub-skill.review".to_string(), true, true),
            ("sub-skill.consolidate".to_string(), true, true),
        ];
        assert_eq!(flags, expected.to_vec());
    }

    /// Given the embedded bodies, when they are read, then the
    /// import-from-cc-codex frontmatter declares `disable-model-invocation`
    /// and the sub-skill parent declares `has-sub-skill` (the raw v2
    /// frontmatter, unaffected by the wrapper overrides).
    #[test]
    fn should_keep_raw_frontmatter_flags_when_parsing_embedded_bodies() {
        assert!(IMPORT_FROM_CC_CODEX_BODY.contains("\ndisable-model-invocation: true\n"));
        assert!(SUB_SKILL_PARENT_BODY.contains("\nhas-sub-skill: true\n"));
    }
}
