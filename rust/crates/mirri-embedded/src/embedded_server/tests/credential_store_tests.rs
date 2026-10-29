//! Credential-store resolution tests (Task 3): the embedded factory
//! resolves provider credentials through the credential store before the
//! env chain, and the `apiKey` field tier is retired (D-M3) so a
//! hand-written config key can never silently bypass the store.
//!
//! The tests target the pure resolution function so no server has to boot;
//! env mutation rides the shared [`ENV_LOCK`](crate::providers::ENV_LOCK)
//! the other env-touching tests serialize on, with capture-and-restore
//! hygiene for a developer's pre-existing variables.

use mirri_credentials::{CredentialStore, CredentialValue};
use serde_json::json;

use crate::providers::config_factory::{ConfigAwareLlmFactory, resolve_provider_credential};
use crate::providers::ENV_LOCK;

/// An openai-shaped provider wire record with no credential fields —
/// nothing but the store or the env chain can answer it.
fn openai_provider_record() -> serde_json::Value {
    json!({ "type": "openai" })
}

#[test]
fn should_prefer_credential_store_over_env_chain() {
    // Given a store holding openai's key while the shell carries a
    // conflicting OPENAI_API_KEY
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");
    store
        .set("openai", CredentialValue::Key { key: "sk-store".into() })
        .expect("store set");

    // When the credential is resolved with the env var set (captured and
    // restored so a developer's variable survives the test)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the store's key wins over the env chain
    assert_eq!(resolved, Ok(Some("sk-store".to_string())));
}

#[test]
fn should_fall_back_to_env_chain_when_store_has_no_entry() {
    // Given an empty store (no entry for the provider) and the shell
    // variable set
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");

    // When the credential is resolved (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the env chain answered
    assert_eq!(resolved, Ok(Some("sk-env".to_string())));
}

#[test]
fn should_return_none_when_store_and_env_both_empty() {
    // Given an empty store and the shell variable cleared
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");

    // When the credential is resolved (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then no source answered
    assert_eq!(resolved, Ok(None));
}

/// Given a store holding an external entry whose source fails to resolve,
/// when the credential is resolved, then the failure is the guidance
/// itself — no env-chain fallback (V5: a wrong key answering is worse
/// than a loud failure, and the guidance names the user's next action).
#[test]
fn should_error_with_az_login_guidance_when_the_external_source_fails_to_resolve() {
    // Given a store holding an external entry for openai whose metadata
    // lacks a resource_name (so the Azure CLI path fails with guidance),
    // while the shell carries a usable key the resolution must NOT fall
    // back to
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");
    store
        .set(
            "openai",
            CredentialValue::External {
                method_id: "azure-cli".into(),
                metadata: Default::default(),
            },
        )
        .expect("store set");

    // When the credential is resolved (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the error carries the action guidance and names the provider —
    // never the env chain's key, never the env-var-not-set face
    let error = resolved.expect_err("the external failure must propagate, not fall back");
    assert!(
        error.contains("az login") || error.contains("resource_name"),
        "the error must carry the user-facing guidance: {error}"
    );
    assert!(error.contains("openai"), "the error names the provider: {error}");
    assert!(
        !error.contains("OPENAI_API_KEY not set"),
        "the error must not disguise itself as the env-var face: {error}"
    );
}

/// Given an unknown external method id in the store, when the credential is
/// resolved, then the failure is the method-not-known guidance — no
/// fallback, the same no-env-fallback contract as the az failure path.
#[test]
fn should_error_when_the_external_method_is_unknown() {
    // Given a store holding an external entry naming no known method
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");
    store
        .set(
            "openai",
            CredentialValue::External {
                method_id: "carrier-pigeon".into(),
                metadata: Default::default(),
            },
        )
        .expect("store set");

    // When the credential is resolved (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the failure is the unknown-method guidance
    let error = resolved.expect_err("an unknown method must fail loudly");
    assert!(
        error.contains("carrier-pigeon"),
        "the error names the unknown method: {error}"
    );
}

