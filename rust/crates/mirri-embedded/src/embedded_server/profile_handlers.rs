//! Agent profile CRUD + lifecycle dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the P6 `agent.*Profile`
//! CRUD surface plus the R4 Task 25 enable/disable/reset lifecycle, all
//! behind the web `/api/v1/agents` REST routes (the lifecycle methods the
//! web `WsRpcApi` calls directly, adjudication b).
//!
//! The methods ride the same model the TS server's `/agents` REST routes
//! own (`packages/kap-server/src/routes/agents.ts`, the v1 wire
//! contract): profiles resolve from the built-in set (`agent`, `coder`,
//! `explore`, `plan`) overlaid by the discovery roots — the user agents
//! dirs (`.md` first-class) plus the legacy `<mirri-home>/profiles/`
//! (`.toml`/`.json`) the CLI boot loads and the hot-reload watcher
//! re-scans. Writes go through
//! [`ProfileManager::upsert_profile`] / [`ProfileManager::delete_profile`]
//! and persist under the user agents root (`<mirri-home>/agents/`) in the
//! same `.md` format the discovery pass reads (R4-1): an upsert over an
//! existing `.md` is a surgical rewrite — only the keys whose values
//! actually changed have their lines rewritten, so untouched keys keep the
//! file's own quote style, alias spelling and list shapes verbatim
//! (T23-m1), and a file that does not parse is replaced wholesale rather
//! than edited in place (T23-m2) — and the same-name legacy files under
//! `profiles/` are cleaned; the watcher-side reload of an unchanged file
//! is idempotent (same content, same registration). Skip-and-collect
//! diagnostics from the latest load are surfaced on the wire by the list
//! handler (`agent.listProfiles` answers `{items, skipped}`, R3-6) off the
//! manager's [`ProfileManager::last_skipped_files`] channel.
//!
//! Policy mirrors the TS routes: the default `agent` profile is essential
//! (never creatable or deletable — and per adjudication D16 its content
//! **is** updatable: `agent.updateProfile` writes the same-name `.md` and
//! registers the merged content as the override, isomorphic with any other
//! covered built-in), a built-in without a user override is read-only
//! (an override file must exist before a delete falls back to the built-in
//! definition), and user names are validated against path escape before
//! they become file names. The engine itself still refuses the essential
//! name on the delete channel and on the lifecycle face (disable/reset),
//! while discovery loads same-name user files as content overrides.

use std::path::PathBuf;
use std::sync::Arc;

use mirri_core::ProfileManager;
use mirri_core::profile::{
    AGENT_NAME_MAX_LENGTH, AgentProfile, ESSENTIAL_PROFILE_NAME, ProfileProvenance, ProfileSource,
    is_builtin_profile_name, is_file_disabled, is_kebab_case, parse_agent_file_text,
    render_system_prompt,
};
use serde::Deserialize;
use serde_json::{Value, json};

use mirri_os::SyncFs as _;
use mirri_transport::adapter::EngineError;

use super::{EmbeddedServer, engine_error, invalid_params, normalize_params, not_found};

/// The editable profile fields the create/update wire carries — the full
/// TS `createProfileRequestSchema` / `updateProfileRequestSchema` surface
/// that maps onto the rust [`AgentProfile`] (`packages/protocol/src/rest/
/// agents.ts`: description, default_model, tools, disallowed_tools,
/// subagents, capabilities_required, system_prompt_template, when_to_use —
/// T19 m5 cleared). Field names accept both the snake_case REST spelling
/// and the camelCase klient spelling.
///
/// `enabled` is deliberately **absent** from this face: enable/disable is
/// lifecycle state owned by the `:enable`/`:disable` action routes (Task
/// 25's methods), never a create/update field — the TS schemas do not
/// carry it either, and an update silently flipping the flag would bypass
/// the essential-profile lock.
#[derive(Debug, Default, Deserialize)]
struct ProfileFields {
    #[serde(default)]
    description: Option<String>,
    #[serde(default, alias = "defaultModel")]
    default_model: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default, alias = "disallowedTools")]
    disallowed_tools: Option<Vec<String>>,
    #[serde(default)]
    subagents: Option<Vec<String>>,
    #[serde(default, alias = "capabilitiesRequired")]
    capabilities_required: Option<Vec<String>>,
    #[serde(default, alias = "systemPromptTemplate")]
    system_prompt_template: Option<String>,
    #[serde(default, alias = "whenToUse")]
    when_to_use: Option<String>,
}

