//! Server/agent configuration loaded from `config.toml`.
//!
//! [`ServerConfig`] mirrors the TypeScript agent configuration file. It is
//! loaded from TOML ([`ServerConfig::load`]) and can be overridden by
//! environment variables ([`ServerConfig::apply_env`]).
//!
//! # Example `config.toml`
//!
//! ```toml
//! [model]
//! id = "claude-sonnet-4-20250514"
//! provider = "anthropic"
//!
//! [provider]
//! api_key = "sk-..."
//! base_url = "https://api.example.com"
//!
//! [permission]
//! mode = "manual"
//!
//! [[permission.rules]]
//! decision = "allow"
//! scope = "user"
//! pattern = "Bash*"
//!
//! [loop_control]
//! max_steps_per_turn = 30
//! max_retries_per_step = 3
//! max_ralph_iterations = 4 # parse-only; camelCase alias: maxRalphIterations
//!
//! [[hooks]]
//! event = "PreToolUse"
//! command = "echo before"
//! pattern = "Bash*" # alias: matcher
//! timeout_ms = 30000 # alias: timeout (TS seconds — auto-converted to ms, so timeout = 5 → 5000ms)
//!
//! [experimental]
//! streaming = true
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mirri_os::SyncFs;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EngineError, EngineResult};
use crate::hook_engine::HookDef;
use crate::permission::PermissionRule;

/// Server/agent configuration loaded from config.toml.
///
/// Every section is optional; a missing section falls back to the engine's
/// built-in default.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    /// Default model used by agents.
    pub model: Option<ModelConfig>,
    /// Provider credentials and endpoint.
    pub provider: Option<ProviderConfig>,
    /// Default permission mode.
    pub permission: Option<PermissionConfig>,
    /// Loop-engine control limits.
    pub loop_control: Option<LoopControlConfig>,
    /// External command hooks (see [`HookDef`]).
    pub hooks: Option<Vec<HookDef>>,
    /// Thinking configuration (parse-only; host-interpreted).
    pub thinking: Option<ThinkingConfig>,
    /// Experimental feature flags (host-interpreted).
    pub experimental: Option<HashMap<String, bool>>,

    /// Multi-provider map (TS-compatible). Key is user-chosen provider id.
    #[serde(default)]
    pub providers: Option<HashMap<String, ProviderEntry>>,
    /// Model alias map. Key is user-chosen alias id.
    #[serde(default)]
    pub models: Option<HashMap<String, ModelAlias>>,
    /// Default provider key in the `providers` map.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default model alias key in the `models` map.
    #[serde(default)]
    pub default_model: Option<String>,

    // Passthrough fields (parsed from TOML, passed to wire, not yet wired
    // into the engine).
    #[serde(default)]
    pub plan_mode: Option<bool>,
    #[serde(default)]
    pub yolo: Option<bool>,
    #[serde(default)]
    pub default_permission_mode: Option<String>,
    #[serde(default)]
    pub default_plan_mode: Option<bool>,
    #[serde(default)]
    pub services: Option<toml::Value>,
    #[serde(default)]
    pub merge_all_available_skills: Option<bool>,
    #[serde(default)]
    pub extra_skill_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub extra_agent_dirs: Option<Vec<String>>,
    /// User-level disabled skill names (B8 D-32): only *disabled* names are
    /// recorded; absent/empty means every discovered skill is enabled.
    /// Seeded into every workspace's `SkillCatalog` at assembly time and
    /// resynced on config writes by the embedded server.
    #[serde(default)]
    pub disabled_skills: Option<Vec<String>>,
    /// Retired enablement key (R3-3): still parsed, round-tripped
    /// (`KNOWN_CONFIG_TOP_LEVEL_KEYS`), and exposed verbatim on the config
    /// wire (`disabledAgents`) so old config.toml files keep working — but
    /// the engine no longer consults it. The profile file's own
    /// `enabled = false` flag is the sole enablement source; a non-empty
    /// value here only produces the one-time boot warning
    /// ([`Self::disabled_agents_deprecation_notice`]).
    #[serde(default)]
    pub disabled_agents: Option<Vec<String>>,
    #[serde(default)]
    pub background: Option<toml::Value>,
    /// Typed `[task]` section — the background-task lifecycle knobs (D-65).
    /// `[task]` is the TS-canonical section; the legacy `[background]`
    /// passthrough above stays readable as the resolution base (see
    /// [`ServerConfig::resolve_task_config`]).
    #[serde(default)]
    pub task: Option<TaskConfig>,
    /// Typed `[subagent]` section — the per-subagent timeout (D-65).
    #[serde(default)]
    pub subagent: Option<SubagentConfig>,
    #[serde(default)]
    pub image: Option<toml::Value>,
    /// Model-catalog behaviors (`[model_catalog]`; typed, host-interpreted).
    #[serde(default)]
    pub model_catalog: Option<ModelCatalogConfig>,
    #[serde(default)]
    pub telemetry: Option<bool>,

    /// The original parsed TOML document (before camelCase alias resolution),
    /// preserved for round-trip fidelity and wire exposure. Mirrors the TS
    /// `config.raw` field.
    #[serde(default, skip)]
    pub raw: Option<toml::Value>,
}

/// Model configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model ID (e.g. `"claude-sonnet-4-20250514"`).
    pub id: String,
    /// Provider name; overrides the provider section when set.
    pub provider: Option<String>,
}

/// Provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API key for the provider.
    pub api_key: Option<String>,
    /// Base URL of the provider endpoint.
    pub base_url: Option<String>,
}

