//! A config-aware [`LlmFactory`](crate::LlmFactory): resolves model aliases
//! through the configured `[models.<alias>]` → `[providers.<id>]` chain the
//! same way `model.generate` does (the `apiKey`/`baseUrl` a real user keeps
//! in config.toml).
//!
//! Resolution order for an agent's `model_id` (spec 11 M1 — the two
//! accepted forms, nothing else):
//!
//! 1. **Known alias** (the id is a key of the config's `models` map) — the
//!    alias's `model` field names the upstream model, its `provider` field
//!    selects a `[providers.<id>]` entry, and the provider's
//!    `type`/`baseUrl` fields drive the adapter construction. Credentials
//!    resolve through the credential store first (the value baked in at
//!    construction; `external` entries resolve through their method's CLI,
//!    e.g. Azure CLI, at call time with env-chain fallback on failure),
//!    then the type's conventional key in the provider's
//!    `env` sub-table, then the type's environment variable; a provider
//!    without one falls back to the type's conventional key in the
//!    provider's `env` sub-table, then to the type's environment variable
//!    (same three-step chain for `baseUrl`).
//! 2. **`provider/model` composite** (the C6 id composition the web
//!    pickers send) — the id matches a configured alias whose `provider` +
//!    `model` fields spell the composite, so a config keeping the pair
//!    under another alias key still resolves. This mirrors the TS daemon,
//!    whose models.dev-seeded registry serves such composites as real
//!    alias keys.
//! 3. **Anything else** — a clear `model 'X' is not configured` error
//!    listing the available aliases: never a silent wrong-provider
//!    default, and never a bare-name environment fallback (ADR-004 — the
//!    environment-synthesized `[models]` entries the config layer seeds
//!    from `MIRRICODE_MODEL_*` are still form 1; this is only about the
//!    unregistered-name bypass).
//!
//! The provider `customHeaders` field is forwarded to the adapter: the
//! kosong transports (`AnthropicProvider`, `OpenAILegacyProvider`, …)
//! apply them as extra request headers, so a gateway requiring a specific
//! `User-Agent` (or similar) answers instead of hanging.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use mirri_core::{
    AgentScopeConfig, Llm, LlmFactory, LlmResolver, ServerConfig, ThinkingEffort,
    resolve_model_alias_in,
};
use mirri_credentials::{CredentialStore, CredentialValue};
use mirri_persistence::provider_file_cache::ProviderFileCache;
use serde_json::Value;

use super::{AnthropicFiles, AnthropicLlm, GoogleLlm, KimiLlm, OpenAILlm, OpenAIResponsesLlm};

/// An [`LlmFactory`](crate::LlmFactory) that resolves model aliases
/// through the server's config snapshot — the two M1 forms or a
/// not-configured error, never a bare-name environment fallback.
pub struct ConfigAwareLlmFactory {
    /// The live server configuration snapshot (`config.toml` + hot reload).
    config: Arc<RwLock<ServerConfig>>,
    /// The explicitly injected Mirri home directory, when one was given —
    /// the root the provider-side media file cache is read from
    /// (`<home>/provider-files`). `None` falls back to the env-based home
    /// resolution exactly like [`EmbeddedServer::home_root`].
    home: Option<PathBuf>,
    /// The credential store rooted at the Mirri home, when one opens —
    /// the first credential tier every provider resolution consults.
    /// `None` (no home, or an unopenable store) leaves the resolution on
    /// the env chain alone.
    credentials: Option<CredentialStore>,
}

impl ConfigAwareLlmFactory {
    /// Build a factory resolving aliases through `config`, rooting
    /// home-scoped consumers in `home` (when given).
    pub fn new(config: Arc<RwLock<ServerConfig>>, home: Option<PathBuf>) -> Self {
        let credentials = resolve_credential_store(home.as_deref());
        Self { config, home, credentials }
    }

    /// Resolve a known alias through its configured provider and construct
    /// the matching adapter.
    fn create_llm_from_alias(
        &self,
        agent_config: &AgentScopeConfig,
        alias: &Value,
        providers: &Value,
    ) -> Result<Arc<dyn Llm>, String> {
        let alias_id = agent_config.model_id.as_str();
        let Some(provider_id) = alias.get("provider").and_then(Value::as_str) else {
            return Err(format!("model '{alias_id}' has no provider configured"));
        };
        let Some(provider) = providers.get(provider_id) else {
            return Err(format!(
                "provider '{provider_id}' (model '{alias_id}') does not exist"
            ));
        };

        // The alias's `model` names the upstream model; a missing field
        // falls back to the alias id itself (the model_handlers resolution).
        let upstream_model = alias
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(alias_id)
            .to_string();

        // The provider type drives the adapter; a missing type defaults to
        // openai (the model_handlers resolution default).
        let provider_type = provider
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("openai")
            .trim()
            .to_ascii_lowercase();

        // API key (#186): the credential store's active entry for the
        // provider first (key → the value itself; external → resolved
        // through the method's CLI at call time, a failure propagating
        // the guidance with no fallback — V5; oauth → reserved slot), else
        // the provider's `env` sub-table (the user's explicit
        // configuration). A provider with no credential in either tier is
        // a clear configuration error — the shell environment variables
        // are never consulted implicitly.
        // The sync-call tradeoff: `create_llm_from_alias` implements the
        // (sync) `LlmFactory` trait consumed by the agent callback chain —
        // the az cold-start (1-3s) blocks that agent worker thread, not a
        // tokio async worker. The async `model.generate` dispatch path
        // wraps its own resolution in `spawn_blocking` (see model_handlers).
        let api_key = match resolve_provider_credential(
            self.credentials.as_ref(),
            provider_id,
            provider,
            &provider_type,
        ) {
            Ok(Some(key)) => key,
            // The external source failed with guidance — surface it
            // verbatim (never a configuration face).
            Err(guidance) => return Err(guidance),
            Ok(None) => return Err(missing_credential_error(provider_id)),
        };

        // Base URL (#184): the provider's `baseUrl` field, else the type's
        // key in the provider's `env` sub-table, else the FIRST-PARTY
        // default — but only when the provider IS that first party (the
        // sanctioned bare usage: picking openai / anthropic / google-genai
        // without a URL). Any other provider (a models.dev import, a
        // hand-added gateway) must carry an explicit base URL: a missing
        // one is a clear configuration error at creation time, never a
        // silent request to the official endpoint that would retry 10×
        // into a low-level network failure. An empty/whitespace-only
        // `baseUrl` field counts as absent (the web clear path writes "").
        let base_url = match provider
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            Some(url) => url.to_string(),
            None => match env_value_of(provider, Some(base_url_env_name(&provider_type))) {
                Some(url) => url,
                None => match first_party_default_base_url(provider_id, &provider_type) {
                    Some(url) => url,
                    None => {
                        return Err(format!(
                            "provider '{provider_id}' has no base URL configured; please configure a \
                             base URL for provider '{provider_id}' (Settings → Providers → \
                             {provider_id}, or the baseUrl field in config.toml)",
                        ))
                    }
                },
            },
        };

