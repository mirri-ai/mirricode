//! Wire protocol version migration for `wire.jsonl` logs.
//!
//! Each `wire.jsonl` file records the wire protocol ops produced during an
//! agent session. Over time the wire protocol evolves — fields are renamed,
//! shapes are flattened, new required fields appear. This module provides a
//! chain of migrations that transform old records into the current protocol
//! shape so that a log written by an older agent can be replayed by the
//! current kernel.
//!
//! ## Migration chain
//!
//! ```text
//! 1.0 → 1.1 → 1.2 → 1.3 → 1.4 → 1.5 → 1.6 → 2.0 (CURRENT_VERSION)
//! ```
//!
//! The last segment (`1.6 → 2.0`) is an **identity** migration — an era
//! re-stamp with no record-shape change (ADR-006-R2: the 2.0 wire era begins
//! with the rust storage-root split). 1.5/1.6 logs therefore stay readable
//! through the chain 1.5→1.6→2.0, whose last link leaves every record
//! byte-equivalent and only the metadata header's version is re-stamped.
//!
//! Each migration is a [`WireMigration`] that knows how to mutate a single
//! [`serde_json::Value`] record in-place. [`resolve_migrations`] finds the
//! sub-chain needed to bring a given source version up to current, and
//! [`migrate_log_file`] applies that chain to an entire file atomically.
//! [`migrate_records_in_memory`] applies it to a file's records without
//! touching the file, so a resume can replay migrated records even when the
//! on-disk rewrite fails.
//!
//! ## Discriminator spellings
//!
//! Records written by the TypeScript kernel use a `type` discriminator
//! (`{"type":"goal.create",...}`); some legacy Rust fixtures and very old
//! logs use `op` (`{"op":"goal.create",...}`). All migrations match both
//! spellings, and after migration every record is normalised to the current
//! `type` spelling so it can be deserialised by [`mirri_proto::WireOp`].
//!
//! ## Forward versions
//!
//! If a log was written by a *newer* protocol than the current kernel
//! ([`is_forward_version`]), it is **not** migrated — the records are
//! replayed as-is. This keeps the kernel forward-compatible with logs it
//! does not yet fully understand.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Current wire protocol version.
///
/// "2.0" is an **era marker** (ADR-006-R2): the wire record shape is
/// unchanged from 1.6 — the bump marks the rust engine's session storage
/// root split (`v3_sessions/`) — so the 1.6→2.0 migration is identity and
/// 1.5/1.6 logs remain readable through the chain 1.5→1.6→2.0.
pub const CURRENT_VERSION: &str = "2.0";

/// The record discriminator value: the `type` field (the current and TS
/// spelling) or the legacy `op` field used by very old logs and fixtures.
fn record_kind(record: &Value) -> Option<&str> {
    record
        .get("type")
        .or_else(|| record.get("op"))
        .and_then(Value::as_str)
}

/// Normalise a record's discriminator to the current `type` spelling.
///
/// Legacy `op`-tagged records are renamed in place (unless a `type` field
/// already exists), so they deserialise as [`mirri_proto::WireOp`]s after
/// migration.
fn ensure_current_discriminator(record: &mut Value) {
    let Some(object) = record.as_object_mut() else {
        return;
    };
    if object.contains_key("type") {
        return;
    }
    if let Some(op) = object.remove("op") {
        object.insert("type".to_string(), op);
    }
}

/// A single wire protocol version migration.
///
/// Each migration transforms records from `source_version` to
/// `target_version`. Records that don't match the migration's target op
/// type are left unchanged.
pub trait WireMigration: Send + Sync {
    /// The protocol version this migration reads from (e.g. `"1.0"`).
    fn source_version(&self) -> &str;

    /// The protocol version this migration produces (e.g. `"1.1"`).
    fn target_version(&self) -> &str;

    /// Mutate a single wire record in-place.
    ///
    /// Records that don't match the migration's target op type are left
    /// unchanged.
    fn migrate_record(&self, record: &mut Value) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Individual migrations
// ---------------------------------------------------------------------------

/// Migration `1.0 → 1.1`: flatten `toolCalls[].function.{name,arguments}`
/// to `toolCalls[].{name,arguments}`.
///
/// Before:
/// ```json
/// { "op": "context.append_message",
///   "toolCalls": [{ "id": "1", "function": { "name": "get_weather",
///   "arguments": "{\"city\":\"SF\"}" } }] }
/// ```
///
/// After:
/// ```json
/// { "op": "context.append_message",
///   "toolCalls": [{ "id": "1", "name": "get_weather",
///   "arguments": "{\"city\":\"SF\"}" }] }
/// ```
#[derive(Default)]
pub struct MigrateV1_0ToV1_1;

impl WireMigration for MigrateV1_0ToV1_1 {
    fn source_version(&self) -> &str {
        "1.0"
    }

    fn target_version(&self) -> &str {
        "1.1"
    }

    fn migrate_record(&self, record: &mut Value) -> Result<()> {
        if record_kind(record) != Some("context.append_message") {
            return Ok(());
        }

        let tool_calls = match record.get_mut("toolCalls") {
            Some(Value::Array(arr)) => arr,
            _ => return Ok(()), // no toolCalls or not an array — skip
        };

        for call in tool_calls.iter_mut() {
            let obj = match call.as_object_mut() {
                Some(o) => o,
                None => continue,
            };
            // Move function.name → name, function.arguments → arguments
            if let Some(func) = obj.get("function").cloned() {
                if let Value::Object(func_obj) = &func {
                    if let Some(name) = func_obj.get("name").cloned() {
                        obj.insert("name".to_string(), name);
                    }
                    if let Some(args) = func_obj.get("arguments").cloned() {
                        obj.insert("arguments".to_string(), args);
                    }
                }
                obj.remove("function");
            }
        }

        Ok(())
    }
}

/// Migration `1.1 → 1.2`: derive `sessionApprovalRule` from legacy `action`
/// + `toolName` fields on `permission.record_approval_result` records.
#[derive(Default)]
pub struct MigrateV1_1ToV1_2;

impl WireMigration for MigrateV1_1ToV1_2 {
    fn source_version(&self) -> &str {
        "1.1"
    }

    fn target_version(&self) -> &str {
        "1.2"
    }

