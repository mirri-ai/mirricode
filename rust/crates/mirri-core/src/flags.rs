//! Feature flags — env-driven experimental feature toggles.
//!
//! [`FlagRegistry`] holds flag definitions (id, default, description) and
//! resolves each flag's effective state at call time from the environment:
//!
//! 1. a per-flag variable `MIRRICODE_EXPERIMENTAL_<NAME>` (where `<NAME>` is
//!    the flag id uppercased with every non-alphanumeric character replaced
//!    by `_`, e.g. `graph.realtime` → `GRAPH_REALTIME`);
//! 2. a global variable [`FLAG_GLOBAL_ENV`] (`MIRRICODE_EXPERIMENTAL_FLAG`)
//!    that lists flag ids to enable, or the special value `all` (any truthy
//!    value such as `1` or `true` also enables every flag);
//! 3. the registered default.
//!
//! Recognition is case-insensitive; `1`, `true`, `yes`, `on` enable, `0`,
//! `false`, `no`, `off` disable, and unrecognized values fall through to the
//! next resolution step.

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// The global environment variable listing flag ids to enable, or `all` (or
/// any truthy value) to enable every registered flag.
pub const FLAG_GLOBAL_ENV: &str = "MIRRICODE_EXPERIMENTAL_FLAG";

/// Prefix of the per-flag environment variable.
const FLAG_ENV_PREFIX: &str = "MIRRICODE_EXPERIMENTAL_";

/// The surface a flag applies to (the klient `FlagSurface` enum).
///
/// The rust registry has no per-surface scoping today — every flag a
/// backend registers is a core-engine flag — so the EMB registration face
/// degrades to `Core` unless a definition states otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FlagSurface {
    #[serde(rename = "core")]
    #[default]
    Core,
    #[serde(rename = "tui")]
    Tui,
    #[serde(rename = "both")]
    Both,
}

/// Where a flag's effective state came from (the klient
/// `ExperimentalFlagSource` enum).
///
/// `Config` reports the config `[experimental]` layer, populated through
/// [`FlagRegistry::set_config_overrides`] by the composition root that owns
/// the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlagSource {
    MasterEnv,
    Env,
    Config,
    Default,
}

/// A feature flag definition.
///
/// The three klient-rich fields (`title`, `surface`, `env_name`) are
/// serde-optional so every existing construction site and serialized form
/// keeps parsing; the wire face degrades honestly when they are unset
/// (title falls back to the id, surface to `core`, the env name to the
/// derived per-flag variable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagDef {
    /// Canonical flag id (e.g. `graph.realtime`).
    pub id: String,
    /// Effective state when no environment variable overrides it.
    pub default: bool,
    /// What the flag controls, for diagnostics and snapshots.
    pub description: String,
    /// Human-readable title (klient `ExperimentalFeatureState.title`).
    /// Falls back to the id when unset.
    #[serde(default)]
    pub title: Option<String>,
    /// The surface the flag applies to. Falls back to `core` (the rust
    /// registry only registers engine flags).
    #[serde(default)]
    pub surface: FlagSurface,
    /// The per-flag environment variable name. Falls back to the derived
    /// name (`MIRRICODE_EXPERIMENTAL_<NAME>`) when unset or empty.
    #[serde(default)]
    pub env_name: Option<String>,
}

impl FlagDef {
    /// The per-flag environment variable this definition reads: the
    /// explicit `env_name` when set, else the derived
    /// `MIRRICODE_EXPERIMENTAL_<NAME>` name.
    pub fn env_var(&self) -> String {
        match self.env_name.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => flag_env_name(&self.id),
        }
    }

    /// The wire `title`: the explicit title when set, else the id (the
    /// honest degradation — no fabricated marketing names).
    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
}

/// A flag's resolved wire state — the klient
/// `experimentalFeatureStateSchema` shape (the rust-side fact object the
/// `flag.explain*` handlers serialize).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlagState {
    pub id: String,
    pub title: String,
    pub description: String,
    pub surface: FlagSurface,
    pub env: String,
    pub default_enabled: bool,
    pub enabled: bool,
    pub source: FlagSource,
    /// The config `[experimental]` layer's value for this flag, when the
    /// composition root installed one (the klient schema marks the field
    /// optional).
    pub config_value: Option<bool>,
}

