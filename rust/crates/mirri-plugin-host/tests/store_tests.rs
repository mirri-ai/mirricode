//! Integration tests for `PluginStore` — installed.json read/write, add,
//! remove, set_enabled, and the TS v1 format (item A6).

use chrono::Utc;
use mirri_plugin_host::{InstalledPlugin, PluginError, PluginSource, PluginStore};
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_plugin(id: &str) -> InstalledPlugin {
    InstalledPlugin {
        id: id.to_string(),
        version: Some("1.0.0".to_string()),
        source: PluginSource::Local {
            path: "/source".to_string(),
        },
        original_source: None,
        enabled: true,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins").join(id),
        github_ref: None,
        installed_sha: None,
        mcp_servers: HashMap::new(),
    }
}

#[test]
fn should_return_empty_list_when_no_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    assert!(store.load().unwrap().is_empty());
}

#[test]
fn should_load_plugins_when_added_to_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.add(sample_plugin("beta")).unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 2);
}

#[test]
fn should_reject_add_when_plugin_already_installed() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    let result = store.add(sample_plugin("alpha"));
    assert!(matches!(result, Err(PluginError::AlreadyInstalled(_))));
}

#[test]
fn should_remove_plugin_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.remove("alpha").unwrap();
    assert!(store.load().unwrap().is_empty());
}

#[test]
fn should_return_error_when_removing_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let result = store.remove("ghost");
    assert!(matches!(result, Err(PluginError::NotFound(_))));
}

#[test]
fn should_toggle_enabled_when_setting_enabled_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.set_enabled("alpha", false).unwrap();

    let plugin = store.find("alpha").unwrap().unwrap();
    assert!(!plugin.enabled);

    store.set_enabled("alpha", true).unwrap();
    let plugin = store.find("alpha").unwrap().unwrap();
    assert!(plugin.enabled);
}

#[test]
fn should_return_error_when_setting_enabled_for_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let result = store.set_enabled("ghost", true);
    assert!(matches!(result, Err(PluginError::NotFound(_))));
}

#[test]
fn should_return_none_when_finding_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    assert!(store.find("ghost").unwrap().is_none());
}

#[test]
fn should_persist_plugins_when_reloaded_from_disk() {
    let dir = tempfile::tempdir().unwrap();

    // First store instance adds a plugin.
    {
        let store = PluginStore::new(dir.path());
        store.add(sample_plugin("alpha")).unwrap();
    }

    // Second store instance should see it.
    {
        let store = PluginStore::new(dir.path());
        let plugins = store.load().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "alpha");
    }
}

#[test]
fn should_serialize_plugins_when_source_types_differ() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    let github_plugin = InstalledPlugin {
        id: "gh-plugin".to_string(),
        version: None,
        source: PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("abc123".to_string()),
            kind: None,
        },
        original_source: Some("github:user/repo@abc123".to_string()),
        enabled: true,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins/gh-plugin"),
        github_ref: Some(mirri_plugin_host::GithubRef::branch("main")),
        installed_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        mcp_servers: HashMap::new(),
    };
    let zip_plugin = InstalledPlugin {
        id: "zip-plugin".to_string(),
        version: None,
        source: PluginSource::Zip {
            url: "https://example.com/p.zip".to_string(),
        },
        original_source: Some("https://example.com/p.zip".to_string()),
        enabled: false,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins/zip-plugin"),
        github_ref: None,
        installed_sha: None,
        mcp_servers: HashMap::new(),
    };

    store.add(github_plugin).unwrap();
    store.add(zip_plugin).unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 2);
    let gh = plugins.iter().find(|p| p.id == "gh-plugin").unwrap();
    assert_eq!(
        gh.source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: None,
            kind: None,
        }
    );
    assert_eq!(gh.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        gh.installed_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(gh.original_source.as_deref(), Some("github:user/repo@abc123"));
    let zip = plugins.iter().find(|p| p.id == "zip-plugin").unwrap();
    assert_eq!(
        zip.source,
        PluginSource::Zip {
            url: "https://example.com/p.zip".to_string()
        }
    );
    assert!(!zip.enabled);
}

