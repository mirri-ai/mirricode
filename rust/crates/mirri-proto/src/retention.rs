//! Session-retention domain wire types — the `retention.*` RPC method face
//! (session-retention-architecture §4, the "管理页 API" component).
//!
//! Six methods serve the management page (US3.1–US3.5):
//!
//! | method | params | result |
//! |---|---|---|
//! | `retention.list` | `{ include: "archived" \| "compressed" }` | the view rows |
//! | `retention.stats` | *none* | counters + per-tier delete estimates |//! | `retention.compressNow` | `{ session_id }` | `{ ok }` |
//! | `retention.delete` | `{ session_id }` | `{ ok, reclaimed_bytes }` |
//! | `retention.deleteTiered` | `{ older_than_days }` | the batch outcome |
//! | `retention.getPolicy` | *none* | the `[retention]` knobs |
//!
//! Naming follows the registry's dotted method convention
//! (`retention.<verb>` in camelCase for multi-word verbs — the
//! `session.beginCompaction` / `session.updateMeta` / `workspace.setTrust`
//! spelling), while the params/result wire field names follow the
//! snake_case-first dialect of the embedded session routes
//! (`session.list`'s `include_archive` / `archived_only` / `before_id` —
//! the REST client's exact spellings, camelCase aliases accepted).
//!
//! The typed structs are contract declarations only: `mirri-embedded` owns
//! the handlers (dispatch_task 9) and the conversion from the core types —
//! mirri-proto stays independent of mirri-core (no dependency, the same
//! posture as the rest of this crate).

use serde::{Deserialize, Serialize};

/// The product's fixed delete-tier day set (architecture §3.2: the tiers are
/// a product constant, NOT configuration) — the only `older_than_days`
/// values `retention.deleteTiered` accepts and the only estimates
/// `retention.stats` carries.
pub const RETENTION_DELETE_TIER_DAYS: [u32; 4] = [30, 90, 180, 365];

/// Which management-page view `retention.list` serves (lowercase wire
/// words, matching the params dialect of `session.list`'s
/// `include_archive` / `archived_only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionListView {
    /// Every archived session (the archive view — the compression candidates).
    #[serde(rename = "archived")]
    Archived,
    /// Every wire-log-compressed session (the compressed view — the space
    /// reclamation candidates).
    #[serde(rename = "compressed")]
    Compressed,
}

/// `retention.list` params: which view to serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionListParams {
    pub include: RetentionListView,
}

/// One `retention.list` row — a session as the management page sees it.
///
/// Every server-computed field is computed by the SERVER (the embedded
/// handler owns the index + disk scans), never derived client-side from
/// timestamps: `compressed_days` avoids client clock divergence, and
/// `current_size_bytes` / `saved_bytes` need the disk walk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionListEntry {
    pub session_id: String,
    pub title: String,
    pub workspace: String,
    /// When the session was archived; `None` only on rows whose legacy
    /// state predates the stamp (the archive view's membership is the
    /// `archived` flag, not the stamp).
    pub archived_at: Option<String>,
    /// When the wire log was compressed (`None` on uncompressed rows).
    pub compressed_at: Option<String>,
    /// The session's current on-disk occupancy in bytes (all files in the
    /// session directory: `wire.jsonl(.gz)` + `snapshot.json` + `state.json`
    /// + the agents subdirectory) — computed server-side.
    pub current_size_bytes: u64,
    /// The compression savings so far — `None` on rows never compressed,
    /// `0`-carrying when compressed from an unrecorded (legacy)
    /// pre-compression size.
    pub saved_bytes: Option<u64>,
    /// How many whole days the wire log has been compressed — computed
    /// SERVER-side (never derived from a client clock). `None` on
    /// uncompressed rows.
    pub compressed_days: Option<u32>,
    /// When the session was created (ADR-010: the merged archive view's
    /// created-time sort key). Empty string only on rows whose backing
    /// state could not be read at listing time (the unknown sinks last).
    #[serde(default)]
    pub created_at: String,
}

/// `retention.list` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionListResult {
    pub items: Vec<RetentionListEntry>,
}

/// One per-tier delete estimate in `retention.stats` — the confirmation
/// dialog's "N sessions / X MB" for one tier button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionTierEstimate {
    /// The tier's day threshold (one of [`RETENTION_DELETE_TIER_DAYS`]).
    pub days: u32,
    /// How many archived sessions are at least this old.
    pub count: u64,
    /// Their summed occupancy in bytes.
    pub bytes: u64,
}

