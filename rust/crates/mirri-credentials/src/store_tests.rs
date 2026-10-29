use crate::store::{CredentialStore, CredentialValue};

fn key(k: &str) -> CredentialValue {
    CredentialValue::Key { key: k.into() }
}

#[test]
fn should_return_empty_and_create_dir_when_opened_on_fresh_home() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    assert!(store.read_all().is_empty());
    assert!(home
        .path()
        .join("credentials/credentials.json")
        .parent()
        .unwrap()
        .exists());
}

#[test]
fn should_set_then_get_roundtrip_and_persist_across_reopen() {
    let home = tempfile::tempdir().unwrap();
    {
        let store = CredentialStore::open(home.path()).unwrap();
        let rec = store.set("openai", key("sk-1")).unwrap();
        assert_eq!(rec.provider_id, "openai");
        assert!(rec.id.starts_with("cred_"));
        assert_eq!(store.get("openai").unwrap().value, key("sk-1"));
    } // drop store
    let store2 = CredentialStore::open(home.path()).unwrap();
    assert_eq!(store2.get("openai").unwrap().value, key("sk-1"));
}

#[test]
fn should_overwrite_in_place_keeping_id_and_created_when_set_twice() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    let first = store.set("openai", key("sk-1")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let second = store.set("openai", key("sk-2")).unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.time_created, first.time_created);
    assert!(second.time_updated > first.time_updated);
    assert_eq!(store.read_all().len(), 1);
    assert_eq!(store.reveal("openai").unwrap(), "sk-2");
}

#[test]
fn should_remove_and_report_absent() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    store.set("openai", key("sk-1")).unwrap();
    assert!(store.remove("openai").unwrap());
    assert!(!store.remove("openai").unwrap());
    assert!(store.get("openai").is_none());
}

#[test]
fn should_treat_corrupt_file_as_empty_after_renaming_to_corrupt() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("credentials.json"), "{ not json").unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    assert!(store.read_all().is_empty());
    assert!(dir.join("credentials.json.corrupt").exists());
}

#[test]
fn should_serialize_external_with_method_id_and_metadata() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    let v = CredentialValue::External {
        method_id: "azure-cli".into(),
        metadata: [("resource_name".to_string(), serde_json::json!("my-models"))].into(),
    };
    store.set("azure", v.clone()).unwrap();
    assert_eq!(store.get("azure").unwrap().value, v);
    // external reveal 是引用描述，不是密钥
    assert_eq!(store.reveal("azure").unwrap(), "external:azure-cli");
}

#[test]
fn should_always_write_plaintext_encoding_and_version_1() {
    let home = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    store.set("openai", key("sk-1")).unwrap();
    let raw = std::fs::read_to_string(home.path().join("credentials/credentials.json")).unwrap();
    assert!(raw.contains("\"cipher\":\"plaintext\""));
    assert!(raw.contains("\"version\":1"));
}

#[test]
fn should_skip_entries_with_unknown_cipher_without_failing() {
    // 未知 cipher → 按条目过滤丢弃（ADR-008：读取方不猜解密方式）。
    // 语义裁决：单条目 cipher 不合法 → 丢弃该条并继续（不做整库改名 .corrupt——
    // 整库改名只发生在文件 parse 失败 / version 越界的场景，见上一个测试；
    // 这与 ADR-008「每条记录独立声明使混合态天然合法」的逐条处理语义一致）。
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("credentials.json"),
        r#"{"version":1,"entries":[{"id":"cred_1","provider_id":"x","value":{"type":"key","key":"k"},"encoding":{"cipher":"aes-256-gcm"},"time_created":1,"time_updated":1},{"id":"cred_2","provider_id":"ok","value":{"type":"key","key":"k2"},"encoding":{"cipher":"plaintext"},"time_created":1,"time_updated":1}]}"#,
    )
    .unwrap();
    let store = CredentialStore::open(home.path()).unwrap();
    let all = store.read_all();
    assert_eq!(all.len(), 1); // 未知 cipher 条目被跳过
    assert_eq!(all[0].provider_id, "ok"); // 合法条目仍在
}
