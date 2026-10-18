// ---------------------------------------------------------------------------
// spec 09 / ADR-020 + SDR-016 — LLM 会话标题生成（旁路调用）：轮内首个
//响应落地后的 cache-riding 旁路调用命名（同一 resolver、同一前缀）、
// 轮内失败→settle 重试→规则式兜底的降级链（§2.3）、手动改名守卫（A2）、
// 脱敏（A5）、claim 的一次性语义（Task 2）、A9（轮内失败不兜底，
// settle 重试后才兜底）、A11（idle steer 首轮语义）、红线 3（cron/task
// 轮次不参与命名）、F-1（abort 残余窗口强制闭合）与 I-1（no-LLM
// 分支标题落盘）。SetSessionTitle 工具与注入器已退役（Task 4）：
// 命名完全旁路。
// ---------------------------------------------------------------------------

use super::*;
use mirri_core::llm::LlmToolCall;

/// The session directory (`…/v3_sessions/<bucket>/<session_id>`) a test
/// server materializes under its tempdir home — the location of the
/// `state.json` the settle write-throughs land in.
fn session_dir_under(tmp: &tempfile::TempDir, session_id: &str) -> std::path::PathBuf {
    let bucket = mirri_core::encode_work_dir_key(tmp.path());
    tmp.path()
        .join(".mirri")
        .join("v3_sessions")
        .join(&bucket)
        .join(session_id)
}

/// Poll until the scripted LLM has served `count` chat calls — the
/// deterministic seam for waiting out the out-of-band side title call
/// (which produces no wire event when its fallback derives nothing, e.g.
/// an image-only prompt).
async fn wait_for_chat_calls(mock: &ScriptedLlm, count: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.chat_call_count() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the scripted LLM must serve {count} chat calls"));
}

/// Poll until the session's live metadata carries `expected` as its title
/// — the side title call is spawned at settle, so the title lands
/// asynchronously after `message.send` already returned.
async fn wait_for_title(session: &SessionScope, expected: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if session.metadata().await.title == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the session title must become {expected:?}"));
}

/// Given an untitled session and a scripted LLM whose FIRST response is the
/// main turn's text and SECOND response is a concise title, when a wait-mode
/// `message.send` completes, then the session title is the side call's title
/// (not the prompt-text excerpt), `is_custom_title` stays false, the wire
/// carries `event.session.meta.updated`, and `state.json` on disk carries the
/// title (the side call's own write-through after the title lands).
#[tokio::test]
async fn should_title_via_side_llm_call_after_the_first_user_turn_settles() {
    // Given a server whose scripted LLM serves the main turn, then the
    // side title call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Refactor the auth module");
    let (server, tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // When the first prompt is sent (wait mode)
    let result = server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please help me restructure the authentication code",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // Then the turn completed and the side call titled the session
    assert_eq!(result["ok"], true);
    let event = next_event_where(
        &mut events,
        |n| n.method == "event.session.meta.updated",
        |_| {},
    )
    .await;
    assert_eq!(event.params["title"], "Refactor the auth module");

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Refactor the auth module");
    assert!(!metadata.is_custom_title);

    // And the side call's write-through persisted the title to disk
    let state = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) =
                std::fs::read_to_string(session_dir_under(&tmp, &session_id).join("state.json"))
                && raw.contains("Refactor the auth module")
            {
                return raw;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("state.json must carry the side call's title");
    let state: serde_json::Value =
        serde_json::from_str(&state).expect("state.json must be valid JSON");
    assert!(!state["is_custom_title"].as_bool().unwrap_or(true));
}

#[tokio::test]
async fn should_title_via_side_llm_call_when_the_first_response_lands_mid_turn() {
    // Given a server whose scripted LLM parks the main turn's first
    // response behind a tool call (the turn stays in flight after it),
    // then serves the side title call
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated_tool_use(vec![LlmToolCall {
        id: "call-read-1".to_string(),
        name: "Read".to_string(),
        args: json!({ "path": "/tmp/some/long/running/task.txt" }),
    }]);
    mock.push_text("Refactor the auth module");
    mock.push_text("Done.");
    let (server, tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let mut events = server.subscribe_events().await.unwrap();

    // When the first prompt is sent (fire mode — the turn keeps running
    // after the first response) and the first step's response lands
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please help me restructure the authentication code",
                "mode": "fire",
            }),
        )
        .await
        .unwrap();
    // Let the first step land (its tool call completes), which publishes
    // the first `TurnStepCompleted` — the in-turn side call fires there,
    // while the main loop parks at the second LLM call behind the gate.
    release.send(()).unwrap();

    // Then the in-turn side call already titled the session (SDR-016) —
    // the title landed while the turn is in flight, no settle needed.
    let expected = "Refactor the auth module";
    wait_for_title(&session, expected).await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, expected);
    assert!(!metadata.is_custom_title);

    // And the side call's write-through persisted the title to disk
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) =
                std::fs::read_to_string(session_dir_under(&tmp, &session_id).join("state.json"))
                && raw.contains(expected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("state.json must carry the side call's title");

    // Cleanup: let the parked turn finish so the test ends with no
    // dangling turn task.
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
}

