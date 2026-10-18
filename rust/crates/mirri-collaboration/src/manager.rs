//! The collaboration manager: owns the sessions and mediates every protocol
//! operation.

use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::domain::CollaborationMode;
use crate::error::{CollaborationError, CollaborationResult};
use crate::protocol::{AgentMessage, AgentStatus, TaskStatus};
use crate::session::{CollaborationSession, CollaborationState};

/// Manages the lifecycle of [`CollaborationSession`]s.
///
/// All state lives behind a lock, so a single manager can be shared between
/// tasks and threads. Reads ([`Self::messages`], [`Self::state`], ...) take a
/// read lock; operations that mutate the session ([`Self::send_message`],
/// [`Self::intervene`], ...) take a write lock.
pub struct CollaborationManager {
    sessions: RwLock<HashMap<Uuid, CollaborationSession>>,
}

impl Default for CollaborationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CollaborationManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Start a new collaboration session with the given mode, working toward
    /// `task` as the shared objective.
    ///
    /// The session starts in [`CollaborationState::InProgress`] with one
    /// [`crate::session::AgentHandle`] per role of the mode.
    ///
    /// # Errors
    ///
    /// Returns [`CollaborationError::InvalidMode`] when `mode` fails
    /// [`CollaborationMode::validate`]: a session must never be started from
    /// an invalid mode, and callers handle the rejection instead of the
    /// process panicking.
    pub fn start(&self, mode: CollaborationMode, task: String) -> CollaborationResult<Uuid> {
        mode.validate()?;

        let session = CollaborationSession::new(Uuid::new_v4(), mode, task);
        let session_id = session.id;
        self.write_lock().insert(session_id, session);
        info!(session_id = %session_id, "collaboration session started");
        Ok(session_id)
    }

    /// Send a message from one agent to another.
    ///
    /// Validation and side effects, in order:
    ///
    /// 1. the session must exist and be in a state that accepts traffic
    ///    ([`CollaborationState::InProgress`]);
    /// 2. the sender — and the recipient, when the message type has one —
    ///    must be agents of the session;
    /// 3. the message is recorded in the session's message log;
    /// 4. status side effects are applied:
    ///    - a `TaskAssignment` marks its recipient `Working`;
    ///    - a `TaskResult` maps its `TaskStatus` onto the sender
    ///      (Completed → Done, Failed → Error, InProgress → Working,
    ///      Blocked → Waiting) and advances the round counter, which trips
    ///      the round-limit loop guard if the mode's `max_rounds` is exceeded
    ///      (the session is then force-terminated into `Failed`);
    ///    - a `StatusUpdate` sets the sender's status;
    ///    - a `ConflictEscalation` pauses the session until the dispute is
    ///      resolved;
    /// 5. once every agent has status `Done`, the session completes.
    pub fn send_message(&self, session_id: Uuid, message: AgentMessage) -> CollaborationResult<()> {
        let mut sessions = self.write_lock();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;

        match &session.state {
            CollaborationState::InProgress { .. } | CollaborationState::NotStarted => {}
            state => {
                return Err(CollaborationError::InvalidState {
                    operation: "send an agent message",
                    state: state.clone(),
                });
            }
        }

        let sender = message.sender().ok_or_else(|| {
            CollaborationError::ProtocolViolation(format!(
                "message type '{}' carries no sender and cannot be sent between agents",
                message.message_type()
            ))
        })?;
        ensure_agent(session, sender)?;
        if let Some(recipient) = message.recipient() {
            ensure_agent(session, recipient)?;
        }

        apply_message_effects(session, session_id, &message);
        debug!(
            session_id = %session_id,
            message_type = message.message_type(),
            "agent message recorded in collaboration log"
        );
        session.message_log.push(message);
        complete_when_all_done(session, session_id);
        Ok(())
    }

    /// Get all messages in a session, in arrival order.
    pub fn messages(&self, session_id: Uuid) -> CollaborationResult<Vec<AgentMessage>> {
        let sessions = self.read_lock();
        let session = sessions
            .get(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;
        Ok(session.message_log.clone())
    }

    /// Get the current state of a session.
    pub fn state(&self, session_id: Uuid) -> CollaborationResult<CollaborationState> {
        let sessions = self.read_lock();
        let session = sessions
            .get(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;
        Ok(session.state.clone())
    }

    /// Get a snapshot of a session (mode, agents, message log, state).
    pub fn session(&self, session_id: Uuid) -> CollaborationResult<CollaborationSession> {
        let sessions = self.read_lock();
        sessions
            .get(&session_id)
            .cloned()
            .ok_or(CollaborationError::SessionNotFound { session_id })
    }

    /// Get the current status of one agent in a session.
    pub fn agent_status(&self, session_id: Uuid, agent: &str) -> CollaborationResult<AgentStatus> {
        let sessions = self.read_lock();
        let session = sessions
            .get(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;
        session
            .agents
            .get(agent)
            .map(|handle| handle.status)
            .ok_or_else(|| CollaborationError::AgentNotFound {
                agent: agent.to_string(),
            })
    }

    /// Human/main-agent intervention: inject a directive into one agent's
    /// context (design doc §7.23, directed intervention).
    ///
    /// The directive is recorded in the message log as an
    /// [`AgentMessage::Intervention`] from `"human"`. It does not change the
    /// collaboration state — a directive steers the target agent's next turn
    /// rather than pausing the collaboration. Interventions are rejected once
    /// the session is `Completed` or `Failed`.
    pub fn intervene(
        &self,
        session_id: Uuid,
        target: &str,
        directive: &str,
    ) -> CollaborationResult<()> {
        let mut sessions = self.write_lock();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;

        match &session.state {
            CollaborationState::Completed | CollaborationState::Failed(_) => {
                return Err(CollaborationError::InvalidState {
                    operation: "intervene in",
                    state: session.state.clone(),
                });
            }
            _ => {}
        }
        ensure_agent(session, target)?;

        session.message_log.push(AgentMessage::Intervention {
            from: "human".to_string(),
            to: target.to_string(),
            directive: directive.to_string(),
        });
        debug!(
            session_id = %session_id,
            target,
            "intervention directive recorded in collaboration log"
        );
        Ok(())
    }

    /// Request conflict resolution (design doc §7.24, escalation request).
    ///
    /// The escalation is recorded in the message log and the session moves to
    /// [`CollaborationState::Paused`] — the dispute is now awaiting
    /// arbitration (in this crate's simplified escalation chain, the human is
    /// the terminal arbiter). The sender and every party must be agents of
    /// the session.
    pub fn escalate_conflict(
        &self,
        session_id: Uuid,
        from: &str,
        description: &str,
        parties: &[String],
    ) -> CollaborationResult<()> {
        let mut sessions = self.write_lock();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;

        match &session.state {
            CollaborationState::NotStarted
            | CollaborationState::InProgress { .. }
            | CollaborationState::Paused => {}
            state => {
                return Err(CollaborationError::InvalidState {
                    operation: "escalate a conflict in",
                    state: state.clone(),
                });
            }
        }

        ensure_agent(session, from)?;
        for party in parties {
            ensure_agent(session, party)?;
        }

        session.state = CollaborationState::Paused;
        session.message_log.push(AgentMessage::ConflictEscalation {
            from: from.to_string(),
            description: description.to_string(),
            parties: parties.to_vec(),
        });
        warn!(
            session_id = %session_id,
            escalation_from = from,
            "conflict escalated — collaboration paused until arbitration resolves it"
        );
        Ok(())
    }

    /// Terminate a session: mark it [`CollaborationState::Failed`] with reason
    /// `"terminated"`.
    ///
    /// Only sessions that are not already `Completed` or `Failed` can be
    /// terminated. The message log stays readable afterwards.
    pub fn terminate(&self, session_id: Uuid) -> CollaborationResult<()> {
        let mut sessions = self.write_lock();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(CollaborationError::SessionNotFound { session_id })?;

        match &session.state {
            CollaborationState::Completed | CollaborationState::Failed(_) => {
                return Err(CollaborationError::InvalidState {
                    operation: "terminate",
                    state: session.state.clone(),
                });
            }
            _ => {}
        }

        session.state = CollaborationState::Failed("terminated".to_string());
        info!(session_id = %session_id, "collaboration session terminated");
        Ok(())
    }

    fn read_lock(&self) -> RwLockReadGuard<'_, HashMap<Uuid, CollaborationSession>> {
        self.sessions.read().unwrap_or_else(|poisoned| {
            warn!("collaboration manager read lock was poisoned; recovering tainted state");
            poisoned.into_inner()
        })
    }

    fn write_lock(&self) -> RwLockWriteGuard<'_, HashMap<Uuid, CollaborationSession>> {
        self.sessions.write().unwrap_or_else(|poisoned| {
            warn!("collaboration manager write lock was poisoned; recovering tainted state");
            poisoned.into_inner()
        })
    }
}

