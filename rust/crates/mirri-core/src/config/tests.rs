use super::doctor::{CheckStatus, validate_config_str};
use super::wire::{KNOWN_HOOK_ENTRY_KEYS, KNOWN_PERMISSION_RULE_KEYS, KNOWN_SECTION_KEYS};
use super::*;
use crate::hook_engine::HookEventType;
use crate::permission::{RuleDecision, RuleScope};
use mirri_os::LocalSyncFs;
use serde_json::json;

/// Parse a TOML config string, panicking on invalid input.
fn parse_config_toml(toml_str: &str) -> ServerConfig {
    ServerConfig::from_toml(toml_str).expect("TOML should parse")
}

fn fake_env<'a>(
    overrides: &'a [(&'a str, &'a str)],
) -> impl for<'b> Fn(&'b str) -> Result<String, std::env::VarError> + 'a {
    move |name: &str| {
        overrides
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
            .ok_or(std::env::VarError::NotPresent)
    }
}

#[test]
fn should_parse_all_sections_when_from_toml_with_full_toml() {
    let config = ServerConfig::from_toml(
        r#"
[model]
id = "claude-sonnet-4-20250514"
provider = "anthropic"

[provider]
api_key = "sk-test"
base_url = "https://api.example.com"

[permission]
mode = "yolo"

[loop_control]
max_steps_per_turn = 30
max_retries_per_step = 3

[experimental]
streaming = true
fast_render = false
"#,
    )
    .expect("TOML should parse");

    let model = config.model.expect("model section should parse");
    assert_eq!(model.id, "claude-sonnet-4-20250514");
    assert_eq!(model.provider.as_deref(), Some("anthropic"));

    let provider = config.provider.expect("provider section should parse");
    assert_eq!(provider.api_key.as_deref(), Some("sk-test"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://api.example.com")
    );

    let permission = config.permission.expect("permission section should parse");
    assert_eq!(permission.mode.as_deref(), Some("yolo"));

    let loop_control = config
        .loop_control
        .expect("loop_control section should parse");
    assert_eq!(loop_control.max_steps_per_turn, Some(30));
    assert_eq!(loop_control.max_retries_per_step, Some(3));

    let experimental = config.experimental.expect("experimental should parse");
    assert_eq!(experimental.get("streaming"), Some(&true));
    assert_eq!(experimental.get("fast_render"), Some(&false));
}

#[test]
fn should_leave_all_sections_none_when_from_toml_with_empty_toml() {
    let config = ServerConfig::from_toml("").expect("empty TOML should parse");
    assert!(config.model.is_none());
    assert!(config.provider.is_none());
    assert!(config.permission.is_none());
    assert!(config.loop_control.is_none());
    assert!(config.experimental.is_none());
}

#[test]
fn should_leave_missing_sections_none_when_from_toml_with_partial_toml() {
    let config =
        ServerConfig::from_toml("[model]\nid = \"gpt-4o\"\n").expect("partial TOML should parse");
    assert_eq!(config.model.expect("model should parse").id, "gpt-4o");
    assert!(config.provider.is_none());
}

#[test]
fn should_return_error_when_from_toml_with_invalid_toml() {
    let error = ServerConfig::from_toml("[model\nid = ").expect_err("malformed TOML should fail");
    assert!(error.to_string().contains("invalid config TOML"));
}

#[test]
fn should_report_reloadable_sections_when_reload_changes_sees_different_config() {
    // Given a previous config with every reloadable section set
    let previous = ServerConfig::from_toml(
        r#"
yolo = true
plan_mode = true

[model]
id = "gpt-4o"
[provider]
api_key = "sk-old"
[providers.anthropic]
type = "anthropic"
[models.fast]
provider = "anthropic"
model = "claude-haiku-4-20250514"
max_context_size = 200000
[thinking]
enabled = true
effort = "high"
[permission]
mode = "manual"
[loop_control]
max_steps_per_turn = 30
[[hooks]]
event = "SessionStart"
command = "echo start"
"#,
    )
    .expect("previous TOML should parse");

    // When a new config differs in every reloadable section (but keeps
    // experimental identical)
    let new = ServerConfig::from_toml(
        r#"
yolo = false
plan_mode = false

[model]
id = "claude-sonnet-4-20250514"
[provider]
api_key = "sk-new"
[providers.openai]
type = "openai"
[models.powerful]
provider = "openai"
model = "gpt-5"
max_context_size = 200000
[thinking]
enabled = false
effort = "low"
[permission]
mode = "yolo"
[loop_control]
max_steps_per_turn = 50
[[hooks]]
event = "SessionEnd"
command = "echo end"
"#,
    )
    .expect("new TOML should parse");

    // Then every reloadable section is reported
    let changes = new.reload_changes(&previous);
    for expected in [
        ConfigChange::Providers,
        ConfigChange::Models,
        ConfigChange::Thinking,
        ConfigChange::Permission,
        ConfigChange::LoopControl,
        ConfigChange::Yolo,
        ConfigChange::PlanMode,
        ConfigChange::Hooks,
    ] {
        assert!(
            changes.contains(&expected),
            "expected {expected:?} in {changes:?}"
        );
    }
}

#[test]
fn should_report_no_changes_when_reload_changes_sees_identical_config() {
    // Given a config parsed twice from the same document
    let toml = "[model]\nid = \"gpt-4o\"\n[permission]\nmode = \"auto\"\n";
    let previous = ServerConfig::from_toml(toml).expect("previous TOML should parse");
    let new = ServerConfig::from_toml(toml).expect("new TOML should parse");

    // When the diff is computed
    let changes = new.reload_changes(&previous);

    // Then no reloadable section is reported
    assert!(changes.is_empty(), "expected no changes, got {changes:?}");
}

#[test]
fn should_not_report_experimental_flags_when_reload_changes_sees_experimental_change() {
    // Given a previous config with experimental flags
    let previous = ServerConfig::from_toml("[experimental]\nstreaming = true\n")
        .expect("previous TOML should parse");

    // When only the experimental section differs on disk
    let new = ServerConfig::from_toml("[experimental]\nstreaming = false\n")
        .expect("new TOML should parse");

    // Then no reloadable section is reported (experimental is restart-only)
    let changes = new.reload_changes(&previous);
    assert!(
        changes.is_empty(),
        "experimental flags must not be hot-reloadable, got {changes:?}"
    );
}

#[test]
fn should_report_hooks_change_when_hook_definition_changes() {
    // Given a previous config with one hook
    let previous =
        ServerConfig::from_toml("[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo start\"\n")
            .expect("previous TOML should parse");

    // When the hook command changes
    let new =
        ServerConfig::from_toml("[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo go\"\n")
            .expect("new TOML should parse");

    // Then the hooks section is reported as changed
    let changes = new.reload_changes(&previous);
    assert_eq!(changes, vec![ConfigChange::Hooks]);
}

#[test]
fn should_parse_and_apply_env_when_loading_config_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let path = temp_dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[model]\nid = \"claude-sonnet-4-20250514\"\n[permission]\nmode = \"manual\"\n",
    )
    .expect("write should succeed");

    let config = ServerConfig::load(&path, &LocalSyncFs).expect("load should succeed");
    assert_eq!(
        config.model.expect("model should parse").id,
        "claude-sonnet-4-20250514"
    );
    assert_eq!(
        config
            .permission
            .expect("permission should parse")
            .mode
            .as_deref(),
        Some("manual")
    );
}

#[test]
fn should_return_default_config_when_loading_missing_config_file() {
    let config =
        ServerConfig::load_without_env(Path::new("/nonexistent/config.toml"), &LocalSyncFs)
            .expect("missing file should load as defaults");
    assert!(config.providers.is_none() && config.models.is_none() && config.model.is_none());
}

#[test]
fn should_error_when_config_file_exists_but_cannot_be_parsed() {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "not [ valid toml").expect("write should succeed");
    let error = ServerConfig::load_without_env(&path, &LocalSyncFs)
        .expect_err("unparseable file must error, never silently reset");
    assert!(error.to_string().contains("invalid config TOML"));
}

#[test]
fn should_update_fields_when_apply_env_with_overrides() {
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
        }),
        provider: Some(ProviderConfig {
            api_key: Some("old-key".to_string()),
            base_url: None,
        }),
        ..Default::default()
    };

    config.apply_env_with(fake_env(&[
        ("MIRRI_MODEL_ID", "gpt-4o"),
        ("MIRRI_MODEL_PROVIDER", "openai"),
        ("MIRRI_API_KEY", "new-key"),
        ("MIRRI_BASE_URL", "https://override.example.com"),
        ("MIRRI_PERMISSION_MODE", "yolo"),
    ]));

    assert_eq!(config.model.as_ref().expect("model").id, "gpt-4o");
    assert_eq!(
        config.model.as_ref().expect("model").provider.as_deref(),
        Some("openai")
    );
    assert_eq!(
        config
            .provider
            .as_ref()
            .expect("provider")
            .api_key
            .as_deref(),
        Some("new-key")
    );
    assert_eq!(
        config
            .provider
            .as_ref()
            .expect("provider")
            .base_url
            .as_deref(),
        Some("https://override.example.com")
    );
    assert_eq!(
        config
            .permission
            .as_ref()
            .expect("permission")
            .mode
            .as_deref(),
        Some("yolo")
    );
}

#[test]
fn should_create_missing_sections_when_apply_env_with_overrides_on_empty_config() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[("MIRRI_MODEL_ID", "gpt-4o")]));
    assert_eq!(config.model.expect("model created").id, "gpt-4o");
}

#[test]
fn should_ignore_empty_or_absent_env_values_when_apply_env() {
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: None,
        }),
        ..Default::default()
    };

    config.apply_env_with(fake_env(&[
        // Empty override must be treated as "not provided".
        ("MIRRI_MODEL_ID", ""),
        // Not present at all.
        ("MIRRI_API_KEY", ""),
    ]));

    assert_eq!(
        config.model.expect("model").id,
        "claude-sonnet-4-20250514",
        "empty override must not clobber the configured value"
    );
    assert!(config.provider.is_none());
}

#[test]
fn should_leave_config_unchanged_when_apply_env_with_no_overrides() {
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: None,
        }),
        ..Default::default()
    };
    let original = config.clone();

    config.apply_env_with(fake_env(&[]));
    assert_eq!(
        config.model.expect("model").id,
        original.model.expect("model").id
    );
}

#[test]
fn should_not_expand_when_env_value_is_empty_string() {
    // Given a config with an existing model id and an empty env override
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
        }),
        provider: Some(ProviderConfig {
            api_key: Some("existing-key".to_string()),
            base_url: None,
        }),
        ..Default::default()
    };

    // When applying env overrides where the model id override is empty
    config.apply_env_with(fake_env(&[("MIRRI_MODEL_ID", "")]));

    // Then the empty override is ignored and existing values are kept
    let model = config.model.as_ref().expect("model");
    assert_eq!(
        model.id, "claude-sonnet-4-20250514",
        "empty override must not clobber the configured value"
    );
    assert_eq!(model.provider.as_deref(), Some("anthropic"));
}

#[test]
fn should_not_expand_when_env_value_is_whitespace_only() {
    // Given a config with an existing model id and a whitespace-only env
    // override
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: None,
        }),
        ..Default::default()
    };

    // When applying env overrides where the base-url override is " "
    config.apply_env_with(fake_env(&[("MIRRI_BASE_URL", " ")]));

    // Then the whitespace-only override is treated as not provided
    assert_eq!(
        config.model.expect("model").id,
        "claude-sonnet-4-20250514",
        "whitespace-only override must not clobber the configured value"
    );
    assert!(config.provider.is_none(), "provider must not be created");
}

