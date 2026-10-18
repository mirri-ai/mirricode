//! Plugin slash commands: the TS markdown-file model, the Rust inline-spec
//! extension, and the command registry.
//!
//! Plugins declare commands either as markdown files ([`CommandEntry::File`],
//! the TS model — frontmatter + `$ARGUMENTS` expansion, `commands.ts`) or as
//! inline specs ([`CommandEntry::Inline`], the Rust-only superset extension,
//! D-B9) carrying a minijinja prompt template. [`CommandRegistry`] indexes
//! the inline commands of all enabled plugins by name and renders their
//! templates at invocation time using minijinja; markdown commands are
//! parsed on demand via [`parse_command_text`] / [`load_command_file`] and
//! expanded with [`expand_command_arguments`] (the TS semantics).
//!
//! # Template context
//!
//! An inline command template is rendered with these context variables:
//!
//! - `$ARGUMENTS` — the raw argument string, substituted as a simple
//!   string replacement before minijinja rendering.
//! - `{{ arguments }}` — the raw argument string.
//! - `{{ arg1 }}`, `{{ arg2 }}`, … — positional arguments split by
//!   whitespace.
//! - Named arguments declared in the command's `arguments` spec — each
//!   `[name]` bracket binds the positional argument at the same index
//!   (e.g. `[file_path]` binds `{{ file_path }}` to the first argument).
//!
//! Full minijinja syntax is available: conditionals, loops, and filters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use mirri_core::profile::agent_md::{FrontmatterValue, parse_frontmatter};
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::warn;

use crate::error::{PluginError, PluginResult};

/// Matches every `[name]` group inside an argument specification string.
static ARGUMENT_SPEC_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\[([^\]]+)\]").expect("hardcoded argument spec regex is valid")
});

/// A slash command declared by a plugin.
///
/// Deserialised from manifest entries such as:
///
/// ```json
/// {
///   "name": "explain",
///   "description": "Explain code in detail",
///   "arguments": "[file_path]",
///   "template": "Analyze the code in {{ file_path }} and explain..."
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandSpec {
    /// Command name used to invoke it (e.g. `"explain"`).
    pub name: String,
    /// Short human-readable description shown in command listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Argument specification string. Bracketed names (e.g. `[file_path]`)
    /// declare named arguments that bind positionally to the invocation's
    /// whitespace-separated arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    /// Prompt template rendered on invocation (minijinja syntax).
    pub template: String,
}

/// A resolved markdown command file — the TS `PluginCommandEntry`
/// (`{ path, name }`, `commands.ts` `readCommands`): the absolute file path
/// and the command name derived from the path relative to its collection
/// root (`.md` stripped, platform separators replaced with `/`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandFile {
    /// Absolute path to the markdown file (realpath'd where possible).
    pub path: PathBuf,
    /// Command name from the path relative to the collection root.
    pub name: String,
}

/// One entry of the manifest `commands` field, after parsing.
///
/// The enum is untagged so that both the TS md-file object form (`{path,
/// name}`) and the inline spec form parse from the same JSON field, keeping
/// manifests written for previous versions compatible.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum CommandEntry {
    /// TS form: a markdown command file (resolved absolute path + name).
    File(CommandFile),
    /// Rust-only inline slash command with a prompt template (D-B9).
    Inline(CommandSpec),
}

/// A parsed markdown command — the TS `parseCommandText` result: the
/// `PluginCommandDef` guts (`name` / `description` / `body`) without the
/// plugin attribution and the file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandFile {
    /// Command name: frontmatter `name`, else the fallback name
    /// (the manifest entry name), else the file basename without `.md`.
    pub name: String,
    /// Frontmatter `description`, else the first non-empty body line.
    pub description: String,
    /// The trimmed Markdown body.
    pub body: String,
}