#[test]
fn should_ignore_a_residual_api_key_field_on_the_provider_record() {
    // Given a provider record still carrying an `apiKey` field (the dead
    // key trap D-M3 retires) and no store or env answer
    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");
    let provider = json!({ "type": "openai", "apiKey": "sk-residual-field" });

    // When the credential is resolved (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("OPENAI_API_KEY").ok();
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let resolved =
            resolve_provider_credential(Some(&store), "openai", &provider, "openai");
        match prior {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the residual field was NOT consumed — the field tier is gone
    assert_eq!(resolved, Ok(None));
}

/// Given a turn failing with an attributed 401 provider auth error, when a
/// wait-mode `message.send` returns the error, then the failure map names
/// the provider with the redacted message (D.5/O-5 — the wait arm of the
/// surfaced-failure wiring), and a `credential.set` write clears it back
/// to normal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_mark_needs_reconfigure_when_wait_mode_turn_fails_with_auth_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let llm = Arc::new(AuthFailingLlm::new(Some("test-provider")));
    let server = credential_test_server_with_factory(
        &home,
        None,
        Arc::new(StaticLlmFactory {
            llm: Arc::clone(&llm) as Arc<dyn mirri_core::Llm>,
        }),
    );

    let created = server
        .call_method("session.create", json!({ "permissionMode": "yolo" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the turn runs and the provider answers 401 (wait mode)
    let error = server
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "hello" }),
        )
        .await
        .expect_err("the 401 turn must fail");
    assert!(
        error.to_string().contains("401"),
        "the surfaced error is the auth failure: {error}"
    );

    // Then the failure map names the provider with the redacted message
    let noted = {
        let failures = server
            .provider_auth_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        failures.get("test-provider").cloned()
    };
    assert_eq!(
        noted.as_deref(),
        Some("OpenAI API error 401: invalid api key [REDACTED]"),
        "the note records the attributed failure, redacted"
    );

    // And credential.set consumes the signal (D.2)
    server
        .call_method(
            "credential.set",
            json!({ "providerId": "test-provider", "value": { "type": "key", "key": "sk-fresh" } }),
        )
        .await
        .expect("credential.set succeeds");
    let status = server
        .call_method("credential.status", json!({ "providerId": "test-provider" }))
        .await
        .expect("status answers");
    assert_eq!(status, json!({ "status": "normal" }));
}

/// Given a turn failing with an attributed 401 provider auth error, when a
/// fire-mode `message.send` publishes `turn.failed` and returns, then the
/// failure map names the provider (D.5/O-5 — the fire arm; the note runs
/// before the TurnFailed publish so the `credential.status` a client
/// consults on seeing the event never races the note).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_mark_needs_reconfigure_when_fire_mode_turn_fails_with_auth_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let llm = Arc::new(AuthFailingLlm::new(Some("test-provider")));
    let server = credential_test_server_with_factory(
        &home,
        None,
        Arc::new(StaticLlmFactory {
            llm: Arc::clone(&llm) as Arc<dyn mirri_core::Llm>,
        }),
    );

    let created = server
        .call_method("session.create", json!({ "permissionMode": "yolo" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the turn runs and the provider answers 401 (fire mode)
    server
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "hello", "mode": "fire" }),
        )
        .await
        .expect("fire dispatch succeeds");

    // Then the note lands on the fire task's failure arm — polling the map
    // until the deadline proves the wiring (the map, not the event, is the
    // asserted face; the TurnFailed publish itself is covered elsewhere).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let noted = loop {
        let noted = {
            let failures = server
                .provider_auth_failures
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            failures.get("test-provider").cloned()
        };
        if let Some(message) = noted {
            break message;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fire arm must note the attributed failure"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(
        noted,
        "OpenAI API error 401: invalid api key [REDACTED]",
        "the note records the attributed failure, redacted"
    );
}

/// Given a turn failing with an UNATTRIBUTED auth error, when the surfaced
/// error reaches the wiring, then nothing is noted — `None` attribution is
/// the ADR-009 boundary and the status stays honestly normal (D.5).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_not_report_when_attribution_is_none() {
    let home = tempfile::tempdir().expect("tempdir");
    let llm = Arc::new(AuthFailingLlm::new(None));
    let server = credential_test_server_with_factory(
        &home,
        None,
        Arc::new(StaticLlmFactory {
            llm: Arc::clone(&llm) as Arc<dyn mirri_core::Llm>,
        }),
    );

    let created = server
        .call_method("session.create", json!({ "permissionMode": "yolo" }))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the turn runs and the provider answers 401 with no attribution
    let _ = server
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "hello" }),
        )
        .await;

    // Then no provider has a recorded failure — the honest no-evidence state
    let failures = server
        .provider_auth_failures
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        failures.is_empty(),
        "an unattributed failure must never be reported: {failures:?}"
    );
}