/// Fails unless `agent` is one of the session's agents. The error lists the
/// available agent (role) names so a misaddressed message can be corrected
/// without guessing.
fn ensure_agent(session: &CollaborationSession, agent: &str) -> CollaborationResult<()> {
    if session.agents.contains_key(agent) {
        Ok(())
    } else {
        let mut names: Vec<&str> = session.agents.keys().map(String::as_str).collect();
        names.sort_unstable();
        Err(CollaborationError::ProtocolViolation(format!(
            "agent '{}' is not part of this collaboration session; available agents: {}",
            agent,
            names.join(", ")
        )))
    }
}

/// Sets an agent's status; panics only if the agent vanished from the map,
/// which cannot happen because the caller validated the agent first.
fn set_agent_status(session: &mut CollaborationSession, agent: &str, status: AgentStatus) {
    let handle = session
        .agents
        .get_mut(agent)
        .expect("agent was validated to be part of the session");
    handle.status = status;
}

/// Maps a task status onto the agent status of the agent that executed it.
fn agent_status_for(task_status: TaskStatus) -> AgentStatus {
    match task_status {
        TaskStatus::Completed => AgentStatus::Done,
        TaskStatus::Failed => AgentStatus::Error,
        TaskStatus::InProgress => AgentStatus::Working,
        TaskStatus::Blocked => AgentStatus::Waiting,
    }
}

