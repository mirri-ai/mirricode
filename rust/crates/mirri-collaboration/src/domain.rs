//! Core domain model of a collaboration: the user-defined [`CollaborationMode`]
//! that declares which [`RoleDefinition`]s take part and which
//! [`CollaborationRules`] govern their interaction.
//!
//! A mode is a reusable template ("Scrum", "code-review team", ...). A running
//! instance of a mode is a [`crate::session::CollaborationSession`].

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{CollaborationError, CollaborationResult};

/// How agents in the collaboration communicate with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommunicationPattern {
    /// One agent finishes its turn before the next one starts.
    #[default]
    Sequential,
    /// Multiple agents work on different tasks at the same time.
    Parallel,
    /// Communication flows along a supervisor/subordinate hierarchy.
    Hierarchical,
}

/// Who is allowed to intervene in a running collaboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionPolicy {
    /// Only humans may send intervention directives.
    #[default]
    Human,
    /// Only the main agent (the parent session's agent) may intervene.
    MainAgent,
    /// Any agent in the collaboration may intervene in another's work.
    AnyAgent,
}

/// How conflicts between agents are resolved once they are escalated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// A human makes the final call (design doc §2.3, escalation Level 3).
    #[default]
    HumanArbitration,
    /// The main agent of the parent session arbitrates (Level 2).
    MainAgentArbitration,
    /// The conflict is put to a vote; the majority position wins.
    MajorityVote,
}

/// A role definition within a collaboration mode — one seat in the team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDefinition {
    /// Unique name of the role within the mode, e.g. `coordinator`.
    pub name: String,
    /// What the role is responsible for.
    pub description: String,
    /// System prompt injected into every agent serving this role.
    pub system_prompt: String,
    /// Tool/ability allow-list for the role, e.g. `["read", "write", "run_tests"]`.
    pub capabilities: Vec<String>,
    /// Upper bound on how many agents may serve this role concurrently.
    /// `None` means the mode places no per-role limit.
    pub max_agents: Option<usize>,
}

/// Rules governing a collaboration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CollaborationRules {
    /// How agents communicate: Sequential, Parallel, Hierarchical.
    pub communication_pattern: CommunicationPattern,
    /// Who can intervene: Human, MainAgent, AnyAgent.
    pub intervention_policy: InterventionPolicy,
    /// How conflicts are resolved: HumanArbitration, MainAgentArbitration,
    /// MajorityVote.
    pub conflict_resolution: ConflictResolution,
    /// Maximum number of rounds before the collaboration is force-terminated
    /// (loop guard, design doc §8.2). `None` means no round limit.
    pub max_rounds: Option<u32>,
}

/// A collaboration mode — the user-defined pattern for how agents work
/// together (design doc §2.1: one 协作模式 = roles + rules, instantiated per
/// collaboration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationMode {
    /// Stable identifier of the pattern, e.g. `code-review-team`.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// What this mode is for and when to use it.
    pub description: String,
    /// The roles that make up the team.
    pub roles: Vec<RoleDefinition>,
    /// The interaction rules every session of this mode must obey.
    pub rules: CollaborationRules,
}

impl CollaborationMode {
    /// Validates the mode against the design-doc validation rules (§5.2):
    ///
    /// - the role list must not be empty,
    /// - every role must have a non-empty, mode-unique name,
    /// - a role's `max_agents` must be at least 1 when present,
    /// - the rules' `max_rounds` must be at least 1 when present.
    ///
    /// A mode that fails this check must not be instantiated.
    pub fn validate(&self) -> CollaborationResult<()> {
        if self.roles.is_empty() {
            return Err(CollaborationError::InvalidMode(
                "must define at least one role".to_string(),
            ));
        }

        let mut seen_role_names = HashSet::new();
        for role in &self.roles {
            let name = role.name.trim();
            if name.is_empty() {
                return Err(CollaborationError::InvalidMode(
                    "every role must have a non-empty name".to_string(),
                ));
            }
            if !seen_role_names.insert(name.to_string()) {
                return Err(CollaborationError::InvalidMode(format!(
                    "role '{name}' is defined more than once"
                )));
            }
            if let Some(max_agents) = role.max_agents
                && max_agents == 0
            {
                return Err(CollaborationError::InvalidMode(format!(
                    "role '{name}' must allow at least one agent (max_agents = 0)"
                )));
            }
        }

        if let Some(max_rounds) = self.rules.max_rounds
            && max_rounds == 0
        {
            return Err(CollaborationError::InvalidMode(
                "max_rounds must be at least 1".to_string(),
            ));
        }

        Ok(())
    }
}
