//! Web search and URL fetch providers for the embedded engine.
//!
//! Implements the `WebSearchProvider` and `FetchUrlProvider` traits from
//! `mirri-core` using `reqwest` for HTTP. Three concrete providers:
//!
//! - [`LocalFetchUrlProvider`] — no-auth local HTTP fetch with SSRF guards,
//!   content-type routing (text → passthrough, HTML → tag-stripping), and
//!   a 10 MB response cap. Always available as a fallback.
//! - [`ManagedFetchUrlProvider`] — delegates to a Mirri fetch service, with
//!   `LocalFetchUrlProvider` as fallback on any error.
//! - [`ManagedWebSearchProvider`] — delegates to a Mirri search service.

use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mirri_core::error::{EngineError, EngineResult};
use mirri_core::tools::net_class::{classify_host_literal, classify_ip, host_of_url, NetClass};
use mirri_core::tools::web_providers::FetchOptions;
use mirri_core::tools::{
    FetchContentKind, FetchUrlProvider, FetchUrlResult, WebSearchProvider, WebSearchResult,
};

/// Maximum response body size (10 MB).
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// HTTP request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of HTTP redirects to follow.
const MAX_REDIRECTS: usize = 5;

// ---------------------------------------------------------------------------
// SSRF guard (ADR-010)
// ---------------------------------------------------------------------------

/// What the pre-connection safety check decided about a URL host.
enum HostVerdict {
    /// Public internet — connect.
    Public,
    /// Private IP literal in the URL (approval-gated).
    AskableLiteralIp,
    /// `localhost` / `*.localhost` hostname (approval-gated).
    AskableLocalhostName,
    /// Hostname resolved to private addresses (approval-gated).
    AskableResolved(IpAddr),
    /// Cloud metadata endpoint (literal or resolved) — permanently denied.
    /// Carries the address to quote in the denial message.
    Metadata(String),
    /// DNS resolution failed. Carries the failure detail.
    Unresolvable(String),
}

// Known limitation (ADR-010 open item): DNS pinning-to-connection is not
// implemented — classification and connection resolve separately (rebinding
// window, pre-existing).
//
/// Classify a URL host before connecting: `classify_host_literal` fast path
/// (IP literals / localhost family), then blocking DNS with the strictest
/// grade across all resolved addresses.
fn classify_target(host: &str) -> HostVerdict {
    if let Some(class) = classify_host_literal(host) {
        return match class {
            NetClass::Public => HostVerdict::Public,
            NetClass::Metadata => HostVerdict::Metadata(host.to_string()),
            NetClass::Askable => {
                if is_ip_literal(host) {
                    HostVerdict::AskableLiteralIp
                } else {
                    HostVerdict::AskableLocalhostName
                }
            }
        };
    }

    let addrs: Vec<IpAddr> = match (host, 0u16).to_socket_addrs() {
        Ok(iter) => iter.map(|sa| sa.ip()).collect(),
        Err(e) => return HostVerdict::Unresolvable(e.to_string()),
    };
    if addrs.is_empty() {
        return HostVerdict::Unresolvable("no IP addresses resolved".to_string());
    }

    let mut verdict = HostVerdict::Public;
    for ip in &addrs {
        match classify_ip(ip) {
            NetClass::Metadata => return HostVerdict::Metadata(ip.to_string()),
            NetClass::Askable => {
                if matches!(verdict, HostVerdict::Public) {
                    verdict = HostVerdict::AskableResolved(*ip);
                }
            }
            NetClass::Public => {}
        }
    }
    verdict
}

/// Whether the host string is a bare / bracketed IP literal.
fn is_ip_literal(host: &str) -> bool {
    let h = strip_ipv6_brackets(host);
    h.parse::<IpAddr>().is_ok()
}

