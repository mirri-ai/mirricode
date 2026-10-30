//! Desktop server probe: detect a live local desktop server from its
//! self-maintained state file (design 14 §D1).
//!
//! The probe is the ACP host's startup answer to "is there a desktop
//! engine already running that I should share?" — its only anchor is
//! `<mirri-home>/server/desktop-state.json` (`{pid, port}`, written by the
//! desktop shell). It never scans ports: on a machine without the desktop
//! app the probe costs exactly one failed file read and falls back to the
//! embedded engine.
//!
//! Probe order (design 14 §D1): state file → pid liveness → healthz →
//! token file. Any failure anywhere returns `None` (fail closed to
//! embedded):
//!
//! - state file missing / unparseable / missing fields → `None`
//! - recorded pid dead → `None`
//! - `GET http://127.0.0.1:<port>/api/v1/healthz` not 200 or the body's
//!   `data.ok` not `true` → `None`
//! - `<mirri-home>/server.token` missing or empty → `None`
//!
//! Pid liveness reuses [`mirri_os::process::probe_process`] — the same
//! helper the orphan sweep uses, which also reports unreaped zombies as
//! not alive (a zombie server is not a server). The healthz body shape
//! matches the server's `ApiResponse` envelope (`{"code":0,...,
//! "data":{"ok":true,"ready":true}}`).

use std::path::Path;
use std::time::Duration;

/// Minimum `server_hello.version` (semver major) the ACP host can share a
/// desktop server with; the gate is enforced where the handshake completes
/// via [`version_satisfies`].
///
/// Bump this in the same commit that lands a wire change the ACP host
/// cannot tolerate — it is the honest protocol floor, not a mirror of any
/// crate version (a build-time cross-crate macro was considered and
/// rejected in design 14 §D1).
pub const MIN_SERVER_VERSION: &str = "2.0.0";

/// Connect parameters of a successfully probed desktop server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopServerEndpoint {
    /// The unified JSON-RPC WebSocket endpoint, e.g.
    /// `ws://127.0.0.1:58627/api/v1/rpc` — the URL shape
    /// `RemoteAdapter::connect` consumes.
    pub url: String,
    /// Bearer token read from `<mirri-home>/server.token`.
    pub token: String,
}

/// Wire shape of the desktop shell's state file
/// (`<mirri-home>/server/desktop-state.json`).
#[derive(Debug, serde::Deserialize)]
struct DesktopState {
    pid: i64,
    port: u16,
}

/// Probe for a live local desktop server (design 14 §D1).
///
/// Returns `Some` only when the state file exists, the recorded pid is
/// alive, healthz answers HTTP 200 with `ok: true`, and the token file is
/// non-empty — `None` on any other outcome. Never scans ports.
pub async fn probe_desktop_server() -> Option<DesktopServerEndpoint> {
    let home = mirri_core::config::mirri_home_dir().ok()?;
    probe_home(&home).await
}

