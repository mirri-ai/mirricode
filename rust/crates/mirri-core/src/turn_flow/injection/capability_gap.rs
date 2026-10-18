//! Capability-gap hint injection: suggests subagent dispatch when the model
//! lacks a modality the user prompt contains.

use super::*;

/// An [`Injector`] that detects modality gaps and suggests subagent dispatch.
///
/// Ports the TypeScript `capability_gap` context-injection provider: when the
/// current model's capabilities lack a modality (image / video / audio) that
/// the last real user message contains, and a feasible alternative exists —
/// either a known profile's model or a model in the catalog snapshot covers
/// that modality (an empty snapshot is permissive) — it returns a
/// capability-oriented hint naming the "Available models" list, with
/// covering profile names as a parenthetical convenience.
///
/// Mirrors the TS semantics of firing once per new user prompt: the hint is
/// only produced when no previous injection with the same text exists after
/// the last user message, so per-step injection does not spam the context.
pub struct CapabilityGapInjector {
    /// Capability flags of the model currently driving the loop.
    model_capabilities: ModelCapabilities,
    /// Known agent profiles, used to find alternatives models for gaps.
    profiles: Vec<ProfileInfo>,
    /// Snapshot of model-catalog capabilities for feasibility scanning; an
    /// empty snapshot is permissive (standalone flows cannot verify).
    catalog: Vec<ModelCapabilities>,
}

impl CapabilityGapInjector {
    /// Create a capability-gap injector for the given model capabilities,
    /// known profiles, and catalog capability snapshot.
    ///
    /// Profiles and catalog are snapshotted at construction time; rebuild the
    /// injector when the profile set or model catalog changes. Model
    /// capabilities can be obtained from an [`Llm`](crate::llm::Llm) adapter
    /// via [`ModelCapabilities::from`]`(&llm.capability())`; the catalog
    /// snapshot via [`catalog_capabilities`](crate::capability_gap::catalog_capabilities).
    pub fn new(
        model_capabilities: ModelCapabilities,
        profiles: Vec<ProfileInfo>,
        catalog: Vec<ModelCapabilities>,
    ) -> Self {
        Self {
            model_capabilities,
            profiles,
            catalog,
        }
    }
}

impl Injector for CapabilityGapInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        let history = memory.history();

        // Find the last *real* user message. Injection reminders are also
        // user-role messages, so they must be excluded — otherwise the hint
        // would analyze its own previous output.
        let last_user_idx = history.iter().rposition(|m| {
            m.message.role == Role::User
                && m.origin
                    .as_ref()
                    .is_none_or(|origin| origin.is_real_user_input())
        })?;

        let user_parts = &history[last_user_idx].message.content;
        let gaps = CapabilityGapDetector::detect_gaps(&self.model_capabilities, user_parts);
        if gaps.is_empty() {
            return None;
        }

        let hint = CapabilityGapDetector::build_hint(
            &gaps,
            &self.profiles,
            &self.catalog,
            &extract_attachment_paths(user_parts),
        )?;

        // Fire at most once per user prompt: skip when this exact hint was
        // already injected after the last user message. (Equals the TS
        // `isNewTurn` gating for a `before_step`-driven injector.)
        let already_injected = history[last_user_idx + 1..].iter().any(|m| {
            m.origin == Some(PromptOrigin::Injection) && m.message.text_content().contains(&hint)
        });
        if already_injected {
            return None;
        }

        tracing::debug!(
            model = ?self.model_capabilities,
            "capability gap hint produced"
        );
        Some(hint)
    }
}