/// Given a store holding an external entry whose source resolves, when the
/// credential is resolved, then the minted token answers — the success
/// path keeps working under the no-fallback contract (V5's flip side).
#[test]
#[cfg_attr(not(unix), ignore)]
fn should_return_the_external_token_when_the_source_resolves() {
    use std::io::Write as _;

    // Given a stub `az` binary on the PATH that prints an accessToken, and
    // a store holding an external entry with a usable resource_name
    let bin = tempfile::tempdir().expect("bindir");
    let az = bin.path().join("az");
    let mut script = std::fs::File::create(&az).expect("create stub az");
    writeln!(
        script,
        "#!/bin/sh\nprintf '{{\"accessToken\":\"tok-external\"}}'\nexit 0"
    )
    .expect("write stub az");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&az, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub az");
    }

    let home = tempfile::tempdir().expect("tempdir");
    let store = CredentialStore::open(home.path()).expect("store opens");
    store
        .set(
            "openai",
            CredentialValue::External {
                method_id: "azure-cli".into(),
                metadata: std::collections::HashMap::from([(
                    "resource_name".to_string(),
                    json!("https://management.azure.com"),
                )]),
            },
        )
        .expect("store set");

    // When the credential is resolved with the stub's directory first on
    // the PATH (capture-and-restore env hygiene)
    let resolved = {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior_path = std::env::var_os("PATH");
        let prior_key = std::env::var("OPENAI_API_KEY").ok();
        let stub_path = format!("{}:", bin.path().display());
        unsafe {
            std::env::set_var(
                "PATH",
                format!("{stub_path}{}", prior_path.as_deref().unwrap_or_default().to_string_lossy()),
            )
        };
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-env") };
        let resolved = resolve_provider_credential(
            Some(&store),
            "openai",
            &openai_provider_record(),
            "openai",
        );
        match prior_path {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        match prior_key {
            Some(value) => unsafe { std::env::set_var("OPENAI_API_KEY", value) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
        resolved
    };

    // Then the external source's token answered, not the env chain's key
    assert_eq!(resolved, Ok(Some("tok-external".to_string())));
}


/// Given a seeded openai provider config, when the config-aware factory
/// constructs the adapter, then the adapter carries the provider's
/// attribution — the real injection point the surfaced auth failure's
/// provider id comes from (O-5, ADR-009; D.5 chain's most fragile link:
/// a factory forgetting the injection would silently report nothing).
#[test]
fn should_carry_provider_id_from_the_real_factory_constructed_adapter() {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use mirri_core::config::ServerConfig;
    use mirri_core::{AgentScopeConfig, LlmFactory, ModelAlias, ProviderEntry, ProviderType};

    // Given a config whose `ultra` alias resolves through the `openai`
    // provider, with the key supplied so the construction succeeds
    let config = Arc::new(RwLock::new(ServerConfig {
        models: Some(HashMap::from([(
            "ultra".to_string(),
            ModelAlias {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                max_context_size: 128000,
                ..Default::default()
            },
        )])),
        providers: Some(HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                r#type: Some("openai".to_string()),
                env: Some(HashMap::from([(
                    "OPENAI_API_KEY".to_string(),
                    "sk-test".to_string(),
                )])),
                ..Default::default()
            },
        )])),
        ..Default::default()
    }));
    let factory = ConfigAwareLlmFactory::new(config, None);

    // When the factory constructs the adapter for the alias
    let llm = factory
        .create_llm(&AgentScopeConfig {
            provider_type: ProviderType::OpenAi,
            model_id: "ultra".to_string(),
            ..Default::default()
        })
        .expect("the adapter is constructed");

    // Then the adapter attributes to the config provider's id
    assert_eq!(llm.provider_id(), Some("openai"));
}

// --- Task 5: the credential RPC surface ----------------------------------
//
// The four embedded routes (list / set / remove / status) over the same
// home-rooted store the factory resolves through, plus the provider.delete
// cascade and the emit/status bookkeeping. The server samples mirror the
// `test_server` helper: a bare workspace + config, with `with_home_dir`
// pinning the credential store to a tempdir.

use mirri_transport::adapter::EngineAdapter;