/// Strip surrounding brackets from a bracketed IPv6 literal (`[::1]` →
/// `::1`); other inputs pass through unchanged.
fn strip_ipv6_brackets(host: &str) -> &str {
    let host = host.trim();
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

/// Normalize a host for exemption comparison: trim, strip IPv6 brackets,
/// lowercase. Port-insensitive by construction (`Url::host_str()` never
/// carries a port).
fn normalize_host(host: &str) -> String {
    strip_ipv6_brackets(host).to_lowercase()
}

/// Whether the request carries a matching private-network exemption for the
/// host.
///
/// Enforcement gates on **host equality only**: the exemption's `host` must
/// equal the (normalized) target host. `approved_ips` is deliberately NOT
/// consulted here — it is the audit record of what the user approved at
/// approval time, not an execution check; address drift on the same host is
/// covered by design (ADR-010).
fn exemption_matches(opts: &FetchOptions, host: &str) -> bool {
    let Some(exemption) = &opts.private_network_exemption else {
        return false;
    };
    normalize_host(&exemption.host) == normalize_host(host)
}

// ---------------------------------------------------------------------------
// LocalFetchUrlProvider
// ---------------------------------------------------------------------------

/// A no-auth HTTP fetch provider with SSRF guards.
///
/// Fetches URLs using `reqwest` with a blocking client. Enforces:
/// - Private/loopback/link-local IP refusal (pre-connection)
/// - Redirect following with per-hop IP re-validation
/// - 10 MB max response body
/// - Non-2xx HTTP status rejection
///
/// Content routing:
/// - `text/plain`, `text/markdown`, `application/json` → passthrough
/// - `text/html` → simple tag stripping (extract text content)
/// - Other content types → passthrough (best effort)
pub struct LocalFetchUrlProvider;

/// Process-wide HTTP client behind [`LocalFetchUrlProvider`].
///
/// A `reqwest::blocking::Client` spawns and owns a private Tokio runtime, so
/// building or dropping one inside an async context panics — while this
/// provider is routinely constructed there (e.g. by
/// `EmbeddedServer::for_workspace_root` from async entry points). The client
/// therefore lives in a process-wide static: it is built lazily on first use
/// (the tool layer runs `fetch` on the blocking pool, where building is
/// legal) and never dropped, which also keeps the connection pool alive
/// across turns.
static LOCAL_FETCH_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

impl LocalFetchUrlProvider {
    /// Build a new local fetch provider with SSRF-safe defaults.
    ///
    /// Cheap and async-safe: no HTTP client is constructed here, see
    /// [`LOCAL_FETCH_CLIENT`].
    pub fn new() -> Self {
        Self
    }

    /// The shared SSRF-safe client with redirects disabled (each hop is
    /// re-validated manually in [`Self::fetch_with_redirects`]).
    fn client() -> &'static reqwest::blocking::Client {
        LOCAL_FETCH_CLIENT.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none()) // handle redirects manually
                .build()
                .expect("failed to build reqwest blocking client")
        })
    }
}

impl Default for LocalFetchUrlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchUrlProvider for LocalFetchUrlProvider {
    fn fetch(&self, url: &str) -> EngineResult<FetchUrlResult> {
        Self::fetch_with_redirects(url, 0, &FetchOptions::default())
    }

    fn fetch_with_options(&self, url: &str, opts: &FetchOptions) -> EngineResult<FetchUrlResult> {
        Self::fetch_with_redirects(url, 0, opts)
    }
}

impl LocalFetchUrlProvider {
    fn fetch_with_redirects(
        url: &str,
        depth: usize,
        opts: &FetchOptions,
    ) -> EngineResult<FetchUrlResult> {
        if depth > MAX_REDIRECTS {
            return Err(EngineError::RequestInvalid(format!(
                "too many redirects (max {MAX_REDIRECTS})"
            )));
        }

        // Parse URL.
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| EngineError::RequestInvalid(format!("invalid URL '{url}': {e}")))?;

