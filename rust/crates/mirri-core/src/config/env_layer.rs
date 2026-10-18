//! Environment layer for `config.toml`: inline `${VAR}` expansion at load
//! time and the whole-field `MIRRI_*` overrides (including the
//! environment-synthesized model), applied after TOML parsing.

use super::*;

impl ServerConfig {
    /// Expand `${VAR}` / `${env:VAR}` references in every string value of
    /// the config using the process environment.
    ///
    /// Recursively walks all string fields, matching the TypeScript
    /// `expandEnvVars` in `config/env-expand.ts:47-58`:
    /// - the legacy `[provider]` / `[model]` sections;
    /// - the `providers` map entries (api_key, base_url, source, custom
    ///   headers, env overrides, ...) including nested JSON passthrough
    ///   fields (`oauth`, `source`);
    /// - the `models` map entries (all string fields);
    /// - permission rules, hooks, thinking, and top-level string fields;
    /// - the passthrough TOML sections (`services`, `background`,
    ///   `subagent`, `image`), recursing into nested tables and arrays.
    ///
    /// Runs after TOML parsing and before [`apply_env`](Self::apply_env),
    /// so whole-field env overrides (`MIRRI_API_KEY`, etc.) take precedence
    /// over inline `${...}` references in the file. Undefined variables
    /// expand to an empty string.
    pub fn expand_env_vars(&mut self) {
        self.expand_env_vars_with(&|name: &str| std::env::var(name).ok());
    }