#[test]
fn should_apply_multiple_sections_when_env_has_overrides_for_all() {
    // Given an empty config and env overrides for the model, provider,
    // and permission sections (loop_control has no env override)
    let mut config = ServerConfig::default();

    // When applying env overrides for every overridable section
    config.apply_env_with(fake_env(&[
        ("MIRRI_MODEL_ID", "gpt-4o"),
        ("MIRRI_MODEL_PROVIDER", "openai"),
        ("MIRRI_API_KEY", "sk-env"),
        ("MIRRI_BASE_URL", "https://env.example.com"),
        ("MIRRI_PERMISSION_MODE", "yolo"),
    ]));

    // Then every overridable section is populated from the environment
    let model = config.model.expect("model section created");
    assert_eq!(model.id, "gpt-4o");
    assert_eq!(model.provider.as_deref(), Some("openai"));

    let provider = config.provider.expect("provider section created");
    assert_eq!(provider.api_key.as_deref(), Some("sk-env"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://env.example.com")
    );

    let permission = config.permission.expect("permission section created");
    assert_eq!(permission.mode.as_deref(), Some("yolo"));

    // The loop_control section has no env override and stays absent
    assert!(config.loop_control.is_none());
    assert!(config.experimental.is_none());
}

#[test]
fn should_preserve_existing_values_when_env_does_not_override_them() {
    // Given a config with existing values across sections
    let mut config = ServerConfig {
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
        }),
        provider: Some(ProviderConfig {
            api_key: Some("existing-key".to_string()),
            base_url: Some("https://existing.example.com".to_string()),
        }),
        permission: Some(PermissionConfig {
            mode: Some("manual".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    // When applying env overrides that only touch the model id
    config.apply_env_with(fake_env(&[("MIRRI_MODEL_ID", "gpt-4o")]));

    // Then only the overridden field changes; everything else is preserved
    let model = config.model.as_ref().expect("model");
    assert_eq!(model.id, "gpt-4o");
    assert_eq!(
        model.provider.as_deref(),
        Some("anthropic"),
        "non-overridden model.provider must be preserved"
    );
    let provider = config.provider.as_ref().expect("provider");
    assert_eq!(
        provider.api_key.as_deref(),
        Some("existing-key"),
        "non-overridden api_key must be preserved"
    );
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://existing.example.com"),
        "non-overridden base_url must be preserved"
    );
    assert_eq!(
        config
            .permission
            .as_ref()
            .expect("permission")
            .mode
            .as_deref(),
        Some("manual"),
        "non-overridden permission mode must be preserved"
    );
}

#[test]
fn should_create_config_with_only_env_overrides_when_toml_is_empty() {
    // Given an empty TOML document and env overrides for several sections
    let mut config = ServerConfig::from_toml("").expect("empty TOML should parse");

    // When applying the env overrides
    config.apply_env_with(fake_env(&[
        ("MIRRI_MODEL_ID", "gpt-4o"),
        ("MIRRI_MODEL_PROVIDER", "openai"),
        ("MIRRI_API_KEY", "sk-env"),
        ("MIRRI_PERMISSION_MODE", "auto"),
    ]));

    // Then the sections are created purely from the environment
    let model = config.model.expect("model created from env");
    assert_eq!(model.id, "gpt-4o");
    assert_eq!(model.provider.as_deref(), Some("openai"));

    let provider = config.provider.expect("provider created from env");
    assert_eq!(provider.api_key.as_deref(), Some("sk-env"));
    assert!(provider.base_url.is_none(), "unset base url must stay None");

    assert_eq!(
        config
            .permission
            .expect("permission created from env")
            .mode
            .as_deref(),
        Some("auto")
    );
    assert!(config.loop_control.is_none());
}

#[test]
fn should_use_mirricode_alias_when_only_alias_env_is_set() {
    // Given an empty config and only the MIRRICODE_* alias env variables
    // set (no MIRRI_* equivalents)
    let mut config = ServerConfig::default();

    // When applying env overrides sourced from the alias variables
    config.apply_env_with(fake_env(&[
        ("MIRRICODE_MODEL_ID", "gpt-4o"),
        ("MIRRICODE_MODEL_PROVIDER", "openai"),
        ("MIRRICODE_API_KEY", "sk-alias"),
        ("MIRRICODE_BASE_URL", "https://alias.example.com"),
        ("MIRRICODE_PERMISSION_MODE", "yolo"),
    ]));

    // Then every alias value populates its section
    let model = config.model.expect("model created from alias");
    assert_eq!(model.id, "gpt-4o");
    assert_eq!(model.provider.as_deref(), Some("openai"));

    let provider = config.provider.expect("provider created from alias");
    assert_eq!(provider.api_key.as_deref(), Some("sk-alias"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://alias.example.com")
    );

    assert_eq!(
        config
            .permission
            .expect("permission created from alias")
            .mode
            .as_deref(),
        Some("yolo")
    );
}

#[test]
fn should_prefer_primary_env_over_alias_when_both_are_set() {
    // Given both the MIRRI_* primary and the MIRRICODE_* alias env
    // variables set, with different values
    let mut config = ServerConfig::default();

    // When applying env overrides where both names exist
    config.apply_env_with(fake_env(&[
        ("MIRRI_MODEL_ID", "primary-model"),
        ("MIRRICODE_MODEL_ID", "alias-model"),
    ]));

    // Then the MIRRI_* primary wins over the alias
    assert_eq!(
        config.model.expect("model created").id,
        "primary-model",
        "MIRRI_* must take precedence over MIRRICODE_*"
    );
}

#[test]
fn should_fall_through_to_alias_when_primary_env_is_empty() {
    // Given the MIRRI_* primary set to an empty value and the
    // MIRRICODE_* alias set to a real value
    let mut config = ServerConfig::default();

    // When applying env overrides where the primary is blank
    config.apply_env_with(fake_env(&[
        ("MIRRI_MODEL_ID", ""),
        ("MIRRICODE_MODEL_ID", "alias-model"),
    ]));

    // Then the blank primary is treated as unset and the alias is used
    assert_eq!(
        config.model.expect("model created").id,
        "alias-model",
        "an empty primary must fall through to a non-empty alias"
    );
}

#[test]
fn should_parse_permission_rules_when_from_toml_with_rules_section() {
    // Given a TOML config with a [[permission.rules]] table (one rule
    // with every field, one rule omitting the optional reason, one rule
    // omitting the optional scope, and one rule in the TS kebab-case
    // `turn-override` spelling)
    let config = ServerConfig::from_toml(
        r#"
[permission]
mode = "yolo"

[[permission.rules]]
decision = "allow"
scope = "user"
pattern = "Bash*"
reason = "testing needs bash"

[[permission.rules]]
decision = "deny"
scope = "project"
pattern = "Edit*"

[[permission.rules]]
decision = "ask"
pattern = "WebSearch*"

[[permission.rules]]
decision = "allow"
scope = "turn-override"
pattern = "Read*"
"#,
    )
    .expect("TOML should parse");

    // When parsing the permission rules section
    let permission = config.permission.expect("permission should parse");
    let rules = permission.rules.expect("rules should parse");

    // Then the rules are populated in order with the given decisions,
    // scopes, patterns, and reasons
    assert_eq!(rules.len(), 4);
    assert_eq!(rules[0].decision, RuleDecision::Allow);
    assert_eq!(rules[0].scope, RuleScope::User);
    assert_eq!(rules[0].pattern, "Bash*");
    assert_eq!(rules[0].reason.as_deref(), Some("testing needs bash"));
    assert_eq!(rules[1].decision, RuleDecision::Deny);
    assert_eq!(rules[1].scope, RuleScope::Project);
    assert_eq!(rules[1].pattern, "Edit*");
    assert_eq!(rules[1].reason, None, "missing reason must default to None");
    assert_eq!(
        rules[2].scope,
        RuleScope::User,
        "an omitted scope must default to User, matching TS behavior"
    );
    assert_eq!(
        rules[3].scope,
        RuleScope::TurnOverride,
        "the kebab-case 'turn-override' spelling must map onto TurnOverride"
    );
}

#[test]
fn should_parse_hooks_with_snake_and_ts_alias_fields_when_from_toml() {
    // Given a TOML config with [[hooks]] entries where the first uses
    // snake_case fields and the second uses the TypeScript spellings
    // (matcher / timeout)
    let config = ServerConfig::from_toml(
        r#"
[[hooks]]
event = "PreToolUse"
command = "echo before"
pattern = "Bash*"
timeout_ms = 30000

[[hooks]]
event = "PostToolUse"
command = "echo after"
matcher = "Write*"
timeout = 60
"#,
    )
    .expect("TOML should parse");

    // When parsing the hooks section
    let hooks = config.hooks.expect("hooks should parse");

    // Then both entries are populated regardless of field naming style
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].event, HookEventType::PreToolUse);
    assert_eq!(hooks[0].pattern.as_deref(), Some("Bash*"));
    assert_eq!(hooks[0].timeout_ms, 30_000);
    assert_eq!(hooks[1].event, HookEventType::PostToolUse);
    assert_eq!(
        hooks[1].pattern.as_deref(),
        Some("Write*"),
        "the 'matcher' alias must map onto pattern"
    );
    assert_eq!(
        hooks[1].timeout_ms, 60_000,
        "the 'timeout' alias must map onto timeout_ms with TS seconds converted to ms"
    );
}

#[test]
fn should_parse_loop_control_compat_fields_when_from_toml_with_snake_case_names() {
    // Given a TOML config with the loop_control compatibility fields in
    // snake_case
    let config = ServerConfig::from_toml(
        r#"
[loop_control]
max_steps_per_turn = 30
max_ralph_iterations = 4
reserved_context_size = 8192
compaction_trigger_ratio = 0.75
max_overflow_compaction_attempts = 2
"#,
    )
    .expect("TOML should parse");

    // When parsing the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the compatibility fields are populated alongside the existing
    // ones
    assert_eq!(loop_control.max_steps_per_turn, Some(30));
    assert_eq!(loop_control.max_ralph_iterations, Some(4));
    assert_eq!(loop_control.reserved_context_size, Some(8192));
    assert_eq!(loop_control.compaction_trigger_ratio, Some(0.75));
    assert_eq!(loop_control.max_overflow_compaction_attempts, Some(2));
}

#[test]
fn should_parse_loop_control_compat_fields_when_from_toml_with_camel_case_names() {
    // Given a TOML config using the TypeScript camelCase spellings of the
    // loop_control compatibility fields
    let config = ServerConfig::from_toml(
        r#"
[loop_control]
maxRalphIterations = 6
reservedContextSize = 16384
compactionTriggerRatio = 0.9
maxOverflowCompactionAttempts = 5
"#,
    )
    .expect("TOML should parse");

    // When parsing the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the camelCase aliases populate the snake_case fields
    assert_eq!(loop_control.max_ralph_iterations, Some(6));
    assert_eq!(loop_control.reserved_context_size, Some(16384));
    assert_eq!(loop_control.compaction_trigger_ratio, Some(0.9));
    assert_eq!(loop_control.max_overflow_compaction_attempts, Some(5));
}

#[test]
fn should_parse_loop_control_capacity_keys_when_from_toml_with_snake_case_names() {
    // Given a TOML config with the capacity keys in snake_case (the caps
    // the turn flow applies via set_limits every turn, and the resume /
    // close-time-snapshot fold simulations model — ReplayLimits)
    let config = ServerConfig::from_toml(
        r#"
[loop_control]
max_history_messages = 4
max_deferred_messages = 2
"#,
    )
    .expect("TOML should parse");

    // When parsing the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then both capacity keys are populated
    assert_eq!(loop_control.max_history_messages, Some(4));
    assert_eq!(loop_control.max_deferred_messages, Some(2));
}

#[test]
fn should_parse_loop_control_capacity_keys_when_from_toml_with_camel_case_names() {
    // Given a TOML config using the TypeScript camelCase spellings of the
    // capacity keys
    let config = ServerConfig::from_toml(
        r#"
[loop_control]
maxHistoryMessages = 8
maxDeferredMessages = 6
"#,
    )
    .expect("TOML should parse");

    // When parsing the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the camelCase aliases populate the snake_case fields
    assert_eq!(loop_control.max_history_messages, Some(8));
    assert_eq!(loop_control.max_deferred_messages, Some(6));
}

#[test]
fn should_keep_default_capacity_limits_when_loop_control_keys_absent() {
    // Given configs whose [loop_control] omits the capacity keys — a
    // partial section, and no section at all (ROADMAP deferred item #3:
    // an absent key must keep the 10 000 / 1 000 production defaults,
    // never silently lift the caps)
    let partial = ServerConfig::from_toml("[loop_control]\nmax_steps_per_turn = 30")
        .expect("partial section should parse");
    let absent = ServerConfig::default();

    // When deriving the resume-time replay limits
    let partial_limits = partial.replay_limits();
    let absent_limits = absent.replay_limits();

    // Then both keep the production defaults (the constants
    // `CompactionStrategy::default` carries — the single source of truth)
    for limits in [partial_limits, absent_limits] {
        assert_eq!(limits.max_history_messages, 10_000);
        assert_eq!(limits.max_deferred_messages, 1_000);
    }
}

#[test]
fn should_parse_turn_stall_timeout_when_from_toml_with_snake_case_name() {
    // Given a [loop_control] section setting the turn stall watchdog
    // window in snake_case (the section's primary spelling — every other
    // key serializes snake_case with a camelCase deserialization alias)
    let config = ServerConfig::from_toml("[loop_control]\nturn_stall_timeout_ms = 120_000\n")
        .expect("TOML should parse");

    // When reading the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the stall window is populated verbatim (absent keeps the 300 s
    // engine default; an explicit value — including 0, disabling — is
    // taken as-is, never remapped here)
    assert_eq!(loop_control.turn_stall_timeout_ms, Some(120_000));
}

#[test]
fn should_parse_turn_stall_timeout_when_from_toml_with_camel_case_alias() {
    // Given a [loop_control] section using the camelCase alias
    let config = ServerConfig::from_toml("[loop_control]\nturnStallTimeoutMs = 45_000\n")
        .expect("TOML should parse");

    // When reading the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the alias populates the same field as the snake_case spelling
    assert_eq!(loop_control.turn_stall_timeout_ms, Some(45_000));
}

#[test]
fn should_parse_zero_turn_stall_timeout_when_configured_to_disable_watchdog() {
    // Given a [loop_control] section that sets the stall window to 0
    // (the `[subagent] timeout_ms` "0 = no timeout" convention)
    let config = ServerConfig::from_toml("[loop_control]\nturn_stall_timeout_ms = 0\n")
        .expect("TOML should parse");

    // When reading the loop_control section
    let loop_control = config.loop_control.expect("loop_control should parse");

    // Then the 0 survives verbatim — disabling is a value the engine
    // interprets, not a value the parser folds back to None
    assert_eq!(loop_control.turn_stall_timeout_ms, Some(0));
}

#[test]
fn should_keep_turn_stall_timeout_none_when_key_absent() {
    // Given configs whose [loop_control] omits the stall key — a partial
    // section, and no section at all
    let partial = ServerConfig::from_toml("[loop_control]\nmax_steps_per_turn = 30\n")
        .expect("partial section should parse");
    let absent = ServerConfig::default();

    // When reading the loop_control sections
    // Then the field is None in both (absent = keep the engine's 300 s
    // default, decided at construction time — not here)
    assert_eq!(
        partial
            .loop_control
            .expect("partial should parse")
            .turn_stall_timeout_ms,
        None
    );
    assert_eq!(absent.loop_control, None);
}

#[test]
fn should_round_trip_turn_stall_timeout_when_config_saved_and_reloaded() {
    // Given a config file whose [loop_control] sets the stall window
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "[loop_control]\nturn_stall_timeout_ms = 90_000\nmax_steps_per_turn = 20\n",
    )
    .unwrap();
    let config = ServerConfig::load_without_env(&path, &LocalSyncFs).expect("should load");

    // When the config is saved and reloaded
    config.save_to(&path).expect("should save");
    let reloaded = ServerConfig::load_without_env(&path, &LocalSyncFs).expect("should reload");

    // Then the stall window survives the round-trip (the struct-path
    // serialization owns the key — it must not be dropped or duplicated)
    assert_eq!(
        reloaded
            .loop_control
            .expect("loop_control should survive the round-trip")
            .turn_stall_timeout_ms,
        Some(90_000)
    );
    // And the written file carries the key exactly once, under the
    // snake_case primary name
    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        contents.matches("turn_stall_timeout_ms").count(),
        1,
        "the saved config must carry the stall key once: {contents}"
    );
}

#[test]
fn should_migrate_thinking_effort_max_to_high_when_config_has_stale_value() {
    // Given a config.toml with a stale thinking.effort = "max" and a
    // fresh home directory (no marker file yet)
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let config_path = temp_dir.path().join("config.toml");
    std::fs::write(&config_path, "[thinking]\neffort = \"max\"\n")
        .expect("write config.toml should succeed");
    let mut config = ServerConfig::load_without_env(&config_path, &LocalSyncFs)
        .expect("config.toml should parse");

    // When running the config migration
    migrate_config(&mut config, &config_path, temp_dir.path(), &LocalSyncFs)
        .expect("migration should succeed");

    // Then the in-memory effort is rewritten to "high"
    assert_eq!(
        config
            .thinking
            .as_ref()
            .expect("thinking should parse")
            .effort
            .as_deref(),
        Some("high")
    );
    // ... the marker file records the applied migration ...
    let marker_path = temp_dir.path().join(MIGRATIONS_MARKER_FILE);
    let markers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&marker_path).expect("marker file should be written"),
    )
    .expect("marker file should be valid JSON");
    assert!(
        markers.get(MIGRATION_THINKING_EFFORT_MAX_TO_HIGH).is_some(),
        "marker file should record the applied migration"
    );
    // ... and the rewrite is persisted to config.toml, so a later boot
    // loads the migrated value from disk (the fix for the in-memory-only
    // migration bug)
    let persisted = ServerConfig::from_toml(
        &std::fs::read_to_string(&config_path)
            .expect("config.toml should be rewritten by the migration"),
    )
    .expect("rewritten config.toml should parse");
    assert_eq!(
        persisted
            .thinking
            .as_ref()
            .expect("thinking should persist")
            .effort
            .as_deref(),
        Some("high"),
        "config.toml must persist the migrated effort"
    );
}

#[test]
fn should_skip_migration_when_marker_already_records_the_migration() {
    // Given a marker file that already records the migration and a
    // config whose effort is still "max"
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let config_path = temp_dir.path().join("config.toml");
    std::fs::write(&config_path, "[thinking]\neffort = \"max\"\n")
        .expect("write config.toml should succeed");
    let marker_path = temp_dir.path().join(MIGRATIONS_MARKER_FILE);
    std::fs::write(
        &marker_path,
        format!("{{\"{MIGRATION_THINKING_EFFORT_MAX_TO_HIGH}\": \"2024-01-01T00:00:00Z\"}}\n"),
    )
    .expect("write marker should succeed");
    let mut config = ServerConfig::load_without_env(&config_path, &LocalSyncFs)
        .expect("config.toml should parse");

    // When running the config migration again
    migrate_config(&mut config, &config_path, temp_dir.path(), &LocalSyncFs)
        .expect("migration should succeed");

    // Then the already-marked migration is skipped entirely (fast path):
    // the value stays "max" in memory, the config file is left untouched,
    // and the marker keeps its original timestamp
    assert_eq!(
        config
            .thinking
            .as_ref()
            .expect("thinking should parse")
            .effort
            .as_deref(),
        Some("max")
    );
    let persisted = std::fs::read_to_string(&config_path).expect("config should be readable");
    assert!(
        persisted.contains("effort = \"max\""),
        "a skipped migration must not rewrite config.toml"
    );
}

#[test]
fn should_apply_migration_when_marker_file_is_corrupt() {
    // Given a corrupt (unparseable) marker file and a config with a
    // stale thinking.effort = "max"
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let config_path = temp_dir.path().join("config.toml");
    std::fs::write(&config_path, "[thinking]\neffort = \"max\"\n")
        .expect("write config.toml should succeed");
    let marker_path = temp_dir.path().join(MIGRATIONS_MARKER_FILE);
    std::fs::write(&marker_path, "{ this is not valid json")
        .expect("write corrupt marker should succeed");
    let mut config = ServerConfig::load_without_env(&config_path, &LocalSyncFs)
        .expect("config.toml should parse");

    // When running the config migration
    migrate_config(&mut config, &config_path, temp_dir.path(), &LocalSyncFs)
        .expect("migration should succeed");

    // Then the corrupt marker is treated as "no migrations applied": the
    // migration runs, the effort is rewritten to "high", and the marker
    // file is replaced with valid JSON recording the migration
    assert_eq!(
        config
            .thinking
            .as_ref()
            .expect("thinking should parse")
            .effort
            .as_deref(),
        Some("high")
    );
    let markers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&marker_path).expect("marker file should be rewritten"),
    )
    .expect("marker file should be valid JSON after the migration");
    assert!(
        markers.get(MIGRATION_THINKING_EFFORT_MAX_TO_HIGH).is_some(),
        "marker file should record the applied migration"
    );
}

#[test]
fn should_not_write_marker_when_config_rewrite_fails() {
    // Given a config with a stale effort = "max" whose config.toml path
    // has a regular file as its parent directory, so writing the
    // rewritten config is guaranteed to fail
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let blocked_parent = temp_dir.path().join("not-a-directory");
    std::fs::write(&blocked_parent, "i am a file, not a directory")
        .expect("write blocker file should succeed");
    let config_path = blocked_parent.join("config.toml");
    let mut config =
        ServerConfig::from_toml("[thinking]\neffort = \"max\"\n").expect("TOML should parse");

    // When running the config migration
    migrate_config(&mut config, &config_path, temp_dir.path(), &LocalSyncFs)
        .expect_err("a failed config.toml rewrite must surface as an error");

    // Then the in-memory config keeps the migrated value for this run,
    // but no marker is written, so the migration retries on the next
    // boot
    assert_eq!(
        config
            .thinking
            .as_ref()
            .expect("thinking should parse")
            .effort
            .as_deref(),
        Some("high"),
        "the in-memory config must keep the migrated value"
    );
    assert!(
        !temp_dir.path().join(MIGRATIONS_MARKER_FILE).exists(),
        "no marker may be written when the config.toml rewrite fails"
    );
}

