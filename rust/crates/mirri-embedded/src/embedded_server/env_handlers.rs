//! Bootstrap-domain dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B5-a `env.*` surface, the
//! klient `bootstrapService` zero-arg reads mirrored on the engine wire.
//!
//! Every read answers a real fact of the running engine process:
//!
//! - host facts: `platform` / `arch` (`std::env::consts`, the rust spelling
//!   — `"macos"`, not Node's `"darwin"`), `cwd` (the live process working
//!   directory), `osHomeDir` (the OS user home), and the client identity
//!   (`clientVersion` / `clientIdentity`, the engine's own crate identity —
//!   the embedded engine has no host-declared identity channel yet, so it
//!   states itself; TSV row note records the drift);
//! - the app path layout, mirroring the klient
//!   `BootstrapService` layout (`<homeDir>/{sessions,blobs,store,cache,logs}`
//!   and `<homeDir>/config.toml`). `sessionsDir` reports the session
//!   manager's *actual* persistence root (which equals
//!   `<home>/v3_sessions` for an injected home, and the workspace-local
//!   `<workspace>/.mirri/v3_sessions` otherwise — that difference is real
//!   and reported honestly); the other four scopes are the klient-layout
//!   derivations (the rust engine has not materialized `blobs` / `store` /
//!   `cache` / `logs` consumers yet — the paths are the layout contract,
//!   not a guarantee the directories exist).
//!
//! Nothing panics: a path/fact derivation that cannot resolve answers the
//! shared internal-error mapping (`-32603`) with the reason, per the
//! program's honesty rule.

use serde_json::Value;

use mirri_transport::adapter::EngineError;

use super::EmbeddedServer;
use super::responses::{ClientIdentityResult, to_result};

/// The embedded engine's product name, as it identifies itself on the wire
/// (the klient `clientIdentity.productName`; the v2 TS host states its own
/// identity — the EMB engine has no host channel and names itself).
const PRODUCT_NAME: &str = "mirri-code";

impl EmbeddedServer {
    /// The Mirri home this server resolves home-relative layout through:
    /// the injected home when one was given, else the env-based
    /// [`mirri_core::config::mirri_home_dir`]. An unresolvable home is an
    /// honest internal error, never a panic or a fabricated path.
    fn env_mirri_home(&self) -> Result<std::path::PathBuf, EngineError> {
        match &self.home_dir {
            Some(home) => Ok(home.clone()),
            None => mirri_core::config::mirri_home_dir().map_err(|e| {
                EngineError::Internal(format!("env.homeDir: mirri home resolution failed: {e}"))
            }),
        }
    }

    /// The OS user home (`$HOME` / `$USERPROFILE`), through the single
    /// OS-layer seam ([`mirri_os::os_home_dir`]). An unresolvable home is
    /// an honest internal error.
    fn env_os_home(&self) -> Result<std::path::PathBuf, EngineError> {
        mirri_os::os_home_dir().ok_or_else(|| {
            EngineError::Internal(
                "env.osHomeDir: neither HOME nor USERPROFILE is set".to_string(),
            )
        })
    }

    /// `env.platform` — the rust `std::env::consts::OS` spelling (the klient
    /// `stringRead` contract takes any string; the TS host's
    /// `process.platform` spells darwin as "darwin", the rust consts spell
    /// it "macos" — TSV row note records the drift).
    pub(super) async fn dispatch_env_platform(&self) -> Result<Value, EngineError> {
        to_result(std::env::consts::OS.to_string())
    }

    /// `env.arch` — the rust `std::env::consts::ARCH` spelling.
    pub(super) async fn dispatch_env_arch(&self) -> Result<Value, EngineError> {
        to_result(std::env::consts::ARCH.to_string())
    }

    /// `env.cwd` — the live process working directory.
    pub(super) async fn dispatch_env_cwd(&self) -> Result<Value, EngineError> {
        let cwd =
            std::env::current_dir().map_err(|e| EngineError::Internal(format!("env.cwd: {e}")))?;
        to_result(cwd.to_string_lossy().to_string())
    }

