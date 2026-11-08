//! Model-specific prompt files: the composition root's IO half.
//!
//! Requirement D-M13 / architecture C9. [`mirri_core::model_prompt`] owns the
//! pure decisions — the builtin rule table, the path-sanitization rule, the
//! file format. This module owns the filesystem: reading `model_prompts/`
//! under the Mirri home, materializing a file for a newly configured model,
//! and serving the edit surface.
//!
//! Placement mirrors the existing `SYSTEM.md` channel (see
//! [`EmbeddedServer::read_system_md_override`]): the composition root reads
//! the files and seeds plain data into the session config, so the engine
//! never performs IO itself (rust/AGENTS.md 契约 6).
//!
//! Two behaviours are easy to get wrong and are pinned by tests below:
//!
//! - **Never rewrite an existing file.** Materialization writes only when the
//!   target is absent. Product rule updates therefore never overwrite a
//!   user's tuning.
//! - **Identity comes from the frontmatter, not the file name.** Two files
//!   declaring the same owner resolve deterministically (lexicographically
//!   first name wins) instead of racing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mirri_core::model_prompt::{
    MAX_PROMPT_BODY_BYTES, builtin_prompt_for, parse_prompt_file, prompt_body_injectable,
    prompt_file_stem, render_prompt_file,
};
use mirri_os::{LocalSyncFs, SyncFs};

/// The directory (under the Mirri home) holding one Markdown file per model.
const MODEL_PROMPTS_DIR: &str = "model_prompts";

/// One resolved prompt: the file it came from and its injectable body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelPrompt {
    /// Absolute path of the file the body was read from.
    pub path: PathBuf,
    /// The prompt body (frontmatter stripped).
    pub body: String,
}

/// A prompt file as the edit surface sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPromptFile {
    /// The raw model name this file belongs to (from the frontmatter).
    pub model: String,
    /// Absolute path of the file.
    pub path: PathBuf,
    /// The full file text (frontmatter included) — what the editor shows.
    pub raw: String,
    /// Whether the frontmatter records a builtin rule match at materialization
    /// time. Informational only: the edit surface uses it to explain where the
    /// starting text came from.
    pub from_builtin_rule: bool,
}

/// The model-prompts directory under `home`.
fn prompts_dir(home: &Path) -> PathBuf {
    home.join(MODEL_PROMPTS_DIR)
}

/// The absolute path a model's prompt file occupies.
fn prompt_path(home: &Path, raw_model_name: &str) -> PathBuf {
    prompts_dir(home).join(format!("{}.md", prompt_file_stem(raw_model_name)))
}

/// Every prompt file under `home`, keyed by the raw model name its frontmatter
/// declares.
///
/// Ownerless files (no frontmatter, or no `model:` field) are skipped with a
/// diagnostic: injecting them under a guessed name would hand one model
/// another model's prompt.
fn read_all(home: &Path, fs: &dyn SyncFs) -> BTreeMap<String, ResolvedModelPrompt> {
    let dir = prompts_dir(home);
    let mut map = BTreeMap::new();
    let Ok(entries) = fs.iterdir(&dir) else {
        return map;
    };
    let mut paths: Vec<PathBuf> = entries
        .into_iter()
        .filter(|entry| entry.is_file)
        .map(|entry| dir.join(entry.name))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    // Deterministic order: two files declaring the same owner resolve to the
    // lexicographically first name rather than whichever the OS listed first.
    paths.sort();
    for path in paths {
        let Ok(text) = fs.read_text(&path) else {
            tracing::warn!(
                path = %path.display(),
                "model prompt file unreadable; skipping it"
            );
            continue;
        };
        let Some((model, body)) = parse_prompt_file(&text) else {
            tracing::warn!(
                path = %path.display(),
                "model prompt file has no model field in its frontmatter; skipping it \
                 (its owner is unknown)"
            );
            continue;
        };
        if map.contains_key(&model) {
            tracing::warn!(
                path = %path.display(),
                model = %model,
                "duplicate model prompt file; keeping the lexicographically first one"
            );
            continue;
        }
        if !prompt_body_injectable(&body) {
            if body.trim().is_empty() {
                // A frontmatter-only file is the "explicitly no prompt" marker
                // (D-M13.2) — record it so materialization does not refill it.
                map.insert(
                    model,
                    ResolvedModelPrompt {
                        path,
                        body: String::new(),
                    },
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    bytes = body.len(),
                    limit = MAX_PROMPT_BODY_BYTES,
                    "model prompt file exceeds the size limit; it is not injected \
                     (not truncated — a partial directive would still be followed)"
                );
            }
            continue;
        }
        map.insert(model, ResolvedModelPrompt { path, body });
    }
    map
}