    fn migrate_record(&self, record: &mut Value) -> Result<()> {
        if record_kind(record) != Some("permission.record_approval_result") {
            return Ok(());
        }

        // Don't overwrite an existing sessionApprovalRule.
        if record.get("sessionApprovalRule").is_some() {
            return Ok(());
        }

        let action = record.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let tool_name = record.get("toolName").and_then(|v| v.as_str());

        let rule = match action {
            "allow" => {
                let mut obj = serde_json::Map::new();
                obj.insert("rule".to_string(), Value::String("allow".to_string()));
                if let Some(tn) = tool_name {
                    obj.insert("toolName".to_string(), Value::String(tn.to_string()));
                }
                Value::Object(obj)
            }
            "deny" => {
                let mut obj = serde_json::Map::new();
                obj.insert("rule".to_string(), Value::String("deny".to_string()));
                if let Some(tn) = tool_name {
                    obj.insert("toolName".to_string(), Value::String(tn.to_string()));
                }
                Value::Object(obj)
            }
            "allowAll" => {
                let mut obj = serde_json::Map::new();
                obj.insert("rule".to_string(), Value::String("allowAll".to_string()));
                Value::Object(obj)
            }
            "denyAll" => {
                let mut obj = serde_json::Map::new();
                obj.insert("rule".to_string(), Value::String("denyAll".to_string()));
                Value::Object(obj)
            }
            _ => return Ok(()), // unknown action — skip
        };

        if let Some(obj) = record.as_object_mut() {
            obj.insert("sessionApprovalRule".to_string(), rule);
        }

        Ok(())
    }
}

/// Migration `1.2 → 1.3`: version marker for blobref offloading.
///
/// No record-level changes are needed; this migration exists solely to
/// advance the protocol version number.
#[derive(Default)]
pub struct MigrateV1_2ToV1_3;

impl WireMigration for MigrateV1_2ToV1_3 {
    fn source_version(&self) -> &str {
        "1.2"
    }

    fn target_version(&self) -> &str {
        "1.3"
    }

    fn migrate_record(&self, _record: &mut Value) -> Result<()> {
        Ok(())
    }
}

/// Migration `1.3 → 1.4`: unify goal op types.
///
/// - `goal.account_usage` → `goal.update`
/// - `goal.continuation` → `goal.update`
/// - `goal.update` records with a `goalId` field have it removed (non-create
///   goal records should not carry `goalId`).
/// - `goal.create` records are left unchanged.
#[derive(Default)]
pub struct MigrateV1_3ToV1_4;

impl WireMigration for MigrateV1_3ToV1_4 {
    fn source_version(&self) -> &str {
        "1.3"
    }

    fn target_version(&self) -> &str {
        "1.4"
    }

    fn migrate_record(&self, record: &mut Value) -> Result<()> {
        let kind = record_kind(record).unwrap_or("").to_string();

        match kind.as_str() {
            "goal.account_usage" | "goal.continuation" => {
                // Rename to `goal.update`, keeping the discriminator
                // spelling (type or op) the record already uses.
                if let Some(obj) = record.as_object_mut() {
                    let discriminator = if obj.contains_key("type") {
                        "type"
                    } else {
                        "op"
                    };
                    obj.insert(
                        discriminator.to_string(),
                        Value::String("goal.update".to_string()),
                    );
                }
            }
            "goal.update" => {
                // Remove goalId from non-create goal records.
                if let Some(obj) = record.as_object_mut() {
                    obj.remove("goalId");
                }
            }
            _ => {}
        }

        Ok(())
    }
}

/// Migration `1.4 → 1.5`: backfill `wallClockResumedAt` on goal records.
///
/// For `goal.create` and `goal.update` records that lack a
/// `wallClockResumedAt` field, set it from the record's `time` field.
/// Records that already have `wallClockResumedAt` are left unchanged.
#[derive(Default)]
pub struct MigrateV1_4ToV1_5;

impl WireMigration for MigrateV1_4ToV1_5 {
    fn source_version(&self) -> &str {
        "1.4"
    }

    fn target_version(&self) -> &str {
        "1.5"
    }

    fn migrate_record(&self, record: &mut Value) -> Result<()> {
        let kind = record_kind(record);
        if kind != Some("goal.create") && kind != Some("goal.update") {
            return Ok(());
        }

        // Don't overwrite an existing wallClockResumedAt.
        if record.get("wallClockResumedAt").is_some() {
            return Ok(());
        }

        if let Some(time) = record.get("time").cloned()
            && let Some(obj) = record.as_object_mut()
        {
            obj.insert("wallClockResumedAt".to_string(), time);
        }

        Ok(())
    }
}

/// Migration `1.5 → 1.6`: normalise the `cron.add` task payload to the
/// cross-language wire DTO (F16).
///
/// Logs written by the pre-T6 rust kernel carried the `CronTask` serde form
/// inside `cron.add.task` — snake_case keys (`created_at`, `last_fired`,
/// `next_fire_at`, `session_id`) with RFC 3339 timestamp strings. The TS
/// kernel's `CronTask` declaration (and the rust kernel since T6) uses
/// camelCase keys with epoch-millisecond numbers and folds the session id
/// into `tags.sessionId`; the absolute `next_fire_at` is dropped (both
/// engines recompute it on restore).
///
/// Idempotent: a task that already carries the DTO's `createdAt` key is
/// left untouched, so the migration is safe to re-run and safe on
/// TS-authored logs (which never carry the legacy shape — their cron ops
/// are transient and never persisted).
#[derive(Default)]
pub struct MigrateV1_5ToV1_6;

impl WireMigration for MigrateV1_5ToV1_6 {
    fn source_version(&self) -> &str {
        "1.5"
    }

    fn target_version(&self) -> &str {
        "1.6"
    }

