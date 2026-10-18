//! Agent profile system.
//!
//! An [`AgentProfile`] defines an agent's model, system prompt, tools, and
//! behavior. Profiles are discovered from directories (the user agents roots
//! and the legacy `<mirri-home>/profiles/` — see [`discover_profiles`]) in
//! three file formats: `.md` agent files (first-class, the unified
//! frontmatter schema), and legacy `.toml` / `.json` files. Discovered
//! profiles are registered in a [`ProfileManager`] and resolved into an
//! [`AgentScopeConfig`] that the engine consumes.
//!
//! # Discovery semantics (R2-3, aligned with the TS `@mirri/agent-profile`)
//!
//! - **Formats and priority**: a `.md` file wins a same-name conflict with
//!   a `.toml`/`.json` file; between `.toml` and `.json` the `.toml` wins
//!   (the deterministic rule the pre-R2-3 sorted load already produced).
//!   `.yaml`/`.yml` are unsupported (the rust engine reads
//!   `.md`/`.toml`/`.json`) and surface as skipped entries with that
//!   reason — they never reach the `.md` parser, whose "missing frontmatter"
//!   error would be misleading. Files with other extensions are ignored by
//!   the scan (the TS `isAgentFileName` behavior).
//! - **Name key**: a profile is keyed by its declared name (the `.md`
//!   frontmatter `name` / the legacy `name` field), not by the file stem —
//!   the TS dedup semantics. The first registration wins: a higher-priority
//!   root outranks a lower one (the scan order is the source-priority
//!   order, computed inside [`discover_profiles`]), and within a root the
//!   extension priority above decides. A same-directory duplicate becomes
//!   a skipped entry; a
//!   cross-directory/cross-root duplicate is silently dropped (TS only
//!   reports same-directory duplicates).
//! - **Skip-and-collect**: one malformed file, one bad legacy `extends`
//!   (cycle/unknown/skipped parent), or one unsupported extension skips
//!   that file with `{path, reason}` — the rest of the directory still
//!   loads (the pre-R2-3 parse was all-or-nothing).
//! - **Flattening**: legacy `extends` declarations are flattened once, at
//!   load time, against the successfully-parsed set overlaid on the
//!   built-in profiles (R2-2). A parent that was itself skipped chains its
//!   reason into the child's skip entry.
//!
//! # Write channel (R4-1: `.md` read-modify-write)
//!
//! [`ProfileManager::upsert_profile`] / [`delete_profile`] persist under the
//! user agents root (`<mirri-home>/agents/`) in the unified `.md` format —
//! no longer the legacy `<dir>/<name>.toml`. An upsert over an existing
//! `.md` rewrites it surgically ([`write::rewrite_agent_md`]): only the
//! lines of the modified keys change, everything else passes through
//! byte-for-byte; a name with no `.md` yet gets a full serialization
//! ([`write::agent_file_def_for_profile`]). After a `.md` exists, the
//! same-name legacy `<profiles>/<name>.{toml,json}` files are removed (the
//! TS `removeLegacyFile` mirror) — the `.md` wins on read, so leaving them
//! behind would only seed stale suppressed candidates.
//!
//! Every manager is pre-registered with the built-in profiles (`agent`,
//! `coder`, `explore`, `plan`); a user profile with the same name overrides
//! the built-in (registration replaces by name) — the essential default
//! `agent` included (adjudication D16, see below).
//!
//! # Enablement source (R3-3): the file flag alone
//!
//! [`ServerConfig::disabled_agents`] is **retired** as an enablement
//! source. The key still parses, round-trips, and stays exposed on the
//! config wire (old config.toml files keep working — see
//! [`ServerConfig::disabled_agents_deprecation_notice`]), but the engine no
//! longer consults it: the profile file's own `enabled = false` flag
//! (after the essential exemption, [`is_file_disabled`]) is the sole
//! semantic source. A profile named in the config key with no file flag is
//! listed and dispatchable; the boot path warns once when the key is
//! non-empty.
//!
//! # Essential profile locking (R3-2, relaxed by D16)
//!
//! The default `agent` profile is **essential** (the TS
//! `DEFAULT_AGENT_PROFILE_NAME`): the core orchestrator dispatches through
//! it, so its **name** is protected — but its **content** is overridable by
//! a same-name user file (adjudication D16, ADR-006: a file is override
//! intent, no declaration required).
//!
//! - **Load/merge** — a discovered candidate declaring `name: "agent"`
//!   loads normally and overrides the built-in by priority, exactly like a
//!   non-essential built-in (`coder`, `explore`, `plan`): the user content
//!   registers, and the displaced built-in definition becomes the row's
//!   suppressed candidate (`{source: builtin, priority: 0, reason:
//!   priority}` — isomorphic with every covered builtin).
//! - **CRUD** — [`ProfileManager::delete_profile`] refuses the essential
//!   name at the engine level (the embedded REST handlers keep their
//!   earlier guards as redundant defense), and the lifecycle face refuses
//!   `disable`/`reset` while `enable` is a legal no-op. Updating/upserting
//!   the essential name is allowed: it writes the same-name `.md` and
//!   registers the merged content. No `override: true` declaration exists
//!   or is emitted anywhere (adjudication D14: a same-name file overrides
//!   by virtue of existing).
//! - **Dispatch** — an `enabled = false` on the essential profile warns
//!   and is treated as enabled ([`is_file_disabled`]), so dispatch through
//!   the default agent can never be bricked.
//!
//! # Example
//!
//! ```toml
//! name = "coder"
//! model = "claude-sonnet-4-20250514"
//! provider = "anthropic"
//! system_prompt = "You are a meticulous software engineer."
//! max_turns = 200
//! permission_mode = "yolo"
//! tools = ["Read", "Glob", "Grep", "Edit"]
//! thinking_effort = "high"
//! subagents = ["explorer"]
//! ```
//!
//! # Subagent allowlist (R3-4, adjudication D11-a)
//!
//! [`AgentProfile::subagents`] is a **name-dispatch allowlist** — the TS
//! `AgentProfile.subagents: readonly string[]`: the subagent *profile
//! names* this agent may dispatch through the `Agent`/`AgentSwarm` tools.
//! `None` (the default) means every dispatchable profile is allowed; an
//! empty list allows none. The pre-R3-4 model stored inline subagent
//! *definitions* (`[[subagents]]` tables with model/prompt/tool
//! overrides, consumed nowhere at dispatch); a legacy file declaring
//! those still loads — [`parse_profile_entry`] migrates them to their
//! names with a one-time per-file warning, at the same parse-time
//! normalization point the TS `parseSubagents` occupies
//! (`packages/agent-profile/src/parse.ts`). The `.md` bridge maps the
//! unified [`AgentFileDef::subagents`] name list through (the R2-era
//! "intentionally NOT mapped" gap). Dispatch enforcement and the
//! tool-description intersection resolve through
//! [`subagent_allowlist_for`] (the TS `subagentAllowlistFor` — a caller
//! with no own list runs as the default profile and inherits *its* list).
//!
//! # Multi-source merge projection (R3-5)
//!
//! Discovery mirrors the TS session catalog's per-name candidate loop
//! (`sessionAgentProfileCatalogService.ts:143-253`) onto its result:
//!
//! - **Source tags** ([`ProfileSource`]): every root is scanned as a
//!   [`ProfileSourceRoot`] carrying the TS wire `source` enum value its
//!   profiles report. Rust roots are the user chain (the brand agents
//!   dir, the generic `~/.agents/agents`, the legacy `profiles/` — the
//!   closest TS semantic for a home-dir profile is `user`) plus the
//!   plugin roots (item H, host-fed from `pluginAgentRoots()`);
//!   `project` / `extra` / `explicit` are reserved values an R4 root can
//!   carry without a discovery or wire change. Builtins never flow through
//!   discovery — the manager tags them at seeding ([`ProfileProvenance`]).
//! - **Winner order**: the winner order is the priority order — roots are
//!   scanned by source priority, highest first (equal-priority roots keep
//!   the caller's order: the user chain brand, generic, legacy), then the
//!   extension rank within a root. The first **enabled** candidate for a
//!   name wins; an `enabled: false` candidate never wins, not even over
//!   a lower-priority one (the TS `:192-204`) — **except** the essential
//!   name, whose file flag is exempt ([`is_file_disabled`], D16): an
//!   `enabled: false` on the essential warns and still wins, so the
//!   default dispatch target can never be bricked.
//! - **Disabled fallbacks** (the TS `!winner` cases): when every candidate
//!   for a name is disabled, a non-essential **built-in** name keeps its
//!   built-in registration (case a — the disabled candidates attach to the
//!   built-in's row), and a non-builtin name registers its first disabled
//!   candidate (case b — the TS `disabledWinners`: listed-but-marked, see
//!   [`ProfileManager::list_all`]).
//! - **Suppressed candidates** ([`SuppressedProfileCandidate`]): the
//!   same-name losers attach to the winner per name with the TS reason
//!   enum — cross-root losers carry `priority`, disabled candidates
//!   processed before the winner carry `disabled`, and a user file that
//!   covers a built-in name displaces the built-in definition into the
//!   winner's suppressed list (`{source: builtin, priority: 0, reason:
//!   priority}`, D16 — isomorphic for the essential and every non-essential
//!   built-in). Same-root losers are a silent debug drop (D13: the skip
//!   channel carries load failures only, and the TS model has no
//!   same-entry suppressed candidates, so none is invented for them). The
//!   manager retains the projection
//!   ([`ProfileManager::suppressed_candidates`]) alongside the per-name
//!   provenance for the wire catalog.
//!
//! # Skip channel invariant (D13, ADR-008)
//!
//! [`ProfileDiscoveryResult::skipped`] carries **load failures only** —
//! parse failures, unreadable files, unsupported extensions, unreadable
//! subdirectories, and bad legacy `extends` chains. A valid file that lost
//! a merge never enters it: cross-root losers surface in
//! [`ProfileDiscoveryResult::suppressed`], same-root duplicates are
//! silently dropped (matching the TS loader's within-entry dedup).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mirri_os::SyncFs;

use crate::error::{EngineError, EngineResult};
use crate::permission::PermissionMode;
use crate::scope::config::{AgentScopeConfig, ProviderType};

/// Agent profile `.md` file parsing/serialization (frontmatter + Markdown
/// body) — the pure-function port of the TS `@mirri/agent-profile` `.md`
/// handling. Zero IO.
pub mod agent_md;

/// Line-level surgical rewrite for agent `.md` files — the R4-1 write
/// channel's round-trip-fidelity core. Pure functions, zero IO.
pub mod write;

pub use agent_md::{
    AGENT_NAME_MAX_LENGTH, AgentFileDef, AgentFileParseError, AgentFileSource, is_kebab_case,
    parse_agent_file_text, parse_agent_file_text_with_source, parse_frontmatter,
    serialize_agent_file,
};

pub use write::{
    AgentMdRewrite, FieldEdit, FieldValue, agent_file_def_for_profile,
    agent_md_rewrite_for_existing, rewrite_agent_md,
};

/// An agent profile — defines the agent's model, system prompt, tools, and
/// behavior.
///
/// All fields except `name` and `model` are optional; [`ProfileManager::resolve`]
/// applies engine defaults for the missing ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentProfile {
    /// Profile name (the key used by `ProfileManager`).
    pub name: String,
    /// Model ID for LLM calls (e.g. `"claude-sonnet-4-20250514"`).
    #[serde(default)]
    pub model: String,
    /// Provider name (`"anthropic"`, `"openai"`, `"openai-responses"`, `"google"`, `"custom"`).
    pub provider: Option<String>,
    /// Inline system prompt.
    pub system_prompt: Option<String>,
    /// Path to a file whose contents are used as the system prompt.
    ///
    /// When both `system_prompt` and `system_prompt_file` are set, the file
    /// wins (a warning is logged).
    pub system_prompt_file: Option<PathBuf>,
    /// Maximum turns for the agent's loop. Defaults to 200.
    pub max_turns: Option<u32>,
    /// Maximum steps per turn. Defaults to `None` (engine default).
    pub max_steps_per_turn: Option<u32>,
    /// Permission mode (`"manual"`, `"yolo"`, `"auto"`). Defaults to
    /// [`PermissionMode::Manual`].
    pub permission_mode: Option<PermissionMode>,
    /// Tool allowlist — tool names the agent may use. When `None`, all
    /// built-in tools are enabled.
    ///
    /// Serialized as `tools` (the unified `AgentFileDef` terminology); the
    /// legacy `enabled_tools` spelling written by older versions is still
    /// accepted on read via a serde alias, but never re-emitted. The alias is
    /// **read-compat only**: a file declaring both spellings in the same
    /// table/map is rejected with serde's duplicate-field error (one value
    /// would silently win otherwise) — see the
    /// `should_skip_the_file_when_both_tool_spellings_are_present` test.
    #[serde(default, alias = "enabled_tools")]
    pub tools: Option<Vec<String>>,
    /// Tool denylist — tool names the agent may not use.
    ///
    /// Serialized as `disallowed_tools`; the legacy `disabled_tools` spelling
    /// is still accepted on read via a serde alias, but never re-emitted (the
    /// same read-only, co-presence-rejected alias contract as `tools` /
    /// `enabled_tools` above).
    #[serde(default, alias = "disabled_tools")]
    pub disallowed_tools: Option<Vec<String>>,
    /// Capabilities the agent requires (reserved for host-side checks).
    pub capabilities_required: Option<Vec<String>>,
    /// How much reasoning effort the model should invest per response.
    pub thinking_effort: Option<ThinkingEffort>,
    /// Subagent dispatch allowlist — the subagent **profile names** this
    /// agent may dispatch through the `Agent`/`AgentSwarm` tools (R3-4, the
    /// TS `AgentProfile.subagents: readonly string[]`).
    ///
    /// `None` (the default) means every dispatchable profile is allowed;
    /// an empty list allows none. This is a *name* list, not the pre-R3-4
    /// inline *definition* shape (`[[subagents]]` tables with model /
    /// prompt / tool overrides): a legacy file declaring those still loads
    /// — [`parse_profile_entry`]'s migration extracts the names (see the
    /// [module docs](self)). Consumed at dispatch through
    /// [`subagent_allowlist_for`].
    pub subagents: Option<Vec<String>>,
    /// Human-readable description of the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Guidance on when to use this profile (shown to the LLM when it picks
    /// a subagent profile).
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "whenToUse")]
    pub when_to_use: Option<String>,
    /// Default model for this profile (used when no model is explicitly specified).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "defaultModel"
    )]
    pub default_model: Option<String>,
    /// Model capabilities (e.g. ["text", "vision"]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Template variables for system prompt (e.g. {"lang": "rust"} → {{lang}} → "rust").
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "promptVars")]
    pub prompt_vars: Option<HashMap<String, String>>,
    /// Whether the profile is enabled. `None` (the default) and
    /// `Some(true)` both mean enabled; only `Some(false)` disables.
    ///
    /// Mirrors the TS semantics: the flag is persisted only when
    /// `Some(false)`, so `None` and `Some(true)` serialize to nothing.
    /// Engine consumption: dispatch filtering and the resolve error
    /// texts landed in R3-1 ([`ProfileManager::list`] /
    /// [`ProfileManager::resolve`]); the essential-profile lock is
    /// Task 16.
    #[serde(default, skip_serializing_if = "skip_enabled")]
    pub enabled: Option<bool>,
    /// Summary distillation policy for subagent turns (D-61, the TS
    /// `AgentProfile.summaryPolicy`). When set, the subagent host re-runs
    /// short final summaries through continuation turns until they meet
    /// `min_chars` (see `host::distill_summary`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "summaryPolicy",
        alias = "summary_policy"
    )]
    pub summary_policy: Option<ProfileSummaryPolicy>,
    /// Static prompt prefix prepended to every fresh (non-resume) dispatch
    /// prompt for this profile (D-47, the TS `AgentProfile.promptPrefix` at
    /// its simplest — a plain string, not the dynamic function form;
    /// recorded as a deliberate deviation in SDR-B4-01). Resume dispatches
    /// take the caller's prompt verbatim — TS parity (`agentTool.ts` applies
    /// `applyProfilePromptPrefix` only on the create branch).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "promptPrefix",
        alias = "prompt_prefix"
    )]
    pub prompt_prefix: Option<String>,
}

/// Summary distillation policy (D-61) — TS `agentProfileCatalog.ts:48-52`
/// counterpart: `minChars` / `continuationPrompt` / `retries`.
///
/// A subagent's final summary shorter than `min_chars` is re-distilled by
/// continuing the turn with `continuation_prompt`, at most `retries` times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileSummaryPolicy {
    /// Minimum character length of an acceptable final summary.
    #[serde(rename = "minChars", alias = "min_chars", default)]
    pub min_chars: usize,
    /// The prompt used to continue a turn whose summary was too short.
    #[serde(rename = "continuationPrompt", alias = "continuation_prompt", default)]
    pub continuation_prompt: String,
    /// Maximum number of continuation attempts before the short summary
    /// is accepted as-is.
    #[serde(default)]
    pub retries: u32,
}

/// serde skip predicate for [`AgentProfile::enabled`]: the flag is
/// persisted only when `Some(false)` — `None` and `Some(true)` both mean
/// enabled and serialize to nothing (the TS semantics).
fn skip_enabled(enabled: &Option<bool>) -> bool {
    !matches!(enabled, Some(false))
}

impl From<AgentFileDef> for AgentProfile {
    /// Adapt a parsed `.md` agent file (the unified [`AgentFileDef`]
    /// schema) into the engine's profile model.
    ///
    /// Field mapping: the Markdown `prompt` body becomes the inline
    /// `system_prompt`; the frontmatter `tools` / `disallowedTools` land in
    /// the renamed `tools` / `disallowed_tools`; `enabled` flows through
    /// (`None` = enabled). `default_model` maps to both `model` and
    /// `default_model` — the unified schema has a single model notion and
    /// the engine resolves against `model`. The unified `subagents` (a
    /// name list) maps onto the engine's R3-4 name-dispatch allowlist —
    /// the R2-era "intentionally NOT mapped" divergence is closed.
    /// `file_path` and `source` are provenance owned by the discovery
    /// layer (Task 10), not the model.
    fn from(def: AgentFileDef) -> Self {
        AgentProfile {
            name: def.name,
            model: def.default_model.clone().unwrap_or_default(),
            description: Some(def.description),
            system_prompt: Some(def.prompt),
            when_to_use: def.when_to_use,
            default_model: def.default_model,
            tools: def.tools,
            disallowed_tools: def.disallowed_tools,
            subagents: def.subagents,
            capabilities_required: def.capabilities_required,
            enabled: def.enabled,
            ..Default::default()
        }
    }
}

/// The level of reasoning effort the model should invest per response.
///
/// The TS-side `ThinkingEffort` vocabulary (`packages/kosong/src/provider.ts`)
/// is `'off' | 'on' | (string & {})`: the four canonical levels plus any
/// model-declared effort string carried verbatim (e.g. `"max"`, `"xhigh"`,
/// advertised through the model catalog's `support_efforts`). This enum
/// mirrors that contract — the canonical spellings stay typed, everything
/// else rides [`ThinkingEffort::Custom`]. Deserialization normalizes (trim +
/// lowercase) and accepts the kosong spellings `off` → `None` and `on` →
/// `Medium`; serialization keeps the rust spellings
/// (`none`/`low`/`medium`/`high`) and passes custom efforts through raw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThinkingEffort {
    /// No extended reasoning.
    #[default]
    None,
    /// Light reasoning.
    Low,
    /// Default reasoning.
    Medium,
    /// Extensive reasoning.
    High,
    /// A model-declared effort the engine does not enumerate (e.g. `"max"`,
    /// `"xhigh"`), carried verbatim to the provider adapters — the kosong
    /// `(string & {})` escape hatch.
    Custom(String),
}

impl ThinkingEffort {
    /// The wire spelling: the canonical `none`/`low`/`medium`/`high`, or
    /// the raw model-declared string for [`ThinkingEffort::Custom`].
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Custom(effort) => effort,
        }
    }
}

impl Serialize for ThinkingEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ThinkingEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = <String as Deserialize>::deserialize(deserializer)?;
        // The TS `normalizeRequestedThinkingEffort` (trim + lowercase) —
        // kosong-canonical spellings, case, and padding all fold.
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" => Err(serde::de::Error::custom(
                "thinking effort must be a non-empty string",
            )),
            "none" | "off" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" | "on" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Ok(Self::Custom(normalized)),
        }
    }
}

/// The effective subagent-dispatch allowlist for a caller — the rust port
/// of the TS `subagentAllowlistFor`
/// (`packages/agent-core-v2/src/app/agentProfileCatalog/profile-shared.ts:47-57`).
///
/// Resolution order, mirrored exactly:
///
/// - A caller **bound to a profile** (`Some(name)`) uses that profile's own
///   `subagents` list — `None` means *all dispatchable profiles are
///   allowed* (a named caller with no own list does **not** inherit the
///   default's).
/// - A caller **without a profile** (the session's main agent, which has
///   no profile in the rust engine) runs as the default — it uses the
///   DEFAULT profile's list, the essential [`ESSENTIAL_PROFILE_NAME`].
///
/// The allowlist is consulted live (per spawn / per description build), so
/// profile hot-reloads take effect on the next dispatch — the TS engine
/// resolves at launch time the same way.
pub fn subagent_allowlist_for(
    manager: &ProfileManager,
    caller_profile_name: Option<&str>,
) -> Option<Vec<String>> {
    match caller_profile_name {
        Some(name) => manager.get(name),
        None => manager.get(ESSENTIAL_PROFILE_NAME),
    }
    .and_then(|profile| profile.subagents)
}

/// The subagent-dispatch refusal message — the TS
/// `subagentTypeNotAllowedMessage` (`profile-shared.ts:59-65`), mirrored
/// character-for-character so the LLM sees the same self-correction text
/// on both engines (an empty allowlist renders as `none`).
pub fn subagent_type_not_allowed_message(name: &str, allowlist: &[String]) -> String {
    let allowed = if allowlist.is_empty() {
        "none".to_string()
    } else {
        allowlist.join(", ")
    };
    format!(
        "Subagent type \"{name}\" is not allowed for this agent. \
         Allowed subagent types: {allowed}."
    )
}

/// Profile manager — loads profiles from files and resolves them.
///
/// Registration is thread-safe: profiles live behind a `RwLock`, so a single
/// `ProfileManager` can be shared across sessions and tasks. Registering a
/// profile whose name already exists replaces the previous definition.
///
/// Every manager is pre-registered with the built-in profiles (`agent`,
/// `coder`, `explore`, `plan`); a user profile loaded later with the same
/// name overrides the built-in. Enablement comes from the file-level
/// `enabled` flag alone (R3-3) — the config's `disabled_agents` key is
/// inert data (see the [module docs](self)).
pub struct ProfileManager {
    profiles: RwLock<HashMap<String, AgentProfile>>,
    /// Files skipped by the most recent directory/roots load (skip-and-
    /// collect, R2-3). Kept so downstream surfaces (the R3 wire extension)
    /// can report them; refreshed wholesale by every successful load.
    last_skipped: RwLock<Vec<SkippedProfileFile>>,
    /// Provenance per registered name (R3-5): the TS source tag and, for
    /// file-sourced profiles, the discovered file — seeded with the
    /// built-ins at construction and updated by every registration channel
    /// (discovery, [`Self::load_from_file`], [`Self::upsert_profile`]).
    /// The wire catalog projects `source` / `file_path` from here.
    provenance: RwLock<HashMap<String, ProfileProvenance>>,
    /// Same-name candidates that lost the merge, per winner name (R3-5) —
    /// refreshed wholesale by every roots load, mirroring
    /// [`Self::last_skipped`] (and by the hot-reload watcher through
    /// [`Self::set_suppressed_candidates`]). Entries may key a built-in
    /// name (a disabled override that lost to the still-registered
    /// built-in — the TS case a) or be **keyed by a user file that covered
    /// a built-in name** (D16: the displaced built-in definition itself is
    /// the losing candidate).
    suppressed: RwLock<HashMap<String, Vec<SuppressedProfileCandidate>>>,
}

/// The names of the built-in profiles every [`ProfileManager`] registers
/// (see [`builtin_profiles`]).
pub const BUILTIN_PROFILE_NAMES: &[&str] = &["agent", "coder", "explore", "plan"];

/// The name of the essential default profile — the TS
/// `DEFAULT_AGENT_PROFILE_NAME` (`agentProfileCatalog.ts`).
///
/// The essential profile is the core orchestrator's fallback dispatch
/// target. Its **name** is protected (it cannot be deleted or disabled),
/// while its **content** is overridable by a same-name user file — see the
/// [module docs](self) for the full D16 semantics.
pub const ESSENTIAL_PROFILE_NAME: &str = "agent";

/// The stable skip token the **defensive** registration arm of
/// [`ProfileManager::load_from_roots`] normalizes an essential-name
/// registration failure into (ADR-008 retains that normalization).
///
/// Post-D16 the registration channels accept the essential name, so this
/// token is no longer produced by the happy path anywhere (discovery
/// loads same-name files as overrides, and the skip channel carries load
/// failures only — D13). It survives as the wire-stable spelling for the
/// defense-in-depth arm: a future register guard must never surface a
/// free-text engine error as a skip reason consumers match by token.
pub const ESSENTIAL_OVERRIDE_SKIP_REASON: &str = "builtin-override-required";

/// Whether `name` is one of the built-in profiles.
///
/// The REST profile CRUD uses this to project `source` / `has_override`
/// provenance and to refuse deleting a built-in definition.
pub fn is_builtin_profile_name(name: &str) -> bool {
    BUILTIN_PROFILE_NAMES.contains(&name)
}

/// Whether the file-level `enabled = false` flag on `profile` actually
/// disables it, after the essential-profile exemption.
///
/// The exemption (TS `sessionAgentProfileCatalogService.ts:192-200`): the
/// essential default `agent` profile cannot be disabled by its own file
/// flag — the flag **warns once (process-wide latch, T16 m2)** and is
/// treated as enabled, so dispatch through the default agent can never be
/// bricked. The TS engine warns once at catalog merge time; this function
/// is consulted on every read (the dispatch listing, the resolve gate,
/// the wire projection), so the warn is gated by a static
/// [`AtomicBool`] latch — the first consult warns, every later one stays
/// silent (the text is unchanged). Every consumer
/// of the file-level flag goes through here: the dispatch listing
/// ([`ProfileManager::list`]), the dispatch gate
/// ([`ProfileManager::resolve`] / [`ProfileManager::resolve_profile`]),
/// and the embedded wire projection (which therefore shows `enabled: true`
/// for the essential, matching the TS catalog).
pub fn is_file_disabled(profile: &AgentProfile) -> bool {
    if profile.enabled != Some(false) {
        return false;
    }
    if profile.name == ESSENTIAL_PROFILE_NAME {
        // T16 m2: warn-once latch — this function is a hot read path, and
        // only the essential name can reach this arm, so a single static
        // flag is the exact-lifetime equivalent of the TS merge-time warn.
        static ESSENTIAL_DISABLED_WARNED: AtomicBool = AtomicBool::new(false);
        if !ESSENTIAL_DISABLED_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                profile = %profile.name,
                "the essential default agent profile sets enabled = false but cannot be \
                 disabled; treating it as enabled"
            );
        }
        false
    } else {
        true
    }
}

impl ProfileManager {
    /// Create a profile manager pre-registered with the built-in profiles
    /// (`agent`, `coder`, `explore`, `plan`) and no disabled profiles.
    pub fn new() -> Self {
        let mut profiles = HashMap::new();
        for profile in builtin_profiles() {
            profiles.insert(profile.name.clone(), profile);
        }
        Self {
            profiles: RwLock::new(profiles),
            last_skipped: RwLock::new(Vec::new()),
            provenance: RwLock::new(builtin_provenance()),
            suppressed: RwLock::new(HashMap::new()),
        }
    }

    /// Load a profile from a TOML, JSON, or Markdown file.
    ///
    /// The file format is chosen by extension (`.toml`, `.json`, or `.md`;
    /// extension matching is case-insensitive). A legacy `extends`
    /// declaration is flattened against the built-in profiles at this point
    /// (a single file cannot see its directory siblings — load the
    /// directory with [`Self::load_from_dir`] for that). Returns the name
    /// of the registered profile. File reads go through the injected
    /// [`SyncFs`] provider (zero-IO T13b; callers pass `LocalSyncFs` for
    /// the engine default). The registration carries the file as its
    /// provenance (R3-5: user-sourced, `.md`/`.toml` file path).
    ///
    /// A file declaring the essential name ([`ESSENTIAL_PROFILE_NAME`])
    /// registers as the content override of the built-in (D16) — same as
    /// any non-essential built-in name.
    pub fn load_from_file(&self, path: &Path, fs: &dyn SyncFs) -> EngineResult<String> {
        let profile = parse_profile_file(path, fs)?;
        let name = profile.name.clone();
        self.register_discovered(&DiscoveredProfile {
            profile,
            path: path.to_path_buf(),
            source: ProfileSource::User,
        })?;
        Ok(name)
    }