    /// Expand `${VAR}` / `${env:VAR}` references using a custom lookup.
    ///
    /// Extracted from [`expand_env_vars`](Self::expand_env_vars) so the
    /// expansion is testable without mutating the process environment.
    pub(crate) fn expand_env_vars_with(&mut self, lookup: &dyn Fn(&str) -> Option<String>) {
        // Legacy `[provider]` / `[model]` sections.
        if let Some(ref mut provider) = self.provider {
            expand_opt_string(&mut provider.api_key, lookup);
            expand_opt_string(&mut provider.base_url, lookup);
        }
        if let Some(ref mut model) = self.model {
            expand_string(&mut model.id, lookup);
            expand_opt_string(&mut model.provider, lookup);
        }

        // `providers` map entries: expand every string value, including the
        // nested JSON passthrough fields (`oauth`, `source`).
        if let Some(ref mut providers) = self.providers {
            for entry in providers.values_mut() {
                expand_opt_string(&mut entry.r#type, lookup);
                expand_opt_string(&mut entry.api_key, lookup);
                expand_opt_string(&mut entry.base_url, lookup);
                expand_opt_string(&mut entry.default_model, lookup);
                if let Some(ref mut removed_ids) = entry.removed_model_ids {
                    for id in removed_ids.iter_mut() {
                        expand_string(id, lookup);
                    }
                }
                if let Some(ref mut oauth) = entry.oauth {
                    expand_json_value(oauth, lookup);
                }
                if let Some(ref mut env) = entry.env {
                    for value in env.values_mut() {
                        expand_string(value, lookup);
                    }
                }
                if let Some(ref mut headers) = entry.custom_headers {
                    for value in headers.values_mut() {
                        expand_string(value, lookup);
                    }
                }
                if let Some(ref mut source) = entry.source {
                    expand_json_value(source, lookup);
                }
            }
        }

        // `models` map entries: expand every string field of each alias.
        if let Some(ref mut models) = self.models {
            for alias in models.values_mut() {
                expand_string(&mut alias.provider, lookup);
                expand_string(&mut alias.model, lookup);
                if let Some(ref mut capabilities) = alias.capabilities {
                    for capability in capabilities.iter_mut() {
                        expand_string(capability, lookup);
                    }
                }
                expand_opt_string(&mut alias.display_name, lookup);
                expand_opt_string(&mut alias.description, lookup);
                expand_opt_string(&mut alias.reasoning_key, lookup);
                if let Some(ref mut efforts) = alias.support_efforts {
                    for effort in efforts.iter_mut() {
                        expand_string(effort, lookup);
                    }
                }
                expand_opt_string(&mut alias.default_effort, lookup);
                expand_opt_string(&mut alias.protocol, lookup);
            }
        }

        // Top-level string fields.
        expand_opt_string(&mut self.default_provider, lookup);
        expand_opt_string(&mut self.default_model, lookup);
        expand_opt_string(&mut self.default_permission_mode, lookup);
        if let Some(ref mut dirs) = self.extra_skill_dirs {
            for dir in dirs.iter_mut() {
                expand_string(dir, lookup);
            }
        }
        if let Some(ref mut dirs) = self.extra_agent_dirs {
            for dir in dirs.iter_mut() {
                expand_string(dir, lookup);
            }
        }
        if let Some(ref mut agents) = self.disabled_agents {
            for agent in agents.iter_mut() {
                expand_string(agent, lookup);
            }
        }

        // `thinking` section.
        if let Some(ref mut thinking) = self.thinking {
            expand_opt_string(&mut thinking.effort, lookup);
            expand_opt_string(&mut thinking.keep, lookup);
        }

        // `permission` section: mode and user-configured rules.
        if let Some(ref mut permission) = self.permission {
            expand_opt_string(&mut permission.mode, lookup);
            if let Some(ref mut rules) = permission.rules {
                for rule in rules.iter_mut() {
                    expand_string(&mut rule.pattern, lookup);
                    expand_opt_string(&mut rule.reason, lookup);
                }
            }
        }

        // `hooks` section: command, pattern, and hook environment.
        if let Some(ref mut hooks) = self.hooks {
            for hook in hooks.iter_mut() {
                expand_string(&mut hook.command, lookup);
                expand_opt_string(&mut hook.pattern, lookup);
                if let Some(ref mut env) = hook.env {
                    for value in env.values_mut() {
                        expand_string(value, lookup);
                    }
                }
            }
        }

        // Passthrough TOML sections: recursively expand every string value
        // in the value tree, in place.
        if let Some(ref mut value) = self.services {
            expand_toml_value(value, lookup);
        }
        if let Some(ref mut background) = self.background {
            expand_toml_value(background, lookup);
        }
        if let Some(ref mut value) = self.image {
            expand_toml_value(value, lookup);
        }
        // `task` / `subagent` are typed sections whose fields are all
        // numeric / boolean / enum values — no string values to expand.
        // `experimental` holds boolean feature flags only, so it has no
        // string values to expand either (its TOML shape is
        // `HashMap<String, bool>`).
        // `model_catalog` is typed to a boolean flag, so it has no string
        // values to expand either.
    }

    /// Apply environment variable overrides.
    ///
    /// Overrides (only applied when the variable is set and non-empty;
    /// whitespace-only values count as empty). `MIRRI_*` is the canonical
    /// name and wins when both are set; the `MIRRICODE_*` alias is honoured
    /// when the canonical name is absent:
    /// - `MIRRI_MODEL_ID` / `MIRRICODE_MODEL_ID` → `model.id`
    /// - `MIRRI_MODEL_PROVIDER` / `MIRRICODE_MODEL_PROVIDER` →
    ///   `model.provider`
    /// - `MIRRI_API_KEY` / `MIRRICODE_API_KEY` → `provider.api_key`
    /// - `MIRRI_BASE_URL` / `MIRRICODE_BASE_URL` → `provider.base_url`
    /// - `MIRRI_PERMISSION_MODE` / `MIRRICODE_PERMISSION_MODE` →
    ///   `permission.mode`
    ///
    /// Also applies [`apply_env_model_config`](Self::apply_env_model_config)
    /// to synthesize a provider + model alias from `MIRRICODE_MODEL_*`
    /// environment variables. An invalid overlay (e.g. missing API key)
    /// is returned as an error.
    pub fn apply_env(&mut self) -> EngineResult<()> {
        // Non-capturing closure: `Copy`, so passing it by value to
        // `apply_env_with` keeps it available for `apply_env_model_config`.
        let get_env = |name: &str| std::env::var(name);
        self.apply_env_with(get_env);
        self.apply_env_model_config(&get_env)?;
        Ok(())
    }

    /// Apply overrides sourced from a `Fn(&str) -> Result<String, _>` lookup.
    ///
    /// Extracted from [`apply_env`](Self::apply_env) so the override
    /// semantics are testable without mutating the process environment.
    /// Does **not** apply the `MIRRICODE_MODEL_*` overlay — use
    /// [`apply_env_model_config`](Self::apply_env_model_config) for that.
    pub(crate) fn apply_env_with(
        &mut self,
        get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    ) {
        if let Some(value) = env_var_with_alias("MIRRI_MODEL_ID", "MIRRICODE_MODEL_ID", &get_env) {
            self.model.get_or_insert_with(ModelConfig::default).id = value;
        }
        if let Some(value) =
            env_var_with_alias("MIRRI_MODEL_PROVIDER", "MIRRICODE_MODEL_PROVIDER", &get_env)
        {
            self.model.get_or_insert_with(ModelConfig::default).provider = Some(value);
        }
        if let Some(value) = env_var_with_alias("MIRRI_API_KEY", "MIRRICODE_API_KEY", &get_env) {
            self.provider
                .get_or_insert_with(ProviderConfig::default)
                .api_key = Some(value);
        }
        if let Some(value) = env_var_with_alias("MIRRI_BASE_URL", "MIRRICODE_BASE_URL", &get_env) {
            self.provider
                .get_or_insert_with(ProviderConfig::default)
                .base_url = Some(value);
        }
        if let Some(value) = env_var_with_alias(
            "MIRRI_PERMISSION_MODE",
            "MIRRICODE_PERMISSION_MODE",
            &get_env,
        ) {
            self.permission
                .get_or_insert_with(PermissionConfig::default)
                .mode = Some(value);
        }

        // Task-lifecycle overrides (D-65). `MIRRI_*` is the canonical name;
        // `MIRRICODE_BACKGROUND_*` is the documented v1 alias the TS
        // `taskEnvBindings` reads (`configSection.ts:74-75`), so both are
        // honoured. Like the model overrides above, these are in-memory
        // only — the save paths serialize from the file-loaded config, so
        // nothing env-sourced lands on disk. An unparseable value is
        // skipped (the TS `parsePositiveInt` returns `undefined`).
        if let Some(value) = env_var_with_alias(
            "MIRRI_TASK_MAX_RUNNING_TASKS",
            "MIRRICODE_BACKGROUND_MAX_RUNNING_TASKS",
            &get_env,
        ) && let Some(parsed) = parse_u64_min(&value, 1)
            // `max_running_tasks` is a `u32` on the config: a value past
            // its range would wrap to 0 (silently unbounded) through a bare
            // `as u32` cast, so out-of-range is skipped like unparseable.
            && parsed <= u32::MAX as u64
        {
            self.task
                .get_or_insert_with(TaskConfig::default)
                .max_running_tasks = Some(parsed as u32);
        }
        if let Some(value) = env_var_with_alias(
            "MIRRI_TASK_KEEP_ALIVE_ON_EXIT",
            "MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT",
            &get_env,
        ) && let Ok(Some(parsed)) =
            parse_bool_env_var(Some(&value), "MIRRI_TASK_KEEP_ALIVE_ON_EXIT")
        {
            self.task
                .get_or_insert_with(TaskConfig::default)
                .keep_alive_on_exit = Some(parsed);
        }
        if let Some(value) = env_var_non_empty("MIRRI_TASK_PRINT_BACKGROUND_MODE", &get_env) {
            let mode = match value.trim().to_lowercase().as_str() {
                "exit" => Some(PrintBackgroundMode::Exit),
                "drain" => Some(PrintBackgroundMode::Drain),
                "steer" => Some(PrintBackgroundMode::Steer),
                _ => None,
            };
            if let Some(mode) = mode {
                self.task
                    .get_or_insert_with(TaskConfig::default)
                    .print_background_mode = Some(mode);
            }
        }
        if let Some(value) = env_var_non_empty("MIRRI_TASK_PRINT_MAX_TURNS", &get_env)
            && let Some(parsed) = parse_u64_min(&value, 1)
        {
            self.task
                .get_or_insert_with(TaskConfig::default)
                .print_max_turns = Some(parsed);
        }
        if let Some(value) = env_var_non_empty("MIRRI_TASK_PRINT_WAIT_CEILING_S", &get_env)
            && let Some(parsed) = parse_u64_min(&value, 1)
        {
            self.task
                .get_or_insert_with(TaskConfig::default)
                .print_wait_ceiling_s = Some(parsed);
        }
        // Orphan-governance overrides (SDR-013 c/d, US14.3/14.4). The
        // `MIRRICODE_TASK_*` spellings are the canonical names for these
        // knobs; they are new Rust-engine-only keys with no v1 alias, so
        // no `MIRRI_TASK_*` twin exists yet. Like the other task-lifetime
        // overrides these are in-memory only (the save paths serialize
        // from the file-loaded config), and an unparseable value is
        // skipped rather than failing the boot (the task-env fail-soft
        // convention).
        if let Some(value) = env_var_non_empty("MIRRICODE_TASK_ORPHAN_DISPOSE_POLICY", &get_env) {
            let policy = match value.trim().to_lowercase().as_str() {
                "reportonly" => Some(OrphanDisposePolicy::ReportOnly),
                "reportandterminate" => Some(OrphanDisposePolicy::ReportAndTerminate),
                _ => None,
            };
            if let Some(policy) = policy {
                self.task
                    .get_or_insert_with(TaskConfig::default)
                    .orphan_dispose_policy = Some(policy);
            }
        }
        if let Some(value) = env_var_non_empty("MIRRICODE_TASK_MAX_LIFETIME_S", &get_env)
            && let Some(parsed) = parse_u64_min(&value, 0)
        {
            self.task
                .get_or_insert_with(TaskConfig::default)
                .max_task_lifetime_s = Some(parsed);
        }
        if let Some(value) = env_var_with_alias(
            "MIRRI_SUBAGENT_TIMEOUT_MS",
            "MIRRICODE_SUBAGENT_TIMEOUT_MS",
            &get_env,
        ) && let Some(parsed) = parse_u64_min(&value, 0)
        {
            self.subagent
                .get_or_insert_with(SubagentConfig::default)
                .timeout_ms = Some(parsed);
        }
    }

    /// Synthesize a provider and model alias from `MIRRICODE_MODEL_*`
    /// environment variables, making the alias the default model.
    ///
    /// When `MIRRICODE_MODEL_NAME` is set, this method:
    /// 1. Requires `MIRRICODE_MODEL_API_KEY` (returns an error if missing).
    /// 2. Synthesizes a [`ENV_MODEL_PROVIDER_KEY`] provider entry with
    ///    `api_key`, `base_url`, and `provider_type`.
    /// 3. Synthesizes a [`ENV_MODEL_ALIAS_KEY`] model alias with
    ///    `max_context_size`, `capabilities`, `display_name`, `reasoning_key`,
    ///    `adaptive_thinking`, and `max_output_size`.
    /// 4. Sets `default_model` to [`ENV_MODEL_ALIAS_KEY`].
    /// 5. Optionally overrides `thinking.effort` when
    ///    `MIRRICODE_MODEL_THINKING_EFFORT` is set.
    ///
    /// Returns `Ok(())` (no-op) when `MIRRICODE_MODEL_NAME` is absent.
    ///
    /// Mirrors the TypeScript `applyEnvModelConfig` in
    /// `config/env-model.ts:82-143`.
    pub fn apply_env_model_config(
        &mut self,
        get_env: &impl Fn(&str) -> Result<String, std::env::VarError>,
    ) -> EngineResult<()> {
        // Trigger: MIRRICODE_MODEL_NAME must be set and non-empty.
        let model_name = match env_var_non_empty("MIRRICODE_MODEL_NAME", get_env) {
            Some(name) => name,
            None => return Ok(()),
        };

        // API key is required when the trigger is set.
        let api_key = env_var_non_empty("MIRRICODE_MODEL_API_KEY", get_env).ok_or_else(|| {
            EngineError::RequestInvalid(
                "MIRRICODE_MODEL_NAME is set but MIRRICODE_MODEL_API_KEY is missing".to_string(),
            )
        })?;

        // Max context size: default 262144, must be a positive integer.
        let max_context_size = match env_var_non_empty("MIRRICODE_MODEL_MAX_CONTEXT_SIZE", get_env)
        {
            Some(raw) => parse_positive_int(&raw, "MIRRICODE_MODEL_MAX_CONTEXT_SIZE")?,
            None => ENV_MODEL_DEFAULT_MAX_CONTEXT_SIZE,
        };

        // Provider type: default "openai", restricted to allowed types.
        let provider_type = parse_provider_type_env(
            env_var_non_empty("MIRRICODE_MODEL_PROVIDER_TYPE", get_env).as_deref(),
        )?;

        // Base URL: explicit value, or default for the provider type.
        let base_url = env_var_non_empty("MIRRICODE_MODEL_BASE_URL", get_env)
            .or_else(|| default_base_url_for_type(&provider_type));

        // Build the synthesized provider entry.
        let provider_entry = ProviderEntry {
            r#type: Some(provider_type.clone()),
            api_key: Some(api_key),
            base_url,
            ..Default::default()
        };

        // Max output size: optional, must be a positive integer when set.
        let max_output_size = match env_var_non_empty("MIRRICODE_MODEL_MAX_OUTPUT_SIZE", get_env) {
            Some(raw) => Some(parse_positive_int(&raw, "MIRRICODE_MODEL_MAX_OUTPUT_SIZE")?),
            None => None,
        };

        // Capabilities: CSV → lowercase trimmed, or default.
        let capabilities = parse_capabilities_env(
            env_var_non_empty("MIRRICODE_MODEL_CAPABILITIES", get_env).as_deref(),
        );

        let display_name = env_var_non_empty("MIRRICODE_MODEL_DISPLAY_NAME", get_env);
        let reasoning_key = env_var_non_empty("MIRRICODE_MODEL_REASONING_KEY", get_env);

        // Adaptive thinking: optional boolean.
        let adaptive_thinking = parse_bool_env_var(
            env_var_non_empty("MIRRICODE_MODEL_ADAPTIVE_THINKING", get_env).as_deref(),
            "MIRRICODE_MODEL_ADAPTIVE_THINKING",
        )?;

        // Build the synthesized model alias.
        let alias = ModelAlias {
            provider: ENV_MODEL_PROVIDER_KEY.to_string(),
            model: model_name,
            max_context_size,
            max_output_size,
            capabilities,
            display_name,
            reasoning_key,
            adaptive_thinking,
            ..Default::default()
        };

        // Insert into providers and models maps (merging with any existing
        // entries, matching TS `{ ...config.providers, [KEY]: provider }`).
        self.providers
            .get_or_insert_with(HashMap::new)
            .insert(ENV_MODEL_PROVIDER_KEY.to_string(), provider_entry);

        self.models
            .get_or_insert_with(HashMap::new)
            .insert(ENV_MODEL_ALIAS_KEY.to_string(), alias);

        // Force the env alias to be the default model.
        self.default_model = Some(ENV_MODEL_ALIAS_KEY.to_string());

        // Optionally override thinking.effort (preserving other thinking
        // fields, matching TS `{ ...config.thinking, effort }`).
        if let Some(effort) = env_var_non_empty("MIRRICODE_MODEL_THINKING_EFFORT", get_env) {
            self.thinking
                .get_or_insert_with(ThinkingConfig::default)
                .effort = Some(effort);
        }

        Ok(())
    }

    /// Remove environment-synthesized provider/alias entries before writing
    /// config back to disk.
    ///
    /// Removes [`ENV_MODEL_PROVIDER_KEY`] from `providers` and
    /// [`ENV_MODEL_ALIAS_KEY`] from `models`. When `default_model` points at
    /// the env alias, it is cleared so the file is not left referencing a
    /// non-existent alias.
    ///
    /// Mirrors the TypeScript `stripEnvModelConfig` in
    /// `config/env-model.ts:148-182`.
    pub fn strip_env_model_config(&mut self) {
        if let Some(ref mut providers) = self.providers {
            providers.remove(ENV_MODEL_PROVIDER_KEY);
            // Remove the map entirely if it's now empty, so the serialized
            // TOML stays clean.
            if providers.is_empty() {
                self.providers = None;
            }
        }
        if let Some(ref mut models) = self.models {
            models.remove(ENV_MODEL_ALIAS_KEY);
            if models.is_empty() {
                self.models = None;
            }
        }
        if self.default_model.as_deref() == Some(ENV_MODEL_ALIAS_KEY) {
            self.default_model = None;
        }
    }
}

/// Expand `${VAR}` / `${env:VAR}` references in a string using the process
/// environment.
///
/// Undefined variables expand to an empty string. Only a single left-to-right
/// pass is performed (no recursive expansion of interpolated values that
/// themselves contain `${...}`).
///
/// Mirrors the TypeScript `expandEnvString` in `config/env-expand.ts`.
pub fn expand_env_string(input: &str) -> String {
    let f = |name: &str| std::env::var(name).ok();
    expand_env_string_with(input, &f)
}

/// Expand `${VAR}` / `${env:VAR}` references using a custom lookup function.
///
/// This is the testable core of [`expand_env_string`]. The `lookup` function
/// receives the variable name (without the `${...}` wrapper) and returns
/// `Some(value)` when the variable is defined, or `None` when undefined.
pub fn expand_env_string_with(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    // Regex matching ${VAR} or ${env:VAR} where VAR is [A-Za-z_][A-Za-z0-9_]*
    let re = regex::Regex::new(r"\$\{(?:env:)?([A-Za-z_][A-Za-z0-9_]*)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let name = &caps[1];
        lookup(name).unwrap_or_default()
    })
    .into_owned()
}

