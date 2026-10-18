//! [`LlmFactory`] re-export — the provider-construction hook of the engine.
//!
//! The trait itself is defined once in `mirri-core` (next to [`Llm`], the
//! types it is built from); this module re-exports it so the historical
//! import path (`mirri_embedded::llm_factory::LlmFactory` /
//! `mirri_embedded::LlmFactory`) keeps compiling after the M5 unification
//! hoisted it. `mirri-server`'s `state` module re-exports the same trait.

use std::sync::Arc;

use mirri_core::{AgentScopeConfig, Llm};

pub use mirri_core::LlmFactory;

/// Adapter that bridges `LlmFactory` → `SubagentLlmFactory`.
///
/// `SessionSubagentHost` needs a `SubagentLlmFactory` to create LLM
/// providers for child agents. Since the embedded server already holds
/// an `LlmFactory`, this thin wrapper delegates without requiring callers
/// to implement a second trait.
pub struct SubagentLlmAdapter(pub Arc<dyn LlmFactory>);

impl mirri_core::SubagentLlmFactory for SubagentLlmAdapter {
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        self.0.create_llm(config)
    }
}