    /// Load all profiles from a directory (e.g. `.mirri-code/profiles/`).
    ///
    /// Every profile file in the directory tree is discovered with the R2-3
    /// semantics (see the [module docs](self)): `.md` first-class, legacy
    /// `.toml`/`.json`, skip-and-collect for malformed/unsupported files.
    /// Returns the number of profiles loaded. The only hard failure is a
    /// root directory that exists but cannot be listed; a missing root is
    /// "nothing here" (the TS semantics) and loads zero profiles.
    pub fn load_from_dir(&self, dir: &Path, fs: &dyn SyncFs) -> EngineResult<usize> {
        let roots = [dir.to_path_buf()];
        self.load_from_roots(&roots, fs)
    }

    /// Load all profiles from several root directories (the TS
    /// `discoverAgentFiles` roots model).
    ///
    /// Every root is tagged [`ProfileSource::User`] — the alias of
    /// [`Self::load_from_source_roots`] for bare paths (the historical rust
    /// boot chain: the brand agents dir, the generic one, and the legacy
    /// `profiles/` — see [`user_profile_root_candidates`]).
    pub fn load_from_roots(&self, roots: &[PathBuf], fs: &dyn SyncFs) -> EngineResult<usize> {
        let tagged: Vec<ProfileSourceRoot> = roots.iter().cloned().map(Into::into).collect();
        self.load_from_source_roots(&tagged, fs)
    }

    /// Load all profiles from several **source-tagged** root directories
    /// (the TS `discoverAgentFiles` roots model, R3-5).
    ///
    /// Roots are scanned in **source-priority order** (highest first,
    /// computed inside [`discover_profiles`] — the caller's argument order
    /// is irrelevant for same-name conflict resolution, though equal-
    /// priority roots keep their relative order: the rust user-level chain
    /// is the brand agents dir, then the generic one, then the legacy
    /// `profiles/` — see [`user_profile_root_candidates`]); the first
    /// **enabled** registration of a profile name wins, so a
    /// higher-priority root outranks a lower one (the TS priority model:
    /// builtin 0, plugin 5, user 10, extra 20, project/workspace 30,
    /// explicit 40) and an `enabled = false` candidate
    /// never wins (the R3-5 TS alignment — see the [module docs](self)).
    /// A plugin root (item H — the TS `pluginAgentRoots` channel) passes a
    /// [`ProfileSourceRoot`] tagged [`ProfileSource::Plugin`] so every
    /// profile found under it registers with the `plugin` source and the
    /// same-name merge keeps the plugin priority.
    /// Malformed or unsupported files are skipped with a logged reason
    /// (and remembered for [`Self::last_skipped_files`], the R3 seam — D13:
    /// load failures only, never a suppressed legal file); the rest load
    /// with their root's source tag and file as provenance
    /// ([`Self::provenance`]), and the same-name losers land in the
    /// suppressed-candidates projection ([`Self::suppressed_candidates`]),
    /// both refreshed wholesale by this load. Returns the number of
    /// profiles loaded.
    ///
    /// **Error contract**: the scan aborts (returns [`Err`]) only when a
    /// root directory exists but cannot be listed — a missing root is
    /// "nothing here" and loads zero profiles, and every other failure
    /// (an unreadable subdirectory, a malformed/unsupported/unreadable
    /// file, a bad legacy `extends`) is collected into the skip channel
    /// and logged as a warn, never failing the load.
    pub fn load_from_source_roots(
        &self,
        roots: &[ProfileSourceRoot],
        fs: &dyn SyncFs,
    ) -> EngineResult<usize> {
        let result = discover_profiles(roots, fs)?;
        let mut skipped = result.skipped;
        let mut loaded = 0;
        for discovered in &result.profiles {
            // Defense in depth (ADR-008 keeps this normalization): the
            // registration channels accept every discovered name today —
            // including the essential (D16) — so the error arm guards only
            // against a future register guard turning a directory load into
            // a hard failure (the skip-and-collect contract). The essential
            // case normalizes to the stable wire token
            // ([`ESSENTIAL_OVERRIDE_SKIP_REASON`]) — consumers match by
            // token, so an essential refusal must never surface as the
            // engine error's free text; the underlying error is logged
            // here, so nothing is lost silently.
            match self.register_discovered(discovered) {
                Ok(()) => loaded += 1,
                Err(err) => {
                    let reason = if discovered.profile.name == ESSENTIAL_PROFILE_NAME {
                        tracing::warn!(
                            error = %err,
                            path = %discovered.path.display(),
                            "essential profile registration failed; \
                             reporting the stable skip token instead of the error"
                        );
                        ESSENTIAL_OVERRIDE_SKIP_REASON.to_string()
                    } else {
                        err.to_string()
                    };
                    skipped.push(SkippedProfileFile {
                        path: discovered.path.clone(),
                        reason,
                    });
                }
            }
        }
        for skipped_file in &skipped {
            tracing::warn!(
                path = %skipped_file.path.display(),
                reason = %skipped_file.reason,
                "skipped agent profile file during discovery"
            );
        }
        *self
            .last_skipped
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = skipped;
        *self
            .suppressed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = result.suppressed;
        Ok(loaded)
    }

    /// The files skipped by the most recent [`Self::load_from_dir`] /
    /// [`Self::load_from_roots`] pass, with their reasons.
    ///
    /// R3 seam: the discovery pass collects skip diagnostics so a wire
    /// surface can report them; this is where they are kept flowing until
    /// then. Empty before the first load.
    pub fn last_skipped_files(&self) -> Vec<SkippedProfileFile> {
        self.last_skipped
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The provenance of a registered profile — its TS wire `source` tag
    /// and, for file-sourced profiles, the file it was discovered from
    /// (`.md` provenance). Built-ins carry `{builtin, None}`; `None` when
    /// the name is not registered.
    pub fn provenance(&self, name: &str) -> Option<ProfileProvenance> {
        self.provenance
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    /// The same-name candidates that lost the merge to `name`'s winner, in
    /// encounter order — empty when the name won outright or was never
    /// shadowed. The R3-5 projection the wire row emits as
    /// `suppressed_candidates`.
    pub fn suppressed_candidates(&self, name: &str) -> Vec<SuppressedProfileCandidate> {
        self.suppressed
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the suppressed-candidates projection with the map from the
    /// latest discovery pass — the hot-reload counterpart of the
    /// bookkeeping [`Self::load_from_roots`] performs, for reload paths
    /// that register through [`Self::clear`] +
    /// [`Self::register_discovered`] (the watcher's last-known-good
    /// merge). Entries for names that no longer have losers linger until
    /// the next refresh only if the caller passes them; pass the current
    /// pass's map wholesale.
    pub fn set_suppressed_candidates(
        &self,
        suppressed: HashMap<String, Vec<SuppressedProfileCandidate>>,
    ) {
        *self
            .suppressed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = suppressed;
    }

    /// Register a profile directly.
    ///
    /// Registering a name that is already registered replaces the previous
    /// definition (so a user profile with the same name overrides a
    /// built-in — the essential default included, D16: its content is
    /// overridable, only its name is protected against delete/disable).
    ///
    /// No `override` declaration exists anywhere (adjudication D14: a
    /// same-name definition overrides by virtue of existing).
    ///
    /// Provenance (R3-5): a direct registration has no file of its own,
    /// so the name keeps whatever provenance it already carries (a
    /// re-registration never invents new provenance — a `register` over a
    /// built-in without a file keeps the built-in tag) and is otherwise
    /// tagged user-level, the closest TS semantic for an engine-side
    /// registration (`explicit` stays reserved for R4's `--agent-file`
    /// channel, which will carry a path). File-sourced channels use
    /// [`Self::register_discovered`].
    pub fn register(&self, profile: AgentProfile) -> EngineResult<()> {
        tracing::debug!(name = %profile.name, "agent profile registered");
        let name = profile.name.clone();
        {
            let mut guard = self
                .profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.insert(name.clone(), profile);
        }
        let mut provenance = self
            .provenance
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        provenance.entry(name).or_insert(ProfileProvenance {
            source: ProfileSource::User,
            file_path: None,
        });
        Ok(())
    }

    /// Register a discovered profile **with its discovery provenance** —
    /// the source tag of the root it was found under plus the file itself,
    /// so the wire catalog can report `source` / `file_path` (R3-5). The
    /// registration channel of [`Self::load_from_roots`] and the
    /// hot-reload watcher. The essential name registers like any other
    /// (D16: content override); provenance is only written after the
    /// registration succeeded.
    pub fn register_discovered(&self, discovered: &DiscoveredProfile) -> EngineResult<()> {
        tracing::debug!(
            name = %discovered.profile.name,
            source = discovered.source.as_str(),
            "agent profile registered"
        );
        let name = discovered.profile.name.clone();
        {
            let mut guard = self
                .profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.insert(name.clone(), discovered.profile.clone());
        }
        let mut provenance = self
            .provenance
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        provenance.insert(
            name,
            ProfileProvenance {
                source: discovered.source,
                file_path: Some(discovered.path.clone()),
            },
        );
        Ok(())
    }

    /// Register a profile **and persist it** as `<agents_dir>/<name>.md` —
    /// the unified frontmatter format the discovery pass reads, so the
    /// profile survives a restart and the profiles hot-reload watcher
    /// (which watches the agents roots already).
    ///
    /// The name is validated first (non-empty, no path separators): it
    /// becomes a file name under `agents_dir`, so it must never be able to
    /// escape the directory. Registering a name that already exists replaces
    /// the previous definition and rewrites the file (the upsert contract).
    ///
    /// **Round-trip fidelity (R4-1)**: when `<agents_dir>/<name>.md` already
    /// exists, it is rewritten surgically
    /// ([`write::rewrite_agent_md`]) — only the lines of the modified keys
    /// change; unknown keys, comments, blank lines and the Markdown body
    /// pass through byte-for-byte. A name with no `.md` yet gets a full
    /// serialization ([`write::agent_file_def_for_profile`]) — there are no
    /// original bytes to preserve. Once the `.md` exists, the same-name
    /// legacy `<legacy_profiles_dir>/<name>.{toml,json}` files are removed
    /// (the TS `removeLegacyFile` mirror, `.md`-exists guard included).
    ///
    /// The essential profile ([`ESSENTIAL_PROFILE_NAME`]) is **allowed**
    /// here (adjudication D16): the same-name `.md` is written and the
    /// merged content registers as the override — isomorphic with upserting
    /// any other built-in name. [`Self::delete_profile`] still refuses it,
    /// and the lifecycle face refuses disable/reset. No
    /// override-declaration key is ever emitted (adjudication D14).
    ///
    /// The registration carries the written file as its provenance (R3-5:
    /// user-sourced), and the suppressed-candidates projection for the
    /// name is reset (T19 m3: the recorded losers are stale after the
    /// legacy cleanup; the next roots load re-derives them). Persistence
    /// goes through the injected [`SyncFs`] provider.
    pub fn upsert_profile(
        &self,
        profile: AgentProfile,
        agents_dir: &Path,
        legacy_profiles_dir: &Path,
        fs: &dyn SyncFs,
    ) -> EngineResult<()> {
        let name = profile.name.clone();
        let path = persisted_profile_path(agents_dir, &name)?;
        fs.mkdir(agents_dir, true).map_err(|err| {
            EngineError::RequestInvalid(format!(
                "cannot create agent profile directory '{}': {err}",
                agents_dir.display()
            ))
        })?;
        // Read-modify-write: an existing `.md` is first parsed (a file that
        // does not parse — no fence, or out-of-subset content — is replaced
        // wholesale, never edited in place: an in-place rewrite of a corrupt
        // file plus the legacy cleanup below would leave nothing loadable
        // for the next restart) and then surgically rewritten, narrowed to
        // the keys whose values actually changed (T23-m1): the parsed def is
        // the diff baseline, so untouched keys keep the file's own
        // spelling, comments and list shapes verbatim.
        let contents = if fs.exists(&path) {
            match fs.read_text(&path) {
                Ok(text) => {
                    let path_label = path.display().to_string();
                    match parse_agent_file_text(&path_label, &text) {
                        Ok(existing) => {
                            match rewrite_agent_md(
                                &text,
                                &agent_md_rewrite_for_existing(&existing, &profile),
                            ) {
                                Ok(rewritten) => rewritten,
                                Err(err) => {
                                    tracing::warn!(
                                        path = %path.display(),
                                        %err,
                                        "cannot rewrite the agent profile file in place; reserializing it"
                                    );
                                    serialize_agent_file(&agent_file_def_for_profile(&profile))
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                path = %path.display(),
                                %err,
                                "the agent profile file does not parse; reserializing it"
                            );
                            serialize_agent_file(&agent_file_def_for_profile(&profile))
                        }
                    }
                }
                Err(err) => {
                    return Err(EngineError::RequestInvalid(format!(
                        "cannot read agent profile file '{}': {err}",
                        path.display()
                    )));
                }
            }
        } else {
            serialize_agent_file(&agent_file_def_for_profile(&profile))
        };
        fs.write_text(&path, &contents).map_err(|err| {
            EngineError::RequestInvalid(format!(
                "cannot write agent profile file '{}': {err}",
                path.display()
            ))
        })?;
        remove_legacy_profile_files(&name, agents_dir, legacy_profiles_dir, fs, true)?;
        // D16: the essential name upserts exactly like any other — the
        // same-name `.md` is written and the merged content registers as
        // the override (the built-in becomes the row's suppressed candidate
        // on the next discovery pass). Delete/disable stay refused
        // elsewhere.
        self.register_discovered(&DiscoveredProfile {
            profile,
            path: path.clone(),
            source: ProfileSource::User,
        })?;
        // T19 m3: the suppressed projection is a per-load snapshot — the
        // losers recorded for this name are stale now (the legacy files
        // were just cleaned); the next load re-derives them.
        self.suppressed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&name);
        Ok(())
    }

    /// Delete a persisted profile: remove the `.md` file under the agents
    /// root (plus the real winning file provenance retained, when it was
    /// discovered elsewhere under a managed root — T19 m4) and the same-name
    /// legacy files, then unregister the in-memory definition.
    ///
    /// When the name is one of the built-ins, the built-in definition is
    /// restored instead of leaving the name unregistered — deleting a user
    /// override falls back to the stock profile (the TS `:reset` semantics)
    /// and the provenance returns to the built-in tag (R3-5). The
    /// essential profile ([`ESSENTIAL_PROFILE_NAME`]) is refused at the
    /// engine level (R3-2/D16: content may be overridden, the name may
    /// not be deleted). A missing file is not an error; the memory state
    /// is still cleared. File removal goes through the injected [`SyncFs`]
    /// provider.
    ///
    /// The suppressed-candidates entry for the name is cleared (T19 m3):
    /// the projection is a per-load snapshot, and the losers it recorded
    /// just lost their files. The next roots load re-derives whatever still
    /// exists.
    pub fn delete_profile(
        &self,
        name: &str,
        agents_dir: &Path,
        legacy_profiles_dir: &Path,
        fs: &dyn SyncFs,
    ) -> EngineResult<()> {
        if name == ESSENTIAL_PROFILE_NAME {
            // The TS-exact refusal (`Cannot delete the essential profile
            // "agent"`, `packages/kap-server/src/routes/agents.ts:972`) —
            // the CRUD handler owns the wire face; this engine-level guard
            // is the backstop and reads the same.
            return Err(EngineError::RequestInvalid(format!(
                "Cannot delete the essential profile \"{ESSENTIAL_PROFILE_NAME}\""
            )));
        }
        let path = persisted_profile_path(agents_dir, name)?;
        remove_file_if_exists(&path, fs)?;
        // T19 m4: delete the *real* winning file when provenance retained
        // one under a managed root — e.g. a nested `agents/sub/<name>.md`
        // or a legacy `<profiles>/<name>.toml` outside the write root's
        // flat layout. Files in the generic `~/.agents/agents` root are
        // deliberately untouched (out of the write root's scope, TS
        // parity); they resurface on the next discovery pass.
        if let Some(file) = self
            .provenance(name)
            .and_then(|provenance| provenance.file_path)
            .filter(|file| is_managed_profile_file(file, agents_dir, legacy_profiles_dir))
        {
            remove_file_if_exists(&file, fs)?;
        }
        remove_legacy_profile_files(name, agents_dir, legacy_profiles_dir, fs, false)?;
        let mut guard = self
            .profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match builtin_profiles().into_iter().find(|p| p.name == name) {
            // A deleted override restores the built-in definition.
            Some(builtin) => {
                guard.insert(name.to_string(), builtin);
                let mut provenance = self
                    .provenance
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                provenance.insert(
                    name.to_string(),
                    ProfileProvenance {
                        source: ProfileSource::Builtin,
                        file_path: None,
                    },
                );
            }
            None => {
                guard.remove(name);
                let mut provenance = self
                    .provenance
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                provenance.remove(name);
            }
        }
        // T19 m3: reset the stale suppressed-candidates projection for the
        // name — the next roots load re-derives it.
        self.suppressed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name);
        Ok(())
    }

    /// Remove all user-registered profiles.
    ///
    /// Built-in profiles are re-registered afterwards, so they remain
    /// available across a hot-reload of the profiles directory — and the
    /// provenance map resets to the built-in tags, the suppressed-
    /// candidates projection to empty (the reload path that calls this
    /// re-applies both from its own discovery pass, see
    /// [`Self::set_suppressed_candidates`]).
    ///
    /// Used by the config hot-reload watcher before re-loading the profiles
    /// directory, so a deleted or renamed profile file disappears from the
    /// registry instead of lingering — while the built-ins stay available.
    pub fn clear(&self) {
        {
            let mut guard = self
                .profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clear();
            for profile in builtin_profiles() {
                guard.insert(profile.name.clone(), profile);
            }
        }
        *self
            .provenance
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = builtin_provenance();
        self.suppressed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Get a profile by name.
    pub fn get(&self, name: &str) -> Option<AgentProfile> {
        self.profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
    }

    /// List the **dispatchable** profile names, sorted alphabetically —
    /// the dispatch face (the subagent host's candidate listing, the
    /// Agent/swarm tool descriptions, the unknown-name error's
    /// "Available types" set).
    ///
    /// Excludes profiles whose file-level `enabled = false` flag disables
    /// them (R3-1 — the TS `catalog.list()` semantics) — with the essential
    /// exemption (R3-2): the essential profile's file flag never disables it
    /// ([`is_file_disabled`]). The host config's `disabled_agents` key no
    /// longer participates (R3-3 — it is inert data). Management faces that
    /// must see the full set (the wire lists-but-marks) use
    /// [`Self::list_all`].
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, profile)| !is_file_disabled(profile))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// List **every registered** profile name, sorted alphabetically —
    /// the management face (the TS `catalog.listAll()` semantics: a
    /// disabled profile stays listed so the wire can mark it).
    ///
    /// No enabled filtering of any kind: a file-level `enabled = false` flag
    /// removes a name from the dispatch face ([`Self::list`]) but never
    /// from here. Dispatch surfaces must use [`Self::list`].
    pub fn list_all(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Project one registered profile onto the base wire row — the six
    /// cross-engine fields `{name, source, builtin, essential, enabled,
    /// has_override}` plus `suppressed_candidates` when the winner
    /// displaced same-name candidates (the TS `catalogEntryToResolved`
    /// subset the shared cross-engine list fixture compares).
    ///
    /// The embedded server's REST row (`EmbeddedServer::profile_wire_row`)
    /// composes this base and adds the REST-only fields on top, and the
    /// cross-engine list acceptance test asserts through this same
    /// function — no test-side copy of the projection logic exists.
    /// Returns `None` when `name` is not registered.
    pub fn profile_wire_row_base(&self, name: &str) -> Option<Value> {
        let profile = self.get(name)?;
        // Defensive fallback for a name registered without provenance (the
        // manager tags every channel; this only pins the wire contract).
        let provenance = self.provenance(name).unwrap_or_else(|| {
            let source = if is_builtin_profile_name(name) {
                ProfileSource::Builtin
            } else {
                ProfileSource::User
            };
            ProfileProvenance {
                source,
                file_path: None,
            }
        });
        // The row IS the built-in definition when a built-in name won its
        // own merge (no user file overrode it); a user-sourced built-in
        // name is an override.
        let builtin_row =
            is_builtin_profile_name(name) && provenance.source == ProfileSource::Builtin;
        let has_override = is_builtin_profile_name(name) && !builtin_row;
        // Wire `enabled` composition (R3-3): the file-level `enabled = false`
        // flag (from a `.md` profile or a legacy file) is the sole source —
        // the host config's `disabled_agents` key is inert data, so a
        // profile named there shows `enabled: true` unless its own file
        // says otherwise.
        let enabled = !is_file_disabled(&profile);

        let mut row = json!({
            "name": name,
            "source": provenance.source.as_str(),
            "builtin": builtin_row,
            "essential": name == ESSENTIAL_PROFILE_NAME,
            "enabled": enabled,
            "has_override": has_override,
        });
        // The same-name candidates that lost the merge to this winner (the
        // TS `AgentProfileSuppressedCandidate` — `{source, priority,
        // reason}`, the priority derived from the source tag).
        let suppressed = self.suppressed_candidates(name);
        if !suppressed.is_empty() {
            row["suppressed_candidates"] = Value::Array(
                suppressed
                    .iter()
                    .map(|candidate| {
                        json!({
                            "source": candidate.source.as_str(),
                            "priority": candidate.source.priority(),
                            "reason": candidate.reason.as_str(),
                        })
                    })
                    .collect(),
            );
        }
        Some(row)
    }

    /// Resolve a profile name into its [`AgentProfile`].
    ///
    /// Profiles are self-contained (R2-2: legacy `extends` declarations
    /// were flattened at load time), so this is a direct lookup with no
    /// merge step. Unlike [`Self::resolve`], this returns the profile
    /// itself rather than an [`AgentScopeConfig`], so callers can inspect
    /// profile metadata.
    pub fn resolve_profile(&self, name: &str) -> EngineResult<AgentProfile> {
        self.ensure_dispatchable(name)?;
        self.get(name)
            .ok_or_else(|| self.unknown_profile_error(name))
    }

    /// Resolve a profile into an [`AgentScopeConfig`], applying defaults
    /// for missing fields:
    /// - `provider` — parsed from the provider name; defaults to
    ///   [`ProviderType::default`] (`Anthropic`).
    /// - `system_prompt_file` — read eagerly; takes precedence over an
    ///   inline `system_prompt`.
    /// - `max_turns` — defaults to `AgentScopeConfig::default().max_turns`
    ///   (200).
    /// - `permission_mode` — defaults to [`PermissionMode::default`]
    ///   (`Manual`).
    ///
    /// Returns [`EngineError::RequestInvalid`] when the profile does not
    /// exist, is disabled by its file-level `enabled = false` flag (see
    /// [`Self::resolve_profile`]), the provider name is unknown, or the
    /// system prompt file cannot be read. File reads go through the
    /// injected [`SyncFs`] provider (zero-IO T13b).
    pub fn resolve(&self, name: &str, fs: &dyn SyncFs) -> EngineResult<AgentScopeConfig> {
        self.ensure_dispatchable(name)?;
        let profile = self
            .get(name)
            .ok_or_else(|| self.unknown_profile_error(name))?;
        Self::scope_config_from_profile(&profile, fs)
    }

    /// The defaults/validation half of [`Self::resolve`], over an
    /// already-resolved profile — so a caller that already holds the
    /// [`AgentProfile`] (e.g. the subagent host's
    /// `available_profiles` listing, Task 15 hygiene) computes the scope
    /// config without resolving the name a second time.
    ///
    /// A profile without a model resolves to an empty `model_id` (spec 11
    /// R6): the "no model" state is legal at the profile level and passes
    /// through — the request-time resolution chain is what fails loudly
    /// when nothing is configured, never the profile resolution.
    pub(crate) fn scope_config_from_profile(
        profile: &AgentProfile,
        fs: &dyn SyncFs,
    ) -> EngineResult<AgentScopeConfig> {
        let system_prompt = match &profile.system_prompt_file {
            Some(path) => {
                if profile.system_prompt.is_some() {
                    tracing::warn!(
                        profile = %profile.name,
                        path = %path.display(),
                        "both system_prompt and system_prompt_file are set; \
                         system_prompt_file takes precedence"
                    );
                }
                let contents = fs.read_text(path).map_err(|err| {
                    EngineError::RequestInvalid(format!(
                        "cannot read system prompt file '{}' for profile '{}': {err}",
                        path.display(),
                        profile.name,
                    ))
                })?;
                Some(contents)
            }
            None => profile.system_prompt.clone(),
        };

        // Substitute {{var}} placeholders from the prompt_vars into the
        // finalized prompt (whether it came from a file or inline).
        let system_prompt =
            system_prompt.map(|prompt| render_system_prompt(&prompt, &profile.prompt_vars));

        // The role text rides the default base template instead of replacing
        // it (the TS v2 `systemPromptTemplate` counterpart, carried under the
        // v1-style `roleAdditional` prompt var).
        let role_additional = profile
            .prompt_vars
            .as_ref()
            .and_then(|vars| vars.get("role_additional"))
            .filter(|role| !role.is_empty())
            .cloned();

        let provider_type = match &profile.provider {
            Some(provider) => parse_provider_type(provider)?,
            None => ProviderType::default(),
        };

        Ok(AgentScopeConfig {
            model_id: profile.model.clone(),
            provider_type,
            role_additional,
            system_prompt,
            max_turns: profile
                .max_turns
                .unwrap_or(AgentScopeConfig::default().max_turns),
            permission_mode: profile.permission_mode.unwrap_or_default(),
            max_steps_per_turn: profile.max_steps_per_turn,
            enabled_tools: profile.tools.clone(),
            disabled_tools: profile.disallowed_tools.clone(),
            capabilities_required: profile.capabilities_required.clone(),
            thinking_effort: profile.thinking_effort.clone(),
            profile_name: None,
            initial_usage: None,
            reverse_rpc_slot: None,
            interaction_kernel: None,
            subagent_host: None,
            web_search_provider: None,
            fetch_url_provider: None,
            event_bus: None,
            tool_io: crate::tools::ToolIo::default(),
            subagent_timeout: None,
            max_swarm_items: None,
            session_approvals: None,
            permission_rules: Vec::new(),
        })
    }

    /// Reject dispatch of a profile disabled by its file-level
    /// `enabled = false` flag (R3-1) — the sole enablement source since
    /// R3-3 retired the host config's `disabled_agents` key (it is inert
    /// data now; see the [module docs](self)).
    ///
    /// The refusal carries the TS-aligned message
    /// (`requireSubagentProfile`,
    /// `packages/agent-core-v2/src/app/agentProfileCatalog/profile-shared.ts`).
    ///
    /// The essential-profile exemption (R3-2): an `enabled = false` on the
    /// essential default `agent` profile warns and is treated as enabled
    /// ([`is_file_disabled`] — the TS
    /// `sessionAgentProfileCatalogService.ts:192-200`), so dispatch
    /// through the default agent can never be bricked.
    fn ensure_dispatchable(&self, name: &str) -> EngineResult<()> {
        if self
            .get(name)
            .is_some_and(|profile| is_file_disabled(&profile))
        {
            return Err(EngineError::RequestInvalid(format!(
                "Agent type \"{name}\" is disabled. Re-enable it in the agent \
                 profiles configuration to dispatch it."
            )));
        }
        Ok(())
    }

    /// The unknown-name error, TS-aligned
    /// (`requireSubagentProfile`, `profile-shared.ts`): names the missing
    /// type and the dispatchable set — the `list()` face, so disabled
    /// profiles are not offered as "available" — joined with ", " exactly
    /// like the TS `catalog.list().map(p => p.name).join(', ')`.
    fn unknown_profile_error(&self, name: &str) -> EngineError {
        let available = self.list().join(", ");
        EngineError::RequestInvalid(format!(
            "Unknown agent type: \"{name}\". Available types: {available}"
        ))
    }
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed profile file plus its legacy `extends` declaration.
///
/// The profile struct no longer models inheritance; `extends` is captured
/// only so [`flatten_extends_entries`] can flatten legacy files at load
/// time. Never persisted, never serialized. `path` is the file the entry
/// came from — the key skip-and-collect failures are reported under — and
/// `source` the root tag it was discovered under (assigned by
/// [`discover_profiles`], the only caller that produces entries).
#[derive(Debug, Clone)]
struct ParsedProfileFile {
    path: PathBuf,
    profile: AgentProfile,
    /// The TS source tag of the root the file was found under.
    source: ProfileSource,
    /// Legacy `extends = "<parent>"` from the file, when declared.
    extends: Option<String>,
}

/// Flatten the legacy `extends` declarations of a parsed profile set into
/// self-contained profiles (R2-2), collecting per-file failures instead of
/// aborting the whole set (R2-3 skip-and-collect).
///
/// Inheritance was removed from the live model: instead of merging at every
/// resolve, a legacy `extends = "X"` is resolved **once, at load time**,
/// against the set being loaded overlaid on the built-in profiles — every
/// manager registers the built-ins, so a legacy `extends = "coder"` keeps
/// resolving. The merge semantics are the old `resolve_merged`'s: child
/// fields replace the parent's, the child's empty `model` inherits the
/// parent's, `subagents` are not inherited, and `prompt_vars` deep-merge.
///
/// A cycle, an unknown parent, `extends = ""`, or a parent whose own
/// flattening failed skips **that entry** (with the failure reason chained
/// for a skipped parent); the rest of the set still flattens.
fn flatten_extends_entries(
    entries: &[ParsedProfileFile],
) -> (Vec<DiscoveredProfile>, Vec<SkippedProfileFile>) {
    let by_name: HashMap<&str, &ParsedProfileFile> = entries
        .iter()
        .map(|entry| (entry.profile.name.as_str(), entry))
        .collect();
    let builtins: HashMap<String, AgentProfile> = builtin_profiles()
        .into_iter()
        .map(|profile| (profile.name.clone(), profile))
        .collect();

    let mut memo: HashMap<String, Result<AgentProfile, String>> = HashMap::new();
    let mut profiles = Vec::with_capacity(entries.len());
    let mut skipped = Vec::new();
    for entry in entries {
        let mut stack: Vec<String> = Vec::new();
        match flatten_one(entry, &by_name, &builtins, &mut memo, &mut stack) {
            Ok(profile) => profiles.push(DiscoveredProfile {
                profile,
                path: entry.path.clone(),
                source: entry.source,
            }),
            Err(reason) => skipped.push(SkippedProfileFile {
                path: entry.path.clone(),
                reason,
            }),
        }
    }
    (profiles, skipped)
}

/// Flatten one entry's inheritance chain (recursively resolving a parent
/// found in the loaded set; a built-in parent is already self-contained).
///
/// `stack` carries the profile names visited so far for cycle detection;
/// `memo` caches per-name results so a shared parent flattens once and a
/// failed parent's reason chains into every child that extends it.
fn flatten_one(
    entry: &ParsedProfileFile,
    by_name: &HashMap<&str, &ParsedProfileFile>,
    builtins: &HashMap<String, AgentProfile>,
    memo: &mut HashMap<String, Result<AgentProfile, String>>,
    stack: &mut Vec<String>,
) -> Result<AgentProfile, String> {
    // Cycle detection.
    if stack.iter().any(|name| name == &entry.profile.name) {
        let chain = stack
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(entry.profile.name.as_str()))
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(format!("agent profile extends cycle detected: {chain}"));
    }

    if let Some(cached) = memo.get(&entry.profile.name) {
        return cached.clone();
    }

    let Some(parent_name) = &entry.extends else {
        // No parent — the entry is already self-contained.
        let profile = entry.profile.clone();
        memo.insert(entry.profile.name.clone(), Ok(profile.clone()));
        return Ok(profile);
    };

    if parent_name.is_empty() {
        let reason = format!(
            "agent profile '{}' declares `extends = \"\"`; \
             remove the field or specify a parent profile name",
            entry.profile.name
        );
        memo.insert(entry.profile.name.clone(), Err(reason.clone()));
        return Err(reason);
    }

    let parent = if let Some(parent_entry) = by_name.get(parent_name.as_str()) {
        // The parent may itself extend — recurse with this entry on the
        // stack so a cycle is caught before it unwinds.
        stack.push(entry.profile.name.clone());
        let flattened = flatten_one(parent_entry, by_name, builtins, memo, stack);
        stack.pop();
        match flattened {
            Ok(parent) => parent,
            Err(parent_reason) => {
                let reason = format!(
                    "agent profile '{}' extends profile '{parent_name}', \
                     which was skipped: {parent_reason}",
                    entry.profile.name
                );
                memo.insert(entry.profile.name.clone(), Err(reason.clone()));
                return Err(reason);
            }
        }
    } else if let Some(builtin) = builtins.get(parent_name) {
        builtin.clone()
    } else {
        let reason = format!(
            "agent profile '{}' extends unknown profile '{parent_name}'",
            entry.profile.name
        );
        memo.insert(entry.profile.name.clone(), Err(reason.clone()));
        return Err(reason);
    };

    let merged = merge_profile(&entry.profile, &parent);
    memo.insert(entry.profile.name.clone(), Ok(merged.clone()));
    Ok(merged)
}

/// Merge parent fields into the child with replace-or-inherit semantics —
/// the old `resolve_merged` merge, kept as the load-time flattening rule.
fn merge_profile(child: &AgentProfile, parent: &AgentProfile) -> AgentProfile {
    AgentProfile {
        name: child.name.clone(),
        model: if child.model.is_empty() {
            parent.model.clone()
        } else {
            child.model.clone()
        },
        provider: child.provider.clone().or_else(|| parent.provider.clone()),
        system_prompt: child
            .system_prompt
            .clone()
            .or_else(|| parent.system_prompt.clone()),
        system_prompt_file: child
            .system_prompt_file
            .clone()
            .or_else(|| parent.system_prompt_file.clone()),
        max_turns: child.max_turns.or(parent.max_turns),
        max_steps_per_turn: child.max_steps_per_turn.or(parent.max_steps_per_turn),
        permission_mode: child.permission_mode.or(parent.permission_mode),
        tools: child.tools.clone().or_else(|| parent.tools.clone()),
        disallowed_tools: child
            .disallowed_tools
            .clone()
            .or_else(|| parent.disallowed_tools.clone()),
        capabilities_required: child
            .capabilities_required
            .clone()
            .or_else(|| parent.capabilities_required.clone()),
        thinking_effort: child
            .thinking_effort
            .clone()
            .or(parent.thinking_effort.clone()),
        // `subagents` are intentionally NOT inherited (matching the old
        // `resolve_merged` / the TS `resolveMergedProfile`): a child
        // profile declares its own subagents or has none.
        subagents: child.subagents.clone(),
        description: child
            .description
            .clone()
            .or_else(|| parent.description.clone()),
        when_to_use: child
            .when_to_use
            .clone()
            .or_else(|| parent.when_to_use.clone()),
        default_model: child
            .default_model
            .clone()
            .or_else(|| parent.default_model.clone()),
        capabilities: child
            .capabilities
            .clone()
            .or_else(|| parent.capabilities.clone()),
        // `prompt_vars` deep-merge: parent vars provide the base and the
        // child's vars override per-key (see [`merge_prompt_vars`]).
        prompt_vars: merge_prompt_vars(&parent.prompt_vars, &child.prompt_vars),
        enabled: child.enabled.or(parent.enabled),
        summary_policy: child
            .summary_policy
            .clone()
            .or_else(|| parent.summary_policy.clone()),
        prompt_prefix: child
            .prompt_prefix
            .clone()
            .or_else(|| parent.prompt_prefix.clone()),
    }
}

/// Merge `prompt_vars` from a parent and a child profile.
///
/// Child variables override parent variables with the same key. When both are
/// `None` the result is `None`; when only one is set, it is returned as-is.
fn merge_prompt_vars(
    parent: &Option<HashMap<String, String>>,
    child: &Option<HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    match (parent, child) {
        (None, None) => None,
        (Some(p), None) => Some(p.clone()),
        (None, Some(c)) => Some(c.clone()),
        (Some(p), Some(c)) => {
            let mut merged = p.clone();
            merged.extend(c.iter().map(|(k, v)| (k.clone(), v.clone())));
            Some(merged)
        }
    }
}

/// The `.md` file a persisted profile with this name occupies under
/// `agents_dir`.
///
/// The name is validated (non-empty, no path separators, no `.`/`..`):
/// it becomes a file name inside `agents_dir`, so it must never be able to
/// escape the directory.
fn persisted_profile_path(agents_dir: &Path, name: &str) -> EngineResult<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(EngineError::RequestInvalid(format!(
            "invalid agent profile name {name:?}: names must be non-empty and free of path separators"
        )));
    }
    Ok(agents_dir.join(format!("{name}.md")))
}