/// A provider entry in the `providers` map.
///
/// The TypeScript wire shape uses camelCase keys, so each field accepts its
/// camelCase spelling as a deserialization alias (matching the TS
/// `snakeToCamel` transform on config.toml read).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Provider type: "anthropic", "openai", "google-genai",
    /// "openai_responses", "vertexai".
    #[serde(alias = "type")]
    pub r#type: Option<String>,
    pub api_key: Option<String>,
    #[serde(alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(alias = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(alias = "removedModelIds")]
    pub removed_model_ids: Option<Vec<String>>,
    /// How the provider's model list is sourced (TS `modelSource`:
    /// static / discover / oauth-catalog — validated at resolve time in
    /// TS, kept as free text here).
    #[serde(default, alias = "modelSource")]
    pub model_source: Option<String>,
    /// OAuth configuration (passthrough; host-interpreted).
    pub oauth: Option<Value>,
    /// Environment variable overrides for this provider.
    pub env: Option<HashMap<String, String>>,
    /// Custom HTTP headers sent with every request to this provider.
    #[serde(alias = "customHeaders")]
    pub custom_headers: Option<HashMap<String, String>>,
    /// Whether image attachments may be uploaded through the provider's
    /// files API and referenced by file id instead of inlined base64
    /// (T11a, adjudication D2: default `true`; `false` keeps the provider
    /// fully inline).
    #[serde(default, alias = "mediaFileRefs")]
    pub media_file_refs: Option<bool>,
    /// Provider source descriptor (passthrough; host-interpreted).
    pub source: Option<Value>,
}

/// A model alias entry in the `models` map.
///
/// The TypeScript wire shape uses camelCase keys, so each field accepts its
/// camelCase spelling as a deserialization alias (matching the TS
/// `snakeToCamel` transform on config.toml read).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelAlias {
    pub provider: String,
    pub model: String,
    pub max_context_size: u64,
    #[serde(alias = "maxOutputSize")]
    pub max_output_size: Option<u64>,
    pub capabilities: Option<Vec<String>>,
    #[serde(alias = "displayName")]
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(alias = "reasoningKey")]
    pub reasoning_key: Option<String>,
    #[serde(alias = "adaptiveThinking")]
    pub adaptive_thinking: Option<bool>,
    #[serde(alias = "supportEfforts")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(alias = "defaultEffort")]
    pub default_effort: Option<String>,
    /// The wire protocol this alias speaks (`anthropic` / `openai` /
    /// `openai_responses` / `google-genai` — TS `ModelRecord.protocol`,
    /// one of the four real wire protocols; a vendor such as `kimi` is
    /// expressed as the referenced provider's `type`, never as a
    /// protocol). Overrides the provider type at resolution time.
    pub protocol: Option<String>,
    /// Whether the model uses a beta API endpoint.
    #[serde(alias = "betaApi")]
    pub beta_api: Option<bool>,
}

/// Permission configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionConfig {
    /// Permission mode name (`"manual"`, `"yolo"`, `"auto"`).
    pub mode: Option<String>,
    /// User-configured permission rules (see [`PermissionRule`]).
    pub rules: Option<Vec<PermissionRule>>,
}

/// Loop-engine control limits. Consumed by `turn_flow_config_from`
/// (embedded_server) to build the per-agent compaction strategy.
///
/// `retention_tail_ratio` and `retention_tail_tokens` are mutually
/// exclusive (US6.2) — setting both fails config load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopControlConfig {
    /// Maximum tool steps per turn.
    pub max_steps_per_turn: Option<u32>,
    /// Maximum retries per tool step.
    pub max_retries_per_step: Option<u32>,
    /// Loop iterations reserved for Ralph (parse-only; not yet wired in).
    #[serde(alias = "maxRalphIterations")]
    pub max_ralph_iterations: Option<u32>,
    /// Context size reserved for the host (compaction strategy reserve).
    #[serde(alias = "reservedContextSize")]
    pub reserved_context_size: Option<u64>,
    /// Token ratio at which compaction triggers (compaction strategy
    /// trigger).
    #[serde(alias = "compactionTriggerRatio")]
    pub compaction_trigger_ratio: Option<f64>,
    /// Maximum overflow-triggered compaction attempts before a context
    /// overflow surfaces as an error (compaction strategy
    /// `max_overflow_compaction_attempts`; 0 = fail on the first overflow
    /// without retrying).
    #[serde(alias = "maxOverflowCompactionAttempts")]
    pub max_overflow_compaction_attempts: Option<u32>,
    /// Maximum number of messages allowed in the canonical history before
    /// further appends are rejected (compaction strategy
    /// `max_history_messages`; absent keeps the 10 000 production
    /// default — the cap the turn flow applies via `set_limits` every
    /// turn, and the resume / close-time-snapshot fold simulations model
    /// through [`mirri_proto::ReplayLimits`]). 0 = a fully-rejecting cap.
    #[serde(alias = "maxHistoryMessages")]
    pub max_history_messages: Option<u32>,
    /// Maximum number of messages buffered while a tool exchange is open
    /// before further buffering is rejected (compaction strategy
    /// `max_deferred_messages`; absent keeps the 1 000 production
    /// default). 0 = a fully-rejecting cap.
    #[serde(alias = "maxDeferredMessages")]
    pub max_deferred_messages: Option<u32>,
    /// Retention-tail budget as a ratio of the model's context window
    /// (0 < ratio < 1; see [`crate::compaction::retention`]).
    #[serde(alias = "retentionTailRatio")]
    pub retention_tail_ratio: Option<f64>,
    /// Retention-tail budget as an absolute token count. Mutually
    /// exclusive with `retention_tail_ratio` (US6.2).
    #[serde(alias = "retentionTailTokens")]
    pub retention_tail_tokens: Option<u64>,
    /// Turn-level stall watchdog window in milliseconds — the engine
    /// cancels a turn that emits no LiveEvent for this long. Absent keeps
    /// the engine default (600 s); 0 disables the watchdog (the
    /// `[subagent] timeout_ms` "0 = no timeout" convention).
    #[serde(default, alias = "turnStallTimeoutMs")]
    pub turn_stall_timeout_ms: Option<u64>,
}

