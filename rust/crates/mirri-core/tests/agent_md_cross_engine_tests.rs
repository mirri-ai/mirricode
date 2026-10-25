//! Cross-engine consistency acceptance (R2-4): the rust `.md` agent profile
//! parser must produce field-by-field identical results to the TS
//! `parseAgentFileText` on the shared fixture corpus.
//!
//! The corpus lives in a single shared location —
//! `packages/agent-profile/test/fixtures/cross-engine/corpus/` — and is
//! consumed by BOTH engines' test suites from there (no copies): this test
//! and the TS `cross-engine-fixture.test.ts` compare against the same
//! `expected-outputs.json` (regenerated from the TS side via
//! `UPDATE_CROSS_ENGINE_SNAPSHOT=1 npx vitest run`).
//!
//! Reading fixture files here is test-only IO: the zero-IO gate scans the
//! production `src` tree only, and this file lives in `tests/`.
//!
//! Normalization convention (mirrors the TS side exactly — see the fixture
//! README for the full contract):
//! - success: `{ ok: true, def: { … } }` with the eleven `AgentFileDef`
//!   payload fields, keys in alphabetical order, absent/`None` optional
//!   fields as explicit `null` (never omitted);
//! - failure: `{ ok: false, error: "<reason>" }` — the full
//!   `AgentFileParseError` message (which embeds the synthetic path);
//! - `source` is excluded (both sides parse without a discovery source);
//! - each fixture is parsed with the synthetic path `agents/<file-name>`.

use std::fs;
use std::path::PathBuf;

use mirri_core::profile::agent_md::{AgentFileDef, parse_agent_file_text};
use serde_json::{Map, Value, json};

/// Root of the shared cross-engine fixture directory, relative to this
/// crate's manifest (`rust/crates/mirri-core` → repo root → `packages/…`).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/agent-profile/test/fixtures/cross-engine")
}

/// Normalizes a parsed def: alphabetical keys, `None` → explicit `null`.
fn normalize_def(def: &AgentFileDef) -> Value {
    json!({
        "ok": true,
        "def": {
            "capabilitiesRequired": def.capabilities_required,
            "defaultModel": def.default_model,
            "description": def.description,
            "disallowedTools": def.disallowed_tools,
            "enabled": def.enabled,
            "filePath": def.file_path,
            "name": def.name,
            "prompt": def.prompt,
            "subagents": def.subagents,
            "tools": def.tools,
            "whenToUse": def.when_to_use,
        }
    })
}

#[test]
fn cross_engine_fixture_corpus_matches_shared_expected_outputs() {
    let root = fixture_root();
    let corpus_dir = root.join("corpus");

    // Sorted fixture file names — the same enumeration the TS side does.
    let mut names: Vec<String> = fs::read_dir(&corpus_dir)
        .expect("corpus directory must exist (run from a full checkout)")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect();
    names.sort();

    let mut actual = Map::new();
    for name in &names {
        let text = fs::read_to_string(corpus_dir.join(name))
            .unwrap_or_else(|error| panic!("failed to read fixture {name}: {error}"));
        let path = format!("agents/{name}");
        let value = match parse_agent_file_text(&path, &text) {
            Ok(def) => normalize_def(&def),
            Err(error) => json!({ "ok": false, "error": error.reason }),
        };
        actual.insert(name.clone(), value);
    }

    let expected_text =
        fs::read_to_string(root.join("expected-outputs.json")).expect("expected-outputs.json");
    let expected: Value = serde_json::from_str(&expected_text).expect("valid expected JSON");
    let expected = expected
        .as_object()
        .expect("expected-outputs.json must be a mapping of fixture → output");

    // Compare fixture-by-fixture so a mismatch names the offending file.
    let mut expected_names: Vec<&String> = expected.keys().collect();
    expected_names.sort();
    let actual_names: Vec<&String> = actual.keys().collect();
    assert_eq!(
        actual_names, expected_names,
        "corpus files covered by expected-outputs.json must match"
    );
    for (name, expected_value) in expected {
        let actual_value = actual
            .get(name)
            .expect("every expected fixture must have been parsed");
        assert_eq!(
            actual_value, expected_value,
            "fixture {name}: rust output differs from the shared expected-outputs.json \
             (regenerate from the TS side with UPDATE_CROSS_ENGINE_SNAPSHOT=1 if the \
             change is intentional — the rust side must then still pass unedited)"
        );
    }
}

/// Base-prompt template mirror guard: the rust `system_prompt.md` and the TS
/// `system.md` are documented mirrors (placeholder spelling aside, content
/// must not drift — both engines emit the template through the
/// `base_prompt_default_template` wire field, so drift is a cross-engine
/// inconsistency, not a cosmetic one). This companion to the TS-side guard
/// (`packages/agent-core-v2/test/lint/base-prompt-mirror.test.ts`) runs in
/// the rust CI suite, which the TS package's does not — keeping the guard
/// effective regardless of which side drifts.
#[test]
fn system_prompt_template_matches_ts_mirror() {
    let rust_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/scope/system_prompt.md");
    let ts_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/agent-core-v2/src/app/agentProfileCatalog/system.md");

    let rust_text = fs::read_to_string(&rust_path).expect("read rust system_prompt.md");
    let ts_text = fs::read_to_string(&ts_path).expect("read TS system.md mirror");

    // Normalize the placeholder spelling — the one sanctioned difference
    // between the two files: rust `{{var}}` → TS `${var}`. Names are `\w+`.
    let rust_chars: Vec<char> = rust_text.chars().collect();
    let mut normalized = String::with_capacity(rust_text.len());
    let mut i = 0;
    while i < rust_chars.len() {
        if rust_chars[i] == '{'
            && rust_chars.get(i + 1) == Some(&'{')
            && rust_chars
                .get(i + 2)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            normalized.push('$');
            normalized.push('{');
            i += 2;
            while i < rust_chars.len()
                && (rust_chars[i].is_ascii_alphanumeric() || rust_chars[i] == '_')
            {
                normalized.push(rust_chars[i]);
                i += 1;
            }
            if rust_chars.get(i) == Some(&'}') && rust_chars.get(i + 1) == Some(&'}') {
                normalized.push('}');
                i += 2;
            }
        } else {
            normalized.push(rust_chars[i]);
            i += 1;
        }
    }

    assert_eq!(
        normalized, ts_text,
        "the base prompt template mirrors drifted — whichever file you edited, \
         mirror the change in the other (placeholder spelling: rust {{var}}, TS ${{var}})"
    );
}
