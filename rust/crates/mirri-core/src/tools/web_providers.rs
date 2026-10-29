//! Provider traits for WebSearch and FetchURL tools.
//!
//! These traits abstract the HTTP backends behind the `WebSearch` and
//! `FetchURL` tools. The host layer injects concrete implementations (or
//! `None` for headless/test mode) via [`BuiltinToolServices`].
//!
//! ## Design
//!
//! The tools are thin shells: they validate input, call the provider, and
//! format the result. All network logic lives in the provider implementation.
//!
//! - `WebSearchProvider` — conditional registration (only when `Some`).
//! - `FetchUrlProvider` — always registered when `Some`; the host can inject
//!   a local fallback provider that needs no auth.

use crate::error::EngineResult;

/// A single web search result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WebSearchResult {
    /// Result title.
    pub title: String,
    /// Result URL.
    pub url: String,
    /// Short snippet/summary.
    pub snippet: String,
    /// Optional site name.
    pub site_name: Option<String>,
    /// Optional publication date.
    pub date: Option<String>,
}

/// Provider for web search queries.
///
/// Implemented by the host layer (e.g. Tavily, Serper, or Moonshot/Kimi
/// search API). Injected into `BuiltinToolServices` as
/// `Option<Arc<dyn WebSearchProvider>>`.
pub trait WebSearchProvider: Send + Sync {
    /// Execute a search query and return results.
    fn search(&self, query: &str) -> EngineResult<Vec<WebSearchResult>>;
}

/// The kind of content returned by a fetch.
#[derive(Debug, Clone)]
pub enum FetchContentKind {
    /// The full response body, returned verbatim (e.g. text/plain, text/markdown).
    Passthrough,
    /// Main text extracted from the page (e.g. HTML → readability extraction).
    Extracted,
}

/// Result of fetching a URL.
#[derive(Debug, Clone)]
pub struct FetchUrlResult {
    /// The page content.
    pub content: String,
    /// Whether the content is verbatim or extracted.
    pub kind: FetchContentKind,
}

/// Exemption granted by an approved private-network fetch (ADR-010).
#[derive(Debug, Clone)]
pub struct PrivateNetworkExemption {
    /// Host the approval covered (normalized, no port).
    pub host: String,
    /// IPs the approval covered — the audit record of what the user
    /// approved. Enforcement gates on host equality; this set records
    /// the approval-time resolution (address drift on the same host is
    /// covered, new private targets are not).
    pub approved_ips: Vec<std::net::IpAddr>,
}

/// Request options for a fetch.
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub private_network_exemption: Option<PrivateNetworkExemption>,
}

/// Provider for fetching URL content.
///
/// Implemented by the host layer. A local fallback (no auth, basic GET +
/// simple extraction) can be used when no managed provider is configured.
///
/// # Security
///
/// Implementations MUST enforce the following protections:
///
/// - Refuse private/loopback/link-local/CGNAT/ULA addresses and hosts
///   (`localhost`, `*.localhost`) **unless the request carries a matching
///   `FetchOptions::private_network_exemption`** (ADR-010: the exemption
///   covers the approved host and its address drift, never new private
///   targets).
/// - Refuse cloud metadata endpoints (`169.254.169.254`, `fd00:ec2::254`)
///   unconditionally — no approval path exists.
/// - Re-validate the target after any HTTP redirect to prevent
///   DNS-rebinding.
/// - Enforce a maximum response size (recommend ≤ 10 MB).
/// - Reject non-2xx HTTP status codes with a clear error message.
///
/// See the TypeScript `LocalFetchURLProvider` for a reference implementation.
pub trait FetchUrlProvider: Send + Sync {
    /// Fetch the content at the given URL.
    fn fetch(&self, url: &str) -> EngineResult<FetchUrlResult>;
    /// Fetch with request options. Default: ignore options (providers
    /// without private-network enforcement delegate to `fetch`).
    fn fetch_with_options(&self, url: &str, opts: &FetchOptions) -> EngineResult<FetchUrlResult> {
        let _ = opts;
        self.fetch(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock provider that counts how many times each entry point ran.
    struct CountingProvider {
        result: FetchUrlResult,
    }

    impl FetchUrlProvider for CountingProvider {
        fn fetch(&self, _url: &str) -> EngineResult<FetchUrlResult> {
            Ok(self.result.clone())
        }
    }

    #[test]
    fn should_delegate_to_fetch_when_default_fetch_with_options_called() {
        let provider = CountingProvider {
            result: FetchUrlResult {
                content: "body".into(),
                kind: FetchContentKind::Passthrough,
            },
        };
        let opts = FetchOptions {
            private_network_exemption: Some(PrivateNetworkExemption {
                host: "example.com".into(),
                approved_ips: vec!["10.0.0.5".parse().unwrap()],
            }),
        };
        let via_default = provider
            .fetch_with_options("https://example.com/x", &opts)
            .expect("fetch_with_options");
        let direct = provider.fetch("https://example.com/x").expect("fetch");
        assert_eq!(via_default.content, direct.content);
    }
}