        // The thinking-off contract for this alias: how the endpoint spells
        // "stop thinking" and whether an off may be sent at all. Resolved
        // once here and attached to the OpenAI-dialect adapter below; the
        // kimi adapter owns its own hardcoded dialect.
        let declared_capabilities: Vec<String> = alias
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|caps| {
                caps.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let declared_efforts = alias
            .get("supportEfforts")
            .and_then(Value::as_array)
            .is_some_and(|efforts| !efforts.is_empty());
        let thinking_intent = mirri_core::thinking_dialect::ThinkingIntent::resolve(
            &declared_capabilities,
            declared_efforts,
            alias.get("thinkingOff").and_then(Value::as_str),
            provider.get("thinkingOff").and_then(Value::as_str),
            &base_url,
            &upstream_model,
        );

        let system_prompt = agent_config.system_prompt.clone().unwrap_or_default();
        let thinking_effort = agent_config.thinking_effort.clone();

        // The provider's `customHeaders` (a name → value map on the wire,
        // header names kept verbatim) travels to the transport as extra
        // request headers.
        let custom_headers = custom_headers_of(provider);

        // Multimodal spec §4: resolve media capabilities from the alias
        // declaration ∪ name inference and merge them into the adapter —
        // replaces the provider-level hardcoded `image_in: true`, which
        // treated text-only models as vision-capable (ROADMAP §3 P0).
        let declared: Vec<String> = alias
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let media_caps =
            mirri_core::model_catalog::resolve_media_capability(&declared, &upstream_model);

        match provider_type.as_str() {
            "anthropic" => {
                // The media-filerefs switch (T11a, adjudication D2: default
                // true) gates the Files API materialization channel; the
                // content-addressed cache roots at the Mirri home's
                // `provider-files/` subdirectory.
                let media_file_refs = provider
                    .get("mediaFileRefs")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let mut llm = AnthropicLlm::with_headers(
                    api_key.clone(),
                    base_url.clone(),
                    upstream_model,
                    system_prompt,
                    thinking_effort,
                    custom_headers.clone(),
                )
                .with_media_capability(media_caps.clone())
                .with_provider_id(provider_id.to_string());
                if media_file_refs && let Some(cache_root) = self.media_cache_root() {
                    llm = llm.with_media_files(
                        AnthropicFiles::new(api_key, base_url).with_extra_headers(custom_headers),
                        Arc::new(ProviderFileCache::new(cache_root)),
                    );
                }
                Ok(Arc::new(llm))
            }
            "openai" => Ok(Arc::new(
                OpenAILlm::with_headers(
                    api_key,
                    base_url,
                    upstream_model,
                    system_prompt,
                    thinking_effort,
                    custom_headers,
                )
                .with_media_capability(media_caps.clone())
                .with_provider_id(provider_id.to_string())
                .with_thinking_intent(thinking_intent),
            )),
            "openai_responses" => Ok(Arc::new(
                OpenAIResponsesLlm::with_headers(
                    api_key,
                    base_url,
                    upstream_model,
                    system_prompt,
                    thinking_effort,
                    custom_headers,
                )
                .with_media_capability(media_caps.clone())
                .with_provider_id(provider_id.to_string()),
            )),
            "google" | "google-genai" | "google_genai" => Ok(Arc::new(
                GoogleLlm::with_headers(
                    api_key,
                    base_url,
                    upstream_model,
                    system_prompt,
                    thinking_effort,
                    custom_headers,
                )
                .with_media_capability(media_caps.clone())
                .with_provider_id(provider_id.to_string()),
            )),
            "kimi" => Ok(Arc::new(
                KimiLlm::with_headers(
                    api_key,
                    base_url,
                    upstream_model,
                    system_prompt,
                    thinking_effort,
                    custom_headers,
                )
                .with_media_capability(media_caps.clone())
                .with_provider_id(provider_id.to_string()),
            )),
            "custom" => Err(format!(
                "Custom provider type not supported by ConfigAwareLlmFactory (model '{alias_id}')"
            )),
            other => Err(format!(
                "unsupported provider type '{other}' for provider '{provider_id}' (model '{alias_id}')"
            )),
        }
    }

    /// The provider-side media file cache root (T11a): the Mirri home's
    /// `provider-files/` subdirectory, resolved through the same home chain
    /// as [`Self::mirri_home`] (home-level lifecycle, adjudication D3).
    fn media_cache_root(&self) -> Option<PathBuf> {
        Some(self.mirri_home().join("provider-files"))
    }

    /// The Mirri home this factory roots its home-scoped consumers in.
    fn mirri_home(&self) -> PathBuf {
        self.mirri_home_from(|name| std::env::var_os(name))
    }

    /// The pure resolver behind [`Self::mirri_home`], parameterised over
    /// the env lookup (the `resolve_*_from` convention of
    /// `mirri-os/src/home.rs`) so the `MIRRICODE_HOME` → `$HOME` fallback
    /// chain is testable without mutating the process environment. The
    /// explicit `self.home` always wins (resolved by the caller, not the
    /// env); with nothing set the relative `.mirri-code` fallback applies.
    fn mirri_home_from(&self, get_env: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
        if let Some(home) = &self.home {
            home.clone()
        } else if let Some(mirri_home) = get_env("MIRRICODE_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(mirri_home)
        } else if let Some(home) = get_env("HOME") {
            PathBuf::from(home).join(".mirri-code")
        } else {
            PathBuf::from(".mirri-code")
        }
    }

    /// The credential store this factory consults, when one opened — the
    /// per-request paths (`model.generate`) re-resolve through the same
    /// store instead of rebuilding a factory per call.
    pub(crate) fn credentials(&self) -> Option<&CredentialStore> {
        self.credentials.as_ref()
    }

    /// The available-model listing appended to the not-configured error
    /// (R10/A11): rendered from the live config snapshot through the same
    /// catalog the dispatch tools and the subagent host use, so every
    /// rejection face names the same set of configured aliases.
    fn available_model_listing(&self) -> String {
        let catalog = crate::model_catalog::ConfigModelCatalog::new(Arc::clone(&self.config));
        mirri_core::render_model_listing(&catalog)
    }
}

impl crate::LlmFactory for ConfigAwareLlmFactory {
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        // Read the alias/provider registries off the config snapshot, then
        // drop the guard before any blocking work below.
        let (models, providers) = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            (
                snapshot.to_model_records_wire(),
                snapshot.to_provider_records_wire(),
            )
        };

        // Resolution order (the module docs): a known alias key first, then
        // the `provider/model` composite the web pickers send (C6 — matched
        // against each alias's own provider + model fields, mirroring the
        // TS daemon's models.dev-seeded registry where composites ARE alias
        // keys). Anything else — bare name or unresolvable composite — is
        // the R10/A11 not-configured error listing the available models:
        // no bare-name environment fallback ever answers (ADR-004), and a
        // drifted alias (deleted after the switch) fails loudly instead of
        // silently falling back to the config default (A15/ADR-005).
        match resolve_model_alias_in(&models, config.model_id.as_str())
            .and_then(|key| models.get(key.as_str()))
        {
            Some(alias) => self.create_llm_from_alias(config, alias, &providers),
            None => Err(format!(
                "model '{}' is not configured (no [models] alias or provider/model pair matches; \
                 add it to config.toml or pick a configured model). {}",
                config.model_id,
                self.available_model_listing(),
            )),
        }
    }

    /// The credential store file's `(mtime, len)` fingerprint — the
    /// file-level granularity the plan adjudicated over ADR-005's
    /// per-provider form (the resolver layer must not learn provider
    /// resolution to compute it). A missing store file (never written
    /// yet) fingerprints as 0, the same value as no store at all: no
    /// rebuild is forced until a credential actually lands on disk.
    fn credential_fingerprint(&self) -> u64 {
        let Some(store) = &self.credentials else {
            return 0;
        };
        let Ok(meta) = std::fs::metadata(store.path()) else {
            return 0;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        mtime.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ meta.len()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Read the provider's `customHeaders` map off its wire record: header
/// names keep their spelling verbatim, and a missing or non-map field
/// contributes no headers.
fn custom_headers_of(provider: &Value) -> Vec<(String, String)> {
    provider
        .get("customHeaders")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|value| (name.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Open the credential store rooted at `home` — the explicitly given home
/// when present, else the ambient Mirri home resolution (the same chain
/// [`ConfigAwareLlmFactory::mirri_home`] builds on). `None` when no home
/// resolves or the store cannot open: the resolution stays on the env
/// chain alone.
pub(crate) fn resolve_credential_store(home: Option<&Path>) -> Option<CredentialStore> {
    let home = match home {
        Some(home) => home.to_path_buf(),
        None => mirri_core::config::mirri_home_dir().ok()?,
    };
    CredentialStore::open(&home).ok()
}

/// Resolve a provider's credential through the full chain: the
/// credential store's active entry first (`key` → the value itself;
/// `external` → resolved through the method's CLI (Azure CLI today) at
/// call time — a failure propagates the guidance as the error, never a
/// env-chain fallback that would answer with the wrong key (V5);
/// `oauth` → the reserved D-M4 slot, not implemented), else the
/// provider's env chain (the `Ok(None)` case — no source answered).
/// Shared by [`ConfigAwareLlmFactory`] and the `model.generate`
/// per-request path so every request-building site resolves credentials
/// identically.
///
/// The `Err` face is the user-actionable guidance (e.g. "run `az login`
/// first") with the provider named — the callers surface it verbatim so
/// the user is told what to do, not which env var was missing.
///
/// The provider record's `apiKey` field is deliberately NOT consulted
/// (D-M3): after migration no key belongs in config.toml, and a
/// hand-written residual one must never silently bypass the store.
pub(crate) fn resolve_provider_credential(
    store: Option<&CredentialStore>,
    provider_id: &str,
    provider: &Value,
    provider_type: &str,
) -> Result<Option<String>, String> {
    let Some(store) = store else {
        return Ok(resolve_provider_env_chain(provider, provider_type));
    };
    let Some(record) = store.get(provider_id) else {
        return Ok(resolve_provider_env_chain(provider, provider_type));
    };
    match record.value {
        CredentialValue::Key { ref key } => Ok(Some(key.clone())),
        CredentialValue::External {
            ref method_id,
            ref metadata,
        } => match mirri_credentials::external::obtain_external(method_id, metadata) {
            // No fallback on failure (V5): a wrongly-scoped env key
            // answering an external-credential request is worse than the
            // failure — the guidance names the user's next action.
            Err(err) => Err(format!(
                "provider '{provider_id}' credential could not be resolved: {}",
                err.guidance
            )),
            Ok(token) => Ok(Some(token)),
        },
        CredentialValue::Oauth { .. } => Ok(None), // D-M4 reserved slot
    }
}

/// Resolve a provider wire record's API key through the env chain only —
/// the type's conventional key in the provider's `env` sub-table, which is
/// the user's explicit configuration written in config.toml. The shell
/// environment tier is retired (#186): the engine only uses explicitly
/// configured credentials, so the implicit `OPENAI_API_KEY` /
/// `ANTHROPIC_API_KEY` / ... fallbacks never answer. The config `apiKey`
/// field tier is retired as well (D-M3): residual hand-written keys must
/// not silently bypass the credential store.
pub(crate) fn resolve_provider_env_chain(provider: &Value, provider_type: &str) -> Option<String> {
    env_value_of(provider, api_key_env_name(provider_type))
}

/// The provider type the credential chain keys off — the resolved WIRE
/// protocol, not the raw provider type: a custom-type provider speaking
/// the openai wire resolves its credential through the openai conventional
/// key, so an env sub-table row written for the wire's convention answers.
/// Every credential-resolution caller (the factory chain, the generate
/// dispatch, and the catalog's `has_api_key` faces) must pass this — the
/// single normalization keeps the UI's "configured" verdict from drifting
/// from the request path's.
pub(crate) fn credential_type_for_wire<'a>(
    provider_type: &'a str,
    wire_protocol: &'a str,
) -> &'a str {
    if provider_type.eq_ignore_ascii_case("kimi") {
        "kimi"
    } else if provider_type.eq_ignore_ascii_case("custom") {
        wire_protocol
    } else {
        provider_type
    }
}

/// The provider's `env` sub-table entry for `key`: the file-level
/// `[providers.<id>.env]` map carried verbatim on the wire record (the
/// TS parity tier — `providerValue` in the TS provider manager). The
/// sub-table is read as a plain map: only the per-type conventional key
/// participates, keys keep their exact case-sensitive spelling, and
/// empty or whitespace-only values fall through to the next source.
pub(crate) fn env_value_of(provider: &Value, key: Option<&str>) -> Option<String> {
    let value = provider
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| key.and_then(|key| env.get(key)))
        .and_then(Value::as_str)?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The type's conventional API-key variable name — the single spelling
/// table shared by the `env` sub-table lookup and the shell fallback
/// ([`env_api_key`]); `None` for types without one (the shell path
/// errors as before).
fn api_key_env_name(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" | "openai_responses" => Some("OPENAI_API_KEY"),
        "google" | "google-genai" | "google_genai" => Some("GOOGLE_API_KEY"),
        "kimi" => Some("MIRRI_API_KEY"),
        _ => None,
    }
}

/// The type's conventional base-URL variable name — the single spelling
/// table shared by the `env` sub-table lookup (the shell tier is retired
/// with [`default_base_url`]); the default branch mirrors it (MIRRI_BASE_URL).
pub(crate) fn base_url_env_name(provider_type: &str) -> &'static str {
    match provider_type {
        "anthropic" => "ANTHROPIC_BASE_URL",
        "openai" | "openai_responses" => "OPENAI_BASE_URL",
        "google" | "google-genai" | "google_genai" => "GOOGLE_GEMINI_BASE_URL",
        _ => "MIRRI_BASE_URL",
    }
}