/// `retention.stats` result — the header counters plus the four
/// per-tier estimates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionStatsResult {
    pub archived_count: u64,
    pub compressed_count: u64,
    /// The total on-disk bytes of the compressed wire logs.
    pub compressed_total_bytes: u64,
    /// The total bytes the archive view still holds (the archive view's
    /// current occupancy — what the delete tiers could reclaim).
    pub reclaimed_potential: u64,
    /// One row per delete tier (30/90/180/365), in ascending order.
    /// Defaults to empty on legacy payloads written before the tier
    /// estimates existed.
    #[serde(default)]
    pub tier_days: Vec<RetentionTierEstimate>,
}

/// `retention.compressNow` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionCompressNowParams {
    pub session_id: String,
}

/// `retention.compressNow` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionCompressNowResult {
    pub ok: bool,
}

/// `retention.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionDeleteParams {
    pub session_id: String,
}

/// `retention.delete` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionDeleteResult {
    pub ok: bool,
    /// How many bytes the deletion reclaimed (the session directory plus
    /// its exclusive attachments).
    pub reclaimed_bytes: u64,
}

/// `retention.deleteTiered` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionDeleteTieredParams {
    /// One of [`RETENTION_DELETE_TIER_DAYS`] — the handler validates against
    /// the constant set (anything else is `request.invalid`).
    pub older_than_days: u32,
}

/// One per-session failure inside a `retention.deleteTiered` batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionDeleteTieredFailure {
    pub session_id: String,
    /// The failure's human-readable reason.
    pub error: String,
}

/// `retention.deleteTiered` result — the batch outcome, per-row success and
/// failure reported separately (US3.3's honest partial accounting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionDeleteTieredResult {
    /// The sessions the batch deleted.
    pub deleted: Vec<String>,
    /// The sessions it could not delete, with the reason.
    pub failed: Vec<RetentionDeleteTieredFailure>,
    /// The bytes the successful deletions reclaimed.
    pub reclaimed_bytes: u64,
}