// ===== env var expansion tests =====

#[test]
fn should_expand_var_reference_when_string_contains_dollar_brace_var() {
    let result = expand_env_string_with("key-${MY_VAR}-suffix", &|name| {
        if name == "MY_VAR" {
            Some("value".to_string())
        } else {
            None
        }
    });
    assert_eq!(result, "key-value-suffix");
}

#[test]
fn should_expand_env_prefix_var_when_string_contains_env_colon_var() {
    let result = expand_env_string_with("prefix-${env:MY_VAR}", &|name| {
        if name == "MY_VAR" {
            Some("expanded".to_string())
        } else {
            None
        }
    });
    assert_eq!(result, "prefix-expanded");
}

#[test]
fn should_return_empty_string_when_var_is_undefined() {
    let result = expand_env_string_with("key-${MISSING}-end", &|_| None);
    assert_eq!(result, "key--end");
}

#[test]
fn should_leave_string_unchanged_when_no_var_references_present() {
    let result = expand_env_string_with("plain-string", &|_| None);
    assert_eq!(result, "plain-string");
}

#[test]
fn should_expand_multiple_var_references_when_string_contains_multiple_references() {
    let result = expand_env_string_with("${A}/${B}", &|name| match name {
        "A" => Some("first".to_string()),
        "B" => Some("second".to_string()),
        _ => None,
    });
    assert_eq!(result, "first/second");
}

#[test]
fn should_expand_provider_api_key_when_config_has_var_reference() {
    let mut config = ServerConfig::from_toml(
        r#"
[provider]
api_key = "sk-${MY_KEY}"
base_url = "https://${HOST}/v1"
"#,
    )
    .expect("TOML should parse");

    config.expand_env_vars_with(&|name| match name {
        "MY_KEY" => Some("secret123".to_string()),
        "HOST" => Some("api.example.com".to_string()),
        _ => None,
    });

    let provider = config.provider.expect("provider should exist");
    assert_eq!(provider.api_key.as_deref(), Some("sk-secret123"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://api.example.com/v1")
    );
}

#[test]
fn should_expand_var_in_providers_map_entries_when_config_has_var_references() {
    // Given a `[providers.<id>]` entry with ${...} references in every
    // string field, including nested value fields
    let mut config = ServerConfig::from_toml(
        r#"
[providers.claude]
type = "${TYPE}"
api_key = "sk-${KEY}"
baseUrl = "https://${HOST}/v1"
defaultModel = "${DEFAULT_MODEL}"
removedModelIds = ["${REMOVED}"]
env = { EXTRA = "${ENV_VALUE}" }
source = { script = "${SCRIPT}", args = ["--token", "${TOKEN}"] }
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "TYPE" => Some("anthropic".to_string()),
        "KEY" => Some("secret123".to_string()),
        "HOST" => Some("api.example.com".to_string()),
        "DEFAULT_MODEL" => Some("sonnet".to_string()),
        "REMOVED" => Some("gpt-3.5-turbo".to_string()),
        "ENV_VALUE" => Some("extra".to_string()),
        "SCRIPT" => Some("/usr/local/bin/fetch".to_string()),
        "TOKEN" => Some("tok-42".to_string()),
        _ => None,
    });

    // Then every string value in the provider entry is expanded, not just
    // api_key
    let provider = &config.providers.expect("providers should exist")["claude"];
    assert_eq!(provider.r#type.as_deref(), Some("anthropic"));
    assert_eq!(provider.api_key.as_deref(), Some("sk-secret123"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://api.example.com/v1")
    );
    assert_eq!(provider.default_model.as_deref(), Some("sonnet"));
    assert_eq!(
        provider.removed_model_ids,
        Some(vec!["gpt-3.5-turbo".to_string()])
    );
    assert_eq!(
        provider
            .env
            .as_ref()
            .expect("env should exist")
            .get("EXTRA"),
        Some(&"extra".to_string())
    );
    let source = provider.source.as_ref().expect("source should exist");
    assert_eq!(source["script"], "/usr/local/bin/fetch");
    assert_eq!(source["args"][1], "tok-42");
}

#[test]
fn should_expand_var_in_models_section_when_config_has_var_references() {
    // Given a `[models.<alias>]` entry with ${...} references in every
    // string field
    let mut config = ServerConfig::from_toml(
        r#"
[models.sonnet]
provider = "${PROVIDER_ID}"
model = "${MODEL_ID}"
max_context_size = 200000
maxOutputSize = 64000
displayName = "${DISPLAY_NAME}"
description = "desc-${DESC}"
reasoningKey = "${REASONING_KEY}"
defaultEffort = "${EFFORT}"
protocol = "${PROTOCOL}"
capabilities = ["${CAP_1}", "${CAP_2}"]
supportEfforts = ["${EFFORT_LOW}", "${EFFORT_HIGH}"]
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "PROVIDER_ID" => Some("claude".to_string()),
        "MODEL_ID" => Some("claude-sonnet-4-20250514".to_string()),
        "DISPLAY_NAME" => Some("Sonnet".to_string()),
        "DESC" => Some("fast".to_string()),
        "REASONING_KEY" => Some("reasoning".to_string()),
        "EFFORT" => Some("high".to_string()),
        "PROTOCOL" => Some("anthropic".to_string()),
        "CAP_1" => Some("text".to_string()),
        "CAP_2" => Some("vision".to_string()),
        "EFFORT_LOW" => Some("low".to_string()),
        "EFFORT_HIGH" => Some("high".to_string()),
        _ => None,
    });

    // Then every string value in the model alias is expanded
    let alias = &config.models.expect("models should exist")["sonnet"];
    assert_eq!(alias.provider, "claude");
    assert_eq!(alias.model, "claude-sonnet-4-20250514");
    assert_eq!(alias.display_name.as_deref(), Some("Sonnet"));
    assert_eq!(alias.description.as_deref(), Some("desc-fast"));
    assert_eq!(alias.reasoning_key.as_deref(), Some("reasoning"));
    assert_eq!(alias.default_effort.as_deref(), Some("high"));
    assert_eq!(alias.protocol.as_deref(), Some("anthropic"));
    assert_eq!(
        alias.capabilities,
        Some(vec!["text".to_string(), "vision".to_string()])
    );
    assert_eq!(
        alias.support_efforts,
        Some(vec!["low".to_string(), "high".to_string()])
    );
}

#[test]
fn should_expand_var_in_custom_headers_when_config_has_var_reference() {
    // Given a provider entry with `${...}` references in custom headers
    let mut config = ServerConfig::from_toml(
        r#"
[providers.myprovider]
api_key = "sk-${KEY}"
customHeaders = { "X-API-Key" = "v1-${HEADER_VAL}", "X-Token" = "${TOKEN}" }
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "KEY" => Some("secret123".to_string()),
        "HEADER_VAL" => Some("abc".to_string()),
        "TOKEN" => Some("tok-42".to_string()),
        _ => None,
    });

    // Then custom header values are expanded alongside api_key
    let provider = &config.providers.expect("providers should exist")["myprovider"];
    assert_eq!(provider.api_key.as_deref(), Some("sk-secret123"));
    let headers = provider
        .custom_headers
        .as_ref()
        .expect("headers should exist");
    assert_eq!(headers.get("X-API-Key").unwrap(), "v1-abc");
    assert_eq!(headers.get("X-Token").unwrap(), "tok-42");
}

#[test]
fn should_expand_var_recursively_in_nested_objects_when_passthrough_sections_have_var_references() {
    // Given passthrough TOML sections with ${...} references nested in
    // tables, arrays, and inline tables
    let mut config = ServerConfig::from_toml(
        r#"
[services.command]
command = "${BIN_PATH}"
args = ["--config", "${CFG_PATH}"]
nested = { level = "${LEVEL}", tags = ["${TAG_A}", "${TAG_B}"] }

[background]
cwd = "${CWD}"
env = { VAR = "${BG_VAR}" }

[image]
url = "${IMG_URL}"

[model_catalog]
persist_cache = false

[experimental]
streaming = true
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "BIN_PATH" => Some("/usr/bin/tool".to_string()),
        "CFG_PATH" => Some("/etc/tool.toml".to_string()),
        "LEVEL" => Some("debug".to_string()),
        "TAG_A" => Some("alpha".to_string()),
        "TAG_B" => Some("beta".to_string()),
        "CWD" => Some("/work".to_string()),
        "BG_VAR" => Some("bg-value".to_string()),
        "IMG_URL" => Some("https://img.example.com/a.png".to_string()),
        _ => None,
    });

    // Then nested string values in every passthrough section are expanded
    let services = config.services.as_ref().expect("services should exist");
    assert_eq!(
        services["command"]["command"].as_str(),
        Some("/usr/bin/tool")
    );
    assert_eq!(
        services["command"]["args"][1].as_str(),
        Some("/etc/tool.toml")
    );
    assert_eq!(
        services["command"]["nested"]["level"].as_str(),
        Some("debug")
    );
    assert_eq!(
        services["command"]["nested"]["tags"][1].as_str(),
        Some("beta")
    );

    let background = config.background.as_ref().expect("background should exist");
    assert_eq!(background["cwd"].as_str(), Some("/work"));
    assert_eq!(background["env"]["VAR"].as_str(), Some("bg-value"));

    let image = config.image.as_ref().expect("image should exist");
    assert_eq!(image["url"].as_str(), Some("https://img.example.com/a.png"));

    // The typed [model_catalog] section has no string values to expand;
    // its boolean flag is intact
    assert!(!config.persist_cache_enabled());

    // The boolean-only experimental section is untouched
    assert_eq!(
        config.experimental,
        Some(HashMap::from([("streaming".to_string(), true)]))
    );
}

#[test]
fn should_expand_var_in_hooks_command_when_config_has_var_reference() {
    // Given a hook whose command, pattern, and env hold ${...} references
    let mut config = ServerConfig::from_toml(
        r#"
[[hooks]]
event = "PreToolUse"
command = "echo ${HOOK_MSG}"
pattern = "${HOOK_PATTERN}"
env = { EXTRA = "${HOOK_ENV}" }
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "HOOK_MSG" => Some("hello".to_string()),
        "HOOK_PATTERN" => Some("Bash*".to_string()),
        "HOOK_ENV" => Some("env-value".to_string()),
        _ => None,
    });

    // Then the hook's string fields are expanded
    let hooks = config.hooks.as_ref().expect("hooks should exist");
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].command, "echo hello");
    assert_eq!(hooks[0].pattern.as_deref(), Some("Bash*"));
    assert_eq!(
        hooks[0]
            .env
            .as_ref()
            .expect("env should exist")
            .get("EXTRA"),
        Some(&"env-value".to_string())
    );
}

#[test]
fn should_support_env_colon_syntax_in_all_sections_when_config_uses_env_prefix() {
    // Given a config using the `${env:VAR}` spelling across several
    // sections
    let mut config = ServerConfig::from_toml(
        r#"
[provider]
api_key = "sk-${env:API_KEY}"

[providers.claude]
api_key = "${env:API_KEY}"
customHeaders = { "X-Token" = "${env:TOKEN}" }

[models.sonnet]
provider = "claude"
model = "${env:MODEL_ID}"
max_context_size = 200000

[[hooks]]
event = "PreToolUse"
command = "run ${env:TOOL}"

[services.host]
url = "${env:HOST_URL}"
"#,
    )
    .expect("TOML should parse");

    // When expanding env vars with a lookup for every referenced name
    config.expand_env_vars_with(&|name| match name {
        "API_KEY" => Some("sk-env-key".to_string()),
        "MODEL_ID" => Some("env-model".to_string()),
        "TOKEN" => Some("tok-env".to_string()),
        "TOOL" => Some("tool-env".to_string()),
        "HOST_URL" => Some("https://host.example.com".to_string()),
        _ => None,
    });

    // Then ${env:...} references are expanded in every section
    assert_eq!(
        config.provider.as_ref().unwrap().api_key.as_deref(),
        Some("sk-sk-env-key")
    );
    let provider = &config.providers.expect("providers should exist")["claude"];
    assert_eq!(provider.api_key.as_deref(), Some("sk-env-key"));
    assert_eq!(
        provider
            .custom_headers
            .as_ref()
            .expect("headers should exist")
            .get("X-Token"),
        Some(&"tok-env".to_string())
    );
    let alias = &config.models.expect("models should exist")["sonnet"];
    assert_eq!(alias.model, "env-model");
    let hooks = config.hooks.as_ref().expect("hooks should exist");
    assert_eq!(hooks[0].command, "run tool-env");
    let services = config.services.as_ref().expect("services should exist");
    assert_eq!(
        services["host"]["url"].as_str(),
        Some("https://host.example.com")
    );
}

#[test]
fn should_not_expand_when_load_without_env_is_used() {
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let config_path = temp_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[provider]
api_key = "sk-${MY_KEY}"
"#,
    )
    .expect("write should succeed");

    let config = ServerConfig::load_without_env(&config_path, &LocalSyncFs).expect("should load");
    let provider = config.provider.expect("provider should exist");
    // The raw ${MY_KEY} is preserved — no expansion in load_without_env
    assert_eq!(provider.api_key.as_deref(), Some("sk-${MY_KEY}"));
}

// ===== mirri home dir resolution tests =====

fn fake_os_env<'a>(
    entries: &'a [(&'a str, &'a str)],
) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
    move |name| {
        entries
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| std::ffi::OsString::from(*value))
    }
}

#[test]
fn should_use_mirricode_home_when_mirricode_home_env_is_set() {
    // Given MIRRICODE_HOME is set to a custom directory and HOME is also
    // set
    let env = fake_os_env(&[("MIRRICODE_HOME", "/custom/mirri"), ("HOME", "/home/user")]);

    // When resolving the mirri home dir
    let dir = resolve_mirri_home_dir_from(env).expect("resolution should succeed");

    // Then MIRRICODE_HOME wins over the HOME-based default
    assert_eq!(dir, PathBuf::from("/custom/mirri"));
}

#[test]
fn should_fall_back_to_home_when_mirricode_home_env_is_not_set() {
    // Given MIRRICODE_HOME is absent and HOME is set
    let env = fake_os_env(&[("HOME", "/home/user")]);

    // When resolving the mirri home dir
    let dir = resolve_mirri_home_dir_from(env).expect("resolution should succeed");

    // Then the dir is $HOME/.mirri-code
    assert_eq!(dir, PathBuf::from("/home/user/.mirri-code"));
}

#[test]
fn should_fall_back_to_home_when_mirricode_home_env_is_empty() {
    // Given MIRRICODE_HOME is set but empty (treated as unset)
    let env = fake_os_env(&[("MIRRICODE_HOME", ""), ("HOME", "/home/user")]);

    // When resolving the mirri home dir
    let dir = resolve_mirri_home_dir_from(env).expect("resolution should succeed");

    // Then the dir is $HOME/.mirri-code
    assert_eq!(dir, PathBuf::from("/home/user/.mirri-code"));
}

#[test]
fn should_return_error_when_home_env_is_not_set_and_mirricode_home_is_absent() {
    // Given neither MIRRICODE_HOME nor HOME is set
    let env = fake_os_env(&[]);

    // When resolving the mirri home dir
    let error = resolve_mirri_home_dir_from(env).expect_err("resolution should fail");

    // Then the error names the missing HOME variable
    assert!(
        error.to_string().contains("HOME is not set"),
        "unexpected error: {error}"
    );
}

// ===== MirriConfig wire shape tests =====