/// Probe against an explicit home directory (test seam: everything but the
/// home resolution, which the tests drive through `MIRRICODE_HOME`).
async fn probe_home(home: &Path) -> Option<DesktopServerEndpoint> {
    // 1. The state file is the only anchor — missing means "no desktop",
    // which is not an error, just a `None`.
    let state: DesktopState = serde_json::from_str(
        &tokio::fs::read_to_string(home.join("server/desktop-state.json"))
            .await
            .ok()?,
    )
    .ok()?;
    if state.pid <= 0 || state.port == 0 {
        return None;
    }

    // 2. The recorded pid must be alive. `probe_process` answers false for
    // dead pids (ESRCH) and zombies; a process we cannot identify is not
    // ours (EPERM discipline from design 14 §D1) — both fail closed.
    let alive = mirri_os::process::probe_process(i32::try_from(state.pid).unwrap_or(-1))
        .map(|probe| probe.alive)
        .unwrap_or(false);
    if !alive {
        return None;
    }

    // 3. The healthz route is public (no token) — a cheap ownership check
    // that also rules out a pid-reused stranger squatting on the port.
    let url = format!("http://127.0.0.1:{}/api/v1/healthz", state.port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    if body.get("data").and_then(|data| data.get("ok")) != Some(&serde_json::Value::Bool(true)) {
        return None;
    }

    // 4. The token lives at the home root (not under `server/`) — same
    // file the web UI and the explicit remote-engine mode read.
    let token = tokio::fs::read_to_string(home.join("server.token"))
        .await
        .ok()?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    Some(DesktopServerEndpoint {
        url: format!("ws://127.0.0.1:{}/api/v1/rpc", state.port),
        token: token.to_string(),
    })
}

/// Semver-major gate on the handshake's `server_hello.version` (design 14
/// §D1): satisfied only when both versions parse and the server's major is
/// at least `min`'s. Unparseable versions fail closed (`false`) so the
/// host falls back to the embedded engine rather than guessing.
pub fn version_satisfies(server_version: &str, min: &str) -> bool {
    match (parse_major(server_version), parse_major(min)) {
        (Some(server), Some(required)) => server >= required,
        _ => false,
    }
}

/// Extract the major component of a `X.Y.Z` version string; `None` when
/// the string is not a parseable dotted version.
fn parse_major(version: &str) -> Option<u64> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    // A bare major ("2") is tolerated, but "2.0" / "2.0.0" must have
    // numeric minor/patch components when present.
    for part in parts {
        if part.parse::<u64>().is_err() {
            return None;
        }
    }
    Some(major)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process environment variables.
    ///
    /// `std::env::set_var`/`remove_var` are `unsafe` in edition 2024, and
    /// environment reads/writes race across threads when tests run in
    /// parallel, so every env-mutating test holds this lock and restores
    /// the previous value on drop.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Saves the current value of a touched variable and restores it on
    /// drop, so tests never leak env state into each other.
    struct EnvGuard {
        saved: Option<String>,
    }

    impl EnvGuard {
        fn new(name: &str) -> Self {
            let guard = Self {
                saved: std::env::var(name).ok(),
            };
            if let Some(value) = &guard.saved {
                // SAFETY: guarded by ENV_LOCK and restored in Drop; no other
                // thread reads or writes this variable concurrently.
                unsafe { std::env::set_var(name, value) };
            } else {
                // SAFETY: same discipline as set.
                unsafe { std::env::remove_var(name) };
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.saved {
                // SAFETY: same discipline as set.
                unsafe { std::env::set_var("MIRRICODE_HOME", value) };
            } else {
                // SAFETY: same discipline as set.
                unsafe { std::env::remove_var("MIRRICODE_HOME") };
            }
        }
    }

    // ------------------------------------------------------------------
    // version_satisfies
    // ------------------------------------------------------------------

    #[test]
    fn should_report_unsatisfied_when_the_server_version_is_unparseable() {
        // Given a server hello version that is not semver
        // When the gate is evaluated
        // Then it fails closed (probe must fall back to embedded)
        assert!(!version_satisfies("not-semver", "1.0.0"));
    }

    #[test]
    fn should_report_unsatisfied_when_the_minimum_version_is_unparseable() {
        assert!(!version_satisfies("1.0.0", "banana"));
    }

    #[test]
    fn should_report_satisfied_when_the_server_major_meets_the_minimum() {
        assert!(version_satisfies("1.4.2", "1.0.0"));
        assert!(!version_satisfies("0.9.9", "1.0.0"));
    }

    #[test]
    fn should_report_satisfied_when_the_server_major_exceeds_the_minimum() {
        // Forward-compatible within the major gate: a newer server major
        // satisfies an older floor.
        assert!(version_satisfies("3.1.0", "2.0.0"));
    }

    #[test]
    fn should_report_satisfied_when_the_versions_match_exactly() {
        assert!(version_satisfies("2.0.0", "2.0.0"));
    }

    #[test]
    fn should_report_unsatisfied_when_minor_or_patch_components_are_garbage() {
        // A dotted version with a non-numeric component is unparseable —
        // the gate must not cherry-pick a leading number.
        assert!(!version_satisfies("2.x.0", "2.0.0"));
    }

    #[test]
    fn should_report_unsatisfied_when_the_version_is_empty() {
        assert!(!version_satisfies("", "1.0.0"));
    }

    // ------------------------------------------------------------------
    // probe_desktop_server
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn should_return_none_when_the_desktop_state_file_is_missing() {
        // Given a temp home with no server/desktop-state.json
        // (set MIRRICODE_HOME to the tempdir for the test)
        // When probing
        // Then the result is None with no side effects (no port contact)
        let _env = {
            let _lock = ENV_LOCK.lock().unwrap();
            let home = tempfile::tempdir().unwrap();
            // SAFETY: guarded by ENV_LOCK and restored by EnvGuard's Drop.
            unsafe { std::env::set_var("MIRRICODE_HOME", home.path()) };
            EnvGuard::new("MIRRICODE_HOME")
        };
        let endpoint = probe_desktop_server().await;
        assert!(endpoint.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_the_state_file_is_not_valid_json() {
        // Given a temp home whose state file is garbage
        let home = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(home.path().join("server"))
            .await
            .unwrap();
        tokio::fs::write(home.path().join("server/desktop-state.json"), "{ not json")
            .await
            .unwrap();
        // When probing that home
        // Then the tolerant parse fails closed to None
        assert!(probe_home(home.path()).await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_the_state_file_misses_the_pid_field() {
        // Given a state file carrying only the port
        let home = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(home.path().join("server"))
            .await
            .unwrap();
        tokio::fs::write(
            home.path().join("server/desktop-state.json"),
            r#"{"port": 58627}"#,
        )
        .await
        .unwrap();
        // When probing
        // Then the missing field fails closed
        assert!(probe_home(home.path()).await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_the_recorded_pid_is_dead() {
        // Given a state file whose pid belongs to an exited process
        let home = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(home.path().join("server"))
            .await
            .unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id() as i64;
        let _ = child.wait();
        tokio::fs::write(
            home.path().join("server/desktop-state.json"),
            serde_json::json!({"pid": dead_pid, "port": 58627}).to_string(),
        )
        .await
        .unwrap();
        // When probing
        // Then the dead pid fails closed before any port contact
        assert!(probe_home(home.path()).await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_the_state_file_records_a_nonpositive_pid() {
        // Given a state file with a sentinel pid
        let home = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(home.path().join("server"))
            .await
            .unwrap();
        tokio::fs::write(
            home.path().join("server/desktop-state.json"),
            r#"{"pid": 0, "port": 58627}"#,
        )
        .await
        .unwrap();
        // When probing
        // Then the sentinel fails closed
        assert!(probe_home(home.path()).await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_nothing_listens_on_the_recorded_port() {
        // Given a live pid but nothing on the recorded port (healthz
        // refused) — the pid below is the test process itself, which is
        // alive, so the probe reaches the healthz step and fails there.
        let home = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(home.path().join("server"))
            .await
            .unwrap();
        tokio::fs::write(
            home.path().join("server/desktop-state.json"),
            serde_json::json!({"pid": std::process::id(), "port": 1}).to_string(),
        )
        .await
        .unwrap();
        // When probing
        // Then the refused healthz fails closed to None
        assert!(probe_home(home.path()).await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_when_the_token_file_is_missing() {
        // Given a live local HTTP server answering healthz but no token
        // file in the home
        let home = tempfile::tempdir().unwrap();
        let (addr, shutdown) = spawn_healthz(HEALTHZ_OK).await;
        write_state(home.path(), std::process::id(), addr.port()).await;
        // When probing
        // Then the missing token fails closed to None
        assert!(probe_home(home.path()).await.is_none());
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn should_return_none_when_the_token_file_is_empty() {
        // Given a live healthz server and an empty (whitespace-only) token
        let home = tempfile::tempdir().unwrap();
        let (addr, shutdown) = spawn_healthz(HEALTHZ_OK).await;
        write_state(home.path(), std::process::id(), addr.port()).await;
        tokio::fs::write(home.path().join("server.token"), "   \n")
            .await
            .unwrap();
        // When probing
        // Then the empty token fails closed to None
        assert!(probe_home(home.path()).await.is_none());
        shutdown.send(()).unwrap();
    }

    // Gated on Windows until `probe_process` gains a real non-Unix
    // implementation: it currently reports every pid as not alive there,
    // so this happy path can never succeed (see the linked issue).
    #[cfg(not(windows))]
    #[tokio::test]
    async fn should_return_the_ws_url_and_token_when_every_probe_step_passes() {
        // Given a live local HTTP server answering healthz with the
        // ApiResponse envelope and a non-empty token file
        let home = tempfile::tempdir().unwrap();
        let (addr, shutdown) = spawn_healthz(HEALTHZ_OK).await;
        write_state(home.path(), std::process::id(), addr.port()).await;
        tokio::fs::write(home.path().join("server.token"), "secret-token\n")
            .await
            .unwrap();
        // When probing
        let endpoint = probe_home(home.path()).await;
        // Then the endpoint carries the unified RPC ws URL and the trimmed
        // token
        assert_eq!(
            endpoint,
            Some(DesktopServerEndpoint {
                url: format!("ws://{}/api/v1/rpc", addr),
                token: "secret-token".to_string(),
            })
        );
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn should_return_none_when_healthz_answers_200_with_ok_false() {
        // Given a live local server whose healthz body reports ok=false
        // (the ApiResponse envelope with a failing data payload)
        let home = tempfile::tempdir().unwrap();
        let (addr, shutdown) = spawn_healthz(HEALTHZ_NOT_OK).await;
        write_state(home.path(), std::process::id(), addr.port()).await;
        tokio::fs::write(home.path().join("server.token"), "secret-token\n")
            .await
            .unwrap();
        // When probing
        // Then the not-ok body fails closed to None even with a token present
        assert!(probe_home(home.path()).await.is_none());
        shutdown.send(()).unwrap();
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Write a well-formed desktop state file for a live pid and `port`.
    async fn write_state(home: &Path, pid: u32, port: u16) {
        tokio::fs::create_dir_all(home.join("server"))
            .await
            .unwrap();
        tokio::fs::write(
            home.join("server/desktop-state.json"),
            serde_json::json!({"pid": pid, "port": port}).to_string(),
        )
        .await
        .unwrap();
    }

    use std::sync::Mutex;

    /// One-shot local HTTP server answering `GET /api/v1/healthz` with
    /// `body`, mirroring the server's ApiResponse envelope
    /// (`rust/crates/mirri-server/src/http.rs` healthz writes
    /// `{"code":0,...,"data":{"ok":true,"ready":true}}`). Serves until the
    /// returned shutdown signal fires. Returns its address.
    async fn spawn_healthz(
        body: &'static str,
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    Ok(accepted) = listener.accept() => accepted,
                    _ = &mut shutdown_rx => break,
                };
                serve_healthz_once(stream, body).await;
            }
        });
        (addr, shutdown_tx)
    }

    /// Answer a single healthz request with a hand-rolled minimal HTTP/1.1
    /// response (no HTTP framework needed — the probe only checks status
    /// and the JSON body).
    async fn serve_healthz_once(mut stream: tokio::net::TcpStream, body: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 1024];
        // Read until end of the request head (healthz carries no body).
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }

    /// The server's healthz body (`ApiResponse<HealthzData>` envelope).
    const HEALTHZ_OK: &str =
        r#"{"code":0,"msg":"success","data":{"ok":true,"ready":true},"request_id":"t"}"#;
    /// The same envelope with a failing `data.ok` (an unhealthy server).
    const HEALTHZ_NOT_OK: &str =
        r#"{"code":0,"msg":"success","data":{"ok":false,"ready":false},"request_id":"t"}"#;
}
