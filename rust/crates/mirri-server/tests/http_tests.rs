//! Integration tests for the HTTP transport layer (Phase 4).
//!
//! Tests boot a real server on an ephemeral port and exercise it with
//! `reqwest`, asserting the `{code, msg, data, request_id}` envelope
//! (`code: 0` = success). Naming follows the Given-When-Then convention.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestServer;
use mirri_core::{AgentId, AgentScopeConfig, PermissionMode, Role, SessionId};
use mirri_test_harness::{MockLlm, MockLlmFactory};
use serde_json::{Value, json};

/// HTTP client with a generous timeout so slow CI machines do not flake.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build http client")
}

async fn get(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    let response = client.get(url).send().await.expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

async fn post(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("POST request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

async fn delete(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    let response = client.delete(url).send().await.expect("DELETE request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

/// Create a session through the HTTP API and return its id.
async fn create_session(server: &TestServer, client: &reqwest::Client, title: &str) -> String {
    let (status, body) = post(
        client,
        &format!("{}/api/v1/sessions", server.base_url()),
        json!({ "title": title }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "session create succeeds");
    body["data"]["id"].as_str().expect("session id").to_string()
}

/// Create a session through the HTTP API and add one fresh `agent-test`
/// agent directly through the session manager (there is no RPC endpoint for
/// agent creation yet). Returns the session id.
async fn create_session_with_agent(server: &TestServer, client: &reqwest::Client) -> String {
    create_session_with_agent_mode(server, client, PermissionMode::default()).await
}

/// Create a session through the HTTP API and add one fresh `agent-test`
/// agent with the given permission mode directly through the session manager.
async fn create_session_with_agent_mode(
    server: &TestServer,
    client: &reqwest::Client,
    permission_mode: PermissionMode,
) -> String {
    let session_id = create_session(server, client, "Engine wiring").await;
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("created session is live");
    session
        .create_agent(
            AgentId::new("agent-test"),
            AgentScopeConfig {
                permission_mode,
                ..AgentScopeConfig::default()
            },
        )
        .await
        .expect("create agent");
    session_id
}

// ===========================================================================
// Infra routes
// ===========================================================================

#[tokio::test]
async fn should_return_ok_and_ready_when_getting_healthz() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting the health endpoint
    let (status, body) = get(&client, &format!("{}/api/v1/healthz", server.base_url())).await;

    // Then it reports healthy and ready in the standard envelope
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["msg"], "success");
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["ready"], true);
}

#[tokio::test]
async fn should_return_version_and_started_at_when_getting_meta() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting the metadata endpoint
    let (status, body) = get(&client, &format!("{}/api/v1/meta", server.base_url())).await;

    // Then it reports the server version, id, and start time in the v1 wire
    // contract's snake_case shape (the protocol schema and the live e2e
    // suites pin these spellings)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["server_version"], "0.1.0-test");
    assert!(
        body["data"]["server_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "server_id is a non-empty string"
    );
    let started_at = body["data"]["started_at"]
        .as_str()
        .expect("started_at is a string");
    chrono::DateTime::parse_from_rfc3339(started_at).expect("started_at is RFC3339");
    assert!(
        body["data"]["open_in_apps"].as_array().is_some(),
        "open_in_apps is always present (may be empty)"
    );
    assert!(
        body["data"]["dangerous_bypass_auth"].as_bool().is_some(),
        "dangerous_bypass_auth is always present"
    );
}

#[tokio::test]
async fn should_echo_envelope_when_calling_api() {
    // Given a running server and a client that sends x-request-id
    let server = TestServer::start().await;
    let client = client();

    // When calling an endpoint with a request id header
    let response = client
        .get(format!("{}/api/v1/healthz", server.base_url()))
        .header("x-request-id", "req-123")
        .send()
        .await
        .expect("GET request");
    let body: Value = response.json().await.expect("JSON body");

    // Then the envelope echoes the request id back
    assert_eq!(body["code"], 0);
    assert_eq!(body["request_id"], "req-123");
}

// ===========================================================================
// Authentication
// ===========================================================================

/// Shared secret enforced by auth-enabled test servers.
const TEST_AUTH_TOKEN: &str = "test-secret-token";

#[tokio::test]
async fn should_return_401_when_requesting_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route without an Authorization header
    let (status, body) = get(&client, &format!("{}/api/v1/config", server.base_url())).await;

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
    assert_eq!(body["data"], Value::Null);
    assert!(
        body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "401 envelope carries a request id"
    );
}

#[tokio::test]
async fn should_return_401_when_requesting_with_wrong_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route with a token that does not match
    let response = client
        .get(format!("{}/api/v1/config", server.base_url()))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
}

#[tokio::test]
async fn should_return_200_when_requesting_with_valid_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route with the correct token
    let response = client
        .get(format!("{}/api/v1/config", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the request is served normally
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
}

#[tokio::test]
async fn should_return_200_when_requesting_healthz_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the public health endpoint without a token
    let (status, body) = get(&client, &format!("{}/api/v1/healthz", server.base_url())).await;

    // Then the probe stays reachable — it is exempt from authentication
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["ready"], true);
}