impl LoopControlConfig {
    /// Validate the retention-budget keys (US6.2): the two spellings are
    /// mutually exclusive, and the ratio must be a proper fraction.
    pub fn validate(&self) -> Result<(), String> {
        if self.retention_tail_ratio.is_some() && self.retention_tail_tokens.is_some() {
            return Err(
                "retention_tail_ratio and retention_tail_tokens are mutually exclusive: \
                 set exactly one (ratio scales with the model window, tokens is absolute)"
                    .to_string(),
            );
        }
        if let Some(ratio) = self.retention_tail_ratio {
            // Exclusive on both ends (0 < ratio < 1): `Range::contains`'s
            // inclusive start would let a degenerate ratio of 0 through, and
            // a plain `<= 0 || >= 1` form would let NaN through (every
            // NaN comparison is false). The negated conjunction rejects
            // both, plus NaN itself.
            if !(ratio > 0.0 && ratio < 1.0) {
                return Err(format!(
                    "retention_tail_ratio must satisfy 0 < ratio < 1 (got {ratio})"
                ));
            }
        }
        Ok(())
    }

    /// Resume-time capacity limits derived from the `[loop_control]` keys.
    ///
    /// Absent keys keep the compaction-strategy defaults (10 000 / 1 000 —
    /// [`crate::compaction::CompactionStrategy::default`] is the single
    /// source of truth those constants mirror), so a partial section never
    /// silently tightens or lifts the caps. The `u32` values are bounded
    /// and 0 is legal (a fully-rejecting cap), so no further validation
    /// is needed.
    pub fn replay_limits(&self) -> mirri_proto::ReplayLimits {
        let defaults = crate::compaction::CompactionStrategy::default();
        mirri_proto::ReplayLimits {
            max_history_messages: self
                .max_history_messages
                .map_or(defaults.max_history_messages, |v| v as usize),
            max_deferred_messages: self
                .max_deferred_messages
                .map_or(defaults.max_deferred_messages, |v| v as usize),
        }
    }
}

/// Thinking configuration, mirroring the TypeScript `thinking` section.
///
/// All fields are parse-only and host-interpreted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Whether extended thinking is enabled.
    pub enabled: Option<bool>,
    /// Thinking effort level (e.g. `"low"`, `"medium"`, `"high"`, `"max"`).
    pub effort: Option<String>,
    /// Mirri Preserved Thinking passthrough (forwarded verbatim to the wire).
    pub keep: Option<String>,
}

/// The print-mode background strategy (D-59): what `mirri print` does with
/// still-running background tasks when the main turn completes. Mirrors
/// the TS `PrintBackgroundModeSchema` (`configSection.ts:23`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrintBackgroundMode {
    /// Finish immediately — do not wait for background tasks.
    Exit,
    /// Wait for every task to reach a terminal status, capped by
    /// `printWaitCeilingS`.
    Drain,
    /// Keep the turn loop alive while tasks run (the default), capped by
    /// `printMaxTurns` / `printWaitCeilingS`.
    Steer,
}

/// The post-restart orphan-disposal policy (SDR-013 c, US14.3): what the
/// startup orphan census does with a live orphan it positively identifies
/// by process fingerprint. Only two gradations exist by design — never a
/// silent-terminate tier (acting requires an explicit opt-in, and the
/// report trail is the audit record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrphanDisposePolicy {
    /// Report the orphan in the census archive and leave it running
    /// (the default).
    ReportOnly,
    /// Report the orphan and terminate its process group (after the
    /// pre-action group re-validation; fingerprint-confirmed only).
    ReportAndTerminate,
}

/// Default total lifetime ceiling for a process-shaped background task
/// (US14.4): 4 hours. `0` = unlimited.
pub const DEFAULT_TASK_MAX_LIFETIME_S: u64 = 4 * 3600;

/// The `[task]` section — background-task lifecycle knobs (D-65). Field
/// set mirrors the TS `AgentTaskConfigSchema` (`configSection.ts:33-41`)
/// plus the Rust-engine orphan-governance knobs (SDR-013 c/d, US14.3/14.4:
/// `orphanDisposePolicy` / `maxTaskLifetimeS` — no TS counterpart yet).
///
/// `bash_auto_background_on_timeout` and `bash_task_timeout_s` are
/// schema-only here: their behavior lands in the Bash batch; this config
/// face just parses, validates, and exposes them.
///
/// Each field accepts its camelCase spelling as a deserialization alias
/// (matching the TS `snakeToCamel` transform on config.toml read).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskConfig {
    /// Maximum number of concurrently running background tasks (`>= 1`).
    #[serde(default, alias = "maxRunningTasks")]
    pub max_running_tasks: Option<u32>,
    /// Whether session close leaves background tasks running (D-64).
    #[serde(default, alias = "keepAliveOnExit")]
    pub keep_alive_on_exit: Option<bool>,
    /// Move a foreground Bash command to the background instead of
    /// killing it on timeout (schema-only; consumed by the Bash batch).
    #[serde(default, alias = "bashAutoBackgroundOnTimeout")]
    pub bash_auto_background_on_timeout: Option<bool>,
    /// Default timeout (seconds) for background Bash tasks; 0 = no
    /// timeout (schema-only; consumed by the Bash batch).
    #[serde(default, alias = "bashTaskTimeoutS")]
    pub bash_task_timeout_s: Option<u64>,
    /// Grace window before an escalated stop lands `stopped` (`>= 0`).
    #[serde(default, alias = "killGracePeriodMs")]
    pub kill_grace_period_ms: Option<u64>,
    /// Drain-mode wait cap in seconds (`>= 1`).
    #[serde(default, alias = "printWaitCeilingS")]
    pub print_wait_ceiling_s: Option<u64>,
    /// The print-mode background strategy.
    #[serde(default, alias = "printBackgroundMode")]
    pub print_background_mode: Option<PrintBackgroundMode>,
    /// Steer-mode turn cap (`>= 1`).
    #[serde(default, alias = "printMaxTurns")]
    pub print_max_turns: Option<u64>,
    /// Liveness heartbeat window (milliseconds) for background-task
    /// staleness — how long a running task may go without activity before
    /// the sweeper flags it stale. `0` disables staleness entirely; absent
    /// keeps the 5-minute default (`TaskLifecycleSettings`).
    #[serde(default, alias = "heartbeatIntervalMs")]
    pub heartbeat_interval_ms: Option<u64>,
    /// The startup orphan-census disposal policy (SDR-013 c, US14.3):
    /// report-only by default; `reportAndTerminate` opts into fingerprint-
    /// confirmed group termination. Absent keeps
    /// [`OrphanDisposePolicy::ReportOnly`].
    #[serde(default, alias = "orphanDisposePolicy")]
    pub orphan_dispose_policy: Option<OrphanDisposePolicy>,
    /// Total lifetime ceiling (seconds) for a process-shaped background
    /// task (US14.4): the startup sweep and the lifetime sweeper use it as
    /// the hard cap, counted continuously from task creation. `0` =
    /// unlimited; absent keeps the 4-hour default
    /// ([`DEFAULT_TASK_MAX_LIFETIME_S`]).
    #[serde(default, alias = "maxTaskLifetimeS")]
    pub max_task_lifetime_s: Option<u64>,
}