/// Given an untitled session whose side title call fails in turn (the
/// scripted second response is a provider error) and whose settle retry
/// also fails, when a wait-mode `message.send` completes, then the
/// rule-based fallback names the session from the user's message text
/// through the shared sanitize/truncate rule (§2.3) — the title is
/// auto-flagged, and the wire carries the `event.session.meta.updated`
/// event announcing it.
#[tokio::test]
async fn should_fall_back_to_prompt_text_when_the_side_title_call_fails() {
    // Given a server whose scripted LLM answers the main turn with plain
    // text and fails both the in-turn side call and its settle retry
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_error("side call exploded");
    mock.push_error("side call retry exploded");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // When the first prompt is sent (wait mode, side call fails)
    let text = "please help me restructure the   authentication code";
    let result = server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": &text,
                "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // Then the turn completed
    assert_eq!(result["turn"]["stopReason"], "end_turn");

    // And the session title is the user text's sanitized excerpt (§2.3) —
    // the same rule the side call's raw text goes through, never a
    // verbatim copy
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let expected = mirri_core::session::title::title_from_prompt_text(text).unwrap();
    wait_for_title(&session, &expected).await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, expected);
    assert!(!metadata.is_custom_title);

    // And the subscriber observed the fallback land on the wire
    let event = next_event_where(
        &mut events,
        |n| n.method == "event.session.meta.updated",
        |_| {},
    )
    .await;
    assert_eq!(event.params["title"], json!(expected));
}

/// Given an untitled session whose first turn is cancelled before the model
/// could reply, when the turn settles as cancelled, then the side call
/// still runs (a cancelled first user turn settles the naming question) —
/// the session takes the side call's title, not the placeholder.
#[tokio::test]
async fn should_fall_back_when_first_turn_is_cancelled() {
    // Given a server whose scripted LLM parks the first reply at a gate,
    // then serves the side title call
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("a reply that arrives too late to matter");
    mock.push_text("Fix the login flow");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the wait-mode send runs while an abort fires as soon as the
    // turn reaches the LLM call, then the gate opens
    let send = server.call_method(
        "message.send",
        json!({
            "session_id": &session_id,
            "text": "fix the login bug in auth",
            "mode": "wait",
        }),
    );
    let aborter = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while mock.chat_call_count() < 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn must reach the LLM call"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        server
            .call_method("message.abort", json!({ "session_id": &session_id }))
            .await
            .unwrap();
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(send, aborter);
    let result = result.unwrap();

    // Then the turn settled as cancelled
    assert_eq!(result["turn"]["stopReason"], "aborted");

    // And the side call still ran: the session takes its title
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_title(&session, "Fix the login flow").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Fix the login flow");
    assert!(!metadata.is_custom_title);
}

/// Given an untitled session whose first turn is cancelled before the model
/// could reply (no response landed, so the in-turn watch never fired) AND
/// whose side title call then fails at the settle retry, when both settle,
/// then the fallback names the session from the user's text (the failure
/// arm of the cancelled-turn semantics).
#[tokio::test]
async fn should_fall_back_to_prompt_text_when_the_side_title_call_fails_after_a_cancelled_first_turn()
 {
    // Given a server whose scripted LLM parks the first reply at a gate,
    // then fails the side title call's in-turn attempt and its settle retry
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("a reply that arrives too late to matter");
    mock.push_error("side call exploded");
    mock.push_error("side call retry exploded");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the wait-mode send is aborted mid-LLM-call and the side call
    // then fails
    let send = server.call_method(
        "message.send",
        json!({
            "session_id": &session_id,
            "text": "fix the login bug in auth",
            "mode": "wait",
        }),
    );
    let aborter = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while mock.chat_call_count() < 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn must reach the LLM call"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        server
            .call_method("message.abort", json!({ "session_id": &session_id }))
            .await
            .unwrap();
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(send, aborter);
    assert_eq!(result.unwrap()["turn"]["stopReason"], "aborted");

    // Then the fallback named the session from the user's text
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_title(&session, "fix the login bug in auth").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "fix the login bug in auth");
    assert!(!metadata.is_custom_title);
}