use super::super::credential_handlers::redact_secrets;
use super::super::{EmbeddedServer, SessionManager, WorkspaceScope};

fn credential_test_server(
    home: &tempfile::TempDir,
    seed_config: Option<&str>,
) -> EmbeddedServer {
    let workspace = std::sync::Arc::new(WorkspaceScope::new(
        mirri_core::WorkspaceId::new("cred-ws"),
        home.path().to_path_buf(),
        mirri_core::WorkspaceScopeConfig::default(),
    ));
    let manager = std::sync::Arc::new(SessionManager::new(workspace));
    // The config file always exists so the write pipeline (provider.set /
    // provider.delete) has a path — an empty config seeds nothing.
    let config_path = home.path().join("config.toml");
    if let Some(toml) = seed_config {
        std::fs::write(&config_path, toml).expect("seed config write");
    } else {
        std::fs::write(&config_path, "").expect("empty config write");
    }
    let config = std::sync::Arc::new(std::sync::RwLock::new(
        mirri_core::config::ServerConfig::load(&config_path, &mirri_os::LocalSyncFs)
            .expect("seed config loads"),
    ));
    EmbeddedServer::with_providers(
        manager,
        None,
        None,
        None,
        None,
        config,
        Some(config_path),
    )
    .with_home_dir(Some(home.path().to_path_buf()))
}

/// Given a server with a stored credential, when the RPC surface is
/// round-tripped, then no key material ever crosses the wire and the
/// remove path empties the list (F.1).
#[tokio::test]
async fn should_roundtrip_credential_rpc_without_leaking_plaintext() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);

    server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "key", "key": "sk-secret" } }),
        )
        .await
        .expect("credential.set succeeds");

    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    let text = list.to_string();
    assert!(text.contains("openai"), "the list names the provider: {list}");
    assert!(
        !text.contains("sk-secret"),
        "the list must never carry the key material: {list}"
    );
    assert_eq!(list[0]["type"], "key");
    assert!(list[0]["time_created"].is_u64());

    server
        .call_method("credential.remove", json!({ "providerId": "openai" }))
        .await
        .expect("credential.remove succeeds");
    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    assert_eq!(list, json!([]), "the remove emptied the store: {list}");
}

/// Given an unknown credential type tag, when `credential.set` is
/// called, then the call is invalid_params — never a silent guess (the
/// closed union contract, D-M4).
#[tokio::test]
async fn should_reject_unknown_credential_type_tag() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);

    let error = server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "carrier-pigeon" } }),
        )
        .await
        .expect_err("an unknown type tag must fail");
    match &error {
        mirri_transport::adapter::EngineError::Rpc { code, .. } => assert_eq!(
            *code, -32602,
            "the failure is the invalid-params domain code: {error}"
        ),
        other => panic!("expected an invalid-params RPC error, got: {other}"),
    }
}

/// Given a recorded auth failure, when `credential.status` is read and
/// then the credential is re-set, then the status answers
/// needs_reconfigure with the redacted message first and normal after
/// the write (D.2).
#[tokio::test]
async fn should_report_needs_reconfigure_after_auth_failure_and_clear_on_set() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);

    // The initial state is honestly normal (no failure recorded).
    let status = server
        .call_method("credential.status", json!({ "providerId": "openai" }))
        .await
        .expect("status answers");
    assert_eq!(status, json!({ "status": "normal" }));

    super::super::credential_handlers::note_provider_auth_failure(
        &server.provider_auth_failures,
        "openai",
        "401: Incorrect API key provided: sk-abc123def456ghi789",
    );
    let status = server
        .call_method("credential.status", json!({ "providerId": "openai" }))
        .await
        .expect("status answers");
    assert_eq!(status["status"], "needs_reconfigure");
    let message = status["message"].as_str().expect("message present");
    assert!(
        !message.contains("sk-abc123def456ghi789"),
        "the status message never carries the secret: {message}"
    );

    // A successful write clears the flag (D.2).
    server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "key", "key": "sk-fresh" } }),
        )
        .await
        .expect("credential.set succeeds");
    let status = server
        .call_method("credential.status", json!({ "providerId": "openai" }))
        .await
        .expect("status answers");
    assert_eq!(status, json!({ "status": "normal" }));
}