    /// `env.osHomeDir` — the OS user home.
    pub(super) async fn dispatch_env_os_home_dir(&self) -> Result<Value, EngineError> {
        let home = self.env_os_home()?;
        to_result(home.to_string_lossy().to_string())
    }

    /// `env.homeDir` — the Mirri home (injected home, else
    /// `MIRRICODE_HOME` / `$HOME/.mirri-code`), the same resolution the
    /// composition root used.
    pub(super) async fn dispatch_env_home_dir(&self) -> Result<Value, EngineError> {
        let home = self.env_mirri_home()?;
        to_result(home.to_string_lossy().to_string())
    }

    /// `env.configPath` — the config.toml the current configuration was
    /// loaded from, or the klient-layout `<home>/config.toml` when the
    /// config fell back to defaults (the klient contract always returns a
    /// path — the derivation is the layout contract).
    pub(super) async fn dispatch_env_config_path(&self) -> Result<Value, EngineError> {
        let path = match &self.config_path {
            Some(path) => path.clone(),
            None => self.env_mirri_home()?.join("config.toml"),
        };
        to_result(path.to_string_lossy().to_string())
    }

    /// `env.clientVersion` — the engine crate's semver (kept alongside
    /// `clientIdentity`; the klient contract still carries both and the
    /// wire serves the pair).
    pub(super) async fn dispatch_env_client_version(&self) -> Result<Value, EngineError> {
        to_result(env!("CARGO_PKG_VERSION").to_string())
    }

    /// `env.clientIdentity` — the klient `MirriHostIdentity` camelCase shape
    /// (`productName` / `version` / `platform`, optional
    /// `userAgentSuffix`). The embedded engine states its own identity
    /// (no host-declared channel exists yet — TSV row note records the
    /// drift); `userAgentSuffix` is omitted (no honest source).
    pub(super) async fn dispatch_env_client_identity(&self) -> Result<Value, EngineError> {
        to_result(ClientIdentityResult {
            product_name: PRODUCT_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: std::env::consts::OS.to_string(),
            user_agent_suffix: None,
        })
    }

    /// `env.sessionsDir` — the session manager's actual persistence root:
    /// `<home>/v3_sessions` for an injected home, the workspace-local
    /// `<workspace>/.mirri/v3_sessions` otherwise (the honest read of where
    /// this engine's sessions live — TSV row note records the divergence
    /// from the klient always-home layout).
    pub(super) async fn dispatch_env_sessions_dir(&self) -> Result<Value, EngineError> {
        to_result(
            self.manager
                .persistence()
                .sessions_dir()
                .to_string_lossy()
                .to_string(),
        )
    }

    /// `env.blobsDir` — the klient-layout `<mirri home>/blobs` derivation.
    pub(super) async fn dispatch_env_blobs_dir(&self) -> Result<Value, EngineError> {
        let dir = self.env_mirri_home()?.join("blobs");
        to_result(dir.to_string_lossy().to_string())
    }

    /// `env.storeDir` — the klient-layout `<mirri home>/store` derivation.
    pub(super) async fn dispatch_env_store_dir(&self) -> Result<Value, EngineError> {
        let dir = self.env_mirri_home()?.join("store");
        to_result(dir.to_string_lossy().to_string())
    }

    /// `env.cacheDir` — the klient-layout `<mirri home>/cache` derivation.
    pub(super) async fn dispatch_env_cache_dir(&self) -> Result<Value, EngineError> {
        let dir = self.env_mirri_home()?.join("cache");
        to_result(dir.to_string_lossy().to_string())
    }

    /// `env.logsDir` — the klient-layout `<mirri home>/logs` derivation.
    pub(super) async fn dispatch_env_logs_dir(&self) -> Result<Value, EngineError> {
        let dir = self.env_mirri_home()?.join("logs");
        to_result(dir.to_string_lossy().to_string())
    }
}
