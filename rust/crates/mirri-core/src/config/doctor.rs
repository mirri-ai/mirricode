//! Config doctor: validate a config.toml text against the rust engine's
//! own parsing behavior — syntax, structure, unknown keys (hard errors),
//! advisory diagnostics and unknown experimental flags (warnings).
//!
//! Pure and side-effect free: takes text, returns a [`CheckReport`]. No file
//! IO, no environment reads, no `${VAR}` expansion — the validation verdict
//! matches what the engine would do with the file in a clean environment.

use super::ServerConfig;
use super::wire::{
    KNOWN_CONFIG_TOP_LEVEL_KEYS, KNOWN_HOOK_ENTRY_KEYS, KNOWN_MODEL_ENTRY_ALIAS_KEYS,
    KNOWN_MODEL_ENTRY_KEYS, KNOWN_PERMISSION_RULE_KEYS, KNOWN_PROVIDER_ENTRY_ALIAS_KEYS,
    KNOWN_PROVIDER_ENTRY_KEYS, KNOWN_SECTION_KEYS,
};

/// Outcome of a single check. The doctor CLI maps `Error` to a non-zero exit
/// code; `Warn` never changes it. `Ok`/`Skip` are produced by the CLI layer
/// (healthy file / missing default file) — the core only emits `Error`/`Warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Skip,
    Error,
    Warn,
}

/// One located problem: a TOML path (`hooks` → `[0]` → `pattern`) plus a
/// human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckIssue {
    pub status: CheckStatus,
    pub path: Vec<String>,
    pub message: String,
}

/// The full verdict for one config.toml text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub file_path: String,
    pub issues: Vec<CheckIssue>,
    /// Whether TOML syntax and structural parsing both succeeded. `false`
    /// implies `issues` contains at least one `Error`.
    pub parse_ok: bool,
}

impl CheckReport {
    /// Whether any hard error was reported (drives the CLI exit code).
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.status == CheckStatus::Error)
    }
}

/// Validate a config.toml text the way the engine would load it (minus
/// environment overrides): TOML syntax, then `ServerConfig::from_toml`
/// structural validation, then unknown-key detection against the known-key
/// inventories, then the advisory diagnostics and the experimental-flag
/// registration check.
///
/// `known_flags` lists the flag ids the consuming engine registers (e.g.
/// `hook-command-rewrite`); experimental keys outside it warn.
pub fn validate_config_str(text: &str, file_path: &str, known_flags: &[String]) -> CheckReport {
    let mut issues: Vec<CheckIssue> = Vec::new();

    // Stage 1: TOML syntax.
    let raw: toml::Value = match toml::from_str(text) {
        Ok(value) => value,
        Err(err) => {
            issues.push(CheckIssue {
                status: CheckStatus::Error,
                path: Vec::new(),
                message: format!("invalid config TOML in {file_path}: {err}"),
            });
            return CheckReport {
                file_path: file_path.to_string(),
                issues,
                parse_ok: false,
            };
        }
    };

    // Stage 2: structural load (serde + hard validations, e.g. mutually
    // exclusive loop_control keys). Empty text parses as default config.
    //
    // A structural failure does NOT stop the scan: unknown keys are still
    // detected below (Stage 3) so a misspelled key that broke a required
    // field (e.g. `patrn` for `pattern` in a permission rule) is named
    // explicitly, not hidden behind the generic load error.
    //
    // The parsed config is kept and fed to Stage 4: both stages run
    // `ServerConfig::from_toml`, so a structurally valid text is parsed
    // only once.
    let mut structural_failed = false;
    let mut parsed: Option<ServerConfig> = None;
    if !text.trim().is_empty() {
        match ServerConfig::from_toml(text) {
            Ok(config) => parsed = Some(config),
            Err(err) => {
                issues.push(CheckIssue {
                    status: CheckStatus::Error,
                    path: Vec::new(),
                    message: format!("invalid configuration in {file_path}: {err}"),
                });
                structural_failed = true;
            }
        }
    }

    // Stage 3: unknown-key detection over the raw document.
    if let Some(table) = raw.as_table() {
        scan_top_level(table, &mut issues);
    }

    // Stage 4: advisory diagnostics (dangling provider/model references,
    // legacy sections, deprecated keys) — warnings only. Uses the config
    // parsed in Stage 2; a structurally failed text simply produces no
    // diagnostics (the raw-key scan above still names its mistakes).
    if let Some(config) = parsed {
        for warning in config.diagnostics_warnings() {
            issues.push(CheckIssue {
                status: CheckStatus::Warn,
                path: Vec::new(),
                message: warning,
            });
        }
    }

    // Stage 5: experimental flags not registered in the consuming engine.
    if let Some(experimental) = raw.get("experimental").and_then(|v| v.as_table()) {
        for name in experimental.keys() {
            if !known_flags.iter().any(|flag| flag == name) {
                issues.push(CheckIssue {
                    status: CheckStatus::Warn,
                    path: vec!["experimental".to_string(), name.clone()],
                    message: format!(
                        "experimental flag \"{name}\" is not registered in this engine and \
                         has no effect"
                    ),
                });
            }
        }
    }

    CheckReport {
        file_path: file_path.to_string(),
        issues,
        parse_ok: !structural_failed,
    }
}

