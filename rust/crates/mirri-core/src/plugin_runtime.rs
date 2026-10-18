//! Plugin runtime consumption data — the pure-data channel between the
//! plugin host and the engine (zero-IO contract 6).
//!
//! The engine (mirri-core) never touches [`PluginManager`]; hosts assemble
//! these values at the composition root (mirri-embedded bootstrap / session
//! assembly) and pass them into the scopes as plain data. The shapes mirror
//! the TS `EnabledPluginSessionStart` / `EnabledPluginSystemPrompt`
//! (`plugin/types.ts`): `{pluginId, skillName}` and `{pluginId, content}`.

use serde::{Deserialize, Serialize};

/// One enabled plugin's rendered system prompt — the TS
/// `EnabledPluginSystemPrompt` (`{pluginId, content}`, camelCase wire shape).
///
/// Rendered at manifest parse time (batch 1: B6); consumed by the system
/// prompt template's `{{plugin_sections}}` slot (batch 3, item J).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSystemPromptSection {
    /// The contributing plugin's id.
    pub plugin_id: String,
    /// The rendered prompt content (inline + file parts joined, trimmed).
    pub content: String,
}

/// One enabled plugin's `sessionStart:{skill}` declaration — the TS
/// `EnabledPluginSessionStart` (`{pluginId, skillName}`, camelCase wire
/// shape). Consumed by the main agent's `<plugin_session_start>` reminder
/// injection (batch 3, item I).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSessionStartSpec {
    /// The contributing plugin's id.
    pub plugin_id: String,
    /// The manifest-declared skill name (`sessionStart.skill`).
    pub skill_name: String,
}
