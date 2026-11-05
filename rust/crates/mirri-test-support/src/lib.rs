//! Parallel-safe test infrastructure for the mirri rust workspace.
//!
//! Single provider of: env mutex guard, fake HOME, wait_for helpers,
//! ephemeral listeners. See docs/specs/rust-core-design/16-test-execution-strategy.md.

pub mod env;
pub mod home;
pub mod port;
pub mod wait_for;
