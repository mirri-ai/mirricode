//! mirri-acp: Agent Client Protocol host binary.
//!
//! This binary embeds the Mirri Code Rust engine and exposes it over
//! the Agent Client Protocol (ACP) via stdio (JSON-RPC 2.0, newline-
//! delimited JSON).
//!
//! IDEs (Zed, JetBrains, custom front-ends) spawn this binary and
//! communicate over stdin/stdout. The engine runs in-process — no
//! network, no IPC, no Node.js runtime required.
//!
//! ## Architecture
//!
//! ```text
//! IDE ──stdio──► mirri-acp ──► mirri-transport (StdioTransport)
//!                    │              │
//!                    │              ▼
//!                    │         mirri-core (engine)
//!                    │
//!                    └─► reverse RPC: approval.ask, question.ask,
//!                        fs.read/write, terminal.*
//! ```
//!
//! ## Usage
//!
//! ```sh
//! mirri-acp                    # start ACP host on stdin/stdout
//! mirri-acp --version          # print version
//! ```

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mirri_acp::{AcpHandler, StdioTransport};
use mirri_core::WorkspaceId;
use mirri_transport::adapter::EngineAdapter;
use mirri_transport::frame::{JsonRpcError, JsonRpcFrame, JsonRpcResponse};
use mirri_transport::transport::{Transport, TransportError};