/// The route-level name-contract violation messages — verbatim the TS
/// `agentNameValidationError` family (`packages/kap-server/src/routes/
/// agents.ts:149-166`), pinned character-for-character: the web layer
/// matches messages by string.
const NAME_PATTERN_MESSAGE: &str = "name must be kebab-case (lowercase alphanumeric \
     segments separated by single hyphens, e.g. \"code-reviewer\")";
const NAME_LENGTH_MESSAGE: &str = "name must be at most 64 characters";

/// The contract-violation message for `name` (pattern or length), or
/// `None` when the name is valid — the TS `agentNameValidationError`
/// mirror. The name is concatenated into a file path by the write channel,
/// so a non-kebab payload is a path-traversal write primitive; the kebab
/// grammar and the 64-character cap (TS `AGENT_NAME_PATTERN` /
/// `AGENT_NAME_MAX_LENGTH`, the same bounds the `.md` parser enforces)
/// reject it up front.
fn agent_name_validation_error(name: &str) -> Option<&'static str> {
    if !is_kebab_case(name) {
        return Some(NAME_PATTERN_MESSAGE);
    }
    if name.len() > AGENT_NAME_MAX_LENGTH {
        return Some(NAME_LENGTH_MESSAGE);
    }
    None
}

/// The profile manager (or the honest error when the server was built
/// without one — the `with_providers` default).
fn require_profile_manager(server: &EmbeddedServer) -> Result<Arc<ProfileManager>, EngineError> {
    server
        .profile_manager
        .clone()
        .ok_or_else(|| invalid_params("agent profiles are unavailable on this engine"))
}

impl EmbeddedServer {
    /// `agent.listProfiles` — the `{items, skipped}` envelope (R3-6): every
    /// registered profile as a `WireAgentProfile` row under `items` (the TS
    /// `ProfileEntry` wire shape, snake_case), plus the files the latest
    /// discovery pass skipped under `skipped` as `{path, reason}` entries
    /// (skip-and-collect diagnostics, US-A1-3). Management face: a
    /// file-disabled profile stays listed **but marked** (`enabled: false`
    /// — the TS `listAll()` semantics); the dispatch face that hides such
    /// profiles is [`ProfileManager::list`] (R3-1).
    pub(super) async fn dispatch_list_profiles(&self) -> Result<Value, EngineError> {
        let manager = require_profile_manager(self)?;
        let names = manager.list_all();
        let items: Vec<Value> = names
            .iter()
            .filter_map(|name| self.profile_wire_row(&manager, name))
            .collect();
        let skipped: Vec<Value> = manager
            .last_skipped_files()
            .into_iter()
            .map(|file| {
                json!({
                    "path": file.path.display().to_string(),
                    "reason": file.reason,
                })
            })
            .collect();
        Ok(json!({ "items": items, "skipped": skipped }))
    }

    /// `agent.getProfile(name)` — one profile row, or not-found.
    ///
    /// The unknown-name message is the TS-exact `Agent profile "X" was
    /// not found` (`packages/kap-server/src/routes/agents.ts:648`) —
    /// capital `A`, pinned character-for-character by the tests.
    pub(super) async fn dispatch_get_profile(&self, params: Value) -> Result<Value, EngineError> {
        let name = profile_name_arg(&params)?;
        let manager = require_profile_manager(self)?;
        self.profile_wire_row(&manager, &name)
            .ok_or_else(|| not_found(format!("Agent profile \"{name}\" was not found")))
    }

