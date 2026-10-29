//! Credential-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the provider-credential
//! store surface (Task 5): `credential.list` / `credential.set` /
//! `credential.remove` / `credential.status`.
//!
//! The store is the Task 1 durable plaintext JSON at
//! `<mirri-home>/credentials/credentials.json`, reached through the
//! server's home-rooted accessor (one open, cached for the lifetime).
//! The wire contract never carries key material (F.1): `list` projects
//! `{provider_id, type, time_created, status}` records (plus
//! `external_available` on external rows — a real CLI-presence probe,
//! G.1) and the sole reveal face is the local CLI (SDR-002/ADR-003). Successful writes emit the
//! `credential.updated` notification (E.3) so subscribed clients
//! re-read the list; `credential.status` answers the in-memory
//! auth-failure map (D.2) with secret-redacted messages (F.2).

use mirri_credentials::CredentialValue;
use mirri_transport::adapter::EngineError;
use serde_json::{Value, json};

use mirri_core::EngineError as CoreEngineError;
use std::collections::HashMap;

use super::config_handlers::first_string_arg;
use super::{EmbeddedServer, engine_error, invalid_params, normalize_params};

impl EmbeddedServer {
    /// `credential.list()` — every stored credential as
    /// `{provider_id, type, time_created, status}` records (plus
    /// `external_available` on external rows), snake_case wire; no key
    /// material ever. An entry whose provider id has no
    /// `[providers.<id>]` section in the live config is an orphan: it is
    /// still listed (the store is the source of truth for what it holds)
    /// but logged as a diagnostic warning — a provider deleted while its
    /// credential store entry survived the best-effort cascade (A.5b).
    pub(super) async fn dispatch_credential_list(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        // The optional providerId filter (the klient shape accepts the
        // scoping argument); a bare call lists everything.
        let filter = match normalize_params(params) {
            Value::Null => None,
            Value::Array(items) => match items.first() {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Object(map)) => match map.get("providerId") {
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => return Err(invalid_params("credential.list takes at most a providerId")),
            },
            Value::Object(map) => match map.get("providerId") {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            },
            _ => return Err(invalid_params("credential.list takes at most a providerId")),
        };
        let Some(store) = self.credentials() else {
            return Err(engine_error(CoreEngineError::Internal(
                "credential store unavailable (the Mirri home cannot be opened)".to_string(),
            )));
        };
        // The configured provider ids — the orphan diagnostic's reference
        // set (A.5b): a stored provider_id with no section is dead weight
        // from a cascade miss.
        let configured: std::collections::HashSet<String> = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            config
                .to_provider_records_wire()
                .as_object()
                .map(|records| records.keys().cloned().collect())
                .unwrap_or_default()
        };
        let records: Vec<Value> = store
            .read_all()
            .into_iter()
            .filter(|record| {
                filter.as_deref().is_none_or(|id| record.provider_id == id)
            })
            .map(|record| {
                if !configured.contains(&record.provider_id) {
                    tracing::warn!(
                        provider = %record.provider_id,
                        "orphan credential entry: no [providers.{}] section in the config",
                        record.provider_id
                    );
                }
                // The status field shares its source with the single-query
                // `credential.status` face (D.2's in-memory map): a provider
                // with a recorded auth failure reads needs_reconfigure, any
                // other provider reads normal.
                let failures = self
                    .provider_auth_failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let status = if failures.contains_key(&record.provider_id) {
                    "needs_reconfigure"
                } else {
                    "normal"
                };
                drop(failures);
                // external_available is a separate fact carried only by
                // external rows (G.1): a real PATH probe of the named
                // method's CLI, never a guess, and never present on key or
                // oauth rows.
                let mut entry = json!({
                    "provider_id": record.provider_id,
                    "type": credential_type_tag(&record.value),
                    "time_created": record.time_created,
                    "status": status,
                });
                if let CredentialValue::External { method_id, .. } = &record.value {
                    entry["external_available"] =
                        json!(mirri_credentials::external_available(method_id));
                }
                entry
            })
            .collect();
        Ok(json!(records))
    }

    /// `credential.set(providerId, value)` — create/replace one
    /// provider's credential. The `value` object deserializes into the
    /// tagged `CredentialValue` union; an unknown `type` tag is
    /// `invalid_params`, never a silent guess. noResult on the wire, and
    /// the `credential.updated` notification goes out on success (E.3)
    /// while the provider's auth-failure flag clears (D.2).
    pub(super) async fn dispatch_credential_set(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let (provider_id, value) = credential_set_args(&params)?;
        let Some(store) = self.credentials() else {
            return Err(engine_error(CoreEngineError::Internal(
                "credential store unavailable (the Mirri home cannot be opened)".to_string(),
            )));
        };
        store
            .set(&provider_id, value)
            .map_err(|e| engine_error(CoreEngineError::Internal(format!("credential store write failed: {e}"))))?;
        self.clear_provider_auth_failure(&provider_id);
        self.emit_credential_updated(&provider_id);
        Ok(Value::Null)
    }

    /// `credential.remove(providerId)` — remove one provider's
    /// credential. A missing entry is a successful no-op (the store is
    /// already in the requested state). noResult; the
    /// `credential.updated` notification goes out on success (E.3) and
    /// the auth-failure flag clears (D.2).
    pub(super) async fn dispatch_credential_remove(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let provider_id = first_string_arg(&params, &["providerId", "provider_id"])?;
        let Some(store) = self.credentials() else {
            return Err(engine_error(CoreEngineError::Internal(
                "credential store unavailable (the Mirri home cannot be opened)".to_string(),
            )));
        };
        store
            .remove(&provider_id)
            .map_err(|e| engine_error(CoreEngineError::Internal(format!("credential store write failed: {e}"))))?;
        self.clear_provider_auth_failure(&provider_id);
        self.emit_credential_updated(&provider_id);
        Ok(Value::Null)
    }

    /// `credential.status(providerId)` — the health face over the
    /// in-memory auth-failure map (D.2): `{"status": "normal"}` with no
    /// recorded failure (the honest no-evidence state — the 401
    /// auto-report wiring is the O-5 open item), or
    /// `{"status": "needs_reconfigure", "message": ...}` with the
    /// secret-redacted failure text.
    pub(super) async fn dispatch_credential_status(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let provider_id = first_string_arg(&params, &["providerId", "provider_id"])?;
        let failures = self
            .provider_auth_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match failures.get(&provider_id) {
            None => Ok(json!({ "status": "normal" })),
            Some(message) => Ok(json!({
                "status": "needs_reconfigure",
                "message": message,
            })),
        }
    }

    /// The O-5 surfaced-failure entry point (D.5): report an engine error
    /// to the credential-status face when — and only when — it is an
    /// attributed provider auth failure. `None` attribution (a test mock,
    /// a direct construction) and every other error shape are ignored —
    /// the honest no-evidence state (ADR-009's boundary).
    pub(crate) fn report_auth_failure_if_attributed(&self, error: &CoreEngineError) {
        report_auth_failure_if_attributed(&self.provider_auth_failures, error);
    }

    /// Clear a provider's auth-failure flag — the `credential.set` /
    /// `credential.remove` success path (D.2: a reconfigure consumes
    /// the signal) and the `provider.delete` cascade (a re-created
    /// provider id must not inherit the stale flag).
    pub(crate) fn clear_provider_auth_failure(&self, provider_id: &str) {
        self.provider_auth_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider_id);
    }

    /// Broadcast one `credential.updated` notification (params carrying
    /// the provider id) to every live event subscriber — the E.3 change
    /// signal clients re-read `credential.list` on.
    ///
    /// Best-effort by design (the `emit_generate_chunk` precedent): a
    /// full channel drops the notification rather than blocking the
    /// write — the store's on-disk state is the authoritative face.
    fn emit_credential_updated(&self, provider_id: &str) {
        let notification = mirri_transport::frame::JsonRpcNotification::new(
            "credential.updated",
            json!({ "provider_id": provider_id }),
        );
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for subscriber in subscribers {
            let _ = subscriber.try_send(notification.clone());
        }
    }
}

