//! `mirri print` — non-interactive single-prompt mode (P1-11).
//!
//! Sends one prompt to the engine through the in-process
//! [`EmbeddedServer`](mirri_embedded::EmbeddedServer) in **wait mode**: the
//! session is created, the message is sent, and the CLI blocks until the turn
//! ends, streaming assistant text to stdout as it is produced.
//!
//! Stream contract:
//!
//! - `event.assistant.delta` params `delta` → stdout (flushed per chunk, so
//!   a long reply appears progressively),
//! - `event.approval.requested` / `event.question.requested` progress →
//!   stderr,
//! - any turn failure (`event.turn.ended` with `reason: "failed"`, or an
//!   error response from `message.send`) → error exit code 1.
//!
//! The LLM provider is resolved from the environment by
//! [`ProviderLlmFactory`]: `ANTHROPIC_API_KEY` (+ optional
//! `ANTHROPIC_BASE_URL`) for `anthropic`, `OPENAI_API_KEY` (+ optional
//! `OPENAI_BASE_URL`) for `openai`.
//!
//! Known limitation (basic print mode, intentionally): no approval
//! responder is installed, so a turn whose tool call needs approval under the
//! manual permission mode blocks on the approval gate until the engine turn
//! timeout (300s). Interactive approval flows are out of scope for print mode.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mirri_core::{
    PermissionMode, SessionManager, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig,
    WorkspaceTrustLevel,
};
use mirri_embedded::{EmbeddedServer, ProviderLlmFactory};
use mirri_transport::adapter::EngineAdapter;
use mirri_transport::frame::JsonRpcNotification;
use serde_json::Value;

/// The agent id every `session.create` provisions (the embedded engine's
/// default main agent).
const MAIN_AGENT_ID: &str = "main";

/// How long the event worker keeps draining after `message.send` resolves.
///
/// A wait-mode turn publishes every assistant `TextDelta` (as
/// `event.assistant.delta`) onto the session's event bus *before* the RPC
/// response returns, but the bus → subscriber
/// relay runs on its own task and may still be forwarding queued
/// notifications when the response arrives. All of the turn's events are
/// already queued by then, so a short quiet window after the response is
/// enough to observe the tail before exiting.
const EVENT_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Run `mirri print`: create a throwaway session rooted at the current
/// working directory, send the prompt in wait mode, and stream the reply.
///
/// Returns an error (exit code 1 from `main`) when the turn fails; the
/// session is in-memory only and is discarded when the process exits.
pub async fn run_print(
    prompt: String,
    model: Option<String>,
    provider: Option<String>,
) -> Result<()> {
    // Validate the provider value up front (matching the `--provider` help
    // text), so a typo surfaces before anything is created; the engine's
    // `ProviderLlmFactory` also rejects `custom` at turn time.
    if let Some(provider_type) = &provider
        && provider_type != "anthropic"
        && provider_type != "openai"
    {
        return Err(anyhow!(
            "unknown provider '{provider_type}'; expected 'anthropic' or 'openai'"
        ));
    }
    // The engine enforces the same invariant, but failing here yields a
    // CLI-flavoured error before any session is created.
    if provider.is_some() && model.is_none() {
        return Err(anyhow!("--model is required when --provider is specified"));
    }

    // 1. Engine scopes rooted at the current working directory (the engine
    //    core requires a concrete, existing directory). The CLI is run by the
    //    user directly on their own directory, so the workspace is trusted —
    //    the same choice the `serve` command makes.
    let workspace_root =
        std::env::current_dir().context("the current working directory must be readable")?;
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("embedded"),
        workspace_root,
        WorkspaceScopeConfig {
            trust_level: WorkspaceTrustLevel::Trusted,
            permission_mode: PermissionMode::Manual,
            hooks: Vec::new(),
            extra_directories: Vec::new(),
            load_project_agents_md: true,
        },
    ));
    // The session manager rooted at the engine's workspace-local storage
    // root — explicitly `.mirri/v3_sessions` (ADR-006-R2: the rust engine's
    // own root, physically separate from the TS engine's `.mirri/sessions`),
    // with the same `LocalFs` io `SessionManager::new` defaults to.
    let persistence_dir = workspace.cwd().join(".mirri").join("v3_sessions");
    // Print-mode config defaults (B1 D-59, `printDefaults.ts`): unset
    // subagent / bash timeouts become "no timeout" — a print run must not
    // kill long work the user asked for — while explicit env config
    // (`MIRRI_TASK_*` / `MIRRI_SUBAGENT_TIMEOUT_MS`) still wins. The
    // overrides are in-memory only: nothing is written to disk.
    let mut server_config = mirri_core::config::ServerConfig::default();
    let mut task = server_config.task.take().unwrap_or_default();
    task.bash_task_timeout_s = Some(0);
    server_config.task = Some(task);
    server_config.subagent = Some(mirri_core::config::SubagentConfig {
        timeout_ms: Some(0),
        // The nesting ceiling keeps its default (absent → 5): print mode
        // lifts the timeouts only (D-59), not the resource guard.
        max_nesting_depth: None,
        // Same for the swarm item cap (absent → 3, SDR-012).
        max_swarm_items: None,
    });
    server_config.apply_env()?;
    let server = EmbeddedServer::with_providers(
        Arc::new(SessionManager::with_persistence_dir(
            workspace,
            persistence_dir,
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        )),
        Some(Arc::new(ProviderLlmFactory::default())),
        None,
        None,
        None,
        std::sync::Arc::new(std::sync::RwLock::new(server_config)),
        None,
    );

    // 2. Subscribe before creating the session so no turn event is missed.
    let events = server.subscribe_events().await?;

    // 3. Create the session (provisions the default main agent).
    let mut create_params = serde_json::json!({ "title": "mirri print" });
    if let Some(model_id) = &model {
        create_params["model_id"] = serde_json::json!(model_id);
    }
    if let Some(provider_type) = &provider {
        create_params["provider_type"] = serde_json::json!(provider_type);
    }
    let created = server.call_method("session.create", create_params).await?;
    let session_id = created
        .get("id")
        .and_then(Value::as_str)
        .context("session.create response must carry the session id")?
        .to_string();

    // 4. Stream events on a worker task while the wait-mode turn runs. The
    //    worker stops on the drain signal sent after `message.send` resolves.
    let (drain_tx, drain_rx) = tokio::sync::mpsc::channel::<()>(1);
    let worker = tokio::spawn(stream_events(events, drain_rx, session_id.clone()));

    // 5. Send the prompt and block until the turn ends.
    let send_result = server
        .call_method(
            "message.send",
            serde_json::json!({
                "session_id": session_id,
                "agent_id": MAIN_AGENT_ID,
                "text": prompt,
                "mode": "wait",
            }),
        )
        .await;

    // 6. Stop streaming once the relay has forwarded everything it queued
    //    for this turn, then surface any failure the worker observed. A
    //    failed drain send just means the worker already exited on its own.
    let _ = drain_tx.send(()).await;
    worker.await.context("print event worker panicked")??;

    // 7. Turn outcome: wait mode reports turn failures as RPC errors.
    send_result.context("engine turn failed")?;

    // 8. Print-mode background strategy (B1 D-59): keep the process alive
    //    while background tasks run, per the configured strategy
    //    (`exit` finishes at once, `drain` waits them out, `steer` keeps
    //    deciding). Each `continue` waits for a settlement (the drain wait
    //    doubles as the throttle — no busy loop), and the server's turn /
    //    duration caps bound the loop. Soft-bound caveat: each wait
    //    re-arms a fresh full ceiling server-side, so the total lifetime
    //    can exceed `printWaitCeilingS` by up to one ceiling per cycle —
    //    the ceiling bounds the decision cadence, not the whole loop
    //    (TS parity is approximate the same way).
    loop {
        let completion = server
            .call_method(
                "session.handlePrintMainTurnCompleted",
                serde_json::json!({ "sessionId": session_id }),
            )
            .await
            .context("print completion decision failed")?;
        if completion.get("decision").and_then(Value::as_str) == Some("finish") {
            break;
        }
        server
            .call_method(
                "session.waitForBackgroundTasksOnPrint",
                serde_json::json!({ "sessionId": session_id }),
            )
            .await
            .context("print background wait failed")?;
    }
    Ok(())
}

