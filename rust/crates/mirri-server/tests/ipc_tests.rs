//! Integration tests for the Unix-socket IPC transport: line-delimited
//! JSON-RPC 2.0 over a Unix domain socket. Naming follows the
//! Given-When-Then convention.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::TestServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type ResponseLines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;
type WriteHalf = tokio::net::unix::OwnedWriteHalf;

/// Connect to the test server's IPC socket (retrying briefly while the
/// async server task binds it) and return the response line reader plus
/// the write half.
async fn connect_ipc(server: &TestServer) -> (ResponseLines, WriteHalf) {
    let path = server
        .ipc_path
        .as_ref()
        .expect("test server booted with IPC");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("connect to the IPC socket: {error}"),
        }
    };
    let (read_half, write_half) = stream.into_split();
    (BufReader::new(read_half).lines(), write_half)
}

/// Send one JSON-RPC line (the transport's request unit).
async fn send(writer: &mut WriteHalf, line: &str) {
    writer.write_all(line.as_bytes()).await.expect("write line");
    writer.write_all(b"\n").await.expect("write newline");
    writer.flush().await.expect("flush");
}

/// Read the next response line as JSON, failing fast instead of hanging.
async fn next_response(lines: &mut ResponseLines) -> serde_json::Value {
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("response arrives within the timeout")
        .expect("stream stays open")
        .expect("a response line");
    serde_json::from_str(&line).expect("response line is JSON")
}

/// Assert that no further response arrives within a short window.
async fn assert_no_response(lines: &mut ResponseLines) {
    let stray = tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
    assert!(
        stray.is_err(),
        "no response should arrive; got {:?}",
        stray.ok().and_then(|line| line.ok().flatten())
    );
}

/// Given a running IPC server, when a client sends a line-delimited
/// JSON-RPC request, then exactly one response line comes back echoing the
/// request id with a `result` payload.
#[tokio::test]
async fn should_answer_line_delimited_requests_over_the_unix_socket() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":1,"method":"session.list","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["jsonrpc"], serde_json::json!("2.0"));
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("result").is_some(),
        "response carries a result: {response}"
    );
}

/// Given a connected IPC client, when it sends a notification (no id) and
/// a stray response frame before a valid request, then only the valid
/// request is answered — the other frames stay silent.
#[tokio::test]
async fn should_stay_silent_for_notifications_and_response_frames() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{}}"#,
    )
    .await;
    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","result":{"ignored":true},"id":99}"#,
    )
    .await;
    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":7,"method":"session.list","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(
        response["id"],
        serde_json::json!(7),
        "the first and only response serves the valid request: {response}"
    );

    assert_no_response(&mut lines).await;
}

/// Given a connected IPC client, when it sends a line that is not valid
/// JSON, then the server answers with a parse-error frame whose id is
/// null (parse errors are always answered).
#[tokio::test]
async fn should_answer_a_garbage_line_with_a_null_id_error_frame() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(&mut writer, "this is not json").await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(response["error"]["code"], serde_json::json!(-32700));
}

/// Given a connected IPC client, when it requests an unknown method with
/// an id, then the server answers with a method-not-found error that
/// echoes the id and names the method.
#[tokio::test]
async fn should_answer_unknown_methods_with_an_echoed_id_error() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":42,"method":"no.such.method","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["id"], serde_json::json!(42));
    assert_eq!(response["error"]["code"], serde_json::json!(-32601));
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no.such.method")),
        "error names the method: {}",
        response["error"]["message"]
    );
}