#[tokio::test]
async fn should_return_401_when_requesting_auth_summary_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the auth summary without a credential — the exact
    // request the web UI makes on first paint
    let (status, body) = get(&client, &format!("{}/api/v1/auth", server.base_url())).await;

    // Then it is rejected like every protected route: the 401 is the signal
    // the web client uses to show its server-token dialog
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
}

#[tokio::test]
async fn should_report_ready_when_a_default_model_and_custom_provider_are_configured() {
    // Given an auth-enabled server with a custom provider and a default
    // model alias
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let config = mirri_core::ServerConfig {
        providers: Some(std::collections::HashMap::from([(
            "custom".to_string(),
            mirri_core::ProviderEntry {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        )])),
        default_model: Some("ultra".to_string()),
        ..Default::default()
    };
    *server
        .state
        .config
        .write()
        .unwrap_or_else(|e| e.into_inner()) = config;
    let client = client();

    // When requesting the auth summary with a valid credential
    let response = client
        .get(format!("{}/api/v1/auth", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the v1 AuthSummary shape reports a ready setup
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ready"], true);
    // The configured custom provider.
    assert_eq!(body["data"]["providers_count"], 1);
    assert_eq!(body["data"]["default_model"], "ultra");
}

#[tokio::test]
async fn should_report_not_ready_when_no_default_model_is_configured() {
    // Given an auth-enabled server with no providers and no default model
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the auth summary with a valid credential
    let response = client
        .get(format!("{}/api/v1/auth", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the summary reports the not-ready setup
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ready"], false);
    assert_eq!(body["data"]["providers_count"], 0);
    assert_eq!(body["data"]["default_model"], Value::Null);
}

#[tokio::test]
async fn should_return_200_when_requesting_protected_route_without_token_on_open_server() {
    // Given a server in open (no-token) mode
    let server = TestServer::start().await;
    let client = client();

    // When requesting a protected route without a token
    let (status, body) = get(&client, &format!("{}/api/v1/config", server.base_url())).await;

    // Then the request is served normally — open mode allows everyone
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
}

#[tokio::test]
async fn should_return_200_when_shutting_down_without_token_on_open_server() {
    // Given a server in open (no-token) mode listening on loopback
    let server = TestServer::start().await;
    let client = client();

    // When requesting a graceful shutdown from the loopback peer
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/shutdown", server.base_url()),
        json!({}),
    )
    .await;

    // Then the shutdown is acknowledged — loopback origins are trusted
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert!(
        *server.state.shutdown_tx.borrow(),
        "shutdown flag is raised on the server state"
    );
}

// ===========================================================================
// Session lifecycle routes
// ===========================================================================

#[tokio::test]
async fn should_return_session_id_when_creating_session() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When creating a session with a title and tags
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/sessions", server.base_url()),
        json!({ "title": "E2E Session", "tags": ["transport"] }),
    )
    .await;

    // Then the envelope carries the new session's wire view (session.create
    // eagerly provisions the "main" agent, mirroring the embedded engine)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["title"], "E2E Session");
    assert_eq!(body["data"]["agentCount"], 1);
    let session_id = body["data"]["id"].as_str().expect("session id");
    assert!(
        session_id.starts_with("session-"),
        "session id has the scope prefix: {session_id}"
    );
    assert!(
        body["data"]["tags"]
            .as_array()
            .is_some_and(|tags| tags.len() == 1)
    );
}

#[tokio::test]
async fn should_return_session_when_listing_sessions() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "List me").await;
    let base = server.base_url();

    // When listing sessions
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions")).await;

    // Then the created session appears exactly once with its summary (the
    // v2 page envelope — `data: {items, has_more}` — matching the TS route)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["has_more"], false);
    let sessions = body["data"]["items"].as_array().expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], session_id);
    assert_eq!(sessions[0]["title"], "List me");
}