/// Flag registry — resolves experimental flags from the environment, the
/// config `[experimental]` section, and defaults (TS parity: the TS
/// `FlagService` chain).
///
/// All operations are synchronous; the registry is cheaply shareable behind
/// an `Arc` and is intended to live as a workspace- or app-scoped service.
pub struct FlagRegistry {
    flags: RwLock<HashMap<String, FlagDef>>,
    /// The config `[experimental]` section's resolved values — the
    /// config-layer overrides sitting between the env chain and the
    /// registered defaults (set by the composition root; it owns the
    /// config).
    config_overrides: RwLock<HashMap<String, bool>>,
}

impl Default for FlagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FlagRegistry {
    /// Create an empty flag registry.
    pub fn new() -> Self {
        Self {
            flags: RwLock::new(HashMap::new()),
            config_overrides: RwLock::new(HashMap::new()),
        }
    }

    /// Replace the config-layer overrides (the `[experimental]` section's
    /// resolved values).
    ///
    /// The composition root calls this when the config loads or changes
    /// (mirroring the TS `FlagService` config watcher). An empty map clears
    /// the layer — the env chain then falls through to the registered
    /// defaults.
    pub fn set_config_overrides(&self, overrides: HashMap<String, bool>) {
        tracing::debug!(count = overrides.len(), "flag config overrides replaced");
        *self
            .config_overrides
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = overrides;
    }

    /// Register a flag definition.
    ///
    /// Registering an id that is already registered replaces its definition.
    pub fn register(&self, def: FlagDef) {
        tracing::debug!(
            flag = %def.id,
            default = %def.default,
            "feature flag registered"
        );
        self.flags
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(def.id.clone(), def);
    }

    /// Check if a flag is enabled.
    ///
    /// Resolution order:
    /// 1. `MIRRICODE_EXPERIMENTAL_<NAME>` (per-flag override; truthy values
    ///    enable, falsy values disable);
    /// 2. [`FLAG_GLOBAL_ENV`] (the flag id listed, or the value `all` /
    ///    any truthy value enabling every flag);
    /// 3. the config `[experimental]` override (see
    ///    [`Self::set_config_overrides`]);
    /// 4. the registered default;
    /// 5. `false` when the flag is not registered at all.
    ///
    /// Unregistered flags still resolve through steps 1-3, so a config or env
    /// override can enable a flag the composition root never registered
    /// (registry-less call sites use [`env_flag_enabled`], which shares
    /// steps 1-2 only).
    pub fn enabled(&self, id: &str) -> bool {
        // 1-2. Environment override chain (per-flag, then global).
        if let Some(enabled) = env_override(id) {
            return enabled;
        }
        // 3. Config-layer override.
        if let Some(enabled) = self
            .config_overrides
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
        {
            return *enabled;
        }
        // 4. Registered default; unregistered flags are off.
        self.flags
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .is_some_and(|def| def.default)
    }

    /// List all registered flags and their effective states.
    ///
    /// The snapshot is sorted by flag id for deterministic ordering. Note
    /// that environment overrides apply, so the reported state may differ
    /// from a flag's registered default.
    pub fn snapshot(&self) -> Vec<(String, bool)> {
        let ids: Vec<String> = self
            .flags
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        // Resolve outside the read lock: `enabled` reads the same lock, and
        // `std::sync::RwLock` is not guaranteed reentrant.
        let mut snapshot: Vec<(String, bool)> = ids
            .into_iter()
            .map(|id| (id.clone(), self.enabled(&id)))
            .collect();
        snapshot.sort_by(|a, b| a.0.cmp(&b.0));
        snapshot
    }

    /// The registered definitions, sorted by id for deterministic wire
    /// ordering (the `flag.snapshot` / `flag.explainAll` basis).
    pub fn defs(&self) -> Vec<FlagDef> {
        let mut defs: Vec<FlagDef> = self
            .flags
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    /// Explain one registered flag: its resolved wire state including the
    /// override source. `None` when the flag is not registered (the klient
    /// `maybe` semantics — an unknown id is a miss, not an error, and the
    /// env chain never promotes an unregistered id).
    pub fn explain(&self, id: &str) -> Option<FlagState> {
        let def = self
            .flags
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)?
            .clone();
        Some(self.resolve_state(&def))
    }