/// Expand `${VAR}` / `${env:VAR}` references in a `String` in place.
fn expand_string(value: &mut String, lookup: &dyn Fn(&str) -> Option<String>) {
    *value = expand_env_string_with(value, lookup);
}

/// Expand `${VAR}` / `${env:VAR}` references in an `Option<String>` in
/// place. `None` passes through unchanged.
fn expand_opt_string(value: &mut Option<String>, lookup: &dyn Fn(&str) -> Option<String>) {
    if let Some(inner) = value.as_mut() {
        expand_string(inner, lookup);
    }
}

/// Recursively expand `${VAR}` / `${env:VAR}` references in every string
/// value of a `toml::Value` tree, in place.
///
/// Used for the passthrough sections (`services`, `background`, `subagent`,
/// `image`), whose contents are opaque to the engine.
fn expand_toml_value(value: &mut toml::Value, lookup: &dyn Fn(&str) -> Option<String>) {
    match value {
        toml::Value::String(inner) => expand_string(inner, lookup),
        toml::Value::Array(items) => {
            for item in items {
                expand_toml_value(item, lookup);
            }
        }
        toml::Value::Table(table) => {
            for (_, entry) in table.iter_mut() {
                expand_toml_value(entry, lookup);
            }
        }
        _ => {}
    }
}