/// Scan the top-level table for unknown keys, then descend into each known
/// section whose internal shape is checkable.
fn scan_top_level(table: &toml::map::Map<String, toml::Value>, issues: &mut Vec<CheckIssue>) {
    for (key, value) in table {
        if !KNOWN_CONFIG_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            issues.push(CheckIssue {
                status: CheckStatus::Error,
                path: vec![key.clone()],
                message: format!(
                    "unknown key \"{key}\" at top level — not recognized by the engine"
                ),
            });
            continue;
        }
        match key.as_str() {
            "hooks" => scan_array_entries(value, "hooks", KNOWN_HOOK_ENTRY_KEYS, issues),
            "providers" => scan_map_entries(
                value,
                "providers",
                KNOWN_PROVIDER_ENTRY_KEYS,
                KNOWN_PROVIDER_ENTRY_ALIAS_KEYS,
                issues,
            ),
            "models" => scan_map_entries(
                value,
                "models",
                KNOWN_MODEL_ENTRY_KEYS,
                KNOWN_MODEL_ENTRY_ALIAS_KEYS,
                issues,
            ),
            "permission" => scan_permission(value, issues),
            "image" | "background" | "services" => {
                // Free passthrough sections (toml::Value) — no inner key
                // check. `services` inner shape is only validated by the
                // engine's own section reader at load time; the doctor has
                // no inventory for it (its section names are not in
                // KNOWN_SECTION_KEYS either, since this arm intercepts them
                // before the table lookup below).
            }
            _ => {
                if let Some((_, known)) = KNOWN_SECTION_KEYS
                    .iter()
                    .find(|(section, _)| section == key)
                {
                    scan_struct_section(value, key, known, issues);
                }
            }
        }
    }
}

/// Scan an array-of-tables (hooks) entry by entry.
fn scan_array_entries(
    value: &toml::Value,
    section: &str,
    known: &[&str],
    issues: &mut Vec<CheckIssue>,
) {
    let Some(entries) = value.as_array() else {
        return;
    };
    for (index, entry) in entries.iter().enumerate() {
        let Some(table) = entry.as_table() else {
            continue;
        };
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                issues.push(CheckIssue {
                    status: CheckStatus::Error,
                    path: vec![section.to_string(), format!("[{index}]"), key.clone()],
                    message: format!(
                        "unknown key \"{key}\" in {section}[{index}] — not recognized by the \
                         engine"
                    ),
                });
            }
        }
    }
}

/// Scan a map-of-tables (providers / models) entry by entry.
///
/// `known_aliases` lists the camelCase serde aliases the entry struct also
/// accepts (see the KNOWN_*_ENTRY_ALIAS_KEYS supplements in `wire.rs`); the
/// round-trip merge path consumes only the base lists, so alias-spelled
/// keys are doctor-recognized here without changing write-back behavior.
fn scan_map_entries(
    value: &toml::Value,
    section: &str,
    known: &[&str],
    known_aliases: &[&str],
    issues: &mut Vec<CheckIssue>,
) {
    let Some(entries) = value.as_table() else {
        return;
    };
    for (name, entry) in entries {
        let Some(table) = entry.as_table() else {
            continue;
        };
        for key in table.keys() {
            if !known.contains(&key.as_str()) && !known_aliases.contains(&key.as_str()) {
                issues.push(CheckIssue {
                    status: CheckStatus::Error,
                    path: vec![section.to_string(), name.clone(), key.clone()],
                    message: format!(
                        "unknown key \"{key}\" in {section}.{name} — not recognized by the \
                         engine"
                    ),
                });
            }
        }
    }
}

/// Scan the permission section: its own keys, plus each rule entry.
fn scan_permission(value: &toml::Value, issues: &mut Vec<CheckIssue>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, inner) in table {
        if key == "rules" {
            scan_array_entries(
                inner,
                "permission.rules",
                KNOWN_PERMISSION_RULE_KEYS,
                issues,
            );
        } else if !["mode"].contains(&key.as_str()) {
            issues.push(CheckIssue {
                status: CheckStatus::Error,
                path: vec!["permission".to_string(), key.clone()],
                message: format!(
                    "unknown key \"{key}\" in permission — not recognized by the engine"
                ),
            });
        }
    }
}

/// Scan a flat struct-shaped section (thinking / loop_control / task / …)
/// against its known-key list.
fn scan_struct_section(
    value: &toml::Value,
    section: &str,
    known: &[&str],
    issues: &mut Vec<CheckIssue>,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            issues.push(CheckIssue {
                status: CheckStatus::Error,
                path: vec![section.to_string(), key.clone()],
                message: format!(
                    "unknown key \"{key}\" in {section} — not recognized by the engine"
                ),
            });
        }
    }
}