#[tokio::test]
async fn should_return_details_when_getting_session() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Details please").await;
    let base = server.base_url();

    // When fetching the session by id
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;

    // Then the envelope carries the session details with the eagerly
    // provisioned main agent as its only agent
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["id"], session_id);
    assert_eq!(body["data"]["title"], "Details please");
    assert_eq!(body["data"]["agentCount"], 1);
    let agents = body["data"]["agents"].as_array().expect("agent list");
    assert_eq!(agents, &vec![json!({ "id": "main" })]);
}

#[tokio::test]
async fn should_remove_session_when_closing_session() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Close me").await;
    let base = server.base_url();

    // When closing the session through the HTTP API
    let (status, body) = delete(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;

    // Then the close reports success
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);

    // And the session is gone: fetching it returns 200 with a NotFound code
    // (TS server convention: HTTP status is always 200, error in envelope)
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001);
}

#[tokio::test]
async fn should_return_404_when_getting_missing_session() {
    // Given a running server without sessions
    let server = TestServer::start().await;
    let client = client();

    // When fetching a session id that was never created
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/session-00000000-0000-0000-0000-000000000000",
            server.base_url()
        ),
    )
    .await;

    // Then it returns 200 with a NotFound error code in the envelope
    // (TS server convention: HTTP status is always 200, error in envelope)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001);
}

// ===========================================================================
// Tool & route fallback
// ===========================================================================

#[tokio::test]
async fn should_return_the_main_agents_builtin_tools_when_listing_tools() {
    // Given a running server with one created session (session.create
    // eagerly provisions the "main" agent with its builtin tools enabled)
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Toolbox").await;
    let base = server.base_url();

    // When listing the session's tools
    let (status, body) = get(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/tools"),
    )
    .await;

    // Then the envelope carries the main agent's builtin tool list (no user
    // or MCP tools are registered)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let tools = body["data"].as_array().expect("tool list");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert!(
        names.contains(&"Read") && names.contains(&"Bash") && names.contains(&"Grep"),
        "the main agent's builtin tools are listed: {names:?}"
    );
    assert!(
        !names.contains(&"Agent") && !names.contains(&"AgentSwarm"),
        "no subagent host is wired on the server side: {names:?}"
    );
}

#[tokio::test]
async fn should_return_404_when_requesting_unknown_route() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting a route that is not registered
    let response = client
        .get(format!(
            "{}/api/v1/definitely/not/a/route",
            server.base_url()
        ))
        .send()
        .await
        .expect("GET request");

    // Then it returns 404
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn should_respond_ok_when_posting_shutdown() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting a graceful shutdown
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/shutdown", server.base_url()),
        json!({}),
    )
    .await;

    // Then the request is acknowledged and the shutdown flag is raised
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert!(
        *server.state.shutdown_tx.borrow(),
        "shutdown flag is raised on the server state"
    );
}

