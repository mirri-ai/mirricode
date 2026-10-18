//! Env-domain routes — the B5-a `bootstrapService` mirror: the thirteen
//! zero-arg reads (host facts, app path layout, client identity), klient
//! camelCase wire shapes.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    Route {
        method: "env.platform",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_platform()),
    },
    Route {
        method: "env.arch",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_arch()),
    },
    Route {
        method: "env.cwd",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_cwd()),
    },
    Route {
        method: "env.osHomeDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_os_home_dir()),
    },
    Route {
        method: "env.homeDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_home_dir()),
    },
    Route {
        method: "env.configPath",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_config_path()),
    },
    Route {
        method: "env.clientVersion",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_client_version()),
    },
    Route {
        method: "env.clientIdentity",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_client_identity()),
    },
    Route {
        method: "env.sessionsDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_sessions_dir()),
    },
    Route {
        method: "env.blobsDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_blobs_dir()),
    },
    Route {
        method: "env.storeDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_store_dir()),
    },
    Route {
        method: "env.cacheDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_cache_dir()),
    },
    Route {
        method: "env.logsDir",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_env_logs_dir()),
    },
];