    /// `agent.createProfile(fields)` — register and persist a new user
    /// profile; the created row comes back.
    ///
    /// The pre-write validation chain mirrors the TS POST `/agents` route
    /// order-for-order and message-for-message (`agents.ts:682-735`):
    /// `name is required` → the kebab/length name contract → the essential
    /// refusal → the non-empty description (the historical "invisible
    /// profile" guard: the `.md` parser silently skips a blank one) → the
    /// duplicate check.
    pub(super) async fn dispatch_create_profile(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct CreateParams {
            // A missing `name` key answers the TS text (not the serde
            // missing-field error): the default lands in the empty-string
            // branch below — the Task 24 handover.
            #[serde(default)]
            name: String,
            #[serde(flatten)]
            fields: ProfileFields,
        }
        let params: CreateParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        if params.name.is_empty() {
            return Err(invalid_params("name is required"));
        }
        if let Some(name_error) = agent_name_validation_error(&params.name) {
            return Err(invalid_params(name_error));
        }
        if params.name == ESSENTIAL_PROFILE_NAME {
            return Err(invalid_params(format!(
                "Cannot create a profile with the reserved name \"{ESSENTIAL_PROFILE_NAME}\""
            )));
        }
        if params
            .fields
            .description
            .as_deref()
            .is_none_or(|description: &str| description.trim().is_empty())
        {
            return Err(invalid_params(
                "description is required and must be a non-empty string",
            ));
        }
        let manager = require_profile_manager(self)?;
        if manager.get(&params.name).is_some() {
            return Err(invalid_params(format!(
                "Agent profile \"{}\" already exists",
                params.name
            )));
        }
        let profile = AgentProfile {
            name: params.name.clone(),
            model: params.fields.default_model.clone().unwrap_or_default(),
            description: params.fields.description,
            default_model: params.fields.default_model,
            system_prompt: params.fields.system_prompt_template,
            when_to_use: params.fields.when_to_use,
            tools: params.fields.tools,
            disallowed_tools: params.fields.disallowed_tools,
            subagents: params.fields.subagents,
            capabilities_required: params.fields.capabilities_required,
            ..Default::default()
        };
        let agents_dir = self.agents_dir()?;
        manager
            .upsert_profile(
                profile,
                &agents_dir,
                &self.legacy_profiles_dir(),
                &mirri_os::LocalSyncFs,
            )
            .map_err(engine_error)?;
        Ok(self
            .profile_wire_row(&manager, &params.name)
            .expect("the profile was just upserted"))
    }

    /// `agent.updateProfile(name, fields)` — merge the provided fields into
    /// the existing profile and persist (a built-in becomes a user override
    /// `.md`); the merged row comes back. The essential profile is
    /// **updatable** (adjudication D16 — the deliberate divergence from the
    /// TS PUT route, which refuses `Cannot modify the essential profile
    /// "agent"`): the same-name `.md` is written and the merged content
    /// registers as the override — isomorphic with updating any other
    /// built-in.
    ///
    /// Post-merge validation mirrors the TS `:852-870` description guard:
    /// an update that would serialize an empty description (an explicit
    /// blank, or merging into a profile that has none) writes a file the
    /// loader skips — the profile would vanish from every subsequent
    /// read. The explicit-blank `system_prompt_template` gets the same
    /// treatment (TS has no such check — its serializer coerces a blank
    /// body to the placeholder prompt; the rust write channel persists
    /// the body verbatim, so a blank would produce the same unparseable
    /// file — rejected here instead, absent stays legal).
    pub(super) async fn dispatch_update_profile(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct UpdateParams {
            name: String,
            #[serde(flatten)]
            fields: ProfileFields,
        }
        let params: UpdateParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let manager = require_profile_manager(self)?;
        let existing = manager
            .get(&params.name)
            .ok_or_else(|| not_found(format!("Agent profile \"{}\" was not found", params.name)))?;
        let mut merged = existing;
        if let Some(description) = params.fields.description {
            merged.description = Some(description);
        }
        if let Some(default_model) = params.fields.default_model {
            merged.default_model = Some(default_model.clone());
            merged.model = default_model;
        }
        if let Some(tools) = params.fields.tools {
            merged.tools = Some(tools);
        }
        if let Some(disallowed_tools) = params.fields.disallowed_tools {
            merged.disallowed_tools = Some(disallowed_tools);
        }
        if let Some(subagents) = params.fields.subagents {
            merged.subagents = Some(subagents);
        }
        if let Some(capabilities_required) = params.fields.capabilities_required {
            merged.capabilities_required = Some(capabilities_required);
        }
        if let Some(template) = params.fields.system_prompt_template.clone() {
            merged.system_prompt = Some(template);
        }
        if let Some(when_to_use) = params.fields.when_to_use {
            merged.when_to_use = Some(when_to_use);
        }
        if merged
            .description
            .as_deref()
            .is_none_or(|description: &str| description.trim().is_empty())
        {
            return Err(invalid_params("description must be a non-empty string"));
        }
        if params
            .fields
            .system_prompt_template
            .as_deref()
            .is_some_and(|template: &str| template.trim().is_empty())
        {
            return Err(invalid_params(
                "system_prompt_template must be a non-empty string",
            ));
        }
        let agents_dir = self.agents_dir()?;
        manager
            .upsert_profile(
                merged,
                &agents_dir,
                &self.legacy_profiles_dir(),
                &mirri_os::LocalSyncFs,
            )
            .map_err(engine_error)?;
        Ok(self
            .profile_wire_row(&manager, &params.name)
            .expect("the profile was just upserted"))
    }