/// The first-party provider ids whose implicit official endpoint is
/// sanctioned (#184): a provider whose id IS one of the official services
/// may omit its base URL — picking `openai` without a URL is a legitimate
/// usage — while every other provider (models.dev imports, gateways,
/// hand-added entries) must configure one explicitly. `kimi` is the Mirri
/// first-party coding service (its official endpoint is
/// api.mirricode.com).
const FIRST_PARTY_PROVIDER_IDS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "google-genai",
    "google_genai",
    "kimi",
];

/// The first-party default base URL for `provider_id`, or `None` when the
/// provider is not a first party (the caller raises the clear
/// missing-base-url configuration error). The official endpoints mirror
/// the static defaults [`ProviderLlmFactory`] served for the env-driven
/// path.
pub(crate) fn first_party_default_base_url(provider_id: &str, provider_type: &str) -> Option<String> {
    if !FIRST_PARTY_PROVIDER_IDS.contains(&provider_id) {
        return None;
    }
    match provider_type {
        "anthropic" => Some("https://api.anthropic.com/v1".to_string()),
        "openai" | "openai_responses" => Some("https://api.openai.com/v1".to_string()),
        "google" | "google-genai" | "google_genai" => {
            Some("https://generativelanguage.googleapis.com/v1beta".to_string())
        }
        "kimi" => Some("https://api.mirricode.com/coding/v1".to_string()),
        _ => None,
    }
}


/// The type's API-key environment variable, or a clear error naming it —
/// the same spellings [`ProviderLlmFactory`] reads (its env-driven path;
/// the config-aware factory raises the explicit-credentials error
/// instead, #186). `#[allow(dead_code)]`: retained as the shared spelling
/// reference for the env-driven factory tier.
#[allow(dead_code)]
fn env_api_key(provider_type: &str) -> Result<String, String> {
    match api_key_env_name(provider_type) {
        Some(name) => std::env::var(name).map_err(|_| format!("{name} not set")),
        None => Err(format!(
            "unsupported provider type '{provider_type}' (no apiKey configured)"
        )),
    }
}

/// The explicit-credentials configuration error (#186): the engine only
/// uses credentials the user explicitly configured (the credential store
/// or the provider's `env` sub-table) — a shell environment variable is
/// never consulted, so a missing credential is this clear, actionable
/// error instead of an env-var-not-set face.
pub(crate) fn missing_credential_error(provider_id: &str) -> String {
    format!(
        "provider '{provider_id}' has no credential configured; please configure credentials for \
         provider '{provider_id}' (Settings → Providers → {provider_id}, or the provider's env \
         sub-table in config.toml)"
    )
}

/// An [`LlmResolver`] over the agent's live `AgentScopeConfig`: caches the
/// built provider and rebuilds it only when the configured `model_id` or
/// `thinking_effort` changes (the effort is baked into the adapter at build
/// time), so a mid-turn `agent.setModel` / `agent.setThinking` takes effect
/// on the next loop step's LLM call. A failed rebuild propagates the
/// factory's human-readable reason (the step fails loudly rather than
/// silently continuing on the old model).
///
/// Lock-order note: the `current` Mutex and the config read guard coexist
/// only inside the short critical sections of `resolve()` — the cache
/// check reads the config first, and `set_model` takes only the config
/// write lock, never `current`. There is no reverse hold, so no
/// lock-order deadlock.
pub struct ConfigResolvingLlm {
    config: Arc<RwLock<AgentScopeConfig>>,
    factory: Arc<dyn LlmFactory>,
    current: Mutex<Option<Resolved>>,
}

struct Resolved {
    model_id: String,
    /// The effort the adapter was built with — it participates in the
    /// cache key because the provider constructors bake the effort in;
    /// a same-model effort change must rebuild (a model change still
    /// rebuilds through `model_id`).
    thinking_effort: Option<ThinkingEffort>,
    /// The factory's credential fingerprint at build time — a rotated
    /// provider key must not keep serving from a stale cached adapter
    /// (A.4). File-level by design (ADR-005 deviation, adjudicated in the
    /// plan): the resolver knows nothing about providers, so the store
    /// file's `(mtime, len)` stands in for per-provider identity —
    /// conservative in the rebuild direction, never stale.
    credential_fingerprint: u64,
    llm: Arc<dyn Llm>,
}

impl ConfigResolvingLlm {
    pub fn new(config: Arc<RwLock<AgentScopeConfig>>, factory: Arc<dyn LlmFactory>) -> Self {
        Self {
            config,
            factory,
            current: Mutex::new(None),
        }
    }
}

impl LlmResolver for ConfigResolvingLlm {
    fn resolve(&self) -> Result<Arc<dyn Llm>, String> {
        self.resolve_with_alias().map(|(llm, _)| llm)
    }

    /// The alias-aware resolution (spec 11 A13): alongside the built
    /// provider, the configured `model_id` (M1 — an alias key or a
    /// `provider/model` composite) is reported as the step's attribution
    /// key, so the usage recording sites attribute to the alias instead
    /// of the raw upstream name (M2) the adapter reports.
    fn resolve_with_alias(&self) -> Result<(Arc<dyn Llm>, Option<String>), String> {
        let guard = self.config.read().unwrap_or_else(|e| e.into_inner());
        let credential_fingerprint = self.factory.credential_fingerprint();
        if let Some(resolved) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && resolved.model_id == guard.model_id
            && resolved.thinking_effort == guard.thinking_effort
            && resolved.credential_fingerprint == credential_fingerprint
        {
            return Ok((resolved.llm.clone(), Some(resolved.model_id.clone())));
        }
        let llm = self.factory.create_llm(&guard)?;
        let model_id = guard.model_id.clone();
        let thinking_effort = guard.thinking_effort.clone();
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(Resolved {
            model_id: model_id.clone(),
            thinking_effort,
            credential_fingerprint,
            llm: llm.clone(),
        });
        Ok((llm, Some(model_id)))
    }
}