/// Drain the session's event stream until the drain signal arrives and the
/// relay has emptied its queue.
///
/// While the turn runs, every notification is routed to [`handle_event`]; a
/// stream-failure there (a failed `event.turn.ended`) aborts with an error. After
/// the drain signal, only the already-published tail remains to be
/// forwarded, so the loop exits after a quiet window of
/// [`EVENT_DRAIN_GRACE`] without waiting for the channel to close.
async fn stream_events(
    mut events: tokio::sync::mpsc::Receiver<JsonRpcNotification>,
    mut drain_rx: tokio::sync::mpsc::Receiver<()>,
    session_id: String,
) -> Result<()> {
    loop {
        tokio::select! {
            notification = events.recv() => match notification {
                Some(notification) => handle_event(&session_id, &notification)?,
                // The event channel closed (server disposed): nothing more
                // will ever arrive, so stop streaming.
                None => return Ok(()),
            },
            _ = drain_rx.recv() => break,
        }
    }

    let drain_deadline = tokio::time::Instant::now() + EVENT_DRAIN_GRACE;
    loop {
        let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(notification)) => handle_event(&session_id, &notification)?,
            // Channel closed, or a quiet window: the relay caught up.
            Ok(None) | Err(_) => break,
        }
    }
    Ok(())
}

/// Route one event notification: assistant text to stdout, turn progress and
/// failures to stderr.
///
/// The engine emits `event.<type>` notifications (the shared projection of
/// `mirri-core`'s `SessionEvent`s, with the flat camelCase payload fields
/// merged into the envelope params). The single event stream covers *every*
/// session, so notifications carrying a different `sessionId` are ignored
/// (print mode has exactly one session, but the filter keeps the routing
/// precise).
fn handle_event(session_id: &str, notification: &JsonRpcNotification) -> Result<()> {
    let Some(event_type) = notification.method.strip_prefix("event.") else {
        // Not an engine event notification — nothing to route.
        return Ok(());
    };
    let params = &notification.params;
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        return Ok(());
    }
    match event_type {
        "assistant.delta" => {
            if let Some(text) = params.get("delta").and_then(Value::as_str) {
                print!("{text}");
                std::io::stdout().flush().context("flush stdout")?;
            }
        }
        // Only a failed turn surfaces here; a successful `turn.ended` is
        // reported through the `message.send` response instead.
        "turn.ended" if params.get("reason").and_then(Value::as_str) == Some("failed") => {
            let error = params
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(anyhow!("engine turn failed: {error}"));
        }
        "approval.requested" => {
            let tool = params
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            eprintln!("[tool] awaiting approval: {tool}");
        }
        "question.requested" => {
            let question = params
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            eprintln!("[question] {question}");
        }
        _ => {}
    }
    Ok(())
}