    /// Explain every registered flag, sorted by id (the
    /// `flag.explainAll` wire basis). Unregistered ids never appear.
    pub fn explain_all(&self) -> Vec<FlagState> {
        self.defs()
            .iter()
            .map(|def| self.resolve_state(def))
            .collect()
    }

    /// Resolve one definition's wire state, attributing which override
    /// chain step decided the effective value:
    ///
    /// 1. the per-flag variable (`MIRRICODE_EXPERIMENTAL_<NAME>` or the
    ///    definition's explicit `env_name`) — source `env`;
    /// 2. the global [`FLAG_GLOBAL_ENV`] — source `master-env`;
    /// 3. the registered default — source `default`.
    ///
    /// `config` is unreachable (see [`FlagSource`]); `config_value` is
    /// always `None`.
    fn resolve_state(&self, def: &FlagDef) -> FlagState {
        let env_var = def.env_var();
        let config_value = self
            .config_overrides
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&def.id)
            .copied();
        let (enabled, source) = if let Ok(value) = std::env::var(&env_var) {
            match parse_truth(value.trim()) {
                Some(enabled) => (enabled, FlagSource::Env),
                // An unrecognized per-flag value falls through to the next
                // resolution step, exactly like `enabled`.
                None => self.resolve_global_or_default(def, config_value),
            }
        } else {
            self.resolve_global_or_default(def, config_value)
        };
        FlagState {
            id: def.id.clone(),
            title: def.display_title().to_string(),
            description: def.description.clone(),
            surface: def.surface,
            env: env_var,
            default_enabled: def.default,
            enabled,
            source,
            config_value,
        }
    }

    /// Steps 2–4 of the resolution chain: the global env override, then the
    /// config `[experimental]` override, then the registered default.
    fn resolve_global_or_default(
        &self,
        def: &FlagDef,
        config_value: Option<bool>,
    ) -> (bool, FlagSource) {
        if let Ok(value) = std::env::var(FLAG_GLOBAL_ENV) {
            let trimmed = value.trim();
            let listed: Vec<&str> = trimmed
                .split([',', ' ', ';'])
                .filter(|item| !item.is_empty())
                .collect();
            if listed.iter().any(|item| item.eq_ignore_ascii_case("all"))
                || parse_truth(trimmed) == Some(true)
                || listed.iter().any(|item| *item == def.id)
            {
                return (true, FlagSource::MasterEnv);
            }
        }
        if let Some(enabled) = config_value {
            return (enabled, FlagSource::Config);
        }
        (def.default, FlagSource::Default)
    }
}

/// The per-flag environment variable name for a flag id:
/// `MIRRICODE_EXPERIMENTAL_` plus the id uppercased with non-alphanumeric
/// characters replaced by underscores.
fn flag_env_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{FLAG_ENV_PREFIX}{name}")
}

/// The PreToolUse `updatedInput` rewriting gate — the TS-parity
/// `hook-command-rewrite` flag (env `MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE`).
///
/// Rewriting lets an external hook transparently replace the arguments a tool
/// runs with, so it stays opt-in: OFF by default on both engines, enabled
/// through the env chain only (the TS flag registry default is `false`).
pub const HOOK_COMMAND_REWRITE_FLAG: &str = "hook-command-rewrite";

/// The session-consistency foreground follow gate — the
/// `session-consistency-follow` flag (env
/// `MIRRICODE_EXPERIMENTAL_SESSION_CONSISTENCY_FOLLOW`).
///
/// When enabled, the embedded server spawns a per-session follow driver on
/// resume that incrementally absorbs foreign appends to the session's main
/// journal (spec §5, US C1.2/C4.2). OFF by default while the batch is
/// experimental; enabled through the env/config/registry chain like any
/// engine flag.
pub const SESSION_CONSISTENCY_FOLLOW_FLAG: &str = "session-consistency-follow";

