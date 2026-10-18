//! `mirri-cli` — the `mirri` binary.
//!
//! The package ships a library and a binary target. The library holds the
//! [`Cli`] argument-parsing types so integration tests (`tests/cli_tests.rs`)
//! can exercise parsing without starting a server; the binary entry point
//! (parsing + serve logic) lives in `src/main.rs`.

use clap::{Parser, Subcommand};

pub mod commands;
pub mod doctor;
pub mod watcher;

/// Top-level command-line interface of the `mirri` binary.
#[derive(Debug, Parser)]
#[command(
    name = "mirri",
    version = env!("CARGO_PKG_VERSION"),
    about = "Mirri Code server"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommands of the `mirri` binary.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the HTTP/WS/IPC server
    Serve {
        /// HTTP bind address
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// HTTP bind port
        #[arg(long, default_value_t = 58627)]
        port: u16,
        /// Unix socket path for IPC (omit to disable IPC)
        #[arg(long)]
        ipc_path: Option<String>,
        /// Workspace root directory (omit to serve every workspace in the
        /// Mirri home's registry — the most-recently-opened row becomes the
        /// default; an empty registry falls back to the current directory)
        #[arg(long)]
        workspace: Option<String>,
        /// Log filter (RUST_LOG style)
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Shared secret enabling Bearer-token auth on the HTTP API.
        /// Every route except `/api/v1/healthz` and `/api/v1/meta` then
        /// requires `Authorization: Bearer <token>`; omit to load (or
        /// create, on first boot) the persistent token from
        /// `<mirri-home>/server.token` — the same file the TypeScript
        /// kap-server uses.
        #[arg(long)]
        auth_token: Option<String>,
        /// Disable Bearer-token auth entirely and run in open (development)
        /// mode. Every route except `/api/v1/shutdown` (loopback peers only)
        /// becomes unauthenticated. Only use on a trusted network.
        #[arg(long)]
        dangerous_bypass_auth: bool,
        /// Directory containing the built web frontend (SPA) to serve
        /// alongside the API. When provided, non-API routes serve static
        /// files with history-mode fallback to `index.html`.
        #[arg(long)]
        web_dir: Option<String>,
    },

    /// Start the server for the web UI (browser / Desktop front end).
    ///
    /// A drop-in for the TypeScript `mirri web` command so packaged front
    /// ends (the Desktop app spawns `mirri web --no-open --port <p>
    /// --log-level error`) can start the Rust binary with the same argv.
    /// Internally it runs the same server as `serve`, with front-end-friendly
    /// defaults: the workspace falls back to the user's home directory when
    /// the cwd is the filesystem root (packaged apps inherit `/`), the SPA
    /// comes from `--web-dir` or, when omitted, the copy embedded in the
    /// binary (full build) or a directory staged next to the executable
    /// (Electron resources layout), and a browser opens at the served URL
    /// unless `--no-open` disables it.
    Web {
        /// HTTP bind address
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// HTTP bind port
        #[arg(long, default_value_t = 58627)]
        port: u16,
        /// Workspace root directory (omit to use the cwd, falling back to
        /// the user home when the cwd is the filesystem root)
        #[arg(long)]
        workspace: Option<String>,
        /// Log filter (RUST_LOG style)
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Shared secret enabling Bearer-token auth (same semantics as
        /// `serve --auth-token`; omitted → persistent `<mirri-home>/
        /// server.token`)
        #[arg(long)]
        auth_token: Option<String>,
        /// Disable Bearer-token auth entirely (same semantics as `serve
        /// --dangerous-bypass-auth`)
        #[arg(long)]
        dangerous_bypass_auth: bool,
        /// Directory containing the built web frontend (SPA) to serve
        /// alongside the API (omit to serve the SPA embedded in the binary,
        /// or probe from the executable's location)
        #[arg(long)]
        web_dir: Option<String>,
        /// Do not open the browser at the served URL (the default for
        /// packaged front ends that manage their own window).
        #[arg(long = "no-open")]
        no_open: bool,
    },

    /// Bring legacy TS-engine sessions into the rust engine's v3 storage root
    ///
    /// Copies (never moves — the legacy root stays readable by the TS
    /// engines) every session under `<mirri-home>/sessions/` into the
    /// sibling `v3_sessions/` root, promoting the TS layout's
    /// `agents/main/wire.jsonl` to the top-level `wire.jsonl` the rust
    /// resume path reads, verifying each copy, and registering the
    /// sessions' workspaces in `workspaces.json` so the engine can route
    /// them after the next server start. `--workspace <dir>` also migrates
    /// that workspace's `.mirri/sessions/`. Wire protocol versions stay
    /// lazy — upgraded when a session actually resumes. Idempotent:
    /// sessions already present in the v3 root are skipped and reported.
    Migrate {
        /// Path to the workspace directory whose `.mirri/sessions/` is
        /// also migrated (omit to migrate only the global home sessions,
        /// `~/.mirri-code/sessions/`)
        #[arg(long)]
        workspace: Option<String>,
        /// Dry-run mode: show what would be migrated without making changes
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },

    /// List persisted sessions
    SessionList {
        /// Path to the workspace directory
        #[arg(long, default_value = ".")]
        workspace: String,
    },

    /// Show current configuration
    ConfigShow {
        /// Path to the workspace directory
        #[arg(long, default_value = ".")]
        workspace: String,
    },

    /// Run a single prompt non-interactively and print the response
    Print {
        /// The prompt to send
        prompt: String,
        /// Model ID to use (defaults to the engine default Claude model)
        #[arg(long)]
        model: Option<String>,
        /// Provider type: anthropic or openai (defaults to anthropic)
        #[arg(long)]
        provider: Option<String>,
    },

    /// Validate config.toml against the rust engine's config schema
    Doctor {
        /// Validate this file as config.toml instead of the default path
        #[arg(long)]
        config: Option<String>,
    },
}
