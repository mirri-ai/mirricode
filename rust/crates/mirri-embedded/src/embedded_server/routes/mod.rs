//! Static route table: every embedded RPC method as registry data.
//!
//! The M3 migration replaced `EngineAdapter::call_method`'s hand-written
//! 95-arm match with a static table of [`Route`]s, domain by domain. Each
//! route pairs its canonical dotted protocol name (the mirri-proto
//! authoritative key) with the legacy TS-client camelCase wire aliases the
//! old match arm also accepted, plus the handler that arm dispatched to.
//! Behavior is byte-for-byte identical to the match: the same names route
//! to the same calls, only the plumbing changed.
//!
//! The migration is complete: all 106 embedded methods (95 migrated arms +
//! the M5-3b `core.echo` drill route + the five B1 session additions +
//! the five B2 gap routes, including `agent.activity`) are table-driven
//! and no residual
//! match remains in `super`. [`ALL_ROUTES`] holds the per-domain tables;
//! [`all_routes`] iterates every route across them. The B5-a shallow
//! batch added four more domain tables (`env`, `flag`, `fs`, `provider`);
//! the B5-b shallow batch B added two (`config`, `model`); the B5-c
//! deep batch added the `model.generate` route to the model table; the
//! B5-d deep batch added the `auth` table. Later MCP batches keep
//! extending the `mcp` table (the P1 config surface, the P2 pool
//! surface) — the current total is pinned by the route-count tests.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use serde_json::Value;

use super::{EmbeddedServer, not_implemented};
use mirri_transport::adapter::EngineError;

mod agent;
mod approval;
mod collaboration;
mod config;
mod core;
mod env;
mod flag;
mod fs;
mod goal;
mod interaction;
mod mcp;
mod message;
mod model;
mod plan;
mod plugin;
mod profile;
mod provider;
mod question;
mod session;
mod swarm;
mod task;
mod workspace;

/// One embedded RPC route: the handler plus every wire name it answers to.
pub(super) struct Route {
    /// Canonical dotted protocol name — the mirri-proto authoritative key.
    pub method: &'static str,
    /// Legacy TS-client camelCase wire aliases routed to the same handler.
    pub aliases: &'static [&'static str],
    /// The dispatch function invoked with the server and the raw params.
    pub handler: HandlerFn,
}

/// A dispatch function: borrows the server, owns the params, returns a
/// boxed future. Every arm of the old match had one of these shapes; the
/// table unifies them behind a single signature.
pub(super) type HandlerFn =
    for<'a> fn(
        &'a EmbeddedServer,
        Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, EngineError>> + Send + 'a>>;

/// Every route, grouped by domain in the order the domains' sections
/// appeared in the original match (session and mcp first, then batch 3's
/// agent and message, then batch 4's core/config/model/shell/btw and
/// plugin, finally batch 5's task/cron, plan, goal, and swarm). Const
/// slice concatenation is not expressible on stable Rust, so the tables
/// stay nested and callers flatten via [`all_routes`].
pub(super) const ALL_ROUTES: &[&[Route]] = &[
    session::ROUTES,
    mcp::ROUTES,
    agent::ROUTES,
    profile::ROUTES,
    message::ROUTES,
    core::ROUTES,
    config::ROUTES,
    model::ROUTES,
    plugin::ROUTES,
    task::ROUTES,
    plan::ROUTES,
    goal::ROUTES,
    swarm::ROUTES,
    collaboration::ROUTES,
    approval::ROUTES,
    question::ROUTES,
    interaction::ROUTES,
    workspace::ROUTES,
    env::ROUTES,
    flag::ROUTES,
    fs::ROUTES,
    provider::ROUTES,
];

/// Iterate every route across all domain tables.
pub(super) fn all_routes() -> impl Iterator<Item = &'static Route> {
    ALL_ROUTES.iter().copied().flatten()
}

/// Look up a route by any of its wire names (canonical or alias).
pub(super) fn find_route(method: &str) -> Option<&'static Route> {
    static INDEX: OnceLock<HashMap<&'static str, &'static Route>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        all_routes()
            .flat_map(|route| {
                std::iter::once((route.method, route))
                    .chain(route.aliases.iter().map(move |alias| (*alias, route)))
            })
            .collect()
    });
    index.get(method).copied()
}