/// Parse a command markdown document into its definition — the TS
/// `parseCommandText` mirror (`commands.ts`): the frontmatter `name` /
/// `description` win, with the fallback name (manifest entry name, else
/// basename) and the first-body-line description as fallbacks.
///
/// Returns a human-readable reason when the frontmatter cannot be parsed
/// (missing closing fence, out-of-subset YAML) — the TS `parseFrontmatter`
/// throw path, surfaced as a dropped command by [`load_command_file`].
pub fn parse_command_text(
    text: &str,
    command_path: &Path,
    fallback_name: Option<&str>,
) -> Result<ParsedCommandFile, String> {
    let parsed = parse_frontmatter(text).map_err(|error| error.to_string())?;
    let frontmatter = parsed.data.unwrap_or_default();

    // The TS `name`: frontmatter name, else the fallback, else the basename
    // (always defined; an empty string when the path has no name).
    let base_name = fallback_name
        .map(str::to_string)
        .or_else(|| command_basename(command_path));
    let name = non_empty_string(frontmatter.get("name"))
        .or(base_name)
        .unwrap_or_default();

    let body = parsed.body.trim().to_string();
    let description = non_empty_string(frontmatter.get("description"))
        .unwrap_or_else(|| description_from_body(&body));

    Ok(ParsedCommandFile {
        name,
        description,
        body,
    })
}

/// Load and parse a command markdown file — the TS `loadPluginCommand`
/// mirror: a read failure or a frontmatter parse failure both drop the
/// command silently (`undefined`), matching the TS catch-all.
pub fn load_command_file(command_path: &Path, fallback_name: Option<&str>) -> Option<ParsedCommandFile> {
    let text = std::fs::read_to_string(command_path).ok()?;
    parse_command_text(&text, command_path, fallback_name).ok()
}

/// Expand a command body with the invocation's argument string — the TS
/// `expandCommandArguments` mirror: every `$ARGUMENTS` occurrence is
/// replaced verbatim; when the template has **no** placeholder and the
/// arguments are non-empty, the arguments are appended as a trailing
/// `ARGUMENTS: <args>` block.
pub fn expand_command_arguments(body: &str, args: &str) -> String {
    let replaced = body.replace("$ARGUMENTS", args);
    if !body.contains("$ARGUMENTS") && !args.is_empty() {
        format!("{replaced}\n\nARGUMENTS: {args}")
    } else {
        replaced
    }
}

