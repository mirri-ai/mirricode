//! E9 / multimodal ADR-007 §11.4: the `message.send` `references`
//! parameter — workspace file/dir references materialized into the
//! turn's context (one user context message with the `file_reference`
//! origin, injected BEFORE the user's body message).

use std::sync::Arc;

use serde_json::json;

use super::*;

/// One `file_reference`-origin context message recorded on the wire —
/// the injected materialization's durable `context.append_message` row.
fn file_reference_message(ops: &[mirri_proto::WireOp]) -> Option<Value> {
    ops.iter().find_map(|op| match op {
        mirri_proto::WireOp::ContextAppendMessage { message }
            if message["origin"]["kind"] == "file_reference" =>
        {
            Some(message.clone())
        }
        _ => None,
    })
}

/// One `skill.activate`-style payload op — the `file.reference` summary
/// record the injection journals for observability (US9.10 / M5).
fn file_reference_summary(ops: &[mirri_proto::WireOp]) -> Option<Value> {
    ops.iter().find_map(|op| match op {
        mirri_proto::WireOp::FileReferenceSummary { summary } => Some(summary.clone()),
        _ => None,
    })
}

/// A server over a throwaway workspace with a scripted LLM and a created
/// session — the standard send-path test shape (`test_server` + one
/// `session.create`).
async fn reference_server(
    mock: &Arc<ScriptedLlm>,
) -> (EmbeddedServer, tempfile::TempDir, String) {
    let (server, workspace) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(
        mock,
    )))));
    let created = server
        .call_method("session.create", json!({ "title": "file-reference" }))
        .await
        .expect("session created");
    let session_id = created["id"].as_str().expect("session id").to_string();
    (server, workspace, session_id)
}

/// should materialize a file reference into the turn context
#[tokio::test]
async fn should_materialize_file_reference_into_turn_context() {
    // Given a workspace file and a scripted LLM
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("seen");
    let (server, workspace, session_id) = reference_server(&mock).await;
    std::fs::create_dir_all(workspace.path().join("src")).unwrap();
    std::fs::write(
        workspace.path().join("src/a.ts"),
        "export const A = 1;",
    )
    .unwrap();

    // When the message carries one file reference
    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "看看这个",
                "references": [{ "path": "src/a.ts", "kind": "file" }],
            }),
        )
        .await
        .expect("send with references succeeds");

    // Then the model's request carries the wrapped file content BEFORE
    // the user's body text
    let first_call = serde_json::to_string(&mock.messages_of_call(0)).unwrap_or_default();
    let wrap_at = first_call
        .find("<file-reference path=\\\"src/a.ts\\\">")
        .expect("the LLM must see the file-reference wrap");
    let body_at = first_call.find("看看这个").expect("the body must reach the LLM");
    assert!(
        wrap_at < body_at,
        "the reference must precede the body: {first_call}"
    );
    assert!(
        first_call.contains("export const A = 1;"),
        "the file content must be inlined: {first_call}"
    );

    // And the wire log carries the durable file_reference message with
    // the user body still recorded verbatim
    let ops = read_all_wire_ops(&server, &session_id).await;
    let reference = file_reference_message(&ops).expect("file_reference wire message");
    assert_eq!(reference["role"], "user");
    assert!(
        reference["content"][0]["text"]
            .as_str()
            .expect("text content")
            .contains("export const A = 1;"),
        "the wire row carries the inlined content: {reference}"
    );
    let body = ops
        .iter()
        .find_map(|op| match op {
            mirri_proto::WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "user" =>
            {
                Some(message.clone())
            }
            _ => None,
        })
        .expect("body message recorded");
    assert_eq!(body["content"][0]["text"], "看看这个");
}

/// should inject a directory as a path listing capped at 200 entries
#[tokio::test]
async fn should_inject_directory_as_listing_with_cap() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, workspace, session_id) = reference_server(&mock).await;
    let dir = workspace.path().join("tree");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..250 {
        std::fs::write(dir.join(format!("f{i:03}.txt")), "x").unwrap();
    }

    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "list it",
                "references": [{ "path": "tree", "kind": "directory" }],
            }),
        )
        .await
        .expect("send with a directory reference succeeds");

    let ops = read_all_wire_ops(&server, &session_id).await;
    let reference = file_reference_message(&ops).expect("file_reference wire message");
    let text = reference["content"][0]["text"].as_str().expect("listing text");
    assert!(
        text.starts_with("<file-reference path=\"tree\" kind=\"directory\">"),
        "the listing wrap names the directory: {text}"
    );
    let listed = text
        .lines()
        .filter(|line| line.starts_with("f") && line.ends_with(".txt"))
        .count();
    assert_eq!(listed, 200, "exactly the 200-entry budget: {text}");
    assert!(
        text.contains("truncated at 200 entries"),
        "the truncation declaration must be present: {text}"
    );
}

