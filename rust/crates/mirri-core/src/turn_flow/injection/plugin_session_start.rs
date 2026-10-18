//! `plugin_session_start` injection — the item-I sessionStart reminder.
//!
//! Mirrors the TS `AgentPluginService` (`agent/plugin/agentPluginService.ts`):
//! for every enabled plugin's `sessionStart:{skill}` declaration, render a
//! `<plugin_session_start plugin=".." skill="..">` block carrying the
//! skill's prompt (the plugin `skillInstructions` prefix included, item J)
//! and inject it into the MAIN agent's context — once per session. The
//! once-only semantics mirror the TS dynamic injector's
//! `injectedPositions.length > 0 → undefined` gate: the injector scans the
//! history for an earlier `<plugin_session_start` marker and stays silent
//! when one exists (a resume replays the recorded injection back into
//! memory, so the dedup holds across restarts).
//!
//! The injected text is wrapped and recorded by the standard per-step
//! injection path (`context.append_message` wire ops), so no new wire
//! domain is introduced (contract 2).
//!
//! Known divergence (recorded in `plan/rust-plugin-parity.md`): the TS
//! "skill change → append a fresh reminder with a supersede line, or
//! neutralize" path (`appendFreshSessionStartReminder` on plugin skill
//! reload) has no rust equivalent — plugin reload does not refresh live
//! sessions (item C4). This injector is startup-once per session.

use std::sync::Arc;

use crate::context::ContextMessage;
use crate::context_memory::ContextMemory;
use crate::origin::PromptOrigin;
use crate::plugin_runtime::PluginSessionStartSpec;
use crate::skill::{SkillCatalog, escape_xml_attr};

use super::Injector;

/// The marker this injector scans history for — the tag the TS engine uses
/// as its `plugin_session_start` injection variant.
const SESSION_START_MARKER: &str = "<plugin_session_start";

/// One session's plugin `sessionStart` declarations, rendered into the main
/// agent's context by the injection manager.
pub struct PluginSessionStartInjector {
    session_starts: Vec<PluginSessionStartSpec>,
    /// The workspace-scoped skill catalog the declared skills resolve
    /// through (`getPluginSkill` + `renderSkillPrompt`).
    catalog: Arc<SkillCatalog>,
}

impl PluginSessionStartInjector {
    /// Create the injector with the session's plugin session starts (the
    /// composition root assembles them from the plugin manager — zero-IO
    /// contract 6) and the session's skill catalog.
    pub fn new(session_starts: Vec<PluginSessionStartSpec>, catalog: Arc<SkillCatalog>) -> Self {
        Self {
            session_starts,
            catalog,
        }
    }
}

impl Injector for PluginSessionStartInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        if self.session_starts.is_empty() {
            return None;
        }
        // Once-only per session (the TS `injectedPositions.length > 0`
        // gate): an earlier injection — live or replayed — suppresses any
        // further reminder. The scan mirrors the TS
        // `shouldNeutralizePluginSessionStart` predicate: an Injection-origin
        // message carrying the marker, or a compaction summary (which may
        // have folded an earlier reminder's guidance into it) both count as
        // "plugin guidance already present" — so a post-compaction step does
        // not re-inject a duplicate block.
        if memory.history().iter().any(message_counts_as_session_start) {
            return None;
        }
        let mut blocks = Vec::new();
        for session_start in &self.session_starts {
            let Some(skill) = self
                .catalog
                .get_plugin_skill(&session_start.plugin_id, &session_start.skill_name)
            else {
                tracing::warn!(
                    plugin_id = %session_start.plugin_id,
                    skill = %session_start.skill_name,
                    "plugin sessionStart skill not found"
                );
                continue;
            };
            let Some(content) = self.catalog.render_skill_prompt(&skill.name) else {
                continue;
            };
            blocks.push(format!(
                "<plugin_session_start plugin=\"{}\" skill=\"{}\">\n{content}\n</plugin_session_start>",
                escape_xml_attr(&session_start.plugin_id),
                escape_xml_attr(&skill.name),
            ));
        }
        if blocks.is_empty() {
            return None;
        }
        Some(blocks.join("\n"))
    }
}

/// Whether a context message counts as "plugin session-start guidance
/// already present" for the once-only gate — the TS
/// `shouldNeutralizePluginSessionStart` analog: an Injection-origin message
/// carrying the marker, or any compaction summary (whose folded content may
/// include an earlier reminder).
fn message_counts_as_session_start(message: &ContextMessage) -> bool {
    match &message.origin {
        Some(PromptOrigin::Injection) => message_text_contains(message, SESSION_START_MARKER),
        Some(PromptOrigin::CompactionSummary) => true,
        _ => false,
    }
}