/// The frontmatter value as a non-empty trimmed string, else `None` — the
/// TS `nonEmptyString` (`commands.ts`): only string scalars count, and
/// blanks behave as missing.
fn non_empty_string(value: Option<&FrontmatterValue>) -> Option<String> {
    let FrontmatterValue::Str(value) = value? else {
        return None;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The file basename without a case-insensitive trailing `.md` — the TS
/// `path.basename(commandPath).replace(/\.md$/i, '')`.
fn command_basename(command_path: &Path) -> Option<String> {
    command_path
        .file_name()
        .map(|name| strip_md_suffix(&name.to_string_lossy()))
}

/// Strip a case-insensitive trailing `.md` suffix (the TS `/\.md$/i`).
fn strip_md_suffix(name: &str) -> String {
    if name.to_ascii_lowercase().ends_with(".md") {
        name[..name.len() - 3].to_string()
    } else {
        name.to_string()
    }
}

/// The TS `descriptionFromBody` mirror (`commands.ts`): the first non-empty
/// trimmed body line, truncated to 240 characters (`239 + "…"`) — the
/// truncation is char-based here, the JS `slice(0, 239)` is UTF-16 based
/// (identical for ASCII). An empty body yields `"No description provided."`.
fn description_from_body(body: &str) -> String {
    let first_line = body
        .split(['\r', '\n'])
        .map(str::trim)
        .find(|line| !line.is_empty());
    match first_line {
        None => "No description provided.".to_string(),
        Some(line) if line.chars().count() > 240 => {
            let truncated: String = line.chars().take(239).collect();
            format!("{truncated}…")
        }
        Some(line) => line.to_string(),
    }
}

/// A command resolved from a plugin, ready to execute.
#[derive(Debug, Clone)]
pub struct RegisteredCommand {
    /// Name of the plugin that contributed this command.
    pub plugin: String,
    /// The command definition.
    pub spec: CommandSpec,
}

/// Extract the named arguments declared in an argument specification.
///
/// Every `[name]` bracket group becomes a named argument, in order. Text
/// outside brackets is ignored, so both `"[file_path] [line]"` and
/// `"[file_path]: the file to analyze"` parse to the same names.
pub fn argument_names_from_spec(spec: &str) -> Vec<String> {
    ARGUMENT_SPEC_RE
        .captures_iter(spec)
        .map(|captures| captures[1].trim().to_string())
        .collect()
}

/// In-memory registry of prompt-building slash commands contributed by
/// plugins.
///
/// Thread-safe via `RwLock`. Name conflicts are resolved in favour of the
/// most recently registered plugin, with a warning logged, so a plugin
/// enabled later overrides an earlier one.
#[derive(Default)]
pub struct CommandRegistry {
    commands: RwLock<HashMap<String, RegisteredCommand>>,
}

impl CommandRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register all inline commands contributed by one plugin.
    ///
    /// If a command name is already registered (by another plugin or by an
    /// earlier registration from the same plugin), the new definition
    /// wins and a warning is logged.
    pub fn register_plugin(&self, plugin_name: &str, specs: &[CommandSpec]) {
        let mut commands = self
            .commands
            .write()
            .expect("command registry lock poisoned");
        for spec in specs {
            if commands.contains_key(&spec.name) {
                warn!(
                    plugin = %plugin_name,
                    command = %spec.name,
                    "command name conflict, later registration wins"
                );
            }
            commands.insert(
                spec.name.clone(),
                RegisteredCommand {
                    plugin: plugin_name.to_string(),
                    spec: spec.clone(),
                },
            );
        }
    }

    /// Remove every command contributed by `plugin_name`.
    ///
    /// Returns the number of commands removed. Used when a plugin is
    /// disabled or removed.
    pub fn unregister_plugin(&self, plugin_name: &str) -> usize {
        let mut commands = self
            .commands
            .write()
            .expect("command registry lock poisoned");
        let count_before = commands.len();
        commands.retain(|_, registered| registered.plugin != plugin_name);
        count_before - commands.len()
    }

    /// Replace the whole registry with commands from the given plugins.
    ///
    /// This is the dynamic-reload entry point: pass the current set of
    /// enabled plugins as `(plugin_name, inline_command_specs)` pairs to
    /// reflect installs, enables, disables, and removes.
    pub fn reload<I>(&self, plugins: I)
    where
        I: IntoIterator<Item = (String, Vec<CommandSpec>)>,
    {
        self.commands
            .write()
            .expect("command registry lock poisoned")
            .clear();
        for (plugin_name, specs) in plugins {
            self.register_plugin(&plugin_name, &specs);
        }
    }

    /// Look up a command by name.
    pub fn get(&self, name: &str) -> Option<RegisteredCommand> {
        self.commands
            .read()
            .expect("command registry lock poisoned")
            .get(name)
            .cloned()
    }

    /// List all registered commands, sorted by command name.
    pub fn list(&self) -> Vec<RegisteredCommand> {
        let mut all: Vec<RegisteredCommand> = self
            .commands
            .read()
            .expect("command registry lock poisoned")
            .values()
            .cloned()
            .collect();
        all.sort_by(|left, right| left.spec.name.cmp(&right.spec.name));
        all
    }

    /// Execute a command: render its template with the given argument
    /// string.
    ///
    /// # Errors
    ///
    /// - [`PluginError::CommandNotFound`] if no command with that name is
    ///   registered.
    /// - [`PluginError::Template`] if the template cannot be rendered.
    pub fn execute(&self, command_name: &str, args: &str) -> PluginResult<String> {
        let registered = self
            .get(command_name)
            .ok_or_else(|| PluginError::CommandNotFound(command_name.to_string()))?;

        // Expand $ARGUMENTS as a simple raw substitution before minijinja
        // rendering.
        let expanded_template = registered.spec.template.replace("$ARGUMENTS", args);

        // Positional arguments: whitespace-separated tokens.
        let positional: Vec<&str> = args.split_whitespace().collect();

        let mut context = JsonMap::new();
        context.insert("arguments".to_string(), JsonValue::String(args.to_string()));
        for (index, arg) in positional.iter().enumerate() {
            context.insert(
                format!("arg{}", index + 1),
                JsonValue::String(arg.to_string()),
            );
        }

        // Named arguments from the argument spec bind positionally to the
        // same-index positional argument; declared-but-absent names stay
        // empty so templates degrade gracefully.
        let declared_names =
            argument_names_from_spec(registered.spec.arguments.as_deref().unwrap_or(""));
        for (index, name) in declared_names.iter().enumerate() {
            let value = positional.get(index).copied().unwrap_or("");
            context.insert(name.clone(), JsonValue::String(value.to_string()));
        }

        render_template(&expanded_template, &JsonValue::Object(context))
    }
}

/// Render a command template with minijinja.
///
/// Undefined variables render as empty strings (lenient behaviour), so a
/// template referencing an unbound named argument does not fail the whole
/// invocation.
fn render_template(template: &str, context: &JsonValue) -> PluginResult<String> {
    let mut environment = minijinja::Environment::new();
    environment.set_undefined_behavior(minijinja::UndefinedBehavior::Lenient);
    environment
        .render_str(template, context)
        .map_err(|error| PluginError::Template(error.to_string()))
}

// ---------------------------------------------------------------------------
// Markdown command collection — the TS `readCommands` helpers
// (`commands.ts` / `manifest.ts` `listMarkdownFilesRecursive`, B9)
// ---------------------------------------------------------------------------

/// Collect every `.md` file under `root`, recursively, with its command
/// name derived from the path relative to `root` — the TS
/// `listMarkdownFilesRecursive` mirror. Unreadable directories are skipped
/// silently (the TS `readdir` catch).
pub(crate) fn list_markdown_files_recursive(root: &Path) -> Vec<CommandFile> {
    let mut out = Vec::new();
    walk_markdown(root, root, &mut out);
    out
}

fn walk_markdown(root: &Path, dir: &Path, out: &mut Vec<CommandFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let full = entry.path();
        if file_type.is_dir() {
            walk_markdown(root, &full, out);
        } else if file_type.is_file() && is_md_file_name(&full) {
            let name = command_name_from_file(&full, root);
            out.push(CommandFile { path: full, name });
        }
    }
}