/// Given a session the side call already named, when a later user turn
/// fails (its settle site runs the rule-based fallback), then the naming
/// guard refuses the second write: the side call's title survives.
#[tokio::test]
async fn should_not_overwrite_a_side_call_title_with_fallback() {
    // Given a server whose scripted LLM answers the main turn, serves the
    // side title call, and then fails the second turn's main call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Model's concise title");
    mock.push_error("the second turn explodes");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the first turn settles and the side call names the session
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "restructure the authentication module please",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_title(&session, "Model's concise title").await;

    // And a second user turn fails (its error settle runs the fallback)
    let error = server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "second prompt",
                "mode": "wait",
            }),
        )
        .await
        .expect_err("the second turn must fail");
    assert!(
        error.to_string().contains("the second turn explodes"),
        "got: {error}"
    );

    // Then the side call's title survived the fallback (guard: first
    // writer wins)
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Model's concise title");
    assert!(!metadata.is_custom_title);
}

/// Given a session whose first user prompt carries no text (an image-only
/// input), when the turn ends and the side call fails, then the fallback
/// has nothing to excerpt and the session stays untitled (A6) — and the
/// next user message that DOES carry text gets the side call again (the
/// failed attempt released the one-shot claim): its side call names it.
#[tokio::test]
async fn should_keep_untitled_when_first_prompt_has_no_text() {
    // Given a server whose scripted LLM answers the image-only main turn,
    // fails the first side call's in-turn attempt and its settle retry,
    // then answers a second text turn and its side call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("I see the image.");
    mock.push_error("side call exploded");
    mock.push_error("side call retry exploded");
    mock.push_text("Done.");
    mock.push_text("Screenshot summary");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the first prompt is a pure image input (no text part)
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "mode": "wait",
                "input": [
                    { "type": "image", "source": { "kind": "base64", "media_type": "image/png", "data": "aGk=" } },
                ],
            }),
        )
        .await
        .unwrap();

    // Then the title stays the placeholder — both side call attempts
    // failed and the fallback had no text to excerpt (A6: no title beats
    // a fabricated one)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_chat_calls(&mock, 3).await;
    let metadata = session.metadata().await;
    assert!(
        mirri_core::session::title::is_untitled_title(&metadata.title),
        "an image-only first prompt with a failed side call must stay \
         untitled (got {:?})",
        metadata.title
    );

    // And the next user message with text re-arms the offer: its side call
    // names the session (the failed attempt released the claim)
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please summarize the screenshot",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();
    wait_for_title(&session, "Screenshot summary").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Screenshot summary");
    assert!(!metadata.is_custom_title);
}

/// Given an untitled session whose side title call returns text that
/// sanitizes to nothing displayable (a control-characters-only title — the
/// one raw shape `title_from_prompt_text` maps to `None`), when the settle
/// runs the D4 degradation, then the claim is released and the rule-based
/// fallback runs (an image-only prompt leaves the session untitled) — and
/// the released claim lets the NEXT user turn's side call retry and title
/// the session.
#[tokio::test]
async fn should_fall_back_and_release_the_claim_when_the_side_call_title_sanitizes_to_nothing() {
    // Given a server whose scripted LLM answers an image-only main turn,
    // serves a control-chars-only side title on the in-turn attempt (the
    // sanitizer's control-char collapse empties it), on the settle retry
    // as well, then answers a second, text turn and its side call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("I see the image.");
    mock.push_text("\u{0007}");
    mock.push_text("\u{0007}");
    mock.push_text("Done.");
    mock.push_text("Retry title");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the first prompt is a pure image input (no text part)
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "mode": "wait",
                "input": [
                    { "type": "image", "source": { "kind": "base64", "media_type": "image/png", "data": "aGk=" } },
                ],
            }),
        )
        .await
        .unwrap();

    // Then the session stays untitled: both side call attempts sanitized
    // to nothing (plan D4) and the fallback had no text to excerpt either
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_chat_calls(&mock, 3).await;
    let metadata = session.metadata().await;
    assert!(
        mirri_core::session::title::is_untitled_title(&metadata.title),
        "a side title that sanitizes to nothing must leave the session \
         untitled (got {:?})",
        metadata.title
    );

    // And the next user message with text re-arms the offer: the D4
    // degradation released the claim, so its side call runs again and
    // names the session
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please summarize the screenshot",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();
    wait_for_title(&session, "Retry title").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Retry title");
    assert!(!metadata.is_custom_title);
}

