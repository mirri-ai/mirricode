//! Core/config/model/shell/btw routes — the 12 misc arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 4): the core info and
//! config surface, the model catalog reads, the shell stubs, and the
//! single btw arm. Doc comments moved with their arms; handlers and
//! inline values are unchanged.

use super::super::{invalid_params, normalize_params};
use super::Route;
use mirri_transport::adapter::EngineError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub(super) const ROUTES: &[Route] = &[
    // Version + engine identity.
    Route {
        method: "core.info",
        aliases: &["getCoreInfo"],
        handler: |server, _params| Box::pin(server.dispatch_get_core_info()),
    },
    // Experimental feature flags — the same registered-flag view
    // `flag.explainAll` serves (the TS wire contract is a bare array,
    // v1 `explainAll()` / v2 `flags.list()` — the client calls `.map` on
    // the result, so it must NOT be wrapped in an object).
    Route {
        method: "core.experimentalFeatures",
        aliases: &["getExperimentalFeatures"],
        handler: |server, _params| Box::pin(server.dispatch_flag_explain_all()),
    },
    // M5 drill (§7 criterion 7): a both-method walking the full
    // registry → handler → wire flow. Echoes the `text` param back as
    // `{echo: "<text>"}`; the serve wire rides the engine fall-through
    // with zero transport code.
    Route {
        method: "core.echo",
        aliases: &[],
        handler: |_server, params| Box::pin(async move { dispatch_core_echo(params) }),
    },
    // Engine identity as the TS `getMirriConfig`/`config.get` shape.
    Route {
        method: "config.get",
        aliases: &["getMirriConfig"],
        handler: |server, _params| Box::pin(server.dispatch_get_mirri_config()),
    },
    Route {
        method: "config.diagnostics",
        aliases: &["getConfigDiagnostics"],
        handler: |server, _params| Box::pin(async move { server.dispatch_config_diagnostics() }),
    },
    // Deep-merges the patch into config.toml and reloads the
    // in-memory snapshot; returns the updated wire shape.
    Route {
        method: "config.set",
        aliases: &["setMirriConfig"],
        handler: |server, params| Box::pin(server.dispatch_set_mirri_config(params)),
    },
    Route {
        method: "config.removeProvider",
        aliases: &["removeMirriProvider"],
        handler: |server, params| Box::pin(server.dispatch_remove_provider(params)),
    },
    Route {
        method: "config.removeModel",
        aliases: &["removeMirriModel"],
        handler: |server, params| Box::pin(server.dispatch_remove_model(params)),
    },
    Route {
        method: "model.list",
        aliases: &["listModels"],
        handler: |server, _params| Box::pin(server.dispatch_model_list()),
    },
    Route {
        method: "model.listProviders",
        aliases: &["listProviders"],
        handler: |server, _params| Box::pin(server.dispatch_model_list_providers()),
    },
    Route {
        method: "shell.run",
        aliases: &["runShellCommand"],
        handler: |server, params| Box::pin(server.dispatch_run_shell_command(params)),
    },
    // Cancels a running foreground `!` shell command by its command id (an
    // unknown id is a silent no-op, matching the TS semantics).
    Route {
        method: "shell.cancel",
        aliases: &["cancelShellCommand"],
        handler: |server, params| Box::pin(server.dispatch_cancel_shell_command(params)),
    },
    Route {
        method: "btw.start",
        aliases: &["startBtw"],
        handler: |server, params| Box::pin(server.dispatch_start_btw(params)),
    },
];

/// `core.echo` — echo the `text` param back as `{echo: "<text>"}`.
///
/// The M5 drill method (§7 criterion 7): typed serde params/result, no
/// session or engine state touched, so the same route answers identically
/// on the embedded transport and on the mirri-server serve wire through the
/// engine fall-through.
fn dispatch_core_echo(params: Value) -> Result<Value, EngineError> {
    #[derive(Debug, Deserialize)]
    struct EchoParams {
        text: String,
    }
    #[derive(Debug, Serialize)]
    struct EchoResult {
        echo: String,
    }

    let params: EchoParams = serde_json::from_value(normalize_params(params))
        .map_err(|e| invalid_params(e.to_string()))?;
    Ok(serde_json::to_value(EchoResult { echo: params.text })
        .expect("EchoResult is always serializable"))
}