/// Resolve a flag from the environment override chain only — the per-flag
/// variable first, then the global master switch
/// ([`FLAG_GLOBAL_ENV`](static@FLAG_GLOBAL_ENV)).
///
/// `Some(false)` means the chain decided OFF (e.g. the per-flag variable is
/// explicitly `0`, which beats the global switch); `None` means the
/// environment did not decide at all. This is steps 1–2 of
/// [`FlagRegistry::enabled`], extracted so registry-less call sites can share
/// the exact same chain.
fn env_override(id: &str) -> Option<bool> {
    // 1. Per-flag override.
    if let Ok(value) = std::env::var(flag_env_name(id))
        && let Some(enabled) = parse_truth(value.trim())
    {
        return Some(enabled);
    }
    // 2. Global list / master switch.
    if let Ok(value) = std::env::var(FLAG_GLOBAL_ENV) {
        let trimmed = value.trim();
        let listed: Vec<&str> = trimmed
            .split([',', ' ', ';'])
            .filter(|item| !item.is_empty())
            .collect();
        if listed.iter().any(|item| item.eq_ignore_ascii_case("all"))
            || parse_truth(trimmed) == Some(true)
            || listed.contains(&id)
        {
            return Some(true);
        }
    }
    None
}

/// Gate a behavior on a flag resolved from the environment chain only.
///
/// For use at call sites with no [`FlagRegistry`] wired (engines assembled
/// standalone, subagent flows): the chain matches the registry's steps 1–2
/// and an undecided environment means OFF — the honest default for
/// experimental features. When a registry IS wired it resolves the same
/// (env first, then the registered default), so the two paths agree unless
/// a registered default is ever flipped to `true` — at which point the
/// registry path must be used instead.
pub fn env_flag_enabled(id: &str) -> bool {
    env_override(id).unwrap_or(false)
}

