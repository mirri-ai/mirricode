//! Cross-engine list-projection consistency acceptance (R4-6 / US-C1):
//! the rust agent-profile discovery + merge + wire-row projection must
//! agree with the TS `GET /api/v1/agents` catalog projection on the same
//! corpus, row by row.
//!
//! The corpus lives in a single shared location —
//! `packages/agent-profile/test/fixtures/cross-engine-list/` — and is
//! consumed by BOTH engines' test suites from there (no copies): this
//! test and the TS `agents-cross-engine-list.test.ts` compare against the
//! same `expected-list.json` (regenerated from the TS side via
//! `UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1`, the reference implementation).
//!
//! Root mounting (see the fixture README for the full contract): the
//! corpus `project-agents/` directory is mounted as a `Project`-tagged
//! root FIRST (mirroring the TS catalog's `workspace 30 > user 10` source
//! priority) and `user-agents/` as the `User`-tagged root second — the
//! same layout the TS suite mounts as the registered workspace's
//! `.mirri-code/agents` and the `<home>/agents` user root. The
//! `legacy-profiles/` directory is rust-only (the TS engine cannot read
//! `.toml`/`.json` agent files — the documented exception) and is
//! asserted separately, never through the shared expectation.
//!
//! Reading fixture files here is test-only IO: the zero-IO gate scans the
//! production `src` tree only, and this file lives in `tests/`.
//!
//! Normalization convention (mirrors the TS side exactly — see the fixture
//! README for the full contract):
//! - rows sorted by name; every projected field comes from the shared
//!   wire-row base projection
//!   ([`ProfileManager::profile_wire_row_base`] — the same function the
//!   embedded server's REST row composes, so this test asserts the real
//!   projection, not a test-side copy);
//! - `suppressed_candidates` emitted only when non-empty, `{source,
//!   priority, reason}` with the reason spelling `priority`/`disabled`;
//! - `skipped` entries carry the file **basename** and a `<load-failure>`
//!   placeholder reason — the two engines word their parse-error messages
//!   differently by design (the TS text also embeds the TS-side mount
//!   path), so only the presence and identity of the skipped file is
//!   asserted (the underlying reason is asserted non-empty locally).

use std::fs;
use std::path::PathBuf;

use mirri_core::ProfileManager;
use mirri_core::profile::{
    ProfileDiscoveryResult, ProfileSource, ProfileSourceRoot, SkippedProfileFile,
    SuppressedProfileCandidate, SuppressedReason, discover_profiles,
};
use mirri_os::LocalSyncFs;
use serde_json::{Value, json};

/// Root of the shared cross-engine list fixture directory, relative to
/// this crate's manifest (`rust/crates/mirri-core` → repo root →
/// `packages/…`).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/agent-profile/test/fixtures/cross-engine-list")
}

/// The shared root mounting: project root first (priority 30), user root
/// second (priority 10) — the same relative order the TS catalog's source
/// priorities produce, so a same-name project file wins its merge.
fn shared_roots() -> Vec<ProfileSourceRoot> {
    vec![
        ProfileSourceRoot {
            path: fixture_root().join("project-agents"),
            source: ProfileSource::Project,
        },
        ProfileSourceRoot {
            path: fixture_root().join("user-agents"),
            source: ProfileSource::User,
        },
    ]
}

/// Discover the roots and register the winners into a fresh manager —
/// the boot bookkeeping [`ProfileManager::load_from_roots`] performs
/// (registration + provenance + the suppressed-candidates projection),
/// with tagged roots that the user-level-only loader cannot express.
/// Returns the manager plus the raw discovery result (whose skip channel
/// is the data `load_from_roots` would surface).
fn boot_manager(roots: &[ProfileSourceRoot]) -> (ProfileManager, ProfileDiscoveryResult) {
    let result = discover_profiles(roots, &LocalSyncFs).expect("discovery should succeed");
    let manager = ProfileManager::new();
    for discovered in &result.profiles {
        manager
            .register_discovered(discovered)
            .expect("registration should succeed");
    }
    manager.set_suppressed_candidates(result.suppressed.clone());
    (manager, result)
}

/// Normalize the skip channel onto the shared projection shape: basename
/// plus the `<load-failure>` placeholder reason (the engines word their
/// parse errors differently; the reason text itself is asserted non-empty
/// separately).
fn normalize_skipped(skipped: &[SkippedProfileFile]) -> Vec<Value> {
    let mut entries: Vec<(String, Value)> = skipped
        .iter()
        .map(|file| {
            let base = file
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.display().to_string());
            let reason = if file.reason.is_empty() {
                "<empty-reason>"
            } else {
                "<load-failure>"
            };
            (base.clone(), json!({ "path": base, "reason": reason }))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(_, value)| value).collect()
}