#[test]
fn should_build_camel_case_wire_config_when_multi_provider_fields_present() {
    // Given a config with explicit multi-provider maps
    let mut providers = HashMap::new();
    providers.insert(
        "my-provider".to_string(),
        ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("sk-123".to_string()),
            base_url: Some("https://api.example.com".to_string()),
            default_model: Some("claude-sonnet-4-20250514".to_string()),
            removed_model_ids: Some(vec!["claude-3-5-sonnet".to_string()]),
            ..Default::default()
        },
    );
    let mut models = HashMap::new();
    models.insert(
        "my-model".to_string(),
        ModelAlias {
            provider: "my-provider".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            max_context_size: 200000,
            max_output_size: Some(64000),
            capabilities: Some(vec!["text".to_string(), "images".to_string()]),
            display_name: Some("Sonnet".to_string()),
            description: Some("The balanced model".to_string()),
            reasoning_key: Some("reasoning".to_string()),
            adaptive_thinking: Some(true),
            support_efforts: Some(vec!["low".to_string(), "high".to_string()]),
            default_effort: Some("high".to_string()),
            protocol: None,
            beta_api: None,
        },
    );
    let config = ServerConfig {
        providers: Some(providers),
        models: Some(models),
        default_provider: Some("my-provider".to_string()),
        default_model: Some("my-model".to_string()),
        ..Default::default()
    };

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the multi-provider fields are serialized with camelCase keys
    assert_eq!(wire["defaultProvider"], "my-provider");
    assert_eq!(wire["defaultModel"], "my-model");
    let provider = &wire["providers"]["my-provider"];
    assert_eq!(provider["type"], "anthropic");
    assert_eq!(provider["apiKey"], "sk-123");
    assert_eq!(provider["baseUrl"], "https://api.example.com");
    assert_eq!(provider["defaultModel"], "claude-sonnet-4-20250514");
    assert_eq!(provider["removedModelIds"], json!(["claude-3-5-sonnet"]));
    let model = &wire["models"]["my-model"];
    assert_eq!(model["provider"], "my-provider");
    assert_eq!(model["model"], "claude-sonnet-4-20250514");
    assert_eq!(model["maxContextSize"], 200000);
    assert_eq!(model["maxOutputSize"], 64000);
    assert_eq!(model["capabilities"], json!(["text", "images"]));
    assert_eq!(model["displayName"], "Sonnet");
    assert_eq!(model["description"], "The balanced model");
    assert_eq!(model["reasoningKey"], "reasoning");
    assert_eq!(model["adaptiveThinking"], true);
    assert_eq!(model["supportEfforts"], json!(["low", "high"]));
    assert_eq!(model["defaultEffort"], "high");
}

#[test]
fn should_redact_provider_api_keys_when_serializing_config_wire() {
    // Given a config wire whose provider carries an api key, and whose
    // raw TOML document carries the same key (plus the legacy singular
    // [provider] table) in the on-disk spelling
    let mut config = ServerConfig::default();
    let mut providers = HashMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("sk-secret".to_string()),
            ..Default::default()
        },
    );
    config.providers = Some(providers);
    let mut raw = toml::map::Map::new();
    let mut raw_providers = toml::map::Map::new();
    let mut raw_anthropic = toml::map::Map::new();
    raw_anthropic.insert("type".to_string(), "anthropic".into());
    raw_anthropic.insert("api_key".to_string(), "sk-raw-secret".into());
    raw_providers.insert("anthropic".to_string(), toml::Value::Table(raw_anthropic));
    let mut raw_legacy = toml::map::Map::new();
    raw_legacy.insert("api_key".to_string(), "sk-legacy-raw".into());
    raw.insert("providers".to_string(), toml::Value::Table(raw_providers));
    raw.insert("provider".to_string(), toml::Value::Table(raw_legacy));
    config.raw = Some(toml::Value::Table(raw));

    // When the wire is redacted for a client
    let redacted = redact_config_wire(config.to_mirri_config_wire());

    // Then the provider's api key reads as the placeholder in every
    // location it can appear — the structured providers map, the raw
    // document's providers map, and the raw legacy provider table —
    // while every non-secret field passes through untouched
    assert_eq!(redacted["providers"]["anthropic"]["apiKey"], "[REDACTED]");
    assert_eq!(redacted["providers"]["anthropic"]["type"], "anthropic");
    assert_eq!(
        redacted["raw"]["providers"]["anthropic"]["api_key"], "[REDACTED]",
        "the raw document's providers must be redacted too"
    );
    assert_eq!(
        redacted["raw"]["provider"]["api_key"], "[REDACTED]",
        "the raw legacy [provider] table must be redacted too"
    );
    assert!(redacted.get("defaultProvider").is_none());
}

#[test]
fn should_map_legacy_provider_and_model_when_multi_provider_fields_absent() {
    // Given a legacy config with [provider] and [model] sections
    let config = ServerConfig {
        provider: Some(ProviderConfig {
            api_key: Some("sk-legacy".to_string()),
            base_url: Some("https://legacy.example.com".to_string()),
        }),
        model: Some(ModelConfig {
            id: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
        }),
        ..Default::default()
    };

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the legacy sections map onto the `default` provider and the
    // model-id alias
    assert_eq!(wire["defaultProvider"], "default");
    assert_eq!(wire["defaultModel"], "claude-sonnet-4-20250514");
    assert_eq!(wire["providers"]["default"]["type"], "anthropic");
    assert_eq!(wire["providers"]["default"]["apiKey"], "sk-legacy");
    assert_eq!(
        wire["providers"]["default"]["baseUrl"],
        "https://legacy.example.com"
    );
    let model = &wire["models"]["claude-sonnet-4-20250514"];
    assert_eq!(model["provider"], "default");
    assert_eq!(model["model"], "claude-sonnet-4-20250514");
    assert_eq!(model["maxContextSize"], 200000);
}

#[test]
fn should_default_legacy_provider_type_and_model_provider_when_absent() {
    // Given a legacy config whose [model] section omits the provider
    let config = ServerConfig {
        provider: Some(ProviderConfig {
            api_key: Some("sk-legacy".to_string()),
            base_url: None,
        }),
        model: Some(ModelConfig {
            id: "gpt-4o".to_string(),
            provider: None,
        }),
        ..Default::default()
    };

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the provider type defaults to "openai" and the model alias
    // points at the provider named "default"
    assert_eq!(wire["providers"]["default"]["type"], "openai");
    assert_eq!(wire["models"]["gpt-4o"]["provider"], "default");
}

#[test]
fn should_omit_absent_fields_when_config_is_default() {
    // Given an empty (default) config
    let config = ServerConfig::default();

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then only the providers map (always present, even if empty) is
    // included — all absent optional fields are omitted, not null,
    // because TS zod `.optional()` rejects `null`.
    let expected = serde_json::json!({
        "providers": {},
    });
    assert_eq!(wire, expected);
    let wire_object = wire.as_object().expect("wire config must be an object");
    for core_info_field in ["version", "serverId", "engine", "startedAt"] {
        assert!(
            !wire_object.contains_key(core_info_field),
            "core.info field '{core_info_field}' must not leak into config.get"
        );
    }
}

