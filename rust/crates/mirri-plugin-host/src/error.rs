//! Error types for the plugin host.
//!
//! [`PluginError`] is the canonical error enum returned by all plugin host
//! operations. Each variant identifies a distinct failure mode so that
//! callers can match on the specific reason rather than parsing a message
//! string.

use std::io;

/// The canonical result type for plugin host operations.
pub type PluginResult<T> = Result<T, PluginError>;

/// Errors that can occur during plugin installation, loading, or management.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The requested plugin was not found in the store or the loaded set.
    #[error("Plugin not found: {0}")]
    NotFound(String),

    /// The plugin manifest (`mirri.plugin.json` or `.mirri-plugin/plugin.json`)
    /// is missing required fields or contains invalid values.
    #[error("Plugin manifest invalid: {0}")]
    InvalidManifest(String),

    /// A plugin with the same id (name) is already installed.
    #[error("Plugin already installed: {0}")]
    AlreadyInstalled(String),

    /// The installation process failed — source resolution, download, copy,
    /// or extraction error.
    #[error("Plugin install failed: {0}")]
    InstallFailed(String),

    /// An install upgrade failed **and** restoring the previous managed copy
    /// also failed — the TS `AggregateError` raised by `PluginManager.install`
    /// (manager.ts): the message carries both the install error and the
    /// rollback error.
    #[error(
        "Plugin installation failed and the previous managed copy could not be restored: \
         install error: {install}; rollback error: {rollback}"
    )]
    InstallRollbackFailed {
        /// The error that failed the install itself.
        install: String,
        /// The error that failed the rollback of the managed copy.
        rollback: String,
    },

    /// A resolved path escapes the plugin root directory. This is a security
    /// guard against path traversal in manifest fields that reference
    /// relative paths.
    #[error("Plugin path outside root: {0}")]
    PathOutsideRoot(String),

    /// The requested slash command is not registered by any enabled plugin.
    #[error("Command not found: {0}")]
    CommandNotFound(String),

    /// The requested MCP server is not declared by the plugin manifest.
    #[error("Plugin MCP server not found: {0}")]
    McpServerNotFound(String),

    /// A command template could not be rendered (syntax error, unknown
    /// filter, etc.).
    #[error("Command template error: {0}")]
    Template(String),

    /// An I/O error from the filesystem.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// A JSON serialization or deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
