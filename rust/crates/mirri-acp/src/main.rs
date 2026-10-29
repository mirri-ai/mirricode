//! mirri-acp: Agent Client Protocol host binary — thin compatibility entry
//! point.
//!
//! The runtime lives in the library (`mirri_acp::run`); users should prefer
//! `mirri acp` (the ACP subcommand of the main `mirri` CLI). This binary is
//! retained only because the conformance tests spawn it through
//! `env!("CARGO_BIN_EXE_mirri-acp")` (tests/conformance.rs).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mirri_acp::run().await
}