/// Remove `path` when it exists (a missing file is a no-op, so delete
/// stays idempotent), through the injected [`SyncFs`] provider.
fn remove_file_if_exists(path: &Path, fs: &dyn SyncFs) -> EngineResult<()> {
    if fs.exists(path) {
        fs.remove_file(path).map_err(|err| {
            EngineError::RequestInvalid(format!(
                "cannot remove agent profile file '{}': {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// Remove the same-name legacy `<profiles>/<name>.{toml,json}` files once
/// the new-format `.md` exists — the TS `removeLegacyFile` mirror. The
/// `.md`-exists guard catches callers that pass a name before the primary
/// file was written (out-of-order writes); the legacy files win on read
/// only while no `.md` shadows them, so leaving them behind a `.md` would
/// just seed stale suppressed candidates.
///
/// The delete channel passes `md_must_exist = false`: the whole profile is
/// going away, so the legacy cleanup must run even after the `.md` is gone
/// — a deliberate divergence from the TS delete path, where the
/// `removeLegacyFile` guard skips cleanup once the `.md` was removed and a
/// leftover legacy file resurrects the profile on the next reload.
fn remove_legacy_profile_files(
    name: &str,
    agents_dir: &Path,
    legacy_profiles_dir: &Path,
    fs: &dyn SyncFs,
    md_must_exist: bool,
) -> EngineResult<()> {
    if md_must_exist && !fs.exists(&agents_dir.join(format!("{name}.md"))) {
        tracing::debug!(
            name,
            "legacy profile cleanup skipped: no .md file exists (out-of-order write?)"
        );
        return Ok(());
    }
    for extension in ["toml", "json"] {
        remove_file_if_exists(&legacy_profiles_dir.join(format!("{name}.{extension}")), fs)?;
    }
    Ok(())
}

/// Whether `path` is a profile file under one of the two roots the write
/// channel manages — a `.md` under the agents root, or a legacy
/// `.toml`/`.json` under the legacy profiles root. Files elsewhere (the
/// generic `~/.agents/agents` root, project roots) are not the write
/// channel's to delete.
fn is_managed_profile_file(path: &Path, agents_dir: &Path, legacy_profiles_dir: &Path) -> bool {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md") => path.starts_with(agents_dir),
        Some("toml") | Some("json") => path.starts_with(legacy_profiles_dir),
        _ => false,
    }
}

/// Replace `{{var}}` placeholders in a system prompt using `prompt_vars`.
pub fn render_system_prompt(template: &str, vars: &Option<HashMap<String, String>>) -> String {
    match vars {
        None => template.to_string(),
        Some(vars) => {
            let mut result = template.to_string();
            for (key, value) in vars {
                result = result.replace(&format!("{{{{{}}}}}", key), value);
            }
            result
        }
    }
}

/// Parse a profile from a TOML, JSON, or Markdown file into a
/// self-contained [`AgentProfile`] without registering it anywhere.
///
/// The file format is chosen by extension (`.toml`, `.json`, or `.md`;
/// extension matching is case-insensitive). A legacy `extends` declaration
/// is flattened at this point against the built-in profiles only — a
/// single file cannot see its siblings; directory-wide flattening is
/// [`discover_profiles`]'s job. File reads go through the injected [`SyncFs`]
/// provider.
fn parse_profile_file(path: &Path, fs: &dyn SyncFs) -> EngineResult<AgentProfile> {
    let contents = fs.read_text(path).map_err(|err| {
        EngineError::RequestInvalid(format!(
            "cannot read agent profile file '{}': {err}",
            path.display()
        ))
    })?;
    let entry = parse_profile_entry(path, &contents, ProfileSource::User)?;
    let (mut profiles, mut skipped) = flatten_extends_entries(std::slice::from_ref(&entry));
    // One file in: either it flattened, or it carries its own skip reason.
    if let Some(skipped) = skipped.pop() {
        return Err(EngineError::RequestInvalid(format!(
            "cannot load agent profile '{}': {}",
            skipped.path.display(),
            skipped.reason
        )));
    }
    Ok(profiles
        .pop()
        .expect("one entry in, one profile out")
        .profile)
}

/// Parse one profile file's contents into a [`ParsedProfileFile`] — the
/// profile plus its source path, its root's [`ProfileSource`] tag, and
/// legacy `extends` declaration, captured for load-time flattening.
///
/// The format is chosen by extension (`.toml`, `.json`, or `.md`; matching
/// is case-insensitive). Pure — no IO. `.toml`/`.json` files with an
/// `extends` key keep it readable (the unknown key is ignored by the
/// struct's serde); a non-string `extends` is a load error, matching the
/// old `Option<String>` field behavior.
fn parse_profile_entry(
    path: &Path,
    contents: &str,
    source: ProfileSource,
) -> EngineResult<ParsedProfileFile> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    match extension.to_ascii_lowercase().as_str() {
        "toml" => {
            let mut value: toml::Value = toml::from_str(contents).map_err(|err| {
                EngineError::RequestInvalid(format!(
                    "invalid TOML agent profile '{}': {err}",
                    path.display()
                ))
            })?;
            let extends = match value.get("extends") {
                None => None,
                Some(extends) => Some(
                    extends
                        .as_str()
                        .ok_or_else(|| {
                            EngineError::RequestInvalid(format!(
                                "invalid `extends` in agent profile '{}': expected a profile \
                                 name string",
                                path.display()
                            ))
                        })?
                        .to_string(),
                ),
            };
            // R3-4: migrate a legacy `subagents` shape (inline
            // definitions / the v1 Record form) to the name allowlist
            // before the struct deserializes — a `[[subagents]]` table
            // would otherwise fail the `Option<Vec<String>>` field and
            // break the whole file.
            normalize_legacy_subagents_toml(&mut value, path);
            let profile = value.try_into().map_err(|err| {
                EngineError::RequestInvalid(format!(
                    "invalid TOML agent profile '{}': {err}",
                    path.display()
                ))
            })?;
            Ok(ParsedProfileFile {
                path: path.to_path_buf(),
                profile,
                source,
                extends,
            })
        }
        "json" => {
            let mut value: serde_json::Value = serde_json::from_str(contents).map_err(|err| {
                EngineError::RequestInvalid(format!(
                    "invalid JSON agent profile '{}': {err}",
                    path.display()
                ))
            })?;
            // `extends: null` is JSON's spelling of "not declared" —
            // treat it exactly like an absent key (serde yields
            // `Some(Value::Null)` for it, which must not become an
            // "invalid extends" load error).
            let extends = match value.get("extends") {
                None | Some(serde_json::Value::Null) => None,
                Some(extends) => Some(
                    extends
                        .as_str()
                        .ok_or_else(|| {
                            EngineError::RequestInvalid(format!(
                                "invalid `extends` in agent profile '{}': expected a profile \
                                 name string",
                                path.display()
                            ))
                        })?
                        .to_string(),
                ),
            };
            // R3-4: the same legacy `subagents` migration as the TOML arm
            // (both share this serde path into `AgentProfile`).
            normalize_legacy_subagents_json(&mut value, path);
            let profile = serde_json::from_value(value).map_err(|err| {
                EngineError::RequestInvalid(format!(
                    "invalid JSON agent profile '{}': {err}",
                    path.display()
                ))
            })?;
            Ok(ParsedProfileFile {
                path: path.to_path_buf(),
                profile,
                source,
                extends,
            })
        }
        "md" => {
            // The unified `.md` agent file (frontmatter + Markdown prompt
            // body), adapted through `AgentFileDef`. `.md` never declares
            // `extends` (the legacy key is silently ignored by the
            // frontmatter parser). The parser error's `reason` is already
            // the TS-format message and embeds the path.
            let def = parse_agent_file_text(&path.to_string_lossy(), contents).map_err(|err| {
                EngineError::RequestInvalid(format!(
                    "invalid agent profile '{}': {}",
                    path.display(),
                    err.reason
                ))
            })?;
            Ok(ParsedProfileFile {
                path: path.to_path_buf(),
                profile: AgentProfile::from(def),
                source,
                extends: None,
            })
        }
        other => Err(EngineError::RequestInvalid(format!(
            "unsupported agent profile extension '.{other}' on '{}'; use .toml, .json, or .md",
            path.display()
        ))),
    }
}

/// Migrate a legacy `subagents` value in a TOML profile to the R3-4 name
/// allowlist, in place — the parse-time normalization point, occupying the
/// same spot the TS `parseSubagents` does (`packages/agent-profile/src/
/// parse.ts:188-201`): the parser normalizes, the model stays a plain
/// `Option<Vec<String>>`.
///
/// Accepted shapes: the native array of strings (left untouched), the
/// pre-R3-4 rust inline definitions (`[[subagents]]` tables — each entry's
/// `name` is extracted and the model/prompt/tool overrides are dropped),
/// and the v1 `Record` form (a table — its keys are the names). Any other
/// shape is left for the struct's serde to reject with its own error. A
/// migrated file logs ONE warning naming the path; a definition without a
/// string `name` names nothing dispatchable and is dropped.
fn normalize_legacy_subagents_toml(value: &mut toml::Value, path: &Path) {
    let toml::Value::Table(root) = value else {
        return;
    };
    let Some(raw) = root.get_mut("subagents") else {
        return;
    };
    let names: Option<Vec<String>> = match raw {
        toml::Value::Array(items) => {
            if items.iter().any(toml::Value::is_table) {
                Some(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            toml::Value::String(name) if !name.trim().is_empty() => {
                                Some(name.clone())
                            }
                            toml::Value::Table(def) => def
                                .get("name")
                                .and_then(|name| name.as_str())
                                .filter(|name| !name.trim().is_empty())
                                .map(str::to_string),
                            _ => None,
                        })
                        .collect(),
                )
            } else {
                // Native name list — nothing to migrate.
                None
            }
        }
        // The v1 `Record` form — its keys are the names.
        toml::Value::Table(defs) => Some(defs.keys().cloned().collect()),
        _ => None,
    };
    let Some(names) = names else {
        return;
    };
    warn_legacy_subagents_migrated(path, &names);
    *raw = toml::Value::Array(names.into_iter().map(toml::Value::String).collect());
}

/// The JSON twin of [`normalize_legacy_subagents_toml`] — the `.json`
/// profile path shares the serde entry into [`AgentProfile`], so it gets
/// the same legacy migration (inline definition objects extract their
/// `name`; the v1 `Record` form's keys are the names).
fn normalize_legacy_subagents_json(value: &mut serde_json::Value, path: &Path) {
    let serde_json::Value::Object(root) = value else {
        return;
    };
    let Some(raw) = root.get_mut("subagents") else {
        return;
    };
    let names: Option<Vec<String>> = match raw {
        serde_json::Value::Array(items) => {
            if items.iter().any(serde_json::Value::is_object) {
                Some(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            serde_json::Value::String(name) if !name.trim().is_empty() => {
                                Some(name.clone())
                            }
                            serde_json::Value::Object(def) => def
                                .get("name")
                                .and_then(|name| name.as_str())
                                .filter(|name| !name.trim().is_empty())
                                .map(str::to_string),
                            _ => None,
                        })
                        .collect(),
                )
            } else {
                None
            }
        }
        serde_json::Value::Object(defs) => Some(defs.keys().cloned().collect()),
        _ => None,
    };
    let Some(names) = names else {
        return;
    };
    warn_legacy_subagents_migrated(path, &names);
    *raw = serde_json::Value::Array(names.into_iter().map(serde_json::Value::String).collect());
}

/// The one-time-per-file warning emitted when a legacy `subagents` shape
/// is migrated to the R3-4 name allowlist — the file still loads (this is
/// NOT the skip channel), so the warning is the visible trace that the
/// inline definitions' model/prompt/tool overrides were dropped.
fn warn_legacy_subagents_migrated(path: &Path, names: &[String]) {
    tracing::warn!(
        path = %path.display(),
        names = names.join(", "),
        "legacy `subagents` definitions migrated to the R3-4 name allowlist; \
         the inline model/prompt/tool overrides are dropped"
    );
}

/// The TS wire `source` enum for a profile — where a registered profile
/// came from (`packages/protocol/src/rest/agents.ts:4-12`).
///
/// The values the rust engine produces today:
///
/// - [`ProfileSource::Builtin`] — the four built-ins every manager seeds
///   (`agent` / `coder` / `explore` / `plan`, [`builtin_profiles`]).
///   Built-ins never flow through discovery, so the tag is attached where
///   profiles enter the manager: the seeding
///   ([`ProfileManager::new`] / [`ProfileManager::clear`]), the discovery
///   registration ([`ProfileManager::register_discovered`]), and the CRUD
///   write channel ([`ProfileManager::upsert_profile`]).
/// - [`ProfileSource::User`] — every root the rust boot chain scans: the
///   brand `<mirri-home>/agents`, the generic `~/.agents/agents`
///   ([`user_profile_root_candidates`]), and the legacy
///   `<mirri-home>/profiles` write directory ([`legacy_profile_root`]).
///   The legacy directory has no TS analog; `user` is the closest semantic
///   for a home-directory profile and is the documented choice.
/// - [`ProfileSource::Plugin`] — plugin-contributed agent roots (item H:
///   the EMB composition root feeds `pluginAgentRoots`-derived
///   [`ProfileSourceRoot`]s, priority 5).
///
/// Reserved for R4+ — values the rust engine cannot produce yet (they
/// parse, compare, and emit on the wire so a future root carries them
/// without a wire change): `Project` (no project-root notion feeds
/// profile discovery), `Extra` (no configurable extra dirs), `Explicit`
/// (no `--agent-file` channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// Seeded by the manager (`agent` / `coder` / `explore` / `plan`).
    Builtin,
    /// A user-level root (brand agents dir, generic agents dir, legacy
    /// profiles dir) or a user write through the CRUD channel.
    User,
    /// Reserved: a project-root source (`.mirri-code/agents` under a
    /// workspace) — R4+.
    Project,
    /// Reserved: a configurable extra dir — the rust engine has none.
    Extra,
    /// A plugin-contributed source (item H): the roots come from the
    /// host's `pluginAgentRoots()` channel at priority 5.
    Plugin,
    /// Reserved: a one-shot explicit file (`--agent-file`) — R4+.
    Explicit,
}

impl ProfileSource {
    /// The TS wire spelling (`profileEntrySchema.source`).
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileSource::Builtin => "builtin",
            ProfileSource::User => "user",
            ProfileSource::Project => "project",
            ProfileSource::Extra => "extra",
            ProfileSource::Plugin => "plugin",
            ProfileSource::Explicit => "explicit",
        }
    }

    /// The merge priority this source carries — the TS
    /// `AGENT_PROFILE_SOURCE_PRIORITY`
    /// (`packages/agent-core-v2/src/app/agentProfileCatalog/agentProfileContribution.ts:32-39`):
    /// builtin 0, plugin 5, user 10, extra 20, workspace 30, explicit 40
    /// (higher wins). [`ProfileSource::Project`] occupies the TS
    /// `workspace` slot — the TS registry names that source `workspace`
    /// while the wire enum spells it `project`; same source, priority 30.
    pub fn priority(self) -> u32 {
        match self {
            ProfileSource::Builtin => 0,
            ProfileSource::Plugin => 5,
            ProfileSource::User => 10,
            ProfileSource::Extra => 20,
            ProfileSource::Project => 30,
            ProfileSource::Explicit => 40,
        }
    }
}

/// A discovery root paired with the [`ProfileSource`] tag every profile
/// found under it carries — the TS source model, where each scanned root
/// belongs to exactly one source. Rust roots are the user chain
/// (brand / generic / legacy) plus plugin roots (item H) today (see
/// [`ProfileSource`]); the tag rides along so an R4 root
/// (project / extra / explicit) needs no discovery or wire change.
#[derive(Debug, Clone)]
pub struct ProfileSourceRoot {
    /// The root directory scanned.
    pub path: PathBuf,
    /// The source tag for profiles discovered under it.
    pub source: ProfileSource,
}

impl From<PathBuf> for ProfileSourceRoot {
    /// A bare path is a user-level root — the only level the rust boot
    /// chain scans today.
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            source: ProfileSource::User,
        }
    }
}

/// Why a same-name candidate lost the merge — the TS suppressed-candidate
/// reason enum (`packages/protocol/src/rest/agents.ts:6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressedReason {
    /// A trailing candidate that lost to an earlier (higher-priority) one
    /// (the TS `:213-218`) — including a built-in definition displaced by a
    /// user file covering its name (D16: user 10 outranks builtin 0).
    Priority,
    /// A candidate whose own file sets `enabled = false` — it never wins,
    /// even over a lower-priority candidate (the TS `:192-204`).
    Disabled,
}

impl SuppressedReason {
    /// The TS wire spelling (`suppressedCandidateSchema.reason`).
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressedReason::Priority => "priority",
            SuppressedReason::Disabled => "disabled",
        }
    }
}

/// One same-name candidate that lost the merge — the rust mirror of the TS
/// `AgentProfileSuppressedCandidate` (`{source, priority, reason}` on the
/// wire). The priority is derived from the source (the TS
/// `AGENT_PROFILE_SOURCE_PRIORITY` pairing) rather than stored, so the two
/// can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressedProfileCandidate {
    /// The source the losing candidate was discovered under.
    pub source: ProfileSource,
    /// Why it lost.
    pub reason: SuppressedReason,
}

/// Provenance of one registered profile: its TS source tag and, for
/// file-sourced profiles, the file it was discovered from (the `.md`
/// provenance the wire row reports as `file_path`). Built-ins carry no
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileProvenance {
    /// The TS wire `source` enum value.
    pub source: ProfileSource,
    /// The discovered/persisted file, when the profile is file-sourced.
    pub file_path: Option<PathBuf>,
}

/// A profile file skipped during discovery, with the reason it did not load.
///
/// The rust mirror of the TS `SkippedAgentFile` — one malformed file, one
/// bad legacy `extends`, or one unsupported extension produces one of these
/// instead of failing the whole pass. D13: load failures only — a valid
/// file that lost a merge is suppressed or silently dropped, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedProfileFile {
    /// The file that was skipped.
    pub path: PathBuf,
    /// Why it was skipped (parse error, extends failure, unsupported
    /// extension, or an unreadable file/directory).
    pub reason: String,
}

/// A successfully discovered profile plus the file it was discovered from.
///
/// The path and source are the discovery layer's provenance — the model
/// itself stays file-agnostic (R3-5 lifts both onto the wire catalog via
/// [`ProfileManager::provenance`]).
#[derive(Debug, Clone)]
pub struct DiscoveredProfile {
    /// The parsed, self-contained profile.
    pub profile: AgentProfile,
    /// The file it was discovered from.
    pub path: PathBuf,
    /// The source tag of the root it was found under (the TS wire
    /// `source` enum).
    pub source: ProfileSource,
}

/// The result of one discovery pass over one or more root directories —
/// the rust mirror of the TS `AgentFileDiscoveryResult`, extended with the
/// R3-5 merge projection.
#[derive(Debug, Clone, Default)]
pub struct ProfileDiscoveryResult {
    /// The discovered profiles, sorted by name.
    pub profiles: Vec<DiscoveredProfile>,
    /// The files that did not load, with reasons (skip-and-collect).
    pub skipped: Vec<SkippedProfileFile>,
    /// Every root the pass scanned, in scan order (including roots that
    /// contributed nothing — the TS `scannedRoots`).
    pub scanned_roots: Vec<PathBuf>,
    /// Same-name candidates that lost the merge, keyed by the winner's
    /// name (the TS catalog inspection's `suppressed`). A key may name a
    /// built-in (a disabled override of a built-in lost to the still
    /// registered built-in — the TS case a), and a user file that covered
    /// a built-in name keys the displaced built-in definition itself
    /// (`{source: builtin, priority: 0, reason: priority}`, D16).
    pub suppressed: HashMap<String, Vec<SuppressedProfileCandidate>>,
}

/// The maximum directory depth [`discover_profiles`] walks below a root —
/// the TS `MAX_AGENT_SCAN_DEPTH`.
const MAX_PROFILE_SCAN_DEPTH: usize = 8;

/// The priority rank of a profile file extension for same-name conflict
/// resolution: `0` = `.md` (first-class), `1` = `.toml`, `2` = `.json`.
///
/// This is the generalization of the TS `AGENT_FILE_EXTENSIONS` convention
/// (`.md` wins; the TS list is ordered by priority). Lower rank parses
/// first and first registration wins, so `.md` beats `.toml`/`.json` and
/// `.toml` beats `.json` — the `.toml`-over-`.json` choice matches the
/// deterministic outcome the pre-R2-3 sorted load already produced (with
/// `.json` before `.toml` in path order and last-registration-wins, the
/// `.toml` won).
fn extension_rank(extension: &str) -> Option<u8> {
    match extension.to_ascii_lowercase().as_str() {
        "md" => Some(0),
        "toml" => Some(1),
        "json" => Some(2),
        _ => None,
    }
}

/// Per-name merge state accumulated during one discovery pass — the R3-5
/// suppressed-candidates projection, mirroring the TS catalog's per-name
/// candidate loop (`sessionAgentProfileCatalogService.ts:162-250`).
#[derive(Default)]
struct NameMergeState {
    /// The index into the winner parse set, once an eligible (enabled)
    /// candidate for the name has been seen.
    winner: Option<usize>,
    /// The root index the winner was found under — classifies later
    /// same-name files as cross-root losers (suppressed `priority`) vs
    /// same-root duplicates (silently dropped, D13).
    winner_root: usize,
    /// The first disabled candidate — the TS `disabledWinners` fallback
    /// (case b) when no eligible candidate and no built-in exists for the
    /// name.
    first_disabled: Option<ParsedProfileFile>,
    /// Same-name candidates that lost the merge so far, in encounter order.
    suppressed: Vec<SuppressedProfileCandidate>,
}