#[test]
fn should_build_wire_thinking_permission_loop_control_hooks_when_present() {
    // Given a config with thinking, permission, loop_control, hooks, and
    // experimental sections
    let config = ServerConfig::from_toml(
        r#"
[thinking]
enabled = true
effort = "high"
keep = "all"

[permission]
mode = "yolo"

[[permission.rules]]
decision = "allow"
scope = "user"
pattern = "Bash*"

[loop_control]
max_steps_per_turn = 30
max_retries_per_step = 3
max_ralph_iterations = 4
reserved_context_size = 8192
compaction_trigger_ratio = 0.75
retention_tail_tokens = 5000
max_overflow_compaction_attempts = 1

[[hooks]]
event = "PreToolUse"
command = "echo before"
pattern = "Bash*"

[experimental]
streaming = true
"#,
    )
    .expect("TOML should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the sections serialize with camelCase keys
    let thinking = &wire["thinking"];
    assert_eq!(thinking["enabled"], true);
    assert_eq!(thinking["effort"], "high");
    assert_eq!(thinking["keep"], "all");
    let rule = &wire["permission"]["rules"][0];
    assert_eq!(rule["decision"], "allow");
    assert_eq!(rule["scope"], "user");
    assert_eq!(rule["pattern"], "Bash*");
    let loop_control = &wire["loopControl"];
    assert_eq!(loop_control["maxStepsPerTurn"], 30);
    assert_eq!(loop_control["maxRetriesPerStep"], 3);
    assert_eq!(loop_control["maxRalphIterations"], 4);
    assert_eq!(loop_control["reservedContextSize"], 8192);
    assert_eq!(loop_control["compactionTriggerRatio"], 0.75);
    assert_eq!(loop_control["retentionTailTokens"], 5000);
    assert_eq!(loop_control["maxOverflowCompactionAttempts"], 1);
    assert!(
        !loop_control
            .as_object()
            .unwrap()
            .contains_key("retentionTailRatio"),
        "retentionTailRatio should be omitted when unset (and cannot co-exist with tokens)"
    );
    assert_eq!(wire["hooks"][0]["event"], "PreToolUse");
    assert_eq!(wire["hooks"][0]["matcher"], "Bash*");
    assert!(
        wire["hooks"][0].get("timeout").is_some(),
        "hooks should use 'timeout' (seconds) not 'timeout_ms'"
    );
    assert!(
        !wire["hooks"][0]
            .as_object()
            .unwrap()
            .contains_key("failClosed"),
        "failClosed should be omitted when false (absent = false in TS wire)"
    );
    assert_eq!(wire["experimental"]["streaming"], true);
}

#[test]
fn should_expose_retention_tail_ratio_on_wire_when_configured() {
    // Given a config whose [loop_control] uses the ratio spelling (the
    // tokens spelling is covered by the camel-case sections test — the two
    // are mutually exclusive, US6.2, so each gets its own config)
    let config = ServerConfig::from_toml("[loop_control]\nretention_tail_ratio = 0.2")
        .expect("ratio-only retention config should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the ratio appears under its camelCase key and the absent
    // absolute spelling is omitted
    let loop_control = &wire["loopControl"];
    assert_eq!(loop_control["retentionTailRatio"], 0.2);
    assert!(
        !loop_control
            .as_object()
            .unwrap()
            .contains_key("retentionTailTokens"),
        "retentionTailTokens should be omitted when unset"
    );
    assert!(
        !loop_control
            .as_object()
            .unwrap()
            .contains_key("maxOverflowCompactionAttempts"),
        "maxOverflowCompactionAttempts should be omitted when unset"
    );
}

#[test]
fn should_expose_capacity_keys_on_wire_when_configured() {
    // Given a config whose [loop_control] sets the capacity keys
    let config = ServerConfig::from_toml(
        "[loop_control]\nmax_history_messages = 4\nmax_deferred_messages = 2",
    )
    .expect("capacity config should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then both keys serialize under their camelCase wire spellings
    let loop_control = &wire["loopControl"];
    assert_eq!(loop_control["maxHistoryMessages"], 4);
    assert_eq!(loop_control["maxDeferredMessages"], 2);

    // And a config without them omits both (absent fields are omitted)
    let bare = ServerConfig::from_toml("[loop_control]\nmax_steps_per_turn = 30")
        .expect("bare config should parse");
    let bare_wire = bare.to_mirri_config_wire();
    let bare_loop_control = &bare_wire["loopControl"];
    assert!(
        !bare_loop_control
            .as_object()
            .unwrap()
            .contains_key("maxHistoryMessages"),
        "maxHistoryMessages should be omitted when unset"
    );
    assert!(
        !bare_loop_control
            .as_object()
            .unwrap()
            .contains_key("maxDeferredMessages"),
        "maxDeferredMessages should be omitted when unset"
    );
}

#[test]
fn should_convert_passthrough_toml_sections_when_from_toml() {
    // Given a config with passthrough TOML sections
    let config = ServerConfig::from_toml(
        r#"
plan_mode = true
yolo = true
default_permission_mode = "yolo"
default_plan_mode = false
merge_all_available_skills = true
extra_skill_dirs = ["skills"]
extra_agent_dirs = ["agents"]
disabled_agents = ["architect"]
disabled_skills = ["old-skill"]
telemetry = false

[services.mcp]
command = "npx"

[background]
max_running_tasks = 2

[subagent]
timeout_ms = 60000

[image]
enabled = false

[model_catalog]
persist_cache = false
"#,
    )
    .expect("TOML should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the passthrough scalars carry their camelCase keys, the
    // background/subagent sections are typed camelCase (D-65: the
    // BackgroundConfigSchema / SubagentConfigSchema spellings), and the
    // typed model_catalog section emits its bool flag
    assert_eq!(wire["planMode"], true);
    assert_eq!(wire["yolo"], true);
    assert_eq!(wire["defaultPermissionMode"], "yolo");
    assert_eq!(wire["defaultPlanMode"], false);
    assert_eq!(wire["mergeAllAvailableSkills"], true);
    assert_eq!(wire["extraSkillDirs"], json!(["skills"]));
    assert_eq!(wire["extraAgentDirs"], json!(["agents"]));
    assert_eq!(wire["disabledAgents"], json!(["architect"]));
    assert_eq!(wire["disabledSkills"], json!(["old-skill"]));
    assert_eq!(wire["telemetry"], false);
    assert_eq!(wire["services"]["mcp"]["command"], "npx");
    assert_eq!(wire["background"]["maxRunningTasks"], 2);
    assert_eq!(wire["subagent"]["timeoutMs"], 60000);
    assert_eq!(wire["image"]["enabled"], false);
    assert_eq!(wire["modelCatalog"]["persistCache"], false);
}

/// should round-trip the retired `disabled_agents` key through a save
/// (R3-3: the key is inert data, but old config.toml files keep working)
#[test]
fn should_round_trip_the_retired_disabled_agents_key_when_config_saved_and_reloaded() {
    // Given a config still carrying the retired key
    let config = ServerConfig::from_toml("disabled_agents = [\"architect\"]\n")
        .expect("the retired key must still parse (compatibility)");

    // When the config is saved and reloaded (the config.set write path)
    let contents = config.to_toml_string().unwrap();
    let reloaded = ServerConfig::from_toml(&contents).unwrap();

    // Then the key survives verbatim — parsed, preserved, and exposed on
    // the wire, even though the engine no longer consults it
    assert_eq!(
        reloaded.disabled_agents,
        Some(vec!["architect".to_string()])
    );
    assert_eq!(
        reloaded.to_mirri_config_wire()["disabledAgents"],
        json!(["architect"])
    );
}

/// should keep array values on a single line when the config is saved
/// (formatting parity with the TypeScript smol-toml writer, which also
/// inlines arrays — a save from either engine must not churn unrelated
/// lines in config.toml)
#[test]
fn should_keep_array_values_on_a_single_line_when_config_saved() {
    // Given a config whose top-level and per-entry fields are arrays
    let config = ServerConfig::from_toml(
        r#"
extra_skill_dirs = ["skills-a", "skills-b"]

[models."a/b"]
provider = "a"
model = "b"
max_context_size = 200000
capabilities = ["tool-use", "vision"]
"#,
    )
    .unwrap();

    // When the config is serialized for persistence
    let contents = config.to_toml_string().unwrap();

    // Then every array renders inline, one line per key
    for line in contents.lines() {
        if line.contains('[') && !line.trim_start().starts_with("[[") {
            assert!(
                line.trim_end().ends_with(']'),
                "array must close on the same line it opens: {contents}"
            );
        }
    }
    assert!(contents.contains(r#"extra_skill_dirs = ["skills-a", "skills-b"]"#));
    assert!(contents.contains(r#"capabilities = ["tool-use", "vision"]"#));
}

#[test]
fn should_default_persist_cache_to_true_when_model_catalog_section_is_absent() {
    // Given a config with a model alias but no [model_catalog] section
    let config = parse_config_toml(
        "[models.\"a/b\"]\nprovider=\"a\"\nmodel=\"b\"\nmax_context_size = 200000\n",
    );

    // When reading the effective persist_cache flag
    // Then it defaults to enabled (D-M4)
    assert!(config.persist_cache_enabled());
}

#[test]
fn should_default_persist_cache_to_true_when_the_struct_default_is_used() {
    // Given the typed [model_catalog] section struct
    // When its Rust-level Default is constructed
    let catalog = ModelCatalogConfig::default();

    // Then the persist_cache flag defaults to enabled (D-M4), matching the
    // serde default so `..Default::default()` construction stays consistent
    assert!(catalog.persist_cache);
}

#[test]
fn should_read_persist_cache_false_when_model_catalog_disables_it() {
    // Given a config whose [model_catalog] section disables the cache
    let config = parse_config_toml("[model_catalog]\npersist_cache = false\n");

    // When reading the effective flag and the wire projection
    assert!(!config.persist_cache_enabled());
    // Then the wire emits the typed field as modelCatalog.persistCache == false
    assert_eq!(
        config.to_mirri_config_wire()["modelCatalog"]["persistCache"],
        false
    );
}

#[test]
fn should_merge_model_catalog_patch_when_config_set_accepts_model_catalog() {
    // Given a config with the model catalog cache left at its default
    let mut config = parse_config_toml(
        "[models.\"a/b\"]\nprovider=\"a\"\nmodel=\"b\"\nmax_context_size = 200000\n",
    );
    assert!(config.persist_cache_enabled());

    // When a config.set patch disables the cache through the camelCase wire key
    config
        .merge_wire_patch(&json!({ "modelCatalog": { "persistCache": false } }))
        .expect("modelCatalog patch should be accepted");

    // Then the typed flag is applied and the wire reflects it
    assert!(!config.persist_cache_enabled());
    assert_eq!(
        config.to_mirri_config_wire()["modelCatalog"]["persistCache"],
        false
    );
}

#[test]
fn should_parse_multi_provider_sections_when_from_toml() {
    // Given a TOML config with [providers.<id>] and [models.<alias>]
    // tables in both snake_case and TS camelCase spellings
    let config = ServerConfig::from_toml(
        r#"
default_provider = "claude"
default_model = "sonnet"

[providers.claude]
type = "anthropic"
api_key = "sk-123"

[providers.openai]
type = "openai"
baseUrl = "https://api.openai.com/v1"
removedModelIds = ["gpt-3.5-turbo"]

[models.sonnet]
provider = "claude"
model = "claude-sonnet-4-20250514"
max_context_size = 200000
maxOutputSize = 64000
displayName = "Sonnet"
"#,
    )
    .expect("TOML should parse");

    // When parsing the multi-provider sections
    let providers = config.providers.expect("providers should parse");
    let models = config.models.expect("models should parse");

    // Then both snake_case names and camelCase aliases populate the maps
    assert_eq!(providers["claude"].r#type.as_deref(), Some("anthropic"));
    assert_eq!(providers["claude"].api_key.as_deref(), Some("sk-123"));
    assert_eq!(
        providers["openai"].base_url.as_deref(),
        Some("https://api.openai.com/v1"),
        "the baseUrl alias must map onto base_url"
    );
    assert_eq!(
        providers["openai"].removed_model_ids,
        Some(vec!["gpt-3.5-turbo".to_string()]),
        "the removedModelIds alias must map onto removed_model_ids"
    );
    let sonnet = &models["sonnet"];
    assert_eq!(sonnet.provider, "claude");
    assert_eq!(sonnet.model, "claude-sonnet-4-20250514");
    assert_eq!(sonnet.max_context_size, 200000);
    assert_eq!(sonnet.max_output_size, Some(64000));
    assert_eq!(sonnet.display_name.as_deref(), Some("Sonnet"));
    assert_eq!(config.default_provider.as_deref(), Some("claude"));
    assert_eq!(config.default_model.as_deref(), Some("sonnet"));
}

#[test]
fn should_parse_oauth_field_when_from_toml() {
    // Given a TOML config with a provider that has an `oauth` sub-table
    let config = ServerConfig::from_toml(
        r#"
[providers.myprovider]
type = "anthropic"
api_key = "sk-123"

[providers.myprovider.oauth]
clientId = "test-client-id"
clientSecret = "test-client-secret"
scopes = ["read", "write"]
tokenUrl = "https://auth.example.com/token"
"#,
    )
    .expect("TOML should parse");

    // When parsing the providers section
    let providers = config.providers.expect("providers should parse");
    let entry = &providers["myprovider"];

    // Then the oauth field is parsed as a JSON object with the expected
    // keys and values
    assert_eq!(entry.r#type.as_deref(), Some("anthropic"));
    let oauth = entry.oauth.as_ref().expect("oauth should parse");
    assert_eq!(oauth["clientId"], "test-client-id");
    assert_eq!(oauth["clientSecret"], "test-client-secret");
    assert_eq!(oauth["scopes"], json!(["read", "write"]));
    assert_eq!(oauth["tokenUrl"], "https://auth.example.com/token");
}

#[test]
fn should_parse_env_field_when_from_toml() {
    // Given a TOML config with a provider that has an `env` sub-table
    let config = ServerConfig::from_toml(
        r#"
[providers.myprovider]
type = "openai"

[providers.myprovider.env]
API_URL = "https://api.example.com"
MAX_RETRIES = "3"
DEBUG = "true"
"#,
    )
    .expect("TOML should parse");

    // When parsing the providers section
    let providers = config.providers.expect("providers should parse");
    let entry = &providers["myprovider"];

    // Then the env field is parsed as a string-to-string map with the
    // expected entries
    let env = entry.env.as_ref().expect("env should parse");
    assert_eq!(env.len(), 3);
    assert_eq!(env.get("API_URL").unwrap(), "https://api.example.com");
    assert_eq!(env.get("MAX_RETRIES").unwrap(), "3");
    assert_eq!(env.get("DEBUG").unwrap(), "true");
}

#[test]
fn should_output_oauth_and_env_in_wire_format_when_to_mirri_config_wire() {
    // Given a config with a provider entry that has oauth and env fields
    let mut providers = HashMap::new();
    providers.insert(
        "my-provider".to_string(),
        ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("sk-123".to_string()),
            oauth: Some(json!({
                "clientId": "wire-client-id",
                "clientSecret": "wire-secret",
                "scopes": ["read"]
            })),
            env: Some(HashMap::from([
                ("API_URL".to_string(), "https://api.example.com".to_string()),
                ("TIMEOUT".to_string(), "30".to_string()),
            ])),
            custom_headers: Some(HashMap::from([(
                "X-Custom-Header".to_string(),
                "custom-value".to_string(),
            )])),
            source: Some(json!({ "type": "config", "path": "/etc/config" })),
            ..Default::default()
        },
    );
    let config = ServerConfig {
        providers: Some(providers),
        ..Default::default()
    };

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the provider entry includes oauth, env, customHeaders, and
    // source with their camelCase wire keys
    let provider = &wire["providers"]["my-provider"];
    assert_eq!(provider["type"], "anthropic");
    assert_eq!(provider["apiKey"], "sk-123");
    assert_eq!(provider["oauth"]["clientId"], "wire-client-id");
    assert_eq!(provider["oauth"]["clientSecret"], "wire-secret");
    assert_eq!(provider["oauth"]["scopes"], json!(["read"]));
    assert_eq!(provider["env"]["API_URL"], "https://api.example.com");
    assert_eq!(provider["env"]["TIMEOUT"], "30");
    assert_eq!(provider["customHeaders"]["X-Custom-Header"], "custom-value");
    assert_eq!(provider["source"]["type"], "config");
    assert_eq!(provider["source"]["path"], "/etc/config");
}

#[test]
fn should_roundtrip_provider_entry_with_all_fields_when_from_toml_then_to_wire() {
    // Given a TOML config with a provider entry using all fields,
    // including the camelCase customHeaders alias
    let config = ServerConfig::from_toml(
        r#"
[providers.full]
type = "anthropic"
api_key = "sk-full"
base_url = "https://api.example.com"
default_model = "claude-sonnet-4-20250514"
removed_model_ids = ["claude-3-5-sonnet"]

[providers.full.oauth]
clientId = "roundtrip-client"
scopes = ["read", "write"]

[providers.full.env]
API_URL = "https://api.example.com"
MAX_RETRIES = "5"

[providers.full.customHeaders]
X-Request-Id = "abc-123"
Authorization = "Bearer token"

[providers.full.source]
type = "config"
path = "/etc/mirri/config"
"#,
    )
    .expect("TOML should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then every field round-trips into the camelCase wire shape
    let provider = &wire["providers"]["full"];
    assert_eq!(provider["type"], "anthropic");
    assert_eq!(provider["apiKey"], "sk-full");
    assert_eq!(provider["baseUrl"], "https://api.example.com");
    assert_eq!(provider["defaultModel"], "claude-sonnet-4-20250514");
    assert_eq!(provider["removedModelIds"], json!(["claude-3-5-sonnet"]));
    assert_eq!(provider["oauth"]["clientId"], "roundtrip-client");
    assert_eq!(provider["oauth"]["scopes"], json!(["read", "write"]));
    assert_eq!(provider["env"]["API_URL"], "https://api.example.com");
    assert_eq!(provider["env"]["MAX_RETRIES"], "5");
    assert_eq!(provider["customHeaders"]["X-Request-Id"], "abc-123");
    assert_eq!(provider["customHeaders"]["Authorization"], "Bearer token");
    assert_eq!(provider["source"]["type"], "config");
    assert_eq!(provider["source"]["path"], "/etc/mirri/config");
}

// ===== apply_env_model_config tests =====

#[test]
fn should_synthesize_provider_and_model_when_mirricode_model_name_is_set() {
    // Given an empty config and the two required MIRRICODE_MODEL_* env vars
    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o-mini"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env-key"),
    ]);

    // When applying the env model config
    config
        .apply_env_model_config(&env)
        .expect("apply_env_model_config should succeed");

    // Then a __mirri_env__ provider is synthesized with the api_key,
    // default provider type "openai", and the default openai base_url
    let providers = config.providers.expect("providers should be created");
    let provider = providers
        .get(ENV_MODEL_PROVIDER_KEY)
        .expect("env provider should exist");
    assert_eq!(provider.r#type.as_deref(), Some("openai"));
    assert_eq!(provider.api_key.as_deref(), Some("sk-env-key"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://api.openai.com/v1"),
        "openai provider should default to the OpenAI base URL"
    );

    // And a __mirri_env_model__ alias is synthesized with the model name,
    // default max_context_size (262144), and default capabilities
    let models = config.models.expect("models should be created");
    let alias = models
        .get(ENV_MODEL_ALIAS_KEY)
        .expect("env model alias should exist");
    assert_eq!(alias.provider, ENV_MODEL_PROVIDER_KEY);
    assert_eq!(alias.model, "gpt-4o-mini");
    assert_eq!(
        alias.max_context_size, 262_144,
        "max_context_size should default to 262144"
    );
    assert_eq!(
        alias.capabilities.as_deref(),
        Some(&["image_in".to_string(), "thinking".to_string()][..]),
        "capabilities should default to ['image_in', 'thinking']"
    );
    assert!(
        alias.max_output_size.is_none(),
        "max_output_size should be None when not set"
    );
    assert!(
        alias.display_name.is_none(),
        "display_name should be None when not set"
    );
    assert!(
        alias.reasoning_key.is_none(),
        "reasoning_key should be None when not set"
    );
    assert!(
        alias.adaptive_thinking.is_none(),
        "adaptive_thinking should be None when not set"
    );
}

#[test]
fn should_set_synthesized_model_as_default_when_env_model_config_is_present() {
    // Given a config with an existing default_model and all 11
    // MIRRICODE_MODEL_* env vars set
    let mut config = ServerConfig {
        default_model: Some("original-model".to_string()),
        ..Default::default()
    };
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "claude-sonnet-4-20250514"),
        ("MIRRICODE_MODEL_API_KEY", "sk-test-key"),
        ("MIRRICODE_MODEL_PROVIDER_TYPE", "anthropic"),
        ("MIRRICODE_MODEL_BASE_URL", "https://api.anthropic.com"),
        ("MIRRICODE_MODEL_MAX_CONTEXT_SIZE", "200000"),
        ("MIRRICODE_MODEL_MAX_OUTPUT_SIZE", "64000"),
        ("MIRRICODE_MODEL_CAPABILITIES", "text, vision, thinking"),
        ("MIRRICODE_MODEL_DISPLAY_NAME", "Claude Sonnet"),
        ("MIRRICODE_MODEL_REASONING_KEY", "reasoning"),
        ("MIRRICODE_MODEL_ADAPTIVE_THINKING", "true"),
        ("MIRRICODE_MODEL_THINKING_EFFORT", "high"),
    ]);

    // When applying the env model config
    config
        .apply_env_model_config(&env)
        .expect("apply_env_model_config should succeed");

    // Then default_model is overridden to the env alias
    assert_eq!(
        config.default_model.as_deref(),
        Some(ENV_MODEL_ALIAS_KEY),
        "default_model should be set to the env model alias"
    );

    // And the synthesized provider has all specified fields
    let providers = config.providers.expect("providers should exist");
    let provider = providers
        .get(ENV_MODEL_PROVIDER_KEY)
        .expect("env provider should exist");
    assert_eq!(provider.r#type.as_deref(), Some("anthropic"));
    assert_eq!(provider.api_key.as_deref(), Some("sk-test-key"));
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://api.anthropic.com"),
        "explicit base_url should override the default"
    );

    // And the synthesized model alias has all specified fields
    let models = config.models.expect("models should exist");
    let alias = models
        .get(ENV_MODEL_ALIAS_KEY)
        .expect("env model alias should exist");
    assert_eq!(alias.provider, ENV_MODEL_PROVIDER_KEY);
    assert_eq!(alias.model, "claude-sonnet-4-20250514");
    assert_eq!(alias.max_context_size, 200_000);
    assert_eq!(alias.max_output_size, Some(64_000));
    assert_eq!(
        alias.capabilities.as_deref(),
        Some(
            &[
                "text".to_string(),
                "vision".to_string(),
                "thinking".to_string()
            ][..]
        ),
        "capabilities should be parsed from CSV, lowercased and trimmed"
    );
    assert_eq!(alias.display_name.as_deref(), Some("Claude Sonnet"));
    assert_eq!(alias.reasoning_key.as_deref(), Some("reasoning"));
    assert_eq!(alias.adaptive_thinking, Some(true));

    // And thinking.effort is overridden
    let thinking = config.thinking.as_ref().expect("thinking should exist");
    assert_eq!(thinking.effort.as_deref(), Some("high"));
}

#[test]
fn should_strip_synthesized_entries_when_writing_config_back() {
    // Given a config with env-synthesized provider/alias entries and
    // default_model pointing at the env alias
    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
    ]);
    config.apply_env_model_config(&env).expect("should succeed");

    // Verify the env entries exist before stripping
    assert!(
        config
            .providers
            .as_ref()
            .unwrap()
            .contains_key(ENV_MODEL_PROVIDER_KEY)
    );
    assert!(
        config
            .models
            .as_ref()
            .unwrap()
            .contains_key(ENV_MODEL_ALIAS_KEY)
    );
    assert_eq!(config.default_model.as_deref(), Some(ENV_MODEL_ALIAS_KEY));

    // When stripping env model config (as save_to does)
    config.strip_env_model_config();

    // Then the synthesized provider is removed
    let providers = config.providers.as_ref();
    assert!(
        providers.is_none() || !providers.unwrap().contains_key(ENV_MODEL_PROVIDER_KEY),
        "__mirri_env__ provider must be removed"
    );

    // And the synthesized model alias is removed
    let models = config.models.as_ref();
    assert!(
        models.is_none() || !models.unwrap().contains_key(ENV_MODEL_ALIAS_KEY),
        "__mirri_env_model__ alias must be removed"
    );

    // And default_model is cleared (it pointed at the env alias)
    assert!(
        config.default_model.is_none(),
        "default_model must be cleared when it was the env alias"
    );
}

#[test]
fn should_leave_config_unchanged_when_mirricode_model_name_is_absent() {
    // Given an empty config with no MIRRICODE_MODEL_NAME set
    let mut config = ServerConfig::default();
    let env = fake_env(&[]);

    // When applying the env model config
    config
        .apply_env_model_config(&env)
        .expect("should succeed (no-op)");

    // Then nothing is synthesized
    assert!(config.providers.is_none());
    assert!(config.models.is_none());
    assert!(config.default_model.is_none());
}

#[test]
fn should_return_error_when_mirricode_model_api_key_is_missing() {
    // Given MIRRICODE_MODEL_NAME is set but MIRRICODE_MODEL_API_KEY is not
    let mut config = ServerConfig::default();
    let env = fake_env(&[("MIRRICODE_MODEL_NAME", "gpt-4o")]);

    // When applying the env model config
    let error = config
        .apply_env_model_config(&env)
        .expect_err("should fail when API key is missing");

    // Then an error is returned mentioning the missing API key
    assert!(
        error.to_string().contains("MIRRICODE_MODEL_API_KEY"),
        "error should mention the missing API key: {error}"
    );
}

#[test]
fn should_preserve_existing_providers_and_models_when_env_overlay_is_applied() {
    // Given a config with an existing provider and model alias
    let mut existing_providers = HashMap::new();
    existing_providers.insert(
        "my-provider".to_string(),
        ProviderEntry {
            r#type: Some("anthropic".to_string()),
            api_key: Some("sk-existing".to_string()),
            ..Default::default()
        },
    );
    let mut existing_models = HashMap::new();
    existing_models.insert(
        "my-alias".to_string(),
        ModelAlias {
            provider: "my-provider".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            max_context_size: 200000,
            ..Default::default()
        },
    );
    let mut config = ServerConfig {
        providers: Some(existing_providers),
        models: Some(existing_models),
        default_model: Some("my-alias".to_string()),
        ..Default::default()
    };

    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
    ]);

    // When applying the env model config
    config.apply_env_model_config(&env).expect("should succeed");

    // Then the existing entries are preserved alongside the env entries
    let providers = config.providers.as_ref().expect("providers");
    assert!(
        providers.contains_key("my-provider"),
        "existing provider must be preserved"
    );
    assert!(
        providers.contains_key(ENV_MODEL_PROVIDER_KEY),
        "env provider must be added"
    );

    let models = config.models.as_ref().expect("models");
    assert!(
        models.contains_key("my-alias"),
        "existing model alias must be preserved"
    );
    assert!(
        models.contains_key(ENV_MODEL_ALIAS_KEY),
        "env model alias must be added"
    );

    // And default_model is overridden to the env alias
    assert_eq!(config.default_model.as_deref(), Some(ENV_MODEL_ALIAS_KEY));
}

#[test]
fn should_not_persist_env_entries_when_save_to_is_called() {
    // Given a config with env-synthesized entries saved to a temp file
    let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
    let path = temp_dir.path().join("config.toml");

    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
    ]);
    config.apply_env_model_config(&env).expect("should succeed");

    // When saving the config to disk
    config.save_to(&path).expect("save should succeed");

    // Then the persisted TOML does not contain the reserved keys
    let contents = std::fs::read_to_string(&path).expect("file should be readable");
    assert!(
        !contents.contains("__mirri_env__"),
        "persisted config must not contain __mirri_env__: {contents}"
    );
    assert!(
        !contents.contains("__mirri_env_model__"),
        "persisted config must not contain __mirri_env_model__: {contents}"
    );

    // And re-loading the file produces a clean config without env entries
    let reloaded = ServerConfig::load_without_env(&path, &LocalSyncFs).expect("should parse");
    assert!(
        reloaded.default_model.is_none(),
        "reloaded config must not have a default_model from the env overlay"
    );
}

