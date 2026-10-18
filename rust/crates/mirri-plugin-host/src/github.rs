//! GitHub ref resolution for update checks — the Rust mirror of
//! `agent-core-v2/src/app/plugin/github-resolver.ts`.
//!
//! Movable refs resolve to commit SHAs through GitHub's Atom feed (no REST
//! API, no auth); unpinned sources resolve through the `releases/latest`
//! redirect with a codeload `zip/HEAD` probe as fallback. All requests
//! carry a 10-second timeout, matching the TS reference.
//!
//! The [`GithubResolver`] trait is the injection seam: the real
//! [`ReqwestGithubResolver`] is the default, and tests inject stubs so the
//! gate never touches the network.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Request timeout for every GitHub resolution call (TS: 10s).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The kind of a GitHub ref (`pluginGithubRefSchema` in klient).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubRefKind {
    /// A movable branch ref (including the symbolic `HEAD`).
    Branch,
    /// An immutable release tag.
    Tag,
    /// An immutable commit SHA.
    Sha,
}

/// A resolved GitHub ref — `{ kind, value }` on the wire, field-for-field
/// the klient `pluginGithubRefSchema`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GithubRef {
    /// Whether the ref is a branch, tag, or commit SHA.
    pub kind: GithubRefKind,
    /// The ref name (`"main"`, `"v1.2.0"`, a 40-hex SHA, or `"HEAD"`).
    pub value: String,
}

impl GithubRef {
    /// Build a branch ref.
    pub fn branch(value: impl Into<String>) -> Self {
        Self {
            kind: GithubRefKind::Branch,
            value: value.into(),
        }
    }

    /// Build a tag ref.
    pub fn tag(value: impl Into<String>) -> Self {
        Self {
            kind: GithubRefKind::Tag,
            value: value.into(),
        }
    }

    /// Build a SHA ref.
    pub fn sha(value: impl Into<String>) -> Self {
        Self {
            kind: GithubRefKind::Sha,
            value: value.into(),
        }
    }
}

/// A boxed resolver future — the trait's async surface without an
/// `async_trait` dependency.
pub type ResolverFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// The GitHub network seam: SHA and latest-ref resolution, injectable for
/// tests (the real implementation is [`ReqwestGithubResolver`]).
///
/// Errors are strings (not [`crate::PluginError`]) because a failed
/// resolution is per-plugin skippable, not an engine failure — the TS
/// reference catches and drops the record.
pub trait GithubResolver: Send + Sync {
    /// Resolve a movable ref (branch or tag name) to its 40-hex commit SHA
    /// through the repo's Atom feed.
    fn resolve_commit_sha<'a>(
        &'a self,
        repo: &'a str,
        ref_name: &'a str,
    ) -> ResolverFuture<'a, String>;

    /// Resolve the repo's "latest" source ref for an unpinned install:
    /// the latest release tag when one exists, else `HEAD` on the default
    /// branch. Returns the ref plus its display version (the tag name, or
    /// `"HEAD"`).
    fn resolve_latest_ref<'a>(&'a self, repo: &'a str) -> ResolverFuture<'a, (GithubRef, String)>;
}

/// The real GitHub resolver — `reqwest` against the same endpoints the TS
/// reference uses, all with 10s timeouts.
#[derive(Debug, Clone)]
pub struct ReqwestGithubResolver {
    client: reqwest::Client,
}

impl ReqwestGithubResolver {
    /// Build a resolver with redirects disabled (the `releases/latest`
    /// lookup reads the 301/302 `Location` header itself, mirroring the
    /// TS `redirect: 'manual'` fetch).
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(RESOLVE_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client with no system proxies always builds"),
        }
    }
}

