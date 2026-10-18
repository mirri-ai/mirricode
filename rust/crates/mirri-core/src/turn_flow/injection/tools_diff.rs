//! Tool-set change announcements between turns.

use super::*;

// ====================================================================
// ToolsDiffInjector
// ====================================================================

/// Announces changes to the available tool set.
///
/// Folds the announced tool names from history (re-derives each call) and
/// diffs against the current enabled tools snapshot.
pub struct ToolsDiffInjector {
    current_tools: Vec<String>,
    disclosure_enabled: bool,
}

impl ToolsDiffInjector {
    /// Create a tools-diff injector with a snapshot of the currently enabled
    /// tools (typically from `ToolManager::enabled_tools()`) and whether
    /// progressive tool disclosure is active (typically from
    /// `ToolManager::disclosure_enabled()`). When disclosure is disabled,
    /// `select_tools` is not configured, so the select_tools instruction tail
    /// is omitted from announcements (tool-calling ADR-3). Whitelist glob
    /// patterns (`mcp__*`, `mcp__<server>__*`) are dropped from the snapshot:
    /// they are activation directives, not callable tool names, and must not
    /// be announced to the LLM as tools.
    pub fn new(current_tools: Vec<String>, disclosure_enabled: bool) -> Self {
        Self {
            current_tools: current_tools
                .into_iter()
                .filter(|name| !name.ends_with('*'))
                .collect(),
            disclosure_enabled,
        }
    }
}

/// Parse `<tools_added>...</tools_added>` and `<tools_removed>...</tools_removed>`
/// blocks from a text string, applying them to the `announced` set.
fn fold_tool_announcements(text: &str, announced: &mut HashSet<String>) {
    for block in text.split("<tools_added>").skip(1) {
        if let Some(end) = block.find("</tools_added>") {
            for line in block[..end].lines() {
                let name = line.trim();
                if !name.is_empty() {
                    announced.insert(name.to_string());
                }
            }
        }
    }
    for block in text.split("<tools_removed>").skip(1) {
        if let Some(end) = block.find("</tools_removed>") {
            for line in block[..end].lines() {
                let name = line.trim();
                if !name.is_empty() {
                    announced.remove(name);
                }
            }
        }
    }
}

impl Injector for ToolsDiffInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        let history = memory.history();

        // Fold announced tools from history.
        let mut announced: HashSet<String> = HashSet::new();
        for m in history.iter() {
            if m.origin == Some(PromptOrigin::SystemTrigger)
                || m.origin == Some(PromptOrigin::Injection)
            {
                let text = m.message.text_content();
                if text.contains("<tools_added>") || text.contains("<tools_removed>") {
                    fold_tool_announcements(&text, &mut announced);
                }
            }
        }

        let current_set: HashSet<&str> = self.current_tools.iter().map(|s| s.as_str()).collect();

        // Added: in current but not in announced. Preserve order of current_tools.
        let added: Vec<&String> = self
            .current_tools
            .iter()
            .filter(|t| !announced.contains(*t))
            .collect();

        // Removed: in announced but not in current. Sort alphabetically.
        let mut removed: Vec<String> = announced
            .iter()
            .filter(|t| !current_set.contains(t.as_str()))
            .cloned()
            .collect();
        removed.sort();

        if added.is_empty() && removed.is_empty() {
            return None;
        }

        let mut blocks: Vec<String> = Vec::new();

        if !added.is_empty() {
            let mut block = String::from("<tools_added>\n");
            for name in &added {
                block.push_str(name);
                block.push('\n');
            }
            block.push_str("</tools_added>");
            blocks.push(block);
        }

        if !removed.is_empty() {
            let mut block = String::from("<tools_removed>\n");
            for name in &removed {
                block.push_str(name);
                block.push('\n');
            }
            block.push_str("</tools_removed>");
            blocks.push(block);
        }

        let mut text = blocks.join("\n\n");
        if self.disclosure_enabled {
            text.push_str(
                "\n\n\
Use the select_tools tool with exact names to load full tool definitions before calling them. Names listed as removed are no longer loadable — do not select them. Fold all announcements in this conversation in order to get the current list.",
            );
        }

        Some(text)
    }
}