// ---------------------------------------------------------------------------
// A6 — the TS v1 installed.json format
// ---------------------------------------------------------------------------

/// Given a store write, when the file is inspected on disk, then it is the
/// TS v1 envelope `{version: 1, plugins: [...]}` with camelCase records —
/// the bidirectional interop shape of item A6.
#[test]
fn should_write_the_ts_v1_envelope_when_saving_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();

    let raw = std::fs::read_to_string(dir.path().join("installed.json")).unwrap();
    let file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(file["version"], serde_json::json!(1));
    let record = &file["plugins"][0];
    assert_eq!(record["id"], serde_json::json!("alpha"));
    // The envelope carries native separators (`/plugins\alpha` on Windows).
    let expected_root = std::path::PathBuf::from("/plugins").join("alpha");
    assert_eq!(
        record["root"],
        serde_json::json!(expected_root.display().to_string())
    );
    assert_eq!(record["source"], serde_json::json!("local-path"));
    assert_eq!(record["enabled"], serde_json::json!(true));
    assert!(record["installedAt"].is_string());
    assert!(record["updatedAt"].is_string());
    assert!(record.get("path").is_none(), "no legacy path field");
    assert!(record.get("version").is_none(), "no version field (TS)");
}

/// Given a TS-written v1 file, when the store loads it, then every field
/// maps onto the record — the TS → rust direction of the bidirectional
/// interop (item A6): source literals, capabilities-wrapped per-server
/// state, and the github attribution.
#[test]
fn should_load_a_ts_written_v1_file_when_reading_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let ts_file = serde_json::json!({
        "version": 1,
        "plugins": [
            {
                "id": "ts-plugin",
                "root": "/home/u/.mirri/plugins/managed/ts-plugin",
                "source": "github",
                "enabled": false,
                "installedAt": "2026-06-01T00:00:00Z",
                "originalSource": "https://github.com/mirricode/demo-plugin/tree/main",
                "capabilities": {
                    "mcpServers": {
                        "helper": { "enabled": false, "enabledTools": ["t1"], "disabledTools": ["t2"] }
                    }
                },
                "github": {
                    "owner": "mirricode",
                    "repo": "demo-plugin",
                    "ref": { "kind": "branch", "value": "main" },
                    "installedSha": "0123456789abcdef0123456789abcdef01234567"
                }
            },
            {
                "id": "local-plugin",
                "root": "/home/u/.mirri/plugins/managed/local-plugin",
                "source": "local-path",
                "enabled": true,
                "installedAt": "2026-06-01T00:00:00Z",
                "updatedAt": "2026-06-02T00:00:00Z"
            },
            {
                "id": "zip-plugin",
                "root": "/home/u/.mirri/plugins/managed/zip-plugin",
                "source": "zip-url",
                "enabled": true,
                "installedAt": "2026-06-01T00:00:00Z",
                "originalSource": "https://example.com/p.zip"
            }
        ]
    });
    std::fs::write(
        dir.path().join("installed.json"),
        serde_json::to_string_pretty(&ts_file).unwrap(),
    )
    .unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 3);

    let gh = plugins.iter().find(|p| p.id == "ts-plugin").unwrap();
    assert_eq!(
        gh.source,
        PluginSource::Github {
            repo: "mirricode/demo-plugin".to_string(),
            commit: None,
            kind: None,
        }
    );
    assert!(!gh.enabled);
    assert_eq!(
        gh.original_source.as_deref(),
        Some("https://github.com/mirricode/demo-plugin/tree/main")
    );
    // No updatedAt in the TS row → the record follows installedAt.
    assert_eq!(gh.updated_at, gh.installed_at);
    assert_eq!(
        gh.mcp_servers.get("helper"),
        Some(&mirri_plugin_host::PluginMcpServerState {
            enabled: false,
            enabled_tools: Some(vec!["t1".to_string()]),
            disabled_tools: Some(vec!["t2".to_string()]),
        })
    );
    assert_eq!(gh.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        gh.installed_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(
        gh.path,
        PathBuf::from("/home/u/.mirri/plugins/managed/ts-plugin")
    );

    let local = plugins.iter().find(|p| p.id == "local-plugin").unwrap();
    assert_eq!(local.source, PluginSource::Local { path: String::new() });
    // The explicit updatedAt row parses on its own.
    assert_eq!(
        local.updated_at.to_rfc3339(),
        "2026-06-02T00:00:00+00:00"
    );

    let zip = plugins.iter().find(|p| p.id == "zip-plugin").unwrap();
    assert_eq!(
        zip.source,
        PluginSource::Zip {
            url: "https://example.com/p.zip".to_string()
        }
    );

    // The file round-trips: a rust write rewrites the same v1 envelope.
    store.save(&plugins).unwrap();
    let round_trip = store.load().unwrap();
    assert_eq!(round_trip.len(), 3);
}