/// Parse `credential.set`'s `(providerId, value)` pair, accepting the
/// object spelling and the positional tuple spelling (the klient tuple
/// contract). The `value` must deserialize into the tagged
/// `CredentialValue` union — an unknown `type` tag is `invalid_params`.
fn credential_set_args(params: &Value) -> Result<(String, CredentialValue), EngineError> {
    let normalized = normalize_params(params.clone());
    let (provider_id, value): (Value, Value) = match &normalized {
        Value::Array(items) if items.len() == 2 => (items[0].clone(), items[1].clone()),
        Value::Object(map) => {
            let Some(provider_id) = map.get("providerId").or_else(|| map.get("provider_id"))
            else {
                return Err(invalid_params("credential.set requires a providerId"));
            };
            let Some(value) = map.get("value") else {
                return Err(invalid_params("credential.set requires a value"));
            };
            (provider_id.clone(), value.clone())
        }
        _ => {
            return Err(invalid_params(
                "credential.set takes (providerId, value) — object or two-item tuple",
            ))
        }
    };
    let provider_id = value_as_string(&provider_id, "providerId")?;
    let value = serde_json::from_value::<CredentialValue>(value).map_err(|e| {
        invalid_params(format!(
            "credential.set value must be a tagged CredentialValue object ({{\"type\": \"key\" | \"external\" | ...}}): {e}"
        ))
    })?;
    Ok((provider_id, value))
}

