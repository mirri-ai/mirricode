//! Integration tests for wire protocol version migration.
//!
//! These tests exercise the full migration chain, version detection, and
//! the atomic file-rewrite path using temporary `wire.jsonl` files.

use mirri_persistence::{
    CURRENT_VERSION, WireMigration, detect_version, is_forward_version, migrate_log_file,
    migrate_record, resolve_migrations,
};
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 1. v1.0 → v1.1: flatten function fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_flatten_function_fields_when_migrating_v1_0_to_v1_1() {
    // Given: a v1.0 record with nested function.name / function.arguments
    let mut record = json!({
        "op": "context.append_message",
        "toolCalls": [
            { "id": "1", "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" } }
        ]
    });

    // When: the v1.0→v1.1 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_0ToV1_1;
    migration.migrate_record(&mut record).unwrap();

    // Then: function.name and function.arguments are flattened to top-level
    assert_eq!(
        record,
        json!({
            "op": "context.append_message",
            "toolCalls": [
                { "id": "1", "name": "get_weather", "arguments": "{\"city\":\"SF\"}" }
            ]
        })
    );
}

// ---------------------------------------------------------------------------
// 2. v1.1 → v1.2: derive sessionApprovalRule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_derive_session_approval_rule_when_migrating_v1_1_to_v1_2() {
    // Given: a v1.1 permission record with action + toolName
    let mut record = json!({
        "op": "permission.record_approval_result",
        "action": "allow",
        "toolName": "bash"
    });

    // When: the v1.1→v1.2 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_1ToV1_2;
    migration.migrate_record(&mut record).unwrap();

    // Then: sessionApprovalRule is derived from action + toolName
    assert_eq!(
        record.get("sessionApprovalRule"),
        Some(&json!({ "rule": "allow", "toolName": "bash" }))
    );
}

// ---------------------------------------------------------------------------
// 3. v1.2 → v1.3: no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_be_noop_when_migrating_v1_2_to_v1_3() {
    // Given: an arbitrary record
    let mut record = json!({
        "op": "goal.create",
        "goalId": "g-1",
        "field": 42
    });
    let original = record.clone();

    // When: the v1.2→v1.3 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_2ToV1_3;
    migration.migrate_record(&mut record).unwrap();

    // Then: the record is unchanged
    assert_eq!(record, original);
}

// ---------------------------------------------------------------------------
// 4. v1.3 → v1.4: merge goal types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_merge_goal_types_when_migrating_v1_3_to_v1_4() {
    // Given: v1.3 records with goal.account_usage and goal.continuation
    let mut account_usage = json!({ "op": "goal.account_usage", "tokens": 100 });
    let mut continuation = json!({ "op": "goal.continuation", "text": "continue" });
    let mut update_with_goal_id = json!({
        "op": "goal.update",
        "goalId": "g-2",
        "status": "active"
    });
    let mut create = json!({ "op": "goal.create", "goalId": "g-3" });

    let migration = mirri_persistence::migration::MigrateV1_3ToV1_4;

    // When: the migration is applied to each
    migration.migrate_record(&mut account_usage).unwrap();
    migration.migrate_record(&mut continuation).unwrap();
    migration.migrate_record(&mut update_with_goal_id).unwrap();
    migration.migrate_record(&mut create).unwrap();

    // Then: account_usage and continuation become goal.update
    assert_eq!(account_usage["op"], "goal.update");
    assert_eq!(account_usage["tokens"], 100);
    assert_eq!(continuation["op"], "goal.update");
    assert_eq!(continuation["text"], "continue");

    // And: goalId is removed from goal.update but preserved on goal.create
    assert!(update_with_goal_id.get("goalId").is_none());
    assert_eq!(update_with_goal_id["status"], "active");
    assert_eq!(create["goalId"], "g-3");
}

// ---------------------------------------------------------------------------
// 5. v1.4 → v1.5: backfill wallClockResumedAt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_backfill_wall_clock_resumed_at_when_migrating_v1_4_to_v1_5() {
    // Given: v1.4 goal records without wallClockResumedAt
    let mut create = json!({
        "op": "goal.create",
        "time": "2024-01-01T00:00:00Z"
    });
    let mut update = json!({
        "op": "goal.update",
        "time": "2024-02-01T00:00:00Z"
    });
    let mut existing = json!({
        "op": "goal.create",
        "time": "2024-01-01T00:00:00Z",
        "wallClockResumedAt": "2024-06-01T00:00:00Z"
    });

    let migration = mirri_persistence::migration::MigrateV1_4ToV1_5;

    // When: the migration is applied
    migration.migrate_record(&mut create).unwrap();
    migration.migrate_record(&mut update).unwrap();
    migration.migrate_record(&mut existing).unwrap();

    // Then: wallClockResumedAt is backfilled from time where absent
    assert_eq!(create["wallClockResumedAt"], "2024-01-01T00:00:00Z");
    assert_eq!(update["wallClockResumedAt"], "2024-02-01T00:00:00Z");
    // And: existing wallClockResumedAt is preserved
    assert_eq!(existing["wallClockResumedAt"], "2024-06-01T00:00:00Z");
}

// ---------------------------------------------------------------------------
// 6. resolve_migrations: current version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_migrations_when_source_is_current_version() {
    // Given: the current protocol version
    // When: migrations are resolved
    let chain = resolve_migrations(CURRENT_VERSION);

    // Then: no migrations are needed
    assert!(chain.is_empty());
}

