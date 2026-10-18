//! Minimal local HTTP callback server for the OAuth authorization-code flow.
//!
//! The authorization server redirects the user's browser to
//! `http://localhost:<port>/callback?code=...&state=...`. This module binds a
//! `tokio::net::TcpListener` on loopback and parses the incoming request with
//! hand-rolled HTTP parsing (no hyper dependency):
//!
//! - reads the request head up to `\r\n\r\n`
//! - parses `GET /callback?code=...&state=... HTTP/1.1`
//! - percent-decodes the query parameters
//! - replies with a small HTML page and closes the connection

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::oauth::error::{OAuthError, OAuthResult};

/// The callback path served by this listener.
pub(crate) const CALLBACK_PATH: &str = "/callback";
/// Cap on the request head size, to avoid buffering abusive requests.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Per-connection deadline for delivering the request head.
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The result of handling one callback connection.
enum ConnectionOutcome {
    /// The request was not the authorization callback (e.g. `/favicon.ico`);
    /// keep waiting for the real one.
    NotTheCallback,
    /// The callback carried a valid authorization code.
    Code(String),
}

/// A single parsed HTTP request head, as far as the flow needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedHttpRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
}

/// Loopback listener that waits for the authorization callback.
pub(crate) struct CallbackServer {
    listener: TcpListener,
    redirect_uri: String,
}

impl CallbackServer {
    /// Bind the callback listener on loopback. `None` picks a random free
    /// port; the actual port is reflected in [`Self::redirect_uri`].
    pub(crate) async fn bind(port: Option<u16>) -> OAuthResult<Self> {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
        let listener = TcpListener::bind(address)
            .await
            .map_err(|error| OAuthError::CallbackServer(format!("failed to bind: {error}")))?;
        let actual_port = listener
            .local_addr()
            .map_err(|error| OAuthError::CallbackServer(format!("failed to read port: {error}")))?
            .port();
        let redirect_uri = format!("http://localhost:{actual_port}{CALLBACK_PATH}");
        tracing::debug!(redirect_uri, "bound OAuth callback server");
        Ok(Self {
            listener,
            redirect_uri,
        })
    }

    /// The redirect URI the authorization server must send the browser to.
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the browser callback carrying an authorization code.
    ///
    /// - Requests that are not the callback (e.g. favicon probes) are answered
    ///   with 404 and ignored.
    /// - A callback with an `error` parameter aborts with [`OAuthError::UserDenied`].
    /// - A callback whose `state` does not match `expected_state` aborts with
    ///   [`OAuthError::StateMismatch`].
    /// - If nothing completes within `timeout`, aborts with
    ///   [`OAuthError::CallbackTimeout`].
    pub(crate) async fn wait_for_authorization_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> OAuthResult<String> {
        tokio::time::timeout(timeout, self.accept_callback(expected_state))
            .await
            .map_err(|_| OAuthError::CallbackTimeout)?
    }

    async fn accept_callback(self, expected_state: &str) -> OAuthResult<String> {
        loop {
            let (mut socket, _peer) =
                self.listener.accept().await.map_err(|error| {
                    OAuthError::CallbackServer(format!("accept failed: {error}"))
                })?;
            match handle_callback_connection(&mut socket, expected_state).await? {
                ConnectionOutcome::NotTheCallback => continue,
                ConnectionOutcome::Code(code) => return Ok(code),
            }
        }
    }
}