/// Recursively expand `${VAR}` / `${env:VAR}` references in every string
/// value of a [`serde_json::Value`] tree, in place.
///
/// Used for the provider entry JSON passthrough fields (`oauth`, `source`).
fn expand_json_value(value: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::String(inner) => expand_string(inner, lookup),
        Value::Array(items) => {
            for item in items {
                expand_json_value(item, lookup);
            }
        }
        Value::Object(map) => {
            for entry in map.values_mut() {
                expand_json_value(entry, lookup);
            }
        }
        _ => {}
    }
}

/// Read an environment variable, returning `None` when unset, non-Unicode, or
/// empty/whitespace-only (a blank override is treated as "not provided").
fn env_var_non_empty(
    name: &str,
    get_env: &impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<String> {
    get_env(name).ok().filter(|value| !value.trim().is_empty())
}

/// Read an environment variable, trying `primary` first and falling back to
/// `alias`.
///
/// Both names go through [`env_var_non_empty`], so an empty or whitespace-only
/// `primary` falls through to a non-empty `alias`. Returns `None` when neither
/// name is set to a non-empty value.
fn env_var_with_alias(
    primary: &str,
    alias: &str,
    get_env: &impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Option<String> {
    env_var_non_empty(primary, get_env).or_else(|| env_var_non_empty(alias, get_env))
}

/// Parse a trimmed string as an integer `>= min` (`u64`), returning `None`
/// for non-digit or below-minimum values. The task-lifecycle env overrides
/// use this; an unparseable value is skipped, matching the TS
/// `parsePositiveInt` (`configSection.ts:66-70`) which yields `undefined`.
fn parse_u64_min(raw: &str, min: u64) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok().filter(|parsed| *parsed >= min)
}

/// Parse a trimmed string as a positive integer (`u64 > 0`), returning an
/// error for non-digit or zero values.
///
/// Mirrors the TypeScript `parsePositiveInt` in `config/env-model.ts`.
fn parse_positive_int(raw: &str, var_name: &str) -> EngineResult<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(EngineError::RequestInvalid(format!(
            "{var_name} must be a positive integer, got '{raw}'"
        )));
    }
    let n: u64 = trimmed.parse().map_err(|_| {
        EngineError::RequestInvalid(format!(
            "{var_name} must be a positive integer, got '{raw}'"
        ))
    })?;
    if n == 0 {
        return Err(EngineError::RequestInvalid(format!(
            "{var_name} must be a positive integer, got '{raw}'"
        )));
    }
    Ok(n)
}