/// Given a pre-A6 bare-array installed.json (snake_case records, tagged
/// source objects), when the store loads it, then the records migrate with
/// the old flat fields mapped onto the record — the rust legacy → v1
/// migration of item A6.
#[test]
fn should_migrate_a_legacy_bare_array_when_reading_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let legacy = serde_json::json!([
        {
            "id": "old-plugin",
            "version": "1.0.0",
            "source": {"type": "github", "repo": "user/repo", "commit": "main"},
            "enabled": true,
            "installed_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "path": "/plugins/old-plugin",
            "github_ref": {"kind": "branch", "value": "main"},
            "installed_sha": "0123456789abcdef0123456789abcdef01234567",
            "mcp_servers": {
                "helper": {"enabled": false, "enabled_tools": ["t1"]}
            }
        }
    ]);
    std::fs::write(
        dir.path().join("installed.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 1);
    let plugin = &plugins[0];
    assert_eq!(plugin.version.as_deref(), Some("1.0.0"));
    assert_eq!(
        plugin.source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("main".to_string()),
            kind: None,
        }
    );
    assert_eq!(plugin.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        plugin.mcp_servers.get("helper"),
        Some(&mirri_plugin_host::PluginMcpServerState {
            enabled: false,
            enabled_tools: Some(vec!["t1".to_string()]),
            disabled_tools: None,
        })
    );

    // The next save migrates the file to the v1 envelope.
    store.save(&plugins).unwrap();
    let raw = std::fs::read_to_string(dir.path().join("installed.json")).unwrap();
    let file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(file["version"], serde_json::json!(1));
    assert_eq!(file["plugins"][0]["root"], serde_json::json!("/plugins/old-plugin"));
    assert_eq!(
        file["plugins"][0]["github"]["owner"],
        serde_json::json!("user")
    );
    assert_eq!(
        file["plugins"][0]["capabilities"]["mcpServers"]["helper"]["enabledTools"],
        serde_json::json!(["t1"])
    );
}

/// Given an installed record, when the store upserts the same id, then the
/// record is replaced in place (item A2) — and a fresh id is appended.
#[test]
fn should_replace_record_when_upserting_an_existing_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();
    store.upsert(sample_plugin("alpha")).unwrap();
    assert_eq!(store.load().unwrap().len(), 1, "upsert never duplicates");

    store.upsert(sample_plugin("beta")).unwrap();
    assert_eq!(store.load().unwrap().len(), 2);
}

/// Given an installed plugin, when set_enabled is called with the value it
/// already holds, then nothing is written — the TS `setEnabled` early
/// return (item C1): no `updatedAt` refresh, no file rewrite.
#[test]
fn should_not_rewrite_when_set_enabled_value_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();
    let before = store.find("alpha").unwrap().unwrap();

    let changed = store.set_enabled("alpha", true).unwrap();
    assert!(!changed, "same value reports no change");

    let after = store.find("alpha").unwrap().unwrap();
    assert_eq!(after.updated_at, before.updated_at, "updated_at must not move");

    let changed = store.set_enabled("alpha", false).unwrap();
    assert!(changed, "a real toggle still reports a change");
}
