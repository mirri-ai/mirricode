//! Agent profile CRUD routes — the P6 `agent.*Profile` surface the web
//! `/api/v1/agents` REST routes dispatch through (see
//! `profile_handlers.rs` for the model).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Every registered profile as a TS `ProfileEntry` wire row.
    Route {
        method: "agent.listProfiles",
        aliases: &["listProfiles"],
        handler: |server, _params| Box::pin(server.dispatch_list_profiles()),
    },
    // One profile row by name (not-found for an unknown name).
    Route {
        method: "agent.getProfile",
        aliases: &["getProfile"],
        handler: |server, params| Box::pin(server.dispatch_get_profile(params)),
    },
    // Register + persist a new user profile under <mirri-home>/profiles/.
    Route {
        method: "agent.createProfile",
        aliases: &["createProfile"],
        handler: |server, params| Box::pin(server.dispatch_create_profile(params)),
    },
    // Merge-provide fields into an existing profile (a built-in gains an
    // override file) and persist.
    Route {
        method: "agent.updateProfile",
        aliases: &["updateProfile"],
        handler: |server, params| Box::pin(server.dispatch_update_profile(params)),
    },
    // Remove the persisted profile (a built-in override delete restores
    // the built-in definition).
    Route {
        method: "agent.deleteProfile",
        aliases: &["deleteProfile"],
        handler: |server, params| Box::pin(server.dispatch_delete_profile(params)),
    },
    // Write the file-level `enabled = false` flag through the `.md` write
    // channel (an existing file keeps every byte outside the enabled
    // line; no override-declaration key, D14).
    Route {
        method: "agent.disableProfile",
        aliases: &["disableProfile"],
        handler: |server, params| Box::pin(server.dispatch_disable_profile(params)),
    },
    // Clear the file-level `enabled = false` flag (already-enabled
    // profiles, e.g. bare built-ins, are a legal no-op success).
    Route {
        method: "agent.enableProfile",
        aliases: &["enableProfile"],
        handler: |server, params| Box::pin(server.dispatch_enable_profile(params)),
    },
    // Delete a built-in's override `.md` so the built-in definition is
    // restored (the delete channel's legacy cleanup rides along).
    Route {
        method: "agent.resetProfile",
        aliases: &["resetProfile"],
        handler: |server, params| Box::pin(server.dispatch_reset_profile(params)),
    },
];