// ===========================================================================
// Engine wiring: message.send
// ===========================================================================

#[tokio::test]
async fn should_return_internal_error_when_sending_message_without_llm_provider() {
    // Given a session with an agent on a server without an LLM provider
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session_with_agent(&server, &client).await;
    let base = server.base_url();

    // When sending a message (addressing agent-test explicitly — the session
    // now also holds the eagerly provisioned main agent, and the first-agent
    // fallback order is unspecified)
    let (status, body) = post(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/messages"),
        json!({ "text": "Hello without an LLM", "agentId": "agent-test" }),
    )
    .await;

    // Then the send fails with a clear internal error
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32603);
    assert!(
        body["msg"]
            .as_str()
            .is_some_and(|msg| { msg.contains("no LLM provider configured for this agent") }),
        "envelope explains the missing provider: {}",
        body["msg"]
    );

    // And — the engine-backed wire semantics (message.send is an engine
    // method now) — the failed send leaves the session state untouched: the
    // provider check runs before any memory mutation, so history stays
    // consistent with the (empty) wire log.
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&AgentId::new("agent-test"))
        .await
        .expect("agent-test exists");
    let messages = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages();
    assert!(
        messages.is_empty(),
        "a no-provider send records nothing: {}",
        messages.len()
    );
}

#[tokio::test]
async fn should_run_turn_and_return_turn_result_when_sending_message_with_scripted_llm() {
    // Given a session with an agent on a server whose engine runs a shared
    // mock LLM
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello from the mock LLM");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let client = client();
    let session_id = create_session_with_agent(&server, &client).await;
    let base = server.base_url();

    // When sending a message (addressing agent-test explicitly — the session
    // now also holds the eagerly provisioned main agent, and the first-agent
    // fallback order is unspecified)
    let (status, body) = post(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/messages"),
        json!({ "text": "Hello engine", "agentId": "agent-test" }),
    )
    .await;

    // Then the envelope carries the successful turn end result
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["agent_id"], "agent-test");
    assert_eq!(body["data"]["turn"]["turnId"], 1);
    assert_eq!(body["data"]["turn"]["stopReason"], "end_turn");
    assert_eq!(body["data"]["turn"]["steps"], 1);
    assert_eq!(body["data"]["turn"]["blockedByPromptHook"], false);

    // And memory holds the user message plus the assistant reply. The step
    // loop may inject user-role system reminders (e.g. the tools_added
    // announcement) between them, so the reply is asserted as the LAST
    // message rather than messages[1].
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&AgentId::new("agent-test"))
        .await
        .expect("agent-test exists");
    let messages = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages();
    assert!(
        messages.len() >= 2,
        "user prompt and assistant reply are both recorded"
    );
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].text_content(), "Hello engine");
    let reply = messages.last().expect("assistant reply recorded");
    assert_eq!(reply.role, Role::Assistant);
    assert_eq!(reply.text_content(), "Hello from the mock LLM");
}

// ===========================================================================
// Terminal routes (web terminal panel)
// ===========================================================================