impl TaskConfig {
    /// Validate the numeric lower bounds (TS zod `.min()` parity): a config
    /// that violates them is rejected at load, exactly like the TS schema.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(max) = self.max_running_tasks
            && max == 0
        {
            return Err("task.maxRunningTasks must be >= 1".to_string());
        }
        if let Some(ceiling) = self.print_wait_ceiling_s
            && ceiling == 0
        {
            return Err("task.printWaitCeilingS must be >= 1".to_string());
        }
        if let Some(turns) = self.print_max_turns
            && turns == 0
        {
            return Err("task.printMaxTurns must be >= 1".to_string());
        }
        Ok(())
    }
}

/// The `[subagent]` section — the per-subagent timeout (D-65). Mirrors
/// the TS `SubagentConfigSchema` (`schema.ts:145-153`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// Per-subagent (`Agent` / `AgentSwarm`, foreground and background)
    /// timeout in milliseconds. `0` means no timeout; absent keeps the
    /// [`DEFAULT_SUBAGENT_TIMEOUT_MS`] default.
    #[serde(default, alias = "timeoutMs")]
    pub timeout_ms: Option<u64>,
    /// The nesting depth ceiling for subagent dispatch (B4 T5 / SDR-B4-03):
    /// a child may spawn grandchildren up to this depth (the main agent is
    /// depth 1). `0` means unlimited; absent keeps the default of
    /// [`DEFAULT_MAX_NESTING_DEPTH`]. Wired through
    /// `session_scope_config_from` so the refusal message's knob is real.
    #[serde(default, alias = "maxNestingDepth", alias = "max_nesting_depth")]
    pub max_nesting_depth: Option<u32>,
    /// The per-call item cap for `AgentSwarm` (SDR-012): a single swarm
    /// dispatches at most this many members. `0` means unlimited (mirrors
    /// `maxNestingDepth`); absent keeps the default of
    /// [`DEFAULT_MAX_SWARM_ITEMS`] (3). Wired through
    /// `session_scope_config_from` and overridable per call by the
    /// `MIRRI_AGENT_SWARM_MAX_ITEMS` env (env > config file > default).
    #[serde(default, alias = "maxSwarmItems", alias = "max_swarm_items")]
    pub max_swarm_items: Option<u32>,
}

/// Default per-subagent timeout: 2 hours (TS `taskService` default).
pub const DEFAULT_SUBAGENT_TIMEOUT_MS: u64 = 2 * 60 * 60 * 1000;

/// Default kill grace window: 5 seconds (TS `taskService.ts:764`).
pub const DEFAULT_KILL_GRACE_MS: u64 = 5_000;

/// `[model_catalog]` section: model-catalog behaviors (parse-only;
/// host-interpreted by the catalog loaders).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogConfig {
    /// Whether the model catalog's disk cache is persisted (adjudication
    /// D-M4: caching is enabled by default).
    #[serde(default = "default_persist_cache_true", alias = "persistCache")]
    pub persist_cache: bool,
}

/// Default for [`ModelCatalogConfig`]: caching is enabled (D-M4), matching
/// the serde default — the derived `Default` would yield `false` and
/// contradict the config-file semantic, so it is implemented manually.
impl Default for ModelCatalogConfig {
    fn default() -> Self {
        Self {
            persist_cache: default_persist_cache_true(),
        }
    }
}

/// Default for [`ModelCatalogConfig::persist_cache`]: caching is on unless
/// the config explicitly disables it (D-M4).
fn default_persist_cache_true() -> bool {
    true
}

/// A hot-reloadable section of [`ServerConfig`] that differs between the
/// loaded config and the previous in-memory snapshot.
///
/// Produced by [`ServerConfig::reload_changes`] when `config.toml` changes on
/// disk. Each variant names one section the hot-reload watcher applies to the
/// live server; [`ConfigChange::Hooks`] additionally replaces the hook
/// engine's live registrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigChange {
    /// `providers` / `default_provider` (including the legacy `[provider]`).
    Providers,
    /// `models` / `default_model` (including the legacy `[model]`).
    Models,
    /// `thinking`.
    Thinking,
    /// `permission` / `default_permission_mode`.
    Permission,
    /// `loop_control`.
    LoopControl,
    /// `yolo`.
    Yolo,
    /// `plan_mode` / `default_plan_mode`.
    PlanMode,
    /// `[[hooks]]`.
    Hooks,
}

impl ServerConfig {
    /// Resume-time capacity limits derived from the `[loop_control]` keys
    /// (`max_history_messages` / `max_deferred_messages`): the caps a
    /// session's live turns ran with (`TurnFlow` applies them via
    /// `set_limits` from its strategy every turn), for the resume /
    /// close-time-snapshot fold simulations. A missing section or absent
    /// keys keep the 10 000 / 1 000 production defaults.
    pub fn replay_limits(&self) -> mirri_proto::ReplayLimits {
        match &self.loop_control {
            Some(loop_control) => loop_control.replay_limits(),
            None => LoopControlConfig::default().replay_limits(),
        }
    }

