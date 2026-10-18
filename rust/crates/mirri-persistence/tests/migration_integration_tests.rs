//! Migration + persistence end-to-end integration tests.
//!
//! These tests exercise the full migration pipeline: fixture `wire.jsonl`
//! files written in older protocol versions are migrated in-place via
//! [`migrate_log_file`], then verified for correct record transformations
//! and for the torn-write vs corruption read policy on legacy shapes.

use mirri_persistence::{detect_version, migrate_log_file};
use serde_json::Value;
use tempfile::TempDir;

/// Copy a fixture file into a fresh [`TempDir`] and return the destination path.
///
/// Tests migrate the file in-place, so each run must operate on its own copy
/// to avoid mutating the committed fixture.
fn copy_fixture_to_temp(fixture_rel: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("wire.jsonl");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture_rel);
    std::fs::copy(&src, &dest).unwrap();
    (dir, dest)
}

/// Read the first non-empty line of a file.
fn read_first_line(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Test 1: v1.0 fixture migration
// ---------------------------------------------------------------------------

/// Given a v1.0 wire log fixture with nested `function.{name,arguments}`
/// tool-call format, when `migrate_log_file` is called, the file should be
/// rewritten to the current protocol version with flattened tool-call fields.
#[tokio::test]
async fn should_migrate_v1_0_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.0-session/wire.jsonl");

    // When — migrate the file.
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then — the source version is reported as "1.0".
    assert_eq!(source_version, Some("1.0".to_string()));

    // Then — the metadata record's protocolVersion is bumped to "2.0".
    let first_line = read_first_line(&path);
    let metadata: Value = serde_json::from_str(&first_line).unwrap();
    assert_eq!(
        metadata.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2.0")
    );

    // Then — the toolCalls in the assistant message have flattened
    // `name` / `arguments` fields (no nested `function` object).
    let content = std::fs::read_to_string(&path).unwrap();
    let assistant_line = content
        .lines()
        .find(|l| l.contains("\"assistant\""))
        .unwrap();
    let assistant: Value = serde_json::from_str(assistant_line).unwrap();

    let tool_calls = assistant
        .get("toolCalls")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(tool_calls.len(), 1);

    let call = &tool_calls[0];
    assert_eq!(
        call.get("name").and_then(|v| v.as_str()),
        Some("get_weather")
    );
    assert_eq!(
        call.get("arguments").and_then(|v| v.as_str()),
        Some("{\"city\":\"SF\"}")
    );
    assert!(
        call.get("function").is_none(),
        "nested `function` object should be removed after migration"
    );
}

// ---------------------------------------------------------------------------
// Test 2: v1.3 fixture migration
// ---------------------------------------------------------------------------

/// Given a v1.3 wire log fixture with `goal.account_usage` and
/// `goal.continuation` ops, when `migrate_log_file` is called, both ops
/// should be rewritten to `goal.update`.
#[tokio::test]
async fn should_migrate_v1_3_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.3-session/wire.jsonl");

    // When — migrate the file.
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then — the source version is reported as "1.3".
    assert_eq!(source_version, Some("1.3".to_string()));

    // Then — every line that was `goal.account_usage` or `goal.continuation`
    // is now `goal.update`, under the current `type` discriminator.
    let content = std::fs::read_to_string(&path).unwrap();
    let mut goal_update_count = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).unwrap();
        let kind = record
            .get("type")
            .or_else(|| record.get("op"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "goal.update" {
            goal_update_count += 1;
        }
        // No line should still carry the old op names.
        assert_ne!(kind, "goal.account_usage");
        assert_ne!(kind, "goal.continuation");
    }

    // Two goal ops were in the fixture (account_usage + continuation).
    assert_eq!(
        goal_update_count, 2,
        "expected 2 goal.update ops after migration"
    );
}

// ---------------------------------------------------------------------------
// Test 3: detect v1.4 from envelope-less fixture
// ---------------------------------------------------------------------------