#[test]
fn should_use_anthropic_provider_type_when_mirricode_model_provider_type_is_anthropic() {
    // Given MIRRICODE_MODEL_PROVIDER_TYPE is set to "anthropic"
    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "claude-sonnet-4-20250514"),
        ("MIRRICODE_MODEL_API_KEY", "sk-anthropic"),
        ("MIRRICODE_MODEL_PROVIDER_TYPE", "Anthropic"),
    ]);

    // When applying the env model config
    config.apply_env_model_config(&env).expect("should succeed");

    // Then the provider type is lowercased and the base_url is None
    // (anthropic uses SDK default, no explicit base URL)
    let provider = config
        .providers
        .as_ref()
        .unwrap()
        .get(ENV_MODEL_PROVIDER_KEY)
        .unwrap();
    assert_eq!(provider.r#type.as_deref(), Some("anthropic"));
    assert!(
        provider.base_url.is_none(),
        "anthropic provider should not have a default base_url"
    );
}

#[test]
fn should_return_error_when_mirricode_model_provider_type_is_invalid() {
    // Given an invalid MIRRICODE_MODEL_PROVIDER_TYPE
    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
        ("MIRRICODE_MODEL_PROVIDER_TYPE", "invalid-type"),
    ]);

    // When applying the env model config
    let error = config
        .apply_env_model_config(&env)
        .expect_err("should fail for invalid provider type");

    // Then an error is returned
    assert!(
        error.to_string().contains("MIRRICODE_MODEL_PROVIDER_TYPE"),
        "error should mention the invalid provider type: {error}"
    );
}

#[test]
fn should_parse_adaptive_thinking_false_when_env_value_is_false() {
    // Given MIRRICODE_MODEL_ADAPTIVE_THINKING is set to "false"
    let mut config = ServerConfig::default();
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
        ("MIRRICODE_MODEL_ADAPTIVE_THINKING", "false"),
    ]);

    // When applying the env model config
    config.apply_env_model_config(&env).expect("should succeed");

    // Then adaptive_thinking is parsed as false
    let alias = config
        .models
        .as_ref()
        .unwrap()
        .get(ENV_MODEL_ALIAS_KEY)
        .unwrap();
    assert_eq!(alias.adaptive_thinking, Some(false));
}

#[test]
fn should_override_thinking_effort_when_mirricode_model_thinking_effort_is_set() {
    // Given a config with an existing thinking section
    let mut config = ServerConfig {
        thinking: Some(ThinkingConfig {
            enabled: Some(true),
            effort: Some("low".to_string()),
            keep: None,
        }),
        ..Default::default()
    };
    let env = fake_env(&[
        ("MIRRICODE_MODEL_NAME", "gpt-4o"),
        ("MIRRICODE_MODEL_API_KEY", "sk-env"),
        ("MIRRICODE_MODEL_THINKING_EFFORT", "max"),
    ]);

    // When applying the env model config
    config.apply_env_model_config(&env).expect("should succeed");

    // Then thinking.effort is overridden but other thinking fields are preserved
    let thinking = config.thinking.as_ref().expect("thinking should exist");
    assert_eq!(thinking.effort.as_deref(), Some("max"));
    assert_eq!(
        thinking.enabled,
        Some(true),
        "existing thinking.enabled must be preserved"
    );
}

// -----------------------------------------------------------------------
// G31 — raw TOML preservation and round-trip fidelity
// -----------------------------------------------------------------------

#[test]
fn should_preserve_raw_toml_when_config_parsed_from_toml() {
    let toml_str = r#"
[model]
id = "claude-sonnet-4-20250514"

[provider]
api_key = "sk-test"

[unknown_section]
custom_field = "preserved"
"#;
    let config = ServerConfig::from_toml(toml_str).expect("TOML should parse");

    // The raw field should contain the original parsed TOML document.
    let raw = config.raw.expect("raw should be set after from_toml");
    assert!(
        raw.get("unknown_section").is_some(),
        "raw should contain unknown keys from the original TOML"
    );
    assert_eq!(
        raw.get("model")
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str()),
        Some("claude-sonnet-4-20250514")
    );
}

#[test]
fn should_include_raw_in_wire_output_when_config_serialized() {
    let config = ServerConfig::from_toml(
        r#"
[model]
id = "gpt-4o"

[custom_section]
custom_key = "hello"
"#,
    )
    .expect("TOML should parse");

    let wire = config.to_mirri_config_wire();

    // The wire output should include a "raw" key.
    let raw = wire
        .get("raw")
        .expect("wire output should include 'raw' field");
    // The raw value should contain the unknown section.
    assert_eq!(
        raw.get("custom_section")
            .and_then(|s| s.get("custom_key"))
            .and_then(|v| v.as_str()),
        Some("hello"),
        "raw in wire output should preserve unknown sections"
    );
}

#[test]
fn should_round_trip_unknown_keys_when_config_saved_and_reloaded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");

    // Write a config with an unknown section.
    let original = r#"
[model]
id = "gpt-4o"

[my_custom_section]
important_value = 42
"#;
    std::fs::write(&path, original).unwrap();

    // Load → save → reload.
    let config = ServerConfig::load_without_env(&path, &LocalSyncFs).expect("should load");
    config.save_to(&path).expect("should save");
    let reloaded = ServerConfig::load_without_env(&path, &LocalSyncFs).expect("should reload");

    // The unknown section should survive the round-trip.
    assert!(reloaded.raw.is_some(), "raw should be set after reload");
    let raw = reloaded.raw.as_ref().unwrap();
    assert_eq!(
        raw.get("my_custom_section")
            .and_then(|s| s.get("important_value"))
            .and_then(|v| v.as_integer()),
        Some(42),
        "unknown section should survive save/load round-trip"
    );

    // The known section should also be preserved.
    assert_eq!(
        reloaded.model.as_ref().expect("model should exist").id,
        "gpt-4o"
    );
}

#[test]
fn should_omit_raw_from_wire_when_config_has_no_raw() {
    // Given a config built from Default (no raw)
    let config = ServerConfig {
        model: Some(ModelConfig {
            id: "gpt-4o".to_string(),
            provider: None,
        }),
        ..Default::default()
    };

    // When building the wire output
    let wire = config.to_mirri_config_wire();

    // Then raw is absent (not null)
    assert!(
        wire.get("raw").is_none(),
        "wire should omit raw when config has no raw"
    );
}

#[test]
fn should_preserve_raw_through_merge_wire_patch() {
    // Given a config parsed from TOML with an unknown section
    let mut config = ServerConfig::from_toml(
        r#"
[model]
id = "gpt-4o"

[my_custom_section]
key = "value"
"#,
    )
    .expect("should parse");
    assert!(config.raw.is_some(), "raw should be set after parsing");

    // When merging a wire patch
    let patch = json!({
        "thinking": {
            "enabled": true,
            "effort": "high"
        }
    });
    config
        .merge_wire_patch(&patch)
        .expect("merge should succeed");

    // Then raw is preserved through the merge
    assert!(
        config.raw.is_some(),
        "raw should be preserved through merge_wire_patch"
    );

    // And the merged thinking field is applied
    assert_eq!(
        config
            .thinking
            .as_ref()
            .expect("thinking")
            .effort
            .as_deref(),
        Some("high")
    );
}

#[test]
fn should_replace_and_clear_sections_when_replace_raw_sections_called() {
    // Given a config parsed from TOML with a known section
    let mut config = ServerConfig::from_toml(
        r#"
[thinking]
effort = "medium"

[providers.kimi]
type = "openai"
"#,
    )
    .unwrap();

    // When a section is replaced, an unmodeled section is added, and a
    // null clears another
    config
        .replace_raw_sections(&[
            (
                "thinking".to_string(),
                Some(serde_json::json!({ "effort": "high" })),
            ),
            (
                "custom".to_string(),
                Some(serde_json::json!({ "flag": true })),
            ),
            ("providers".to_string(), None),
        ])
        .unwrap();

    // Then the parsed struct reflects all three changes
    assert_eq!(
        config.thinking.as_ref().and_then(|t| t.effort.clone()),
        Some("high".into())
    );
    assert!(
        config.providers.is_none(),
        "cleared: {:?}",
        config.providers
    );
    let raw = config.raw.as_ref().unwrap();
    assert!(raw.get("custom").is_some(), "unmodeled section persisted");
    assert!(raw.get("providers").is_none());

    // And an invalid replacement (a section the struct cannot parse)
    // fails and leaves the config untouched
    let before = serde_json::to_value(&config).unwrap();
    let error = config
        .replace_raw_sections(&[(
            "thinking".to_string(),
            Some(serde_json::json!("not-a-table")),
        )])
        .unwrap_err();
    assert!(error.to_string().contains("thinking"), "error: {error}");
    let after = serde_json::to_value(&config).unwrap();
    assert_eq!(before, after, "the config is unchanged after the failure");
}

#[test]
fn should_preserve_unknown_nested_entry_keys_when_config_saved_and_reloaded() {
    // Given a raw document whose models entry carries klient
    // passthrough keys the typed ModelAlias does not model
    let mut config = ServerConfig::from_toml(
        r#"
[models."test/alias"]
provider = "kimi"
model = "m1"
max_context_size = 1000
provider_id = "kimi"
custom_flag = "keep-me"
"#,
    )
    .unwrap();

    // When a struct-path save rewrites the file (the config.set flow)
    config
        .merge_wire_patch(&serde_json::json!({ "yolo": true }))
        .unwrap();
    let contents = config.to_toml_string().unwrap();

    // Then the passthrough keys survive the save/load cycle (the
    // per-entry unknown-key merge), next to the typed fields
    let reloaded = ServerConfig::from_toml(&contents).unwrap();
    let raw = reloaded.raw.as_ref().unwrap();
    let entry = raw
        .get("models")
        .and_then(|m| m.get("test/alias"))
        .cloned()
        .expect("the entry survived the round trip");
    assert_eq!(entry.get("provider").and_then(|v| v.as_str()), Some("kimi"));
    assert_eq!(
        entry.get("custom_flag").and_then(|v| v.as_str()),
        Some("keep-me"),
        "passthrough keys preserved: {entry}"
    );
    assert_eq!(
        entry.get("provider_id").and_then(|v| v.as_str()),
        Some("kimi")
    );
}

// ===== diagnostics (P6: the config.diagnostics warnings face) =====

/// should report no warnings when the config is internally consistent
#[test]
fn should_report_no_warnings_when_config_is_consistent() {
    // Given a config whose pointers all resolve
    let config = ServerConfig::from_toml(
        r#"
default_provider = "kimi"
default_model = "kimi/flagship"

[providers.kimi]
type = "openai"

[models."kimi/flagship"]
provider = "kimi"
model = "kimi-k2"
max_context_size = 262144
"#,
    )
    .expect("TOML should parse");

    // When the diagnostics are computed
    let warnings = config.diagnostics_warnings();

    // Then nothing is flagged
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

/// should surface the retired disabled_agents key's deprecation notice
/// through the config.diagnostics face (D15, ADR-009)
#[test]
fn should_surface_disabled_agents_deprecation_through_diagnostics() {
    // Given a config still carrying a non-empty retired key
    let config = ServerConfig::from_toml(
        r#"
disabled_agents = ["coder", "explore"]

[providers.kimi]
type = "openai"
"#,
    )
    .expect("TOML should parse");

    // When the diagnostics are computed
    let warnings = config.diagnostics_warnings();

    // Then exactly one warning carries the D15 deprecation notice — the
    // same text the boot path warns once at startup, now also exposed on
    // the config.diagnostics face.
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        warnings[0].contains("disabled_agents"),
        "warning should name the retired key: {warnings:?}"
    );
    assert!(
        warnings[0].contains("enabled = false"),
        "warning should point at the file-level flag: {warnings:?}"
    );

    // And an empty key stays silent (the read-compat exemption).
    let clean = ServerConfig::from_toml("disabled_agents = []\n").expect("TOML should parse");
    assert!(
        clean.diagnostics_warnings().is_empty(),
        "an empty retired key must not warn"
    );
}

/// should warn when default_provider points at a provider that is not
/// configured
#[test]
fn should_warn_when_default_provider_points_at_missing_provider() {
    // Given a config whose default_provider dangles
    let config = ServerConfig::from_toml(
        r#"
default_provider = "ghost"

[providers.kimi]
type = "openai"
"#,
    )
    .expect("TOML should parse");

    // When the diagnostics are computed
    let warnings = config.diagnostics_warnings();

    // Then exactly one warning names the dangling pointer and the
    // configured alternatives
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        warnings[0].contains("default_provider \"ghost\""),
        "warning should name the dangling provider: {warnings:?}"
    );
    assert!(
        warnings[0].contains("kimi"),
        "warning should list the configured providers: {warnings:?}"
    );
}

/// should warn when a model alias points at a provider that is not
/// configured
#[test]
fn should_warn_when_model_alias_points_at_missing_provider() {
    // Given a config whose only model alias references a ghost provider
    let config = ServerConfig::from_toml(
        r#"
[providers.kimi]
type = "openai"

[models."kimi/old"]
provider = "retired-provider"
model = "some-model"
max_context_size = 131072
"#,
    )
    .expect("TOML should parse");

    // When the diagnostics are computed
    let warnings = config.diagnostics_warnings();

    // Then the warning names the alias and the missing provider
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        warnings[0].contains("kimi/old") && warnings[0].contains("retired-provider"),
        "warning should name the alias and its missing provider: {warnings:?}"
    );
}

/// should hint at the multi-provider migration when the legacy
/// single-provider [model] section is still in use
#[test]
fn should_hint_migration_when_legacy_model_section_is_present() {
    // Given a config using the legacy [model] spelling
    let config = ServerConfig::from_toml(
        r#"
[model]
id = "claude-sonnet-4-20250514"
provider = "anthropic"
"#,
    )
    .expect("TOML should parse");

    // When the diagnostics are computed
    let warnings = config.diagnostics_warnings();

    // Then a migration hint points at the multi-provider sections
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        warnings[0].contains("[model]"),
        "warning should name the legacy section: {warnings:?}"
    );
    assert!(
        warnings[0].contains("[models") || warnings[0].contains("default_model"),
        "warning should point at the new spelling: {warnings:?}"
    );
}

// ===== injected SyncFs (T-13b1-2: marker double-write protection) =====

/// In-memory [`mirri_os::SyncFs`] double backing the migration-marker
/// tests, so the marker write and the second-run skip are verified
/// without the real filesystem.
#[derive(Default)]
struct MarkerFs {
    files: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, String>>,
    writes: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

impl mirri_os::SyncFs for MarkerFs {
    fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
    }

    fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.writes.lock().unwrap().push(path.to_path_buf());
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), content.to_string());
        Ok(())
    }

    fn mkdir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
}