#[cfg(test)]
use mirri_core::{ModelAlias, ProviderEntry};
/// The test config shape: one alias `ultra` pointing at the `rakuten`
/// provider, with the provider entry overridden per test.
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
fn alias_config_with_model(provider: ProviderEntry, model: &str) -> Arc<RwLock<ServerConfig>> {
    first_party_config_with_model("rakuten", provider, model)
}

#[cfg(test)]
fn alias_config(provider: ProviderEntry) -> Arc<RwLock<ServerConfig>> {
    alias_config_with_model(provider, "GLM-5.3-Coding")
}

#[cfg(test)]
fn first_party_config(provider_id: &str, provider: ProviderEntry) -> Arc<RwLock<ServerConfig>> {
    first_party_config_with_model(provider_id, provider, "GLM-5.3-Coding")
}

#[cfg(test)]
fn first_party_config_with_model(
    provider_id: &str,
    provider: ProviderEntry,
    model: &str,
) -> Arc<RwLock<ServerConfig>> {
    Arc::new(RwLock::new(ServerConfig {
        models: Some(HashMap::from([(
            "ultra".to_string(),
            ModelAlias {
                provider: provider_id.to_string(),
                model: model.to_string(),
                max_context_size: 128000,
                ..Default::default()
            },
        )])),
        providers: Some(HashMap::from([(provider_id.to_string(), provider)])),
        ..Default::default()
    }))
}

#[cfg(test)]
fn official_openai_config(provider: ProviderEntry) -> Arc<RwLock<ServerConfig>> {
    first_party_config("openai", provider)
}

#[cfg(test)]
fn official_openai_agent_config() -> AgentScopeConfig {
    first_party_agent_config("openai")
}