/// Given a session that is still untitled after its (text-less) first user
/// turn, when a cron turn of the same agent runs on the reused turn flow,
/// then the cron turn neither derives a title from its own prompt (red
/// line 3 — the side call lives only on user-turn settle sites) nor offers
/// the model any naming surface: the retired tool is absent from its
/// serialized tools list and no naming instruction rides the request.
#[tokio::test]
async fn should_not_fall_back_or_inject_for_cron_turns() {
    // Given a server whose scripted LLM answers the text-less user turn,
    // fails that turn's side title call both in turn and at the settle
    // retry (the session stays untitled), and answers the later cron
    // turn with plain text
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("I see the image.");
    mock.push_error("side call exploded");
    mock.push_error("side call retry exploded");
    mock.push_text("cron reply");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // And a first user turn whose prompt carries no text (the session stays
    // untitled — the state a leaking naming surface would be visible in)
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "mode": "wait",
                "input": [
                    { "type": "image", "source": { "kind": "base64", "media_type": "image/png", "data": "aGk=" } },
                ],
            }),
        )
        .await
        .unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    // Both side title attempts failed with nothing to fall back on —
    // wait them out so the scripted queue is deterministic from here on.
    wait_for_chat_calls(&mock, 3).await;
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");

    // When a cron turn of the same agent runs on the same turn flow
    let tool = run_main_agent_tool(
        &server,
        &session_id,
        "CronCreate",
        json!({ "cron": "*/5 * * * *", "prompt": "tick", "recurring": true }),
    )
    .await;
    assert!(!tool.is_error, "{tool:?}");
    let task_id = cron_task_id_from_tool_result(&tool);
    server
        .call_method(
            "cron.trigger",
            json!({ "session_id": &session_id, "task_id": task_id }),
        )
        .await
        .unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;

    // Then the session title is still the placeholder — the cron prompt
    // never became a title (red line 3)
    let metadata = session.metadata().await;
    assert!(
        mirri_core::session::title::is_untitled_title(&metadata.title),
        "a cron turn must not derive the title (got {:?})",
        metadata.title
    );

    // And the cron turn's LLM request offers no naming surface: the
    // retired tool is absent from its serialized tools list, and no naming
    // instruction rides any message (the reminder injector is gone).
    // (Call index 2: the user turn, its side title call, then the cron
    // turn.)
    let cron_params = &mock.captured_params()[2];
    assert!(
        cron_params
            .tools
            .iter()
            .all(|t| t.name != "SetSessionTitle"),
        "a cron turn must not offer the retired title tool: {:?}",
        cron_params
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );
    let whole = serde_json::to_string(&cron_params.messages).unwrap_or_default();
    assert!(
        !whole.contains("The session does not have a title yet"),
        "a cron turn must carry no naming instruction: {whole}"
    );
}