/// Given a live subscriber, when a credential is written, then the
/// `credential.updated` notification reaches the event stream with the
/// provider id (E.3).
#[tokio::test]
async fn should_emit_credential_updated_notification_on_set() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);
    let mut events = server.subscribe_events().await.expect("subscribes");

    server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "key", "key": "sk-secret" } }),
        )
        .await
        .expect("credential.set succeeds");

    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        events.recv(),
    )
    .await
    .expect("a notification arrives")
    .expect("the channel is live");
    assert_eq!(notification.method, "credential.updated");
    assert_eq!(notification.params["provider_id"], "openai");

    // The remove path notifies too (E.3).
    server
        .call_method("credential.remove", json!({ "providerId": "openai" }))
        .await
        .expect("credential.remove succeeds");
    let notification = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        events.recv(),
    )
    .await
    .expect("a notification arrives")
    .expect("the channel is live");
    assert_eq!(notification.method, "credential.updated");
}

/// Given a store holding a key and an external credential plus a recorded
/// auth failure on the key's provider, when `credential.list` is read,
/// then each row carries a `status` reflecting the same in-memory view as
/// `credential.status` (unknown→normal semantics) while only the external
/// row carries `external_available` (V13/G.1 — the two facts stay separate).
#[tokio::test]
async fn should_expose_external_availability_and_status_in_list() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);

    server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "key", "key": "sk-secret" } }),
        )
        .await
        .expect("credential.set succeeds");
    server
        .call_method(
            "credential.set",
            json!({
                "providerId": "azure",
                "value": { "type": "external", "method_id": "azure-cli", "metadata": {} }
            }),
        )
        .await
        .expect("credential.set succeeds");
    // A recorded auth failure on the key provider — the list must surface
    // the same verdict a `credential.status` single query would.
    super::super::credential_handlers::note_provider_auth_failure(
        &server.provider_auth_failures,
        "openai",
        "401: bad key",
    );

    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    let text = list.to_string();
    assert!(
        !text.contains("sk-secret"),
        "the list must never carry the key material: {list}"
    );

    let openai = list
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["provider_id"] == "openai")
        .expect("openai row present")
        .clone();
    let azure = list
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["provider_id"] == "azure")
        .expect("azure row present")
        .clone();

    // status: same source as the single-query face — the failure reads
    // needs_reconfigure; a provider with no recorded failure reads normal.
    assert_eq!(openai["status"], "needs_reconfigure");
    assert_eq!(azure["status"], "normal");

    // external_available: only on the external row, answering a real
    // availability probe; the key row carries no such field at all.
    assert!(
        azure.get("external_available").is_some(),
        "the external row carries external_available: {azure}"
    );
    assert!(
        azure["external_available"].is_boolean(),
        "external_available is a boolean flag: {azure}"
    );
    assert!(
        openai.get("external_available").is_none(),
        "a key row carries no external_available field: {openai}"
    );
}

/// Given failure texts carrying each secret shape, when the redaction
/// runs, then every secret collapses to the marker while the diagnostic
/// text survives (F.2 — the pure function level).
#[test]
fn should_redact_secret_patterns_in_status_message() {
    // The sk- API-key shape.
    assert_eq!(
        redact_secrets("401: Incorrect API key provided: sk-abc123def456ghi789"),
        "401: Incorrect API key provided: [REDACTED]"
    );
    // The Bearer token shape.
    assert_eq!(
        redact_secrets("request rejected: Bearer eyJhbGciOi.abc123 rejected by upstream"),
        "request rejected: Bearer [REDACTED] rejected by upstream"
    );
    // The private-key PEM block.
    assert_eq!(
        redact_secrets("bad key: -----BEGIN RSA PRIVATE KEY-----"),
        "bad key: [REDACTED]"
    );
    // Plain diagnostics pass through untouched.
    assert_eq!(
        redact_secrets("401: authentication failed for provider openai"),
        "401: authentication failed for provider openai"
    );
}

/// Given a config provider with a stored credential, when the provider
/// is deleted, then the cascade removes the credential too (A.5b/V11).
#[tokio::test]
async fn should_remove_credential_when_provider_deleted() {
    let home = tempfile::tempdir().expect("tempdir");
    let seed = r#"
[providers.openai]
type = "openai"
"#;
    let server = credential_test_server(&home, Some(seed));

    server
        .call_method(
            "credential.set",
            json!({ "providerId": "openai", "value": { "type": "key", "key": "sk-secret" } }),
        )
        .await
        .expect("credential.set succeeds");
    server
        .call_method("provider.delete", json!({ "name": "openai" }))
        .await
        .expect("provider.delete succeeds");

    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    assert_eq!(list, json!([]), "the cascade emptied the store: {list}");
}

