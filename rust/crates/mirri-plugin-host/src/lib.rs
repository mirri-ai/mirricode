//! `mirri-plugin-host` — plugin lifecycle management for the mirricode
//! agent engine.
//!
//! This crate manages the full lifecycle of plugins: installation from
//! local paths, GitHub repos, or ZIP URLs; loading and parsing of plugin
//! manifests; enabling/disabling; and removal.
//!
//! # Module overview
//!
//! - [`manifest`] — plugin manifest (`mirri.plugin.json`) parsing and
//!   validation, including path-traversal guards.
//! - [`commands`] — plugin slash commands: [`CommandRegistry`],
//!   [`CommandSpec`], and minijinja template rendering.
//! - [`source`] — installation source resolution (`local:`, `github:`,
//!   HTTP(S) URL) and fetch logic.
//! - [`store`] — on-disk persistence of installed-plugin records
//!   (`installed.json`).
//! - [`manager`] — [`PluginManager`], the high-level orchestrator that ties
//!   everything together.
//! - [`error`] — the canonical [`PluginError`] type.
//!
//! # Quick start
//!
//! ```no_run
//! use mirri_plugin_host::PluginManager;
//! use std::path::PathBuf;
//!
//! # async fn example() {
//! let manager = PluginManager::new(PathBuf::from("/path/to/plugins"));
//!
//! // Install from a local directory.
//! let id = manager.install("local:/path/to/my-plugin").await.unwrap();
//!
//! // Load all installed plugins into memory.
//! manager.load_all().unwrap();
//!
//! // List plugins.
//! for info in manager.list() {
//!     println!("{}: enabled={}", info.id, info.enabled);
//! }
//! # }
//! ```

pub mod commands;
pub mod error;
pub mod github;
pub mod manager;
pub mod manifest;
pub mod source;
pub mod store;

pub use commands::{
    CommandEntry, CommandFile, CommandRegistry, CommandSpec, ParsedCommandFile,
    RegisteredCommand, expand_command_arguments, load_command_file, parse_command_text,
};
pub use error::{PluginError, PluginResult};
pub use github::{GithubRef, GithubRefKind, GithubResolver, ReqwestGithubResolver, ResolverFuture};
pub use manager::{
    PluginAgentRoot, PluginCommandDef, PluginGithubMetadata, PluginInfo, PluginInfoView,
    PluginManager, PluginMcpServerEntry, PluginMcpServerInfo, PluginReloadError, PluginSkillRoot,
    PluginSummary, PluginUpdateStatus, ReloadSummary, SkillPluginContext, plugin_mcp_runtime_name,
};
pub use manifest::{
    ParsedManifest, PluginDiagnostic, PluginDiagnosticSeverity, PluginManifest, PluginManifestKind,
    PluginState,
};
pub use source::{
    PluginSource, download_zip, extract_zip, install_from_source, parse_source,
};
pub use store::{InstalledPlugin, PluginMcpServerState, PluginStore};