/// Parse a boolean environment variable.
///
/// Accepts `true`/`1`/`yes`/`on` → `true`, `false`/`0`/`no`/`off` → `false`
/// (case-insensitive). Returns `Ok(None)` when `raw` is `None`. Returns an
/// error for unrecognized values.
///
/// Mirrors the TypeScript `parseBooleanVar` in `config/env-model.ts`.
fn parse_bool_env_var(raw: Option<&str>, var_name: &str) -> EngineResult<Option<bool>> {
    match raw {
        None => Ok(None),
        Some(value) => {
            let lower = value.trim().to_lowercase();
            match lower.as_str() {
                "true" | "1" | "yes" | "on" => Ok(Some(true)),
                "false" | "0" | "no" | "off" => Ok(Some(false)),
                _ => Err(EngineError::RequestInvalid(format!(
                    "{var_name} must be a boolean (true/false/1/0/yes/no/on/off), got '{value}'"
                ))),
            }
        }
    }
}

/// Parse a comma-separated capabilities string into a lowercase, trimmed,
/// non-empty vector. Returns the default capabilities when `raw` is `None`.
///
/// Mirrors the TypeScript `parseCapabilities` + `DEFAULT_CAPABILITIES` in
/// `config/env-model.ts`.
fn parse_capabilities_env(raw: Option<&str>) -> Option<Vec<String>> {
    match raw {
        None => Some(
            ENV_MODEL_DEFAULT_CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        Some(value) => Some(
            value
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        ),
    }
}

/// Parse the provider type environment variable.
///
/// Defaults to `"openai"` when `raw` is `None`. The value is lowercased and
/// must be one of [`ENV_MODEL_ALLOWED_PROVIDER_TYPES`], otherwise an error is
/// returned.
///
/// Mirrors the TypeScript `parseProviderType` in `config/env-model.ts`.
fn parse_provider_type_env(raw: Option<&str>) -> EngineResult<String> {
    match raw {
        None => Ok("openai".to_string()),
        Some(value) => {
            let lower = value.trim().to_lowercase();
            if ENV_MODEL_ALLOWED_PROVIDER_TYPES.contains(&lower.as_str()) {
                Ok(lower)
            } else {
                Err(EngineError::RequestInvalid(format!(
                    "MIRRICODE_MODEL_PROVIDER_TYPE '{value}' is not a valid provider type"
                )))
            }
        }
    }
}

/// Return the default base URL for a provider type, if one exists.
///
/// Only `"openai"` has a default base URL (`https://api.openai.com/v1`);
/// `"anthropic"` and `"google-genai"` use their SDK defaults (return `None`).
///
/// Mirrors the TypeScript `DEFAULT_BASE_URL` in `config/env-model.ts`.
fn default_base_url_for_type(provider_type: &str) -> Option<String> {
    match provider_type {
        "openai" => Some("https://api.openai.com/v1".to_string()),
        _ => None,
    }
}
