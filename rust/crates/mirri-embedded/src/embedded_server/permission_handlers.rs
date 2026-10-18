//! User-level permission rule management dispatch handlers for the
//! embedded [`EmbeddedServer`](super::EmbeddedServer) — the B9 approval
//! CRUD surface (`approval.listRules` / `approval.addRule` /
//! `approval.removeRule`).
//!
//! The persisted fact source is the mirri home `config.toml`'s
//! `[[permission.rules]]` section (user scope only — project /
//! turn-override / session-runtime rules are not manageable through this
//! face). The write pipeline mirrors `config.set`: load-without-env →
//! mutate → atomic `save_to` → reload-with-env into the shared snapshot,
//! then the new rule set is pushed into every live agent of every built
//! workspace runtime (`apply_user_permission_rules`), so CRUD writes take
//! effect without a restart.

use serde::Deserialize;
use serde_json::{Value, json};

use mirri_core::config::ServerConfig;
use mirri_core::permission::{PermissionRule, RuleDecision, RuleScope, parse_pattern};

use super::{EmbeddedServer, engine_error, invalid_params, not_found};

/// The user-scope rules the shared config snapshot declares.
fn snapshot_rules(config: &ServerConfig) -> Vec<PermissionRule> {
    config
        .permission
        .as_ref()
        .and_then(|permission| permission.rules.clone())
        .unwrap_or_default()
}

/// The shared result wire shape of all three CRUD methods:
/// `{ "rules": [ { decision, scope, pattern, reason? } ] }` (the klient
/// row spelling — `reason` omitted when `None`, zod `.optional()` rejects
/// `null`).
fn rules_wire(rules: &[PermissionRule]) -> Value {
    json!({
        "rules": rules
            .iter()
            .map(mirri_core::config::permission_rule_to_wire)
            .collect::<Vec<_>>(),
    })
}