/// Given an envelope-less wire log (no metadata header), when `detect_version`
/// is called on the first line, it should return `Some("1.4")`.
#[tokio::test]
async fn should_detect_v1_4_when_opening_envelope_less_fixture() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/envelope-less/wire.jsonl");

    let first_line = read_first_line(&fixture_path);

    let version = detect_version(&first_line);

    assert_eq!(version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// Test 4: migrate envelope-less fixture
// ---------------------------------------------------------------------------

/// Given an envelope-less wire log fixture, when `migrate_log_file` is called,
/// the file should be migrated and the source version reported as `"1.4"`.
#[tokio::test]
async fn should_migrate_envelope_less_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/envelope-less/wire.jsonl");

    let source_version = migrate_log_file(&path).await.unwrap();

    assert_eq!(source_version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// Test 5: no re-migration when already at current version
// ---------------------------------------------------------------------------

/// Given a wire log already at the current protocol version (1.6), when
/// `migrate_log_file` is called, it should return `None` (no migration needed).
#[tokio::test]
async fn should_not_re_migrate_when_log_is_already_at_current_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(
        &path,
        r#"{"op":"metadata","protocolVersion":"1.7","agentId":"test","createdAt":"2024-01-01T00:00:00Z"}
"#,
    )
    .await
    .unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for already-current log");
}

// ---------------------------------------------------------------------------
// Test 6: no migration for forward version
// ---------------------------------------------------------------------------

/// Given a wire log with a forward protocol version (1.7, newer than current),
/// when `migrate_log_file` is called, it should return `None`.
#[tokio::test]
async fn should_not_migrate_when_log_is_forward_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(
        &path,
        r#"{"op":"metadata","protocolVersion":"1.7","agentId":"test","createdAt":"2024-01-01T00:00:00Z"}
"#,
    )
    .await
    .unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for forward-version log");
}

// ---------------------------------------------------------------------------
// Test 7: empty log file is handled gracefully
// ---------------------------------------------------------------------------

/// Given an empty wire log file, when `migrate_log_file` is called, it should
/// return `None` without panicking.
#[tokio::test]
async fn should_return_none_when_migrating_empty_log_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(&path, "").await.unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for empty log file");
}

// ---------------------------------------------------------------------------
// Test 8: reading an op-tagged fixture after migration reports corruption
// ---------------------------------------------------------------------------

/// Given a v1.0 wire log fixture (records tagged with the legacy `"op"`
/// discriminator and pre-`message` record shapes), when it is migrated and
/// then opened with `JsonlAppendLog`, the migrated records that are not
/// deserialisable as `WireOp`s must fail the read with
/// `AppendLogError::Corrupted` instead of being silently skipped.
///
/// The fixture predates the current wire protocol by several shape changes
/// (top-level role/content instead of a `message` object), so migration
/// alone cannot make every record parseable — and under the torn-write vs
/// corruption policy such records are genuine corruption, reported loudly.
#[tokio::test]
async fn should_report_corruption_when_reading_migrated_v1_0_fixture_with_legacy_record_shapes() {
    use mirri_persistence::{AppendLogError, AppendLogStore, JsonlAppendLog};

    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.0-session/wire.jsonl");

    // Migrate the fixture to the current protocol version.
    let source_version = migrate_log_file(&path).await.unwrap();
    assert_eq!(source_version, Some("1.0".to_string()));

    // The migrated metadata line is now `type`-tagged and version-bumped.
    let first_line = read_first_line(&path);
    let metadata: Value = serde_json::from_str(&first_line).unwrap();
    assert_eq!(
        metadata.get("type").and_then(|v| v.as_str()),
        Some("metadata")
    );
    assert_eq!(
        metadata.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2.0")
    );

    // Opening the migrated file and reading every line fails with
    // AppendLogError::Corrupted on the first legacy-shape record.
    let log = JsonlAppendLog::open(&path).await.unwrap();
    let error = log.read_all().await.unwrap_err();
    assert!(
        error.downcast_ref::<AppendLogError>().is_some(),
        "reading a migrated log with unparseable legacy records must fail with \
         AppendLogError::Corrupted, got: {error}"
    );
}