    /// `agent.deleteProfile(name)` — remove the persisted file and the
    /// registration; a built-in override delete restores the built-in
    /// definition. `{deleted: true}` on the wire. The essential profile
    /// stays undeletable (D16: content may be overridden, the name may not
    /// be removed). The refusals are the TS-exact texts
    /// (`agents.ts:968-1000`): `Cannot delete the essential profile
    /// "agent"` / `Cannot delete built-in profile "X"`.
    pub(super) async fn dispatch_delete_profile(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let name = profile_name_arg(&params)?;
        if name == ESSENTIAL_PROFILE_NAME {
            return Err(invalid_params(format!(
                "Cannot delete the essential profile \"{ESSENTIAL_PROFILE_NAME}\""
            )));
        }
        let manager = require_profile_manager(self)?;
        if manager.get(&name).is_none() {
            return Err(not_found(format!("Agent profile \"{name}\" was not found")));
        }
        // Cannot delete built-in definitions (only their override): the
        // provenance a user file produced is the override marker — path-
        // agnostic, so a legacy `profiles/<name>.toml` override is
        // deletable just like an agents-root `.md` one.
        if is_builtin_profile_name(&name)
            && !manager
                .provenance(&name)
                .is_some_and(|provenance| provenance.source != ProfileSource::Builtin)
        {
            return Err(invalid_params(format!(
                "Cannot delete built-in profile \"{name}\""
            )));
        }
        let agents_dir = self.agents_dir()?;
        manager
            .delete_profile(
                &name,
                &agents_dir,
                &self.legacy_profiles_dir(),
                &mirri_os::LocalSyncFs,
            )
            .map_err(engine_error)?;
        Ok(json!({ "deleted": true }))
    }

    /// `agent.disableProfile(name)` — set the file-level `enabled = false`
    /// flag through the `.md` write channel. `{ok: true}` on the wire.
    ///
    /// The essential refusal carries the TS-exact text
    /// (`agents.ts:1064-1072`): `Cannot disable the essential profile
    /// "agent"` — `enable` on the essential is the legal no-op below.
    pub(super) async fn dispatch_disable_profile(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let name = profile_name_arg(&params)?;
        self.toggle_profile_enabled(&name, false)
    }

    /// `agent.enableProfile(name)` — clear the file-level `enabled = false`
    /// flag. `{ok: true}` on the wire. A profile that is already enabled
    /// (e.g. a bare built-in with no `.md`) is a legal no-op success.
    pub(super) async fn dispatch_enable_profile(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let name = profile_name_arg(&params)?;
        self.toggle_profile_enabled(&name, true)
    }

    /// `agent.resetProfile(name)` — delete a built-in's override `.md` (plus
    /// the same-name legacy files, the delete channel's cleanup semantics)
    /// so the built-in definition is restored. `{ok: true}` on the wire.
    ///
    /// The refusals carry the TS-exact texts (`agents.ts:1064-1072` and
    /// `:1099-1107`): `Cannot reset the essential profile "agent"` /
    /// `Can only reset built-in profiles with overrides` — a user-created
    /// profile is not resettable either (it has no built-in to restore).
    pub(super) async fn dispatch_reset_profile(&self, params: Value) -> Result<Value, EngineError> {
        let name = profile_name_arg(&params)?;
        if name == ESSENTIAL_PROFILE_NAME {
            return Err(invalid_params(format!(
                "Cannot reset the essential profile \"{ESSENTIAL_PROFILE_NAME}\""
            )));
        }
        let manager = require_profile_manager(self)?;
        if manager.get(&name).is_none() {
            return Err(not_found(format!("Agent profile \"{name}\" was not found")));
        }
        // Only a built-in with an override is resettable — the same
        // provenance check the delete channel's built-in guard uses.
        if !is_builtin_profile_name(&name)
            || !manager
                .provenance(&name)
                .is_some_and(|provenance| provenance.source != ProfileSource::Builtin)
        {
            return Err(invalid_params(
                "Can only reset built-in profiles with overrides",
            ));
        }
        let agents_dir = self.agents_dir()?;
        manager
            .delete_profile(
                &name,
                &agents_dir,
                &self.legacy_profiles_dir(),
                &mirri_os::LocalSyncFs,
            )
            .map_err(engine_error)?;
        Ok(json!({ "ok": true }))
    }