/// should record the migration marker through the injected fs and skip
/// on the second run (T-13b1-2)
#[test]
fn should_record_migration_marker_through_injected_fs_and_skip_second_run() {
    // Given a config without a stale effort (so only the marker write
    // path runs — the config.toml rewrite itself stays with
    // mirri-persistence) and an in-memory fs double
    let fs = MarkerFs::default();
    let home = std::path::PathBuf::from("/virtual-home");
    let config_path = home.join("config.toml");
    let marker_path = home.join(MIGRATIONS_MARKER_FILE);
    let mut config =
        ServerConfig::from_toml("[thinking]\neffort = \"high\"\n").expect("TOML should parse");

    // When the migration runs through the double
    migrate_config(&mut config, &config_path, &home, &fs).expect("migration should succeed");

    // Then the marker was written through the injected fs and records
    // the migration
    assert!(
        fs.writes.lock().unwrap().contains(&marker_path),
        "marker write must go through the injected fs"
    );
    let markers: serde_json::Value = serde_json::from_str(
        fs.files
            .lock()
            .unwrap()
            .get(&marker_path)
            .expect("marker file written through the double"),
    )
    .expect("marker file should be valid JSON");
    assert!(
        markers.get(MIGRATION_THINKING_EFFORT_MAX_TO_HIGH).is_some(),
        "marker file should record the applied migration"
    );

    // And a second run is skipped entirely (double-write protection):
    // no additional marker writes, the value is left as the caller
    // holds it, and the marker keeps its original timestamp
    let writes_before = fs.writes.lock().unwrap().len();
    let marker_before = fs
        .files
        .lock()
        .unwrap()
        .get(&marker_path)
        .cloned()
        .expect("marker present");
    let mut second =
        ServerConfig::from_toml("[thinking]\neffort = \"max\"\n").expect("TOML should parse");
    migrate_config(&mut second, &config_path, &home, &fs).expect("migration should succeed");
    assert_eq!(
        second
            .thinking
            .as_ref()
            .expect("thinking should parse")
            .effort
            .as_deref(),
        Some("max"),
        "an already-marked migration must not rewrite the value"
    );
    assert_eq!(
        fs.writes.lock().unwrap().len(),
        writes_before,
        "an already-marked migration must not write the marker again"
    );
    assert_eq!(
        fs.files.lock().unwrap().get(&marker_path).cloned(),
        Some(marker_before),
        "the marker file must be left untouched on the skip path"
    );
}

/// should project the disabled_skills set onto the wire with its camelCase
/// key, and accept it back through a config.set wire patch
#[test]
fn should_round_trip_disabled_skills_between_toml_and_the_config_wire() {
    // Given a config.toml carrying the user-level disabled-skill set
    let config = ServerConfig::from_toml("disabled_skills = [\"pdf\", \"git-commit\"]\n")
        .expect("TOML should parse");

    // When building the wire config
    let wire = config.to_mirri_config_wire();

    // Then the set surfaces under its camelCase key
    assert_eq!(wire["disabledSkills"], json!(["pdf", "git-commit"]));

    // And when an empty-set patch merges in (the re-enable-all write)
    let mut patched = config.clone();
    patched
        .merge_wire_patch(&serde_json::json!({ "disabledSkills": [] }))
        .expect("patch should merge");
    // Then the empty set round-trips too (clearing != removing the key)
    assert_eq!(patched.to_mirri_config_wire()["disabledSkills"], json!([]));
}

/// should reject nothing when merge_wire_patch carries disabledSkills — the
/// key is on the known-top-level whitelist
#[test]
fn should_accept_disabled_skills_in_a_wire_patch() {
    let mut config = ServerConfig::default();
    config
        .merge_wire_patch(&serde_json::json!({ "disabledSkills": ["pdf"] }))
        .expect("disabledSkills must be a known config key");
    assert_eq!(
        config.disabled_skills.as_deref(),
        Some(&["pdf".to_string()][..])
    );
}

// ============================================================
// B1 T1: typed [task] / [background] / [subagent] sections
// ============================================================

/// Given a config.toml with a legacy `[background]` base and a `[task]`
/// override, when parsed and resolved, then `[task]` fields win per-field
/// and unset `[task]` fields fall through to `[background]` (TS
/// configSection.ts:50-56 parity).
#[test]
fn should_merge_legacy_background_under_task_when_resolving_task_config() {
    let config = ServerConfig::from_toml(
        r#"
        [background]
        maxRunningTasks = 3
        keepAliveOnExit = true

        [task]
        maxRunningTasks = 5
    "#,
    )
    .expect("parse");

    let resolved = config.resolve_task_config();
    assert_eq!(
        resolved.max_running_tasks,
        Some(5),
        "task overrides background"
    );
    assert_eq!(
        resolved.keep_alive_on_exit,
        Some(true),
        "fallthrough to background"
    );
}

/// Given a subagent section and the MIRRI_SUBAGENT_TIMEOUT_MS env override,
/// when resolved, then env wins and the default is 2h (0 = unbounded).
#[test]
fn should_prefer_env_over_toml_when_resolving_subagent_timeout() {
    let mut config = ServerConfig::from_toml("[subagent]\ntimeoutMs = 1000\n").unwrap();
    config.apply_env_with(fake_env(&[("MIRRI_SUBAGENT_TIMEOUT_MS", "2000")]));
    assert_eq!(
        config.resolve_subagent_timeout(),
        std::time::Duration::from_millis(2000)
    );

    let plain = ServerConfig::default();
    assert_eq!(
        plain.resolve_subagent_timeout(),
        std::time::Duration::from_millis(DEFAULT_SUBAGENT_TIMEOUT_MS)
    );
}

/// Given env overrides for the task section, when applied, then capacity and
/// keep-alive flip without touching the on-disk document.
#[test]
fn should_apply_env_overrides_when_task_section_env_vars_are_set() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[
        ("MIRRICODE_BACKGROUND_MAX_RUNNING_TASKS", "7"),
        ("MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT", "true"),
    ]));
    assert_eq!(config.resolve_task_config().max_running_tasks, Some(7));
    assert_eq!(config.resolve_task_config().keep_alive_on_exit, Some(true));
}

/// Given a config without an orphan-disposal policy, when resolved, then
/// the census is report-only by default (SDR-013 c — the sweep never acts
/// unless the operator opts in).
#[test]
fn should_default_the_orphan_dispose_policy_to_report_only_when_unset() {
    let config = ServerConfig::default();
    assert_eq!(
        config.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportOnly
    );
}

/// Given a config without a task lifetime ceiling, when resolved, then the
/// built-in 4-hour default applies (US14.4).
#[test]
fn should_default_the_max_task_lifetime_to_four_hours_when_unset() {
    let config = ServerConfig::default();
    assert_eq!(
        config.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(4 * 3600)
    );
}

/// Given a zero lifetime ceiling, when resolved, then the ceiling is
/// disabled — `Duration::ZERO` is the unbounded sentinel the consumer
/// skips (US14.4: 0 = 不设限).
#[test]
fn should_disable_the_lifetime_ceiling_when_configured_as_zero() {
    let config = ServerConfig {
        task: Some(TaskConfig {
            max_task_lifetime_s: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        config.resolve_max_task_lifetime(),
        std::time::Duration::ZERO
    );
}

/// Given the MIRRICODE_TASK_* orphan-knob env overrides, when applied,
/// then the policy and the ceiling flip without touching the on-disk
/// document (env > config file > default).
#[test]
fn should_apply_env_overrides_when_orphan_knob_env_vars_are_set() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[
        ("MIRRICODE_TASK_ORPHAN_DISPOSE_POLICY", "reportAndTerminate"),
        ("MIRRICODE_TASK_MAX_LIFETIME_S", "7200"),
    ]));
    assert_eq!(
        config.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportAndTerminate
    );
    assert_eq!(
        config.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(7200)
    );
}

/// Given an env lifetime value that is not a number, when applied, then it
/// is skipped like an unparseable value (the task-env fail-soft
/// convention — a bare cast would never be attempted here).
#[test]
fn should_skip_an_unparseable_env_lifetime_override_instead_of_failing() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[("MIRRICODE_TASK_MAX_LIFETIME_S", "soon")]));
    assert_eq!(
        config.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(4 * 3600),
        "an unparseable env lifetime must keep the default"
    );
}

/// Given an env orphan-policy value that is not a known policy name, when
/// applied, then it is skipped like an unparseable value and the census
/// stays report-only (the task-env fail-soft convention — the same shape
/// the lifetime knob's unparseable test pins).
#[test]
fn should_skip_an_unparseable_env_dispose_policy_override_instead_of_failing() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[(
        "MIRRICODE_TASK_ORPHAN_DISPOSE_POLICY",
        "garbage",
    )]));
    assert_eq!(
        config.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportOnly,
        "an unparseable env policy must keep the report-only default"
    );
}

/// Given a typed `[task]` section spelling the orphan knobs in either
/// casing, when parsed, then both key spellings resolve with camelCase
/// enum values (the TS snakeToCamel alias parity).
#[test]
fn should_parse_the_orphan_knobs_from_the_typed_task_section_in_both_casings() {
    let camel = ServerConfig::from_toml(
        "[task]\norphanDisposePolicy = \"reportAndTerminate\"\nmaxTaskLifetimeS = 60\n",
    )
    .unwrap();
    assert_eq!(
        camel.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportAndTerminate
    );
    assert_eq!(
        camel.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(60)
    );

    let snake = ServerConfig::from_toml(
        "[task]\norphan_dispose_policy = \"reportOnly\"\nmax_task_lifetime_s = 120\n",
    )
    .unwrap();
    assert_eq!(
        snake.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportOnly
    );
    assert_eq!(
        snake.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(120)
    );
}

/// Given a legacy `[background]` section carrying the orphan knobs, when
/// resolved, then the values flow through the base layer — the same
/// fallthrough `[task]` fields enjoy (D-65 resolve semantics).
#[test]
fn should_fall_through_to_the_legacy_background_base_when_resolving_orphan_knobs() {
    let config = ServerConfig::from_toml(
        "[background]\norphanDisposePolicy = \"reportAndTerminate\"\nmaxTaskLifetimeS = 30\n",
    )
    .unwrap();
    assert_eq!(
        config.resolve_orphan_dispose_policy(),
        OrphanDisposePolicy::ReportAndTerminate
    );
    assert_eq!(
        config.resolve_max_task_lifetime(),
        std::time::Duration::from_secs(30)
    );
}

/// Given an env capacity override PAST the u32 range, when applied, then it
/// is skipped like an unparseable value — a bare cast would wrap to 0 and
/// silently lift the cap.
#[test]
fn should_skip_an_out_of_range_env_capacity_override_instead_of_wrapping() {
    let mut config = ServerConfig::default();
    config.apply_env_with(fake_env(&[("MIRRI_TASK_MAX_RUNNING_TASKS", "4294967296")]));
    assert_eq!(
        config.resolve_task_config().max_running_tasks,
        None,
        "an out-of-range capacity must not wrap to a silently unbounded 0"
    );
}

/// Given a legacy `[background]` base with a zero capacity, when the config
/// is parsed, then the load is rejected — TS zod enforces `.min(1)` wherever
/// the field is set, not only in the typed `[task]` section.
#[test]
fn should_reject_a_zero_legacy_background_capacity_at_load() {
    let error = ServerConfig::from_toml("[background]\nmax_running_tasks = 0\n")
        .expect_err("a zero legacy capacity must fail the load");
    assert!(
        error.to_string().contains("maxRunningTasks must be >= 1"),
        "the bounds error must surface the field: {error}"
    );
}

/// Given a legacy `[background]` section with an `env` sub-map, when the
/// wire config is built, then the top-level keys are camelCased but the env
/// var names stay verbatim — they are case-sensitive identifiers, not
/// config keys (`FOO_BAR` must not become `FOOBAR`).
#[test]
fn should_preserve_env_map_keys_verbatim_in_the_background_wire_passthrough() {
    let config = ServerConfig::from_toml(
        "[background]\nmax_running_tasks = 2\n\n[background.env]\nFOO_BAR = \"x\"\n",
    )
    .expect("parse");

    let wire = config.to_mirri_config_wire();
    assert_eq!(wire["background"]["maxRunningTasks"], 2);
    assert_eq!(
        wire["background"]["env"]["FOO_BAR"], "x",
        "env var names must survive the camelCase conversion verbatim: {}",
        wire["background"]
    );
}

/// Given a `[task]` section, when the wire config is built and a `config.set`
/// patch arrives, then the section round-trips typed camelCase and `task` is
/// an accepted patch key (D-65).
#[test]
fn should_emit_and_accept_the_typed_task_section_when_configured() {
    let mut config = ServerConfig::from_toml(
        r#"
[task]
maxRunningTasks = 4
printBackgroundMode = "drain"
"#,
    )
    .expect("parse");

    let wire = config.to_mirri_config_wire();
    assert_eq!(wire["task"]["maxRunningTasks"], 4);
    assert_eq!(wire["task"]["printBackgroundMode"], "drain");

    config
        .merge_wire_patch(&serde_json::json!({
            "task": { "maxRunningTasks": 9, "killGracePeriodMs": 1500 }
        }))
        .expect("task must be a known config key");
    let wire = config.to_mirri_config_wire();
    assert_eq!(wire["task"]["maxRunningTasks"], 9);
    assert_eq!(wire["task"]["killGracePeriodMs"], 1500);
    assert_eq!(
        config.resolve_task_config().print_background_mode,
        Some(PrintBackgroundMode::Drain)
    );
}

/// Given a `[task]` section with an out-of-bounds value, when the config is
/// parsed, then the load is rejected at parse time (TS zod .min() parity).
#[test]
fn should_reject_the_config_when_a_task_section_value_is_out_of_bounds() {
    let error = ServerConfig::from_toml("[task]\nmaxRunningTasks = 0\n")
        .expect_err("zero maxRunningTasks must be rejected");
    assert!(
        error.to_string().contains("maxRunningTasks"),
        "error names the offending key: {error}"
    );
}

/// Given a subagent section, when the wire config is built, then it emits
/// the typed camelCase spelling (timeoutMs), not the passthrough file key.
#[test]
fn should_emit_the_typed_subagent_section_when_configured() {
    let config = ServerConfig::from_toml("[subagent]\ntimeout_ms = 60000\n").unwrap();
    let wire = config.to_mirri_config_wire();
    assert_eq!(wire["subagent"]["timeoutMs"], 60_000);
    assert!(
        wire["subagent"].get("timeout_ms").is_none(),
        "the legacy snake_case spelling must not leak to the wire"
    );
}

/// Given a `[subagent]` maxNestingDepth, when resolved, then the raw value
/// passes through (0 = unlimited, absent = the default of 5) — the depth
/// guard's refusal message names this knob, so it must be real (B4 T5 /
/// SDR-B4-03, review SF-4).
#[test]
fn should_resolve_the_subagent_max_nesting_depth_from_the_typed_section() {
    let unset = ServerConfig::from_toml("").unwrap();
    assert_eq!(
        unset.resolve_subagent_max_nesting_depth(),
        crate::subagent::DEFAULT_MAX_NESTING_DEPTH,
        "absent keeps the default of 5"
    );
    let zero = ServerConfig::from_toml("[subagent]\nmaxNestingDepth = 0\n").unwrap();
    assert_eq!(zero.resolve_subagent_max_nesting_depth(), 0);
    let camel = ServerConfig::from_toml("[subagent]\nmax_nesting_depth = 9\n").unwrap();
    assert_eq!(camel.resolve_subagent_max_nesting_depth(), 9);
}

/// Given a `[subagent]` maxNestingDepth, when the session scope config is
/// built, then the ceiling flows through `session_scope_config_from` (the
/// production configuration surface, not just the Rust API).
#[test]
fn should_flow_the_subagent_max_nesting_depth_into_the_session_scope_config() {
    let server = ServerConfig::from_toml("[subagent]\nmaxNestingDepth = 3\n").unwrap();
    let scope = crate::session_scope_config_from(&server);
    assert_eq!(scope.max_nesting_depth, 3);
    let unset = crate::session_scope_config_from(&ServerConfig::from_toml("").unwrap());
    assert_eq!(
        unset.max_nesting_depth,
        crate::subagent::DEFAULT_MAX_NESTING_DEPTH
    );
}

/// Given a `[subagent]` maxSwarmItems, when resolved, then the value passes
/// through (0 = unlimited, absent = the default of 3) — the AgentSwarm item
/// cap's knob must be real and spellable in both casings (SDR-012).
#[test]
fn should_resolve_the_subagent_max_swarm_items_from_the_typed_section() {
    let unset = ServerConfig::from_toml("").unwrap();
    assert_eq!(
        unset.resolve_subagent_max_swarm_items(),
        crate::subagent::DEFAULT_MAX_SWARM_ITEMS,
        "absent keeps the default of 3"
    );
    let zero = ServerConfig::from_toml("[subagent]\nmaxSwarmItems = 0\n").unwrap();
    assert_eq!(zero.resolve_subagent_max_swarm_items(), 0, "0 = unlimited");
    let camel = ServerConfig::from_toml("[subagent]\nmaxSwarmItems = 12\n").unwrap();
    assert_eq!(camel.resolve_subagent_max_swarm_items(), 12);
    let snake = ServerConfig::from_toml("[subagent]\nmax_swarm_items = 9\n").unwrap();
    assert_eq!(snake.resolve_subagent_max_swarm_items(), 9);
}