#[cfg(test)]
fn first_party_agent_config(provider_id: &str) -> AgentScopeConfig {
    use mirri_core::ProviderType;
    let _ = provider_id;
    AgentScopeConfig {
        provider_type: ProviderType::Anthropic,
        model_id: "ultra".to_string(),
        system_prompt: Some("You are a test assistant.".to_string()),
        thinking_effort: None,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use mirri_core::message::{ContentPart, ImageUrlData, Message, Role};
    use mirri_core::{
        CancelToken, EngineResult, Llm, LlmChatParams, LlmEventStream, LlmResolver,
        ModelCapability, ProviderType,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::*;
    use crate::LlmFactory;

    /// Given an agent config addressing the `ultra` alias (any provider
    /// type — the alias resolution ignores it in favor of the provider
    /// record's own type).
    fn alias_agent_config() -> AgentScopeConfig {
        AgentScopeConfig {
            provider_type: ProviderType::Anthropic,
            model_id: "ultra".to_string(),
            system_prompt: Some("You are a test assistant.".to_string()),
            thinking_effort: None,
            ..Default::default()
        }
    }

    #[test]
    fn should_read_provider_custom_headers_when_the_wire_record_carries_them() {
        // Given a provider wire record whose customHeaders map mixes a
        // gateway-required User-Agent with a non-string value
        let provider = serde_json::json!({
            "type": "openai",
            "customHeaders": { "User-Agent": "opencode/1.18.19", "X-Empty": null }
        });

        // When the headers are read off the record
        let headers = custom_headers_of(&provider);

        // Then the string value is kept verbatim and the non-string dropped
        assert_eq!(
            headers,
            vec![("User-Agent".to_string(), "opencode/1.18.19".to_string())]
        );
    }

    #[test]
    fn should_contribute_no_headers_when_the_provider_record_has_no_custom_headers() {
        // Given a provider wire record without customHeaders
        let provider = serde_json::json!({ "type": "openai" });

        // When the headers are read off the record
        let headers = custom_headers_of(&provider);

        // Then none are contributed
        assert!(headers.is_empty());
    }

    #[test]
    fn should_create_llm_from_the_configured_provider_when_model_id_is_a_known_alias() {
        // Given a config whose `ultra` alias resolves through a provider
        // with an apiKey and baseUrl
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                base_url: Some("https://proxy.example.com".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created from the configured provider");

        // Then the adapter carries the alias's upstream model and the
        // agent's system prompt
        assert_eq!(llm.model_name(), "GLM-5.3-Coding");
        assert_eq!(llm.system_prompt(), "You are a test assistant.");
    }

    #[test]
    fn should_inject_name_inferred_media_capability_when_the_alias_names_a_vision_model() {
        // Given a config whose `ultra` alias names a vision-capable upstream
        // model (the name-inference catalog) with no declared capabilities
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config_with_model(
                ProviderEntry {
                    r#type: Some("openai".to_string()),
                    env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                    base_url: Some("https://proxy.example.com".to_string()),
                    ..Default::default()
                },
                "gpt-4o",
            ),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created");

        // Then the adapter carries the resolved capability: name inference
        // marks image support
        assert!(llm.capability().image_in);
    }

    #[test]
    fn should_keep_kimi_video_capability_when_the_factory_merges_resolved_capability() {
        // Given a config whose `ultra` alias resolves through the kimi
        // provider for a model no source can resolve (the provider id
        // `kimi` is the Mirri first-party coding service, so its official
        // endpoint answers without an explicit base URL)
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            first_party_config_with_model(
                "kimi",
                ProviderEntry {
                    r#type: Some("kimi".to_string()),
                    env: Some(HashMap::from([("MIRRI_API_KEY".to_string(), "kimi-test".to_string())])),
                    ..Default::default()
                },
                "kimi-latest",
            ),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created");

        // Then the intrinsic video capability survives the merge (catalog
        // resolution merges into the intrinsic set, OR), with image support
        // unset — no source claimed it (ADR-003 revision: unresolved
        // modalities default to unsupported)
        assert!(llm.capability().video_in, "kimi accepts video input");
        assert!(!llm.capability().image_in);
    }

    #[test]
    fn should_propagate_the_external_failure_guidance_when_creating_the_llm() {
        // Given a factory whose alias resolves through a provider whose
        // credential store entry is an external source that fails (no
        // resource_name → guidance), while the shell carries a usable key
        // the creation must NOT fall back to
        let home = tempfile::tempdir().expect("tempdir");
        let store = mirri_credentials::CredentialStore::open(home.path()).expect("store opens");
        store
            .set(
                "rakuten",
                mirri_credentials::CredentialValue::External {
                    method_id: "azure-cli".into(),
                    metadata: Default::default(),
                },
            )
            .expect("store set");

        // When an LLM is created for the alias (capture-and-restore env
        // hygiene)
        let result = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("OPENAI_API_KEY").ok();
            unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env") };
            let factory = ConfigAwareLlmFactory::new(
                alias_config(ProviderEntry {
                    r#type: Some("openai".to_string()),
                    ..Default::default()
                }),
                Some(home.path().to_path_buf()),
            );
            let result = factory.create_llm(&alias_agent_config());
            // env_lock: restore under the workspace env lock guard above
            match prior {
                Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
                None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
            }
            result
        };

        // Then the creation failed with the guidance — the provider is
        // named, the action text is there, and never the env-var face
        let err = result.err().expect("the external failure must fail creation");
        assert!(err.contains("rakuten"), "the provider is named: {err}");
        assert!(
            err.contains("resource_name") || err.contains("az login"),
            "the guidance names the next action: {err}"
        );
        assert!(
            !err.contains("OPENAI_API_KEY not set"),
            "never the env-var face: {err}"
        );
    }

    #[test]
    fn should_error_when_an_alias_provider_has_only_a_shell_env_api_key() {
        // Given a config whose alias resolves through a provider with no
        // configured credential, and the type's env var set — the shell
        // variable must NOT answer (#186: explicit credentials only)
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "env-key") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("anthropic".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let err = factory
            .create_llm(&alias_agent_config())
            .err()
            .expect("the shell env var must not answer");

        // Then the failure is the explicit-credentials configuration error
        assert!(
            err.contains("rakuten") && err.contains("credential"),
            "the error names the provider and the missing credential: {err}"
        );
        assert!(
            !err.contains("ANTHROPIC_API_KEY"),
            "never the env-var face: {err}"
        );
    }

    // -- the `env` sub-table fallback tier (apiKey / baseUrl chains) ------

    /// Mount a mock Kimi (OpenAI-shaped) chat-completions endpoint on
    /// `server`.
    async fn mount_kimi_chat(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(server)
            .await;
    }

    /// Read a request header value off a recorded wiremock request (the
    /// wiremock 0.6 header map is the single-value `http::HeaderMap`).
    fn request_header(request: &wiremock::Request, name: &str) -> String {
        request
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    /// A minimal one-message chat params for the credential-flow tests.
    fn simple_chat_params() -> LlmChatParams {
        LlmChatParams {
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        }
    }

    /// Drain a chat stream to EOF so the spawned HTTP request completes
    /// and the mock server records it.
    async fn drain_chat(mut stream: LlmEventStream) {
        use futures::StreamExt;
        while let Some(_event) = stream.next().await {}
    }

    #[test]
    fn should_error_naming_the_provider_when_a_third_party_provider_has_no_base_url() {
        // Given a config whose alias resolves through a non-first-party
        // provider with no baseUrl and no env sub-table base-URL entry (a
        // third-party provider must never fall back to the official
        // OpenAI endpoint — the #184 silent wrong-provider request face)
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("OPENAI_BASE_URL") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let err = factory
            .create_llm(&alias_agent_config())
            .err()
            .expect("creation must fail without a base URL");

        // Then the error names the provider and the remedy — never a silent
        // request to api.openai.com
        assert!(
            err.contains("rakuten") && err.contains("base URL"),
            "the error names the provider and the missing base URL: {err}"
        );
        assert!(
            !err.contains("api.openai.com"),
            "never the silent official-endpoint face: {err}"
        );
    }

    #[test]
    fn should_create_llm_with_the_default_base_url_when_the_provider_is_official_openai() {
        // Given a config whose alias resolves through the first-party
        // `openai` provider with no baseUrl (the sanctioned bare usage —
        // picking OpenAI without a URL is legitimate)
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("OPENAI_BASE_URL") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            official_openai_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let llm = factory
            .create_llm(&official_openai_agent_config())
            .expect("the official openai provider keeps its default base URL");

        // Then the adapter was built (the creation is a Result<String> —
        // the Ok itself proves the default answered instead of an error)
        assert_eq!(llm.model_name(), "GLM-5.3-Coding");
    }

    #[test]
    fn should_create_llm_with_the_default_base_url_when_the_provider_is_official_anthropic_or_google()
    {
        // Given configs whose aliases resolve through the first-party
        // `anthropic` and `google-genai` providers with no baseUrl
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("ANTHROPIC_BASE_URL") };
        unsafe { std::env::remove_var("GOOGLE_GEMINI_BASE_URL") };
        for (provider_id, provider_type, base_env) in [
            ("anthropic", "anthropic", "ANTHROPIC_BASE_URL"),
            ("google-genai", "google-genai", "GOOGLE_GEMINI_BASE_URL"),
        ] {
            let mut provider = ProviderEntry {
                r#type: Some(provider_type.to_string()),
                ..Default::default()
            };
            if provider_type == "anthropic" {
                provider.env = Some(HashMap::from([(
                    "ANTHROPIC_API_KEY".to_string(),
                    "sk-test".to_string(),
                )]));
            } else {
                provider.env = Some(HashMap::from([(
                    "GOOGLE_API_KEY".to_string(),
                    "sk-test".to_string(),
                )]));
            }
            let _ = base_env;
            let home = tempfile::tempdir().expect("tempdir");
            let factory = ConfigAwareLlmFactory::new(
                first_party_config(provider_id, provider),
                Some(home.path().to_path_buf()),
            );

            // When an LLM is created for the alias
            let llm = factory
                .create_llm(&first_party_agent_config(provider_id))
                .unwrap_or_else(|e| panic!("the official {provider_id} provider keeps its default base URL: {e}"));

            // Then the adapter was built
            assert_eq!(llm.model_name(), "GLM-5.3-Coding");
        }
    }

    #[test]
    fn should_keep_the_env_sub_table_base_url_tier_for_third_party_providers() {
        // Given a third-party provider with no baseUrl field but an env
        // sub-table entry carrying the conventional base URL key (the user's
        // explicit config tier must keep answering)
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("OPENAI_BASE_URL") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([
                    ("OPENAI_API_KEY".to_string(), "sk-test".to_string()),
                    ("OPENAI_BASE_URL".to_string(), "https://gw.example.test/v1".to_string()),
                ])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("the env sub-table base URL answers");

        // Then the adapter was built
        assert_eq!(llm.model_name(), "GLM-5.3-Coding");
    }

    // -- TDD #184/#186 red tests --------------------------------------------

    #[test]
    fn should_error_with_a_configuration_error_when_only_the_shell_env_var_holds_the_api_key() {
        // Given a provider with no credential-store record, no env
        // sub-table entry, but the shell OPENAI_API_KEY set (the implicit
        // fallback #186 removes — the engine must only use explicitly
        // configured credentials)
        let home = tempfile::tempdir().expect("tempdir");
        // (no store entry, no config env tier)
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                base_url: Some("https://gw.example.test/v1".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created with the shell variable set
        let _guard = mirri_test_support::env::lock();
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-shell") };
        let result = factory.create_llm(&alias_agent_config());
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }

        // Then the creation failed with the explicit-configuration error —
        // the shell variable must not answer
        let err = result.err().expect("the shell env var must not answer");
        assert!(
            err.contains("rakuten"),
            "the error names the provider: {err}"
        );
        assert!(
            err.to_lowercase().contains("credential") || err.contains("API key"),
            "the error names the missing credential: {err}"
        );
        assert!(
            !err.contains("OPENAI_API_KEY not set"),
            "never the env-var face: {err}"
        );
    }

    #[test]
    fn should_error_when_an_alias_provider_has_no_configured_api_key_at_all() {
        // Given a provider with no store entry, no env sub-table entry, and
        // the shell variables cleared — the old chain would have failed
        // with the "OPENAI_API_KEY not set" face; the new chain must name
        // the provider and point at the settings
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        unsafe { std::env::remove_var("MIRRI_API_KEY") };
        unsafe { std::env::remove_var("GOOGLE_API_KEY") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                base_url: Some("https://gw.example.test/v1".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias
        let err = factory
            .create_llm(&alias_agent_config())
            .err()
            .expect("creation must fail without credentials");

        // Then the error names the provider and the settings remedy
        assert!(
            err.contains("rakuten") && err.contains("provider"),
            "the error names the provider: {err}"
        );
        assert!(
            !err.contains("OPENAI_API_KEY not set"),
            "never the env-var face: {err}"
        );
    }

    #[tokio::test]
    async fn should_use_the_env_sub_table_api_key_when_the_api_key_field_is_absent() {
        // Given a mock Kimi endpoint, and a provider whose api key lives
        // only in its `env` sub-table (the file-level
        // `[providers.<id>.env]` tier), with a conflicting value in the
        // shell variable
        let server = MockServer::start().await;
        mount_kimi_chat(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("kimi".to_string()),
                base_url: Some(format!("{}/v1", server.uri())),
                env: Some(HashMap::from([(
                    "MIRRI_API_KEY".to_string(),
                    "table-key".to_string(),
                )])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias and chats — the shell
        // variable is set to a conflicting value so the assertion pins the
        // sub-table-over-shell precedence, and restored afterwards so a
        // developer's pre-existing variable survives the test
        let llm = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("MIRRI_API_KEY").ok();
            unsafe { std::env::set_var("MIRRI_API_KEY", "shell-key") };
            let llm = factory
                .create_llm(&alias_agent_config())
                .expect("llm is created from the env sub-table api key");
            match prior {
                Some(value) => unsafe { std::env::set_var("MIRRI_API_KEY", value) },
                None => unsafe { std::env::remove_var("MIRRI_API_KEY") },
            }
            llm
        };
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the request carried the sub-table key, not the shell
        // variable's
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        assert_eq!(
            request_header(&requests[0], "authorization"),
            "Bearer table-key"
        );
    }

    #[tokio::test]
    async fn should_prefer_a_credential_store_entry_over_the_env_sub_table() {
        // Given a mock Kimi endpoint, a credential store holding the
        // provider's key, and a provider whose `env` sub-table carries a
        // conflicting conventional key (the store-over-env precedence —
        // the config `apiKey` field tier is retired, D-M3)
        let server = MockServer::start().await;
        mount_kimi_chat(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let store = mirri_credentials::CredentialStore::open(home.path()).expect("store opens");
        store
            .set(
                "rakuten",
                mirri_credentials::CredentialValue::Key { key: "store-key".into() },
            )
            .expect("store set");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("kimi".to_string()),
                base_url: Some(format!("{}/v1", server.uri())),
                env: Some(HashMap::from([(
                    "MIRRI_API_KEY".to_string(),
                    "table-key".to_string(),
                )])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias and chats
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created");
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the request carried the store's key, not the env chain's
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        assert_eq!(
            request_header(&requests[0], "authorization"),
            "Bearer store-key"
        );
    }
    #[tokio::test]
    async fn should_error_when_the_shell_env_var_is_the_only_credential_source() {
        // Given a mock Kimi endpoint, and a provider with no `apiKey`
        // field and an `env` sub-table holding only an unrelated key, but
        // the type's shell variable set — the shell variable must NOT
        // answer (#186: explicit credentials only, so no request leaves)
        let server = MockServer::start().await;
        mount_kimi_chat(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("kimi".to_string()),
                base_url: Some(format!("{}/v1", server.uri())),
                env: Some(HashMap::from([(
                    "UNRELATED_FLAG".to_string(),
                    "1".to_string(),
                )])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias — the shell variable is set
        // with the prior value captured and restored so a developer's
        // pre-existing variable survives the test
        let result = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("MIRRI_API_KEY").ok();
            unsafe { std::env::set_var("MIRRI_API_KEY", "shell-key") };
            let result = factory.create_llm(&alias_agent_config());
            match prior {
                Some(value) => unsafe { std::env::set_var("MIRRI_API_KEY", value) },
                None => unsafe { std::env::remove_var("MIRRI_API_KEY") },
            }
            result
        };

        // Then the creation failed with the explicit-credentials error and
        // no request ever reached the mock
        let err = result.err().expect("the shell env var must not answer");
        assert!(
            err.contains("rakuten") && err.contains("credential"),
            "the error names the provider and the missing credential: {err}"
        );
        assert!(!err.contains("MIRRI_API_KEY"), "never the env-var face: {err}");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(
            requests.len(),
            0,
            "no request may leave when the credential is not explicitly configured"
        );
    }

    #[tokio::test]
    async fn should_use_the_env_sub_table_base_url_when_the_base_url_field_is_absent() {
        // Given two mock Kimi endpoints — the shell MIRRI_BASE_URL routing
        // to one, the provider's `env` sub-table entry to the other — and
        // a provider with no `baseUrl` field
        let shell_server = MockServer::start().await;
        mount_kimi_chat(&shell_server).await;
        let table_server = MockServer::start().await;
        mount_kimi_chat(&table_server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("kimi".to_string()),
                env: Some(HashMap::from([
                    ("MIRRI_API_KEY".to_string(), "field-key".to_string()),
                    (
                        "MIRRI_BASE_URL".to_string(),
                        format!("{}/v1", table_server.uri()),
                    ),
                ])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias and chats — the shell
        // variable is set with the prior value captured and restored so a
        // developer's pre-existing variable survives the test
        let llm = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("MIRRI_BASE_URL").ok();
            unsafe { std::env::set_var("MIRRI_BASE_URL", format!("{}/v1", shell_server.uri())) };
            let llm = factory
                .create_llm(&alias_agent_config())
                .expect("llm is created");
            match prior {
                Some(value) => unsafe { std::env::set_var("MIRRI_BASE_URL", value) },
                None => unsafe { std::env::remove_var("MIRRI_BASE_URL") },
            }
            llm
        };
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the request reached the sub-table's endpoint, never the
        // shell one
        let table_requests = table_server.received_requests().await.expect("requests");
        assert_eq!(table_requests.len(), 1);
        let shell_requests = shell_server.received_requests().await.expect("requests");
        assert_eq!(
            shell_requests.len(),
            0,
            "the shell base url must not answer when the sub-table carries one"
        );
    }

    #[tokio::test]
    async fn should_prefer_the_base_url_field_over_the_env_sub_table() {
        // Given two mock Kimi endpoints — the provider's `baseUrl` field
        // routing to one, the `env` sub-table entry for MIRRI_BASE_URL to
        // the other — with the shell variable unset
        let field_server = MockServer::start().await;
        mount_kimi_chat(&field_server).await;
        let table_server = MockServer::start().await;
        mount_kimi_chat(&table_server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("kimi".to_string()),
                base_url: Some(format!("{}/v1", field_server.uri())),
                env: Some(HashMap::from([
                    ("MIRRI_API_KEY".to_string(), "field-key".to_string()),
                    (
                        "MIRRI_BASE_URL".to_string(),
                        format!("{}/v1", table_server.uri()),
                    ),
                ])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias and chats
        let llm = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("MIRRI_BASE_URL").ok();
            unsafe { std::env::remove_var("MIRRI_BASE_URL") };
            let llm = factory
                .create_llm(&alias_agent_config())
                .expect("llm is created");
            match prior {
                Some(value) => unsafe { std::env::set_var("MIRRI_BASE_URL", value) },
                None => unsafe { std::env::remove_var("MIRRI_BASE_URL") },
            }
            llm
        };
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the request reached the field's endpoint, never the
        // sub-table's
        let field_requests = field_server.received_requests().await.expect("requests");
        assert_eq!(field_requests.len(), 1);
        let table_requests = table_server.received_requests().await.expect("requests");
        assert_eq!(
            table_requests.len(),
            0,
            "the env sub-table base url must not answer when the field carries one"
        );
    }

    #[tokio::test]
    async fn should_ignore_empty_env_sub_table_values_and_fall_through_to_the_next_source() {
        // Given a mock Kimi endpoint serving as the shell MIRRI_BASE_URL
        // target, and a provider whose `env` sub-table carries a real api
        // key but an empty base URL — the empty value falls through the
        // sub-table tier; with the shell routing retired (#184/#186) the
        // fall-through lands on the explicit-config source that remains
        let shell_server = MockServer::start().await;
        mount_kimi_chat(&shell_server).await;
        let table_server = MockServer::start().await;
        mount_kimi_chat(&table_server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            first_party_config(
                "kimi",
                ProviderEntry {
                    r#type: Some("kimi".to_string()),
                    env: Some(HashMap::from([
                        ("MIRRI_API_KEY".to_string(), "table-key".to_string()),
                        ("MIRRI_BASE_URL".to_string(), String::new()),
                    ])),
                    ..Default::default()
                },
            ),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias — the shell variables are
        // set with the prior values captured and restored so a developer's
        // pre-existing variables survive the test. The empty sub-table base
        // URL falls through PAST the shell tier (retired for credentials'
        // consistency: the first-party default wins for a first-party
        // provider), and the non-empty sub-table api key answers.
        let llm = {
            let _guard = mirri_test_support::env::lock();
            let prior_api_key = std::env::var("MIRRI_API_KEY").ok();
            let prior_base_url = std::env::var("MIRRI_BASE_URL").ok();
            unsafe { std::env::remove_var("MIRRI_API_KEY") };
            unsafe { std::env::set_var("MIRRI_BASE_URL", format!("{}/v1", shell_server.uri())) };
            let llm = factory
                .create_llm(&first_party_agent_config("kimi"))
                .expect("llm is created from the sub-table api key");
            match prior_api_key {
                Some(value) => unsafe { std::env::set_var("MIRRI_API_KEY", value) },
                None => unsafe { std::env::remove_var("MIRRI_API_KEY") },
            }
            // env_lock: restore under the workspace env lock guard above
            match prior_base_url {
                Some(value) => unsafe { std::env::set_var("MIRRI_BASE_URL", value) },
                None => unsafe { std::env::remove_var("MIRRI_BASE_URL") },
            }
            llm
        };
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the empty base URL fell through to the first-party default
        // (never the shell's endpoint — the shell tier no longer routes
        // provider traffic) while the non-empty api key answered from the
        // sub-table
        let requests = shell_server.received_requests().await.expect("requests");
        assert_eq!(
            requests.len(),
            0,
            "the shell base-url endpoint must not answer (the first-party default won)"
        );

        // And when the sub-table api key is whitespace-only instead, with
        // the credential store holding the key and the sub-table carrying
        // the base URL — the whitespace value is ignored and the store
        // answers (the explicit tiers)
        let home = tempfile::tempdir().expect("tempdir");
        let store = mirri_credentials::CredentialStore::open(home.path()).expect("store opens");
        store
            .set(
                "kimi",
                mirri_credentials::CredentialValue::Key { key: "store-key".into() },
            )
            .expect("store set");
        let factory = ConfigAwareLlmFactory::new(
            first_party_config(
                "kimi",
                ProviderEntry {
                    r#type: Some("kimi".to_string()),
                    env: Some(HashMap::from([
                        ("MIRRI_API_KEY".to_string(), "   ".to_string()),
                        (
                            "MIRRI_BASE_URL".to_string(),
                            format!("{}/v1", table_server.uri()),
                        ),
                    ])),
                    ..Default::default()
                },
            ),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the alias and chats — same
        // capture-and-restore hygiene for the shell key
        let llm = {
            let _guard = mirri_test_support::env::lock();
            let prior = std::env::var("MIRRI_API_KEY").ok();
            unsafe { std::env::set_var("MIRRI_API_KEY", "shell-key") };
            let llm = factory
                .create_llm(&first_party_agent_config("kimi"))
                .expect("llm is created");
            match prior {
                Some(value) => unsafe { std::env::set_var("MIRRI_API_KEY", value) },
                None => unsafe { std::env::remove_var("MIRRI_API_KEY") },
            }
            llm
        };
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the whitespace value was ignored (the store's key answered
        // — never the shell's) and the sub-table's base URL routed the
        // request
        let table_requests = table_server.received_requests().await.expect("requests");
        assert_eq!(table_requests.len(), 1);
        assert_eq!(
            request_header(&table_requests[0], "authorization"),
            "Bearer store-key"
        );
        assert_eq!(
            shell_server
                .received_requests()
                .await
                .expect("requests")
                .len(),
            0,
            "the shell-routed endpoint never answers (its tier is retired)"
        );
    }

    #[test]
    fn should_reject_unknown_bare_model_name_with_available_list() {
        // Given a config that knows the `ultra` alias and the Anthropic
        // env var set (the legacy bare-name path's credential source — its
        // presence proves a silent fallback would have answered), but an
        // agent addressing a bare model name that is no configured alias
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "env-key") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );
        let agent_config = AgentScopeConfig {
            provider_type: ProviderType::Anthropic,
            model_id: "claude-test".to_string(),
            ..Default::default()
        };

        // When an LLM is created for the unknown bare model name
        let err = factory
            .create_llm(&agent_config)
            .err()
            .expect("creation fails on an unknown bare model name");

        // Then the failure names the model, lists the configured aliases,
        // and never constructs an env-based LLM (ADR-004: the bare-name
        // environment fallback is gone)
        assert!(
            err.contains("model 'claude-test' is not configured"),
            "got: {err}"
        );
        assert!(
            err.contains("ultra"),
            "the available list must name the configured alias: {err}"
        );
        assert!(!err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }

    #[test]
    fn should_accept_canonical_composite_id_resolving_to_alias() {
        // Given a config whose `ultra` alias names the rakuten provider's
        // GLM-5.3-Coding, with the credentials carried in the provider
        // record, and the Anthropic env var set (a bare-name env fallback
        // would answer a wrong-provider LLM for the composite id)
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "env-key") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                base_url: Some("https://proxy.example.com".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the canonical composite id (the C6
        // composition of the alias's own provider + model pair — NOT an
        // alias key of this config)
        let mut agent_config = alias_agent_config();
        agent_config.model_id = "rakuten/GLM-5.3-Coding".to_string();
        let llm = factory
            .create_llm(&agent_config)
            .expect("llm is created from the configured provider");

        // Then the alias's configured provider answered (the adapter
        // carries the alias's upstream model, not the composite id a
        // wrong-provider env fallback would have echoed)
        assert_eq!(llm.model_name(), "GLM-5.3-Coding");
        assert_eq!(llm.system_prompt(), "You are a test assistant.");
    }

    #[test]
    fn should_create_llm_from_the_configured_provider_when_model_id_is_a_provider_model_composite()
    {
        // Given a config whose `ultra` alias names the rakuten provider's
        // GLM-5.3-Coding, with the credentials carried in the provider
        // record (so no environment variable is consulted)
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                base_url: Some("https://proxy.example.com".to_string()),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When an LLM is created for the web picker's composite id
        // (`rakuten/GLM-5.3-Coding` — the C6 id composed from the alias's
        // own provider + model pair, not an alias key of this config)
        let mut agent_config = alias_agent_config();
        agent_config.model_id = "rakuten/GLM-5.3-Coding".to_string();
        let llm = factory
            .create_llm(&agent_config)
            .expect("llm is created from the configured provider");

        // Then the adapter carries the alias's upstream model and the
        // agent's system prompt
        assert_eq!(llm.model_name(), "GLM-5.3-Coding");
        assert_eq!(llm.system_prompt(), "You are a test assistant.");
    }

    #[test]
    fn should_answer_a_clear_not_configured_error_when_a_composite_resolves_to_no_alias() {
        // Given the env-based fallback vars cleared (the dead-end must not
        // answer) and a config whose aliases name no rakuten Ghost model
        let _guard = mirri_test_support::env::lock();
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([("OPENAI_API_KEY".to_string(), "sk-test".to_string())])),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );
        let mut agent_config = alias_agent_config();
        agent_config.model_id = "rakuten/Ghost-9".to_string();

        // When an LLM is created for the unresolvable composite
        let err = factory
            .create_llm(&agent_config)
            .err()
            .expect("creation fails on an unresolvable composite");

        // Then the failure names the model and the config remedy — never
        // the Anthropic env dead-end
        assert!(
            err.contains("model 'rakuten/Ghost-9' is not configured"),
            "got: {err}"
        );
        assert!(!err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }

    // -- T11a: the provider-level mediaFileRefs switch (adjudication D2) ----

    /// Mount a mock Anthropic files + messages pair on `server`.
    async fn mount_anthropic_media_pair(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "file-x", "type": "file" })),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(server)
            .await;
    }

    /// An image-carrying params for the materialization wiring tests.
    fn image_params() -> LlmChatParams {
        LlmChatParams {
            messages: vec![Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                        detail: None,
                        id: None,
                    },
                    detail: None,
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }],
            tools: Vec::new(),
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        }
    }

    #[tokio::test]
    async fn should_build_the_media_channel_when_media_file_refs_is_left_enabled() {
        // Given a mock Anthropic endpoint and a config whose alias resolves
        // through an anthropic provider that does not set mediaFileRefs
        // (adjudication D2: default true)
        let server = MockServer::start().await;
        mount_anthropic_media_pair(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("anthropic".to_string()),
                env: Some(HashMap::from([("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string())])),
                base_url: Some(format!("{}/v1", server.uri())),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When the LLM is created and an image-carrying params is
        // materialized through the trait hook
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created");
        let mut params = image_params();
        llm.materialize_media(&mut params)
            .await
            .expect("materialize succeeds");

        // Then the image was uploaded through the files channel rooted at
        // the injected home's provider-files/ directory
        let uploads = server
            .received_requests()
            .await
            .expect("recorded requests")
            .iter()
            .filter(|request| request.url.path() == "/v1/files")
            .count();
        assert_eq!(uploads, 1);
        assert!(home.path().join("provider-files").exists());
    }

    #[tokio::test]
    async fn should_stay_fully_inline_when_an_anthropic_alias_disables_media_file_refs() {
        // Given a mock Anthropic endpoint and a config whose anthropic
        // provider sets mediaFileRefs = false (the D2 opt-out)
        let server = MockServer::start().await;
        mount_anthropic_media_pair(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config(ProviderEntry {
                r#type: Some("anthropic".to_string()),
                env: Some(HashMap::from([("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string())])),
                base_url: Some(format!("{}/v1", server.uri())),
                media_file_refs: Some(false),
                ..Default::default()
            }),
            Some(home.path().to_path_buf()),
        );

        // When the LLM is created and an image-carrying params is
        // materialized through the trait hook
        let llm = factory
            .create_llm(&alias_agent_config())
            .expect("llm is created");
        let mut params = image_params();
        llm.materialize_media(&mut params)
            .await
            .expect("materialize succeeds");

        // Then no upload happened and the image stayed inline
        let uploads = server
            .received_requests()
            .await
            .expect("recorded requests")
            .iter()
            .filter(|request| request.url.path() == "/v1/files")
            .count();
        assert_eq!(uploads, 0);
        let ContentPart::ImageUrl { image_url, .. } = &params.messages[0].content[0] else {
            panic!("image part expected");
        };
        assert!(image_url.url.starts_with("data:"), "url: {}", image_url.url);
    }

    // -- ConfigResolvingLlm: switch-aware caching over the live config (T6) --

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stub [`Llm`] echoing the model it was built for — the same
    /// hold-and-return shape as the provider adapters: `model_name()` and
    /// `capability()` return references into the struct.
    struct StubLlm {
        model_name: String,
        capability: ModelCapability,
    }

    impl Llm for StubLlm {
        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            &self.model_name
        }

        fn capability(&self) -> &ModelCapability {
            &self.capability
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            unimplemented!()
        }
    }

    /// A counting [`LlmFactory`] test double: records every `create_llm`
    /// call and serves a [`StubLlm`] echoing the `config.model_id` it saw.
    /// `fail_on` makes creation fail verbatim for one specific model id.
    #[derive(Default)]
    struct CountingLlmFactory {
        create_calls: AtomicUsize,
        fail_on: Option<String>,
    }

    impl CountingLlmFactory {
        fn create_calls(&self) -> usize {
            self.create_calls.load(Ordering::Relaxed)
        }
    }

    impl LlmFactory for CountingLlmFactory {
        fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            self.create_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_on.as_deref() == Some(config.model_id.as_str()) {
                return Err(format!("model '{}' cannot be created", config.model_id));
            }
            Ok(Arc::new(StubLlm {
                model_name: config.model_id.clone(),
                capability: ModelCapability::default(),
            }))
        }
    }

    /// A minimal agent config carrying just the given model id.
    fn test_agent_config(model_id: &str) -> AgentScopeConfig {
        AgentScopeConfig {
            model_id: model_id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn should_rebuild_only_when_the_configured_model_changes() {
        // Given a resolver over a live agent config and a counting factory
        // (recording every create_llm call and serving a stub per
        // config.model_id)
        let config: Arc<std::sync::RwLock<AgentScopeConfig>> =
            Arc::new(std::sync::RwLock::new(test_agent_config("model-a")));
        let factory = Arc::new(CountingLlmFactory::default());
        let resolver = ConfigResolvingLlm::new(config.clone(), factory.clone());
        // When resolved twice without a config change
        let a = resolver.resolve().unwrap();
        let b = resolver.resolve().unwrap();
        // Then the instance is cached and the factory ran once
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(factory.create_calls(), 1);
        // When the configured model then changes
        config.write().unwrap().model_id = "model-b".to_string();
        let c = resolver.resolve().unwrap();
        // Then a new instance is built for the new model
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(factory.create_calls(), 2);
        assert_eq!(c.model_name(), "model-b");
    }

    #[test]
    fn should_rebuild_the_llm_when_only_the_thinking_effort_changes() {
        // Given a resolver over a live agent config and a counting factory
        let config: Arc<std::sync::RwLock<AgentScopeConfig>> =
            Arc::new(std::sync::RwLock::new(test_agent_config("model-a")));
        let factory = Arc::new(CountingLlmFactory::default());
        let resolver = ConfigResolvingLlm::new(config.clone(), factory.clone());
        let a = resolver.resolve().unwrap();
        assert_eq!(factory.create_calls(), 1);
        // When only the thinking effort changes (same model)
        config.write().unwrap().thinking_effort = Some(ThinkingEffort::Custom("max".to_string()));
        let b = resolver.resolve().unwrap();
        // Then a new instance is built — the effort is baked into the adapter,
        // so it must participate in the cache key
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(factory.create_calls(), 2);
    }

    #[test]
    fn should_propagate_the_factory_reason_when_creation_fails() {
        // Given a resolver whose factory fails for model-b (model-a builds
        // fine and is cached first)
        let config: Arc<std::sync::RwLock<AgentScopeConfig>> =
            Arc::new(std::sync::RwLock::new(test_agent_config("model-a")));
        let factory = Arc::new(CountingLlmFactory {
            fail_on: Some("model-b".to_string()),
            ..CountingLlmFactory::default()
        });
        let resolver = ConfigResolvingLlm::new(config.clone(), factory.clone());
        resolver.resolve().expect("model-a resolves");
        assert_eq!(factory.create_calls(), 1);
        // When resolved after switching to the failing model
        config.write().unwrap().model_id = "model-b".to_string();
        let err = resolver
            .resolve()
            .err()
            .expect("resolve fails after switching to the failing model");
        // Then the factory's reason surfaces verbatim — the step fails
        // loudly instead of silently continuing on the cached instance
        assert_eq!(err, "model 'model-b' cannot be created");
        assert!(err.contains("model-b"));
        assert_eq!(factory.create_calls(), 2);
    }

    // ===== Injectable home resolution (16-test-execution-strategy §4.2:
    // production HOME reads reachable from lib test targets expose a
    // `resolve_*_from` variant so tests pass the value instead of pinning
    // the process env) =====

    /// Given the home fallback chain, when it is resolved from injected env
    /// lookups, then the tiers apply in order — explicit home wins,
    /// `MIRRICODE_HOME` beats `$HOME`, and `$HOME` joins `.mirri-code` —
    /// with no process-env mutation involved.
    #[test]
    fn should_apply_the_home_fallback_tiers_when_resolved_from_injected_env() {
        let explicit = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            Arc::new(std::sync::RwLock::new(ServerConfig::default())),
            Some(explicit.path().to_path_buf()),
        );
        assert_eq!(factory.mirri_home(), explicit.path());

        // The no-home arm is built as a literal on purpose: this test only
        // exercises `mirri_home_from`, and `new(.., None)` would resolve the
        // AMBIENT home to open its credential store — creating
        // `<real home>/.mirri-code/credentials` on every run.
        let no_home_factory = ConfigAwareLlmFactory {
            config: Arc::new(std::sync::RwLock::new(ServerConfig::default())),
            home: None,
            credentials: None,
        };
        // When `MIRRICODE_HOME` is set in the injected env (it outranks $HOME)
        assert_eq!(
            no_home_factory.mirri_home_from(|name| {
                (name == "MIRRICODE_HOME").then(|| std::ffi::OsString::from("/mirri/home"))
            }),
            PathBuf::from("/mirri/home")
        );
        // And when only `$HOME` is set (the `.mirri-code` suffix joins)
        assert_eq!(
            no_home_factory.mirri_home_from(|name| {
                (name == "HOME").then(|| std::ffi::OsString::from("/fake/home"))
            }),
            PathBuf::from("/fake/home/.mirri-code")
        );
        // And when nothing is set (the relative fallback)
        assert_eq!(
            no_home_factory.mirri_home_from(|_| None),
            PathBuf::from(".mirri-code")
        );
    }

    /// The end-to-end proof that "off" reaches the wire: build the adapter from
    /// a real config record, run a real HTTP chat against a mock endpoint, and
    /// read the request body the engine actually sent.
    ///
    /// This is the check the unit tests on `build_request` cannot make — they
    /// see the composed JSON, not what survives the transport. It is also the
    /// reason a wire-log inspection cannot answer the question: `wire.jsonl`
    /// records session ops, never HTTP bodies.
    #[tokio::test]
    async fn should_send_the_off_dialect_in_the_real_request_when_the_agent_asks_for_off() {
        // Given a MiMo-endpoint provider (thinking.type dialect) and an agent
        // whose effort is explicitly off
        let server = MockServer::start().await;
        mount_kimi_chat(&server).await;
        let home = tempfile::tempdir().expect("tempdir");
        let config = alias_config_with_model(
            ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([(
                    "OPENAI_API_KEY".to_string(),
                    "sk-test".to_string(),
                )])),
                base_url: Some(format!("{}/v1", server.uri())),
                ..Default::default()
            },
            "mimo-v2.6-flash",
        );
        // The dialect resolves from the model name's host, but the mock's base
        // URL is localhost — pin it explicitly so the test asserts the encoding
        // rather than the host table.
        config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .models
            .as_mut()
            .expect("models")
            .get_mut("ultra")
            .expect("alias")
            .thinking_off = Some("thinking_type".to_string());
        let factory = ConfigAwareLlmFactory::new(config, Some(home.path().to_path_buf()));
        let mut agent = alias_agent_config();
        agent.thinking_effort = Some(ThinkingEffort::None);

        // When a chat runs
        let llm = factory.create_llm(&agent).expect("adapter builds");
        drain_chat(llm.chat(simple_chat_params()).expect("chat starts")).await;

        // Then the request body the engine sent carries the off payload
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("body is JSON");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "disabled"}),
            "the real request must carry the endpoint's off spelling: {body}"
        );
        // And it must NOT also send a contradictory effort tier
        assert!(
            body.get("reasoning_effort").is_none(),
            "thinking.type and reasoning_effort must not both be sent: {body}"
        );
    }

    #[test]
    fn should_attach_a_thinking_intent_resolved_from_the_provider_endpoint_when_building_an_openai_adapter() {
        // Given a config whose openai-type provider points at MiMo's endpoint
        // And an agent asking for an explicit off
        let home = tempfile::tempdir().expect("tempdir");
        let factory = ConfigAwareLlmFactory::new(
            alias_config_with_model(
                ProviderEntry {
                    r#type: Some("openai".to_string()),
                    env: Some(HashMap::from([(
                        "OPENAI_API_KEY".to_string(),
                        "sk-test".to_string(),
                    )])),
                    base_url: Some("https://token-plan-cn.xiaomimimo.com/v1".to_string()),
                    ..Default::default()
                },
                "mimo-v2.6-flash",
            ),
            Some(home.path().to_path_buf()),
        );
        let mut agent = alias_agent_config();
        agent.thinking_effort = Some(ThinkingEffort::None);

        // When the adapter is built
        let llm = factory.create_llm(&agent).expect("adapter builds");

        // Then its request spells the off in the endpoint's own dialect
        let adapter = llm
            .as_any()
            .downcast_ref::<OpenAILlm>()
            .expect("the adapter is the OpenAI dialect");
        let request = adapter
            .build_request(&simple_chat_params())
            .expect("request builds");
        assert_eq!(request["thinking"], serde_json::json!({"type": "disabled"}));
    }

}