/// Materialize a prompt file for `raw_model_name` when a builtin rule covers
/// it and no file exists yet (D-M13.1/D-M13.2, architecture C9.4).
///
/// Returns the path written, or `None` when nothing was written — either no
/// rule matched, or a file already existed (the one-way-overwrite rule).
///
/// Failures are logged and swallowed: prompt materialization is an
/// enhancement and must never fail the config write that triggered it.
pub fn ensure_prompt_file(home: &Path, raw_model_name: &str) -> Option<PathBuf> {
    let body = builtin_prompt_for(raw_model_name)?;
    let path = prompt_path(home, raw_model_name);
    let fs = LocalSyncFs;
    if fs.exists(&path) {
        // The file belongs to the user now; never rewrite it.
        return None;
    }
    if let Err(error) = fs.mkdir(&prompts_dir(home), true) {
        tracing::warn!(
            dir = %prompts_dir(home).display(),
            error = %error,
            "could not create the model_prompts directory; skipping materialization"
        );
        return None;
    }
    let text = render_prompt_file(raw_model_name, body);
    match fs.write_text(&path, &text) {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not write the model prompt file; the model runs without it"
            );
            None
        }
    }
}

/// Seed the `raw model name → prompt body` table a session injects
/// (architecture C9.5).
///
/// Called once at session creation (and at restore, which recreates agents).
/// Existing files are read as-is; a model that a builtin rule covers but that
/// has no file yet is materialized first (the lazy backstop that serves
/// models configured before the rule existed). Returns an empty map when the
/// master switch is on.
pub fn seed_prompt_table(home: &Path, raw_model_names: &[String]) -> BTreeMap<String, String> {
    let fs = LocalSyncFs;
    // Lazy materialization: a model that predates its rule never goes through
    // a config write again, so without this it would never get a file.
    for name in raw_model_names {
        let path = prompt_path(home, name);
        if !fs.exists(&path) {
            ensure_prompt_file(home, name);
        }
    }
    read_all(home, &fs)
        .into_iter()
        .filter(|(_, prompt)| !prompt.body.is_empty())
        .map(|(model, prompt)| (model, prompt.body))
        .collect()
}