    /// Load config from a TOML file, apply `${VAR}` expansion, then apply
    /// environment overrides.
    ///
    /// Equivalent to [`ServerConfig::load_without_env`] followed by
    /// [`ServerConfig::expand_env_vars`] and [`ServerConfig::apply_env`].
    /// The `${VAR}` expansion runs **before** `apply_env` so that a
    /// whole-field env override (e.g. `MIRRI_API_KEY`) still wins over an
    /// inline `${...}` reference in the file. File reads go through the
    /// injected [`SyncFs`] provider (zero-IO T13b).
    pub fn load(path: &Path, fs: &dyn SyncFs) -> EngineResult<Self> {
        let mut config = Self::load_without_env(path, fs)?;
        config.expand_env_vars();
        config.apply_env()?;
        Ok(config)
    }

    /// Load config from a TOML file, without applying environment overrides.
    ///
    /// A missing file is the fresh-environment case: the default config is
    /// returned (the TypeScript `readConfigFile` contract) so a caller that
    /// merges a patch into this base and saves creates the file at `path`
    /// instead of failing. An existing-but-unreadable or unparseable file
    /// still errors — a broken config must surface, never silently reset.
    /// Writes triggered from the loaded config go through the atomic write
    /// helper, which creates the parent directory.
    ///
    /// Use this instead of [`load`](Self::load) when the config is about to
    /// be written back to disk (e.g. a one-shot config migration): a rewrite
    /// must persist exactly what the file held, so env-provided values —
    /// such as an injected `MIRRI_API_KEY` — are never baked into
    /// `config.toml`. File reads go through the injected [`SyncFs`]
    /// provider.
    pub fn load_without_env(path: &Path, fs: &dyn SyncFs) -> EngineResult<Self> {
        let contents = match fs.read_text(path) {
            Ok(contents) => contents,
            Err(_missing) if !fs.exists(path) => {
                tracing::debug!(
                    path = %path.display(),
                    "no config.toml; using default config"
                );
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(EngineError::RequestInvalid(format!(
                    "cannot read config file '{}': {err}",
                    path.display()
                )));
            }
        };
        Self::from_toml(&contents)
    }

    /// Load config from a TOML string.
    ///
    /// Missing sections default to `None` (the engine then uses its built-in
    /// defaults). The original TOML document is preserved in [`ServerConfig::raw`]
    /// for round-trip fidelity (unknown keys survive save/load) and wire
    /// exposure (the `config.get` response includes `raw`).
    pub fn from_toml(toml_str: &str) -> EngineResult<Self> {
        let raw: toml::Value = toml::from_str(toml_str)
            .map_err(|err| EngineError::RequestInvalid(format!("invalid config TOML: {err}")))?;
        let mut config: ServerConfig = toml::from_str(toml_str)
            .map_err(|err| EngineError::RequestInvalid(format!("invalid config TOML: {err}")))?;
        config.raw = Some(raw);
        // Hard validation (US6.2): a config that sets mutually exclusive
        // keys — e.g. both `retention_tail_ratio` and
        // `retention_tail_tokens` — is rejected here, at load, instead of
        // being silently parsed and ignored. Every load path (boot, hot
        // reload, `config.set` re-read) funnels through this parse, so the
        // rejection surfaces wherever the config first enters the system.
        if let Some(loop_control) = &config.loop_control {
            loop_control
                .validate()
                .map_err(EngineError::RequestInvalid)?;
        }
        // Same hard validation for the typed lifecycle sections (D-65): a
        // `[task] maxRunningTasks = 0` fails the load, mirroring the TS
        // zod `.min(1)` rejection, instead of being parsed and ignored.
        if let Some(task) = &config.task {
            task.validate().map_err(EngineError::RequestInvalid)?;
        }
        // The legacy `[background]` base passes the same bounds when it
        // carries lifecycle fields (TS zod rejects them wherever they
        // are set — a `maxRunningTasks = 0` base must not load as valid
        // just because `[task]` masks it). An unparseable base is not a
        // load error here (passthrough legacy payload); the resolve path
        // warns about it.
        if let Some(base) = config.background_task_base() {
            base.validate().map_err(EngineError::RequestInvalid)?;
        }
        Ok(config)
    }

    /// Report which hot-reloadable sections of `self` differ from `previous`.
    ///
    /// This is the diff the config.toml hot-reload watcher uses to decide what
    /// changed on disk:
    /// - Comparisons cover exactly the reloadable sections: providers/models,
    ///   thinking, permission, loop control, yolo, plan mode, and hooks.
    /// - `experimental` is deliberately **excluded** — experimental feature
    ///   flags are restart-only by design, so an edit that touches only that
    ///   section returns an empty list and leaves the live config untouched.
    /// - Session-level configuration is not part of [`ServerConfig`] (it lives
    ///   on session scopes) and is therefore never touched here either.
    ///
    /// An empty result means "nothing reloadable changed": the caller must
    /// keep the previous config (and the hook registrations) in place.
    pub fn reload_changes(&self, previous: &ServerConfig) -> Vec<ConfigChange> {
        let mut changes = Vec::new();
        if self.provider != previous.provider
            || self.providers != previous.providers
            || self.default_provider != previous.default_provider
        {
            changes.push(ConfigChange::Providers);
        }
        if self.model != previous.model
            || self.models != previous.models
            || self.default_model != previous.default_model
        {
            changes.push(ConfigChange::Models);
        }
        if self.thinking != previous.thinking {
            changes.push(ConfigChange::Thinking);
        }
        if self.permission != previous.permission
            || self.default_permission_mode != previous.default_permission_mode
        {
            changes.push(ConfigChange::Permission);
        }
        if self.loop_control != previous.loop_control {
            changes.push(ConfigChange::LoopControl);
        }
        if self.yolo != previous.yolo {
            changes.push(ConfigChange::Yolo);
        }
        if self.plan_mode != previous.plan_mode
            || self.default_plan_mode != previous.default_plan_mode
        {
            changes.push(ConfigChange::PlanMode);
        }
        if self.hooks != previous.hooks {
            changes.push(ConfigChange::Hooks);
        }
        changes
    }