/// Given a provider with a recorded auth failure, when the provider is
/// deleted, then the cascade clears the in-memory failure flag too — a
/// re-created provider with the same id must not inherit the stale
/// needs_reconfigure signal (the ghost-entry guard).
#[tokio::test]
async fn should_clear_auth_failure_when_provider_deleted() {
    let home = tempfile::tempdir().expect("tempdir");
    let seed = r#"
[providers.openai]
type = "openai"
"#;
    let server = credential_test_server(&home, Some(seed));

    super::super::credential_handlers::note_provider_auth_failure(
        &server.provider_auth_failures,
        "openai",
        "401: Incorrect API key provided: sk-abc123def456ghi789",
    );
    let status = server
        .call_method("credential.status", json!({ "providerId": "openai" }))
        .await
        .expect("status answers");
    assert_eq!(status["status"], "needs_reconfigure");

    server
        .call_method("provider.delete", json!({ "name": "openai" }))
        .await
        .expect("provider.delete succeeds");

    let status = server
        .call_method("credential.status", json!({ "providerId": "openai" }))
        .await
        .expect("status answers");
    assert_eq!(
        status,
        json!({ "status": "normal" }),
        "the cascade cleared the failure flag: {status}"
    );
}

/// Given a stored credential whose provider id has no config section,
/// when `credential.list` runs, then the entry is still listed (the
/// store is the source of truth for what it holds) — the orphan path is
/// exercised end-to-end with the list the observable face (A.5b).
#[tokio::test]
async fn should_list_orphan_credential_entries_with_the_diagnostic_path() {
    let home = tempfile::tempdir().expect("tempdir");
    let server = credential_test_server(&home, None);

    server
        .call_method(
            "credential.set",
            json!({ "providerId": "ghost", "value": { "type": "key", "key": "sk-orphan" } }),
        )
        .await
        .expect("credential.set succeeds");

    // The config has no [providers.ghost] section — the entry is an
    // orphan (the warn fires on the list path); it is still listed.
    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    assert_eq!(list[0]["provider_id"], "ghost");
    assert!(
        !list.to_string().contains("sk-orphan"),
        "no key material on the wire: {list}"
    );
}

/// Given the migration's start-up wiring, when a config.toml carries a
/// typed `providers.*.api_key`, then the assemble path moves it into the
/// store and strips the field (E.2/E.6 — exercised at the migrate
/// function level; the bootstrap call is the same function).
#[test]
fn should_move_config_api_keys_into_the_store_on_assemble_inputs() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[providers.openai]
type = "openai"
api_key = "sk-from-config"
"#,
    )
    .expect("seed config write");

    let store = CredentialStore::open(home.path()).expect("store opens");
    let report =
        mirri_credentials::migrate_config_api_keys(&store, &config_path).expect("migration runs");
    assert_eq!(report.migrated, vec!["openai".to_string()]);
    assert_eq!(store.reveal("openai").as_deref(), Some("sk-from-config"));
    let text = std::fs::read_to_string(&config_path).expect("config re-read");
    assert!(!text.contains("sk-from-config"), "the field was stripped");
}

/// Given a provider.set carrying a non-empty apiKey field, when the record
/// is written, then the key is runtime-adopted into the credential store
/// (R2): config.toml carries no api_key field, the store holds the entry,
/// and the catalog face reports the provider credentialed.
#[tokio::test]
async fn should_adopt_the_api_key_into_the_store_when_provider_set_carries_one() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join("config.toml");
    let server = credential_test_server(&home, None);

    server
        .call_method(
            "provider.set",
            json!({ "name": "openai", "config": {
                "type": "openai",
                "apiKey": "sk-set-key",
                "baseUrl": "https://api.example.test",
            }}),
        )
        .await
        .expect("provider.set succeeds");

    // Then the on-disk config carries no api_key field (no plaintext, no
    // double-write) while the rest of the record round-trips
    let disk = std::fs::read_to_string(&config_path).expect("config re-read");
    assert!(!disk.contains("sk-set-key"), "no plaintext key on disk: {disk}");
    assert!(!disk.contains("api_key"), "no api_key field at all: {disk}");
    assert!(disk.contains("api.example.test"), "the record itself persisted: {disk}");

    // And the store holds the adopted entry, never on the wire in plaintext
    let list = server
        .call_method("credential.list", json!({}))
        .await
        .expect("credential.list succeeds");
    assert_eq!(list[0]["provider_id"], "openai");
    assert!(!list.to_string().contains("sk-set-key"), "never the plaintext: {list}");

    // And the catalog face reports the provider credentialed
    let store = CredentialStore::open(home.path()).expect("store opens");
    assert_eq!(store.reveal("openai").as_deref(), Some("sk-set-key"));
}