/// `retention.getPolicy` result — the resolved `[retention]` knobs the
/// strategy hint (US3.5) renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetentionPolicyResult {
    pub enabled: bool,
    pub archive_grace_days: u32,
    pub interval_hours: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RetentionListEntry: camelCase roundtrip + legacy tolerance ------

    #[test]
    fn should_roundtrip_retention_list_entry_when_serialized_to_the_wire() {
        // Given a fully-populated list row
        let entry = RetentionListEntry {
            session_id: "session-1".to_string(),
            title: "A task".to_string(),
            workspace: "ws-1".to_string(),
            created_at: "2025-12-01T00:00:00Z".to_string(),
            archived_at: Some("2026-01-01T00:00:00Z".to_string()),
            compressed_at: Some("2026-02-01T00:00:00Z".to_string()),
            current_size_bytes: 2048,
            saved_bytes: Some(4096),
            compressed_days: Some(12),
        };

        // When serialized to the wire shape
        let value = serde_json::to_value(&entry).unwrap();

        // Then every field keeps its snake_case wire spelling
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["workspace"], "ws-1");
        assert_eq!(value["archived_at"], "2026-01-01T00:00:00Z");
        assert_eq!(value["compressed_at"], "2026-02-01T00:00:00Z");
        assert_eq!(value["current_size_bytes"], 2048);
        assert_eq!(value["saved_bytes"], 4096);
        assert_eq!(value["compressed_days"], 12);

        // And deserializes back losslessly
        let back: RetentionListEntry = serde_json::from_value(value).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn should_default_every_new_field_to_none_when_deserializing_a_legacy_row() {
        // Given a legacy compressed-view row written before the optional
        // metadata fields existed (only the identity + sizes)
        let value = serde_json::json!({
            "session_id": "session-legacy",
            "title": "Legacy",
            "workspace": "ws-1",
            "current_size_bytes": 512
        });

        // When deserialized
        let entry: RetentionListEntry = serde_json::from_value(value).unwrap();

        // Then the optional fields default (old payloads stay readable)
        assert_eq!(entry.session_id, "session-legacy");
        assert_eq!(entry.current_size_bytes, 512);
        assert_eq!(entry.archived_at, None);
        assert_eq!(entry.compressed_at, None);
        assert_eq!(entry.saved_bytes, None);
        assert_eq!(entry.compressed_days, None);
        assert_eq!(entry.created_at, String::new());
    }

    // --- The six method messages: roundtrip + required-field checks ------

    #[test]
    fn should_roundtrip_retention_list_params_and_result() {
        // Given both views
        let archived = RetentionListParams {
            include: RetentionListView::Archived,
        };
        let compressed = RetentionListParams {
            include: RetentionListView::Compressed,
        };

        // When serialized and back
        let archived_value = serde_json::to_value(&archived).unwrap();
        let compressed_value = serde_json::to_value(&compressed).unwrap();

        // Then the view discriminators are the lowercase wire words
        assert_eq!(archived_value["include"], "archived");
        assert_eq!(compressed_value["include"], "compressed");
        assert_eq!(
            serde_json::from_value::<RetentionListParams>(archived_value).unwrap(),
            archived
        );
        assert_eq!(
            serde_json::from_value::<RetentionListParams>(compressed_value).unwrap(),
            compressed
        );

        // And the result envelope roundtrips
        let result = RetentionListResult {
            items: vec![RetentionListEntry {
                session_id: "session-1".to_string(),
                title: "A task".to_string(),
                workspace: "ws-1".to_string(),
                created_at: "2025-12-01T00:00:00Z".to_string(),
                archived_at: Some("2026-01-01T00:00:00Z".to_string()),
                compressed_at: None,
                current_size_bytes: 1024,
                saved_bytes: None,
                compressed_days: None,
            }],
        };
        let value = serde_json::to_value(&result).unwrap();
        assert!(value["items"].is_array());
        assert_eq!(
            serde_json::from_value::<RetentionListResult>(value).unwrap(),
            result
        );
    }

    #[test]
    fn should_reject_retention_list_params_when_the_include_field_is_missing() {
        // Given params without the required view selector
        let value = serde_json::json!({});

        // When deserialized
        let outcome = serde_json::from_value::<RetentionListParams>(value);

        // Then the required field is enforced (no silent default view)
        assert!(outcome.is_err());
    }

    #[test]
    fn should_reject_retention_list_params_when_the_include_word_is_unknown() {
        // Given an unknown view word
        let value = serde_json::json!({ "include": "everything" });

        // When deserialized
        let outcome = serde_json::from_value::<RetentionListParams>(value);

        // Then the discriminator is rejected (closed vocabulary)
        assert!(outcome.is_err());
    }

    #[test]
    fn should_roundtrip_retention_stats_result_with_the_tier_estimates() {
        // Given a stats result carrying all four tier estimates
        let result = RetentionStatsResult {
            archived_count: 12,
            compressed_count: 8,
            compressed_total_bytes: 4096,
            reclaimed_potential: 100_000,
            tier_days: vec![
                RetentionTierEstimate {
                    days: 30,
                    count: 5,
                    bytes: 10_000,
                },
                RetentionTierEstimate {
                    days: 90,
                    count: 3,
                    bytes: 6_000,
                },
                RetentionTierEstimate {
                    days: 180,
                    count: 1,
                    bytes: 2_000,
                },
                RetentionTierEstimate {
                    days: 365,
                    count: 0,
                    bytes: 0,
                },
            ],
        };

        // When serialized to the wire shape
        let value = serde_json::to_value(&result).unwrap();

        // Then every field keeps its snake_case wire spelling
        assert_eq!(value["archived_count"], 12);
        assert_eq!(value["compressed_count"], 8);
        assert_eq!(value["compressed_total_bytes"], 4096);
        assert_eq!(value["reclaimed_potential"], 100_000);
        assert_eq!(value["tier_days"].as_array().unwrap().len(), 4);
        assert_eq!(value["tier_days"][0]["days"], 30);
        assert_eq!(value["tier_days"][0]["count"], 5);
        assert_eq!(value["tier_days"][0]["bytes"], 10_000);
        assert_eq!(value["tier_days"][3]["days"], 365);

        // And deserializes back losslessly
        let back: RetentionStatsResult = serde_json::from_value(value).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn should_default_the_tier_estimates_to_empty_when_deserializing_a_stats_result_without_them() {
        // Given a legacy stats result carrying only the counters
        let value = serde_json::json!({
            "archived_count": 1,
            "compressed_count": 0,
            "compressed_total_bytes": 0,
            "reclaimed_potential": 0
        });

        // When deserialized
        let result: RetentionStatsResult = serde_json::from_value(value).unwrap();

        // Then the tier list defaults to empty (old payloads stay readable)
        assert_eq!(result.archived_count, 1);
        assert!(result.tier_days.is_empty());
    }

    #[test]
    fn should_roundtrip_retention_compress_now_and_delete_messages() {
        // Given the single-session action messages
        let compress_params = RetentionCompressNowParams {
            session_id: "session-1".to_string(),
        };
        let compress_result = RetentionCompressNowResult { ok: true };
        let delete_params = RetentionDeleteParams {
            session_id: "session-1".to_string(),
        };
        let delete_result = RetentionDeleteResult {
            ok: true,
            reclaimed_bytes: 12_345,
        };

        // When serialized and back
        let compress_value = serde_json::to_value(&compress_params).unwrap();
        assert_eq!(compress_value["session_id"], "session-1");
        assert_eq!(
            serde_json::from_value::<RetentionCompressNowParams>(compress_value).unwrap(),
            compress_params
        );

        let delete_value = serde_json::to_value(&delete_params).unwrap();
        assert_eq!(delete_value["session_id"], "session-1");
        assert_eq!(
            serde_json::from_value::<RetentionDeleteParams>(delete_value).unwrap(),
            delete_params
        );

        // And the results roundtrip with their snake_case spellings
        let delete_result_value = serde_json::to_value(delete_result).unwrap();
        assert_eq!(delete_result_value["reclaimed_bytes"], 12_345);
        assert_eq!(
            serde_json::from_value::<RetentionDeleteResult>(delete_result_value).unwrap(),
            delete_result
        );

        // And the acks keep their single-letter wire spellings
        let compress_ok = serde_json::to_value(compress_result).unwrap();
        assert_eq!(compress_ok["ok"], true);
        assert_eq!(
            serde_json::from_value::<RetentionCompressNowResult>(compress_ok).unwrap(),
            compress_result
        );
    }

    #[test]
    fn should_reject_the_single_session_messages_when_the_session_id_is_missing() {
        // Given params without the required session id
        // When deserialized as both action faces
        let compress = serde_json::from_value::<RetentionCompressNowParams>(serde_json::json!({}));
        let delete = serde_json::from_value::<RetentionDeleteParams>(serde_json::json!({}));

        // Then both reject (the session id is required, never defaulted)
        assert!(compress.is_err());
        assert!(delete.is_err());
    }

    #[test]
    fn should_roundtrip_retention_delete_tiered_params_and_result() {
        // Given a tiered batch over the 90-day tier with a partial outcome
        let params = RetentionDeleteTieredParams {
            older_than_days: 90,
        };
        let result = RetentionDeleteTieredResult {
            deleted: vec!["session-1".to_string(), "session-2".to_string()],
            failed: vec![RetentionDeleteTieredFailure {
                session_id: "session-3".to_string(),
                error: "directory lock held by another process".to_string(),
            }],
            reclaimed_bytes: 98_765,
        };

        // When serialized to the wire shape
        let params_value = serde_json::to_value(&params).unwrap();
        let result_value = serde_json::to_value(&result).unwrap();

        // Then the spellings hold
        assert_eq!(params_value["older_than_days"], 90);
        assert_eq!(result_value["reclaimed_bytes"], 98_765);
        assert_eq!(result_value["failed"][0]["session_id"], "session-3");
        assert_eq!(
            result_value["failed"][0]["error"],
            "directory lock held by another process"
        );

        // And both deserialize back losslessly
        assert_eq!(
            serde_json::from_value::<RetentionDeleteTieredParams>(params_value).unwrap(),
            params
        );
        assert_eq!(
            serde_json::from_value::<RetentionDeleteTieredResult>(result_value).unwrap(),
            result
        );
    }

    #[test]
    fn should_reject_delete_tiered_params_when_the_days_field_is_missing() {
        // Given tiered params without the day threshold
        let value = serde_json::json!({});

        // When deserialized
        let outcome = serde_json::from_value::<RetentionDeleteTieredParams>(value);

        // Then the required field is enforced
        assert!(outcome.is_err());
    }

    #[test]
    fn should_roundtrip_retention_policy_result() {
        // Given a resolved policy
        let result = RetentionPolicyResult {
            enabled: true,
            archive_grace_days: 30,
            interval_hours: 24,
        };

        // When serialized to the wire shape
        let value = serde_json::to_value(result).unwrap();

        // Then the knob spellings match the config section's keys
        assert_eq!(value["enabled"], true);
        assert_eq!(value["archive_grace_days"], 30);
        assert_eq!(value["interval_hours"], 24);

        // And it deserializes back losslessly
        assert_eq!(
            serde_json::from_value::<RetentionPolicyResult>(value).unwrap(),
            result
        );
    }

    // --- The tier constant --------------------------------------------------

    #[test]
    fn should_pin_the_delete_tier_day_set_as_the_product_constant() {
        // Given the product's tier set
        // Then it is exactly 30/90/180/365 in ascending order
        assert_eq!(RETENTION_DELETE_TIER_DAYS, [30, 90, 180, 365]);
    }
}
