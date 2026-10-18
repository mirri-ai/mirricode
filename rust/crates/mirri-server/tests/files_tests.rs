//! Integration tests for the files REST surface (P4): multipart upload,
//! binary download, expiry metadata, and the error paths.

mod common;

use std::time::Duration;

use common::TestServer;
use serde_json::Value;

/// HTTP client with a generous timeout so slow CI machines do not flake.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build http client")
}

/// Build a `multipart/form-data` body by hand: one `file` part (with
/// filename and content type) plus the extra text fields, in order.
fn multipart_body(
    boundary: &str,
    filename: &str,
    mime_type: &str,
    data: &[u8],
    fields: &[(&str, &str)],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// POST a multipart upload and return `(status, envelope json)`.
async fn upload(
    client: &reqwest::Client,
    server: &TestServer,
    boundary: &str,
    body: Vec<u8>,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(format!("{}/api/v1/files", server.base_url()))
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .expect("POST upload");
    let status = response.status();
    let envelope: Value = response.json().await.expect("JSON envelope");
    (status, envelope)
}

#[tokio::test]
async fn should_return_file_meta_when_uploading_multipart_file() {
    // Given an open server with a files dir
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When a file is uploaded as multipart/form-data
    let body = multipart_body(
        "testboundary",
        "photo.png",
        "image/png",
        b"png-bytes-here",
        &[],
    );
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the answer is the success envelope with the file metadata (the
    // TS `FileMeta` wire shape)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    let data = &envelope["data"];
    let id = data["id"].as_str().expect("file id");
    assert!(id.starts_with("f_"), "id must be f_-prefixed: {id}");
    assert_eq!(data["name"], "photo.png");
    assert_eq!(data["media_type"], "image/png");
    assert_eq!(data["size"], 14);
    assert!(
        data.get("expires_at").is_none() || data["expires_at"].is_null(),
        "no expiry without expires_in_sec"
    );
}

#[tokio::test]
async fn should_return_uploaded_bytes_when_getting_file_by_id() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = b"the quick brown fox jumps over the lazy dog";
    let body = multipart_body(
        "testboundary",
        "fox.txt",
        "text/plain",
        payload,
        &[("name", "renamed.txt")],
    );
    let (_, envelope) = upload(&client, &server, "testboundary", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When the file is downloaded by id
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact bytes come back with the metadata-driven headers
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let headers = response.headers().clone();
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], payload);
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    let disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .expect("content-disposition header");
    assert!(
        disposition.starts_with("attachment; filename=\"renamed.txt\""),
        "disposition: {disposition}"
    );
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("43")
    );
}