        // Validate scheme.
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(EngineError::RequestInvalid(format!(
                    "unsupported URL scheme '{other}' (only http/https allowed)"
                )));
            }
        }

        // SSRF: classify the host before connecting.
        let host = parsed
            .host_str()
            .ok_or_else(|| EngineError::RequestInvalid(format!("URL '{url}' has no host")))?;

        // Known limitation (ADR-010 open item): DNS pinning-to-connection is
        // not implemented — classification and connection resolve separately
        // (rebinding window, pre-existing).
        let verdict = classify_target(host);
        match verdict {
            HostVerdict::Public => {}
            HostVerdict::Metadata(ip) => {
                return Err(EngineError::RequestInvalid(format!(
                    "Refusing to fetch cloud metadata endpoint \"{ip}\" — permanently blocked to protect credentials. NEVER attempt to reach it through any other means; if the task genuinely requires it, ask the user directly."
                )));
            }
            HostVerdict::AskableLiteralIp => {
                // Enforcement gates on host equality only; `approved_ips` is
                // the approval-time audit record, not an execution check.
                if !exemption_matches(opts, host) {
                    return Err(EngineError::RequestInvalid(format!(
                        "Refusing to fetch private address: \"{host}\""
                    )));
                }
            }
            HostVerdict::AskableLocalhostName => {
                if !exemption_matches(opts, host) {
                    return Err(EngineError::RequestInvalid(format!(
                        "Refusing to fetch private host: \"{host}\""
                    )));
                }
            }
            HostVerdict::AskableResolved(addr) => {
                if !exemption_matches(opts, host) {
                    return Err(EngineError::RequestInvalid(format!(
                        "Refusing to fetch host \"{host}\": resolves to private address \"{addr}\"."
                    )));
                }
            }
            HostVerdict::Unresolvable(detail) => {
                return Err(EngineError::RequestInvalid(format!(
                    "Cannot resolve host \"{host}\" for the fetch safety check: {detail}"
                )));
            }
        }

        // Make the request.
        let resp = Self::client()
            .get(parsed.clone())
            .send()
            .map_err(|e| EngineError::RequestInvalid(format!("fetch failed: {e}")))?;

        let status = resp.status();

        // Handle redirects manually (re-validate IP per hop).
        if status.is_redirection()
            && let Some(loc) = resp.headers().get(reqwest::header::LOCATION)
        {
            let loc_str = loc.to_str().map_err(|e| {
                EngineError::RequestInvalid(format!("invalid redirect location: {e}"))
            })?;
            // Resolve relative redirects.
            let next_url = parsed
                .join(loc_str)
                .map_err(|e| EngineError::RequestInvalid(format!("invalid redirect URL: {e}")))?;
            let next_host = next_url
                .host_str()
                .ok_or_else(|| {
                    EngineError::RequestInvalid(format!(
                        "redirect URL '{}' has no host",
                        next_url.as_str()
                    ))
                })?;

            // Re-classify the redirect target before following it.
            // Known limitation (ADR-010 open item): DNS pinning-to-connection
            // is not implemented — classification and connection resolve
            // separately (rebinding window, pre-existing).
            let next_verdict = classify_target(next_host);
            match &next_verdict {
                HostVerdict::Public => {}
                HostVerdict::Metadata(ip) => {
                    return Err(EngineError::RequestInvalid(format!(
                        "Refusing to fetch cloud metadata endpoint \"{ip}\" — permanently blocked to protect credentials. NEVER attempt to reach it through any other means; if the task genuinely requires it, ask the user directly."
                    )));
                }
                HostVerdict::AskableLiteralIp
                | HostVerdict::AskableLocalhostName
                | HostVerdict::AskableResolved(_) => {
                    // Same-host drift is covered by the approval; a new
                    // private target is not — ask the user to approve the
                    // final URL directly.
                    if !exemption_matches(opts, next_host) {
                        let address = match next_verdict {
                            HostVerdict::AskableResolved(addr) => addr.to_string(),
                            _ => normalize_host(next_host),
                        };
                        let redirect_chain = format!("{url} → {}", next_url.as_str());
                        return Err(EngineError::RequestInvalid(format!(
                            "Redirect to private address \"{address}\" is not covered by the current approval. Fetch the final URL directly so the user can approve it: {redirect_chain}"
                        )));
                    }
                }
                HostVerdict::Unresolvable(detail) => {
                    return Err(EngineError::RequestInvalid(format!(
                        "Cannot resolve host \"{next_host}\" for the fetch safety check: {detail}"
                    )));
                }
            }
            return Self::fetch_with_redirects(next_url.as_str(), depth + 1, opts);
        }

        // Reject non-2xx.
        if !status.is_success() {
            return Err(EngineError::RequestInvalid(format!(
                "HTTP {status} for {url}"
            )));
        }

        // Check content-length before reading.
        if let Some(len) = resp.content_length()
            && (len as usize) > MAX_RESPONSE_BYTES
        {
            return Err(EngineError::RequestInvalid(format!(
                "response too large ({len} bytes, max {MAX_RESPONSE_BYTES})"
            )));
        }

        // Read body with size cap.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        // Read up to MAX_RESPONSE_BYTES.
        let mut body = Vec::new();
        let mut reader = resp;
        let mut buf = [0u8; 8192];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| EngineError::RequestInvalid(format!("read error: {e}")))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(EngineError::RequestInvalid(format!(
                    "response body exceeded {MAX_RESPONSE_BYTES} bytes"
                )));
            }
        }

        let body_str = String::from_utf8_lossy(&body).to_string();

        // Route by content type.
        let (content, kind) = if content_type.contains("text/html") {
            (strip_html_tags(&body_str), FetchContentKind::Extracted)
        } else {
            (body_str, FetchContentKind::Passthrough)
        };

        Ok(FetchUrlResult { content, kind })
    }
}