/// should exclude gitignored children from a directory listing
#[tokio::test]
async fn should_exclude_gitignored_children_from_listing() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, workspace, session_id) = reference_server(&mock).await;
    let dir = workspace.path().join("proj");
    std::fs::create_dir_all(dir.join("keep")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("visible.ts"), "x").unwrap();
    std::fs::write(dir.join("secret.txt"), "x").unwrap();
    // The workspace root's .gitignore rules out both a whole directory and
    // a single file (M2: the listing must honor ignore rules).
    std::fs::write(
        workspace.path().join(".gitignore"),
        "proj/node_modules/\nproj/secret.txt\n",
    )
    .unwrap();

    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "list",
                "references": [{ "path": "proj", "kind": "directory" }],
            }),
        )
        .await
        .expect("send with a directory reference succeeds");

    let ops = read_all_wire_ops(&server, &session_id).await;
    let reference = file_reference_message(&ops).expect("file_reference wire message");
    let text = reference["content"][0]["text"].as_str().expect("listing text");
    assert!(
        text.contains("keep/"),
        "the kept directory is listed: {text}"
    );
    assert!(
        text.contains("visible.ts"),
        "the kept file is listed: {text}"
    );
    assert!(
        !text.contains("node_modules"),
        "a gitignored directory must not be listed: {text}"
    );
    assert!(
        !text.contains("secret.txt"),
        "a gitignored file must not be listed: {text}"
    );
}

/// should inject a truncation notice when the file exceeds 1500 lines
#[tokio::test]
async fn should_inject_truncation_notice_for_large_file() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, workspace, session_id) = reference_server(&mock).await;
    let big: String = (0..2000)
        .map(|i| format!("line-{i}\n"))
        .collect();
    std::fs::write(workspace.path().join("big.txt"), big).unwrap();

    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "summarize",
                "references": [{ "path": "big.txt", "kind": "file" }],
            }),
        )
        .await
        .expect("send with a large file reference succeeds");

    let ops = read_all_wire_ops(&server, &session_id).await;
    let reference = file_reference_message(&ops).expect("file_reference wire message");
    let text = reference["content"][0]["text"].as_str().expect("file text");
    assert!(
        text.contains("line-1499\n"),
        "the first 1500 lines are inlined: {}",
        &text[..text.len().min(200)]
    );
    assert!(
        !text.contains("line-1500\n"),
        "nothing past the 1500-line budget survives"
    );
    assert!(
        text.contains("truncated at 1500 lines"),
        "the truncation declaration must be present"
    );
}

/// should report an invalid reference in the response and still run the turn
#[tokio::test]
async fn should_report_invalid_reference_in_response() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, _workspace, session_id) = reference_server(&mock).await;

    let result = server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "gone?",
                "references": [{ "path": "src/gone.ts", "kind": "file" }],
            }),
        )
        .await
        .expect("the turn still runs");

    // Then the response carries the invalid-reference list
    let invalid = result["invalidReferences"]
        .as_array()
        .expect("invalidReferences array");
    assert_eq!(invalid.len(), 1);
    assert_eq!(invalid[0]["path"], "src/gone.ts");
    assert_eq!(invalid[0]["reason"], "not_found");
    // And the turn completed (the scripted reply reached the client)
    assert_eq!(result["turn"]["stopReason"], "end_turn");

    // And the model was told the reference is unavailable — the notice
    // rides the same injected context message
    let first_call = serde_json::to_string(&mock.messages_of_call(0)).unwrap_or_default();
    assert!(
        first_call.contains("src/gone.ts"),
        "the model sees the unavailable reference named: {first_call}"
    );
    assert!(
        first_call.contains("gone"),
        "the turn's body still reached the model: {first_call}"
    );
}

/// should deduplicate repeated references to the same path
#[tokio::test]
async fn should_deduplicate_repeated_references() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, workspace, session_id) = reference_server(&mock).await;
    std::fs::write(workspace.path().join("dup.txt"), "only once").unwrap();

    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "twice",
                "references": [
                    { "path": "dup.txt", "kind": "file" },
                    { "path": "dup.txt", "kind": "file" },
                ],
            }),
        )
        .await
        .expect("send with duplicate references succeeds");

    let ops = read_all_wire_ops(&server, &session_id).await;
    let reference = file_reference_message(&ops).expect("file_reference wire message");
    let text = reference["content"][0]["text"].as_str().expect("text");
    assert_eq!(
        text.matches("<file-reference path=\"dup.txt\">").count(),
        1,
        "one wrap per unique path: {text}"
    );
    assert_eq!(
        text.matches("only once").count(),
        1,
        "the content is injected exactly once: {text}"
    );
}

/// should record a fileReferenceSummary wire op for observability (US9.10)
#[tokio::test]
async fn should_record_file_reference_summary_op() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("ok");
    let (server, workspace, session_id) = reference_server(&mock).await;
    std::fs::write(workspace.path().join("small.txt"), "abc").unwrap();

    server
        .call_method(
            "message.send",
            json!({
                "session_id": session_id,
                "text": "note it",
                "references": [{ "path": "small.txt", "kind": "file" }],
            }),
        )
        .await
        .expect("send with a reference succeeds");

    let ops = read_all_wire_ops(&server, &session_id).await;
    let summary = file_reference_summary(&ops).expect("file.reference summary op");
    let entries = summary.as_array().expect("summary entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "small.txt");
    assert_eq!(entries[0]["form"], "full");
    assert_eq!(entries[0]["lines"], 1);
    assert_eq!(entries[0]["bytes"], 3);
}