/// Whether any text part of a context message carries the given marker.
fn message_text_contains(message: &ContextMessage, marker: &str) -> bool {
    message.message.content.iter().any(|part| match part {
        crate::message::ContentPart::Text { text } => text.contains(marker),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_memory::ContextMemory;
    use crate::message::ContentPart;
    use crate::plugin_runtime::PluginSessionStartSpec;
    use crate::skill::{SKILL_MD_FILE, SkillPluginContext};
    use std::fs;
    use tempfile::TempDir;

    /// A catalog with one plugin-contributed skill (optionally carrying
    /// instructions).
    fn catalog_with_plugin_skill(
        skill_name: &str,
        body: &str,
        instructions: Option<&str>,
    ) -> (TempDir, Arc<SkillCatalog>) {
        catalog_with_plugin_skill_by("demo", skill_name, body, instructions)
    }

    fn catalog_with_plugin_skill_by(
        plugin_id: &str,
        skill_name: &str,
        body: &str,
        instructions: Option<&str>,
    ) -> (TempDir, Arc<SkillCatalog>) {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("skills").join(skill_name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SKILL_MD_FILE), body).unwrap();
        let catalog = Arc::new(SkillCatalog::new());
        catalog
            .discover_plugin(
                &temp.path().join("skills"),
                &SkillPluginContext {
                    id: plugin_id.into(),
                    instructions: instructions.map(str::to_string),
                },
            )
            .unwrap();
        (temp, catalog)
    }

    fn injector(
        starts: Vec<PluginSessionStartSpec>,
        catalog: Arc<SkillCatalog>,
    ) -> PluginSessionStartInjector {
        PluginSessionStartInjector::new(starts, catalog)
    }

    /// Given a session with one plugin sessionStart and a fresh memory, when
    /// the injector runs, then the `<plugin_session_start>` block is
    /// produced with the escaped plugin/skill attributes and the skill body
    /// (the TS `renderPluginSessionStartReminder` shape).
    #[test]
    fn should_inject_one_session_start_block_when_session_starts_exist() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Do the demo thing.", None);
        let injector = injector(
            vec![PluginSessionStartSpec {
                plugin_id: "demo".into(),
                skill_name: "greeter".into(),
            }],
            catalog,
        );
        let memory = ContextMemory::default();

        let text = injector.inject(&memory).expect("reminder text");

        assert!(
            text.starts_with(
                "<plugin_session_start plugin=\"demo\" skill=\"greeter\">\nDo the demo thing.\n</plugin_session_start>"
            ),
            "unexpected block: {text}"
        );
    }

    /// Given a plugin skill carrying `skillInstructions`, when the injector
    /// runs, then the rendered prompt includes the `<kimi-plugin-instructions>`
    /// prefix — the item-J consumption composes with the item-I block.
    #[test]
    fn should_include_plugin_instructions_in_the_session_start_block() {
        let (_temp, catalog) =
            catalog_with_plugin_skill("greeter", "Do the demo thing.", Some("Always be helpful."));
        let injector = injector(
            vec![PluginSessionStartSpec {
                plugin_id: "demo".into(),
                skill_name: "greeter".into(),
            }],
            catalog,
        );
        let memory = ContextMemory::default();

        let text = injector.inject(&memory).expect("reminder text");

        assert!(
            text.contains(
                "<kimi-plugin-instructions plugin=\"demo\">\nAlways be helpful.\n</kimi-plugin-instructions>"
            ),
            "the instructions must ride the block: {text}"
        );
        assert!(text.contains("Do the demo thing."));
    }

    /// Given two session starts, when the injector runs, then both blocks
    /// are joined with a newline (the TS `blocks.join('\n')`).
    #[test]
    fn should_join_multiple_session_start_blocks_with_newline() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Greet.", None);
        let injector = injector(
            vec![
                PluginSessionStartSpec {
                    plugin_id: "demo".into(),
                    skill_name: "greeter".into(),
                },
                PluginSessionStartSpec {
                    plugin_id: "demo".into(),
                    skill_name: "greeter".into(),
                },
            ],
            catalog,
        );
        let memory = ContextMemory::default();

        let text = injector.inject(&memory).expect("reminder text");
        assert_eq!(text.matches("<plugin_session_start").count(), 2);
        assert!(
            text.contains("</plugin_session_start>\n<plugin_session_start"),
            "blocks join with a newline: {text}"
        );
    }

    /// Given a memory that already carries a `<plugin_session_start>`
    /// injection, when the injector runs, then it stays silent — the
    /// TS once-only gate (a live re-injection or a replayed resume).
    #[test]
    fn should_not_reinject_when_history_already_has_a_session_start() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Greet.", None);
        let injector = injector(
            vec![PluginSessionStartSpec {
                plugin_id: "demo".into(),
                skill_name: "greeter".into(),
            }],
            catalog,
        );
        let mut memory = ContextMemory::default();
        memory.append_system_reminder(
            "<plugin_session_start plugin=\"demo\" skill=\"greeter\">\nold\n</plugin_session_start>".to_string(),
        );

        assert!(injector.inject(&memory).is_none());
    }

    /// Given no session starts, when the injector runs, then nothing is
    /// injected.
    #[test]
    fn should_inject_nothing_when_no_session_starts_exist() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Greet.", None);
        let injector = injector(Vec::new(), catalog);
        assert!(injector.inject(&ContextMemory::default()).is_none());
    }

    /// Given a sessionStart whose skill is not resolvable through the
    /// catalog, when the injector runs, then the declared block is skipped
    /// with a warn — the TS `skill === undefined` skip.
    #[test]
    fn should_skip_session_start_whose_skill_is_not_resolvable() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Greet.", None);
        let injector = injector(
            vec![
                PluginSessionStartSpec {
                    plugin_id: "demo".into(),
                    skill_name: "missing".into(),
                },
                PluginSessionStartSpec {
                    plugin_id: "demo".into(),
                    skill_name: "greeter".into(),
                },
            ],
            catalog,
        );
        let memory = ContextMemory::default();

        let text = injector
            .inject(&memory)
            .expect("the resolvable block still injects");
        assert!(
            text.contains("skill=\"greeter\""),
            "the unresolvable one is skipped: {text}"
        );
        assert!(!text.contains("missing"));
    }

    /// Given a sessionStart whose skill exists under a DIFFERENT plugin
    /// attribution, when the injector runs, then it does not match — the
    /// TS `getPluginSkill(pluginId, name)` plugin-scoped lookup.
    #[test]
    fn should_not_match_a_same_named_skill_from_another_plugin() {
        let (_temp, catalog) = catalog_with_plugin_skill("greeter", "Greet.", None);
        let injector = injector(
            vec![PluginSessionStartSpec {
                plugin_id: "other-plugin".into(),
                skill_name: "greeter".into(),
            }],
            catalog,
        );
        let memory = ContextMemory::default();

        assert!(injector.inject(&memory).is_none());
    }

    /// Given a session start with XML-special plugin id, when the block
    /// renders, then the attribute is escaped (the TS `escapeXmlAttr`).
    #[test]
    fn should_escape_plugin_and_skill_attributes_in_the_block() {
        let (_temp, catalog) = catalog_with_plugin_skill_by("weird\"id", "greeter", "Greet.", None);
        let injector = injector(
            vec![PluginSessionStartSpec {
                plugin_id: "weird\"id".into(),
                skill_name: "greeter".into(),
            }],
            catalog,
        );
        let memory = ContextMemory::default();

        let text = injector.inject(&memory).expect("reminder text");
        assert!(
            text.contains("plugin=\"weird&quot;id\""),
            "the plugin id attribute is escaped: {text}"
        );
    }

    /// The message scan used for the once-only gate mirrors the TS
    /// `shouldNeutralizePluginSessionStart`: an Injection-origin message
    /// carrying the marker counts, a user message merely quoting the tag
    /// does NOT, and a compaction summary counts (it may have folded an
    /// earlier reminder).
    #[test]
    fn should_only_match_injection_markers_and_compaction_summaries_for_the_gate() {
        // Given a memory where the marker text exists only inside a user
        // message quoting the tag
        let mut memory = ContextMemory::default();
        memory.append_user_message(
            vec![ContentPart::Text {
                text: "see: <plugin_session_start plugin=\"x\" ...".to_string(),
            }],
            PromptOrigin::User,
        );

        // When the scan runs
        let found = memory.history().iter().any(message_counts_as_session_start);

        // Then the user quote does not count as an injection
        assert!(!found);

        // And a compaction summary DOES count — the TS neutralization
        // considers it (plugin guidance may be folded inside).
        memory.append_user_message(
            vec![ContentPart::Text {
                text: "summary of earlier conversation".to_string(),
            }],
            PromptOrigin::CompactionSummary,
        );
        assert!(
            memory.history().iter().any(message_counts_as_session_start),
            "a compaction summary counts as plugin guidance already present"
        );
    }
}