    fn migrate_record(&self, record: &mut Value) -> Result<()> {
        use chrono::{DateTime, Utc};

        if record_kind(record) != Some("cron.add") {
            return Ok(());
        }
        let Some(task) = record.get_mut("task").and_then(Value::as_object_mut) else {
            return Ok(());
        };
        // Idempotence: already-DTO records keep their shape.
        if task.contains_key("createdAt") {
            return Ok(());
        }

        /// Convert a timestamp field pair: remove the legacy RFC 3339
        /// string under `legacy_key`, re-insert epoch millis under
        /// `dto_key`.
        fn convert_timestamp(
            task: &mut serde_json::Map<String, Value>,
            dto_key: &str,
            legacy_key: &str,
        ) {
            if let Some(raw) = task.remove(legacy_key) {
                let millis = match raw {
                    Value::String(rfc3339) => DateTime::parse_from_rfc3339(&rfc3339)
                        .map(|at| at.with_timezone(&Utc).timestamp_millis())
                        .unwrap_or_default(),
                    // A legacy record that somehow already carries a number
                    // passes through unchanged.
                    Value::Number(number) => number.as_i64().unwrap_or_default(),
                    // `null` (an unfired task's last_fired) is simply
                    // dropped — the DTO omits the key.
                    _ => return,
                };
                task.insert(dto_key.to_string(), Value::from(millis));
            }
        }

        convert_timestamp(task, "createdAt", "created_at");
        convert_timestamp(task, "lastFiredAt", "last_fired");
        // The absolute next fire is never trusted on restore — drop it.
        task.remove("next_fire_at");
        // The session id folds into the TS CRON_SESSION_TAG shape.
        if let Some(session_id) = task.remove("session_id") {
            let tags = task
                .entry("tags".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(tags) = tags.as_object_mut() {
                tags.insert("sessionId".to_string(), session_id);
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chain resolution and application
// ---------------------------------------------------------------------------

/// Migration `1.6 → 2.0`: identity — the era re-stamp (ADR-006-R2).
///
/// The 2.0 wire protocol makes **no record-shape change** over 1.6: the
/// version bump marks the rust engine's session storage root split
/// (`v3_sessions/`), not a protocol change. This migration exists solely to
/// advance the protocol version number so `resolve_migrations` can chain
/// 1.5-era logs through 1.5→1.6→2.0 — every record passes through
/// byte-equivalent, and only the metadata header's version is re-stamped
/// (by [`migrate_log_file`]).
#[derive(Default)]
pub struct MigrateV1_6ToV2_0;

impl WireMigration for MigrateV1_6ToV2_0 {
    fn source_version(&self) -> &str {
        "1.6"
    }

    fn target_version(&self) -> &str {
        "2.0"
    }

    fn migrate_record(&self, _record: &mut Value) -> Result<()> {
        Ok(())
    }
}

/// Type alias for a migration constructor entry.
type MigrationEntry = (&'static str, &'static str, fn() -> Box<dyn WireMigration>);

/// All known migrations, ordered from oldest to newest.
///
/// Each entry is `(source_version, target_version, constructor)`.
const MIGRATIONS: &[MigrationEntry] = &[
    ("1.0", "1.1", || Box::new(MigrateV1_0ToV1_1)),
    ("1.1", "1.2", || Box::new(MigrateV1_1ToV1_2)),
    ("1.2", "1.3", || Box::new(MigrateV1_2ToV1_3)),
    ("1.3", "1.4", || Box::new(MigrateV1_3ToV1_4)),
    ("1.4", "1.5", || Box::new(MigrateV1_4ToV1_5)),
    ("1.5", "1.6", || Box::new(MigrateV1_5ToV1_6)),
    ("1.6", "2.0", || Box::new(MigrateV1_6ToV2_0)),
];

/// Resolve the chain of migrations needed to bring a log from
/// `source_version` up to [`CURRENT_VERSION`].
///
/// Returns an empty vec if `source_version` is already current, is a
/// forward version (newer than current), or is not recognised.
pub fn resolve_migrations(source_version: &str) -> Vec<Box<dyn WireMigration>> {
    // Find the starting position: the first migration whose source_version
    // matches the given version.
    let start = MIGRATIONS
        .iter()
        .position(|(src, _, _)| *src == source_version);

    match start {
        Some(idx) => MIGRATIONS[idx..]
            .iter()
            .map(|(_, _, ctor)| ctor())
            .collect(),
        None => Vec::new(),
    }
}

/// Apply a chain of migrations to a single record (in-place).
///
/// Migrations are applied in order; each one may mutate the record. Records
/// that don't match a given migration's target op type are passed through
/// unchanged by that migration.
pub fn migrate_record(record: &mut Value, migrations: &[Box<dyn WireMigration>]) -> Result<()> {
    for migration in migrations {
        migration.migrate_record(record)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Version detection
// ---------------------------------------------------------------------------

/// Detect the wire protocol version from the first line of a `wire.jsonl`
/// file.
///
/// - If the first line parses as JSON and is a metadata record (tagged
///   `type` or legacy `op`), read its protocol version from either the
///   `protocol_version` (TS spelling) or `protocolVersion` field.
/// - If the first line is not a metadata record, assume `v1.4` (an
///   envelope-less log written without a metadata header).
/// - Returns `None` if the line is unparseable as JSON.
pub fn detect_version(first_line: &str) -> Option<String> {
    let json: Value = serde_json::from_str(first_line).ok()?;
    if record_kind(&json) == Some("metadata") {
        json.get("protocol_version")
            .or_else(|| json.get("protocolVersion"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        // Envelope-less log — assume v1.4
        Some("1.4".to_string())
    }
}

/// Check if a version is newer than current (a forward version).
///
/// Forward-version logs are not migrated; records are replayed as-is.
///
/// Versions are compared as floating-point numbers, so `"2.1"` > `"2.0"`
/// → `true`, while `"2.0"` > `"2.0"` → `false`.
pub fn is_forward_version(version: &str) -> bool {
    let current: f64 = CURRENT_VERSION.parse().unwrap_or(0.0);
    let given: f64 = version.parse().unwrap_or(0.0);
    given > current
}

// ---------------------------------------------------------------------------
// File-level migration
// ---------------------------------------------------------------------------

/// Migrate an entire `wire.jsonl` file in-place.
///
/// Reads all lines, detects the source version, applies the migration
/// chain, and atomically rewrites the file using a temp-file + rename. The
/// rewrite is crash-durable: the temp file is fsynced before the rename and
/// the parent directory is fsynced afterwards (mirroring
/// [`crate::atomic_doc::AtomicDocument::write`]). If the file is already at
/// the current version or is a forward version, this is a no-op.
///
/// Returns `Ok(Some(source_version))` when a migration was performed, or
/// `Ok(None)` when no migration was needed (empty file, already current,
/// or forward version).
pub async fn migrate_log_file(path: &Path) -> Result<Option<String>> {
    // 1. Read all lines from the file.
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No file — nothing to migrate.
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };

    // 2. If empty, return None.
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Ok(None);
    }

    // 3. Detect version from first line.
    let source_version = match detect_version(lines[0]) {
        Some(v) => v,
        None => return Ok(None), // unparseable — leave untouched
    };

    // 4. If version >= current or forward → return None.
    if source_version == CURRENT_VERSION || is_forward_version(&source_version) {
        return Ok(None);
    }

    // 5. Resolve migrations.
    let migrations = resolve_migrations(&source_version);
    if migrations.is_empty() {
        return Ok(None);
    }

    // 6. For each line: parse as JSON, migrate, normalise the discriminator
    //    to `type`, and re-serialize.
    // 7. Also bump the metadata record's protocol version to current.
    let mut migrated_lines: Vec<String> = Vec::with_capacity(lines.len());

    for (i, line) in lines.iter().enumerate() {
        let mut record: Value = serde_json::from_str(line)?;

        // Apply the migration chain.
        migrate_record(&mut record, &migrations)?;

        // Normalise a legacy `op` discriminator to the current `type`
        // spelling so the migrated file deserialises as WireOps.
        ensure_current_discriminator(&mut record);

        // If this is the metadata record, bump its protocol version to
        // current under both field spellings.
        if i == 0
            && record_kind(&record) == Some("metadata")
            && let Some(obj) = record.as_object_mut()
        {
            obj.insert(
                "protocolVersion".to_string(),
                Value::String(CURRENT_VERSION.to_string()),
            );
            obj.insert(
                "protocol_version".to_string(),
                Value::String(CURRENT_VERSION.to_string()),
            );
        }

        migrated_lines.push(serde_json::to_string(&record)?);
    }

    let migrated_content = migrated_lines.join("\n") + "\n";

    // 8. Atomically write the migrated content back, crash-durably (the
    //    shared primitive: fsynced temp + rename + best-effort directory
    //    fsync).
    crate::fs_utils::write_file_atomically(path, migrated_content.as_bytes())?;

    // 9. Return the source version we migrated from.
    Ok(Some(source_version))
}

/// The record content of a raw line, with its line terminator removed the way
/// [`str::lines`] does: a trailing `\n` is stripped, and a `\r` immediately
/// before it (a CRLF terminator) is stripped with it. A trailing `\r` without
/// a following `\n` is **kept** (it is not a CRLF terminator), matching the
/// in-memory reader's behaviour line-for-line.
fn strip_line_ending(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(without_newline) => without_newline
            .strip_suffix('\r')
            .unwrap_or(without_newline),
        None => line,
    }
}

/// Read a `wire.jsonl` file and return its records as parsed JSON values,
/// migrated in-memory to [`CURRENT_VERSION`] whenever the file carries an
/// older protocol version.
///
/// Unlike [`migrate_log_file`], this never touches the file — the migrated
/// records are returned to the caller for immediate use (e.g. replay after a
/// resume). It applies the same torn-write vs corruption policy as the
/// append-log reader: a torn final line (unparseable, last line, no trailing
/// newline) is dropped, while any other unparseable line fails with
/// [`crate::append_log::AppendLogError::Corrupted`]. Blank and
/// whitespace-only lines are skipped, mirroring the append-log reader.
///
/// Records whose version cannot be detected (unparseable first line, current
/// version, or forward version) are returned unchanged, except that a legacy
/// `op` discriminator is still normalised to `type` so the records are
/// deserialisable by [`mirri_proto::WireOp`].
///
/// **Dual-shape aware (ADR-001 / the B3 fix)**: the read goes through
/// [`crate::compress::open_wire_log_lines`] — the unified entry that serves
/// either on-disk shape (plain `wire.jsonl` or compressed `wire.jsonl.gz`)
/// — never a bare `File::open`. A bare open of the plain path would find
/// nothing on a compressed session and fail the whole read (the resume /
/// search / snapshot read faces would hard-error or silently vanish).
///
/// The line reader streams through a [`std::io::BufRead`] buffer, so the
/// full-file [`String`] and the per-line [`String`] copies never coexist
/// with the collected [`Value`]s — the resume hot path keeps only the
/// parsed records in memory.
pub async fn migrate_records_in_memory(path: &Path) -> Result<Vec<Value>> {
    use crate::append_log::AppendLogError;

    // The dual-shape read entry (ADR-001 / the B3 fix): `path` names the
    // plain `wire.jsonl`; the shape probe inside
    // [`crate::compress::open_wire_log_lines`] picks the compressed form
    // when that is what the directory holds, so a compressed session's
    // records read exactly like a plain one (a bare `File::open` of the
    // plain path would hard-fail on the compressed shape — the resume /
    // search read faces' silent-vanish / hard-error bug). The blocking
    // read rides `spawn_blocking` so the async worker never performs the
    // file IO.
    //
    // The terminator probe rides along (M7): the reader reports whether
    // the SOURCE stream's last line carried its `\n` — the real-file
    // truth, never a hardcoded `true` — so the torn-final-line policy
    // below behaves identically for both shapes (a gzipped torn tail is
    // still a torn tail).
    let (lines, source_ends_with_newline) = tokio::task::spawn_blocking({
        let dir = path.parent().map(Path::to_path_buf);
        let plain_path = path.to_path_buf();
        move || -> std::io::Result<(Vec<String>, bool)> {
            let Some(dir) = dir else {
                // A bare file name with no directory cannot be
                // shape-probed; fall back to the plain read.
                let content = std::fs::read_to_string(&plain_path)?;
                let ends = content.ends_with('\n');
                return Ok((content.lines().map(str::to_string).collect(), ends));
            };
            let mut reader = crate::compress::open_wire_log_lines(&dir)?;
            let mut lines = Vec::new();
            let mut buffer = String::new();
            while reader.read_line(&mut buffer)? {
                lines.push(std::mem::take(&mut buffer));
            }
            let ends = reader.source_ends_with_newline();
            Ok((lines, ends))
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("wire log read task failed for {}: {e}", path.display()))?
    .with_context(|| format!("failed to read {}", path.display()))?;

    if lines.is_empty() {
        // Empty (or absent) log: no lines, no records, and no
        // torn-final-line edge.
        return Ok(Vec::new());
    }

    // Version detection takes the FIRST line's raw text (blank or not,
    // matching the all-lines reader) and `detect_version` needs a `&str`.
    let source_version = detect_version(strip_line_ending(&lines[0]));
    let migrations = source_version
        .as_deref()
        .map(resolve_migrations)
        .unwrap_or_default();

    let mut records = Vec::new();
    let line_count = lines.len();
    for (line_index, raw) in lines.iter().enumerate() {
        let line_number = line_index + 1;
        // A line whose stream lacked the trailing terminator can only be
        // the FINAL line (only a crash mid-append leaves the last line
        // unterminated) — the torn-write candidate. The probe is the
        // SOURCE's, valid for both shapes (M7).
        let is_torn_final = line_index + 1 == line_count && !source_ends_with_newline;
        let line = strip_line_ending(raw);

        // Blank and whitespace-only lines carry no record — skip them like
        // the append-log reader does, so a stray blank line cannot fail a
        // resume with a Corrupted error.
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut record) => {
                if !migrations.is_empty() {
                    migrate_record(&mut record, &migrations)?;
                }
                ensure_current_discriminator(&mut record);
                records.push(record);
            }
            Err(error) if is_torn_final => {
                tracing::debug!(
                    line_number,
                    error = %error,
                    "dropping torn final wire log line during in-memory migration"
                );
            }
            Err(error) => {
                return Err(AppendLogError::Corrupted {
                    line_number,
                    message: error.to_string(),
                }
                .into());
            }
        }
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // MigrateV1_0ToV1_1
    // -----------------------------------------------------------------------

    #[test]
    fn should_flatten_function_fields_when_record_has_context_append_message_op() {
        let mut record = serde_json::json!({
            "op": "context.append_message",
            "toolCalls": [
                { "id": "1", "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" } },
                { "id": "2", "function": { "name": "get_time", "arguments": "{}" } }
            ]
        });

        MigrateV1_0ToV1_1.migrate_record(&mut record).unwrap();

        assert_eq!(
            record,
            serde_json::json!({
                "op": "context.append_message",
                "toolCalls": [
                    { "id": "1", "name": "get_weather", "arguments": "{\"city\":\"SF\"}" },
                    { "id": "2", "name": "get_time", "arguments": "{}" }
                ]
            })
        );
    }

    #[test]
    fn should_skip_migration_when_op_does_not_match() {
        let mut record = serde_json::json!({
            "op": "some.other_op",
            "toolCalls": [{ "id": "1", "function": { "name": "x", "arguments": "{}" } }]
        });
        let original = record.clone();
        MigrateV1_0ToV1_1.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    #[test]
    fn should_skip_migration_when_record_has_no_tool_calls() {
        let mut record = serde_json::json!({
            "op": "context.append_message",
            "message": "hello"
        });
        let original = record.clone();
        MigrateV1_0ToV1_1.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    // -----------------------------------------------------------------------
    // MigrateV1_1ToV1_2
    // -----------------------------------------------------------------------

    #[test]
    fn should_derive_allow_approval_rule_when_action_is_allow() {
        let mut record = serde_json::json!({
            "op": "permission.record_approval_result",
            "action": "allow",
            "toolName": "bash"
        });
        MigrateV1_1ToV1_2.migrate_record(&mut record).unwrap();
        assert_eq!(
            record.get("sessionApprovalRule"),
            Some(&serde_json::json!({ "rule": "allow", "toolName": "bash" }))
        );
    }

    #[test]
    fn should_derive_deny_approval_rule_when_action_is_deny() {
        let mut record = serde_json::json!({
            "op": "permission.record_approval_result",
            "action": "deny",
            "toolName": "edit"
        });
        MigrateV1_1ToV1_2.migrate_record(&mut record).unwrap();
        assert_eq!(
            record.get("sessionApprovalRule"),
            Some(&serde_json::json!({ "rule": "deny", "toolName": "edit" }))
        );
    }

    #[test]
    fn should_derive_allow_all_approval_rule_when_action_is_allow_all() {
        let mut record = serde_json::json!({
            "op": "permission.record_approval_result",
            "action": "allowAll"
        });
        MigrateV1_1ToV1_2.migrate_record(&mut record).unwrap();
        assert_eq!(
            record.get("sessionApprovalRule"),
            Some(&serde_json::json!({ "rule": "allowAll" }))
        );
    }

    #[test]
    fn should_derive_deny_all_approval_rule_when_action_is_deny_all() {
        let mut record = serde_json::json!({
            "op": "permission.record_approval_result",
            "action": "denyAll"
        });
        MigrateV1_1ToV1_2.migrate_record(&mut record).unwrap();
        assert_eq!(
            record.get("sessionApprovalRule"),
            Some(&serde_json::json!({ "rule": "denyAll" }))
        );
    }

    #[test]
    fn should_preserve_existing_approval_rule_when_session_approval_rule_already_present() {
        let existing = serde_json::json!({ "rule": "allow", "toolName": "existing" });
        let mut record = serde_json::json!({
            "op": "permission.record_approval_result",
            "action": "deny",
            "toolName": "bash",
            "sessionApprovalRule": existing
        });
        MigrateV1_1ToV1_2.migrate_record(&mut record).unwrap();
        assert_eq!(
            record.get("sessionApprovalRule"),
            Some(&serde_json::json!({ "rule": "allow", "toolName": "existing" }))
        );
    }

    // -----------------------------------------------------------------------
    // MigrateV1_2ToV1_3
    // -----------------------------------------------------------------------

    #[test]
    fn should_leave_record_unchanged_when_migrating_from_v1_2_to_v1_3() {
        let mut record = serde_json::json!({
            "op": "some_op",
            "field": 42
        });
        let original = record.clone();
        MigrateV1_2ToV1_3.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    // -----------------------------------------------------------------------
    // MigrateV1_3ToV1_4
    // -----------------------------------------------------------------------

    #[test]
    fn should_merge_goal_account_usage_into_goal_update_when_op_is_goal_account_usage() {
        let mut record = serde_json::json!({
            "op": "goal.account_usage",
            "data": "x"
        });
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert_eq!(record["op"], "goal.update");
        assert_eq!(record["data"], "x");
    }

    #[test]
    fn should_merge_goal_continuation_into_goal_update_when_op_is_goal_continuation() {
        let mut record = serde_json::json!({
            "op": "goal.continuation",
            "data": "y"
        });
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert_eq!(record["op"], "goal.update");
        assert_eq!(record["data"], "y");
    }

    #[test]
    fn should_remove_goal_id_when_op_is_goal_update() {
        let mut record = serde_json::json!({
            "op": "goal.update",
            "goalId": "g-123",
            "status": "active"
        });
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert!(record.get("goalId").is_none());
        assert_eq!(record["status"], "active");
    }

    #[test]
    fn should_preserve_goal_id_when_op_is_goal_create() {
        let mut record = serde_json::json!({
            "op": "goal.create",
            "goalId": "g-456"
        });
        let original = record.clone();
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    // -----------------------------------------------------------------------
    // MigrateV1_4ToV1_5
    // -----------------------------------------------------------------------

    #[test]
    fn should_backfill_wall_clock_resumed_at_from_time_when_field_is_missing() {
        let mut record = serde_json::json!({
            "op": "goal.create",
            "time": "2024-01-01T00:00:00Z"
        });
        MigrateV1_4ToV1_5.migrate_record(&mut record).unwrap();
        assert_eq!(record["wallClockResumedAt"], "2024-01-01T00:00:00Z");
    }

    #[test]
    fn should_preserve_existing_wall_clock_resumed_at_when_field_already_present() {
        let mut record = serde_json::json!({
            "op": "goal.update",
            "time": "2024-01-01T00:00:00Z",
            "wallClockResumedAt": "2024-06-01T00:00:00Z"
        });
        MigrateV1_4ToV1_5.migrate_record(&mut record).unwrap();
        assert_eq!(record["wallClockResumedAt"], "2024-06-01T00:00:00Z");
    }

    #[test]
    fn should_skip_backfill_when_op_is_not_a_goal_op() {
        let mut record = serde_json::json!({
            "op": "other_op",
            "time": "2024-01-01T00:00:00Z"
        });
        let original = record.clone();
        MigrateV1_4ToV1_5.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    // -----------------------------------------------------------------------
    // MigrateV1_5ToV1_6
    // -----------------------------------------------------------------------

    #[test]
    fn should_convert_the_cron_task_payload_when_migrating_a_legacy_cron_add() {
        let mut record = serde_json::json!({
            "type": "cron.add",
            "task": {
                "id": "cron-1",
                "cron": "*/5 * * * *",
                "prompt": "tick",
                "session_id": "s-1",
                "recurring": true,
                "created_at": "2026-09-07T10:00:00Z",
                "last_fired": "2026-09-07T10:05:00Z",
                "next_fire_at": "2026-09-07T10:10:00Z",
            }
        });

        MigrateV1_5ToV1_6.migrate_record(&mut record).unwrap();

        let task = &record["task"];
        assert_eq!(
            task["createdAt"], 1_788_775_200_000i64,
            "epoch millis: {task}"
        );
        assert_eq!(task["lastFiredAt"], 1_788_775_500_000i64);
        assert_eq!(task["tags"]["sessionId"], "s-1");
        assert_eq!(task["id"], "cron-1");
        assert_eq!(task["cron"], "*/5 * * * *");
        assert_eq!(task["recurring"], true);
        assert!(
            task.get("created_at").is_none()
                && task.get("last_fired").is_none()
                && task.get("next_fire_at").is_none()
                && task.get("session_id").is_none(),
            "the legacy keys must be gone: {task}"
        );
    }

    #[test]
    fn should_drop_the_last_fired_key_when_the_legacy_task_never_fired() {
        let mut record = serde_json::json!({
            "type": "cron.add",
            "task": {
                "id": "cron-2",
                "cron": "0 9 * * *",
                "prompt": "standup",
                "session_id": "s-2",
                "recurring": false,
                "created_at": "2026-09-07T09:00:00Z",
                "last_fired": null,
                "next_fire_at": null,
            }
        });

        MigrateV1_5ToV1_6.migrate_record(&mut record).unwrap();

        let task = &record["task"];
        assert!(
            task.get("lastFiredAt").is_none(),
            "a null last_fired drops the key entirely: {task}"
        );
        assert_eq!(task["createdAt"], 1_788_771_600_000i64);
    }

    #[test]
    fn should_be_idempotent_when_the_task_is_already_the_dto_shape() {
        let mut record = serde_json::json!({
            "type": "cron.add",
            "task": {
                "id": "cron-3",
                "cron": "0 * * * *",
                "prompt": "already converted",
                "createdAt": 1_773_000_000_000i64,
                "recurring": true,
                "tags": { "sessionId": "s-3" },
            }
        });
        let original = record.clone();

        MigrateV1_5ToV1_6.migrate_record(&mut record).unwrap();

        assert_eq!(record, original, "an already-converted record is untouched");
    }

    #[test]
    fn should_leave_other_ops_untouched_when_migrating_v1_5_to_v1_6() {
        let mut record = serde_json::json!({
            "type": "goal.update",
            "turnsUsed": 3,
            "created_at": "2026-09-07T10:00:00Z",
        });
        let original = record.clone();
        MigrateV1_5ToV1_6.migrate_record(&mut record).unwrap();
        assert_eq!(record, original);
    }

    // -----------------------------------------------------------------------
    // MigrateV1_6ToV2_0 (identity — the 2.0 era re-stamp)
    // -----------------------------------------------------------------------

    #[test]
    fn should_migrate_1_6_log_to_2_0_as_identity_when_resumed() {
        // Given records stamped 1.6 in the 1.6 shape (a fully-converted cron
        // DTO record — the shape 2.0 keeps unchanged; the bump is an era
        // marker, not a shape change)
        let mut record = serde_json::json!({
            "type": "cron.add",
            "task": {
                "id": "cron-4",
                "cron": "0 12 * * *",
                "prompt": "era test",
                "createdAt": 1_773_000_000_000i64,
                "recurring": true,
                "tags": { "sessionId": "s-4" },
            }
        });
        let original = record.clone();

        // When migration resolves from "1.6" with CURRENT_VERSION "2.0"
        let migrations = resolve_migrations("1.6");
        assert_eq!(migrations.len(), 1, "exactly the identity segment applies");
        assert_eq!(migrations[0].source_version(), "1.6");
        assert_eq!(migrations[0].target_version(), "2.0");
        migrate_record(&mut record, &migrations).unwrap();

        // Then the records are byte-equivalent (no shape change, era
        // re-stamp only)
        assert_eq!(
            record, original,
            "the 1.6→2.0 migration must be identity: {record}"
        );
    }

    #[test]
    fn should_chain_both_segments_when_resolving_from_v1_5() {
        // Given a 1.5-stamped log, when the chain resolves, then it
        // serialises both segments: 1.5→1.6 (the cron DTO normalisation)
        // followed by 1.6→2.0 (the identity era re-stamp)
        let chain = resolve_migrations("1.5");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].source_version(), "1.5");
        assert_eq!(chain[0].target_version(), "1.6");
        assert_eq!(chain[1].source_version(), "1.6");
        assert_eq!(chain[1].target_version(), "2.0");
    }

    #[tokio::test]
    async fn should_replay_a_v1_5_cron_log_through_the_full_migration_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        tokio::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.5","created_at":123}"#,
                "\n",
                r#"{"type":"cron.add","task":{"id":"cron-1","cron":"*/5 * * * *","prompt":"tick","session_id":"s-1","recurring":true,"created_at":"2026-09-07T10:00:00Z","last_fired":null,"next_fire_at":"2026-09-07T10:05:00Z"}}"#,
                "\n",
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(records.len(), 2);
        let task = &records[1]["task"];
        assert_eq!(task["createdAt"], 1_788_775_200_000i64);
        assert_eq!(task["tags"]["sessionId"], "s-1");
        assert!(
            task.get("last_fired").is_none() && task.get("next_fire_at").is_none(),
            "legacy keys are converted: {task}"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_migrations
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_all_seven_migrations_when_source_version_is_v1_0() {
        let chain = resolve_migrations("1.0");
        assert_eq!(chain.len(), 7);
        assert_eq!(chain[0].source_version(), "1.0");
        assert_eq!(chain[6].target_version(), "2.0");
    }

    #[test]
    fn should_return_four_migrations_when_source_version_is_v1_3() {
        let chain = resolve_migrations("1.3");
        assert_eq!(chain.len(), 4);
        assert_eq!(chain[0].source_version(), "1.3");
        assert_eq!(chain[1].source_version(), "1.4");
        assert_eq!(chain[2].source_version(), "1.5");
        assert_eq!(chain[3].source_version(), "1.6");
    }

    #[test]
    fn should_return_empty_chain_when_source_version_is_current() {
        let chain = resolve_migrations(CURRENT_VERSION);
        assert!(chain.is_empty());
    }

    #[test]
    fn should_return_empty_chain_when_source_version_is_forward() {
        let chain = resolve_migrations("2.1");
        assert!(chain.is_empty());
    }

    #[test]
    fn should_return_empty_chain_when_source_version_is_unknown() {
        let chain = resolve_migrations("0.9");
        assert!(chain.is_empty());
    }

    // -----------------------------------------------------------------------
    // detect_version
    // -----------------------------------------------------------------------

    #[test]
    fn should_detect_version_from_metadata_record_when_first_line_has_metadata_op() {
        let line = r#"{"op":"metadata","protocolVersion":"1.2"}"#;
        assert_eq!(detect_version(line), Some("1.2".to_string()));
    }

    #[test]
    fn should_return_v1_4_when_first_line_is_not_a_metadata_record() {
        let line = r#"{"op":"goal.create","goalId":"g1"}"#;
        assert_eq!(detect_version(line), Some("1.4".to_string()));
    }

    #[test]
    fn should_return_none_when_first_line_is_unparseable_json() {
        assert_eq!(detect_version("not json at all"), None);
    }

    // -----------------------------------------------------------------------
    // is_forward_version
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_true_when_version_is_newer_than_current() {
        assert!(is_forward_version("2.1"));
        // "2.0" is the current era marker — equal, never forward (the
        // identity 1.6→2.0 migration handles 1.6 logs instead).
        assert!(!is_forward_version("2.0"));
    }

    #[test]
    fn should_return_false_when_version_is_current() {
        assert!(!is_forward_version(CURRENT_VERSION));
    }

    #[test]
    fn should_return_false_when_version_is_older_than_current() {
        assert!(!is_forward_version("1.0"));
    }

    // -----------------------------------------------------------------------
    // detect_version — TS spellings (type tag, protocol_version field)
    // -----------------------------------------------------------------------

    #[test]
    fn should_detect_version_from_ts_metadata_when_first_line_uses_type_tag_and_snake_case_field() {
        let line = r#"{"type":"metadata","protocol_version":"1.3","created_at":123}"#;
        assert_eq!(detect_version(line), Some("1.3".to_string()));
    }

    #[test]
    fn should_detect_version_from_ts_metadata_when_first_line_uses_type_tag_and_camel_case_field() {
        let line = r#"{"type":"metadata","protocolVersion":"1.1"}"#;
        assert_eq!(detect_version(line), Some("1.1".to_string()));
    }

    #[test]
    fn should_detect_current_version_when_first_line_is_rust_native_metadata() {
        let line = r#"{"type":"metadata","protocol_version":"2.0","created_at":123}"#;
        assert_eq!(detect_version(line), Some(CURRENT_VERSION.to_string()));
    }

    // -----------------------------------------------------------------------
    // Migration matching on `type`-tagged records (the TS spelling)
    // -----------------------------------------------------------------------

    #[test]
    fn should_rename_type_discriminator_when_migrating_type_tagged_goal_account_usage_record() {
        let mut record = serde_json::json!({
            "type": "goal.account_usage",
            "goalId": "g-1",
            "tokensUsed": 10
        });
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert_eq!(record["type"], "goal.update");
        assert_eq!(
            record["goalId"], "g-1",
            "non-create goal usage keeps its goalId"
        );
        assert_eq!(record["tokensUsed"], 10);
    }

    #[test]
    fn should_remove_goal_id_from_type_tagged_goal_update_when_migrating() {
        let mut record = serde_json::json!({
            "type": "goal.update",
            "goalId": "g-2",
            "status": "active"
        });
        MigrateV1_3ToV1_4.migrate_record(&mut record).unwrap();
        assert!(record.get("goalId").is_none());
        assert_eq!(record["status"], "active");
    }

    #[test]
    fn should_backfill_wall_clock_resumed_at_on_type_tagged_goal_record_when_migrating() {
        let mut record = serde_json::json!({
            "type": "goal.create",
            "goalId": "g-3",
            "time": 1700000000000i64
        });
        MigrateV1_4ToV1_5.migrate_record(&mut record).unwrap();
        assert_eq!(record["wallClockResumedAt"], 1700000000000i64);
    }

    // -----------------------------------------------------------------------
    // migrate_records_in_memory
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_migrate_records_in_memory_when_the_log_is_compressed_on_disk() {
        // Given a CURRENT-version log that was compressed (the .gz is the
        // only on-disk shape — the B3 regression: the old bare File::open
        // of wire.jsonl failed with NotFound on exactly this state)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let content = concat!(
            r#"{"type":"metadata","protocolVersion":"2.0"}"#,
            "\n",
            r#"{"type":"context.append_message","message":{"role":"user","content":"compressed session body"}}"#,
            "\n",
        );
        tokio::fs::write(&path, content).await.unwrap();
        crate::compress::compress_wire_log(tmp.path()).unwrap();
        assert!(!path.exists(), "the plain log must be gone");

        // When the in-memory migration reads the COMPRESSED shape
        let records = migrate_records_in_memory(&path).await.unwrap();

        // Then both records come back (the dual-shape read entry served
        // the .gz), with no error and no silent vanishing
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "metadata");
        assert_eq!(records[1]["message"]["content"], "compressed session body");
    }

    #[tokio::test]
    async fn should_migrate_an_old_protocol_log_that_was_compressed() {
        // Given an OLD-protocol log (v1.0 op-discriminator records) that
        // was compressed — the full B3 shape: migration + decompression
        // must both apply
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let content = concat!(
            r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"op":"context.append_message","role":"assistant","content":"from the compressed archive"}"#,
            "\n",
        );
        tokio::fs::write(&path, content).await.unwrap();
        crate::compress::compress_wire_log(tmp.path()).unwrap();

        // When the in-memory migration reads it
        let records = migrate_records_in_memory(&path).await.unwrap();

        // Then the records are BOTH decompressed AND migrated (the op
        // discriminator normalised to type)
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "metadata");
        assert_eq!(records[0]["protocolVersion"], "1.0");
        assert_eq!(records[1]["type"], "context.append_message");
        assert_eq!(records[1]["content"], "from the compressed archive");
    }

    #[tokio::test]
    async fn should_drop_a_torn_final_line_that_was_compressed_before_its_newline_landed() {
        // Given a log whose final line is torn (no trailing newline) that
        // was THEN compressed — the M7 regression: the old gz read path
        // hardcoded ends_with_newline = true, turning this tolerated tail
        // into a hard Corrupted error
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let torn = concat!(
            r#"{"type":"context.append_message","message":{"role":"user","content":"first"}}"#,
            "\n",
            // The torn tail: valid JSON prefix, no newline, unparseable
            // as a complete record.
            r#"{"type":"context.append_message","message":{"role":"user","content":"sec"#,
        );
        tokio::fs::write(&path, torn).await.unwrap();
        crate::compress::compress_wire_log(tmp.path()).unwrap();

        // When the in-memory migration reads the compressed torn tail
        let records = migrate_records_in_memory(&path).await.unwrap();

        // Then the complete first record survives and the torn tail is
        // DROPPED (tolerated, the same policy the plain shape applies) —
        // never a Corrupted error
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["message"]["content"], "first");
    }

    #[tokio::test]
    async fn should_fail_with_corrupted_when_a_mid_file_line_is_unparseable_and_compressed() {
        // Given a compressed log whose MIDDLE line is genuine corruption
        // (not a torn tail — the parse failure is a real defect)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let content = concat!(
            r#"{"type":"metadata","protocolVersion":"2.0"}"#,
            "\n",
            "not json at all\n",
            r#"{"type":"context.append_message","message":{"role":"user","content":"after"}}"#,
            "\n",
        );
        tokio::fs::write(&path, content).await.unwrap();
        crate::compress::compress_wire_log(tmp.path()).unwrap();

        // When the in-memory migration reads it
        let outcome = migrate_records_in_memory(&path).await;

        // Then it fails loudly (Corrupted) — the dual-shape tolerance must
        // not extend to genuine mid-file corruption
        assert!(outcome.is_err(), "mid-file corruption must fail, not drop");
    }

    #[tokio::test]
    async fn should_migrate_records_in_memory_when_log_has_old_protocol_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        tokio::fs::write(
            &path,
            concat!(
                r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
                "\n",
                r#"{"op":"context.append_message","role":"assistant","content":"Let me check.","toolCalls":[{"id":"c1","function":{"name":"Read","arguments":"{\"path\":\"/tmp/f\"}"}}]}"#,
                "\n",
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0]["type"], "metadata",
            "legacy op discriminator must be normalised to type"
        );
        assert_eq!(
            records[0]["protocolVersion"], "1.0",
            "the in-memory path migrates records but does not bump the metadata header \
             (only the optional on-disk rewrite does)"
        );
        let tool_calls = records[1]["toolCalls"].as_array().unwrap();
        assert_eq!(
            tool_calls[0]
                .get("name")
                .and_then(serde_json::Value::as_str),
            Some("Read"),
            "v1.0 nested function fields must be flattened by the in-memory migration"
        );
        assert!(tool_calls[0].get("function").is_none());
    }

    #[tokio::test]
    async fn should_return_records_unchanged_when_log_is_already_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"context.clear","time":124}
"#;
        tokio::fs::write(&path, content).await.unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["protocol_version"], "1.5");
        assert_eq!(records[1]["type"], "context.clear");
    }