impl EmbeddedServer {
    /// `approval.listRules` — the user-level permission rules of the shared
    /// config snapshot (the read face; no session addressing, no writes).
    pub(super) async fn dispatch_approval_list_rules(
        &self,
        _params: Value,
    ) -> Result<Value, super::EngineError> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        Ok(rules_wire(&snapshot_rules(&config)))
    }

    /// `approval.addRule` — append one user-scope rule
    /// (`{ rule: { decision, pattern, reason?, scope? } }`) through the
    /// config.toml write pipeline, then push the new rule set into every
    /// live agent. Same pattern + same decision is an idempotent no-op
    /// (returns the current list); same pattern + different decision is
    /// a conflicting-rule invalid-params.
    pub(super) async fn dispatch_approval_add_rule(
        &self,
        params: Value,
    ) -> Result<Value, super::EngineError> {
        #[derive(Debug, Deserialize)]
        struct RuleParams {
            decision: RuleDecision,
            #[serde(default)]
            scope: Option<RuleScope>,
            pattern: String,
            #[serde(default)]
            reason: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct AddRuleParams {
            rule: RuleParams,
        }
        let params: AddRuleParams = serde_json::from_value(params)
            .map_err(|e| invalid_params(format!("addRule requires a 'rule' object: {e}")))?;

        // Only the user scope is manageable through this face.
        match params.rule.scope {
            None | Some(RuleScope::User) => {}
            Some(other) => {
                return Err(invalid_params(format!(
                    "addRule manages user-scope rules only; scope '{other:?}' is not accepted"
                )));
            }
        }
        // The pattern must parse against the engine's rule DSL.
        let parsed = parse_pattern(&params.rule.pattern).ok_or_else(|| {
            invalid_params(format!(
                "unparseable rule pattern: {:?}",
                params.rule.pattern
            ))
        })?;
        if parsed.tool_name.is_empty() {
            return Err(invalid_params("rule pattern must name a tool"));
        }

        let new_rule = PermissionRule {
            decision: params.rule.decision,
            scope: RuleScope::User,
            pattern: params.rule.pattern,
            reason: params.rule.reason,
        };
        self.write_rules(|rules| {
            // Adjudication 4.d: this face manages user-scope rules only —
            // a project-scope row of the same pattern is not a conflict
            // (both may coexist; the chain's deny-before-allow order
            // governs evaluation).
            if let Some(existing) = rules
                .iter()
                .find(|rule| rule.pattern == new_rule.pattern && rule.scope == RuleScope::User)
            {
                if existing.decision == new_rule.decision {
                    // Idempotent no-op: the rule is already in force.
                    return Ok(false);
                }
                return Err(invalid_params(format!(
                    "conflicting rule: pattern {:?} is already {existing:?}",
                    new_rule.pattern
                )));
            }
            rules.push(new_rule);
            Ok(true)
        })
        .await
    }

    /// `approval.removeRule` — remove one user-scope rule by
    /// `{ pattern, decision }`; an unknown pair answers not-found (aligned
    /// with `approval.decide`'s unknown-id semantics).
    pub(super) async fn dispatch_approval_remove_rule(
        &self,
        params: Value,
    ) -> Result<Value, super::EngineError> {
        #[derive(Debug, Deserialize)]
        struct RemoveRuleParams {
            pattern: String,
            decision: RuleDecision,
        }
        let params: RemoveRuleParams = serde_json::from_value(params)
            .map_err(|e| invalid_params(format!("removeRule params: {e}")))?;

        self.write_rules(|rules| {
            // Adjudication 4.d: removal is scoped to user rules too — a
            // project-scope row with the same {pattern, decision} is not
            // addressable through this face.
            let before = rules.len();
            rules.retain(|rule| {
                !(rule.pattern == params.pattern
                    && rule.decision == params.decision
                    && rule.scope == RuleScope::User)
            });
            if rules.len() == before {
                return Err(not_found(format!(
                    "no rule {{pattern: {:?}, decision: {:?}}} to remove",
                    params.pattern, params.decision
                )));
            }
            Ok(true)
        })
        .await
    }

    /// The shared write pipeline (the `config.set` skeleton applied to
    /// `[[permission.rules]]`): re-read the persisted config without env
    /// overrides, apply `mutate` to the rule list (`Ok(false)` = no change
    /// needed — answer the current list without writing), persist
    /// atomically, reload with env overrides into the shared snapshot, and
    /// push the new rule set into every live agent of every built
    /// workspace runtime.
    async fn write_rules(
        &self,
        mutate: impl FnOnce(&mut Vec<PermissionRule>) -> Result<bool, super::EngineError>,
    ) -> Result<Value, super::EngineError> {
        let config_path = self.config_path.clone().ok_or_else(|| {
            invalid_params("the rule write requires a config file path; the server has none")
        })?;

        // Merge base: the persisted config exactly as on disk, without env
        // overrides (never the in-memory snapshot, so env-injected values
        // are not baked into config.toml).
        let mut config = ServerConfig::load_without_env(&config_path, &mirri_os::LocalSyncFs)
            .map_err(engine_error)?;
        let mut rules = snapshot_rules(&config);
        let changed = mutate(&mut rules)?;
        if !changed {
            // Idempotent no-op: report the current effective list.
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            return Ok(rules_wire(&snapshot_rules(&snapshot)));
        }

        // Preserve the `[permission]` mode key (if any) and swap the rules.
        let mode = config
            .permission
            .as_ref()
            .and_then(|permission| permission.mode.clone());
        config.permission = Some(mirri_core::config::PermissionConfig {
            mode,
            rules: Some(rules.clone()),
        });

        // Persist atomically (temp + fsync + rename, mode 0o600), then
        // reload with env overrides into the shared snapshot.
        config.save_to(&config_path).map_err(engine_error)?;
        let reloaded =
            ServerConfig::load(&config_path, &mirri_os::LocalSyncFs).map_err(engine_error)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = reloaded;

        // The reloaded snapshot carries the env-expanded rules (the same
        // form session create/resume seeds from) — push and answer with
        // that set, never the raw disk rows, so live agents and new
        // sessions evaluate the same patterns.
        let effective = {
            let snapshot = self.config.read().unwrap_or_else(|e| e.into_inner());
            snapshot_rules(&snapshot)
        };

        // The new rules reach every live agent without a restart (D-34);
        // sessions created later read the swapped snapshot at their own
        // create/resume seeding.
        self.apply_user_permission_rules(effective.clone()).await;

        Ok(rules_wire(&effective))
    }
}