/// Applies the status side effects of a message on the session state machine.
fn apply_message_effects(
    session: &mut CollaborationSession,
    session_id: Uuid,
    message: &AgentMessage,
) {
    match message {
        AgentMessage::TaskAssignment { to, .. } => {
            set_agent_status(session, to, AgentStatus::Working);
        }
        AgentMessage::TaskResult { from, status, .. } => {
            set_agent_status(session, from, agent_status_for(*status));
            advance_round(session, session_id);
        }
        AgentMessage::StatusUpdate { from, status } => {
            set_agent_status(session, from, *status);
        }
        AgentMessage::ConflictEscalation { .. } => {
            session.state = CollaborationState::Paused;
            warn!(
                session_id = %session_id,
                "conflict escalation received — collaboration paused until arbitration resolves it"
            );
        }
        AgentMessage::Question { .. }
        | AgentMessage::Answer { .. }
        | AgentMessage::Intervention { .. }
        | AgentMessage::Unknown => {}
    }
}

/// Advances the session's round counter after a completed task-result round.
///
/// When the mode's `max_rounds` limit would be exceeded, the collaboration is
/// force-terminated into [`CollaborationState::Failed`] — the loop guard of
/// the design doc (§8.2).
fn advance_round(session: &mut CollaborationSession, session_id: Uuid) {
    let CollaborationState::InProgress { round } = session.state else {
        // E.g. already force-terminated by an earlier message in this exchange.
        return;
    };
    let next_round = round + 1;
    if let Some(max_rounds) = session.mode.rules.max_rounds
        && next_round > max_rounds
    {
        session.state = CollaborationState::Failed(format!("max_rounds ({max_rounds}) exceeded"));
        warn!(
            session_id = %session_id,
            max_rounds,
            "round limit reached — collaboration force-terminated by the loop guard"
        );
        return;
    }
    session.state = CollaborationState::InProgress { round: next_round };
    debug!(session_id = %session_id, round = next_round, "round advanced");
}

/// Completes the session once every agent has status [`AgentStatus::Done`].
///
/// Only transitions from [`CollaborationState::InProgress`]; a session that
/// was already paused or force-terminated stays as it is.
fn complete_when_all_done(session: &mut CollaborationSession, session_id: Uuid) {
    let all_done = session
        .agents
        .values()
        .all(|handle| handle.status == AgentStatus::Done);
    if !all_done {
        return;
    }
    if !matches!(session.state, CollaborationState::InProgress { .. }) {
        return;
    }
    session.state = CollaborationState::Completed;
    info!(
        session_id = %session_id,
        "all agents reported done — collaboration completed"
    );
}
