//! External credential sources (C — Azure CLI first). An `external` store
//! entry names a method (`method_id`) plus metadata; at call time the
//! resolver shells out to the method's CLI to mint a fresh token — the
//! CLI's own token cache makes this a millisecond-local call, so a plain
//! synchronous [`std::process::Command`] is the right shape here (no
//! engine runtime, no async surface).
//!
//! Process execution lives only in the [`obtain_external`] thin shell; all
//! decision logic (arg assembly, output parsing, guidance wording) is the
//! injectable [`obtain_with`], fully unit-tested without an `az` binary.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

/// The only supported external method in this batch (C.1): the Azure CLI.
pub const AZURE_CLI: &str = "azure-cli";

/// An external source failed to produce a token — `method_id` names the
/// failed source, `guidance` is the user-facing action text (C.3: always
/// tell the user what to do next, e.g. run `az login`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSourceError {
    pub method_id: String,
    pub guidance: String,
}

/// Extract the `accessToken` field from `az account get-access-token
/// --output json` stdout — `None` on any non-string, empty, or unparseable
/// output (a partial answer is never a token).
pub fn parse_az_token(stdout: &str) -> Option<String> {
    let token = serde_json::from_str::<Value>(stdout)
        .ok()?
        .get("accessToken")?
        .as_str()?
        .trim()
        .to_string();
    (!token.is_empty()).then_some(token)
}

/// Assemble the `az account get-access-token` argument list for the
/// metadata's `resource_name` — empty when no usable resource is present
/// (the caller turns that into re-register guidance).
pub fn az_args(metadata: &HashMap<String, Value>) -> Vec<String> {
    let Some(resource) = metadata
        .get("resource_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    [
        "account",
        "get-access-token",
        "--resource",
        resource,
        "--output",
        "json",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Whether the external method's CLI is expected on this machine: a known
/// `method_id` whose `az` binary is reachable on `PATH` (a `which`-style
/// probe that spawns nothing). Unknown method ids are simply unavailable.
pub fn external_available(method_id: &str) -> bool {
    if method_id != AZURE_CLI {
        return false;
    }
    which_az().is_some()
}

/// Resolve the `az` executable path — `None` when not on `PATH`. Spawned
/// via `env PATH` lookup only; no token, no login state is touched.
fn which_az() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("az"))
        .find(|candidate| candidate.is_file())
}

/// The full external-source resolution against an injected process
/// runner: the runner receives the assembled `az` argument list and
/// returns its stdout (or an error message). Every failure maps to an
/// [`ExternalSourceError`] whose guidance names the user's next action.
pub fn obtain_with(
    runner: &dyn Fn(&[String]) -> Result<String, String>,
    metadata: &HashMap<String, Value>,
) -> Result<String, ExternalSourceError> {
    let args = az_args(metadata);
    if args.is_empty() {
        return Err(ExternalSourceError {
            method_id: AZURE_CLI.to_string(),
            guidance: "Azure resource name is required; re-register the credential with a \
                       non-empty resource_name"
                .to_string(),
        });
    }
    let stdout = runner(&args).map_err(|e| ExternalSourceError {
        method_id: AZURE_CLI.to_string(),
        guidance: format!("Azure CLI unavailable or not logged in ({e}); run `az login` first"),
    })?;
    parse_az_token(&stdout).ok_or_else(|| ExternalSourceError {
        method_id: AZURE_CLI.to_string(),
        guidance: "Azure CLI returned an unreadable token; run `az login` and retry".to_string(),
    })
}

/// The real-process shell over [`obtain_with`]: run `az` with the
/// assembled arguments, capture stdout, and resolve through the shared
/// logic. An unknown `method_id` fails with guidance instead of guessing.
pub fn obtain_external(
    method_id: &str,
    metadata: &HashMap<String, Value>,
) -> Result<String, ExternalSourceError> {
    if method_id != AZURE_CLI {
        return Err(ExternalSourceError {
            method_id: method_id.to_string(),
            guidance: format!("unknown external credential method '{method_id}'"),
        });
    }
    let runner = |args: &[String]| -> Result<String, String> {
        let output = Command::new("az")
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            String::from_utf8(output.stdout).map_err(|e| e.to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "az exited with {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ))
        }
    };
    obtain_with(&runner, metadata)
}