/// Serve one connection; see [`CallbackServer::wait_for_authorization_code`]
/// for the outcome semantics.
async fn handle_callback_connection(
    socket: &mut TcpStream,
    expected_state: &str,
) -> OAuthResult<ConnectionOutcome> {
    let head = match tokio::time::timeout(CONNECTION_READ_TIMEOUT, read_request_head(socket)).await
    {
        Ok(Ok(head)) => head,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "failed to read callback request head");
            return Ok(ConnectionOutcome::NotTheCallback);
        }
        Err(_) => {
            tracing::warn!("timed out reading callback request head");
            return Ok(ConnectionOutcome::NotTheCallback);
        }
    };

    let Some(request) = parse_http_request_head(&head) else {
        let _ = send_response(
            socket,
            "400 Bad Request",
            "text/plain",
            "Malformed HTTP request",
        )
        .await;
        return Ok(ConnectionOutcome::NotTheCallback);
    };

    if request.method != "GET" {
        let _ = send_response(
            socket,
            "405 Method Not Allowed",
            "text/plain",
            "OAuth callback expects a GET request",
        )
        .await;
        return Ok(ConnectionOutcome::NotTheCallback);
    }

    if request.path != CALLBACK_PATH {
        let _ = send_response(socket, "404 Not Found", "text/plain", "Not found").await;
        return Ok(ConnectionOutcome::NotTheCallback);
    }

    // `?error=access_denied` — the user (or the server) declined.
    if let Some(error) = request.query.get("error") {
        let description = request
            .query
            .get("error_description")
            .map(String::as_str)
            .unwrap_or(error);
        let _ = send_response(
            socket,
            "400 Bad Request",
            "text/html",
            &denied_page(error, description),
        )
        .await;
        return Err(OAuthError::UserDenied(description.to_string()));
    }

    let Some(code) = request.query.get("code") else {
        // A request to /callback without a code — not the real callback.
        let _ = send_response(
            socket,
            "400 Bad Request",
            "text/plain",
            "Missing authorization code",
        )
        .await;
        return Ok(ConnectionOutcome::NotTheCallback);
    };

    // CSRF protection: the state must match the one we sent.
    if request.query.get("state").map(String::as_str) != Some(expected_state) {
        let _ = send_response(
            socket,
            "403 Forbidden",
            "text/html",
            "<html><body><h1>State mismatch</h1><p>The OAuth callback state did not match.</p></body></html>",
        )
        .await;
        return Err(OAuthError::StateMismatch);
    }

    tracing::debug!("received OAuth authorization code callback");
    let _ = send_response(socket, "200 OK", "text/html", &success_page()).await;
    Ok(ConnectionOutcome::Code(code.clone()))
}

/// Read the request head (everything up to and including `\r\n\r\n`).
async fn read_request_head(socket: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head exceeds size limit",
            ));
        }
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Parse the request line and query string of an HTTP request head.
pub(crate) fn parse_http_request_head(head: &str) -> Option<ParsedHttpRequest> {
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() || !version.starts_with("HTTP/") {
        return None;
    }
    let (path, query_str) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query),
        None => (target.to_string(), ""),
    };
    let query = parse_query_params(query_str);
    Some(ParsedHttpRequest {
        method,
        path,
        query,
    })
}

/// Parse `application/x-www-form-urlencoded` style query parameters:
/// split on `&`, then on the first `=`, decoding both keys and values.
fn parse_query_params(query_str: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query_str.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

/// Decode percent-escapes (`%XX`) and `+` as a space (form encoding).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
                {
                    decoded.push(high * 16 + low);
                    index += 3;
                    continue;
                }
                decoded.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn send_response(
    socket: &mut TcpStream,
    status_line: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

fn success_page() -> String {
    "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Authorization complete</title></head>\
     <body><h1>Authorization complete</h1>\
     <p>You have successfully authorized mirri-code. You can close this tab and return to the terminal.</p>\
     </body></html>"
        .to_string()
}

/// Escape HTML special characters to prevent XSS in callback pages.
fn html_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn denied_page(error: &str, description: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Authorization failed</title></head>\
         <body><h1>Authorization failed</h1><p><strong>{}</strong>: {}</p>\
         <p>You can close this tab and return to the terminal.</p></body></html>",
        html_escape(error),
        html_escape(description)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_a_callback_request_with_query_params_when_parsing_http_request_head() {
        let head = "GET /callback?code=auth-code-1&state=state-1&scope=read%20write HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let request = parse_http_request_head(head).expect("request should parse");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/callback");
        assert_eq!(
            request.query.get("code").map(String::as_str),
            Some("auth-code-1")
        );
        assert_eq!(
            request.query.get("state").map(String::as_str),
            Some("state-1")
        );
        assert_eq!(
            request.query.get("scope").map(String::as_str),
            Some("read write")
        );
    }

    #[test]
    fn should_parse_a_request_without_a_query_when_parsing_http_request_head() {
        let head = "GET /favicon.ico HTTP/1.1\r\n\r\n";
        let request = parse_http_request_head(head).expect("request should parse");
        assert_eq!(request.path, "/favicon.ico");
        assert!(request.query.is_empty());
    }

    #[test]
    fn should_percent_decode_keys_and_values_when_parsing_query_params() {
        let head = "GET /callback?co%64e=a%20b&plus=a+b&raw=plain HTTP/1.1\r\n\r\n";
        let request = parse_http_request_head(head).expect("request should parse");
        assert_eq!(request.query.get("code").map(String::as_str), Some("a b"));
        assert_eq!(request.query.get("plus").map(String::as_str), Some("a b"));
        assert_eq!(request.query.get("raw").map(String::as_str), Some("plain"));
    }

    #[test]
    fn should_keep_malformed_escapes_literal_when_decoding_percent_escapes() {
        assert_eq!(percent_decode("100%zz"), "100%zz");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn should_return_none_for_garbage_input_when_parsing_http_request_head() {
        assert!(parse_http_request_head("").is_none());
        assert!(parse_http_request_head("not a request line").is_none());
    }
}