/// Discover agent profiles under `roots` without registering anything —
/// the R2-3 alignment with the TS `discoverAgentFiles`, extended with the
/// R3-5 multi-source merge projection.
///
/// Each root (a [`ProfileSourceRoot`] — a path plus its TS source tag) is
/// walked recursively (max depth [`MAX_PROFILE_SCAN_DEPTH`], skipping
/// dot-directories and `node_modules`), in **source-priority order** —
/// highest priority first (the TS `AGENT_PROFILE_SOURCE_PRIORITY`), with
/// equal-priority roots keeping the caller's relative order — so the
/// same-name winner is decided by priority, never by the caller's argument
/// order; within a
/// root, files parse in extension-priority order (`.md`, then `.toml`,
/// then `.json`, each sorted by path). Profiles are keyed by their declared
/// name and the **first enabled registration wins**, so a higher-priority
/// root and a higher-priority extension beat lower ones; an `enabled = false`
/// candidate never wins, not even over a lower-priority one (the TS
/// `:192-204` — except the essential name, whose file flag is exempt per
/// D16 and warns at consumption instead), and the same-name losers land in
/// [`ProfileDiscoveryResult::suppressed`] with the TS reason enum —
/// cross-root losers as `priority`, disabled candidates as `disabled`. A
/// user file covering a built-in name (the essential included, D16)
/// displaces the built-in definition into the winner's suppressed list as
/// `{source: builtin, priority: 0, reason: priority}`. Same-root duplicates
/// (same directory or cross-directory) are silently dropped (the TS
/// loader's within-entry dedup; D13 keeps the skip channel for load
/// failures only). Unsupported `.yaml`/`.yml` files are skipped
/// with an explicit reason (the rust engine reads `.md`/`.toml`/`.json`);
/// files with other extensions are ignored by the scan. After the set
/// parses, legacy `extends` declarations are flattened with per-file
/// skip-and-collect (see [`flatten_extends_entries`]).
///
/// A root that does not exist contributes nothing (the TS
/// "missing = nothing here" rule); a root that exists but cannot be listed
/// is an error, so callers can keep prior state. An unreadable
/// **sub**directory is skip-and-collect (see [`walk_profile_dir`]) — one
/// locked directory never aborts the scan. Directory scanning and
/// file reads go through the injected [`SyncFs`] provider.
pub fn discover_profiles(
    roots: &[ProfileSourceRoot],
    fs: &dyn SyncFs,
) -> EngineResult<ProfileDiscoveryResult> {
    let mut result = ProfileDiscoveryResult {
        scanned_roots: roots.iter().map(|root| root.path.clone()).collect(),
        ..Default::default()
    };
    // T19 m1: the scan order is the roots' **source-priority order**
    // (highest first, stable), not the caller's argument order — the TS
    // catalog model, where the same-name winner is decided by source
    // priority (builtin 0 < plugin 5 < user 10 < extra 20 < project 30 <
    // explicit 40), never by which order a caller happens to pass the
    // roots in. Because the merge below is first-eligible-registration-
    // wins, the HIGHEST priority root must be scanned first so its
    // candidate wins and the trailing lower-priority candidates record
    // themselves as suppressed `priority` losers; equal priorities keep
    // the caller's relative order (a stable sort — the user chain: brand,
    // generic, legacy).
    let scan_order: Vec<usize> = {
        let mut order: Vec<usize> = (0..roots.len()).collect();
        // Descending by priority, STABLE (equal priorities keep the
        // caller's relative order).
        order.sort_by(|&a, &b| roots[b].source.priority().cmp(&roots[a].source.priority()));
        order
    };

    // The deduped parse set (first eligible registration of a name wins)
    // plus the per-name merge state for the suppressed projection.
    let mut parsed: Vec<ParsedProfileFile> = Vec::new();
    let mut by_name: HashMap<String, NameMergeState> = HashMap::new();

    for &root_index in &scan_order {
        let root = &roots[root_index];
        // A missing root is "nothing here", not an error (TS semantics).
        if !fs.exists(&root.path) {
            continue;
        }
        let mut candidates: Vec<(PathBuf, u8)> = Vec::new();
        walk_profile_dir(
            &root.path,
            &root.path,
            0,
            fs,
            &mut candidates,
            &mut result.skipped,
        )?;

        // Parse in extension-priority order, then path — the deterministic
        // same-name conflict resolution (first eligible registration wins).
        candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        for (path, _rank) in candidates {
            let contents = match fs.read_text(&path) {
                Ok(contents) => contents,
                Err(err) => {
                    result.skipped.push(SkippedProfileFile {
                        path,
                        reason: format!("cannot read agent profile file: {err}"),
                    });
                    continue;
                }
            };
            match parse_profile_entry(&path, &contents, root.source) {
                Ok(entry) => {
                    let name = entry.profile.name.clone();
                    // D16: a candidate declaring the essential name is a
                    // normal candidate — it loads and (on winning) overrides
                    // the built-in content by priority, exactly like a
                    // non-essential built-in. No declaration key exists
                    // (D14: a file is override intent).
                    let state = by_name.entry(name).or_default();
                    if let Some(winner_index) = state.winner {
                        // A trailing same-name candidate lost the merge
                        // (R3-5). D13: a legal file that lost a merge never
                        // enters the skip channel (load failures only), so
                        // both the same-directory and the same-root
                        // cross-directory duplicate are silently dropped —
                        // the TS loader dedups within an entry without a
                        // diagnostic, and the TS model has no same-entry
                        // suppressed candidates. Only CROSS-ROOT losers
                        // become suppressed candidates with reason
                        // `priority`.
                        let winner_path = &parsed[winner_index].path;
                        if state.winner_root != root_index {
                            state.suppressed.push(SuppressedProfileCandidate {
                                source: root.source,
                                reason: SuppressedReason::Priority,
                            });
                        } else {
                            tracing::debug!(
                                path = %path.display(),
                                winner = %winner_path.display(),
                                "duplicate agent profile name within the same root; \
                                 the earlier registration wins"
                            );
                        }
                        continue;
                    }
                    // A disabled candidate never wins (the TS `:192-204`) —
                    // except the essential name, whose file flag is exempt
                    // (D16, mirroring [`is_file_disabled`]): the flag warns
                    // at consumption time and the candidate still wins, so
                    // the default dispatch target can never be bricked. A
                    // non-essential disabled candidate stays attached as a
                    // suppressed candidate to whoever wins next; when
                    // nothing else does, the first one is the
                    // disabled-winner fallback below.
                    if entry.profile.enabled == Some(false)
                        && entry.profile.name != ESSENTIAL_PROFILE_NAME
                    {
                        state.suppressed.push(SuppressedProfileCandidate {
                            source: root.source,
                            reason: SuppressedReason::Disabled,
                        });
                        if state.first_disabled.is_none() {
                            state.first_disabled = Some(entry);
                        }
                        continue;
                    }
                    state.winner = Some(parsed.len());
                    state.winner_root = root_index;
                    parsed.push(entry);
                }
                Err(err) => result.skipped.push(SkippedProfileFile {
                    path,
                    reason: err.to_string(),
                }),
            }
        }
    }

    // The TS `!winner` cases (R3-5): a name whose every candidate was
    // disabled falls back to the built-in when one exists (case a — the
    // built-in registration, which the manager seeds, stays the winner and
    // carries the suppressed candidates), otherwise to its first disabled
    // candidate (case b — the TS `disabledWinners`: registered,
    // listed-but-marked, never dispatched through [`ProfileManager::list`]).
    for (name, state) in &mut by_name {
        if state.winner.is_none()
            && let Some(first_disabled) = state.first_disabled.take()
            && !is_builtin_profile_name(name)
        {
            state.winner = Some(parsed.len());
            parsed.push(first_disabled);
        }
    }

    // Flatten legacy `extends` declarations now that the whole set is
    // parsed (parents resolve within the set + the built-ins), with
    // per-file skip-and-collect.
    let (mut profiles, flatten_skipped) = flatten_extends_entries(&parsed);
    result.skipped.extend(flatten_skipped);

    // D16: a user file that covered a built-in name displaces the built-in
    // definition into the winner's suppressed list —
    // `{source: builtin, priority: 0, reason: priority}` — appended LAST so
    // the order matches the TS catalog (file-candidate losers first, the
    // displaced built-in after them), isomorphic for the essential and
    // every non-essential built-in.
    //
    // This runs **after** the flattening pass, gated on the winner having
    // survived it (Task 27 review M1): a covering winner whose legacy
    // `extends` chain is broken is moved to the skip channel there, so the
    // name falls back to the built-in definition — appending the built-in
    // loser for it would leave a self-referential phantom candidate (the
    // row IS the built-in listing the built-in as its own loser).
    let survived_flatten: HashSet<&str> = profiles
        .iter()
        .map(|discovered| discovered.profile.name.as_str())
        .collect();
    for (name, state) in &mut by_name {
        if state.winner.is_some()
            && is_builtin_profile_name(name)
            && survived_flatten.contains(name.as_str())
        {
            state.suppressed.push(SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            });
        }
    }
    // Deterministic, TS-shaped output: sorted by profile name.
    profiles.sort_by(|a, b| a.profile.name.cmp(&b.profile.name));
    result.profiles = profiles;
    result.suppressed = by_name
        .into_iter()
        .filter_map(|(name, state)| {
            (!state.suppressed.is_empty()).then_some((name, state.suppressed))
        })
        .collect();
    Ok(result)
}

/// Walk one directory level of a profile root, collecting candidate files.
///
/// An unreadable **sub**directory is warn-and-skip (the TS strict-mode walk,
/// `discovery.ts:100-112`): the subtree contributes nothing, a
/// [`SkippedProfileFile`] entry records the failure (surfacing through the
/// caller's skip channel, where TS only warns — the rust R3 seam collects
/// the same fact), and the rest of the scan continues. Only an unreadable
/// **root** stays a hard error — the [`discover_profiles`] contract the
/// hot-reload keep-previous-state fallback relies on.
fn walk_profile_dir(
    root: &Path,
    dir: &Path,
    depth: usize,
    fs: &dyn SyncFs,
    candidates: &mut Vec<(PathBuf, u8)>,
    skipped: &mut Vec<SkippedProfileFile>,
) -> EngineResult<()> {
    if depth > MAX_PROFILE_SCAN_DEPTH {
        return Ok(());
    }
    let entries = match fs.iterdir(dir) {
        Ok(entries) => entries,
        Err(err) if dir != root => {
            // A locked subdirectory must not zero the scan: collect the
            // failure and keep walking the siblings.
            skipped.push(SkippedProfileFile {
                path: dir.to_path_buf(),
                reason: format!(
                    "cannot read agent profile directory '{}': {err}",
                    dir.display()
                ),
            });
            return Ok(());
        }
        Err(err) => {
            return Err(EngineError::RequestInvalid(format!(
                "cannot read agent profile directory '{}': {err}",
                root.display()
            )));
        }
    };
    let mut entries = entries;
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in entries {
        // Dot-entries and node_modules are pruned (TS walk semantics).
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let path = dir.join(&entry.path);
        if entry.is_dir {
            walk_profile_dir(root, &path, depth + 1, fs, candidates, skipped)?;
            continue;
        }
        if !entry.is_file {
            continue;
        }
        let extension = std::path::Path::new(&entry.name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        match extension.to_ascii_lowercase().as_str() {
            "yaml" | "yml" => {
                // The rust engine has no YAML profile parser; skip with an
                // explicit reason instead of a misleading "missing
                // frontmatter" from the `.md` path (Task 8 review note).
                skipped.push(SkippedProfileFile {
                    path,
                    reason: format!(
                        "unsupported profile extension '.{extension}': \
                         the rust engine reads .md/.toml/.json"
                    ),
                });
            }
            _ => {
                if let Some(rank) = extension_rank(extension) {
                    candidates.push((path, rank));
                }
                // Other extensions are not profile files — ignored by the
                // scan (the TS `isAgentFileName` filter).
            }
        }
    }
    Ok(())
}

/// Parse every profile file in `dir` (non-registering) — the single-root
/// convenience over [`discover_profiles`], tagging the root
/// [`ProfileSource::User`] (a bare directory is user-level). See that
/// function and the [module docs](self) for the discovery semantics.
pub fn parse_dir(dir: &Path, fs: &dyn SyncFs) -> EngineResult<ProfileDiscoveryResult> {
    let roots = [ProfileSourceRoot::from(dir.to_path_buf())];
    discover_profiles(&roots, fs)
}

/// The user-level agent profile root **candidates**, mirroring the TS
/// `userAgentRootCandidates` (`packages/agent-profile/src/roots.ts`,
/// Task 12): the brand directory `<mirri-home>/agents` first, then the
/// generic `<os-home>/.agents/agents`.
///
/// Candidates are returned without probing existence (the watch-side
/// counterpart of the roots resolution); callers filter to existing
/// directories before discovery. Project roots (`.mirri-code/agents`
/// under a project root, TS `projectAgentRoots`) are **R3/R4 scope**: the
/// rust boot path has no project-root notion feeding profile discovery
/// (the workspace root exists in the composition root, but profile
/// loading predates it) — do not invent a workspace registry here.
pub fn user_profile_root_candidates(mirri_home: &Path, os_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![mirri_home.join("agents")];
    if let Some(os_home) = os_home {
        roots.push(os_home.join(".agents").join("agents"));
    }
    roots
}

/// The legacy agent profile root `<mirri-home>/profiles` — scanned
/// read-only for existing `.toml`/`.json` users, after (i.e. with lower
/// priority than) the user agents roots, and cleaned of same-name files by
/// the `.md` write channel after a `.md` exists (R4-1, the TS
/// `removeLegacyFile` mirror).
pub fn legacy_profile_root(mirri_home: &Path) -> PathBuf {
    mirri_home.join("profiles")
}

/// Parse a provider name into a [`ProviderType`].
///
/// Recognizes `"anthropic"`, `"openai"`, `"openai-responses"`, `"google"`,
/// `"kimi"`, and `"custom"` (case-insensitive).
fn parse_provider_type(provider: &str) -> EngineResult<ProviderType> {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => Ok(ProviderType::Anthropic),
        "openai" => Ok(ProviderType::OpenAi),
        "openai-responses" | "openai_responses" => Ok(ProviderType::OpenAiResponses),
        "google" => Ok(ProviderType::Google),
        "kimi" => Ok(ProviderType::Kimi),
        "custom" => Ok(ProviderType::Custom),
        other => Err(EngineError::RequestInvalid(format!(
            "unknown provider '{other}' in agent profile; expected 'anthropic', 'openai', 'openai-responses', 'google', 'kimi', or 'custom'"
        ))),
    }
}

/// The built-in agent profiles available on every [`ProfileManager`].
///
/// Registered at construction (before user profiles are loaded), so a fresh
/// manager always has a working `agent`, `coder`, `explore`, and `plan`
/// profile. Registration is replace-by-name, so a user profile file with
/// the same name overrides the built-in — except the essential `agent`,
/// which discovery suppresses and `register` refuses (R3-2; see the
/// [module docs](self)).
///
/// Kept intentionally simple: a name and a basic system prompt (const
/// strings, no file dependencies). No built-in profile declares a model
/// (spec 11 R6/A7): built-in types ship responsibility, prompt, and tool
/// surface only — the model is decided by the resolution chain at request
/// time (explicit alias → profile default → caller inheritance), so the
/// empty `model` field here is the intended "unset" state.
fn builtin_profiles() -> Vec<AgentProfile> {
    vec![
        AgentProfile {
            name: "agent".to_string(),
            model: String::new(),
            description: Some("Default Mirri Code agent".to_string()),
            // No `system_prompt` and no `role_additional`: the plain base
            // template render. No `subagents` allowlist either — the
            // default agent can spawn any profile in the catalog, so
            // user-defined subagents are discoverable without code changes
            // (the TS v2 semantics; a profile that wants to restrict its
            // children declares its own `subagents` list).
            tools: Some(AGENT_PROFILE_TOOLS.iter().map(|t| t.to_string()).collect()),
            ..Default::default()
        },
        AgentProfile {
            name: "coder".to_string(),
            model: String::new(),
            description: Some(
                "General software engineering agent — the only subagent type with file-editing \
                 tools; use it for any delegated task that must modify code."
                    .to_string(),
            ),
            when_to_use: Some(
                "Use this agent for non-trivial software engineering work that may require \
                 reading files, editing code, running commands, and returning a compact but \
                 technically complete summary to the parent agent."
                    .to_string(),
            ),
            tools: Some(CODER_PROFILE_TOOLS.iter().map(|t| t.to_string()).collect()),
            prompt_vars: Some(HashMap::from([(
                "role_additional".to_string(),
                format!("{TASK_AGENT_ROLE_PREFIX}\n\n{CODER_ROLE}"),
            )])),
            summary_policy: Some(default_summary_policy()),
            ..Default::default()
        },
        AgentProfile {
            name: "explore".to_string(),
            model: String::new(),
            description: Some(
                "Fast codebase exploration with prompt-enforced read-only behavior.".to_string(),
            ),
            when_to_use: Some(
                "Fast agent specialized for exploring codebases. Use this when you need to \
                 quickly find files by patterns (e.g. \"src/**/*.yaml\"), search code for \
                 keywords (e.g. \"database connection\"), or answer questions about the codebase \
                 (e.g. \"how does the auth module work?\"). When calling this agent, specify the \
                 desired thoroughness level: \"quick\" for basic searches, \"medium\" for \
                 moderate exploration, or \"thorough\" for comprehensive analysis across multiple \
                 locations and naming conventions. Use this agent for any read-only exploration \
                 that will clearly require more than 3 search queries. Prefer launching multiple \
                 explore agents concurrently when investigating independent questions."
                    .to_string(),
            ),
            capabilities_required: Some(vec![
                "code.explore".to_string(),
                "code.read".to_string(),
                "web.search".to_string(),
                "web.fetch".to_string(),
            ]),
            tools: Some(
                EXPLORE_PROFILE_TOOLS
                    .iter()
                    .map(|t| t.to_string())
                    .collect(),
            ),
            prompt_vars: Some(HashMap::from([(
                "role_additional".to_string(),
                format!("{TASK_AGENT_ROLE_PREFIX}\n\n{EXPLORE_ROLE}"),
            )])),
            summary_policy: Some(default_summary_policy()),
            ..Default::default()
        },
        AgentProfile {
            name: "plan".to_string(),
            model: String::new(),
            description: Some(
                "Read-only implementation planning and architecture design.".to_string(),
            ),
            when_to_use: Some(
                "Use this agent when the parent agent needs a step-by-step implementation \
                 plan, key file identification, and architectural trade-off analysis before \
                 code changes are made."
                    .to_string(),
            ),
            capabilities_required: Some(vec![
                "code.explore".to_string(),
                "code.read".to_string(),
                "web.search".to_string(),
                "web.fetch".to_string(),
            ]),
            tools: Some(PLAN_PROFILE_TOOLS.iter().map(|t| t.to_string()).collect()),
            prompt_vars: Some(HashMap::from([(
                "role_additional".to_string(),
                format!("{TASK_AGENT_ROLE_PREFIX}\n\n{PLAN_ROLE}"),
            )])),
            ..Default::default()
        },
    ]
}

/// The subagent role prefix every task-agent profile's role text opens with
/// (the TS v2 `TASK_AGENT_ROLE_PREFIX` mirror).
const TASK_AGENT_ROLE_PREFIX: &str = "You are now running as a subagent. All the `user` messages are sent by the main agent. The main agent cannot see your context, it can only see your last message when you finish the task. You must treat the parent agent as your caller. Do not directly ask the end user questions. If something is unclear, explain the ambiguity in your final summary to the parent agent.";

/// The `agent` profile's tool whitelist (the TS v2 `AGENT_TOOLS` mirror):
/// the full builtin surface plus the every-server MCP glob.
const AGENT_PROFILE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Grep",
    "Glob",
    "Bash",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "CronCreate",
    "CronList",
    "CronDelete",
    "ReadMediaFile",
    "TodoList",
    "Skill",
    "WebSearch",
    "Agent",
    "AgentSwarm",
    "FetchURL",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "CreateGoal",
    "GetGoal",
    "SetGoalBudget",
    "UpdateGoal",
    "mcp__*",
];

/// The `coder` profile's tool whitelist (the TS v2 `CODER_TOOLS` mirror):
/// the editing tools plus dispatch, so a delegated coding task can fan out
/// further.
const CODER_PROFILE_TOOLS: &[&str] = &[
    "Agent",
    "AgentSwarm",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "EnterPlanMode",
    "ExitPlanMode",
    "Glob",
    "Grep",
    "Read",
    "ReadMediaFile",
    "Skill",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TodoList",
    "WebSearch",
    "FetchURL",
    "Write",
    "mcp__*",
];

/// The `explore` profile's tool whitelist (the TS v2 `EXPLORE_TOOLS`
/// mirror): read-only surface. `Bash` stays (read-only is
/// prompt-enforced); the dispatch and file-editing tools are absent, so an
/// explore agent cannot re-dispatch — the tool-level defense the nesting
/// depth guard only backs up.
const EXPLORE_PROFILE_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "ReadMediaFile",
    "Glob",
    "Grep",
    "WebSearch",
    "FetchURL",
];

/// The `plan` profile's tool whitelist (the TS v2 `PLAN_TOOLS` mirror):
/// reading, searching, and the web — no shell, no edits, no dispatch.
const PLAN_PROFILE_TOOLS: &[&str] = &[
    "Read",
    "ReadMediaFile",
    "Glob",
    "Grep",
    "WebSearch",
    "FetchURL",
];

/// The `coder` profile's role body (the TS v2 `CODER_ROLE` mirror) —
/// composed with [`TASK_AGENT_ROLE_PREFIX`] at the definition site, like
/// v2's `${TASK_AGENT_ROLE_PREFIX}\n\n` + body.
const CODER_ROLE: &str = "Your final message is the entire handoff — the parent sees nothing else from your run. Make it technically complete: what you changed and why, the path of every file you touched, how you verified the change (tests or commands run, with results), and anything left undone or worth follow-up. A final message of only a sentence or two is treated as too brief and sent back to you for expansion, costing an extra turn.";

/// The `explore` profile's role body (the TS v2 `EXPLORE_ROLE` mirror) —
/// composed with [`TASK_AGENT_ROLE_PREFIX`] at the definition site.
const EXPLORE_ROLE: &str = "\
You are a codebase exploration specialist. Your role is EXCLUSIVELY to search, read, and analyze existing code and resources. You do NOT have access to file editing tools.

Your strengths:
- Rapidly finding files using glob patterns
- Searching code and text with powerful regex
- Reading and analyzing file contents
- Running read-only shell commands (git log, git diff, ls, find, etc.)

Guidelines:
- Use Glob for broad file pattern matching. Prefer patterns with a literal anchor (extension or subdirectory); pure wildcards like `*` or `**/*` are allowed but usually truncate at the match cap.
- Use Grep for searching file contents with regex
- Use Read when you know the specific file path
- Use Bash ONLY for read-only operations (ls, git status, git log, git diff, find)
- NEVER use Bash for any file creation or modification commands";

/// The `plan` profile's role body (the TS v2 `PLAN_ROLE` mirror) —
/// composed with [`TASK_AGENT_ROLE_PREFIX`] at the definition site.
const PLAN_ROLE: &str = "\
Before designing your implementation plan, consider whether you fully understand the codebase areas relevant to the task. If not, recommend the parent agent to use the explore agent (subagent_type=\"explore\") to investigate key questions first. In your response, clearly state:
1. What you already know from the information provided
2. What questions remain unanswered that would benefit from explore agent investigation
3. Your implementation plan (either preliminary if questions remain, or final if sufficient context exists)

You are a read-only planning agent: you can read and search files (Read, Glob, Grep, ReadMediaFile) and consult the web (WebSearch, FetchURL), but you have no shell and no file-editing tools. Where the general instructions tell you to make changes with tools, that does not apply to you — do not attempt to run commands or modify files. Your deliverable is the plan itself, returned as your final message.";

/// The default summary-distillation policy the summarizing task profiles
/// carry (the TS v2 `DEFAULT_SUMMARY_POLICY` mirror: a 200-character
/// minimum, the continuation prompt, one retry).
fn default_summary_policy() -> ProfileSummaryPolicy {
    ProfileSummaryPolicy {
        min_chars: 200,
        continuation_prompt: SUMMARY_CONTINUATION_PROMPT.to_string(),
        retries: 1,
    }
}

/// The too-brief continuation prompt (the TS v2 `SUMMARY_CONTINUATION_PROMPT`
/// mirror).
const SUMMARY_CONTINUATION_PROMPT: &str = "\
Your previous response was too brief. Please provide a more comprehensive summary that includes:

1. Specific technical details and implementations
2. Detailed findings and analysis
3. All important information that the parent agent should know";