    /// The enable/disable lifecycle write: flip the profile's file-level
    /// `enabled` flag through [`ProfileManager::upsert_profile`] (Task 23's
    /// write channel).
    ///
    /// Policy mirrors the TS action route (`agents.ts:1064-1145`): the
    /// essential profile refuses `disable` with the TS-exact text while
    /// `enable` is a legal no-op success (the built-in is always
    /// effectively enabled, R3-2), an unknown name answers the TS-exact
    /// not-found text, and a profile already in the requested state is a
    /// no-op success — the TS route re-serializes the file on every
    /// action; the rust channel does not rewrite a file whose flag
    /// already matches (D14: no gratuitous rewrites).
    ///
    /// Round-trip fidelity (the disable→enable byte round trip): when the
    /// agents-root `.md` exists and parses, the upsert base is the
    /// **file's own parsed definition** — the registered profile may
    /// carry merged built-in values the file never declared, and writing
    /// those would violate the verbatim contract. With the file-def base
    /// the surgical rewrite narrows to the single `enabled` edit (Set on
    /// disable, Remove on enable); every other byte passes through. A
    /// name with no `.md` (a bare built-in, or a legacy
    /// `profiles/<name>.toml` registration) serializes the registered
    /// definition fully — plus the flag on disable, without it on enable —
    /// and the channel's legacy cleanup removes the same-name files.
    fn toggle_profile_enabled(&self, name: &str, enable: bool) -> Result<Value, EngineError> {
        if !enable && name == ESSENTIAL_PROFILE_NAME {
            return Err(invalid_params(format!(
                "Cannot disable the essential profile \"{ESSENTIAL_PROFILE_NAME}\""
            )));
        }
        let manager = require_profile_manager(self)?;
        let existing = manager
            .get(name)
            .ok_or_else(|| not_found(format!("Agent profile \"{name}\" was not found")))?;
        if is_file_disabled(&existing) != enable {
            // Already in the requested state (an enabled built-in's
            // `enable`, an idempotent `disable`): a legal no-op success.
            return Ok(json!({ "ok": true }));
        }
        let agents_dir = self.agents_dir()?;
        let fs = &mirri_os::LocalSyncFs;
        let path = agents_dir.join(format!("{name}.md"));
        let mut profile = if fs.exists(&path) {
            // Read-modify-write: the file's own parsed definition is the
            // upsert base, so the surgical rewrite narrows to the single
            // `enabled` edit. A file that does not parse (or cannot be
            // read) falls back to the registered definition — upsert
            // replaces it wholesale, its corrupt-file policy. A
            // hand-edited file that declares a *foreign* `name` (e.g.
            // `agents/coder.md` saying `name: other`) falls back the same
            // way: writing that definition would upsert the foreign
            // name's file, not this one's — the TS route pins the base to
            // the registered profile (`existing.name`, agents.ts:1126).
            fs.read_text(&path)
                .ok()
                .and_then(|text| parse_agent_file_text(&path.display().to_string(), &text).ok())
                .filter(|parsed| parsed.name == name)
                .map(AgentProfile::from)
                .unwrap_or_else(|| existing.clone())
        } else {
            // No `.md` yet: the registered (merged) definition serializes
            // in full — plus the flag on disable.
            existing.clone()
        };
        profile.enabled = (!enable).then_some(false);
        manager
            .upsert_profile(profile, &agents_dir, &self.legacy_profiles_dir(), fs)
            .map_err(engine_error)?;
        Ok(json!({ "ok": true }))
    }

    // --- shared helpers -------------------------------------------------