    /// Whether the model catalog cache persistence is enabled.
    ///
    /// The `[model_catalog]` section defaults to caching enabled when the
    /// section is absent (`persist_cache` defaults to `true`, D-M4). The
    /// flag is read at use time and is deliberately **not** part of the
    /// hot-reload diff ([`reload_changes`](Self::reload_changes) carries no
    /// variant for it): toggling it takes effect on the next catalog load.
    pub fn persist_cache_enabled(&self) -> bool {
        self.model_catalog
            .as_ref()
            .is_none_or(|catalog| catalog.persist_cache)
    }

    /// Parse the legacy `[background]` payload into the task-lifecycle
    /// fields it shares with `[task]` (D-65). `None` when `[background]` is
    /// absent or does not parse — `resolve_task_config` warns on the
    /// unparseable case, and the load-time bounds check below skips it.
    fn background_task_base(&self) -> Option<TaskConfig> {
        self.background.as_ref().and_then(|background| {
            serde_json::to_value(background)
                .ok()
                .and_then(|value| serde_json::from_value(value).ok())
        })
    }

    /// Resolve the effective `[task]` config (D-65).
    ///
    /// TS semantics (`configSection.ts:50-56`): the legacy `[background]`
    /// section is the base and `[task]` overrides it **per field** — an
    /// unset `[task]` field falls through to the `[background]` value.
    /// The `[background]` payload keeps its passthrough spelling, so its
    /// keys parse through the same camelCase aliases.
    pub fn resolve_task_config(&self) -> TaskConfig {
        // A mis-typed legacy value must not vanish silently — surface it,
        // then fall through to the defaults (the TS base is advisory the
        // same way). An absent `[background]` is simply the defaults.
        let base = if self.background.is_some() {
            self.background_task_base().unwrap_or_else(|| {
                tracing::warn!(
                    "failed to parse the legacy [background] section into \
                     task-lifecycle fields; using defaults for it"
                );
                TaskConfig::default()
            })
        } else {
            TaskConfig::default()
        };
        let mut resolved = base;
        if let Some(task) = &self.task {
            // Per-field override: only the fields `[task]` actually sets
            // win; the rest keep the `[background]` value.
            override_if_set(&mut resolved.max_running_tasks, &task.max_running_tasks);
            override_if_set(&mut resolved.keep_alive_on_exit, &task.keep_alive_on_exit);
            override_if_set(
                &mut resolved.bash_auto_background_on_timeout,
                &task.bash_auto_background_on_timeout,
            );
            override_if_set(&mut resolved.bash_task_timeout_s, &task.bash_task_timeout_s);
            override_if_set(
                &mut resolved.kill_grace_period_ms,
                &task.kill_grace_period_ms,
            );
            override_if_set(
                &mut resolved.print_wait_ceiling_s,
                &task.print_wait_ceiling_s,
            );
            override_if_set(
                &mut resolved.print_background_mode,
                &task.print_background_mode,
            );
            override_if_set(&mut resolved.print_max_turns, &task.print_max_turns);
            override_if_set(
                &mut resolved.heartbeat_interval_ms,
                &task.heartbeat_interval_ms,
            );
            override_if_set(
                &mut resolved.orphan_dispose_policy,
                &task.orphan_dispose_policy,
            );
            override_if_set(
                &mut resolved.max_task_lifetime_s,
                &task.max_task_lifetime_s,
            );
        }
        resolved
    }

    /// The resolved per-subagent timeout (D-65): the `[subagent]`
    /// `timeoutMs` when set (env can override it), else the TS default of
    /// 2 hours. `Duration::ZERO` is the unbounded sentinel — `timeoutMs = 0`
    /// means "no timeout" (TS parity), and callers that need an
    /// `Option<Duration>` convert zero to `None`.
    pub fn resolve_subagent_timeout(&self) -> std::time::Duration {
        match self.subagent.as_ref().and_then(|s| s.timeout_ms) {
            Some(ms) => std::time::Duration::from_millis(ms),
            None => std::time::Duration::from_millis(DEFAULT_SUBAGENT_TIMEOUT_MS),
        }
    }

    /// The resolved subagent nesting ceiling (B4 T5 / SDR-B4-03): the
    /// `[subagent]` `maxNestingDepth` when set, else the default of
    /// [`DEFAULT_MAX_NESTING_DEPTH`] (5). `0` means unlimited.
    pub fn resolve_subagent_max_nesting_depth(&self) -> u32 {
        self.subagent
            .as_ref()
            .and_then(|s| s.max_nesting_depth)
            .unwrap_or(crate::subagent::DEFAULT_MAX_NESTING_DEPTH)
    }

    /// The resolved `AgentSwarm` per-call item cap (SDR-012): the
    /// `[subagent]` `maxSwarmItems` when set, else the default of
    /// [`DEFAULT_MAX_SWARM_ITEMS`] (3). `0` means unlimited. The
    /// `MIRRI_AGENT_SWARM_MAX_ITEMS` env override is resolved at call time
    /// by the tool itself (it wins over this value when set).
    pub fn resolve_subagent_max_swarm_items(&self) -> u32 {
        self.subagent
            .as_ref()
            .and_then(|s| s.max_swarm_items)
            .unwrap_or(crate::subagent::DEFAULT_MAX_SWARM_ITEMS)
    }

