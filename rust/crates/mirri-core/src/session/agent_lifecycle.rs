//! Agent lifecycle management within a session.
//!
//! [`AgentLifecycle`] wraps a [`SessionScope`] and provides create / fork /
//! remove / list operations for agents. It acts as a thin orchestration layer
//! over the session scope's agent collection, adding:
//!
//! - Automatic agent ID generation (UUID v4).
//! - Context copying on fork (deep-cloning the source agent's
//!   [`ContextMemory`]).
//! - Structured [`AgentInfo`] for introspection.
//!
//! The underlying [`SessionScope`] remains the source of truth for agent
//! storage; [`AgentLifecycle`] does not duplicate that state.

use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;

use crate::context_memory::ContextMemory;
use crate::scope::{AgentId, AgentScope, AgentScopeConfig, ProviderType, SessionScope};

use super::error::{SessionError, SessionResult};

/// Serializable summary of an agent, returned by [`AgentLifecycle::list_agents`].
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// The agent's unique identifier within its session.
    pub id: String,
    /// Model ID configured for this agent.
    pub model: String,
    /// Provider type as a string (`"anthropic"`, `"openai"`, `"custom"`).
    pub provider: String,
    /// When the agent was created (UTC).
    ///
    /// This is the time at which [`AgentLifecycle::create_agent`] was called,
    /// not a field stored in [`AgentScopeConfig`] itself. Since `AgentScope`
    /// does not currently track a creation timestamp, this value is set to
    /// the time of the `list_agents` call for agents created via
    /// [`AgentLifecycle`]. Agents created directly through [`SessionScope`]
    /// will appear with the current timestamp.
    pub created_at: chrono::DateTime<Utc>,
}

/// Manages agent lifecycle operations within a single session.
///
/// Each [`AgentLifecycle`] is bound to one [`SessionScope`]. It delegates
/// storage to the session scope and adds ID generation, context copying
/// (for fork), and structured info extraction.
pub struct AgentLifecycle {
    session: Arc<SessionScope>,
}

impl AgentLifecycle {
    /// Create a new agent lifecycle manager bound to the given session.
    pub fn new(session: Arc<SessionScope>) -> Self {
        Self { session }
    }

    /// Create a new agent with an auto-generated UUID ID and register it in
    /// the session.
    ///
    /// Returns the newly created [`AgentScope`] on success. Fails if the
    /// generated ID collides with an existing agent (astronomically unlikely
    /// with UUID v4).
    pub async fn create_agent(&self, config: AgentScopeConfig) -> SessionResult<Arc<AgentScope>> {
        let agent_id = AgentId::new(format!("agent-{}", Uuid::new_v4()));
        self.session
            .create_agent(agent_id, config)
            .await
            .map_err(|e| SessionError::AgentAlreadyExists(e.to_string()))
    }

    /// Create a new agent with a specific ID and register it in the session.
    ///
    /// Returns [`SessionError::AgentAlreadyExists`] if an agent with the
    /// given ID already exists.
    pub async fn create_agent_with_id(
        &self,
        agent_id: AgentId,
        config: AgentScopeConfig,
    ) -> SessionResult<Arc<AgentScope>> {
        self.session
            .create_agent(agent_id, config)
            .await
            .map_err(|e| SessionError::AgentAlreadyExists(e.to_string()))
    }

    /// Fork an agent — create a new agent whose context memory is a deep
    /// copy of the source agent's.
    ///
    /// The new agent gets a fresh UUID ID, the provided config (which may
    /// differ from the source agent's config — e.g. a different model), and
    /// a copy of all messages in the source agent's [`ContextMemory`].
    ///
    /// The source agent is not modified.
    pub async fn fork_agent(
        &self,
        source_agent: &Arc<AgentScope>,
        config: AgentScopeConfig,
    ) -> SessionResult<Arc<AgentScope>> {
        // Deep-copy the source agent's context memory into the new agent.
        let source_memory = source_agent.context_memory();
        let source_history = {
            let guard = source_memory.lock().expect("context memory mutex poisoned");
            guard.history().to_vec()
        };
        // A member fork (e.g. the BTW side channel) mints a fresh child id —
        // the new agent is a PEER of the source, never the session's main.
        let new_agent_id = AgentId::new(format!("agent-{}", Uuid::new_v4()));
        self.fork_agent_with_memory_as(source_agent, new_agent_id, config, move || {
            let mut memory = ContextMemory::new();
            for message in source_history {
                memory.append_message(message);
            }
            memory
        })
        .await
    }