    /// `<mirri-home>/agents/` — the write root: the `.md` files the CRUD
    /// channel reads and writes live here (R4-1), first in the discovery
    /// root chain, so a written profile wins its same-name merges.
    fn agents_dir(&self) -> Result<PathBuf, EngineError> {
        Ok(self.home_root().join("agents"))
    }

    /// `<mirri-home>/profiles/` — the legacy root: read by discovery, and
    /// cleaned of same-name files by the write channel once a `.md`
    /// exists.
    fn legacy_profiles_dir(&self) -> PathBuf {
        self.home_root().join("profiles")
    }

    /// Project one registered profile onto the TS `ProfileEntry` wire shape
    /// (snake_case, `packages/protocol/src/rest/agents.ts:9-37`), optional
    /// fields absent — never null — when missing.
    ///
    /// R3-5 provenance: `source` / `file_path` / `has_override` derive
    /// from the provenance the manager retained at load (builtins:
    /// `{builtin, no file}`; discovery winners: their root's tag + the
    /// discovered file; CRUD writes: user + the persisted TOML) — no
    /// filesystem probing, so a row always matches the registration it
    /// projects. A user file overriding a built-in (the essential
    /// included, D16) projects `source: "user"` + `builtin: false` +
    /// `has_override: true` + the file's path — the cross-engine shape
    /// (the TS `catalogEntryToResolved` projects the same triple).
    ///
    /// R3-2/D16: the essential row always reports `essential: true` and
    /// `enabled: true` (the file-level `enabled = false` flag warns
    /// server-side and is exempt — [`is_file_disabled`], the TS `:192-200`
    /// exemption — the TS catalog treats the essential as always enabled),
    /// whether it carries a user override or not.
    fn profile_wire_row(&self, manager: &ProfileManager, name: &str) -> Option<Value> {
        let profile = manager.get(name)?;
        // Provenance also drives the REST-only `file_path` below; the base
        // row recomputes it for the shared cross-engine fields (same
        // fallback contract).
        let provenance = manager.provenance(name).unwrap_or_else(|| {
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
        // The six cross-engine fields plus the suppressed-candidates list
        // come from the shared projection ([`ProfileManager::
        // profile_wire_row_base`]) — the same function the cross-engine
        // list acceptance test asserts through, so no test-side mirror of
        // this shape exists.
        let mut row = manager.profile_wire_row_base(name)?;
        if let Some(file) = &provenance.file_path {
            row["file_path"] = json!(file.display().to_string());
        }
        if let Some(description) = &profile.description {
            row["description"] = json!(description);
        }
        let default_model = profile
            .default_model
            .clone()
            .filter(|model| !model.is_empty())
            .or_else(|| {
                let model = profile.model.clone();
                (!model.is_empty()).then_some(model)
            });
        if let Some(default_model) = default_model {
            row["default_model"] = json!(default_model);
        }
        if let Some(tools) = &profile.tools {
            row["tools"] = json!(tools);
        }
        if let Some(disallowed_tools) = &profile.disallowed_tools {
            row["disallowed_tools"] = json!(disallowed_tools);
        }
        if let Some(capabilities_required) = &profile.capabilities_required {
            row["capabilities_required"] = json!(capabilities_required);
        }
        // R3-4: the subagent name-dispatch allowlist (default-deny:
        // absent = none — consumption-domain SDR-012).
        if let Some(subagents) = &profile.subagents {
            row["subagents"] = json!(subagents);
        }
        if let Some(when_to_use) = &profile.when_to_use {
            row["when_to_use"] = json!(when_to_use);
        }
        if let Some(template) = &profile.system_prompt {
            row["system_prompt_template"] = json!(template);
            row["effective_system_prompt"] =
                json!(render_system_prompt(template, &profile.prompt_vars));
        }
        if let Some(vars) = &profile.prompt_vars {
            row["prompt_vars"] = json!(vars);
        }
        Some(row)
    }
}

/// Extract the `name` argument (the object spelling only — the REST layer
/// injects the path name; the klient spelling sends `{name}`).
fn profile_name_arg(params: &Value) -> Result<String, EngineError> {
    let params = normalize_params(params.clone());
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_params("name is required"))?;
    if name.is_empty() {
        return Err(invalid_params("name must be non-empty"));
    }
    Ok(name)
}