fn value_as_string(value: &Value, name: &str) -> Result<String, EngineError> {
    value.as_str().map(str::to_string).ok_or_else(|| {
        invalid_params(format!("credential argument {name} must be a string"))
    })
}

/// The wire `type` tag of a stored credential — the closed union's
/// discriminant, projected onto `credential.list` rows.
fn credential_type_tag(value: &CredentialValue) -> &'static str {
    match value {
        CredentialValue::Key { .. } => "key",
        CredentialValue::External { .. } => "external",
        CredentialValue::Oauth { .. } => "oauth",
    }
}

/// Redact secret-looking material out of a failure message (F.2): the
/// `sk-`-prefixed API-key shape, `Bearer` authorization tokens, and
/// `-----BEGIN ... PRIVATE KEY-----` blocks each collapse to
/// `[REDACTED]` — the message keeps its diagnostic shape (the provider
/// error text) while the secret never reaches a status response or a
/// log line.
pub(crate) fn redact_secrets(message: &str) -> String {
    let mut redacted = redact_pattern(message, |line| {
        line.find("sk-").map(|start| (start, redact_len(&line[start..])))
    });
    redacted = redact_pattern(&redacted, |line| {
        line.find("Bearer ").map(|start| (start + "Bearer ".len(), redact_len(&line[start + "Bearer ".len()..])))
    });
    redact_pattern(&redacted, |line| {
        line.find("-----BEGIN").map(|start| (start, line.len() - start))
    })
}

/// The map-level [`EmbeddedServer::note_provider_auth_failure`] — a free
/// function over the shared field Arc so the spawn'd turn-settle sites (the
/// fire/steer arms, the cron/task/goal drivers — none of which hold a
/// `&EmbeddedServer`) note failures through the exact code path the RPC
/// surface does.
pub(crate) fn note_provider_auth_failure(
    failures: &std::sync::Mutex<HashMap<String, String>>,
    provider_id: &str,
    message: &str,
) {
    let redacted = redact_secrets(message);
    failures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(provider_id.to_string(), redacted);
}

/// The map-level [`EmbeddedServer::report_auth_failure_if_attributed`] —
/// the single O-5 classification entry (D.5): an attributed
/// `ProviderAuthError` (the attribution Task 1 threaded through the
/// adapters and `classify_surface_error`) is noted into the
/// credential-status map; `None` attribution and every other error shape
/// are the honest no-evidence state and are never reported.
pub(crate) fn report_auth_failure_if_attributed(
    failures: &std::sync::Mutex<HashMap<String, String>>,
    error: &CoreEngineError,
) {
    if let CoreEngineError::ProviderAuthError {
        message,
        provider_id: Some(provider_id),
    } = error
    {
        note_provider_auth_failure(failures, provider_id, message);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
/// Where a redaction applies: scan line by line (keys and PEM blocks
/// never span lines in practice), replacing `[start, start+len)` with
/// the marker.
fn redact_pattern(
    message: &str,
    locate: impl Fn(&str) -> Option<(usize, usize)>,
) -> String {
    message
        .lines()
        .map(|line| match locate(line) {
            Some((start, len)) => format!("{}[REDACTED]{}", &line[..start], &line[start + len..]),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg_attr(not(test), allow(dead_code))]
/// The length of the secret run starting at a key/token start: the run
/// of key-ish characters (alphanumerics, `_`, `-`, `.`, `:`) — a key
/// followed by punctuation or prose stops there.
fn redact_len(rest: &str) -> usize {
    rest.chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        .map(char::len_utf8)
        .sum()
}