/// The provenance map seeding every fresh manager / [`ProfileManager::clear`]:
/// each built-in tagged [`ProfileSource::Builtin`] with no file (R3-5 —
/// builtins never flow through discovery, so the tag originates here).
fn builtin_provenance() -> HashMap<String, ProfileProvenance> {
    builtin_profiles()
        .into_iter()
        .map(|profile| {
            (
                profile.name,
                ProfileProvenance {
                    source: ProfileSource::Builtin,
                    file_path: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::ServerConfig;
    use crate::scope::config::ProviderType;
    use mirri_os::LocalSyncFs;

    /// The names of the built-in profiles registered on every manager.
    fn builtin_profile_names() -> Vec<String> {
        vec!["agent", "coder", "explore", "plan"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn sample_profile(name: &str) -> AgentProfile {
        AgentProfile {
            name: name.to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
            system_prompt: Some("You are helpful.".to_string()),
            system_prompt_file: None,
            max_turns: Some(50),
            max_steps_per_turn: Some(10),
            permission_mode: Some(PermissionMode::Yolo),
            tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            disallowed_tools: Some(vec!["Bash".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            thinking_effort: Some(ThinkingEffort::High),
            subagents: Some(vec!["explorer".to_string()]),
            description: None,
            when_to_use: None,
            ..Default::default()
        }
    }

    /// should expose only the built-in profiles when no user profiles are
    /// registered
    #[test]
    fn should_expose_only_builtin_profiles_when_no_user_profiles_are_registered() {
        let manager = ProfileManager::new();
        assert_eq!(manager.list(), builtin_profile_names());
        assert!(manager.resolve("does-not-exist", &LocalSyncFs).is_err());
    }

    /// Given the builtin profiles, when inspecting their tool whitelists,
    /// then every one carries the explicit TS v2 set: the default agent's
    /// whitelist covers the full tool surface plus the MCP glob, the coder
    /// keeps the editing tools, and the read-only task agents (explore,
    /// plan) exclude the dispatch and file-editing tools their roles must
    /// not have (explore keeps Bash — read-only is prompt-enforced, TS v2
    /// parity).
    #[test]
    fn should_carry_the_ts_tool_whitelists_on_every_builtin_profile() {
        let manager = ProfileManager::new();

        let agent = manager.get("agent").expect("builtin agent profile");
        let agent_tools = agent.tools.as_ref().expect("agent whitelist");
        for expected in [
            "Read",
            "Write",
            "Edit",
            "Bash",
            "Grep",
            "Glob",
            "Agent",
            "AgentSwarm",
        ] {
            assert!(
                agent_tools.contains(&expected.to_string()),
                "the default agent whitelist must include {expected}"
            );
        }
        assert!(
            agent_tools.contains(&"mcp__*".to_string()),
            "the default agent whitelist must carry the MCP glob"
        );

        let coder = manager.get("coder").expect("builtin coder profile");
        let coder_tools = coder.tools.as_ref().expect("coder whitelist");
        for expected in ["Edit", "Write", "Bash", "Agent", "AgentSwarm"] {
            assert!(
                coder_tools.contains(&expected.to_string()),
                "coder keeps {expected}"
            );
        }

        let explore = manager.get("explore").expect("builtin explore profile");
        let explore_tools = explore.tools.as_ref().expect("explore whitelist");
        assert!(
            explore_tools.contains(&"Bash".to_string()),
            "explore keeps Bash (read-only is prompt-enforced)"
        );
        for forbidden in ["Agent", "AgentSwarm", "Edit", "Write"] {
            assert!(
                !explore_tools.contains(&forbidden.to_string()),
                "explore must not carry the dispatch/edit tool {forbidden}"
            );
        }

        let plan = manager.get("plan").expect("builtin plan profile");
        let plan_tools = plan.tools.as_ref().expect("plan whitelist");
        for forbidden in ["Agent", "AgentSwarm", "Edit", "Write", "Bash"] {
            assert!(
                !plan_tools.contains(&forbidden.to_string()),
                "plan must not carry {forbidden}"
            );
        }
        for expected in ["Read", "Glob", "Grep"] {
            assert!(
                plan_tools.contains(&expected.to_string()),
                "plan keeps {expected}"
            );
        }
    }

    /// Given the builtin task profiles, when their prompt variables are
    /// read, then each carries the subagent role text (the TS
    /// `systemPromptTemplate` mirror under `role_additional`) opening with
    /// the subagent prefix — while the default agent carries none (its
    /// prompt is the plain base template).
    #[test]
    fn should_carry_the_subagent_role_text_on_the_builtin_task_profiles() {
        let manager = ProfileManager::new();

        for name in ["coder", "explore", "plan"] {
            let profile = manager.get(name).expect("task profile");
            let role = profile
                .prompt_vars
                .as_ref()
                .and_then(|vars| vars.get("role_additional"))
                .unwrap_or_else(|| panic!("{name} must carry role_additional"));
            assert!(
                role.starts_with("You are now running as a subagent."),
                "{name}'s role text must open with the subagent prefix: {role}"
            );
            assert!(
                profile.system_prompt.is_none(),
                "{name}'s prompt must ride the base template, not replace it"
            );
        }

        let agent = manager.get("agent").expect("agent profile");
        assert!(
            agent
                .prompt_vars
                .as_ref()
                .and_then(|vars| vars.get("role_additional"))
                .is_none(),
            "the default agent has no role text"
        );
    }

    /// Given the builtin profiles, when listing their metadata, then every
    /// one carries a description (the dispatch listing reads it) and the
    /// task agents carry when-to-use guidance.
    #[test]
    fn should_carry_descriptions_and_when_to_use_on_the_builtin_profiles() {
        let manager = ProfileManager::new();
        for name in builtin_profile_names() {
            let profile = manager.get(&name).expect("builtin profile");
            assert!(
                profile
                    .description
                    .as_deref()
                    .is_some_and(|d| !d.is_empty()),
                "{name} must carry a description"
            );
        }
        for name in ["coder", "explore", "plan"] {
            let profile = manager.get(name).expect("task profile");
            assert!(
                profile
                    .when_to_use
                    .as_deref()
                    .is_some_and(|w| !w.is_empty()),
                "{name} must carry when-to-use guidance"
            );
        }
    }

    /// Given the summarizing task profiles, when their summary policy is
    /// read, then each enforces the TS default distillation contract (a
    /// minimum length with a continuation prompt and one retry).
    #[test]
    fn should_carry_a_summary_policy_on_the_summarizing_task_profiles() {
        let manager = ProfileManager::new();
        for name in ["coder", "explore"] {
            let profile = manager.get(name).expect("task profile");
            let policy = profile
                .summary_policy
                .as_ref()
                .unwrap_or_else(|| panic!("{name} must carry a summary policy"));
            assert!(
                policy.min_chars > 0,
                "{name}'s policy sets a minimum length"
            );
            assert!(
                !policy.continuation_prompt.is_empty(),
                "{name}'s policy carries the continuation prompt"
            );
            assert!(policy.retries >= 1, "{name}'s policy retries at least once");
        }
    }

    /// Given the default agent profile, when its subagent allowlist is
    /// read, then it stays unset — `None` means every dispatchable profile
    /// is allowed, the load-bearing default the main agent's dispatch
    /// resolves through (`subagent_allowlist_for`).
    #[test]
    fn should_keep_the_default_agent_dispatchable_to_every_profile() {
        let manager = ProfileManager::new();
        let agent = manager.get("agent").expect("agent profile");
        assert!(
            agent.subagents.is_none(),
            "the default agent's allowlist must stay unset (all dispatchable profiles)"
        );
    }

    /// Given a builtin task profile, when it is resolved into a scope
    /// config, then the role text flows to the config's `role_additional`
    /// (for the base-template render) and the tool whitelist flows to
    /// `enabled_tools`.
    #[test]
    fn should_flow_the_role_text_into_the_scope_config_for_a_builtin_task_profile() {
        let manager = ProfileManager::new();
        let config = manager
            .resolve("explore", &LocalSyncFs)
            .expect("explore resolves");
        assert!(
            config
                .role_additional
                .as_deref()
                .is_some_and(|r| r.starts_with("You are now running as a subagent.")),
            "the role text must flow to the scope config"
        );
        let enabled = config.enabled_tools.expect("whitelist flows");
        assert!(!enabled.contains(&"Agent".to_string()));
        assert!(enabled.contains(&"Bash".to_string()));
    }

    /// Given an agent profile that declares no model, when it is resolved
    /// into a scope config, then the resolution succeeds with an empty
    /// `model_id` — the "no model" state is legal at the profile level
    /// (spec 11 R6); the request-time resolution chain is what fails
    /// loudly, not the profile resolution.
    #[test]
    fn should_accept_profile_without_model() {
        let profile = AgentProfile {
            name: "model-less".to_string(),
            ..Default::default()
        };

        let config = ProfileManager::scope_config_from_profile(&profile, &LocalSyncFs)
            .expect("a profile without a model must resolve");

        assert!(
            config.model_id.is_empty(),
            "the missing model passes through as the empty/unset model id"
        );
    }

    /// Given the built-in agent profiles, when they are inspected, then
    /// none of them declares a concrete model (spec 11 R6/A7 — built-in
    /// types ship responsibility, prompt, and tool surface only; the
    /// model is decided by the resolution chain at request time).
    #[test]
    fn should_have_no_model_on_builtin_profiles() {
        for profile in builtin_profiles() {
            assert!(
                profile.model.is_empty(),
                "built-in profile '{}' must not declare a model",
                profile.name
            );
        }
    }

    #[test]
    fn should_return_registered_profile_when_get_is_called() {
        let manager = ProfileManager::new();
        manager.register(sample_profile("coder")).unwrap();
        let profile = manager.get("coder").expect("profile should exist");
        assert_eq!(profile.name, "coder");
        assert_eq!(profile.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn should_return_last_registered_definition_when_getting_profile_registered_twice() {
        let manager = ProfileManager::new();
        manager.register(sample_profile("coder")).unwrap();
        let mut updated = sample_profile("coder");
        updated.model = "gpt-4o".to_string();
        manager.register(updated).unwrap();

        let profile = manager.get("coder").expect("profile should exist");
        assert_eq!(profile.model, "gpt-4o");
        // The custom "coder" replaced the built-in of the same name; the
        // other built-ins are unaffected.
        assert_eq!(manager.list(), builtin_profile_names());
    }

    #[test]
    fn should_clear_user_profiles_but_keep_builtins_when_clear_is_called() {
        // Given a manager with several registered profiles
        let manager = ProfileManager::new();
        manager.register(sample_profile("coder")).unwrap();
        manager.register(sample_profile("explorer")).unwrap();
        // 4 built-ins, plus "explorer" (a new name); "coder" replaced the
        // built-in of the same name rather than adding a sixth entry.
        assert_eq!(manager.list().len(), 5);

        // When clear is called
        manager.clear();

        // Then user profiles are removed while the built-ins survive
        assert_eq!(manager.list(), builtin_profile_names());
        assert!(manager.resolve("explorer", &LocalSyncFs).is_err());
        assert!(manager.resolve("agent", &LocalSyncFs).is_ok());
    }

    #[test]
    fn should_list_profiles_sorted_by_name_when_multiple_are_registered() {
        let manager = ProfileManager::new();
        manager.register(sample_profile("zed")).unwrap();
        manager.register(sample_profile("alpha")).unwrap();
        manager.register(sample_profile("mike")).unwrap();
        assert_eq!(
            manager.list(),
            vec!["agent", "alpha", "coder", "explore", "mike", "plan", "zed"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_map_all_fields_when_resolving_full_profile() {
        let manager = ProfileManager::new();
        manager.register(sample_profile("coder")).unwrap();

        let config = manager
            .resolve("coder", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "claude-sonnet-4-20250514");
        assert_eq!(config.provider_type, ProviderType::Anthropic);
        assert_eq!(config.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(config.max_turns, 50);
        assert_eq!(config.permission_mode, PermissionMode::Yolo);
        assert_eq!(config.max_steps_per_turn, Some(10));
        assert_eq!(
            config.enabled_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(config.disabled_tools, Some(vec!["Bash".to_string()]));
        assert_eq!(
            config.capabilities_required,
            Some(vec!["code.read".to_string()])
        );
        assert_eq!(config.thinking_effort, Some(ThinkingEffort::High));
    }

    #[test]
    fn should_apply_defaults_when_resolving_minimal_profile() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "minimal".to_string(),
                model: "gpt-4o".to_string(),
                provider: Some("openai".to_string()),
                system_prompt: None,
                system_prompt_file: None,
                max_turns: None,
                max_steps_per_turn: None,
                permission_mode: None,
                tools: None,
                disallowed_tools: None,
                capabilities_required: None,
                thinking_effort: None,
                subagents: None,
                description: None,
                when_to_use: None,
                ..Default::default()
            })
            .unwrap();

        let config = manager
            .resolve("minimal", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "gpt-4o");
        assert_eq!(config.provider_type, ProviderType::OpenAi);
        assert_eq!(config.system_prompt, None);
        assert_eq!(config.max_turns, AgentScopeConfig::default().max_turns);
        assert_eq!(config.permission_mode, PermissionMode::Manual);
        assert_eq!(config.max_steps_per_turn, None);
        assert_eq!(config.enabled_tools, None);
        assert_eq!(config.thinking_effort, None);
    }

    #[test]
    fn should_resolve_kimi_provider_type_when_profile_provider_is_kimi() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "kimi-minimal".to_string(),
                model: "kimi-k2-0711-preview".to_string(),
                provider: Some("kimi".to_string()),
                system_prompt: None,
                system_prompt_file: None,
                max_turns: None,
                max_steps_per_turn: None,
                permission_mode: None,
                tools: None,
                disallowed_tools: None,
                capabilities_required: None,
                thinking_effort: None,
                subagents: None,
                description: None,
                when_to_use: None,
                ..Default::default()
            })
            .unwrap();

        let config = manager
            .resolve("kimi-minimal", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.provider_type, ProviderType::Kimi);
        assert_eq!(config.model_id, "kimi-k2-0711-preview");
    }

    #[test]
    fn should_error_when_resolving_profile_with_unknown_provider() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "weird".to_string(),
                model: "x".to_string(),
                provider: Some("deepseek".to_string()),
                system_prompt: None,
                system_prompt_file: None,
                max_turns: None,
                max_steps_per_turn: None,
                permission_mode: None,
                tools: None,
                disallowed_tools: None,
                capabilities_required: None,
                thinking_effort: None,
                subagents: None,
                description: None,
                when_to_use: None,
                ..Default::default()
            })
            .unwrap();

        let error = manager
            .resolve("weird", &LocalSyncFs)
            .expect_err("should fail");
        assert!(error.to_string().contains("unknown provider 'deepseek'"));
    }

    #[test]
    fn should_read_system_prompt_from_file_when_resolving_profile_with_prompt_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let prompt_path = temp_dir.path().join("system.md");
        fs::write(&prompt_path, "You are a meticulous engineer.").expect("write should succeed");

        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "file-prompt".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                provider: None,
                system_prompt: None,
                system_prompt_file: Some(prompt_path),
                max_turns: None,
                max_steps_per_turn: None,
                permission_mode: None,
                tools: None,
                disallowed_tools: None,
                capabilities_required: None,
                thinking_effort: None,
                subagents: None,
                description: None,
                when_to_use: None,
                ..Default::default()
            })
            .unwrap();

        let config = manager
            .resolve("file-prompt", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(
            config.system_prompt.as_deref(),
            Some("You are a meticulous engineer.")
        );
    }

    #[test]
    fn should_error_when_resolving_profile_with_missing_system_prompt_file() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "broken".to_string(),
                model: "x".to_string(),
                provider: None,
                system_prompt: None,
                system_prompt_file: Some(PathBuf::from("/nonexistent/prompt.md")),
                max_turns: None,
                max_steps_per_turn: None,
                permission_mode: None,
                tools: None,
                disallowed_tools: None,
                capabilities_required: None,
                thinking_effort: None,
                subagents: None,
                description: None,
                when_to_use: None,
                ..Default::default()
            })
            .unwrap();

        let error = manager
            .resolve("broken", &LocalSyncFs)
            .expect_err("should fail");
        assert!(error.to_string().contains("cannot read system prompt file"));
    }

    #[test]
    fn should_register_profile_when_loading_toml_profile_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("coder.toml");
        fs::write(
            &path,
            r#"
name = "coder"
model = "claude-sonnet-4-20250514"
provider = "anthropic"
system_prompt = "You are a meticulous software engineer."
max_turns = 120
permission_mode = "yolo"
thinking_effort = "high"

[[subagents]]
name = "explorer"
model = "claude-haiku-4-20250514"
"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(name, "coder");

        let config = manager
            .resolve("coder", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "claude-sonnet-4-20250514");
        assert_eq!(config.max_turns, 120);
        assert_eq!(config.permission_mode, PermissionMode::Yolo);
        assert_eq!(config.thinking_effort, Some(ThinkingEffort::High));

        let profile = manager.get("coder").expect("profile should exist");
        // The legacy `[[subagents]]` inline definition migrated to its name.
        assert_eq!(profile.subagents, Some(vec!["explorer".to_string()]));
    }

    #[test]
    fn should_register_profile_when_loading_json_profile_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("coder.json");
        fs::write(
            &path,
            // The legacy `enabled_tools` spelling: accepted on read via the
            // serde alias, so files written by older versions keep loading.
            r#"{
  "name": "coder",
  "model": "gpt-4o",
  "provider": "openai",
  "permission_mode": "auto",
  "enabled_tools": ["Read", "Grep"]
}"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(name, "coder");

        let config = manager
            .resolve("coder", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "gpt-4o");
        assert_eq!(config.provider_type, ProviderType::OpenAi);
        assert_eq!(config.permission_mode, PermissionMode::Auto);
        // The legacy key landed in the renamed `tools` field.
        assert_eq!(
            config.enabled_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
    }

    /// Given a JSON profile declaring `"extends": null`, When the file is
    /// loaded, Then it loads exactly like a file without the key — JSON's
    /// null is "not declared", never an invalid-extends load error (the
    /// Task-9-era edge regression, never pinned before).
    #[test]
    fn should_treat_a_json_null_extends_as_an_absent_extends_when_loading() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("nullable.json");
        fs::write(
            &path,
            r#"{
  "name": "nullable",
  "model": "gpt-4o",
  "extends": null
}"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("a null extends must load like an absent one");
        assert_eq!(name, "nullable");
        assert!(manager.resolve("nullable", &LocalSyncFs).is_ok());
        // A non-string, non-null extends stays a load error (the old
        // `Option<String>` behavior).
        fs::write(&path, r#"{ "name": "bad", "model": "m", "extends": 42 }"#)
            .expect("write should succeed");
        assert!(manager.load_from_file(&path, &LocalSyncFs).is_err());
    }

    /// Given a TOML profile declaring both `tools` and the legacy
    /// `enabled_tools` spelling in the same table, When the directory is
    /// discovered, Then the file is skipped with serde's duplicate-field
    /// error instead of one spelling silently winning — the alias is
    /// read-compat for old files, not a merge of two live keys.
    #[test]
    fn should_skip_the_file_when_both_tool_spellings_are_present() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("mkdir should succeed");
        fs::write(
            profiles_dir.join("dual.toml"),
            "name = \"dual\"\nmodel = \"gpt-4o\"\ntools = [\"Read\"]\nenabled_tools = [\"Grep\"]\n",
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the scan itself must not fail (skip-and-collect)");
        assert_eq!(loaded, 0, "the dual-spelling file must not register");

        let skipped = manager.last_skipped_files();
        assert_eq!(
            skipped.len(),
            1,
            "exactly the dual-spelling file is skipped"
        );
        assert!(
            skipped[0].reason.contains("duplicate field"),
            "the serde duplicate-field error must surface: {}",
            skipped[0].reason
        );
    }

    /// Given a discovery root holding an unreadable subdirectory plus
    /// readable profiles, and a second healthy root, When the roots load,
    /// Then the unreadable subtree is skipped with a collected reason
    /// (warn-and-skip, the TS walk) while every other profile still loads.
    #[cfg(unix)]
    #[test]
    fn should_skip_an_unreadable_subdirectory_but_keep_loading_the_other_roots() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let first = temp_dir.path().join("first");
        let second = temp_dir.path().join("second");
        fs::create_dir_all(first.join("locked")).expect("mkdir should succeed");
        fs::create_dir_all(&second).expect("mkdir should succeed");
        fs::write(
            first.join("kept.toml"),
            "name = \"kept\"\nmodel = \"gpt-4o\"\n",
        )
        .expect("write should succeed");
        fs::write(
            first.join("locked").join("hidden.toml"),
            "name = \"hidden\"\nmodel = \"gpt-4o\"\n",
        )
        .expect("write should succeed");
        fs::write(
            second.join("other.toml"),
            "name = \"other\"\nmodel = \"gpt-4o\"\n",
        )
        .expect("write should succeed");
        fs::set_permissions(first.join("locked"), fs::Permissions::from_mode(0o000))
            .expect("chmod should succeed");

        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_roots(&[first.clone(), second], &LocalSyncFs)
            .expect("one unreadable subdirectory must not abort the scan");

        // Restore readability so the tempdir cleanup can delete the tree.
        fs::set_permissions(first.join("locked"), fs::Permissions::from_mode(0o755))
            .expect("restore chmod should succeed");

        assert_eq!(loaded, 2, "both readable profiles must register");
        assert!(manager.get("kept").is_some());
        assert!(manager.get("other").is_some());
        assert!(
            manager.get("hidden").is_none(),
            "the locked subtree yields nothing"
        );

        let skipped = manager.last_skipped_files();
        let locked = skipped
            .iter()
            .find(|entry| entry.path.ends_with("locked"))
            .expect("the unreadable subdirectory is recorded on the skip channel");
        assert!(
            locked
                .reason
                .contains("cannot read agent profile directory"),
            "the skip reason names the failure: {}",
            locked.reason
        );
        assert_eq!(skipped.len(), 1, "nothing else was skipped: {skipped:?}");
    }

    /// Given a legacy `.toml` profile declaring `[[subagents]]` inline
    /// definitions (the pre-R3-4 model), When the file is loaded, Then it
    /// still loads (not skipped) and the definitions migrate to their
    /// extracted names as the R3-4 allowlist.
    #[test]
    fn should_migrate_legacy_inline_subagent_definitions_when_loading_toml() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("mkdir should succeed");
        fs::write(
            profiles_dir.join("curator.toml"),
            r#"
name = "curator"
model = "claude-sonnet-4-20250514"

[[subagents]]
name = "scout"
model = "claude-haiku-4-20250514"
system_prompt = "You scout."
tools = ["Read"]

[[subagents]]
name = "probe"
"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(loaded, 1);
        assert!(manager.last_skipped_files().is_empty());

        let profile = manager.get("curator").expect("profile should exist");
        // The inline definitions' names survive (in file order); the
        // model/prompt/tool overrides are dropped.
        assert_eq!(
            profile.subagents,
            Some(vec!["scout".to_string(), "probe".to_string()])
        );
    }

    /// Given a legacy `.json` profile using the v1 `Record` subagents form,
    /// When the file is loaded, Then the record's keys become the name
    /// allowlist (the TS `parseSubagents` normalization).
    #[test]
    fn should_migrate_legacy_subagents_record_when_loading_json() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("curator.json");
        fs::write(
            &path,
            r#"{
  "name": "curator",
  "model": "claude-sonnet-4-20250514",
  "subagents": {
    "scout": { "description": "scouts" },
    "probe": { "description": "probes" }
  }
}"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(name, "curator");

        let profile = manager.get("curator").expect("profile should exist");
        let mut names = profile.subagents.expect("names should be extracted");
        names.sort();
        assert_eq!(names, vec!["probe".to_string(), "scout".to_string()]);
    }

    /// Given a native name-list `subagents`, When the file is loaded,
    /// Then the list passes through untouched (no migration).
    #[test]
    fn should_keep_native_subagents_name_list_when_loading_toml() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("curator.toml");
        fs::write(
            &path,
            r#"
name = "curator"
model = "claude-sonnet-4-20250514"
subagents = ["coder", "explore"]
"#,
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("load should succeed");

        let profile = manager.get("curator").expect("profile should exist");
        assert_eq!(
            profile.subagents,
            Some(vec!["coder".to_string(), "explore".to_string()])
        );
    }

    /// Given a `.md` agent file declaring `subagents` names in its
    /// frontmatter, When it is loaded through discovery, Then the names map
    /// onto the engine model's allowlist (the R3-4 bridge).
    #[test]
    fn should_map_md_subagents_names_when_loading_agent_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("agents");
        fs::create_dir(&profiles_dir).expect("mkdir should succeed");
        fs::write(
            profiles_dir.join("curator.md"),
            "---\nname: curator\ndescription: Curates things.\nsubagents:\n  - coder\n  - explore\n---\nYou curate.",
        )
        .expect("write should succeed");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("parse should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(
            result.profiles[0].profile.subagents,
            Some(vec!["coder".to_string(), "explore".to_string()])
        );
    }

    /// Given a caller bound to a profile with its own allowlist, When the
    /// effective allowlist is resolved, Then it is exactly that profile's
    /// list — a named caller with no own list allows everything and does
    /// NOT inherit the default's.
    #[test]
    fn should_resolve_caller_own_list_and_not_inherit_default_for_named_callers() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "reviewer".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                subagents: Some(vec!["coder".to_string()]),
                ..AgentProfile::default()
            })
            .unwrap();
        manager
            .register(AgentProfile {
                name: "planner".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                ..AgentProfile::default()
            })
            .unwrap();

        assert_eq!(
            subagent_allowlist_for(&manager, Some("reviewer")),
            Some(vec!["coder".to_string()])
        );
        // A named caller with no own list = all dispatchable profiles.
        assert_eq!(subagent_allowlist_for(&manager, Some("planner")), None);
    }

    /// Given a caller with no profile (the session's main agent), When the
    /// effective allowlist is resolved, Then it inherits the DEFAULT
    /// profile's list — the essential `agent` (the TS `subagentAllowlistFor`
    /// default arm).
    #[test]
    fn should_inherit_default_profile_allowlist_for_unbound_caller() {
        let manager = ProfileManager::new();
        // The essential profile is locked against every public channel
        // (R3-2), so its allowlist is injected through the private registry
        // the same way `new()` seeds it — the default arm's data cannot
        // flow from a user file until the R4 override channel exists.
        let mut essential = builtin_profiles()
            .into_iter()
            .find(|profile| profile.name == ESSENTIAL_PROFILE_NAME)
            .expect("the essential profile is registered");
        essential.subagents = Some(vec!["coder".to_string(), "explore".to_string()]);
        manager
            .profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(ESSENTIAL_PROFILE_NAME.to_string(), essential);

        assert_eq!(
            subagent_allowlist_for(&manager, None),
            Some(vec!["coder".to_string(), "explore".to_string()])
        );
    }

    /// Given no default-profile allowlist either, When an unbound caller
    /// resolves, Then everything is allowed (`None`).
    #[test]
    fn should_allow_all_when_neither_caller_nor_default_declares_a_list() {
        let manager = ProfileManager::new();
        assert_eq!(subagent_allowlist_for(&manager, None), None);
    }

    /// Given a caller bound to a profile name that is NOT registered,
    /// When the effective allowlist is resolved, Then everything is
    /// allowed (`None`) — an absent profile cannot restrict dispatch
    /// (T18 m1: the locked rust semantics; the TS `subagentAllowlistFor`
    /// reads the caller's own stale `subagents` snapshot instead — a
    /// launch-time copy — so an absent profile there inherits whatever
    /// list the caller record was created with. The stale list is a TS
    /// defect, not a target: rust resolves live and treats absence as
    /// "no restriction declared").
    #[test]
    fn should_allow_all_when_the_named_callers_profile_is_absent() {
        let manager = ProfileManager::new();
        assert_eq!(subagent_allowlist_for(&manager, Some("ghost")), None);
        // The absent arm does not leak into the default arm: an unbound
        // caller still resolves through the essential's own list.
        assert_eq!(subagent_allowlist_for(&manager, None), None);
    }

    /// Given an allowlist refusal, When the message is rendered, Then it
    /// matches the TS `subagentTypeNotAllowedMessage` exactly.
    #[test]
    fn should_render_ts_aligned_not_allowed_message() {
        assert_eq!(
            subagent_type_not_allowed_message(
                "explore",
                &["coder".to_string(), "reviewer".to_string()],
            ),
            "Subagent type \"explore\" is not allowed for this agent. \
             Allowed subagent types: coder, reviewer."
        );
        // An empty allowlist renders as `none`.
        assert_eq!(
            subagent_type_not_allowed_message("explore", &[]),
            "Subagent type \"explore\" is not allowed for this agent. \
             Allowed subagent types: none."
        );
    }

    #[test]
    fn should_error_when_loading_file_with_unsupported_extension() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("coder.yaml");
        fs::write(&path, "name: coder").expect("write should succeed");

        let manager = ProfileManager::new();
        let error = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect_err("should fail for .yaml");
        assert!(
            error
                .to_string()
                .contains("unsupported agent profile extension")
        );
    }

    #[test]
    fn should_error_when_loading_file_with_invalid_toml() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("coder.toml");
        fs::write(&path, "name = [unclosed").expect("write should succeed");

        let manager = ProfileManager::new();
        let error = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect_err("should fail for malformed toml");
        assert!(error.to_string().contains("invalid TOML agent profile"));
    }

    #[test]
    fn should_register_all_profiles_when_loading_directory_of_profiles() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write should succeed");
        fs::write(
            profiles_dir.join("explore.json"),
            r#"{"name": "explore", "model": "claude-haiku-4-20250514"}"#,
        )
        .expect("write should succeed");
        fs::write(profiles_dir.join("notes.txt"), "not a profile").expect("write should succeed");

        let manager = ProfileManager::new();
        let count = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(count, 2);
        // "coder" and "explore" override the built-in profiles of the same
        // name; the built-in "agent" and "plan" remain.
        assert_eq!(manager.list(), builtin_profile_names());
    }

    // ===== R2-3: multi-format discovery + priority + skip-and-collect =====

    /// Write a minimal `.md` agent file.
    fn write_md_file(dir: &Path, file_name: &str, name: &str, prompt: &str) -> PathBuf {
        write_profile_file(
            dir,
            file_name,
            &format!("---\nname: {name}\ndescription: {name} agent\n---\n{prompt}"),
        )
    }

    /// should let the .md file win when .md, .toml, and .json declare the
    /// same profile name in one directory
    #[test]
    fn should_prefer_md_over_toml_and_json_for_the_same_profile_name() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(
            &profiles_dir,
            "coder.md",
            "coder",
            "You are the markdown coder.",
        );
        write_profile_file(
            &profiles_dir,
            "coder.toml",
            "name = \"coder\"\nmodel = \"from-toml\"\n",
        );
        write_profile_file(
            &profiles_dir,
            "coder.json",
            r#"{"name": "coder", "model": "from-json"}"#,
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        // One profile — the .md — and the shadowed files produce no skip
        // entries (D13: a legal loser is never a load failure; the TS
        // loader dedups same-entry duplicates without a diagnostic).
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].path, profiles_dir.join("coder.md"));
        assert_eq!(
            result.profiles[0].profile.system_prompt.as_deref(),
            Some("You are the markdown coder.")
        );
        assert!(result.skipped.is_empty());
    }

    /// should let .toml win over .json for the same profile name when no
    /// .md exists (the documented deterministic choice)
    #[test]
    fn should_prefer_toml_over_json_for_the_same_profile_name_without_md() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        // json sorts before toml in path order; the priority rule (not the
        // sort) must decide, and it picks .toml — the same winner the
        // pre-R2-3 last-registration-sorted load produced.
        write_profile_file(
            &profiles_dir,
            "coder.json",
            r#"{"name": "coder", "model": "from-json"}"#,
        );
        write_profile_file(
            &profiles_dir,
            "coder.toml",
            "name = \"coder\"\nmodel = \"from-toml\"\n",
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].path, profiles_dir.join("coder.toml"));
        assert_eq!(result.profiles[0].profile.model, "from-toml");
        // D13: the shadowed .json produces no skip entry (a legal loser,
        // not a load failure).
        assert!(result.skipped.is_empty());
    }

    /// should return the two good profiles and one skipped entry when a
    /// directory holds one malformed and two valid .md files
    #[test]
    fn should_skip_one_bad_md_and_load_the_rest() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(&profiles_dir, "good-a.md", "good-a", "A.");
        write_profile_file(&profiles_dir, "bad.md", "No frontmatter at all.");
        write_md_file(&profiles_dir, "good-b.md", "good-b", "B.");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 2);
        assert_eq!(result.profiles[0].profile.name, "good-a");
        assert_eq!(
            result.profiles[0].profile.system_prompt.as_deref(),
            Some("A.")
        );
        assert_eq!(result.profiles[1].profile.name, "good-b");
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].path, profiles_dir.join("bad.md"));
        assert!(
            result.skipped[0].reason.contains("frontmatter"),
            "should carry the parse reason: {}",
            result.skipped[0].reason
        );
    }

    /// should skip .yaml and .yml files with an unsupported-extension reason
    #[test]
    fn should_skip_yaml_and_yml_files_with_an_unsupported_extension_reason() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(&profiles_dir, "legacy.yaml", "name: legacy\n");
        write_profile_file(&profiles_dir, "legacy.yml", "name: legacy\n");
        write_profile_file(&profiles_dir, "notes.txt", "not a profile");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        // The YAML files are skipped with the explicit reason (they must
        // not reach the .md parser and its misleading "missing frontmatter"
        // error)...
        assert_eq!(result.skipped.len(), 2);
        for skipped in &result.skipped {
            assert!(
                skipped.reason.contains("unsupported profile extension"),
                "should name the unsupported extension: {}",
                skipped.reason
            );
            assert!(
                skipped.reason.contains(".md/.toml/.json"),
                "should point at the supported set: {}",
                skipped.reason
            );
        }
        // ...while the unknown .txt extension is ignored by the scan (the
        // TS isAgentFileName behavior), not collected as a skip.
        assert!(result.profiles.is_empty());
    }

    /// should key profiles by the declared name, not the file stem, and let
    /// the first same-name .md in sort order win
    #[test]
    fn should_key_profiles_by_declared_name_with_first_registration_winning() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        // The file stem says "zzz" but the frontmatter name is "alpha" —
        // the declared name is the key (the TS dedup semantics).
        write_md_file(&profiles_dir, "zzz.md", "alpha", "I am alpha.");
        // Two files declaring the same name: the alphabetically-first
        // registers, the other is a same-directory duplicate (silently
        // dropped, D13 — never a skip entry).
        write_md_file(&profiles_dir, "a-dup.md", "shared", "A wins.");
        write_md_file(&profiles_dir, "b-dup.md", "shared", "B loses.");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 2);
        let alpha = result
            .profiles
            .iter()
            .find(|discovered| discovered.profile.name == "alpha")
            .expect("alpha is keyed by the frontmatter name");
        assert_eq!(alpha.path, profiles_dir.join("zzz.md"));
        let shared = result
            .profiles
            .iter()
            .find(|discovered| discovered.profile.name == "shared")
            .expect("shared is loaded once");
        assert_eq!(shared.profile.system_prompt.as_deref(), Some("A wins."));
        assert!(result.skipped.is_empty());
    }

    /// should discover subdirectories and prune dot-dirs and node_modules
    #[test]
    fn should_discover_subdirectories_and_prune_hidden_and_node_modules_dirs() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        let nested = profiles_dir.join("team");
        let hidden = profiles_dir.join(".hidden");
        let vendor = profiles_dir.join("node_modules");
        for dir in [&nested, &hidden, &vendor] {
            fs::create_dir_all(dir).expect("create_dir_all should succeed");
        }
        write_md_file(&nested, "reviewer.md", "reviewer", "You review.");
        write_md_file(&hidden, "ghost.md", "ghost", "Boo.");
        write_md_file(&vendor, "vendor.md", "vendor", "Vend.");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "reviewer");
        assert!(result.skipped.is_empty());
    }

    /// should let the earlier root win a cross-root name collision, report
    /// every scanned root, and attach the cross-root loser as a suppressed
    /// `priority` candidate
    #[test]
    fn should_let_the_earlier_root_win_a_cross_root_name_collision() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&agents_dir).expect("create_dir should succeed");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(&agents_dir, "coder.md", "coder", "From the agents root.");
        write_profile_file(
            &profiles_dir,
            "coder.toml",
            "name = \"coder\"\nmodel = \"from-profiles\"\n",
        );
        write_md_file(
            &profiles_dir,
            "other.md",
            "other",
            "From the profiles root.",
        );

        let roots: Vec<ProfileSourceRoot> = [agents_dir.clone(), profiles_dir.clone()]
            .into_iter()
            .map(Into::into)
            .collect();
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");
        // Both roots load; the earlier root (agents) wins the "coder"
        // collision (the user chain order: brand agents root over the
        // legacy profiles dir), and the cross-root loser attaches to the
        // winner as a suppressed `priority` candidate (R3-5) instead of
        // vanishing.
        assert_eq!(result.profiles.len(), 2);
        let coder = result
            .profiles
            .iter()
            .find(|discovered| discovered.profile.name == "coder")
            .expect("coder is loaded");
        assert_eq!(coder.path, agents_dir.join("coder.md"));
        assert_eq!(
            coder.profile.system_prompt.as_deref(),
            Some("From the agents root.")
        );
        assert_eq!(coder.source, ProfileSource::User);
        assert!(result.skipped.is_empty());
        // The cross-root loser attaches as a suppressed `priority`
        // candidate (R3-5), and — because the winner covers a built-in
        // name — the displaced built-in definition is appended after it
        // (D16: `{source: builtin, priority: 0, reason: priority}`).
        assert_eq!(
            result.suppressed["coder"],
            vec![
                SuppressedProfileCandidate {
                    source: ProfileSource::User,
                    reason: SuppressedReason::Priority,
                },
                SuppressedProfileCandidate {
                    source: ProfileSource::Builtin,
                    reason: SuppressedReason::Priority,
                },
            ]
        );
        assert_eq!(
            result.scanned_roots,
            vec![agents_dir.clone(), profiles_dir.clone()]
        );
    }

    /// should not leave a phantom builtin-loser candidate when a broken
    /// legacy `extends` chain skips the file covering a built-in name
    /// (Task 27 review M1)
    #[test]
    fn should_not_leave_a_phantom_builtin_loser_when_a_broken_extends_skips_the_covering_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        // A legacy `.toml` covering the built-in `coder` name whose
        // `extends` chain is broken (unknown parent): a legitimate load
        // failure that flattening moves to the skip channel.
        write_profile_file(
            &profiles_dir,
            "coder.toml",
            "name = \"coder\"\nextends = \"no-such-parent\"\n",
        );

        let manager = ProfileManager::new();
        manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("load should succeed");
        // The covering file lands in the skip channel …
        let skipped = manager.last_skipped_files();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].path.ends_with("coder.toml"));
        // … and the built-in row stays normal: the built-in definition is
        // still registered (the manager's seeding) with provenance
        // `builtin` and NO suppressed candidates — the builtin-loser
        // append must not survive a winner that flattening moved to the
        // skip channel (that would leave a self-referential phantom
        // candidate: the row IS the built-in listing the built-in).
        assert!(manager.get("coder").is_some());
        assert_eq!(
            manager.provenance("coder").map(|p| p.source),
            Some(ProfileSource::Builtin)
        );
        assert!(manager.suppressed_candidates("coder").is_empty());
    }

    /// should chain the parent's skip reason when a child extends a parent
    /// whose own flattening failed
    #[test]
    fn should_chain_the_parent_skip_reason_when_a_child_extends_a_skipped_parent() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        // The parent extends an unknown profile (itself skipped); the
        // child extending the parent must carry the chained reason.
        write_profile_file(
            &profiles_dir,
            "base.toml",
            "name = \"base\"\nextends = \"missing\"\n",
        );
        write_profile_file(
            &profiles_dir,
            "child.toml",
            "name = \"child\"\nextends = \"base\"\n",
        );
        write_profile_file(&profiles_dir, "ok.toml", "name = \"ok\"\nmodel = \"m\"\n");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "ok");
        let child = result
            .skipped
            .iter()
            .find(|skipped| skipped.path.ends_with("child.toml"))
            .expect("the child skips");
        assert!(
            child.reason.contains("'base'"),
            "should name the skipped parent: {}",
            child.reason
        );
        assert!(
            child.reason.contains("was skipped"),
            "should chain the skip: {}",
            child.reason
        );
        assert!(
            child.reason.contains("unknown profile 'missing'"),
            "should include the underlying reason: {}",
            child.reason
        );
    }

    /// should keep skip diagnostics flowing on the manager for the R3 wire
    #[test]
    fn should_keep_skip_diagnostics_available_on_the_manager() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(&profiles_dir, "legacy.yaml", "name: legacy\n");
        write_profile_file(&profiles_dir, "ok.toml", "name = \"ok\"\nmodel = \"m\"\n");

        let manager = ProfileManager::new();
        assert!(manager.last_skipped_files().is_empty());
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(loaded, 1);
        assert!(manager.get("ok").is_some());
        let skipped = manager.last_skipped_files();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].path.ends_with("legacy.yaml"));
        assert!(skipped[0].reason.contains("unsupported profile extension"));
    }

    /// should load a .md profile through the roots chain on the manager
    #[test]
    fn should_load_md_profiles_through_the_roots_chain_on_the_manager() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&agents_dir).expect("create_dir should succeed");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(&agents_dir, "reviewer.md", "reviewer", "You review code.");

        let roots = [agents_dir, profiles_dir];
        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_roots(&roots, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(loaded, 1);
        let reviewer = manager.get("reviewer").expect("the .md profile loads");
        assert_eq!(reviewer.system_prompt.as_deref(), Some("You review code."));
    }

    /// should expose the TS-shaped user root candidates and the legacy root
    #[test]
    fn should_expose_the_user_root_candidates_and_the_legacy_root() {
        let mirri_home = PathBuf::from("/home/tester/.mirri-code");
        let os_home = PathBuf::from("/home/tester");
        assert_eq!(
            user_profile_root_candidates(&mirri_home, Some(&os_home)),
            vec![
                mirri_home.join("agents"),
                os_home.join(".agents").join("agents"),
            ]
        );
        // No OS home resolvable: only the brand candidate.
        assert_eq!(
            user_profile_root_candidates(&mirri_home, None),
            vec![mirri_home.join("agents")]
        );
        assert_eq!(
            legacy_profile_root(&mirri_home),
            mirri_home.join("profiles")
        );
    }

    /// Given an existing `.md` with unknown keys, comments and a custom
    /// body, When a same-name profile is upserted, Then the file is
    /// surgically rewritten — only the modified keys' lines change — and no
    /// `.toml` is written (the R4-1 write channel; the R2 transition caveat
    /// is retired).
    #[test]
    fn should_surgically_rewrite_the_md_when_upsert_targets_a_name_with_an_existing_md() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        fs::create_dir(&agents_dir).expect("create_dir should succeed");
        write_profile_file(
            &agents_dir,
            "coder.md",
            "---\nname: coder\ndescription: Hand-written.\n# curated by hand\nunknown_key: keep me\nenabled: false\ntools:\n  - Read\n---\nThe .md body wins and survives.",
        );
        let manager = ProfileManager::new();

        // When the coder profile is upserted with a new model
        manager
            .upsert_profile(
                AgentProfile {
                    name: "coder".to_string(),
                    model: "updated-model".to_string(),
                    description: Some("Hand-written.".to_string()),
                    system_prompt: Some("The .md body wins and survives.".to_string()),
                    enabled: Some(false),
                    tools: Some(vec!["Read".to_string()]),
                    ..Default::default()
                },
                &agents_dir,
                &temp_dir.path().join("profiles"),
                &LocalSyncFs,
            )
            .expect("upsert should succeed");

        // Then no legacy .toml was written and the .md was rewritten in
        // place: the model line was inserted, and every untouched byte —
        // the comment, the unknown key, the disabled flag, the tools list,
        // the custom body — is preserved.
        assert!(!agents_dir.join("coder.toml").exists());
        let rewritten =
            fs::read_to_string(agents_dir.join("coder.md")).expect("the .md is readable");
        let expected = "---\nname: coder\ndescription: Hand-written.\n# curated by hand\nunknown_key: keep me\nenabled: false\ntools:\n  - Read\ndefaultModel: updated-model\n---\nThe .md body wins and survives.";
        assert_eq!(rewritten, expected);

        // And because the file carries `enabled: false`, the next discovery
        // pass does not register the disabled candidate (it never wins)
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("discovery should succeed");
        assert!(result.profiles.is_empty());
        assert!(result.skipped.is_empty());

        // And when the profile is re-upserted enabled (the flag removed),
        // the file loses only the `enabled` line and now parses back with
        // the updated model
        manager
            .upsert_profile(
                AgentProfile {
                    name: "coder".to_string(),
                    model: "updated-model".to_string(),
                    description: Some("Hand-written.".to_string()),
                    system_prompt: Some("The .md body wins and survives.".to_string()),
                    tools: Some(vec!["Read".to_string()]),
                    ..Default::default()
                },
                &agents_dir,
                &temp_dir.path().join("profiles"),
                &LocalSyncFs,
            )
            .expect("re-upsert should succeed");
        let enabled = fs::read_to_string(agents_dir.join("coder.md")).expect("the .md is readable");
        assert_eq!(
            enabled,
            "---\nname: coder\ndescription: Hand-written.\n# curated by hand\nunknown_key: keep me\ntools:\n  - Read\ndefaultModel: updated-model\n---\nThe .md body wins and survives."
        );
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.model, "updated-model");
        assert!(result.skipped.is_empty());
    }

    /// Given a legacy `<profiles>/<name>.{toml,json}` pair, When a profile
    /// is upserted (creating the `.md`), Then the legacy files are removed
    /// — the TS `removeLegacyFile` mirror.
    #[test]
    fn should_remove_legacy_files_when_an_upsert_creates_the_md() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        fs::create_dir(&legacy_dir).expect("create_dir should succeed");
        write_profile_file(&legacy_dir, "reviewer.toml", "name = \"reviewer\"\n");
        write_profile_file(&legacy_dir, "reviewer.json", "{\"name\": \"reviewer\"}");
        let manager = ProfileManager::new();

        manager
            .upsert_profile(
                AgentProfile {
                    name: "reviewer".to_string(),
                    description: Some("Reviews.".to_string()),
                    system_prompt: Some("You review.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");

        assert!(agents_dir.join("reviewer.md").is_file());
        assert!(!legacy_dir.join("reviewer.toml").exists());
        assert!(!legacy_dir.join("reviewer.json").exists());
    }

    /// Given an existing `.md` whose values already match the upsert except
    /// one key — including a value stored under its snake_case alias and a
    /// comma-separated list spelling — When the profile is upserted, Then
    /// only the changed key's line is rewritten: the alias spelling, the
    /// inline list and the custom quote style of every unchanged key
    /// survive verbatim (T23-m1, the rewrite-narrowing acceptance core).
    #[test]
    fn should_rewrite_only_changed_keys_when_an_upsert_matches_the_existing_values() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        fs::create_dir(&agents_dir).expect("create_dir should succeed");
        write_profile_file(
            &agents_dir,
            "reviewer.md",
            "---\n\
             name: reviewer\n\
             description: 'Old, with comma'\n\
             whenToUse: later\n\
             default_model: claude-sonnet-4-20250514\n\
             tools: Read, Grep\n\
             ---\n\
             You review.",
        );
        let manager = ProfileManager::new();

        // When the profile is upserted with only the description changed
        // (the same model, the same tools)
        manager
            .upsert_profile(
                AgentProfile {
                    name: "reviewer".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    description: Some("Reviews, carefully.".to_string()),
                    when_to_use: Some("later".to_string()),
                    system_prompt: Some("You review.".to_string()),
                    tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
                    ..Default::default()
                },
                &agents_dir,
                &temp_dir.path().join("profiles"),
                &LocalSyncFs,
            )
            .expect("upsert should succeed");

        // Then only the description line changed — the quoted description
        // spelling became the canonical dump of the *new* value, while the
        // snake_case model alias and the inline tools list survived as-is
        let rewritten =
            fs::read_to_string(agents_dir.join("reviewer.md")).expect("the .md is readable");
        assert_eq!(
            rewritten,
            "---\n\
             name: reviewer\n\
             description: Reviews, carefully.\n\
             whenToUse: later\n\
             default_model: claude-sonnet-4-20250514\n\
             tools: Read, Grep\n\
             ---\n\
             You review."
        );

        // And the file still parses back to the upserted definition (the
        // alias and the inline list resolve to the same values)
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(
            result.profiles[0].profile.description.as_deref(),
            Some("Reviews, carefully.")
        );
        assert!(result.skipped.is_empty());
    }

    /// Given a `.md` that does not parse (a missing description — the file
    /// can never load) alongside a same-name legacy `.toml` that does, When
    /// the profile is upserted, Then the corrupt file is replaced wholesale
    /// with a valid serialization instead of being rewritten in place —
    /// otherwise the legacy cleanup would leave nothing loadable and the
    /// profile would vanish on the next restart (T23-m2).
    #[test]
    fn should_reserialize_a_corrupt_md_instead_of_rewriting_it_in_place() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        fs::create_dir(&agents_dir).expect("create agents dir should succeed");
        fs::create_dir(&legacy_dir).expect("create legacy dir should succeed");
        write_profile_file(&agents_dir, "scout.md", "---\nname: scout\n---\nYou scout.");
        write_profile_file(
            &legacy_dir,
            "scout.toml",
            "name = \"scout\"\nmodel = \"from-legacy\"\n",
        );
        let manager = ProfileManager::new();

        // When the profile is upserted over the corrupt .md
        manager
            .upsert_profile(
                AgentProfile {
                    name: "scout".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    description: Some("Scouts ahead.".to_string()),
                    system_prompt: Some("You scout ahead.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");

        // Then the .md was replaced wholesale with a parseable file (the
        // corrupt frontmatter — no description — is gone, not preserved)
        let rewritten =
            fs::read_to_string(agents_dir.join("scout.md")).expect("the .md is readable");
        let reparsed = parse_agent_file_text(
            &agents_dir.join("scout.md").display().to_string(),
            &rewritten,
        )
        .expect("the rewritten .md parses");
        assert_eq!(reparsed.description, "Scouts ahead.");
        assert_eq!(reparsed.prompt, "You scout ahead.");

        // And the legacy file was cleaned (the .md exists) — the restart
        // load still finds the profile instead of silently losing it
        assert!(!legacy_dir.join("scout.toml").exists());
        let restarted = ProfileManager::new();
        restarted
            .load_from_roots(&[agents_dir], &LocalSyncFs)
            .expect("roots load should succeed");
        assert!(restarted.get("scout").is_some());
    }

    // ===== legacy extends flattening + unified .md model (R2-2) =====

    /// Write one profile file into `dir` and return its path.
    fn write_profile_file(dir: &Path, file_name: &str, contents: &str) -> PathBuf {
        let path = dir.join(file_name);
        fs::write(&path, contents).expect("write should succeed");
        path
    }

    /// should flatten the child's overrides and the parent's defaults into
    /// a self-contained profile when a directory declares legacy extends
    #[test]
    fn should_flatten_overrides_and_inherited_defaults_when_a_dir_declares_extends() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        // The base uses the legacy `enabled_tools` key on purpose: the
        // serde-alias read must survive the flattening path too.
        write_profile_file(
            &profiles_dir,
            "base.toml",
            r#"
name = "base"
model = "claude-sonnet-4-20250514"
provider = "anthropic"
system_prompt = "Base prompt."
max_turns = 100
permission_mode = "yolo"
enabled_tools = ["Read", "Grep"]
disabled_tools = ["Bash"]
thinking_effort = "high"

[prompt_vars]
lang = "rust"
style = "clean"
"#,
        );
        write_profile_file(
            &profiles_dir,
            "child.toml",
            r#"
name = "child"
extends = "base"
system_prompt = "Child prompt."

[prompt_vars]
lang = "typescript"
"#,
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("flatten should succeed");
        let child = result
            .profiles
            .iter()
            .find(|discovered| discovered.profile.name == "child")
            .expect("child is loaded");

        // Overridden fields come from the child...
        assert_eq!(
            child.profile.system_prompt.as_deref(),
            Some("Child prompt.")
        );
        // ...the omitted (empty) model inherits the parent's — the old
        // empty-string rule — and the parent's fields are inherited as-is
        // (including the base's legacy `enabled_tools` key spelling).
        assert_eq!(child.profile.model, "claude-sonnet-4-20250514");
        assert_eq!(child.profile.provider.as_deref(), Some("anthropic"));
        assert_eq!(child.profile.max_turns, Some(100));
        assert_eq!(child.profile.permission_mode, Some(PermissionMode::Yolo));
        assert_eq!(
            child.profile.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(
            child.profile.disallowed_tools,
            Some(vec!["Bash".to_string()])
        );
        assert_eq!(child.profile.thinking_effort, Some(ThinkingEffort::High));
        // `subagents` are never inherited (the old G-23 rule)...
        assert_eq!(child.profile.subagents, None);
        // ...and `prompt_vars` deep-merge with the child winning per-key.
        assert_eq!(
            child.profile.prompt_vars,
            Some(HashMap::from([
                ("lang".to_string(), "typescript".to_string()),
                ("style".to_string(), "clean".to_string()),
            ]))
        );

        // The flattened profile is self-contained: it resolves with no
        // merge step and the inherited allowlist reaches the scope config.
        let manager = ProfileManager::new();
        manager.register(child.profile.clone()).unwrap();
        let config = manager
            .resolve("child", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "claude-sonnet-4-20250514");
        assert_eq!(
            config.enabled_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
    }

    /// should flatten each level's overrides while inheriting down a
    /// multi-level extends chain
    #[test]
    fn should_flatten_each_level_overriding_or_inheriting_in_a_multi_level_chain() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "root.toml",
            r#"
name = "root"
model = "claude-sonnet-4-20250514"
provider = "anthropic"
system_prompt = "Root prompt."

[[subagents]]
name = "scout"
"#,
        );
        write_profile_file(
            &profiles_dir,
            "middle.toml",
            r#"
name = "middle"
extends = "root"
max_turns = 50
"#,
        );
        write_profile_file(
            &profiles_dir,
            "leaf.toml",
            r#"
name = "leaf"
extends = "middle"
model = "gpt-4o"
tools = ["Edit"]
"#,
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("flatten should succeed");
        let by_name = |name: &str| {
            result
                .profiles
                .iter()
                .find(|discovered| discovered.profile.name == name)
                .unwrap_or_else(|| panic!("{name} is loaded"))
        };

        // The middle level's empty model inherits root's (the old
        // empty-string rule through a chain).
        let middle = by_name("middle");
        assert_eq!(middle.profile.model, "claude-sonnet-4-20250514");
        assert_eq!(middle.profile.max_turns, Some(50));

        // The leaf overrides its own model, keeps middle's max_turns, and
        // inherits root's provider/prompt — with root's subagents NOT
        // inherited (G-23).
        let leaf = by_name("leaf");
        assert_eq!(leaf.profile.model, "gpt-4o");
        assert_eq!(leaf.profile.max_turns, Some(50));
        assert_eq!(leaf.profile.provider.as_deref(), Some("anthropic"));
        assert_eq!(leaf.profile.system_prompt.as_deref(), Some("Root prompt."));
        assert_eq!(leaf.profile.tools, Some(vec!["Edit".to_string()]));
        assert_eq!(leaf.profile.subagents, None);
    }

    /// should flatten extends referencing a built-in profile when the
    /// parent is not in the loaded set
    #[test]
    fn should_flatten_extends_referencing_a_builtin_profile_when_the_parent_is_not_in_the_set() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "my-coder.toml",
            r#"
name = "my-coder"
extends = "coder"
description = "My coder."
"#,
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("flatten should succeed");
        let my_coder = &result.profiles[0];
        // The built-in coder's prompt is flattened in (every manager
        // registers the built-ins, so a legacy `extends = "coder"` keeps
        // resolving). No model is flattened: the built-ins ship no model
        // (spec 11 R6/A7), so the child keeps the unset state.
        assert!(my_coder.profile.model.is_empty());
        assert!(
            my_coder.profile.system_prompt.is_none(),
            "the builtin parent rides the base template, so the child inherits no inline prompt"
        );
        assert!(
            my_coder
                .profile
                .prompt_vars
                .as_ref()
                .and_then(|vars| vars.get("role_additional"))
                .is_some_and(|role| role.contains("Your final message is the entire handoff")),
            "the builtin parent's role text flattens into the child's prompt vars"
        );
        assert_eq!(my_coder.profile.description.as_deref(), Some("My coder."));
    }

    /// should keep the child's own subagents and inherit the parent's
    /// system_prompt_file when flattening
    #[test]
    fn should_keep_own_subagents_and_inherit_system_prompt_file_when_flattening() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let prompt_path = temp_dir.path().join("base_prompt.md");
        fs::write(&prompt_path, "Base file prompt.").expect("write should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "base.toml",
            &format!(
                r#"
name = "base"
model = "claude-sonnet-4-20250514"
system_prompt_file = "{}"
"#,
                prompt_path.display()
            ),
        );
        write_profile_file(
            &profiles_dir,
            "child.toml",
            r#"
name = "child"
extends = "base"

[[subagents]]
name = "scout"
"#,
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("flatten should succeed");
        let child = &result.profiles[1];
        assert_eq!(child.profile.system_prompt_file, Some(prompt_path));
        // The child's own legacy `[[subagents]]` declaration migrated to a
        // name allowlist entry (and was not inherited away).
        assert_eq!(child.profile.subagents, Some(vec!["scout".to_string()]));

        // And the inherited prompt file is read at resolve time.
        let manager = ProfileManager::new();
        for discovered in result.profiles {
            manager.register(discovered.profile).unwrap();
        }
        let config = manager
            .resolve("child", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.system_prompt.as_deref(), Some("Base file prompt."));
    }

    /// should skip only the cycle members and load the rest when a
    /// flattened extends chain forms a cycle
    #[test]
    fn should_skip_only_the_cycle_members_when_a_flattened_extends_chain_forms_a_cycle() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "a.toml",
            "name = \"a\"\nmodel = \"m\"\nextends = \"b\"\n",
        );
        write_profile_file(
            &profiles_dir,
            "b.toml",
            "name = \"b\"\nmodel = \"m\"\nextends = \"a\"\n",
        );
        write_profile_file(&profiles_dir, "ok.toml", "name = \"ok\"\nmodel = \"m\"\n");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        // The untouched profile still loads; only the cycle members skip.
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "ok");
        assert_eq!(result.skipped.len(), 2, "both cycle members skip");
        for skipped in &result.skipped {
            assert!(
                skipped.reason.contains("cycle"),
                "should mention cycle: {}",
                skipped.reason
            );
        }
    }

    /// should skip only the orphan and load the rest when flattening an
    /// extends that references an unknown profile
    #[test]
    fn should_skip_only_the_orphan_when_flattening_extends_referencing_an_unknown_profile() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "orphan.toml",
            "name = \"orphan\"\nextends = \"nonexistent\"\n",
        );
        write_profile_file(&profiles_dir, "ok.toml", "name = \"ok\"\nmodel = \"m\"\n");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "ok");
        let skipped = &result.skipped[0];
        assert_eq!(skipped.path, profiles_dir.join("orphan.toml"));
        assert!(
            skipped.reason.contains("nonexistent"),
            "should mention the missing parent: {}",
            skipped.reason
        );
    }

    /// should skip only the offender and load the rest when a flattened
    /// extends is the empty string
    #[test]
    fn should_skip_only_the_offender_when_a_flattened_extends_is_empty() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "bad.toml",
            "name = \"bad\"\nextends = \"\"\n",
        );
        write_profile_file(&profiles_dir, "ok.toml", "name = \"ok\"\nmodel = \"m\"\n");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "ok");
        let skipped = &result.skipped[0];
        assert_eq!(skipped.path, profiles_dir.join("bad.toml"));
        assert!(
            skipped.reason.contains("extends = \"\""),
            "should mention the empty extends: {}",
            skipped.reason
        );
    }

    /// A profile without a model no longer errors at resolve time — the
    /// rejection moved to the request-time resolution chain (spec 11 R6;
    /// see [`should_accept_profile_without_model`]). The historical
    /// "error when no model" behavior was removed with the built-in model
    /// zeroing (spec 11 R4/ADR-001).
    #[test]
    fn should_resolve_profile_without_model_to_empty_model_id() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "nomodel".to_string(),
                model: String::new(),
                ..Default::default()
            })
            .unwrap();

        let config = manager.resolve("nomodel", &LocalSyncFs).unwrap();
        assert!(
            config.model_id.is_empty(),
            "the missing model passes through as the unset state"
        );
    }

    /// should load a unified .md agent file into the engine model
    #[test]
    fn should_load_a_unified_md_agent_file_into_the_engine_model() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = temp_dir.path().join("reviewer.md");
        fs::write(
            &path,
            "---\n\
             name: reviewer\n\
             description: Reviews code\n\
             whenToUse: When a change needs review\n\
             defaultModel: claude-sonnet-4-20250514\n\
             tools:\n  - Read\n  - Grep\n\
             disallowedTools:\n  - Bash\n\
             capabilitiesRequired:\n  - code.read\n\
             enabled: false\n\
             ---\n\
             You review code.",
        )
        .expect("write should succeed");

        let manager = ProfileManager::new();
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("load should succeed");
        assert_eq!(name, "reviewer");

        let profile = manager.get("reviewer").expect("profile should exist");
        // The Markdown body is the inline system prompt; the frontmatter
        // fields map onto the renamed model fields.
        assert_eq!(profile.system_prompt.as_deref(), Some("You review code."));
        assert_eq!(profile.description.as_deref(), Some("Reviews code"));
        assert_eq!(
            profile.when_to_use.as_deref(),
            Some("When a change needs review")
        );
        assert_eq!(profile.model, "claude-sonnet-4-20250514");
        assert_eq!(
            profile.default_model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(
            profile.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(profile.disallowed_tools, Some(vec!["Bash".to_string()]));
        assert_eq!(
            profile.capabilities_required,
            Some(vec!["code.read".to_string()])
        );
        assert_eq!(profile.enabled, Some(false));

        // And the R3-1 engine consumption: the file-disabled profile is
        // refused by the regular engine path (dispatch face).
        let error = manager
            .resolve("reviewer", &LocalSyncFs)
            .expect_err("a file-disabled profile must not resolve");
        match error {
            EngineError::RequestInvalid(message) => assert_eq!(
                message,
                "Agent type \"reviewer\" is disabled. Re-enable it in the agent \
                 profiles configuration to dispatch it."
            ),
            other => panic!("expected RequestInvalid, got: {other}"),
        }
    }

    /// should map the unified subagents name list through the AgentFileDef
    /// adapter
    #[test]
    fn should_map_subagents_name_list_when_adapting_an_agent_file_def() {
        let def = AgentFileDef {
            name: "scout".to_string(),
            description: "Scouts code.".to_string(),
            prompt: "You scout.".to_string(),
            subagents: Some(vec!["coder".to_string()]),
            file_path: Some("/agents/scout.md".to_string()),
            source: Some(AgentFileSource::User),
            ..Default::default()
        };

        let profile = AgentProfile::from(def);
        assert_eq!(profile.name, "scout");
        assert_eq!(profile.system_prompt.as_deref(), Some("You scout."));
        assert_eq!(profile.description.as_deref(), Some("Scouts code."));
        assert_eq!(profile.subagents, Some(vec!["coder".to_string()]));
    }

    // ===== disabled agents retirement (R3-3) =====

    /// should list and dispatch a profile named in the retired config key
    /// when it carries no file-level flag (the key is semantically inert)
    #[test]
    fn should_list_and_dispatch_a_profile_named_in_the_retired_config_key() {
        // Given a config still naming `coder` and `explore` in the retired
        // `disabled_agents` key, and a manager whose profiles carry no
        // file-level `enabled = false` flag
        let config = ServerConfig {
            disabled_agents: Some(vec!["coder".to_string(), "explore".to_string()]),
            ..Default::default()
        };
        let manager = ProfileManager::new();

        // Then the key is inert: the named profiles stay listed (the
        // dispatch face no longer filters on it)...
        let names = manager.list();
        assert!(names.contains(&"coder".to_string()));
        assert!(names.contains(&"explore".to_string()));
        assert!(names.contains(&"agent".to_string()));
        assert!(names.contains(&"plan".to_string()));
        // ...and both resolve paths dispatch them normally
        assert!(manager.resolve("coder", &LocalSyncFs).is_ok());
        assert!(manager.resolve_profile("explore").is_ok());
        assert!(manager.resolve("agent", &LocalSyncFs).is_ok());

        // And the deprecation notice fires for exactly this config,
        // pointing the user at the file-level flag
        let notice = config
            .disabled_agents_deprecation_notice()
            .expect("a non-empty disabled_agents key must produce the notice");
        assert!(notice.contains("disabled_agents"), "{notice}");
        assert!(notice.contains("enabled"), "{notice}");
        assert!(notice.contains("coder"), "{notice}");
    }

    /// should keep a config-named unknown profile unknown (the key does not
    /// resurrect or invent profiles either)
    #[test]
    fn should_keep_a_config_named_unregistered_profile_unknown() {
        // Given a config naming profiles that are not registered
        let config = ServerConfig {
            disabled_agents: Some(vec!["architect".to_string(), "rag-pipeline".to_string()]),
            ..Default::default()
        };
        let manager = ProfileManager::new();

        // Then the key neither disables nor registers anything: the names
        // stay unknown, while the real profiles resolve normally
        assert!(manager.resolve("architect", &LocalSyncFs).is_err());
        assert!(manager.resolve("rag-pipeline", &LocalSyncFs).is_err());
        assert!(manager.resolve("agent", &LocalSyncFs).is_ok());
        assert!(manager.list().contains(&"plan".to_string()));
        assert!(config.disabled_agents_deprecation_notice().is_some());
    }

    /// should produce no deprecation notice when the key is absent or empty
    #[test]
    fn should_skip_the_deprecation_notice_when_disabled_agents_is_absent_or_empty() {
        // Given configs without the key, or with an empty one
        // Then no notice is produced (nothing was retired from them)
        assert!(
            ServerConfig::default()
                .disabled_agents_deprecation_notice()
                .is_none()
        );
        assert!(
            ServerConfig {
                disabled_agents: Some(vec![]),
                ..Default::default()
            }
            .disabled_agents_deprecation_notice()
            .is_none()
        );
    }

    // ===== file-level enabled flag (R3-1) =====

    /// should split the dispatch listing from the management listing when
    /// a profile file declares `enabled = false`
    #[test]
    fn should_split_dispatch_from_management_listing_when_a_profile_declares_itself_disabled() {
        // Given a registered profile whose file declared `enabled = false`
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "ghost".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..Default::default()
            })
            .unwrap();

        // Then the dispatch face (list — the host's candidate listing and
        // the Agent/swarm tool descriptions) hides it
        assert!(!manager.list().contains(&"ghost".to_string()));
        // And the management face (list_all — the wire lists-but-marks)
        // keeps it, so the row can still carry the flag
        assert!(manager.list_all().contains(&"ghost".to_string()));
    }

    /// should report the TS-aligned disabled message when resolving a
    /// file-disabled profile (both resolve paths)
    #[test]
    fn should_report_the_unified_disabled_message_when_resolving_a_file_disabled_profile() {
        // Given a registered profile whose file declared `enabled = false`
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "ghost".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..Default::default()
            })
            .unwrap();

        // Then both resolve paths reject it with the exact TS text
        // (`requireSubagentProfile`, profile-shared.ts)
        let expected = "Agent type \"ghost\" is disabled. Re-enable it in the agent profiles \
             configuration to dispatch it.";
        for error in [
            manager
                .resolve("ghost", &LocalSyncFs)
                .expect_err("resolve should fail"),
            manager
                .resolve_profile("ghost")
                .expect_err("resolve_profile should fail"),
        ] {
            match error {
                EngineError::RequestInvalid(message) => assert_eq!(message, expected),
                other => panic!("expected RequestInvalid, got: {other}"),
            }
        }
    }

    /// should report the TS-aligned unknown message listing only the
    /// dispatchable types when resolving an unknown profile
    #[test]
    fn should_report_available_dispatchable_types_when_resolving_an_unknown_profile() {
        // Given a manager with one file-disabled profile among the built-ins
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "ghost".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..Default::default()
            })
            .unwrap();

        // When an unknown name is resolved, the error names it and the
        // dispatchable set — the disabled profile is NOT offered
        // (", "-joined, the TS `catalog.list().map(p=>p.name).join(', ')`)
        let error = manager
            .resolve("missing", &LocalSyncFs)
            .expect_err("resolve should fail");
        let expected =
            "Unknown agent type: \"missing\". Available types: agent, coder, explore, plan";
        match error {
            EngineError::RequestInvalid(message) => assert_eq!(message, expected),
            other => panic!("expected RequestInvalid, got: {other}"),
        }
    }

    /// should keep a profile dispatchable when only the retired config key
    /// names it (config-only disablement no longer errors — R3-3)
    #[test]
    fn should_keep_a_profile_dispatchable_when_only_the_retired_config_key_names_it() {
        // Given a profile named via the host config only (no file flag)
        let config = ServerConfig {
            disabled_agents: Some(vec!["coder".to_string()]),
            ..Default::default()
        };
        let manager = ProfileManager::new();

        // Then both resolve paths dispatch it normally — the config track
        // no longer gates anything — and the deprecation notice points at
        // the file-level flag that replaces it
        assert!(manager.resolve("coder", &LocalSyncFs).is_ok());
        assert!(manager.resolve_profile("coder").is_ok());
        assert!(manager.list().contains(&"coder".to_string()));
        let notice = config
            .disabled_agents_deprecation_notice()
            .expect("the retired key must still produce its deprecation notice");
        assert!(notice.contains("coder"), "{notice}");
    }

    /// should let the file flag alone disable a profile that the retired
    /// config key also names (the config track no longer gates dispatch)
    #[test]
    fn should_disable_a_profile_by_its_file_flag_even_when_the_retired_config_key_names_it() {
        // Given a profile disabled by its file flag while the retired
        // config key also names it
        let config = ServerConfig {
            disabled_agents: Some(vec!["ghost".to_string()]),
            ..Default::default()
        };
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "ghost".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..Default::default()
            })
            .unwrap();

        // Then the file flag is the sole enablement source: the config
        // key's presence neither adds nor removes disablement, and the
        // error is the TS-aligned file-flag text
        let error = manager
            .resolve_profile("ghost")
            .expect_err("resolve_profile should fail");
        match error {
            EngineError::RequestInvalid(message) => {
                assert_eq!(
                    message,
                    "Agent type \"ghost\" is disabled. Re-enable it in the agent \
                     profiles configuration to dispatch it."
                );
            }
            other => panic!("expected RequestInvalid, got: {other}"),
        }

        // And the config still carries its deprecation notice (inert data
        // with a warning, not an error)
        assert!(config.disabled_agents_deprecation_notice().is_some());
    }

    // ===== built-in profiles (G-25) =====

    /// should register the built-in profiles at initialization
    #[test]
    fn should_register_builtin_profiles_on_init() {
        let manager = ProfileManager::new();
        assert_eq!(manager.list(), builtin_profile_names());

        for name in builtin_profile_names() {
            assert!(
                manager.get(&name).is_some(),
                "built-in '{name}' should be registered"
            );
        }
    }

    /// should allow a user profile with the same name to override a built-in
    #[test]
    fn should_allow_custom_profile_to_override_builtin() {
        let manager = ProfileManager::new();
        manager
            .register(AgentProfile {
                name: "coder".to_string(),
                model: "gpt-4o".to_string(),
                system_prompt: Some("Custom coder.".to_string()),
                ..Default::default()
            })
            .unwrap();

        let config = manager
            .resolve("coder", &LocalSyncFs)
            .expect("resolve should succeed");
        assert_eq!(config.model_id, "gpt-4o");
        assert_eq!(config.system_prompt.as_deref(), Some("Custom coder."));
    }

    // ===== essential profile locking (R3-2, relaxed by D16) =====

    /// should load a user file declaring the essential name as the content
    /// override of the built-in (D16 — isomorphic with any covered builtin)
    #[test]
    fn should_load_a_user_file_declaring_the_essential_name_as_an_override() {
        // Given a directory whose file declares the essential name (with a
        // model, so the dispatch face can resolve through the override)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        fs::write(
            profiles_dir.join("agent.md"),
            "---\nname: agent\ndescription: agent agent\ndefaultModel: gpt-4o\n---\n\
             You are a customized default agent.",
        )
        .expect("agent .md must be writable");
        let manager = ProfileManager::new();

        // When the roots load, the pass succeeds and registers the override
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the load succeeds");
        assert_eq!(loaded, 1, "the essential candidate loads as an override");

        // Then the registered content IS the user file's (prompt and file)
        let essential = manager
            .get(ESSENTIAL_PROFILE_NAME)
            .expect("the essential name stays registered");
        assert_eq!(
            essential.system_prompt.as_deref(),
            Some("You are a customized default agent.")
        );
        assert_eq!(
            manager.provenance(ESSENTIAL_PROFILE_NAME),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(profiles_dir.join("agent.md")),
            })
        );

        // And the displaced built-in definition is the row's suppressed
        // candidate ({source: builtin, priority: 0, reason: priority} —
        // isomorphic with every covered builtin)
        assert_eq!(
            manager.suppressed_candidates(ESSENTIAL_PROFILE_NAME),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            }]
        );

        // And the skip channel carries nothing (D13: load failures only —
        // the legal file that won the merge is never reported as skipped)
        assert!(manager.last_skipped_files().is_empty());

        // And the engine still dispatches the (overridden) essential
        assert!(manager.list().contains(&ESSENTIAL_PROFILE_NAME.to_string()));
        assert!(
            manager
                .resolve(ESSENTIAL_PROFILE_NAME, &LocalSyncFs)
                .is_ok()
        );
    }

    /// should let the first essential-named candidate across roots win and
    /// suppress the later ones plus the displaced built-in
    #[test]
    fn should_keep_the_first_essential_candidate_across_roots() {
        // Given two roots each declaring the essential name in a different
        // format
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let first = temp_dir.path().join("first");
        let second = temp_dir.path().join("second");
        fs::create_dir(&first).expect("create_dir should succeed");
        fs::create_dir(&second).expect("create_dir should succeed");
        write_md_file(&first, "agent.md", "agent", "First take.");
        write_profile_file(
            &second,
            "agent.toml",
            "name = \"agent\"\nmodel = \"gpt-4o\"\n",
        );
        let manager = ProfileManager::new();

        // When the roots load
        let loaded = manager
            .load_from_roots(&[first.clone(), second.clone()], &LocalSyncFs)
            .expect("the load succeeds");

        // Then the earlier root's candidate wins (the priority order any
        // other name follows), the later one is suppressed as `priority`,
        // and the displaced built-in definition is suppressed last
        assert_eq!(loaded, 1);
        assert_eq!(
            manager
                .provenance(ESSENTIAL_PROFILE_NAME)
                .map(|p| p.file_path),
            Some(Some(first.join("agent.md")))
        );
        assert_eq!(
            manager.suppressed_candidates(ESSENTIAL_PROFILE_NAME),
            vec![
                SuppressedProfileCandidate {
                    source: ProfileSource::User,
                    reason: SuppressedReason::Priority,
                },
                SuppressedProfileCandidate {
                    source: ProfileSource::Builtin,
                    reason: SuppressedReason::Priority,
                },
            ]
        );
        // And the skip channel stays empty (D13)
        assert!(manager.last_skipped_files().is_empty());
    }

    /// Given the essential built-in, When a same-name profile is upserted,
    /// Then the write is **allowed** (D16: content may be overridden) — the
    /// `.md` file is written and the merged content registers as the
    /// override (isomorphic with upserting any other builtin); `delete`
    /// still refuses the name.
    #[test]
    fn should_register_the_override_content_when_the_essential_is_upserted() {
        // Given a manager (the essential built-in is always registered)
        let manager = ProfileManager::new();
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");

        // When a direct registration declares the essential name (D16: the
        // content channels are open; only delete/disable stay locked)
        manager
            .register(sample_profile(ESSENTIAL_PROFILE_NAME))
            .expect("registering the essential name replaces its content");

        // Then the registration replaced the definition in place
        assert_eq!(
            manager.get(ESSENTIAL_PROFILE_NAME).unwrap().model,
            sample_profile(ESSENTIAL_PROFILE_NAME).model
        );

        // When an upsert targets the essential name (the write channel)
        manager
            .upsert_profile(
                sample_profile(ESSENTIAL_PROFILE_NAME),
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upserting the essential profile writes its override file");

        // Then the same-name .md is written and the merged content is the
        // registered definition with user provenance…
        assert!(agents_dir.join("agent.md").is_file());
        assert_eq!(
            manager.provenance(ESSENTIAL_PROFILE_NAME),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(agents_dir.join("agent.md")),
            })
        );
        // …and the suppressed projection is reset (the next discovery pass
        // re-derives it, carrying the displaced built-in)
        assert!(
            manager
                .suppressed_candidates(ESSENTIAL_PROFILE_NAME)
                .is_empty()
        );

        // When a delete targets the essential name
        let error = manager
            .delete_profile(
                ESSENTIAL_PROFILE_NAME,
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect_err("delete must refuse the essential name");
        match error {
            EngineError::RequestInvalid(message) => {
                assert_eq!(message, "Cannot delete the essential profile \"agent\"")
            }
            other => panic!("expected RequestInvalid, got: {other}"),
        }
        assert!(manager.get(ESSENTIAL_PROFILE_NAME).is_some());
        assert!(agents_dir.join("agent.md").is_file());
    }

    /// should load an explicit single-file declaration of the essential
    /// name as the content override (D16 — same as the directory loads)
    #[test]
    fn should_load_a_single_file_declaring_the_essential_name() {
        // Given a file declaring the essential name
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let path = write_profile_file(
            temp_dir.path(),
            "agent.toml",
            "name = \"agent\"\nmodel = \"gpt-4o\"\n",
        );
        let manager = ProfileManager::new();

        // When the file is loaded explicitly
        let name = manager
            .load_from_file(&path, &LocalSyncFs)
            .expect("the essential-name load registers the override");

        // Then the registration is the file's content with its provenance
        assert_eq!(name, ESSENTIAL_PROFILE_NAME);
        assert_eq!(manager.get(ESSENTIAL_PROFILE_NAME).unwrap().model, "gpt-4o");
        assert_eq!(
            manager.provenance(ESSENTIAL_PROFILE_NAME),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(path),
            })
        );
    }

    /// should treat an essential profile carrying enabled = false as
    /// enabled (the TS `:192-200` exemption — dispatch can never be bricked)
    #[test]
    fn should_treat_an_essential_profile_with_enabled_false_as_enabled() {
        // Given a manager whose essential profile carries the file-level
        // disable flag. A user `agents/agent.md` with `enabled: false`
        // produces exactly this state post-D16 (discovery exempts the
        // essential from the disabled branch); the built-in seeding path
        // is used here to pin the invariant independently of discovery.
        let manager = ProfileManager::new();
        {
            let mut guard = manager
                .profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .get_mut(ESSENTIAL_PROFILE_NAME)
                .expect("the essential built-in is registered")
                .enabled = Some(false);
        }

        // Then the dispatch listing keeps it and both resolve paths succeed
        assert!(manager.list().contains(&"agent".to_string()));
        assert!(
            manager
                .resolve(ESSENTIAL_PROFILE_NAME, &LocalSyncFs)
                .is_ok()
        );
        assert!(manager.resolve_profile(ESSENTIAL_PROFILE_NAME).is_ok());
        // While a non-essential profile with the same flag stays disabled
        manager
            .register(AgentProfile {
                name: "ghost".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                enabled: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(!manager.list().contains(&"ghost".to_string()));
        assert!(manager.resolve("ghost", &LocalSyncFs).is_err());
    }

    /// should resolve the built-in coder profile without a baked-in model:
    /// the built-ins ship responsibility, prompt, and tool surface only
    /// (spec 11 R6/A7) — the model is decided by the request-time
    /// resolution chain, so the resolved config carries the empty/unset
    /// model id.
    #[test]
    fn should_resolve_builtin_coder_profile_without_a_model() {
        let manager = ProfileManager::new();

        let config = manager
            .resolve("coder", &LocalSyncFs)
            .expect("resolve should succeed");
        assert!(
            config.model_id.is_empty(),
            "the built-in coder must not bake in a concrete model"
        );
        assert_eq!(config.provider_type, ProviderType::Anthropic);
        assert!(
            config.system_prompt.as_deref().is_none(),
            "the built-in coder rides the base template, not an inline prompt"
        );
        assert!(
            config
                .role_additional
                .as_deref()
                .is_some_and(|role| role.contains("Your final message is the entire handoff")),
            "the built-in coder's role text should carry the subagent handoff contract"
        );
    }

    // ===== multi-source merge projection (R3-5) =====

    /// should carry the root's source tag on every discovered profile,
    /// including the reserved values an R4 root can pass
    #[test]
    fn should_carry_the_root_source_tag_on_discovered_profiles() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let user_dir = temp_dir.path().join("agents");
        let extra_dir = temp_dir.path().join("extra");
        fs::create_dir(&user_dir).expect("create_dir should succeed");
        fs::create_dir(&extra_dir).expect("create_dir should succeed");
        write_md_file(&user_dir, "reviewer.md", "reviewer", "You review.");
        write_md_file(&extra_dir, "auditor.md", "auditor", "You audit.");

        let roots = vec![
            ProfileSourceRoot::from(user_dir),
            ProfileSourceRoot {
                path: extra_dir,
                source: ProfileSource::Extra,
            },
        ];
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(result.profiles.len(), 2);
        let reviewer = result
            .profiles
            .iter()
            .find(|d| d.profile.name == "reviewer")
            .expect("reviewer discovered");
        assert_eq!(reviewer.source, ProfileSource::User);
        let auditor = result
            .profiles
            .iter()
            .find(|d| d.profile.name == "auditor")
            .expect("auditor discovered");
        // The reserved `extra` value flows end to end when a root carries it
        // (today no rust boot path produces one — R4's extra dirs will).
        assert_eq!(auditor.source, ProfileSource::Extra);
        assert_eq!(auditor.source.as_str(), "extra");
        // The reserved `extra` rank sits between the user and project
        // ranks — the relative ordering is the spec contract; the numeric
        // constants are implementation details this test must not pin.
        assert!(
            auditor.source.priority() > ProfileSource::User.priority()
                && auditor.source.priority() < ProfileSource::Project.priority(),
            "the extra rank must sit between the user and project ranks"
        );
    }

    // ===== plugin source (item H) =====

    /// Given same-name candidates under a user root and a plugin root (item
    /// H — the TS `PluginAgentProfileLoaderService` at priority 5), when
    /// discovery runs, then the user candidate wins and the plugin one
    /// attaches as the suppressed `priority` loser — the same source-priority
    /// merge every other root gets.
    #[test]
    fn should_let_user_profiles_win_over_same_name_plugin_profiles_with_plugin_loser_suppressed() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let user_dir = temp_dir.path().join("agents");
        let plugin_dir = temp_dir.path().join("plugin-agents");
        fs::create_dir(&user_dir).expect("create_dir should succeed");
        fs::create_dir(&plugin_dir).expect("create_dir should succeed");
        write_md_file(&user_dir, "shared.md", "shared", "You are the user one.");
        write_md_file(
            &plugin_dir,
            "shared.md",
            "shared",
            "You are the plugin one.",
        );

        let roots = vec![
            ProfileSourceRoot::from(user_dir),
            ProfileSourceRoot {
                path: plugin_dir,
                source: ProfileSource::Plugin,
            },
        ];
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");
        let winner = result
            .profiles
            .iter()
            .find(|d| d.profile.name == "shared")
            .expect("shared discovered");
        assert_eq!(
            winner.source,
            ProfileSource::User,
            "the user (10) candidate must beat the plugin (5) candidate"
        );
        assert_eq!(
            winner.profile.system_prompt.as_deref().unwrap_or_default(),
            "You are the user one."
        );
        assert_eq!(
            result.suppressed.get("shared"),
            Some(&vec![SuppressedProfileCandidate {
                source: ProfileSource::Plugin,
                reason: SuppressedReason::Priority,
            }]),
            "the plugin candidate must attach as the suppressed priority loser"
        );
    }

    /// Given a plugin root carrying a profile that covers a built-in name
    /// (item H — plugin priority 5 above builtin 0), when discovery runs,
    /// then the plugin profile wins and the displaced built-in attaches as
    /// the suppressed `priority` loser (D16), with the wire `source: plugin`.
    #[test]
    fn should_let_plugin_profiles_override_builtins_with_plugin_provenance() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let plugin_dir = temp_dir.path().join("agents");
        fs::create_dir(&plugin_dir).expect("create_dir should succeed");
        write_md_file(
            &plugin_dir,
            "coder.md",
            "coder",
            "You are the plugin coder.",
        );

        let roots = vec![ProfileSourceRoot {
            path: plugin_dir,
            source: ProfileSource::Plugin,
        }];
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");
        let winner = result
            .profiles
            .iter()
            .find(|d| d.profile.name == "coder")
            .expect("coder discovered");
        assert_eq!(winner.source, ProfileSource::Plugin);
        assert_eq!(
            winner.source.as_str(),
            "plugin",
            "the wire source spell is `plugin`"
        );
        assert_eq!(
            winner.source.priority(),
            5,
            "the plugin rank is above builtin (0) and below user (10)"
        );
        assert_eq!(
            winner.profile.system_prompt.as_deref().unwrap_or_default(),
            "You are the plugin coder."
        );
        assert_eq!(
            result.suppressed["coder"],
            vec![SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            }],
            "the displaced built-in attaches as the loser (D16)"
        );
    }

    /// Given a plugin-sourced root, when it is loaded through the tagged-
    /// roots channel (`load_from_source_roots` — the item H registration
    /// path), then the profile registers with `plugin` provenance and the
    /// plugin priority sits between builtin and user.
    #[test]
    fn should_load_plugin_source_roots_into_the_manager_with_plugin_provenance() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let plugin_dir = temp_dir.path().join("plugin-agents");
        fs::create_dir(&plugin_dir).expect("create_dir should succeed");
        write_md_file(&plugin_dir, "writer.md", "writer", "You write.");

        let manager = ProfileManager::new();
        let roots = vec![ProfileSourceRoot {
            path: plugin_dir,
            source: ProfileSource::Plugin,
        }];
        let loaded = manager
            .load_from_source_roots(&roots, &LocalSyncFs)
            .expect("the tagged-roots load should succeed");
        assert_eq!(loaded, 1, "the plugin profile must load");
        let provenance = manager
            .provenance("writer")
            .expect("the profile carries provenance");
        assert_eq!(provenance.source, ProfileSource::Plugin);
        assert_eq!(provenance.source.as_str(), "plugin");
        assert!(
            provenance.file_path.is_some(),
            "the plugin profile is file-sourced"
        );
        assert!(
            ProfileSource::Plugin.priority() > ProfileSource::Builtin.priority()
                && ProfileSource::Plugin.priority() < ProfileSource::User.priority(),
            "the plugin rank (5) must sit between builtin (0) and user (10)"
        );
    }

    /// Given same-name candidates under roots of different source
    /// priorities mounted OUT OF priority order (the lowest-priority root
    /// passed first), When discovery runs, Then the higher-priority root's
    /// candidate wins and the lower-priority one attaches as the
    /// suppressed `priority` loser — the winner is decided by source
    /// priority (T19 m1, the TS `AGENT_PROFILE_SOURCE_PRIORITY`), never by
    /// the caller's argument order.
    #[test]
    fn should_decide_same_name_winners_by_source_priority_not_argument_order() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let user_dir = temp_dir.path().join("agents");
        let project_dir = temp_dir.path().join("project-agents");
        fs::create_dir(&user_dir).expect("create_dir should succeed");
        fs::create_dir(&project_dir).expect("create_dir should succeed");
        write_md_file(&user_dir, "shared.md", "shared", "You are the user one.");
        write_md_file(
            &project_dir,
            "shared.md",
            "shared",
            "You are the project one.",
        );

        // Deliberately out of priority order: user (10) first, project (30)
        // second — the discovery-internal priority sort must decide.
        let roots = vec![
            ProfileSourceRoot::from(user_dir),
            ProfileSourceRoot {
                path: project_dir,
                source: ProfileSource::Project,
            },
        ];
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");
        let winner = result
            .profiles
            .iter()
            .find(|d| d.profile.name == "shared")
            .expect("shared discovered");
        assert_eq!(
            winner.source,
            ProfileSource::Project,
            "the higher-priority root must win regardless of argument order"
        );
        assert_eq!(
            winner.profile.system_prompt.as_deref().unwrap_or_default(),
            "You are the project one."
        );
        assert_eq!(
            result.suppressed.get("shared"),
            Some(&vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Priority,
            }]),
            "the user candidate must attach as the suppressed priority loser"
        );

        // The same corpus mounted in the priority order produces the same
        // winner — the two orderings are equivalent by construction.
        let mut priority_order = roots.clone();
        priority_order.reverse();
        let reordered =
            discover_profiles(&priority_order, &LocalSyncFs).expect("discovery should succeed");
        assert_eq!(
            reordered
                .profiles
                .iter()
                .find(|d| d.profile.name == "shared")
                .expect("shared discovered")
                .source,
            ProfileSource::Project
        );
    }

    /// should let an enabled candidate win over an earlier-root disabled
    /// one and attach the loser with reason `disabled`
    #[test]
    fn should_attach_a_disabled_loser_and_let_the_enabled_candidate_win() {
        // Given a first root whose candidate is disabled and a second root
        // with an enabled same-name candidate
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let first = temp_dir.path().join("first");
        let second = temp_dir.path().join("second");
        fs::create_dir(&first).expect("create_dir should succeed");
        fs::create_dir(&second).expect("create_dir should succeed");
        write_profile_file(
            &first,
            "muted.toml",
            "name = \"muted\"\nmodel = \"m\"\nenabled = false\n",
        );
        write_md_file(&second, "muted.md", "muted", "I am loud.");

        let roots: Vec<ProfileSourceRoot> = [first.clone(), second.clone()]
            .into_iter()
            .map(Into::into)
            .collect();
        let result = discover_profiles(&roots, &LocalSyncFs).expect("discovery should succeed");

        // Then the enabled candidate wins despite the lower-priority root
        // (a disabled candidate never wins — the TS `:192-204`) and the
        // disabled loser attaches with reason `disabled`
        assert_eq!(result.profiles.len(), 1);
        let muted = &result.profiles[0];
        assert_eq!(muted.profile.name, "muted");
        assert_eq!(muted.path, second.join("muted.md"));
        assert_eq!(
            result.suppressed["muted"],
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Disabled,
            }]
        );
    }

    /// should register a sole disabled candidate (the TS `disabledWinners`
    /// case b — listed-but-marked, never dispatched)
    #[test]
    fn should_register_a_sole_disabled_candidate_as_the_disabled_winner() {
        // Given a directory whose only candidate for the name is disabled
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "ghost.toml",
            "name = \"ghost\"\nmodel = \"m\"\nenabled = false\n",
        );

        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the load succeeds");

        // Then the disabled candidate still registers (case b): the
        // management face lists it, the dispatch face excludes it, and its
        // own row carries it as a `disabled` suppressed candidate (the TS
        // inspection semantics — the winner's suppressed list includes
        // every disabled candidate, itself included)
        assert_eq!(loaded, 1);
        assert!(manager.list_all().contains(&"ghost".to_string()));
        assert!(!manager.list().contains(&"ghost".to_string()));
        assert_eq!(
            manager.suppressed_candidates("ghost"),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Disabled,
            }]
        );
        assert_eq!(
            manager.provenance("ghost"),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(profiles_dir.join("ghost.toml")),
            })
        );
    }

    /// should keep the built-in and attach the disabled losers when every
    /// override candidate of a built-in name is disabled (the TS case a)
    #[test]
    fn should_keep_the_builtin_when_a_disabled_override_loses() {
        // Given a profiles directory holding a disabled coder override
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "coder.toml",
            "name = \"coder\"\nmodel = \"hijack\"\nenabled = false\n",
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");
        // Then no user candidate registers for the built-in name...
        assert!(result.profiles.is_empty());
        // ...and the built-in carries the disabled loser on its projection
        assert_eq!(
            result.suppressed["coder"],
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Disabled,
            }]
        );

        let manager = ProfileManager::new();
        manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the load succeeds");
        // And the built-in definition stays the registered coder — with no
        // model of its own (spec 11 R6/A7: built-ins ship no model)
        assert!(manager.get("coder").unwrap().model.is_empty());
        assert_eq!(
            manager.suppressed_candidates("coder"),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Disabled,
            }]
        );
        assert_eq!(
            manager.provenance("coder"),
            Some(ProfileProvenance {
                source: ProfileSource::Builtin,
                file_path: None,
            })
        );
    }

    /// should treat a file declaring the essential name as a normal
    /// candidate in the discovery merge (D16) — and displace the built-in
    /// into the suppressed projection, never the skip channel (D13)
    #[test]
    fn should_load_essential_candidates_in_the_discovery_merge() {
        // Given a directory declaring the essential name
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "agent.toml",
            "name = \"agent\"\nmodel = \"gpt-4o\"\n",
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");

        // Then the candidate parses into the profile set (an override)…
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, ESSENTIAL_PROFILE_NAME);
        assert_eq!(result.profiles[0].profile.model, "gpt-4o");
        // …the skip channel stays empty (D13: load failures only)…
        assert!(result.skipped.is_empty());
        // …and the displaced built-in definition is the suppressed candidate
        assert_eq!(
            result.suppressed[ESSENTIAL_PROFILE_NAME],
            vec![SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            }]
        );

        // And the manager registers the override next to that projection
        let manager = ProfileManager::new();
        manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the load succeeds");
        assert_eq!(manager.get(ESSENTIAL_PROFILE_NAME).unwrap().model, "gpt-4o");
        assert_eq!(
            manager.suppressed_candidates(ESSENTIAL_PROFILE_NAME),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            }]
        );
    }

    /// should silently drop same-root same-name losers (D13 — the skip
    /// channel carries load failures only; the TS loader dedups within an
    /// entry without a diagnostic)
    #[test]
    fn should_silently_drop_same_root_same_name_losers() {
        // Given one root with two same-name files
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "a.toml",
            "name = \"shared\"\nmodel = \"a\"\n",
        );
        write_profile_file(
            &profiles_dir,
            "b.toml",
            "name = \"shared\"\nmodel = \"b\"\n",
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");

        // Then the first file wins, the same-directory loser produces no
        // diagnostic at all (the TS model has no same-entry suppressed
        // candidates, and a legal loser is never a skip entry), and no
        // suppressed projection is invented for it
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.model, "a");
        assert!(result.skipped.is_empty());
        assert!(result.suppressed.is_empty());
    }

    /// should displace a covered non-essential built-in into the winner's
    /// suppressed candidates (D16 isomorphism: the essential and `coder`
    /// project the same shape)
    #[test]
    fn should_displace_a_covered_builtin_into_the_suppressed_candidates() {
        // Given a directory whose file covers the `coder` built-in
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(&profiles_dir, "coder.md", "coder", "My own coder.");

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("discovery should succeed");

        // Then the user file is the winner and the built-in definition is
        // its suppressed candidate — `{source: builtin, priority: 0,
        // reason: priority}`, the same shape the essential override carries
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(
            result.profiles[0].profile.system_prompt.as_deref(),
            Some("My own coder.")
        );
        assert!(result.skipped.is_empty());
        assert_eq!(
            result.suppressed["coder"],
            vec![SuppressedProfileCandidate {
                source: ProfileSource::Builtin,
                reason: SuppressedReason::Priority,
            }]
        );
    }

    /// should let an essential candidate carrying enabled = false win the
    /// merge anyway (the D16 discovery-level mirror of the
    /// `is_file_disabled` exemption)
    #[test]
    fn should_treat_an_essential_candidate_with_enabled_false_as_a_winner() {
        // Given a directory whose essential-name declaration carries the
        // file-level disable flag
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "agent.toml",
            "name = \"agent\"\nmodel = \"gpt-4o\"\nenabled = false\n",
        );

        let manager = ProfileManager::new();
        let loaded = manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("the load succeeds");

        // Then the candidate still wins (the flag never disables the
        // essential) and the wire-side exemption projects it enabled
        assert_eq!(loaded, 1);
        let essential = manager.get(ESSENTIAL_PROFILE_NAME).unwrap();
        assert_eq!(essential.model, "gpt-4o");
        assert!(!is_file_disabled(&essential));
        assert!(manager.list().contains(&ESSENTIAL_PROFILE_NAME.to_string()));
        // And no `disabled` suppressed candidate was recorded for it
        assert!(
            !manager
                .suppressed_candidates(ESSENTIAL_PROFILE_NAME)
                .iter()
                .any(|candidate| candidate.reason == SuppressedReason::Disabled)
        );
    }

    /// should retain provenance and the suppressed projection on the
    /// manager across a roots load, with builtins tagged builtin
    #[test]
    fn should_retain_provenance_and_suppressed_on_the_manager() {
        // Given the user chain (brand agents root over the legacy profiles
        // dir) with a same-name collision plus a plain user profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&agents_dir).expect("create_dir should succeed");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_md_file(
            &agents_dir,
            "reviewer.md",
            "reviewer",
            "From the agents root.",
        );
        write_profile_file(
            &profiles_dir,
            "reviewer.toml",
            "name = \"reviewer\"\nmodel = \"from-profiles\"\n",
        );
        write_profile_file(
            &profiles_dir,
            "other.toml",
            "name = \"other\"\nmodel = \"m\"\n",
        );

        let manager = ProfileManager::new();
        let roots = [agents_dir.clone(), profiles_dir.clone()];
        let loaded = manager
            .load_from_roots(&roots, &LocalSyncFs)
            .expect("the load succeeds");
        assert_eq!(loaded, 2);

        // Then the winner carries its root's tag and file, the built-ins
        // carry the builtin tag with no file, and the loser's projection is
        // queryable for the wire row
        assert_eq!(
            manager.provenance("reviewer"),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(agents_dir.join("reviewer.md")),
            })
        );
        assert_eq!(
            manager.provenance("other"),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(profiles_dir.join("other.toml")),
            })
        );
        assert_eq!(
            manager.provenance("coder"),
            Some(ProfileProvenance {
                source: ProfileSource::Builtin,
                file_path: None,
            })
        );
        assert_eq!(
            manager.suppressed_candidates("reviewer"),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Priority,
            }]
        );
        assert!(manager.suppressed_candidates("other").is_empty());
    }

    // ===== default_model / capabilities / prompt_vars (G-22) =====

    /// should inherit default_model and capabilities from the parent when
    /// flattening a legacy extends
    #[test]
    fn should_inherit_default_model_and_capabilities_when_flattening() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir(&profiles_dir).expect("create_dir should succeed");
        write_profile_file(
            &profiles_dir,
            "parent.toml",
            r#"
name = "parent"
model = "claude-sonnet-4-20250514"
default_model = "claude-sonnet-4"
capabilities = ["text", "vision"]
"#,
        );
        write_profile_file(
            &profiles_dir,
            "child.toml",
            "name = \"child\"\nextends = \"parent\"\n",
        );

        let result = parse_dir(&profiles_dir, &LocalSyncFs).expect("flatten should succeed");
        let child = result
            .profiles
            .iter()
            .find(|discovered| discovered.profile.name == "child")
            .expect("child is loaded");
        assert_eq!(
            child.profile.default_model,
            Some("claude-sonnet-4".to_string())
        );
        assert_eq!(
            child.profile.capabilities,
            Some(vec!["text".to_string(), "vision".to_string()])
        );
    }

    /// should substitute prompt_vars into a system prompt template
    #[test]
    fn should_substitute_prompt_vars_in_system_prompt() {
        let vars = Some(HashMap::from([("lang".into(), "rust".into())]));
        let result = render_system_prompt("You write {{lang}} code.", &vars);
        assert_eq!(result, "You write rust code.");
    }

    // ===== persistence write face (P6: the REST agent-profile CRUD) =====

    /// Given a manager and an agents directory that does not exist yet,
    /// When a profile is upserted, Then a `.md` file is written (the
    /// `serialize_agent_file` emission) and the profile registers.
    #[test]
    fn should_write_profile_file_and_register_when_upsert_profile_is_called() {
        // Given a manager and an agents directory that does not exist yet
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();

        // When a profile is upserted into the directory
        manager
            .upsert_profile(
                AgentProfile {
                    name: "reviewer".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    description: Some("A code reviewer.".to_string()),
                    system_prompt: Some("You review code.".to_string()),
                    tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");

        // Then the profile is registered in memory
        let registered = manager.get("reviewer").expect("profile should exist");
        assert_eq!(registered.description.as_deref(), Some("A code reviewer."));

        // And the on-disk file exists in the same format `parse_dir` reads
        let file = agents_dir.join("reviewer.md");
        assert!(file.is_file(), "profile file should be written to disk");
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("parse_dir should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.name, "reviewer");
        assert_eq!(
            result.profiles[0].profile.system_prompt.as_deref(),
            Some("You review code.")
        );
        assert_eq!(
            result.profiles[0].profile.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
    }

    /// Given a full-field profile upserted to disk, When a fresh manager
    /// reloads it, Then every `.md`-persisted field survived the round trip
    /// — and the fields the `.md` schema cannot carry fall back to their
    /// defaults (the unified schema is the round-trippable surface, the
    /// `From<AgentFileDef>` inverse).
    #[test]
    fn should_roundtrip_every_md_field_when_an_upserted_profile_is_reloaded() {
        // Given a full-field profile upserted to disk
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();
        let profile = AgentProfile {
            name: "full".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            provider: Some("anthropic".to_string()),
            system_prompt: Some("You are thorough.".to_string()),
            max_turns: Some(120),
            max_steps_per_turn: Some(8),
            permission_mode: Some(PermissionMode::Yolo),
            tools: Some(vec!["Read".to_string(), "Edit".to_string()]),
            disallowed_tools: Some(vec!["Bash".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            thinking_effort: Some(ThinkingEffort::High),
            subagents: Some(vec!["scout".to_string()]),
            description: Some("A full profile.".to_string()),
            when_to_use: Some("When everything must roundtrip.".to_string()),
            default_model: Some("claude-sonnet-4-20250514".to_string()),
            capabilities: Some(vec!["text".to_string(), "vision".to_string()]),
            prompt_vars: Some(HashMap::from([("lang".to_string(), "rust".to_string())])),
            enabled: Some(false),
            ..Default::default()
        };
        manager
            .upsert_profile(profile, &agents_dir, &legacy_dir, &LocalSyncFs)
            .expect("upsert should succeed");

        // When a fresh manager loads the same directory (the restart path)
        let restarted = ProfileManager::new();
        restarted
            .load_from_dir(&agents_dir, &LocalSyncFs)
            .expect("load should succeed");

        // Then every `.md`-persisted field survived the write/read round trip
        let reloaded = restarted.get("full").expect("profile should exist");
        assert_eq!(reloaded.model, "claude-sonnet-4-20250514");
        assert_eq!(reloaded.system_prompt.as_deref(), Some("You are thorough."));
        assert_eq!(
            reloaded.tools,
            Some(vec!["Read".to_string(), "Edit".to_string()])
        );
        assert_eq!(reloaded.disallowed_tools, Some(vec!["Bash".to_string()]));
        assert_eq!(
            reloaded.capabilities_required,
            Some(vec!["code.read".to_string()])
        );
        assert_eq!(reloaded.subagents, Some(vec!["scout".to_string()]));
        assert_eq!(reloaded.description.as_deref(), Some("A full profile."));
        assert_eq!(
            reloaded.when_to_use.as_deref(),
            Some("When everything must roundtrip.")
        );
        assert_eq!(
            reloaded.default_model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(reloaded.enabled, Some(false));
        // And the fields the `.md` schema cannot carry (the legacy TOML-only
        // surface) are not persisted — they fall back to their defaults.
        assert_eq!(reloaded.provider, None);
        assert_eq!(reloaded.max_turns, None);
        assert_eq!(reloaded.max_steps_per_turn, None);
        assert_eq!(reloaded.permission_mode, None);
        assert_eq!(reloaded.thinking_effort, None);
        assert_eq!(reloaded.capabilities, None);
        assert_eq!(reloaded.prompt_vars, None);
    }

    /// should persist the enabled flag only when false (the TS semantics)
    #[test]
    fn should_persist_enabled_only_when_false_when_upserting_a_profile() {
        // Given an agents directory and a manager
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();

        // When a disabled profile is upserted
        manager
            .upsert_profile(
                AgentProfile {
                    name: "off".to_string(),
                    model: "model-a".to_string(),
                    description: Some("Off.".to_string()),
                    enabled: Some(false),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");
        // Then the flag is on disk...
        let off = fs::read_to_string(agents_dir.join("off.md")).expect("file readable");
        assert!(off.contains("enabled: false"), "file: {off}");

        // And when an explicitly enabled profile is upserted, nothing is
        // written (None and Some(true) both mean enabled)
        manager
            .upsert_profile(
                AgentProfile {
                    name: "on".to_string(),
                    model: "model-a".to_string(),
                    description: Some("On.".to_string()),
                    enabled: Some(true),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");
        let on = fs::read_to_string(agents_dir.join("on.md")).expect("file readable");
        let parsed = parse_agent_file_text("on.md", &on).expect("the file must parse");
        assert_eq!(
            parsed.enabled, None,
            "the enabled flag must not be written for an enabled profile: {on}"
        );
    }

    /// should replace the previous file content and registration when
    /// upsert_profile is called twice for the same name
    #[test]
    fn should_replace_previous_definition_when_upsert_is_called_twice() {
        // Given a profile upserted once
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();
        manager
            .upsert_profile(
                AgentProfile {
                    name: "twice".to_string(),
                    model: "model-a".to_string(),
                    description: Some("First.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("first upsert should succeed");

        // When the same name is upserted with a new definition
        manager
            .upsert_profile(
                AgentProfile {
                    name: "twice".to_string(),
                    model: "model-b".to_string(),
                    description: Some("Updated.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("second upsert should succeed");

        // Then exactly one file exists and it holds the new definition
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("parse_dir should succeed");
        assert_eq!(result.profiles.len(), 1);
        assert_eq!(result.profiles[0].profile.model, "model-b");
        assert_eq!(
            manager.get("twice").expect("profile exists").model,
            "model-b"
        );
    }

    /// Given a discovered agents-root `.md` profile, When it is deleted,
    /// Then the real file is removed (T19 m4), the legacy same-name files
    /// are cleaned, and the stale suppressed-candidates projection is reset
    /// (T19 m3).
    #[test]
    fn should_delete_the_discovered_md_file_and_reset_the_projection_when_delete_is_called() {
        // Given an agents root whose .md won a same-name merge over a legacy
        // .toml (the loser recorded as a suppressed candidate)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        fs::create_dir(&agents_dir).expect("create agents dir should succeed");
        fs::create_dir(&legacy_dir).expect("create legacy dir should succeed");
        write_md_file(&agents_dir, "scout.md", "scout", "You scout.");
        write_profile_file(&legacy_dir, "scout.toml", "name = \"scout\"\n");
        let manager = ProfileManager::new();
        manager
            .load_from_roots(&[agents_dir.clone(), legacy_dir.clone()], &LocalSyncFs)
            .expect("roots load should succeed");
        assert_eq!(
            manager.provenance("scout").and_then(|p| p.file_path),
            Some(agents_dir.join("scout.md"))
        );
        assert_eq!(manager.suppressed_candidates("scout").len(), 1);

        // When the profile is deleted
        manager
            .delete_profile("scout", &agents_dir, &legacy_dir, &LocalSyncFs)
            .expect("delete should succeed");

        // Then the real file is gone (T19 m4) — not just a `<name>.toml`
        // that never existed — the legacy loser is cleaned, the name is
        // unregistered, and the suppressed projection is reset (T19 m3)
        assert!(!agents_dir.join("scout.md").exists());
        assert!(!legacy_dir.join("scout.toml").exists());
        assert!(manager.get("scout").is_none());
        assert!(manager.suppressed_candidates("scout").is_empty());
    }

    /// should remove the file and the registration when delete_profile is
    /// called for a user profile
    #[test]
    fn should_remove_file_and_registration_when_delete_profile_is_called() {
        // Given an upserted user profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();
        manager
            .upsert_profile(
                AgentProfile {
                    name: "gone".to_string(),
                    model: "model-a".to_string(),
                    description: Some("Gone soon.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");
        assert!(agents_dir.join("gone.md").is_file());

        // When the profile is deleted
        manager
            .delete_profile("gone", &agents_dir, &legacy_dir, &LocalSyncFs)
            .expect("delete should succeed");

        // Then the file is gone and the name is no longer registered
        assert!(!agents_dir.join("gone.md").exists());
        assert!(manager.get("gone").is_none());
        assert!(!manager.list().contains(&"gone".to_string()));
    }

    /// should restore the built-in definition when an override profile is
    /// deleted
    #[test]
    fn should_restore_builtin_when_an_override_profile_is_deleted() {
        // Given a user file overriding the built-in coder profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();
        manager
            .upsert_profile(
                AgentProfile {
                    name: "coder".to_string(),
                    model: "gpt-4o".to_string(),
                    description: Some("My coder.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &LocalSyncFs,
            )
            .expect("upsert should succeed");
        assert!(manager.get("coder").expect("profile exists").model == "gpt-4o");

        // When the override is deleted
        manager
            .delete_profile("coder", &agents_dir, &legacy_dir, &LocalSyncFs)
            .expect("delete should succeed");

        // Then the built-in coder definition is back and the file is gone
        assert!(!agents_dir.join("coder.md").exists());
        let restored = manager.get("coder").expect("builtin coder should be back");
        // The restored built-in carries no model (spec 11 R6/A7)
        assert!(restored.model.is_empty());
    }

    /// should reject unsafe or empty profile names on the persistence face
    #[test]
    fn should_reject_unsafe_profile_names_when_upserting() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let legacy_dir = temp_dir.path().join("profiles");
        let manager = ProfileManager::new();

        for name in ["", "a/b", "a\\b", "..", "."] {
            let error = manager
                .upsert_profile(
                    AgentProfile {
                        name: name.to_string(),
                        model: "model-a".to_string(),
                        ..Default::default()
                    },
                    &agents_dir,
                    &legacy_dir,
                    &LocalSyncFs,
                )
                .expect_err(&format!("name {name:?} must be rejected"));
            assert!(
                error.to_string().contains("invalid agent profile name"),
                "name {name:?} error should explain the rejection: {error}"
            );
        }
        // And nothing was written — the agents directory was never
        // created (a missing root is "nothing here", not an error)
        let result = parse_dir(&agents_dir, &LocalSyncFs).expect("missing root is not an error");
        assert!(result.profiles.is_empty(), "no files should be discovered");
        assert!(result.skipped.is_empty());
        assert_eq!(result.scanned_roots, vec![agents_dir]);
    }

    // ===== injected SyncFs (T-13b1-1: zero-IO state plane) =====

    /// In-memory [`mirri_os::SyncFs`] double recording the persistence
    /// operations the profile CRUD performs, so the write/delete face can be
    /// verified without touching the real filesystem.
    #[derive(Default)]
    struct RecordingFs {
        files: std::sync::Mutex<HashMap<PathBuf, String>>,
        mkdirs: std::sync::Mutex<Vec<(PathBuf, bool)>>,
        removed: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl mirri_os::SyncFs for RecordingFs {
        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
        }

        fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), content.to_string());
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        fn mkdir(&self, path: &Path, recursive: bool) -> anyhow::Result<()> {
            self.mkdirs
                .lock()
                .unwrap()
                .push((path.to_path_buf(), recursive));
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
            self.files.lock().unwrap().remove(path);
            self.removed.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    /// should write the profile file through the injected fs when
    /// upsert_profile is called (T-13b1-1)
    #[test]
    fn should_write_profile_file_through_injected_fs_when_upsert_profile_is_called() {
        // Given a manager and an in-memory fs double (no real directories)
        let fs = RecordingFs::default();
        let agents_dir = PathBuf::from("/virtual/agents");
        let legacy_dir = PathBuf::from("/virtual/profiles");
        let manager = ProfileManager::new();

        // When a profile is upserted through the double
        manager
            .upsert_profile(
                AgentProfile {
                    name: "reviewer".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    description: Some("Reviews.".to_string()),
                    system_prompt: Some("You review code.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &fs,
            )
            .expect("upsert should succeed");

        // Then the double observed a recursive mkdir of the agents dir
        assert_eq!(
            fs.mkdirs.lock().unwrap().as_slice(),
            &[(agents_dir.clone(), true)]
        );
        // ...and the .md file was written through the double, not the disk
        let written = fs
            .files
            .lock()
            .unwrap()
            .get(&agents_dir.join("reviewer.md"))
            .expect("profile file written through the double")
            .clone();
        assert!(written.contains("name: reviewer"));
        assert!(written.contains("defaultModel: claude-sonnet-4-20250514"));
        assert!(written.contains("You review code."));
        assert!(!agents_dir.join("reviewer.md").exists());
        // ...and the profile is registered in memory
        assert!(manager.get("reviewer").is_some());
    }

    /// should remove the file through the injected fs and stay idempotent
    /// when delete_profile is called (T-13b1-1)
    #[test]
    fn should_remove_file_through_injected_fs_and_stay_idempotent_when_delete_profile_is_called() {
        // Given a manager with a profile upserted behind a double
        let fs = RecordingFs::default();
        let agents_dir = PathBuf::from("/virtual/agents");
        let legacy_dir = PathBuf::from("/virtual/profiles");
        let manager = ProfileManager::new();
        manager
            .upsert_profile(
                AgentProfile {
                    name: "gone".to_string(),
                    model: "model-a".to_string(),
                    description: Some("Gone soon.".to_string()),
                    ..Default::default()
                },
                &agents_dir,
                &legacy_dir,
                &fs,
            )
            .expect("upsert should succeed");

        // When the profile is deleted through the double
        manager
            .delete_profile("gone", &agents_dir, &legacy_dir, &fs)
            .expect("delete should succeed");

        // Then the removal went through the double and the state is cleared
        assert_eq!(
            fs.removed.lock().unwrap().as_slice(),
            &[agents_dir.join("gone.md")]
        );
        assert!(!fs.exists(&agents_dir.join("gone.md")));
        assert!(manager.get("gone").is_none());

        // And a second delete of the already-missing file is a no-op
        manager
            .delete_profile("gone", &agents_dir, &legacy_dir, &fs)
            .expect("delete of a missing file should be idempotent");
        assert_eq!(fs.removed.lock().unwrap().len(), 1);
    }
    // ===== summaryPolicy + promptPrefix (B4 T1, D-61/D-47) =====

    /// should parse summaryPolicy and promptPrefix when a TOML profile
    /// declares them
    #[test]
    fn should_parse_summary_policy_and_prompt_prefix_when_profile_declares_them() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().to_path_buf();
        write_profile_file(
            &profiles_dir,
            "summarizer.toml",
            r#"
name = "summarizer"
model = "claude-sonnet-4-20250514"
promptPrefix = "You are operating inside a monorepo."

[summaryPolicy]
minChars = 80
continuationPrompt = "Your final summary is too short — expand it."
retries = 2
"#,
        );

        let manager = ProfileManager::new();
        manager
            .load_from_roots(&[profiles_dir], &LocalSyncFs)
            .expect("load should succeed");

        let profile = manager.get("summarizer").expect("profile");
        assert_eq!(
            profile.prompt_prefix.as_deref(),
            Some("You are operating inside a monorepo.")
        );
        let policy = profile.summary_policy.expect("summary policy");
        assert_eq!(policy.min_chars, 80);
        assert_eq!(
            policy.continuation_prompt,
            "Your final summary is too short — expand it."
        );
        assert_eq!(policy.retries, 2);
    }

    /// should default summaryPolicy and promptPrefix to None when absent
    #[test]
    fn should_default_summary_policy_and_prompt_prefix_to_none_when_absent() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().to_path_buf();
        write_profile_file(
            &profiles_dir,
            "plain.toml",
            r#"
name = "plain"
model = "claude-sonnet-4-20250514"
"#,
        );

        let manager = ProfileManager::new();
        manager
            .load_from_roots(&[profiles_dir], &LocalSyncFs)
            .expect("load should succeed");

        let profile = manager.get("plain").expect("profile");
        assert_eq!(profile.summary_policy, None);
        assert_eq!(profile.prompt_prefix, None);
    }

    /// should round-trip summaryPolicy and promptPrefix through serde with
    /// the TS camelCase field names
    #[test]
    fn should_roundtrip_summary_policy_and_prompt_prefix_with_camel_case_fields() {
        let profile = AgentProfile {
            name: "summarizer".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            prompt_prefix: Some("prefix text".to_string()),
            summary_policy: Some(ProfileSummaryPolicy {
                min_chars: 120,
                continuation_prompt: "keep going".to_string(),
                retries: 3,
            }),
            ..AgentProfile::default()
        };

        let value = serde_json::to_value(&profile).expect("serialize");
        // The wire field names are camelCase, matching the TS profile shape.
        assert_eq!(value["promptPrefix"], json!("prefix text"));
        assert_eq!(value["summaryPolicy"]["minChars"], json!(120));
        assert_eq!(
            value["summaryPolicy"]["continuationPrompt"],
            json!("keep going")
        );
        assert_eq!(value["summaryPolicy"]["retries"], json!(3));
        // Absent fields must not serialize (clean files for profiles that
        // do not opt in).
        let plain = serde_json::to_value(AgentProfile {
            name: "plain".to_string(),
            model: "m".to_string(),
            ..AgentProfile::default()
        })
        .expect("serialize");
        assert!(plain.get("promptPrefix").is_none());
        assert!(plain.get("summaryPolicy").is_none());

        let back: AgentProfile = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, profile);
    }

    #[test]
    fn should_round_trip_model_declared_custom_efforts_verbatim() {
        // Given a model-declared effort the engine does not enumerate
        let effort: ThinkingEffort = serde_json::from_value(json!("max")).expect("max parses");
        // Then it is carried as Custom and serializes back unchanged
        assert_eq!(effort, ThinkingEffort::Custom("max".to_string()));
        assert_eq!(serde_json::to_value(&effort).unwrap(), json!("max"));
    }

    #[test]
    fn should_normalize_kosong_and_case_spellings_when_deserializing_efforts() {
        for (wire, expected) in [
            ("off", ThinkingEffort::None),
            ("none", ThinkingEffort::None),
            ("on", ThinkingEffort::Medium),
            ("low", ThinkingEffort::Low),
            ("medium", ThinkingEffort::Medium),
            ("high", ThinkingEffort::High),
            ("MAX", ThinkingEffort::Custom("max".to_string())),
            (" Low ", ThinkingEffort::Low),
        ] {
            let effort: ThinkingEffort = serde_json::from_value(json!(wire)).expect("parses");
            assert_eq!(effort, expected, "spelling {wire:?}");
        }
    }

    #[test]
    fn should_reject_blank_thinking_effort_strings() {
        // Given a blank effort string (the TS setThinking blank rejection,
        // SESSION_THINKING_EMPTY)
        let err = serde_json::from_value::<ThinkingEffort>(json!("  "));
        // Then deserialization fails instead of binding a phantom effort
        assert!(err.is_err());
    }
}