// ---------------------------------------------------------------------------
// 7. resolve_migrations: forward version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_migrations_when_source_is_forward_version() {
    // Given: a version newer than current
    // When: migrations are resolved
    let chain = resolve_migrations("1.7");

    // Then: no migrations are returned (forward-version logs are not migrated)
    assert!(chain.is_empty());
}

// ---------------------------------------------------------------------------
// 8. detect_version: envelope-less log → v1.4
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_detect_v1_4_when_first_line_is_not_metadata() {
    // Given: a first line that is a regular op, not a metadata record
    let line = r#"{"op":"goal.create","goalId":"g-1"}"#;

    // When: detecting the version
    let version = detect_version(line);

    // Then: v1.4 is assumed for envelope-less logs
    assert_eq!(version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// 9. detect_version: metadata record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_detect_version_when_metadata_record_present() {
    // Given: a metadata header line with protocolVersion
    let line = r#"{"op":"metadata","protocolVersion":"1.2"}"#;

    // When: detecting the version
    let version = detect_version(line);

    // Then: the version is read from protocolVersion
    assert_eq!(version, Some("1.2".to_string()));
}

// ---------------------------------------------------------------------------
// 10. Full chain: v1.0 → v1.5
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_migrate_full_chain_when_migrating_from_v1_0_to_v1_5() {
    // Given: a v1.0 record that is affected by multiple migrations in the chain
    let mut record = json!({
        "op": "goal.create",
        "goalId": "g-chain",
        "time": "2024-03-01T00:00:00Z"
    });

    // And: a v1.0 toolCalls record
    let mut tool_call_record = json!({
        "op": "context.append_message",
        "toolCalls": [
            { "id": "tc1", "function": { "name": "search", "arguments": "{\"q\":\"hi\"}" } }
        ]
    });

    // When: the full migration chain from v1.0 is resolved and applied
    // (now seven segments: 1.0→…→1.6 plus the identity 1.6→2.0 era re-stamp)
    let chain = resolve_migrations("1.0");
    assert_eq!(chain.len(), 7);
    migrate_record(&mut record, &chain).unwrap();
    migrate_record(&mut tool_call_record, &chain).unwrap();

    // Then: the goal record keeps goalId (create), gains wallClockResumedAt
    assert_eq!(record["op"], "goal.create");
    assert_eq!(record["goalId"], "g-chain");
    assert_eq!(record["wallClockResumedAt"], "2024-03-01T00:00:00Z");

    // And: the toolCalls record has flattened function fields
    assert_eq!(tool_call_record["toolCalls"][0]["name"], "search");
    assert_eq!(
        tool_call_record["toolCalls"][0]["arguments"],
        "{\"q\":\"hi\"}"
    );
    assert!(tool_call_record["toolCalls"][0].get("function").is_none());
}

// ---------------------------------------------------------------------------
// 11. migrate_log_file: atomic rewrite
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_atomically_rewrite_file_when_migrating_log_file() {
    // Given: a wire.jsonl file at v1.0 with a metadata header and a record
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = r#"{"op":"metadata","protocolVersion":"1.0"}"#;
    let record_line = r#"{"op":"goal.create","goalId":"g-file","time":"2024-01-01T00:00:00Z"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: the log file is migrated
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then: the source version is reported
    assert_eq!(source_version, Some("1.0".to_string()));

    // And: the file has been rewritten with the current protocol version
    let migrated = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = migrated.lines().collect();
    assert_eq!(lines.len(), 2);

    let metadata: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(metadata["type"], "metadata");
    assert_eq!(metadata["protocolVersion"], CURRENT_VERSION);
    assert_eq!(
        metadata["protocol_version"], CURRENT_VERSION,
        "the TS spelling of the version field is written too"
    );

    let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(record["type"], "goal.create");
    assert_eq!(record["goalId"], "g-file");
    assert_eq!(record["wallClockResumedAt"], "2024-01-01T00:00:00Z");

    // And: the temp file is gone
    let tmp_path = path.with_extension("jsonl.tmp");
    assert!(
        !tmp_path.exists(),
        "temp file should be cleaned up after rename"
    );
}

// ---------------------------------------------------------------------------
// 12. migrate_log_file: already current → no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_skip_migration_when_log_is_already_current() {
    // Given: a wire.jsonl file already at the current version
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = format!(r#"{{"op":"metadata","protocolVersion":"{CURRENT_VERSION}"}}"#);
    let record_line = r#"{"op":"goal.create","goalId":"g-current"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: migrate_log_file is called
    let result = migrate_log_file(&path).await.unwrap();

    // Then: no migration is performed
    assert_eq!(result, None);

    // And: the file content is unchanged
    let after = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(after, content);
}

// ---------------------------------------------------------------------------
// 13. migrate_log_file: forward version → no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_not_migrate_when_log_is_forward_version() {
    // Given: a wire.jsonl file with a forward protocol version
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = r#"{"op":"metadata","protocolVersion":"2.1"}"#;
    let record_line = r#"{"op":"goal.create","goalId":"g-fwd"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: migrate_log_file is called
    let result = migrate_log_file(&path).await.unwrap();

    // Then: no migration is performed
    assert_eq!(result, None);

    // And: the file content is unchanged
    let after = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(after, content);

    // Sanity: is_forward_version agrees
    assert!(is_forward_version("2.1"));
}