#[tokio::test]
async fn should_serve_inline_disposition_when_getting_image_file() {
    // Given an uploaded image
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body("b", "cat.png", "image/png", b"not-really-png", &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When it is downloaded
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the disposition is inline (image/* renders in the browser)
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .expect("content-disposition header");
    assert!(
        disposition.starts_with("inline"),
        "disposition: {disposition}"
    );
}

#[tokio::test]
async fn should_record_expires_at_when_uploading_with_expires_in_sec() {
    // Given an upload carrying the expiry field
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body(
        "testboundary",
        "temp.bin",
        "application/octet-stream",
        b"xyz",
        &[("expires_in_sec", "3600")],
    );
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the metadata records the future expiry timestamp
    assert_eq!(status, reqwest::StatusCode::OK);
    let expires_at = envelope["data"]["expires_at"].as_str().expect("expires_at");
    assert!(!expires_at.is_empty(), "expiry must be an ISO timestamp");
}

#[tokio::test]
async fn should_answer_404_when_getting_unknown_file() {
    // Given an empty files store
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When an unknown file id is fetched
    let response = client
        .get(format!("{}/api/v1/files/f_nope", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the answer is the 404 error envelope (the binary route's error
    // shape — matching the TS server's FILE_NOT_FOUND mapping)
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_204_and_remove_file_when_deleting_by_id() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body("b", "doomed.txt", "text/plain", b"delete me", &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When the file is deleted by id
    let response = client
        .delete(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the answer is 204 No Content with an empty body
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    let body = response.bytes().await.expect("empty body");
    assert!(body.is_empty(), "204 must carry no body: {body:?}");

    // And the file is gone: a follow-up GET answers the 404 error envelope
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");

    // And the blob itself was removed from the store root
    let files_dir = server.state.files_dir.clone().expect("files dir injected");
    assert!(
        !files_dir.join(&id).exists(),
        "the blob file must be removed from the files root"
    );
}

#[tokio::test]
async fn should_answer_404_when_deleting_unknown_file() {
    // Given an empty files store
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When an unknown file id is deleted
    let response = client
        .delete(format!("{}/api/v1/files/f_nope", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the answer is the 404 error envelope
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_401_when_deleting_file_without_token() {
    // Given a token-protected server
    let server = TestServer::start_with_auth("secret-token").await;
    let client = client();

    // When a file delete is requested without the credential
    let response = client
        .delete(format!("{}/api/v1/files/f_anything", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_error_envelope_when_uploading_without_file_part() {
    // Given a multipart body that carries no file part at all — only text
    // fields (fastify's `req.file()` treats the first filename-carrying part
    // as the upload regardless of its field name, so a body without one has
    // nothing to store)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = b"--testboundary\r\n\
Content-Disposition: form-data; name=\"name\"\r\n\
\r\n\
just-a-text-field\r\n\
--testboundary--\r\n"
        .to_vec();

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the answer is the error envelope (validation failed)
    assert_eq!(status, reqwest::StatusCode::OK, "TS serves 200 + envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_serve_partial_content_when_range_requested() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = b"0123456789abcdef";
    let body = multipart_body("b", "hex.bin", "application/octet-stream", payload, &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When a byte range is requested
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .header(reqwest::header::RANGE, "bytes=2-5")
        .send()
        .await
        .expect("GET file range");

    // Then the slice comes back as 206 Partial Content
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let headers = response.headers().clone();
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], b"2345");
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 2-5/16")
    );
}

/// A deterministic 100 MiB byte pattern (a splitmix-style hash of the
/// offset): regenerable on both the upload and the download side without
/// ever buffering the whole payload.
#[cfg(test)]
mod peak {
    use super::*;

    const TOTAL: usize = 100 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    const BOUNDARY: &str = "biguploadboundary";

    fn pattern_at(offset: usize, buf: &mut [u8]) {
        for (i, byte) in buf.iter_mut().enumerate() {
            let position = (offset + i) as u64;
            *byte = (position.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
        }
    }

    /// Stream the full multipart body (headers, then the 100 MiB pattern in
    /// 64 KiB chunks, then the closing delimiter) without ever materializing
    /// it client-side — the upload memory profile of the *server* under test
    /// is what the surrounding `/usr/bin/time -l` measurement observes.
    fn upload_body() -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
        use futures::StreamExt as _;
        let prefix = format!(
            "--{BOUNDARY}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n"
        );
        let data = futures::stream::unfold(0usize, |offset| async move {
            if offset >= TOTAL {
                return None;
            }
            let len = CHUNK.min(TOTAL - offset);
            let mut chunk = vec![0u8; len];
            pattern_at(offset, &mut chunk);
            Some((Ok(chunk), offset + len))
        });
        let suffix = Ok(format!("\r\n--{BOUNDARY}--\r\n").into_bytes());
        futures::stream::iter(vec![Ok(prefix.into_bytes())])
            .chain(data)
            .chain(futures::stream::iter(vec![suffix]))
    }

    /// Manual peak-RSS measurement (plans/07 T10 F-P4-4): boot the test binary
    /// under `/usr/bin/time -l` and record the maximum resident set size —
    /// with a streaming upload the peak must stay far below the 100 MiB
    /// payload. Not part of the regular gate (it uploads 100 MiB through
    /// loopback on every run).
    #[tokio::test]
    #[ignore = "manual peak-RSS measurement: run under /usr/bin/time -l (plans/07 T10 F-P4-4)"]
    async fn should_stream_a_100mb_upload_with_bounded_memory() {
        // Given an open server with a files dir
        let server = TestServer::start_with_files_dir().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("build http client");

        // When the 100 MiB pattern is streamed up as one multipart upload
        let response = client
            .post(format!("{}/api/v1/files", server.base_url()))
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(reqwest::Body::wrap_stream(upload_body()))
            .send()
            .await
            .expect("POST upload");

        // Then the upload succeeds with the full size recorded
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let envelope: Value = response.json().await.expect("JSON envelope");
        assert_eq!(envelope["code"], 0, "envelope: {envelope}");
        assert_eq!(envelope["data"]["size"], TOTAL as u64);
        let id = envelope["data"]["id"]
            .as_str()
            .expect("file id")
            .to_string();

        // And the blob on disk is byte-exact, verified through a windowed
        // read (never a whole-file load — the GET route buffers its whole
        // body, which would pollute the memory-peak measurement under test)
        let files_dir = server.state.files_dir.clone().expect("files dir injected");
        let mut blob = std::fs::File::open(files_dir.join(&id)).expect("blob file exists");
        let mut offset = 0usize;
        let mut window = vec![0u8; CHUNK];
        loop {
            let read = std::io::Read::read(&mut blob, &mut window).expect("blob read");
            if read == 0 {
                break;
            }
            let mut expected = vec![0u8; read];
            pattern_at(offset, &mut expected);
            assert!(
                window[..read] == expected[..],
                "stored bytes diverge from the pattern at offset {offset}"
            );
            offset += read;
        }
        assert_eq!(offset, TOTAL, "the whole payload was stored");
    }
}

#[tokio::test]
async fn should_upload_file_larger_than_default_body_limit() {
    // Given a payload an order of magnitude above axum's default 2 MB body
    // limit (the TS server caps uploads at MAX_SAFE_INTEGER — effectively
    // uncapped — and the rust route must not truncate at 2 MB)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = vec![0x5Au8; 3 * 1024 * 1024];
    let body = multipart_body("b", "big.bin", "application/octet-stream", &payload, &[]);

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "b", body).await;

    // Then the upload succeeds with the full size recorded
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["data"]["size"], 3 * 1024 * 1024);
}

#[tokio::test]
async fn should_store_the_full_payload_when_the_close_delimiter_ends_at_eof() {
    // Given a hand-rolled multipart body that ends with the close delimiter
    // and no trailing CRLF (RFC 2046 legal; browsers/curl always append the
    // CRLF, a hand-written client need not) — the review's F-T10-1 repro
    // shape: the pending payload must be flushed, not silently dropped
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = b"--b\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"eof.bin\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
8-bytes!\r\n--b--"
        .to_vec();

    // When it is uploaded and downloaded back
    let (status, envelope) = upload(&client, &server, "b", body).await;
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    assert_eq!(envelope["data"]["size"], 8);
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact payload survives — no silent truncation at EOF
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], b"8-bytes!", "bytes: {bytes:?}");
}

#[tokio::test]
async fn should_reject_upload_with_excessive_transport_padding() {
    // Given a multipart body padding its close delimiter with far more LWSP
    // than the cap (an unbounded padding run would pin the parser buffer —
    // the review's F-T10-2 DoS shape)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let mut body = b"--b\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"pad.bin\"\r\n\
\r\n\
payload\r\n--b--"
        .to_vec();
    body.extend(std::iter::repeat_n(b' ', 8 * 1024));

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "b", body).await;

    // Then the request is rejected (the route's error convention: 200 + the
    // error envelope, as for any malformed multipart body)
    assert_eq!(status, reqwest::StatusCode::OK, "TS serves 200 + envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
    let message = envelope["msg"].as_str().unwrap_or_default();
    assert!(message.contains("padding"), "envelope: {envelope}");
}

/// A multipart body whose file payload deliberately embeds byte sequences
/// that collide with the boundary delimiters — the naive-scan parser
/// (F-P4-6) treated any `--boundary` byte run as a delimiter and truncated
/// the file there.
fn adversarial_body(boundary: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"AA--bZZ"); // bare boundary inline
    payload.extend_from_slice(b"\r\n--bXYZ\r\n"); // CRLF boundary prefix + junk suffix
    payload.extend_from_slice(b"\r\n--b--junk"); // close-delimiter variant + junk
    payload.extend_from_slice(b"\r\n--b-- \ttail\r\n"); // close + LWSP + junk
    payload.extend_from_slice(b"\r\n--b-"); // boundary prefix fragment
    payload.push(b'\r');
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n\
          Content-Type: application/octet-stream\r\n\
          \r\n",
    );
    body.extend_from_slice(&payload);
    // A trailing text field after the file part proves the collision did not
    // desynchronize the parser either (no cross-part bleed).
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"label\"\r\n\r\ncollides\r\n");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn should_roundtrip_file_content_containing_boundary_collisions() {
    // Given an upload whose file content embeds boundary-shaped byte runs
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = adversarial_body("b");

    // When it is uploaded and downloaded back
    let (status, envelope) = upload(&client, &server, "b", body).await;
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact adversarial payload round-trips — no truncation at any
    // of the collision byte runs, and no bleed from the trailing field
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("file bytes");
    let expected: &[u8] = b"AA--bZZ\r\n--bXYZ\r\n\r\n--b--junk\r\n--b-- \ttail\r\n\r\n--b-\r";
    assert_eq!(&bytes[..], expected, "bytes: {bytes:?}");
}