/// Strip HTML tags and extract text content.
///
/// Simple tag removal — removes `<script>`/`<style>` blocks entirely,
/// then strips remaining tags, decodes common entities, and collapses
/// whitespace.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    // First, remove script and style blocks.
    let html: String = {
        let mut cleaned = String::with_capacity(html.len());
        let mut rest = html;
        while !rest.is_empty() {
            let skip = if starts_with_ascii_ci(rest, "<script") {
                find_ascii_ci(rest, "</script>").map(|end| end + "</script>".len())
            } else if starts_with_ascii_ci(rest, "<style") {
                find_ascii_ci(rest, "</style>").map(|end| end + "</style>".len())
            } else {
                // Advance one character at a time so `rest` always starts on
                // a char boundary; the helpers below only ever return offsets
                // that land on an ASCII byte, which is always a boundary too.
                let ch = rest.chars().next().unwrap();
                cleaned.push(ch);
                Some(ch.len_utf8())
            };
            match skip {
                // Skip past the whole block; a missing closing tag drops the
                // rest of the document, same as the old byte-scanner did.
                Some(n) => rest = &rest[n..],
                None => break,
            }
        }
        cleaned
    };

    // Strip remaining tags.
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                result.push(' ');
            }
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    // Decode common HTML entities.
    let result = result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Collapse whitespace.
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_ws = false;
    for ch in result.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
                prev_ws = true;
            }
        } else {
            collapsed.push(ch);
            prev_ws = false;
        }
    }

    collapsed.trim().to_string()
}

/// ASCII case-insensitive `starts_with`.
fn starts_with_ascii_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()]
            .iter()
            .zip(prefix.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// ASCII case-insensitive `find` — returns a byte offset into `haystack`.
///
/// The needle must be pure ASCII, so every returned offset lands on the
/// starting byte of an ASCII character, which is always a valid char
/// boundary in UTF-8.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return Some(0);
    }
    if h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

// ---------------------------------------------------------------------------
// ManagedFetchUrlProvider
// ---------------------------------------------------------------------------

/// Configuration for a managed Mirri fetch service.
#[derive(Clone)]
pub struct ManagedFetchConfig {
    /// Base URL of the fetch service (e.g. `https://search.example.com`).
    pub base_url: String,
    /// Bearer token for authentication.
    pub api_key: String,
}

/// Fetch provider that delegates to a Mirri managed service, falling back
/// to [`LocalFetchUrlProvider`] on any error.
pub struct ManagedFetchUrlProvider {
    config: ManagedFetchConfig,
    local_fallback: Arc<LocalFetchUrlProvider>,
}

/// Process-wide client for [`ManagedFetchUrlProvider`] — same async-context
/// constraints as [`LOCAL_FETCH_CLIENT`]; redirects are followed by the
/// client itself (limited to [`MAX_REDIRECTS`]).
static MANAGED_FETCH_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

impl ManagedFetchUrlProvider {
    /// Build a managed fetch provider with a local fallback.
    ///
    /// Cheap and async-safe: no HTTP client is constructed here, see
    /// [`MANAGED_FETCH_CLIENT`].
    pub fn new(config: ManagedFetchConfig) -> Self {
        Self {
            config,
            local_fallback: Arc::new(LocalFetchUrlProvider::new()),
        }
    }

    fn client() -> &'static reqwest::blocking::Client {
        MANAGED_FETCH_CLIENT.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .build()
                .expect("failed to build reqwest client")
        })
    }
}