/// Given a `[subagent]` maxSwarmItems, when the session scope config is
/// built, then the cap flows through `session_scope_config_from` into the
/// task-lifecycle settings (the production configuration surface).
#[test]
fn should_flow_the_subagent_max_swarm_items_into_the_session_scope_config() {
    let server = ServerConfig::from_toml("[subagent]\nmaxSwarmItems = 8\n").unwrap();
    let scope = crate::session_scope_config_from(&server);
    assert_eq!(scope.task_lifecycle.max_swarm_items, Some(8));
    let unset = crate::session_scope_config_from(&ServerConfig::from_toml("").unwrap());
    assert_eq!(
        unset.task_lifecycle.max_swarm_items,
        Some(crate::subagent::DEFAULT_MAX_SWARM_ITEMS),
        "absent seeds the resolved default of 3"
    );
}

/// Given a `[task]` heartbeatIntervalMs, when the session scope config is
/// built, then the staleness window flows through
/// `session_scope_config_from` into the task-lifecycle settings (0
/// disables staleness; absent keeps the 5-minute default) — the knob must
/// be real and spellable in both casings.
#[test]
fn should_flow_the_task_heartbeat_interval_into_the_session_scope_config() {
    let camel = ServerConfig::from_toml("[task]\nheartbeatIntervalMs = 1000\n").unwrap();
    let scope = crate::session_scope_config_from(&camel);
    assert_eq!(
        scope.task_lifecycle.heartbeat_interval,
        Some(std::time::Duration::from_millis(1000))
    );
    let snake = ServerConfig::from_toml("[task]\nheartbeat_interval_ms = 250\n").unwrap();
    let scope = crate::session_scope_config_from(&snake);
    assert_eq!(
        scope.task_lifecycle.heartbeat_interval,
        Some(std::time::Duration::from_millis(250))
    );
    let zero = ServerConfig::from_toml("[task]\nheartbeatIntervalMs = 0\n").unwrap();
    let scope = crate::session_scope_config_from(&zero);
    assert_eq!(
        scope.task_lifecycle.heartbeat_interval, None,
        "0 disables staleness entirely"
    );
    let unset = crate::session_scope_config_from(&ServerConfig::from_toml("").unwrap());
    assert_eq!(
        unset.task_lifecycle.heartbeat_interval,
        Some(std::time::Duration::from_secs(300)),
        "absent keeps the 5-minute default"
    );
}

// ---------------------------------------------------------------------------
// Doctor key inventories (Task 1: config/wire.rs)
// ---------------------------------------------------------------------------

#[test]
fn should_contain_both_hook_key_spellings_when_inspecting_known_hook_entry_keys() {
    // Given the known-hooks key list used for unknown-key detection
    // When checking both spelling families
    // Then every serde-accepted spelling is listed
    for key in [
        "event",
        "command",
        "pattern",
        "matcher",
        "timeout_ms",
        "timeout",
        "fail_closed",
        "failClosed",
        "env",
        "cwd",
    ] {
        assert!(
            KNOWN_HOOK_ENTRY_KEYS.contains(&key),
            "expected hook key {key} in KNOWN_HOOK_ENTRY_KEYS"
        );
    }
}

#[test]
fn should_contain_permission_rule_keys_when_inspecting_known_permission_rule_keys() {
    // Given the permission rule key list
    // When checking the serde/rust-accepted spellings
    // Then all accepted spellings are listed
    for key in ["decision", "scope", "pattern", "reason"] {
        assert!(
            KNOWN_PERMISSION_RULE_KEYS.contains(&key),
            "expected permission key {key} in KNOWN_PERMISSION_RULE_KEYS"
        );
    }
}

#[test]
fn should_cover_typed_sections_when_inspecting_known_section_keys() {
    // Given the per-section known-key table
    // When looking up each typed section
    // Then the section is present with a non-empty key list
    for section in [
        "thinking",
        "loop_control",
        "task",
        "subagent",
        "model_catalog",
    ] {
        let entry = KNOWN_SECTION_KEYS
            .iter()
            .find(|(name, _)| *name == section)
            .unwrap_or_else(|| panic!("section {section} missing from KNOWN_SECTION_KEYS"));
        assert!(
            !entry.1.is_empty(),
            "section {section} has an empty key list"
        );
    }
}

#[test]
fn should_list_effort_and_aliases_for_thinking_when_inspecting_known_section_keys() {
    // Given the thinking section entry
    // When checking its keys
    // Then both the snake_case field and its accepted spellings are present
    let (_, thinking_keys) = KNOWN_SECTION_KEYS
        .iter()
        .find(|(name, _)| *name == "thinking")
        .expect("thinking section entry");
    for key in ["enabled", "effort", "keep"] {
        assert!(thinking_keys.contains(&key), "expected thinking key {key}");
    }
}

#[test]
fn should_list_loop_control_fields_and_aliases_when_inspecting_known_section_keys() {
    // Given the loop_control section entry
    // When checking its keys
    // Then every serde field name and camelCase alias is listed
    let (_, keys) = KNOWN_SECTION_KEYS
        .iter()
        .find(|(name, _)| *name == "loop_control")
        .expect("loop_control section entry");
    for key in [
        "max_steps_per_turn",
        "max_retries_per_step",
        "max_ralph_iterations",
        "maxRalphIterations",
        "reserved_context_size",
        "reservedContextSize",
        "compaction_trigger_ratio",
        "compactionTriggerRatio",
        "max_overflow_compaction_attempts",
        "maxOverflowCompactionAttempts",
        "max_history_messages",
        "maxHistoryMessages",
        "max_deferred_messages",
        "maxDeferredMessages",
        "retention_tail_ratio",
        "retentionTailRatio",
        "retention_tail_tokens",
        "retentionTailTokens",
    ] {
        assert!(keys.contains(&key), "expected loop_control key {key}");
    }
}

#[test]
fn should_list_task_fields_and_aliases_when_inspecting_known_section_keys() {
    // Given the task section entry
    // When checking its keys
    // Then every serde field name and camelCase alias is listed
    let (_, keys) = KNOWN_SECTION_KEYS
        .iter()
        .find(|(name, _)| *name == "task")
        .expect("task section entry");
    for key in [
        "max_running_tasks",
        "maxRunningTasks",
        "keep_alive_on_exit",
        "keepAliveOnExit",
        "bash_auto_background_on_timeout",
        "bashAutoBackgroundOnTimeout",
        "bash_task_timeout_s",
        "bashTaskTimeoutS",
        "kill_grace_period_ms",
        "killGracePeriodMs",
        "print_wait_ceiling_s",
        "printWaitCeilingS",
        "print_background_mode",
        "printBackgroundMode",
        "print_max_turns",
        "printMaxTurns",
        "heartbeat_interval_ms",
        "heartbeatIntervalMs",
        "orphan_dispose_policy",
        "orphanDisposePolicy",
        "max_task_lifetime_s",
        "maxTaskLifetimeS",
    ] {
        assert!(keys.contains(&key), "expected task key {key}");
    }
}

#[test]
fn should_list_subagent_fields_and_aliases_when_inspecting_known_section_keys() {
    // Given the subagent section entry
    // When checking its keys
    // Then every serde field name and camelCase alias is listed
    let (_, keys) = KNOWN_SECTION_KEYS
        .iter()
        .find(|(name, _)| *name == "subagent")
        .expect("subagent section entry");
    for key in [
        "timeout_ms",
        "timeoutMs",
        "max_nesting_depth",
        "maxNestingDepth",
        "max_swarm_items",
        "maxSwarmItems",
    ] {
        assert!(keys.contains(&key), "expected subagent key {key}");
    }
}

#[test]
fn should_list_model_catalog_fields_and_aliases_when_inspecting_known_section_keys() {
    // Given the model_catalog section entry
    // When checking its keys
    // Then every serde field name and camelCase alias is listed
    let (_, keys) = KNOWN_SECTION_KEYS
        .iter()
        .find(|(name, _)| *name == "model_catalog")
        .expect("model_catalog section entry");
    for key in ["persist_cache", "persistCache"] {
        assert!(keys.contains(&key), "expected model_catalog key {key}");
    }
}

// ---------------------------------------------------------------------------
// config doctor (`super::doctor`) — validate_config_str
// ---------------------------------------------------------------------------

#[test]
fn should_report_syntax_error_with_location_when_toml_is_malformed() {
    // Given a config text with broken TOML syntax (unclosed string)
    // When validating
    // Then the report carries a single hard error mentioning the position
    let report = validate_config_str("default_model = \"unclosed\n", "config.toml", &[]);
    assert!(!report.parse_ok);
    assert!(report.has_errors());
    assert_eq!(report.issues.len(), 1);
    assert!(
        report.issues[0].message.contains("line"),
        "error message should locate the syntax error: {}",
        report.issues[0].message
    );
}

#[test]
fn should_pass_when_config_text_is_empty() {
    // Given an empty (or whitespace-only) config text
    // When validating
    // Then no issues are reported and the parse is considered OK
    for text in ["", "   \n"] {
        let report = validate_config_str(text, "config.toml", &[]);
        assert!(report.parse_ok, "empty config should parse: {text:?}");
        assert!(!report.has_errors(), "empty config should have no errors");
        assert!(
            report.issues.is_empty(),
            "empty config should have no issues at all"
        );
    }
}

#[test]
fn should_accept_both_hook_spellings_when_validating_hooks() {
    // Given a config whose hooks use snake_case keys and another whose hooks
    // use the TS camelCase aliases
    // When validating
    // Then neither produces unknown-key errors
    let snake = r#"
[[hooks]]
event = "PreToolUse"
command = "node hook.mjs"
pattern = "Bash"
timeout_ms = 30000
fail_closed = false
"#;
    let camel = r#"
[[hooks]]
event = "PreToolUse"
command = "node hook.mjs"
matcher = "Bash"
timeout = 30
failClosed = false
"#;
    for text in [snake, camel] {
        let report = validate_config_str(text, "config.toml", &[]);
        let unknown = report
            .issues
            .iter()
            .filter(|i| i.message.contains("unknown key"))
            .collect::<Vec<_>>();
        assert!(
            unknown.is_empty(),
            "unexpected unknown-key errors: {unknown:?}"
        );
        assert!(report.parse_ok);
    }
}

#[test]
fn should_report_unknown_top_level_key_when_key_is_not_in_the_inventory() {
    // Given a config with a misspelled top-level key
    // When validating
    // Then a hard error names the key
    let report = validate_config_str("defualt_model = \"ultra\"\n", "config.toml", &[]);
    assert!(report.has_errors());
    let issue = report
        .issues
        .iter()
        .find(|i| i.message.contains("defualt_model"))
        .expect("error should name the unknown top-level key");
    assert!(matches!(issue.status, CheckStatus::Error));
}

#[test]
fn should_report_unknown_provider_entry_key_when_field_is_misspelled() {
    // Given a provider entry with a misspelled field
    // When validating
    // Then a hard error names the field and the provider
    let report = validate_config_str(
        "[providers.myprov]\ntype = \"openai\"\nbse_url = \"https://example.com\"\n",
        "config.toml",
        &[],
    );
    assert!(report.has_errors());
    let issue = report
        .issues
        .iter()
        .find(|i| i.message.contains("bse_url"))
        .expect("error should name the misspelled provider field");
    assert!(
        issue.message.contains("myprov"),
        "error should name the provider entry: {}",
        issue.message
    );
}

#[test]
fn should_accept_camel_case_provider_aliases_when_validating_providers() {
    // Given a provider entry spelled with the TypeScript camelCase wire keys
    // the serde aliases accept
    // When validating
    // Then no unknown-key errors are reported
    let report = validate_config_str(
        "[providers.acme]\n\
         type = \"openai\"\n\
         api_key = \"sk-test\"\n\
         baseUrl = \"https://api.example.com/v1\"\n\
         defaultModel = \"gpt-5\"\n\
         removedModelIds = [\"gpt-4\"]\n\
         modelSource = \"static\"\n\
         customHeaders = { \"X-Tenant\" = \"acme\" }\n\
         mediaFileRefs = false\n",
        "config.toml",
        &[],
    );
    let unknown = report
        .issues
        .iter()
        .filter(|i| i.message.contains("unknown key"))
        .collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "camelCase provider keys must be recognized: {unknown:?}"
    );
}

#[test]
fn should_accept_camel_case_model_aliases_when_validating_models() {
    // Given a model alias spelled with the TypeScript camelCase wire keys the
    // serde aliases accept
    // When validating
    // Then no unknown-key errors are reported
    let report = validate_config_str(
        "[models.acme-fast]\n\
         provider = \"acme\"\n\
         model = \"acme-flash\"\n\
         max_context_size = 200000\n\
         displayName = \"Acme Fast\"\n\
         reasoningKey = \"reasoning\"\n\
         adaptiveThinking = true\n\
         supportEfforts = [\"high\"]\n\
         defaultEffort = \"high\"\n\
         protocol = \"openai\"\n\
         betaApi = true\n\
         maxOutputSize = 8000\n",
        "config.toml",
        &[],
    );
    let unknown = report
        .issues
        .iter()
        .filter(|i| i.message.contains("unknown key"))
        .collect::<Vec<_>>();
    assert!(
        unknown.is_empty(),
        "camelCase model keys must be recognized: {unknown:?}"
    );
}

#[test]
fn should_report_unknown_hook_key_when_field_is_not_in_the_inventory() {
    // Given a hooks entry with an unrecognized key
    // When validating
    // Then a hard error names the key and its array position
    let report = validate_config_str(
        "[[hooks]]\nevent = \"PreToolUse\"\ncommand = \"x\"\npatern = \"Bash\"\n",
        "config.toml",
        &[],
    );
    assert!(report.has_errors());
    let issue = report
        .issues
        .iter()
        .find(|i| i.message.contains("patern"))
        .expect("error should name the misspelled hook key");
    assert!(
        issue.path.contains(&"hooks".to_string()),
        "path should include the hooks section: {:?}",
        issue.path
    );
}

#[test]
fn should_report_unknown_permission_rule_key_when_field_is_misspelled() {
    // Given a permission rule with an unrecognized key
    // When validating
    // Then a hard error names the key
    let report = validate_config_str(
        "[[permission.rules]]\ndecision = \"allow\"\npatrn = \"Read\"\n",
        "config.toml",
        &[],
    );
    assert!(report.has_errors());
    assert!(
        report.issues.iter().any(|i| i.message.contains("patrn")),
        "an error should name the misspelled permission key: {:?}",
        report.issues
    );
}

#[test]
fn should_warn_without_error_when_diagnostics_find_dangling_references() {
    // Given a config whose default_provider names no configured provider
    // When validating
    // Then the report carries a warning (not an error) and stays parse-ok
    let report = validate_config_str("default_provider = \"ghost\"\n", "config.toml", &[]);
    assert!(report.parse_ok);
    assert!(
        !report.has_errors(),
        "advisory diagnostics must not be hard errors: {:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i.status, CheckStatus::Warn)),
        "a dangling default_provider should produce a warning: {:?}",
        report.issues
    );
}

#[test]
fn should_error_when_loop_control_sets_mutually_exclusive_keys() {
    // Given a loop_control section setting both retention keys
    // When validating
    // Then a hard error is reported
    let report = validate_config_str(
        "[loop_control]\nretention_tail_ratio = 0.5\nretention_tail_tokens = 1000\n",
        "config.toml",
        &[],
    );
    assert!(!report.parse_ok);
    assert!(report.has_errors());
}

#[test]
fn should_warn_when_experimental_flag_is_not_registered() {
    // Given a config enabling an experimental flag that is not in the known list
    // When validating
    // Then a warning names the flag
    let report = validate_config_str(
        "[experimental]\nmicro_compaction = false\n",
        "config.toml",
        &["hook-command-rewrite".to_string()],
    );
    assert!(report.parse_ok);
    let issue = report
        .issues
        .iter()
        .find(|i| i.message.contains("micro_compaction"))
        .expect("a warning should name the unknown flag");
    assert!(matches!(issue.status, CheckStatus::Warn));
}

#[test]
fn should_not_warn_when_experimental_flag_is_registered() {
    // Given a config enabling a registered experimental flag
    // When validating
    // Then no experimental warning appears
    let report = validate_config_str(
        "[experimental]\nhook-command-rewrite = true\n",
        "config.toml",
        &["hook-command-rewrite".to_string()],
    );
    assert!(report.parse_ok);
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.message.contains("hook-command-rewrite")),
        "registered flags must not warn: {:?}",
        report.issues
    );
}

#[test]
fn should_skip_inner_key_check_when_section_is_untyped_passthrough() {
    // Given a config whose [image] section (free passthrough toml::Value)
    // carries arbitrary keys
    // When validating
    // Then no unknown-key errors come from inside [image]
    let report = validate_config_str(
        "[image]\nread_byte_budget = 123\nanything_else = \"free\"\n",
        "config.toml",
        &[],
    );
    assert!(
        !report.has_errors(),
        "passthrough sections must not trigger unknown-key errors: {:?}",
        report.issues
    );
}