#[test]
fn cross_engine_list_projection_matches_shared_expected_list() {
    let (manager, result) = boot_manager(&shared_roots());

    let profiles: Vec<Value> = manager
        .list_all()
        .iter()
        .map(|name| {
            manager
                .profile_wire_row_base(name)
                .expect("every listed name is registered and projects a row")
        })
        .collect();
    let actual = json!({
        "profiles": profiles,
        "skipped": normalize_skipped(&result.skipped),
    });

    let expected_text = fs::read_to_string(fixture_root().join("expected-list.json"))
        .expect("expected-list.json must exist (regenerate from the TS side)");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");

    // Compare row-by-row so a mismatch names the offending profile.
    let expected_rows = expected["profiles"]
        .as_array()
        .expect("expected profiles must be an array");
    let actual_rows = actual["profiles"]
        .as_array()
        .expect("actual profiles must be an array");
    assert_eq!(
        actual_rows.len(),
        expected_rows.len(),
        "rust list row count differs from the shared expected-list.json"
    );
    for (index, expected_row) in expected_rows.iter().enumerate() {
        assert_eq!(
            &actual_rows[index], expected_row,
            "rust row {} differs from the shared expected-list.json \
             (regenerate from the TS side with UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1 \
             if the change is intentional — the rust side must then still pass unedited)",
            expected_row["name"],
        );
    }
    assert_eq!(
        actual["skipped"], expected["skipped"],
        "rust skip channel differs from the shared expected-list.json"
    );
}

/// The rust-only legacy root (documented exception): `.toml` / `.json`
/// agent files load through the legacy `profiles/` root — a surface the
/// TS engine does not have — and a broken legacy `extends` covering a
/// built-in name must leave the built-in row clean (the Task 27 review
/// M1 regression, at integration level).
#[test]
fn legacy_root_loads_rust_only_formats_and_keeps_the_builtin_row_clean() {
    let mut roots = shared_roots();
    roots.push(ProfileSourceRoot {
        path: fixture_root().join("legacy-profiles"),
        source: ProfileSource::User,
    });
    let (manager, result) = boot_manager(&roots);

    // The legacy formats register as user-level profiles through the
    // legacy root (lowest priority — after the user agents root, the real
    // boot chain's order).
    for name in ["veteran", "archivist"] {
        let provenance = manager
            .provenance(name)
            .unwrap_or_else(|| panic!("legacy profile {name} must be registered"));
        assert_eq!(provenance.source, ProfileSource::User);
        assert!(manager.suppressed_candidates(name).is_empty());
        assert!(manager.get(name).is_some());
    }

    // The broken-extends `.toml` covering the built-in `coder` name: the
    // file is a legitimate load failure, so the built-in row stays clean
    // (builtin provenance, NO suppressed candidates — no self-referential
    // phantom builtin loser) and the file lands in the skip channel with
    // a non-empty reason.
    assert_eq!(
        manager.provenance("coder").map(|p| p.source),
        Some(ProfileSource::Builtin)
    );
    assert!(manager.suppressed_candidates("coder").is_empty());
    let broken = result
        .skipped
        .iter()
        .find(|file| file.path.ends_with("coder.toml"))
        .expect("the broken legacy coder override must be skipped");
    assert!(!broken.reason.is_empty());

    // The broken file must not disturb the shared corpus's own rows.
    for name in [
        "agent", "coder", "explore", "guarded", "mystery", "plan", "plain", "shared", "sleeper",
    ] {
        assert!(
            manager.get(name).is_some(),
            "profile {name} must stay registered with the legacy root mounted"
        );
    }
}

/// The same shared corpus mounted OUT OF priority order (the user root
/// first, the project root second) must produce the identical projection —
/// the same-name winner is decided by source priority inside discovery
/// (T19 m1), never by the caller's root order, so the disordered mounting
/// is equivalent to the canonical one by construction.
#[test]
fn cross_engine_list_projection_is_root_order_independent() {
    let mut roots = shared_roots();
    roots.reverse();
    let (manager, result) = boot_manager(&roots);

    // The priority-decided winners: the project file wins `shared`, the
    // user file wins `guarded` (its project candidate is disabled), and the
    // user file covers the essential — exactly as in the canonical order.
    assert_eq!(
        manager.provenance("shared").map(|p| p.source),
        Some(ProfileSource::Project)
    );
    assert_eq!(
        manager.provenance("guarded").map(|p| p.source),
        Some(ProfileSource::User)
    );
    assert_eq!(
        manager.suppressed_candidates("shared"),
        vec![SuppressedProfileCandidate {
            source: ProfileSource::User,
            reason: SuppressedReason::Priority,
        }]
    );

    // And the full row-by-row projection still matches the shared
    // expected-list.json (the same acceptance as the canonical mounting).
    let profiles: Vec<Value> = manager
        .list_all()
        .iter()
        .map(|name| {
            manager
                .profile_wire_row_base(name)
                .expect("every listed name is registered and projects a row")
        })
        .collect();
    let actual = json!({
        "profiles": profiles,
        "skipped": normalize_skipped(&result.skipped),
    });

    let expected_text = fs::read_to_string(fixture_root().join("expected-list.json"))
        .expect("expected-list.json must exist (regenerate from the TS side)");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");
    assert_eq!(
        actual, expected,
        "the disordered mounting must produce the same projection as the canonical one"
    );
}