/// How long the host waits for the client to answer a
/// `session/request_permission` request before falling back to a rejected
/// approval — earlier than the engine's turn stall timeout, so an
/// unresponsive client denies the tool instead of hanging the turn.
/// Test knobs can shorten it through `MIRRI_ACP_PERMISSION_TIMEOUT_MS`.
const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The effective permission-request timeout ([`PERMISSION_REQUEST_TIMEOUT`]
/// unless `MIRRI_ACP_PERMISSION_TIMEOUT_MS` overrides it — a test knob for
/// exercising the fallback without waiting a minute).
fn permission_request_timeout() -> Duration {
    std::env::var("MIRRI_ACP_PERMISSION_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(PERMISSION_REQUEST_TIMEOUT)
}

/// Entry point for the ACP binary.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging to stderr (stdout is the protocol channel).
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::args().any(|arg| arg == "--version") {
        println!("mirri-acp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing::info!("mirri-acp starting (pid={})", std::process::id());

    // The engine backing the ACP host: the embedded engine rooted at the
    // workspace directory by default; a remote `mirri-server` engine when
    // `MIRRI_ACP_REMOTE_ENGINE` is set (e.g. `ws://127.0.0.1:58627`). The
    // remote engine speaks the unified JSON-RPC protocol through
    // `mirri-remote`, with the approval/question reverse-RPC domains
    // negotiated at the handshake.
    let engine: Arc<dyn EngineAdapter> = match std::env::var("MIRRI_ACP_REMOTE_ENGINE") {
        Ok(remote_url) => {
            let token = std::env::var("MIRRI_ACP_REMOTE_TOKEN").ok();
            tracing::info!(url = %remote_url, "mirri-acp using REMOTE engine");
            mirri_remote::RemoteAdapter::connect(
                &remote_url,
                token.as_deref(),
                &["approval", "question"],
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("remote engine connect failed ({remote_url}): {error}")
            })?
        }
        Err(_) => {
            // Fully-wired embedded engine: config.toml, MCP servers, plugins,
            // and agent profiles are auto-discovered, and the config-aware
            // LLM factory is attached so prompts run turns on the configured
            // `[models]` aliases, with provider credentials injectable through
            // the environment (ANTHROPIC_API_KEY / OPENAI_API_KEY — the
            // provider-resolution layer under each configured alias).
            // Rooted at the workspace directory: `MIRRI_ACP_WORKSPACE_DIR` when
            // set (tests point it at a throwaway tempdir), the current working
            // directory otherwise.
            let workspace_dir = std::env::var("MIRRI_ACP_WORKSPACE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    std::env::current_dir().expect("current working directory must be readable")
                });
            tracing::info!("mirri-acp using embedded engine");
            let default_engine: Arc<dyn EngineAdapter> =
                Arc::new(mirri_embedded::EmbeddedServer::for_workspace_root(
                    WorkspaceId::new("acp"),
                    workspace_dir.clone(),
                    None,
                    Vec::new(),
                ));
            // The per-cwd engine pool: every `session/new` / `session/load`
            // carrying a `cwd` routes to the engine cached under that
            // (canonicalized) directory — same-cwd sessions share it, so the
            // engine's workspace-rooted FS / permission scope stays per-cwd
            // without ever making the engine's sessions cwd-aware. A request
            // whose cwd is the default root reuses the default engine
            // instead of minting a duplicate over the same directory.
            let default_root = std::fs::canonicalize(&workspace_dir).unwrap_or(workspace_dir);
            let engine_for_pool = Arc::clone(&default_engine);
            let factory_root = default_root.clone();
            let factory: mirri_acp::EngineFactory = Arc::new(move |cwd: &std::path::Path| {
                if cwd == factory_root {
                    return Arc::clone(&engine_for_pool);
                }
                Arc::new(mirri_embedded::EmbeddedServer::for_workspace_root(
                    WorkspaceId::new("acp"),
                    cwd.to_path_buf(),
                    None,
                    Vec::new(),
                )) as Arc<dyn EngineAdapter>
            });
            let transport = Arc::new(StdioTransport::new());
            let handler = Arc::new(AcpHandler::with_engine_pool(
                Arc::clone(&default_engine),
                factory,
            ));
            // The default engine's root is what its sessions list as their
            // cwd and where `session/delete` removes the persisted files
            // (the pool mint records every factory engine itself).
            handler.remember_engine_cwd(&default_engine, default_root);
            serve(&transport, &handler).await?;
            tracing::info!("mirri-acp shutting down (stdin closed)");
            return Ok(());
        }
    };

    let transport = Arc::new(StdioTransport::new());
    let handler = Arc::new(AcpHandler::with_engine(engine));
    serve(&transport, &handler).await?;

    tracing::info!("mirri-acp shutting down (stdin closed)");
    Ok(())
}

/// Read frames from the transport and dispatch them to the handler until
/// stdin closes, sending one response per request and relaying ACP
/// `session/update` notifications as they stream from the engine.
///
/// JSON-RPC notifications (frames without an `id`) are processed but never
/// answered — in particular `session/cancel` and `$/cancel_request`, which
/// ACP / JSON-RPC define as notifications carrying no response.
///
/// Each client request is handled on its own task: a `session/prompt` turn
/// runs to completion before its response, and the engine may need the
/// client to answer a `session/request_permission` request *while that turn
/// is still running* (the approval reverse-RPC bridge). Those outbound
/// requests are pumped onto the transport and resolved when the matching
/// response frame comes back (the transport's pending-request table).
async fn serve(transport: &Arc<StdioTransport>, handler: &Arc<AcpHandler>) -> anyhow::Result<()> {
    let mut updates = handler.subscribe_updates();
    let mut client_requests = handler.subscribe_client_requests();
    loop {
        tokio::select! {
            frame = transport.recv() => {
                match frame {
                    Ok(JsonRpcFrame::Request(request)) => {
                        tracing::debug!(method = %request.method, id = ?request.id, "request received");
                        let is_notification = request.id.is_none();
                        let request_id = request.id;
                        let response_id = request_id.clone();
                        let method = request.method.clone();
                        let transport = Arc::clone(transport);
                        let handler = Arc::clone(handler);
                        // Handle concurrently: the loop must stay free to pump
                        // the engine's reverse-RPC requests (and to process
                        // `session/cancel` / `$/cancel_request`) while a
                        // prompt turn runs. The client-facing request id
                        // rides along, so `$/cancel_request` can name the
                        // in-flight prompt it wants aborted.
                        tokio::spawn(async move {
                            let response = match handler
                                .handle_request(request_id, &method, request.params)
                                .await
                            {
                                Ok(result) => JsonRpcResponse {
                                    id: response_id,
                                    result: Some(result),
                                    error: None,
                                },
                                Err((code, message)) => JsonRpcResponse {
                                    id: response_id,
                                    result: None,
                                    error: Some(JsonRpcError {
                                        code,
                                        message,
                                        data: None,
                                    }),
                                },
                            };
                            if is_notification {
                                // A request without an id is a JSON-RPC
                                // notification: process it but never reply
                                // (e.g. `session/cancel`).
                                tracing::debug!(method, "request without id processed as notification; no response sent");
                            } else {
                                let _ = transport
                                    .send(JsonRpcFrame::Response(response))
                                    .await;
                            }
                        });
                    }
                    Ok(JsonRpcFrame::Notification(notification)) => {
                        // Notifications never receive a response, but
                        // `session/cancel` and `$/cancel_request` still
                        // carry work: the former aborts the addressed
                        // session's in-flight turns, the latter aborts the
                        // in-flight request its `requestId` names.
                        if notification.method == "session/cancel"
                            || notification.method == "$/cancel_request"
                        {
                            tracing::debug!(
                                method = %notification.method,
                                "cancellation notification processed (no response)"
                            );
                            let _ = handler
                                .handle(&notification.method, notification.params)
                                .await;
                        } else {
                            tracing::debug!(method = %notification.method, "notification received (no response)");
                        }
                    }
                    Ok(JsonRpcFrame::Response(frame)) => {
                        // Responses matching a parked outbound request are
                        // resolved inside the transport and never surface
                        // here; anything else is a protocol violation (the
                        // host only expects responses to the
                        // `session/request_permission` requests it issues).
                        tracing::warn!("received a response frame from the client; ignoring: {frame:?}");
                    }
                    Err(TransportError::Closed) => break,
                    Err(TransportError::Parse(error)) => {
                        // A request whose text cannot be parsed is answered with a
                        // -32700 error and a null id (JSON-RPC 2.0 §4.2).
                        tracing::debug!(%error, "request failed to parse");
                        transport.send(parse_error_response()).await?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(notification) => {
                        tracing::debug!(method = %notification.method, "relaying update notification");
                        transport.send(JsonRpcFrame::Notification(notification)).await?;
                    }
                    Err(error) => {
                        // A lagged receiver keeps streaming (P5e decision
                        // B): no bounded blocking, the dropped batch is
                        // logged; only channel closure ends the relay.
                        if !handle_update_relay_failure(error) {
                            break;
                        }
                    }
                }
            }
            request = client_requests.recv() => {
                match request {
                    Some(pending) => {
                        tracing::debug!(method = %pending.method, "relaying reverse-RPC request to the client");
                        let method = pending.method.clone();
                        let transport = Arc::clone(transport);
                        let params = pending.params.clone();
                        let timeout = permission_request_timeout();
                        tokio::spawn(async move {
                            let result = transport
                                .request(&method, params, timeout)
                                .await;
                            pending.respond(result);
                        });
                    }
                    None => {
                        // Every handler is gone (shutdown); nothing to pump.
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build the JSON-RPC 2.0 parse-error response (`-32700`, null id).
fn parse_error_response() -> JsonRpcFrame {
    JsonRpcFrame::Response(JsonRpcResponse {
        id: None,
        result: None,
        error: Some(JsonRpcError {
            code: -32700,
            message: "parse error".to_string(),
            data: None,
        }),
    })
}

/// Handle one failed `session/update` receive on the relay (the serve
/// loop's lag branch).
///
/// A lag means the engine outran the update channel and `skipped`
/// notifications were dropped — with the 1024-slot capacity this only
/// happens to a genuinely slow consumer, and the loss is deliberately
/// *loud* (a `tracing::warn!`, never a silent continue): the durable
/// session history lives in the wire log, so a client that cares can
/// `session/load` to recover what the UI missed. There is **no bounded
/// blocking** (P5e decision B): backpressuring the relay would cascade
/// through the engine's event forwarder and stall the whole turn for
/// every slow client, which is strictly worse than losing UI chunks.
///
/// Returns whether the relay should keep streaming — `false` only when
/// the channel closed (every producer is gone; shutdown).
fn handle_update_relay_failure(error: tokio::sync::broadcast::error::RecvError) -> bool {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(skipped) => {
            tracing::warn!(
                skipped,
                "session/update relay lagged: {skipped} update notification(s) dropped \
                 (the client fell behind the update channel); the stream resumes with the \
                 next notification and session/load replays any lost history"
            );
            true
        }
        tokio::sync::broadcast::error::RecvError::Closed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_transport::frame::JsonRpcNotification;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A shared byte buffer usable as a `tracing` writer, so a test can
    /// capture what the lag branch logs.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone())
                .expect("the captured log is valid UTF-8")
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Given an update channel one notification past its capacity, when
    /// the relay's receive fails with the lag, then the failure handler
    /// warns (never a silent drop), keeps the relay streaming, and the
    /// stream resumes with the oldest retained notification.
    #[test]
    fn should_warn_and_keep_streaming_when_the_update_relay_lags() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let (tx, mut rx) = tokio::sync::broadcast::channel(2);
                    let notification = |seq: i64| JsonRpcNotification {
                        method: "session/update".to_string(),
                        params: json!({"seq": seq}),
                    };
                    for seq in 0..3 {
                        tx.send(notification(seq))
                            .expect("a subscribed receiver takes every send");
                    }
                    let error = rx
                        .recv()
                        .await
                        .expect_err("a capacity-2 channel lags after 3 sends");
                    assert!(
                        handle_update_relay_failure(error),
                        "a lag keeps the relay streaming (the next notification resumes)"
                    );
                    assert!(
                        rx.recv().await.is_ok(),
                        "the stream must resume with the oldest retained notification"
                    );
                });
        });
        let logged = buf.contents();
        assert!(
            logged.contains("session/update"),
            "the lag must warn loudly, not pass silently: {logged:?}"
        );
        assert!(
            logged.contains("dropped"),
            "the warn names the dropped batch: {logged:?}"
        );
    }

    /// Given a closed update channel, when the relay's receive fails with
    /// closure, then the failure handler ends the relay.
    #[tokio::test]
    async fn should_stop_the_relay_when_the_update_channel_closes() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<JsonRpcNotification>(2);
        drop(tx);
        let error = rx
            .recv()
            .await
            .expect_err("a dropped sender closes the channel");
        assert!(
            !handle_update_relay_failure(error),
            "channel closure ends the relay"
        );
    }
}