/// Read one model's prompt file for the edit surface.
///
/// `Ok(None)` means the model has no file yet (the empty state the UI offers
/// to create from). The builtin flag reports whether a rule currently covers
/// the model — it explains the empty state's starting text, and deliberately
/// does not claim the file came from that rule.
pub fn read_prompt_file(
    home: &Path,
    raw_model_name: &str,
) -> Result<Option<ModelPromptFile>, String> {
    let path = prompt_path(home, raw_model_name);
    let fs = LocalSyncFs;
    if !fs.exists(&path) {
        return Ok(None);
    }
    let raw = fs
        .read_text(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(Some(ModelPromptFile {
        model: raw_model_name.to_string(),
        path,
        raw,
        from_builtin_rule: builtin_prompt_for(raw_model_name).is_some(),
    }))
}

/// Write one model's prompt file from the edit surface (architecture C9.7).
///
/// The caller passes editable content (frontmatter excluded, or a complete
/// document — both are accepted: content that does not already declare an
/// owner is wrapped with one). The path is always derived from the model name
/// server-side, so the RPC cannot write outside `model_prompts/`.
pub fn write_prompt_file(
    home: &Path,
    raw_model_name: &str,
    content: &str,
) -> Result<PathBuf, String> {
    if raw_model_name.trim().is_empty() {
        return Err("a model name is required".to_string());
    }
    let path = prompt_path(home, raw_model_name);
    let dir = prompts_dir(home);
    // Invariant, not a trust boundary: the protocol accepts only a model name,
    // so the derived path is always inside `prompts_dir`. Asserting it keeps a
    // future signature change from silently opening a write outside.
    debug_assert_eq!(path.parent(), Some(dir.as_path()));
    let fs = LocalSyncFs;
    fs.mkdir(&dir, true)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let text = if parse_prompt_file(content).is_some() {
        content.to_string()
    } else {
        render_prompt_file(raw_model_name, content)
    };
    fs.write_text(&path, &text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Resolve the config snapshot into the two maps a session's prompt table
/// needs (D-M13.1/3, architecture C9.5).
///
/// Returns `(raw model names, model id → raw model name)`. **Every configured
/// alias contributes an id entry** — alias key and `provider/model` composite
/// both map to the alias's raw model — because an agent's `model_id` may be
/// either form (glossary M1). Only distinct raw names are returned as names,
/// so the materialization pass visits each family member once.
pub fn model_prompt_index(
    config: &mirri_core::ServerConfig,
) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut id_to_raw = std::collections::BTreeMap::new();
    let mut raw_names: Vec<String> = Vec::new();
    let Some(aliases) = config.models.as_ref() else {
        return (raw_names, id_to_raw);
    };
    for (alias_key, alias) in aliases {
        // The alias's `model` field is the raw model name; a missing field
        // falls back to the alias key itself (the same fallback
        // `ConfigAwareLlmFactory` uses when resolving an upstream model).
        let raw = if alias.model.trim().is_empty() {
            alias_key.clone()
        } else {
            alias.model.clone()
        };
        id_to_raw.insert(alias_key.clone(), raw.clone());
        id_to_raw.insert(format!("{}/{}", alias.provider, alias.model), raw.clone());
        if !raw_names.contains(&raw) {
            raw_names.push(raw);
        }
    }
    (raw_names, id_to_raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique scratch home per test — no process-wide path collisions.
    fn scratch_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mirri-model-prompts-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch home");
        dir
    }

    #[test]
    fn should_materialize_a_file_when_a_builtin_rule_covers_the_model() {
        // Given a home with no prompt files
        let home = scratch_home("materialize");
        // When a covered model is ensured
        let path = ensure_prompt_file(&home, "mimo-v2.6-pro").expect("file written");
        // Then the file exists, its stem is the clean name, and it declares
        // its owner in the frontmatter
        assert_eq!(path.file_name().unwrap(), "mimo-v2.6-pro.md");
        let text = std::fs::read_to_string(&path).unwrap();
        let (model, body) = parse_prompt_file(&text).expect("owner recorded");
        assert_eq!(model, "mimo-v2.6-pro");
        assert!(body.contains("strictly within what the task requires"));
    }

    #[test]
    fn should_write_nothing_when_no_rule_covers_the_model() {
        // Given a model the rule table makes no claim about
        let home = scratch_home("no-rule");
        // When ensured
        // Then no file appears — an empty shell would be noise in the home dir
        assert!(ensure_prompt_file(&home, "deepseek-v3").is_none());
        assert!(!prompts_dir(&home).exists());
    }

    #[test]
    fn should_never_rewrite_an_existing_file_when_materialization_runs_again() {
        // Given a model whose file the user has already tuned
        let home = scratch_home("no-overwrite");
        let path = ensure_prompt_file(&home, "mimo-v2.6-pro").expect("first write");
        std::fs::write(&path, "---\nmodel: \"mimo-v2.6-pro\"\n---\nMy own words.\n").unwrap();
        // When the same model is ensured again
        assert!(ensure_prompt_file(&home, "mimo-v2.6-pro").is_none());
        // Then the user's text survives byte for byte
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("My own words."));
        assert!(!text.contains("strictly within what the task requires"));
    }

    #[test]
    fn should_not_refill_a_cleared_file_because_clearing_is_an_explicit_choice() {
        // Given the user emptied the file (the "no prompt" marker)
        let home = scratch_home("cleared");
        let path = ensure_prompt_file(&home, "mimo-v2.6-pro").expect("first write");
        std::fs::write(&path, "---\nmodel: \"mimo-v2.6-pro\"\n---\n").unwrap();
        // When materialization runs again
        assert!(ensure_prompt_file(&home, "mimo-v2.6-pro").is_none());
        // Then the file stays empty
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("strictly within what the task requires"));
        // And the seeded table does not inject it either
        let table = seed_prompt_table(&home, &["mimo-v2.6-pro".to_string()]);
        assert!(table.is_empty());
    }

    #[test]
    fn should_materialize_on_seed_when_a_covered_model_predates_its_rule() {
        // Given a model already configured before the rule existed — it will
        // never go through another config write
        let home = scratch_home("lazy");
        // When a session seeds its table
        let table = seed_prompt_table(&home, &["mimo-v2.6-flash".to_string()]);
        // Then the backstop materialized the file and the body is injected
        assert!(prompt_path(&home, "mimo-v2.6-flash").exists());
        assert!(table.contains_key("mimo-v2.6-flash"));
    }

    #[test]
    fn should_share_one_file_when_two_providers_use_the_same_raw_model_name() {
        // Given one model registered under two providers (two aliases, one
        // raw name)
        let home = scratch_home("shared");
        // When both are ensured and one is edited
        let first = ensure_prompt_file(&home, "mimo-v2.6-pro").expect("write");
        let second = ensure_prompt_file(&home, "mimo-v2.6-pro");
        assert!(second.is_none());
        std::fs::write(&first, "---\nmodel: \"mimo-v2.6-pro\"\n---\nShared edit.\n").unwrap();
        // Then the single file carries the edit (US-17: sharing is by raw name)
        let table = seed_prompt_table(&home, &["mimo-v2.6-pro".to_string()]);
        assert_eq!(
            table.get("mimo-v2.6-pro").map(String::as_str),
            Some("Shared edit.\n")
        );
    }

    #[test]
    fn should_resolve_the_lexicographically_first_file_when_two_declare_the_same_owner() {
        // Given two files (from a hand-edit or a collision corpus) claiming
        // the same owner
        let home = scratch_home("duplicate");
        let dir = prompts_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("b.md"),
            "---\nmodel: \"mimo-v2.6-pro\"\n---\nFrom b.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("a.md"),
            "---\nmodel: \"mimo-v2.6-pro\"\n---\nFrom a.\n",
        )
        .unwrap();
        // When the table is seeded
        let table = seed_prompt_table(&home, &[]);
        // Then the choice is deterministic (a.md sorts first), not OS-order
        assert_eq!(
            table.get("mimo-v2.6-pro").map(String::as_str),
            Some("From a.\n")
        );
    }

    #[test]
    fn should_skip_a_file_without_an_owner_rather_than_guess_from_its_name() {
        // Given a hand-written file with no `model:` frontmatter
        let home = scratch_home("ownerless");
        let dir = prompts_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mimo-v2.6-pro.md"), "Just a body.\n").unwrap();
        // When the table is seeded
        let table = seed_prompt_table(&home, &[]);
        // Then nothing is injected — a guessed owner could hit the wrong model
        assert!(table.is_empty());
    }

    #[test]
    fn should_drop_an_oversized_body_whole_when_seeding() {
        // Given a file over the size limit
        let home = scratch_home("oversize");
        let dir = prompts_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let body = "x".repeat(MAX_PROMPT_BODY_BYTES + 1);
        std::fs::write(
            dir.join("big.md"),
            format!("---\nmodel: \"big-model\"\n---\n{body}\n"),
        )
        .unwrap();
        // When the table is seeded
        let table = seed_prompt_table(&home, &[]);
        // Then it is absent rather than truncated
        assert!(!table.contains_key("big-model"));
    }

    #[test]
    fn should_return_an_empty_table_when_the_directory_does_not_exist() {
        // Given a fresh home with no prompt files
        let home = scratch_home("empty-dir");
        // Then seeding is a no-op, not an error
        assert!(seed_prompt_table(&home, &[]).is_empty());
    }

    #[test]
    fn should_report_the_empty_state_when_the_edit_surface_reads_a_missing_file() {
        // Given a model with no file
        let home = scratch_home("read-missing");
        // Then the read answers "none" (the UI's create-from-empty state)
        assert!(read_prompt_file(&home, "mimo-v2.6-pro").unwrap().is_none());
    }

    #[test]
    fn should_wrap_bare_content_with_frontmatter_when_the_edit_surface_saves_it() {
        // Given the user typed only a body, with no frontmatter
        let home = scratch_home("write-bare");
        // When saved
        let path = write_prompt_file(&home, "mimo-v2.6-pro", "Be very brief.\n").unwrap();
        // Then the file records its owner and keeps the body
        let text = std::fs::read_to_string(&path).unwrap();
        let (model, body) = parse_prompt_file(&text).expect("owner recorded");
        assert_eq!(model, "mimo-v2.6-pro");
        assert_eq!(body, "Be very brief.\n");
        // And the edit surface reads it back as created-from-builtin
        let file = read_prompt_file(&home, "mimo-v2.6-pro").unwrap().unwrap();
        assert!(file.from_builtin_rule);
    }

    #[test]
    fn should_keep_a_complete_document_verbatim_when_the_edit_surface_saves_it() {
        // Given the user pasted a full document including frontmatter
        let home = scratch_home("write-full");
        let doc = "---\nmodel: \"mimo-v2.6-pro\"\n---\nFull doc.\n";
        // When saved
        let path = write_prompt_file(&home, "mimo-v2.6-pro", doc).unwrap();
        // Then it lands byte for byte (no second frontmatter prepended)
        assert_eq!(std::fs::read_to_string(&path).unwrap(), doc);
    }

    #[test]
    fn should_derive_the_path_from_the_model_name_only_so_the_rpc_cannot_escape() {
        // Given a model name containing path separators and traversal
        let home = scratch_home("traversal");
        // When saved
        let path = write_prompt_file(&home, "../../etc/passwd", "Body.\n").unwrap();
        // Then the file still lands inside model_prompts/ (the sanitizer folds
        // the separators and the fingerprint keeps it distinct)
        assert_eq!(path.parent(), Some(prompts_dir(&home).as_path()));
    }
}