/// Given a server with NO LLM factory, when the first `message.send` fails
/// with the no-provider error, then the dispatch-time rule-based title the
/// no-provider branch derived is persisted — the on-disk `state.json` and
/// the session index row both carry the derived value, not the placeholder
/// (I-1 — no turn ever settles on this path, so the branch must write
/// through itself).
#[tokio::test]
async fn should_persist_the_derived_title_when_no_llm_provider_is_configured() {
    // Given a home-rooted server with no LLM factory and an untitled session
    let (server, tmp) = test_server_with_home_and_llm(None);
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the first send fails (no provider → no turn)
    let error = server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "fix the login bug" }),
        )
        .await
        .expect_err("the no-provider send must fail");
    assert!(
        error.to_string().contains("no LLM provider configured"),
        "got: {error}"
    );

    // Then the in-memory session carries the derived title
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "fix the login bug");
    assert!(!metadata.is_custom_title);

    // And the on-disk state.json carries it (I-1: the branch's own write)
    let state_path = session_dir_under(&tmp, &session_id).join("state.json");
    let raw = std::fs::read_to_string(&state_path).expect("state.json was written on disk");
    let state: serde_json::Value =
        serde_json::from_str(&raw).expect("state.json must be valid JSON");
    assert_eq!(
        state.get("title").and_then(Value::as_str),
        Some("fix the login bug"),
        "state.json must carry the derived title, not the placeholder: {raw}"
    );

    // And the session index row carries it too (the listing must not show
    // the placeholder for a conversed-but-providerless session)
    let row = server
        .session_index()
        .expect("the home-rooted server opens the index")
        .get_session(&session_id)
        .expect("the send path upserted the index row");
    assert_eq!(row.title.as_deref(), Some("fix the login bug"));
}

/// Given a session the user renamed (`is_custom_title = true`), when the
/// next send's turn settles and the side title call returns a title
/// anyway, then the naming guard refuses the write — the user's title
/// stays in place (A2).
#[tokio::test]
async fn should_never_overwrite_a_user_renamed_title() {
    // Given a renamed session whose scripted LLM serves the main turn and
    // then a side title the guard must refuse
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("The model's worse idea");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "session.rename",
            json!({ "session_id": &session_id, "title": "My debugging session" }),
        )
        .await
        .unwrap();

    // When the first prompt is sent (wait mode) and the turn completes
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please restructure the build scripts",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // Then the user's title survived the side call's attempt (A2) — the
    // naming guard refuses a custom-titled session before the side call
    // ever consults the LLM, so no second response is consumed
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "My debugging session");
    assert!(metadata.is_custom_title);

    // And the request carried no naming instruction (naming is out-of-band —
    // no reminder rides any turn)
    let first_request = mock.messages_of_call(0).to_string();
    assert!(
        !first_request.contains("The session does not have a title yet"),
        "a custom-titled session's turn must carry no naming instruction: {first_request}"
    );

    // And the side call never consulted the LLM: exactly the main turn's
    // one chat call ran (the guard refuses before any provider round trip)
    assert_eq!(
        mock.chat_call_count(),
        1,
        "a custom-titled session's side call must consume no scripted response"
    );
}

/// Given a scripted LLM whose side title call returns a title containing a
/// credential, when the first user turn runs, then the stored title is
/// the sanitized value (A5 — the same single redaction implementation the
/// fallback uses), and the side call leaves no record on the session's
/// wire log (nothing about it enters the conversation).
#[tokio::test]
async fn should_redact_credentials_in_the_side_call_title() {
    // Given a server whose scripted LLM answers the main turn and serves a
    // secret-bearing side title
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("deploy with password: hunter2");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the first prompt is sent (wait mode)
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "deploy the staging stack",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // Then the stored title is the redacted value (A5)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_title(&session, "deploy with password=[redacted]").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "deploy with password=[redacted]");
    assert!(!metadata.is_custom_title);

    // And the wire log carries no trace of the side call: no `SetSessionTitle`
    // tool call ran, and the raw secret never entered any recorded op (the
    // side call records nothing — the sanitized storage is the only write)
    let ops = read_wire_ops(&server, &session_id).await;
    let whole = serde_json::to_string(&ops).unwrap_or_default();
    assert!(
        !whole.contains("hunter2"),
        "the raw secret must never enter the wire log: {whole}"
    );
    assert!(
        !ops.iter().any(|op| matches!(
            op,
            WireOp::ContextAppendLoopEvent { event }
                if event.get("name").and_then(Value::as_str) == Some("SetSessionTitle")
        )),
        "the side title call must leave no tool-call record on the wire log"
    );
}

/// Given an untitled session with NO turn in flight, when `message.steer`
/// launches a turn of its own (the idle path) and the scripted LLM serves
/// the side title call, then the steer turn settles the naming question
/// the same way a first `message.send` does (A11): the side call's title
/// wins at settle.
#[tokio::test]
async fn should_treat_idle_steer_as_first_user_turn() {
    // Given a server whose scripted LLM answers the steer-launched turn
    // and then serves its side title call
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Steer-launched title");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // When a steer arrives on the idle agent (no turn in flight)
    server
        .call_method(
            "message.steer",
            json!({
                "session_id": &session_id,
                "text": "please reorganize the import graph",
            }),
        )
        .await
        .unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;

    // Then the side call's title won (A11: the steer-launched turn is the
    // session's first user turn for the naming question)
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_title(&session, "Steer-launched title").await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, "Steer-launched title");
    assert!(!metadata.is_custom_title);
}

