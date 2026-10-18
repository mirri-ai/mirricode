//! ACP v1 `mcpServers` → engine overlay parameter conversion.
//!
//! ACP v1's `McpServer` union has four variants, and only three of them
//! carry a `type` discriminant (`http`, `sse`, `acp`) — **stdio is
//! identified by the absence of `type`** (the same trap the TS adapter
//! documents in `packages/acp-adapter/src/mcp.ts`; the vendored official
//! schema snapshot's stdio arm has no `type` const either). The
//! conversion branches accordingly.
//!
//! Semantics (TS `acpMcpServersToConfigs` parity):
//!
//! - `http` / `sse` → the corresponding transport with the header array
//!   (`[{name, value}]`) projected onto a record;
//! - no `type` → stdio with `command` / `args` / the env array projected
//!   onto a record;
//! - `acp` or any unknown type → **warn-drop**: the entry is logged and
//!   skipped, never failing the request;
//! - same-name entries are last-write-wins;
//! - malformed entries (missing name / command / url) warn-drop too.
//!
//! The output is the engine's overlay parameter shape — one
//! `{"name": …, "config": <McpServerConfig>}` object per server, passed
//! through `session.create` / `session.resume`'s `mcpServers` field (the
//! embedded engine parses each entry into a
//! [`McpServerConfig`](mirri_mcp::McpServerConfig)).

use serde_json::{Value, json};

/// Convert the ACP `mcpServers` request parameter (v1 `McpServer[]`) into
/// the engine's overlay parameter entries (`{name, config}[]`),
/// warn-dropping every unsupported or malformed entry (see the module
/// docs). An absent / null / non-array parameter converts to no entries.
pub(crate) fn overlay_entries(mcp_servers: Option<&Value>) -> Vec<Value> {
    let Some(entries) = mcp_servers.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut converted: Vec<(String, Value)> = Vec::new();
    for entry in entries {
        match convert_one(entry) {
            Some((name, config)) => {
                match converted.iter_mut().find(|(existing, _)| existing == &name) {
                    // Last-write-wins on duplicate names (mcp.ts parity).
                    Some((_, existing_config)) => *existing_config = config,
                    None => converted.push((name, config)),
                }
            }
            None => continue, // warn-dropped
        }
    }
    converted
        .into_iter()
        .map(|(name, config)| json!({"name": name, "config": config}))
        .collect()
}

/// Convert one v1 `McpServer` entry into `(name, McpServerConfig json)`,
/// or `None` when the entry must be warn-dropped.
fn convert_one(entry: &Value) -> Option<(String, Value)> {
    let name = entry.get("name").and_then(Value::as_str)?.to_string();
    // Only http / sse / acp carry an explicit `type`; stdio is identified
    // by its absence (see the module docs).
    let transport_type = entry.get("type").and_then(Value::as_str);
    let config = match transport_type {
        None => stdio_config(entry)?,
        Some("http") => remote_config(entry, "http")?,
        Some("sse") => remote_config(entry, "sse")?,
        Some(other) => {
            // `acp` and any future transport are not supported by this
            // host: warn-drop, never fail the request (the session is
            // still created).
            tracing::warn!(server = %name, r#type = %other,
                "dropping unsupported MCP server transport from mcpServers");
            return None;
        }
    };
    Some((name, config))
}

/// The stdio config (v1 `McpServerStdio` — no `type` field). `command`
/// is required; a missing one warn-drops the entry.
fn stdio_config(entry: &Value) -> Option<Value> {
    let command = entry.get("command").and_then(Value::as_str)?;
    Some(json!({
        "transport": {
            "type": "stdio",
            "command": command,
            "args": string_array(entry.get("args")),
            "env": name_value_array_to_record(entry.get("env")),
        }
    }))
}

/// The http / sse config (v1 `McpServerHttp` / `McpServerSse`). `url` is
/// required; a missing one warn-drops the entry.
fn remote_config(entry: &Value, transport: &str) -> Option<Value> {
    let url = entry.get("url").and_then(Value::as_str)?;
    Some(json!({
        "transport": {
            "type": transport,
            "url": url,
            "headers": name_value_array_to_record(entry.get("headers")),
        }
    }))
}