impl FetchUrlProvider for ManagedFetchUrlProvider {
    fn fetch(&self, url: &str) -> EngineResult<FetchUrlResult> {
        self.fetch_with_options(url, &FetchOptions::default())
    }

    fn fetch_with_options(&self, url: &str, opts: &FetchOptions) -> EngineResult<FetchUrlResult> {
        // Private-network targets never leave the machine: only host
        // **literals** are gated here — hostnames that need DNS are delegated
        // to the managed service (ADR-010 decision 8; the residual risk is
        // recorded in the spec).
        if let Some(host) = host_of_url(url)
            && let Some(NetClass::Metadata | NetClass::Askable) = classify_host_literal(&host)
        {
            return self.local_fallback.fetch_with_options(url, opts);
        }

        // Try the managed service first.
        let endpoint = format!("{}/fetch", self.config.base_url.trim_end_matches('/'));

        let resp = Self::client()
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Accept", "text/markdown")
            .json(&serde_json::json!({ "url": url }))
            .send();

        match resp {
            Ok(r) if r.status().is_success() => {
                let body = r
                    .text()
                    .map_err(|e| EngineError::RequestInvalid(format!("read body: {e}")))?;
                Ok(FetchUrlResult {
                    content: body,
                    kind: FetchContentKind::Extracted,
                })
            }
            _ => {
                // Fallback to local on any error.
                self.local_fallback.fetch_with_options(url, opts)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ManagedWebSearchProvider
// ---------------------------------------------------------------------------

/// Configuration for a managed Mirri web search service.
#[derive(Clone)]
pub struct ManagedSearchConfig {
    /// Base URL of the search service.
    pub base_url: String,
    /// Bearer token for authentication.
    pub api_key: String,
}

/// Web search provider backed by a Mirri managed search service.
pub struct ManagedWebSearchProvider {
    config: ManagedSearchConfig,
}

/// Process-wide client for [`ManagedWebSearchProvider`] — same async-context
/// constraints as [`LOCAL_FETCH_CLIENT`].
static MANAGED_SEARCH_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

impl ManagedWebSearchProvider {
    /// Build a managed search provider.
    ///
    /// Cheap and async-safe: no HTTP client is constructed here, see
    /// [`MANAGED_SEARCH_CLIENT`].
    pub fn new(config: ManagedSearchConfig) -> Self {
        Self { config }
    }

    fn client() -> &'static reqwest::blocking::Client {
        MANAGED_SEARCH_CLIENT.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("failed to build reqwest client")
        })
    }
}

/// JSON response shape from the search service.
#[derive(serde::Deserialize)]
struct SearchResponse {
    #[serde(default)]
    search_results: Vec<SearchItem>,
}

#[derive(serde::Deserialize)]
struct SearchItem {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

impl WebSearchProvider for ManagedWebSearchProvider {
    fn search(&self, query: &str) -> EngineResult<Vec<WebSearchResult>> {
        let endpoint = format!("{}/search", self.config.base_url.trim_end_matches('/'));

        let resp = Self::client()
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&serde_json::json!({ "text_query": query }))
            .send()
            .map_err(|e| EngineError::RequestInvalid(format!("web search request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(EngineError::RequestInvalid(format!(
                "web search returned HTTP {}",
                resp.status()
            )));
        }

        let parsed: SearchResponse = resp.json().map_err(|e| {
            EngineError::RequestInvalid(format!("failed to parse search response: {e}"))
        })?;