// --- O-5 surfaced-failure wiring test bed ------------------------------------
//
// The auth-failure arm needs a real engine turn failing with a classified
// ProviderAuthError, so the store tests grow their own factory-backed
// server variant and a 401-always mock — the same shape the b1 lifecycle
// bed builds with `ScriptedLlmFactory`, plus the `provider_id` attribution
// Task 1 threaded through the adapters (the mock implements the default
// `is_retryable_error` = false, so a 401 chat error classifies to
// ProviderAuthError without retrying).

use mirri_core::llm::{LlmChatParams, ModelCapability};
use mirri_core::{EngineResult, Llm, LlmEventStream, LlmFactory};
use std::sync::Arc;

/// The `credential_test_server` shape plus an LLM factory — the wait/fire
/// turn arms need one, and the config file keeps the same semantics (an
/// empty config seeds nothing).
fn credential_test_server_with_factory(
    home: &tempfile::TempDir,
    seed_config: Option<&str>,
    llm_factory: Arc<dyn LlmFactory>,
) -> EmbeddedServer {
    let workspace = std::sync::Arc::new(WorkspaceScope::new(
        mirri_core::WorkspaceId::new("cred-ws"),
        home.path().to_path_buf(),
        mirri_core::WorkspaceScopeConfig::default(),
    ));
    let manager = std::sync::Arc::new(SessionManager::new(workspace));
    let config_path = home.path().join("config.toml");
    if let Some(toml) = seed_config {
        std::fs::write(&config_path, toml).expect("seed config write");
    } else {
        std::fs::write(&config_path, "").expect("empty config write");
    }
    let mut config =
        mirri_core::config::ServerConfig::load(&config_path, &mirri_os::LocalSyncFs)
            .expect("seed config loads");
    // The create chain requires a resolvable model (spec 11 R4): the
    // factory-backed turn tests need SOME default, exactly like the bare
    // test-server helpers' seed.
    if config.default_model.is_none() {
        config.default_model = Some("test-default-model".to_string());
    }
    EmbeddedServer::with_providers(
        manager,
        Some(llm_factory),
        None,
        None,
        None,
        std::sync::Arc::new(std::sync::RwLock::new(config)),
        Some(config_path),
    )
    .with_home_dir(Some(home.path().to_path_buf()))
}

/// An [`LlmFactory`] answering one fixed LLM instance.
struct StaticLlmFactory {
    llm: Arc<dyn Llm>,
}

impl LlmFactory for StaticLlmFactory {
    fn create_llm(&self, _config: &mirri_core::AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        Ok(Arc::clone(&self.llm))
    }
}

/// An LLM whose every chat call fails with a 401 provider error — the
/// message wording is a shape `ProviderErrorInfo::from_message` classifies
/// as `provider.auth_error` (the status code and the "invalid api key"
/// wording both hit the table), so `chat_with_retry` surfaces it as a
/// `ProviderAuthError` attributed to `provider_id`.
struct AuthFailingLlm {
    provider_id: Option<&'static str>,
}

impl AuthFailingLlm {
    fn new(provider_id: Option<&'static str>) -> Self {
        Self { provider_id }
    }
}

impl Llm for AuthFailingLlm {
    fn provider_id(&self) -> Option<&str> {
        self.provider_id
    }

    fn system_prompt(&self) -> &str {
        "You are a failing test provider."
    }

    fn model_name(&self) -> &str {
        "auth-failing-llm"
    }

    fn capability(&self) -> &ModelCapability {
        static CAPABILITY: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAPABILITY
    }

    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        Err(mirri_core::error::EngineError::ProviderError(
            "OpenAI API error 401: invalid api key sk-AbCdEf123456".to_string(),
        ))
    }
}