/// The TS `entry.name.endsWith('.md')` — a case-sensitive filename suffix
/// check (the `commandNameFromFile` `.md` strip, in contrast, is
/// case-insensitive).
pub(crate) fn is_md_file_name(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".md"))
}

/// The command name of a markdown file relative to a collection root — the
/// TS `commandNameFromFile`: the relative path with a case-insensitive
/// `.md` suffix stripped and platform separators replaced with `/`.
pub(crate) fn command_name_from_file(file: &Path, root: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    strip_md_suffix(&relative.to_string_lossy())
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_replace_arg_placeholder_when_body_contains_dollar_arguments() {
        let expanded = expand_command_arguments("Read $ARGUMENTS and summarize.", "src/a.rs src/b.rs");
        assert_eq!(expanded, "Read src/a.rs src/b.rs and summarize.");
    }

    #[test]
    fn should_append_arguments_block_when_body_has_no_placeholder_and_args_non_empty() {
        let expanded = expand_command_arguments("Do the thing", "x y");
        assert_eq!(expanded, "Do the thing\n\nARGUMENTS: x y");
    }

    #[test]
    fn should_not_append_arguments_when_args_are_empty() {
        assert_eq!(expand_command_arguments("Do the thing", ""), "Do the thing");
    }

    #[test]
    fn should_not_append_arguments_when_placeholder_is_present() {
        assert_eq!(expand_command_arguments("$ARGUMENTS", "x"), "x");
    }

    #[test]
    fn should_parse_name_and_description_when_frontmatter_is_present() {
        let text = "---\nname: my-cmd\ndescription: Does things\n---\n\nBody here.";
        let parsed = parse_command_text(text, Path::new("/p/anything.md"), None)
            .expect("parse should succeed");
        assert_eq!(parsed.name, "my-cmd");
        assert_eq!(parsed.description, "Does things");
        assert_eq!(parsed.body, "Body here.");
    }

    #[test]
    fn should_fallback_name_to_basename_when_frontmatter_name_is_missing() {
        let parsed = parse_command_text("# Just a body", Path::new("/p/hello.md"), None)
            .expect("parse should succeed");
        assert_eq!(parsed.name, "hello");
        assert_eq!(parsed.body, "# Just a body");
    }

    #[test]
    fn should_fallback_name_to_named_entry_when_manifest_provides_it() {
        // The TS `fallbackName` (the `PluginCommandEntry.name` from a
        // directory collection, e.g. "sub/tool") beats the basename.
        let parsed = parse_command_text("body", Path::new("/p/ignored.md"), Some("sub/tool"))
            .expect("parse should succeed");
        assert_eq!(parsed.name, "sub/tool");
    }

    #[test]
    fn should_fallback_description_to_first_body_line_when_frontmatter_description_is_missing() {
        let parsed = parse_command_text(
            "\nFirst line.\nSecond line.",
            Path::new("/p/c.md"),
            None,
        )
        .expect("parse should succeed");
        // The body is trimmed before the first-line scan, so the leading
        // blank line does not become the description.
        assert_eq!(parsed.description, "First line.");
        assert_eq!(parsed.body, "First line.\nSecond line.");
    }

    #[test]
    fn should_use_no_description_provided_when_body_is_empty() {
        let parsed = parse_command_text("---\n---\n\n   \n", Path::new("/p/e.md"), None)
            .expect("parse should succeed");
        assert_eq!(parsed.description, "No description provided.");
    }

    #[test]
    fn should_truncate_description_over_240_chars_with_ellipsis() {
        let long_line = format!("{}\nrest", "x".repeat(300));
        let parsed = parse_command_text(&long_line, Path::new("/p/l.md"), None)
            .expect("parse should succeed");
        assert_eq!(parsed.description.chars().count(), 240);
        assert!(parsed.description.ends_with('…'));
        assert_eq!(parsed.description.chars().filter(|&c| c == 'x').count(), 239);
    }

    #[test]
    fn should_drop_command_when_frontmatter_fence_is_unterminated() {
        let text = "---\nname: broken\nno closing fence";
        assert!(parse_command_text(text, Path::new("/p/b.md"), None).is_err());
    }

    #[test]
    fn should_collect_markdown_files_recursively_when_given_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("zeta.md"), "# z").unwrap();
        std::fs::write(dir.path().join("sub").join("alpha.md"), "# a").unwrap();
        std::fs::write(dir.path().join("sub").join("notes.txt"), "not md").unwrap();
        std::fs::write(dir.path().join("README.MD"), "upper ext").unwrap();

        let mut files = list_markdown_files_recursive(dir.path());
        files.sort_by(|a, b| a.name.cmp(&b.name));

        // `.MD` is not collected (the TS `endsWith('.md')` is
        // case-sensitive); `notes.txt` never matches.
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "sub/alpha");
        assert_eq!(files[1].name, "zeta");
        assert!(files[0].path.ends_with("sub/alpha.md"));
    }
}