/// Project a v1 `[{name, value}]` array onto a JSON object record.
/// Malformed items are skipped; an absent array yields an empty record.
fn name_value_array_to_record(array: Option<&Value>) -> Value {
    let mut record = serde_json::Map::new();
    if let Some(items) = array.and_then(Value::as_array) {
        for item in items {
            let (Some(name), Some(value)) = (
                item.get("name").and_then(Value::as_str),
                item.get("value").and_then(Value::as_str),
            ) else {
                continue;
            };
            record.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    Value::Object(record)
}

/// Project a v1 string array; an absent array yields an empty one.
fn string_array(array: Option<&Value>) -> Vec<String> {
    array
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v1 stdio entry (no `type` — identified by absence) converts to
    /// the stdio transport with the env array projected onto a record.
    #[test]
    fn should_convert_stdio_server_identified_by_absent_type() {
        let entries = overlay_entries(Some(&json!([{
            "name": "echo",
            "command": "/bin/echo-mcp",
            "args": ["--verbose"],
            "env": [{"name": "FOO", "value": "1"}, {"name": "BAR", "value": "2"}],
        }])));

        assert_eq!(
            entries,
            vec![json!({
                "name": "echo",
                "config": {
                    "transport": {
                        "type": "stdio",
                        "command": "/bin/echo-mcp",
                        "args": ["--verbose"],
                        "env": {"FOO": "1", "BAR": "2"},
                    }
                }
            })]
        );
    }

    /// http / sse entries convert with their header arrays projected.
    #[test]
    fn should_convert_http_and_sse_servers_with_projected_headers() {
        let entries = overlay_entries(Some(&json!([
            {"type": "http", "name": "api", "url": "https://example/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer x"}]},
            {"type": "sse", "name": "legacy", "url": "https://example/sse", "headers": []},
        ])));

        assert_eq!(
            entries,
            vec![
                json!({
                    "name": "api",
                    "config": {
                        "transport": {
                            "type": "http",
                            "url": "https://example/mcp",
                            "headers": {"Authorization": "Bearer x"},
                        }
                    }
                }),
                json!({
                    "name": "legacy",
                    "config": {
                        "transport": {"type": "sse", "url": "https://example/sse", "headers": {}}
                    }
                }),
            ]
        );
    }

    /// `acp` and unknown transports warn-drop; so do malformed entries
    /// (missing name / command / url). The rest still converts.
    #[test]
    fn should_warn_drop_acp_unknown_and_malformed_entries() {
        let entries = overlay_entries(Some(&json!([
            {"type": "acp", "name": "acp-one", "serverId": "component-1"},
            {"type": "websocket", "name": "future", "url": "ws://nowhere"},
            {"name": "no-command"},
            {"type": "http", "name": "no-url", "headers": []},
            {"command": "/bin/echo-mcp", "args": []},
            {"name": "echo", "command": "/bin/echo-mcp", "args": []},
        ])));

        assert_eq!(
            entries,
            vec![json!({
                "name": "echo",
                "config": {
                    "transport": {
                        "type": "stdio",
                        "command": "/bin/echo-mcp",
                        "args": [],
                        "env": {},
                    }
                }
            })],
            "only the well-formed stdio entry may survive"
        );
    }

    /// Same-name entries are last-write-wins.
    #[test]
    fn should_keep_the_last_write_for_duplicate_names() {
        let entries = overlay_entries(Some(&json!([
            {"name": "echo", "command": "/bin/first", "args": []},
            {"name": "echo", "command": "/bin/second", "args": []},
        ])));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["config"]["transport"]["command"], "/bin/second");
    }

    /// An absent or null `mcpServers` converts to no entries.
    #[test]
    fn should_convert_absent_mcp_servers_to_no_entries() {
        assert!(overlay_entries(None).is_empty());
        assert!(overlay_entries(Some(&Value::Null)).is_empty());
    }
}
