//! Live TLS smoke test and dependency guard for the rustls-backed HTTP client.
//!
//! The workspace pins `reqwest` to `rustls-tls` (no `default-tls`), so no
//! OpenSSL is linked on any target. Two protections live here:
//!
//! 1. A lockfile purity guard: the resolved workspace `Cargo.lock` must not
//!    contain `openssl-sys`, so any change that reintroduces native-tls
//!    (e.g. re-enabling reqwest default features) fails a normal test run.
//! 2. A live TLS handshake test against a public endpoint, proving the
//!    rustls path works end to end. The wiremock suites exercise the HTTP
//!    stack over plain HTTP only.
//!
//! The live test is `#[ignore]`d by default: it requires network access.
//! Run locally with:
//!
//! ```sh
//! cargo test -p mirri-kosong --test tls_smoke -- --ignored
//! ```

#[test]
fn should_not_have_openssl_in_the_resolved_workspace_lockfile() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .expect("workspace Cargo.lock should be readable from mirri-kosong");

    assert!(
        !lock.contains("name = \"openssl-sys\""),
        "openssl-sys found in workspace Cargo.lock — the HTTP stack must stay on rustls \
         (check the reqwest features in the workspace Cargo.toml)"
    );
}

#[tokio::test]
#[ignore = "requires network access"]
async fn should_complete_real_tls_handshake_when_fetching_https_endpoint() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("client with rustls backend should build");

    let response = client
        .get("https://example.com/")
        .send()
        .await
        .expect("TLS handshake and HTTP round trip should succeed against example.com");

    assert!(
        response.status().is_success(),
        "expected success status, got {}",
        response.status()
    );
}