    /// The resolved kill grace window (D-63): `task.killGracePeriodMs`
    /// when set, else the 5s TS default.
    pub fn resolve_kill_grace(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.resolve_task_config()
                .kill_grace_period_ms
                .unwrap_or(DEFAULT_KILL_GRACE_MS),
        )
    }

    /// The resolved orphan-disposal policy (SDR-013 c, US14.3): the
    /// `[task]` `orphanDisposePolicy` when set (env can override it), else
    /// the report-only default — the startup census never acts unless the
    /// operator opts in.
    pub fn resolve_orphan_dispose_policy(&self) -> OrphanDisposePolicy {
        self.resolve_task_config()
            .orphan_dispose_policy
            .unwrap_or(OrphanDisposePolicy::ReportOnly)
    }

    /// The resolved total lifetime ceiling for a process-shaped background
    /// task (US14.4): the `[task]` `maxTaskLifetimeS` when set (env can
    /// override it), else the 4-hour default. `0` resolves to
    /// `Duration::ZERO` — the unbounded sentinel consumers skip (the
    /// ceiling is disabled).
    pub fn resolve_max_task_lifetime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.resolve_task_config()
                .max_task_lifetime_s
                .unwrap_or(DEFAULT_TASK_MAX_LIFETIME_S),
        )
    }

    /// The deprecation notice for a config still carrying a non-empty
    /// `disabled_agents` key (R3-3), or `None` when the key is absent or
    /// empty.
    ///
    /// The key no longer drives agent enablement — the profile file's own
    /// `enabled = false` flag is the sole semantic source — but it still
    /// parses, round-trips, and stays exposed on the config wire, so a
    /// config carrying it must not break: the boot path logs this notice
    /// exactly once at startup (`tracing::warn!`), pointing the user at
    /// the file-level flag. Pure (no logging here) so the semantics are
    /// unit-testable; the caller owns emitting it.
    pub fn disabled_agents_deprecation_notice(&self) -> Option<String> {
        let agents = self.disabled_agents.as_ref()?;
        if agents.is_empty() {
            return None;
        }
        Some(format!(
            "config key `disabled_agents` ({}) no longer controls agent enablement and is \
             kept for compatibility only; use the profile file's own `enabled = false` flag \
             to disable an agent profile",
            agents.join(", ")
        ))
    }

    /// Report the human-readable config warnings behind
    /// `config.diagnostics` — the advisory consistency checks over the
    /// multi-provider sections.
    ///
    /// Covered today (each maps to a TS-side degradation the user would
    /// otherwise debug blind):
    /// - a `default_provider` that names no `[providers]` entry;
    /// - a `[models.*]` alias whose `provider` names no `[providers]` entry;
    /// - the legacy single-provider `[model]` section, with a pointer at
    ///   the multi-provider spelling that replaces it;
    /// - the retired `disabled_agents` key (D15, ADR-009): a config still
    ///   carrying a non-empty key gets the deprecation notice
    ///   ([`Self::disabled_agents_deprecation_notice`]) here — the same
    ///   text the boot path warns once — so the diagnostics face surfaces
    ///   it alongside the other advisory warnings.
    ///
    /// The list is advisory: nothing is refused, and an internally
    /// consistent config reports none.
    pub fn diagnostics_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let providers = self.providers.as_ref();

        if let Some(default_provider) = self
            .default_provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let known = providers
                .map(|map| {
                    map.keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if !providers.is_some_and(|map| map.contains_key(default_provider)) {
                warnings.push(format!(
                    "default_provider \"{default_provider}\" does not match any [providers] \
                     entry{known_hint}{known}. Point it at a configured provider or remove it.",
                    known_hint = if known.is_empty() {
                        ""
                    } else {
                        "; configured providers: "
                    },
                ));
            }
        }

        if let Some(models) = &self.models {
            for (alias, record) in models {
                let provider = record.provider.trim();
                if !provider.is_empty() && !providers.is_some_and(|map| map.contains_key(provider))
                {
                    warnings.push(format!(
                        "model alias \"{alias}\" points at provider \"{provider}\", which is \
                         not configured in [providers]. Remove the alias or configure the \
                         provider."
                    ));
                }
            }
        }

        if self.model.is_some() {
            warnings.push(
                "the legacy [model] section is deprecated; migrate to the multi-provider \
                 spelling (default_provider + default_model + [providers.*] + [models.*] \
                 aliases) — the legacy section keeps working but is not wired into the \
                 provider/model management surface."
                    .to_string(),
            );
        }

        // D15 (ADR-009): the retired `disabled_agents` key surfaces its
        // deprecation notice through the diagnostics face too (the boot
        // path already warns once at startup).
        if let Some(notice) = self.disabled_agents_deprecation_notice() {
            warnings.push(notice);
        }

        warnings
    }
}

/// Copy `src` into `dst` only when `src` is set — the per-field override
/// rule of [`ServerConfig::resolve_task_config`] (`[task]` wins over the
/// legacy `[background]` base only for the fields it actually sets).
fn override_if_set<T: Clone>(dst: &mut Option<T>, src: &Option<T>) {
    if src.is_some() {
        *dst = src.clone();
    }
}

/// Marker file recording which one-shot config migrations have been applied
/// (and when), stored in the Mirri home directory next to `config.toml`.
const MIGRATIONS_MARKER_FILE: &str = "migrations-effort.json";

/// Migration ID for `thinking.effort: "max"` → `"high"` (same id as the
/// TypeScript migration).
const MIGRATION_THINKING_EFFORT_MAX_TO_HIGH: &str = "thinking-effort-max-to-high";

/// Reserved provider key for the environment-synthesized provider.
///
/// Mirrors the TypeScript `ENV_MODEL_PROVIDER_KEY` in `config/env-model.ts`.
pub const ENV_MODEL_PROVIDER_KEY: &str = "__mirri_env__";

/// Reserved model alias key for the environment-synthesized model.
///
/// Mirrors the TypeScript `ENV_MODEL_ALIAS_KEY` in `config/env-model.ts`.
pub const ENV_MODEL_ALIAS_KEY: &str = "__mirri_env_model__";

/// Default max context size when `MIRRICODE_MODEL_MAX_CONTEXT_SIZE` is unset.
const ENV_MODEL_DEFAULT_MAX_CONTEXT_SIZE: u64 = 262_144;

/// Default capabilities when `MIRRICODE_MODEL_CAPABILITIES` is unset.
const ENV_MODEL_DEFAULT_CAPABILITIES: &[&str] = &["image_in", "thinking"];

/// Provider types allowed for `MIRRICODE_MODEL_PROVIDER_TYPE`.
const ENV_MODEL_ALLOWED_PROVIDER_TYPES: &[&str] = &["anthropic", "openai", "google-genai"];