impl Default for ReqwestGithubResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubResolver for ReqwestGithubResolver {
    fn resolve_commit_sha<'a>(
        &'a self,
        repo: &'a str,
        ref_name: &'a str,
    ) -> ResolverFuture<'a, String> {
        Box::pin(async move {
            let url = format!("https://github.com/{repo}/commits/{ref_name}.atom");
            let resp = self
                .client
                .get(&url)
                .header("accept", "application/atom+xml")
                .send()
                .await
                .map_err(|e| format!("could not resolve {repo}@{ref_name}: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "could not resolve {repo}@{ref_name}: HTTP {} {}",
                    resp.status().as_u16(),
                    resp.status()
                ));
            }
            let feed = resp
                .text()
                .await
                .map_err(|e| format!("could not resolve {repo}@{ref_name}: {e}"))?;
            let sha = find_commit_sha(&feed)
                .ok_or_else(|| format!("could not resolve {repo}@{ref_name} to a commit SHA"))?;
            Ok(sha.to_lowercase())
        })
    }

    fn resolve_latest_ref<'a>(&'a self, repo: &'a str) -> ResolverFuture<'a, (GithubRef, String)> {
        Box::pin(async move {
            if let Some(tag) = self.try_latest_release_tag(repo).await? {
                return Ok((GithubRef::tag(tag.clone()), tag));
            }
            // HEAD fallback: probe the codeload tarball endpoint.
            let url = format!("https://codeload.github.com/{repo}/zip/HEAD");
            let resp = self
                .client
                .head(&url)
                .send()
                .await
                .map_err(|e| format!("could not access {repo} at HEAD: {e}"))?;
            let status = resp.status();
            if status.as_u16() == 404 {
                return Err(format!("repository `{repo}` not found or not accessible"));
            }
            if !status.is_success() {
                return Err(format!(
                    "could not access `{repo}`: HTTP {} {}",
                    status.as_u16(),
                    status
                ));
            }
            Ok((GithubRef::branch("HEAD"), "HEAD".to_string()))
        })
    }
}

impl ReqwestGithubResolver {
    /// The latest release tag via the `releases/latest` redirect, or `None`
    /// when the repo has no releases (404) or the redirect carries no
    /// parseable tag. Mirrors the TS `tryResolveLatestReleaseTag`.
    async fn try_latest_release_tag(&self, repo: &str) -> Result<Option<String>, String> {
        let url = format!("https://github.com/{repo}/releases/latest");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("could not look up latest release of `{repo}`: {e}"))?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if status != 301 && status != 302 {
            return Err(format!(
                "could not look up latest release of `{repo}`: HTTP {status} ({url})"
            ));
        }
        let Some(location) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
        else {
            return Ok(None);
        };
        Ok(parse_release_tag(&location))
    }
}

/// Extract the newest commit SHA from an Atom feed body — the first
/// `Grit::Commit/<40-hex>` occurrence (case-insensitive, like the TS regex).
fn find_commit_sha(feed: &str) -> Option<String> {
    static SHA_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = SHA_RE.get_or_init(|| {
        regex::Regex::new(r"Grit::Commit/([0-9a-fA-F]{40})").expect("valid literal regex")
    });
    re.captures(feed)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Parse `…/releases/tag/<tag>` out of a redirect `Location` header value.
fn parse_release_tag(location: &str) -> Option<String> {
    let tag = location.split("/releases/tag/").nth(1)?;
    let tag = tag.split(['?', '#']).next()?;
    let decoded = percent_encoding_decode(tag);
    Some(decoded.unwrap_or_else(|| tag.to_string()))
}

/// Minimal percent-decoding for the tag name (the TS uses
/// `decodeURIComponent` with a fallthrough to the raw value). Returns
/// `None` on malformed escapes — the caller falls back to the raw input,
/// mirroring the TS try/catch fallback.
pub(crate) fn percent_encoding_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = |b: u8| (b as char).to_digit(16);
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a ref string is a full 40-hex commit SHA.
pub(crate) fn is_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_extract_sha_when_feed_contains_commit_entry() {
        let feed = r#"<feed><entry><id>tag:github.com,2008:Grit::Commit/0123456789abcdef0123456789abcdef01234567</id></entry></feed>"#;
        assert_eq!(
            find_commit_sha(feed).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn should_return_none_when_feed_has_no_commit_entry() {
        assert_eq!(find_commit_sha("<feed></feed>"), None);
    }

    #[test]
    fn should_extract_tag_when_location_points_at_release() {
        assert_eq!(
            parse_release_tag("https://github.com/user/repo/releases/tag/v1.2.0").as_deref(),
            Some("v1.2.0")
        );
        // No tag segment → None (the TS regex miss).
        assert_eq!(parse_release_tag("https://github.com/user/repo"), None);
    }

    #[test]
    fn should_decode_percent_escapes_when_tag_contains_them() {
        assert_eq!(
            parse_release_tag("https://github.com/u/r/releases/tag/v1%2E0").as_deref(),
            Some("v1.0")
        );
    }

    #[test]
    fn should_classify_sha_when_ref_is_40_hex() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("main"));
        assert!(!is_full_sha("0123456789abcdef0123456789abcdef0123456g"));
    }
}