/// Given an untitled session with no turn in flight, when an idle steer
/// launches a turn whose side title call fails both in turn and at the
/// settle retry, then the steer turn's settle runs the rule-based
/// fallback from the steer content's text (A11 — the failure arm of the
/// steer-as-first-turn semantics).
#[tokio::test]
async fn should_fall_back_when_an_idle_steer_skips_the_tool() {
    // Given a server whose scripted LLM answers the steer-launched turn
    // and fails its side title call twice (in turn, then at settle retry)
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_error("side call exploded");
    mock.push_error("side call retry exploded");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    // When a steer arrives on the idle agent and its turn completes
    let steer_text = "untangle the module graph please";
    server
        .call_method(
            "message.steer",
            json!({ "session_id": &session_id, "text": steer_text }),
        )
        .await
        .unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;

    // Then the fallback named the session from the steer content's text
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let expected = mirri_core::session::title::title_from_prompt_text(steer_text).unwrap();
    wait_for_title(&session, &expected).await;
    let metadata = session.metadata().await;
    assert_eq!(metadata.title, expected);
    assert!(!metadata.is_custom_title);
}

/// Given an untitled session whose first (fire-mode) user turn is
/// hard-aborted mid-LLM-call (its settle sites never run), then the abort
/// itself closes the naming question (the fallback runs at cancel time,
/// lock-free, from the dispatched `last_prompt`) and the next cron turn
/// carries no naming surface at all — the tool is retired and the
/// reminder injector is gone, so there is no residue to leak — and the
/// title is not polluted by the cron context (F-1, red line 3).
#[tokio::test]
async fn should_close_the_abort_residue_before_the_next_cron_turn() {
    // Given a server whose scripted LLM parks the first (fire-mode) reply
    // at a gate and answers the later cron turn with plain text
    let mock = Arc::new(ScriptedLlm::new());
    let release = mock.push_gated("a reply that arrives after the abort");
    mock.push_text("cron reply");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");

    // And a fire-mode first send that parks inside the LLM call
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "fix the login bug in auth",
                "mode": "fire",
            }),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.chat_call_count() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn must reach the LLM call");

    // When the turn is aborted (the fire task dies hard — its settle sites
    // never run) and the zombie turn unwinds
    server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    // Then the aborting side already settled the naming question: the
    // fallback ran at cancel time from the dispatched last_prompt (A10 —
    // a cancelled first user turn still names the session)
    let metadata = session.metadata().await;
    assert_eq!(
        metadata.title, "fix the login bug in auth",
        "the aborted fire turn's fallback must run at cancel time (F-1b)"
    );
    assert!(!metadata.is_custom_title);
    release.send(()).unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;

    // And when a cron turn of the same agent runs on the reused turn flow
    let tool = run_main_agent_tool(
        &server,
        &session_id,
        "CronCreate",
        json!({ "cron": "*/5 * * * *", "prompt": "tick", "recurring": true }),
    )
    .await;
    assert!(!tool.is_error, "{tool:?}");
    let task_id = cron_task_id_from_tool_result(&tool);
    server
        .call_method(
            "cron.trigger",
            json!({ "session_id": &session_id, "task_id": task_id }),
        )
        .await
        .unwrap();
    next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;

    // Then the cron request carries no naming surface at all: no naming
    // instruction rides any message (the reminder injector is retired), and
    // the retired tool is absent from the request's tools list — there is
    // no residual gear for the abort to leak (F-1a)
    let cron_params = &mock.captured_params()[1];
    assert!(
        cron_params
            .tools
            .iter()
            .all(|t| t.name != "SetSessionTitle"),
        "a driver turn must not offer the retired title tool: {:?}",
        cron_params
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );
    let whole = serde_json::to_string(&cron_params.messages).unwrap_or_default();
    assert!(
        !whole.contains("The session does not have a title yet"),
        "no naming instruction may ride the cron request: {whole}"
    );

    // And the title was not polluted by the cron context (red line 3)
    let metadata = session.metadata().await;
    assert_eq!(
        metadata.title, "fix the login bug in auth",
        "the cron turn must not overwrite the settled title (red line 3)"
    );
}