/// Apply one-shot config migrations to `config`, persisting any change back
/// to `config.toml` and recording each applied migration in a marker file
/// under the Mirri home directory.
///
/// `config_path` is the path of the `config.toml` the config was loaded
/// from; `home_dir` is the Mirri data directory — the directory that
/// contains `config.toml` (e.g. `$HOME/.mirri-code`, or `$MIRRICODE_HOME`
/// when set). The marker file is `<home_dir>/migrations-effort.json`, a JSON
/// object mapping migration id → ISO-8601 timestamp. A migration whose id is
/// already recorded is skipped, so a value the user re-sets by hand
/// afterwards is never migrated again.
///
/// When a migration mutates the config, the mutated config is first
/// serialized back to TOML and written to `config_path` atomically
/// (write-to-temp + fsync + rename, mirroring
/// [`mirri_persistence::AtomicDocument`]). The marker is written only after
/// that rewrite succeeds: a failed rewrite leaves no marker, so the
/// migration is retried on the next boot. The in-memory `config` keeps the
/// migrated value either way, so the current run behaves as migrated.
///
/// Mirrors the TypeScript one-shot migration mechanism
/// (`migrations-effort.json`): the marker records that a migration has been
/// *considered* for this home, not that a value changed. Missing or corrupt
/// marker files are treated as "no migrations applied".
///
/// This runs once at startup and must never block it: it costs one file
/// read plus at most one config.toml rewrite and one marker write. All
/// marker and file reads/writes go through the injected [`SyncFs`] provider
/// (zero-IO T13b).
pub fn migrate_config(
    config: &mut ServerConfig,
    config_path: &Path,
    home_dir: &Path,
    fs: &dyn SyncFs,
) -> EngineResult<()> {
    let marker_path = home_dir.join(MIGRATIONS_MARKER_FILE);
    if read_migration_markers(&marker_path, fs).contains_key(MIGRATION_THINKING_EFFORT_MAX_TO_HIGH)
    {
        return Ok(());
    }

    // Persisted `thinking.effort = "max"` dates from when the UI recorded
    // any pick unconditionally. `max` is session-only now, so rewrite it to
    // `"high"` once, matching TypeScript. All other values are honored as-is.
    let mut effort_migrated = false;
    if let Some(thinking) = config.thinking.as_mut()
        && thinking.effort.as_deref() == Some("max")
    {
        thinking.effort = Some("high".to_string());
        effort_migrated = true;
        tracing::info!("migrated config: thinking.effort \"max\" -> \"high\"");
    }

    // Persist the migration into config.toml BEFORE recording the marker:
    // only a successful rewrite is followed by the marker write, so a failed
    // rewrite leaves the migration unmarked and it retries on the next boot.
    if effort_migrated {
        let contents = config.to_toml_string().map_err(|err| {
            EngineError::RequestInvalid(format!(
                "cannot serialize migrated config for '{}': {err}",
                config_path.display()
            ))
        })?;
        write_config_file_atomically(config_path, &contents)?;
    }

    write_migration_marker(&marker_path, MIGRATION_THINKING_EFFORT_MAX_TO_HIGH, fs)?;
    Ok(())
}

/// Write `contents` to `config_path` atomically and durably.
///
/// Delegates to [`mirri_persistence::write_file_atomically`]: write to a
/// unique sibling temp file (mode 0o600 — the file may hold provider
/// credentials), fsync it, then rename over `config_path` (atomic on the
/// same filesystem). The parent directory is fsynced best-effort afterwards
/// so the rename itself survives a crash; a failure there is logged, not
/// fatal, because the renamed file is already complete.
pub(crate) fn write_config_file_atomically(config_path: &Path, contents: &str) -> EngineResult<()> {
    mirri_persistence::write_file_atomically(config_path, contents.as_bytes())?;
    Ok(())
}

/// Read the migration marker file as a map of migration id → timestamp.
///
/// A missing or corrupt marker file is treated as "no migrations applied"
/// so the migrations run once. Reads go through the injected [`SyncFs`]
/// provider.
fn read_migration_markers(marker_path: &Path, fs: &dyn SyncFs) -> HashMap<String, String> {
    let Ok(contents) = fs.read_text(marker_path) else {
        return HashMap::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Record an applied migration in the marker file, preserving any markers
/// already present (e.g. written by an earlier migration run). Writes go
/// through the injected [`SyncFs`] provider.
fn write_migration_marker(
    marker_path: &Path,
    migration_id: &str,
    fs: &dyn SyncFs,
) -> EngineResult<()> {
    if let Some(parent) = marker_path.parent() {
        fs.mkdir(parent, true).map_err(sync_fs_io_error)?;
    }
    let mut markers = read_migration_markers(marker_path, fs);
    markers.insert(migration_id.to_string(), chrono::Utc::now().to_rfc3339());
    let contents = format!("{}\n", serde_json::to_string_pretty(&markers)?);
    fs.write_text(marker_path, &contents)
        .map_err(sync_fs_io_error)?;
    Ok(())
}

/// Map a [`SyncFs`] error onto the engine error the pre-trait code produced:
/// an underlying [`std::io::Error`] keeps the `Io` variant (with its source
/// chain), anything else becomes `RequestInvalid`.
fn sync_fs_io_error(err: anyhow::Error) -> EngineError {
    match err.downcast::<std::io::Error>() {
        Ok(io) => EngineError::Io(io),
        Err(err) => EngineError::RequestInvalid(err.to_string()),
    }
}

mod doctor;
mod env_layer;
mod home;
mod raw_toml;
mod wire;

#[cfg(test)]
mod tests;

pub use doctor::{CheckIssue, CheckReport, CheckStatus, validate_config_str};
pub use env_layer::{expand_env_string, expand_env_string_with};
pub use home::{mirri_home_dir, resolve_mirri_home_dir_from};
pub use wire::{
    camel_to_snake, convert_keys_to_camel_case, convert_keys_to_snake_case, json_to_toml_value,
    permission_rule_to_wire, redact_config_wire, resolve_model_alias_in, snake_to_camel,
    toml_to_json,
};