    #[tokio::test]
    async fn should_drop_torn_final_line_when_migrating_records_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        // The final line is truncated mid-string and has no trailing newline.
        tokio::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.0","created_at":123}"#,
                "\n",
                r#"{"type":"context.append_message","message":{"role":"us"#,
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(records.len(), 1);
        assert!(
            records[0].get("created_at").is_some(),
            "metadata record must survive"
        );
    }

    #[tokio::test]
    async fn should_skip_blank_lines_when_migrating_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        // A stray empty line and a whitespace-only line sit between the
        // metadata header and a v1.0 nested-function record; both must be
        // skipped instead of failing the migration with a Corrupted error.
        tokio::fs::write(
            &path,
            concat!(
                r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
                "\n",
                "\n",
                "   \n",
                r#"{"op":"context.append_message","role":"assistant","content":"Let me check.","toolCalls":[{"id":"c1","function":{"name":"Read","arguments":"{\"path\":\"/tmp/f\"}"}}]}"#,
                "\n",
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(
            records.len(),
            2,
            "blank lines must be skipped, not fail the migration"
        );
        assert_eq!(
            records[0]["type"], "metadata",
            "legacy op discriminator must be normalised to type"
        );
        let tool_calls = records[1]["toolCalls"].as_array().unwrap();
        assert_eq!(
            tool_calls[0]
                .get("name")
                .and_then(serde_json::Value::as_str),
            Some("Read"),
            "the record after the blank lines must still be migrated"
        );
        assert!(tool_calls[0].get("function").is_none());
    }

    // -----------------------------------------------------------------------
    // Streaming line reader (F5): the file is read line-by-line through a
    // BufReader instead of whole-file read_to_string + per-line copies.
    // These characterize the streaming path against the all-lines-reader
    // semantics it must preserve bit-for-bit.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_get_empty_records_when_file_is_empty_without_torn_false_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        tokio::fs::write(&path, "").await.unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert!(
            records.is_empty(),
            "an empty file must yield no records and no torn-line error"
        );
    }

    #[tokio::test]
    async fn should_keep_final_line_when_it_parses_and_file_lacks_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        // The final line is complete, valid JSON, and has NO trailing newline
        // (e.g. the last append crashed after writing the bytes but before the
        // terminator — the earlier reader only tolerates this for UNPARSEABLE
        // final lines; a parseable one must survive).
        tokio::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.5","created_at":123}"#,
                "\n",
                r#"{"type":"context.clear","time":124}"#,
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(
            records.len(),
            2,
            "a parseable final line without a trailing newline must not be dropped"
        );
        assert_eq!(records[1]["type"], "context.clear");
    }

    #[tokio::test]
    async fn should_skip_blank_lines_and_drop_torn_final_when_streaming_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        // A stray blank line and a whitespace-only line sit between the
        // metadata header and a v1.0 nested-function record, and a torn
        // (unparseable, no trailing newline) line closes the file. All the
        // blank lines must be skipped and the torn final line dropped, so
        // the two real records survive.
        tokio::fs::write(
            &path,
            concat!(
                r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
                "\n",
                "\n",
                "   \n",
                r#"{"op":"context.append_message","role":"assistant","content":"Let me check.","toolCalls":[{"id":"c1","function":{"name":"Read","arguments":"{\"path\":\"/tmp/f\"}"}}]}"#,
                "\n",
                r#"{"type":"context.append_message","message":{"role":"us"#,
            ),
        )
        .await
        .unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(
            records.len(),
            2,
            "blank lines must be skipped and the torn final line dropped"
        );
        assert_eq!(
            records[0]["type"], "metadata",
            "legacy op discriminator must be normalised to type"
        );
        let tool_calls = records[1]["toolCalls"].as_array().unwrap();
        assert_eq!(
            tool_calls[0]
                .get("name")
                .and_then(serde_json::Value::as_str),
            Some("Read"),
            "the record after the blank lines must still be migrated"
        );
        assert!(tool_calls[0].get("function").is_none());
    }

    #[tokio::test]
    async fn should_report_corruption_with_1_based_line_number_when_streaming_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        // A valid header, then an unparseable MID-FILE line (newline-terminated,
        // so NOT a torn write), then a valid tail record. The corruption must
        // fail with line number 2 — never 0-based, and never muted by the tail.
        tokio::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.5","created_at":123}"#,
                "\n",
                r#"{not json"#,
                "\n",
                r#"{"type":"context.clear","time":124}"#,
                "\n",
            ),
        )
        .await
        .unwrap();

        let error = migrate_records_in_memory(&path).await.unwrap_err();

        let append_error = error
            .downcast_ref::<crate::append_log::AppendLogError>()
            .expect("corruption must surface as the append-log Corrupted error");
        let crate::append_log::AppendLogError::Corrupted { line_number, .. } = append_error;
        assert_eq!(
            *line_number, 2,
            "mid-file corruption must report its 1-based line number"
        );
    }

    #[tokio::test]
    async fn should_round_trip_large_log_identically_when_streaming_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");

        // A synthetic multi-megabyte log: a metadata header plus enough
        // context-clear records to exceed several megabytes of JSON.
        let expected: Vec<serde_json::Value> = {
            let mut records = vec![serde_json::json!({
                "type": "metadata",
                "protocol_version": "1.5",
                "created_at": 123,
            })];
            for i in 0..20_000u32 {
                records.push(serde_json::json!({
                    "type": "context.append_message",
                    "time": i,
                    "message": {
                        "role": "user",
                        "content": format!("line {i} with a padded payload {}", "x".repeat(180)),
                    },
                }));
            }
            records
        };

        let mut content = String::new();
        for record in &expected {
            content.push_str(&serde_json::to_string(record).unwrap());
            content.push('\n');
        }
        assert!(
            content.len() > 3 * 1024 * 1024,
            "the synthetic log must actually be multi-megabyte (got {} bytes)",
            content.len()
        );
        tokio::fs::write(&path, content).await.unwrap();

        let records = migrate_records_in_memory(&path).await.unwrap();

        assert_eq!(
            records, expected,
            "the streaming reader must yield byte-identical migrated records"
        );
    }
}
