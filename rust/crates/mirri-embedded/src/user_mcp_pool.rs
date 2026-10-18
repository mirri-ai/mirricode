//! The app-level user MCP pool (spec `docs/specs/mcp-architecture.md`
//! §4.2, batch P2).
//!
//! The user config layer's connections are an app-level asset: every
//! workspace engine would otherwise connect its own copy of the same
//! user-layer server (the "connection explosion" the three-pool model
//! removes). A [`UserMcpPool`] wraps one [`McpConnectionManager`] plus a
//! connect-once guard, and the process-level registry
//! ([`shared_user_mcp_pool_for_home`]) hands every engine loaded from the
//! same home the same pool — the desktop N-workspaces case connects the
//! user layer exactly once. Engines with distinct homes (tests, isolated
//! sessions) get their own pool.
//!
//! Pool lifetime is the process: engines attach and detach freely, and an
//! engine's dispose never tears the pool down (the user layer outlives any
//! one workspace). The registry keeps strong references so a pool stays
//! alive for the process even when no engine currently holds it —
//! reconnecting a dropped pool would silently fork duplicate server
//! processes, so the cheap option (retain) is the correct one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use mirri_mcp::McpConnectionManager;

/// One shared user-layer connection pool.
#[derive(Debug)]
pub struct UserMcpPool {
    manager: Arc<McpConnectionManager>,
    /// Guards that the user layer's `connect_all` is spawned at most once
    /// per pool (engines sharing the pool must not re-connect it).
    connect_spawned: AtomicBool,
}

impl UserMcpPool {
    /// A private pool — the default for engines assembled without a shared
    /// one (tests, `serve`'s single-workspace process).
    pub fn private() -> Self {
        Self::with_oauth_token_dir(None)
    }

    /// A private pool whose manager shares the MCP OAuth token store: the
    /// connect path attaches cached tokens to remote transports (the
    /// persisted-token wiring of plans/07 T8).
    pub fn with_oauth_token_dir(oauth_token_dir: Option<PathBuf>) -> Self {
        let manager = oauth_token_dir
            .map(|dir| Arc::new(McpConnectionManager::with_oauth_token_dir(dir)))
            .unwrap_or_else(|| Arc::new(McpConnectionManager::new()));
        Self {
            manager,
            connect_spawned: AtomicBool::new(false),
        }
    }

    /// The underlying connection manager (the pool's connection set).
    pub fn manager(&self) -> Arc<McpConnectionManager> {
        Arc::clone(&self.manager)
    }

    /// Claim the pool's one-shot connect: the first caller connects the
    /// user layer, later engines sharing the pool skip it.
    pub fn claim_connect(&self) -> bool {
        !self.connect_spawned.swap(true, Ordering::SeqCst)
    }
}

impl Default for UserMcpPool {
    fn default() -> Self {
        Self::private()
    }
}

/// The process-level pool registry, keyed by the home the user layer was
/// loaded from.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<UserMcpPool>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<UserMcpPool>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared user pool for the given home: every engine in this process
/// loaded from the same home gets the same pool, so the user layer
/// connects once however many workspaces are open. Distinct homes get
/// distinct pools. `oauth_token_dir` roots the pool manager's MCP OAuth
/// token store (`<mirri home>/mcp-tokens` — the caller resolves it through
/// the same chain it resolved `home` with; only the first construction for
/// a home installs the directory, which is fine because it is derived from
/// that same home).
pub fn shared_user_mcp_pool_for_home(
    home: &Path,
    oauth_token_dir: Option<PathBuf>,
) -> Arc<UserMcpPool> {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .entry(home.to_path_buf())
        .or_insert_with(|| Arc::new(UserMcpPool::with_oauth_token_dir(oauth_token_dir)))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given two lookups for the same home, when the registry is consulted,
    /// then both return the same pool; a different home gets its own.
    #[test]
    fn should_share_one_pool_per_home_across_lookups() {
        let home_a = std::env::temp_dir().join("user-pool-test-home-a");
        let home_b = std::env::temp_dir().join("user-pool-test-home-b");

        let first = shared_user_mcp_pool_for_home(&home_a, None);
        let second = shared_user_mcp_pool_for_home(&home_a, None);
        let other = shared_user_mcp_pool_for_home(&home_b, None);

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    /// Given a fresh pool, when the connect is claimed twice, then only the
    /// first claim wins.
    #[test]
    fn should_win_the_connect_claim_exactly_once() {
        let pool = UserMcpPool::private();
        assert!(pool.claim_connect());
        assert!(!pool.claim_connect());
    }
}