/// Given a conversed session (settled, auto-titled), when the user renames
/// it via `session.updateMeta`, then `state.json` on disk carries the new
/// custom title immediately — no turn has to settle afterwards, so a
/// process death right after the rename cannot lose it.
#[tokio::test]
async fn should_write_a_user_rename_to_state_json_immediately() {
    // Given a server whose scripted LLM completes a first turn (the session
    // materializes and settles with an auto title)
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    let (server, tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id,
                "text": "please help me restructure the authentication code",
                "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // When the user renames the session (no turn follows)
    server
        .call_method(
            "session.updateMeta",
            json!({ "session_id": &session_id, "title": "User's own title" }),
        )
        .await
        .unwrap();

    // Then the on-disk state.json carries the custom title and flag right
    // away — the write-through fired on the change itself
    let state_path = session_dir_under(&tmp, &session_id).join("state.json");
    let raw = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(&state_path)
                && raw.contains("User's own title")
            {
                return raw;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("state.json must carry the user rename without a settling turn");
    let state: serde_json::Value =
        serde_json::from_str(&raw).expect("state.json must be valid JSON");
    assert!(state["is_custom_title"].as_bool().unwrap_or(false));
}

/// Given a fresh untitled session, when the first turn runs, then the
/// serialized tool list offered to the model contains no
/// `SetSessionTitle` entry (the mechanism is retired; naming is
/// out-of-band).
#[tokio::test]
async fn should_not_offer_a_session_title_tool_to_the_model() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id, "text": "hello", "mode": "wait",
            }),
        )
        .await
        .unwrap();

    let params = mock.captured_params();
    let first = params.first().expect("main loop call captured");
    assert!(
        first.tools.iter().all(|t| t.name != "SetSessionTitle"),
        "the retired tool must not appear in the offered tool list: {:?}",
        first
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );
}

/// Given a completed first turn, when the side title call fires, then its
/// request prefix matches the main loop's request byte-for-byte up to the
/// appended instruction: the same tool list in the same order, and a
/// message list that extends (never rewrites) the main call's messages —
/// the provider prefix-cache hit precondition (rust AGENTS.md gate 5).
#[tokio::test]
async fn should_ride_the_main_loop_request_prefix_for_cache_hits() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_text("Fix login timeout");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id, "text": "my login always times out", "mode": "wait",
            }),
        )
        .await
        .unwrap();
    // The meta event fires only after the side call's title landed, so both
    // chat calls (main, then side) are captured by then — the capture
    // appends in call order, and the await chain guarantees the main call
    // precedes the settle-spawned side call.
    next_event_where(
        &mut events,
        |n| n.method == "event.session.meta.updated",
        |_| {},
    )
    .await;

    let params = mock.captured_params();
    assert_eq!(params.len(), 2, "main call + side call");
    let (main_call, side_call) = (&params[0], &params[1]);
    assert!(
        !main_call.tools.is_empty(),
        "the prefix gate is vacuous when the main call offers no tools"
    );
    // Tools identical, same order (`ToolDefinition` carries no `PartialEq`,
    // so the wire-relevant fields are compared field-by-field).
    assert_eq!(
        main_call.tools.len(),
        side_call.tools.len(),
        "main: {:?}; side: {:?}",
        main_call
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        side_call
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );
    for (a, b) in main_call.tools.iter().zip(side_call.tools.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.description, b.description);
        assert_eq!(a.parameters, b.parameters);
        assert_eq!(a.deferred, b.deferred);
    }
    // The side call extends the main call's message prefix — byte-for-byte
    // (`Message` is `PartialEq`), never a rewrite of it.
    assert!(side_call.messages.len() > main_call.messages.len());
    for (a, b) in main_call.messages.iter().zip(side_call.messages.iter()) {
        assert_eq!(a, b, "the side call must extend, not rewrite, the prefix");
    }
    // The only tail additions are the settled turn's assistant reply
    // (committed to memory before the side call reads it) and the
    // instruction message; nothing before them was rewritten. The prefix
    // identity above is the cache contract — the tail bound is the
    // "nothing else snuck in" guard.
    let tail = &side_call.messages[main_call.messages.len()..];
    assert!(tail.len() <= 2, "unexpected tail: {tail:?}");
    assert_eq!(
        tail.last().unwrap().text_content(),
        mirri_core::session::title_llm::TITLE_GENERATION_INSTRUCTION
    );
}