/// Given a session, when a terminal is created through the REST API, then
/// the wire object follows the protocol's terminal schema and the terminal
/// is listed and fetchable.
#[tokio::test]
async fn should_create_list_and_get_a_terminal_when_the_rest_api_is_used() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminals").await;

    // Create answers the protocol's `Terminal` wire shape (snake_case).
    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({ "cols": 120, "rows": 30 }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "terminal create succeeds");
    assert_eq!(body["code"], 0, "terminal create envelope: {body}");
    let terminal = &body["data"];
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();
    assert!(terminal_id.starts_with("term_"), "id: {terminal_id}");
    assert_eq!(terminal["session_id"], session_id);
    assert_eq!(terminal["cols"], 120);
    assert_eq!(terminal["rows"], 30);
    assert_eq!(terminal["status"], "running");
    assert!(
        terminal["cwd"]
            .as_str()
            .is_some_and(|cwd| !cwd.starts_with('/')),
        "cwd stays relative to the workspace: {}",
        terminal["cwd"]
    );
    assert!(
        terminal["shell"]
            .as_str()
            .is_some_and(|shell| !shell.is_empty()),
        "shell resolves to the detected default"
    );
    assert!(
        terminal["created_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "created_at is present"
    );
    assert!(
        terminal.get("exited_at").is_none(),
        "a running terminal carries no exited_at"
    );

    // The terminal appears in the list…
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let items = body["data"]["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|item| item["id"] == terminal_id),
        "the created terminal is listed: {items:?}"
    );

    // …and is fetchable on its own.
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["data"]["id"], terminal_id);
    assert_eq!(body["data"]["status"], "running");
}

/// Given a created terminal, when it is closed through the REST API, then
/// the close acknowledges and the terminal reports exited with an exit code.
#[tokio::test]
async fn should_close_a_terminal_and_report_it_as_exited() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminal close").await;
    let (_, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({}),
    )
    .await;
    let terminal_id = body["data"]["id"]
        .as_str()
        .expect("terminal id")
        .to_string();

    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}/close",
            server.base_url()
        ),
        json!({}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "terminal close succeeds");
    assert_eq!(body["data"], json!({ "closed": true }));

    // The exited spelling carries status/exit_code/exited_at. The pool
    // observes the child's exit asynchronously after the kill, so the
    // status is polled for briefly instead of asserted immediately (the
    // close→read race otherwise flakes under a fully parallel workspace
    // run).
    let mut body;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, fetched) = get(
            &client,
            &format!(
                "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}",
                server.base_url()
            ),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        body = fetched;
        if body["data"]["status"] == "exited" || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(body["data"]["status"], "exited");
    assert!(
        body["data"].get("exit_code").is_some(),
        "exit_code rides the exited spelling"
    );
    assert!(
        body["data"]["exited_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "exited_at is present after the close"
    );
}

/// Given a terminal create body with an absolute cwd, when the terminal is
/// created, then the request fails the schema's relative-path rule instead
/// of spawning outside the session workspace.
#[tokio::test]
async fn should_reject_an_absolute_cwd_when_creating_a_terminal() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminal validation").await;

    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({ "cwd": "/etc" }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "errors answer in the envelope"
    );
    assert_eq!(body["code"], -32602, "invalid params: {body}");
}

/// Given an unknown session or terminal, when the terminal routes are
/// called, then the envelope answers the not-found application code.
#[tokio::test]
async fn should_answer_not_found_for_unknown_session_or_terminal() {
    let server = TestServer::start().await;
    let client = client();

    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/session-nope/terminals",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001, "unknown session: {body}");

    let session_id = create_session(&server, &client, "Terminal unknown").await;
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/term-nope",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001, "unknown terminal: {body}");
}

// ===========================================================================
// Agent profiles REST surface (P6 — the web `/agents` contract)
// ===========================================================================

/// `PUT` helper (the profile update route is the first PUT on the wire).
async fn put(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client
        .put(url)
        .json(&body)
        .send()
        .await
        .expect("PUT request");
    let status = response.status();
    let json_body = response.json().await.expect("JSON body");
    (status, json_body)
}