    /// [`Self::fork_agent`] with the new agent's context memory supplied by
    /// the caller (the fork pipeline's turn-anchored rebuild, F2): the
    /// memory factory folds the forked wire ops — the same stream the
    /// fork's persisted log carries — instead of deep-copying the source
    /// agent's live memory, so the live fork and the persisted fork agree
    /// by construction. The source agent's config (e.g. its model) is still
    /// the seed; only the history comes from the factory.
    ///
    /// The forked session's primary agent lands under the SESSION MAIN agent
    /// id (`main`): the forked history is the session's main log (ADR-F5),
    /// every main-agent convention keys on that id (the wire's turn events
    /// carry it, the resume path rebuilds it, the web client streams main-
    /// agent transcript frames only), so a session fork that minted an
    /// `agent-<uuid>` instead would run its turns as a "background
    /// subagent" on every client surface.
    pub async fn fork_agent_with_memory(
        &self,
        _source_agent: &Arc<AgentScope>,
        config: AgentScopeConfig,
        build_memory: impl FnOnce() -> ContextMemory,
    ) -> SessionResult<Arc<AgentScope>> {
        self.fork_agent_with_memory_as(
            _source_agent,
            AgentId::new(crate::session::manager::MAIN_AGENT_ID),
            config,
            build_memory,
        )
        .await
    }

    /// [`Self::fork_agent_with_memory`] landing under an explicit agent id —
    /// the caller owns the new agent's identity (the session fork's `main`,
    /// or a member fork's fresh `agent-<uuid>`).
    pub async fn fork_agent_with_memory_as(
        &self,
        _source_agent: &Arc<AgentScope>,
        new_agent_id: AgentId,
        config: AgentScopeConfig,
        build_memory: impl FnOnce() -> ContextMemory,
    ) -> SessionResult<Arc<AgentScope>> {
        // Create the new agent in the session.
        let new_agent = self
            .session
            .create_agent(new_agent_id, config)
            .await
            .map_err(|e| SessionError::AgentAlreadyExists(e.to_string()))?;

        // Install the caller-built context memory into the new agent.
        let memory = build_memory();
        *new_agent
            .context_memory()
            .lock()
            .expect("context memory mutex poisoned") = memory;

        Ok(new_agent)
    }

    /// Remove an agent from the session.
    ///
    /// Returns [`SessionError::AgentNotFound`] if no agent with the given ID
    /// exists.
    pub async fn remove_agent(&self, agent_id: &AgentId) -> SessionResult<()> {
        self.session
            .remove_agent(agent_id)
            .await
            .ok_or_else(|| SessionError::AgentNotFound(agent_id.to_string()))?;
        Ok(())
    }

    /// Retrieve an agent by ID.
    pub async fn get_agent(&self, agent_id: &AgentId) -> Option<Arc<AgentScope>> {
        self.session.get_agent(agent_id).await
    }

    /// List all agents in the session with their summary info.
    ///
    /// Each [`AgentInfo`] includes the agent ID, model, provider, and a
    /// creation timestamp. Since [`AgentScope`] does not currently store a
    /// creation timestamp, `created_at` is set to the current time.
    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        let now = Utc::now();
        let mut infos = Vec::new();
        for id in self.session.list_agents().await {
            if let Some(agent) = self.session.get_agent(&id).await {
                let config = agent.config();
                let config_guard = config.read().unwrap_or_else(|e| e.into_inner());
                infos.push(AgentInfo {
                    id: id.as_str().to_string(),
                    model: config_guard.model_id.clone(),
                    provider: provider_to_string(config_guard.provider_type),
                    created_at: now,
                });
            }
        }
        infos
    }
}

/// Convert a [`ProviderType`] to its string representation for serialization.
fn provider_to_string(provider: ProviderType) -> String {
    match provider {
        ProviderType::Anthropic => "anthropic".to_string(),
        ProviderType::OpenAi => "openai".to_string(),
        ProviderType::OpenAiResponses => "openai-responses".to_string(),
        ProviderType::Google => "google".to_string(),
        ProviderType::Kimi => "kimi".to_string(),
        ProviderType::Custom => "custom".to_string(),
    }
}
