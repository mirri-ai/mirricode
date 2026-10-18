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

/// Provider for fetching URL content.
///
/// Implemented by the host layer. A local fallback (no auth, basic GET +
/// simple extraction) can be used when no managed provider is configured.
///
/// # Security
///
/// Implementations MUST enforce the following protections:
///
/// - Refuse private/loopback addresses (`127.0.0.0/8`, `10.0.0.0/8`,
///   `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` including the
///   cloud-metadata endpoint `169.254.169.254`).
/// - Refuse `localhost` and IPv6 loopback (`::1`) / ULA (`fc00::/7`).
/// - Re-validate the resolved IP after any HTTP redirect to prevent
///   DNS-rebinding.
/// - Enforce a maximum response size (recommend ≤ 10 MB).
/// - Reject non-2xx HTTP status codes with a clear error message.
///
/// See the TypeScript `LocalFetchURLProvider` for a reference implementation.
pub trait FetchUrlProvider: Send + Sync {
    /// Fetch the content at the given URL.
    fn fetch(&self, url: &str) -> EngineResult<FetchUrlResult>;
}