#[tokio::test]
async fn should_create_list_get_update_and_delete_profiles_when_the_rest_api_is_used() {
    // Given a server whose engine roots its Mirri home in a tempdir
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let base = format!("{}/api/v1/agents", server.base_url());

    // When a profile is created over the REST contract (the full TS
    // `createProfileRequestSchema` surface — T19 m5 cleared)
    let (status, body) = post(
        &client,
        &base,
        json!({
            "name": "reviewer",
            "description": "Reviews code.",
            "default_model": "claude-sonnet-4-20250514",
            "tools": ["Read", "Grep"],
            "disallowed_tools": ["Bash"],
            "subagents": ["coder", "explore"],
            "capabilities_required": ["code.read"],
            "system_prompt_template": "You review code.",
            "when_to_use": "When a change needs review.",
        }),
    )
    .await;

    // Then the created row answers with the WireAgentProfile shape and
    // the profile is persisted under <home>/agents/ (the unified .md
    // format the discovery pass reads — the R4-1 write channel)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0, "create succeeds: {body}");
    let data = &body["data"];
    assert_eq!(data["name"], "reviewer");
    assert_eq!(data["source"], "user");
    assert_eq!(data["builtin"], false);
    assert_eq!(data["essential"], false);
    assert_eq!(data["enabled"], true);
    assert_eq!(data["has_override"], false);
    assert_eq!(data["default_model"], "claude-sonnet-4-20250514");
    assert_eq!(data["tools"], json!(["Read", "Grep"]));
    assert_eq!(data["disallowed_tools"], json!(["Bash"]));
    assert_eq!(data["subagents"], json!(["coder", "explore"]));
    assert_eq!(data["capabilities_required"], json!(["code.read"]));
    assert_eq!(data["system_prompt_template"], "You review code.");
    assert!(
        home.path().join("agents").join("reviewer.md").is_file(),
        "the profile file must be persisted"
    );

    // And the list answers the {items, skipped} envelope with the built-ins
    // plus the new profile
    let (status, body) = get(&client, &base).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().expect("items array");
    let mut builtin_names: Vec<&str> = items
        .iter()
        .filter(|item| item["source"] == "builtin")
        .map(|item| item["name"].as_str().expect("a builtin row carries a name"))
        .collect();
    builtin_names.sort();
    assert_eq!(
        builtin_names,
        vec!["agent", "coder", "explore", "plan"],
        "the four built-in profiles are listed"
    );
    let reviewer = items
        .iter()
        .find(|item| item["name"] == "reviewer")
        .expect("the created profile is listed");
    assert!(reviewer["file_path"].as_str().is_some());
    assert_eq!(
        body["data"]["skipped"],
        json!([]),
        "a clean home skips nothing"
    );

    // And the single-profile read answers the same row
    let (status, body) = get(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["name"], "reviewer");
    assert_eq!(body["data"]["effective_system_prompt"], "You review code.");

    // And a partial update merges (the description moves, the new-field
    // surface keeps its values, the prompt stays)
    let (status, body) = put(
        &client,
        &format!("{base}/reviewer"),
        json!({ "description": "Reviews code carefully." }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["description"], "Reviews code carefully.");
    assert_eq!(body["data"]["disallowed_tools"], json!(["Bash"]));
    assert_eq!(
        body["data"]["system_prompt_template"], "You review code.",
        "an omitted field keeps its previous value"
    );

    // And the delete removes the row and the file
    let (status, body) = delete(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["deleted"], true);
    assert!(
        !home.path().join("agents").join("reviewer.md").exists(),
        "the profile file must be removed"
    );
    let (status, body) = get(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_ne!(body["code"], 0, "the deleted profile is not found anymore");
}

/// Given a home whose profiles root holds a malformed `.md` (a parse
/// failure) and a file declaring the essential name, When the agents list
/// is fetched over the REST contract, Then the essential file LOADS as the
/// content override (D16) while the `skipped` diagnostics carry only the
/// genuine parse failure (D13 — load failures only).
#[tokio::test]
async fn should_report_skipped_files_when_the_agents_list_is_fetched() {
    // Given a home whose profiles directory holds a broken .md and an
    // essential-name declaration
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let profiles_dir = home.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("profiles dir must be creatable");
    std::fs::write(
        profiles_dir.join("broken.md"),
        "---\nname: broken\n---\nNo description here.",
    )
    .expect("broken .md must be writable");
    std::fs::write(
        profiles_dir.join("agent.md"),
        "---\nname: agent\ndescription: Imposter.\n---\nYou are not the default.",
    )
    .expect("essential .md must be writable");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();

    // When the agents list is fetched
    let (status, body) = get(&client, &format!("{}/api/v1/agents", server.base_url())).await;

    // Then the essential row is the user override (the covered-builtin
    // wire shape) and only the broken file is skipped
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"]
        .as_array()
        .expect("items ride alongside skipped");
    let essential = items
        .iter()
        .find(|item| item["name"] == "agent")
        .expect("the essential override is listed");
    assert_eq!(essential["source"], "user");
    assert_eq!(essential["builtin"], false);
    assert_eq!(essential["has_override"], true);
    assert_eq!(essential["essential"], true);
    let skipped = body["data"]["skipped"]
        .as_array()
        .expect("skipped rides alongside items");
    assert_eq!(
        skipped.len(),
        1,
        "only the malformed file is skipped (D13): {skipped:?}"
    );
    let broken = skipped
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("broken.md"))
        })
        .expect("the malformed file is reported");
    assert!(
        broken["reason"].as_str().is_some_and(
            |reason| reason.contains("Missing required frontmatter field \"description\"")
        ),
        "the parse failure is reported readably: {broken:?}"
    );
}

