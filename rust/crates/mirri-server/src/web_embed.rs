//! Compile-time embedded web SPA serving.
//!
//! The full distribution build compiles `mirri-server` with the
//! `embedded-web` feature, which materializes the staged web SPA
//! (`web-dist/`, populated from `apps/mirri-web/dist` by
//! `rust/scripts/stage-web.mjs`) into the binary via `rust-embed`. The
//! standalone `mirri` executable then serves the web UI with no external
//! files at all — the lightweight headless build simply compiles this
//! module away.
//!
//! Serving semantics mirror the `ServeDir` fallback used for an explicit
//! `--web-dir`: exact asset hits are served as-is, extension-less misses
//! fall back to `index.html` (SPA history mode), and misses that carry a
//! file extension 404 (a broken asset URL must not be masked by the app
//! shell). Unmatched `/api/` routes 404 too — clients distinguish a missing
//! API route from the SPA shell, so the history-mode fallback is reserved
//! for browser routes.

use axum::Router;
#[cfg(feature = "embedded-web")]
use rust_embed::RustEmbed;

#[cfg(feature = "embedded-web")]
use std::borrow::Cow;

/// Content types for the file extensions the staged SPA emits. Unknown
/// extensions default to `application/octet-stream`, which browsers
/// download rather than execute — the safe default for a static face.
#[cfg(feature = "embedded-web")]
fn content_type_for(path: &str) -> &'static str {
    let extension = path.rsplit('.').next().unwrap_or_default();
    match extension {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// The embedded file that answers a request path, and the path to serve it
/// from. Splitting the decision from the asset lookup keeps it testable
/// without a staged `web-dist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedWebResolution {
    /// The request maps to an embedded asset (after SPA fallback).
    Serve { path: String },
    /// The request cannot be answered: an extension-bearing miss (broken
    /// asset URL) or an SPA-less build.
    NotFound,
}

/// Decide which embedded file answers `request_path`.
///
/// - a path with no file extension misses the asset table → `index.html`
///   (SPA history mode: client-side routes like `/session/42` must render
///   the app shell);
/// - a path with an extension misses the asset table → `NotFound` (a
///   missing `assets/app-abc.js` must fail loudly, not return HTML);
/// - an `/api/` path that missed the registered routes → `NotFound` (the
///   web client and scripts distinguish a missing API route from the SPA
///   shell).
pub fn resolve_embedded_path(
    lookup: &dyn Fn(&str) -> bool,
    request_path: &str,
) -> EmbeddedWebResolution {
    let path = request_path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if lookup(path) {
        return EmbeddedWebResolution::Serve {
            path: path.to_owned(),
        };
    }

    let is_api_route = path.starts_with("api/");
    let has_extension = path
        .rsplit_once('.')
        .is_some_and(|(_, ext)| !ext.is_empty());
    if !is_api_route && !has_extension {
        let fallback = "index.html";
        if lookup(fallback) {
            return EmbeddedWebResolution::Serve {
                path: fallback.to_owned(),
            };
        }
    }
    EmbeddedWebResolution::NotFound
}

#[cfg(feature = "embedded-web")]
#[derive(RustEmbed)]
#[folder = "web-dist/"]
struct WebAssets;

/// Whether this build carries a usable embedded web SPA. True only when the
/// `embedded-web` feature is on **and** the staged folder actually contained
/// an `index.html` at compile time (a bare checkout stages nothing but the
/// `.gitkeep`, which must not count as a web UI).
#[cfg(feature = "embedded-web")]
pub fn embedded_web_available() -> bool {
    WebAssets::get("index.html").is_some()
}

#[cfg(not(feature = "embedded-web"))]
pub fn embedded_web_available() -> bool {
    false
}

/// Look up an embedded asset's bytes by serving path.
#[cfg(feature = "embedded-web")]
fn embedded_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    WebAssets::get(path).map(|file| file.data)
}

/// The axum fallback handler that answers non-API routes from the
/// compile-time asset table (feature builds only). The request path —
/// which excludes the query string — goes through
/// [`resolve_embedded_path`]; a hit is served with its content type, a
/// miss answers 404.
#[cfg(feature = "embedded-web")]
pub(crate) async fn serve_embedded(uri: axum::http::Uri) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    match resolve_embedded_path(&|candidate| WebAssets::get(candidate).is_some(), uri.path()) {
        EmbeddedWebResolution::Serve { path } => match embedded_asset(&path) {
            Some(bytes) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type_for(&path))],
                bytes.into_owned(),
            )
                .into_response(),
            // The static table cannot change under us — an entry that
            // resolved but vanished is served as a plain miss.
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
        EmbeddedWebResolution::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Attach the embedded-SPA fallback to the router when this build carries
/// one; otherwise return the router unchanged. The `web_dir` branch of the
/// caller keeps an explicit `--web-dir` strictly ahead of the embedded
/// assets.
#[cfg(feature = "embedded-web")]
pub(crate) fn attach_embedded_fallback(router: Router) -> Router {
    if embedded_web_available() {
        router.fallback(serve_embedded)
    } else {
        router
    }
}

/// The lite build (`embedded-web` off) never carries a SPA — the router
/// serves the API only.
#[cfg(not(feature = "embedded-web"))]
pub(crate) fn attach_embedded_fallback(router: Router) -> Router {
    router
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table<'a>(paths: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |candidate| paths.contains(&candidate)
    }

    #[test]
    fn should_serve_index_for_the_bare_root_path() {
        let lookup = table(&["index.html", "favicon.ico"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/"),
            EmbeddedWebResolution::Serve {
                path: "index.html".to_owned()
            }
        );
    }

    #[test]
    fn should_fall_back_to_index_for_extensionless_routes() {
        let lookup = table(&["index.html", "assets/app.js"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/session/42"),
            EmbeddedWebResolution::Serve {
                path: "index.html".to_owned()
            }
        );
    }

    #[test]
    fn should_return_the_exact_asset_when_it_exists() {
        let lookup = table(&["index.html", "assets/app.js"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/assets/app.js"),
            EmbeddedWebResolution::Serve {
                path: "assets/app.js".to_owned()
            }
        );
    }

    #[test]
    fn should_not_fall_back_to_index_for_missing_assets() {
        let lookup = table(&["index.html"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/assets/missing.js"),
            EmbeddedWebResolution::NotFound
        );
    }

    #[test]
    fn should_not_mask_unknown_api_routes_with_the_app_shell() {
        let lookup = table(&["index.html"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/api/v1/definitely/not/a/route"),
            EmbeddedWebResolution::NotFound
        );
    }

    #[test]
    fn should_report_no_web_ui_when_index_is_not_staged() {
        let lookup = table(&[".gitkeep"]);
        assert_eq!(
            resolve_embedded_path(&lookup, "/"),
            EmbeddedWebResolution::NotFound
        );
        assert!(!lookup("index.html"));
    }
}
