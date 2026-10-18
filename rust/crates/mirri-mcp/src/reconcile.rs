//! Pool reconcile diff — the pure planning step of the config → running
//! pool event chain (spec `docs/specs/mcp-architecture.md` §4.3).
//!
//! [`diff_pool_ops`] compares the pool's currently registered configs (the
//! runtime state) with a freshly loaded config layer's desired entries and
//! derives the incremental pool operations that move the pool to the
//! desired state:
//!
//! - an entry only in the desired config → [`PoolOp::Connect`] (a new
//!   server, or a re-enable carrying the new config),
//! - an entry only in the runtime state → [`PoolOp::Disconnect`] with
//!   `unregister: true` (the config entry was removed),
//! - an enable → disable transition → [`PoolOp::Disconnect`] with
//!   `unregister: false` (the entry stays registered in the `Disabled`
//!   state — `mcp.list` shows the flip — while the connection is torn
//!   down),
//! - a transport / credential change on an enabled server →
//!   [`PoolOp::Reconnect`] (disconnect + fresh connect),
//! - a tool-filter or timeout-only change → [`PoolOp::RefreshToolFilter`]
//!   (the stored config is swapped in place; the connection stays up — the
//!   manager applies filters and timeouts per call from the stored
//!   config),
//! - otherwise (identical, or both sides disabled) → no operation.
//!
//! The function is pure: no IO, no clocks, no ordering assumptions beyond
//! sorting its output by server name so consumers apply operations
//! deterministically.

use std::collections::HashMap;

use crate::config::McpServerConfig;

/// One incremental operation on a connection pool, derived by
/// [`diff_pool_ops`].
#[derive(Debug, Clone, PartialEq)]
pub enum PoolOp {
    /// Connect a server (added, or re-enabled) with the given config.
    Connect {
        /// Server name (the pool's registration key).
        name: String,
        /// The desired config to connect with.
        config: McpServerConfig,
    },
    /// Tear a server's connection down.
    Disconnect {
        /// Server name.
        name: String,
        /// `true` = the config entry is gone: unregister the entry
        /// entirely. `false` = only disabled: keep the entry registered in
        /// the `Disabled` state so status views keep showing it.
        unregister: bool,
    },
    /// Transport or credentials changed: disconnect, then connect fresh
    /// with the new config.
    Reconnect {
        /// Server name.
        name: String,
        /// The desired config to reconnect with.
        config: McpServerConfig,
    },
    /// Only the tool filters (or call timeouts) changed: swap the stored
    /// config in place and refresh tool registrations — the connection is
    /// not touched.
    RefreshToolFilter {
        /// Server name.
        name: String,
        /// The desired config (its filter/timeout fields are the change).
        config: McpServerConfig,
    },
}

/// Whether two configs agree on everything a live connection is bound to
/// (the transport endpoint and the auth material). Everything else — tool
/// filters, call timeouts, the enable flag — is read from the stored config
/// at use time and can be swapped without reconnecting.
fn same_connection_binding(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.transport == b.transport && a.auth == b.auth
}