#[tokio::test]
async fn should_keep_created_profiles_when_a_new_server_shares_the_home() {
    // Given a server with one REST-created profile
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/agents", server.base_url()),
        json!({ "name": "survivor", "description": "persists" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);

    // When a new server instance boots over the same home (the restart
    // persistence path)
    let restarted = TestServer::start_with_home_dir(home.path().to_path_buf()).await;

    // Then the profile is still listed
    let (status, body) = get(&client, &format!("{}/api/v1/agents", restarted.base_url())).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|item| item["name"] == "survivor"),
        "the profile survives the restart"
    );
}

#[tokio::test]
async fn should_reject_essential_and_builtin_profile_writes_over_the_rest_api() {
    // Given a home-rooted server
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let base = format!("{}/api/v1/agents", server.base_url());

    // When the essential default profile name is created over, Then the
    // envelope carries the TS-exact refusal (`agents.ts:700`; the
    // transport prefixes every invalid-params error with its RPC label)
    let (status, body) = post(&client, &base, json!({ "name": "agent" })).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_ne!(body["code"], 0, "the essential name is refused: {body}");
    assert_eq!(
        body["msg"], "invalid params: Cannot create a profile with the reserved name \"agent\"",
        "the TS-exact essential refusal (transport-prefixed): {body}"
    );

    // And a non-kebab name is refused with the TS-exact contract message
    // before any filesystem access (`agents.ts:149-166`)
    let (status, body) = post(
        &client,
        &base,
        json!({ "name": "../evil", "description": "x" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"],
        "invalid params: name must be kebab-case (lowercase alphanumeric segments separated by \
         single hyphens, e.g. \"code-reviewer\")",
        "the TS-exact kebab refusal: {body}"
    );

    // And a create without a description is refused (the invisible-profile
    // guard, `agents.ts:705-722`)
    let (status, body) = post(&client, &base, json!({ "name": "blank" })).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"], "invalid params: description is required and must be a non-empty string",
        "the TS-exact description guard: {body}"
    );

    // And deleting a built-in without an override is refused (the TS-exact
    // builtin refusal, `agents.ts:997`)
    let (status, body) = delete(&client, &format!("{base}/coder")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"], "invalid params: Cannot delete built-in profile \"coder\"",
        "the TS-exact builtin refusal: {body}"
    );

    // And an unknown profile read answers not-found with the TS-exact
    // message (`agents.ts:648`; the not-found transport label prefixes it)
    let (status, body) = get(&client, &format!("{base}/ghost")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        body["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("Agent profile \"ghost\" was not found")),
        "the TS-exact not-found message: {body}"
    );
}