/// Parse an environment variable value as a boolean.
///
/// Returns `None` for unrecognized values so callers can fall through to the
/// next resolution step.
fn parse_truth(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process environment variables.
    ///
    /// `std::env::set_var`/`remove_var` are `unsafe` in edition 2024, and
    /// environment reads/writes race across threads when tests run in
    /// parallel, so every env-mutating test holds this lock and restores the
    /// previous values on drop.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Saves the current value of each touched variable and restores it on
    /// drop, so tests never leak env state into each other.
    struct EnvGuard {
        saved: HashMap<String, Option<String>>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                saved: HashMap::new(),
            }
        }

        fn set(&mut self, name: &str, value: Option<&str>) {
            self.saved
                .entry(name.to_string())
                .or_insert_with(|| std::env::var(name).ok());
            match value {
                // SAFETY: guarded by ENV_LOCK and restored in Drop; no other
                // thread reads or writes these variables concurrently.
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                // SAFETY: same discipline as `set`.
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    fn flag(id: &str, default: bool) -> FlagDef {
        FlagDef {
            id: id.to_string(),
            default,
            description: format!("flag {id}"),
            title: None,
            surface: FlagSurface::Core,
            env_name: None,
        }
    }

    fn registry_with(flags: &[(&str, bool)]) -> FlagRegistry {
        let registry = FlagRegistry::new();
        for (id, default) in flags {
            registry.register(flag(id, *default));
        }
        registry
    }

    #[test]
    fn should_use_registered_default_when_enabled_without_env_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_GRAPH_ON", None);
        env.set("MIRRICODE_EXPERIMENTAL_GRAPH_OFF", None);
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("graph.on", true), ("graph.off", false)]);

        assert!(registry.enabled("graph.on"));
        assert!(!registry.enabled("graph.off"));
    }

    #[test]
    fn should_return_false_when_enabled_with_unregistered_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = FlagRegistry::new();

        assert!(!registry.enabled("never-registered"));
    }

    #[test]
    fn should_override_false_default_when_enabled_with_true_per_flag_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_GRAPH_REALTIME", Some("1"));
        env.set(FLAG_GLOBAL_ENV, None);
        // Also verifies the id→env-name transform: `graph.realtime` reads
        // `MIRRICODE_EXPERIMENTAL_GRAPH_REALTIME`.
        let registry = registry_with(&[("graph.realtime", false)]);

        assert!(registry.enabled("graph.realtime"));
    }

    #[test]
    fn should_override_true_default_when_enabled_with_false_per_flag_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_BOLD_LOG", Some("false"));
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("bold.log", true)]);

        assert!(!registry.enabled("bold.log"));
    }

    #[test]
    fn should_enable_truthy_forms_and_disable_falsy_forms_when_enabled_with_per_flag_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_A", Some("true"));
        env.set("MIRRICODE_EXPERIMENTAL_B", Some("yes"));
        env.set("MIRRICODE_EXPERIMENTAL_C", Some("ON"));
        env.set("MIRRICODE_EXPERIMENTAL_D", Some("0"));
        env.set("MIRRICODE_EXPERIMENTAL_E", Some("off"));
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", true),
            ("e", true),
        ]);

        assert!(registry.enabled("a"));
        assert!(registry.enabled("b"));
        assert!(registry.enabled("c"));
        assert!(!registry.enabled("d"));
        assert!(!registry.enabled("e"));
    }

    #[test]
    fn should_fall_back_to_default_when_enabled_with_unrecognized_per_flag_env_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_QUIRKY", Some("banana"));
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("quirky", true)]);

        assert!(registry.enabled("quirky"));
    }

    #[test]
    fn should_return_false_when_env_flag_enabled_reads_unset_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", None);
        env.set(FLAG_GLOBAL_ENV, None);

        // The registry-less gate: an undecided environment means OFF.
        assert!(!env_flag_enabled(HOOK_COMMAND_REWRITE_FLAG));
    }

    #[test]
    fn should_return_true_when_env_flag_enabled_reads_truthy_per_flag_variable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", Some("1"));
        env.set(FLAG_GLOBAL_ENV, None);

        // The per-flag variable derives from the id's dashes: verify the
        // constant resolves through the same chain the registry does.
        assert!(env_flag_enabled(HOOK_COMMAND_REWRITE_FLAG));
        assert!(FlagRegistry::new().enabled(HOOK_COMMAND_REWRITE_FLAG));
    }

    #[test]
    fn should_return_false_when_env_flag_enabled_reads_explicitly_false_variable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", Some("0"));
        env.set(FLAG_GLOBAL_ENV, Some("all"));

        // The explicit per-flag false beats the global master switch —
        // the same precedence `FlagRegistry::enabled` applies.
        assert!(!env_flag_enabled(HOOK_COMMAND_REWRITE_FLAG));
    }

    #[test]
    fn should_enable_flag_when_config_override_set_and_env_undecided() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", None);
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("hook-command-rewrite", false)]);
        let mut overrides = HashMap::new();
        overrides.insert("hook-command-rewrite".to_string(), true);
        registry.set_config_overrides(overrides);

        // The config layer sits between the env chain and the defaults
        // (TS parity: the `[experimental]` config section).
        assert!(registry.enabled("hook-command-rewrite"));
    }

    #[test]
    fn should_fall_back_to_default_when_config_override_absent_for_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("hook-command-rewrite", false), ("other", true)]);
        let mut overrides = HashMap::new();
        overrides.insert("hook-command-rewrite".to_string(), true);
        registry.set_config_overrides(overrides);

        // Only the overridden flag changes; the untouched one keeps its
        // registered default.
        assert!(registry.enabled("hook-command-rewrite"));
        assert!(registry.enabled("other"));
    }

    #[test]
    fn should_beat_config_override_when_per_flag_env_decides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", Some("0"));
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("hook-command-rewrite", true)]);
        let mut overrides = HashMap::new();
        overrides.insert("hook-command-rewrite".to_string(), true);
        registry.set_config_overrides(overrides);

        // The env chain outranks the config layer.
        assert!(!registry.enabled("hook-command-rewrite"));
    }

    #[test]
    fn should_report_config_source_and_value_when_explaining_config_layer() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE", None);
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("hook-command-rewrite", false)]);
        let mut overrides = HashMap::new();
        overrides.insert("hook-command-rewrite".to_string(), true);
        registry.set_config_overrides(overrides);

        let state = registry
            .explain("hook-command-rewrite")
            .expect("registered");
        assert!(state.enabled);
        assert_eq!(state.source, FlagSource::Config);
        assert_eq!(state.config_value, Some(true));
    }

    #[test]
    fn should_clear_config_layer_when_overrides_replaced_with_empty_map() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("hook-command-rewrite", false)]);
        let mut overrides = HashMap::new();
        overrides.insert("hook-command-rewrite".to_string(), true);
        registry.set_config_overrides(overrides);
        registry.set_config_overrides(HashMap::new());

        // An empty replacement clears the layer — the default applies again.
        assert!(!registry.enabled("hook-command-rewrite"));
    }

    #[test]
    fn should_enable_listed_flags_when_global_env_lists_flag_ids() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_FOO", None);
        env.set("MIRRICODE_EXPERIMENTAL_BAR", None);
        env.set("MIRRICODE_EXPERIMENTAL_BAZ", None);
        env.set(FLAG_GLOBAL_ENV, Some("foo, bar;baz"));
        let registry = registry_with(&[("foo", false), ("bar", false), ("baz", false)]);

        assert!(registry.enabled("foo"));
        assert!(registry.enabled("bar"));
        assert!(registry.enabled("baz"));
        let extra = registry_with(&[("unlisted", false)]);
        assert!(!extra.enabled("unlisted"));
    }

    #[test]
    fn should_enable_every_registered_flag_when_global_env_is_all() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_ALPHA", None);
        env.set("MIRRICODE_EXPERIMENTAL_BETA", None);
        env.set(FLAG_GLOBAL_ENV, Some("all"));
        let registry = registry_with(&[("alpha", false), ("beta", false)]);

        assert!(registry.enabled("alpha"));
        assert!(registry.enabled("beta"));
    }

    #[test]
    fn should_enable_every_registered_flag_when_global_env_is_truthy() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_GAMMA", None);
        env.set(FLAG_GLOBAL_ENV, Some("true"));
        let registry = registry_with(&[("gamma", false)]);

        assert!(registry.enabled("gamma"));
    }

    #[test]
    fn should_report_effective_state_sorted_by_id_when_snapshotting_registered_flags() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_A", None);
        env.set("MIRRICODE_EXPERIMENTAL_B", Some("1")); // overrides default
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("b", false), ("a", true)]);

        let snapshot = registry.snapshot();

        assert_eq!(
            snapshot,
            vec![("a".to_string(), true), ("b".to_string(), true)]
        );
    }

    #[test]
    fn should_preserve_fields_when_serde_roundtripping_flag_def() {
        let def = flag("graph.realtime", true);

        let json = serde_json::to_string(&def).unwrap();
        let decoded: FlagDef = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, def);
    }

    #[test]
    fn should_parse_a_legacy_three_field_flag_def_when_decoding_serde() {
        // The klient-rich fields are serde-optional: a serialized legacy
        // definition (or hand-written TOML/JSON) keeps parsing with the
        // degraded defaults.
        let decoded: FlagDef =
            serde_json::from_str(r#"{"id":"a","default":true,"description":"d"}"#).unwrap();

        assert_eq!(decoded.title, None);
        assert_eq!(decoded.surface, FlagSurface::Core);
        assert_eq!(decoded.env_name, None);
        assert_eq!(decoded.env_var(), "MIRRICODE_EXPERIMENTAL_A");
    }

    #[test]
    fn should_return_none_when_explaining_an_unregistered_flag() {
        let registry = registry_with(&[("known", true)]);

        assert!(registry.explain("known").is_some());
        assert!(registry.explain("never-registered").is_none());
    }

    #[test]
    fn should_report_the_default_source_when_explaining_a_flag_without_env_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_KNOWN", None);
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("known", false)]);

        let state = registry.explain("known").unwrap();

        assert!(!state.enabled);
        assert_eq!(state.source, FlagSource::Default);
        assert_eq!(state.env, "MIRRICODE_EXPERIMENTAL_KNOWN");
        assert_eq!(state.title, "known"); // title degrades to the id
        assert_eq!(state.config_value, None);
    }

    #[test]
    fn should_report_the_env_source_when_explaining_a_flag_with_a_per_flag_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_KNOWN", Some("1"));
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("known", false)]);

        let state = registry.explain("known").unwrap();

        assert!(state.enabled);
        assert_eq!(state.source, FlagSource::Env);
    }

    #[test]
    fn should_report_the_master_env_source_when_explaining_a_flag_enabled_globally() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("MIRRICODE_EXPERIMENTAL_KNOWN", None);
        env.set(FLAG_GLOBAL_ENV, Some("all"));
        let registry = registry_with(&[("known", false)]);

        let state = registry.explain("known").unwrap();

        assert!(state.enabled);
        assert_eq!(state.source, FlagSource::MasterEnv);
    }

    #[test]
    fn should_explain_every_registered_flag_sorted_by_id() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set(FLAG_GLOBAL_ENV, None);
        let registry = registry_with(&[("b", false), ("a", true)]);

        let states = registry.explain_all();

        assert_eq!(
            states.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(states.iter().all(|s| s.config_value.is_none()));
    }
}