/// Given a legacy wire log whose recorded turn contains a `SetSessionTitle`
/// tool_use/tool_result pair (written by the retired mechanism), when the
/// session is restored and a new turn runs, then the replay tolerates the
/// retired tool: the pair is restored as plain message content (never
/// executed, no "unknown tool" error) and the new turn settles normally.
#[tokio::test]
async fn should_restore_a_legacy_session_that_recorded_title_tool_calls() {
    // Given a server whose first turn settles ("Done."), whose side title
    // call then fails both in turn and at the settle retry (scripted
    // provider errors, so the fallback names the session — and the title
    // blocks any further side call), and whose wire log carries a legacy
    // SetSessionTitle tool_use + tool_result pair
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("Done.");
    mock.push_error("side call failed");
    mock.push_error("side call retry failed");
    let (server, _tmp) = test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id, "text": "my login always times out", "mode": "wait",
            }),
        )
        .await
        .unwrap();
    // Wait out both side title attempts (their fallback title lands) so no
    // async writer can race the close below.
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    wait_for_chat_calls(&mock, 3).await;
    let fallback_title =
        mirri_core::session::title::title_from_prompt_text("my login always times out").unwrap();
    wait_for_title(&session, &fallback_title).await;

    // The legacy pair, recorded the way the retired mechanism wrote it: a
    // durable `context.append_message` op per message (the resume's replay
    // source). Recording on the live session writes the wire log only —
    // the resume below is what folds it back into memory.
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "assistant",
                "content": [],
                "tool_calls": [{
                    "id": "call-legacy-title",
                    "type": "function",
                    "function": {
                        "name": "SetSessionTitle",
                        "arguments": "{\"title\":\"Legacy recorded title\"}",
                    },
                }],
            }),
        })
        .await;
    session
        .record_op(WireOp::ContextAppendMessage {
            message: json!({
                "role": "tool",
                "tool_call_id": "call-legacy-title",
                "content": [{ "type": "text", "text": "Title set." }],
            }),
        })
        .await;

    // When the session is closed and resumed (the wire log replays into a
    // fresh memory) and a new turn runs on the restored history
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    server
        .call_method("session.resume", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    mock.push_text("resumed ack");
    let result = server
        .call_method(
            "message.send",
            json!({
                "session_id": &session_id, "text": "next question please", "mode": "wait",
            }),
        )
        .await
        .unwrap();

    // Then the new turn settled normally — no "unknown tool" error, no
    // execution of the retired tool (exactly one new chat call: main,
    // the two failed side call attempts, then this turn).
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(mock.chat_call_count(), 4);

    // And the restored history carries the legacy pair as plain message
    // content: the assistant tool_use and its tool result survived the
    // replay verbatim.
    let history = main_agent_history(&server, &session_id).await;
    let tool_use = history
        .iter()
        .find(|m| {
            m.message
                .tool_calls
                .iter()
                .any(|call| call.function.name == "SetSessionTitle")
        })
        .expect("the legacy SetSessionTitle tool_use must survive the resume");
    assert_eq!(tool_use.message.role, mirri_core::Role::Assistant);
    let call = &tool_use.message.tool_calls[0];
    assert_eq!(call.id, "call-legacy-title");
    assert_eq!(
        call.function.arguments,
        "{\"title\":\"Legacy recorded title\"}"
    );
    let tool_result = history
        .iter()
        .find(|m| m.message.tool_call_id.as_deref() == Some("call-legacy-title"))
        .expect("the legacy tool result must survive the resume");
    assert_eq!(tool_result.message.role, mirri_core::Role::Tool);
    assert_eq!(tool_result.message.text_content(), "Title set.");

    // And the new turn's request replayed the pair as plain history — the
    // model saw the retired tool call in its context, and nothing in the
    // turn treated it as executable.
    let request = mock.messages_of_call(3).to_string();
    assert!(
        request.contains("SetSessionTitle"),
        "the restored pair must ride the request as history: {request}"
    );
    assert!(
        !request.contains("unknown tool") && !request.contains("Unknown tool"),
        "no unknown-tool error may ride the restored turn: {request}"
    );
}