/// Derive the incremental pool operations moving `old` (the pool's
/// registered configs) to `new` (a config layer's desired entries).
///
/// Both maps use the loader's canonical form (raw `${VAR}` references,
/// anchored stdio `cwd`), so the two sides are directly comparable.
pub fn diff_pool_ops(
    old: &HashMap<String, McpServerConfig>,
    new: &HashMap<String, McpServerConfig>,
) -> Vec<PoolOp> {
    let mut names: Vec<&String> = old.keys().chain(new.keys()).collect();
    names.sort();
    names.dedup();

    let mut ops = Vec::new();
    for name in names {
        match (old.get(name), new.get(name)) {
            (Some(old_config), Some(new_config)) => {
                match (old_config.is_enabled(), new_config.is_enabled()) {
                    (false, false) => {} // both disabled — nothing to move
                    (true, false) => ops.push(PoolOp::Disconnect {
                        name: name.clone(),
                        unregister: false,
                    }),
                    (false, true) => ops.push(PoolOp::Connect {
                        name: name.clone(),
                        config: new_config.clone(),
                    }),
                    (true, true) => {
                        if !same_connection_binding(old_config, new_config) {
                            ops.push(PoolOp::Reconnect {
                                name: name.clone(),
                                config: new_config.clone(),
                            });
                        } else if old_config != new_config {
                            // Only filters / call timeouts differ.
                            ops.push(PoolOp::RefreshToolFilter {
                                name: name.clone(),
                                config: new_config.clone(),
                            });
                        }
                    }
                }
            }
            (None, Some(new_config)) => ops.push(PoolOp::Connect {
                name: name.clone(),
                config: new_config.clone(),
            }),
            (Some(_), None) => ops.push(PoolOp::Disconnect {
                name: name.clone(),
                unregister: true,
            }),
            (None, None) => unreachable!("the name came from one of the two maps"),
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::McpTransportConfig;

    fn stdio_config(command: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: command.to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: None,
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: None,
            auth: None,
        }
    }

    /// Given a pool holding one server the desired config no longer
    /// defines, when the diff runs, then exactly one unregistering
    /// Disconnect is derived.
    #[test]
    fn should_derive_an_unregistering_disconnect_when_the_desired_config_drops_a_server() {
        let old = HashMap::from([("gone".to_string(), stdio_config("node"))]);
        let new = HashMap::new();

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::Disconnect {
                name: "gone".to_string(),
                unregister: true
            }]
        );
    }

    /// Given a desired config defining a server the pool does not hold,
    /// when the diff runs, then exactly one Connect carrying the new
    /// config is derived.
    #[test]
    fn should_derive_a_connect_when_the_desired_config_adds_a_server() {
        let old = HashMap::new();
        let mut added = stdio_config("node");
        added.enabled_tools = Some(vec!["only".to_string()]);
        let new = HashMap::from([("added".to_string(), added.clone())]);

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::Connect {
                name: "added".to_string(),
                config: added
            }]
        );
    }

    /// Given an enabled server whose desired config disables it, when the
    /// diff runs, then a non-unregistering Disconnect is derived (the entry
    /// stays registered as Disabled; only the connection goes down).
    #[test]
    fn should_derive_a_disabling_disconnect_when_the_desired_config_disables_a_server() {
        let old = HashMap::from([("flip".to_string(), stdio_config("node"))]);
        let mut desired = stdio_config("node");
        desired.enabled = Some(false);
        let new = HashMap::from([("flip".to_string(), desired)]);

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::Disconnect {
                name: "flip".to_string(),
                unregister: false
            }]
        );
    }

    /// Given a disabled registration whose desired config enables it, when
    /// the diff runs, then a Connect carrying the desired config is
    /// derived (the re-enable path reconnects with the new config, not the
    /// stale stored one).
    #[test]
    fn should_derive_a_connect_when_the_desired_config_reenables_a_server() {
        let mut disabled = stdio_config("node");
        disabled.enabled = Some(false);
        let old = HashMap::from([("back".to_string(), disabled)]);
        let new = HashMap::from([("back".to_string(), stdio_config("node"))]);

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::Connect {
                name: "back".to_string(),
                config: stdio_config("node")
            }]
        );
    }

    /// Given an enabled server whose transport changes in the desired
    /// config, when the diff runs, then a Reconnect is derived — the live
    /// connection is bound to the old endpoint and must be re-established.
    #[test]
    fn should_derive_a_reconnect_when_the_transport_changes() {
        let old = HashMap::from([("moved".to_string(), stdio_config("node-old"))]);
        let new = HashMap::from([("moved".to_string(), stdio_config("node-new"))]);

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::Reconnect {
                name: "moved".to_string(),
                config: stdio_config("node-new")
            }]
        );
    }

    /// Given an enabled server whose config changes only in its tool
    /// filters, when the diff runs, then a RefreshToolFilter is derived —
    /// the connection itself stays up.
    #[test]
    fn should_derive_a_refresh_when_only_the_tool_filter_changes() {
        let old = HashMap::from([("filter".to_string(), stdio_config("node"))]);
        let mut desired = stdio_config("node");
        desired.enabled_tools = Some(vec!["one".to_string()]);
        let new = HashMap::from([("filter".to_string(), desired.clone())]);

        let ops = diff_pool_ops(&old, &new);

        assert_eq!(
            ops,
            vec![PoolOp::RefreshToolFilter {
                name: "filter".to_string(),
                config: desired
            }]
        );
    }

    /// Given identical runtime and desired configs, when the diff runs,
    /// then no operation is derived.
    #[test]
    fn should_derive_nothing_when_the_configs_match() {
        let old = HashMap::from([
            ("alpha".to_string(), stdio_config("node-a")),
            ("beta".to_string(), stdio_config("node-b")),
        ]);
        let new = old.clone();

        let ops = diff_pool_ops(&old, &new);

        assert!(ops.is_empty(), "no-op diff must be empty: {ops:?}");
    }

    /// Given a server disabled on both sides whose transport changed, when
    /// the diff runs, then no operation is derived — a disabled entry has
    /// no live connection to rebind.
    #[test]
    fn should_derive_nothing_when_both_sides_are_disabled() {
        let mut disabled = stdio_config("node-old");
        disabled.enabled = Some(false);
        let old = HashMap::from([("down".to_string(), disabled)]);
        let mut desired = stdio_config("node-new");
        desired.enabled = Some(false);
        let new = HashMap::from([("down".to_string(), desired)]);

        let ops = diff_pool_ops(&old, &new);

        assert!(
            ops.is_empty(),
            "disabled-on-both-sides must be a no-op: {ops:?}"
        );
    }

    /// Given several changed servers, when the diff runs, then the
    /// operations come out sorted by server name (deterministic apply
    /// order for consumers and tests).
    #[test]
    fn should_emit_operations_sorted_by_server_name() {
        let old = HashMap::from([
            ("zulu".to_string(), stdio_config("node-z")),
            ("alpha".to_string(), stdio_config("node-a")),
        ]);
        let new = HashMap::from([("alpha".to_string(), stdio_config("node-a-new"))]);

        let ops = diff_pool_ops(&old, &new);

        let names: Vec<&str> = ops
            .iter()
            .map(|op| match op {
                PoolOp::Connect { name, .. }
                | PoolOp::Disconnect { name, .. }
                | PoolOp::Reconnect { name, .. }
                | PoolOp::RefreshToolFilter { name, .. } => name.as_str(),
            })
            .collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
    }
}