        Ok(parsed
            .search_results
            .into_iter()
            .map(|item| WebSearchResult {
                title: item.title,
                url: item.url,
                snippet: item.snippet,
                site_name: item.site_name,
                date: item.date,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mirri_core::tools::web_providers::PrivateNetworkExemption;

    use super::*;
    fn opts_for(host: &str, ips: &[&str]) -> FetchOptions {
        FetchOptions {
            private_network_exemption: Some(PrivateNetworkExemption {
                host: host.to_string(),
                approved_ips: ips.iter().map(|s| s.parse().unwrap()).collect(),
            }),
        }
    }

    /// should refuse private fetches when no exemption is present
    #[test]
    fn should_refuse_when_private_fetch_without_exemption() {
        let provider = LocalFetchUrlProvider::new();
        let result = provider.fetch_with_options("http://10.0.0.1/x", &FetchOptions::default());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("Refusing to fetch private address"), "got: {err}");
    }

    /// should connect (and fail at the network layer) when the exemption matches the host
    #[test]
    fn should_allow_when_private_fetch_with_matching_exemption() {
        let provider = LocalFetchUrlProvider::new();
        // Nothing listens on port 9, so a connection attempt that passes the
        // gate fails immediately at the network layer — a network error, not
        // a refusal.
        let result = provider.fetch_with_options("http://127.0.0.1:9/x", &opts_for("127.0.0.1", &["127.0.0.1"]));
        let err = result.err().unwrap().to_string().to_lowercase();
        assert!(
            !err.contains("refusing to fetch"),
            "expected a network error, got refusal: {err}"
        );
    }

    /// should refuse the metadata endpoint even with a matching exemption
    #[test]
    fn should_refuse_when_metadata_endpoint_with_exemption() {
        let provider = LocalFetchUrlProvider::new();
        let opts = opts_for("169.254.169.254", &["169.254.169.254"]);
        let result = provider.fetch_with_options(
            "http://169.254.169.254/latest/meta-data/",
            &opts,
        );
        let err = result.err().unwrap().to_string();
        assert!(err.contains("cloud metadata endpoint"), "got: {err}");
        assert!(err.contains("NEVER"), "got: {err}");
    }

    /// should refuse an IPv4-mapped IPv6 metadata literal (historical bypass)
    #[test]
    fn should_refuse_when_ipv4_mapped_metadata_attempt() {
        let provider = LocalFetchUrlProvider::new();
        let result = provider.fetch("http://[::ffff:169.254.169.254]/x");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("cloud metadata endpoint"), "got: {err}");
    }

    /// should distinguish error wording between IP literal, localhost name, and unresolvable host
    #[test]
    fn should_distinguish_errors_when_private_ip_literal_vs_hostname() {
        let provider = LocalFetchUrlProvider::new();

        let err = provider
            .fetch("http://10.0.0.1/x")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Refusing to fetch private address"), "got: {err}");

        let err = provider
            .fetch("http://localhost/x")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Refusing to fetch private host"), "got: {err}");

        let err = provider
            .fetch("http://no-such-host.invalid/x")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Cannot resolve host"), "got: {err}");
    }

    /// should refuse a redirect that lands on a different private target
    #[test]
    fn should_refuse_when_redirect_to_new_private_target() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2/x\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });

        let provider = LocalFetchUrlProvider::new();
        let opts = opts_for("127.0.0.1", &["127.0.0.1"]);
        let result = provider.fetch_with_options(&format!("http://127.0.0.1:{port}/start"), &opts);
        let err = result.err().unwrap().to_string();
        assert!(err.contains("Redirect to private address"), "got: {err}");
        assert!(err.contains("127.0.0.2"), "got: {err}");
    }

    /// should follow a redirect that stays on the approved host
    #[test]
    fn should_follow_redirect_when_same_host_under_exemption() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/other\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\n\r\nbody",
                );
            }
        });

        let provider = LocalFetchUrlProvider::new();
        let opts = opts_for("127.0.0.1", &["127.0.0.1"]);
        let result = provider.fetch_with_options(&format!("http://127.0.0.1:{port}/start"), &opts);
        let fetched = result.expect("same-host redirect under exemption should succeed");
        assert!(fetched.content.contains("body"), "got: {}", fetched.content);
    }

    /// should route private literals locally and public literals to the managed service
    #[test]
    fn should_route_local_when_managed_private_target() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_server = hits.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut conn, _)) = listener.accept() {
                hits_for_server.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
            }
        });

        let provider = ManagedFetchUrlProvider::new(ManagedFetchConfig {
            base_url: format!("http://127.0.0.1:{base_port}"),
            api_key: "unused".to_string(),
        });

        // Private literal: never sent to the managed service — refused by the
        // local fallback (no exemption) without a cloud POST.
        let result = provider.fetch("http://10.0.0.1/x");
        assert!(result.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        // Public literal: sent to the managed service (which answers 500);
        // the local fallback then tries to fetch 8.8.8.8 for real — the
        // outcome of that second hop is irrelevant here, only the count.
        let _ = provider.fetch("http://8.8.8.8/x");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// should reject non-http schemes
    #[test]
    fn should_reject_non_http_schemes() {
        let provider = LocalFetchUrlProvider::new();
        let result = provider.fetch("file:///etc/passwd");
        assert!(result.is_err());
    }

    /// should strip HTML tags and extract text content
    #[test]
    fn should_strip_html_tags() {
        let html = r#"<html><head><style>body{}</style></head><body><script>alert(1)</script><h1>Title</h1><p>Paragraph &amp; more</p></body></html>"#;
        let text = strip_html_tags(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Paragraph & more"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("body{}"));
        assert!(!text.contains("<"));
    }

    /// should not panic when non-ASCII bytes appear before a script block
    ///
    /// Regression: the scanner used to advance `i` one byte at a time from a
    /// multi-byte character's middle, and slicing `lower[i..]` panicked with
    /// "byte index is not a char boundary" (took down the whole process in
    /// release builds, where panic=abort).
    #[test]
    fn should_survive_multibyte_chars_before_script_blocks() {
        let html = "<p>🌙Subscribe 🎉</p><script>alert(1)</script><p>tail</p>";
        let text = strip_html_tags(html);
        assert!(text.contains("🌙Subscribe 🎉"));
        assert!(text.contains("tail"));
        assert!(!text.contains("alert"));
    }

    /// should not panic when a char's lowercase form changes byte length
    ///
    /// Regression: the scanner used to keep a `to_lowercase()` copy aligned
    /// with the original by byte offset; chars like 'İ' (2 bytes) lowercase
    /// to 3 bytes, desyncing the two slices and panicking on a non-char
    /// boundary again.
    #[test]
    fn should_survive_length_changing_lowercase_chars() {
        let html = "<p>İstanbul ẞtraße</p><script>alert(1)</script><p>tail</p>";
        let text = strip_html_tags(html);
        assert!(text.contains("İstanbul ẞtraße"));
        assert!(text.contains("tail"));
        assert!(!text.contains("alert"));
    }

    /// should preserve non-ASCII text as-is (no mojibake)
    ///
    /// Regression: the scanner used to push each raw byte as a `char`,
    /// turning every multi-byte character into Latin-1 style mojibake even
    /// when it didn't panic.
    #[test]
    fn should_preserve_multibyte_text() {
        let html = "<p>こんにちは世界 🌙</p>";
        let text = strip_html_tags(html);
        assert_eq!(text, "こんにちは世界 🌙");
    }

    /// should drop everything after an unclosed script tag
    #[test]
    fn should_drop_remainder_after_unclosed_script_tag() {
        let html = "<p>visible</p><script>evil payload without closing tag";
        let text = strip_html_tags(html);
        assert!(text.contains("visible"));
        assert!(!text.contains("evil"));
    }

    /// should collapse whitespace in stripped HTML
    #[test]
    fn should_collapse_whitespace() {
        let html = "<p>one\n\n\ntwo   three</p>";
        let text = strip_html_tags(html);
        assert_eq!(text, "one two three");
    }

    /// should reject invalid URLs
    #[test]
    fn should_reject_invalid_urls() {
        let provider = LocalFetchUrlProvider::new();
        let result = provider.fetch("not-a-url");
        assert!(result.is_err());
    }

    /// should construct and drop every provider inside a Tokio runtime
    ///
    /// Providers are wired from async entry points (the ACP binary and
    /// `EmbeddedServer::for_workspace_root`); eager blocking-client
    /// construction used to panic here with "Cannot drop a runtime in a
    /// context where blocking is not allowed".
    #[tokio::test]
    async fn should_construct_and_drop_every_provider_inside_a_tokio_runtime() {
        // Bindings (not `drop(...)`) so each value is destroyed at the end of
        // this async scope — exactly the path that used to panic.
        let _local = LocalFetchUrlProvider::new();
        let _managed_fetch = ManagedFetchUrlProvider::new(ManagedFetchConfig {
            base_url: "https://example.com".to_string(),
            api_key: "unused".to_string(),
        });
        let _managed_search = ManagedWebSearchProvider::new(ManagedSearchConfig {
            base_url: "https://example.com".to_string(),
            api_key: "unused".to_string(),
        });
    }
}
